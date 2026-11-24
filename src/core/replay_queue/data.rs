use std::{
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
};

use osu_db::Replay;
use rosu_v2::prelude::Grade;
use serde::Serialize;
use tokio::sync::{Notify, RwLock};
use twilight_model::id::{
    marker::{ChannelMarker, MessageMarker, UserMarker},
    Id,
};

#[derive(Clone, Debug, Serialize)]
pub struct WebJobState {
    pub status: String,
    pub progress: u8,
    pub title: Option<String>,
    pub watch_url: Option<String>,
    pub watch_path: Option<String>,
    pub video_url: Option<String>,
    pub thumbnail_url: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub finished_at: Option<i64>,
}

impl WebJobState {
    pub fn new_queued(title: Option<String>, created_at: i64) -> Self {
        Self {
            status: "queued".to_owned(),
            progress: 0,
            title,
            watch_url: None,
            watch_path: None,
            video_url: None,
            thumbnail_url: None,
            error: None,
            created_at,
            finished_at: None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "done" | "error" | "cancelled")
    }
}

pub struct WebJobHandle {
    pub token: String,
    pub client_ip: String,
    pub settings_name: String,
    pub settings_path: PathBuf,
    pub state: RwLock<WebJobState>,
}

#[derive(Clone, Debug, Default)]
pub struct RenderOptions {
    pub start: u32,
    pub end: u32,
    pub pitch: Option<f64>,
    pub skin: Option<String>,
    pub skip_intro: bool,
    pub offset_ms: Option<i32>,
    pub force_rerender: bool,
    pub skip_info_embed: bool,
}

#[derive(Clone)]
pub struct ReplayData {
    pub job_id: u64,
    pub input_channel: Id<ChannelMarker>,
    pub pitch: Option<f64>,
    pub skin: Option<String>,
    pub skip_intro: bool,
    pub offset_ms: Option<i32>,
    pub force_rerender: bool,
    pub skip_info_embed: bool,
    pub path: PathBuf,
    pub replay: ReplaySlim,
    pub time_points: TimePoints,
    pub user: Id<UserMarker>,
    pub title: Option<String>,
    pub queue_message: Option<(Id<MessageMarker>, Id<ChannelMarker>)>,
    pub is_lazer: bool,
    pub cancelled: Arc<AtomicBool>,
    pub cancel_notify: Arc<Notify>,
    pub web: Option<Arc<WebJobHandle>>,
}

impl ReplayData {
    pub fn from_options(
        input_channel: Id<ChannelMarker>,
        path: PathBuf,
        replay: ReplaySlim,
        user: Id<UserMarker>,
        is_lazer: bool,
        opts: RenderOptions,
    ) -> Self {
        Self {
            job_id: 0,
            input_channel,
            pitch: opts.pitch,
            skin: opts.skin,
            skip_intro: opts.skip_intro,
            offset_ms: opts.offset_ms,
            force_rerender: opts.force_rerender,
            skip_info_embed: opts.skip_info_embed,
            path,
            replay,
            time_points: TimePoints {
                start: opts.start,
                end: opts.end,
            },
            user,
            title: None,
            queue_message: None,
            is_lazer,
            cancelled: Arc::new(AtomicBool::new(false)),
            cancel_notify: Arc::new(Notify::new()),
            web: None,
        }
    }

    pub fn for_web(
        path: PathBuf,
        replay: ReplaySlim,
        is_lazer: bool,
        opts: RenderOptions,
        web: Arc<WebJobHandle>,
    ) -> Self {
        Self {
            job_id: 0,
            input_channel: Id::new(1),
            pitch: opts.pitch,
            skin: opts.skin,
            skip_intro: opts.skip_intro,
            offset_ms: opts.offset_ms,
            force_rerender: opts.force_rerender,
            skip_info_embed: true,
            path,
            replay,
            time_points: TimePoints {
                start: opts.start,
                end: opts.end,
            },
            user: Id::new(1),
            title: None,
            queue_message: None,
            is_lazer,
            cancelled: Arc::new(AtomicBool::new(false)),
            cancel_notify: Arc::new(Notify::new()),
            web: Some(web),
        }
    }

    pub fn replay_name(&self) -> String {
        let raw_name = self
            .path
            .file_name()
            .unwrap_or_else(|| self.path.as_os_str())
            .to_string_lossy();

        let lower = raw_name.to_ascii_lowercase();
        let extension = lower.rfind(".osr").unwrap_or(raw_name.len());
        let suffix = raw_name[..extension].rfind("_Osu").unwrap_or(extension);
        let without_ext = &raw_name[..suffix];

        // Strip internal routing prefixes added when storing replays:
        // - `{seq}_{discord_user_id}_{score_id}_{desc}`
        // - `{snowflake_id}_{filename}`
        let trimmed_stem = strip_internal_prefix(without_ext);

        if trimmed_stem.contains('_') {
            trimmed_stem.replace('_', " ")
        } else {
            trimmed_stem.to_owned()
        }
    }
}

fn is_ascii_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn strip_internal_prefix(stem: &str) -> &str {
    // Check `{seq}_{discord_user_id (>=15 digits)}_{score_id}_{desc}` first
    let mut parts = stem.splitn(4, '_');
    if let (Some(p0), Some(p1), Some(p2), Some(p3)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    {
        if is_ascii_digits(p0)
            && p1.len() >= 15
            && is_ascii_digits(p1)
            && is_ascii_digits(p2)
            && !p3.is_empty()
        {
            return p3;
        }
    }

    // Check `{snowflake_id (>=15 digits)}_{filename}` from message/interaction uploads
    if let Some((first, rest)) = stem.split_once('_') {
        if first.len() >= 15 && is_ascii_digits(first) && !rest.is_empty() {
            return rest;
        }
    }

    stem
}

#[derive(Copy, Clone)]
pub struct TimePoints {
    pub start: u32,
    pub end: u32,
}

impl TimePoints {
    pub fn parse_single(s: &str) -> Result<u32, &'static str> {
        let s = s.trim();
        let mut iter = s.split(':').map(str::parse::<u32>);

        match (iter.next(), iter.next(), iter.next()) {
            (Some(Ok(minutes)), Some(Ok(seconds @ 0..=59)), None) => minutes
                .checked_mul(60)
                .and_then(|m| m.checked_add(seconds))
                .ok_or("Time value is too large!"),
            (Some(Ok(_)), Some(Ok(_)), None) => Err("Seconds must be between 0 and 59!"),
            (Some(Ok(seconds)), None, None) => Ok(seconds),
            _ => Err("A value you supplied is not a valid timestamp!"),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum ReplayStatus {
    Waiting,
    Downloading,
    MapFound,
    Rendering(u8),
    Uploading,
}

#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct PpTriple {
    pub pp: f64,
    pub nochoke_pp: f64,
    pub max_pp: f64,
    pub aim_pp: Option<f64>,
    pub speed_pp: Option<f64>,
    pub acc_pp: Option<f64>,
}

impl PpTriple {
    #[inline]
    pub const fn basic(pp: f64, nochoke_pp: f64, max_pp: f64) -> Self {
        Self {
            pp,
            nochoke_pp,
            max_pp,
            aim_pp: None,
            speed_pp: None,
            acc_pp: None,
        }
    }
}

#[derive(Clone)]
pub struct ReplaySlim {
    pub beatmap_hash: Option<String>,
    pub count_300: u16,
    pub count_100: u16,
    pub count_50: u16,
    pub count_miss: u16,
    pub max_combo: u16,
    pub mods: u32,
    pub player_name: Option<String>,
    pub replay_hash: Option<String>,
    pub score: u32,
    pub timestamp: Option<i64>,
    pub grade: Grade,
    pub accuracy_override: Option<f32>,
    pub large_tick_hits: Option<u32>,
    pub small_tick_hits: Option<u32>,
    pub slider_end_hits: Option<u32>,
}

impl ReplaySlim {
    pub fn total_hits(&self) -> u32 {
        self.count_300 as u32
            + self.count_100 as u32
            + self.count_50 as u32
            + self.count_miss as u32
    }

    pub fn accuracy(&self) -> f32 {
        if let Some(acc) = self.accuracy_override {
            return (acc * 100.0).round() / 100.0;
        }

        let denominator = self.total_hits() as f32 * 300.0;
        if denominator == 0.0 {
            return 0.0;
        }

        let numerator = (self.count_50 as u32 * 50
            + self.count_100 as u32 * 100
            + self.count_300 as u32 * 300) as f32;

        (10_000.0 * numerator / denominator).round() / 100.0
    }
}

pub(crate) fn sanitize_player_name(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    let mut cleaned = String::with_capacity(raw.len().min(32));
    let mut count = 0usize;
    for ch in raw.chars() {
        if count >= 32 {
            break;
        }
        if ch.is_control() || matches!(ch, '<' | '>' | '"' | '\'' | '`' | '&') {
            continue;
        }
        cleaned.push(ch);
        count += 1;
    }
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

pub(crate) fn sanitize_hex_hash(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 64 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(trimmed.to_ascii_lowercase())
}

impl From<Replay> for ReplaySlim {
    #[inline]
    fn from(replay: Replay) -> Self {
        let mods = replay.mods.bits();
        let grade = calculate_grade_osu_std(
            replay.count_300,
            replay.count_100,
            replay.count_50,
            replay.count_miss,
            mods,
            replay.version, // game version from .osr header
        );
        Self {
            beatmap_hash: sanitize_hex_hash(replay.beatmap_hash),
            count_300: replay.count_300,
            count_100: replay.count_100,
            count_50: replay.count_50,
            count_miss: replay.count_miss,
            max_combo: replay.max_combo,
            mods,
            player_name: sanitize_player_name(replay.player_name),
            replay_hash: sanitize_hex_hash(replay.replay_hash),
            score: replay.score,
            timestamp: Some(replay.timestamp.timestamp()),
            grade,
            accuracy_override: None,
            large_tick_hits: None,
            small_tick_hits: None,
            slider_end_hits: None,
        }
    }
}

/// Detect whether this replay was set on osu! lazer.
/// Lazer writes versions >= 30_000_000. Stable uses date-based versions
/// like 20240101.
pub fn is_lazer_replay(game_version: u32) -> bool {
    game_version >= 30_000_000
}

/// osu!standard grade calculation for both stable and lazer.
/// Lazer (without Classic/ScoreV2 bit) uses accuracy thresholds (100% SS, >=95% & 0 miss S, >=90% A, >=80% B, >=70% C);
/// Stable / Classic uses hit-count ratios (>90% 300s & <=1% 50s & 0 miss S, etc.).
/// Silver (XH/SH) requires Hidden (8), Flashlight (1024), or FadeIn (1048576).
pub fn calculate_grade_osu_std(
    count_300: u16,
    count_100: u16,
    count_50: u16,
    count_miss: u16,
    mods: u32,
    game_version: u32,
) -> Grade {
    let n300 = count_300 as u32;
    let n100 = count_100 as u32;
    let n50 = count_50 as u32;
    let nmiss = count_miss as u32;
    let total = n300 + n100 + n50 + nmiss;

    if total == 0 {
        return Grade::F;
    }

    let silver = (mods & 8) != 0        // Hidden
              || (mods & 1024) != 0     // Flashlight
              || (mods & 1048576) != 0; // FadeIn

    // Bit 29 (1 << 29) in legacy bitmask denotes ScoreV2 / classic toggle if present.
    let is_lazer_modern = is_lazer_replay(game_version) && (mods & (1 << 29)) == 0;

    if is_lazer_modern {
        if n100 == 0 && n50 == 0 && nmiss == 0 {
            return if silver { Grade::XH } else { Grade::X };
        }
        let acc = (n300 * 300 + n100 * 100 + n50 * 50) as f32 / (total as f32 * 300.0);
        if nmiss == 0 && acc >= 0.95 {
            return if silver { Grade::SH } else { Grade::S };
        }
        if acc >= 0.90 {
            return Grade::A;
        }
        if acc >= 0.80 {
            return Grade::B;
        }
        if acc >= 0.70 {
            return Grade::C;
        }
        return Grade::D;
    }

    let ratio_300 = n300 as f32 / total as f32;
    let ratio_50 = n50 as f32 / total as f32;

    // SS: 100% accuracy — no 100s, no 50s, no misses
    if n100 == 0 && n50 == 0 && nmiss == 0 {
        return if silver { Grade::XH } else { Grade::X };
    }

    // S: >90% 300s, <=1% 50s, 0 misses
    if nmiss == 0 && ratio_300 > 0.90 && ratio_50 <= 0.01 {
        return if silver { Grade::SH } else { Grade::S };
    }

    // A: >80% 300s + 0 misses  OR  >90% 300s
    if (nmiss == 0 && ratio_300 > 0.80) || ratio_300 > 0.90 {
        return Grade::A;
    }

    // B: >70% 300s + 0 misses  OR  >80% 300s
    if (nmiss == 0 && ratio_300 > 0.70) || ratio_300 > 0.80 {
        return Grade::B;
    }

    // C: >60% 300s
    if ratio_300 > 0.60 {
        return Grade::C;
    }

    Grade::D
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timepoints_parse_single() {
        assert_eq!(TimePoints::parse_single("90"), Ok(90));
        assert_eq!(TimePoints::parse_single("1:30"), Ok(90));
        assert_eq!(TimePoints::parse_single(" 02:05 "), Ok(125));
        assert!(TimePoints::parse_single("1:60").is_err());
        assert!(TimePoints::parse_single("1:2:3").is_err());
        assert!(TimePoints::parse_single("999999999:00").is_err());
    }

    #[test]
    fn test_replay_name_handles_multiple_underscores_and_unicode() {
        let mut data = ReplayData::from_options(
            Id::new(1),
            PathBuf::from("/tmp/123_player_alpha_beta_map_Osu.osr"),
            ReplaySlim {
                beatmap_hash: None,
                count_300: 100,
                count_100: 0,
                count_50: 0,
                count_miss: 0,
                max_combo: 100,
                mods: 0,
                player_name: None,
                replay_hash: None,
                score: 0,
                timestamp: None,
                grade: Grade::X,
                accuracy_override: None,
                large_tick_hits: None,
                small_tick_hits: None,
                slider_end_hits: None,
            },
            Id::new(1),
            false,
            RenderOptions::default(),
        );
        data.replay.player_name = None;
        assert_eq!(data.replay_name(), "123 player alpha beta map");

        data.path = PathBuf::from("/tmp/7_123456789012345678_987654321_Cookiezi - Blue Zenith.OSR");
        assert_eq!(data.replay_name(), "Cookiezi - Blue Zenith");
    }

    #[test]
    fn test_replay_slim_total_hits_and_zero_accuracy() {
        let mut slim = ReplaySlim {
            beatmap_hash: None,
            count_300: 0,
            count_100: 0,
            count_50: 0,
            count_miss: 0,
            max_combo: 0,
            mods: 0,
            player_name: None,
            replay_hash: None,
            score: 0,
            timestamp: None,
            grade: Grade::F,
            accuracy_override: None,
            large_tick_hits: None,
            small_tick_hits: None,
            slider_end_hits: None,
        };
        assert_eq!(slim.total_hits(), 0);
        assert_eq!(slim.accuracy(), 0.0);

        slim.count_300 = 50_000;
        slim.count_100 = 20_000;
        assert_eq!(slim.total_hits(), 70_000);
        assert!(slim.accuracy() > 80.0);

        slim.accuracy_override = Some(98.764);
        assert!((slim.accuracy() - 98.76).abs() < 0.001);
    }

    #[test]
    fn test_lazer_grade_calculation() {
        // 95.0% acc on lazer with 0 misses and 2% (>1%) 50s -> S (stable would downgrade to A)
        assert_eq!(
            calculate_grade_osu_std(93, 5, 2, 0, 0, 30_000_000),
            Grade::S
        );
        // Same counts with HD on lazer -> SH
        assert_eq!(
            calculate_grade_osu_std(93, 5, 2, 0, 8, 30_000_000),
            Grade::SH
        );
        // Stable with same counts (ratio_50 == 2% > 1%) -> A
        assert_eq!(
            calculate_grade_osu_std(93, 5, 2, 0, 0, 20240101),
            Grade::A
        );
    }
}
