use std::{
    borrow::Cow,
    collections::{HashSet, VecDeque},
    process::ExitCode,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
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
                application_command::{ApplicationCommandInteraction, CommandDataOptionValue},
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

use crate::{
    options::Options,
    recorder::{Action, Recorder},
    soundboard::Soundboard,
};

mod options;
mod recorder;
mod soundboard;
mod wav;

/// Max body size is 25MiB including other fields. We cut at 24MiB because calculating the rest of
/// the body is too unreliable.
const MAX_FILE_SIZE: usize = 24 * (1 << 20);

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
        create_global_commands(&ctx).await;

        let message = ctx
            .cache
            .guild_channel(658770754984345603)
            .unwrap()
            .send_message(&ctx, |message| {
                message.components(|components| {
                    components.create_action_row(|row| {
                        row.create_button(|button| {
                            button
                                .custom_id("id_0")
                                .label("Sound 1")
                                .emoji(ReactionType::from('💦'))
                        })
                    })
                })
            })
            .await
            .unwrap();
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
            "help" => self.help(ctx, command).await,
            "version" => self.version(ctx, command).await,
            "download" => self.download(ctx, command).await,
            "list" => self.list(ctx, command).await,
            "join" => self.join(ctx, command).await,
            "listen" => self.listen(ctx, command).await,
            "leave" => self.leave(ctx, command).await,
            "upload" => self.upload(ctx, command).await,
            _ => (),
        };
    }

    async fn dispatch_component(&self, ctx: Context, component: MessageComponentInteraction) {
        // TODO: Use futures::join;
        dbg!(&component.data.custom_id);
        component.defer(&ctx).await.unwrap();

        let wav = self
            .soundboard
            .get_wav(Ulid::from_string(&component.data.custom_id).expect("Invalid sound id"))
            .await
            .expect("Cannot find sound");

        let manager = songbird::get(&ctx)
            .await
            .expect("Failed to get songbird manager");
        let handler_lock = manager.get(component.guild_id.unwrap()).unwrap();
        handler_lock.lock().await.play_source(Input::new(
            false,
            Reader::from_memory(wav[wav::HEADER_SIZE..].to_vec()),
            Codec::Pcm,
            Container::Raw,
            None,
        ));
    }

    async fn help(&self, ctx: Context, command: ApplicationCommandInteraction) {
        command
            .create_interaction_response(&ctx, |response| {
                response
                    .kind(InteractionResponseType::ChannelMessageWithSource)
                    .interaction_response_data(|message| {
                        message.content("Use Audacity to load and cut parts of the recordings.")
                    })
            })
            .await
            .expect("Help response failure");
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

    async fn list(&self, ctx: Context, command: ApplicationCommandInteraction) {
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
                            list.into_iter().map(|user| Mention::from(user)).join(", ")
                        })
                    })
            })
            .await
            .expect("Cannot send whitelist");
    }

    async fn join(&self, ctx: Context, command: ApplicationCommandInteraction) {
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

    async fn leave(&self, ctx: Context, command: ApplicationCommandInteraction) {
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

    async fn listen(&self, ctx: Context, command: ApplicationCommandInteraction) {
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
                    .interaction_response_data(|message| message.content("Listening..."))
            })
            .await
            .expect("Cannot send listen message");
    }

    async fn download(&self, ctx: Context, command: ApplicationCommandInteraction) {
        let Some(requested_user) = command.data.options.get(0).and_then(|option| {
            option.resolved.as_ref()
        }).and_then(|arg| {
            match arg {
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
        match data.map(|data| Vec::from(data)) {
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

    async fn upload(&self, ctx: Context, command: ApplicationCommandInteraction) {
        let Some(guild) = command.guild_id else {
            return;
        };
        let Some(sound) = find_option(&command, "sound").and_then(|opt| {
            match opt {
                CommandDataOptionValue::Attachment(att) => Some(att),
                _ => None,
            }
        }) else {
            return;
        };
        let Some(name) = find_option(&command, "name").and_then(|opt| {
            match opt {
                CommandDataOptionValue::String(s) => Some(s),
                _ => None,
            }
        }) else {
            return;
        };
        let Some(color) = find_option(&command, "color").and_then(|opt| {
            match opt {
                CommandDataOptionValue::String(s) => Some(s),
                _ => None,
            }
        }) else {
            return;
        };
        let Some(group) = find_option(&command, "group").and_then(|opt| {
            match opt {
                CommandDataOptionValue::String(s) => Some(s),
                _ => None,
            }
        }) else {
            return;
        };
        let emoji = find_option(&command, "emoji").and_then(|opt| match opt {
            CommandDataOptionValue::String(s) => s.chars().next(),
            _ => None,
        });
        let index = find_option(&command, "index").and_then(|opt| match opt {
            CommandDataOptionValue::Integer(n) => Some(*n as usize),
            _ => None,
        });

        dbg!(sound, name, color, group, emoji, index);
        let id = self
            .soundboard
            .add(
                &sound.url,
                guild,
                name.clone(),
                emoji,
                ButtonStyle::Primary,
                group.clone(),
            )
            .await
            .expect("Sound saving failed");

        command
            .create_interaction_response(&ctx, |response| {
                response
                    .kind(InteractionResponseType::ChannelMessageWithSource)
                    .interaction_response_data(|message| {
                        message.components(|components| {
                            components.create_action_row(|row| {
                                row.create_button(|button| {
                                    button.custom_id(id.to_string()).label(name);
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
            .expect("Failed to create button");
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

        let manager = songbird::get(&ctx).await.expect("Cannot get voice manager");
        manager
            .leave(channel.guild_id)
            .await
            .expect("Voice disconnection failure");
    }
}

async fn create_global_commands(ctx: &Context) {
    info!("creating global commands");
    Command::set_global_application_commands(ctx, |builder| {
        // Help.
        builder.create_application_command(|command| {
            command
                .kind(CommandType::ChatInput)
                .name("help")
                .description("Display help")
        });

        // Version.
        builder.create_application_command(|command| {
            command
                .kind(CommandType::ChatInput)
                .name("version")
                .description("Display help")
        });

        // Get whitelist.
        builder.create_application_command(|command| {
            command
                .kind(CommandType::ChatInput)
                .name("list")
                .description("List recordable users")
        });

        // Join whitelist.
        builder.create_application_command(|command| {
            command
                .kind(CommandType::ChatInput)
                .name("join")
                .description("Join recordable users list")
        });

        // Leave whitelist.
        builder.create_application_command(|command| {
            command
                .kind(CommandType::ChatInput)
                .name("leave")
                .description("Leave recordable users list")
        });

        // Listen.
        builder.create_application_command(|command| {
            command
                .kind(CommandType::ChatInput)
                .name("listen")
                .description("Join your voice channel")
        });

        // Download.
        builder.create_application_command(|command| {
            command
                .kind(CommandType::ChatInput)
                .name("download")
                .description("Download a user's voice data")
                .create_option(|option| {
                    option
                        .kind(CommandOptionType::User)
                        .name("user")
                        .description("User to download data for")
                        .required(true)
                })
        });

        // Upload.
        builder.create_application_command(|command| {
            command
                .kind(CommandType::ChatInput)
                .name("upload")
                .description("Upload sound")
                .create_option(|option| {
                    option
                        .kind(CommandOptionType::Attachment)
                        .name("sound")
                        .description("WAV sound file")
                        .required(true)
                })
                .create_option(|option| {
                    option
                        .kind(CommandOptionType::String)
                        .name("name")
                        .description("The name of the sound that will appear on the button")
                        .required(true)
                })
                .create_option(|option| {
                    option
                        .kind(CommandOptionType::String)
                        .name("color")
                        .add_string_choice("blue", "blue")
                        .add_string_choice("green", "green")
                        .add_string_choice("red", "red")
                        .add_string_choice("grey", "grey")
                        .description("Color of the button")
                        .required(true)
                })
                .create_option(|option| {
                    option
                        .kind(CommandOptionType::String)
                        .name("group")
                        .description("The group to add this sound to")
                        .required(true)
                })
                .create_option(|option| {
                    option
                        .kind(CommandOptionType::String)
                        .name("emoji")
                        .description("The emoji to prepend to the button")
                        .required(false)
                })
                .create_option(|option| {
                    option
                        .kind(CommandOptionType::Integer)
                        .name("index")
                        .description("The position of the sound in its group")
                        .required(false)
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

fn find_option<'a>(
    command: &'a ApplicationCommandInteraction,
    name: &str,
) -> Option<&'a CommandDataOptionValue> {
    command
        .data
        .options
        .iter()
        .find(|opt| opt.name == name)
        .map(|opt| opt.resolved.as_ref())
        .flatten()
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
        options.whitelist_path,
    )
    .await;
    let tx = recorder.run_loop();

    let soundboard = Soundboard::new(
        "soundboard".into(),
        ".".into(),
        Duration::from_secs(10),
        Duration::from_secs(3 * 60),
    )
    .await;

    let intents = GatewayIntents::all();
    let mut client = Client::builder(options.discord_token, intents)
        .event_handler(Handler {
            bot_id: Arc::new(AtomicU64::new(0)),
            recorder_actions_tx: tx,
            soundboard: Arc::new(soundboard),
        })
        .register_songbird_from_config(songbird::Config::default().decode_mode(DecodeMode::Decode))
        .await
        .expect("Error creating client");

    info!("disrecord bot started");
    error!("bot starting error: {}", client.start().await.unwrap_err());
    ExitCode::FAILURE
}
