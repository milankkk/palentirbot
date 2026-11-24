use std::{
    ffi::OsStr,
    path::Path,
    sync::{atomic::Ordering, Arc},
    time::{SystemTime, UNIX_EPOCH},
};

use eyre::{Context as _, Report, Result};
use rosu_v2::prelude::{GameModsLegacy, Grade};
use twilight_model::{
    channel::message::MessageFlags,
    id::{
        marker::{ChannelMarker, MessageMarker, UserMarker},
        Id,
    },
};

use crate::{
    core::{BotConfig, Context, ReplayStatus},
    util::{
        builder::{action_row, link_button, MessageBuilder},
        ChannelExt, MessageExt,
    },
};

pub(crate) use super::{
    danser::{build_danser_ld_library_path, read_danser_log_tail},
    embed::{
        analyze_replay_for_info, format_pp_field, pp_version_label, spawn_pp_select_expiry,
        ReplayInfoEmbed,
    },
    mapset::extract_zip_safely,
    md5::md5_hex,
};

use super::{
    adaptive::{
        adaptive_discord_max_mb, build_danser_spatch_with_quality, clamp_oversized_mp4,
        compute_adaptive_render_plan, estimate_rendered_duration_secs,
    },
    cache::{build_render_cache_key, CachedRender},
    danser::{
        cleanup_danser_intermediate, cleanup_danser_outputs, run_cancellable, run_danser_attempt,
        should_retry_low_vram, DanserAttemptOutcome,
    },
    embed::build_replay_info_embed,
    mapset::{
        resolve_and_ensure_beatmap, sweep_stale_temp_dirs, BeatmapResolveError, ResolvedBeatmap,
    },
    pp::create_title,
    ReplayData, ReplayQueue, WebJobHandle,
};

impl ReplayQueue {
    pub fn process(ctx: Arc<Context>) {
        tokio::spawn(Self::async_process(ctx));
    }

    async fn async_process(ctx: Arc<Context>) -> Result<()> {
        let config = BotConfig::get();
        sweep_stale_temp_dirs(&config.paths.songs()).await;
        sweep_stale_temp_dirs(&config.paths.skins()).await;
        let danser_path = config.paths.danser().join("danser");
        if !tokio::fs::try_exists(&danser_path).await.unwrap_or(false) {
            warn!("danser binary not found at {danser_path:?}");
        }

        loop {
            let data = ctx.replay_queue.peek().await;
            let replay_path = data.path.clone();
            let queue_message = data.queue_message;
            let web_handle = data.web.clone();
            let ctx_job = Arc::clone(&ctx);
            let danser_job_path = danser_path.clone();

            // Run each job in an isolated spawned task so a panic in any replay/parser can never
            // kill the queue worker loop and stall subsequent renders.
            let outcome = tokio::spawn(async move {
                process_single_replay(ctx_job, data, &danser_job_path).await
            })
            .await;

            let failed = match outcome {
                Ok(Ok(())) => false,
                Ok(Err(err)) => {
                    warn!("Replay processing failed: {err:?}");
                    true
                }
                Err(join_err) => {
                    error!("Replay processing task panicked ({join_err:?}); recovering queue worker");
                    true
                }
            };

            if failed {
                if let Some((msg_id, channel_id)) = queue_message {
                    let _ = ctx.http.delete_message(channel_id, msg_id).await;
                }
                let _ = tokio::fs::remove_file(&replay_path).await;
                if let Some(ref w) = web_handle {
                    let mut st = w.state.write().await;
                    if !st.is_terminal() {
                        st.status = "error".to_owned();
                        st.error = Some("Render failed unexpectedly. Please try again.".to_owned());
                        st.finished_at = Some(time::OffsetDateTime::now_utc().unix_timestamp());
                    }
                }
            }

            if let Some(ref w) = web_handle {
                let _ = tokio::fs::remove_file(&w.settings_path).await;
            }

            ctx.replay_queue.reset_peek().await;
        }
    }
}

fn sanitize_output_segment(s: &str) -> String {
    let mut res = String::with_capacity(s.len());
    let mut last_was_underscore = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            res.push(c);
            last_was_underscore = false;
        } else if c == '-' {
            res.push('-');
            last_was_underscore = false;
        } else if !last_was_underscore {
            res.push('_');
            last_was_underscore = true;
        }
    }
    res.trim_matches(|c| c == '_' || c == '-').to_owned()
}

async fn process_single_replay(
    ctx: Arc<Context>,
    data: ReplayData,
    danser_path: &Path,
) -> Result<()> {
    let ReplayData {
        job_id: _,
        input_channel,
        pitch,
        skin,
        skip_intro,
        offset_ms,
        force_rerender,
        skip_info_embed,
        path,
        replay,
        time_points,
        user,
        title: _,
        queue_message,
        is_lazer,
        cancelled,
        cancel_notify,
        web,
    } = data;

    if cancelled.load(Ordering::Acquire) {
        if let Some((msg_id, channel_id)) = queue_message {
            let _ = ctx.http.delete_message(channel_id, msg_id).await;
        }
        let _ = tokio::fs::remove_file(&path).await;
        if let Some(ref w) = web {
            mark_web_cancelled(w).await;
        }
        return Ok(());
    }

    let replay_hash = replay.replay_hash.as_deref().unwrap_or("");
    let beatmap_hash = replay.beatmap_hash.as_deref();

    let ResolvedBeatmap {
        mapset_id,
        map_id,
        map_version,
        mapset_title,
        osu_path,
        osu_bytes,
    } = match resolve_and_ensure_beatmap(&ctx, beatmap_hash, true, Some((&cancelled, &cancel_notify)))
        .await
    {
        Ok(resolved) => resolved,
        Err(BeatmapResolveError::Cancelled) => {
            return handle_cancelled_cleanup(
                &ctx,
                queue_message,
                &path,
                None,
                input_channel,
                user,
                web.as_deref(),
            )
            .await;
        }
        Err(BeatmapResolveError::Failure { user_msg, report }) => {
            report_render_error(&ctx, input_channel, web.as_deref(), user_msg).await;
            return Err(report);
        }
    };

    let config = BotConfig::get();
    let (settings, active_settings_path) = if let Some(ref w) = web {
        (w.settings_name.clone(), w.settings_path.clone())
    } else {
        crate::commands::danser::settings::resolve_user_or_default_settings(user).await
    };
    let settings_bytes = tokio::fs::read(&active_settings_path).await.unwrap_or_default();
    let parsed_user_settings =
        serde_json::from_slice::<crate::core::settings::DanserSettings>(&settings_bytes).ok();
    let effective_skip_intro = skip_intro
        || parsed_user_settings
            .as_ref()
            .is_some_and(|s| s.gameplay.always_skip_intro);
    let effective_quality = if web.is_some() {
        // Web renders already validated osu! session entitlement and wrote the target profile.
        parsed_user_settings
            .as_ref()
            .map(crate::core::settings::DanserSettings::current_render_quality)
            .unwrap_or(crate::core::settings::RenderQuality::Hd720p60)
    } else {
        // Discord renders unlock 900p60 when the user has linked their osu! account via /link;
        // unlinked users default to 720p60.
        let discord_linked = ctx
            .psql()
            .get_linked_osu_user(user)
            .await
            .ok()
            .flatten()
            .is_some();
        let desired = parsed_user_settings
            .as_ref()
            .map(crate::core::settings::DanserSettings::current_render_quality)
            .unwrap_or(crate::core::settings::RenderQuality::HdPlus900p60);
        desired.cap_for_entitlement(discord_linked)
    };
    let (base_width, base_height, base_fps) = effective_quality.dimensions();
    let forced_quality_patch = if parsed_user_settings
        .as_ref()
        .is_some_and(|s| s.matches_render_quality(effective_quality))
    {
        None
    } else {
        Some(effective_quality)
    };
    let effective_skin_override: Option<String> = skin.clone().or_else(|| {
        let s = parsed_user_settings.as_ref()?;
        if s.has_normalized_skin() {
            return None;
        }
        let bare = s
            .skin
            .current_skin
            .trim()
            .rsplit(['/', '\\'])
            .find(|seg| !seg.is_empty())?;
        Some(bare.to_owned())
    });

    let Some(filename) = path.file_stem().and_then(OsStr::to_str) else {
        warn!("replay path `{path:?}` has an unexpected form");
        report_render_error(
            &ctx,
            input_channel,
            web.as_deref(),
            "There was an error resolving the beatmap path",
        )
        .await;
        eyre::bail!("replay path `{path:?}` has an unexpected form");
    };

    let title_for_display = format!("{mapset_title} [{map_version}]");
    let title_result = match create_title(
        &replay,
        osu_path,
        Arc::from(osu_bytes),
        &title_for_display,
        is_lazer,
        !skip_info_embed,
    )
    .await
    {
        Ok(result) => result,
        Err(err) => {
            let err = err.wrap_err("failed to create title");
            warn!("{err:?}");
            report_render_error(
                &ctx,
                input_channel,
                web.as_deref(),
                "There was an error while trying to create the video title",
            )
            .await;
            return Err(err);
        }
    };

    // Store the formatted title early so the queue embed / web status displays it immediately.
    if let Some(front) = ctx.replay_queue.queue.lock().await.front_mut() {
        front.title = Some(title_result.title.clone());
    }
    if let Some(ref w) = web {
        w.state.write().await.title = Some(title_result.title.clone());
    }
    ctx.replay_queue.notify.notify_waiters();

    if cancelled.load(Ordering::Acquire) {
        return handle_cancelled_cleanup(
            &ctx,
            queue_message,
            &path,
            None,
            input_channel,
            user,
            web.as_deref(),
        )
        .await;
    }

    let osr_bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(err) => {
            let err = Report::from(err).wrap_err("failed to read queued .osr file for cache key");
            warn!("{err:?}");
            report_render_error(
                &ctx,
                input_channel,
                web.as_deref(),
                "Failed to read the queued replay file.",
            )
            .await;
            return Err(err);
        }
    };
    let cache_key = build_render_cache_key(
        &osr_bytes,
        beatmap_hash.unwrap_or(""),
        &settings_bytes,
        effective_quality,
        time_points.start,
        time_points.end,
        pitch,
        skin.as_deref(),
        effective_skip_intro,
        offset_ms,
    );

    let cached_hit = if !force_rerender {
        if let Some(ref key) = cache_key {
            if let Some(cached) = ctx.replay_queue.cache.get(key).await {
                let probe_url =
                    derive_mp4_probe_url(&cached.watch_url, cached.video_url.as_deref());
                if ctx.client().check_url_exists(&probe_url).await {
                    info!(key = %key, url = %cached.watch_url, "render cache hit");
                    Some((cached.watch_url, cached.video_url))
                } else {
                    ctx.replay_queue.cache.remove(key).await;
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let (link, video_url, thumb_url, rendered_mp4_path, adaptive_plan, uploaded_size_bytes) =
        if let Some((watch_url, vid_url)) = cached_hit {
            (watch_url, vid_url, None, None, None, None)
        } else {
            let adaptive_target_mb = adaptive_discord_max_mb();
            let estimated_secs = estimate_rendered_duration_secs(
                title_result.map_first_obj_ms,
                title_result.map_last_obj_ms,
                replay.mods,
                time_points.start,
                time_points.end,
                effective_skip_intro,
            );
            let adaptive_plan = compute_adaptive_render_plan(
                estimated_secs,
                adaptive_target_mb,
                base_width,
                base_height,
                base_fps,
            );
            if let Some(ref plan) = adaptive_plan {
                info!(
                    estimated_secs,
                    target_mb = adaptive_target_mb,
                    video_kbps = plan.video_kbps,
                    audio_kbps = plan.audio_kbps,
                    width = plan.width,
                    height = plan.height,
                    fps = plan.fps,
                    "Applying adaptive marathon NVENC bitrate cap"
                );
            }

            let canonical_path = match tokio::fs::canonicalize(&path).await {
                Ok(p) => p,
                Err(err) => {
                    let err = Report::from(err).wrap_err("failed to canonicalize replay path");
                    warn!("{err:?}");
                    report_render_error(
                        &ctx,
                        input_channel,
                        web.as_deref(),
                        "Failed to access the queued replay file on disk.",
                    )
                    .await;
                    return Err(err);
                }
            };

            let ld_library_path = build_danser_ld_library_path(config.paths.danser());
            ctx.replay_queue
                .set_status(ReplayStatus::Rendering(0))
                .await;

            let Some(_gpu_guard) =
                run_cancellable(&cancelled, &cancel_notify, ctx.gpu_lock().lock()).await
            else {
                return handle_cancelled_cleanup(
                    &ctx,
                    queue_message,
                    &path,
                    None,
                    input_channel,
                    user,
                    web.as_deref(),
                )
                .await;
            };

            if cancelled.load(Ordering::Acquire) {
                return handle_cancelled_cleanup(
                    &ctx,
                    queue_message,
                    &path,
                    None,
                    input_channel,
                    user,
                    web.as_deref(),
                )
                .await;
            }

            ctx.ai().unload_loaded_models().await;

            let mut actual_loaded_skin: Option<String> = None;
            for attempt in 1..=2u8 {
                let low_vram_nvenc = attempt == 2;
                let spatch_owned = build_danser_spatch_with_quality(
                    adaptive_plan.as_ref(),
                    low_vram_nvenc,
                    forced_quality_patch,
                );
                let attempt_outcome = run_danser_attempt(
                    &ctx,
                    danser_path,
                    &ld_library_path,
                    &canonical_path,
                    &settings,
                    filename,
                    &time_points,
                    pitch,
                    effective_skin_override.as_deref(),
                    effective_skip_intro,
                    offset_ms,
                    spatch_owned.as_deref(),
                    low_vram_nvenc,
                    attempt,
                    &cancelled,
                    &cancel_notify,
                )
                .await;

                match attempt_outcome {
                    Ok(DanserAttemptOutcome::Success { loaded_skin }) => {
                        actual_loaded_skin = loaded_skin;
                        break;
                    }
                    Ok(DanserAttemptOutcome::Cancelled) => {
                        cleanup_danser_outputs(config.paths.replays(), filename, &path).await;
                        return handle_cancelled_cleanup(
                            &ctx,
                            queue_message,
                            &path,
                            None,
                            input_channel,
                            user,
                            web.as_deref(),
                        )
                        .await;
                    }
                    other => {
                        let danser_log_tail =
                            read_danser_log_tail(config.paths.danser(), 25).await;
                        let (can_retry, user_msg, bail_res) = match other {
                            Ok(DanserAttemptOutcome::TotalTimeout { out_tail, err_tail }) => {
                                warn!(attempt, low_vram_nvenc, ?out_tail, ?err_tail, ?danser_log_tail, "danser render timed out after 1800s");
                                (
                                    false,
                                    "Danser render timed out after 30 minutes.",
                                    Err(eyre::eyre!("danser render timed out after 1800s")),
                                )
                            }
                            Ok(DanserAttemptOutcome::FatalLine { line, out_tail, err_tail }) => {
                                warn!(attempt, low_vram_nvenc, fatal_line = %line, ?out_tail, ?err_tail, ?danser_log_tail, "danser reported fatal error during render");
                                let lower = line.to_ascii_lowercase();
                                let msg = if lower.contains("beatmap not found") {
                                    "Danser could not match the replay's beatmap version (the map was likely updated after this replay was set, or is unsubmitted)."
                                } else {
                                    "Danser encountered a fatal encoding/rendering error. Check logs for details."
                                };
                                (
                                    should_retry_low_vram(&line),
                                    msg,
                                    Err(eyre::eyre!("danser fatal error: {line}")),
                                )
                            }
                            Ok(DanserAttemptOutcome::NonZeroExit { status, out_tail, err_tail }) => {
                                warn!(attempt, low_vram_nvenc, %status, ?out_tail, ?err_tail, ?danser_log_tail, "danser exited with non-zero status");
                                (
                                    true,
                                    "Danser crashed or exited with an error code.",
                                    Err(eyre::eyre!("danser exited with non-zero status: {status}")),
                                )
                            }
                            Ok(DanserAttemptOutcome::Stalled { idle_secs, saw_progress, out_tail, err_tail }) => {
                                warn!(attempt, low_vram_nvenc, idle_secs, saw_progress, ?out_tail, ?err_tail, ?danser_log_tail, "danser render stalled with no output/progress");
                                (
                                    true,
                                    "Danser render stalled with no progress and was aborted.",
                                    Err(eyre::eyre!("danser stalled with no progress for {idle_secs}s")),
                                )
                            }
                            Err(err) => {
                                warn!(attempt, low_vram_nvenc, ?err, ?danser_log_tail, "failed to run danser");
                                cleanup_danser_outputs(config.paths.replays(), filename, &path).await;
                                report_render_error(
                                    &ctx,
                                    input_channel,
                                    web.as_deref(),
                                    "Failed to run danser on the replay",
                                )
                                .await;
                                return Err(err);
                            }
                            Ok(DanserAttemptOutcome::Success { .. } | DanserAttemptOutcome::Cancelled) => unreachable!(),
                        };

                        if can_retry && attempt == 1 {
                            warn!("Attempt 1 failed under full NVENC settings; unloading VRAM and retrying with low-VRAM GPU h264_nvenc settings");
                            cleanup_danser_intermediate(config.paths.replays(), filename).await;
                            ctx.ai().unload_loaded_models().await;
                            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                            ctx.replay_queue
                                .set_status(ReplayStatus::Rendering(0))
                                .await;
                            if cancelled.load(Ordering::Acquire) {
                                cleanup_danser_outputs(config.paths.replays(), filename, &path).await;
                                return handle_cancelled_cleanup(
                                    &ctx,
                                    queue_message,
                                    &path,
                                    None,
                                    input_channel,
                                    user,
                                    web.as_deref(),
                                )
                                .await;
                            }
                            continue;
                        }

                        cleanup_danser_outputs(config.paths.replays(), filename, &path).await;
                        report_render_error(&ctx, input_channel, web.as_deref(), user_msg).await;
                        return bail_res;
                    }
                }
            }

            let old_filepath = config.paths.replays().join(format!("{filename}.mp4"));
            let out_meta = match tokio::fs::metadata(&old_filepath).await {
                Ok(meta) if meta.len() > 0 => meta,
                Ok(_) => {
                    drop(_gpu_guard);
                    cleanup_danser_outputs(config.paths.replays(), filename, &path).await;
                    report_render_error(
                        &ctx,
                        input_channel,
                        web.as_deref(),
                        "Danser exited without writing any video output.",
                    )
                    .await;
                    eyre::bail!("danser produced an empty output file at {old_filepath:?}");
                }
                Err(err) => {
                    drop(_gpu_guard);
                    cleanup_danser_outputs(config.paths.replays(), filename, &path).await;
                    report_render_error(
                        &ctx,
                        input_channel,
                        web.as_deref(),
                        "Danser exited without creating the output video file.",
                    )
                    .await;
                    return Err(err).context("danser output mp4 missing after exit 0");
                }
            };

            if adaptive_target_mb > 0 && out_meta.len() > 485 * 1024 * 1024 {
                let cancelled_clamp = clamp_oversized_mp4(
                    config.paths.danser(),
                    &ld_library_path,
                    &old_filepath,
                    estimated_secs,
                    out_meta.len(),
                    &cancelled,
                    &cancel_notify,
                )
                .await;
                if cancelled_clamp || cancelled.load(Ordering::Acquire) {
                    drop(_gpu_guard);
                    return handle_cancelled_cleanup(
                        &ctx,
                        queue_message,
                        &path,
                        Some(&old_filepath),
                        input_channel,
                        user,
                        web.as_deref(),
                    )
                    .await;
                }
            }
            drop(_gpu_guard);

            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let stars_str = format!("{:.2}", title_result.stars);
            let player_str = replay.player_name.as_deref().unwrap_or("Unknown");
            let acc_str = format!("{:.2}", replay.accuracy());
            let legacy_mods = GameModsLegacy::from_bits(replay.mods);
            let mods_str = if legacy_mods.is_empty() {
                "NM".to_owned()
            } else {
                legacy_mods.to_string()
            };
            let hash_prefix: String = replay_hash.chars().take(8).collect();
            let safe_hash_prefix = sanitize_output_segment(&hash_prefix);
            let raw_safe_filename = format!(
                "{}-{}-{}-{}-{}_{}_{}",
                sanitize_output_segment(&stars_str),
                sanitize_output_segment(player_str),
                sanitize_output_segment(&title_for_display),
                sanitize_output_segment(&mods_str),
                sanitize_output_segment(&acc_str),
                if safe_hash_prefix.is_empty() { "nohash" } else { &safe_hash_prefix },
                timestamp
            );
            let safe_filename = clamp_utf8_bytes(&raw_safe_filename, 160);
            let mut new_filepath = config.paths.replays().join(format!("{safe_filename}.mp4"));
            if let Err(err) = tokio::fs::rename(&old_filepath, &new_filepath).await {
                tracing::debug!("Failed to rename MP4 file: {err:?}");
                new_filepath = old_filepath;
            }
            let local_size_bytes = tokio::fs::metadata(&new_filepath).await.ok().map(|m| m.len());

            info!("Started upload to server");
            ctx.replay_queue
                .set_status(ReplayStatus::Uploading)
                .await;
            let beatmap_link = if map_id > 0 {
                format!("https://osu.ppy.sh/b/{map_id}")
            } else {
                format!("https://osu.ppy.sh/beatmapsets/{mapset_id}")
            };
            let upload_hash = if replay_hash.is_empty() {
                "nohash"
            } else {
                replay_hash
            };
            let active_pp = title_result.pp_latest.unwrap_or(title_result.pp_v4);
            let grade_str = match replay.grade {
                Grade::XH => "SSH",
                Grade::X => "SS",
                Grade::SH => "SH",
                Grade::S => "S",
                Grade::A => "A",
                Grade::B => "B",
                Grade::C => "C",
                Grade::D => "D",
                Grade::F => "F",
            };
            let configured_skin = effective_skin_override
                .as_deref()
                .or_else(|| {
                    parsed_user_settings
                        .as_ref()
                        .map(|s| s.skin.current_skin.as_str())
                })
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .and_then(|s| Path::new(s).file_name().and_then(OsStr::to_str).or(Some(s)));
            if let (Some(expected), Some(actual)) = (configured_skin, actual_loaded_skin.as_deref()) {
                if !expected.eq_ignore_ascii_case(actual) {
                    warn!(
                        expected_skin = expected,
                        actual_skin = actual,
                        "danser loaded a different skin than configured; recording actual loaded skin"
                    );
                }
            }
            let resolved_skin = actual_loaded_skin
                .as_deref()
                .or(configured_skin)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .and_then(|s| Path::new(s).file_name().and_then(OsStr::to_str).or(Some(s)));
            let finite_opt = |v: f64| v.is_finite().then_some((v * 100.0).round() / 100.0);
            let upload_meta_json = serde_json::json!({
                "player": player_str,
                "map_title": &title_for_display,
                "stars": finite_opt(title_result.stars),
                "mods": &mods_str,
                "mods_bits": replay.mods,
                "accuracy": f64::from(replay.accuracy()),
                "grade": grade_str,
                "score": replay.score,
                "max_combo": replay.max_combo,
                "map_max_combo": title_result.max_possible_combo,
                "count_300": replay.count_300,
                "count_100": replay.count_100,
                "count_50": replay.count_50,
                "count_miss": replay.count_miss,
                "pp": finite_opt(active_pp.pp),
                "nochoke_pp": finite_opt(active_pp.nochoke_pp),
                "max_pp": finite_opt(active_pp.max_pp),
                "aim_pp": active_pp.aim_pp.and_then(finite_opt),
                "speed_pp": active_pp.speed_pp.and_then(finite_opt),
                "acc_pp": active_pp.acc_pp.and_then(finite_opt),
                "beatmap_id": (map_id > 0).then_some(map_id),
                "mapset_id": (mapset_id > 0).then_some(mapset_id),
                "skin": resolved_skin,
                "is_lazer": is_lazer,
                "play_timestamp": replay.timestamp,
            })
            .to_string();

            let Some(upload_res) = run_cancellable(
                &cancelled,
                &cancel_notify,
                ctx.client().upload_video(
                    &title_result.title,
                    user,
                    &new_filepath,
                    &beatmap_link,
                    upload_hash,
                    Some(&upload_meta_json),
                ),
            )
            .await
            else {
                return handle_cancelled_cleanup(
                    &ctx,
                    queue_message,
                    &path,
                    Some(&new_filepath),
                    input_channel,
                    user,
                    web.as_deref(),
                )
                .await;
            };

            let (uploaded_link, uploaded_video_url, uploaded_thumb_url, uploaded_size_bytes) =
                match upload_res {
                    Ok(res) if res.error != 0 => {
                        let err = format!("failed to upload: `{}`", res.text);
                        warn!("{err}");
                        report_render_error(&ctx, input_channel, web.as_deref(), &err).await;
                        let _ = tokio::fs::remove_file(&new_filepath).await;
                        eyre::bail!("upload server returned error {}", res.error);
                    }
                    Ok(res) => (
                        res.text,
                        res.video_url,
                        res.thumbnail_url,
                        res.size_bytes.or(local_size_bytes),
                    ),
                    Err(err) => {
                        let err = err.wrap_err("failed to upload file");
                        warn!("{err:?}");
                        report_render_error(
                            &ctx,
                            input_channel,
                            web.as_deref(),
                            "Failed to upload file",
                        )
                        .await;
                        let _ = tokio::fs::remove_file(&new_filepath).await;
                        return Err(err);
                    }
                };

            info!("Finished upload to server");
            if let Some(ref key) = cache_key {
                let created_at = time::OffsetDateTime::now_utc().unix_timestamp();
                ctx.replay_queue
                    .cache
                    .insert(
                        key.clone(),
                        CachedRender {
                            watch_url: uploaded_link.clone(),
                            video_url: uploaded_video_url.clone(),
                            created_at,
                        },
                    )
                    .await;
            }

            if cancelled.load(Ordering::Acquire) {
                return handle_cancelled_cleanup(
                    &ctx,
                    queue_message,
                    &path,
                    Some(&new_filepath),
                    input_channel,
                    user,
                    web.as_deref(),
                )
                .await;
            }

            ctx.replay_queue.notify.notify_waiters();
            (
                uploaded_link,
                uploaded_video_url,
                uploaded_thumb_url,
                Some(new_filepath),
                adaptive_plan,
                uploaded_size_bytes,
            )
        };

    let watch_url = link.strip_suffix(".mp4").unwrap_or(&link).to_owned();

    if cancelled.load(Ordering::Acquire) {
        return handle_cancelled_cleanup(
            &ctx,
            queue_message,
            &path,
            rendered_mp4_path.as_deref(),
            input_channel,
            user,
            web.as_deref(),
        )
        .await;
    }

    if let Some(ref w) = web {
        let watch_path = watch_url
            .find("/watch/")
            .map(|idx| watch_url[idx..].to_owned())
            .unwrap_or_else(|| watch_url.clone());
        let thumb = thumb_url.or_else(|| {
            watch_path
                .strip_prefix("/watch/")
                .map(|slug| format!("/{slug}.jpg"))
        });
        let mut st = w.state.write().await;
        st.status = "done".to_owned();
        st.progress = 100;
        st.title = Some(title_result.title.clone());
        st.watch_url = Some(watch_url.clone());
        st.watch_path = Some(watch_path);
        st.video_url = video_url.clone();
        st.thumbnail_url = thumb;
        st.finished_at = Some(time::OffsetDateTime::now_utc().unix_timestamp());
    } else {
        let mut link_buttons = vec![link_button("Watch Replay", &watch_url)];
        if let Some(ref vid_url) = video_url {
            if vid_url.starts_with("http") {
                link_buttons.push(link_button("Download MP4", vid_url));
            }
        }
        let link_buttons_row = action_row(link_buttons);

        let mut link_row_for_msg = Some(link_buttons_row);
        if !skip_info_embed {
            let info = build_replay_info_embed(&replay, map_id, &title_result, adaptive_plan.as_ref());
            let row = link_row_for_msg.take().expect("link row present");
            let (components, expiry_components) = match info.pp_select_row {
                Some(pp_row) => (vec![pp_row, row.clone()], Some(vec![row])),
                None => (vec![row], None),
            };

            let embed_builder = MessageBuilder::new().embed(info.embed).components(components);
            match input_channel.create_message(&ctx, &embed_builder).await {
                Ok(msg) => {
                    if let Some(remaining) = expiry_components {
                        spawn_pp_select_expiry(Arc::clone(&ctx), input_channel, msg.id, remaining);
                    }
                }
                Err(err) => warn!("{:?}", err.wrap_err("failed to send replay details embed")),
            }
        }

        let content = format!("<@{user}> [Replay]({watch_url})");
        let mut builder = MessageBuilder::new()
            .content(content)
            .allowed_user_mention(user);
        if let Some(row) = link_row_for_msg {
            builder = builder.components(vec![row]);
        }

        if let Some((msg_id, channel_id)) = queue_message {
            let _ = ctx.http.delete_message(channel_id, msg_id).await;
        }

        match input_channel.create_message(&ctx, &builder).await {
            Ok(sent_msg) => {
                if uploaded_size_bytes.is_none_or(|s| s <= 510_000_000) {
                    let ctx_verify = Arc::clone(&ctx);
                    let msg_id = sent_msg.id;
                    tokio::spawn(async move {
                        tokio::time::sleep(tokio::time::Duration::from_secs(6)).await;
                        let should_retry = match ctx_verify.http.message(input_channel, msg_id).await {
                            Ok(resp) => match resp.model().await {
                                Ok(m) => {
                                    let suppressed = m
                                        .flags
                                        .is_some_and(|f| f.contains(MessageFlags::SUPPRESS_EMBEDS));
                                    !suppressed && !m.embeds.iter().any(|e| e.video.is_some())
                                }
                                Err(_) => false,
                            },
                            Err(_) => false,
                        };
                        if should_retry {
                            let busted = append_cache_buster(&watch_url);
                            warn!(
                                msg_id = %msg_id,
                                url = %busted,
                                "Discord embed missing video after 6s; triggering ?v=2 refresh"
                            );
                            let retry_builder =
                                MessageBuilder::new().content(format!("<@{user}> [Replay]({busted})"));
                            match (msg_id, input_channel)
                                .update(&ctx_verify, &retry_builder)
                                .await
                            {
                                Ok(_) => info!(msg_id = %msg_id, "Dispatched ?v=2 embed refresh edit"),
                                Err(err) => warn!(
                                    ?err,
                                    msg_id = %msg_id,
                                    "Failed to edit message for ?v=2 embed refresh"
                                ),
                            }
                        }
                    });
                }
            }
            Err(err) => warn!("{:?}", err.wrap_err("failed to send video link to channel")),
        }
    }

    if let Some(new_filepath) = rendered_mp4_path {
        if let Err(err) = tokio::fs::remove_file(&new_filepath).await {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!("Failed to delete local MP4 file {:?}: {}", new_filepath, err);
            }
        }
    }

    if let Err(err) = tokio::fs::remove_file(&path).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            warn!("Failed to delete local OSR file {:?}: {}", path, err);
        }
    }

    Ok(())
}

async fn report_render_error(
    ctx: &Context,
    input_channel: Id<ChannelMarker>,
    web: Option<&WebJobHandle>,
    msg: impl Into<String>,
) {
    let msg = msg.into();
    if let Some(w) = web {
        let mut st = w.state.write().await;
        if !st.is_terminal() {
            st.status = "error".to_owned();
            st.error = Some(msg);
            st.finished_at = Some(time::OffsetDateTime::now_utc().unix_timestamp());
        }
    } else {
        let _ = input_channel.error(ctx, msg).await;
    }
}

async fn mark_web_cancelled(web: &WebJobHandle) {
    let mut st = web.state.write().await;
    if !st.is_terminal() {
        st.status = "cancelled".to_owned();
        st.finished_at = Some(time::OffsetDateTime::now_utc().unix_timestamp());
    }
}

async fn handle_cancelled_cleanup(
    ctx: &Context,
    queue_message: Option<(Id<MessageMarker>, Id<ChannelMarker>)>,
    osr_path: &Path,
    mp4_path: Option<&Path>,
    input_channel: Id<ChannelMarker>,
    user: Id<UserMarker>,
    web: Option<&WebJobHandle>,
) -> Result<()> {
    if let Some((msg_id, channel_id)) = queue_message {
        let _ = ctx.http.delete_message(channel_id, msg_id).await;
    }
    let _ = tokio::fs::remove_file(osr_path).await;
    if let Some(mp4) = mp4_path {
        let _ = tokio::fs::remove_file(mp4).await;
    }
    if let Some(w) = web {
        let _ = tokio::fs::remove_file(&w.settings_path).await;
        mark_web_cancelled(w).await;
    } else {
        let msg = MessageBuilder::new()
            .content(format!("🛑 <@{user}> Render cancelled."))
            .allowed_user_mention(user);
        let _ = input_channel.create_message(ctx, &msg).await;
    }
    Ok(())
}

pub(crate) fn append_cache_buster(url: &str) -> String {
    let (base, frag) = match url.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (url, None),
    };
    let sep = if base.contains('?') { '&' } else { '?' };
    match frag {
        Some(f) => format!("{base}{sep}v=2#{f}"),
        None => format!("{base}{sep}v=2"),
    }
}

pub(crate) fn clamp_utf8_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s.trim_end_matches(['_', '-']);
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].trim_end_matches(['_', '-'])
}

pub(crate) fn derive_mp4_probe_url(watch_url: &str, video_url: Option<&str>) -> String {
    if let Some(url) = video_url {
        let trimmed = url.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    if let Some(pos) = watch_url.rfind("/watch/") {
        let origin = &watch_url[..pos];
        let rest = &watch_url[pos + "/watch/".len()..];
        let stem = rest
            .split(['?', '#'])
            .next()
            .unwrap_or(rest)
            .trim_matches('/');
        if !stem.is_empty() {
            if stem.ends_with(".mp4") {
                return format!("{origin}/{stem}");
            }
            return format!("{origin}/{stem}.mp4");
        }
    }
    watch_url.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_mp4_probe_url() {
        assert_eq!(
            derive_mp4_probe_url("https://cdn.example.com/watch/clip123", None),
            "https://cdn.example.com/clip123.mp4"
        );
        assert_eq!(
            derive_mp4_probe_url(
                "https://cdn.example.com/watch/clip123",
                Some("https://cdn.example.com/custom.mp4")
            ),
            "https://cdn.example.com/custom.mp4"
        );
        assert_eq!(
            derive_mp4_probe_url("https://cdn.example.com/v/clip123", None),
            "https://cdn.example.com/v/clip123"
        );
    }

    #[test]
    fn test_clamp_utf8_bytes() {
        assert_eq!(clamp_utf8_bytes("hello_world", 64), "hello_world");
        assert_eq!(clamp_utf8_bytes("abc_def_", 4), "abc");
        assert_eq!(clamp_utf8_bytes("abécd", 3), "ab");
    }

    #[test]
    fn test_append_cache_buster() {
        assert_eq!(
            append_cache_buster("https://replays.example.com/watch/abc"),
            "https://replays.example.com/watch/abc?v=2"
        );
        assert_eq!(
            append_cache_buster("https://replays.example.com/watch/abc?foo=1#t=10"),
            "https://replays.example.com/watch/abc?foo=1&v=2#t=10"
        );
    }
}
