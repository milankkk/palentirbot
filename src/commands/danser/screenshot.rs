use std::{
    collections::HashMap,
    convert::TryFrom,
    process::Stdio,
    sync::{Arc, LazyLock, Mutex as StdMutex},
    time::Duration,
};

use command_macros::SlashCommand;
use eyre::Result;
use tokio::io::AsyncReadExt as _;
use twilight_interactions::command::{AutocompleteValue, CommandModel, CreateCommand};
use twilight_model::{
    channel::{Attachment, Message},
    guild::PremiumTier,
    id::{
        marker::{GuildMarker, UserMarker},
        Id,
    },
};

use super::{
    render_score::{
        check_render_access, parse_osu_score_ref, parse_slash_score_url, resolve_optional_skin,
        resolve_prefix_replay_input, resolve_slash_replay_input, split_quoted_tokens,
        ParsedScoreUrl,
    },
    settings::{autocomplete_skins, resolve_user_or_default_settings},
};
use crate::{
    core::{
        replay_queue::{
            danser::{
                build_danser_base_command, build_danser_ld_library_path, read_danser_log_tail,
                terminate_danser_group,
            },
            mapset::{resolve_and_ensure_beatmap, BeatmapResolveError, ResolvedBeatmap},
            RenderOptions, ReplayData, TimePoints,
        },
        BotConfig, Context,
    },
    util::{
        builder::MessageBuilder, interaction::InteractionCommand, Authored, ChannelExt,
        InteractionCommandExt, MessageExt,
    },
};

const MAX_SCREENSHOTS_PER_USER: usize = 1;
const MAX_ACTIVE_SCREENSHOTS: usize = 6;

static ACTIVE_SCREENSHOTS: LazyLock<StdMutex<HashMap<Id<UserMarker>, usize>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

struct ScreenshotSlotGuard {
    user: Id<UserMarker>,
}

impl ScreenshotSlotGuard {
    fn acquire(user: Id<UserMarker>, is_owner: bool) -> std::result::Result<Self, &'static str> {
        let mut counts = ACTIVE_SCREENSHOTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let total: usize = counts.values().sum();
        if total >= MAX_ACTIVE_SCREENSHOTS {
            return Err("Too many screenshots are currently processing globally. Please wait a moment and try again.");
        }
        let user_active = counts.get(&user).copied().unwrap_or(0);
        if !is_owner && user_active >= MAX_SCREENSHOTS_PER_USER {
            return Err("You already have a screenshot in progress. Please wait for it to finish.");
        }
        *counts.entry(user).or_insert(0) += 1;
        Ok(Self { user })
    }
}

impl Drop for ScreenshotSlotGuard {
    fn drop(&mut self) {
        let mut counts = ACTIVE_SCREENSHOTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = counts.get_mut(&self.user) {
            if *count <= 1 {
                counts.remove(&self.user);
            } else {
                *count -= 1;
            }
        }
    }
}

fn max_discord_attachment_bytes(ctx: &Context, guild_id: Option<Id<GuildMarker>>) -> usize {
    match guild_id.and_then(|gid| ctx.cache.guild(gid, |g| g.premium_tier()).ok()) {
        Some(PremiumTier::Tier3) => 100 * 1024 * 1024,
        Some(PremiumTier::Tier2) => 50 * 1024 * 1024,
        _ => 10 * 1024 * 1024,
    }
}

fn check_screenshot_size(
    ctx: &Context,
    guild_id: Option<Id<GuildMarker>>,
    png_len: usize,
) -> std::result::Result<(), String> {
    let max_bytes = max_discord_attachment_bytes(ctx, guild_id);
    if png_len > max_bytes {
        let mb = png_len as f64 / (1024.0 * 1024.0);
        let limit_mb = max_bytes / (1024 * 1024);
        return Err(format!(
            "Screenshot PNG ({mb:.2} MiB) exceeds this server's {limit_mb} MiB upload limit."
        ));
    }
    Ok(())
}

#[derive(CreateCommand, SlashCommand)]
#[command(name = "screenshot")]
#[flags(SKIP_DEFER)]
/// Snap a PNG screenshot from a replay at a specific timestamp
pub struct Screenshot {
    /// Timestamp to capture in seconds or MM:SS (e.g. 1:23)
    time: String,
    #[command(rename = "replay")]
    /// Specify the replay through a .osr file
    attachment: Option<Attachment>,
    /// Paste an osu! score URL or score ID
    score_url: Option<String>,
    #[command(autocomplete = true)]
    /// Temporarily override skin for this screenshot
    skin: Option<String>,
}

#[derive(CommandModel)]
#[command(autocomplete = true)]
pub struct ScreenshotAutocomplete {
    time: Option<String>,
    #[command(rename = "replay")]
    attachment: Option<Attachment>,
    score_url: Option<String>,
    skin: AutocompleteValue<String>,
}

impl TryFrom<ScreenshotAutocomplete> for Screenshot {
    type Error = String;

    fn try_from(args: ScreenshotAutocomplete) -> std::result::Result<Self, Self::Error> {
        let skin = match args.skin {
            AutocompleteValue::Focused(focused) => return Err(focused),
            AutocompleteValue::None => None,
            AutocompleteValue::Completed(val) => Some(val),
        };

        Ok(Self {
            time: args.time.unwrap_or_default(),
            attachment: args.attachment,
            score_url: args.score_url,
            skin,
        })
    }
}

pub async fn slash_screenshot(ctx: Arc<Context>, mut command: InteractionCommand) -> Result<()> {
    let raw_args = ScreenshotAutocomplete::from_interaction(command.input_data())?;
    let Screenshot {
        time,
        attachment,
        score_url,
        skin,
    } = match Screenshot::try_from(raw_args) {
        Ok(args) => args,
        Err(focused) => {
            let choices = autocomplete_skins(&ctx, &focused)?;
            command.autocomplete(&ctx, choices).await?;
            return Ok(());
        }
    };

    if attachment.is_none() && score_url.is_none() {
        command
            .error_callback(
                &ctx,
                "Please attach a `.osr` replay file or provide an osu! `score_url`.",
                true,
            )
            .await?;
        return Ok(());
    }

    let secs = match TimePoints::parse_single(&time) {
        Ok(s) => s,
        Err(err) => {
            command.error_callback(&ctx, err, true).await?;
            return Ok(());
        }
    };

    let resolved_skin = match resolve_optional_skin(
        &ctx,
        skin.as_deref(),
        "Use `/skin list` to browse skins.",
    ) {
        Ok(name) => name,
        Err(msg) => {
            command.error_callback(&ctx, msg, true).await?;
            return Ok(());
        }
    };

    let parsed_score_id = match parse_slash_score_url(score_url.as_deref(), attachment.is_some()) {
        Ok(parsed) => parsed,
        Err(msg) => {
            command.error_callback(&ctx, msg, true).await?;
            return Ok(());
        }
    };

    let user = command.user_id()?;
    let is_owner = BotConfig::get().owners.contains(&user);
    let _slot_guard = match ScreenshotSlotGuard::acquire(user, is_owner) {
        Ok(guard) => guard,
        Err(msg) => {
            command.error_callback(&ctx, msg, true).await?;
            return Ok(());
        }
    };

    command.defer(&ctx, false).await?;

    if let Err(err_msg) = check_render_access(&ctx, user, command.guild_id, command.channel_id).await {
        command.error(&ctx, err_msg).await?;
        return Ok(());
    }

    let opts = RenderOptions {
        skin: resolved_skin,
        ..Default::default()
    };

    let replay_data = match resolve_slash_replay_input(
        &ctx,
        attachment.as_ref(),
        parsed_score_id,
        command.id,
        command.channel_id,
        user,
        opts,
    )
    .await
    {
        Ok(data) => data,
        Err(err_msg) => {
            command.error(&ctx, err_msg).await?;
            return Ok(());
        }
    };

    match take_replay_screenshot(&ctx, replay_data, secs, command.id, user).await {
        Ok((png_bytes, caption)) => {
            if let Err(err_msg) = check_screenshot_size(&ctx, command.guild_id, png_bytes.len()) {
                command.error(&ctx, err_msg).await?;
                return Ok(());
            }
            let builder = MessageBuilder::new()
                .content(caption)
                .attachment(format!("screenshot_{secs}s.png"), png_bytes);
            command.update(&ctx, &builder).await?;
        }
        Err(err_msg) => {
            command.error(&ctx, err_msg).await?;
        }
    }

    Ok(())
}

/// Handle prefix `!ss` / `!screenshot` commands (`!ss <time> [score_url] [skin="..."]`).
pub async fn handle_prefix_screenshot(ctx: Arc<Context>, msg: &Message, args_str: &str) {
    if let Err(err_msg) = check_render_access(&ctx, msg.author.id, msg.guild_id, msg.channel_id).await
    {
        let _ = msg.error(&ctx, err_msg).await;
        return;
    }

    let tokens = match split_quoted_tokens(args_str) {
        Ok(t) => t,
        Err(err_msg) => {
            let _ = msg.error(&ctx, err_msg).await;
            return;
        }
    };

    let mut time_secs: Option<u32> = None;
    let mut score_id: Option<ParsedScoreUrl> = None;
    let mut raw_skin: Option<String> = None;

    for token in tokens {
        if let Some((key, val)) = token.split_once('=') {
            match key.to_ascii_lowercase().as_str() {
                "skin" => {
                    let trimmed = val.trim();
                    if trimmed.is_empty() {
                        let _ = msg.error(&ctx, "`skin=` cannot be empty.").await;
                        return;
                    }
                    raw_skin = Some(trimmed.to_owned());
                    continue;
                }
                "time" | "t" if time_secs.is_none() => {
                    match TimePoints::parse_single(val) {
                        Ok(s) => time_secs = Some(s),
                        Err(e) => {
                            let _ = msg
                                .error(&ctx, format!("Invalid timestamp `{val}`: {e}"))
                                .await;
                            return;
                        }
                    }
                    continue;
                }
                _ => {}
            }
        }

        if let Some(parsed) = parse_osu_score_ref(&token) {
            if matches!(parsed, ParsedScoreUrl::UnsupportedMode) {
                let _ = msg
                    .error(&ctx, "danser only accepts osu!standard plays, sorry :(")
                    .await;
                return;
            }
            score_id = Some(parsed);
        } else if time_secs.is_none() {
            match TimePoints::parse_single(&token) {
                Ok(s) => time_secs = Some(s),
                Err(e) => {
                    let _ = msg
                        .error(&ctx, format!("Invalid timestamp `{token}`: {e}"))
                        .await;
                    return;
                }
            }
        }
    }

    let Some(secs) = time_secs else {
        let _ = msg
            .error(
                &ctx,
                "Please specify a timestamp (e.g. `!ss 1:23` with an `.osr`, a score URL, or replying to a score embed).",
            )
            .await;
        return;
    };

    let skin = match resolve_optional_skin(
        &ctx,
        raw_skin.as_deref(),
        "Use `/skin list` or `!skinlist`.",
    ) {
        Ok(name) => name,
        Err(err_msg) => {
            let _ = msg.error(&ctx, err_msg).await;
            return;
        }
    };

    let is_owner = BotConfig::get().owners.contains(&msg.author.id);
    let _slot_guard = match ScreenshotSlotGuard::acquire(msg.author.id, is_owner) {
        Ok(guard) => guard,
        Err(err_msg) => {
            let _ = msg.error(&ctx, err_msg).await;
            return;
        }
    };

    let opts = RenderOptions {
        skin,
        ..Default::default()
    };

    let (replay_data, _) = match resolve_prefix_replay_input(
        &ctx,
        msg,
        score_id,
        opts,
        "Attach a `.osr` replay, pass an osu! score URL, or reply to a score embed.",
    )
    .await
    {
        Ok(res) => res,
        Err(err_msg) => {
            let _ = msg.error(&ctx, err_msg).await;
            return;
        }
    };

    let status_msg = msg
        .channel_id
        .create_message(
            &ctx,
            &MessageBuilder::new()
                .content("📸 Capturing screenshot...")
                .reply(msg.id),
        )
        .await
        .ok();

    match take_replay_screenshot(&ctx, replay_data, secs, msg.id, msg.author.id).await {
        Ok((png_bytes, caption)) => {
            if let Err(err_msg) = check_screenshot_size(&ctx, msg.guild_id, png_bytes.len()) {
                if let Some(ref ph) = status_msg {
                    let _ = ph.delete(&ctx).await;
                }
                let _ = msg.error(&ctx, err_msg).await;
                return;
            }
            let builder = MessageBuilder::new()
                .content(caption)
                .attachment(format!("screenshot_{secs}s.png"), png_bytes)
                .reply(msg.id);
            let send_res = msg.channel_id.create_message(&ctx, &builder).await;
            if let Some(ref ph) = status_msg {
                let _ = ph.delete(&ctx).await;
            }
            if let Err(err) = send_res {
                tracing::warn!(?err, "failed to upload screenshot message");
                let _ = msg
                    .error(&ctx, "Failed to upload screenshot to Discord.")
                    .await;
            }
        }
        Err(err_msg) => {
            if let Some(ref ph) = status_msg {
                let _ = ph.delete(&ctx).await;
            }
            let _ = msg.error(&ctx, err_msg).await;
        }
    }
}

async fn take_replay_screenshot(
    ctx: &Context,
    replay_data: ReplayData,
    secs: u32,
    tx_id: impl std::fmt::Display,
    user: Id<UserMarker>,
) -> std::result::Result<(Vec<u8>, String), String> {
    let ReplayData {
        path,
        replay,
        skin,
        ..
    } = replay_data;

    let res = run_screenshot_inner(ctx, &path, replay.beatmap_hash.as_deref(), skin.as_deref(), secs, tx_id, user).await;
    let _ = tokio::fs::remove_file(&path).await;
    res
}

async fn run_screenshot_inner(
    ctx: &Context,
    replay_path: &std::path::Path,
    beatmap_hash: Option<&str>,
    skin: Option<&str>,
    secs: u32,
    tx_id: impl std::fmt::Display,
    user: Id<UserMarker>,
) -> std::result::Result<(Vec<u8>, String), String> {
    let ResolvedBeatmap {
        mapset_title,
        map_version,
        ..
    } = resolve_and_ensure_beatmap(ctx, beatmap_hash, false, None)
        .await
        .map_err(|err| match err {
            BeatmapResolveError::Failure { user_msg, .. } => user_msg.to_owned(),
            BeatmapResolveError::Cancelled => "Cancelled.".to_owned(),
        })?;
    let map_label = format!("{mapset_title} [{map_version}]");

    let _gpu_guard = ctx.gpu_lock().lock().await;
    ctx.ai().unload_loaded_models().await;

    let config = BotConfig::get();
    let danser_bin = config.paths.danser().join("danser");
    let (settings, _) = resolve_user_or_default_settings(user).await;

    let canonical_osr = tokio::fs::canonicalize(replay_path).await.map_err(|err| {
        tracing::warn!(?err, "failed to canonicalize replay path for screenshot");
        "Failed to access stored replay file.".to_owned()
    })?;

    let out_name = format!("ss_{tx_id}");
    let out_png = config.paths.danser().join(format!("screenshots/{out_name}.png"));

    let ld_library_path = build_danser_ld_library_path(config.paths.danser());
    let mut cmd = build_danser_base_command(
        &danser_bin,
        &ld_library_path,
        &canonical_osr,
        &settings,
        &out_name,
        skin,
    );
    cmd.stdout(Stdio::null())
        .stderr(Stdio::piped())
        .args(["-ss", &secs.to_string()]);

    let mut child = cmd.spawn().map_err(|err| {
        tracing::warn!(?err, "failed to launch danser for screenshot");
        "Failed to run danser for screenshot.".to_owned()
    })?;
    let pgid = child.id();

    let stderr_handle = child.stderr.take().map(|mut stderr| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                match stderr.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if buf.len() < 16 * 1024 {
                            let take = n.min(16 * 1024 - buf.len());
                            buf.extend_from_slice(&tmp[..take]);
                        }
                    }
                }
            }
            buf
        })
    });

    let wait_res = tokio::time::timeout(Duration::from_secs(60), child.wait()).await;
    let status = match wait_res {
        Ok(Ok(status)) => status,
        other => {
            terminate_danser_group(&mut child, pgid).await;
            if let Some(h) = stderr_handle {
                h.abort();
            }
            let _ = tokio::fs::remove_file(&out_png).await;
            let danser_log_tail = read_danser_log_tail(config.paths.danser(), 15).await;
            if let Ok(Err(err)) = other {
                tracing::warn!(?err, ?danser_log_tail, "failed awaiting danser screenshot process");
                return Err("Failed to run danser for screenshot.".to_owned());
            }
            tracing::warn!(?danser_log_tail, "danser screenshot timed out after 60s");
            return Err("danser screenshot timed out after 60s.".to_owned());
        }
    };

    let stderr_bytes = if let Some(mut h) = stderr_handle {
        match tokio::time::timeout(Duration::from_secs(1), &mut h).await {
            Ok(Ok(buf)) => buf,
            _ => {
                h.abort();
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        let danser_log_tail = read_danser_log_tail(config.paths.danser(), 15).await;
        tracing::warn!(status = %status, stderr = %stderr, ?danser_log_tail, "danser -ss failed");
        let _ = tokio::fs::remove_file(&out_png).await;
        return Err("danser failed to capture screenshot (timestamp may be past map end).".to_owned());
    }

    let bytes = tokio::fs::read(&out_png).await.map_err(|err| {
        tracing::warn!(?err, ?out_png, "failed to read screenshot png");
        "Screenshot output file was not produced.".to_owned()
    });
    let _ = tokio::fs::remove_file(&out_png).await;
    let png_bytes = bytes?;

    let mm = secs / 60;
    let ss = secs % 60;
    let caption = format!("📸 `{map_label}` @ `{mm}:{ss:02}`");
    Ok((png_bytes, caption))
}
