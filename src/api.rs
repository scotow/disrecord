use std::sync::Arc;

use axum::{
    Router,
    extract::{FromRef, Path, State},
    http::StatusCode,
    routing,
};
use rand::seq::IteratorRandom;
use serenity::all::{Cache, ChannelId, GuildId, Http, UserId};
use songbird::{CoreEvent, Event, Songbird};
use tokio::sync::Mutex;
use ulid::Ulid;

use crate::{
    VoiceHandler, find_voice_channel, history::History, recorder::Recorder, soundboard::Soundboard,
};

#[derive(FromRef, Clone)]
pub struct ApiState {
    pub http: Arc<Http>,
    pub cache: Arc<Cache>,
    pub songbird: Arc<Songbird>,
    pub recorder: Arc<Mutex<Recorder>>,
    pub soundboard: Arc<Soundboard>,
    pub history: Arc<History>,
}

async fn join_channel(
    State(songbird): State<Arc<Songbird>>,
    State(recorder): State<Arc<Mutex<Recorder>>>,
    Path((guild, channel)): Path<(GuildId, ChannelId)>,
) -> StatusCode {
    let call = songbird.get_or_insert(guild);
    let mut call_lock = call.lock().await;

    let recorder = VoiceHandler {
        guild_recorder: recorder.lock().await.get_guild_recorder(guild).await,
    };
    call_lock.remove_all_global_events();
    call_lock.add_global_event(
        Event::Core(CoreEvent::SpeakingStateUpdate),
        recorder.clone(),
    );
    call_lock.add_global_event(Event::Core(CoreEvent::VoiceTick), recorder);

    let handle = call_lock
        .join(channel)
        .await
        .expect("Voice connexion failure");
    drop(call_lock);
    handle.await.expect("Voice connexion failure");

    StatusCode::OK
}

async fn join_user_channel(
    State(http): State<Arc<Http>>,
    State(cache): State<Arc<Cache>>,
    State(songbird): State<Arc<Songbird>>,
    State(recorder): State<Arc<Mutex<Recorder>>>,
    Path((guild, user)): Path<(GuildId, UserId)>,
) -> StatusCode {
    let Some(channel) = find_voice_channel(http, cache, guild, user).await else {
        return StatusCode::NOT_FOUND;
    };
    join_channel(State(songbird), State(recorder), Path((guild, channel))).await
}

async fn play_sound(
    State(songbird): State<Arc<Songbird>>,
    State(soundboard): State<Arc<Soundboard>>,
    Path((guild, sounds)): Path<(GuildId, String)>,
) -> StatusCode {
    let Some(selected) = sounds
        .split('|')
        .choose(&mut rand::rng())
        .and_then(|s| s.parse().ok())
    else {
        return StatusCode::BAD_REQUEST;
    };
    if super::play_sound(songbird, &soundboard, guild, selected).await {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn play_random_sound(
    State(songbird): State<Arc<Songbird>>,
    State(soundboard): State<Arc<Soundboard>>,
    Path(guild): Path<GuildId>,
) -> StatusCode {
    let Some(sound) = soundboard.random_id(guild).await else {
        return StatusCode::NOT_FOUND;
    };
    play_sound_id(State(songbird), State(soundboard), Path((guild, sound))).await
}

async fn play_latest_sound(
    State(songbird): State<Arc<Songbird>>,
    State(soundboard): State<Arc<Soundboard>>,
    Path(guild): Path<GuildId>,
) -> StatusCode {
    let Some(sound) = soundboard.latest_id(guild).await else {
        return StatusCode::NOT_FOUND;
    };
    play_sound_id(State(songbird), State(soundboard), Path((guild, sound))).await
}

async fn play_last_played_sound(
    State(songbird): State<Arc<Songbird>>,
    State(soundboard): State<Arc<Soundboard>>,
    State(history): State<Arc<History>>,
    Path(guild): Path<GuildId>,
) -> StatusCode {
    let Some(sound) = history.get_latest_played(guild, 0).await else {
        return StatusCode::NOT_FOUND;
    };
    play_sound_id(State(songbird), State(soundboard), Path((guild, sound))).await
}

async fn play_last_played_offset_sound(
    State(songbird): State<Arc<Songbird>>,
    State(soundboard): State<Arc<Soundboard>>,
    State(history): State<Arc<History>>,
    Path((guild, offset)): Path<(GuildId, usize)>,
) -> StatusCode {
    let Some(sound) = history.get_latest_played(guild, offset).await else {
        return StatusCode::NOT_FOUND;
    };
    play_sound_id(State(songbird), State(soundboard), Path((guild, sound))).await
}

async fn play_sound_id(
    State(songbird): State<Arc<Songbird>>,
    State(soundboard): State<Arc<Soundboard>>,
    Path((guild, sound)): Path<(GuildId, Ulid)>,
) -> StatusCode {
    if super::play_sound(songbird, &soundboard, guild, sound).await {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route(
            "/guilds/{guild}/channels/{channel}/join",
            routing::post(join_channel),
        )
        .route(
            "/guilds/{guild}/users/{user}/follow",
            routing::post(join_user_channel),
        )
        .route(
            "/guilds/{guild}/sounds/{sound}/play",
            routing::post(play_sound),
        )
        .route(
            "/guilds/{guild}/sounds/random/play",
            routing::post(play_random_sound),
        )
        .route(
            "/guilds/{guild}/sounds/latest/play",
            routing::post(play_latest_sound),
        )
        .route(
            "/guilds/{guild}/sounds/last-played/play",
            routing::post(play_last_played_sound),
        )
        .route(
            "/guilds/{guild}/sounds/last-played/{offset}/play",
            routing::post(play_last_played_offset_sound),
        )
        .with_state(state)
}
