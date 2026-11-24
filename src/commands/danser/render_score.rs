use std::{
    fmt::Write as _,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use super::queue::send_queue_status;
use super::settings::resolve_skin_name;
use crate::{
    core::{
        replay_queue::{RenderOptions, ReplaySlim},
        BotConfig, Context, ReplayData, TimePoints,
    },
    database::BlacklistHit,
    util::{
        builder::{action_row, button, MessageBuilder},
        constants::GENERAL_ISSUE,
        interaction::InteractionCommand,
        Authored, ChannelExt, InteractionCommandExt, MessageExt,
    },
};
use command_macros::msg_command;
use eyre::{Context as _, Report, Result};
use osu_db::{Mode, Replay};
use rosu_v2::{
    error::OsuError,
    prelude::{GameMode, Score},
};
use tokio::{fs::File, io::AsyncWriteExt as _};
use twilight_interactions::command::CommandInputData;
use twilight_model::{
    channel::{
        message::{component::ButtonStyle, embed::Embed},
        Attachment, Message,
    },
    id::{
        marker::{ChannelMarker, GuildMarker, UserMarker},
        Id,
    },
    util::Timestamp,
};

const MAX_ACTIVE_RENDERS_PER_USER: usize = 3;
static OSR_FILE_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ParsedScoreUrl {
    /// `/scores/<id>` or bare numeric `<id>`
    Id(u64),
    /// `/scores/osu/<id>`
    Osu(u64),
    /// Non-standard score URL such as `/scores/taiko/<id>`
    UnsupportedMode,
}

/// Sanitize filenames so slashes and special characters don't break file paths.
pub fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other => other,
        })
        .collect()
}

/// Parse an osu! score URL or numeric ID while distinguishing mode-qualified URLs.
pub fn parse_osu_score_ref(input: &str) -> Option<ParsedScoreUrl> {
    let trimmed = input.trim().trim_matches(|c| c == '<' || c == '>');
    let without_query = trimmed.split(['?', '#']).next()?.trim_end_matches('/');

    if let Some((_, rest)) = without_query.split_once("://") {
        let lower = rest.to_ascii_lowercase();
        let path_after_host = rest.split_once('/')?.1;
        if !(lower.starts_with("osu.ppy.sh/")
            || lower.starts_with("lazer.ppy.sh/")
            || lower.starts_with("old.ppy.sh/"))
        {
            return None;
        }
        let after_scores = path_after_host
            .strip_prefix("scores/")
            .or_else(|| path_after_host.split_once("/scores/").map(|x| x.1))?;
        let mut segments = after_scores.split('/').filter(|s| !s.is_empty());
        let first = segments.next()?;
        if let Some(second) = segments.next() {
            if segments.next().is_some() {
                return None;
            }
            let id = second.parse::<u64>().ok()?;
            return match first.to_ascii_lowercase().as_str() {
                "osu" => Some(ParsedScoreUrl::Osu(id)),
                "taiko" | "fruits" | "catch" | "mania" => Some(ParsedScoreUrl::UnsupportedMode),
                _ => None,
            };
        }
        return first.parse::<u64>().ok().map(ParsedScoreUrl::Id);
    }

    if without_query.len() >= 6 && without_query.bytes().all(|b| b.is_ascii_digit()) {
        return without_query.parse::<u64>().ok().map(ParsedScoreUrl::Id);
    }

    None
}

/// Extract an osu! score ID from a URL or bare numeric ID, rejecting explicitly non-std URLs.
#[cfg(test)]
pub fn parse_osu_score_url(input: &str) -> Option<u64> {
    match parse_osu_score_ref(input)? {
        ParsedScoreUrl::Id(id) | ParsedScoreUrl::Osu(id) => Some(id),
        ParsedScoreUrl::UnsupportedMode => None,
    }
}

/// Validate an optional `score_url` when no `.osr` attachment is present.
pub(crate) fn parse_slash_score_url(
    score_url: Option<&str>,
    has_attachment: bool,
) -> std::result::Result<Option<ParsedScoreUrl>, &'static str> {
    let Some(url_str) = score_url.filter(|_| !has_attachment) else {
        return Ok(None);
    };
    match parse_osu_score_ref(url_str) {
        Some(ParsedScoreUrl::UnsupportedMode) => {
            Err("danser only accepts osu!standard plays, sorry :(")
        }
        Some(parsed) => Ok(Some(parsed)),
        None => Err(
            "Could not parse score ID from `score_url`. Expected e.g. `https://osu.ppy.sh/scores/1234567890`.",
        ),
    }
}

/// Resolve an optional skin override string against installed skins.
pub(crate) fn resolve_optional_skin(
    ctx: &Context,
    skin: Option<&str>,
    list_hint: &str,
) -> std::result::Result<Option<String>, String> {
    let Some(raw) = skin.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    match resolve_skin_name(ctx, raw) {
        Ok(Some(name)) => Ok(Some(name)),
        Ok(None) => Err(format!("Unknown skin `{raw}`. {list_hint}")),
        Err(_) => Err("Failed to read skin list.".to_owned()),
    }
}

/// Verify user blacklist, server blacklist, and allowed input channel.
pub async fn check_render_access(
    ctx: &Context,
    user_id: Id<UserMarker>,
    guild_id: Option<Id<GuildMarker>>,
    channel_id: Id<ChannelMarker>,
) -> std::result::Result<(), String> {
    match ctx.psql().check_blacklisted(user_id, guild_id).await {
        Ok(Some(BlacklistHit::User(reason_opt))) => {
            let mut msg = String::from("You are blacklisted from using render commands.");
            if let Some(reason) = reason_opt {
                let _ = write!(msg, " Reason: {reason}");
            }
            return Err(msg);
        }
        Ok(Some(BlacklistHit::Server(reason_opt))) => {
            let mut content = String::from("Seems like this server has been blacklisted.");
            if let Some(reason) = reason_opt {
                let _ = write!(content, " {reason}");
            }
            return Err(content);
        }
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(?err, "failed to check blacklist");
            return Err("Failed to check user/server status, try again later.".to_owned());
        }
    }

    if let Some(guild) = guild_id {
        let is_allowed = ctx
            .guild_settings(guild, |s| {
                s.input_channels.is_empty() || s.input_channels.contains(&channel_id)
            })
            .unwrap_or(true);

        if !is_allowed {
            return Err("This channel is not an allowed input channel for rendering.".to_owned());
        }
    }

    Ok(())
}

/// Enforce the per-user active/queued render cap (owners exempt).
pub async fn check_user_queue_limit(
    ctx: &Context,
    user_id: Id<UserMarker>,
) -> std::result::Result<(), String> {
    if BotConfig::get().owners.contains(&user_id) {
        return Ok(());
    }
    let active = ctx.replay_queue.count_user_jobs(user_id).await;
    if active >= MAX_ACTIVE_RENDERS_PER_USER {
        return Err(format!(
            "You already have {active} renders queued or running (limit: {MAX_ACTIVE_RENDERS_PER_USER}). Please wait or cancel one first."
        ));
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq)]
pub struct RawPrefixRenderArgs {
    pub start: u32,
    pub end: u32,
    pub pitch: Option<f64>,
    pub raw_skin: Option<String>,
    pub skip_intro: bool,
    pub offset_ms: Option<i32>,
    pub force_rerender: bool,
    pub score_id: Option<u64>,
    pub score_ref: Option<ParsedScoreUrl>,
}

pub(crate) fn split_quoted_tokens(input: &str) -> std::result::Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;

    for ch in input.chars() {
        match quote {
            Some(q) if ch == q => {
                quote = None;
            }
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
            }
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }

    if quote.is_some() {
        return Err("Unclosed quote in render arguments.".to_owned());
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

fn apply_score_ref_token(
    parsed: &mut RawPrefixRenderArgs,
    token: &str,
) -> std::result::Result<(), String> {
    let Some(score_ref) = parse_osu_score_ref(token) else {
        return Ok(());
    };
    match score_ref {
        ParsedScoreUrl::Id(id) | ParsedScoreUrl::Osu(id) => {
            parsed.score_id = Some(id);
            parsed.score_ref = Some(score_ref);
            Ok(())
        }
        ParsedScoreUrl::UnsupportedMode => Err("Only osu!standard scores can be rendered.".to_owned()),
    }
}

/// Parse prefix render options (`start=`, `end=`, `pitch=`, `skin=`, `skip`, `offset=`, `force`, score URL).
pub fn parse_raw_prefix_args(args: &str) -> std::result::Result<RawPrefixRenderArgs, String> {
    let tokens = split_quoted_tokens(args)?;
    let mut parsed = RawPrefixRenderArgs::default();

    for token in tokens {
        if let Some((key, v)) = token.split_once('=') {
            match key.to_ascii_lowercase().as_str() {
                "start" => {
                    parsed.start =
                        TimePoints::parse_single(v).map_err(|e| format!("Invalid `start`: {e}"))?;
                }
                "end" => {
                    parsed.end =
                        TimePoints::parse_single(v).map_err(|e| format!("Invalid `end`: {e}"))?;
                }
                "pitch" => {
                    let p = v.parse::<f64>().map_err(|_| {
                        "Invalid `pitch`: must be a positive number (e.g. `pitch=1.5`).".to_owned()
                    })?;
                    if !p.is_finite() || p <= 0.0 {
                        return Err(
                            "Invalid `pitch`: must be a positive number (e.g. `pitch=1.5`)."
                                .to_owned(),
                        );
                    }
                    parsed.pitch = Some(p.clamp(0.5, 5.0));
                }
                "skin" => {
                    let trimmed = v.trim();
                    if trimmed.is_empty() {
                        return Err("`skin=` cannot be empty.".to_owned());
                    }
                    parsed.raw_skin = Some(trimmed.to_owned());
                }
                "offset" => {
                    let off = v.parse::<i32>().map_err(|_| {
                        "Invalid `offset`: integer milliseconds expected (e.g. `offset=-15`)."
                            .to_owned()
                    })?;
                    parsed.offset_ms = Some(off.clamp(-5000, 5000));
                }
                "skip" | "skip_intro" => {
                    if v.eq_ignore_ascii_case("true") || v == "1" {
                        parsed.skip_intro = true;
                    } else if v.eq_ignore_ascii_case("false") || v == "0" {
                        parsed.skip_intro = false;
                    } else {
                        return Err("Invalid `skip`: expected `true` or `false`.".to_owned());
                    }
                }
                "force" | "rerender" => {
                    if v.eq_ignore_ascii_case("true") || v == "1" {
                        parsed.force_rerender = true;
                    } else if v.eq_ignore_ascii_case("false") || v == "0" {
                        parsed.force_rerender = false;
                    } else {
                        return Err("Invalid `force`: expected `true` or `false`.".to_owned());
                    }
                }
                _ => apply_score_ref_token(&mut parsed, &token)?,
            }
            continue;
        }

        let lower = token.to_ascii_lowercase();
        if matches!(lower.as_str(), "skip" | "skip_intro") {
            parsed.skip_intro = true;
        } else if matches!(lower.as_str(), "force" | "rerender") {
            parsed.force_rerender = true;
        } else {
            apply_score_ref_token(&mut parsed, &token)?;
        }
    }

    if parsed.end > 0 && parsed.end <= parsed.start {
        return Err("`end` timestamp must be greater than `start` timestamp.".to_owned());
    }

    Ok(parsed)
}

/// Parse prefix render options and validate any `skin=` against installed skins.
pub fn parse_prefix_render_args(
    ctx: &Context,
    args: &str,
) -> std::result::Result<(RenderOptions, Option<ParsedScoreUrl>), String> {
    let raw = parse_raw_prefix_args(args)?;
    let skin = resolve_optional_skin(
        ctx,
        raw.raw_skin.as_deref(),
        "Use `/skin list` or `!skinlist` to see available skins.",
    )?;

    Ok((
        RenderOptions {
            start: raw.start,
            end: raw.end,
            pitch: raw.pitch,
            skin,
            skip_intro: raw.skip_intro,
            offset_ms: raw.offset_ms,
            force_rerender: raw.force_rerender,
            skip_info_embed: false,
        },
        raw.score_ref,
    ))
}

fn build_queue_added_message(job_id: u64, note: Option<&str>) -> MessageBuilder<'static> {
    let mut text = String::from("Replay has been added to the queue!");
    if let Some(extra) = note {
        let _ = write!(text, "\n⚠️ {extra}");
    }

    let cancel_btn = action_row(vec![button(
        format!("render_cancel:{job_id}"),
        "Cancel",
        ButtonStyle::Danger,
    )]);

    MessageBuilder::new().embed(text).components(vec![cancel_btn])
}

/// Allocate a job ID, update the slash command response with the queue card, and enqueue the job.
pub async fn enqueue_replay_interaction(
    ctx: Arc<Context>,
    command: &InteractionCommand,
    mut replay_data: ReplayData,
    note: Option<&str>,
) {
    let job_id = ctx.replay_queue.allocate_job_id();
    replay_data.job_id = job_id;
    let builder = build_queue_added_message(job_id, note);
    let queue_msg = command.update(&ctx, &builder).await.ok();
    finalize_enqueue(ctx, replay_data, queue_msg).await;
}

/// Allocate a job ID, post a queue card in `channel_id`, and enqueue the job.
pub async fn enqueue_replay_in_channel(
    ctx: Arc<Context>,
    channel_id: Id<ChannelMarker>,
    mut replay_data: ReplayData,
    note: Option<&str>,
) {
    let job_id = ctx.replay_queue.allocate_job_id();
    replay_data.job_id = job_id;
    let builder = build_queue_added_message(job_id, note);
    let queue_msg = channel_id.create_message(&ctx, &builder).await.ok();
    finalize_enqueue(ctx, replay_data, queue_msg).await;
}

/// Attach `queue_message` to `replay_data`, push it onto `ReplayQueue`, and start status updates if idle.
async fn finalize_enqueue(
    ctx: Arc<Context>,
    mut replay_data: ReplayData,
    queue_msg: Option<Message>,
) {
    let input_channel = replay_data.input_channel;
    let user = replay_data.user;
    let osr_path = replay_data.path.clone();
    let queue_msg_id = queue_msg.map(|m| (m.id, m.channel_id));
    replay_data.queue_message = queue_msg_id;

    let max_per_user =
        (!BotConfig::get().owners.contains(&user)).then_some(MAX_ACTIVE_RENDERS_PER_USER);
    match ctx.replay_queue.push(replay_data, max_per_user).await {
        Ok((_, _)) => {
            let ctx_clone = Arc::clone(&ctx);
            tokio::spawn(async move {
                let _ = send_queue_status(ctx_clone, input_channel).await;
            });
        }
        Err(active) => {
            let _ = tokio::fs::remove_file(&osr_path).await;
            let err_msg = format!(
                "You already have {active} renders queued or running (limit: {MAX_ACTIVE_RENDERS_PER_USER}). Please wait or cancel one first."
            );
            if let Some((msg_id, ch_id)) = queue_msg_id {
                let builder = MessageBuilder::new().embed(err_msg).components(vec![]);
                let _ = (msg_id, ch_id).update(&ctx, &builder).await;
            } else {
                let _ = input_channel.error(&ctx, err_msg).await;
            }
        }
    }
}

async fn fetch_beatmap_user_scores(
    ctx: &Context,
    beatmap_id: u32,
    user_id: u32,
) -> Result<Vec<Score>> {
    crate::osu_call!(ctx, |osu| {
        osu.beatmap_user_scores(beatmap_id, user_id)
            .mode(GameMode::Osu)
    })
    .context("failed to get user scores on beatmap")
}

async fn find_score_to_render(
    ctx: &Context,
    user_id: u32,
    beatmap_id: u32,
    timestamp: Option<Timestamp>,
) -> Result<(Option<Score>, bool)> {
    if let Some(timestamp) = timestamp {
        let ts_unix = timestamp.as_secs();

        // check recents (osu!standard only)
        let recent_scores = crate::osu_call!(ctx, |osu| {
            osu.user_scores(user_id)
                .recent()
                .mode(GameMode::Osu)
                .include_fails(false)
                .limit(100)
        })
        .context("failed to get recent scores")?;

        if let Some(score) = recent_scores.into_iter().find(|score| {
            score.map_id == beatmap_id && (score.ended_at.unix_timestamp() - ts_unix).abs() <= 3
        }) {
            return Ok((Some(score), false));
        }

        // check tops (osu!standard only)
        let top_scores = crate::osu_call!(ctx, |osu| {
            osu.user_scores(user_id)
                .best()
                .mode(GameMode::Osu)
                .limit(100)
        })
        .context("failed to get top scores")?;

        if let Some(score) = top_scores.into_iter().find(|score| {
            score.map_id == beatmap_id && (score.ended_at.unix_timestamp() - ts_unix).abs() <= 3
        }) {
            return Ok((Some(score), false));
        }

        tracing::debug!("Fetching map scores for user {user_id} on beatmap {beatmap_id}");
        let mut map_scores = fetch_beatmap_user_scores(ctx, beatmap_id, user_id).await?;
        tracing::debug!("Got {} map scores", map_scores.len());
        if let Some(pos) = map_scores
            .iter()
            .position(|score| (score.ended_at.unix_timestamp() - ts_unix).abs() <= 3)
        {
            return Ok((Some(map_scores.swap_remove(pos)), false));
        }

        let fallback = map_scores.into_iter().next();
        let used_fallback = fallback.is_some();
        Ok((fallback, used_fallback))
    } else {
        // No timestamp (<current embed) — pick best score on this map
        tracing::debug!(
            user_id,
            beatmap_id,
            "no timestamp, fetching beatmap user scores"
        );

        let map_scores = fetch_beatmap_user_scores(ctx, beatmap_id, user_id).await?;
        Ok((map_scores.into_iter().next(), false))
    }
}

pub enum ScorePrepareOutcome {
    Ready(Box<ReplayData>),
    Unavailable(&'static str),
}

pub(crate) async fn prepare_score_replay_data(
    ctx: &Context,
    score_to_render: Score,
    input_channel: Id<ChannelMarker>,
    user: Id<UserMarker>,
    opts: RenderOptions,
) -> Result<ScorePrepareOutcome> {
    if score_to_render.mode != GameMode::Osu {
        return Ok(ScorePrepareOutcome::Unavailable(
            "danser only accepts osu!standard plays, sorry :(",
        ));
    }

    let score_id = score_to_render.id;
    let mut replay_bytes_res = crate::osu_call!(ctx, |osu| osu.replay_raw(score_id));
    if replay_bytes_res.is_err() {
        if let Some(legacy_id) = score_to_render.legacy_score_id {
            replay_bytes_res = crate::osu_call!(ctx, |osu| {
                osu.replay_raw(legacy_id).mode(score_to_render.mode)
            });
        }
    }

    let replay_bytes = match replay_bytes_res {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(?err, score_id, "failed to download replay from osu! API");
            return Ok(ScorePrepareOutcome::Unavailable(
                "Couldn't download the replay for this score. The osu! API says 'Replay not available'.",
            ));
        }
    };

    let mut replay = match Replay::from_bytes(&replay_bytes) {
        Ok(replay) => ReplaySlim::from(replay),
        Err(err) => return Err(Report::new(err).wrap_err("failed to parse replay")),
    };
    replay.grade = score_to_render.grade;
    if score_to_render.set_on_lazer {
        replay.accuracy_override = Some((score_to_render.accuracy * 100.0).round() / 100.0);
        if replay.score == 0 {
            replay.score = score_to_render.score;
        }
        if score_to_render.maximum_statistics.large_tick_hit > 0
            || score_to_render.statistics.large_tick_hit > 0
        {
            replay.large_tick_hits = Some(score_to_render.statistics.large_tick_hit);
        }
        if score_to_render.maximum_statistics.small_tick_hit > 0
            || score_to_render.statistics.small_tick_hit > 0
        {
            replay.small_tick_hits = Some(score_to_render.statistics.small_tick_hit);
        }
        if score_to_render.maximum_statistics.slider_tail_hit > 0
            || score_to_render.statistics.slider_tail_hit > 0
        {
            replay.slider_end_hits = Some(score_to_render.statistics.slider_tail_hit);
        }
    }

    let (fetched_user, fetched_map) = tokio::join!(
        async {
            if score_to_render.user.is_none() {
                crate::osu_call!(ctx, |osu| osu.user(score_to_render.user_id)).ok()
            } else {
                None
            }
        },
        async {
            if score_to_render.mapset.is_none() || score_to_render.map.is_none() {
                crate::osu_call!(ctx, |osu| osu.beatmap().map_id(score_to_render.map_id)).ok()
            } else {
                None
            }
        },
    );

    let osu_user = score_to_render
        .user
        .as_ref()
        .map(|u| u.username.as_str())
        .or_else(|| fetched_user.as_ref().map(|u| u.username.as_str()))
        .unwrap_or("unknown user");

    let map_title = score_to_render
        .mapset
        .as_ref()
        .map(|ms| ms.title.as_str())
        .or_else(|| fetched_map.as_ref().and_then(|m| m.mapset.as_ref().map(|ms| ms.title.as_str())))
        .unwrap_or("unknown map");

    let diff_name = score_to_render
        .map
        .as_ref()
        .map(|m| m.version.as_str())
        .or_else(|| fetched_map.as_ref().map(|m| m.version.as_str()))
        .unwrap_or("unknown diff");

    let mut path = BotConfig::get().paths.downloads().to_owned();
    let safe_user = sanitize_filename(osu_user);
    let safe_title = sanitize_filename(map_title);
    let safe_diff = sanitize_filename(diff_name);
    let seq = OSR_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let desc = format!("{safe_user} - {safe_title} [{safe_diff}]");
    let safe_desc = clamp_replay_stem(&desc);
    path.push(format!("{seq}_{user}_{score_id}_{safe_desc}.osr"));

    tokio::fs::write(&path, replay_bytes)
        .await
        .context("failed to write into replay file")?;

    let data = ReplayData::from_options(
        input_channel,
        path,
        replay,
        user,
        score_to_render.set_on_lazer,
        opts,
    );

    Ok(ScorePrepareOutcome::Ready(Box::new(data)))
}

fn clamp_replay_stem(raw: &str) -> &str {
    let clamped = crate::core::replay_queue::process::clamp_utf8_bytes(raw, 80);
    if clamped.is_empty() {
        "replay"
    } else {
        clamped
    }
}

/// Download a Discord `.osr` attachment, validate osu!standard mode, save to disk, and build `ReplayData`.
pub async fn prepare_attachment_replay_data(
    ctx: &Context,
    att: &Attachment,
    tx_id: impl std::fmt::Display,
    input_channel: Id<ChannelMarker>,
    user: Id<UserMarker>,
    opts: RenderOptions,
) -> std::result::Result<ReplayData, String> {
    const MAX_OSR_BYTES: u64 = 10 * 1024 * 1024;
    if !matches!(att.filename.rsplit('.').next(), Some(ext) if ext.eq_ignore_ascii_case("osr")) {
        return Err("The attachment must be a `.osr` file!".to_owned());
    }
    if att.size > MAX_OSR_BYTES {
        return Err("The `.osr` file is too large (max 10 MB).".to_owned());
    }

    let bytes = ctx
        .client()
        .get_discord_attachment_limited(att, MAX_OSR_BYTES as usize)
        .await
        .map_err(|err| {
            tracing::error!(?err, "failed to download replay attachment");
            "Failed to download attachment.".to_owned()
        })?;

    let replay = Replay::from_bytes(&bytes).map_err(|err| {
        tracing::warn!(?err, "failed to parse .osr attachment");
        "Failed to parse the `.osr` file. Is it a valid replay?".to_owned()
    })?;

    if replay.mode != Mode::Standard {
        return Err("danser only accepts osu!standard plays, sorry :(".to_owned());
    }

    let mut replay_file = BotConfig::get().paths.downloads();
    let safe_filename = sanitize_filename(&att.filename);
    let raw_stem = safe_filename
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(&safe_filename);
    let final_stem = clamp_replay_stem(raw_stem);
    replay_file.push(format!("{tx_id}_{final_stem}.osr"));

    let mut file = File::create(&replay_file).await.map_err(|err| {
        tracing::error!(?err, ?replay_file, "failed to create replay file");
        "Failed to store replay file.".to_owned()
    })?;
    if let Err(err) = file.write_all(&bytes).await {
        tracing::error!(?err, ?replay_file, "failed writing replay file");
        let _ = tokio::fs::remove_file(&replay_file).await;
        return Err("Failed to store replay file.".to_owned());
    }

    let is_lazer = crate::core::replay_queue::is_lazer_replay(replay.version);
    let data = ReplayData::from_options(
        input_channel,
        replay_file,
        ReplaySlim::from(replay),
        user,
        is_lazer,
        opts,
    );
    Ok(data)
}

pub(crate) async fn resolve_slash_replay_input(
    ctx: &Context,
    attachment: Option<&Attachment>,
    score_ref: Option<ParsedScoreUrl>,
    interaction_id: impl std::fmt::Display,
    channel_id: Id<ChannelMarker>,
    user: Id<UserMarker>,
    opts: RenderOptions,
) -> std::result::Result<ReplayData, String> {
    if let Some(att) = attachment {
        return prepare_attachment_replay_data(ctx, att, interaction_id, channel_id, user, opts)
            .await;
    }
    if let Some(sr) = score_ref {
        return match prepare_score_by_id(ctx, sr, channel_id, user, opts).await {
            Ok(ScorePrepareOutcome::Ready(data)) => Ok(*data),
            Ok(ScorePrepareOutcome::Unavailable(reason)) => Err(reason.to_owned()),
            Err(err) => {
                tracing::warn!(?err, ?sr, "failed to fetch score by ID");
                Err(
                    "Could not fetch that score from the osu! API. Make sure the URL/ID is valid."
                        .to_owned(),
                )
            }
        };
    }
    unreachable!("validated attachment or score_url is present")
}

pub(crate) async fn resolve_prefix_replay_input(
    ctx: &Arc<Context>,
    msg: &Message,
    score_ref: Option<ParsedScoreUrl>,
    opts: RenderOptions,
    missing_msg: &'static str,
) -> std::result::Result<(ReplayData, Option<&'static str>), String> {
    if let Some(att) = msg.attachments.first() {
        let data =
            prepare_attachment_replay_data(ctx, att, msg.id, msg.channel_id, msg.author.id, opts)
                .await?;
        return Ok((data, None));
    }
    if let Some(sid) = score_ref {
        return match prepare_score_by_id(ctx, sid, msg.channel_id, msg.author.id, opts).await {
            Ok(ScorePrepareOutcome::Ready(data)) => Ok((*data, None)),
            Ok(ScorePrepareOutcome::Unavailable(reason)) => Err(reason.to_owned()),
            Err(err) => {
                tracing::warn!(?err, score_id = ?sid, "failed to fetch score by URL/ID");
                Err("Could not fetch that score from the osu! API.".to_owned())
            }
        };
    }
    if let Some(replied) = msg.referenced_message.as_deref() {
        return match render_score_from_message(
            Arc::clone(ctx),
            replied,
            msg.channel_id,
            msg.author.id,
            opts,
        )
        .await
        {
            Ok(EmbedRenderOutcome::Enqueued { data, warning }) => Ok((*data, warning)),
            Ok(EmbedRenderOutcome::NotScoreEmbed) => {
                Err("Could not recognize a Bathbot/Owobot score embed on that message.".to_owned())
            }
            Ok(EmbedRenderOutcome::ReplayUnavailable(reason)) => Err(reason.to_owned()),
            Err(err) => {
                tracing::warn!(?err, "failed to process score embed");
                Err("Failed to process the replied score embed.".to_owned())
            }
        };
    }
    Err(missing_msg.to_owned())
}

#[msg_command(name = "Render score", dm_permission = false)]
async fn render_from_msg(ctx: Arc<Context>, mut command: InteractionCommand) -> Result<()> {
    let user = command.user_id()?;
    if command.guild_id.is_none() {
        command
            .error(&ctx, "This command cannot be used in DMs.")
            .await?;
        return Ok(());
    }

    if let Err(msg) = check_render_access(&ctx, user, command.guild_id, command.channel_id).await {
        command.error(&ctx, msg).await?;
        return Ok(());
    }

    if let Err(msg) = check_user_queue_limit(&ctx, user).await {
        command.error(&ctx, msg).await?;
        return Ok(());
    }

    let input_data = command.input_data();

    let (user_id, beatmap_id, timestamp) = match parse_embed_input_data(input_data) {
        Some(ParsedEmbed {
            user_id,
            beatmap_id,
            timestamp,
        }) => (user_id, beatmap_id, timestamp),
        None => {
            let content = "The command can only be used on Bathbot.rs embeds!";
            command.error(&ctx, content).await?;
            return Ok(());
        }
    };

    let (score_opt, used_fallback) =
        match find_score_to_render(&ctx, user_id, beatmap_id, timestamp).await {
            Ok(res) => res,
            Err(err) => {
                let _ = command.error(&ctx, GENERAL_ISSUE).await;
                return Err(err);
            }
        };
    let score_to_render = match score_opt {
        Some(score) => score,
        None => {
            let content = "Couldn't find the replay for this score (not in recents, tops, or map bests).";
            command.error(&ctx, content).await?;
            return Ok(());
        }
    };

    let replay_data = match prepare_score_replay_data(
        &ctx,
        score_to_render,
        command.channel_id,
        user,
        RenderOptions::default(),
    )
    .await
    {
        Ok(ScorePrepareOutcome::Ready(data)) => *data,
        Ok(ScorePrepareOutcome::Unavailable(reason)) => {
            command.error(&ctx, reason).await?;
            return Ok(());
        }
        Err(err) => {
            let _ = command.error(&ctx, GENERAL_ISSUE).await;
            return Err(err);
        }
    };

    let warning = used_fallback
        .then_some("Exact play timestamp wasn't found; queued your best score on this beatmap instead.");

    enqueue_replay_interaction(ctx, &command, replay_data, warning).await;
    Ok(())
}

pub enum EmbedRenderOutcome {
    Enqueued {
        data: Box<ReplayData>,
        warning: Option<&'static str>,
    },
    NotScoreEmbed,
    ReplayUnavailable(&'static str),
}

pub async fn render_score_from_message(
    ctx: Arc<Context>,
    message: &Message,
    input_channel: Id<ChannelMarker>,
    user: Id<UserMarker>,
    opts: RenderOptions,
) -> Result<EmbedRenderOutcome> {
    let Some(embed) = message.embeds.first() else {
        return Ok(EmbedRenderOutcome::NotScoreEmbed);
    };

    render_score_from_embed(ctx, embed, input_channel, user, opts).await
}

pub async fn render_score_from_embed(
    ctx: Arc<Context>,
    embed: &Embed,
    input_channel: Id<ChannelMarker>,
    user: Id<UserMarker>,
    opts: RenderOptions,
) -> Result<EmbedRenderOutcome> {
    tracing::debug!("render_score_from_embed: entered for map/user");

    let Some(ParsedEmbed {
        user_id,
        beatmap_id,
        timestamp,
    }) = parse_embed(embed)
    else {
        tracing::debug!("render_score_from_embed: parse_embed returned None");
        return Ok(EmbedRenderOutcome::NotScoreEmbed);
    };

    let (score_opt, used_fallback) = find_score_to_render(&ctx, user_id, beatmap_id, timestamp).await?;
    let score_to_render = match score_opt {
        Some(s) => s,
        None => {
            return Ok(EmbedRenderOutcome::ReplayUnavailable(
                "Couldn't find the replay for this score (not in recents, tops, or map bests).",
            ));
        }
    };

    let data = match prepare_score_replay_data(
        &ctx,
        score_to_render,
        input_channel,
        user,
        opts,
    )
    .await?
    {
        ScorePrepareOutcome::Ready(data) => data,
        ScorePrepareOutcome::Unavailable(reason) => {
            return Ok(EmbedRenderOutcome::ReplayUnavailable(reason));
        }
    };

    let warning = used_fallback
        .then_some("Exact play timestamp wasn't found; queued your best score on this beatmap instead.");

    Ok(EmbedRenderOutcome::Enqueued { data, warning })
}

/// Fetch a score by osu! score reference and prepare a `ReplayData` entry for rendering.
pub async fn prepare_score_by_id(
    ctx: &Context,
    score_ref: ParsedScoreUrl,
    input_channel: Id<ChannelMarker>,
    user: Id<UserMarker>,
    opts: RenderOptions,
) -> Result<ScorePrepareOutcome> {
    let (score_id, prefer_osu_mode) = match score_ref {
        ParsedScoreUrl::Osu(id) => (id, true),
        ParsedScoreUrl::Id(id) => (id, false),
        ParsedScoreUrl::UnsupportedMode => {
            return Ok(ScorePrepareOutcome::Unavailable(
                "danser only accepts osu!standard plays, sorry :(",
            ));
        }
    };

    let fetch_score = |use_osu_mode: bool| async move {
        if use_osu_mode {
            crate::osu_call!(ctx, |osu| osu.score(score_id).mode(GameMode::Osu))
        } else {
            crate::osu_call!(ctx, |osu| osu.score(score_id))
        }
    };

    let mut res = fetch_score(prefer_osu_mode).await;
    if matches!(res, Err(OsuError::NotFound)) {
        res = fetch_score(!prefer_osu_mode).await;
    }

    let score = res.with_context(|| format!("failed to fetch osu! score {score_id}"))?;
    prepare_score_replay_data(ctx, score, input_channel, user, opts).await
}

struct ParsedEmbed {
    user_id: u32,
    beatmap_id: u32,
    timestamp: Option<Timestamp>,
}

fn parse_embed(embed: &Embed) -> Option<ParsedEmbed> {
    let user_url = embed.author.as_ref().and_then(|a| a.url.as_ref())?;
    let user_clean = user_url.split(['?', '#']).next()?.trim_end_matches('/');
    let mut segs = user_clean.split('/').filter(|s| !s.is_empty());
    let mut user_id: Option<u32> = None;
    while let Some(seg) = segs.next() {
        if seg == "users" || seg == "u" {
            user_id = segs.next().and_then(|id| id.parse::<u32>().ok());
            break;
        }
    }
    let user_id = user_id?;

    let beatmap_url = embed.url.as_ref()?;
    let beatmap_id = if let Some((_, after_b)) = beatmap_url.split_once("/b/") {
        let id_str = after_b.split(['?', '#', '/']).next()?;
        id_str.parse::<u32>().ok()?
    } else if beatmap_url.contains("/beatmaps/") {
        let clean = beatmap_url.split(['?', '#']).next()?.trim_end_matches('/');
        clean.split('/').next_back().and_then(|id| id.parse::<u32>().ok())?
    } else if beatmap_url.contains("/beatmapsets/") {
        let (_, fragment) = beatmap_url.split_once('#')?;
        let clean_frag = fragment.split('?').next()?.trim_end_matches('/');
        clean_frag
            .split('/')
            .next_back()
            .and_then(|id| id.parse::<u32>().ok())?
    } else {
        return None;
    };

    let timestamp = embed
        .timestamp
        .or_else(|| get_timestamp_from_minimized_embed(embed));

    Some(ParsedEmbed {
        user_id,
        beatmap_id,
        timestamp,
    })
}

fn get_timestamp_from_minimized_embed(embed: &Embed) -> Option<Timestamp> {
    let field = embed.fields.first()?;

    let discord_timestamp = field.name.rsplit('\t').next()?.trim();

    let inner = discord_timestamp.strip_prefix("<t:")?;
    let actual_timestamp_value = inner
        .strip_suffix(":R>")
        .or_else(|| inner.split(':').next())?;

    let timestamp_value_as_int = actual_timestamp_value.parse().ok()?;

    Timestamp::from_secs(timestamp_value_as_int).ok()
}

fn parse_embed_input_data(input_: CommandInputData<'_>) -> Option<ParsedEmbed> {
    let embed = input_
        .resolved
        .as_ref()
        .and_then(|resolved| resolved.messages.values().next())
        .and_then(|msg| msg.embeds.first())?;

    parse_embed(embed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_osu_score_url_variants() {
        assert_eq!(parse_osu_score_url("https://osu.ppy.sh/scores/1234567890"), Some(1234567890));
        assert_eq!(parse_osu_score_url("<https://osu.ppy.sh/scores/osu/987654321?foo=bar>"), Some(987654321));
        assert_eq!(parse_osu_score_url("https://lazer.ppy.sh/scores/4455667788/"), Some(4455667788));
        assert_eq!(parse_osu_score_url("1234567"), Some(1234567));
        assert_eq!(parse_osu_score_url("https://osu.ppy.sh/beatmaps/123456"), None);
        assert_eq!(parse_osu_score_url("12345"), None);

        assert_eq!(
            parse_osu_score_ref("https://osu.ppy.sh/scores/osu/987654321?foo=bar"),
            Some(ParsedScoreUrl::Osu(987654321))
        );
        assert_eq!(
            parse_osu_score_ref("https://osu.ppy.sh/scores/taiko/123456"),
            Some(ParsedScoreUrl::UnsupportedMode)
        );
        assert_eq!(
            parse_osu_score_ref("https://osu.ppy.sh/scores/mania/123456"),
            Some(ParsedScoreUrl::UnsupportedMode)
        );
    }

    #[test]
    fn test_parse_raw_prefix_args_supports_quotes_and_all_flags() {
        let parsed = parse_raw_prefix_args(
            "https://osu.ppy.sh/scores/55667788 start=1:10 end=2:00 pitch=1.5 skin=\"Aristia +1.0\" offset=-25 skip force",
        )
        .expect("valid args");

        assert_eq!(parsed.score_id, Some(55667788));
        assert_eq!(parsed.start, 70);
        assert_eq!(parsed.end, 120);
        assert_eq!(parsed.pitch, Some(1.5));
        assert_eq!(parsed.raw_skin.as_deref(), Some("Aristia +1.0"));
        assert_eq!(parsed.offset_ms, Some(-25));
        assert!(parsed.skip_intro);
        assert!(parsed.force_rerender);

        let mixed = parse_raw_prefix_args(
            "START=0:10 PITCH=1.2 SKIN=\"My Skin\" OFFSET=10 https://osu.ppy.sh/scores/osu/11223344",
        )
        .expect("mixed case args");
        assert_eq!(mixed.start, 10);
        assert_eq!(mixed.pitch, Some(1.2));
        assert_eq!(mixed.raw_skin.as_deref(), Some("My Skin"));
        assert_eq!(mixed.offset_ms, Some(10));
        assert_eq!(mixed.score_ref, Some(ParsedScoreUrl::Osu(11223344)));

        assert!(parse_raw_prefix_args("https://osu.ppy.sh/scores/fruits/123456").is_err());
        assert!(parse_raw_prefix_args("start=2:00 end=1:00").is_err());
        assert!(parse_raw_prefix_args("skin=\"unclosed").is_err());
    }

    #[test]
    fn test_parse_embed_handles_queries_and_trailing_slashes() {
        use twilight_model::channel::message::embed::{EmbedAuthor, EmbedField};
        let mut embed = Embed {
            author: Some(EmbedAuthor {
                icon_url: None,
                name: "Player".to_owned(),
                proxy_icon_url: None,
                url: Some("https://osu.ppy.sh/users/124493/osu?mode=osu".to_owned()),
            }),
            color: None,
            description: None,
            fields: vec![EmbedField {
                inline: false,
                name: "Map\t<t:1700000000:R>".to_owned(),
                value: String::new(),
            }],
            footer: None,
            image: None,
            kind: "rich".to_owned(),
            provider: None,
            thumbnail: None,
            timestamp: None,
            title: None,
            url: Some("https://osu.ppy.sh/beatmapsets/999?mode=osu#osu/88888/".to_owned()),
            video: None,
        };
        let parsed = parse_embed(&embed).expect("valid embed");
        assert_eq!(parsed.user_id, 124493);
        assert_eq!(parsed.beatmap_id, 88888);
        assert_eq!(parsed.timestamp.unwrap().as_secs(), 1_700_000_000);

        embed.url = Some("https://osu.ppy.sh/beatmaps/77777?foo=bar/".to_owned());
        let parsed2 = parse_embed(&embed).expect("beatmaps query");
        assert_eq!(parsed2.beatmap_id, 77777);
    }
}
