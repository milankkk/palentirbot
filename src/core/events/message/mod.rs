use std::sync::Arc;

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

    let bot_id = ctx.bot_user_id();

    // Check if the message is a direct reply to a conversational AI message sent by the bot
    let is_reply_to_bot = msg
        .referenced_message
        .as_deref()
        .map(|replied| {
            replied.author.id == bot_id
                && crate::commands::utility::is_conversational_ai_message(replied)
        })
        .unwrap_or(false);

    let is_dm = msg.guild_id.is_none();
    let is_ai_channel = msg
        .guild_id
        .and_then(|gid| ctx.guild_settings(gid, |s| s.ai_channel))
        .flatten()
        == Some(msg.channel_id);
    let ai_passive_eligible = is_reply_to_bot || is_dm || is_ai_channel;

    let content = match msg.content.strip_prefix(prefix.as_str()) {
        Some(rest) => rest.trim_start(),
        None => {
            let trimmed = msg.content.trim();
            if is_reply_to_bot
                && (!trimmed.is_empty() || !msg.attachments.is_empty())
                && !crate::commands::utility::is_non_ai_command_with_prefix(trimmed, Some(&prefix))
            {
                crate::commands::utility::handle_prefix_ask(ctx, &msg, &msg.content, &prefix).await;
                return;
            }
            if let Some(stripped) =
                crate::commands::utility::strip_bot_mention(&msg.content, bot_id)
            {
                if !crate::commands::utility::is_non_ai_command_with_prefix(&stripped, Some(&prefix))
                {
                    crate::commands::utility::handle_prefix_ask(ctx, &msg, &stripped, &prefix)
                        .await;
                }
                return;
            }
            forward_passive_ai_if_eligible(ctx, &msg, &prefix, is_dm || is_ai_channel).await;
            return;
        }
    };
    if content.is_empty() {
        forward_passive_ai_if_eligible(ctx, &msg, &prefix, ai_passive_eligible).await;
        return;
    }

    let mut parts = content.splitn(2, char::is_whitespace);
    let cmd_name = parts.next().unwrap_or("").to_ascii_lowercase();
    let args_str = parts.next().unwrap_or("").trim();

    match cmd_name.as_str() {
        // ── ask / ai ──────────────────────────────────────────────────────────
        "ask" | "ai" | "a" => {
            crate::commands::utility::handle_prefix_ask(ctx, &msg, args_str, &prefix).await;
        }

        // ── aicontext / context ───────────────────────────────────────────────
        "aicontext" | "context" | "aic" => {
            if let Err(reason) =
                crate::commands::utility::check_ai_access(&ctx, msg.author.id, msg.guild_id).await
            {
                let _ = msg.error(&ctx, reason).await;
                return;
            }
            use crate::util::builder::MessageBuilder;
            let embed = crate::commands::utility::create_context_embed(&ctx, msg.channel_id).await;
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

        // ── kys ───────────────────────────────────────────────────────────────
        "kys" => {
            use crate::util::builder::MessageBuilder;
            let builder =
                MessageBuilder::new().content("https://klipy.com/gifs/finnish-hospital");
            if let Err(err) = msg.channel_id.create_message(&ctx, &builder).await {
                tracing::error!(?err, "prefix kys failed");
            }
        }

        // ── render ────────────────────────────────────────────────────────────
        "render" => {
            use crate::commands::danser::render_score::{
                build_queue_added_message, check_render_access, check_user_queue_limit,
                finalize_enqueue, parse_prefix_render_args, prepare_attachment_replay_data,
                prepare_score_by_id, render_score_from_message, EmbedRenderOutcome,
                ScorePrepareOutcome,
            };

            if let Err(err_msg) = check_render_access(
                &ctx,
                msg.author.id,
                msg.guild_id,
                msg.channel_id,
            )
            .await
            {
                let _ = msg.error(&ctx, err_msg).await;
                return;
            }

            if let Err(err_msg) = check_user_queue_limit(&ctx, msg.author.id).await {
                let _ = msg.error(&ctx, err_msg).await;
                return;
            }

            let (opts, score_id_opt) = match parse_prefix_render_args(&ctx, args_str) {
                Ok(parsed) => parsed,
                Err(err_msg) => {
                    let _ = msg.error(&ctx, err_msg).await;
                    return;
                }
            };

            let (mut replay_data, warning): (_, Option<&'static str>) = if let Some(attachment) =
                msg.attachments.first()
            {
                match prepare_attachment_replay_data(
                    &ctx,
                    attachment,
                    msg.id,
                    msg.channel_id,
                    msg.author.id,
                    Some(msg.author.name.clone()),
                    opts,
                )
                .await
                {
                    Ok(data) => (data, None),
                    Err(err_msg) => {
                        let _ = msg.error(&ctx, err_msg).await;
                        return;
                    }
                }
            } else if let Some(score_id) = score_id_opt {
                match prepare_score_by_id(
                    &ctx,
                    score_id,
                    msg.channel_id,
                    msg.author.id,
                    opts,
                )
                .await
                {
                    Ok(ScorePrepareOutcome::Ready(data)) => (*data, None),
                    Ok(ScorePrepareOutcome::Unavailable(reason)) => {
                        let _ = msg.error(&ctx, reason).await;
                        return;
                    }
                    Err(err) => {
                        tracing::warn!(?err, ?score_id, "failed to fetch score by URL/ID");
                        let _ = msg
                            .error(&ctx, "Could not fetch that score from the osu! API.")
                            .await;
                        return;
                    }
                }
            } else if let Some(replied) = msg.referenced_message.as_deref() {
                match render_score_from_message(
                    Arc::clone(&ctx),
                    replied,
                    msg.channel_id,
                    msg.author.id,
                    opts,
                )
                .await
                {
                    Ok(EmbedRenderOutcome::Enqueued { data, warning }) => (*data, warning),
                    Ok(EmbedRenderOutcome::NotScoreEmbed) => {
                        let _ = msg
                            .error(
                                &ctx,
                                "Could not recognize a Bathbot/Owobot score embed on that message.",
                            )
                            .await;
                        return;
                    }
                    Ok(EmbedRenderOutcome::ReplayUnavailable(reason)) => {
                        let _ = msg.error(&ctx, reason).await;
                        return;
                    }
                    Err(err) => {
                        tracing::error!(?err, "failed to render score from replied embed");
                        let _ = msg
                            .error(&ctx, "Failed to process the replied score embed.")
                            .await;
                        return;
                    }
                }
            } else {
                let _ = msg
                    .error(
                        &ctx,
                        "Attach a `.osr` replay file, pass an osu! score URL, or reply to a Bathbot/Owobot-style score embed with this command.",
                    )
                    .await;
                return;
            };

            let job_id = ctx.replay_queue.allocate_job_id();
            replay_data.job_id = job_id;
            let builder = build_queue_added_message(job_id, warning);
            let queue_msg = msg.channel_id.create_message(&ctx, &builder).await.ok();
            finalize_enqueue(ctx, replay_data, queue_msg).await;
        }

        // ── rr / rtop / rrn / link ────────────────────────────────────────────
        "rr" => {
            crate::commands::danser::profile_render::handle_prefix_rr(
                ctx, &msg, args_str, &prefix,
            )
            .await;
        }
        "rtop" => {
            crate::commands::danser::profile_render::handle_prefix_rtop(
                ctx, &msg, args_str, &prefix,
            )
            .await;
        }
        "rrn" => {
            crate::commands::danser::profile_render::handle_prefix_rrn(
                ctx, &msg, args_str, &prefix,
            )
            .await;
        }
        "link" => {
            crate::commands::danser::profile_render::handle_prefix_link(
                ctx, &msg, args_str, &prefix,
            )
            .await;
        }

        // ── screenshot ────────────────────────────────────────────────────────
        "ss" | "screenshot" => {
            crate::commands::danser::screenshot::handle_prefix_screenshot(ctx, &msg, args_str)
                .await;
        }

        // ── queue ─────────────────────────────────────────────────────────────
        "queue" => {
            use crate::util::builder::MessageBuilder;
            let embed = crate::commands::danser::build_queue_embed(&ctx, msg.channel_id)
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
                 `{p}ask <question>` — Ask the AI (also works by `@mention`, DM, reply, or in `/setup aichannel`)\n\
                 `{p}aicontext` — Show context window usage & engine stats (aliases: `{p}context`, `{p}aic`)\n\
                 `{p}ping` — Check latency\n\
                 `{p}render` — Render a replay (attach `.osr`, pass an osu! score URL, or reply to a score embed). Args: `start=MM:SS` `end=MM:SS` `pitch=<float>` `skin=\"<name>\"` `skip` `offset=<ms>` `force`\n\
                 `{p}rr [user]` — List renderable recent osu!standard plays with render buttons\n\
                 `{p}rtop [user]` — List renderable osu!standard plays from a player's top 200 (paginated)\n\
                 `{p}rrn [user]` — Analyze and immediately render the most recent osu!standard play\n\
                 `{p}link <user>` — Link your osu! profile for `{p}rr`, `{p}rtop`, and `{p}rrn`\n\
                 `{p}ss <time>` — Snap a PNG screenshot from an `.osr`, score URL, or replied score embed (`{p}screenshot`)\n\
                 `{p}queue` — Show the render queue\n\
                 `{p}skinlist` — List available skins\n\
                 `{p}invite` — Bot invite link\n\
                 `{p}help` — Show this message\n\n\
                 Slash commands are also available — type `/` to browse them.\n\
                 Server admins can configure prefix/AI channel with `/setup`."
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
            use crate::util::builder::{EmbedBuilder, MessageBuilder};
            let invite_url = format!(
                "https://discord.com/oauth2/authorize?client_id={}&permissions=274878286848&integration_type=0&scope=bot+applications.commands",
                ctx.bot_user_id()
            );
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
                    use std::fmt::Write as _;
                    let mut list = String::new();
                    for (i, skin) in skins.iter().enumerate() {
                        let entry = format!("{}. {}\n", i + 1, skin.to_string_lossy());
                        if list.len() + entry.len() > 3800 {
                            let _ = write!(
                                list,
                                "...and {} more (use `/skin list` for all)",
                                skins.len() - i
                            );
                            break;
                        }
                        list.push_str(&entry);
                    }

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

        // ── unknown: forward to passive AI if eligible ────────────────────────
        _ => {
            forward_passive_ai_if_eligible(ctx, &msg, &prefix, ai_passive_eligible).await;
        }
    }
}

async fn forward_passive_ai_if_eligible(
    ctx: Arc<Context>,
    msg: &Message,
    prefix: &str,
    eligible: bool,
) {
    let trimmed = msg.content.trim();
    if eligible
        && (!trimmed.is_empty() || !msg.attachments.is_empty())
        && !crate::commands::utility::is_non_ai_command_with_prefix(trimmed, Some(prefix))
    {
        crate::commands::utility::handle_prefix_ask(ctx, msg, &msg.content, prefix).await;
    }
}
