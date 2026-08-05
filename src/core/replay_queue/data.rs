use std::{borrow::Cow, path::PathBuf};

use crate::util::CowUtils;
use osu_db::Replay;
use rosu_v2::prelude::Grade;
use twilight_model::id::{
    marker::{ChannelMarker, MessageMarker, UserMarker},
    Id,
};

#[derive(Clone)]
pub struct ReplayData {
    pub input_channel: Id<ChannelMarker>,
    pub output_channel: Id<ChannelMarker>,
    pub pitch: Option<f64>,
    pub path: PathBuf,
    pub replay: ReplaySlim,
    pub time_points: TimePoints,
    pub user: Id<UserMarker>,
    pub title: Option<String>,
    pub player_name: Option<String>,
    pub map_title: Option<String>,
    pub difficulty_name: Option<String>,
    pub queue_message: Option<(Id<MessageMarker>, Id<ChannelMarker>)>,
}

impl ReplayData {
    pub fn replay_name(&self) -> Cow<'_, str> {
        let name = self
            .path
            .file_name()
            .expect("missing file name")
            .to_string_lossy();

        let extension = name.rfind(".osr").unwrap_or(name.len());
        let suffix = name[..extension].rfind("_Osu").unwrap_or(extension);

        match name {
            Cow::Borrowed(name) => name[..suffix].cow_replace('_', " "),
            Cow::Owned(mut name) => {
                name.truncate(suffix);

                let mut idx = 0;

                while let Some(i) = name.get(idx..).and_then(|suffix| suffix.find('_')) {
                    let bytes = unsafe { name[idx..].as_bytes_mut() };
                    bytes[i] = b' ';
                    idx = i + 1;
                }

                Cow::Owned(name)
            }
        }
    }
}

#[derive(Copy, Clone)]
pub struct TimePoints {
    pub start: u32,
    pub end: u32,
}

impl TimePoints {
    pub fn parse_single(s: &str) -> Result<u32, &'static str> {
        let mut iter = s.split(':').map(str::parse);

        match (iter.next(), iter.next()) {
            (Some(Ok(minutes)), Some(Ok(seconds @ 0..=59))) => Ok(minutes * 60 + seconds),
            (Some(Ok(_)), Some(Ok(_))) => Err("Seconds must be between 0 and 60!"),
            (Some(Ok(seconds)), None) => Ok(seconds),
            _ => Err("A value you supplied is not a number!"),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum ReplayStatus {
    Waiting,
    Downloading,
    MapFound,
    Rendering(u8),
    Uploading(u64),
}

#[derive(Clone)]
pub struct ReplaySlim {
    pub beatmap_hash: Option<String>,
    pub count_300: u16,
    pub count_100: u16,
    pub count_50: u16,
    pub count_geki: u16,
    pub count_katsu: u16,
    pub count_miss: u16,
    pub max_combo: u16,
    pub mods: u32,
    pub player_name: Option<String>,
    pub replay_hash: Option<String>,
    pub score: u32,
    pub timestamp: Option<i64>,
    pub grade: Grade,
}

impl ReplaySlim {
    pub fn total_hits(&self) -> u16 {
        self.count_300 + self.count_100 + self.count_50 + self.count_miss
    }

    pub fn accuracy(&self) -> f32 {
        let numerator = (self.count_50 as u32 * 50
            + self.count_100 as u32 * 100
            + self.count_300 as u32 * 300) as f32;

        let denominator = self.total_hits() as f32 * 300.0;

        (10_000.0 * numerator / denominator).round() / 100.0
    }
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
            beatmap_hash: replay.beatmap_hash,
            count_300: replay.count_300,
            count_100: replay.count_100,
            count_50: replay.count_50,
            count_geki: replay.count_geki,
            count_katsu: replay.count_katsu,
            count_miss: replay.count_miss,
            max_combo: replay.max_combo,
            mods: replay.mods.bits(),
            player_name: replay.player_name,
            replay_hash: replay.replay_hash,
            score: replay.score,
            timestamp: Some(replay.timestamp.timestamp()),
            grade,
        }
    }
}

/// Detect whether this replay was set on osu! lazer.
/// Lazer writes versions >= 30_000_000. Stable uses date-based versions
/// like 20240101. ScoreV2 mod on stable (bit 29) uses accuracy grading
/// but for osu!std the grade thresholds are identical, so no special
/// case is needed there.
pub fn is_lazer_replay(game_version: u32) -> bool {
    game_version >= 30_000_000
}

/// osu!standard grade calculation for both stable and lazer.
/// Per the osu! wiki, the grade thresholds for osu!standard are
/// identical between stable and lazer — both use hit-count ratios.
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

    // A failing score (life bar drained) should remain F regardless of
    // hit counts. We can't detect that from hit counts alone, but the
    // caller (embed path) overwrites this with the API grade anyway.
    // For local .osr files a passing score is assumed if total > 0.

    let silver = (mods & 8) != 0        // Hidden
              || (mods & 1024) != 0     // Flashlight
              || (mods & 1048576) != 0; // FadeIn

    // Note: lazer uses the same hit-count-based thresholds for osu!standard.
    // The _is_lazer_ flag is available for future mode-specific divergence.
    let _is_lazer = is_lazer_replay(game_version);

    let ratio_300 = n300 as f32 / total as f32;
    let ratio_50 = n50 as f32 / total as f32;

    // SS: 100% accuracy — no 100s, no 50s, no misses
    if n100 == 0 && n50 == 0 && nmiss == 0 {
        return if silver { Grade::XH } else { Grade::X };
    }

    // S: >90% 300s, ≤1% 50s, 0 misses
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
