use std::sync::Arc;

use crate::commands::danser::{queue::send_queue_status, render_score::render_score_from_message};

use crate::util::builder::MessageBuilder;

use twilight_model::channel::Message;

use crate::{core::Context, util::ChannelExt};

const DEFAULT_PREFIX: &str = "!";

pub async fn handle_message(ctx: Arc<Context>, msg: Message) {
    // Ignore bots
    if msg.author.bot {
        return;
    }

    // ── Resolve per-guild prefix ─────────────────────────────────────────────
    let prefix = msg
        .guild_id
        .and_then(|gid| ctx.guild_settings(gid, |s| s.effective_prefix().to_owned()))
        .unwrap_or_else(|| DEFAULT_PREFIX.to_owned());

    let bot_id = ctx.cache.current_user(|u| u.id).ok();

    // Check if the message is a direct reply to a message sent by the bot
    let is_reply_to_bot = msg
        .referenced_message
        .as_deref()
        .map(|replied| Some(replied.author.id) == bot_id)
        .unwrap_or(false);

    let content = match msg.content.strip_prefix(prefix.as_str()) {
        Some(rest) => rest.trim_start(),
        None => {
            if is_reply_to_bot && !msg.content.trim().is_empty() {
                // Unprefixed reply to the bot: silently return if AI is disabled
                if !crate::ai::is_enabled_for_guild(&ctx, msg.guild_id) {
                    return;
                }
                // Direct response to the AI without command prefix
                crate::ai::commands::handle_prefix_ask(ctx, &msg, &msg.content, &prefix, crate::ai::AiMode::Thinking).await;
            }
            return;
        }
    };
    if content.is_empty() {
        return;
    }

    let mut parts = content.splitn(2, char::is_whitespace);
    let cmd_name = parts.next().unwrap_or("").to_ascii_lowercase();
    let args_str = parts.next().unwrap_or("").trim();

    match cmd_name.as_str() {
        // ── ask / ai (Thinking mode) ──────────────────────────────────────────
        "ask" | "ai" => {
            if !crate::ai::is_enabled_for_guild(&ctx, msg.guild_id) {
                let _ = msg.error(&ctx, crate::ai::AI_DISABLED_REPLY).await;
                return;
            }
            crate::ai::commands::handle_prefix_ask(ctx, &msg, args_str, &prefix, crate::ai::AiMode::Thinking).await;
        }

        // ── a / fast (Fast mode) ──────────────────────────────────────────────
        "a" | "fast" => {
            if !crate::ai::is_enabled_for_guild(&ctx, msg.guild_id) {
                let _ = msg.error(&ctx, crate::ai::AI_DISABLED_REPLY).await;
                return;
            }
            crate::ai::commands::handle_prefix_ask(ctx, &msg, args_str, &prefix, crate::ai::AiMode::Fast).await;
        }

        // ── aicontext / context ───────────────────────────────────────────────
        "aicontext" | "context" | "aic" => {
            if !crate::ai::is_enabled_for_guild(&ctx, msg.guild_id) {
                let _ = msg.error(&ctx, crate::ai::AI_DISABLED_REPLY).await;
                return;
            }
            use crate::util::builder::MessageBuilder;
            let embed = crate::ai::commands::create_context_embed(&ctx, msg.channel_id, msg.guild_id).await;
            let _ = msg
                .channel_id
                .create_message(&ctx, &MessageBuilder::new().embed(embed))
                .await;
        }

        // ── ping ──────────────────────────────────────────────────────────────
        "ping" => {
            use crate::util::builder::MessageBuilder;
            use crate::util::MessageExt as _;
            let start = std::time::Instant::now();
            let builder = MessageBuilder::new().content("Pong!");
            match msg.channel_id.create_message(&ctx, &builder).await {
                Ok(sent) => {
                    let elapsed = start.elapsed().as_millis();
                    let update = MessageBuilder::new().content(format!("🏓 Pong! ({elapsed}ms)"));
                    let _ = (sent.id, sent.channel_id).update(&ctx, &update).await;
                }
                Err(err) => tracing::error!(?err, "prefix ping failed"),
            }
        }

        // ── render ────────────────────────────────────────────────────────────
        "render" => {
            if let Some(attachment) = msg.attachments.first() {
                if !matches!(attachment.filename.split('.').next_back(), Some("osr")) {
                    let _ = msg
                        .error(&ctx, "The attachment must be a `.osr` file!")
                        .await;
                    return;
                }

                // Blacklist check
                if let Some(guild_id) = msg.guild_id {
                    let is_allowed = ctx
                        .guild_settings(guild_id, |s| {
                            s.input_channels.is_empty() || s.input_channels.contains(&msg.channel_id)
                        })
                        .unwrap_or(true);

                    if !is_allowed {
                        let _ = msg
                            .error(
                                &ctx,
                                "This channel is not an allowed input channel for rendering.",
                            )
                            .await;
                        return;
                    }

                    if let Ok((true, reason)) = ctx.psql()._is_server_blacklisted(guild_id).await {
                        let mut c = String::from("This server has been blacklisted.");
                        if let Some(r) = reason {
                            c.push(' ');
                            c.push_str(&r);
                        }
                        let _ = msg.error(&ctx, c).await;
                        return;
                    }
                }

                // Always respond in the channel where the message was sent
                let output_channel = msg.channel_id;

                // Parse optional args: start=MM:SS end=MM:SS pitch=1.5
                let mut start_secs: u32 = 0;
                let mut end_secs: u32 = 0;
                let mut pitch: Option<f64> = None;
                for token in args_str.split_whitespace() {
                    if let Some(v) = token.strip_prefix("start=") {
                        if let Ok(s) = crate::core::replay_queue::TimePoints::parse_single(v) {
                            start_secs = s;
                        }
                    } else if let Some(v) = token.strip_prefix("end=") {
                        if let Ok(s) = crate::core::replay_queue::TimePoints::parse_single(v) {
                            end_secs = s;
                        }
                    } else if let Some(v) = token.strip_prefix("pitch=") {
                        if let Ok(p) = v.parse::<f64>() {
                            pitch = Some(p.clamp(0.5, 5.0));
                        }
                    }
                }

                // Download attachment
                let bytes = match ctx.client().get_discord_attachment(attachment).await {
                    Ok(b) => b,
                    Err(err) => {
                        tracing::error!(?err, "failed to download attachment");
                        let _ = msg.error(&ctx, "Failed to download attachment.").await;
                        return;
                    }
                };

                // Parse replay
                use osu_db::{Mode, Replay};
                let replay = match Replay::from_bytes(&bytes.clone()) {
                    Ok(r) => r,
                    Err(err) => {
                        tracing::error!(?err, "failed to parse .osr");
                        let _ = msg
                            .error(
                                &ctx,
                                "Failed to parse the `.osr` file. Is it a valid replay?",
                            )
                            .await;
                        return;
                    }
                };
                if replay.mode != Mode::Standard {
                    let _ = msg
                        .error(&ctx, "danser only accepts osu!standard plays, sorry")
                        .await;
                    return;
                }

                // Save to disk
                let config = crate::core::BotConfig::get();
                let mut replay_file = config.paths.downloads();
                let safe_filename = crate::commands::danser::render_score::sanitize_filename(&attachment.filename);
                replay_file.push(format!("{}_{safe_filename}", msg.id));
                use tokio::io::AsyncWriteExt as _;
                match tokio::fs::File::create(&replay_file).await {
                    Ok(mut f) => {
                        if let Err(e) = f.write_all(&bytes).await {
                            tracing::error!(?e);
                            let _ = msg.error(&ctx, "Failed to save replay.").await;
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::error!(?e);
                        let _ = msg.error(&ctx, "Failed to save replay.").await;
                        return;
                    }
                }

                // Push to queue
                use crate::core::replay_queue::{is_lazer_replay, ReplayData, ReplaySlim, TimePoints};
                let was_empty = ctx.replay_queue.queue.lock().await.is_empty();
                let is_lazer = is_lazer_replay(replay.version);
                ctx.replay_queue
                    .push(ReplayData {
                        input_channel: msg.channel_id,
                        output_channel,
                        pitch,
                        path: replay_file,
                        replay: ReplaySlim::from(replay),
                        time_points: TimePoints {
                            start: start_secs,
                            end: end_secs,
                        },
                        user: msg.author.id,
                        title: None,
                        player_name: None,
                        map_title: None,
                        difficulty_name: None,
                        queue_message: None,
                        is_lazer,
                    })
                    .await;

                use crate::util::builder::MessageBuilder;
                let _ = msg
                    .channel_id
                    .create_message(
                        &ctx,
                        &MessageBuilder::new().embed("Replay has been added to the queue!"),
                    )
                    .await;

                if was_empty {
                    let ctx_clone = Arc::clone(&ctx);
                    tokio::spawn(async move {
                        let _ = send_queue_status(ctx_clone, output_channel).await;
                    });
                }
            } else if let Some(replied) = msg.referenced_message.as_deref() {
                if let Some(guild_id) = msg.guild_id {
                    let is_allowed = ctx
                        .guild_settings(guild_id, |s| {
                            s.input_channels.is_empty() || s.input_channels.contains(&msg.channel_id)
                        })
                        .unwrap_or(true);

                    if !is_allowed {
                        let _ = msg
                            .error(
                                &ctx,
                                "This channel is not an allowed input channel for rendering.",
                            )
                            .await;
                        return;
                    }

                    if let Ok((true, reason)) = ctx.psql()._is_server_blacklisted(guild_id).await {
                        let mut c = String::from("This server has been blacklisted.");
                        if let Some(r) = reason {
                            c.push(' ');
                            c.push_str(&r);
                        }
                        let _ = msg.error(&ctx, c).await;
                        return;
                    }
                }

                // Always respond in the channel where the message was sent
                let output_channel = msg.channel_id;

                let was_empty = ctx.replay_queue.queue.lock().await.is_empty();

                match render_score_from_message(
                    Arc::clone(&ctx),
                    replied,
                    msg.channel_id,
                    output_channel,
                    msg.author.id,
                )
                .await
                {
                    Ok(Some(_)) => {
                        let _ = msg
                            .channel_id
                            .create_message(
                                &ctx,
                                &MessageBuilder::new().embed("Replay has been added to the queue!"),
                            )
                            .await;

                        if was_empty {
                            let ctx_clone = Arc::clone(&ctx);
                            tokio::spawn(async move {
                                let _ = send_queue_status(ctx_clone, output_channel).await;
                            });
                        }
                    }
                    Ok(None) => {
                        let _ = msg
                            .error(
                                &ctx,
                                "Could not download a replay from that embed. Assuming the score is not top 1000 or replay-available.",
                            )
                            .await;
                    }
                    Err(err) => {
                        tracing::error!(?err, "failed to render score from replied embed");
                        let _ = msg
                            .error(&ctx, "Failed to process the replied score embed.")
                            .await;
                    }
                }
            } else {
                let _ = msg
                    .error(
                        &ctx,
                        "Attach a `.osr` replay file, or reply to a Bathbot/Owobot-style score embed with this command.",
                    )
                    .await;
            }
        }

        // ── queue ─────────────────────────────────────────────────────────────
        "queue" => {
            use crate::util::builder::MessageBuilder;
            let embed = crate::commands::danser::build_queue_embed(&ctx)
                .await
                .build();
            let _ = msg
                .channel_id
                .create_message(&ctx, &MessageBuilder::new().embed(embed))
                .await;
        }

        // ── help ──────────────────────────────────────────────────────────────
        "help" => {
            use crate::util::builder::{EmbedBuilder, MessageBuilder};
            let p = msg
                .guild_id
                .and_then(|gid| ctx.guild_settings(gid, |s| s.effective_prefix().to_owned()))
                .unwrap_or_else(|| DEFAULT_PREFIX.to_owned());

            let description = format!(
                "**Prefix commands** (current prefix: `{p}`)\n\n\
                 `{p}ask <question>` — Ask the AI with deep reasoning (aliases: `{p}ai`)\n\
                 `{p}a <question>` — Fast AI response without reasoning (aliases: `{p}fast`)\n\
                 `{p}aicontext` — Show context window usage & engine stats (aliases: `{p}context`, `{p}aic`)\n\
                 `{p}ping` — Check latency\n\
                 `{p}render` — Render a replay (attach `.osr`). Args: `start=MM:SS` `end=MM:SS` `pitch=<float>`\n\
                 `{p}queue` — Show the render queue\n\
                 `{p}skinlist` — List available skins (aliases: `{p}skins`)\n\
                 `{p}invite` — Bot invite link\n\
                 `{p}help` — Show this message\n\n\
                 Slash commands are also available — type `/` to browse them.\n\
                 Server admins can change the prefix with `/setup setprefix`."
            );
            let embed = EmbedBuilder::new()
                .title("Help")
                .description(description)
                .build();
            let _ = msg
                .channel_id
                .create_message(&ctx, &MessageBuilder::new().embed(embed))
                .await;
        }

        // ── invite ────────────────────────────────────────────────────────────
        "invite" => {
            use crate::util::{
                builder::{EmbedBuilder, MessageBuilder},
                constants::INVITE_LINK,
            };
            let invite_url = ctx
                .cache
                .current_user(|u| {
                    format!("https://discord.com/oauth2/authorize?client_id={}&permissions=4503599627388928&integration_type=0&scope=bot+applications.commands", u.id)
                })
                .unwrap_or_else(|_| INVITE_LINK.to_string());
            let embed = EmbedBuilder::new()
                .description(invite_url)
                .title("Invite me to your server!")
                .build();
            let _ = msg
                .channel_id
                .create_message(&ctx, &MessageBuilder::new().embed(embed))
                .await;
        }

        // ── skinlist ──────────────────────────────────────────────────────────
        "skinlist" | "skins" => {
            use crate::util::builder::{EmbedBuilder, MessageBuilder};
            use std::ffi::OsString;

            let skins_result: Result<Vec<OsString>, _> = {
                let mut skin_list = ctx.skin_list();
                skin_list.get().map(|skins| skins.to_vec())
            };

            match skins_result {
                Ok(skins) if skins.is_empty() => {
                    let _ = msg.error(&ctx, "No skins available.").await;
                }
                Ok(skins) => {
                    let list = skins
                        .iter()
                        .enumerate()
                        .map(|(i, skin)| format!("{}. {}", i + 1, skin.to_string_lossy()))
                        .collect::<Vec<_>>()
                        .join("\n");

                    let embed = EmbedBuilder::new()
                        .title("Available skins")
                        .description(list)
                        .build();

                    let builder = MessageBuilder::new().embed(embed);
                    let _ = msg.channel_id.create_message(&ctx, &builder).await;
                }
                Err(err) => {
                    tracing::error!(?err, "failed to load skin list");
                    let _ = msg.error(&ctx, "Failed to load skin list.").await;
                }
            }
        }

        // ── unknown: silently ignore ──────────────────────────────────────────
        _ => {}
    }
}
