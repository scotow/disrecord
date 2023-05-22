pub use std::collections::HashMap;
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use itertools::Itertools;
use serenity::model::id::{ChannelId, UserId};
use tokio::{sync::Mutex, time::sleep};

const CHANNEL_TOPIC_EDIT_RATE_LIMIT: Duration = Duration::from_secs(5 * 60);
const LOGS_DURATION: Duration = Duration::from_secs(5 * 60);
const MIN_LOGS_FETCH: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct History {
    counters: Mutex<HashMap<ChannelId, Arc<Mutex<ChannelHistory>>>>,
}

impl History {
    pub async fn register(&self, channel: ChannelId, user: UserId) -> Option<Vec<(UserId, u32)>> {
        let channel_history = self
            .counters
            .lock()
            .await
            .entry(channel)
            .or_default()
            .clone();
        let mut channel_history_lock = channel_history.lock().await;
        channel_history_lock.register(user);

        if channel_history_lock.pushing_counters {
            None
        } else {
            channel_history_lock.pushing_counters = true;
            drop(channel_history_lock);
            sleep(CHANNEL_TOPIC_EDIT_RATE_LIMIT + Duration::from_secs(15)).await;

            let mut channel_history_lock = channel_history.lock().await;
            channel_history_lock.pushing_counters = false;
            Some(channel_history_lock.top_counters())
        }
    }

    pub async fn get_logs(
        &self,
        channel: ChannelId,
        duration: Duration,
    ) -> Option<(Duration, Vec<(UserId, u32)>)> {
        let duration = duration.clamp(MIN_LOGS_FETCH, LOGS_DURATION);
        Some((
            duration,
            self.counters
                .lock()
                .await
                .get(&channel)?
                .lock()
                .await
                .logs_counters(duration),
        ))
    }
}

#[derive(Default)]
struct ChannelHistory {
    pushing_counters: bool,
    counters: HashMap<UserId, (u32, Instant)>,
    logs: VecDeque<(UserId, Instant)>,
}

impl ChannelHistory {
    fn register(&mut self, user: UserId) {
        // Increment all-time counter.
        let counter = self
            .counters
            .entry(user)
            .or_insert_with(|| (0, Instant::now()));
        counter.0 += 1;
        counter.1 = Instant::now();

        // Clear old logs.
        loop {
            match self.logs.front() {
                Some(oldest) if oldest.1.elapsed() > LOGS_DURATION => {
                    self.logs.pop_front();
                }
                _ => break,
            }
        }
        // Append log.
        self.logs.push_back((user, Instant::now()));
    }

    fn top_counters(&self) -> Vec<(UserId, u32)> {
        self.counters
            .iter()
            .sorted_by(|u1, u2| u1.1 .1.cmp(&u2.1 .1).reverse())
            .take(5)
            .map(|(u, (n, _ts))| (*u, *n))
            .collect()
    }

    fn logs_counters(&self, duration: Duration) -> Vec<(UserId, u32)> {
        self.logs
            .iter()
            .rev()
            .take_while(|(_user, ts)| ts.elapsed() <= duration)
            .map(|(user, _ts)| *user)
            .counts()
            .into_iter()
            .sorted_by(|(_u1, c1), (_u2, c2)| c1.cmp(c2).reverse())
            .map(|(user, count)| (user, count as u32))
            .rev()
            .collect()
    }
}
