use std::{
    borrow::Cow,
    collections::{HashSet, VecDeque},
    io::{Cursor, Write},
    process::ExitCode,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use clap::Parser;
use env_logger::Builder;
use itertools::Itertools;
use log::{error, info};
use serenity::{
    async_trait,
    client::{Context, EventHandler},
    model::{
        application::{
            command::{Command, CommandOptionType, CommandType},
            component::ButtonStyle,
            interaction::{
                application_command::{
                    ApplicationCommandInteraction, CommandDataOption, CommandDataOptionValue,
                },
                autocomplete::AutocompleteInteraction,
                message_component::MessageComponentInteraction,
                Interaction, InteractionResponseType,
            },
        },
        channel::{AttachmentType, ChannelType, ReactionType},
        gateway::Ready,
        id::{ChannelId, GuildId, UserId},
        mention::Mention,
        prelude::VoiceState,
    },
    prelude::GatewayIntents,
    Client,
};
use songbird::{
    driver::DecodeMode,
    input::{Codec, Container, Input, Reader},
    CoreEvent, Event, EventContext, EventHandler as VoiceEventHandler, SerenityInit,
};
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use ulid::Ulid;
use zip::{write::FileOptions as ZipFileOptions, ZipWriter};

use crate::{
    options::Options,
    recorder::{Action, Recorder},
    soundboard::Soundboard,
};

mod button;
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

#[derive(Clone)]
struct Handler {
    bot_id: Arc<AtomicU64>,
    recorder_actions_tx: UnboundedSender<Action>,
    soundboard: Arc<Soundboard>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, data_about_bot: Ready) {
        info!("bot ready");
        self.bot_id
            .store(*data_about_bot.user.id.as_u64(), Ordering::Relaxed);
        register_global_commands(&ctx).await;
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
            Interaction::ApplicationCommand(command) => self.dispatch_command(ctx, command).await,
            Interaction::MessageComponent(component) => {
                self.dispatch_component(ctx, component).await
            }
            Interaction::Autocomplete(autocomplete) => {
                self.dispatch_autocomplete(ctx, autocomplete).await
            }
            _ => return,
        }
    }
}

#[async_trait]
impl VoiceEventHandler for Handler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::SpeakingStateUpdate(event) => {
                if let Some(user) = event.user_id {
                    self.recorder_actions_tx
                        .send(Action::MapUser(UserId(user.0), event.ssrc))
                        .expect("Event dispatch error");
                }
            }
            EventContext::VoicePacket(packet) => {
                if let Some(audio) = packet.audio {
                    self.recorder_actions_tx
                        .send(Action::RegisterVoiceData(
                            packet.packet.ssrc,
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
    async fn dispatch_command(&self, ctx: Context, command: ApplicationCommandInteraction) {
        match command.data.name.as_str() {
            // Common.
            "version" => self.version(ctx, command).await,
            "help" => self.help(ctx, command).await,
            "join" => self.join_channel(ctx, command).await,

            // Recorder.
            "recorder" => match parse_subcommand(&command) {
                Some("list") => self.get_whitelist(ctx, command).await,
                Some("join") => self.join_whitelist(ctx, command).await,
                Some("leave") => self.leave_whitelist(ctx, command).await,
                Some("download") => self.download_recording(ctx, command).await,
                _ => (),
            },

            // Soundboard.
            "soundboard" => match parse_subcommand(&command) {
                Some("list") => self.list_sounds(ctx, command).await,
                Some("upload") => self.upload_sound(ctx, command).await,
                Some("download") => self.download_sound(ctx, command).await,
                Some("delete") => self.delete_sound(ctx, command).await,
                Some("backup") => self.backup_sounds(ctx, command).await,
                _ => (),
            },
            _ => (),
        };
    }

    async fn dispatch_component(&self, ctx: Context, component: MessageComponentInteraction) {
        let Some(guild) = component.guild_id else {
            return
        };

        let play_future = async {
            let Some(mut data) = self
                .soundboard
                .get_wav(Ulid::from_string(&component.data.custom_id).expect("Invalid sound id"))
                .await else {
                return;
            };

            let manager = songbird::get(&ctx)
                .await
                .expect("Failed to get songbird manager");
            let Some(call) = manager.get(guild) else {
                return;
            };

            wav::remove_header(&mut data);
            call.lock().await.play_source(Input::new(
                false,
                Reader::from_memory(data),
                Codec::Pcm,
                Container::Raw,
                None,
            ));
        };

        let (defer, _play) = tokio::join!(component.defer(&ctx), play_future);
        defer.expect("Failed to defer sound play");
    }

    async fn dispatch_autocomplete(&self, ctx: Context, autocomplete: AutocompleteInteraction) {
        let Some(guild) = autocomplete.guild_id else {
            return
        };

        let matches = match find_autocompleting(&autocomplete.data.options) {
            Some(("sound", CommandDataOptionValue::String(search))) => {
                self.soundboard
                    .names_matching(guild, search, AUTOCOMPLETE_MAX_CHOICES)
                    .await
            }
            Some(("group", CommandDataOptionValue::String(search))) => {
                self.soundboard
                    .groups_matching(guild, search, AUTOCOMPLETE_MAX_CHOICES)
                    .await
            }
            _ => return,
        };

        autocomplete
            .create_autocomplete_response(&ctx, |response| {
                for m in matches {
                    response.add_string_choice(&m, &m);
                }
                response
            })
            .await
            .expect("Failed to send autocomplete response");
    }

    async fn version(&self, ctx: Context, command: ApplicationCommandInteraction) {
        command
            .create_interaction_response(&ctx, |response| {
                response
                    .kind(InteractionResponseType::ChannelMessageWithSource)
                    .interaction_response_data(|message| message.content(env!("CARGO_PKG_VERSION")))
            })
            .await
            .expect("Version response failure");
    }

    async fn help(&self, ctx: Context, command: ApplicationCommandInteraction) {
        command
            .create_interaction_response(&ctx, |response| {
                response
                    .kind(InteractionResponseType::ChannelMessageWithSource)
                    .interaction_response_data(|message| {
                        message.content("Use **Audacity** to load and cut parts of the recordings. Expected format is WAV, mono / 48kHZ / 16bit signed")
                    })
            })
            .await
            .expect("Help response failure");
    }

    async fn get_whitelist(&self, ctx: Context, command: ApplicationCommandInteraction) {
        let Some(guild) = command.guild_id else {
            return
        };

        let (tx, rx) = oneshot::channel();
        self.recorder_actions_tx
            .send(Action::GetWhitelist(tx))
            .expect("List request failure");

        let list = rx
            .await
            .expect("List fetching failure")
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
            .create_interaction_response(&ctx, |response| {
                response
                    .kind(InteractionResponseType::ChannelMessageWithSource)
                    .interaction_response_data(|message| {
                        message.content(if list.is_empty() {
                            "*Nobody.*".to_owned()
                        } else {
                            list.into_iter().map(Mention::from).join(", ")
                        })
                    })
            })
            .await
            .expect("Cannot send whitelist");
    }

    async fn join_whitelist(&self, ctx: Context, command: ApplicationCommandInteraction) {
        self.recorder_actions_tx
            .send(Action::AddToWhitelist(command.user.id))
            .expect("Adding to whitelist failed");

        command
            .create_interaction_response(&ctx, |response| {
                response
                    .kind(InteractionResponseType::ChannelMessageWithSource)
                    .interaction_response_data(|message| {
                        message.content("You are now in the whitelist.")
                    })
            })
            .await
            .expect("Adding to whitelist failed");
    }

    async fn leave_whitelist(&self, ctx: Context, command: ApplicationCommandInteraction) {
        self.recorder_actions_tx
            .send(Action::RemoveFromWhitelist(command.user.id))
            .expect("Leaving whitelist failed");

        command
            .create_interaction_response(&ctx, |response| {
                response
                    .kind(InteractionResponseType::ChannelMessageWithSource)
                    .interaction_response_data(|message| {
                        message.content("You have been removed from the whitelist.")
                    })
            })
            .await
            .expect("Leaving whitelist failed");
    }

    async fn join_channel(&self, ctx: Context, command: ApplicationCommandInteraction) {
        let Some(guild) = command.guild_id else {
            return
        };
        let channel = match find_voice_channel(&ctx, guild, command.user.id).await {
            Some(channel) => channel,
            None => {
                command
                    .create_interaction_response(&ctx, |response| {
                        response
                            .kind(InteractionResponseType::ChannelMessageWithSource)
                            .interaction_response_data(|message| {
                                message.content("You aren't in a voice channel. Dahhh...")
                            })
                    })
                    .await
                    .expect("Cannot find voice channel");
                return;
            }
        };

        let manager = songbird::get(&ctx)
            .await
            .expect("Failed to get songbird manager");
        let (handler_lock, conn_result) = manager.join(guild, channel).await;
        conn_result.expect("Voice connexion failure");

        {
            let mut handler = handler_lock.lock().await;
            handler.remove_all_global_events();
            handler.add_global_event(Event::Core(CoreEvent::SpeakingStateUpdate), self.clone());
            handler.add_global_event(Event::Core(CoreEvent::VoicePacket), self.clone());
        }

        command
            .create_interaction_response(&ctx, |response| {
                response
                    .kind(InteractionResponseType::ChannelMessageWithSource)
                    .interaction_response_data(|message| {
                        message.content("Listening and ready to play sounds...")
                    })
            })
            .await
            .expect("Cannot send listen message");
    }

    async fn download_recording(&self, ctx: Context, command: ApplicationCommandInteraction) {
        let Some(requested_user) = find_option(&command, "user", false).and_then(|opt| {
            match opt {
                CommandDataOptionValue::User(user, _) => Some(user),
                _ => None,
            }
        }) else {
            return;
        };

        let (tx, rx) = oneshot::channel::<Option<VecDeque<i16>>>();
        self.recorder_actions_tx
            .send(Action::GetData(requested_user.id, tx))
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
                        format!("{}.wav", requested_user.name)
                    } else {
                        format!("{}-{}.wav", requested_user.name, i + 1)
                    };

                    command
                        .create_followup_message(&ctx, |response| {
                            response.add_file(AttachmentType::Bytes {
                                data: Cow::from(wav::package(chunk)),
                                filename,
                            })
                        })
                        .await
                        .expect("Voice data transmission failure");
                }
            }
            None => {
                command
                    .create_interaction_response(&ctx, |response| {
                        response
                            .kind(InteractionResponseType::ChannelMessageWithSource)
                            .interaction_response_data(|message| {
                                message
                                    .content(format!("No voice data found for {}.", requested_user))
                            })
                    })
                    .await
                    .expect("Download response failure");
            }
        }
    }

    async fn list_sounds(&self, ctx: Context, command: ApplicationCommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };

        let sounds = self.soundboard.list(guild).await;
        if sounds.is_empty() {
            command
                .create_interaction_response(&ctx, |response| {
                    response
                        .kind(InteractionResponseType::ChannelMessageWithSource)
                        .interaction_response_data(|message| {
                            message.content("There is no sounds uploaded to this server... yet.")
                        })
                })
                .await
                .expect("Cannot send empty soundboard message");
            return;
        }

        command
            .defer(&ctx)
            .await
            .expect("Failed to defer sound list");
        command
            .delete_original_interaction_response(&ctx)
            .await
            .expect("Failed to delete original sound list interaction");
        for (group, sounds) in sounds {
            // Send the group name once then at most 4 sound rows per message.
            for (message_index, sounds_message) in sounds
                .chunks((ROWS_PER_MESSAGE - 1) * SOUNDS_PER_ROW)
                .enumerate()
            {
                command
                    .channel_id
                    .send_message(&ctx, |message| {
                        message.components(|components| {
                            if message_index == 0 {
                                components.create_action_row(|row| {
                                    row.create_select_menu(|menu| {
                                        menu.custom_id("group").options(|menu_options| {
                                            menu_options.create_option(|option| {
                                                option
                                                    .label(&group)
                                                    .value(&group)
                                                    .default_selection(true)
                                            })
                                        })
                                    })
                                });
                            }
                            for sounds_row in sounds_message.chunks(SOUNDS_PER_ROW) {
                                components.create_action_row(|row| {
                                    for sound in sounds_row {
                                        row.create_button(|button| {
                                            button
                                                .custom_id(sound.id.to_string())
                                                .style(sound.color)
                                                .label(&sound.name);
                                            if let Some(emoji) = sound.emoji {
                                                button.emoji(ReactionType::from(emoji));
                                            }
                                            button
                                        });
                                    }
                                    row
                                });
                            }
                            components
                        })
                    })
                    .await
                    .expect("Failed to send sounds list");
            }
        }
    }

    async fn upload_sound(&self, ctx: Context, command: ApplicationCommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };
        let Some(attachment) = find_option(&command, "sound", false).and_then(|opt| {
            match opt {
                CommandDataOptionValue::Attachment(att) => Some(att),
                _ => None,
            }
        }) else {
            return;
        };
        let Some(name) = find_option(&command, "name", false).and_then(|opt| {
            match opt {
                CommandDataOptionValue::String(s) => Some(s),
                _ => None,
            }
        }) else {
            return;
        };
        let Some(group) = find_option(&command, "group", false).and_then(|opt| {
            match opt {
                CommandDataOptionValue::String(s) => Some(s),
                _ => None,
            }
        }) else {
            return;
        };
        let emoji = find_option(&command, "emoji", false).and_then(|opt| match opt {
            CommandDataOptionValue::String(s) => s.chars().next(),
            _ => None,
        });
        let color = find_option(&command, "color", false)
            .and_then(|opt| match opt {
                CommandDataOptionValue::String(s) => Some(button::parse_color(s)),
                _ => None,
            })
            .unwrap_or_else(|| button::determinist(&name.to_lowercase()));
        let index = find_option(&command, "position", false).and_then(|opt| match opt {
            CommandDataOptionValue::Integer(n) => Some((*n - 1) as usize),
            _ => None,
        });

        match self
            .soundboard
            .add(
                attachment,
                guild,
                name.clone(),
                emoji,
                color,
                group.clone(),
                index,
            )
            .await
        {
            Ok(id) => {
                command
                    .create_interaction_response(&ctx, |response| {
                        response
                            .kind(InteractionResponseType::ChannelMessageWithSource)
                            .interaction_response_data(|message| {
                                message.components(|components| {
                                    components.create_action_row(|row| {
                                        row.create_button(|button| {
                                            button
                                                .custom_id(id.to_string())
                                                .label(name)
                                                .style(color);
                                            if let Some(emoji) = emoji {
                                                button.emoji(ReactionType::from(emoji));
                                            }
                                            button
                                        })
                                    })
                                })
                            })
                    })
                    .await
                    .expect("Failed to create sound button");
            }
            Err(err) => {
                command
                    .create_interaction_response(&ctx, |response| {
                        response
                            .kind(InteractionResponseType::ChannelMessageWithSource)
                            .interaction_response_data(|message| message.content(err.to_string()))
                    })
                    .await
                    .expect("Cannot send sound creation error message");
            }
        }
    }

    async fn download_sound(&self, ctx: Context, command: ApplicationCommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };
        let Some(name) = find_option(&command, "sound", false).and_then(|opt| {
            match opt {
                CommandDataOptionValue::String(s) => Some(s),
                _ => None,
            }
        }) else {
            return;
        };

        match self.soundboard.get_wav_by_name(name, guild).await {
            Some(data) => {
                command.defer(&ctx).await.expect("Download defer failed");
                // Does not support splitting.
                command
                    .create_followup_message(&ctx, |response| {
                        response.add_file(AttachmentType::Bytes {
                            data: Cow::from(data),
                            filename: format!("{name}.wav"),
                        })
                    })
                    .await
                    .expect("Sound data transmission failure");
            }
            None => {
                command
                    .create_interaction_response(&ctx, |response| {
                        response
                            .kind(InteractionResponseType::ChannelMessageWithSource)
                            .interaction_response_data(|message| {
                                message.content("Sound not found.")
                            })
                    })
                    .await
                    .expect("Download response failure");
            }
        }
    }

    async fn delete_sound(&self, ctx: Context, command: ApplicationCommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };
        let Some(name) = find_option(&command, "sound", false).and_then(|opt| {
            match opt {
                CommandDataOptionValue::String(s) => Some(s),
                _ => None,
            }
        }) else {
            return;
        };

        let text = match self.soundboard.delete(guild, name).await {
            Ok(()) => "Deleted. *(for ever)*".to_owned(),
            Err(err) => err.to_string(),
        };
        command
            .create_interaction_response(&ctx, |response| {
                response
                    .kind(InteractionResponseType::ChannelMessageWithSource)
                    .interaction_response_data(|message| message.content(text))
            })
            .await
            .expect("Cannot send sound creation error message");
    }

    async fn backup_sounds(&self, ctx: Context, command: ApplicationCommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };

        match self.soundboard.backup(guild).await {
            Ok((metadata, sounds)) => {
                if sounds.is_empty() {
                    command
                        .create_interaction_response(&ctx, |response| {
                            response
                                .kind(InteractionResponseType::ChannelMessageWithSource)
                                .interaction_response_data(|message| {
                                    message.content("There is no sounds on this server.")
                                })
                        })
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
                        .create_followup_message(&ctx, |response| {
                            if too_large > 0 {
                                response.content("{too_large} files were too large and weren't included in the backup.");
                            }
                            response.add_file(AttachmentType::Bytes {
                                data: Cow::from(archive),
                                filename: "backup.zip".to_owned(),
                            })
                        })
                        .await
                        .expect("Backup response failure");
                }
            }
            Err(err) => {
                command
                    .create_interaction_response(&ctx, |response| {
                        response
                            .kind(InteractionResponseType::ChannelMessageWithSource)
                            .interaction_response_data(|message| message.content(err.to_string()))
                    })
                    .await
                    .expect("Backup response failure");
            }
        }
    }

    async fn disconnect_if_alone(&self, ctx: &Context, channel: ChannelId) {
        let Some(channel) = ctx.cache.guild_channel(channel) else {
            return;
        };
        if channel.kind != ChannelType::Voice {
            return;
        }

        let members = channel
            .members(&ctx)
            .await
            .expect("Cannot fetch member list");
        if !(members.len() == 1 && members[0].user.id == self.bot_id.load(Ordering::Relaxed)) {
            return;
        }

        let manager = songbird::get(ctx).await.expect("Cannot get voice manager");
        manager
            .leave(channel.guild_id)
            .await
            .expect("Voice disconnection failure");
    }
}

async fn register_global_commands(ctx: &Context) {
    info!("creating global commands");
    Command::set_global_application_commands(ctx, |builder| {
        // Version.
        builder.create_application_command(|command| {
            command
                .kind(CommandType::ChatInput)
                .name("version")
                .description("Display help")
        });

        // Help.
        builder.create_application_command(|command| {
            command
                .kind(CommandType::ChatInput)
                .name("help")
                .description("Display help")
        });

        // Join voice channel.
        builder.create_application_command(|command| {
            command
                .kind(CommandType::ChatInput)
                .name("join")
                .description("Join your voice channel")
        });

        // Recorder.
        builder.create_application_command(|command| {
            command
                .kind(CommandType::ChatInput)
                .name("recorder")
                .description("Manage the recorder whitelist and download recordings");

            // List.
            command.create_option(|subcommand| {
                subcommand
                    .kind(CommandOptionType::SubCommand)
                    .name("list")
                    .description("Get recorder's whitelist")
            });

            // Join whitelist.
            command.create_option(|subcommand| {
                subcommand
                    .kind(CommandOptionType::SubCommand)
                    .name("join")
                    .description("Join recorder whitelist")
            });

            // Leave whitelist.
            command.create_option(|subcommand| {
                subcommand
                    .kind(CommandOptionType::SubCommand)
                    .name("leave")
                    .description("Leave recorder whitelist")
            });

            // Download recording.
            command.create_option(|subcommand| {
                subcommand
                    .kind(CommandOptionType::SubCommand)
                    .name("download")
                    .description("Download a user's recording")
                    .create_sub_option(|option| {
                        option
                            .kind(CommandOptionType::User)
                            .name("user")
                            .description("User to download data for")
                            .required(true)
                    })
            })
        });

        // Soundboard.
        builder.create_application_command(|command| {
            command
                .kind(CommandType::ChatInput)
                .name("soundboard")
                .description("Add, delete or download sounds to/from the soundboard");

            // List.
            command.create_option(|subcommand| {
                subcommand
                    .kind(CommandOptionType::SubCommand)
                    .name("list")
                    .description("List all sounds available on this server")
            });

            // Upload.
            command.create_option(|subcommand| {
                subcommand
                    .kind(CommandOptionType::SubCommand)
                    .name("upload")
                    .description("Upload a sound")
                    .create_sub_option(|option| {
                        option
                            .kind(CommandOptionType::Attachment)
                            .name("sound")
                            .description("WAV sound file")
                            .required(true)
                    })
                    .create_sub_option(|option| {
                        option
                            .kind(CommandOptionType::String)
                            .name("name")
                            .description("The name of the sound that will appear on the button")
                            .required(true)
                    })
                    .create_sub_option(|option| {
                        option
                            .kind(CommandOptionType::String)
                            .name("group")
                            .description("The group to add this sound to")
                            .required(true)
                            .set_autocomplete(true)
                    })
                    .create_sub_option(|option| {
                        option
                            .kind(CommandOptionType::String)
                            .name("emoji")
                            .description("The emoji to prepend to the button")
                            .required(false)
                    })
                    .create_sub_option(|option| {
                        option
                            .kind(CommandOptionType::String)
                            .name("color")
                            .description("Color of the button")
                            .required(false)
                            .add_string_choice("blue", button::as_str(ButtonStyle::Primary))
                            .add_string_choice("green", button::as_str(ButtonStyle::Success))
                            .add_string_choice("red", button::as_str(ButtonStyle::Danger))
                            .add_string_choice("grey", button::as_str(ButtonStyle::Secondary))
                    })
                    .create_sub_option(|option| {
                        option
                            .kind(CommandOptionType::Integer)
                            .name("position")
                            .description("The position of the sound in its group")
                            .required(false)
                            .min_int_value(1)
                    })
            });

            // Download.
            command.create_option(|subcommand| {
                subcommand
                    .kind(CommandOptionType::SubCommand)
                    .name("download")
                    .description("Download a sound")
                    .create_sub_option(|option| {
                        option
                            .kind(CommandOptionType::String)
                            .name("sound")
                            .description("Sound name to download")
                            .required(true)
                            .set_autocomplete(true)
                    })
            });

            // Delete.
            command.create_option(|subcommand| {
                subcommand
                    .kind(CommandOptionType::SubCommand)
                    .name("delete")
                    .description("Delete a sound from the soundboard")
                    .create_sub_option(|option| {
                        option
                            .kind(CommandOptionType::String)
                            .name("sound")
                            .description("Sound name to delete")
                            .required(true)
                            .set_autocomplete(true)
                    })
            });

            // Backup.
            command.create_option(|subcommand| {
                subcommand
                    .kind(CommandOptionType::SubCommand)
                    .name("backup")
                    .description("Download all sounds and metadata as a zip archive")
            })
        })
    })
    .await
    .expect("Global commands creation failure");
    info!("global commands created");
}

async fn find_voice_channel(ctx: &Context, guild: GuildId, user: UserId) -> Option<ChannelId> {
    for (id, channel) in guild
        .channels(&ctx.http)
        .await
        .expect("Failed to fetch channels list")
    {
        if channel.kind == ChannelType::Voice {
            let members = channel
                .members(ctx)
                .await
                .expect("Failed to fetch channel members");
            if members.iter().any(|m| m.user.id == user) {
                return Some(id);
            }
        }
    }
    None
}

fn parse_subcommand(command: &ApplicationCommandInteraction) -> Option<&str> {
    let first_option = command.data.options.first()?;
    if first_option.kind != CommandOptionType::SubCommand {
        return None;
    };
    Some(&first_option.name)
}

/// Only check for a depth of 1 if `top_level` if set to false.
fn find_option<'a>(
    command: &'a ApplicationCommandInteraction,
    name: &str,
    top_level: bool,
) -> Option<&'a CommandDataOptionValue> {
    let options = if top_level {
        &command.data.options
    } else {
        &command.data.options.first()?.options
    };
    options
        .iter()
        .find(|opt| opt.name == name)
        .and_then(|opt| opt.resolved.as_ref())
}

fn find_autocompleting(options: &[CommandDataOption]) -> Option<(&str, &CommandDataOptionValue)> {
    options.iter().find_map(|option| {
        if option.focused {
            option.resolved.as_ref().map(|v| (option.name.as_str(), v))
        } else {
            find_autocompleting(&option.options)
        }
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let options = Options::parse();
    Builder::new()
        .filter_level(options.log_level())
        .parse_default_env()
        .init();
    log_panics::init();

    let recorder = Recorder::new(
        options.voice_buffer_duration,
        options.voice_buffer_expiration,
        options.record_whitelist_path,
    )
    .await;
    let tx = recorder.run_loop();

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

    let intents = GatewayIntents::all();
    let mut client = Client::builder(options.discord_token, intents)
        .event_handler(Handler {
            bot_id: Arc::new(AtomicU64::new(0)),
            recorder_actions_tx: tx,
            soundboard,
        })
        .register_songbird_from_config(songbird::Config::default().decode_mode(DecodeMode::Decode))
        .await
        .expect("Error creating client");

    info!("disrecord bot started");
    error!("bot starting error: {}", client.start().await.unwrap_err());
    ExitCode::FAILURE
}
