use std::sync::Arc;

use crate::commands::danser::{
    queue::send_queue_status,
    render_score::render_score_from_message,
};

use crate::util::builder::MessageBuilder;

use twilight_model::channel::Message;

use crate::{core::Context, util::ChannelExt};

const DEFAULT_PREFIX: &str = "!";

pub async fn handle_message(ctx: Arc<Context>, msg: Message) {
    // Ignore bots
    if msg.author.bot {
        return;
    }

    // ── Legacy OSR attachment forwarder ──────────────────────────────────────
    if let Some(attachment) = msg.attachments.first() {
        if matches!(attachment.filename.split('.').last(), Some("osr")) {
            let valid = msg
                .guild_id
                .map(|gid| ctx.guild_settings(gid, |s| s.input_channels.contains(&msg.channel_id)));
            match valid {
                Some(Some(true)) => {}
                _ => {
                    let content = "Hey! Looks like you tried to send a replay \
                        — use `/render` as we have fully migrated to slash commands.";
                    let _ = msg.error(&ctx, content).await;
                    return;
                }
            }
        }
    }

    // ── Resolve per-guild prefix ─────────────────────────────────────────────
    let prefix = msg
        .guild_id
        .and_then(|gid| {
            ctx.guild_settings(gid, |s| {
                s.prefix.clone().unwrap_or_else(|| DEFAULT_PREFIX.to_owned())
            })
        })
        .unwrap_or_else(|| DEFAULT_PREFIX.to_owned());

    let content = match msg.content.strip_prefix(prefix.as_str()) {
        Some(rest) => rest.trim_start(),
        None => return,
    };
    if content.is_empty() {
        return;
    }

    let mut parts = content.splitn(2, char::is_whitespace);
    let cmd_name = parts.next().unwrap_or("").to_ascii_lowercase();
    let args_str = parts.next().unwrap_or("").trim();

    match cmd_name.as_str() {
        // ── ping ──────────────────────────────────────────────────────────────
        "ping" => {
            use crate::util::MessageExt as _;
            use crate::util::builder::MessageBuilder;
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
                if !matches!(attachment.filename.split('.').last(), Some("osr")) {
                    let _ = msg.error(&ctx, "The attachment must be a `.osr` file!").await;
                    return;
                }

                // Blacklist check
                if let Some(guild_id) = msg.guild_id {
                    if let Ok((true, reason)) = ctx.psql()._is_server_blacklisted(guild_id).await {
                        let mut c = String::from("This server has been blacklisted.");
                        if let Some(r) = reason { c.push(' '); c.push_str(&r); }
                        let _ = msg.error(&ctx, c).await;
                        return;
                    }
                }

                // Resolve output channel
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
                        let _ = msg.error(&ctx,
                            "Failed to parse the `.osr` file. Is it a valid replay?").await;
                        return;
                    }
                };
                if replay.mode != Mode::Standard {
                    let _ = msg.error(&ctx,
                        "danser only accepts osu!standard plays, sorry").await;
                    return;
                }

                // Save to disk
                let config = crate::core::BotConfig::get();
                let mut replay_file = config.paths.downloads();
                replay_file.push(&attachment.filename);
                use tokio::io::AsyncWriteExt as _;
                match tokio::fs::File::create(&replay_file).await {
                    Ok(mut f) => { if let Err(e) = f.write_all(&bytes).await {
                        tracing::error!(?e); let _ = msg.error(&ctx, "Failed to save replay.").await; return;
                    }},
                    Err(e) => { tracing::error!(?e); let _ = msg.error(&ctx, "Failed to save replay.").await; return; }
                }

                // Push to queue
                use crate::core::replay_queue::{ReplayData, ReplaySlim, TimePoints};
                let was_empty = ctx.replay_queue.queue.lock().await.is_empty();
                ctx.replay_queue.push(ReplayData {
                    input_channel: msg.channel_id,
                    output_channel,
                    pitch,
                    path: replay_file,
                    replay: ReplaySlim::from(replay),
                    time_points: TimePoints { start: start_secs, end: end_secs },
                    user: msg.author.id,
                    title: None,
                    player_name: None,
                    map_title: None,
                    difficulty_name: None,
                    queue_message: None,

                }).await;

                use crate::util::builder::MessageBuilder;
                let _ = msg
                    .channel_id
                    .create_message(&ctx, &MessageBuilder::new().embed("Replay has been added to the queue!"))
                    .await;

                if was_empty {
                    let ctx_clone = Arc::clone(&ctx);
                    let out_channel = msg.channel_id; // Copy the ID to move into the task
                    tokio::spawn(async move {
                        let _ = send_queue_status(ctx_clone, out_channel).await;
                    });
                }
            } else if let Some(replied) = msg.referenced_message.as_deref() {
                match render_score_from_message(
                    Arc::clone(&ctx),
                    replied,
                    msg.channel_id,
                    msg.channel_id,
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

                        let _ = send_queue_status(Arc::clone(&ctx), msg.channel_id).await;
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
            let embed = crate::commands::danser::build_queue_embed(&ctx).await.build();
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
                .and_then(|gid| {
                    ctx.guild_settings(gid, |s| {
                        s.prefix
                            .clone()
                            .unwrap_or_else(|| DEFAULT_PREFIX.to_owned())
                    })
                })
                .unwrap_or_else(|| DEFAULT_PREFIX.to_owned());

            let description = format!(
                "**Prefix commands** (current prefix: `{p}`)\n\n\
                 `{p}ping` — Check latency\n\
                 `{p}render` — Render a replay (attach `.osr`). Args: `start=MM:SS` `end=MM:SS` `pitch=<float>`\n\
                 `{p}queue` — Show the render queue\n\
                 `{p}skinlist` — List available skins\n\
                 `{p}invite` — Bot invite link\n\
                 `{p}help` — Show this message\n\n\
                 Slash commands are also available — type `/` to browse them.\n\
                 Server admins can change the prefix with `/setup setprefix`."
            );
            let embed = EmbedBuilder::new().title("Help").description(description).build();
            let _ = msg.channel_id
                .create_message(&ctx, &MessageBuilder::new().embed(embed))
                .await;
        }

        // ── invite ────────────────────────────────────────────────────────────
        "invite" => {
            use crate::util::{builder::{EmbedBuilder, MessageBuilder}, constants::INVITE_LINK};
            let embed = EmbedBuilder::new()
                .description(INVITE_LINK).title("Invite me to your server!").build();
            let _ = msg.channel_id
                .create_message(&ctx, &MessageBuilder::new().embed(embed))
                .await;
        }

        // ── skinlist ──────────────────────────────────────────────────────────
        "skinlist" | "skins" => {
            use std::ffi::OsString;
            use crate::util::builder::{EmbedBuilder, MessageBuilder};

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
