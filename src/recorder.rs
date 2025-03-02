use std::{
    collections::{HashMap, HashSet, VecDeque},
    mem,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use itertools::Itertools;
use log::{Level, debug, info, log, log_enabled};
use serenity::model::id::{GuildId, UserId};
use tokio::{
    fs::{File, OpenOptions},
    io::AsyncWriteExt,
    sync::{Mutex, mpsc, mpsc::UnboundedSender, oneshot::Sender as OneshotSender},
    time::sleep,
};

pub type Ssrc = u32;

pub const FREQUENCY: usize = 48_000;

// Log every voice data packet on debug and only one every five minutes on info
// level.
macro_rules! log_voice_data {
    ($st:tt, $($arg:tt)+) => {
        if log_enabled!(Level::Debug) {
            debug!($($arg)+);
        } else if log_enabled!(Level::Info) {
            $st.voice_data_received += 1;
            if $st.voice_data_received % 15_000 == 0 { // ~ Once every five minute of voice data.
                info!($($arg)+);
            }
        }
    };
}

pub struct Recorder {
    buffer_size: Duration,
    clean_timeout: Duration,
    whitelist: HashSet<UserId>,
    whitelist_path: PathBuf,
    guilds: HashMap<GuildId, UnboundedSender<RecorderAction>>,
}

impl Recorder {
    pub async fn new(
        buffer_size: Duration,
        clean_timeout: Duration,
        whitelist_path: PathBuf,
    ) -> Self {
        info!("creating storage");
        assert!(buffer_size > Duration::from_secs(1));

        let whitelist = tokio::fs::read(&whitelist_path)
            .await
            .ok()
            .map(|file| {
                file.chunks(mem::size_of::<u64>())
                    .map(|l| {
                        UserId::new(u64::from_be_bytes(
                            l.try_into().expect("Invalid whitelist user id"),
                        ))
                    })
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        info!("initial whitelist has {} users", whitelist.len());

        Self {
            buffer_size,
            clean_timeout,
            whitelist,
            whitelist_path,
            guilds: HashMap::new(),
        }
    }

    pub fn get_whitelist(&self) -> HashSet<UserId> {
        info!("fetching whitelist ({} users)", self.whitelist.len());
        self.whitelist.clone()
    }

    pub async fn add_whitelist(&mut self, user: UserId) {
        info!("adding user {user} to whitelist");
        if self.whitelist.insert(user) {
            let mut file = OpenOptions::new()
                .append(true)
                .create(true)
                .open(&self.whitelist_path)
                .await
                .expect("Cannot create whitelist file");
            file.write_u64(user.get())
                .await
                .expect("Cannot append user id to whitelist");

            for guild in self.guilds.values() {
                guild
                    .send(RecorderAction::AddToWhitelist(user))
                    .expect("Failed to propagate whitelist removal");
            }

            info!("user {user} added to whitelist");
        } else {
            info!("user {user} already in whitelist");
        }
    }

    pub async fn remove_whitelist(&mut self, user: UserId) {
        info!("removing user {user} from whitelist");
        if self.whitelist.remove(&user) {
            File::create(&self.whitelist_path)
                .await
                .expect("Cannot open whitelist file")
                .write_all(
                    &self
                        .whitelist
                        .iter()
                        .flat_map(|user| user.get().to_be_bytes())
                        .collect::<Vec<_>>(),
                )
                .await
                .expect("Cannot write whitelist file");

            for guild in self.guilds.values() {
                guild
                    .send(RecorderAction::RemoveFromWhitelist(user))
                    .expect("Failed to propagate whitelist removal");
            }

            info!("user {user} removed from whitelist");
        } else {
            info!("user {user} not in whitelist");
        }
    }

    pub async fn get_guild_recorder(&mut self, guild: GuildId) -> UnboundedSender<RecorderAction> {
        match self.guilds.get(&guild) {
            Some(channel) => channel.clone(),
            None => {
                let channel = GuildRecorder {
                    whitelist: self.whitelist.clone(),
                    buffer_size: self.buffer_size,
                    voice_data: HashMap::new(),
                    voice_data_received: 0,
                    clean_timeout: self.clean_timeout,
                }
                .run_loop();
                self.guilds.insert(guild, channel.clone());
                channel
            }
        }
    }

    pub fn cleanup_loop(recorder: Arc<Mutex<Self>>) {
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(30)).await;
                for guild_recorder in recorder.lock().await.guilds.values() {
                    guild_recorder
                        .send(RecorderAction::CleanOld)
                        .expect("Failed to send cleanup message");
                }
            }
        });
    }
}

pub struct GuildRecorder {
    whitelist: HashSet<UserId>,
    buffer_size: Duration,
    voice_data: HashMap<Ssrc, UserVoiceData>,
    voice_data_received: usize,
    clean_timeout: Duration,
}

impl GuildRecorder {
    fn run_loop(mut self) -> UnboundedSender<RecorderAction> {
        let (tx, mut rx) = mpsc::unbounded_channel::<RecorderAction>();
        tokio::spawn(async move {
            loop {
                let event = rx.recv().await.expect("Event channel closed.");
                match event {
                    RecorderAction::AddToWhitelist(user) => {
                        self.whitelist.insert(user);
                    }
                    RecorderAction::RemoveFromWhitelist(user) => {
                        self.whitelist.remove(&user);
                        if let Some(user_data) = self
                            .voice_data
                            .values_mut()
                            .find(|user_data| user_data.id == user)
                        {
                            user_data.data = None;
                        }
                    }
                    RecorderAction::MapUser(id, ssrc) => {
                        info!("mapping ssrc {ssrc} to user {id}");
                        let user_data = if let Some(previous) = self
                            .voice_data
                            .iter()
                            .find_map(|(ssrc, user)| (user.id == id).then_some(*ssrc))
                        {
                            info!("replacing mapping for ssrc {ssrc} and user {id}");
                            let mut previous = self.voice_data.remove(&previous).unwrap();
                            previous.last_insert = Instant::now();
                            previous
                        } else {
                            info!("creating new mapping for ssrc {ssrc} and user {id}");
                            UserVoiceData::new(id)
                        };
                        self.voice_data.insert(ssrc, user_data);
                        info!("mapped ssrc {ssrc} to user {id}");
                    }
                    RecorderAction::RegisterVoiceData(ssrc, data) => {
                        log_voice_data!(
                            self,
                            "registering {} bytes voice data for ssrc {ssrc}",
                            data.len() * 2
                        );

                        match self.voice_data.get_mut(&ssrc) {
                            Some(user_data) => {
                                log_voice_data!(
                                    self,
                                    "adding voice data to user {} for ssrc {ssrc}",
                                    user_data.id
                                );
                                if self.whitelist.contains(&user_data.id) {
                                    user_data.push_data(data, self.buffer_size);
                                    log_voice_data!(
                                        self,
                                        "added voice data to user {} for ssrc {ssrc}",
                                        user_data.id
                                    );
                                }
                            }
                            None => {
                                log_voice_data!(self, "no user mapping found for ssrc {ssrc}",);
                            }
                        }
                    }
                    RecorderAction::GetVoiceData(user, tx) => {
                        info!("fetching data for user {user}");
                        let data =
                            match self.voice_data.values().find_map(|user_data| {
                                (user_data.id == user).then_some(&user_data.data)
                            }) {
                                Some(Some(data)) if !data.is_empty() => Some(data.clone()),
                                _ => None,
                            };
                        info!(
                            "fetched {} bytes of data for user {user}",
                            data.as_ref().map(|d| d.len()).unwrap_or(0) * 2
                        );
                        tx.send(data).expect("Voice data send failed.");
                    }
                    RecorderAction::GetVoiceDataChunks(user, len, min_duration, tx) => {
                        info!("fetching data for user {user}");
                        let data = match self
                            .voice_data
                            .values()
                            .find_map(|user_data| (user_data.id == user).then_some(&user_data.data))
                        {
                            Some(Some(data)) if !data.is_empty() => {
                                let mut data = data.clone();
                                data.make_contiguous().reverse();
                                let data = Vec::from(data);

                                let mut chunks = data
                                    .chunks(FREQUENCY / 50)
                                    .chunk_by(|c| c.iter().any(|&f| f != 0))
                                    .into_iter()
                                    .filter(|&(is_voice, _)| is_voice)
                                    .filter_map(|(_, frames)| {
                                        let mut chunk =
                                            frames.into_iter().flatten().copied().collect_vec();
                                        if chunk.len()
                                            < min_duration.as_millis() as usize * FREQUENCY / 1000
                                        {
                                            return None;
                                        }
                                        chunk.reverse();
                                        Some(chunk)
                                    })
                                    .take(len)
                                    .collect_vec();
                                if chunks.is_empty() {
                                    None
                                } else {
                                    chunks.reverse();
                                    Some(chunks)
                                }
                            }
                            _ => None,
                        };
                        info!(
                            "fetched {} voice chunks for user {user}",
                            data.as_ref().map(|d| d.len()).unwrap_or(0)
                        );
                        tx.send(data).expect("Voice data chunks send failed.");
                    }
                    RecorderAction::CleanOld => {
                        debug!("cleaning users voice data that hasn't speak for a while");
                        let mut cleaned = 0;
                        for user_data in self.voice_data.values_mut() {
                            if user_data.last_insert.elapsed() > self.clean_timeout
                                && user_data.data.is_some()
                            {
                                user_data.data = None;
                                cleaned += 1;
                            }
                        }
                        log!(
                            if cleaned > 0 {
                                Level::Info
                            } else {
                                Level::Debug
                            },
                            "cleaned {cleaned} users voice data"
                        );
                    }
                }
            }
        });
        tx
    }
}

struct UserVoiceData {
    id: UserId,
    data: Option<VecDeque<i16>>,
    last_insert: Instant,
}

impl UserVoiceData {
    fn new(id: UserId) -> Self {
        Self {
            id,
            data: None,
            last_insert: Instant::now(),
        }
    }

    fn push_data(&mut self, new_data: Vec<i16>, buffer_size: Duration) {
        self.last_insert = Instant::now();
        let data = self.data.get_or_insert_with(|| {
            VecDeque::with_capacity(buffer_size.as_secs() as usize * FREQUENCY)
        });

        // Make space without increasing capacity (if needed).
        if data.capacity() < data.len() + new_data.len() {
            for _ in 0..new_data.len() - (data.capacity() - data.len()) {
                data.pop_front();
            }
        }
        data.extend(new_data);
    }
}

#[derive(Debug)]
pub enum RecorderAction {
    AddToWhitelist(UserId),
    RemoveFromWhitelist(UserId),
    MapUser(UserId, Ssrc),
    RegisterVoiceData(Ssrc, Vec<i16>),
    GetVoiceData(UserId, OneshotSender<Option<VecDeque<i16>>>),
    GetVoiceDataChunks(
        UserId,
        usize,
        Duration,
        OneshotSender<Option<Vec<Vec<i16>>>>,
    ),
    CleanOld,
}
