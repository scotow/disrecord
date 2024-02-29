pub use std::collections::HashMap;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use itertools::Itertools;
use serenity::model::id::{GuildId, UserId};
use tokio::sync::Mutex;
use ulid::Ulid;

const LOGS_DURATION: Duration = Duration::from_secs(5 * 60);
const MIN_LOGS_FETCH: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct History {
    guild_counters: Mutex<HashMap<GuildId, GuildHistory>>,
}

impl History {
    pub async fn register(&self, guild: GuildId, user: UserId, sound: Ulid) {
        self.guild_counters
            .lock()
            .await
            .entry(guild)
            .or_default()
            .register(user, sound);
    }

    pub async fn get_logs(
        &self,
        guild: GuildId,
        duration: Duration,
    ) -> Option<(Duration, Vec<(UserId, u32)>)> {
        let duration = duration.clamp(MIN_LOGS_FETCH, LOGS_DURATION);
        Some((
            duration,
            self.guild_counters
                .lock()
                .await
                .get(&guild)?
                .logs_counters(duration),
        ))
    }

    pub async fn get_latest_played(&self, guild: GuildId, offset: usize) -> Option<Ulid> {
        self.guild_counters
            .lock()
            .await
            .get(&guild)?
            .last_played_sound(offset)
    }
}

#[derive(Default)]
struct GuildHistory {
    logs: VecDeque<(UserId, Instant, Ulid)>,
}

impl GuildHistory {
    fn register(&mut self, user: UserId, sound: Ulid) {
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
        self.logs.push_back((user, Instant::now(), sound));
    }

    fn logs_counters(&self, duration: Duration) -> Vec<(UserId, u32)> {
        self.logs
            .iter()
            .rev()
            .take_while(|(_user, ts, _sound)| ts.elapsed() <= duration)
            .map(|(user, _ts, _sound)| *user)
            .counts()
            .into_iter()
            .sorted_by(|(_u1, c1), (_u2, c2)| c1.cmp(c2).reverse())
            .map(|(user, count)| (user, count as u32))
            .collect()
    }

    fn last_played_sound(&self, offset: usize) -> Option<Ulid> {
        self.logs
            .iter()
            .rev()
            .map(|(_user, _ts, sound)| sound)
            .unique()
            .nth(offset)
            .cloned()
    }
}
