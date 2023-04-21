use std::{
    borrow::Cow,
    collections::{HashSet, VecDeque},
    env,
    io::Cursor,
    mem,
    path::PathBuf,
    time::Duration,
};

use itertools::Itertools;
use serenity::{
    async_trait,
    client::{Context, EventHandler},
    model::{
        application::{
            command::{Command, CommandOptionType, CommandType},
            interaction::{
                application_command::{ApplicationCommandInteraction, CommandDataOptionValue},
                Interaction, InteractionResponseType,
            },
        },
        channel::{AttachmentType, ChannelType},
        gateway::Ready,
        id::{ChannelId, GuildId, UserId},
        mention::Mention,
    },
    prelude::GatewayIntents,
    Client,
};
use songbird::{
    driver::DecodeMode, CoreEvent, Event, EventContext, EventHandler as VoiceEventHandler,
    SerenityInit,
};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

use crate::storage::{Action, Storage};

mod storage;

#[derive(Clone)]
struct Handler(UnboundedSender<Action>);

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, _data_about_bot: Ready) {
        create_global_commands(&ctx).await;
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Interaction::ApplicationCommand(command) = interaction else {
            return;
        };

        match command.data.name.as_str() {
            "download" => self.download(ctx, command).await,
            "list" => self.list(ctx, command).await,
            "join" => self.join(ctx, command).await,
            "listen" => self.listen(ctx, command).await,
            "leave" => self.leave(ctx, command).await,
            "help" => self.help(ctx, command).await,
            _ => (),
        };
    }
}

#[async_trait]
impl VoiceEventHandler for Handler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::SpeakingStateUpdate(event) => {
                if let Some(user) = event.user_id {
                    self.0
                        .send(Action::MapUser(UserId(user.0), event.ssrc))
                        .expect("Event dispatch error");
                }
            }
            EventContext::VoicePacket(packet) => {
                if let Some(audio) = packet.audio {
                    self.0
                        .send(Action::RegisterVoiceData(
                            packet.packet.ssrc,
                            audio.iter().step_by(2).copied().collect(),
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
    async fn list(&self, ctx: Context, command: ApplicationCommandInteraction) {
        let Some(guild) = command.guild_id else {
            return
        };

        let (tx, rx) = oneshot::channel();
        self.0
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
                            list.into_iter()
                                .map(|user| format!("- {}", Mention::from(user)))
                                .join("\n")
                        })
                    })
            })
            .await
            .expect("Voice data transmission failure");
    }

    async fn join(&self, ctx: Context, command: ApplicationCommandInteraction) {
        self.0
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
        self.0
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

        let manager = songbird::get(&ctx).await.unwrap();
        let (handler_lock, conn_result) = manager.join(guild, channel).await;
        conn_result.expect("Voice connexion failure");

        let mut handler = handler_lock.lock().await;
        handler.add_global_event(CoreEvent::SpeakingStateUpdate.into(), self.clone());
        handler.add_global_event(CoreEvent::VoicePacket.into(), self.clone());

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
        self.0
            .send(Action::GetData(requested_user.id, tx))
            .expect("Download request failure");

        let data = rx.await.expect("Voice data fetching error");
        match data.map(|data| Vec::from(data)) {
            Some(data) => {
                let mut wav_file = Cursor::new(Vec::with_capacity(44 + data.len() * 2));
                wav::write(
                    wav::Header::new(wav::WAV_FORMAT_PCM, 1, storage::FREQUENCY as u32, 16),
                    &wav::BitDepth::Sixteen(data),
                    &mut wav_file,
                )
                .expect("Cannot create wav file");
                command
                    .create_interaction_response(&ctx, |response| {
                        response
                            .kind(InteractionResponseType::ChannelMessageWithSource)
                            .interaction_response_data(|message| {
                                message.add_file(AttachmentType::Bytes {
                                    data: Cow::from(wav_file.into_inner()),
                                    filename: format!("{}.wav", requested_user.name),
                                })
                            })
                    })
                    .await
                    .expect("Voice data transmission failure");
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
}

async fn create_global_commands(ctx: &Context) {
    Command::set_global_application_commands(ctx, |builder| {
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

        // Help.
        builder.create_application_command(|command| {
            command
                .kind(CommandType::ChatInput)
                .name("help")
                .description("Display help")
        })
    })
    .await
    .expect("Global commands creation failure");
}

async fn find_voice_channel(ctx: &Context, guild: GuildId, user: UserId) -> Option<ChannelId> {
    for (id, channel) in guild.channels(&ctx.http).await.unwrap() {
        if channel.kind == ChannelType::Voice {
            let members = channel.members(ctx).await.unwrap();
            if members.iter().any(|m| m.user.id == user) {
                return Some(id);
            }
        }
    }
    None
}

#[tokio::main]
async fn main() {
    let whitelist_path = PathBuf::from(env::var("WHITELIST").expect("Missing whitelist"));
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

    let storage = Storage::new(
        Duration::from_secs(3 * 60),
        Duration::from_secs(5 * 60),
        whitelist,
        whitelist_path,
    );
    let tx = storage.run_loop();

    let token = env::var("DISCORD_TOKEN").expect("Missing token");
    let intents = GatewayIntents::all();
    let mut client = Client::builder(token, intents)
        .event_handler(Handler(tx))
        .register_songbird_from_config(songbird::Config::default().decode_mode(DecodeMode::Decode))
        .await
        .expect("Error creating client");

    if let Err(why) = client.start().await {
        println!("An error occurred while running the client: {:?}", why);
    }
}
