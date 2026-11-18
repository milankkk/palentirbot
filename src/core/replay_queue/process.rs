use crate::util::builder::EmbedBuilder;
use crate::util::MessageExt;
use crate::{
    core::{BotConfig, Context, ReplayStatus},
    util::{builder::MessageBuilder, ChannelExt},
};
use bytes::Bytes;
use eyre::{Context as _, Report, Result};
use rosu_pp::Beatmap;
use rosu_v2::prelude::GameModsLegacy;
use std::{
    collections::{HashMap, VecDeque},
    error::Error as StdError,
    ffi::OsStr,
    fmt::{Display, Formatter, Result as FmtResult},
    io::{BufWriter, Cursor, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};
use once_cell::sync::Lazy;
use tokio::process::Command;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};
use twilight_model::id::{
    marker::{ChannelMarker, MessageMarker, UserMarker},
    Id,
};
use zip::ZipArchive;

use super::{PpTriple, ReplayData, ReplayQueue, ReplaySlim};

impl ReplayQueue {
    pub fn process(ctx: Arc<Context>) {
        tokio::spawn(Self::async_process(ctx));
    }

    async fn async_process(ctx: Arc<Context>) -> Result<()> {
        let config = BotConfig::get();
        let mut danser_path = config.paths.danser().to_owned();
        danser_path.push("danser");
        if !tokio::fs::try_exists(&danser_path).await.unwrap_or(false) {
            warn!("danser binary not found at {danser_path:?}");
        }

        loop {
            let data = ctx.replay_queue.peek().await;
            let replay_path = data.path.clone();
            let queue_message = data.queue_message;

            if let Err(err) = process_single_replay(Arc::clone(&ctx), data, &danser_path).await {
                warn!("Replay processing failed: {err:?}");
                if let Some((msg_id, channel_id)) = queue_message {
                    let _ = ctx.http.delete_message(channel_id, msg_id).await;
                }
                // Best effort cleanup of replay file if not already deleted
                let _ = tokio::fs::remove_file(&replay_path).await;
            }

            ctx.replay_queue.reset_peek().await;
        }
    }
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
        path,
        replay,
        time_points,
        user,
        title: _,
        player_name: _,
        map_title: _,
        difficulty_name: _,
        queue_message,
        is_lazer,
        cancelled,
        cancel_notify,
    } = data;

    if cancelled.load(Ordering::Acquire) {
        if let Some((msg_id, channel_id)) = queue_message {
            let _ = ctx.http.delete_message(channel_id, msg_id).await;
        }
        let _ = tokio::fs::remove_file(&path).await;
        return Ok(());
    }

    let replay_hash = replay.replay_hash.as_deref().unwrap_or("");
    let beatmap_hash = replay.beatmap_hash.as_deref();

    let (mapset_id, map_id, map_version, mapset_title) = match beatmap_hash {
        Some(hash) => {
            let mut res = ctx.osu().beatmap().checksum(hash).await;
            if let Err(ref err) = res {
                if Context::is_osu_auth_error(err) {
                    if let Ok(fresh) = ctx.refresh_osu_client().await {
                        res = fresh.beatmap().checksum(hash).await;
                    }
                }
            }
            match res {
                Ok(map) => match map.mapset {
                    Some(mapset) => (
                        mapset.mapset_id,
                        map.map_id,
                        map.version,
                        format!("{} - {}", mapset.artist, mapset.title),
                    ),
                    None => {
                        warn!("map without mapset");
                        let content =
                            "The mapset was not received when requesting the map from the osu!api";
                        let _ = input_channel.error(&ctx, content).await;
                        eyre::bail!("map without mapset");
                    }
                },
                Err(err) => {
                    let is_not_found = matches!(err, rosu_v2::error::OsuError::NotFound);
                    let context = format!("failed to request map with hash {hash}");
                    let err = Report::from(err).wrap_err(context);
                    warn!("{err:?}");
                    let content = if is_not_found {
                        "Failed to retrieve map. Maybe it's not submitted?"
                    } else {
                        "Failed to contact the osu! API while looking up the beatmap. Please try again in a moment."
                    };
                    let _ = input_channel.error(&ctx, content).await;
                    return Err(err);
                }
            }
        }
        None => {
            warn!("missing hash in replay requested by user {user}");
            let content = "Missing the beatmap hash in the replay file";
            let _ = input_channel.error(&ctx, content).await;
            eyre::bail!("missing hash in replay");
        }
    };

    let config = BotConfig::get();
    let mut mapset_path = config.paths.songs();
    mapset_path.push(mapset_id.to_string());

    let mapset_already_cached = tokio::fs::try_exists(&mapset_path).await.unwrap_or(false);
    if mapset_already_cached {
        tracing::debug!("Mapset {mapset_id} already exists locally, verifying difficulty.");
        ctx.replay_queue.set_status(ReplayStatus::MapFound).await;
        ctx.replay_queue.notify.notify_waiters();
    } else {
        tracing::debug!("Started map download for mapset {mapset_id}");
        ctx.replay_queue.set_status(ReplayStatus::Downloading).await;
        ctx.replay_queue.notify.notify_waiters();

        let dl_cancelled = tokio::select! {
            biased;
            _ = cancel_notify.notified() => true,
            res = download_mapset_inner(&ctx, mapset_id, false) => {
                if let Err(err) = res {
                    warn!("{err:?}");
                    let content = "Failed to download map. Mirrors are likely down, try again later.";
                    let _ = input_channel.error(&ctx, content).await;
                    return Err(err);
                }
                false
            }
        };
        if dl_cancelled || cancelled.load(Ordering::Acquire) {
            return handle_cancelled_cleanup(&ctx, queue_message, &path, None, input_channel, user).await;
        }
        tracing::debug!("Finished map download for mapset {mapset_id}");
    }

    let mut settings_path = config.paths.danser().to_owned();
    settings_path.push(format!("settings/{user}.json"));
    let mut default_settings_path = config.paths.danser().to_owned();
    default_settings_path.push("settings/default.json");

    let (settings, active_settings_path) = if tokio::fs::try_exists(&settings_path).await.unwrap_or(false) {
        (user.to_string(), settings_path)
    } else {
        ("default".to_owned(), default_settings_path)
    };
    let settings_bytes = tokio::fs::read(&active_settings_path).await.unwrap_or_default();
    let effective_skip_intro = skip_intro
        || serde_json::from_slice::<crate::core::settings::DanserSettings>(&settings_bytes)
            .map(|s| s.gameplay.always_skip_intro)
            .unwrap_or(false);

    let filename_opt = path.file_stem().and_then(OsStr::to_str);

    let filename = match filename_opt {
        Some(name) => name,
        None => {
            warn!("replay path `{path:?}` has an unexpected form");
            let content = "There was an error resolving the beatmap path";
            let _ = input_channel.error(&ctx, content).await;
            eyre::bail!("replay path `{path:?}` has an unexpected form");
        }
    };

    let mut found = find_beatmap_osu_file(mapset_id, map_id, &map_version, beatmap_hash).await;
    let needs_refresh = match &found {
        Ok((_, md5_ok)) => !md5_ok && mapset_already_cached,
        Err(_) => mapset_already_cached,
    };
    if needs_refresh {
        tracing::info!(
            mapset_id,
            map_id,
            "Cached mapset missing expected beatmap or checksum mismatch; re-downloading"
        );
        ctx.replay_queue.set_status(ReplayStatus::Downloading).await;
        ctx.replay_queue.notify.notify_waiters();
        download_mapset_inner(&ctx, mapset_id, true).await?;
        found = find_beatmap_osu_file(mapset_id, map_id, &map_version, beatmap_hash).await;
    }

    let (map_osu_file, _) = match found {
        Ok(res) => res,
        Err(err) => {
            let err = err.wrap_err("failed to find beatmap osu file");
            warn!("{err:?}");
            let content = "Could not find matching difficulty file in beatmapset";
            let _ = input_channel.error(&ctx, content).await;
            return Err(err);
        }
    };

    let mut map_path = config.paths.songs();
    map_path.push(format!("{mapset_id}/{map_osu_file}"));

    let title_for_display = format!("{mapset_title} [{map_version}]");

    let TitleResult {
        title: video_title,
        stars,
        pp_v4,
        pp_latest,
        pp_2025,
        pp_2024,
        pp_2022,
        pp_2021,
        max_possible_combo,
    } = match create_title(&replay, map_path.clone(), &title_for_display, is_lazer).await {
        Ok(result) => result,
        Err(err) => {
            let err = err.wrap_err("failed to create title");
            warn!("{err:?}");
            let content = "There was an error while trying to create the video title";
            let _ = input_channel.error(&ctx, content).await;
            return Err(err);
        }
    };

    // Store the formatted title back early so the queue embed shows it immediately
    if let Some(front) = ctx.replay_queue.queue.lock().await.front_mut() {
        front.title = Some(video_title.clone());
    }
    ctx.replay_queue.notify.notify_waiters();

    if cancelled.load(Ordering::Acquire) {
        return handle_cancelled_cleanup(&ctx, queue_message, &path, None, input_channel, user).await;
    }

    let osr_bytes = tokio::fs::read(&path)
        .await
        .context("failed to read queued .osr file for cache key")?;
    let cache_key = crate::core::replay_queue::cache::build_render_cache_key(
        &osr_bytes,
        beatmap_hash.unwrap_or(""),
        &settings_bytes,
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

    let (link, video_url, rendered_mp4_path) = if let Some((watch_url, vid_url)) = cached_hit {
        (watch_url, vid_url, None)
    } else {
        const NVENC_LOW_VRAM_SPATCH: &str = r#"{"Recording":{"Encoder":"h264_nvenc","h264_nvenc":{"Preset":"p4","AdditionalOptions":"-movflags +faststart"}}}"#;

        tracing::debug!(mods = replay.mods, "replay mods being sent to danser");

        let canonical_path = tokio::fs::canonicalize(&path)
            .await
            .context("failed to canonicalize replay path")?;

        let ld_library_path = build_danser_ld_library_path(config.paths.danser());

        ctx.replay_queue
            .set_status(ReplayStatus::Rendering(0))
            .await;

        ctx.ai()
            .release_gpu_for_render(&cancel_notify, &cancelled)
            .await;

        if cancelled.load(Ordering::Acquire) {
            return handle_cancelled_cleanup(&ctx, queue_message, &path, None, input_channel, user).await;
        }

        for attempt in 1..=2u8 {
            let low_vram_nvenc = attempt == 2;
            let spatch_opt = low_vram_nvenc.then_some(NVENC_LOW_VRAM_SPATCH);

            let attempt_outcome = run_danser_attempt(
                &ctx,
                danser_path,
                &ld_library_path,
                &canonical_path,
                &settings,
                filename,
                &time_points,
                pitch,
                skin.as_deref(),
                effective_skip_intro,
                offset_ms,
                spatch_opt,
                attempt,
                &cancelled,
                &cancel_notify,
            )
            .await;

            let (can_retry, user_msg, bail_msg) = match attempt_outcome {
                Ok(DanserAttemptOutcome::Success) => break,
                Ok(DanserAttemptOutcome::Cancelled) => {
                    cleanup_danser_outputs(config.paths.replays(), filename, &path).await;
                    return handle_cancelled_cleanup(
                        &ctx,
                        queue_message,
                        &path,
                        None,
                        input_channel,
                        user,
                    )
                    .await;
                }
                Ok(DanserAttemptOutcome::TotalTimeout { out_tail, err_tail }) => {
                    let danser_log_tail = read_danser_log_tail(config.paths.danser(), 25).await;
                    warn!(
                        attempt,
                        low_vram_nvenc,
                        ?out_tail,
                        ?err_tail,
                        ?danser_log_tail,
                        "danser render timed out after 1800s"
                    );
                    (
                        false,
                        "Danser render timed out after 30 minutes.".to_owned(),
                        "danser render timed out after 1800s".to_owned(),
                    )
                }
                Ok(DanserAttemptOutcome::FatalLine {
                    line,
                    out_tail,
                    err_tail,
                }) => {
                    let danser_log_tail = read_danser_log_tail(config.paths.danser(), 25).await;
                    warn!(
                        attempt,
                        low_vram_nvenc,
                        fatal_line = %line,
                        ?out_tail,
                        ?err_tail,
                        ?danser_log_tail,
                        "danser reported fatal error during render"
                    );
                    (
                        true,
                        "Danser encountered a fatal encoding/rendering error. Check logs for details."
                            .to_owned(),
                        format!("danser fatal error: {line}"),
                    )
                }
                Ok(DanserAttemptOutcome::NonZeroExit {
                    status,
                    out_tail,
                    err_tail,
                }) => {
                    let danser_log_tail = read_danser_log_tail(config.paths.danser(), 25).await;
                    warn!(
                        attempt,
                        low_vram_nvenc,
                        status = %status,
                        ?out_tail,
                        ?err_tail,
                        ?danser_log_tail,
                        "danser exited with non-zero status"
                    );
                    (
                        true,
                        "Danser crashed or exited with an error code.".to_owned(),
                        format!("danser exited with non-zero status: {status}"),
                    )
                }
                Ok(DanserAttemptOutcome::Stalled {
                    idle_secs,
                    saw_progress,
                    out_tail,
                    err_tail,
                }) => {
                    let danser_log_tail = read_danser_log_tail(config.paths.danser(), 25).await;
                    warn!(
                        attempt,
                        low_vram_nvenc,
                        idle_secs,
                        saw_progress,
                        ?out_tail,
                        ?err_tail,
                        ?danser_log_tail,
                        "danser render stalled with no output/progress"
                    );
                    (
                        true,
                        format!(
                            "Danser render stalled with no progress for {idle_secs}s and was aborted."
                        ),
                        format!("danser stalled with no progress for {idle_secs}s"),
                    )
                }
                Err(err) => {
                    let danser_log_tail = read_danser_log_tail(config.paths.danser(), 25).await;
                    warn!(
                        attempt,
                        low_vram_nvenc,
                        ?err,
                        ?danser_log_tail,
                        "failed to run danser"
                    );
                    cleanup_danser_outputs(config.paths.replays(), filename, &path).await;
                    let content = "Failed to run danser on the replay";
                    let _ = input_channel.error(&ctx, content).await;
                    return Err(err);
                }
            };

            if can_retry && attempt == 1 {
                warn!("Attempt 1 failed under full NVENC settings; unloading VRAM and retrying with low-VRAM GPU h264_nvenc settings");
                cleanup_danser_intermediate(config.paths.replays(), filename).await;
                ctx.ai().unload_idle_models().await;
                sleep(Duration::from_secs(1)).await;
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
                    )
                    .await;
                }
                continue;
            }

            cleanup_danser_outputs(config.paths.replays(), filename, &path).await;
            let _ = input_channel.error(&ctx, user_msg).await;
            eyre::bail!("{bail_msg}");
        }

        tracing::debug!(
            mods_bits = replay.mods,
            mods = ?GameModsLegacy::from_bits(replay.mods),
            "Finished replay processing"
        );

        let sanitize = |s: &str| -> String {
            let mut res = String::new();
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
            res.trim_matches(|c| c == '_' || c == '-').to_string()
        };

        use std::time::{SystemTime, UNIX_EPOCH};

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let stars_str = format!("{stars:.2}");
        let player_str = replay.player_name.as_deref().unwrap_or("Unknown");
        let acc_str = format!("{:.2}", replay.accuracy());
        let legacy_mods = GameModsLegacy::from_bits(replay.mods);
        let mods_str = if legacy_mods.is_empty() {
            "NM".to_owned()
        } else {
            legacy_mods.to_string()
        };
        let hash_prefix: String = replay_hash.chars().take(8).collect();
        let raw_safe_filename = format!(
            "{}-{}-{}-{}-{}_{}_{}",
            sanitize(&stars_str),
            sanitize(player_str),
            sanitize(&title_for_display),
            sanitize(&mods_str),
            sanitize(&acc_str),
            hash_prefix,
            timestamp
        );
        let safe_filename = clamp_utf8_bytes(&raw_safe_filename, 160);

        let mut old_filepath = config.paths.replays().clone();
        old_filepath.push(format!("{filename}.mp4"));

        let mut new_filepath = config.paths.replays().clone();
        new_filepath.push(format!("{safe_filename}.mp4"));

        if let Err(err) = tokio::fs::rename(&old_filepath, &new_filepath).await {
            tracing::debug!("Failed to rename MP4 file: {err:?}");
            new_filepath = old_filepath;
        }

        info!("Started upload to server");
        ctx.replay_queue
            .set_status(ReplayStatus::Uploading)
            .await;
        let beatmap_link = format!("https://osu.ppy.sh/beatmapsets/{mapset_id}");

        let upload_hash = if replay_hash.is_empty() {
            "nohash"
        } else {
            replay_hash
        };
        let upload_fut = ctx.client().upload_video(
            &video_title,
            user,
            &new_filepath,
            &beatmap_link,
            upload_hash,
        );

        let upload_res = tokio::select! {
            biased;
            _ = cancel_notify.notified() => None,
            res = upload_fut => Some(res),
        };

        let Some(upload_res) = upload_res else {
            return handle_cancelled_cleanup(
                &ctx,
                queue_message,
                &path,
                Some(&new_filepath),
                input_channel,
                user,
            )
            .await;
        };

        let (uploaded_link, uploaded_video_url) = match upload_res {
            Ok(res) if res.error != 0 => {
                let err = format!("failed to upload: `{}`", res.text);
                warn!("{err}");
                let _ = input_channel.error(&ctx, err).await;
                let _ = tokio::fs::remove_file(&new_filepath).await;
                eyre::bail!("upload server returned error {}", res.error);
            }
            Ok(res) => (res.text, res.video_url),
            Err(err) => {
                let err = err.wrap_err("failed to upload file");
                warn!("{err:?}");
                let content = "Failed to upload file";
                let _ = input_channel.error(&ctx, content).await;
                let _ = tokio::fs::remove_file(&new_filepath).await;
                return Err(err);
            }
        };

        info!("Finished upload to server");
        tracing::debug!("upload returned link: {}", uploaded_link);

        if let Some(ref key) = cache_key {
            let created_at = time::OffsetDateTime::now_utc().unix_timestamp();
            ctx.replay_queue
                .cache
                .insert(
                    key.clone(),
                    crate::core::replay_queue::cache::CachedRender {
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
            )
            .await;
        }

        ctx.replay_queue.notify.notify_waiters();

        (uploaded_link, uploaded_video_url, Some(new_filepath))
    };

    let _legacy_mods = GameModsLegacy::from_bits(replay.mods);
    let acc = replay.accuracy();

    use rosu_v2::prelude::Grade;
    use twilight_model::channel::message::embed::EmbedField;
    let emojis = &BotConfig::get().emojis;

    let score_value = {
        use crate::util::numbers::with_comma_int;
        with_comma_int(replay.score as u64).to_string()
    };

    let grade = replay.grade;
    tracing::warn!("grade reported: {:?}", grade);
    let rank_value = match grade {
        Grade::XH => emojis.grade_xh.to_string(),
        Grade::X => emojis.grade_x.to_string(),
        Grade::SH => emojis.grade_sh.to_string(),
        Grade::S => emojis.grade_s.to_string(),
        Grade::A => emojis.grade_a.to_string(),
        Grade::B => emojis.grade_b.to_string(),
        Grade::C => emojis.grade_c.to_string(),
        Grade::D => emojis.grade_d.to_string(),
        _ => emojis.grade_fail.to_string(),
    };

    let is_perfect_fc = u32::from(replay.max_combo) == max_possible_combo && replay.count_miss == 0;
    let replay_datetime: Option<time::OffsetDateTime> = replay
        .timestamp
        .and_then(|ts: i64| time::OffsetDateTime::from_unix_timestamp(ts).ok());

    let active_pp = pp_latest.unwrap_or(pp_v4);
    let display_pp = active_pp.pp;
    let display_max_pp = active_pp.max_pp;
    let display_nochoke_pp = active_pp.nochoke_pp;

    let played_field_value = match replay_datetime {
        Some(dt) => format!("Played on <t:{}:F>", dt.unix_timestamp()),
        None => "Unknown play date".to_string(),
    };

    let (aim_pp, speed_pp, acc_pp) = (active_pp.aim_pp, active_pp.speed_pp, active_pp.acc_pp);

    let mut embed_fields = vec![
        EmbedField {
            inline: true,
            name: "Rank".to_owned(),
            value: rank_value,
        },
        EmbedField {
            inline: true,
            name: "Score".to_owned(),
            value: score_value,
        },
        EmbedField {
            inline: true,
            name: "Accuracy".to_owned(),
            value: format!("{acc}%"),
        },
        EmbedField {
            inline: true,
            name: "Max Combo".to_owned(),
            value: format!("{}/{}x", replay.max_combo, max_possible_combo),
        },
        EmbedField {
            inline: true,
            name: "pp".to_owned(),
            value: if is_perfect_fc {
                format!("{:.2}pp / {:.2}pp", display_pp, display_max_pp)
            } else {
                format!(
                    "{:.2}pp *(No Choke: {:.2}pp)* / {:.2}pp",
                    display_pp, display_nochoke_pp, display_max_pp
                )
            },
        },
    ];

    if let (Some(aim), Some(speed), Some(acc_val)) = (aim_pp, speed_pp, acc_pp) {
        embed_fields.push(EmbedField {
            inline: true,
            name: "PP Breakdown".to_owned(),
            value: format!("Aim: `{aim:.1}` | Speed: `{speed:.1}` | Acc: `{acc_val:.1}`"),
        });
    }

    embed_fields.extend([
        EmbedField {
            inline: false,
            name: "Hits".to_owned(),
            value: format!(
                "{} {} | {} {} | {} {} | {} {}",
                emojis.hit_300,
                replay.count_300,
                emojis.hit_100,
                replay.count_100,
                emojis.hit_50,
                replay.count_50,
                emojis.hit_miss,
                replay.count_miss,
            ),
        },
        EmbedField {
            inline: false,
            name: "\u{200B}".to_owned(),
            value: played_field_value,
        },
    ]);

    let embed_title = if video_title.chars().count() > 256 {
        let mut s: String = video_title.chars().take(253).collect();
        s.push_str("...");
        s
    } else {
        video_title.clone()
    };

    let embed_builder = EmbedBuilder::new()
        .color(0x96DFE3)
        .title(embed_title)
        .url(format!("https://osu.ppy.sh/beatmaps/{map_id}"))
        .fields(embed_fields);

    let embed = embed_builder.build();
    let mut components: Vec<twilight_model::channel::message::Component> = vec![];

    let mut options = Vec::new();
    let mut push_opt = |id: &str, label: &str, desc: &str, t: PpTriple, is_default: bool| {
        let base_val = format!(
            "{id}|{is_perfect_fc}|{:.2}|{:.2}|{:.2}",
            t.pp, t.max_pp, t.nochoke_pp
        );
        if base_val.len() > 100 {
            return;
        }
        let mut value = base_val.clone();
        if let (Some(aim), Some(speed), Some(acc_val)) = (t.aim_pp, t.speed_pp, t.acc_pp) {
            let ext = format!("{base_val}|{aim:.1}|{speed:.1}|{acc_val:.1}");
            if ext.len() <= 100 {
                value = ext;
            }
        }
        options.push(
            twilight_model::channel::message::component::SelectMenuOption {
                default: is_default,
                description: Some(desc.to_string()),
                emoji: None,
                label: label.to_string(),
                value,
            },
        );
    };

    if let Some(lat) = pp_latest {
        push_opt(
            "pp_lat",
            "Latest Rework (C# backend)",
            "Real-time official osu! backend calculations",
            lat,
            true,
        );
    }
    push_opt(
        "pp_v4",
        "rosu-pp v4 (July 2026)",
        "Performance points & star rating updates",
        pp_v4,
        pp_latest.is_none(),
    );

    if let Some(t) = pp_2025 {
        push_opt("pp_v25", "March 2025 Rework", "March 2025 Rework", t, false);
    }
    if let Some(t) = pp_2024 {
        push_opt(
            "pp_v24",
            "October 2024 Combo Scaling Removal",
            "Removal of combo scaling from standard PP",
            t,
            false,
        );
    }
    if let Some(t) = pp_2022 {
        push_opt(
            "pp_v22",
            "September 2022 Rework",
            "Major performance & difficulty changes",
            t,
            false,
        );
    }
    if let Some(t) = pp_2021 {
        push_opt(
            "pp_v21",
            "November 2021 Xexxar Rework",
            "Rhythm complexity & aim rework",
            t,
            false,
        );
    }

    if options.len() > 1 {
        components.push(
            twilight_model::channel::message::component::Component::ActionRow(
                twilight_model::channel::message::component::ActionRow {
                    components: vec![
                        twilight_model::channel::message::component::Component::SelectMenu(
                            twilight_model::channel::message::component::SelectMenu {
                                custom_id: "pp_version_select".to_string(),
                                disabled: false,
                                max_values: Some(1),
                                min_values: Some(1),
                                options: Some(options),
                                placeholder: Some("Select PP Version".to_string()),
                                channel_types: None,
                                default_values: None,
                                kind:
                                    twilight_model::channel::message::component::SelectMenuType::Text,
                            },
                        ),
                    ],
                },
            ),
        );
    }

    let watch_url = link.strip_suffix(".mp4").unwrap_or(&link).to_string();
    let mut link_buttons = vec![twilight_model::channel::message::component::Component::Button(
        twilight_model::channel::message::component::Button {
            sku_id: None,
            custom_id: None,
            disabled: false,
            emoji: None,
            label: Some("Watch Replay".to_owned()),
            style: twilight_model::channel::message::component::ButtonStyle::Link,
            url: Some(watch_url.clone()),
        },
    )];
    if let Some(ref vid_url) = video_url {
        if vid_url.starts_with("http") {
            link_buttons.push(twilight_model::channel::message::component::Component::Button(
                twilight_model::channel::message::component::Button {
                    sku_id: None,
                    custom_id: None,
                    disabled: false,
                    emoji: None,
                    label: Some("Download MP4".to_owned()),
                    style: twilight_model::channel::message::component::ButtonStyle::Link,
                    url: Some(vid_url.clone()),
                },
            ));
        }
    }
    components.push(
        twilight_model::channel::message::component::Component::ActionRow(
            twilight_model::channel::message::component::ActionRow {
                components: link_buttons,
            },
        ),
    );

    if cancelled.load(Ordering::Acquire) {
        return handle_cancelled_cleanup(
            &ctx,
            queue_message,
            &path,
            rendered_mp4_path.as_deref(),
            input_channel,
            user,
        )
        .await;
    }

    let embed_builder = MessageBuilder::new().embed(embed).components(components);
    if let Err(err) = input_channel.create_message(&ctx, &embed_builder).await {
        warn!(
            "{:?}",
            err.wrap_err("failed to send replay details embed")
        );
    }

    let content = format!("<@{user}> [Replay]({watch_url})");

    let builder = MessageBuilder::new()
        .content(content.clone())
        .allowed_user_mention(user);
    if let Some((msg_id, channel_id)) = queue_message {
        let _ = ctx.http.delete_message(channel_id, msg_id).await;
    }

    match input_channel.create_message(&ctx, &builder).await {
        Ok(sent_msg) => {
            let ctx_verify = Arc::clone(&ctx);
            let watch_url_retry = watch_url.clone();
            let msg_id = sent_msg.id;
            tokio::spawn(async move {
                sleep(Duration::from_secs(6)).await;
                let has_video = match ctx_verify.http.message(input_channel, msg_id).await {
                    Ok(resp) => match resp.model().await {
                        Ok(m) => m.embeds.iter().any(|e| e.video.is_some()),
                        Err(_) => true,
                    },
                    Err(_) => true,
                };
                if !has_video {
                    let sep = if watch_url_retry.contains('?') { '&' } else { '?' };
                    let busted = format!("{watch_url_retry}{sep}v=2");
                    let retry_builder = MessageBuilder::new()
                        .content(format!("<@{user}> [Replay]({busted})"))
                        .allowed_user_mention(user);
                    let _ = (msg_id, input_channel)
                        .update(&ctx_verify, &retry_builder)
                        .await;
                }
            });
        }
        Err(err) => {
            let err = err.wrap_err("failed to send video link to channel");
            warn!("{err:?}");
        }
    }

    // 1. Delete generated .mp4 from Replays folder
    if let Some(new_filepath) = rendered_mp4_path {
        if let Err(err) = tokio::fs::remove_file(&new_filepath).await {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!("Failed to delete local MP4 file {:?}: {}", new_filepath, err);
            }
        } else {
            tracing::debug!("Cleaned up local MP4 file at: {:?}.", new_filepath);
        }
    }

    // 2. Delete .osr from Downloads folder
    if let Err(err) = tokio::fs::remove_file(&path).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            warn!("Failed to delete local OSR file {:?}: {}", path, err);
        }
    } else {
        tracing::debug!("Cleaned up local OSR file at: {:?}.", path);
    }

    Ok(())
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
    ];
    if FATAL_SUBSTRINGS.iter().any(|needle| body.contains(needle)) {
        return true;
    }

    (body.contains("[h264_nvenc @")
        || body.contains("[hevc_nvenc @")
        || body.contains("[av1_nvenc @"))
        && (body.contains("failed") || body.contains("out of memory") || body.contains("Error"))
}

pub(crate) async fn read_danser_log_tail(danser_dir: &Path, max_lines: usize) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }
    let log_path = danser_dir.join("danser.log");
    let Ok(contents) = tokio::fs::read_to_string(&log_path).await else {
        return Vec::new();
    };
    let mut ring = VecDeque::with_capacity(max_lines);
    for line in contents.lines() {
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
    fatal_notify: Notify,
}

impl DanserWatchdog {
    fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
            last_activity_ms: AtomicU64::new(0),
            saw_progress: AtomicBool::new(false),
            fatal_line: std::sync::Mutex::new(None),
            fatal_notify: Notify::new(),
        }
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

enum DanserAttemptOutcome {
    Success,
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
async fn run_danser_attempt(
    ctx: &Arc<Context>,
    danser_path: &Path,
    ld_library_path: &str,
    canonical_path: &Path,
    settings: &str,
    filename: &str,
    time_points: &crate::core::TimePoints,
    pitch: Option<f64>,
    skin: Option<&str>,
    effective_skip_intro: bool,
    offset_ms: Option<i32>,
    spatch: Option<&str>,
    attempt: u8,
    cancelled: &AtomicBool,
    cancel_notify: &Notify,
) -> Result<DanserAttemptOutcome> {
    let config = BotConfig::get();
    let mut command = Command::new(danser_path);
    tracing::debug!(
        danser_dir = ?config.paths.danser(),
        ?danser_path,
        attempt,
        low_vram_nvenc = spatch.is_some(),
        "launching danser"
    );
    #[cfg(unix)]
    command.process_group(0);
    command
        .current_dir(config.paths.danser())
        .env("LD_LIBRARY_PATH", ld_library_path)
        .kill_on_drop(true)
        .arg("-noupdatecheck")
        .arg("-replay")
        .arg(canonical_path)
        .arg("-record")
        .arg("-settings")
        .arg(settings)
        .arg("-quickstart")
        .arg("-out")
        .arg(filename)
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
    if let Some(skin_name) = skin {
        command.args(["-skin", skin_name]);
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

    info!(attempt, low_vram_nvenc = spatch.is_some(), "Started replay processing");

    let cancel_fut = cancel_notify.notified();
    tokio::pin!(cancel_fut);
    cancel_fut.as_mut().enable();

    if cancelled.load(Ordering::Acquire) {
        return Ok(DanserAttemptOutcome::Cancelled);
    }

    let mut child = command
        .spawn()
        .map_err(|err| Report::from(err).wrap_err("failed to start danser command"))?;

    let stdout = child.stdout.take().expect("missing stdout on child");
    let stderr = child.stderr.take().expect("missing stderr on child");

    let stdout_reader = tokio::io::BufReader::new(stdout);
    let stderr_reader = tokio::io::BufReader::new(stderr);

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
            terminate_danser_group(&mut child).await;
            let _ = drain_or_abort_stdio(stdout_handle, stderr_handle).await;
            Ok(DanserAttemptOutcome::Cancelled)
        }
        SelectStop::Fatal => {
            if tokio::time::timeout(Duration::from_millis(250), child.wait())
                .await
                .is_err()
            {
                terminate_danser_group(&mut child).await;
            }
            let (out_tail, err_tail) = drain_or_abort_stdio(stdout_handle, stderr_handle).await;
            let line = watchdog.take_fatal().unwrap_or_default();
            Ok(DanserAttemptOutcome::FatalLine {
                line,
                out_tail: out_tail.unwrap_or_default(),
                err_tail: err_tail.unwrap_or_default(),
            })
        }
        SelectStop::Exit(Ok(status)) => {
            let (out_tail, err_tail) = drain_or_abort_stdio(stdout_handle, stderr_handle).await;
            let out_tail = out_tail.unwrap_or_default();
            let err_tail = err_tail.unwrap_or_default();
            if let Some(line) = watchdog.take_fatal() {
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
            Ok(DanserAttemptOutcome::Success)
        }
        SelectStop::Exit(Err(err)) => {
            terminate_danser_group(&mut child).await;
            let _ = drain_or_abort_stdio(stdout_handle, stderr_handle).await;
            Err(Report::from(err).wrap_err("failed to run danser command"))
        }
        SelectStop::TotalTimeout => {
            terminate_danser_group(&mut child).await;
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
            terminate_danser_group(&mut child).await;
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

async fn read_danser_progress<R: tokio::io::AsyncRead + Unpin>(
    ctx: Arc<Context>,
    mut reader: tokio::io::BufReader<R>,
    watchdog: Arc<DanserWatchdog>,
) -> Vec<String> {
    use tokio::io::AsyncReadExt;

    let mut buf = [0u8; 1024];
    let mut text_buffer = String::new();
    let mut tail: VecDeque<String> = VecDeque::with_capacity(20);

    while let Ok(n) = reader.read(&mut buf).await {
        if n == 0 {
            break;
        } // EOF

        let chunk = String::from_utf8_lossy(&buf[..n]);
        text_buffer.push_str(&chunk);

        let mut lines: Vec<&str> = text_buffer.split(['\r', '\n']).collect();
        let incomplete = lines.pop().unwrap_or("").to_string();

        for line in lines {
            if !line.trim().is_empty() {
                if tail.len() >= 20 {
                    tail.pop_front();
                }
                tail.push_back(line.to_owned());
                watchdog.touch_activity();
                if is_fatal_danser_line(line) {
                    watchdog.record_fatal(line);
                }
            }
            if let Some(idx) = line.find("Progress: ") {
                let remainder = &line[idx + 10..];
                if let Some(pct_str) = remainder.split('%').next() {
                    if let Ok(mut progress) = pct_str.parse::<u8>() {
                        progress = progress.min(100);
                        watchdog.mark_progress();
                        *ctx.replay_queue.status.lock().await =
                            ReplayStatus::Rendering(progress);
                        ctx.replay_queue.notify.notify_waiters();
                    }
                }
            }
        }
        text_buffer = incomplete;
    }

    if !text_buffer.trim().is_empty() {
        if tail.len() >= 20 {
            tail.pop_front();
        }
        tail.push_back(text_buffer.clone());
        watchdog.touch_activity();
        if is_fatal_danser_line(&text_buffer) {
            watchdog.record_fatal(&text_buffer);
        }
    }

    tail.into_iter().collect()
}

#[derive(Debug)]
struct MapsetDownloadError {
    kitsu: Report,
    nerinyan: Report,
    catboy: Report,
}

impl Display for MapsetDownloadError {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(
            f,
            "failed to download mapset:\n\
            kitsu: {:?}\n\
            nerinyan: {:?}\n\
            catboy: {:?}",
            self.kitsu, self.nerinyan, self.catboy
        )
    }
}

impl StdError for MapsetDownloadError {
    #[inline]
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        None
    }
}

static MAPSET_LOCKS: Lazy<AsyncMutex<HashMap<u32, Arc<AsyncMutex<()>>>>> =
    Lazy::new(|| AsyncMutex::new(HashMap::new()));
static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

async fn lock_for_mapset(mapset_id: u32) -> Arc<AsyncMutex<()>> {
    let mut map = MAPSET_LOCKS.lock().await;
    Arc::clone(
        map.entry(mapset_id)
            .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
    )
}

/// Extract a ZIP archive safely with strict true decompressed byte caps, entry limits, and no `0o000` chmod.
pub(crate) fn extract_zip_safely(
    bytes: &[u8],
    dest: &Path,
    max_total_bytes: u64,
    max_entries: usize,
) -> Result<()> {
    let cursor = Cursor::new(bytes);
    let mut archive = ZipArchive::new(cursor).context("failed to create zip archive")?;
    eyre::ensure!(
        archive.len() <= max_entries,
        "zip archive has {} entries (max {max_entries})",
        archive.len()
    );
    std::fs::create_dir_all(dest).with_context(|| format!("failed to create extract dir {dest:?}"))?;

    let mut written_total: u64 = 0;
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .context("corrupt zip entry in archive")?;
        let rel = entry
            .enclosed_name()
            .ok_or_else(|| eyre::eyre!("zip archive contains unsafe path traversal entry"))?
            .to_owned();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out_path = dest.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let remaining = max_total_bytes.saturating_sub(written_total);
        let limit = remaining.saturating_add(1);
        let file = std::fs::File::create(&out_path)
            .with_context(|| format!("failed to create extracted file {out_path:?}"))?;
        let mut writer = BufWriter::new(file);
        let mut limited = entry.take(limit);
        let copied = std::io::copy(&mut limited, &mut writer)
            .with_context(|| format!("failed extracting {rel:?}"))?;
        writer.flush()?;
        written_total = written_total
            .checked_add(copied)
            .ok_or_else(|| eyre::eyre!("zip archive size overflow"))?;
        eyre::ensure!(
            written_total <= max_total_bytes,
            "zip archive exceeds {} MB uncompressed limit",
            max_total_bytes / (1024 * 1024)
        );
    }
    Ok(())
}

pub(crate) async fn download_mapset(ctx: &Context, mapset_id: u32) -> Result<()> {
    download_mapset_inner(ctx, mapset_id, false).await
}

async fn download_mapset_inner(
    ctx: &Context,
    mapset_id: u32,
    force_redownload: bool,
) -> Result<()> {
    const MAX_UNCOMPRESSED_MAPSET_BYTES: u64 = 500 * 1024 * 1024;
    const MAX_MAPSET_ENTRIES: usize = 4_096;

    let mapset_lock = lock_for_mapset(mapset_id).await;
    let _guard = mapset_lock.lock().await;

    let songs_root = BotConfig::get().paths.songs();
    let out_path = songs_root.join(mapset_id.to_string());
    if !force_redownload && tokio::fs::try_exists(&out_path).await.unwrap_or(false) {
        return Ok(());
    }

    let bytes = request_mapset(ctx, mapset_id).await?;
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = songs_root.join(format!(".tmp_{mapset_id}_{seq}"));
    let tmp_clone = tmp_path.clone();

    let extract_res = tokio::task::spawn_blocking(move || {
        extract_zip_safely(&bytes, &tmp_clone, MAX_UNCOMPRESSED_MAPSET_BYTES, MAX_MAPSET_ENTRIES)
    })
    .await
    .context("blocking zip extraction task panicked")?;

    if extract_res.is_err() {
        let _ = tokio::fs::remove_dir_all(&tmp_path).await;
        return extract_res;
    }

    if tokio::fs::try_exists(&out_path).await.unwrap_or(false) {
        let old_tmp = songs_root.join(format!(".old_{mapset_id}_{seq}"));
        if tokio::fs::rename(&out_path, &old_tmp).await.is_ok() {
            if let Err(err) = tokio::fs::rename(&tmp_path, &out_path).await {
                let _ = tokio::fs::rename(&old_tmp, &out_path).await;
                let _ = tokio::fs::remove_dir_all(&tmp_path).await;
                return Err(err).context("failed moving extracted beatmapset into place");
            }
            let _ = tokio::fs::remove_dir_all(&old_tmp).await;
        } else {
            let _ = tokio::fs::remove_dir_all(&out_path).await;
            if let Err(err) = tokio::fs::rename(&tmp_path, &out_path).await {
                let _ = tokio::fs::remove_dir_all(&tmp_path).await;
                return Err(err).context("failed replacing cached beatmapset");
            }
        }
    } else if let Err(err) = tokio::fs::rename(&tmp_path, &out_path).await {
        let _ = tokio::fs::remove_dir_all(&tmp_path).await;
        return Err(err).context("failed moving extracted beatmapset into place");
    }

    Ok(())
}

async fn request_mapset(ctx: &Context, mapset_id: u32) -> Result<Bytes> {
    let kitsu = match ctx.client().download_kitsu_mapset(mapset_id).await {
        Ok(bytes) => {
            tracing::debug!("osu.direct success");
            return Ok(bytes);
        }
        Err(err) => {
            tracing::debug!("osu.direct failed: {err}");
            err
        }
    };
    let nerinyan = match ctx.client().download_nerinyan_mapset(mapset_id).await {
        Ok(bytes) => {
            tracing::debug!("nerinyan success");
            return Ok(bytes);
        }
        Err(err) => {
            tracing::debug!("nerinyan failed: {err}");
            err
        }
    };
    let catboy = match ctx.client().download_catboy_mapset(mapset_id).await {
        Ok(bytes) => {
            tracing::debug!("catboy/nekoha success");
            return Ok(bytes);
        }
        Err(err) => {
            tracing::debug!("catboy/nekoha failed: {err}");
            err
        }
    };

    Err(Report::from(MapsetDownloadError {
        kitsu,
        nerinyan,
        catboy,
    }))
}

struct TitleResult {
    title: String,
    stars: f64,
    pp_v4: PpTriple,
    pp_latest: Option<PpTriple>,
    pp_2025: Option<PpTriple>,
    pp_2024: Option<PpTriple>,
    pp_2022: Option<PpTriple>,
    pp_2021: Option<PpTriple>,
    max_possible_combo: u32,
}

#[derive(Copy, Clone)]
enum SimulateMode {
    Actual,
    NoChoke,
    Max,
}

async fn calc_single_latest_pp(
    dotnet_path: &str,
    perf_calc_path: &str,
    map_path: &std::path::Path,
    replay: &crate::core::replay_queue::data::ReplaySlim,
    max_combo: u32,
    mode: SimulateMode,
    is_lazer: bool,
) -> Option<(f64, Option<f64>, Option<f64>, Option<f64>)> {
    let mut cmd = tokio::process::Command::new(dotnet_path);
    cmd.env("DOTNET_ROLL_FORWARD", "Major");
    cmd.arg(perf_calc_path);
    cmd.arg("simulate").arg("osu").arg("-j");

    match mode {
        SimulateMode::Max => {
            // Defaults to SS max combo
        }
        SimulateMode::NoChoke => {
            cmd.arg("--combo").arg(max_combo.to_string());
            cmd.arg("--goods").arg(replay.count_100.to_string());
            cmd.arg("--mehs").arg(replay.count_50.to_string());
            cmd.arg("--misses").arg("0");
        }
        SimulateMode::Actual => {
            cmd.arg("--combo").arg(replay.max_combo.to_string());
            cmd.arg("--goods").arg(replay.count_100.to_string());
            cmd.arg("--mehs").arg(replay.count_50.to_string());
            cmd.arg("--misses").arg(replay.count_miss.to_string());
            if !is_lazer && replay.score > 0 {
                cmd.arg("-l").arg(replay.score.to_string());
            }
        }
    }

    let m = replay.mods;
    if (m & 1) != 0 {
        cmd.arg("-m").arg("nf");
    }
    if (m & 2) != 0 {
        cmd.arg("-m").arg("ez");
    }
    if (m & 4) != 0 {
        cmd.arg("-m").arg("td");
    }
    if (m & 8) != 0 {
        cmd.arg("-m").arg("hd");
    }
    if (m & 16) != 0 {
        cmd.arg("-m").arg("hr");
    }
    if (m & 16384) != 0 {
        cmd.arg("-m").arg("pf");
    } else if (m & 32) != 0 {
        cmd.arg("-m").arg("sd");
    }
    if (m & 512) != 0 {
        cmd.arg("-m").arg("nc");
    } else if (m & 64) != 0 {
        cmd.arg("-m").arg("dt");
    }
    if (m & 128) != 0 {
        cmd.arg("-m").arg("rx");
    }
    if (m & 256) != 0 {
        cmd.arg("-m").arg("ht");
    }
    if (m & 1024) != 0 {
        cmd.arg("-m").arg("fl");
    }
    if (m & 4096) != 0 {
        cmd.arg("-m").arg("so");
    }
    if !is_lazer {
        cmd.arg("-m").arg("cl");
    }

    cmd.kill_on_drop(true);
    cmd.arg(map_path);

    match tokio::time::timeout(Duration::from_secs(30), cmd.output()).await {
        Ok(Ok(output)) => {
            if output.status.success() {
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
            } else {
                tracing::warn!(
                    "Command failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
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

async fn calc_latest_pp(
    map_path: &std::path::Path,
    replay: &crate::core::replay_queue::data::ReplaySlim,
    max_combo: u32,
    is_lazer: bool,
) -> Option<PpTriple> {
    static DOTNET_PATH: once_cell::sync::Lazy<Option<String>> = once_cell::sync::Lazy::new(|| {
        if let Ok(custom) = std::env::var("DOTNET_PATH") {
            if std::path::Path::new(&custom).exists() || custom == "dotnet" {
                return Some(custom);
            }
        }
        if let Ok(output) = std::process::Command::new("dotnet").arg("--version").output() {
            if output.status.success() {
                return Some("dotnet".to_string());
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            let user_dotnet = std::path::PathBuf::from(home).join(".dotnet/dotnet");
            if user_dotnet.exists() {
                return Some(user_dotnet.to_string_lossy().into_owned());
            }
        }
        for p in &["/usr/bin/dotnet", "/usr/local/bin/dotnet", "/app/dotnet/dotnet"] {
            if std::path::Path::new(p).exists() {
                return Some((*p).to_string());
            }
        }
        None
    });

    static PERF_CALC_PATH: once_cell::sync::Lazy<Option<String>> = once_cell::sync::Lazy::new(|| {
        let candidate = std::env::var("PERF_CALC_PATH").unwrap_or_else(|_| {
            let base = crate::core::BotConfig::get().paths.folders();
            let net10 = base.join("osu-tools/PerformanceCalculator/bin/Release/net10.0/PerformanceCalculator.dll");
            if net10.exists() {
                net10.to_string_lossy().into_owned()
            } else {
                let net8 = base.join("osu-tools/PerformanceCalculator/bin/Release/net8.0/PerformanceCalculator.dll");
                net8.to_string_lossy().into_owned()
            }
        });
        if std::path::Path::new(&candidate).exists() {
            Some(candidate)
        } else {
            None
        }
    });

    let dotnet_path = DOTNET_PATH.as_deref()?;
    let perf_calc_path = PERF_CALC_PATH.as_deref()?;

    let (pp, max_pp, nochoke_pp) = tokio::join!(
        calc_single_latest_pp(
            dotnet_path,
            perf_calc_path,
            map_path,
            replay,
            max_combo,
            SimulateMode::Actual,
            is_lazer
        ),
        calc_single_latest_pp(
            dotnet_path,
            perf_calc_path,
            map_path,
            replay,
            max_combo,
            SimulateMode::Max,
            is_lazer
        ),
        calc_single_latest_pp(
            dotnet_path,
            perf_calc_path,
            map_path,
            replay,
            max_combo,
            SimulateMode::NoChoke,
            is_lazer
        )
    );

    match (pp, max_pp, nochoke_pp) {
        (Some((pp, aim_pp, speed_pp, acc_pp)), Some((max_pp, _, _, _)), Some((nochoke_pp, _, _, _))) => {
            Some(PpTriple {
                pp,
                nochoke_pp,
                max_pp,
                aim_pp,
                speed_pp,
                acc_pp,
            })
        }
        _ => None,
    }
}

async fn create_title(
    replay: &ReplaySlim,
    map_path: PathBuf,
    map_title: &str,
    is_lazer: bool,
) -> Result<TitleResult> {
    let osu_bytes: Arc<[u8]> = Arc::from(
        tokio::fs::read(&map_path)
            .await
            .with_context(|| format!("failed to read map at {map_path:?}"))?,
    );
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

    let (pp_v4, stars, max_possible_combo) = tokio::task::spawn_blocking(move || -> Result<_> {
        let map = Beatmap::from_bytes(&osu_bytes_v4)
            .with_context(|| format!("failed to parse map at {map_path_v4:?}"))?;

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
            (stars * 100.0).round() / 100.0,
            max_possible_combo,
        ))
    })
    .await
    .context("rosu_pp calculation task panicked")??;

    let pp_latest = calc_latest_pp(&map_path, replay, max_possible_combo, is_lazer).await;

    let (pp_2025, pp_2024, pp_2022, pp_2021) = if !is_lazer {
        let osu_bytes_old = Arc::clone(&osu_bytes);
        tokio::task::spawn_blocking(move || {
            let pp_2025 = rosu_pp_2025::Beatmap::from_bytes(&osu_bytes_old)
                .ok()
                .map(|map_2025| {
                    let diff_2025 = rosu_pp_2025::Difficulty::new()
                        .mods(mods_val)
                        .calculate(&map_2025);
                    let pp = rosu_pp_2025::Performance::new(diff_2025.clone())
                        .mods(mods_val)
                        .n300(count_300)
                        .n100(count_100)
                        .n50(count_50)
                        .misses(count_miss)
                        .combo(max_combo)
                        .calculate()
                        .pp();
                    let max_pp = rosu_pp_2025::Performance::new(diff_2025.clone())
                        .mods(mods_val)
                        .calculate()
                        .pp();
                    let nochoke_pp = rosu_pp_2025::Performance::new(diff_2025)
                        .mods(mods_val)
                        .n300(total_300_nochoke)
                        .n100(count_100)
                        .n50(count_50)
                        .misses(0)
                        .combo(max_possible_combo)
                        .calculate()
                        .pp();
                    PpTriple::basic(pp, nochoke_pp, max_pp)
                });

            let pp_2024 = rosu_pp_2024::Beatmap::from_bytes(&osu_bytes_old)
                .ok()
                .map(|map_2024| {
                    let diff_2024 = rosu_pp_2024::Difficulty::new()
                        .mods(mods_val)
                        .calculate(&map_2024);
                    let pp = rosu_pp_2024::Performance::new(diff_2024.clone())
                        .mods(mods_val)
                        .n300(count_300)
                        .n100(count_100)
                        .n50(count_50)
                        .misses(count_miss)
                        .combo(max_combo)
                        .calculate()
                        .pp();
                    let max_pp = rosu_pp_2024::Performance::new(diff_2024.clone())
                        .mods(mods_val)
                        .calculate()
                        .pp();
                    let nochoke_pp = rosu_pp_2024::Performance::new(diff_2024)
                        .mods(mods_val)
                        .n300(total_300_nochoke)
                        .n100(count_100)
                        .n50(count_50)
                        .misses(0)
                        .combo(max_possible_combo)
                        .calculate()
                        .pp();
                    PpTriple::basic(pp, nochoke_pp, max_pp)
                });

            let pp_2022 = rosu_pp_2022::Beatmap::from_bytes(&osu_bytes_old)
                .ok()
                .map(|map_2022| {
                    let pp = rosu_pp_2022::AnyPP::new(&map_2022)
                        .mods(mods_val)
                        .n300(count_300 as usize)
                        .n100(count_100 as usize)
                        .n50(count_50 as usize)
                        .n_misses(count_miss as usize)
                        .combo(max_combo as usize)
                        .calculate()
                        .pp();
                    let max_pp = rosu_pp_2022::AnyPP::new(&map_2022)
                        .mods(mods_val)
                        .calculate()
                        .pp();
                    let nochoke_pp = rosu_pp_2022::AnyPP::new(&map_2022)
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

            let pp_2021 = rosu_pp_2021::Beatmap::from_bytes(&osu_bytes_old)
                .ok()
                .map(|map_2021| {
                    let diff_2021 = rosu_pp_2021::Difficulty::new()
                        .mods(mods_val)
                        .calculate(&map_2021);
                    let pp = rosu_pp_2021::Performance::new(diff_2021.clone())
                        .mods(mods_val)
                        .n300(count_300)
                        .n100(count_100)
                        .n50(count_50)
                        .misses(count_miss)
                        .combo(max_combo)
                        .calculate()
                        .pp();
                    let max_pp = rosu_pp_2021::Performance::new(diff_2021.clone())
                        .mods(mods_val)
                        .calculate()
                        .pp();
                    let nochoke_pp = rosu_pp_2021::Performance::new(diff_2021)
                        .mods(mods_val)
                        .n300(total_300_nochoke)
                        .n100(count_100)
                        .n50(count_50)
                        .misses(0)
                        .combo(max_possible_combo)
                        .calculate()
                        .pp();
                    PpTriple::basic(pp, nochoke_pp, max_pp)
                });

            (pp_2025, pp_2024, pp_2022, pp_2021)
        })
        .await
        .context("historical rosu_pp task panicked")?
    } else {
        (None, None, None, None)
    };

    let player_name = replay.player_name.clone();
    let player = player_name.as_deref().unwrap_or("Unknown player");
    let acc = replay.accuracy();
    let legacy = GameModsLegacy::from_bits(replay.mods);
    let mods = if legacy.is_empty() {
        String::new()
    } else {
        format!(" +{}", legacy)
    };

    Ok(TitleResult {
        title: format!("[{stars}⭐] {player} | {map_title}{mods} ({acc}%)"),
        stars,
        pp_v4,
        pp_latest,
        pp_2025,
        pp_2024,
        pp_2022,
        pp_2021,
        max_possible_combo,
    })
}

async fn find_beatmap_osu_file(
    mapset_id: u32,
    map_id: u32,
    version: &str,
    expected_md5: Option<&str>,
) -> Result<(String, bool)> {
    let mut items_dir = BotConfig::get().paths.songs();
    items_dir.push(mapset_id.to_string());

    let mut entries = tokio::fs::read_dir(&items_dir)
        .await
        .with_context(|| format!("failed to read songs dir at {items_dir:?}"))?;

    let mut candidate_by_id = None;
    let mut candidate_by_version_content = None;
    let mut candidate_by_filename = None;

    let target_diff_suffix = format!("[{}].osu", version.to_lowercase());

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("osu") {
            continue;
        }

        let file_name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => continue,
        };

        let Ok(bytes) = tokio::fs::read(&path).await else {
            continue;
        };

        if expected_md5.is_some_and(|h| md5_hex(&bytes).eq_ignore_ascii_case(h)) {
            return Ok((file_name, true));
        }

        if candidate_by_filename.is_none()
            && file_name.to_lowercase().ends_with(&target_diff_suffix)
        {
            candidate_by_filename = Some(file_name.clone());
        }

        let content = String::from_utf8_lossy(&bytes);
        let mut file_beatmap_id = None;
        let mut file_version = None;

        for line in content.lines() {
            let trimmed = line.trim_end_matches('\r');
            if trimmed.starts_with("[TimingPoints]") || trimmed.starts_with("[HitObjects]") {
                break;
            }
            if let Some(rest) = trimmed.strip_prefix("BeatmapID:") {
                if let Ok(id) = rest.trim().parse::<u32>() {
                    file_beatmap_id = Some(id);
                }
            } else if let Some(rest) = trimmed.strip_prefix("Version:") {
                file_version = Some(rest.trim().to_string());
            }
        }

        if file_beatmap_id == Some(map_id) {
            if expected_md5.is_none() {
                return Ok((file_name, true));
            }
            if candidate_by_id.is_none() {
                candidate_by_id = Some(file_name.clone());
            }
        }

        if candidate_by_version_content.is_none()
            && file_version
                .as_deref()
                .is_some_and(|v| v.eq_ignore_ascii_case(version))
        {
            candidate_by_version_content = Some(file_name);
        }
    }

    if let Some(f) = candidate_by_id
        .or(candidate_by_version_content)
        .or(candidate_by_filename)
    {
        return Ok((f, expected_md5.is_none()));
    }

    eyre::bail!("no matching .osu file found in {items_dir:?} for map_id {map_id} ({version})")
}

async fn handle_cancelled_cleanup(
    ctx: &Context,
    queue_message: Option<(Id<MessageMarker>, Id<ChannelMarker>)>,
    osr_path: &Path,
    mp4_path: Option<&Path>,
    input_channel: Id<ChannelMarker>,
    user: Id<UserMarker>,
) -> Result<()> {
    if let Some((msg_id, channel_id)) = queue_message {
        let _ = ctx.http.delete_message(channel_id, msg_id).await;
    }
    let _ = tokio::fs::remove_file(osr_path).await;
    if let Some(mp4) = mp4_path {
        let _ = tokio::fs::remove_file(mp4).await;
    }
    let msg = MessageBuilder::new()
        .content(format!("🛑 <@{user}> Render cancelled."))
        .allowed_user_mention(user);
    let _ = input_channel.create_message(ctx, &msg).await;
    Ok(())
}

pub(crate) async fn terminate_danser_group(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
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

async fn drain_or_abort_stdio<T>(
    mut out_h: JoinHandle<T>,
    mut err_h: JoinHandle<T>,
) -> (Option<T>, Option<T>) {
    let out = match tokio::time::timeout(Duration::from_secs(2), &mut out_h).await {
        Ok(Ok(val)) => Some(val),
        Ok(Err(_)) => None,
        Err(_) => {
            out_h.abort();
            None
        }
    };
    let err = match tokio::time::timeout(Duration::from_secs(2), &mut err_h).await {
        Ok(Ok(val)) => Some(val),
        Ok(Err(_)) => None,
        Err(_) => {
            err_h.abort();
            None
        }
    };
    (out, err)
}

async fn cleanup_danser_intermediate(replays_dir: impl AsRef<Path>, filename: &str) {
    let replays_dir = replays_dir.as_ref();
    let mp4_path = replays_dir.join(format!("{filename}.mp4"));
    let _ = tokio::fs::remove_file(&mp4_path).await;
    let temp_dir = replays_dir.join(format!("{filename}_temp"));
    let _ = tokio::fs::remove_dir_all(&temp_dir).await;
}

async fn cleanup_danser_outputs(replays_dir: impl AsRef<Path>, filename: &str, osr_path: &Path) {
    cleanup_danser_intermediate(replays_dir, filename).await;
    let _ = tokio::fs::remove_file(osr_path).await;
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
        let stem = rest.split(['?', '#']).next().unwrap_or(rest).trim_matches('/');
        if !stem.is_empty() {
            if stem.ends_with(".mp4") {
                return format!("{origin}/{stem}");
            }
            return format!("{origin}/{stem}.mp4");
        }
    }
    watch_url.to_owned()
}

pub(crate) fn md5_hex(data: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
        5,  9, 14, 20, 5,  9, 14, 20, 5,  9, 14, 20, 5,  9, 14, 20,
        4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
        6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
        0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
        0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
        0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
    ];

    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let mut a0: u32 = 0x6745_2301;
    let mut b0: u32 = 0xefcd_ab89;
    let mut c0: u32 = 0x98ba_dcfe;
    let mut d0: u32 = 0x1032_5476;

    for chunk in msg.chunks_exact(64) {
        let mut m = [0_u32; 16];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            m[i] = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
        }

        let mut a = a0;
        let mut b = b0;
        let mut c = c0;
        let mut d = d0;

        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | ((!b) & d), i),
                16..=31 => ((d & b) | ((!d) & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | (!d)), (7 * i) % 16),
            };
            let temp = d;
            d = c;
            c = b;
            let sum = a
                .wrapping_add(f)
                .wrapping_add(K[i])
                .wrapping_add(m[g]);
            b = b.wrapping_add(sum.rotate_left(S[i]));
            a = temp;
        }

        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = String::with_capacity(32);
    for word in [a0, b0, c0, d0] {
        for byte in word.to_le_bytes() {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_md5_hex_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

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
        // Multi-byte UTF-8 boundary ('é' is 2 bytes)
        assert_eq!(clamp_utf8_bytes("abécd", 3), "ab");
    }

    #[test]
    fn test_is_fatal_danser_line_detects_crashes_and_ignores_song_titles() {
        assert!(is_fatal_danser_line(
            "2026/11/18 12:36:48 ffmpeg's video process finished abruptly! Please check if you have enough storage"
        ));
        assert!(is_fatal_danser_line(
            "2026/11/18 12:36:48 ffmpeg's audio process failed to start! Error: exit status 1"
        ));
        assert!(is_fatal_danser_line("panic: runtime error: invalid memory address"));
        assert!(is_fatal_danser_line(
            "[h264_nvenc @ 0x55a1234] OpenEncodeSessionEx failed: out of memory"
        ));
        assert!(is_fatal_danser_line("CUDA_ERROR_OUT_OF_MEMORY"));

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
}
