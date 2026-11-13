use std::time::Duration;

use reqwest::Client;
use rosu_v2::{
    model::GameMode,
    prelude::{Osu, Score, UserId},
};

const MAX_OSU_FILE_BYTES: usize = 5 * 1024 * 1024;

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

/// Parse classic osu! mod acronyms (e.g. `HDDT`, `HD,HR`, `+EZDT`) into legacy bitmask + label.
pub fn parse_mod_bits(raw: Option<&str>) -> (u32, String) {
    let cleaned: String = raw
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();

    if cleaned.is_empty() || cleaned == "NM" || cleaned == "NOMOD" {
        return (0, "NM".to_owned());
    }

    let mut bits = 0u32;
    let mut labels = Vec::new();
    let bytes = cleaned.as_bytes();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        let pair = &cleaned[i..i + 2];
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
        if bit != 0 && !labels.contains(&canonical) {
            bits |= bit;
            labels.push(canonical);
        }
        i += 2;
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

    let without_query = trimmed.split('?').next().unwrap_or(trimmed);
    for marker in ["/beatmaps/", "/b/", "#osu/", "#taiko/", "#fruits/", "#mania/"] {
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

/// Parse a user ID or username from tool call arguments or an `/users/<id>` profile URL.
pub fn parse_osu_user_id(args: &serde_json::Value) -> Option<UserId> {
    if let Some(id) = args
        .get("user_id")
        .or_else(|| args.get("username"))
        .or_else(|| args.get("user"))
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok())
    {
        return Some(UserId::Id(id));
    }

    let raw = args
        .get("username")
        .or_else(|| args.get("user"))
        .or_else(|| args.get("player"))
        .or_else(|| args.as_str().map(|_| args))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;

    if let Some(pos) = raw.find("/users/") {
        let rest = &raw[pos + "/users/".len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(id) = digits.parse::<u32>() {
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
        "map": format!("{map_title} [{diff}]"),
        "stars": stars,
        "mods": mods_label,
        "grade": format!("{:?}", score.grade),
        "acc": format!("{:.2}%", score.accuracy),
        "pp": score.pp.map(|p| (p * 100.0).round() / 100.0),
        "combo": score.max_combo,
        "misses": score.statistics.miss,
    })
}

/// Look up live osu! profile stats, top 5 plays, and 3 most recent plays.
pub async fn execute_osu_player_tool(osu: &Osu, args: &serde_json::Value) -> String {
    let Some(user_id) = parse_osu_user_id(args) else {
        return "[osu_player error: missing `username` argument]".to_owned();
    };
    let mode = parse_game_mode(args.get("mode").and_then(|v| v.as_str()));

    let user = match osu.user(user_id).mode(mode).await {
        Ok(u) => u,
        Err(err) => return format!("[osu_player error: could not fetch player: {err}]"),
    };

    let (top_res, recent_res) = tokio::join!(
        osu.user_scores(user.user_id).best().mode(mode).limit(5).into_future_wrapper(),
        osu.user_scores(user.user_id)
            .recent()
            .include_fails(true)
            .mode(mode)
            .limit(3)
            .into_future_wrapper(),
    );

    let top_plays: Vec<serde_json::Value> = top_res
        .unwrap_or_default()
        .iter()
        .map(summarize_score)
        .collect();
    let recent_plays: Vec<serde_json::Value> = recent_res
        .unwrap_or_default()
        .iter()
        .map(summarize_score)
        .collect();

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

/// Look up live osu! beatmap metadata and calculate modded star rating + 95/98/99/100% PP via `rosu-pp`.
pub async fn execute_osu_beatmap_tool(
    osu: &Osu,
    http: &Client,
    args: &serde_json::Value,
) -> String {
    let id_opt = args
        .get("beatmap_id")
        .or_else(|| args.get("map_id"))
        .or_else(|| args.get("id"))
        .or_else(|| args.get("url"))
        .and_then(|v| {
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .or_else(|| v.as_str().and_then(parse_beatmap_id))
        });

    let Some(map_id) = id_opt else {
        return "[osu_beatmap error: provide numeric `beatmap_id` or an osu.ppy.sh beatmap URL]".to_owned();
    };

    let map = match osu.beatmap().map_id(map_id).await {
        Ok(m) => m,
        Err(err) => return format!("[osu_beatmap error: could not fetch beatmap {map_id}: {err}]"),
    };

    let (mod_bits, mods_label) = parse_mod_bits(args.get("mods").and_then(|v| v.as_str()));

    let pp_calc = match http
        .get(format!("https://osu.ppy.sh/osu/{map_id}"))
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => match resp.bytes().await {
            Ok(bytes) if bytes.len() <= MAX_OSU_FILE_BYTES => {
                tokio::task::spawn_blocking(move || compute_rosu_pp(&bytes, mod_bits))
                    .await
                    .ok()
                    .flatten()
            }
            _ => None,
        },
        _ => None,
    };

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
        "status": format!("{:?}", map.status),
        "url": map.url,
        "base_stars": (map.stars * 100.0).round() / 100.0,
        "bpm": map.bpm,
        "drain_seconds": map.seconds_drain,
        "ar": map.ar,
        "cs": map.cs,
        "od": map.od,
        "hp": map.hp,
        "max_combo": map.max_combo,
        "objects": map.count_objects(),
        "playcount": map.playcount,
        "passcount": map.passcount,
        "requested_mods": mods_label,
        "modded_pp_estimate": pp_calc,
    });

    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned())
}

fn compute_rosu_pp(osu_bytes: &[u8], mod_bits: u32) -> Option<serde_json::Value> {
    let map = rosu_pp::Beatmap::from_bytes(osu_bytes).ok()?;
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
const OSU_SUBSTRINGS: &[&str] = &[
    "osu.ppy.sh",
    ".osr",
    "beatmap",
    "rosu-pp",
    "rosu_pp",
    "bancho",
    "peppy",
    "danser",
    "bathbot",
    "owobot",
    "sliderbreak",
    "overstream",
    "diffspike",
    "star rating",
    "pp record",
    "pp rework",
    "full combo",
    "osu!mania",
    "osu!taiko",
    "osu!catch",
    "osu_player",
    "osu_beatmap",
];

#[cfg(test)]
const OSU_WORDS: &[&str] = &[
    "osu",
    "taiko",
    "mania",
    "ctb",
    "owc",
    "mwc",
    "twc",
    "cwc",
    "nomod",
    "hddt",
    "hdhr",
    "dthr",
    "ezdt",
    "hrdt",
    "hdfl",
    "dtfl",
    "hdhrdt",
    "mrekk",
    "cookiezi",
    "chocomint",
    "whitecat",
    "vaxei",
    "btmc",
    "accolibed",
    "ivaxa",
    "gnahus",
    "lifeline",
    "maliszewski",
    "ninerik",
    "aetrna",
    "merami",
    "wubwoofwolf",
    "shigetora",
];

#[cfg(test)]
fn contains_ascii_word(haystack: &str, word: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut start = 0usize;
    while let Some(rel) = haystack[start..].find(word) {
        let idx = start + rel;
        let end = idx + word.len();
        let left_ok = idx == 0 || !bytes[idx - 1].is_ascii_alphanumeric();
        let right_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if left_ok && right_ok {
            return true;
        }
        start = idx + 1;
    }
    false
}

#[cfg(test)]
fn contains_pp_mention(haystack: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut start = 0usize;
    while let Some(rel) = haystack[start..].find("pp") {
        let idx = start + rel;
        let end = idx + 2;
        let left_ok = idx == 0
            || bytes[idx - 1].is_ascii_whitespace()
            || bytes[idx - 1].is_ascii_digit()
            || matches!(bytes[idx - 1], b'(' | b'[' | b'+' | b'/' | b'~');
        let right_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if left_ok && right_ok {
            return true;
        }
        start = idx + 1;
    }
    false
}

#[cfg(test)]
fn contains_map_stat_token(haystack: &str) -> bool {
    let bytes = haystack.as_bytes();
    for stat in ["ar", "od"] {
        let mut start = 0usize;
        while let Some(rel) = haystack[start..].find(stat) {
            let idx = start + rel;
            let end = idx + stat.len();
            let left_ok = idx == 0 || !bytes[idx - 1].is_ascii_alphanumeric();
            if left_ok && end < bytes.len() {
                let rest = &bytes[end..];
                let num_idx = usize::from(rest.first().copied() == Some(b' '));
                if rest.get(num_idx).is_some_and(u8::is_ascii_digit) {
                    return true;
                }
            }
            start = idx + 1;
        }
    }
    false
}

/// Return true when user text or a referenced score card actually mentions osu!.
#[cfg(test)]
pub fn text_mentions_osu(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    OSU_SUBSTRINGS.iter().any(|s| lower.contains(s))
        || OSU_WORDS.iter().any(|w| contains_ascii_word(&lower, w))
        || contains_pp_mention(&lower)
        || contains_map_stat_token(&lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mod_bits() {
        assert_eq!(parse_mod_bits(None), (0, "NM".to_owned()));
        assert_eq!(parse_mod_bits(Some("nm")), (0, "NM".to_owned()));
        assert_eq!(parse_mod_bits(Some("+HDDT")), ((1 << 3) | (1 << 6), "HDDT".to_owned()));
        assert_eq!(parse_mod_bits(Some("HD, HR, FL")), ((1 << 3) | (1 << 4) | (1 << 10), "HDHRFL".to_owned()));
    }

    #[test]
    fn test_parse_beatmap_id() {
        assert_eq!(parse_beatmap_id("2101986"), Some(2_101_986));
        assert_eq!(parse_beatmap_id("https://osu.ppy.sh/b/75"), Some(75));
        assert_eq!(
            parse_beatmap_id("https://osu.ppy.sh/beatmapsets/1011011#osu/2116202"),
            Some(2_116_202)
        );
        assert_eq!(
            parse_beatmap_id("https://osu.ppy.sh/beatmaps/4000123?mode=osu"),
            Some(4_000_123)
        );
        assert_eq!(parse_beatmap_id("not-an-id"), None);
    }

    #[test]
    fn test_parse_osu_user_id() {
        let name_val = serde_json::json!({ "username": "mrekk" });
        assert!(matches!(parse_osu_user_id(&name_val), Some(UserId::Name(_))));

        let url_val = serde_json::json!({ "username": "https://osu.ppy.sh/users/7562902/osu" });
        assert!(matches!(parse_osu_user_id(&url_val), Some(UserId::Id(7_562_902))));

        let num_val = serde_json::json!({ "username": 7562902 });
        assert!(matches!(parse_osu_user_id(&num_val), Some(UserId::Id(7_562_902))));
    }

    #[test]
    fn test_text_mentions_osu() {
        assert!(text_mentions_osu("what is osu?"));
        assert!(text_mentions_osu("check mrekk on osu!"));
        assert!(text_mentions_osu("how much pp is 1,400pp HDDT?"));
        assert!(text_mentions_osu("https://osu.ppy.sh/beatmapsets/1011011#osu/2116202"));
        assert!(text_mentions_osu("if AR 9.3 and OD 8.5"));

        assert!(!text_mentions_osu("why is the sky blue?"));
        assert!(!text_mentions_osu("check my apple support app please"));
        assert!(!text_mentions_osu("let's focus on physics homework"));
        assert!(!text_mentions_osu("what is the AR-15 rifle or CS 2 rank?"));
        assert!(!text_mentions_osu("what was the football score yesterday?"));
    }
}
