use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::AtomicBool,
};

use tokio::{process::Command, sync::Notify, time::Duration};

use super::danser::run_cancellable;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdaptiveRenderPlan {
    pub video_kbps: u32,
    pub maxrate_kbps: u32,
    pub bufsize_kbps: u32,
    pub audio_kbps: u32,
    pub width: i32,
    pub height: i32,
    pub fps: i32,
}

pub(crate) fn adaptive_discord_max_mb() -> u64 {
    std::env::var("ADAPTIVE_DISCORD_MAX_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(460)
}

pub(crate) fn show_pp_select_menu() -> bool {
    std::env::var("SHOW_PP_SELECT_MENU")
        .ok()
        .is_some_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

pub(crate) fn pp_select_timeout_secs() -> u64 {
    std::env::var("PP_SELECT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(crate::core::events::interaction::COMPONENT_IDLE_TIMEOUT_SECS)
}

pub(crate) fn estimate_rendered_duration_secs(
    first_obj_ms: f64,
    last_obj_ms: f64,
    mods: u32,
    start_sec: u32,
    end_sec: u32,
    skip_intro: bool,
) -> f64 {
    let start_ms = if start_sec > 0 {
        f64::from(start_sec) * 1000.0
    } else if skip_intro {
        (first_obj_ms - 2000.0).max(0.0)
    } else {
        0.0
    };
    let end_ms = if end_sec > 0 {
        let end_target = f64::from(end_sec) * 1000.0;
        if last_obj_ms > 0.0 {
            end_target.min(last_obj_ms)
        } else {
            end_target
        }
    } else {
        last_obj_ms
    };
    let raw_secs = ((end_ms - start_ms) / 1000.0).max(0.0);
    if raw_secs <= 0.0 {
        return 0.0;
    }
    let clock_rate = if (mods & (64 | 512)) != 0 {
        1.5
    } else if (mods & 256) != 0 {
        0.75
    } else {
        1.0
    };
    (raw_secs / clock_rate) + 12.0
}

fn scale_down_even(bw: i32, bh: i32, max_w: i32, max_h: i32) -> (i32, i32) {
    if bw <= max_w && bh <= max_h {
        return ((bw & !1).max(2), (bh & !1).max(2));
    }
    let rw = f64::from(max_w) / f64::from(bw);
    let rh = f64::from(max_h) / f64::from(bh);
    let scale = rw.min(rh);
    let w = (((f64::from(bw) * scale).round() as i32) & !1).max(2);
    let h = (((f64::from(bh) * scale).round() as i32) & !1).max(2);
    (w, h)
}

fn nvenc_rate_window(total_kbps: f64, audio_kbps: u32) -> (u32, u32, u32) {
    let raw_video_kbps = total_kbps - f64::from(audio_kbps);
    let video_kbps = (raw_video_kbps.floor() as i64).clamp(300, 3500) as u32;
    let maxrate_kbps = ((f64::from(video_kbps) * 1.15).round() as u32).max(video_kbps);
    let bufsize_kbps = video_kbps.saturating_mul(2);
    (video_kbps, maxrate_kbps, bufsize_kbps)
}

pub(crate) fn compute_adaptive_render_plan(
    rendered_secs: f64,
    target_mb: u64,
    base_width: i32,
    base_height: i32,
    base_fps: i32,
) -> Option<AdaptiveRenderPlan> {
    if target_mb == 0 || !rendered_secs.is_finite() || rendered_secs <= 0.0 {
        return None;
    }
    let total_kbps = ((target_mb as f64) * 8_000.0 / rendered_secs) * 0.96;
    let audio_kbps = if total_kbps >= 2000.0 {
        192
    } else if total_kbps >= 1100.0 {
        160
    } else {
        128
    };
    if total_kbps - f64::from(audio_kbps) >= 3500.0 {
        return None;
    }

    let (video_kbps, maxrate_kbps, bufsize_kbps) = nvenc_rate_window(total_kbps, audio_kbps);

    let bw = base_width.max(640);
    let bh = base_height.max(360);
    let bfps = base_fps.max(24);

    let (width, height, fps) = if video_kbps >= 1800 {
        ((bw & !1).max(2), (bh & !1).max(2), bfps)
    } else if video_kbps >= 850 {
        let (w, h) = scale_down_even(bw, bh, 1280, 720);
        (w, h, bfps.min(60))
    } else {
        let (w, h) = scale_down_even(bw, bh, 1280, 720);
        (w, h, bfps.min(30))
    };

    Some(AdaptiveRenderPlan {
        video_kbps,
        maxrate_kbps,
        bufsize_kbps,
        audio_kbps,
        width,
        height,
        fps,
    })
}

#[cfg(test)]
pub(crate) fn build_danser_spatch(
    plan: Option<&AdaptiveRenderPlan>,
    low_vram_nvenc: bool,
) -> Option<String> {
    build_danser_spatch_with_quality(plan, low_vram_nvenc, None)
}

pub(crate) fn build_danser_spatch_with_quality(
    plan: Option<&AdaptiveRenderPlan>,
    low_vram_nvenc: bool,
    forced_quality: Option<crate::core::settings::RenderQuality>,
) -> Option<String> {
    let Some(p) = plan else {
        if !low_vram_nvenc && forced_quality.is_none() {
            return None;
        }
        let mut recording = serde_json::Map::new();
        if let Some(quality) = forced_quality {
            let (w, h, fps) = quality.dimensions();
            recording.insert("FrameWidth".to_owned(), w.into());
            recording.insert("FrameHeight".to_owned(), h.into());
            recording.insert("FPS".to_owned(), fps.into());
        }
        if low_vram_nvenc {
            recording.insert("Encoder".to_owned(), "h264_nvenc".into());
            recording.insert(
                "h264_nvenc".to_owned(),
                serde_json::json!({
                    "Preset": "p4",
                    "AdditionalOptions": "-movflags +faststart",
                }),
            );
        }
        return Some(serde_json::json!({ "Recording": recording }).to_string());
    };
    let add_opts = if low_vram_nvenc {
        format!(
            "-movflags +faststart -maxrate {}k -bufsize {}k",
            p.maxrate_kbps, p.bufsize_kbps
        )
    } else {
        format!(
            "-movflags +faststart -maxrate {}k -bufsize {}k -bf 3 -b_ref_mode middle -spatial_aq 1 -temporal_aq 1 -aq-strength 6 -rc-lookahead 20",
            p.maxrate_kbps, p.bufsize_kbps
        )
    };
    let val = serde_json::json!({
        "Recording": {
            "FrameWidth": p.width,
            "FrameHeight": p.height,
            "FPS": p.fps,
            "Encoder": "h264_nvenc",
            "AudioCodec": "aac",
            "h264_nvenc": {
                "RateControl": "vbr",
                "Bitrate": format!("{}k", p.video_kbps),
                "Preset": if low_vram_nvenc { "p4" } else { "p5" },
                "AdditionalOptions": add_opts,
            },
            "aac": {
                "Bitrate": format!("{}k", p.audio_kbps),
            },
        }
    });
    Some(val.to_string())
}

async fn probe_mp4_duration_secs(
    danser_dir: &Path,
    ld_library_path: &str,
    mp4_path: &Path,
) -> Option<f64> {
    let bundled = danser_dir.join("ffmpeg/bin/ffprobe");
    let ffprobe_bin: PathBuf = if tokio::fs::try_exists(&bundled).await.unwrap_or(false) {
        bundled
    } else if tokio::fs::try_exists("/usr/lib/jellyfin-ffmpeg/ffprobe")
        .await
        .unwrap_or(false)
    {
        PathBuf::from("/usr/lib/jellyfin-ffmpeg/ffprobe")
    } else {
        PathBuf::from("ffprobe")
    };

    let output = tokio::time::timeout(
        Duration::from_secs(15),
        Command::new(ffprobe_bin)
            .env("LD_LIBRARY_PATH", ld_library_path)
            .kill_on_drop(true)
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(mp4_path)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let dur = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .ok()?;
    (dur.is_finite() && dur > 0.0).then_some(dur)
}

pub(crate) async fn clamp_oversized_mp4(
    danser_dir: &Path,
    ld_library_path: &str,
    mp4_path: &Path,
    fallback_secs: f64,
    orig_bytes: u64,
    cancelled: &AtomicBool,
    cancel_notify: &Notify,
) -> bool {
    let dur_secs = probe_mp4_duration_secs(danser_dir, ld_library_path, mp4_path)
        .await
        .unwrap_or(fallback_secs);
    if dur_secs <= 0.0 {
        warn!(
            orig_bytes,
            "Output MP4 exceeded 485 MiB but duration is unknown; keeping original"
        );
        return false;
    }

    let total_kbps = (450.0 * 8_000.0 / dur_secs) * 0.96;
    let (video_kbps, maxrate_kbps, bufsize_kbps) = nvenc_rate_window(total_kbps, 128);

    let bundled_ffmpeg = danser_dir.join("ffmpeg/bin/ffmpeg");
    let ffmpeg_bin: PathBuf = if tokio::fs::try_exists(&bundled_ffmpeg).await.unwrap_or(false) {
        bundled_ffmpeg
    } else {
        PathBuf::from("ffmpeg")
    };

    let clamp_path = mp4_path.with_extension("clamp.mp4");
    let _ = tokio::fs::remove_file(&clamp_path).await;

    warn!(
        orig_bytes,
        dur_secs,
        video_kbps,
        "Rendered MP4 exceeded 485 MiB safety threshold; running 1-pass NVENC clamp to ~450 MB"
    );

    let mut cmd = Command::new(ffmpeg_bin);
    cmd.env("LD_LIBRARY_PATH", ld_library_path)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .args(["-y", "-nostats", "-loglevel", "error", "-i"])
        .arg(mp4_path)
        .args([
            "-c:v",
            "h264_nvenc",
            "-rc",
            "vbr",
            "-b:v",
            &format!("{video_kbps}k"),
            "-maxrate",
            &format!("{maxrate_kbps}k"),
            "-bufsize",
            &format!("{bufsize_kbps}k"),
            "-profile:v",
            "high",
            "-preset",
            "p5",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-movflags",
            "+faststart",
        ])
        .arg(&clamp_path);

    let Ok(mut child) = cmd.spawn() else {
        warn!("Failed to spawn ffmpeg for post-render size clamp; keeping original");
        return false;
    };

    let stderr_handle = child.stderr.take().map(|mut err| {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt as _;
            let mut buf = Vec::new();
            let _ = (&mut err).take(8192).read_to_end(&mut buf).await;
            String::from_utf8_lossy(&buf).into_owned()
        })
    });

    let Some(wait_res) = run_cancellable(
        cancelled,
        cancel_notify,
        tokio::time::timeout(Duration::from_secs(600), child.wait()),
    )
    .await
    else {
        let _ = child.kill().await;
        let _ = child.wait().await;
        if let Some(h) = stderr_handle {
            h.abort();
        }
        let _ = tokio::fs::remove_file(&clamp_path).await;
        return true;
    };

    let stderr_text = match stderr_handle {
        Some(h) => h.await.unwrap_or_default(),
        None => String::new(),
    };

    match wait_res {
        Ok(Ok(status)) if status.success() => {
            if let Ok(meta) = tokio::fs::metadata(&clamp_path).await {
                if meta.len() > 0 && meta.len() < orig_bytes {
                    if let Err(err) = tokio::fs::rename(&clamp_path, mp4_path).await {
                        warn!(?err, "Failed to replace MP4 with clamped file");
                        let _ = tokio::fs::remove_file(&clamp_path).await;
                    } else {
                        info!(
                            orig_bytes,
                            clamped_bytes = meta.len(),
                            "Successfully clamped oversized MP4 for Discord embedding"
                        );
                    }
                    return false;
                }
            }
            let _ = tokio::fs::remove_file(&clamp_path).await;
        }
        Ok(Ok(status)) => {
            warn!(%status, stderr = %stderr_text.trim(), "Post-render clamp ffmpeg failed; keeping original");
            let _ = tokio::fs::remove_file(&clamp_path).await;
        }
        Ok(Err(err)) => {
            warn!(?err, "Post-render clamp ffmpeg wait failed; keeping original");
            let _ = tokio::fs::remove_file(&clamp_path).await;
        }
        Err(_) => {
            warn!("Post-render clamp ffmpeg timed out after 600s; keeping original");
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = tokio::fs::remove_file(&clamp_path).await;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_rendered_duration_secs() {
        let nm = estimate_rendered_duration_secs(2000.0, 180_000.0, 0, 0, 0, false);
        assert!((nm - 192.0).abs() < 0.01);

        let dt = estimate_rendered_duration_secs(2000.0, 180_000.0, 64, 0, 0, false);
        assert!((dt - 132.0).abs() < 0.01);

        let ht = estimate_rendered_duration_secs(2000.0, 180_000.0, 256, 0, 0, false);
        assert!((ht - 252.0).abs() < 0.01);

        let trim = estimate_rendered_duration_secs(2000.0, 300_000.0, 0, 60, 120, false);
        assert!((trim - 72.0).abs() < 0.01);

        let skip = estimate_rendered_duration_secs(20_000.0, 180_000.0, 0, 0, 0, true);
        assert!((skip - 174.0).abs() < 0.01);
    }

    #[test]
    fn test_compute_adaptive_render_plan_tiers() {
        assert!(compute_adaptive_render_plan(2000.0, 0, 1920, 1080, 60).is_none());
        assert!(compute_adaptive_render_plan(900.0, 460, 1920, 1080, 60).is_none());

        let p28 = compute_adaptive_render_plan(1720.0, 460, 1920, 1080, 60).expect("28m plan");
        assert_eq!((p28.width, p28.height, p28.fps), (1920, 1080, 60));
        assert_eq!(p28.audio_kbps, 192);
        assert!((1800..=2000).contains(&p28.video_kbps));

        let p45 = compute_adaptive_render_plan(2700.0, 460, 1920, 1080, 60).expect("45m plan");
        assert_eq!((p45.width, p45.height, p45.fps), (1280, 720, 60));
        assert_eq!(p45.audio_kbps, 160);
        assert!((850..1800).contains(&p45.video_kbps));

        let p75 = compute_adaptive_render_plan(4500.0, 460, 1920, 1080, 60).expect("75m plan");
        assert_eq!((p75.width, p75.height, p75.fps), (1280, 720, 30));
        assert_eq!(p75.audio_kbps, 128);
        assert!(p75.video_kbps < 850);

        let custom = compute_adaptive_render_plan(4500.0, 460, 1024, 768, 48).expect("custom plan");
        assert!(custom.width <= 1024 && custom.height <= 720);
        assert_eq!(custom.width % 2, 0);
        assert_eq!(custom.height % 2, 0);
        assert_eq!(custom.fps, 30);
    }

    #[test]
    fn test_build_danser_spatch_json_and_low_vram() {
        assert!(build_danser_spatch(None, false).is_none());
        let lv = build_danser_spatch(None, true).unwrap();
        let lv_json: serde_json::Value = serde_json::from_str(&lv).unwrap();
        assert_eq!(lv_json["Recording"]["h264_nvenc"]["Preset"], "p4");

        let plan = compute_adaptive_render_plan(1720.0, 460, 1920, 1080, 60).unwrap();
        let normal_str = build_danser_spatch(Some(&plan), false).unwrap();
        let v: serde_json::Value = serde_json::from_str(&normal_str).unwrap();
        assert_eq!(v["Recording"]["FrameWidth"], 1920);
        assert_eq!(v["Recording"]["FrameHeight"], 1080);
        assert_eq!(v["Recording"]["h264_nvenc"]["RateControl"], "vbr");
        assert_eq!(
            v["Recording"]["h264_nvenc"]["Bitrate"],
            format!("{}k", plan.video_kbps)
        );
        assert!(v["Recording"]["h264_nvenc"]["AdditionalOptions"]
            .as_str()
            .unwrap()
            .contains("-spatial_aq 1"));

        let both_str = build_danser_spatch(Some(&plan), true).unwrap();
        let vb: serde_json::Value = serde_json::from_str(&both_str).unwrap();
        assert_eq!(vb["Recording"]["Preset"], serde_json::Value::Null);
        assert_eq!(vb["Recording"]["h264_nvenc"]["Preset"], "p4");
        assert!(!vb["Recording"]["h264_nvenc"]["AdditionalOptions"]
            .as_str()
            .unwrap()
            .contains("-rc-lookahead"));

        let locked_720 = build_danser_spatch_with_quality(
            None,
            false,
            Some(crate::core::settings::RenderQuality::Hd720p60),
        )
        .unwrap();
        let vl: serde_json::Value = serde_json::from_str(&locked_720).unwrap();
        assert_eq!(vl["Recording"]["FrameWidth"], 1280);
        assert_eq!(vl["Recording"]["FrameHeight"], 720);
        assert_eq!(vl["Recording"]["FPS"], 60);

        let locked_900 = build_danser_spatch_with_quality(
            None,
            false,
            Some(crate::core::settings::RenderQuality::HdPlus900p60),
        )
        .unwrap();
        let v9: serde_json::Value = serde_json::from_str(&locked_900).unwrap();
        assert_eq!(v9["Recording"]["FrameWidth"], 1600);
        assert_eq!(v9["Recording"]["FrameHeight"], 900);
        assert_eq!(v9["Recording"]["FPS"], 60);

        let locked_1080 = build_danser_spatch_with_quality(
            None,
            false,
            Some(crate::core::settings::RenderQuality::FullHd1080p60),
        )
        .unwrap();
        let v10: serde_json::Value = serde_json::from_str(&locked_1080).unwrap();
        assert_eq!(v10["Recording"]["FrameWidth"], 1920);
        assert_eq!(v10["Recording"]["FrameHeight"], 1080);
        assert_eq!(v10["Recording"]["FPS"], 60);
    }
}
