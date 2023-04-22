use std::{
    collections::{HashMap, HashSet, VecDeque},
    mem,
    path::PathBuf,
    time::{Duration, Instant},
};

use serenity::model::id::UserId;
use tokio::{
    fs::{File, OpenOptions},
    io::AsyncWriteExt,
    sync::{mpsc, mpsc::UnboundedSender, oneshot::Sender as OneshotSender},
    time::sleep,
};

pub type Ssrc = u32;

pub const FREQUENCY: usize = 48_000;

pub struct Storage {
    buffer_size: Duration,
    clean_timeout: Duration,
    mapping: HashMap<Ssrc, UserVoiceData>,
    whitelist: HashSet<UserId>,
    whitelist_path: PathBuf,
}

impl Storage {
    pub async fn new(
        buffer_size: Duration,
        clean_timeout: Duration,
        whitelist_path: PathBuf,
    ) -> Self {
        assert!(buffer_size > Duration::from_secs(1) && buffer_size < Duration::from_secs(4 * 60));

        let whitelist = tokio::fs::read(&whitelist_path)
            .await
            .ok()
            .map(|file| {
                file.chunks(mem::size_of::<u64>())
                    .map(|l| {
                        UserId(u64::from_be_bytes(
                            l.try_into().expect("Invalid whitelist user id"),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();

        Self {
            buffer_size,
            clean_timeout,
            mapping: HashMap::new(),
            whitelist,
            whitelist_path,
        }
    }

    pub fn run_loop(mut self) -> UnboundedSender<Action> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Action>();
        tokio::spawn(async move {
            loop {
                let event = rx.recv().await.expect("Event channel closed.");
                match event {
                    Action::AddToWhitelist(user) => {
                        if self.whitelist.insert(user) {
                            let mut file = OpenOptions::new()
                                .append(true)
                                .create(true)
                                .open(&self.whitelist_path)
                                .await
                                .expect("Cannot create whitelist file");
                            file.write_u64(*user.as_u64())
                                .await
                                .expect("Cannot append user id to whitelist");
                        }
                    }
                    Action::RemoveFromWhitelist(user) => {
                        // Remove to white list.
                        if self.whitelist.remove(&user) {
                            File::create(&self.whitelist_path)
                                .await
                                .expect("Cannot open whitelist file")
                                .write_all(
                                    &self
                                        .whitelist
                                        .iter()
                                        .flat_map(|user| user.as_u64().to_be_bytes())
                                        .collect::<Vec<_>>(),
                                )
                                .await
                                .expect("Cannot write whitelist file");
                        }

                        // Remove data, but keep mapping.
                        if let Some(user_data) = self
                            .mapping
                            .values_mut()
                            .find(|user_data| user_data.id == user)
                        {
                            user_data.data = None;
                        }
                    }
                    Action::GetWhitelist(tx) => {
                        tx.send(self.whitelist.clone())
                            .expect("Cannot send whitelist.");
                    }
                    Action::MapUser(id, ssrc) => {
                        let user_data = if let Some(previous) = self
                            .mapping
                            .iter()
                            .find_map(|(ssrc, user)| (user.id == id).then_some(*ssrc))
                        {
                            let mut previous = self.mapping.remove(&previous).unwrap();
                            previous.last_insert = Instant::now();
                            previous
                        } else {
                            UserVoiceData::new(id)
                        };
                        self.mapping.insert(ssrc, user_data);
                    }
                    Action::RegisterVoiceData(ssrc, data) => {
                        if let Some(user_data) = self.mapping.get_mut(&ssrc) {
                            if self.whitelist.contains(&user_data.id) {
                                user_data.push_data(data, self.buffer_size);
                            }
                        };
                    }
                    Action::GetData(user, tx) => {
                        let data =
                            match self.mapping.values().find_map(|user_data| {
                                (user_data.id == user).then_some(&user_data.data)
                            }) {
                                Some(Some(data)) if !data.is_empty() => Some(data.clone()),
                                _ => None,
                            };
                        tx.send(data).expect("Voice data send failed.");
                    }
                    Action::Cleanup => {
                        for user_data in self.mapping.values_mut() {
                            if user_data.last_insert.elapsed() > self.clean_timeout {
                                user_data.data = None;
                            }
                        }
                    }
                }
            }
        });

        // Cleaner loop.
        let tx_ref = tx.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(30)).await;
                tx_ref
                    .send(Action::Cleanup)
                    .expect("Cleanup message failure.");
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
pub enum Action {
    AddToWhitelist(UserId),
    RemoveFromWhitelist(UserId),
    GetWhitelist(OneshotSender<HashSet<UserId>>),
    MapUser(UserId, Ssrc),
    RegisterVoiceData(Ssrc, Vec<i16>),
    GetData(UserId, OneshotSender<Option<VecDeque<i16>>>),
    Cleanup,
}
