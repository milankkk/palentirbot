use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    sync::{Arc, Mutex, OnceLock},
};

use command_macros::SlashCommand;
use eyre::Result;
use rosu_v2::{
    error::OsuError,
    prelude::{GameMode, GameMods, Score, UserExtended, UserId},
};
use tokio::time::{Duration, Instant};
use twilight_interactions::command::{CommandModel, CreateCommand};
use twilight_model::{
    channel::{
        message::{component::ButtonStyle, embed::EmbedAuthor, Component},
        Message,
    },
    id::{
        marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker},
        Id,
    },
};

use super::render_score::{
    check_render_access, check_user_queue_limit, enqueue_replay_in_channel,
    prepare_score_replay_data, ScorePrepareOutcome,
};
use crate::{
    core::{
        replay_queue::{
            process::{analyze_replay_for_info, spawn_pp_select_expiry, ReplayInfoEmbed},
            RenderOptions,
        },
        BotConfig, Context,
    },
    database::BlacklistHit,
    util::{
        builder::{action_row, button, button_disabled, link_button, EmbedBuilder, MessageBuilder},
        constants::RED,
        interaction::{InteractionCommand, InteractionComponent},
        str::{percent_decode_utf8, truncate_with_ellipsis},
        Authored, ChannelExt, ComponentExt, InteractionCommandExt, MessageExt,
    },
};

const PLAYS_PER_PAGE: usize = 5;
const SESSION_IDLE_SECS: u64 = 90;
const SESSION_EXTEND_SECS: u64 = 60;
const SESSION_HARD_CAP_SECS: u64 = 300;
const ACCENT_COLOR: u32 = 0x96DFE3;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ListKind {
    Recent,
    Top,
}

#[derive(Clone)]
struct RenderableEntry {
    /// 1-based rank for Top (#1..=#200), or 1-based index for Recent (1..=N).
    display_index: usize,
    score: Score,
}

struct ProfileScoreSession {
    owner_id: Id<UserMarker>,
    kind: ListKind,
    osu_user_id: u32,
    osu_username: String,
    avatar_url: String,
    checked_count: usize,
    entries: Vec<RenderableEntry>,
    page: usize,
    created_at: Instant,
    last_activity: Instant,
}

type InFlightSet = Mutex<HashSet<(Id<UserMarker>, u64)>>;

static PROFILE_SESSIONS: OnceLock<Mutex<HashMap<Id<MessageMarker>, ProfileScoreSession>>> =
    OnceLock::new();
static IN_FLIGHT_RENDERS: OnceLock<InFlightSet> = OnceLock::new();

fn session_map() -> &'static Mutex<HashMap<Id<MessageMarker>, ProfileScoreSession>> {
    PROFILE_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn in_flight_set() -> &'static InFlightSet {
    IN_FLIGHT_RENDERS.get_or_init(|| Mutex::new(HashSet::new()))
}

struct InFlightGuard {
    key: (Id<UserMarker>, u64),
}

impl InFlightGuard {
    fn try_acquire(user_id: Id<UserMarker>, score_id: u64) -> Option<Self> {
        let mut set = in_flight_set().lock().unwrap_or_else(|p| p.into_inner());
        let key = (user_id, score_id);
        if set.insert(key) {
            Some(Self { key })
        } else {
            None
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut set = in_flight_set().lock().unwrap_or_else(|p| p.into_inner());
        set.remove(&self.key);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OsuProfileTarget {
    DiscordUser(Id<UserMarker>),
    OsuId(u32),
    OsuUsername(String),
}

// ── Slash command definitions ─────────────────────────────────────────────────

#[derive(CommandModel, CreateCommand, SlashCommand)]
#[command(name = "rr")]
/// List recent renderable osu!standard plays with render buttons
pub struct Rr {
    /// osu! username, ID, profile URL, or @mention (defaults to linked account)
    user: Option<String>,
}

#[derive(CommandModel, CreateCommand, SlashCommand)]
#[command(name = "rtop")]
/// List renderable osu!standard plays from a player's top 200
pub struct Rtop {
    /// osu! username, ID, profile URL, or @mention (defaults to linked account)
    user: Option<String>,
}

#[derive(CommandModel, CreateCommand, SlashCommand)]
#[command(name = "rrn")]
/// Analyze and immediately render the most recent osu!standard play
pub struct Rrn {
    /// osu! username, ID, profile URL, or @mention (defaults to linked account)
    user: Option<String>,
}

#[derive(CommandModel, CreateCommand, SlashCommand)]
#[command(name = "link")]
#[flags(EPHEMERAL)]
/// Verify and link your osu! account via osu! OAuth
pub struct Link;

// ── Pure helpers (unit-tested) ────────────────────────────────────────────────

pub(crate) fn is_score_renderable(score: &Score) -> bool {
    score.mode == GameMode::Osu && score.passed && (score.has_replay || score.replay)
}

pub(crate) fn total_pages_for(count: usize, per_page: usize) -> usize {
    if count == 0 || per_page == 0 {
        1
    } else {
        count.div_ceil(per_page)
    }
}

pub(crate) fn page_bounds(count: usize, page: usize, per_page: usize) -> (usize, usize) {
    if count == 0 || per_page == 0 {
        return (0, 0);
    }
    let total_pages = total_pages_for(count, per_page);
    let clamped_page = page.min(total_pages - 1);
    let start = clamped_page * per_page;
    let end = (start + per_page).min(count);
    (start, end)
}

pub(crate) fn parse_osu_profile_ref(raw: &str) -> Option<OsuProfileTarget> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Strip matching surrounding quotes (`"user name"` or `'user name'`)
    let unquoted = if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        trimmed[1..trimmed.len() - 1].trim()
    } else {
        trimmed
    };
    if unquoted.is_empty() {
        return None;
    }

    // Discord mention: <@12345678901234567> or <@!12345678901234567>
    if let Some(inner) = unquoted.strip_prefix("<@").and_then(|s| s.strip_suffix('>')) {
        let digits = inner.strip_prefix('!').unwrap_or(inner);
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(id) = digits.parse::<u64>() {
                if id > 0 {
                    return Some(OsuProfileTarget::DiscordUser(Id::new(id)));
                }
            }
        }
    }

    // osu! profile URL: https://osu.ppy.sh/users/12345(/osu) or /u/12345 or lazer.ppy.sh/users/...
    let no_angle = unquoted.trim_matches(|c| c == '<' || c == '>');
    let without_scheme = no_angle
        .strip_prefix("https://")
        .or_else(|| no_angle.strip_prefix("http://"))
        .unwrap_or(no_angle);
    let lower = without_scheme.to_ascii_lowercase();
    for host in ["osu.ppy.sh/", "lazer.ppy.sh/", "old.ppy.sh/"] {
        if lower.starts_with(host) {
            let path_part = &without_scheme[host.len()..];
            let path_only = path_part.split(['?', '#']).next().unwrap_or(path_part);
            let mut segs = path_only.split('/').filter(|s| !s.is_empty());
            if let (Some(kind), Some(user_seg)) = (segs.next(), segs.next()) {
                if kind.eq_ignore_ascii_case("users") || kind.eq_ignore_ascii_case("u") {
                    let decoded = percent_decode_utf8(user_seg);
                    let decoded = decoded.trim();
                    if !decoded.is_empty() {
                        if decoded.bytes().all(|b| b.is_ascii_digit()) {
                            if let Ok(uid) = decoded.parse::<u32>() {
                                if uid > 0 {
                                    return Some(OsuProfileTarget::OsuId(uid));
                                }
                            }
                        }
                        return Some(OsuProfileTarget::OsuUsername(decoded.to_owned()));
                    }
                }
            }
        }
    }

    // Pure numeric osu! ID (will still fall back to username lookup on 404)
    if unquoted.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(uid) = unquoted.parse::<u32>() {
            if uid > 0 {
                return Some(OsuProfileTarget::OsuId(uid));
            }
        }
    }

    Some(OsuProfileTarget::OsuUsername(unquoted.to_owned()))
}

/// Sanitize text placed inside a Discord `[label](url)` markdown link or bold span.
/// Discord's embed link parser does NOT unescape `\[` / `\]` and breaks across lines
/// if brackets inside `[...](url)` are unbalanced, so brackets are mapped to parentheses
/// and `*`/`\`/`` ` `` are neutralized without adding backslashes.
fn sanitize_md_link_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '[' => out.push('('),
            ']' => out.push(')'),
            '*' => out.push('∗'),
            '`' => out.push('\''),
            '\\' => out.push('/'),
            '\n' | '\r' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out
}

fn format_mods_display(mods: &GameMods) -> String {
    if mods.is_empty() {
        return "NM".to_owned();
    }
    if let Some(bits) = mods.checked_bits() {
        let legacy = rosu_v2::prelude::GameModsLegacy::from_bits(bits);
        if !legacy.is_empty() {
            return format!("+{legacy}");
        }
    }
    format!("+{mods}")
}

fn format_score_mods(mods: &GameMods, set_on_lazer: bool) -> String {
    // osu! API v2 attaches `CL` to every legacy osu!stable score; hide it for stable
    // plays while preserving `+CL` when explicitly selected on osu!lazer.
    if set_on_lazer || !mods.iter().any(|m| m.acronym().as_str() == "CL") {
        return format_mods_display(mods);
    }
    let mut filtered = GameMods::new();
    for m in mods.iter() {
        if m.acronym().as_str() != "CL" {
            filtered.insert(m.clone());
        }
    }
    format_mods_display(&filtered)
}

fn score_map_title(score: &Score) -> String {
    match score.mapset.as_ref() {
        Some(ms) => format!("{} - {}", ms.artist, ms.title),
        None => format!("Beatmap #{}", score.map_id),
    }
}

fn format_map_link_parts(map_id: u32, raw_title: &str, diff: Option<&str>) -> String {
    let safe_title = sanitize_md_link_label(&truncate_with_ellipsis(raw_title, 52, "…"));
    let map_url = format!("https://osu.ppy.sh/beatmaps/{map_id}");

    match diff.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => {
            let safe_diff = sanitize_md_link_label(&truncate_with_ellipsis(d, 28, "…"));
            format!("**[{safe_title}]({map_url}) [{safe_diff}]**")
        }
        None => format!("**[{safe_title}]({map_url})**"),
    }
}

fn format_score_map_link(score: &Score) -> String {
    let raw_title = score_map_title(score);
    let diff = score.map.as_ref().map(|m| m.version.as_str());
    format_map_link_parts(score.map_id, &raw_title, diff)
}

fn score_map_display(score: &Score) -> String {
    let title = score_map_title(score);
    match score.map.as_ref() {
        Some(m) => format!("{title} [{}]", m.version),
        None => title,
    }
}

/// Merge two top-score pages (0..100 and 100..200), deduplicate by score ID,
/// ensure descending PP order, assign true 1-based `#rank`, and keep only renderable scores.
fn merge_and_filter_top_scores(
    page1: Vec<Score>,
    page2: Vec<Score>,
) -> (usize, Vec<RenderableEntry>) {
    let mut seen = HashSet::with_capacity(page1.len() + page2.len());
    let mut all = Vec::with_capacity(page1.len() + page2.len());
    for s in page1.into_iter().chain(page2) {
        if seen.insert(s.id) {
            all.push(s);
        }
    }

    // Preserve osu! top-play ordering strictly highest-PP first.
    all.sort_by(|a, b| {
        b.pp.unwrap_or(0.0)
            .total_cmp(&a.pp.unwrap_or(0.0))
    });

    let checked_count = all.len();
    let renderable = all
        .into_iter()
        .enumerate()
        .filter(|(_, s)| is_score_renderable(s))
        .map(|(idx, score)| RenderableEntry {
            display_index: idx + 1,
            score,
        })
        .collect();

    (checked_count, renderable)
}

fn filter_recent_scores(recent: Vec<Score>) -> (usize, Vec<RenderableEntry>) {
    let checked_count = recent.len();
    let renderable = recent
        .into_iter()
        .filter(is_score_renderable)
        .enumerate()
        .map(|(idx, score)| RenderableEntry {
            display_index: idx + 1,
            score,
        })
        .collect();

    (checked_count, renderable)
}

// ── osu! API helpers with automatic OAuth refresh ─────────────────────────────

async fn fetch_osu_user(ctx: &Context, user_id: UserId) -> std::result::Result<UserExtended, OsuError> {
    crate::osu_call!(ctx, |osu| osu.user(user_id.clone()).mode(GameMode::Osu))
}

async fn fetch_recent_scores(
    ctx: &Context,
    osu_user_id: u32,
    include_fails: bool,
    limit: usize,
) -> std::result::Result<Vec<Score>, OsuError> {
    crate::osu_call!(ctx, |osu| {
        osu.user_scores(osu_user_id)
            .recent()
            .mode(GameMode::Osu)
            .include_fails(include_fails)
            .limit(limit)
    })
}

async fn fetch_top_scores_page(
    ctx: &Context,
    osu_user_id: u32,
    offset: usize,
) -> std::result::Result<Vec<Score>, OsuError> {
    crate::osu_call!(ctx, |osu| {
        osu.user_scores(osu_user_id)
            .best()
            .mode(GameMode::Osu)
            .limit(100)
            .offset(offset)
    })
}

async fn resolve_osu_id_or_name(
    ctx: &Context,
    target: OsuProfileTarget,
) -> std::result::Result<UserExtended, String> {
    match target {
        OsuProfileTarget::OsuId(uid) => match fetch_osu_user(ctx, UserId::from(uid)).await {
            Ok(user) => Ok(user),
            Err(OsuError::NotFound) => {
                let as_name = uid.to_string();
                fetch_osu_user(ctx, UserId::from(as_name.as_str()))
                    .await
                    .map_err(|err| format_osu_user_error(&as_name, &err))
            }
            Err(err) => Err(format_osu_user_error(&uid.to_string(), &err)),
        },
        OsuProfileTarget::OsuUsername(name) => fetch_osu_user(ctx, UserId::from(name.as_str()))
            .await
            .map_err(|err| format_osu_user_error(&name, &err)),
        OsuProfileTarget::DiscordUser(_) => Err("Expected an osu! username or ID.".to_owned()),
    }
}

async fn resolve_osu_profile(
    ctx: &Context,
    caller_id: Id<UserMarker>,
    raw_arg: Option<&str>,
    prefix: &str,
) -> std::result::Result<UserExtended, String> {
    let target = match raw_arg.and_then(parse_osu_profile_ref) {
        Some(t) => t,
        None => {
            let linked = ctx.psql().get_linked_osu_user(caller_id).await.map_err(|err| {
                tracing::warn!(?err, "failed to look up linked osu! profile");
                "Failed to read linked osu! profile from database. Please pass a username directly."
                    .to_owned()
            })?;
            match linked {
                Some((uid, _)) => OsuProfileTarget::OsuId(uid),
                None => {
                    return Err(format!(
                        "No osu! profile specified and your Discord account isn't linked yet.\n\
                         Verify and link once with `/link` (or `{prefix}link`), or pass a profile directly (e.g. `{prefix}rr mrekk`)."
                    ));
                }
            }
        }
    };

    match target {
        OsuProfileTarget::DiscordUser(discord_id) => {
            let linked = ctx.psql().get_linked_osu_user(discord_id).await.map_err(|err| {
                tracing::warn!(?err, "failed to look up mentioned user's osu! profile");
                "Failed to look up that user's linked osu! profile.".to_owned()
            })?;
            let Some((uid, _)) = linked else {
                return Err(
                    "That Discord user hasn't linked an osu! profile yet. Ask them to verify with `/link`, or pass their osu! username directly."
                        .to_owned(),
                );
            };
            fetch_osu_user(ctx, UserId::from(uid))
                .await
                .map_err(|err| format_osu_user_error(&uid.to_string(), &err))
        }
        other => resolve_osu_id_or_name(ctx, other).await,
    }
}

fn format_osu_user_error(query: &str, err: &OsuError) -> String {
    if matches!(err, OsuError::NotFound) {
        format!("Could not find osu! user `{query}`.")
    } else {
        tracing::warn!(?err, query, "osu! API user lookup failed");
        "Failed to contact the osu! API while looking up that profile. Please try again in a moment."
            .to_owned()
    }
}

// ── Embed & component builders for !rr / !rtop ───────────────────────────────

fn build_session_message(session: &ProfileScoreSession) -> MessageBuilder<'static> {
    let total = session.entries.len();
    let total_pages = total_pages_for(total, PLAYS_PER_PAGE);
    let page = session.page.min(total_pages - 1);
    let (start, end) = page_bounds(total, page, PLAYS_PER_PAGE);

    let mut desc = String::with_capacity(1024);
    for (i, entry) in session.entries[start..end].iter().enumerate() {
        if i > 0 {
            desc.push('\n');
        }
        let score = &entry.score;
        let idx_label = match session.kind {
            ListKind::Recent => format!("**{}.**", entry.display_index),
            ListKind::Top => format!("**#{}**", entry.display_index),
        };
        let grade = BotConfig::get().emojis.grade_emoji(score.grade);
        let map_link = format_score_map_link(score);
        let mods = format_score_mods(&score.mods, score.set_on_lazer);

        let primary_stat = if let Some(pp) = score.pp {
            format!("**{pp:.1}pp**")
        } else if let Some(m) = score.map.as_ref() {
            format!("**{:.2}★**", m.stars)
        } else {
            "**Unranked**".to_owned()
        };

        let acc = (score.accuracy * 100.0).round() / 100.0;
        let combo = match score.map.as_ref().and_then(|m| m.max_combo) {
            Some(max_c) if max_c > 0 => format!("{}/{}x", score.max_combo, max_c),
            _ => format!("{}x", score.max_combo),
        };
        let stats = &score.statistics;
        let hits = format!("{}/{}/{}/{}", stats.great, stats.ok, stats.meh, stats.miss);
        let ts = score.ended_at.unix_timestamp();

        let _ = writeln!(
            desc,
            "{idx_label} {grade} {map_link} `{mods}`\n\
             {primary_stat} • **{acc:.2}%** • **{combo}** • `{hits}` • <t:{ts}:R>"
        );
    }

    let heading = match session.kind {
        ListKind::Recent => format!("{} — Renderable Recent Plays", session.osu_username),
        ListKind::Top => format!("{} — Renderable Top 200 Plays", session.osu_username),
    };

    let footer = if total_pages > 1 {
        format!(
            "Page {}/{} • {} renderable of {} checked • Click a button below to render",
            page + 1,
            total_pages,
            total,
            session.checked_count
        )
    } else {
        format!(
            "{} renderable of {} checked • Click a button below to render",
            total, session.checked_count
        )
    };

    let author = EmbedAuthor {
        icon_url: (!session.avatar_url.is_empty()).then(|| session.avatar_url.clone()),
        name: heading,
        proxy_icon_url: None,
        url: Some(format!(
            "https://osu.ppy.sh/users/{}/osu",
            session.osu_user_id
        )),
    };

    let embed = EmbedBuilder::new()
        .color(ACCENT_COLOR)
        .author(author)
        .description(desc.trim_end().to_owned())
        .footer(footer)
        .build();

    let mut rows = Vec::with_capacity(2);

    // Row 1: Render buttons for each entry shown on this page (1..=5)
    let render_buttons: Vec<Component> = session.entries[start..end]
        .iter()
        .enumerate()
        .map(|(offset, entry)| {
            let abs_idx = start + offset;
            let label = match session.kind {
                ListKind::Recent => format!("🎬 {}", entry.display_index),
                ListKind::Top => format!("🎬 #{}", entry.display_index),
            };
            button(
                format!("pr_ren:{}:{abs_idx}", session.owner_id),
                label,
                ButtonStyle::Success,
            )
        })
        .collect();

    if !render_buttons.is_empty() {
        rows.push(action_row(render_buttons));
    }

    // Row 2: Pagination buttons (only when > 5 renderable plays)
    if total_pages > 1 {
        let on_first = page == 0;
        let on_last = page + 1 >= total_pages;
        let owner = session.owner_id;
        rows.push(action_row(vec![
            button_disabled(
                format!("pr_pg:{owner}:first"),
                "⏮",
                ButtonStyle::Secondary,
                on_first,
            ),
            button_disabled(
                format!("pr_pg:{owner}:prev"),
                "◀",
                ButtonStyle::Primary,
                on_first,
            ),
            button_disabled(
                format!("pr_pg:{owner}:noop"),
                format!("Page {}/{}", page + 1, total_pages),
                ButtonStyle::Secondary,
                true,
            ),
            button_disabled(
                format!("pr_pg:{owner}:next"),
                "▶",
                ButtonStyle::Primary,
                on_last,
            ),
            button_disabled(
                format!("pr_pg:{owner}:last"),
                "⏭",
                ButtonStyle::Secondary,
                on_last,
            ),
        ]));
    }

    MessageBuilder::new().embed(embed).components(rows)
}

fn register_session_and_spawn_expiry(
    ctx: Arc<Context>,
    msg_id: Id<MessageMarker>,
    channel_id: Id<ChannelMarker>,
    session: ProfileScoreSession,
) {
    {
        let mut map = session_map().lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        // Defense-in-depth pruning of any orphaned old sessions
        map.retain(|_, s| {
            now.saturating_duration_since(s.created_at)
                < Duration::from_secs(SESSION_HARD_CAP_SECS + 120)
        });
        map.insert(msg_id, session);
    }

    tokio::spawn(async move {
        let expired = crate::core::events::interaction::wait_idle_expiry(
            Instant::now(),
            Duration::from_secs(SESSION_IDLE_SECS),
            Duration::from_secs(SESSION_EXTEND_SECS),
            Duration::from_secs(SESSION_HARD_CAP_SECS),
            || {
                let map = session_map().lock().unwrap_or_else(|p| p.into_inner());
                map.get(&msg_id).map(|s| s.last_activity)
            },
        )
        .await;

        if !expired {
            return;
        }

        {
            let mut map = session_map().lock().unwrap_or_else(|p| p.into_inner());
            map.remove(&msg_id);
        }

        let clear = MessageBuilder::new().components(Vec::new());
        if let Err(err) = (msg_id, channel_id).update(&ctx, &clear).await {
            tracing::debug!(?err, msg_id = %msg_id, "failed to clear expired profile render buttons");
        }
    });
}

// ── Shared core execution for !rr / /rr, !rtop / /rtop, !rrn / /rrn, !link ───

async fn build_profile_list_session(
    ctx: &Context,
    owner_id: Id<UserMarker>,
    guild_id: Option<Id<GuildMarker>>,
    channel_id: Id<ChannelMarker>,
    raw_arg: Option<&str>,
    prefix: &str,
    kind: ListKind,
) -> std::result::Result<ProfileScoreSession, String> {
    check_render_access(ctx, owner_id, guild_id, channel_id).await?;
    let osu_user = resolve_osu_profile(ctx, owner_id, raw_arg, prefix).await?;
    let username = osu_user.username.as_str().to_owned();

    let (checked_count, entries) = match kind {
        ListKind::Recent => {
            let scores = fetch_recent_scores(ctx, osu_user.user_id, false, 100)
                .await
                .map_err(|err| {
                    tracing::warn!(?err, osu_id = osu_user.user_id, "failed to fetch recent scores");
                    "Failed to fetch recent scores from the osu! API. Please try again in a moment."
                        .to_owned()
                })?;

            if scores.is_empty() {
                return Err(format!(
                    "No passed osu!standard recent plays found for **{username}** in the last 24 hours."
                ));
            }

            let (checked, renderable) = filter_recent_scores(scores);
            if renderable.is_empty() {
                return Err(format!(
                    "Found **{checked}** recent passed play(s) for **{username}**, but none have a downloadable replay available on osu!."
                ));
            }
            (checked, renderable)
        }
        ListKind::Top => {
            let (page1_res, page2_res) = tokio::join!(
                fetch_top_scores_page(ctx, osu_user.user_id, 0),
                fetch_top_scores_page(ctx, osu_user.user_id, 100),
            );
            let page1 = page1_res.map_err(|err| {
                tracing::warn!(?err, osu_id = osu_user.user_id, "failed to fetch top 1-100 scores");
                "Failed to fetch top scores from the osu! API. Please try again in a moment."
                    .to_owned()
            })?;
            let page2 = match page2_res {
                Ok(scores) => scores,
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        osu_id = osu_user.user_id,
                        "failed to fetch top 101-200 scores; proceeding with top 1-100"
                    );
                    Vec::new()
                }
            };

            if page1.is_empty() && page2.is_empty() {
                return Err(format!(
                    "No osu!standard top plays found for **{username}**."
                ));
            }

            let (checked, renderable) = merge_and_filter_top_scores(page1, page2);
            if renderable.is_empty() {
                return Err(format!(
                    "Checked **{username}**'s top **{checked}** osu!standard play(s), but none have a downloadable replay available on osu!."
                ));
            }
            (checked, renderable)
        }
    };

    let now = Instant::now();
    Ok(ProfileScoreSession {
        owner_id,
        kind,
        osu_user_id: osu_user.user_id,
        osu_username: username,
        avatar_url: osu_user.avatar_url,
        checked_count,
        entries,
        page: 0,
        created_at: now,
        last_activity: now,
    })
}

async fn prepare_rrn_play(
    ctx: &Context,
    caller_id: Id<UserMarker>,
    guild_id: Option<Id<GuildMarker>>,
    channel_id: Id<ChannelMarker>,
    raw_arg: Option<&str>,
    prefix: &str,
) -> std::result::Result<
    (
        crate::core::replay_queue::ReplayData,
        crate::core::replay_queue::process::ReplayInfoEmbed,
    ),
    String,
> {
    check_render_access(ctx, caller_id, guild_id, channel_id).await?;
    check_user_queue_limit(ctx, caller_id).await?;

    let osu_user = resolve_osu_profile(ctx, caller_id, raw_arg, prefix).await?;
    let username = osu_user.username.as_str().to_owned();

    let recent = fetch_recent_scores(ctx, osu_user.user_id, true, 1)
        .await
        .map_err(|err| {
            tracing::warn!(?err, osu_id = osu_user.user_id, "failed to fetch latest recent score");
            "Failed to fetch recent plays from the osu! API. Please try again in a moment."
                .to_owned()
        })?;

    let Some(latest) = recent.into_iter().next() else {
        return Err(format!(
            "No recent osu!standard plays found for **{username}** in the last 24 hours."
        ));
    };

    let map_desc = score_map_display(&latest);
    if !latest.passed {
        return Err(format!(
            "**{username}**'s most recent play (**{map_desc}**) was a fail and cannot be rendered.\n\
             Use `{prefix}rr` to view recent renderable plays."
        ));
    }
    if !(latest.has_replay || latest.replay) {
        return Err(format!(
            "**{username}**'s most recent play (**{map_desc}**) does not have a replay available on osu!.\n\
             Use `{prefix}rr` to view recent renderable plays."
        ));
    }

    let opts = RenderOptions {
        skip_info_embed: true,
        ..Default::default()
    };

    let mut replay_data = match prepare_score_replay_data(ctx, latest, channel_id, caller_id, opts)
        .await
    {
        Ok(ScorePrepareOutcome::Ready(data)) => *data,
        Ok(ScorePrepareOutcome::Unavailable(reason)) => return Err(reason.to_owned()),
        Err(err) => {
            tracing::warn!(?err, "failed to prepare .osr replay for rrn");
            return Err("Failed to download or parse the replay file.".to_owned());
        }
    };

    let info = match analyze_replay_for_info(ctx, &replay_data.replay, replay_data.is_lazer).await {
        Ok(info) => info,
        Err(err_msg) => {
            let _ = tokio::fs::remove_file(&replay_data.path).await;
            return Err(err_msg);
        }
    };

    replay_data.title = Some(info.title.clone());
    Ok((replay_data, info))
}

async fn prepare_oauth_link_message(
    ctx: &Context,
    caller_id: Id<UserMarker>,
    channel_id: Id<ChannelMarker>,
    guild_id: Option<Id<GuildMarker>>,
    had_typed_arg: bool,
) -> std::result::Result<(MessageBuilder<'static>, [u8; 32]), String> {
    match ctx.psql().check_blacklisted(caller_id, guild_id).await {
        Ok(Some(BlacklistHit::User(reason))) => {
            let mut msg = "You are blacklisted from using this bot.".to_owned();
            if let Some(r) = reason {
                let _ = write!(msg, "\nReason: {r}");
            }
            return Err(msg);
        }
        Ok(Some(BlacklistHit::Server(reason))) => {
            let mut msg = "This server is blacklisted from using this bot.".to_owned();
            if let Some(r) = reason {
                let _ = write!(msg, "\nReason: {r}");
            }
            return Err(msg);
        }
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(?err, "failed to check blacklist in link");
            return Err("Failed to check user/server status, try again later.".to_owned());
        }
    }

    let existing = ctx
        .psql()
        .get_linked_osu_user(caller_id)
        .await
        .ok()
        .flatten();

    let (verify_url, token_hash) = crate::core::web_auth::create_discord_link_url(
        ctx, caller_id, channel_id, guild_id,
    )
    .await
    .map_err(|err| {
        tracing::error!(?err, "failed to create Discord OAuth link token");
        "Failed to generate a verification link. Please try again in a moment.".to_owned()
    })?;

    let mut desc = String::new();
    if had_typed_arg {
        desc.push_str(
            "Linking now requires verifying account ownership through **osu! OAuth** (instead of typing a username).\n\n",
        );
    }
    desc.push_str(
        "Click the button below to sign in with your osu! account and verify ownership.\n\
         Once authorized, your verified osu! profile will be linked to your Discord account automatically.\n\n\
         • **Single-use link** — expires in **10 minutes**.\n\
         • **Do not share this link** — anyone who clicks it would link their osu! account to your Discord.",
    );
    if let Some((cur_id, cur_name)) = existing {
        let cur_url = format!("https://osu.ppy.sh/users/{cur_id}/osu");
        let _ = write!(
            desc,
            "\n\nCurrently linked: **[{}]({cur_url})** (`#{cur_id}`). Authorizing will replace this link.",
            sanitize_md_link_label(&cur_name)
        );
    }

    let embed = EmbedBuilder::new()
        .color(ACCENT_COLOR)
        .title("Verify & Link Your osu! Account")
        .description(desc)
        .footer("Powered by osu! OAuth2")
        .build();

    let row = action_row(vec![link_button("Verify & Link osu! Account", verify_url)]);
    let builder = MessageBuilder::new().embed(embed).components(vec![row]);
    Ok((builder, token_hash))
}

pub(crate) async fn notify_discord_link_verified(
    ctx: Arc<Context>,
    discord_id: Id<UserMarker>,
    channel_id: Id<ChannelMarker>,
    guild_id: Option<Id<GuildMarker>>,
    osu_user_id: u32,
    osu_username: String,
    osu_avatar_url: String,
) {
    let prefix = ctx.guild_prefix(guild_id);
    let profile_url = format!("https://osu.ppy.sh/users/{osu_user_id}/osu");
    let author = EmbedAuthor {
        icon_url: (!osu_avatar_url.is_empty()).then_some(osu_avatar_url),
        name: format!("Verified & linked osu! profile: {osu_username}"),
        proxy_icon_url: None,
        url: Some(profile_url.clone()),
    };

    let embed = EmbedBuilder::new()
        .color(ACCENT_COLOR)
        .author(author)
        .description(format!(
            "<@{discord_id}> Your Discord account is now verified and linked to **[{}]({profile_url})** (`#{osu_user_id}`).\n\
             You can now use `{prefix}rr`, `{prefix}rtop`, and `{prefix}rrn` without typing a username.",
            sanitize_md_link_label(&osu_username),
        ))
        .footer("Verified via osu! OAuth2")
        .build();

    let builder = MessageBuilder::new()
        .content(format!("<@{discord_id}>"))
        .allowed_user_mention(discord_id)
        .embed(embed);

    if let Err(err) = channel_id.create_message(&ctx, &builder).await {
        tracing::warn!(?err, %discord_id, %channel_id, "Failed posting Discord OAuth link confirmation in channel");
    }
}

// ── Prefix command entrypoints ────────────────────────────────────────────────

pub async fn handle_prefix_rr(ctx: Arc<Context>, msg: &Message, args_str: &str, prefix: &str) {
    handle_prefix_list(ctx, msg, args_str, prefix, ListKind::Recent).await;
}

pub async fn handle_prefix_rtop(ctx: Arc<Context>, msg: &Message, args_str: &str, prefix: &str) {
    handle_prefix_list(ctx, msg, args_str, prefix, ListKind::Top).await;
}

async fn handle_prefix_list(
    ctx: Arc<Context>,
    msg: &Message,
    args_str: &str,
    prefix: &str,
    kind: ListKind,
) {
    let _ = ctx.http.create_typing_trigger(msg.channel_id).await;
    let raw_arg = (!args_str.trim().is_empty()).then_some(args_str);

    let session = match build_profile_list_session(
        &ctx,
        msg.author.id,
        msg.guild_id,
        msg.channel_id,
        raw_arg,
        prefix,
        kind,
    )
    .await
    {
        Ok(session) => session,
        Err(err_msg) => {
            let _ = msg.error(&ctx, err_msg).await;
            return;
        }
    };

    let builder = build_session_message(&session);
    match msg.channel_id.create_message(&ctx, &builder).await {
        Ok(sent) => {
            register_session_and_spawn_expiry(Arc::clone(&ctx), sent.id, sent.channel_id, session);
        }
        Err(err) => {
            tracing::error!(?err, "failed to send profile render list embed");
        }
    }
}

fn rrn_info_message(info: ReplayInfoEmbed) -> (MessageBuilder<'static>, bool) {
    let has_pp_select = info.pp_select_row.is_some();
    let mut builder = MessageBuilder::new().embed(info.embed);
    if let Some(pp_row) = info.pp_select_row {
        builder = builder.components(vec![pp_row]);
    }
    (builder, has_pp_select)
}

pub async fn handle_prefix_rrn(ctx: Arc<Context>, msg: &Message, args_str: &str, prefix: &str) {
    let _ = ctx.http.create_typing_trigger(msg.channel_id).await;
    let raw_arg = (!args_str.trim().is_empty()).then_some(args_str);

    let (replay_data, info) = match prepare_rrn_play(
        &ctx,
        msg.author.id,
        msg.guild_id,
        msg.channel_id,
        raw_arg,
        prefix,
    )
    .await
    {
        Ok(res) => res,
        Err(err_msg) => {
            let _ = msg.error(&ctx, err_msg).await;
            return;
        }
    };

    // 1. Immediately return score info embed (with auto-expiring PP rework menu)
    let (info_builder, has_pp_select) = rrn_info_message(info);
    if let Ok(info_msg) = msg.channel_id.create_message(&ctx, &info_builder).await {
        if has_pp_select {
            spawn_pp_select_expiry(Arc::clone(&ctx), msg.channel_id, info_msg.id, Vec::new());
        }
    }

    // 2. Enqueue render (process_single_replay skips duplicate info embed and sends only replay video)
    enqueue_replay_in_channel(ctx, msg.channel_id, replay_data, None).await;
}

pub async fn handle_prefix_link(ctx: Arc<Context>, msg: &Message, args_str: &str, _prefix: &str) {
    let had_typed_arg = !args_str.trim().is_empty();
    let (builder, token_hash) = match prepare_oauth_link_message(
        &ctx,
        msg.author.id,
        msg.channel_id,
        msg.guild_id,
        had_typed_arg,
    )
    .await
    {
        Ok(res) => res,
        Err(err_msg) => {
            let _ = msg.error(&ctx, err_msg).await;
            return;
        }
    };

    // Already in a DM: safe to post the private verification link directly.
    if msg.guild_id.is_none() {
        let _ = msg.channel_id.create_message(&ctx, &builder).await;
        return;
    }

    // In a public server channel: deliver the single-use verification link via private DM
    // so nobody else in the channel can click it.
    let dm_sent = match ctx.http.create_private_channel(msg.author.id).await {
        Ok(resp) => match resp.model().await {
            Ok(dm_chan) => dm_chan.id.create_message(&ctx, &builder).await.is_ok(),
            Err(_) => false,
        },
        Err(_) => false,
    };

    if dm_sent {
        let notice = if had_typed_arg {
            "📩 Linking now uses **osu! OAuth** verification instead of typing a username. I sent your private verification link via DM — or run `/link` here for a private button!"
        } else {
            "📩 I sent you a private DM with your **osu! OAuth** verification link! (Or run `/link` here for a private button.)"
        };
        let _ = msg
            .channel_id
            .create_message(&ctx, &MessageBuilder::new().embed(notice))
            .await;
    } else {
        let _ = ctx.psql().delete_discord_link_token(&token_hash).await;
        let _ = msg
            .error(
                &ctx,
                "I couldn't DM you your private osu! verification link (DMs are closed, and verification links can't be posted publicly).\n\
                 Please run `/link` here to get an instant private verification button!",
            )
            .await;
    }
}

// ── Slash command entrypoints ─────────────────────────────────────────────────

pub async fn slash_rr(ctx: Arc<Context>, mut command: InteractionCommand) -> Result<()> {
    let args = Rr::from_interaction(command.input_data())?;
    handle_slash_list(ctx, command, args.user, ListKind::Recent).await
}

pub async fn slash_rtop(ctx: Arc<Context>, mut command: InteractionCommand) -> Result<()> {
    let args = Rtop::from_interaction(command.input_data())?;
    handle_slash_list(ctx, command, args.user, ListKind::Top).await
}

async fn handle_slash_list(
    ctx: Arc<Context>,
    command: InteractionCommand,
    user: Option<String>,
    kind: ListKind,
) -> Result<()> {
    let caller_id = command.user_id()?;
    let prefix = ctx.guild_prefix(command.guild_id);

    let session = match build_profile_list_session(
        &ctx,
        caller_id,
        command.guild_id,
        command.channel_id,
        user.as_deref(),
        prefix.as_ref(),
        kind,
    )
    .await
    {
        Ok(session) => session,
        Err(err_msg) => {
            command.error(&ctx, err_msg).await?;
            return Ok(());
        }
    };

    let builder = build_session_message(&session);
    let sent = command.update(&ctx, &builder).await?;
    register_session_and_spawn_expiry(Arc::clone(&ctx), sent.id, sent.channel_id, session);
    Ok(())
}

pub async fn slash_rrn(ctx: Arc<Context>, mut command: InteractionCommand) -> Result<()> {
    let args = Rrn::from_interaction(command.input_data())?;
    let caller_id = command.user_id()?;
    let prefix = ctx.guild_prefix(command.guild_id);

    let (replay_data, info) = match prepare_rrn_play(
        &ctx,
        caller_id,
        command.guild_id,
        command.channel_id,
        args.user.as_deref(),
        prefix.as_ref(),
    )
    .await
    {
        Ok(res) => res,
        Err(err_msg) => {
            command.error(&ctx, err_msg).await?;
            return Ok(());
        }
    };

    // 1. Replace deferred response immediately with the score info embed
    let (info_builder, has_pp_select) = rrn_info_message(info);
    let info_msg = command.update(&ctx, &info_builder).await?;
    if has_pp_select {
        spawn_pp_select_expiry(Arc::clone(&ctx), info_msg.channel_id, info_msg.id, Vec::new());
    }

    // 2. Post queue status card separately so completion deletes only the queue card
    enqueue_replay_in_channel(ctx, command.channel_id, replay_data, None).await;
    Ok(())
}

pub async fn slash_link(ctx: Arc<Context>, command: InteractionCommand) -> Result<()> {
    let caller_id = command.user_id()?;

    match prepare_oauth_link_message(&ctx, caller_id, command.channel_id, command.guild_id, false)
        .await
    {
        Ok((builder, _)) => {
            command.update(&ctx, &builder).await?;
        }
        Err(err_msg) => {
            command.error(&ctx, err_msg).await?;
        }
    }
    Ok(())
}

// ── Interactive component handlers (strictly author-only) ─────────────────────

fn parse_profile_component_id(custom_id: &str) -> Option<(Id<UserMarker>, &str)> {
    let mut parts = custom_id.split(':');
    let _prefix = parts.next()?;
    let owner_id = parts
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .and_then(Id::new_checked)?;
    Some((owner_id, parts.next().unwrap_or("")))
}

async fn authorize_profile_component<T>(
    ctx: &Context,
    component: &InteractionComponent,
    invalid_msg: &str,
    not_owner_msg: &str,
    lookup_fn: impl FnOnce(&mut ProfileScoreSession, &str) -> Option<T>,
) -> Result<Option<T>> {
    let Some((owner_id, arg)) = parse_profile_component_id(&component.data.custom_id) else {
        component.ephemeral_text(ctx, invalid_msg).await?;
        return Ok(None);
    };

    let caller_id = component.user_id()?;
    if caller_id != owner_id {
        component.ephemeral_text(ctx, not_owner_msg).await?;
        return Ok(None);
    }

    enum Lookup<T> {
        Ready(T),
        Unauthorized,
        Expired,
    }

    let lookup = {
        let mut map = session_map().lock().unwrap_or_else(|p| p.into_inner());
        if let Some(session) = map.get_mut(&component.message.id) {
            if session.owner_id != caller_id {
                Lookup::Unauthorized
            } else {
                session.last_activity = Instant::now();
                match lookup_fn(session, arg) {
                    Some(val) => Lookup::Ready(val),
                    None => Lookup::Expired,
                }
            }
        } else {
            Lookup::Expired
        }
    };

    match lookup {
        Lookup::Ready(val) => Ok(Some(val)),
        Lookup::Unauthorized => {
            component.ephemeral_text(ctx, not_owner_msg).await?;
            Ok(None)
        }
        Lookup::Expired => {
            component
                .ephemeral_text(ctx, "This play list has expired. Please run the command again.")
                .await?;
            Ok(None)
        }
    }
}

pub async fn handle_profile_page_button(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let Some(builder) = authorize_profile_component(
        &ctx,
        &component,
        "Invalid pagination button.",
        "Only the user who ran this command can use these buttons.",
        |session, action| {
            let total_pages = total_pages_for(session.entries.len(), PLAYS_PER_PAGE);
            match action {
                "first" => session.page = 0,
                "prev" => session.page = session.page.saturating_sub(1),
                "next" => session.page = (session.page + 1).min(total_pages - 1),
                "last" => session.page = total_pages - 1,
                _ => {}
            }
            Some(Box::new(build_session_message(session)))
        },
    )
    .await?
    else {
        return Ok(());
    };

    component.callback(&ctx, *builder).await
}

pub async fn handle_profile_render_button(
    ctx: Arc<Context>,
    component: InteractionComponent,
) -> Result<()> {
    let Some(abs_idx) = component
        .data
        .custom_id
        .split(':')
        .nth(2)
        .and_then(|s| s.parse::<usize>().ok())
    else {
        return component.ephemeral_text(&ctx, "Invalid render button.").await;
    };

    let Some(entry) = authorize_profile_component(
        &ctx,
        &component,
        "Invalid render button.",
        "Only the user who ran this command can render plays from this list.",
        |session, _| session.entries.get(abs_idx).cloned().map(Box::new),
    )
    .await?
    else {
        return Ok(());
    };
    let entry = *entry;

    let caller_id = component.user_id()?;
    if let Err(err_msg) =
        check_render_access(&ctx, caller_id, component.guild_id, component.channel_id).await
    {
        return component.ephemeral_text(&ctx, err_msg).await;
    }

    if let Err(err_msg) = check_user_queue_limit(&ctx, caller_id).await {
        return component.ephemeral_text(&ctx, err_msg).await;
    }

    let Some(_in_flight) = InFlightGuard::try_acquire(caller_id, entry.score.id) else {
        return component
            .ephemeral_text(
                &ctx,
                "Already downloading and queuing that replay — please wait a moment!",
            )
            .await;
    };

    // Defer ephemerally so downloading the .osr never hits Discord's 3-second interaction deadline
    component.defer_ephemeral(&ctx).await?;

    let map_label = score_map_display(&entry.score);
    let mods_label = format_score_mods(&entry.score.mods, entry.score.set_on_lazer);

    let replay_data = match prepare_score_replay_data(
        &ctx,
        entry.score,
        component.channel_id,
        caller_id,
        RenderOptions::default(),
    )
    .await
    {
        Ok(ScorePrepareOutcome::Ready(data)) => *data,
        Ok(ScorePrepareOutcome::Unavailable(reason)) => {
            let err_embed = EmbedBuilder::new().description(reason).color(RED).build();
            let _ = component
                .update_response(&ctx, &MessageBuilder::new().embed(err_embed))
                .await;
            return Ok(());
        }
        Err(err) => {
            tracing::warn!(?err, "failed to prepare replay from profile render button");
            let err_embed = EmbedBuilder::new()
                .description("Failed to download or parse the replay file.")
                .color(RED)
                .build();
            let _ = component
                .update_response(&ctx, &MessageBuilder::new().embed(err_embed))
                .await;
            return Ok(());
        }
    };

    enqueue_replay_in_channel(Arc::clone(&ctx), component.channel_id, replay_data, None).await;

    let confirmation = MessageBuilder::new().content(format!(
        "✅ Queued **{}** `{}` for rendering!",
        truncate_with_ellipsis(&map_label, 120, "…"),
        mods_label
    ));
    let _ = component.update_response(&ctx, &confirmation).await;

    Ok(())
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rosu_v2::model::score::{ScoreStatistics, UserAttributes};
    use rosu_v2::prelude::Grade;
    use time::OffsetDateTime;

    fn dummy_score(id: u64, mode: GameMode, passed: bool, has_replay: bool, pp: Option<f32>) -> Score {
        Score {
            set_on_lazer: false,
            classic_score: 0,
            ranked: Some(true),
            preserve: Some(true),
            processed: Some(true),
            maximum_statistics: ScoreStatistics::default(),
            mods: GameMods::default(),
            statistics: ScoreStatistics::default(),
            map_id: 1000 + id as u32,
            best_id: None,
            id,
            grade: Grade::S,
            kind: Box::from("score_best_osu"),
            user_id: 7562902,
            accuracy: 99.12,
            build_id: None,
            ended_at: OffsetDateTime::UNIX_EPOCH,
            has_replay,
            is_perfect_combo: false,
            legacy_perfect: None,
            legacy_score_id: Some(id),
            legacy_score: 1_000_000,
            max_combo: 500,
            passed,
            pp,
            mode,
            started_at: None,
            score: 1_000_000,
            replay: has_replay,
            current_user_attributes: UserAttributes { pin: None },
            total_score_without_mods: None,
            map: None,
            mapset: None,
            rank_global: None,
            user: None,
            weight: None,
        }
    }

    #[test]
    fn parses_profile_refs() {
        assert_eq!(parse_osu_profile_ref(""), None);
        assert_eq!(parse_osu_profile_ref("   "), None);
        assert_eq!(
            parse_osu_profile_ref("<@123456789012345678>"),
            Some(OsuProfileTarget::DiscordUser(Id::new(123456789012345678)))
        );
        assert_eq!(
            parse_osu_profile_ref("<@!98765432109876543>"),
            Some(OsuProfileTarget::DiscordUser(Id::new(98765432109876543)))
        );
        assert_eq!(
            parse_osu_profile_ref("https://osu.ppy.sh/users/7562902/osu"),
            Some(OsuProfileTarget::OsuId(7562902))
        );
        assert_eq!(
            parse_osu_profile_ref("https://osu.ppy.sh/u/7562902"),
            Some(OsuProfileTarget::OsuId(7562902))
        );
        assert_eq!(
            parse_osu_profile_ref("lazer.ppy.sh/users/White%20Cat/osu?tab=top"),
            Some(OsuProfileTarget::OsuUsername("White Cat".to_owned()))
        );
        assert_eq!(
            parse_osu_profile_ref("7562902"),
            Some(OsuProfileTarget::OsuId(7562902))
        );
        assert_eq!(
            parse_osu_profile_ref("\"Black W0lf\""),
            Some(OsuProfileTarget::OsuUsername("Black W0lf".to_owned()))
        );
        assert_eq!(
            parse_osu_profile_ref("mrekk"),
            Some(OsuProfileTarget::OsuUsername("mrekk".to_owned()))
        );
    }

    #[test]
    fn filters_only_renderable_std_passes() {
        assert!(is_score_renderable(&dummy_score(1, GameMode::Osu, true, true, Some(500.0))));
        assert!(!is_score_renderable(&dummy_score(2, GameMode::Osu, false, true, Some(500.0))));
        assert!(!is_score_renderable(&dummy_score(3, GameMode::Osu, true, false, Some(500.0))));
        assert!(!is_score_renderable(&dummy_score(4, GameMode::Taiko, true, true, Some(500.0))));
        assert!(!is_score_renderable(&dummy_score(5, GameMode::Mania, true, true, Some(500.0))));
    }

    #[test]
    fn top_merge_preserves_true_rank_and_sorts_descending_pp() {
        let p1 = vec![
            dummy_score(10, GameMode::Osu, true, true, Some(900.0)),  // #1 (renderable)
            dummy_score(11, GameMode::Osu, true, false, Some(850.0)), // #2 (no replay)
            dummy_score(12, GameMode::Osu, true, true, Some(800.0)),  // #3 (renderable)
        ];
        // Include a duplicate id=12 and a higher-PP play out of order to verify dedup & sorting
        let p2 = vec![
            dummy_score(12, GameMode::Osu, true, true, Some(800.0)),  // dup
            dummy_score(13, GameMode::Osu, true, true, Some(875.0)),  // actually #2!
            dummy_score(14, GameMode::Osu, true, true, Some(700.0)),  // #5 (renderable)
        ];

        let (checked, entries) = merge_and_filter_top_scores(p1, p2);
        assert_eq!(checked, 5);
        let ranks_and_ids: Vec<(usize, u64)> =
            entries.iter().map(|e| (e.display_index, e.score.id)).collect();
        // Sorted PP: 900 (#1, ok), 875 (#2, ok), 850 (#3, no replay), 800 (#4, ok), 700 (#5, ok)
        assert_eq!(ranks_and_ids, vec![(1, 10), (2, 13), (4, 12), (5, 14)]);
    }

    #[test]
    fn pagination_math_splits_by_five() {
        assert_eq!(total_pages_for(0, 5), 1);
        assert_eq!(total_pages_for(5, 5), 1);
        assert_eq!(total_pages_for(6, 5), 2);
        assert_eq!(total_pages_for(11, 5), 3);
        assert_eq!(page_bounds(11, 0, 5), (0, 5));
        assert_eq!(page_bounds(11, 1, 5), (5, 10));
        assert_eq!(page_bounds(11, 2, 5), (10, 11));
        assert_eq!(page_bounds(11, 99, 5), (10, 11));
    }

    #[test]
    fn long_map_titles_and_brackets_never_break_discord_markdown_links() {
        // Exact regression case: long title whose diff used to be truncated mid-[bracket]
        let rendered = format_map_link_parts(
            5335659,
            "Masayoshi Minoshima feat. misato - Necro Fantasia (Eunomia Ver.)",
            Some("Unimaginable"),
        );
        // Extract the `[label](url)` link text and verify zero brackets/backslashes/asterisks inside
        let after_open = rendered.strip_prefix("**[").expect("starts with **[");
        let (label, rest) = after_open
            .split_once("](https://osu.ppy.sh/beatmaps/5335659)")
            .expect("valid markdown link");
        assert!(!label.contains('[') && !label.contains(']'));
        assert!(!label.contains('\\') && !label.contains('*'));
        assert_eq!(rest, " [Unimaginable]**");

        // Titles with raw `[brackets]`, `**stars**`, and `\` inside title & diff
        let nasty = format_map_link_parts(
            12345,
            "Camellia [feat. Nanahira] - **Song\\Name**",
            Some("[Super Long Difficulty Name That Exceeds Twenty Eight Chars]"),
        );
        let after_open = nasty.strip_prefix("**[").expect("starts with **[");
        let (label, rest) = after_open
            .split_once("](https://osu.ppy.sh/beatmaps/12345)")
            .expect("valid markdown link");
        assert!(!label.contains('[') && !label.contains(']') && !label.contains('\\'));
        assert!(!label.contains('*'));
        assert!(rest.starts_with(" [") && rest.ends_with("]**"));
        assert!(!rest.contains('\\'));
    }

    #[test]
    fn hides_implicit_classic_mod_on_stable_only() {
        use rosu_v2::prelude::GameMod;
        let mut mods = GameMods::new();
        mods.insert(GameMod::ClassicOsu(Default::default()));
        assert_eq!(format_score_mods(&mods, false), "NM");
        assert_eq!(format_score_mods(&mods, true), "+CL");

        mods.insert(GameMod::HiddenOsu(Default::default()));
        mods.insert(GameMod::DoubleTimeOsu(Default::default()));
        assert_eq!(format_score_mods(&mods, false), "+HDDT");
        let lazer_mods = format_score_mods(&mods, true);
        assert!(lazer_mods.starts_with('+'));
        assert!(lazer_mods.contains("HD"));
        assert!(lazer_mods.contains("DT"));
        assert!(lazer_mods.contains("CL"));
    }
}
