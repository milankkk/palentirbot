use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use eyre::{Context as _, Result};
use rosu_pp::Beatmap;
use rosu_v2::prelude::GameModsLegacy;
use tokio::{sync::OnceCell, time::Duration};

use super::{adaptive::show_pp_select_menu, data::ReplaySlim, PpTriple};

pub(crate) struct TitleResult {
    pub title: String,
    pub stars: f64,
    pub pp_v4: PpTriple,
    pub pp_latest: Option<PpTriple>,
    pub pp_2025: Option<PpTriple>,
    pub pp_2024: Option<PpTriple>,
    pub pp_2022: Option<PpTriple>,
    pub pp_2021: Option<PpTriple>,
    pub max_possible_combo: u32,
    pub map_first_obj_ms: f64,
    pub map_last_obj_ms: f64,
}

#[derive(Copy, Clone)]
enum SimulateMode {
    Actual,
    NoChoke,
    Max,
}

const LEGACY_MOD_FLAGS: &[(u32, &str)] = &[
    (1, "nf"),
    (2, "ez"),
    (4, "td"),
    (8, "hd"),
    (16, "hr"),
    (128, "rx"),
    (256, "ht"),
    (1024, "fl"),
    (4096, "so"),
];

async fn calc_single_latest_pp(
    dotnet_path: &str,
    perf_calc_path: &str,
    map_path: &Path,
    replay: &ReplaySlim,
    max_combo: u32,
    mode: SimulateMode,
    is_lazer: bool,
) -> Option<(f64, Option<f64>, Option<f64>, Option<f64>)> {
    let mut cmd = tokio::process::Command::new(dotnet_path);
    cmd.env("DOTNET_ROLL_FORWARD", "Major");
    cmd.arg(perf_calc_path);
    cmd.arg("simulate").arg("osu").arg("-j");

    match mode {
        SimulateMode::Max => {}
        SimulateMode::NoChoke => {
            cmd.args(["--combo", &max_combo.to_string()]);
            cmd.args(["--goods", &replay.count_100.to_string()]);
            cmd.args(["--mehs", &replay.count_50.to_string()]);
            cmd.args(["--misses", "0"]);
        }
        SimulateMode::Actual => {
            cmd.args(["--combo", &replay.max_combo.to_string()]);
            cmd.args(["--goods", &replay.count_100.to_string()]);
            cmd.args(["--mehs", &replay.count_50.to_string()]);
            cmd.args(["--misses", &replay.count_miss.to_string()]);
            if !is_lazer && replay.score > 0 {
                cmd.args(["-l", &replay.score.to_string()]);
            }
        }
    }

    let m = replay.mods;
    for &(bit, acronym) in LEGACY_MOD_FLAGS {
        if (m & bit) != 0 {
            cmd.args(["-m", acronym]);
        }
    }
    if (m & 16384) != 0 {
        cmd.args(["-m", "pf"]);
    } else if (m & 32) != 0 {
        cmd.args(["-m", "sd"]);
    }
    if (m & 512) != 0 {
        cmd.args(["-m", "nc"]);
    } else if (m & 64) != 0 {
        cmd.args(["-m", "dt"]);
    }
    if !is_lazer {
        cmd.args(["-m", "cl"]);
    }

    cmd.kill_on_drop(true);
    cmd.arg(map_path);

    match tokio::time::timeout(Duration::from_secs(30), cmd.output()).await {
        Ok(Ok(output)) if output.status.success() => {
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                if let Some(attrs) = json.get("performance_attributes") {
                    if let Some(pp) = attrs.get("pp").and_then(|pp| pp.as_f64()) {
                        let aim = attrs.get("aim").and_then(|v| v.as_f64());
                        let speed = attrs.get("speed").and_then(|v| v.as_f64());
                        let acc = attrs.get("accuracy").and_then(|v| v.as_f64());
                        return Some((pp, aim, speed, acc));
                    }
                }
                tracing::warn!("Failed to parse PP from JSON: {:?}", json);
            } else {
                tracing::warn!(
                    "Failed to parse JSON: {}",
                    String::from_utf8_lossy(&output.stdout)
                );
            }
        }
        Ok(Ok(output)) => {
            tracing::warn!(
                "Command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(Err(err)) => {
            tracing::warn!("Failed to spawn PerformanceCalculator command: {err}");
        }
        Err(_) => {
            tracing::warn!("PerformanceCalculator timed out after 30s");
        }
    }
    None
}

async fn resolve_dotnet_path() -> Option<String> {
    if let Ok(custom) = std::env::var("DOTNET_PATH") {
        if custom == "dotnet" || tokio::fs::try_exists(&custom).await.unwrap_or(false) {
            return Some(custom);
        }
    }
    if let Ok(output) = tokio::process::Command::new("dotnet")
        .arg("--version")
        .output()
        .await
    {
        if output.status.success() {
            return Some("dotnet".to_owned());
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let user_dotnet = std::path::PathBuf::from(home).join(".dotnet/dotnet");
        if tokio::fs::try_exists(&user_dotnet).await.unwrap_or(false) {
            return Some(user_dotnet.to_string_lossy().into_owned());
        }
    }
    for p in ["/usr/bin/dotnet", "/usr/local/bin/dotnet", "/app/dotnet/dotnet"] {
        if tokio::fs::try_exists(p).await.unwrap_or(false) {
            return Some(p.to_owned());
        }
    }
    None
}

async fn resolve_perf_calc_path() -> Option<String> {
    let candidate = if let Ok(custom) = std::env::var("PERF_CALC_PATH") {
        custom
    } else {
        let base = crate::core::BotConfig::get().paths.folders();
        let net10 =
            base.join("osu-tools/PerformanceCalculator/bin/Release/net10.0/PerformanceCalculator.dll");
        if tokio::fs::try_exists(&net10).await.unwrap_or(false) {
            net10.to_string_lossy().into_owned()
        } else {
            base.join("osu-tools/PerformanceCalculator/bin/Release/net8.0/PerformanceCalculator.dll")
                .to_string_lossy()
                .into_owned()
        }
    };
    tokio::fs::try_exists(&candidate)
        .await
        .unwrap_or(false)
        .then_some(candidate)
}

async fn calc_latest_pp(
    map_path: &Path,
    replay: &ReplaySlim,
    max_combo: u32,
    is_lazer: bool,
) -> Option<PpTriple> {
    static DOTNET_PATH: OnceCell<Option<String>> = OnceCell::const_new();
    static PERF_CALC_PATH: OnceCell<Option<String>> = OnceCell::const_new();

    let (dotnet_path, perf_calc_path) = tokio::join!(
        DOTNET_PATH.get_or_init(resolve_dotnet_path),
        PERF_CALC_PATH.get_or_init(resolve_perf_calc_path),
    );
    let dotnet_path = dotnet_path.as_deref()?;
    let perf_calc_path = perf_calc_path.as_deref()?;

    let is_fc = replay.count_miss == 0 && u32::from(replay.max_combo) == max_combo;
    let is_ss = is_fc && replay.count_100 == 0 && replay.count_50 == 0;
    let same_actual_nochoke = is_fc && (is_lazer || replay.score == 0);
    let skip_max = is_ss && same_actual_nochoke;
    let skip_nochoke = same_actual_nochoke || is_ss;

    let opt_sim = |skip: bool, mode: SimulateMode| async move {
        if skip {
            None
        } else {
            calc_single_latest_pp(
                dotnet_path,
                perf_calc_path,
                map_path,
                replay,
                max_combo,
                mode,
                is_lazer,
            )
            .await
        }
    };

    let (actual_res, max_res, nochoke_res) = tokio::join!(
        calc_single_latest_pp(
            dotnet_path,
            perf_calc_path,
            map_path,
            replay,
            max_combo,
            SimulateMode::Actual,
            is_lazer,
        ),
        opt_sim(skip_max, SimulateMode::Max),
        opt_sim(skip_nochoke, SimulateMode::NoChoke),
    );

    let actual = actual_res?;
    let max_pp = if skip_max { actual.0 } else { max_res?.0 };
    let nochoke_pp = if same_actual_nochoke {
        actual.0
    } else if is_ss {
        max_pp
    } else {
        nochoke_res?.0
    };

    Some(PpTriple {
        pp: actual.0,
        nochoke_pp,
        max_pp,
        aim_pp: actual.1,
        speed_pp: actual.2,
        acc_pp: actual.3,
    })
}

macro_rules! calc_historical_pp {
    (
        $krate:ident,
        $bytes:expr,
        $mods:expr,
        $c300:expr,
        $c100:expr,
        $c50:expr,
        $cmiss:expr,
        $combo:expr,
        $nc300:expr,
        $max_combo:expr
    ) => {
        $krate::Beatmap::from_bytes($bytes).ok().map(|map| {
            let diff = $krate::Difficulty::new().mods($mods).calculate(&map);
            let pp = $krate::Performance::new(diff.clone())
                .mods($mods)
                .n300($c300)
                .n100($c100)
                .n50($c50)
                .misses($cmiss)
                .combo($combo)
                .calculate()
                .pp();
            let max_pp = $krate::Performance::new(diff.clone())
                .mods($mods)
                .calculate()
                .pp();
            let nochoke_pp = $krate::Performance::new(diff)
                .mods($mods)
                .n300($nc300)
                .n100($c100)
                .n50($c50)
                .misses(0)
                .combo($max_combo)
                .calculate()
                .pp();
            PpTriple::basic(pp, nochoke_pp, max_pp)
        })
    };
}

pub(crate) async fn create_title(
    replay: &ReplaySlim,
    map_path: PathBuf,
    osu_bytes: Arc<[u8]>,
    map_title: &str,
    is_lazer: bool,
    include_historical_pp: bool,
) -> Result<TitleResult> {
    let osu_bytes_v4 = Arc::clone(&osu_bytes);
    let map_path_v4 = map_path.clone();
    let mods_val = replay.mods;
    let count_300 = replay.count_300 as u32;
    let count_100 = replay.count_100 as u32;
    let count_50 = replay.count_50 as u32;
    let count_miss = replay.count_miss as u32;
    let max_combo = replay.max_combo as u32;
    let total_300_nochoke = count_300 + count_miss;
    let score_val = replay.score;
    let large_tick_hits = replay.large_tick_hits;
    let small_tick_hits = replay.small_tick_hits;
    let slider_end_hits = replay.slider_end_hits;

    let (pp_v4, stars, max_possible_combo, map_first_obj_ms, map_last_obj_ms) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            let map = Beatmap::from_bytes(&osu_bytes_v4)
                .with_context(|| format!("failed to parse map at {map_path_v4:?}"))?;

            let map_first_obj_ms = map.hit_objects.first().map_or(0.0, |o| o.start_time);
            let map_last_obj_ms = map
                .hit_objects
                .iter()
                .map(|o| {
                    use rosu_pp::model::hit_object::{HitObjectKind, HoldNote, Spinner};
                    match &o.kind {
                        HitObjectKind::Spinner(Spinner { duration })
                        | HitObjectKind::Hold(HoldNote { duration }) => o.start_time + *duration,
                        _ => o.start_time,
                    }
                })
                .fold(0.0_f64, f64::max);

            let diff_attrs = rosu_pp::Difficulty::new()
                .mods(mods_val)
                .lazer(is_lazer)
                .calculate(&map);

            let stars = diff_attrs.stars();
            let max_possible_combo = diff_attrs.max_combo();

            let mut perf = rosu_pp::Performance::new(diff_attrs.clone())
                .mods(mods_val)
                .n300(count_300)
                .n100(count_100)
                .n50(count_50)
                .misses(count_miss)
                .combo(max_combo)
                .lazer(is_lazer);

            if is_lazer {
                if let Some(v) = large_tick_hits {
                    perf = perf.large_tick_hits(v);
                }
                if let Some(v) = small_tick_hits {
                    perf = perf.small_tick_hits(v);
                }
                if let Some(v) = slider_end_hits {
                    perf = perf.slider_end_hits(v);
                }
            } else if score_val > 0 {
                perf = perf.legacy_total_score(score_val);
            }
            let actual_attrs = perf.calculate();
            let pp = actual_attrs.pp();
            let (aim_pp, speed_pp, acc_pp) = match &actual_attrs {
                rosu_pp::any::PerformanceAttributes::Osu(osu) => {
                    (Some(osu.pp_aim), Some(osu.pp_speed), Some(osu.pp_acc))
                }
                _ => (None, None, None),
            };

            let max_pp = rosu_pp::Performance::new(diff_attrs.clone())
                .mods(mods_val)
                .lazer(is_lazer)
                .calculate()
                .pp();

            let nochoke_pp = rosu_pp::Performance::new(diff_attrs)
                .mods(mods_val)
                .n300(total_300_nochoke)
                .n100(count_100)
                .n50(count_50)
                .misses(0)
                .combo(max_possible_combo)
                .lazer(is_lazer)
                .calculate()
                .pp();

            Ok((
                PpTriple {
                    pp,
                    nochoke_pp,
                    max_pp,
                    aim_pp,
                    speed_pp,
                    acc_pp,
                },
                crate::util::numbers::round_2_decimals(stars),
                max_possible_combo,
                map_first_obj_ms,
                map_last_obj_ms,
            ))
        })
        .await
        .context("rosu_pp calculation task panicked")??;

    let latest_fut = async {
        if include_historical_pp {
            calc_latest_pp(&map_path, replay, max_possible_combo, is_lazer).await
        } else {
            None
        }
    };

    let hist_fut = async {
        if include_historical_pp && !is_lazer && show_pp_select_menu() {
            tokio::task::spawn_blocking(move || {
                let pp_2025 = calc_historical_pp!(
                    rosu_pp_2025,
                    &osu_bytes,
                    mods_val,
                    count_300,
                    count_100,
                    count_50,
                    count_miss,
                    max_combo,
                    total_300_nochoke,
                    max_possible_combo
                );
                let pp_2024 = calc_historical_pp!(
                    rosu_pp_2024,
                    &osu_bytes,
                    mods_val,
                    count_300,
                    count_100,
                    count_50,
                    count_miss,
                    max_combo,
                    total_300_nochoke,
                    max_possible_combo
                );
                let pp_2022 = rosu_pp_2022::Beatmap::from_bytes(&osu_bytes)
                    .ok()
                    .map(|map_2022| {
                        let max_attrs = rosu_pp_2022::AnyPP::new(&map_2022)
                            .mods(mods_val)
                            .calculate();
                        let max_pp = max_attrs.pp();
                        let actual_attrs = rosu_pp_2022::AnyPP::new(&map_2022)
                            .attributes(max_attrs)
                            .mods(mods_val)
                            .n300(count_300 as usize)
                            .n100(count_100 as usize)
                            .n50(count_50 as usize)
                            .n_misses(count_miss as usize)
                            .combo(max_combo as usize)
                            .calculate();
                        let pp = actual_attrs.pp();
                        let nochoke_pp = rosu_pp_2022::AnyPP::new(&map_2022)
                            .attributes(actual_attrs)
                            .mods(mods_val)
                            .n300(total_300_nochoke as usize)
                            .n100(count_100 as usize)
                            .n50(count_50 as usize)
                            .n_misses(0)
                            .combo(max_possible_combo as usize)
                            .calculate()
                            .pp();
                        PpTriple::basic(pp, nochoke_pp, max_pp)
                    });
                let pp_2021 = calc_historical_pp!(
                    rosu_pp_2021,
                    &osu_bytes,
                    mods_val,
                    count_300,
                    count_100,
                    count_50,
                    count_miss,
                    max_combo,
                    total_300_nochoke,
                    max_possible_combo
                );
                (pp_2025, pp_2024, pp_2022, pp_2021)
            })
            .await
            .context("historical rosu_pp task panicked")
        } else {
            Ok((None, None, None, None))
        }
    };

    let (pp_latest, hist_res) = tokio::join!(latest_fut, hist_fut);
    let (pp_2025, pp_2024, pp_2022, pp_2021) = hist_res?;

    let player = replay.player_name.as_deref().unwrap_or("Unknown player");
    let acc = replay.accuracy();
    let legacy = GameModsLegacy::from_bits(replay.mods);
    let mods = if legacy.is_empty() {
        String::new()
    } else {
        format!(" +{legacy}")
    };
    let active_pp = pp_latest.unwrap_or(pp_v4).pp;
    let pp_suffix = if active_pp.is_finite() && active_pp > 0.0 {
        format!(" • {}pp", active_pp.round() as u64)
    } else {
        String::new()
    };

    Ok(TitleResult {
        title: format!("[{stars}⭐] {player} | {map_title}{mods} ({acc}%){pp_suffix}"),
        stars,
        pp_v4,
        pp_latest,
        pp_2025,
        pp_2024,
        pp_2022,
        pp_2021,
        max_possible_combo,
        map_first_obj_ms,
        map_last_obj_ms,
    })
}
