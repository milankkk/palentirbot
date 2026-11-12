use std::{convert::TryFrom, sync::Arc, time::Duration};

use command_macros::SlashCommand;
use eyre::Result;
use tokio::{process::Command, sync::Mutex};
use twilight_interactions::command::{AutocompleteValue, CommandModel, CreateCommand};
use twilight_model::{
    channel::{Attachment, Message},
    id::{marker::UserMarker, Id},
};

use super::{
    render_score::{
        check_render_access, parse_osu_score_url, prepare_attachment_replay_data,
        prepare_score_by_id, render_score_from_message, split_quoted_tokens, EmbedRenderOutcome,
        ScorePrepareOutcome,
    },
    settings::{autocomplete_skins, resolve_skin_name},
};
use crate::{
    core::{
        replay_queue::{process::download_mapset, RenderOptions, ReplayData, TimePoints},
        BotConfig, Context,
    },
    util::{
        builder::MessageBuilder, interaction::InteractionCommand, Authored, ChannelExt,
        InteractionCommandExt, MessageExt,
    },
};

static SCREENSHOT_LOCK: Mutex<()> = Mutex::const_new(());

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

    let resolved_skin = match skin {
        Some(ref raw) if !raw.trim().is_empty() => match resolve_skin_name(&ctx, raw)? {
            Some(name) => Some(name),
            None => {
                command
                    .error_callback(
                        &ctx,
                        format!("Unknown skin `{raw}`. Use `/skin list` to browse skins."),
                        true,
                    )
                    .await?;
                return Ok(());
            }
        },
        _ => None,
    };

    let parsed_score_id = match score_url {
        Some(ref url_str) if attachment.is_none() => match parse_osu_score_url(url_str) {
            Some(id) => Some(id),
            None => {
                command
                    .error_callback(
                        &ctx,
                        "Could not parse score ID from `score_url`. Expected e.g. `https://osu.ppy.sh/scores/1234567890`.",
                        true,
                    )
                    .await?;
                return Ok(());
            }
        },
        _ => None,
    };

    command.defer(&ctx, false).await?;

    let user = command.user_id()?;
    if let Err(err_msg) = check_render_access(&ctx, user, command.guild_id, command.channel_id).await {
        command.error(&ctx, err_msg).await?;
        return Ok(());
    }

    let opts = RenderOptions {
        skin: resolved_skin,
        ..Default::default()
    };

    let replay_data = if let Some(ref att) = attachment {
        match prepare_attachment_replay_data(
            &ctx,
            att,
            command.id,
            command.channel_id,
            command.channel_id,
            user,
            command.username().ok().map(str::to_owned),
            opts,
        )
        .await
        {
            Ok(data) => data,
            Err(err_msg) => {
                command.error(&ctx, err_msg).await?;
                return Ok(());
            }
        }
    } else if let Some(score_id) = parsed_score_id {
        match prepare_score_by_id(
            &ctx,
            score_id,
            command.channel_id,
            command.channel_id,
            user,
            opts,
        )
        .await
        {
            Ok(ScorePrepareOutcome::Ready(data)) => *data,
            Ok(ScorePrepareOutcome::Unavailable(reason)) => {
                command.error(&ctx, reason).await?;
                return Ok(());
            }
            Err(err) => {
                tracing::warn!(?err, score_id, "failed to fetch score for screenshot");
                command
                    .error(&ctx, "Could not fetch that score from the osu! API.")
                    .await?;
                return Ok(());
            }
        }
    } else {
        unreachable!("validated attachment or score_url is present");
    };

    match take_replay_screenshot(&ctx, replay_data, secs, command.id, user).await {
        Ok((png_bytes, caption)) => {
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
    let mut score_id: Option<u64> = None;
    let mut raw_skin: Option<String> = None;

    for token in tokens {
        if let Some(v) = token.strip_prefix("skin=").or_else(|| token.strip_prefix("SKIN=")) {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                let _ = msg.error(&ctx, "`skin=` cannot be empty.").await;
                return;
            }
            raw_skin = Some(trimmed.to_owned());
        } else if let Some(id) = parse_osu_score_url(&token) {
            score_id = Some(id);
        } else if time_secs.is_none() {
            let time_tok = token
                .strip_prefix("time=")
                .or_else(|| token.strip_prefix("t="))
                .unwrap_or(&token);
            match TimePoints::parse_single(time_tok) {
                Ok(s) => time_secs = Some(s),
                Err(e) => {
                    let _ = msg.error(&ctx, format!("Invalid timestamp `{time_tok}`: {e}")).await;
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

    let skin = match raw_skin {
        Some(ref name) => match resolve_skin_name(&ctx, name) {
            Ok(Some(resolved)) => Some(resolved),
            Ok(None) => {
                let _ = msg
                    .error(
                        &ctx,
                        format!("Unknown skin `{name}`. Use `/skin list` or `!skinlist`."),
                    )
                    .await;
                return;
            }
            Err(_) => {
                let _ = msg.error(&ctx, "Failed to read skin list.").await;
                return;
            }
        },
        None => None,
    };

    let opts = RenderOptions {
        skin,
        ..Default::default()
    };

    let replay_data = if let Some(att) = msg.attachments.first() {
        match prepare_attachment_replay_data(
            &ctx,
            att,
            msg.id,
            msg.channel_id,
            msg.channel_id,
            msg.author.id,
            Some(msg.author.name.clone()),
            opts,
        )
        .await
        {
            Ok(data) => data,
            Err(err_msg) => {
                let _ = msg.error(&ctx, err_msg).await;
                return;
            }
        }
    } else if let Some(sid) = score_id {
        match prepare_score_by_id(&ctx, sid, msg.channel_id, msg.channel_id, msg.author.id, opts).await
        {
            Ok(ScorePrepareOutcome::Ready(data)) => *data,
            Ok(ScorePrepareOutcome::Unavailable(reason)) => {
                let _ = msg.error(&ctx, reason).await;
                return;
            }
            Err(err) => {
                tracing::warn!(?err, score_id = sid, "failed to fetch score for !ss");
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
            msg.channel_id,
            msg.author.id,
            opts,
        )
        .await
        {
            Ok(EmbedRenderOutcome::Enqueued { data, .. }) => *data,
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
                tracing::warn!(?err, "failed to process score embed for !ss");
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
                "Attach a `.osr` replay, pass an osu! score URL, or reply to a score embed.",
            )
            .await;
        return;
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
            if let Some(ref ph) = status_msg {
                let _ = ph.delete(&ctx).await;
            }
            let builder = MessageBuilder::new()
                .content(caption)
                .attachment(format!("screenshot_{secs}s.png"), png_bytes)
                .reply(msg.id);
            let _ = msg.channel_id.create_message(&ctx, &builder).await;
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
    let Some(hash) = beatmap_hash else {
        return Err("Missing the beatmap hash in the replay file.".to_owned());
    };

    let map = ctx.osu().beatmap().checksum(hash).await.map_err(|err| {
        tracing::warn!(?err, %hash, "failed to resolve beatmap for screenshot");
        "Failed to retrieve map from osu! API. Maybe it's unsubmitted?".to_owned()
    })?;

    let mapset_id = map.mapset_id;
    let map_label = match map.mapset {
        Some(ref ms) => format!("{} - {} [{}]", ms.artist, ms.title, map.version),
        None => format!("Beatmap #{} [{}]", map.map_id, map.version),
    };

    let config = BotConfig::get();
    let mut mapset_path = config.paths.songs();
    mapset_path.push(mapset_id.to_string());
    if !tokio::fs::try_exists(&mapset_path).await.unwrap_or(false) {
        download_mapset(ctx, mapset_id).await.map_err(|err| {
            tracing::warn!(?err, mapset_id, "failed to download mapset for screenshot");
            "Failed to download beatmapset. Mirrors might be down.".to_owned()
        })?;
    }

    let _guard = SCREENSHOT_LOCK.lock().await;

    let mut danser_bin = config.paths.danser().to_owned();
    danser_bin.push("danser");

    let mut settings_path = config.paths.danser().to_owned();
    settings_path.push(format!("settings/{user}.json"));
    let settings = if tokio::fs::try_exists(&settings_path).await.unwrap_or(false) {
        user.to_string()
    } else {
        "default".to_owned()
    };

    let canonical_osr = tokio::fs::canonicalize(replay_path).await.map_err(|err| {
        tracing::warn!(?err, "failed to canonicalize replay path for screenshot");
        "Failed to access stored replay file.".to_owned()
    })?;

    let out_name = format!("ss_{tx_id}");
    let mut out_png = config.paths.danser().to_owned();
    out_png.push(format!("screenshots/{out_name}.png"));

    let mut cmd = Command::new(&danser_bin);
    cmd.current_dir(config.paths.danser())
        .kill_on_drop(true)
        .arg("-noupdatecheck")
        .arg("-replay")
        .arg(&canonical_osr)
        .arg("-ss")
        .arg(secs.to_string())
        .arg("-settings")
        .arg(&settings)
        .arg("-out")
        .arg(&out_name);

    if let Some(skin_name) = skin {
        cmd.args(["-skin", skin_name]);
    }

    let output = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .map_err(|_| "danser screenshot timed out after 30s.".to_owned())?
        .map_err(|err| {
            tracing::warn!(?err, "failed to launch danser for screenshot");
            "Failed to run danser for screenshot.".to_owned()
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(status = %output.status, stderr = %stderr, "danser -ss failed");
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
