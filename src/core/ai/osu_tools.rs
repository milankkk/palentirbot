use std::{collections::HashSet, time::Duration};

use reqwest::Client;
use rosu_v2::{
    model::{beatmap::RankStatus, GameMode},
    prelude::{Osu, Score, UserId},
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
        "map": format!("{map_title} [{diff}]"),
        "map_nm_stars": stars,
        "mods": mods_label,
        "grade": format!("{:?}", score.grade),
        "acc": format!("{:.2}%", score.accuracy),
        "pp": score.pp.map(|p| (p * 100.0).round() / 100.0),
        "combo": score.max_combo,
        "misses": score.statistics.miss,
        "passed": score.passed,
        "set_on_lazer": score.set_on_lazer,
    })
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

/// Look up live osu! profile stats, top 5 plays, and 3 most recent plays.
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

    let user = match osu_call_with_refresh!(osu_ctx, |osu| {
        let uid = user_id.clone();
        osu.user(uid).mode(mode)
    }) {
        Ok(u) => u,
        Err(err) => return format!("[osu_player error: could not fetch player: {err}]"),
    };

    let (top_res, recent_res) = tokio::join!(
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
                .limit(3)
                .into_future_wrapper())
        },
    );

    let top_scores = top_res.unwrap_or_default();
    let recent_scores = recent_res.unwrap_or_default();
    for s in top_scores.iter().chain(recent_scores.iter()) {
        verified_ids.insert(s.map_id);
    }

    let top_plays: Vec<serde_json::Value> = top_scores.iter().map(summarize_score).collect();
    let recent_plays: Vec<serde_json::Value> = recent_scores.iter().map(summarize_score).collect();

    let stats = user.statistics.as_ref();
    let payload = serde_json::json!({
        "username": user.username.as_str(),
        "user_id": user.user_id,
        "country": user.country_code.as_str(),
        "mode": format!("{mode:?}"),
        "global_rank": stats.and_then(|s| s.global_rank),
        "country_rank": stats.and_then(|s| s.country_rank),
        "pp": stats.map(|s| (s.pp * 100.0).round() / 100.0),
        "accuracy": stats.map(|s| format!("{:.2}%", s.accuracy)),
        "playcount": stats.map(|s| s.playcount),
        "playtime_hours": stats.map(|s| s.playtime / 3600),
        "level": stats.map(|s| s.level.current),
        "is_online": user.is_online,
        "top_5_plays": top_plays,
        "recent_plays": recent_plays,
    });

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

/// Search official osu! beatmapsets by song title, artist, difficulty, or mapper.
pub async fn execute_osu_beatmap_search_tool(
    osu_ctx: &OsuToolContext<'_>,
    args: &serde_json::Value,
    verified_ids: &mut HashSet<u32>,
) -> String {
    let query = args
        .get("query")
        .or_else(|| args.get("q"))
        .or_else(|| args.get("song"))
        .or_else(|| args.get("title"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| args.as_str().map(str::trim).filter(|s| !s.is_empty()));

    let Some(query) = query else {
        return "[osu_beatmap_search error: missing `query` argument]".to_owned();
    };

    let mode = args
        .get("mode")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|m| parse_game_mode(Some(m)));
    let status = parse_search_status(args.get("status").and_then(|v| v.as_str()));

    let res = osu_call_with_refresh!(osu_ctx, |osu| {
        let mut req = osu.beatmapset_search().query(query).nsfw(false);
        if let Some(m) = mode {
            req = req.mode(m);
        }
        if let Some(s) = status {
            req = req.status(s);
        }
        req
    });

    let search_result = match res {
        Ok(r) => r,
        Err(err) => return format!("[osu_beatmap_search error: search failed: {err}]"),
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
        diffs.sort_by(|a, b| b.stars.total_cmp(&a.stars));
        for m in &diffs {
            verified_ids.insert(m.map_id);
        }
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
                })
            })
            .collect();

        sets.push(serde_json::json!({
            "beatmapset_id": ms.mapset_id,
            "artist": ms.artist,
            "title": ms.title,
            "mapper": ms.creator_name.as_str(),
            "status": format!("{:?}", ms.status),
            "bpm": ms.bpm,
            "url": format!("https://osu.ppy.sh/beatmapsets/{}", ms.mapset_id),
            "difficulties": diff_entries,
        }));
    }

    let payload = serde_json::json!({
        "query": query,
        "total": search_result.total,
        "mapsets": sets,
    });

    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned())
}

/// Look up live osu! beatmap metadata and calculate modded star rating, attributes, and 95/98/99/100% PP via `rosu-pp`.
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
                "[osu_beatmap error: unverified bare beatmap_id `{bare_id}` — never guess numeric beatmap IDs from memory. Call `osu_beatmap_search` with the song, artist, mapper, or difficulty name first (or pass an explicit `osu.ppy.sh` URL) to obtain a verified beatmap_id.]"
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
                tokio::task::spawn_blocking(move || compute_rosu_pp(&bytes, mod_bits, pp_mode))
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
        "objects": map.count_objects(),
        "playcount": map.playcount,
        "passcount": map.passcount,
        "requested_mods": mods_label,
        "modded_pp_estimate": pp_calc,
    });

    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned())
}

fn compute_rosu_pp(
    osu_bytes: &[u8],
    mod_bits: u32,
    target_mode: rosu_pp::model::mode::GameMode,
) -> Option<serde_json::Value> {
    let mut map = rosu_pp::Beatmap::from_bytes(osu_bytes).ok()?;
    if map.mode != target_mode {
        let pp_mods = rosu_pp::GameMods::from(mod_bits);
        map.convert_mut(target_mode, &pp_mods).ok()?;
    }
    if map.check_suspicion().is_err() {
        return None;
    }

    let attrs = map.attributes().mods(mod_bits).build();
    let clock_rate = attrs.clock_rate();
    let adj = attrs.apply_clock_rate();
    let modded_bpm = (map.bpm() * clock_rate * 10.0).round() / 10.0;

    let diff = rosu_pp::Difficulty::new().mods(mod_bits).calculate(&map);
    let stars = (diff.stars() * 100.0).round() / 100.0;
    let max_combo = diff.max_combo();

    let perf_100 = rosu_pp::Performance::new(diff.clone())
        .mods(mod_bits)
        .accuracy(100.0)
        .calculate();
    let pp_100 = (perf_100.pp() * 10.0).round() / 10.0;
    let pp_99 = (rosu_pp::Performance::new(diff.clone())
        .mods(mod_bits)
        .accuracy(99.0)
        .calculate()
        .pp()
        * 10.0)
        .round()
        / 10.0;
    let pp_98 = (rosu_pp::Performance::new(diff.clone())
        .mods(mod_bits)
        .accuracy(98.0)
        .calculate()
        .pp()
        * 10.0)
        .round()
        / 10.0;
    let pp_95 = (rosu_pp::Performance::new(diff)
        .mods(mod_bits)
        .accuracy(95.0)
        .calculate()
        .pp()
        * 10.0)
        .round()
        / 10.0;

    let (aim_pp, speed_pp, acc_pp) = match perf_100 {
        rosu_pp::any::PerformanceAttributes::Osu(osu) => (
            Some((osu.pp_aim * 10.0).round() / 10.0),
            Some((osu.pp_speed * 10.0).round() / 10.0),
            Some((osu.pp_acc * 10.0).round() / 10.0),
        ),
        _ => (None, None, None),
    };

    Some(serde_json::json!({
        "stars": stars,
        "max_combo": max_combo,
        "clock_rate": (clock_rate * 100.0).round() / 100.0,
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
        "ss_breakdown_osu": {
            "aim_pp": aim_pp,
            "speed_pp": speed_pp,
            "acc_pp": acc_pp,
        }
    }))
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
}
