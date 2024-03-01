use std::{
    collections::{HashSet, VecDeque},
    io::{Cursor, Write},
    net::SocketAddr,
    process::ExitCode,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    extract::{FromRef, Path, State},
    http::StatusCode,
    routing, Router, Server,
};
use clap::Parser;
use env_logger::Builder;
use itertools::Itertools;
use log::{error, info, warn};
use serenity::{
    all::{
        AutocompleteChoice, ButtonStyle, ChannelId, ChannelType, Command, CommandInteraction,
        CommandOptionType, CommandType, ComponentInteraction, CreateAllowedMentions,
        CreateAutocompleteResponse, CreateButton, CreateCommand, CreateCommandOption,
        CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, GuildId,
        HttpError, Interaction, Mention, ReactionType, Ready, UserId, VoiceState,
    },
    async_trait,
    builder::{CreateActionRow, CreateAttachment, CreateInteractionResponseFollowup},
    cache::Cache,
    client::{Context, EventHandler},
    http::Http,
    prelude::{GatewayIntents, SerenityError},
    Client,
};
// use songbird::{
//     driver::DecodeMode,
//     input::{Codec, Container, Input, Reader},
//     CoreEvent, Event, EventContext, EventHandler as VoiceEventHandler, SerenityInit,
// Songbird, };
use songbird::EventHandler as VoiceEventHandler;
use songbird::{
    driver::DecodeMode, input::Input, CoreEvent, Event, EventContext, SerenityInit, Songbird,
};
use tokio::sync::{mpsc::UnboundedSender, oneshot, Mutex};
use ulid::Ulid;
use zip::{write::FileOptions as ZipFileOptions, ZipWriter};

use crate::{
    button::SoundButton,
    history::History,
    options::Options,
    recorder::{Recorder, RecorderAction},
    soundboard::Soundboard,
};

mod button;
mod command;
mod history;
mod options;
mod recorder;
mod soundboard;
mod wav;

/// Max body size is 25MiB including other fields. We cut at 24MiB because
/// calculating the rest of the body is too unreliable.
const MAX_FILE_SIZE: usize = 24 * (1 << 20);
const ROWS_PER_MESSAGE: usize = 5;
const SOUNDS_PER_ROW: usize = 5;
const AUTOCOMPLETE_MAX_CHOICES: usize = 25;
const MAX_ATTACHEMENTS_PER_MESSAGE: usize = 10;

/// Invalid Emoji error.
const INVALID_EMOJI_CODE: isize = 50035;
const INVALID_EMOJI_MESSAGE: &str = "BUTTON_COMPONENT_INVALID_EMOJI";

#[derive(Clone)]
struct Handler {
    bot_id: Arc<AtomicU64>,
    allow_delete: bool,
    allow_grey: bool,
    recorder: Arc<Mutex<Recorder>>,
    soundboard: Arc<Soundboard>,
    history: Arc<History>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, data_about_bot: Ready) {
        info!("bot ready");
        self.bot_id
            .store(data_about_bot.user.id.get(), Ordering::Relaxed);
        self.register_global_commands(&ctx).await;
    }

    async fn voice_state_update(&self, ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        if let Some(channel) = old.and_then(|c| c.channel_id) {
            self.disconnect_if_alone(&ctx, channel).await;
        }
        if let Some(channel) = new.channel_id {
            self.disconnect_if_alone(&ctx, channel).await;
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(command) => self.dispatch_command(ctx, command).await,
            Interaction::Component(component) => self.dispatch_component(ctx, component).await,
            Interaction::Autocomplete(autocomplete) => {
                self.dispatch_autocomplete(ctx, autocomplete).await
            }
            _ => return,
        }
    }
}

#[derive(Clone)]
struct VoiceHandler {
    guild_recorder: UnboundedSender<RecorderAction>,
}

#[async_trait]
impl VoiceEventHandler for VoiceHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::SpeakingStateUpdate(event) => {
                if let Some(user) = event.user_id {
                    self.guild_recorder
                        .send(RecorderAction::MapUser(UserId::new(user.0), event.ssrc))
                        .expect("Event dispatch error");
                }
            }
            EventContext::VoiceTick(packet) => {
                for (ssrc, audio) in packet.speaking.iter().filter_map(|(ssrc, data)| {
                    data.decoded_voice.as_ref().map(|decoded| (*ssrc, decoded))
                }) {
                    self.guild_recorder
                        .send(RecorderAction::RegisterVoiceData(
                            ssrc,
                            audio
                                .chunks_exact(2)
                                .map(|cs| ((cs[0] as i32 + cs[1] as i32) / 2) as i16)
                                .collect(),
                        ))
                        .expect("Event dispatch error");
                }
            }
            _ => {}
        }
        None
    }
}

impl Handler {
    async fn dispatch_command(&self, ctx: Context, command: CommandInteraction) {
        match command.data.name.as_str() {
            // Common.
            "version" => self.version(ctx, command).await,
            "join" => self.join_voice(ctx, command).await,

            // Recorder.
            "recorder" => match parse_subcommand(&command) {
                Some("list") => self.get_whitelist(ctx, command).await,
                Some("join") => self.join_whitelist(ctx, command).await,
                Some("leave") => self.leave_whitelist(ctx, command).await,
                Some("download") => self.download_recording(ctx, command).await,
                Some("download-chunks") => self.download_recording_chunks(ctx, command).await,
                _ => (),
            },

            // Soundboard.
            "soundboard" => match parse_subcommand(&command) {
                Some("list") => self.list_sounds(ctx, command).await,
                Some("upload") => self.upload_sound(ctx, command).await,
                Some("download") => self.download_sound(ctx, command).await,
                Some("delete") => self.delete_sound(ctx, command).await,
                Some("rename") => self.rename_sound(ctx, command).await,
                Some("move") => self.move_sound(ctx, command).await,
                Some("change-color") => self.change_sound_color(ctx, command).await,
                Some("change-emoji") => self.change_sound_emoji(ctx, command).await,
                Some("id") => self.sound_id(ctx, command).await,
                Some("backup") => self.backup_sounds(ctx, command).await,
                Some("logs") => self.soundboard_logs(ctx, command).await,
                _ => (),
            },
            _ => (),
        };
    }

    async fn dispatch_component(&self, ctx: Context, component: ComponentInteraction) {
        let Some(guild) = component.guild_id else {
            return;
        };

        let sound = if component.data.custom_id.starts_with("random-") {
            let Ok(hash) = component
                .data
                .custom_id
                .trim_start_matches("random-")
                .parse()
            else {
                return;
            };
            let Some(sound) = self.soundboard.random_id_in_group(guild, hash).await else {
                return;
            };
            sound
        } else if component.data.custom_id == "random" {
            let Some(sound) = self.soundboard.random_id(guild).await else {
                return;
            };
            sound
        } else if component.data.custom_id == "latest" {
            let Some(sound) = self.soundboard.latest_id(guild).await else {
                return;
            };
            sound
        } else {
            let Ok(sound) = Ulid::from_string(&component.data.custom_id) else {
                return;
            };
            sound
        };

        let manager = songbird::get(&ctx)
            .await
            .expect("Failed to get songbird manager");

        let (defer, played) = tokio::join!(
            component.defer(&ctx),
            play_sound(manager, &self.soundboard, guild, sound)
        );
        defer.expect("Failed to defer sound play");
        if !played {
            return;
        }

        self.history.register(guild, component.user.id, sound).await;
    }

    async fn dispatch_autocomplete(&self, ctx: Context, interaction: CommandInteraction) {
        let Some(guild) = interaction.guild_id else {
            return;
        };

        let Some(autocomplete) = interaction.data.autocomplete() else {
            return;
        };

        let matches = match autocomplete.name {
            "sound" => {
                self.soundboard
                    .names_matching(guild, autocomplete.value, AUTOCOMPLETE_MAX_CHOICES)
                    .await
            }
            "group" => {
                self.soundboard
                    .groups_matching(guild, autocomplete.value, AUTOCOMPLETE_MAX_CHOICES)
                    .await
            }
            _ => return,
        };

        interaction
            .create_response(
                ctx,
                CreateInteractionResponse::Autocomplete(
                    CreateAutocompleteResponse::new()
                        .set_choices(matches.into_iter().map(AutocompleteChoice::from).collect()),
                ),
            )
            .await
            .expect("Failed to send autocomplete response");
    }

    async fn version(&self, ctx: Context, command: CommandInteraction) {
        command
            .create_response(
                &ctx,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new().content(env!("CARGO_PKG_VERSION")),
                ),
            )
            .await
            .expect("Version response failure");
    }

    async fn get_whitelist(&self, ctx: Context, command: CommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };

        let list = self
            .recorder
            .lock()
            .await
            .get_whitelist()
            .intersection(
                &ctx.cache
                    .guild(guild)
                    .expect("Cannot find guild")
                    .members
                    .keys()
                    .copied()
                    .collect(),
            )
            .copied()
            .collect::<HashSet<_>>();

        command
            .create_response(
                &ctx,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(if list.is_empty() {
                            "*Nobody.*".to_owned()
                        } else {
                            list.into_iter().map(Mention::from).join(", ")
                        })
                        .allowed_mentions(CreateAllowedMentions::new()),
                ),
            )
            .await
            .expect("Cannot send whitelist");
    }

    async fn join_whitelist(&self, ctx: Context, command: CommandInteraction) {
        self.recorder
            .lock()
            .await
            .add_whitelist(command.user.id)
            .await;

        command
            .create_response(
                &ctx,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("You are now in the whitelist."),
                ),
            )
            .await
            .expect("Adding to whitelist failed");
    }

    async fn leave_whitelist(&self, ctx: Context, command: CommandInteraction) {
        self.recorder
            .lock()
            .await
            .remove_whitelist(command.user.id)
            .await;

        command
            .create_response(
                &ctx,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("You have been removed from the whitelist."),
                ),
            )
            .await
            .expect("Leaving whitelist failed");
    }

    async fn join_voice(&self, ctx: Context, command: CommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };
        let channel = match find_voice_channel(&ctx.http, &ctx.cache, guild, command.user.id).await
        {
            Some(channel) => channel,
            None => {
                command
                    .create_response(
                        &ctx,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content("You aren't in a voice channel. Dahhh..."),
                        ),
                    )
                    .await
                    .expect("Cannot send voice channel not found message");
                return;
            }
        };

        let manager = songbird::get(&ctx)
            .await
            .expect("Failed to get songbird manager");
        let call = manager.get_or_insert(guild);
        let mut call_lock = call.lock().await;

        let recorder = VoiceHandler {
            guild_recorder: self.recorder.lock().await.get_guild_recorder(guild).await,
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

        command
            .create_response(
                &ctx,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("Listening and ready to play sounds."),
                ),
            )
            .await
            .expect("Cannot send listen message");
    }

    async fn download_recording(&self, ctx: Context, command: CommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };
        let Some(requested_user) = command::find_user_option(&command, "user") else {
            return;
        };

        let (tx, rx) = oneshot::channel::<Option<VecDeque<i16>>>();
        self.recorder
            .lock()
            .await
            .get_guild_recorder(guild)
            .await
            .send(RecorderAction::GetVoiceData(requested_user.id, tx))
            .expect("Download request failure");

        let data = rx.await.expect("Voice data fetching error");
        match data.map(Vec::from) {
            Some(data) => {
                command.defer(&ctx).await.expect("Download defer failed");
                for (i, chunk) in data
                    .chunks((MAX_FILE_SIZE - wav::HEADER_SIZE) / 2)
                    .enumerate()
                {
                    let filename = if data.len() <= (MAX_FILE_SIZE - wav::HEADER_SIZE) / 2 {
                        format!("{}.wav", requested_user)
                    } else {
                        format!("{}-{}.wav", requested_user, i + 1)
                    };

                    command
                        .create_followup(
                            &ctx,
                            CreateInteractionResponseFollowup::new()
                                .add_file(CreateAttachment::bytes(wav::package(chunk), filename)),
                        )
                        .await
                        .expect("Voice data transmission failure");
                }
            }
            None => {
                command
                    .create_response(
                        &ctx,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content(format!("No voice data found for {}.", requested_user))
                                .allowed_mentions(CreateAllowedMentions::new()),
                        ),
                    )
                    .await
                    .expect("Download response failure");
            }
        }
    }

    async fn download_recording_chunks(&self, ctx: Context, command: CommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };
        let Some(requested_user) = command::find_user_option(&command, "user") else {
            return;
        };
        let Some(count) = command::find_integer_option(
            &command,
            "count",
            Some(MAX_ATTACHEMENTS_PER_MESSAGE as i64),
        )
        .map(|c| c as usize) else {
            return;
        };
        let Some(min_duration) = command::find_duration_option(
            &command,
            "min-duration",
            Some(Duration::from_millis(500)),
        ) else {
            command
                .create_response(
                    &ctx,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new().content("Invalid duration."),
                    ),
                )
                .await
                .expect("Recording chunks invalid duration response failure");
            return;
        };

        let (tx, rx) = oneshot::channel::<Option<Vec<Vec<i16>>>>();
        self.recorder
            .lock()
            .await
            .get_guild_recorder(guild)
            .await
            .send(RecorderAction::GetVoiceDataChunks(
                requested_user.id,
                count,
                min_duration,
                tx,
            ))
            .expect("Download request failure");

        let data = rx.await.expect("Voice data fetching error");
        match data {
            Some(data) => {
                command.defer(&ctx).await.expect("Download defer failed");
                let count = data.len();
                for (group_index, chunks) in data.chunks(MAX_ATTACHEMENTS_PER_MESSAGE).enumerate() {
                    command
                        .create_followup(
                            &ctx,
                            CreateInteractionResponseFollowup::new().add_files(
                                chunks.into_iter().enumerate().map(|(i, chunk)| {
                                    CreateAttachment::bytes(
                                        wav::package(&chunk),
                                        if count > 1 {
                                            format!(
                                                "{}-{}.wav",
                                                requested_user,
                                                group_index * MAX_ATTACHEMENTS_PER_MESSAGE + i + 1
                                            )
                                        } else {
                                            format!("{}.wav", requested_user)
                                        },
                                    )
                                }),
                            ),
                        )
                        .await
                        .expect("Voice data transmission failure");
                }
            }
            None => {
                command
                    .create_response(
                        &ctx,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content(format!("No voice data found for {}.", requested_user))
                                .allowed_mentions(CreateAllowedMentions::new()),
                        ),
                    )
                    .await
                    .expect("Download response failure");
            }
        }
    }

    async fn list_sounds(&self, ctx: Context, command: CommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };

        let Some(add_random) = command::find_boolean_option(&command, "random", Some(true)) else {
            return;
        };
        let Some(add_latest) = command::find_boolean_option(&command, "latest", Some(true)) else {
            return;
        };

        let sounds = self.soundboard.list(guild).await;
        if sounds.is_empty() {
            command
                .create_response(
                    &ctx,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content("There is no sounds uploaded to this server... yet."),
                    ),
                )
                .await
                .expect("Cannot send empty soundboard message");
            return;
        }

        let mut sounds = sounds
            .into_iter()
            .map(|(g, sounds)| {
                (
                    g,
                    sounds
                        .into_iter()
                        .map(|s| SoundButton::Sound(s))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();

        // Add random and latest buttons.
        let total_sounds = sounds
            .iter()
            .map(|(_g, sounds)| sounds.len())
            .sum::<usize>();
        let mut has_shortcuts_row = false;
        if add_random && total_sounds >= 2 {
            has_shortcuts_row = true;
            sounds.push(("Shortcuts".to_owned(), vec![SoundButton::Random(None)]))
        }
        if add_latest && total_sounds >= 2 {
            if has_shortcuts_row {
                sounds.last_mut().unwrap().1.push(SoundButton::Latest);
            } else {
                has_shortcuts_row = true;
                sounds.push(("Shortcuts".to_owned(), vec![SoundButton::Latest]));
            }
        }

        command
            .defer(&ctx)
            .await
            .expect("Failed to defer sound list");
        command
            .delete_response(&ctx)
            .await
            .expect("Failed to delete original sound list interaction");

        let groups_len = sounds.len();
        for (i, (group, mut sounds)) in sounds.into_iter().enumerate() {
            // Add random button if enough sounds in group.
            if add_random && sounds.len() >= 2 && (has_shortcuts_row && i != groups_len - 1) {
                sounds.insert(0, SoundButton::Random(Some(group.clone())));
            }

            // Send the group name once then at most 5 sound rows per message.
            for (message_index, sounds_message) in
                sounds.chunks(ROWS_PER_MESSAGE * SOUNDS_PER_ROW).enumerate()
            {
                command
                    .channel_id
                    .send_message(&ctx, {
                        let mut message = CreateMessage::new();
                        if message_index == 0 {
                            message = message.content(format!("# {group}"));
                        }
                        message.components(
                            sounds_message
                                .chunks(SOUNDS_PER_ROW)
                                .map(|sounds_row| {
                                    CreateActionRow::Buttons(
                                        sounds_row
                                            .into_iter()
                                            .map(|button| button.create())
                                            .collect(),
                                    )
                                })
                                .collect(),
                        )
                    })
                    .await
                    .expect("Failed to send sounds list");
            }
        }
    }

    async fn upload_sound(&self, ctx: Context, command: CommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };
        let Some(attachment) = command::find_attachment_option(&command, "sound") else {
            return;
        };
        let Some(name) = command::find_string_option(&command, "name", None) else {
            return;
        };
        let Some(group) = command::find_string_option(&command, "group", None) else {
            return;
        };
        let emoji = command::find_emoji_option(&command, "emoji");
        let color = command::find_string_option(&command, "color", None)
            .map(button::parse_color)
            .unwrap_or_else(|| button::determinist(&name.to_lowercase(), self.allow_grey));
        let index =
            command::find_integer_option(&command, "position", None).map(|p| (p - 1) as usize);

        match self
            .soundboard
            .add(
                attachment,
                guild,
                name.to_owned(),
                emoji.clone(),
                color,
                group.to_owned(),
                index,
            )
            .await
        {
            Ok(id) => {
                match command
                    .create_response(
                        &ctx,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new().components(vec![
                                CreateActionRow::Buttons(vec![{
                                    let mut button =
                                        CreateButton::new(id.to_string()).label(name).style(color);
                                    if let Some(emoji) = emoji {
                                        button = button.emoji(ReactionType::Unicode(emoji));
                                    }
                                    button
                                }]),
                            ]),
                        ),
                    )
                    .await
                {
                    Ok(()) => {}
                    Err(err) => {
                        // Try to catch invalid emoji error and rollback creation.
                        self.soundboard
                            .delete_id(guild, id)
                            .await
                            .expect("Failed to delete sound due to error");
                        let err_msg = match err {
                            SerenityError::Http(http_error) => match http_error {
                                HttpError::UnsuccessfulRequest(req) => {
                                    if req.status_code == StatusCode::BAD_REQUEST
                                        && req.error.code == INVALID_EMOJI_CODE
                                        && req.error.errors.into_iter().any(|sub_error| {
                                            sub_error.code == INVALID_EMOJI_MESSAGE
                                        })
                                    {
                                        command
                                            .create_response(
                                                &ctx,
                                                CreateInteractionResponse::Message(
                                                    CreateInteractionResponseMessage::new()
                                                        .content("Invalid emoji."),
                                                ),
                                            )
                                            .await
                                            .expect("Cannot send sound creation emoji error");
                                        "Uncaught invalid emoji".to_owned()
                                    } else {
                                        format!("Different status code: {}", req.error.code)
                                    }
                                }
                                err => err.to_string(),
                            },
                            err => err.to_string(),
                        };
                        warn!(
                            "unexpected error while sending sound button for the first time: {:?}",
                            err_msg
                        );
                    }
                }
            }
            Err(err) => {
                command
                    .create_response(
                        &ctx,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new().content(err.to_string()),
                        ),
                    )
                    .await
                    .expect("Cannot send sound creation error message");
            }
        }
    }

    async fn download_sound(&self, ctx: Context, command: CommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };
        let Some(name) = command::find_string_option(&command, "sound", None) else {
            return;
        };
        let group = command::find_string_option(&command, "group", None);

        // Does not support splitting.
        match self.soundboard.get_wav_by_name(guild, name, group).await {
            Ok(data) if data.len() <= MAX_FILE_SIZE => {
                command.defer(&ctx).await.expect("Download defer failed");
                command
                    .create_followup(
                        &ctx,
                        CreateInteractionResponseFollowup::new()
                            .add_file(CreateAttachment::bytes(data, format!("{name}.wav"))),
                    )
                    .await
                    .expect("Sound data transmission failure");
            }
            Ok(_) => {
                command
                    .create_response(
                        &ctx,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new().content("Sound too large."),
                        ),
                    )
                    .await
                    .expect("Download response failure");
            }
            Err(err) => {
                command
                    .create_response(
                        &ctx,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new().content(err.to_string()),
                        ),
                    )
                    .await
                    .expect("Download response failure");
            }
        }
    }

    async fn delete_sound(&self, ctx: Context, command: CommandInteraction) {
        if !self.allow_delete {
            return;
        }
        let Some(guild) = command.guild_id else {
            return;
        };
        let Some(name) = command::find_string_option(&command, "sound", None) else {
            return;
        };
        let group = command::find_string_option(&command, "group", None);

        let text = match self.soundboard.delete(guild, name, group).await {
            Ok(()) => "Deleted. *(for ever)*".to_owned(),
            Err(err) => err.to_string(),
        };
        command
            .create_response(
                &ctx,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new().content(text),
                ),
            )
            .await
            .expect("Cannot send sound deletion error message");
    }

    async fn rename_sound(&self, ctx: Context, command: CommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };
        let Some(name) = command::find_string_option(&command, "sound", None) else {
            return;
        };
        let Some(new_name) = command::find_string_option(&command, "new-name", None) else {
            return;
        };
        let group = command::find_string_option(&command, "group", None);

        let text = match self
            .soundboard
            .rename(guild, name, group, new_name.to_owned())
            .await
        {
            Ok(true) => "Sound's name changed.".to_owned(),
            Ok(false) => "The sound already had this name.".to_owned(),
            Err(err) => err.to_string(),
        };
        command
            .create_response(
                &ctx,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new().content(text),
                ),
            )
            .await
            .expect("Cannot send sound's name change error message");
    }

    async fn move_sound(&self, ctx: Context, command: CommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };
        let Some(name) = command::find_string_option(&command, "sound", None) else {
            return;
        };
        let Some(new_name) = command::find_string_option(&command, "new-group", None) else {
            return;
        };
        let group = command::find_string_option(&command, "group", None);

        let text = match self
            .soundboard
            .move_group(guild, name, group, new_name.to_owned())
            .await
        {
            Ok(true) => "Sound's group changed.".to_owned(),
            Ok(false) => "This sound already was in this group.".to_owned(),
            Err(err) => err.to_string(),
        };
        command
            .create_response(
                &ctx,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new().content(text),
                ),
            )
            .await
            .expect("Cannot send sound's group change error message");
    }

    async fn change_sound_color(&self, ctx: Context, command: CommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };
        let Some(name) = command::find_string_option(&command, "sound", None) else {
            return;
        };
        let group = command::find_string_option(&command, "group", None);
        let Some(color) =
            command::find_string_option(&command, "color", None).map(button::parse_color)
        else {
            return;
        };

        let text = match self
            .soundboard
            .change_color(guild, name, group, color)
            .await
        {
            Ok(true) => "Sound's color changed.".to_owned(),
            Ok(false) => "This sound already had this color.".to_owned(),
            Err(err) => err.to_string(),
        };
        command
            .create_response(
                &ctx,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new().content(text),
                ),
            )
            .await
            .expect("Cannot send sound's color change error message");
    }

    async fn change_sound_emoji(&self, ctx: Context, command: CommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };
        let Some(name) = command::find_string_option(&command, "sound", None) else {
            return;
        };
        let group = command::find_string_option(&command, "group", None);
        let Some(emoji) = command::find_emoji_option(&command, "emoji") else {
            return;
        };

        let text = match self
            .soundboard
            .change_emoji(guild, name, group, emoji)
            .await
        {
            Ok(true) => "Sound's emoji changed.".to_owned(),
            Ok(false) => "This sound already had this emoji.".to_owned(),
            Err(err) => err.to_string(),
        };
        command
            .create_response(
                &ctx,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new().content(text),
                ),
            )
            .await
            .expect("Cannot send sound's emoji change error message");
    }

    async fn sound_id(&self, ctx: Context, command: CommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };
        let Some(name) = command::find_string_option(&command, "sound", None) else {
            return;
        };
        let group = command::find_string_option(&command, "group", None);

        let text = match self.soundboard.get_id(guild, name, group).await {
            Ok(id) => id.to_string(),
            Err(err) => err.to_string(),
        };
        command
            .create_response(
                &ctx,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new().content(text),
                ),
            )
            .await
            .expect("Cannot send sound ID error message");
    }

    async fn backup_sounds(&self, ctx: Context, command: CommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };

        match self.soundboard.backup(guild).await {
            Ok((metadata, sounds)) => {
                if sounds.is_empty() {
                    command
                        .create_response(
                            &ctx,
                            CreateInteractionResponse::Message(
                                CreateInteractionResponseMessage::new()
                                    .content("There is no sounds on this server."),
                            ),
                        )
                        .await
                        .expect("Backup response failure");
                    return;
                }
                command.defer(&ctx).await.expect("Download defer failed");

                let mut sound_index = 0;
                let mut too_large = 0;
                while sound_index < sounds.len() {
                    let mut written = 0;

                    // Write metadata in every archive.
                    let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
                    written += metadata.len();
                    archive
                        .start_file("sounds.json", ZipFileOptions::default())
                        .expect("Failed to create backup archive");
                    archive
                        .write_all(metadata.as_bytes())
                        .expect("Failed to create backup archive");

                    while written < MAX_FILE_SIZE && sound_index < sounds.len() {
                        let (id, data) = &sounds[sound_index];
                        if data.len() > MAX_FILE_SIZE {
                            too_large += 1;
                        } else {
                            written += data.len();
                            archive
                                .start_file(id, ZipFileOptions::default())
                                .expect("Failed to create backup archive");
                            archive
                                .write_all(data)
                                .expect("Failed to create backup archive");
                        }
                        sound_index += 1;
                    }

                    let archive = archive
                        .finish()
                        .expect("Failed to create backup archive")
                        .into_inner();
                    command
                        .create_followup(&ctx,
                                         {
                                             let mut response = CreateInteractionResponseFollowup::new();
                                             if too_large > 0 {
                                                 response = response.content("{too_large} files were too large and weren't included in the backup.");
                                             }
                                             response.add_file(
                                                 CreateAttachment::bytes(
                                                     archive,
                                                     "backup.zip"
                                                 )
                                             )
                                         }
                        )
                        .await
                        .expect("Backup response failure");
                }
            }
            Err(err) => {
                command
                    .create_response(
                        &ctx,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new().content(err.to_string()),
                        ),
                    )
                    .await
                    .expect("Backup response failure");
            }
        }
    }

    async fn soundboard_logs(&self, ctx: Context, command: CommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };

        let Some(duration) =
            command::find_duration_option(&command, "duration", Some(Duration::from_secs(30)))
        else {
            command
                .create_response(
                    &ctx,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new().content("Invalid duration."),
                    ),
                )
                .await
                .expect("Logs response failure");
            return;
        };

        match self.history.get_logs(guild, duration).await {
            Some((resolved_duration, logs)) if !logs.is_empty() => {
                command
                    .create_response(
                        &ctx,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content(format!(
                                    "Soundboard usage for the last {}:\n{}",
                                    humantime::format_duration(resolved_duration),
                                    logs.into_iter()
                                        .map(|(user, count)| format!(
                                            // Markdown list auto increment number.
                                            "1. {}: {}",
                                            Mention::from(user),
                                            count
                                        ))
                                        .join("\n")
                                ))
                                .allowed_mentions(CreateAllowedMentions::new()),
                        ),
                    )
                    .await
                    .expect("Logs response failure");
            }
            _ => {
                command
                    .create_response(
                        &ctx,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new().content("No logs available."),
                        ),
                    )
                    .await
                    .expect("Logs response failure");
            }
        }
    }

    async fn disconnect_if_alone(&self, ctx: &Context, channel_id: ChannelId) {
        let guild_id = {
            let Some(channel) = ctx.cache.channel(channel_id) else {
                return;
            };
            if channel.kind != ChannelType::Voice {
                return;
            }

            let members = channel.members(&ctx).expect("Cannot fetch member list");
            if !(members.len() == 1 && members[0].user.id == self.bot_id.load(Ordering::Relaxed)) {
                return;
            }
            channel.guild_id
        };

        let manager = songbird::get(ctx)
            .await
            .expect("Failed to get songbird manager");
        if let Some(call) = manager.get(guild_id) {
            let mut call_lock = call.lock().await;
            call_lock
                .leave()
                .await
                .expect("Voice disconnection failure");
            call_lock.remove_all_global_events();
        }
    }

    async fn register_global_commands(&self, ctx: &Context) {
        info!("creating global commands");

        let version = CreateCommand::new("version")
            .description("Display version")
            .kind(CommandType::ChatInput);
        let join_voice_channel = CreateCommand::new("join")
            .description("Join your voice channel")
            .kind(CommandType::ChatInput);
        let recorder = CreateCommand::new("recorder")
            .description("Manage the recorder whitelist and download recordings")
            .kind(CommandType::ChatInput)
            // List.
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "list",
                "Get recorder's whitelist",
            ))
            // Join whitelist.
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "join",
                "Join recorder's whitelist",
            ))
            // Leave whitelist.
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "leave",
                "Leave recorder's whitelist",
            ))
            // Download recording.
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "download",
                    "Download a user's recording",
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::User,
                        "user",
                        "User to download data for",
                    )
                    .required(true),
                ),
            )
            // Download recording chunks.
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "download-chunks",
                    "Download a user's recording chunks",
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::User,
                        "user",
                        "User to download data for",
                    )
                    .required(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::Integer,
                        "count",
                        "Maximum number of chunks to fetch",
                    )
                    .required(false)
                    .min_int_value(1),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "min-duration",
                        "Minimum duration of chunks",
                    )
                    .required(false),
                ),
            );
        let mut soundboard = CreateCommand::new("soundboard")
            .description("Add, delete or download sounds to/from the soundboard")
            .kind(CommandType::ChatInput)
            // List.
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "list",
                    "List all sounds available on this server",
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::Boolean,
                        "random",
                        "Add a random sound button",
                    )
                    .required(false),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::Boolean,
                        "latest",
                        "Add a latest sound button",
                    )
                    .required(false),
                ),
            )
            // Upload.
            .add_option(
                CreateCommandOption::new(CommandOptionType::SubCommand, "upload", "Upload a sound")
                    .add_sub_option(
                        CreateCommandOption::new(
                            CommandOptionType::Attachment,
                            "sound",
                            "Sound file",
                        )
                        .required(true),
                    )
                    .add_sub_option(
                        CreateCommandOption::new(
                            CommandOptionType::String,
                            "name",
                            "The name of the sound that will appear on the button",
                        )
                        .required(true),
                    )
                    .add_sub_option(
                        CreateCommandOption::new(
                            CommandOptionType::String,
                            "group",
                            "The group to add this sound to",
                        )
                        .required(true)
                        .set_autocomplete(true),
                    )
                    .add_sub_option(
                        CreateCommandOption::new(
                            CommandOptionType::String,
                            "emoji",
                            "The emoji to prepend to the button",
                        )
                        .required(false),
                    )
                    .add_sub_option(
                        CreateCommandOption::new(
                            CommandOptionType::String,
                            "color",
                            "Color of the button",
                        )
                        .required(false)
                        .add_string_choice("blue", button::as_str(ButtonStyle::Primary))
                        .add_string_choice("green", button::as_str(ButtonStyle::Success))
                        .add_string_choice("red", button::as_str(ButtonStyle::Danger))
                        .add_string_choice("grey", button::as_str(ButtonStyle::Secondary)),
                    )
                    .add_sub_option(
                        CreateCommandOption::new(
                            CommandOptionType::Integer,
                            "position",
                            "The position of the sound in its group",
                        )
                        .required(false)
                        .min_int_value(1),
                    ),
            )
            // Download.
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "download",
                    "Download a sound",
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "sound",
                        "Sound name to download",
                    )
                    .required(true)
                    .set_autocomplete(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "group",
                        "Group name of the sound to delete",
                    )
                    .required(false)
                    .set_autocomplete(true),
                ),
            )
            // Rename.
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "rename",
                    "Change the name of a soundboard button",
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "sound",
                        "Sound name to change",
                    )
                    .required(true)
                    .set_autocomplete(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "new-name",
                        "New name of the button",
                    )
                    .required(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "group",
                        "Group name of the button",
                    )
                    .required(false)
                    .set_autocomplete(true),
                ),
            )
            // Move.
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "move",
                    "Change the group of a soundboard button",
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "sound",
                        "Sound name to move",
                    )
                    .required(true)
                    .set_autocomplete(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "new-group",
                        "New group of the button",
                    )
                    .required(true)
                    .set_autocomplete(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "group",
                        "Group name of the button to modify",
                    )
                    .required(false)
                    .set_autocomplete(true),
                ),
            )
            // Change color.
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "change-color",
                    "Change the color of a soundboard button",
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "sound",
                        "Sound name to change",
                    )
                    .required(true)
                    .set_autocomplete(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "color",
                        "New color of the button",
                    )
                    .required(true)
                    .add_string_choice("blue", button::as_str(ButtonStyle::Primary))
                    .add_string_choice("green", button::as_str(ButtonStyle::Success))
                    .add_string_choice("red", button::as_str(ButtonStyle::Danger))
                    .add_string_choice("grey", button::as_str(ButtonStyle::Secondary)),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "group",
                        "Group name of the button to modify",
                    )
                    .required(false)
                    .set_autocomplete(true),
                ),
            )
            // Change Emoji.
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "change-emoji",
                    "Change the emoji of a soundboard button",
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "sound",
                        "Sound name to change",
                    )
                    .required(true)
                    .set_autocomplete(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "emoji",
                        "New emoji of the button",
                    )
                    .required(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "group",
                        "Group name of the button to modify",
                    )
                    .required(false)
                    .set_autocomplete(true),
                ),
            )
            // ID.
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "id",
                    "Get the ID of a sound",
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "sound",
                        "Sound name to fetch",
                    )
                    .required(true)
                    .set_autocomplete(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "group",
                        "Group name of the sound to fetch",
                    )
                    .required(false)
                    .set_autocomplete(true),
                ),
            )
            // Backup.
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "backup",
                "Download all sounds and metadata as a zip archive",
            ))
            // Logs.
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "logs",
                    "Get latest usage of the soundboard in this channel",
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "duration",
                        "Logs aggregation duration",
                    )
                    .required(false),
                ),
            );
        if self.allow_delete {
            soundboard = soundboard.add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "delete",
                    "Delete a sound from the soundboard",
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "sound",
                        "Sound name to delete",
                    )
                    .required(true)
                    .set_autocomplete(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "group",
                        "Group name of the sound to delete",
                    )
                    .required(false)
                    .set_autocomplete(true),
                ),
            );
        }

        Command::set_global_commands(ctx, vec![version, join_voice_channel, recorder, soundboard])
            .await
            .expect("Global commands creation failure");
        info!("global commands created");
    }
}

async fn find_voice_channel<H: AsRef<Http>, C: AsRef<Cache>>(
    http: H,
    cache: C,
    guild: GuildId,
    user: UserId,
) -> Option<ChannelId> {
    for (id, channel) in guild
        .channels(http)
        .await
        .expect("Failed to fetch channels list")
    {
        if channel.kind == ChannelType::Voice {
            let members = channel
                .members(&cache)
                .expect("Failed to fetch channel members");
            if members.iter().any(|m| m.user.id == user) {
                return Some(id);
            }
        }
    }
    None
}

fn parse_subcommand(command: &CommandInteraction) -> Option<&str> {
    let first_option = command.data.options.first()?;
    if first_option.kind() != CommandOptionType::SubCommand {
        return None;
    };
    Some(&first_option.name)
}

async fn play_sound(
    manager: Arc<Songbird>,
    soundboard: &Soundboard,
    guild: GuildId,
    sound: Ulid,
) -> bool {
    let Some(wav) = soundboard.get_wav(sound).await else {
        return false;
    };

    let Some(call) = manager.get(guild) else {
        return false;
    };
    let mut call_guard = call.lock().await;
    if call_guard.current_channel().is_none() {
        return false;
    }

    call_guard.play_input(Input::from(wav));
    true
}

#[derive(FromRef, Clone)]
struct ApiState {
    http: Arc<Http>,
    cache: Arc<Cache>,
    songbird: Arc<Songbird>,
    recorder: Arc<Mutex<Recorder>>,
    soundboard: Arc<Soundboard>,
    history: Arc<History>,
}

async fn join_channel_http(
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

async fn join_channel_user_http(
    State(http): State<Arc<Http>>,
    State(cache): State<Arc<Cache>>,
    State(songbird): State<Arc<Songbird>>,
    State(recorder): State<Arc<Mutex<Recorder>>>,
    Path((guild, user)): Path<(GuildId, UserId)>,
) -> StatusCode {
    let Some(channel) = find_voice_channel(http, cache, guild, user).await else {
        return StatusCode::NOT_FOUND;
    };
    join_channel_http(State(songbird), State(recorder), Path((guild, channel))).await
}

async fn play_sound_http(
    State(songbird): State<Arc<Songbird>>,
    State(soundboard): State<Arc<Soundboard>>,
    Path((guild, sound)): Path<(GuildId, Ulid)>,
) -> StatusCode {
    if play_sound(songbird, &soundboard, guild, sound).await {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn play_random_sound_http(
    State(songbird): State<Arc<Songbird>>,
    State(soundboard): State<Arc<Soundboard>>,
    Path(guild): Path<GuildId>,
) -> StatusCode {
    let Some(sound) = soundboard.random_id(guild).await else {
        return StatusCode::NOT_FOUND;
    };
    play_sound_http(State(songbird), State(soundboard), Path((guild, sound))).await
}

async fn play_latest_sound_http(
    State(songbird): State<Arc<Songbird>>,
    State(soundboard): State<Arc<Soundboard>>,
    Path(guild): Path<GuildId>,
) -> StatusCode {
    let Some(sound) = soundboard.latest_id(guild).await else {
        return StatusCode::NOT_FOUND;
    };
    play_sound_http(State(songbird), State(soundboard), Path((guild, sound))).await
}

async fn play_last_played_sound_http(
    State(songbird): State<Arc<Songbird>>,
    State(soundboard): State<Arc<Soundboard>>,
    State(history): State<Arc<History>>,
    Path(guild): Path<GuildId>,
) -> StatusCode {
    let Some(sound) = history.get_latest_played(guild, 0).await else {
        return StatusCode::NOT_FOUND;
    };
    play_sound_http(State(songbird), State(soundboard), Path((guild, sound))).await
}

async fn play_last_played_offset_sound_http(
    State(songbird): State<Arc<Songbird>>,
    State(soundboard): State<Arc<Soundboard>>,
    State(history): State<Arc<History>>,
    Path((guild, offset)): Path<(GuildId, usize)>,
) -> StatusCode {
    let Some(sound) = history.get_latest_played(guild, offset).await else {
        return StatusCode::NOT_FOUND;
    };
    play_sound_http(State(songbird), State(soundboard), Path((guild, sound))).await
}

#[tokio::main]
async fn main() -> ExitCode {
    let options = Options::parse();
    Builder::new()
        .filter_level(options.log_level())
        .parse_default_env()
        .init();
    log_panics::init();

    let recorder = Arc::new(Mutex::new(
        Recorder::new(
            options.voice_buffer_duration,
            options.voice_buffer_expiration,
            options.record_whitelist_path,
        )
        .await,
    ));
    Recorder::cleanup_loop(recorder.clone());

    let soundboard = Arc::new(
        Soundboard::new(
            options.soundboard_metadata_path,
            options.sounds_dir_path,
            options.sound_max_duration,
            options.sound_cache_duration,
            options.ffmpeg_path,
        )
        .await,
    );
    Arc::clone(&soundboard).cache_loop();

    let history = Arc::new(History::default());

    let intents = GatewayIntents::GUILDS
        | GatewayIntents::GUILD_PRESENCES
        | GatewayIntents::GUILD_VOICE_STATES;
    let songbird =
        Songbird::serenity_from_config(songbird::Config::default().decode_mode(DecodeMode::Decode));
    let mut client = Client::builder(options.discord_token, intents)
        .event_handler(Handler {
            bot_id: Arc::new(AtomicU64::new(0)),
            allow_delete: !options.disable_delete,
            allow_grey: options.allow_grey,
            recorder: Arc::clone(&recorder),
            soundboard: Arc::clone(&soundboard),
            history: Arc::clone(&history),
        })
        .register_songbird_with(Arc::clone(&songbird))
        .await
        .expect("Error creating client");

    let server = Server::bind(&SocketAddr::new(
        options.soundboard_http_address,
        options.soundboard_http_port,
    ))
    .serve(
        Router::new()
            .route(
                "/guilds/:guild/channels/:channel/join",
                routing::post(join_channel_http),
            )
            .route(
                "/guilds/:guild/users/:user/follow",
                routing::post(join_channel_user_http),
            )
            .route(
                "/guilds/:guild/sounds/:sound/play",
                routing::post(play_sound_http),
            )
            .route(
                "/guilds/:guild/sounds/random/play",
                routing::post(play_random_sound_http),
            )
            .route(
                "/guilds/:guild/sounds/latest/play",
                routing::post(play_latest_sound_http),
            )
            .route(
                "/guilds/:guild/sounds/last-played/play",
                routing::post(play_last_played_sound_http),
            )
            .route(
                "/guilds/:guild/sounds/last-played/:offset/play",
                routing::post(play_last_played_offset_sound_http),
            )
            .with_state(ApiState {
                http: Arc::clone(&client.http),
                cache: Arc::clone(&client.cache),
                songbird,
                recorder,
                soundboard,
                history,
            })
            .into_make_service(),
    );

    info!("starting disrecord bot");
    tokio::select! {
        err = client.start() => {
            error!("bot starting error: {}", err.unwrap_err());
        },
        err = server => {
            error!("http endpoint error: {}", err.unwrap_err());
        }
    }
    ExitCode::FAILURE
}
