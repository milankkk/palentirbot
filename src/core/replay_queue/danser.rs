use std::{
    collections::VecDeque,
    path::Path,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

use eyre::{Report, Result};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    process::{Child, Command},
    sync::Notify,
    task::JoinHandle,
    time::Duration,
};

use crate::core::{BotConfig, Context, ReplayStatus, TimePoints};

/// Race-free cancellation helper for long-running replay queue stages.
pub(crate) async fn run_cancellable<T>(
    cancelled: &AtomicBool,
    cancel_notify: &Notify,
    fut: impl std::future::Future<Output = T>,
) -> Option<T> {
    let cancel_fut = cancel_notify.notified();
    tokio::pin!(cancel_fut);
    cancel_fut.as_mut().enable();
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    tokio::select! {
        biased;
        _ = &mut cancel_fut => None,
        val = fut => Some(val),
    }
}

pub(crate) fn build_danser_ld_library_path(danser_dir: &Path) -> String {
    let ffmpeg_lib = danser_dir.join("ffmpeg").join("lib");
    let mut parts = vec![
        danser_dir.display().to_string(),
        ffmpeg_lib.display().to_string(),
    ];
    if let Ok(existing) = std::env::var("LD_LIBRARY_PATH") {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_owned());
        }
    }
    parts.join(":")
}

/// Build the base `danser` command with process group, working dir, LD_LIBRARY_PATH, and `-out`/`-skin`.
pub(crate) fn build_danser_base_command(
    danser_bin: &Path,
    ld_library_path: &str,
    canonical_replay: &Path,
    settings: &str,
    out_name: &str,
    skin: Option<&str>,
) -> Command {
    let mut cmd = Command::new(danser_bin);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.current_dir(BotConfig::get().paths.danser())
        .env("LD_LIBRARY_PATH", ld_library_path)
        .kill_on_drop(true)
        .arg("-noupdatecheck")
        .arg("-replay")
        .arg(canonical_replay)
        .arg("-settings")
        .arg(settings)
        .arg("-out")
        .arg(out_name);

    if let Some(skin_name) = skin {
        cmd.args(["-skin", skin_name]);
    }
    cmd
}

fn strip_danser_log_timestamp(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 20
        && b[4] == b'/'
        && b[7] == b'/'
        && b[10] == b' '
        && b[13] == b':'
        && b[16] == b':'
        && b[19] == b' '
        && b[0..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit)
        && b[11..13].iter().all(u8::is_ascii_digit)
        && b[14..16].iter().all(u8::is_ascii_digit)
        && b[17..19].iter().all(u8::is_ascii_digit)
    {
        s[20..].trim_start()
    } else {
        s
    }
}

pub(crate) fn parse_loaded_skin_line(raw_line: &str) -> Option<String> {
    let body = strip_danser_log_timestamp(raw_line.trim());
    let rest = body.strip_prefix("SkinManager: Skin \"")?;
    let name = rest.strip_suffix("\" loaded.")?;
    let trimmed = name.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

pub(crate) fn is_fatal_danser_line(raw_line: &str) -> bool {
    let trimmed = raw_line.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with("panic:") || trimmed.starts_with("fatal error:") {
        return true;
    }

    let body = strip_danser_log_timestamp(trimmed);
    if body.starts_with("Playing:")
        || body.starts_with("Loading:")
        || body.starts_with("Loading replay for")
        || body.starts_with("Trying to load storyboard")
        || body.starts_with("Ran using:")
        || body.starts_with("Audio track:")
    {
        return false;
    }

    const FATAL_SUBSTRINGS: &[&str] = &[
        "ffmpeg's video process finished abruptly!",
        "ffmpeg's audio process finished abruptly!",
        "ffmpeg finished abruptly!",
        "ffmpeg's video process failed to start!",
        "ffmpeg's audio process failed to start!",
        "ffmpeg not found!",
        "ffmpeg was installed incorrectly!",
        "Codec is not available/supported",
        "SettingsManager: Failed to parse the patch!",
        "Your GPU does not support one or more required OpenGL extensions",
        "CUDA_ERROR_OUT_OF_MEMORY",
        "No capable devices found",
        "OpenEncodeSessionEx failed",
        "Beatmap not found, closing...",
        "Beatmap not found",
        "Replay not found",
    ];
    if FATAL_SUBSTRINGS.iter().any(|needle| body.contains(needle)) {
        return true;
    }

    (body.contains("[h264_nvenc @")
        || body.contains("[hevc_nvenc @")
        || body.contains("[av1_nvenc @"))
        && (body.contains("failed") || body.contains("out of memory") || body.contains("Error"))
}

pub(crate) fn should_retry_low_vram(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    ![
        "beatmap not found",
        "skin not found",
        "replay not found",
        "failed to load beatmap",
        "invalid replay",
        "no such file",
    ]
    .iter()
    .any(|pat| lower.contains(pat))
}

/// Read up to the last `max_lines` non-empty lines from the trailing 64 KiB of `danser.log`.
pub(crate) async fn read_danser_log_tail(danser_dir: &Path, max_lines: usize) -> Vec<String> {
    const TAIL_BYTES: u64 = 64 * 1024;
    if max_lines == 0 {
        return Vec::new();
    }

    let log_path = danser_dir.join("danser.log");
    let Ok(mut file) = tokio::fs::File::open(&log_path).await else {
        return Vec::new();
    };
    let Ok(meta) = file.metadata().await else {
        return Vec::new();
    };
    let len = meta.len();
    let offset = len.saturating_sub(TAIL_BYTES);
    if offset > 0 && file.seek(std::io::SeekFrom::Start(offset)).await.is_err() {
        return Vec::new();
    }

    let mut buf = Vec::with_capacity(len.saturating_sub(offset) as usize);
    if file.read_to_end(&mut buf).await.is_err() {
        return Vec::new();
    }
    let contents = String::from_utf8_lossy(&buf);
    let mut lines = contents.lines();
    if offset > 0 {
        // Seek may land mid-line; discard the potentially partial leading slice.
        let _ = lines.next();
    }

    let mut ring = VecDeque::with_capacity(max_lines);
    for line in lines {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        if ring.len() >= max_lines {
            ring.pop_front();
        }
        ring.push_back(trimmed.to_owned());
    }
    ring.into_iter().collect()
}

struct DanserWatchdog {
    start: std::time::Instant,
    last_activity_ms: AtomicU64,
    saw_progress: AtomicBool,
    fatal_line: std::sync::Mutex<Option<String>>,
    loaded_skin: std::sync::Mutex<Option<String>>,
    fatal_notify: Notify,
}

impl DanserWatchdog {
    fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
            last_activity_ms: AtomicU64::new(0),
            saw_progress: AtomicBool::new(false),
            fatal_line: std::sync::Mutex::new(None),
            loaded_skin: std::sync::Mutex::new(None),
            fatal_notify: Notify::new(),
        }
    }

    fn record_loaded_skin(&self, skin: String) {
        let mut guard = self
            .loaded_skin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(skin);
    }

    fn take_loaded_skin(&self) -> Option<String> {
        self.loaded_skin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn touch_activity(&self) {
        let ms = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_activity_ms.store(ms, Ordering::Release);
    }

    fn mark_progress(&self) {
        self.saw_progress.store(true, Ordering::Release);
        self.touch_activity();
    }

    fn record_fatal(&self, line: &str) {
        {
            let mut guard = self
                .fatal_line
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if guard.is_none() {
                *guard = Some(line.to_owned());
            }
        }
        self.fatal_notify.notify_waiters();
    }

    fn take_fatal(&self) -> Option<String> {
        self.fatal_line
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn idle_ms(&self) -> u64 {
        let now_ms = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let last = self.last_activity_ms.load(Ordering::Acquire);
        now_ms.saturating_sub(last)
    }
}

pub(crate) enum DanserAttemptOutcome {
    Success {
        loaded_skin: Option<String>,
    },
    Cancelled,
    FatalLine {
        line: String,
        out_tail: Vec<String>,
        err_tail: Vec<String>,
    },
    NonZeroExit {
        status: std::process::ExitStatus,
        out_tail: Vec<String>,
        err_tail: Vec<String>,
    },
    Stalled {
        idle_secs: u64,
        saw_progress: bool,
        out_tail: Vec<String>,
        err_tail: Vec<String>,
    },
    TotalTimeout {
        out_tail: Vec<String>,
        err_tail: Vec<String>,
    },
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_danser_attempt(
    ctx: &Arc<Context>,
    danser_path: &Path,
    ld_library_path: &str,
    canonical_path: &Path,
    settings: &str,
    filename: &str,
    time_points: &TimePoints,
    pitch: Option<f64>,
    skin: Option<&str>,
    effective_skip_intro: bool,
    offset_ms: Option<i32>,
    spatch: Option<&str>,
    low_vram_nvenc: bool,
    attempt: u8,
    cancelled: &AtomicBool,
    cancel_notify: &Notify,
) -> Result<DanserAttemptOutcome> {
    let config = BotConfig::get();
    tracing::debug!(
        danser_dir = ?config.paths.danser(),
        ?danser_path,
        attempt,
        low_vram_nvenc,
        has_spatch = spatch.is_some(),
        "launching danser"
    );

    let mut command = build_danser_base_command(
        danser_path,
        ld_library_path,
        canonical_path,
        settings,
        filename,
        skin,
    );
    command
        .arg("-record")
        .arg("-quickstart")
        .arg("-preciseprogress")
        .stderr(Stdio::piped())
        .stdout(Stdio::piped());

    if time_points.start > 0 {
        command.args(["-start", &time_points.start.to_string()]);
    }
    if time_points.end > 0 {
        command.args(["-end", &time_points.end.to_string()]);
    }
    if let Some(pitch_val) = pitch {
        command.args(["-pitch", &pitch_val.to_string()]);
    }
    if effective_skip_intro {
        command.arg("-skip");
    }
    if let Some(offset) = offset_ms {
        command.args(["-offset", &offset.to_string()]);
    }
    if let Some(patch) = spatch {
        command.args(["-sPatch", patch]);
    }

    info!(attempt, low_vram_nvenc, "Started replay processing");

    let cancel_fut = cancel_notify.notified();
    tokio::pin!(cancel_fut);
    cancel_fut.as_mut().enable();

    if cancelled.load(Ordering::Acquire) {
        return Ok(DanserAttemptOutcome::Cancelled);
    }

    let mut child = command
        .spawn()
        .map_err(|err| Report::from(err).wrap_err("failed to start danser command"))?;
    let pgid = child.id();

    let stdout_reader = tokio::io::BufReader::new(child.stdout.take().expect("missing stdout on child"));
    let stderr_reader = tokio::io::BufReader::new(child.stderr.take().expect("missing stderr on child"));

    let watchdog = Arc::new(DanserWatchdog::new());
    let fatal_fut = watchdog.fatal_notify.notified();
    tokio::pin!(fatal_fut);
    fatal_fut.as_mut().enable();

    let stdout_handle = tokio::spawn(read_danser_progress(
        Arc::clone(ctx),
        stdout_reader,
        Arc::clone(&watchdog),
    ));
    let stderr_handle = tokio::spawn(read_danser_progress(
        Arc::clone(ctx),
        stderr_reader,
        Arc::clone(&watchdog),
    ));

    enum SelectStop {
        Cancelled,
        Fatal,
        Exit(std::io::Result<std::process::ExitStatus>),
        Stall { idle_secs: u64, saw_progress: bool },
        TotalTimeout,
    }

    let watchdog_poll = async {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.tick().await;
        loop {
            interval.tick().await;
            if watchdog.start.elapsed() >= Duration::from_secs(1800) {
                return SelectStop::TotalTimeout;
            }
            let saw_progress = watchdog.saw_progress.load(Ordering::Acquire);
            let limit_ms = if saw_progress { 90_000 } else { 120_000 };
            let idle_ms = watchdog.idle_ms();
            if idle_ms >= limit_ms {
                return SelectStop::Stall {
                    idle_secs: idle_ms / 1000,
                    saw_progress,
                };
            }
        }
    };

    let stop = if watchdog.take_fatal().is_some() {
        SelectStop::Fatal
    } else {
        tokio::select! {
            biased;
            _ = &mut cancel_fut => SelectStop::Cancelled,
            _ = &mut fatal_fut => SelectStop::Fatal,
            res = child.wait() => SelectStop::Exit(res),
            wd_stop = watchdog_poll => wd_stop,
        }
    };

    tracing::trace!(attempt, "Danser attempt wait completed");

    match stop {
        SelectStop::Cancelled => {
            terminate_danser_group(&mut child, pgid).await;
            let _ = drain_or_abort_stdio(stdout_handle, stderr_handle).await;
            Ok(DanserAttemptOutcome::Cancelled)
        }
        SelectStop::Fatal => {
            let _ = tokio::time::timeout(Duration::from_millis(250), child.wait()).await;
            terminate_danser_group(&mut child, pgid).await;
            let (out_tail, err_tail) = drain_or_abort_stdio(stdout_handle, stderr_handle).await;
            Ok(DanserAttemptOutcome::FatalLine {
                line: watchdog.take_fatal().unwrap_or_default(),
                out_tail: out_tail.unwrap_or_default(),
                err_tail: err_tail.unwrap_or_default(),
            })
        }
        SelectStop::Exit(Ok(status)) => {
            let fatal = watchdog.take_fatal();
            if fatal.is_some() || !status.success() {
                terminate_danser_group(&mut child, pgid).await;
            }
            let (out_tail, err_tail) = drain_or_abort_stdio(stdout_handle, stderr_handle).await;
            let out_tail = out_tail.unwrap_or_default();
            let err_tail = err_tail.unwrap_or_default();
            if let Some(line) = fatal {
                return Ok(DanserAttemptOutcome::FatalLine {
                    line,
                    out_tail,
                    err_tail,
                });
            }
            if !status.success() {
                return Ok(DanserAttemptOutcome::NonZeroExit {
                    status,
                    out_tail,
                    err_tail,
                });
            }
            Ok(DanserAttemptOutcome::Success {
                loaded_skin: watchdog.take_loaded_skin(),
            })
        }
        SelectStop::Exit(Err(err)) => {
            terminate_danser_group(&mut child, pgid).await;
            let _ = drain_or_abort_stdio(stdout_handle, stderr_handle).await;
            Err(Report::from(err).wrap_err("failed to run danser command"))
        }
        SelectStop::TotalTimeout => {
            terminate_danser_group(&mut child, pgid).await;
            let (out_tail, err_tail) = drain_or_abort_stdio(stdout_handle, stderr_handle).await;
            Ok(DanserAttemptOutcome::TotalTimeout {
                out_tail: out_tail.unwrap_or_default(),
                err_tail: err_tail.unwrap_or_default(),
            })
        }
        SelectStop::Stall {
            idle_secs,
            saw_progress,
        } => {
            terminate_danser_group(&mut child, pgid).await;
            let (out_tail, err_tail) = drain_or_abort_stdio(stdout_handle, stderr_handle).await;
            Ok(DanserAttemptOutcome::Stalled {
                idle_secs,
                saw_progress,
                out_tail: out_tail.unwrap_or_default(),
                err_tail: err_tail.unwrap_or_default(),
            })
        }
    }
}

fn push_danser_line(tail: &mut VecDeque<String>, watchdog: &DanserWatchdog, line: &str) {
    if line.trim().is_empty() {
        return;
    }
    if tail.len() >= 20 {
        tail.pop_front();
    }
    tail.push_back(line.to_owned());
    watchdog.touch_activity();
    if let Some(loaded) = parse_loaded_skin_line(line) {
        watchdog.record_loaded_skin(loaded);
    }
    if is_fatal_danser_line(line) {
        watchdog.record_fatal(line);
    }
}

pub(crate) fn parse_danser_progress(line: &str) -> Option<u8> {
    let idx = line.find("Progress:")?;
    let remainder = &line[idx + "Progress:".len()..];
    let pct_str = remainder.split('%').next()?.trim();
    if pct_str.is_empty() {
        return None;
    }
    let val = pct_str.parse::<f32>().ok()?;
    if !val.is_finite() || val < 0.0 {
        return None;
    }
    Some(val.round().clamp(0.0, 100.0) as u8)
}

async fn read_danser_progress<R: tokio::io::AsyncRead + Unpin>(
    ctx: Arc<Context>,
    mut reader: tokio::io::BufReader<R>,
    watchdog: Arc<DanserWatchdog>,
) -> Vec<String> {
    let mut buf = [0u8; 1024];
    let mut text_buffer = String::new();
    let mut tail: VecDeque<String> = VecDeque::with_capacity(20);
    let mut last_progress: Option<u8> = None;

    while let Ok(n) = reader.read(&mut buf).await {
        if n == 0 {
            break;
        }

        text_buffer.push_str(&String::from_utf8_lossy(&buf[..n]));

        let Some(last_sep) = text_buffer.rfind(['\r', '\n']) else {
            continue;
        };

        for line in text_buffer[..last_sep].split(['\r', '\n']) {
            push_danser_line(&mut tail, &watchdog, line);
            if let Some(progress) = parse_danser_progress(line) {
                watchdog.mark_progress();
                if last_progress != Some(progress) {
                    last_progress = Some(progress);
                    ctx.replay_queue
                        .set_status(ReplayStatus::Rendering(progress))
                        .await;
                }
            }
        }

        text_buffer.drain(..=last_sep);
    }

    push_danser_line(&mut tail, &watchdog, &text_buffer);
    tail.into_iter().collect()
}

pub(crate) async fn terminate_danser_group(child: &mut Child, pgid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = child.id().or(pgid) {
        if pid > 1 {
            let pgid_arg = format!("-{pid}");
            let _ = tokio::task::spawn_blocking(move || {
                let _ = std::process::Command::new("kill")
                    .args(["-s", "KILL", "--", &pgid_arg])
                    .status();
            })
            .await;
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn drain_or_abort_one<T>(mut handle: JoinHandle<T>) -> Option<T> {
    match tokio::time::timeout(Duration::from_secs(2), &mut handle).await {
        Ok(Ok(val)) => Some(val),
        Ok(Err(_)) => None,
        Err(_) => {
            handle.abort();
            None
        }
    }
}

async fn drain_or_abort_stdio<T>(
    out_h: JoinHandle<T>,
    err_h: JoinHandle<T>,
) -> (Option<T>, Option<T>) {
    tokio::join!(drain_or_abort_one(out_h), drain_or_abort_one(err_h))
}

pub(crate) async fn cleanup_danser_intermediate(replays_dir: impl AsRef<Path>, filename: &str) {
    let replays_dir = replays_dir.as_ref();
    let _ = tokio::fs::remove_file(replays_dir.join(format!("{filename}.mp4"))).await;
    let _ = tokio::fs::remove_dir_all(replays_dir.join(format!("{filename}_temp"))).await;
}

pub(crate) async fn cleanup_danser_outputs(
    replays_dir: impl AsRef<Path>,
    filename: &str,
    osr_path: &Path,
) {
    cleanup_danser_intermediate(replays_dir, filename).await;
    let _ = tokio::fs::remove_file(osr_path).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_fatal_danser_line_detects_crashes_and_ignores_song_titles() {
        assert!(is_fatal_danser_line(
            "2026/11/18 12:36:48 ffmpeg's video process finished abruptly! Please check if you have enough storage"
        ));
        assert!(is_fatal_danser_line(
            "2026/11/18 12:36:48 ffmpeg's audio process failed to start! Error: exit status 1"
        ));
        assert!(is_fatal_danser_line(
            "panic: runtime error: invalid memory address"
        ));
        assert!(is_fatal_danser_line(
            "[h264_nvenc @ 0x55a1234] OpenEncodeSessionEx failed: out of memory"
        ));
        assert!(is_fatal_danser_line("CUDA_ERROR_OUT_OF_MEMORY"));
        assert!(is_fatal_danser_line(
            "2026/11/23 22:06:05 Beatmap not found, closing..."
        ));
        assert!(!should_retry_low_vram(
            "2026/11/23 22:06:05 Beatmap not found, closing..."
        ));

        assert!(!is_fatal_danser_line(
            "2026/11/18 17:21:44 Playing: Panic! At The Disco - Out of Memory [Finished Abruptly]"
        ));
        assert!(!is_fatal_danser_line(
            "2026/11/18 17:22:17 Progress: 50%, Speed: 3.50x, ETA: 31s"
        ));
        assert!(!is_fatal_danser_line("   "));
    }

    #[test]
    fn test_build_danser_ld_library_path_includes_ffmpeg_lib() {
        let ld = build_danser_ld_library_path(Path::new("/opt/danser"));
        assert!(
            ld == "/opt/danser:/opt/danser/ffmpeg/lib"
                || ld.starts_with("/opt/danser:/opt/danser/ffmpeg/lib:"),
            "unexpected LD_LIBRARY_PATH: {ld}"
        );
    }

    #[test]
    fn test_should_retry_low_vram() {
        assert!(!should_retry_low_vram("Error: Beatmap not found in Songs"));
        assert!(!should_retry_low_vram("Skin not found: MySkin"));
        assert!(!should_retry_low_vram("Invalid replay header"));
        assert!(should_retry_low_vram("CUDA_ERROR_OUT_OF_MEMORY"));
        assert!(should_retry_low_vram(
            "[h264_nvenc] OpenEncodeSessionEx failed"
        ));
    }

    #[test]
    fn test_parse_danser_progress() {
        assert_eq!(
            parse_danser_progress("2026/11/24 17:15:35 Progress: 44%, Speed: 6.69x, ETA: 25s"),
            Some(44)
        );
        assert_eq!(
            parse_danser_progress("2026/11/24 17:15:56 Progress: 99%, Speed: 7.49x, ETA: 0s"),
            Some(99)
        );
        assert_eq!(
            parse_danser_progress("Progress: 44.6%, Speed: 5.0x"),
            Some(45)
        );
        assert_eq!(
            parse_danser_progress("Progress: 105%"),
            Some(100)
        );
        assert_eq!(parse_danser_progress("Playing: Something"), None);
    }

    #[test]
    fn test_parse_loaded_skin_line() {
        assert_eq!(
            parse_loaded_skin_line("2026/11/23 23:39:31 SkinManager: Skin \"default\" loaded."),
            Some("default".to_owned())
        );
        assert_eq!(
            parse_loaded_skin_line(
                "2026/11/23 23:39:32 SkinManager: Skin \"%E2%80%B9+Spodai\" loaded."
            ),
            Some("%E2%80%B9+Spodai".to_owned())
        );
        assert_eq!(
            parse_loaded_skin_line("2026/11/23 23:39:31 SkinManager: Loading skin: foo"),
            None
        );
    }
}
