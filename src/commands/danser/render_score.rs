use std::{fmt::Write as _, sync::Arc};

use super::queue::send_queue_status;
use super::settings::resolve_skin_name;
use crate::{
    core::{
        replay_queue::{RenderOptions, ReplaySlim},
        BotConfig, Context, ReplayData, TimePoints,
    },
    util::{
        builder::MessageBuilder, interaction::InteractionCommand, Authored, InteractionCommandExt,
    },
};
use command_macros::msg_command;
use eyre::{Context as _, Report, Result};
use osu_db::Replay;
use rosu_v2::prelude::{GameMode, Score};
use twilight_interactions::command::CommandInputData;
use twilight_model::{
    channel::{
        message::{
            component::{ActionRow, Button, ButtonStyle},
            embed::Embed,
            Component,
        },
        Message,
    },
    id::{
        marker::{ChannelMarker, GuildMarker, UserMarker},
        Id,
    },
    util::Timestamp,
};

const MAX_ACTIVE_RENDERS_PER_USER: usize = 3;

/// Sanitize filenames so slashes and special characters don't break file paths.
pub fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other => other,
        })
        .collect()
}

/// Extract an osu! score ID from a URL (e.g. `https://osu.ppy.sh/scores/123456` or
/// `https://osu.ppy.sh/scores/osu/123456`) or a standalone numeric ID (`>= 6` digits).
pub fn parse_osu_score_url(input: &str) -> Option<u64> {
    let trimmed = input.trim().trim_matches(|c| c == '<' || c == '>');
    let without_query = trimmed.split(&['?', '#'][..]).next()?.trim_end_matches('/');

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
            return second.parse::<u64>().ok();
        }
        return first.parse::<u64>().ok();
    }

    if without_query.len() >= 6 && without_query.bytes().all(|b| b.is_ascii_digit()) {
        return without_query.parse::<u64>().ok();
    }

    None
}

/// Verify user blacklist, server blacklist, and allowed input channel; returns resolved output channel.
pub async fn check_render_access(
    ctx: &Context,
    user_id: Id<UserMarker>,
    guild_id: Option<Id<GuildMarker>>,
    channel_id: Id<ChannelMarker>,
) -> std::result::Result<Id<ChannelMarker>, String> {
    match ctx.psql().is_user_blacklisted(user_id).await {
        Ok((true, reason_opt)) => {
            let mut msg = String::from("You are blacklisted from using render commands.");
            if let Some(reason) = reason_opt {
                let _ = write!(msg, " Reason: {reason}");
            }
            return Err(msg);
        }
        Ok((false, _)) => {}
        Err(err) => {
            tracing::warn!(?err, "failed to check user blacklist");
            return Err("Failed to check user status, try again later.".to_owned());
        }
    }

    if let Some(guild) = guild_id {
        match ctx.psql()._is_server_blacklisted(guild).await {
            Ok((true, reason_opt)) => {
                let mut content = String::from("Seems like this server has been blacklisted.");
                if let Some(reason) = reason_opt {
                    let _ = write!(content, " {reason}");
                }
                return Err(content);
            }
            Ok((false, _)) => {}
            Err(err) => {
                tracing::warn!(?err, "failed to check server blacklist");
                return Err("Failed to check server status, try again later.".to_owned());
            }
        }

        let is_allowed = ctx
            .guild_settings(guild, |s| {
                s.input_channels.is_empty() || s.input_channels.contains(&channel_id)
            })
            .unwrap_or(true);

        if !is_allowed {
            return Err("This channel is not an allowed input channel for rendering.".to_owned());
        }

        Ok(ctx
            .guild_settings(guild, |s| s.output_channel)
            .flatten()
            .unwrap_or(channel_id))
    } else {
        Ok(channel_id)
    }
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
}

fn split_quoted_tokens(input: &str) -> std::result::Result<Vec<String>, String> {
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

/// Parse prefix render options (`start=`, `end=`, `pitch=`, `skin=`, `skip`, `offset=`, `force`, score URL).
pub fn parse_raw_prefix_args(args: &str) -> std::result::Result<RawPrefixRenderArgs, String> {
    let tokens = split_quoted_tokens(args)?;
    let mut parsed = RawPrefixRenderArgs::default();

    for token in tokens {
        let lower = token.to_ascii_lowercase();
        if let Some(v) = token.strip_prefix("start=").or_else(|| token.strip_prefix("START=")) {
            parsed.start = TimePoints::parse_single(v).map_err(|e| format!("Invalid `start`: {e}"))?;
        } else if let Some(v) = token.strip_prefix("end=").or_else(|| token.strip_prefix("END=")) {
            parsed.end = TimePoints::parse_single(v).map_err(|e| format!("Invalid `end`: {e}"))?;
        } else if let Some(v) = token.strip_prefix("pitch=").or_else(|| token.strip_prefix("PITCH=")) {
            let p = v
                .parse::<f64>()
                .map_err(|_| "Invalid `pitch`: must be a positive number (e.g. `pitch=1.5`).".to_owned())?;
            if !p.is_finite() || p <= 0.0 {
                return Err("Invalid `pitch`: must be a positive number (e.g. `pitch=1.5`).".to_owned());
            }
            parsed.pitch = Some(p.clamp(0.5, 5.0));
        } else if let Some(v) = token.strip_prefix("skin=").or_else(|| token.strip_prefix("SKIN=")) {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return Err("`skin=` cannot be empty.".to_owned());
            }
            parsed.raw_skin = Some(trimmed.to_owned());
        } else if let Some(v) = token.strip_prefix("offset=").or_else(|| token.strip_prefix("OFFSET=")) {
            let off = v
                .parse::<i32>()
                .map_err(|_| "Invalid `offset`: integer milliseconds expected (e.g. `offset=-15`).".to_owned())?;
            parsed.offset_ms = Some(off.clamp(-5000, 5000));
        } else if matches!(lower.as_str(), "skip" | "skip_intro" | "skip=true" | "skip_intro=true") {
            parsed.skip_intro = true;
        } else if matches!(lower.as_str(), "force" | "rerender" | "force=true" | "rerender=true") {
            parsed.force_rerender = true;
        } else if let Some(score_id) = parse_osu_score_url(&token) {
            parsed.score_id = Some(score_id);
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
) -> std::result::Result<(RenderOptions, Option<u64>), String> {
    let raw = parse_raw_prefix_args(args)?;
    let skin = match raw.raw_skin {
        Some(ref name) => {
            let resolved = resolve_skin_name(ctx, name)
                .map_err(|_| "Failed to read skin list.".to_owned())?
                .ok_or_else(|| format!("Unknown skin `{name}`. Use `/skin list` or `!skinlist` to see available skins."))?;
            Some(resolved)
        }
        None => None,
    };

    Ok((
        RenderOptions {
            start: raw.start,
            end: raw.end,
            pitch: raw.pitch,
            skin,
            skip_intro: raw.skip_intro,
            offset_ms: raw.offset_ms,
            force_rerender: raw.force_rerender,
        },
        raw.score_id,
    ))
}

/// Build the confirmation embed + Cancel button for a newly enqueued render job.
pub fn build_queue_added_message(job_id: u64, note: Option<&str>) -> MessageBuilder<'static> {
    let mut text = String::from("Replay has been added to the queue!");
    if let Some(extra) = note {
        let _ = write!(text, "\n⚠️ {extra}");
    }

    let cancel_btn = Component::ActionRow(ActionRow {
        components: vec![Component::Button(Button {
            sku_id: None,
            custom_id: Some(format!("render_cancel:{job_id}")),
            disabled: false,
            emoji: None,
            label: Some("Cancel".to_owned()),
            style: ButtonStyle::Danger,
            url: None,
        })],
    });

    MessageBuilder::new().embed(text).components(vec![cancel_btn])
}

/// Attach `queue_message` to `replay_data`, push it onto `ReplayQueue`, and start status updates if idle.
pub async fn finalize_enqueue(
    ctx: Arc<Context>,
    mut replay_data: ReplayData,
    queue_msg: Option<Message>,
) {
    let output_channel = replay_data.output_channel;
    replay_data.queue_message = queue_msg.map(|m| (m.id, m.channel_id));
    let (was_empty, _) = ctx.replay_queue.push(replay_data).await;
    if was_empty {
        let ctx_clone = Arc::clone(&ctx);
        tokio::spawn(async move {
            let _ = send_queue_status(ctx_clone, output_channel).await;
        });
    }
}

async fn find_score_to_render(
    ctx: &Context,
    user_id: u32,
    beatmap_id: u32,
    timestamp: Option<Timestamp>,
) -> Result<(Option<Score>, bool)> {
    if let Some(timestamp) = timestamp {
        let ts_unix = timestamp.as_secs();

        // check recents
        let recent_scores = ctx
            .osu()
            .user_scores(user_id)
            .recent()
            .include_fails(false)
            .limit(100)
            .await
            .context("failed to get recent scores")?;

        if let Some(score) = recent_scores
            .into_iter()
            .find(|score| (score.ended_at.unix_timestamp() - ts_unix).abs() <= 3)
        {
            return Ok((Some(score), false));
        }

        // check tops
        let top_scores = ctx
            .osu()
            .user_scores(user_id)
            .best()
            .limit(100)
            .await
            .context("failed to get top scores")?;

        if let Some(score) = top_scores
            .into_iter()
            .find(|score| (score.ended_at.unix_timestamp() - ts_unix).abs() <= 3)
        {
            return Ok((Some(score), false));
        }

        tracing::warn!(
            "Fetching map scores for user {} on beatmap {}",
            user_id,
            beatmap_id
        );
        let map_scores = ctx
            .osu()
            .beatmap_user_scores(beatmap_id, user_id)
            .await
            .context("failed to get map scores")?;
        tracing::warn!("Got {} map scores", map_scores.len());
        if let Some(exact) = map_scores
            .iter()
            .find(|score| (score.ended_at.unix_timestamp() - ts_unix).abs() <= 3)
            .cloned()
        {
            return Ok((Some(exact), false));
        }

        let fallback = map_scores.into_iter().next();
        let used_fallback = fallback.is_some();
        Ok((fallback, used_fallback))
    } else {
        // No timestamp (<current embed) — pick best score on this map
        tracing::warn!(
            user_id,
            beatmap_id,
            "no timestamp, fetching beatmap user scores"
        );

        let map_scores = ctx
            .osu()
            .beatmap_user_scores(beatmap_id, user_id)
            .await
            .context("failed to get user scores on beatmap")?;

        Ok((map_scores.into_iter().next(), false))
    }
}

pub enum ScorePrepareOutcome {
    Ready(Box<ReplayData>),
    Unavailable(&'static str),
}

async fn prepare_score_replay_data(
    ctx: &Context,
    score_to_render: Score,
    input_channel: Id<ChannelMarker>,
    output_channel: Id<ChannelMarker>,
    user: Id<UserMarker>,
    opts: RenderOptions,
) -> Result<ScorePrepareOutcome> {
    if score_to_render.mode != GameMode::Osu {
        return Ok(ScorePrepareOutcome::Unavailable(
            "danser only accepts osu!standard plays, sorry :(",
        ));
    }

    let score_id = score_to_render.id;
    let mut replay_bytes_res = ctx.osu().replay_raw(score_id).await;
    if replay_bytes_res.is_err() {
        if let Some(legacy_id) = score_to_render.legacy_score_id {
            replay_bytes_res = ctx.osu().replay_raw(legacy_id).mode(score_to_render.mode).await;
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

    let fetched_user = if score_to_render.user.is_none() {
        ctx.osu().user(score_to_render.user_id).await.ok()
    } else {
        None
    };
    let fetched_map = if score_to_render.mapset.is_none() || score_to_render.map.is_none() {
        ctx.osu().beatmap().map_id(score_to_render.map_id).await.ok()
    } else {
        None
    };

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
    path.push(format!(
        "{}_{}_{safe_user} - {safe_title} [{safe_diff}].osr",
        user, score_id
    ));

    tokio::fs::write(&path, replay_bytes)
        .await
        .context("failed to write into replay file")?;

    let mut data = ReplayData::from_options(
        input_channel,
        output_channel,
        path,
        replay,
        user,
        score_to_render.set_on_lazer,
        opts,
    );
    data.player_name = Some(osu_user.to_owned());
    data.map_title = Some(map_title.to_owned());
    data.difficulty_name = Some(diff_name.to_owned());

    Ok(ScorePrepareOutcome::Ready(Box::new(data)))
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

    let output_channel = match check_render_access(&ctx, user, command.guild_id, command.channel_id).await {
        Ok(ch) => ch,
        Err(msg) => {
            command.error(&ctx, msg).await?;
            return Ok(());
        }
    };

    if let Err(msg) = check_user_queue_limit(&ctx, user).await {
        command.error(&ctx, msg).await?;
        return Ok(());
    }

    let input_data = command.input_data();

    let (user_id, beatmap_id, timestamp) = match parseembedinputdata(input_data) {
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

    let (score_opt, used_fallback) = find_score_to_render(&ctx, user_id, beatmap_id, timestamp).await?;
    let score_to_render = match score_opt {
        Some(score) => score,
        None => {
            let content = "Couldn't find the replay for this score (not in recents, tops, or map bests).";
            command.error(&ctx, content).await?;
            return Ok(());
        }
    };

    let mut replay_data = match prepare_score_replay_data(
        &ctx,
        score_to_render,
        command.channel_id,
        output_channel,
        user,
        RenderOptions::default(),
    )
    .await?
    {
        ScorePrepareOutcome::Ready(data) => *data,
        ScorePrepareOutcome::Unavailable(reason) => {
            command.error(&ctx, reason).await?;
            return Ok(());
        }
    };

    let warning = used_fallback
        .then_some("Exact play timestamp wasn't found; queued your best score on this beatmap instead.");

    let job_id = ctx.replay_queue.allocate_job_id();
    replay_data.job_id = job_id;
    let builder = build_queue_added_message(job_id, warning);
    let queue_msg = command.update(&ctx, &builder).await.ok();
    finalize_enqueue(ctx, replay_data, queue_msg).await;
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
    output_channel: Id<ChannelMarker>,
    user: Id<UserMarker>,
    opts: RenderOptions,
) -> Result<EmbedRenderOutcome> {
    let Some(embed) = message.embeds.first() else {
        return Ok(EmbedRenderOutcome::NotScoreEmbed);
    };

    render_score_from_embed(ctx, embed, input_channel, output_channel, user, opts).await
}

pub async fn render_score_from_embed(
    ctx: Arc<Context>,
    embed: &Embed,
    input_channel: Id<ChannelMarker>,
    output_channel: Id<ChannelMarker>,
    user: Id<UserMarker>,
    opts: RenderOptions,
) -> Result<EmbedRenderOutcome> {
    tracing::warn!("render_score_from_embed: entered for map/user");

    let Some(ParsedEmbed {
        user_id,
        beatmap_id,
        timestamp,
    }) = parse_embed(embed)
    else {
        tracing::warn!("render_score_from_embed: parse_embed returned None");
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
        output_channel,
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

/// Fetch a score by osu! score ID and prepare a `ReplayData` entry for rendering.
pub async fn prepare_score_by_id(
    ctx: &Context,
    score_id: u64,
    input_channel: Id<ChannelMarker>,
    output_channel: Id<ChannelMarker>,
    user: Id<UserMarker>,
    opts: RenderOptions,
) -> Result<ScorePrepareOutcome> {
    let score = ctx
        .osu()
        .score(score_id)
        .await
        .with_context(|| format!("failed to fetch osu! score {score_id}"))?;

    prepare_score_replay_data(ctx, score, input_channel, output_channel, user, opts).await
}

struct ParsedEmbed {
    user_id: u32,
    beatmap_id: u32,
    timestamp: Option<Timestamp>,
}

fn parse_embed(embed: &Embed) -> Option<ParsedEmbed> {
    let user_url = embed.author.as_ref().and_then(|a| a.url.as_ref())?;
    let user_id = user_url
        .split('/')
        .nth(4)
        .and_then(|id| id.parse::<u32>().ok())?;

    let beatmap_url = embed.url.as_ref()?;
    let beatmap_id = if beatmap_url.contains("/b/") {
        beatmap_url
            .split("/b/")
            .nth(1)
            .and_then(|id| id.parse::<u32>().ok())?
    } else if beatmap_url.contains("/beatmaps/") {
        beatmap_url
            .split('/')
            .next_back()
            .and_then(|id| id.parse::<u32>().ok())?
    } else if beatmap_url.contains("/beatmapsets/") {
        if let Some(fragment) = beatmap_url.split('#').nth(1) {
            fragment
                .split('/')
                .next_back()
                .and_then(|id| id.parse::<u32>().ok())?
        } else {
            return None;
        }
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

    let discord_timestamp = field.name.rsplit('\t').next()?;

    let actual_timestamp_value = discord_timestamp
        .trim_start_matches("<t:")
        .trim_end_matches(":R>");

    let timestamp_value_as_int = actual_timestamp_value.parse().ok()?;

    Timestamp::from_secs(timestamp_value_as_int).ok()
}

fn parseembedinputdata(input_: CommandInputData<'_>) -> Option<ParsedEmbed> {
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

        assert!(parse_raw_prefix_args("start=2:00 end=1:00").is_err());
        assert!(parse_raw_prefix_args("skin=\"unclosed").is_err());
    }
}
