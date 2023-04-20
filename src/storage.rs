use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use serenity::model::id::UserId;
use tokio::{
    sync::{mpsc, mpsc::UnboundedSender, oneshot::Sender as OneshotSender},
    time::sleep,
};

pub type Ssrc = u32;

const FREQUENCY: usize = 48_000;

pub struct Storage {
    buffer_size: Duration,
    clean_timeout: Duration,
    mapping: HashMap<Ssrc, UserVoiceData>,
    white_list: HashSet<UserId>,
}

impl Storage {
    pub fn new(buffer_size: Duration, clean_timeout: Duration) -> Self {
        assert!(buffer_size > Duration::from_secs(1));

        Self {
            buffer_size,
            clean_timeout,
            mapping: HashMap::new(),
            white_list: HashSet::new(),
        }
    }

    pub fn run_loop(mut self) -> UnboundedSender<Action> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Action>();
        tokio::spawn(async move {
            loop {
                let event = rx.recv().await.expect("Event channel closed.");
                match event {
                    Action::AddToWhitelist(users) => {
                        self.white_list.extend(users);
                    }
                    Action::RemoveToWhitelist(users) => {
                        for to_remove in users {
                            // Remove to white list.
                            self.white_list.remove(&to_remove);

                            // Remove data, but keep mapping.
                            if let Some(user_data) =
                                self.mapping.values_mut().find(|user| user.id == to_remove)
                            {
                                user_data.data.clear();
                            }
                        }
                    }
                    Action::GetWhitelist(tx) => {
                        tx.send(self.white_list.clone())
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
                            UserVoiceData::new(id, self.buffer_size)
                        };
                        self.mapping.insert(ssrc, user_data);
                    }
                    Action::RegisterVoiceData(ssrc, data) => {
                        if let Some(user_data) = self.mapping.get_mut(&ssrc) {
                            if self.white_list.contains(&user_data.id) {
                                user_data.last_insert = Instant::now();
                                if user_data.data.capacity() < user_data.data.len() + data.len() {
                                    for _ in 0..data.len()
                                        - (user_data.data.capacity() - user_data.data.len())
                                    {
                                        user_data.data.pop_front();
                                    }
                                }
                                user_data.data.extend(data);
                            }
                        };
                    }
                    Action::GetData(user, tx) => {
                        let data = match self
                            .mapping
                            .values()
                            .find_map(|user_data| (user_data.id == user).then_some(user_data))
                        {
                            Some(user_data) if !user_data.data.is_empty() => {
                                Some(user_data.data.clone())
                            }
                            Some(_) | None => None,
                        };
                        tx.send(data).expect("Voice data send failed.");
                    }
                    Action::Cleanup => {
                        self.mapping.retain(|_, user_data| {
                            user_data.last_insert.elapsed() <= self.clean_timeout
                        });
                    }
                }
            }
        });

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
    data: VecDeque<i16>,
    last_insert: Instant,
}

impl UserVoiceData {
    fn new(id: UserId, buffer_size: Duration) -> Self {
        Self {
            id,
            data: VecDeque::with_capacity(buffer_size.as_secs() as usize * FREQUENCY),
            last_insert: Instant::now(),
        }
    }
}

#[derive(Debug)]
pub enum Action {
    AddToWhitelist(HashSet<UserId>),
    RemoveToWhitelist(HashSet<UserId>),
    GetWhitelist(OneshotSender<HashSet<UserId>>),
    MapUser(UserId, Ssrc),
    RegisterVoiceData(Ssrc, Vec<i16>),
    GetData(UserId, OneshotSender<Option<VecDeque<i16>>>),
    Cleanup,
}
