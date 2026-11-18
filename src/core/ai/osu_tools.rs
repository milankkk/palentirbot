use std::{collections::HashSet, time::Duration};

use reqwest::Client;
use rosu_v2::{
    model::{
        beatmap::{BeatmapsetSearchSort, RankStatus},
        GameMode,
    },
    prelude::{GameModsIntermode, Osu, Score, UserId},
};

const MAX_OSU_FILE_BYTES: usize = 5 * 1024 * 1024;

macro_rules! osu_call_with_refresh {
    ($osu_ctx:expr, |$client:ident| $req:expr) => {{
        let $client: &Osu = $osu_ctx.osu;
        match $req.await {
            Ok(val) => Ok(val),
            Err(err) => {
                if crate::core::Context::is_osu_auth_error(&err) {
                    if let Some(ctx) = $osu_ctx.ctx {
                        if let Ok(fresh) = ctx.refresh_osu_client().await {
                            let $client: &Osu = fresh.as_ref();
                            $req.await
                        } else {
                            Err(err)
                        }
                    } else {
                        Err(err)
                    }
                } else {
                    Err(err)
                }
            }
        }
    }};
}

/// Live osu! API client paired with Discord handles that must not be guessed as osu! IGNs.
#[derive(Clone, Copy)]
pub struct OsuToolContext<'a> {
    pub osu: &'a Osu,
    pub blocked_discord_handles: &'a [String],
    pub raw_user_prompt: Option<&'a str>,
    pub ctx: Option<&'a crate::core::Context>,
}

pub fn parse_game_mode(raw: Option<&str>) -> GameMode {
    match raw
        .map(str::trim)
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "taiko" | "osu!taiko" => GameMode::Taiko,
        "catch" | "ctb" | "fruits" | "osu!catch" => GameMode::Catch,
        "mania" | "osu!mania" => GameMode::Mania,
        _ => GameMode::Osu,
    }
}

/// Parse classic osu! mod acronyms (e.g. `HDDT`, `HD,HR`, `+EZDT`, `HD and DT`, `4KHD`) into legacy bitmask + label.
pub fn parse_mod_bits(raw: Option<&str>) -> (u32, String) {
    let input = raw.unwrap_or("").trim();
    if input.is_empty() {
        return (0, "NM".to_owned());
    }

    let mut bits = 0u32;
    let mut labels = Vec::new();

    for token in input
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        let upper = token.to_ascii_uppercase();
        if matches!(
            upper.as_str(),
            "NM" | "NOMOD" | "AND" | "WITH" | "MOD" | "MODS" | "PLUS" | "OR"
        ) {
            continue;
        }

        let bytes = upper.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            // Skip mania key count markers like `4K`, `7K`, `10K` inside `4KHD` / `HD4K`.
            if bytes[i].is_ascii_digit() {
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'K' {
                    i += 1;
                }
                continue;
            }

            if i + 1 >= bytes.len() {
                break;
            }
            if bytes[i + 1].is_ascii_digit() {
                i += 1;
                continue;
            }

            let pair = &upper[i..i + 2];
            if pair == "CL" {
                i += 2;
                continue;
            }
            let (bit, canonical) = match pair {
                "NF" => (1 << 0, "NF"),
                "EZ" => (1 << 1, "EZ"),
                "TD" => (1 << 2, "TD"),
                "HD" => (1 << 3, "HD"),
                "HR" => (1 << 4, "HR"),
                "SD" => (1 << 5, "SD"),
                "DT" => (1 << 6, "DT"),
                "RX" => (1 << 7, "RX"),
                "HT" => (1 << 8, "HT"),
                "DC" => (1 << 8, "DC"),
                "NC" => ((1 << 9) | (1 << 6), "NC"),
                "FL" => (1 << 10, "FL"),
                "SO" => (1 << 12, "SO"),
                "PF" => ((1 << 14) | (1 << 5), "PF"),
                _ => (0, ""),
            };
            if bit != 0 {
                if !labels.contains(&canonical) {
                    bits |= bit;
                    labels.push(canonical);
                }
                i += 2;
            } else {
                i += 1;
            }
        }
    }

    if labels.is_empty() {
        (0, "NM".to_owned())
    } else {
        (bits, labels.concat())
    }
}

/// Parse an osu! beatmap ID from either a raw number or a `/b/`, `/beatmaps/`, `#mode/` URL.
pub fn parse_beatmap_id(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(id) = trimmed.parse::<u32>() {
        return (id > 0).then_some(id);
    }

    // Check fragment difficulty IDs first so query params before `#` (e.g. `?mode=osu#osu/2116202`) work.
    for marker in ["#osu/", "#taiko/", "#fruits/", "#catch/", "#mania/"] {
        if let Some(pos) = trimmed.find(marker) {
            let rest = &trimmed[pos + marker.len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(id) = digits.parse::<u32>() {
                if id > 0 {
                    return Some(id);
                }
            }
        }
    }

    let without_query = trimmed.split(['?', '#']).next().unwrap_or(trimmed);
    for marker in ["/beatmaps/", "/b/"] {
        if let Some(pos) = without_query.find(marker) {
            let rest = &without_query[pos + marker.len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(id) = digits.parse::<u32>() {
                if id > 0 {
                    return Some(id);
                }
            }
        }
    }

    None
}

/// Parse an osu! beatmapset ID from `/beatmapsets/<id>` or `/s/<id>` URLs.
pub fn parse_beatmapset_id(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_query = trimmed.split(['?', '#']).next().unwrap_or(trimmed);
    for marker in ["/beatmapsets/", "/s/"] {
        if let Some(pos) = without_query.find(marker) {
            let rest = &without_query[pos + marker.len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(id) = digits.parse::<u32>() {
                if id > 0 {
                    return Some(id);
                }
            }
        }
    }
    None
}

/// Extract verified osu! beatmap IDs referenced inside osu! URLs in text.
pub fn extract_beatmap_ids_from_text(text: &str) -> Vec<u32> {
    let mut ids = Vec::new();
    let mut push_id = |id: u32| {
        if !ids.contains(&id) {
            ids.push(id);
        }
    };

    for url in super::search::extract_http_urls(text) {
        if url.to_ascii_lowercase().contains("osu.ppy.sh") {
            if let Some(id) = parse_beatmap_id(&url) {
                push_id(id);
            }
        }
    }

    let lower = text.to_ascii_lowercase();
    let mut search_from = 0usize;
    while let Some(rel) = lower[search_from..].find("osu.ppy.sh/") {
        let start = search_from + rel;
        let rest = &text[start..];
        let end_rel = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`' | '(' | ')' | '[' | ']' | ','))
            .unwrap_or(rest.len());
        let token = &rest[..end_rel];
        if let Some(id) = parse_beatmap_id(token) {
            push_id(id);
        }
        search_from = start + end_rel.max("osu.ppy.sh/".len());
    }

    ids
}

fn percent_decode_utf8(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn extract_raw_player_u64(args: &serde_json::Value) -> Option<u64> {
    args.get("user_id")
        .or_else(|| args.get("username"))
        .or_else(|| args.get("user"))
        .or_else(|| args.get("player"))
        .and_then(|v| v.as_u64())
}

fn extract_raw_player_str(args: &serde_json::Value) -> Option<&str> {
    args.get("username")
        .or_else(|| args.get("user"))
        .or_else(|| args.get("player"))
        .or_else(|| args.as_str().map(|_| args))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Parse a user ID or username from tool call arguments or an `/users/<id|name>` or `/u/<id|name>` profile URL.
pub fn parse_osu_user_id(args: &serde_json::Value) -> Option<UserId> {
    if let Some(id) = extract_raw_player_u64(args).and_then(|v| u32::try_from(v).ok()) {
        return Some(UserId::Id(id));
    }

    let raw = extract_raw_player_str(args)?;

    for marker in ["/users/", "/u/"] {
        if let Some(pos) = raw.find(marker) {
            let rest = &raw[pos + marker.len()..];
            let seg = rest.split(['/', '?', '#']).next().unwrap_or("").trim();
            if !seg.is_empty() {
                if seg.bytes().all(|b| b.is_ascii_digit()) {
                    if let Ok(id) = seg.parse::<u32>() {
                        return Some(UserId::Id(id));
                    }
                }
                return Some(UserId::Name(percent_decode_utf8(seg).into()));
            }
        }
    }

    if raw.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(id) = raw.parse::<u32>() {
            return Some(UserId::Id(id));
        }
    }

    Some(UserId::Name(raw.into()))
}

fn summarize_score(score: &Score) -> serde_json::Value {
    let map_title = score
        .mapset
        .as_ref()
        .map(|ms| format!("{} - {}", ms.artist, ms.title))
        .unwrap_or_else(|| format!("Map #{}", score.map_id));
    let diff = score
        .map
        .as_ref()
        .map(|m| m.version.as_str())
        .unwrap_or("?");
    let stars = score.map.as_ref().map(|m| (m.stars * 100.0).round() / 100.0);
    let mods_str = score.mods.to_string();
    let mods_label = if mods_str.is_empty() {
        "NM".to_owned()
    } else {
        mods_str
    };

    serde_json::json!({
        "score_id": score.id,
        "beatmap_id": score.map_id,
        "username": score.user.as_ref().map(|u| u.username.as_str()),
        "user_id": score.user_id,
        "map": format!("{map_title} [{diff}]"),
        "map_nm_stars": stars,
        "map_max_combo": score.map.as_ref().and_then(|m| m.max_combo),
        "mods": mods_label,
        "grade": format!("{:?}", score.grade),
        "acc": format!("{:.2}%", score.accuracy),
        "pp": score.pp.map(|p| (p * 100.0).round() / 100.0),
        "weighted_pp": score.weight.as_ref().map(|w| (w.pp * 100.0).round() / 100.0),
        "combo": score.max_combo,
        "misses": score.statistics.miss,
        "hits": {
            "300": score.statistics.great,
            "100": score.statistics.ok,
            "50": score.statistics.meh,
            "miss": score.statistics.miss,
        },
        "rank_global": score.rank_global,
        "date": score.ended_at.date().to_string(),
        "passed": score.passed,
        "set_on_lazer": score.set_on_lazer,
    })
}

pub fn compute_digit_tier(global_rank: Option<u32>) -> &'static str {
    match global_rank.filter(|&r| r > 0) {
        None => "unranked",
        Some(1..=9) => "1_digit",
        Some(10..=99) => "2_digit",
        Some(100..=999) => "3_digit",
        Some(1_000..=9_999) => "4_digit",
        Some(10_000..=99_999) => "5_digit",
        Some(100_000..=999_999) => "6_digit",
        Some(_) => "7_digit",
    }
}

pub fn compute_bws_rank(global_rank: u32, badge_count: usize) -> u32 {
    if global_rank == 0 || badge_count == 0 {
        return global_rank;
    }
    let b = i32::try_from(badge_count.min(100)).unwrap_or(100);
    let exp = 0.9937_f64.powi(b * b);
    (f64::from(global_rank)).powf(exp).round().max(1.0) as u32
}

fn looks_like_discord_user_ref(args: &serde_json::Value) -> Option<String> {
    if let Some(id) = extract_raw_player_u64(args) {
        if id > u64::from(u32::MAX) {
            return Some(id.to_string());
        }
    }

    let raw = extract_raw_player_str(args)?;
    if (raw.starts_with("<@") && raw.ends_with('>'))
        || (raw.len() >= 17 && raw.bytes().all(|b| b.is_ascii_digit()))
    {
        return Some(raw.to_owned());
    }

    None
}

fn discord_ref_blocked_message(label: &str, kind: &str) -> String {
    format!(
        "[osu_player blocked: `{label}` is a {kind}, NOT a confirmed osu! IGN or user ID. Render Slave does NOT store Discord-to-osu! links and did NOT call the osu! API (do NOT tell the user the osu! API returned a 404). Check recent `[Bot ...]` embeds in chat or replies for an `osu.ppy.sh/users/<id>` profile URL or linked osu! IGN; if none is present, tell the user you don't know their osu! username yet and ask them to run `<osu` (for themselves) or `<osu @name` (for someone else — with ONLY the leading `<` and NO closing `>`) in chat first, or reply with their osu! IGN or profile URL.]"
    )
}

/// Reject Discord IDs, mentions, `@handles`, or unconfirmed Discord usernames before hitting the osu! API.
pub fn blocked_osu_player_error(
    args: &serde_json::Value,
    blocked_discord_handles: &[String],
) -> Option<String> {
    if let Some(discord_ref) = looks_like_discord_user_ref(args) {
        return Some(discord_ref_blocked_message(
            &discord_ref,
            "Discord user ID/mention",
        ));
    }

    let raw = extract_raw_player_str(args)?;
    if raw.contains("/users/") || raw.contains("/u/") || raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    let had_at = raw.starts_with('@');
    let stripped_suffix = raw
        .strip_suffix("(Discord user)")
        .or_else(|| raw.strip_suffix("(Discord bot)"))
        .map(str::trim)
        .unwrap_or(raw);
    let had_discord_suffix = stripped_suffix.len() != raw.len();
    let cleaned = stripped_suffix.trim_start_matches('@').trim();

    if had_at || had_discord_suffix {
        return Some(discord_ref_blocked_message(raw, "Discord @mention/handle"));
    }

    if !cleaned.is_empty()
        && blocked_discord_handles
            .iter()
            .any(|h| h.eq_ignore_ascii_case(cleaned))
    {
        return Some(discord_ref_blocked_message(
            raw,
            "unconfirmed Discord username",
        ));
    }

    None
}

/// Look up live osu! profile stats, top 5 plays, 5 recent plays, and pinned or #1 plays.
pub async fn execute_osu_player_tool(
    osu_ctx: &OsuToolContext<'_>,
    args: &serde_json::Value,
    verified_ids: &mut HashSet<u32>,
) -> String {
    if let Some(err) = blocked_osu_player_error(args, osu_ctx.blocked_discord_handles) {
        return err;
    }
    let Some(user_id) = parse_osu_user_id(args) else {
        return "[osu_player error: missing `username` argument]".to_owned();
    };
    let mode = parse_game_mode(args.get("mode").and_then(|v| v.as_str()));
    let scores_type = args
        .get("scores_type")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_ascii_lowercase());
    let want_firsts = matches!(scores_type.as_deref(), Some("firsts" | "first"));

    let user = match osu_call_with_refresh!(osu_ctx, |osu| {
        let uid = user_id.clone();
        osu.user(uid).mode(mode)
    }) {
        Ok(u) => u,
        Err(err) => return format!("[osu_player error: could not fetch player: {err}]"),
    };

    let (top_res, recent_res, extra_res) = tokio::join!(
        async {
            osu_call_with_refresh!(osu_ctx, |osu| osu
                .user_scores(user.user_id)
                .best()
                .mode(mode)
                .limit(5)
                .into_future_wrapper())
        },
        async {
            osu_call_with_refresh!(osu_ctx, |osu| osu
                .user_scores(user.user_id)
                .recent()
                .include_fails(true)
                .mode(mode)
                .limit(5)
                .into_future_wrapper())
        },
        async {
            if want_firsts {
                osu_call_with_refresh!(osu_ctx, |osu| osu
                    .user_scores(user.user_id)
                    .firsts()
                    .mode(mode)
                    .limit(5)
                    .into_future_wrapper())
            } else {
                osu_call_with_refresh!(osu_ctx, |osu| osu
                    .user_scores(user.user_id)
                    .pinned()
                    .mode(mode)
                    .limit(3)
                    .into_future_wrapper())
            }
        },
    );

    let top_scores = top_res.unwrap_or_default();
    let recent_scores = recent_res.unwrap_or_default();
    let extra_scores = extra_res.unwrap_or_default();
    for s in top_scores
        .iter()
        .chain(recent_scores.iter())
        .chain(extra_scores.iter())
    {
        verified_ids.insert(s.map_id);
    }

    let top_plays: Vec<serde_json::Value> = top_scores.iter().map(summarize_score).collect();
    let recent_plays: Vec<serde_json::Value> = recent_scores.iter().map(summarize_score).collect();
    let extra_plays: Vec<serde_json::Value> = extra_scores.iter().map(summarize_score).collect();

    let stats = user.statistics.as_ref();
    let global_rank = stats.and_then(|s| s.global_rank);
    let badges_vec = user.badges.as_deref().unwrap_or(&[]);
    let badges_count = badges_vec.len();
    let badge_descriptions: Vec<&str> = badges_vec
        .iter()
        .take(6)
        .map(|b| b.description.as_str())
        .collect();
    let approx_bws_rank = global_rank.map(|r| compute_bws_rank(r, badges_count));

    let playstyles: Vec<String> = user
        .playstyle
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|p| format!("{p:?}").to_ascii_lowercase())
        .collect();
    let previous_usernames: Vec<&str> = user
        .previous_usernames
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|u| u.as_str())
        .collect();

    let mut payload = serde_json::json!({
        "username": user.username.as_str(),
        "user_id": user.user_id,
        "profile_url": format!("https://osu.ppy.sh/users/{}", user.user_id),
        "country": user.country_code.as_str(),
        "mode": format!("{mode:?}"),
        "global_rank": global_rank,
        "digit_tier": compute_digit_tier(global_rank),
        "country_rank": stats.and_then(|s| s.country_rank),
        "peak_rank": user.highest_rank.as_ref().map(|h| serde_json::json!({
            "rank": h.rank,
            "date": h.updated_at.date().to_string(),
        })),
        "approx_bws_rank": approx_bws_rank,
        "badges_count": badges_count,
        "badges": badge_descriptions,
        "pp": stats.map(|s| (s.pp * 100.0).round() / 100.0),
        "accuracy": stats.map(|s| format!("{:.2}%", s.accuracy)),
        "playcount": stats.map(|s| s.playcount),
        "playtime_hours": stats.map(|s| s.playtime / 3600),
        "level": stats.map(|s| s.level.current),
        "max_combo": stats.map(|s| s.max_combo),
        "grade_counts": stats.map(|s| serde_json::json!({
            "ssh": s.grade_counts.ssh,
            "ss": s.grade_counts.ss,
            "sh": s.grade_counts.sh,
            "s": s.grade_counts.s,
            "a": s.grade_counts.a,
        })),
        "first_place_count": user.scores_first_count,
        "medals_count": user.medals.as_ref().map(|m| m.len()),
        "playstyle": playstyles,
        "previous_usernames": previous_usernames,
        "join_year": user.join_date.year(),
        "mapper_stats": {
            "ranked_mapsets": user.ranked_mapset_count.unwrap_or(0),
            "loved_mapsets": user.loved_mapset_count.unwrap_or(0),
            "pending_mapsets": user.pending_mapset_count.unwrap_or(0),
            "graveyard_mapsets": user.graveyard_mapset_count.unwrap_or(0),
            "kudosu_total": user.kudosu.total,
        },
        "is_online": user.is_online,
        "top_5_plays": top_plays,
        "recent_plays": recent_plays,
    });

    if let Some(obj) = payload.as_object_mut() {
        if want_firsts {
            obj.insert("first_place_plays".to_owned(), serde_json::Value::Array(extra_plays));
        } else {
            obj.insert("pinned_plays".to_owned(), serde_json::Value::Array(extra_plays));
        }
    }

    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned())
}

// Helper trait to evaluate IntoFuture directly inside tokio::join!
trait ScoreRequestExt<'a> {
    async fn into_future_wrapper(self) -> Result<Vec<Score>, rosu_v2::error::OsuError>;
}

impl<'a> ScoreRequestExt<'a> for rosu_v2::request::GetUserScores<'a> {
    async fn into_future_wrapper(self) -> Result<Vec<Score>, rosu_v2::error::OsuError> {
        self.await
    }
}

fn parse_search_status(raw: Option<&str>) -> Option<Option<RankStatus>> {
    let s = raw?.trim().to_ascii_lowercase();
    match s.as_str() {
        "" => None,
        "any" | "all" => Some(None),
        "ranked" => Some(Some(RankStatus::Ranked)),
        "loved" => Some(Some(RankStatus::Loved)),
        "qualified" => Some(Some(RankStatus::Qualified)),
        "pending" => Some(Some(RankStatus::Pending)),
        "wip" => Some(Some(RankStatus::WIP)),
        "graveyard" => Some(Some(RankStatus::Graveyard)),
        "approved" => Some(Some(RankStatus::Approved)),
        _ => None,
    }
}

const SEARCH_META_SLANG_PHRASES: &[&str] = &[
    "farm map",
    "farm maps",
    "aim farm",
    "dt farm",
    "stream farm",
    "tapping farm",
    "speed farm",
    "burst farm",
    "aim slop",
    "tech map",
    "tech maps",
    "stream map",
    "stream maps",
    "jump map",
    "jump maps",
    "alt map",
    "alt maps",
    "gimmick map",
    "stamina map",
    "speed map",
    "reading map",
    "6 digit",
    "5 digit",
    "4 digit",
    "7 digit",
    "3 digit",
    "6-digit",
    "5-digit",
    "4-digit",
    "mappool",
    "tournament map",
];

fn query_has_osu_operators(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    [
        "stars>", "stars<", "stars=", "bpm>", "bpm<", "bpm=", "ar>", "ar<", "ar=", "cs>", "cs<",
        "cs=", "od>", "od<", "od=", "length>", "length<", "creator=", "status=",
    ]
    .iter()
    .any(|op| lower.contains(op))
}

fn contains_meta_slang(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    if SEARCH_META_SLANG_PHRASES.iter().any(|p| lower.contains(p)) {
        return true;
    }
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| matches!(w, "farm" | "farming"))
}

fn strip_ascii_ci(haystack: &str, needle: &str) -> String {
    let mut out = String::with_capacity(haystack.len());
    let hay_lower = haystack.to_ascii_lowercase();
    let needle_lower = needle.to_ascii_lowercase();
    let mut i = 0usize;
    while let Some(rel) = hay_lower[i..].find(&needle_lower) {
        let idx = i + rel;
        out.push_str(&haystack[i..idx]);
        i = idx + needle_lower.len();
    }
    out.push_str(&haystack[i..]);
    out
}

pub fn sanitize_beatmap_search_query(
    raw_query: &str,
    has_structured_filters: bool,
) -> std::result::Result<String, String> {
    let trimmed = raw_query.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let has_ops = query_has_osu_operators(trimmed);
    let has_slang = contains_meta_slang(trimmed);
    if has_slang && !has_structured_filters && !has_ops {
        return Err(
            "[osu_beatmap_search blocked: osu! beatmap search matches LITERAL song titles, artists, mappers, tags, and operators — passing community slang like `farm`, `farm map`, `6 digit`, or `tapping farm` returns wrong songs containing the word \"Farmer\" (e.g. Farmer's Market). Re-call `osu_beatmap_search` NOW with `query: \"\"` plus structured filters: `min_stars`/`max_stars` (e.g. 4.3..5.2 for 6-digit), `min_bpm`/`max_bpm`, `status: \"ranked\"`, `sort: \"plays\"` or `\"favourites\"`, and/or `creator` (e.g. Sotarks, browiec,Reform, Log Off Now for jumps; Kuki1537, Plasma, Monstrata, Akitoshi for alt/streams).]"
                .to_owned(),
        );
    }

    if !has_slang {
        return Ok(trimmed.to_owned());
    }

    let mut cleaned = trimmed.to_owned();
    for phrase in SEARCH_META_SLANG_PHRASES {
        cleaned = strip_ascii_ci(&cleaned, phrase);
    }
    let words: Vec<&str> = cleaned
        .split_whitespace()
        .filter(|w| {
            let lw = w.trim_matches(|c: char| !c.is_ascii_alphanumeric()).to_ascii_lowercase();
            !matches!(lw.as_str(), "farm" | "farming")
        })
        .collect();
    Ok(words.join(" "))
}

fn fmt_num_clean(n: f64) -> String {
    let s = format!("{n:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn format_attr_filter(val: &serde_json::Value, key: &str) -> Option<String> {
    if let Some(n) = val.as_f64().filter(|n| n.is_finite()) {
        return Some(format!("{key}={}", fmt_num_clean(n)));
    }
    let raw = val.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    let (op, num_str) = if let Some(rest) = raw.strip_prefix(">=") {
        (">=", rest.trim())
    } else if let Some(rest) = raw.strip_prefix("<=") {
        ("<=", rest.trim())
    } else if let Some(rest) = raw.strip_prefix("==") {
        ("=", rest.trim())
    } else if let Some(rest) = raw.strip_prefix("!=") {
        ("!=", rest.trim())
    } else if let Some(rest) = raw.strip_prefix('>') {
        (">", rest.trim())
    } else if let Some(rest) = raw.strip_prefix('<') {
        ("<", rest.trim())
    } else if let Some(rest) = raw.strip_prefix('=') {
        ("=", rest.trim())
    } else {
        ("=", raw)
    };
    let parsed: f64 = num_str.parse().ok().filter(|n: &f64| n.is_finite())?;
    Some(format!("{key}{op}{}", fmt_num_clean(parsed)))
}

fn parse_search_sort(raw: Option<&str>) -> Option<(BeatmapsetSearchSort, bool)> {
    let s = raw?.trim().to_ascii_lowercase();
    match s.as_str() {
        "" => None,
        "plays" | "playcount" | "popular" => Some((BeatmapsetSearchSort::Playcount, true)),
        "favourites" | "favorites" | "favs" => Some((BeatmapsetSearchSort::Favourites, true)),
        "rating" => Some((BeatmapsetSearchSort::Rating, true)),
        "ranked" | "date" | "newest" => Some((BeatmapsetSearchSort::ApprovedDate, true)),
        "updated" => Some((BeatmapsetSearchSort::LastUpdate, true)),
        "stars" | "difficulty" | "sr" => Some((BeatmapsetSearchSort::Stars, true)),
        "relevance" => Some((BeatmapsetSearchSort::Relevance, true)),
        _ => None,
    }
}

/// Search official osu! beatmapsets by song/artist/mapper text plus structured SR, BPM, AR/CS/OD, status, and sort filters.
pub async fn execute_osu_beatmap_search_tool(
    osu_ctx: &OsuToolContext<'_>,
    args: &serde_json::Value,
    verified_ids: &mut HashSet<u32>,
) -> String {
    let raw_query = args
        .get("query")
        .or_else(|| args.get("q"))
        .or_else(|| args.get("song"))
        .or_else(|| args.get("title"))
        .and_then(|v| v.as_str())
        .or_else(|| args.as_str())
        .map(str::trim)
        .unwrap_or("");

    let creator = args
        .get("creator")
        .or_else(|| args.get("mapper"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let min_stars = args.get("min_stars").and_then(|v| v.as_f64()).filter(|n| n.is_finite() && *n >= 0.0);
    let max_stars = args.get("max_stars").and_then(|v| v.as_f64()).filter(|n| n.is_finite() && *n >= 0.0);
    let min_bpm = args.get("min_bpm").and_then(|v| v.as_f64()).filter(|n| n.is_finite() && *n > 0.0);
    let max_bpm = args.get("max_bpm").and_then(|v| v.as_f64()).filter(|n| n.is_finite() && *n > 0.0);
    let ar_filter = args.get("ar").and_then(|v| format_attr_filter(v, "ar"));
    let cs_filter = args.get("cs").and_then(|v| format_attr_filter(v, "cs"));
    let od_filter = args.get("od").and_then(|v| format_attr_filter(v, "od"));

    let has_structured_filters = creator.is_some()
        || min_stars.is_some()
        || max_stars.is_some()
        || min_bpm.is_some()
        || max_bpm.is_some()
        || ar_filter.is_some()
        || cs_filter.is_some()
        || od_filter.is_some();

    let cleaned_title_query = match sanitize_beatmap_search_query(raw_query, has_structured_filters) {
        Ok(q) => q,
        Err(msg) => return msg,
    };

    let mut filter_tokens = Vec::new();
    if let Some(m) = creator {
        let safe_m = m.replace('"', "");
        if safe_m.contains(char::is_whitespace) {
            filter_tokens.push(format!("creator=\"{safe_m}\""));
        } else {
            filter_tokens.push(format!("creator={safe_m}"));
        }
    }
    if let Some(v) = min_stars {
        filter_tokens.push(format!("stars>={}", fmt_num_clean(v)));
    }
    if let Some(v) = max_stars {
        filter_tokens.push(format!("stars<={}", fmt_num_clean(v)));
    }
    if let Some(v) = min_bpm {
        filter_tokens.push(format!("bpm>={}", fmt_num_clean(v)));
    }
    if let Some(v) = max_bpm {
        filter_tokens.push(format!("bpm<={}", fmt_num_clean(v)));
    }
    if let Some(f) = ar_filter {
        filter_tokens.push(f);
    }
    if let Some(f) = cs_filter {
        filter_tokens.push(f);
    }
    if let Some(f) = od_filter {
        filter_tokens.push(f);
    }

    let mut parts = Vec::new();
    if !cleaned_title_query.is_empty() {
        parts.push(cleaned_title_query.clone());
    }
    parts.extend(filter_tokens);
    let final_query = parts.join(" ");
    if final_query.trim().is_empty() {
        return "[osu_beatmap_search error: provide a song/artist `query` or at least one filter (`min_stars`, `max_stars`, `min_bpm`, `creator`, `ar`, `cs`, `od`)]".to_owned();
    }

    let mode = args
        .get("mode")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|m| parse_game_mode(Some(m)));
    let status = parse_search_status(args.get("status").and_then(|v| v.as_str()));
    let sort = parse_search_sort(args.get("sort").and_then(|v| v.as_str())).or_else(|| {
        cleaned_title_query
            .trim()
            .is_empty()
            .then_some((BeatmapsetSearchSort::Playcount, true))
    });

    let res = osu_call_with_refresh!(osu_ctx, |osu| {
        let mut req = osu.beatmapset_search().query(&final_query).nsfw(false);
        if let Some(m) = mode {
            req = req.mode(m);
        }
        if let Some(s) = status {
            req = req.status(s);
        }
        if let Some((sort_by, desc)) = sort {
            req = req.sort(sort_by, desc);
        }
        req
    });

    let search_result = match res {
        Ok(r) => r,
        Err(err) => return format!("[osu_beatmap_search error: search failed: {err}]"),
    };

    let has_star_bounds = min_stars.is_some() || max_stars.is_some();
    let in_star_bounds = |stars: f32| -> bool {
        let s = f64::from(stars);
        let lo_ok = min_stars.is_none_or(|lo| s >= lo - 0.15);
        let hi_ok = max_stars.is_none_or(|hi| s <= hi + 0.15);
        lo_ok && hi_ok
    };

    let mut sets = Vec::with_capacity(search_result.mapsets.len().min(5));
    for ms in search_result.mapsets.into_iter().take(5) {
        let mut diffs = ms.maps.unwrap_or_default();
        if let Some(target_mode) = mode {
            let matching: Vec<_> = diffs.iter().filter(|m| m.mode == target_mode).cloned().collect();
            if !matching.is_empty() {
                diffs = matching;
            }
        }
        if has_star_bounds {
            diffs.sort_by(|a, b| {
                in_star_bounds(b.stars)
                    .cmp(&in_star_bounds(a.stars))
                    .then_with(|| b.stars.total_cmp(&a.stars))
            });
        } else {
            diffs.sort_by(|a, b| b.stars.total_cmp(&a.stars));
        }
        for m in &diffs {
            verified_ids.insert(m.map_id);
        }
        let drain_secs = diffs.first().map(|d| d.seconds_drain);
        let diff_entries: Vec<serde_json::Value> = diffs
            .into_iter()
            .take(8)
            .map(|m| {
                serde_json::json!({
                    "beatmap_id": m.map_id,
                    "version": m.version,
                    "mode": format!("{:?}", m.mode),
                    "stars": (m.stars * 100.0).round() / 100.0,
                    "ar": m.ar,
                    "cs": m.cs,
                    "od": m.od,
                    "hp": m.hp,
                    "max_combo": m.max_combo,
                    "drain_seconds": m.seconds_drain,
                })
            })
            .collect();

        let tags_trimmed: String = ms.tags.chars().take(120).collect();
        sets.push(serde_json::json!({
            "beatmapset_id": ms.mapset_id,
            "artist": ms.artist,
            "title": ms.title,
            "mapper": ms.creator_name.as_str(),
            "status": format!("{:?}", ms.status),
            "bpm": ms.bpm,
            "playcount": ms.playcount,
            "favourite_count": ms.favourite_count,
            "seconds_drain": drain_secs,
            "tags": tags_trimmed,
            "url": format!("https://osu.ppy.sh/beatmapsets/{}", ms.mapset_id),
            "difficulties": diff_entries,
        }));
    }

    let payload = serde_json::json!({
        "query": final_query,
        "total": search_result.total,
        "mapsets": sets,
    });

    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned())
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CustomPerfParams {
    pub accuracy: Option<f64>,
    pub combo: Option<u32>,
    pub misses: Option<u32>,
    pub n100: Option<u32>,
    pub n50: Option<u32>,
    pub clock_rate: Option<f64>,
    pub lazer: bool,
}

impl CustomPerfParams {
    fn has_custom_score(&self) -> bool {
        self.accuracy.is_some()
            || self.combo.is_some()
            || self.misses.is_some()
            || self.n100.is_some()
            || self.n50.is_some()
    }
}

/// Look up live osu! beatmap metadata and calculate modded star rating, attributes, FC table, and optional custom choke/rate PP via `rosu-pp`.
pub async fn execute_osu_beatmap_tool(
    osu_ctx: &OsuToolContext<'_>,
    http: &Client,
    args: &serde_json::Value,
    verified_ids: &mut HashSet<u32>,
) -> String {
    let raw_val = args
        .get("beatmap_id")
        .or_else(|| args.get("map_id"))
        .or_else(|| args.get("id"))
        .or_else(|| args.get("url"));

    let raw_str_opt = raw_val
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let bare_numeric_id = raw_val
        .and_then(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()))
        .or_else(|| {
            raw_str_opt.and_then(|s| {
                s.bytes()
                    .all(|b| b.is_ascii_digit())
                    .then(|| s.parse::<u32>().ok())
                    .flatten()
            })
        })
        .filter(|&id| id > 0);

    if let Some(bare_id) = bare_numeric_id {
        if !verified_ids.contains(&bare_id) {
            return format!(
                "[osu_beatmap error: unverified bare beatmap_id `{bare_id}` — never guess numeric beatmap IDs from memory. Call `osu_beatmap_search` (or `osu_player` / `osu_scores`, or pass an explicit `osu.ppy.sh` URL) to obtain a verified beatmap_id.]"
            );
        }
    }

    let requested_mode = args
        .get("mode")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|m| parse_game_mode(Some(m)));

    let direct_map_id = bare_numeric_id.or_else(|| raw_str_opt.and_then(parse_beatmap_id));

    let map_id = if let Some(id) = direct_map_id {
        id
    } else if let Some(set_id) = raw_str_opt.and_then(parse_beatmapset_id) {
        let mapset = match osu_call_with_refresh!(osu_ctx, |osu| osu.beatmapset(set_id)) {
            Ok(ms) => ms,
            Err(err) => {
                return format!("[osu_beatmap error: could not fetch beatmapset {set_id}: {err}]");
            }
        };
        let mut candidates = mapset.maps.unwrap_or_default();
        for m in &candidates {
            verified_ids.insert(m.map_id);
        }
        if let Some(mode) = requested_mode {
            let matching: Vec<_> = candidates.iter().filter(|m| m.mode == mode).cloned().collect();
            if !matching.is_empty() {
                candidates = matching;
            }
        }
        let Some(best) = candidates
            .into_iter()
            .max_by(|a, b| a.stars.total_cmp(&b.stars))
        else {
            return format!("[osu_beatmap error: beatmapset {set_id} contains no difficulties]");
        };
        best.map_id
    } else {
        return "[osu_beatmap error: provide a verified `beatmap_id` or an `osu.ppy.sh` beatmap/beatmapset URL]"
            .to_owned();
    };

    let map = match osu_call_with_refresh!(osu_ctx, |osu| osu.beatmap().map_id(map_id)) {
        Ok(m) => m,
        Err(err) => return format!("[osu_beatmap error: could not fetch beatmap {map_id}: {err}]"),
    };
    verified_ids.insert(map.map_id);

    let (mod_bits, mods_label) = parse_mod_bits(args.get("mods").and_then(|v| v.as_str()));
    let custom_params = CustomPerfParams {
        accuracy: args
            .get("accuracy")
            .and_then(|v| v.as_f64())
            .filter(|n| n.is_finite())
            .map(|a| a.clamp(0.0, 100.0)),
        combo: args
            .get("combo")
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok()),
        misses: args
            .get("misses")
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok()),
        n100: args
            .get("n100")
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok()),
        n50: args
            .get("n50")
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok()),
        clock_rate: args
            .get("clock_rate")
            .and_then(|v| v.as_f64())
            .filter(|n| n.is_finite())
            .map(|cr| cr.clamp(0.5, 2.0)),
        lazer: args.get("lazer").and_then(|v| v.as_bool()).unwrap_or(false),
    };

    let target_mode = requested_mode.unwrap_or(map.mode);
    let pp_mode = match target_mode {
        GameMode::Osu => rosu_pp::model::mode::GameMode::Osu,
        GameMode::Taiko => rosu_pp::model::mode::GameMode::Taiko,
        GameMode::Catch => rosu_pp::model::mode::GameMode::Catch,
        GameMode::Mania => rosu_pp::model::mode::GameMode::Mania,
    };

    let pp_calc = match http
        .get(format!("https://osu.ppy.sh/osu/{map_id}"))
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => match resp.bytes().await {
            Ok(bytes) if bytes.len() <= MAX_OSU_FILE_BYTES => {
                tokio::task::spawn_blocking(move || {
                    compute_rosu_pp(&bytes, mod_bits, pp_mode, custom_params)
                })
                .await
                .ok()
                .flatten()
            }
            _ => None,
        },
        _ => None,
    };

    let modded_drain_seconds = pp_calc
        .as_ref()
        .and_then(|v| v.get("clock_rate").and_then(|c| c.as_f64()))
        .filter(|&cr| cr > 0.0)
        .map(|cr| (f64::from(map.seconds_drain) / cr).round() as u32)
        .unwrap_or(map.seconds_drain);

    let title = map
        .mapset
        .as_ref()
        .map(|ms| format!("{} - {}", ms.artist, ms.title))
        .unwrap_or_else(|| format!("Beatmap #{map_id}"));

    let objects = map.count_objects();
    let slider_ratio_pct = if objects > 0 {
        ((f64::from(map.count_sliders) / f64::from(objects)) * 1000.0).round() / 10.0
    } else {
        0.0
    };

    let payload = serde_json::json!({
        "beatmap_id": map.map_id,
        "beatmapset_id": map.mapset_id,
        "map": format!("{title} [{}]", map.version),
        "mapper": map.mapset.as_ref().map(|ms| ms.creator_name.as_str()).unwrap_or("?"),
        "mode": format!("{target_mode:?}"),
        "is_convert": target_mode != map.mode,
        "status": format!("{:?}", map.status),
        "url": map.url,
        "base_stars": (map.stars * 100.0).round() / 100.0,
        "base_bpm": map.bpm,
        "base_drain_seconds": map.seconds_drain,
        "modded_drain_seconds": modded_drain_seconds,
        "base_ar": map.ar,
        "base_cs": map.cs,
        "base_od": map.od,
        "base_hp": map.hp,
        "max_combo": map.max_combo,
        "objects": objects,
        "circles": map.count_circles,
        "sliders": map.count_sliders,
        "spinners": map.count_spinners,
        "slider_ratio_pct": slider_ratio_pct,
        "playcount": map.playcount,
        "passcount": map.passcount,
        "requested_mods": mods_label,
        "modded_pp_estimate": pp_calc,
    });

    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned())
}

fn build_perf<'d>(
    diff: rosu_pp::any::DifficultyAttributes,
    mod_bits: u32,
    lazer: bool,
    clock_rate: Option<f64>,
) -> rosu_pp::Performance<'d> {
    let mut perf = rosu_pp::Performance::new(diff).mods(mod_bits).lazer(lazer);
    if let Some(cr) = clock_rate {
        perf = perf.clock_rate(cr);
    }
    perf
}

fn round_1_decimal(val: f64) -> f64 {
    (val * 10.0).round() / 10.0
}

fn osu_skill_breakdown(perf: &rosu_pp::any::PerformanceAttributes) -> serde_json::Value {
    match perf {
        rosu_pp::any::PerformanceAttributes::Osu(osu) => serde_json::json!({
            "aim_pp": round_1_decimal(osu.pp_aim),
            "speed_pp": round_1_decimal(osu.pp_speed),
            "acc_pp": round_1_decimal(osu.pp_acc),
            "flashlight_pp": round_1_decimal(osu.pp_flashlight),
        }),
        _ => serde_json::json!({
            "aim_pp": null,
            "speed_pp": null,
            "acc_pp": null,
            "flashlight_pp": null,
        }),
    }
}

pub fn compute_rosu_pp(
    osu_bytes: &[u8],
    mod_bits: u32,
    target_mode: rosu_pp::model::mode::GameMode,
    params: CustomPerfParams,
) -> Option<serde_json::Value> {
    let mut map = rosu_pp::Beatmap::from_bytes(osu_bytes).ok()?;
    if map.mode != target_mode {
        let pp_mods = rosu_pp::GameMods::from(mod_bits);
        map.convert_mut(target_mode, &pp_mods).ok()?;
    }
    if map.check_suspicion().is_err() {
        return None;
    }

    let mut attr_builder = map.attributes();
    attr_builder.mods(mod_bits);
    if let Some(cr) = params.clock_rate {
        attr_builder.clock_rate(cr);
    }
    let attrs = attr_builder.build();
    let clock_rate = attrs.clock_rate();
    let adj = attrs.apply_clock_rate();
    let modded_bpm = round_1_decimal(map.bpm() * clock_rate);

    let mut diff_builder = rosu_pp::Difficulty::new().mods(mod_bits).lazer(params.lazer);
    if let Some(cr) = params.clock_rate {
        diff_builder = diff_builder.clock_rate(cr);
    }
    let diff = diff_builder.calculate(&map);
    let stars = (diff.stars() * 100.0).round() / 100.0;
    let max_combo = diff.max_combo();

    let perf_100 = build_perf(diff.clone(), mod_bits, params.lazer, params.clock_rate)
        .accuracy(100.0)
        .calculate();
    let pp_100 = round_1_decimal(perf_100.pp());
    let pp_99 = round_1_decimal(
        build_perf(diff.clone(), mod_bits, params.lazer, params.clock_rate)
            .accuracy(99.0)
            .calculate()
            .pp(),
    );
    let pp_98 = round_1_decimal(
        build_perf(diff.clone(), mod_bits, params.lazer, params.clock_rate)
            .accuracy(98.0)
            .calculate()
            .pp(),
    );
    let pp_95 = round_1_decimal(
        build_perf(diff.clone(), mod_bits, params.lazer, params.clock_rate)
            .accuracy(95.0)
            .calculate()
            .pp(),
    );

    let custom_score_pp = if params.has_custom_score() {
        let mut custom = build_perf(diff, mod_bits, params.lazer, params.clock_rate);
        if let Some(acc) = params.accuracy {
            custom = custom.accuracy(acc);
        }
        if let Some(combo) = params.combo {
            custom = custom.combo(combo.min(max_combo));
        }
        if let Some(misses) = params.misses {
            custom = custom.misses(misses);
        }
        if let Some(n100) = params.n100 {
            custom = custom.n100(n100);
        }
        if let Some(n50) = params.n50 {
            custom = custom.n50(n50);
        }
        let custom_attrs = custom.calculate();
        Some(serde_json::json!({
            "pp": round_1_decimal(custom_attrs.pp()),
            "accuracy": params.accuracy,
            "combo": params.combo.map(|c| c.min(max_combo)),
            "misses": params.misses,
            "n100": params.n100,
            "n50": params.n50,
            "breakdown_osu": osu_skill_breakdown(&custom_attrs),
        }))
    } else {
        None
    };

    let base_mod_rate = if (mod_bits & (1 << 6)) != 0 {
        1.5
    } else if (mod_bits & (1 << 8)) != 0 {
        0.75
    } else {
        1.0
    };
    let is_unranked_rate_adjust = params
        .clock_rate
        .is_some_and(|cr| (cr - base_mod_rate).abs() > 0.005);

    Some(serde_json::json!({
        "stars": stars,
        "max_combo": max_combo,
        "clock_rate": (clock_rate * 100.0).round() / 100.0,
        "lazer": params.lazer,
        "is_unranked_rate_adjust": is_unranked_rate_adjust,
        "modded_attributes": {
            "ar": (adj.ar * 100.0).round() / 100.0,
            "cs": (f64::from(adj.cs) * 100.0).round() / 100.0,
            "od": (adj.od * 100.0).round() / 100.0,
            "hp": (f64::from(adj.hp) * 100.0).round() / 100.0,
            "bpm": modded_bpm,
        },
        "ss_100": pp_100,
        "fc_99": pp_99,
        "fc_98": pp_98,
        "fc_95": pp_95,
        "ss_breakdown_osu": osu_skill_breakdown(&perf_100),
        "custom_score_pp": custom_score_pp,
    }))
}

pub fn parse_osu_score_id(raw: &str) -> Option<(Option<GameMode>, u64)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(id) = trimmed.parse::<u64>() {
        return (id > 0).then_some((None, id));
    }

    let without_query = trimmed.split(['?', '#']).next().unwrap_or(trimmed);
    let pos = without_query.find("/scores/")?;
    let rest = &without_query[pos + "/scores/".len()..];
    let mut parts = rest.split('/').filter(|s| !s.is_empty());
    let first = parts.next()?;
    if first.bytes().all(|b| b.is_ascii_digit()) {
        let id = first.parse::<u64>().ok()?;
        return (id > 0).then_some((None, id));
    }
    let mode = match first.to_ascii_lowercase().as_str() {
        "osu" => Some(GameMode::Osu),
        "taiko" => Some(GameMode::Taiko),
        "fruits" | "catch" => Some(GameMode::Catch),
        "mania" => Some(GameMode::Mania),
        _ => None,
    };
    let second = parts.next()?;
    let id = second.parse::<u64>().ok()?;
    (id > 0).then_some((mode, id))
}

/// Fetch a single osu! score, a beatmap's top-10 leaderboard, or a specific player's scores on a beatmap.
pub async fn execute_osu_scores_tool(
    osu_ctx: &OsuToolContext<'_>,
    args: &serde_json::Value,
    verified_ids: &mut HashSet<u32>,
) -> String {
    let raw_score_val = args
        .get("score_id")
        .or_else(|| {
            args.get("url")
                .filter(|v| v.as_str().is_some_and(|s| s.contains("/scores/")))
        });
    let score_parsed = raw_score_val.and_then(|v| {
        v.as_u64()
            .filter(|&n| n > 0)
            .map(|id| (None, id))
            .or_else(|| v.as_str().and_then(parse_osu_score_id))
    });

    let requested_mode = args
        .get("mode")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|m| parse_game_mode(Some(m)));

    if let Some((url_mode, score_id)) = score_parsed {
        let effective_mode = requested_mode.or(url_mode);
        let res = osu_call_with_refresh!(osu_ctx, |osu| {
            let mut req = osu.score(score_id);
            if let Some(m) = effective_mode {
                req = req.mode(m);
            }
            req
        });
        return match res {
            Ok(score) => {
                verified_ids.insert(score.map_id);
                let mut entry = summarize_score(&score);
                if let Some(obj) = entry.as_object_mut() {
                    obj.insert(
                        "map_url".to_owned(),
                        serde_json::json!(format!("https://osu.ppy.sh/b/{}", score.map_id)),
                    );
                    obj.insert(
                        "score_url".to_owned(),
                        serde_json::json!(format!("https://osu.ppy.sh/scores/{}", score.id)),
                    );
                }
                serde_json::to_string(&entry).unwrap_or_else(|_| "{}".to_owned())
            }
            Err(err) => format!("[osu_scores error: could not fetch score {score_id}: {err}]"),
        };
    }

    let raw_map_val = args
        .get("beatmap_id")
        .or_else(|| args.get("map_id"))
        .or_else(|| args.get("url"));
    let raw_map_str = raw_map_val
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let bare_map_id = raw_map_val
        .and_then(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()))
        .or_else(|| {
            raw_map_str.and_then(|s| {
                s.bytes()
                    .all(|b| b.is_ascii_digit())
                    .then(|| s.parse::<u32>().ok())
                    .flatten()
            })
        })
        .filter(|&id| id > 0);

    if let Some(bare_id) = bare_map_id {
        if !verified_ids.contains(&bare_id) {
            return format!(
                "[osu_scores error: unverified bare beatmap_id `{bare_id}` — call `osu_beatmap_search` first or pass an `osu.ppy.sh` URL.]"
            );
        }
    }

    let Some(map_id) = bare_map_id.or_else(|| raw_map_str.and_then(parse_beatmap_id)) else {
        return "[osu_scores error: provide a `score_id` (or `/scores/` URL) or a verified `beatmap_id`]".to_owned();
    };
    verified_ids.insert(map_id);

    let has_user = args
        .get("username")
        .or_else(|| args.get("user"))
        .or_else(|| args.get("user_id"))
        .or_else(|| args.get("player"))
        .is_some_and(|v| {
            v.as_u64().is_some()
                || v.as_str().is_some_and(|s| !s.trim().is_empty())
        });

    if has_user {
        if let Some(err) = blocked_osu_player_error(args, osu_ctx.blocked_discord_handles) {
            return err;
        }
        let Some(user_id) = parse_osu_user_id(args) else {
            return "[osu_scores error: invalid `username` argument]".to_owned();
        };
        let res = osu_call_with_refresh!(osu_ctx, |osu| {
            let mut req = osu.beatmap_user_scores(map_id, user_id.clone());
            if let Some(m) = requested_mode {
                req = req.mode(m);
            }
            req
        });
        return match res {
            Ok(scores) => {
                let items: Vec<serde_json::Value> =
                    scores.iter().take(8).map(summarize_score).collect();
                let payload = serde_json::json!({
                    "beatmap_id": map_id,
                    "map_url": format!("https://osu.ppy.sh/b/{map_id}"),
                    "scores": items,
                });
                serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned())
            }
            Err(err) => format!(
                "[osu_scores error: could not fetch user scores on beatmap {map_id}: {err}]"
            ),
        };
    }

    let (mod_bits, mods_label) = parse_mod_bits(args.get("mods").and_then(|v| v.as_str()));
    let res = osu_call_with_refresh!(osu_ctx, |osu| {
        let mut req = osu.beatmap_scores(map_id).limit(10);
        if let Some(m) = requested_mode {
            req = req.mode(m);
        }
        if mod_bits != 0 {
            req = req.mods(GameModsIntermode::from_bits(mod_bits));
        }
        req
    });

    match res {
        Ok(lb) => {
            let entries: Vec<serde_json::Value> = lb
                .scores
                .iter()
                .take(10)
                .enumerate()
                .map(|(idx, s)| {
                    let mut val = summarize_score(s);
                    if let Some(obj) = val.as_object_mut() {
                        obj.insert("leaderboard_rank".to_owned(), serde_json::json!(idx + 1));
                    }
                    val
                })
                .collect();
            let payload = serde_json::json!({
                "beatmap_id": map_id,
                "map_url": format!("https://osu.ppy.sh/b/{map_id}"),
                "mods_filter": mods_label,
                "score_count": lb.score_count,
                "leaderboard": entries,
            });
            serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned())
        }
        Err(err) => {
            format!("[osu_scores error: could not fetch beatmap leaderboard {map_id}: {err}]")
        }
    }
}

pub fn normalize_country_code(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() == 2 && trimmed.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Some(trimmed.to_ascii_uppercase());
    }
    let code = match trimmed.to_ascii_lowercase().as_str() {
        "belgium" | "belgie" | "belgië" => "BE",
        "usa" | "us" | "united states" | "united states of america" | "america" => "US",
        "uk" | "united kingdom" | "great britain" | "britain" | "england" => "GB",
        "south korea" | "korea" | "republic of korea" => "KR",
        "poland" | "polska" => "PL",
        "germany" | "deutschland" => "DE",
        "france" => "FR",
        "netherlands" | "the netherlands" | "holland" => "NL",
        "japan" => "JP",
        "canada" => "CA",
        "australia" => "AU",
        "russia" | "russian federation" => "RU",
        "brazil" | "brasil" => "BR",
        "indonesia" => "ID",
        "philippines" => "PH",
        "taiwan" => "TW",
        "hong kong" => "HK",
        "china" => "CN",
        "chile" => "CL",
        "sweden" => "SE",
        "finland" => "FI",
        "norway" => "NO",
        "denmark" => "DK",
        "spain" => "ES",
        "italy" => "IT",
        "singapore" => "SG",
        "malaysia" => "MY",
        "thailand" => "TH",
        "vietnam" => "VN",
        "mexico" => "MX",
        "argentina" => "AR",
        "ukraine" => "UA",
        "new zealand" => "NZ",
        _ => return None,
    };
    Some(code.to_owned())
}

/// Fetch live osu! global/country PP player rankings or country team rankings.
pub async fn execute_osu_rankings_tool(
    osu_ctx: &OsuToolContext<'_>,
    args: &serde_json::Value,
) -> String {
    let mode = parse_game_mode(args.get("mode").and_then(|v| v.as_str()));
    let kind = args
        .get("kind")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();

    if matches!(kind.as_str(), "countries" | "country") {
        return match osu_call_with_refresh!(osu_ctx, |osu| osu.country_rankings(mode)) {
            Ok(res) => {
                let top: Vec<serde_json::Value> = res
                    .ranking
                    .iter()
                    .take(15)
                    .enumerate()
                    .map(|(i, c)| {
                        serde_json::json!({
                            "rank": i + 1,
                            "country": c.country,
                            "code": c.country_code.as_str(),
                            "pp": round_1_decimal(f64::from(c.pp)),
                            "active_users": c.active_users,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "kind": "countries",
                    "mode": format!("{mode:?}"),
                    "total": res.total,
                    "rankings": top,
                })
                .to_string()
            }
            Err(err) => format!("[osu_rankings error: could not fetch country rankings: {err}]"),
        };
    }

    let country_raw = args.get("country").and_then(|v| v.as_str());
    let country_code = country_raw.and_then(normalize_country_code);
    if country_raw.is_some_and(|s| !s.trim().is_empty()) && country_code.is_none() {
        return format!(
            "[osu_rankings error: unrecognized country `{}` — pass a 2-letter ISO code like `BE`, `US`, `GB`, `KR`, `PL`, `JP`]",
            country_raw.unwrap_or("")
        );
    }
    let variant = args
        .get("variant")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_ascii_lowercase());

    let res = osu_call_with_refresh!(osu_ctx, |osu| {
        let mut req = osu.performance_rankings(mode);
        if let Some(ref cc) = country_code {
            req = req.country(cc.as_str());
        }
        if mode == GameMode::Mania {
            match variant.as_deref() {
                Some("4k") => req = req.variant_4k(),
                Some("7k") => req = req.variant_7k(),
                _ => {}
            }
        }
        req
    });

    match res {
        Ok(r) => {
            let players: Vec<serde_json::Value> = r
                .ranking
                .iter()
                .take(15)
                .enumerate()
                .map(|(i, u)| {
                    let s = u.statistics.as_ref();
                    serde_json::json!({
                        "rank": i + 1,
                        "username": u.username.as_str(),
                        "user_id": u.user_id,
                        "country": u.country_code.as_str(),
                        "pp": s.map(|st| round_1_decimal(f64::from(st.pp))),
                        "accuracy": s.map(|st| format!("{:.2}%", st.accuracy)),
                        "playcount": s.map(|st| st.playcount),
                    })
                })
                .collect();
            serde_json::json!({
                "kind": "performance",
                "mode": format!("{mode:?}"),
                "country": country_code,
                "variant": if mode == GameMode::Mania { variant } else { None },
                "total": r.total,
                "rankings": players,
            })
            .to_string()
        }
        Err(err) => format!("[osu_rankings error: could not fetch player rankings: {err}]"),
    }
}

pub fn resolve_osu_wiki_alias(raw: &str) -> (String, Option<String>) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return ("Sitemap".to_owned(), None);
    }

    let (without_anchor, anchor) = match trimmed.split_once('#') {
        Some((before, frag)) => {
            let clean_frag = frag.trim();
            (
                before.trim(),
                (!clean_frag.is_empty()).then(|| clean_frag.to_owned()),
            )
        }
        None => (trimmed, None),
    };

    if let Some(wiki_pos) = without_anchor.find("/wiki/") {
        let after_wiki = &without_anchor[wiki_pos + "/wiki/".len()..];
        let mut segs = after_wiki.splitn(2, '/');
        let first = segs.next().unwrap_or("");
        let rest = segs.next().unwrap_or("");
        let page = if first.len() == 2 && first.bytes().all(|b| b.is_ascii_alphabetic()) && !rest.is_empty() {
            rest
        } else {
            after_wiki
        };
        let clean_page = page.split('?').next().unwrap_or(page).trim_matches('/');
        if !clean_page.is_empty() {
            return (clean_page.to_owned(), anchor);
        }
    }

    let key = without_anchor.to_ascii_lowercase().replace([' ', '-'], "_");
    let mapped = match key.as_str() {
        "pp" | "performance_points" | "performance_point" | "csr" | "combo_scaling_removal" => {
            ("Performance_points", None)
        }
        "mods" | "mod" | "game_modifier" | "game_modifiers" | "difficulty_adjust" | "rate_adjust" | "classic" => {
            ("Gameplay/Game_modifier", None)
        }
        "scorev2" | "score_v2" | "scorev1" | "score" | "scoring" => {
            ("Gameplay/Score", Some("scorev2"))
        }
        "grade" | "grades" | "ss" | "rank_grade" => ("Gameplay/Grade", None),
        "ur" | "unstable_rate" | "hit_error" => ("Gameplay/Unstable_rate", None),
        "ar" | "approach_rate" => ("Beatmap/Approach_rate", None),
        "od" | "overall_difficulty" => ("Beatmap/Overall_difficulty", None),
        "cs" | "circle_size" => ("Beatmap/Circle_size", None),
        "hp" | "hp_drain" | "hp_drain_rate" | "drain_rate" => ("Beatmap/HP_drain_rate", None),
        "sr" | "star_rating" | "stars" => ("Beatmap/Star_rating", None),
        "beatmap_search" | "search" | "search_operators" => ("Beatmap_search", None),
        "tournaments" | "tournament" | "owc" => ("Tournaments", None),
        "ranking_criteria" | "rc" | "qualified" | "ranked" => ("Ranking_criteria", None),
        "bn" | "bns" | "beatmap_nominators" | "beatmap_nominator" => {
            ("People/Beatmap_Nominators", None)
        }
        "kudosu" => ("Modding/Kudosu", None),
        "skin.ini" | "skinning" | "instafade" => ("Skinning/skin.ini", None),
        "lazer" | "osu!lazer" | "osu_lazer" => ("Client/Release_stream/Lazer", None),
        "osu!" | "osu" | "osu!standard" | "standard" => ("Game_mode/osu!", None),
        "taiko" | "osu!taiko" => ("Game_mode/osu!taiko", None),
        "catch" | "ctb" | "fruits" | "osu!catch" => ("Game_mode/osu!catch", None),
        "mania" | "osu!mania" => ("Game_mode/osu!mania", None),
        _ => {
            return (
                without_anchor.trim_matches('/').replace(' ', "_"),
                anchor,
            );
        }
    };
    (
        mapped.0.to_owned(),
        anchor.or_else(|| mapped.1.map(str::to_owned)),
    )
}

fn slice_wiki_markdown(markdown: &str, anchor: Option<&str>, max_chars: usize) -> String {
    let start_idx = if let Some(anch) = anchor {
        let needle: String = anch
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if !needle.is_empty() {
            let mut found_byte = None;
            let mut offset = 0usize;
            for line in markdown.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with('#') {
                    let heading_norm: String = trimmed
                        .trim_start_matches('#')
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric())
                        .collect::<String>()
                        .to_ascii_lowercase();
                    if heading_norm.contains(&needle) {
                        found_byte = Some(offset);
                        break;
                    }
                }
                offset += line.len() + 1;
            }
            found_byte.unwrap_or(0)
        } else {
            0
        }
    } else {
        0
    };

    let slice = &markdown[start_idx..];
    let mut out = String::new();
    for ch in slice.chars().take(max_chars) {
        out.push(ch);
    }
    if slice.chars().count() > max_chars {
        out.push_str("\n...[truncated]");
    }
    out
}

/// Fetch an official osu! wiki article (`osu.ppy.sh/wiki/en/...`) via `rosu-v2` with canonical redirect resolution.
pub async fn execute_osu_wiki_tool(
    osu_ctx: &OsuToolContext<'_>,
    http: &Client,
    args: &serde_json::Value,
) -> String {
    let raw = args
        .get("topic")
        .or_else(|| args.get("page"))
        .or_else(|| args.get("url"))
        .or_else(|| args.get("query"))
        .and_then(|v| v.as_str())
        .or_else(|| args.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let Some(raw) = raw else {
        return "[osu_wiki error: missing `topic` or `url` argument]".to_owned();
    };

    let (mut path, mut anchor) = resolve_osu_wiki_alias(raw);
    if path.contains("..") || path.contains('\\') || path.is_empty() {
        return "[osu_wiki error: invalid wiki page path]".to_owned();
    }

    let mut wiki_res = osu_call_with_refresh!(osu_ctx, |osu| osu.wiki("en").page(&path));
    if wiki_res.is_err() {
        let lookup_url = format!("https://osu.ppy.sh/wiki/en/{}", path.replace(' ', "_"));
        if let Ok(resp) = http
            .get(&lookup_url)
            .timeout(Duration::from_secs(6))
            .send()
            .await
        {
            let final_url = resp.url();
            if final_url.host_str() == Some("osu.ppy.sh") && final_url.path().starts_with("/wiki/")
            {
                let (redir_path, redir_anchor) = resolve_osu_wiki_alias(final_url.as_str());
                if !redir_path.is_empty()
                    && redir_path != "Sitemap"
                    && !redir_path.contains("..")
                    && redir_path != path
                {
                    path = redir_path;
                    if anchor.is_none() {
                        anchor = redir_anchor;
                    }
                    wiki_res = osu_call_with_refresh!(osu_ctx, |osu| osu.wiki("en").page(&path));
                }
            }
        }
    }

    match wiki_res {
        Ok(page) => {
            let excerpt = slice_wiki_markdown(&page.markdown, anchor.as_deref(), 3_200);
            let url = match anchor.as_deref() {
                Some(a) => format!("https://osu.ppy.sh/wiki/{}/{}#{a}", page.locale, page.path),
                None => format!("https://osu.ppy.sh/wiki/{}/{}", page.locale, page.path),
            };
            serde_json::json!({
                "title": page.title,
                "subtitle": page.subtitle,
                "path": page.path,
                "url": url,
                "tags": page.tags,
                "markdown": excerpt,
            })
            .to_string()
        }
        Err(err) => format!("[osu_wiki error: could not fetch wiki page `{path}`: {err}]"),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OsuUrlKind {
    Wiki(String),
    Score(String),
    Beatmap(String),
    User(String),
}

pub fn classify_osu_url(raw_url: &str) -> Option<OsuUrlKind> {
    let lower = raw_url.trim().to_ascii_lowercase();
    if !lower.contains("://osu.ppy.sh/") && !lower.contains("://old.ppy.sh/") {
        return None;
    }
    if lower.contains("/wiki/") {
        return Some(OsuUrlKind::Wiki(raw_url.to_owned()));
    }
    if lower.contains("/scores/") {
        return Some(OsuUrlKind::Score(raw_url.to_owned()));
    }
    if parse_beatmap_id(raw_url).is_some() || parse_beatmapset_id(raw_url).is_some() {
        return Some(OsuUrlKind::Beatmap(raw_url.to_owned()));
    }
    if lower.contains("/users/") || lower.contains("/u/") {
        return Some(OsuUrlKind::User(raw_url.to_owned()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mod_bits() {
        assert_eq!(parse_mod_bits(None), (0, "NM".to_owned()));
        assert_eq!(parse_mod_bits(Some("nm")), (0, "NM".to_owned()));
        assert_eq!(parse_mod_bits(Some("+HDDT")), ((1 << 3) | (1 << 6), "HDDT".to_owned()));
        assert_eq!(
            parse_mod_bits(Some("HD, HR, FL")),
            ((1 << 3) | (1 << 4) | (1 << 10), "HDHRFL".to_owned())
        );
        assert_eq!(parse_mod_bits(Some("4KHD")), ((1 << 3), "HD".to_owned()));
        assert_eq!(parse_mod_bits(Some("7K")), (0, "NM".to_owned()));
        assert_eq!(
            parse_mod_bits(Some("HD and DT")),
            ((1 << 3) | (1 << 6), "HDDT".to_owned())
        );
        assert_eq!(parse_mod_bits(Some("HDCL")), ((1 << 3), "HD".to_owned()));
        assert_eq!(
            parse_mod_bits(Some("HDDTCL")),
            ((1 << 3) | (1 << 6), "HDDT".to_owned())
        );
        assert_eq!(parse_mod_bits(Some("DC")), ((1 << 8), "DC".to_owned()));
    }

    #[test]
    fn test_parse_beatmap_id_and_beatmapset_id() {
        assert_eq!(parse_beatmap_id("2101986"), Some(2_101_986));
        assert_eq!(parse_beatmap_id("https://osu.ppy.sh/b/75"), Some(75));
        assert_eq!(
            parse_beatmap_id("https://osu.ppy.sh/beatmapsets/1011011#osu/2116202"),
            Some(2_116_202)
        );
        assert_eq!(
            parse_beatmap_id("https://osu.ppy.sh/beatmapsets/1011011?mode=osu#osu/2116202"),
            Some(2_116_202)
        );
        assert_eq!(
            parse_beatmap_id("https://osu.ppy.sh/beatmaps/4000123?mode=osu"),
            Some(4_000_123)
        );
        assert_eq!(parse_beatmap_id("not-an-id"), None);

        assert_eq!(
            parse_beatmapset_id("https://osu.ppy.sh/beatmapsets/1011011#osu/2116202"),
            Some(1_011_011)
        );
        assert_eq!(parse_beatmapset_id("https://osu.ppy.sh/s/41823"), Some(41_823));

        let found = extract_beatmap_ids_from_text(
            "Check https://osu.ppy.sh/beatmapsets/1011011?mode=osu#osu/2116202 and https://osu.ppy.sh/b/75 plus osu.ppy.sh/beatmaps/2101986!",
        );
        assert_eq!(found, vec![2_116_202, 75, 2_101_986]);
    }

    #[test]
    fn test_parse_osu_user_id() {
        let name_val = serde_json::json!({ "username": "mrekk" });
        assert!(matches!(parse_osu_user_id(&name_val), Some(UserId::Name(_))));

        let url_val = serde_json::json!({ "username": "https://osu.ppy.sh/users/7562902/osu" });
        assert!(matches!(parse_osu_user_id(&url_val), Some(UserId::Id(7_562_902))));

        let short_url = serde_json::json!({ "username": "https://osu.ppy.sh/u/7562902" });
        assert!(matches!(parse_osu_user_id(&short_url), Some(UserId::Id(7_562_902))));

        let digit_name = serde_json::json!({ "username": "https://osu.ppy.sh/users/404_AimNotFound" });
        assert_eq!(
            parse_osu_user_id(&digit_name),
            Some(UserId::Name("404_AimNotFound".into()))
        );

        let pct_name = serde_json::json!({ "username": "https://osu.ppy.sh/users/Cookiezi%202/osu" });
        assert_eq!(
            parse_osu_user_id(&pct_name),
            Some(UserId::Name("Cookiezi 2".into()))
        );

        let num_val = serde_json::json!({ "username": 7562902 });
        assert!(matches!(parse_osu_user_id(&num_val), Some(UserId::Id(7_562_902))));

        assert!(looks_like_discord_user_ref(&serde_json::json!({ "username": "<@1313644055317446710>" })).is_some());
        assert!(looks_like_discord_user_ref(&serde_json::json!({ "username": "<@!339076467105333248>" })).is_some());
        assert!(looks_like_discord_user_ref(&serde_json::json!({ "username": "1313644055317446710" })).is_some());
        assert!(looks_like_discord_user_ref(&serde_json::json!({ "username": 1_313_644_055_317_446_710_u64 })).is_some());
        assert!(looks_like_discord_user_ref(&serde_json::json!({ "username": "7562902" })).is_none());
        assert!(looks_like_discord_user_ref(&serde_json::json!({ "username": "mrekk" })).is_none());
    }

    #[test]
    fn test_blocked_osu_player_error() {
        let blocked = vec!["milan.be".to_owned(), "lovecats102".to_owned()];

        assert!(blocked_osu_player_error(&serde_json::json!({ "username": "milan.be" }), &blocked).is_some());
        assert!(blocked_osu_player_error(&serde_json::json!({ "username": "MILAN.BE" }), &blocked).is_some());
        assert!(blocked_osu_player_error(&serde_json::json!({ "username": "@milan.be" }), &blocked).is_some());
        assert!(blocked_osu_player_error(&serde_json::json!({ "username": "@otheruser" }), &blocked).is_some());
        assert!(blocked_osu_player_error(&serde_json::json!({ "username": "@lovecats102 (Discord user)" }), &blocked).is_some());
        assert!(blocked_osu_player_error(&serde_json::json!({ "username": "@lovecats102 (Discord bot)" }), &blocked).is_some());
        assert!(blocked_osu_player_error(&serde_json::json!({ "username": "<@1313644055317446710>" }), &blocked).is_some());
        assert!(blocked_osu_player_error(&serde_json::json!({ "username": "1313644055317446710" }), &blocked).is_some());

        assert!(blocked_osu_player_error(&serde_json::json!({ "username": "mrekk" }), &blocked).is_none());
        assert!(blocked_osu_player_error(&serde_json::json!({ "username": "7562902" }), &blocked).is_none());
        assert!(blocked_osu_player_error(&serde_json::json!({ "username": "https://osu.ppy.sh/users/7562902" }), &blocked).is_none());
        assert!(blocked_osu_player_error(&serde_json::json!({ "username": "https://osu.ppy.sh/u/7562902" }), &blocked).is_none());
        assert!(blocked_osu_player_error(&serde_json::json!({ "username": "milan.be" }), &[]).is_none());
    }

    #[test]
    fn test_digit_tier_and_bws_rank() {
        assert_eq!(compute_digit_tier(None), "unranked");
        assert_eq!(compute_digit_tier(Some(1)), "1_digit");
        assert_eq!(compute_digit_tier(Some(99)), "2_digit");
        assert_eq!(compute_digit_tier(Some(750)), "3_digit");
        assert_eq!(compute_digit_tier(Some(5_432)), "4_digit");
        assert_eq!(compute_digit_tier(Some(42_000)), "5_digit");
        assert_eq!(compute_digit_tier(Some(250_000)), "6_digit");
        assert_eq!(compute_digit_tier(Some(1_250_000)), "7_digit");

        assert_eq!(compute_bws_rank(50_000, 0), 50_000);
        let with_badges = compute_bws_rank(50_000, 3);
        assert!(with_badges < 50_000);
        assert_eq!(with_badges, 27_489);
    }

    #[test]
    fn test_sanitize_beatmap_search_query_blocks_slang_and_keeps_song_with_filters() {
        assert!(sanitize_beatmap_search_query("farm map 6 digit", false).is_err());
        assert!(sanitize_beatmap_search_query("tapping farm", false).is_err());
        assert!(sanitize_beatmap_search_query("Sidetracked Day", false).is_ok());
        assert_eq!(
            sanitize_beatmap_search_query("farm map stars>=5", false).as_deref(),
            Ok("stars>=5")
        );
        assert_eq!(
            sanitize_beatmap_search_query("Sidetracked Day stream map", true).as_deref(),
            Ok("Sidetracked Day")
        );
    }

    #[test]
    fn test_compute_rosu_pp_fc_choke_and_rate_adjust() {
        let synthetic_osu = b"osu file format v14\n\n[General]\nMode: 0\n\n[Metadata]\nTitle:Test\nArtist:Tester\nVersion:Insane\n\n[Difficulty]\nHPDrainRate:6\nCircleSize:4\nOverallDifficulty:8\nApproachRate:9\nSliderMultiplier:1.4\nSliderTickRate:1\n\n[TimingPoints]\n0,333.333333333333,4,1,0,100,1,0\n\n[HitObjects]\n100,100,0,1,0,0:0:0:0:\n260,220,166,1,0,0:0:0:0:\n420,100,333,1,0,0:0:0:0:\n100,260,500,1,0,0:0:0:0:\n260,120,666,1,0,0:0:0:0:\n420,280,833,1,0,0:0:0:0:\n160,160,1000,1,0,0:0:0:0:\n360,240,1166,1,0,0:0:0:0:\n";

        let nm = compute_rosu_pp(
            synthetic_osu,
            0,
            rosu_pp::model::mode::GameMode::Osu,
            CustomPerfParams::default(),
        )
        .expect("nm pp");
        let hddt = compute_rosu_pp(
            synthetic_osu,
            (1 << 3) | (1 << 6),
            rosu_pp::model::mode::GameMode::Osu,
            CustomPerfParams::default(),
        )
        .expect("hddt pp");
        assert!(hddt["stars"].as_f64().unwrap() > nm["stars"].as_f64().unwrap());
        assert!(hddt["ss_100"].as_f64().unwrap() > nm["ss_100"].as_f64().unwrap());
        assert!(!hddt["is_unranked_rate_adjust"].as_bool().unwrap());

        let choke = compute_rosu_pp(
            synthetic_osu,
            (1 << 3) | (1 << 6),
            rosu_pp::model::mode::GameMode::Osu,
            CustomPerfParams {
                accuracy: Some(96.0),
                combo: Some(4),
                misses: Some(2),
                clock_rate: Some(1.25),
                lazer: true,
                ..Default::default()
            },
        )
        .expect("choke pp");
        assert!(choke["is_unranked_rate_adjust"].as_bool().unwrap());
        let custom = &choke["custom_score_pp"];
        assert!(custom.is_object());
        assert!(custom["pp"].as_f64().unwrap() > 0.0);
        assert!(custom["pp"].as_f64().unwrap() < hddt["ss_100"].as_f64().unwrap());
    }

    #[test]
    fn test_score_id_country_wiki_and_url_classifier() {
        assert_eq!(parse_osu_score_id("481920312"), Some((None, 481_920_312)));
        assert_eq!(
            parse_osu_score_id("https://osu.ppy.sh/scores/osu/481920312?ref=1"),
            Some((Some(GameMode::Osu), 481_920_312))
        );
        assert_eq!(
            parse_osu_score_id("https://osu.ppy.sh/scores/mania/998877"),
            Some((Some(GameMode::Mania), 998_877))
        );

        assert_eq!(normalize_country_code("belgium").as_deref(), Some("BE"));
        assert_eq!(normalize_country_code("nl").as_deref(), Some("NL"));
        assert_eq!(normalize_country_code("South Korea").as_deref(), Some("KR"));
        assert_eq!(normalize_country_code("nowhere"), None);

        assert_eq!(
            resolve_osu_wiki_alias("scorev2"),
            ("Gameplay/Score".to_owned(), Some("scorev2".to_owned()))
        );
        assert_eq!(
            resolve_osu_wiki_alias("https://osu.ppy.sh/wiki/en/Performance_points#combo-scaling-removal"),
            ("Performance_points".to_owned(), Some("combo-scaling-removal".to_owned()))
        );
        assert_eq!(
            resolve_osu_wiki_alias("BN"),
            ("People/Beatmap_Nominators".to_owned(), None)
        );

        assert!(matches!(
            classify_osu_url("https://osu.ppy.sh/wiki/en/Performance_points"),
            Some(OsuUrlKind::Wiki(_))
        ));
        assert!(matches!(
            classify_osu_url("https://osu.ppy.sh/scores/osu/123456"),
            Some(OsuUrlKind::Score(_))
        ));
        assert!(matches!(
            classify_osu_url("https://osu.ppy.sh/beatmapsets/1011011#osu/2116202"),
            Some(OsuUrlKind::Beatmap(_))
        ));
        assert!(matches!(
            classify_osu_url("https://osu.ppy.sh/users/7562902"),
            Some(OsuUrlKind::User(_))
        ));
        assert_eq!(classify_osu_url("https://example.com/test"), None);
    }
}
