pub use std::collections::HashMap;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use itertools::Itertools;
use serenity::model::id::{ChannelId, UserId};
use tokio::{sync::Mutex, time::sleep};

const CHANNEL_EDIT_RATE_LIMIT: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
pub struct History {
    channels: Mutex<HashMap<ChannelId, Arc<Mutex<ChannelHistory>>>>,
}

impl History {
    pub async fn register(&self, channel: ChannelId, user: UserId) -> Option<Vec<(UserId, u64)>> {
        let chan_history = self
            .channels
            .lock()
            .await
            .entry(channel)
            .or_default()
            .clone();
        let mut chan_history_lock = chan_history.lock().await;
        chan_history_lock.register(user);

        if chan_history_lock.pushing {
            None
        } else {
            chan_history_lock.pushing = true;
            drop(chan_history_lock);
            sleep(CHANNEL_EDIT_RATE_LIMIT + Duration::from_secs(15)).await;

            let mut chan_history_lock = chan_history.lock().await;
            chan_history_lock.pushing = false;
            Some(chan_history_lock.top())
        }
    }
}

#[derive(Default)]
struct ChannelHistory {
    pushing: bool,
    users: HashMap<UserId, (u64, Instant)>,
}

impl ChannelHistory {
    fn register(&mut self, user: UserId) {
        let user = self
            .users
            .entry(user)
            .or_insert_with(|| (0, Instant::now()));
        user.0 += 1;
        user.1 = Instant::now();
    }

    fn top(&self) -> Vec<(UserId, u64)> {
        self.users
            .iter()
            .sorted_by(|u1, u2| u1.1 .1.cmp(&u2.1 .1).reverse())
            .take(5)
            .map(|(u, (n, _ts))| (*u, *n))
            .collect()
    }
}
