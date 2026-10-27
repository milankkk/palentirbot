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
    error::Error as StdError,
    ffi::OsStr,
    fmt::{Display, Formatter, Result as FmtResult},
    io::Cursor,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use tokio::process::Command;
use tokio::time::{sleep, Duration};
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
        if !danser_path.exists() {
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
        input_channel,
        output_channel,
        pitch,
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
    } = data;

    let replay_hash = replay.replay_hash.as_deref().unwrap_or("");

    let (mapset_id, map_id, map_version, mapset_title) = match replay.beatmap_hash.as_deref() {
        Some(hash) => match ctx.osu().beatmap().checksum(hash).await {
            Ok(map) => match map.mapset {
                Some(mapset) => (
                    mapset.mapset_id,
                    map.map_id,
                    map.version,
                    format!("{} - {}", mapset.artist, mapset.title),
                ),
                None => {
                    warn!("map without mapset");
                    let content = "The mapset was not received when requesting the map from the osu!api";
                    let _ = input_channel.error(&ctx, content).await;
                    eyre::bail!("map without mapset");
                }
            },
            Err(err) => {
                let context = format!("failed to request map with hash {hash}");
                let err = Report::from(err).wrap_err(context);
                warn!("{err:?}");
                let content = "Failed to retrieve map. Maybe it's not submitted?";
                let _ = input_channel.error(&ctx, content).await;
                return Err(err);
            }
        },
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

    if mapset_path.exists() {
        warn!("Mapset {mapset_id} already exists locally, skipping download.");
        ctx.replay_queue.set_status(ReplayStatus::MapFound).await;
        ctx.replay_queue.notify.notify_waiters();
        sleep(Duration::from_secs(1)).await;
    } else {
        warn!("Started map download");
        ctx.replay_queue.set_status(ReplayStatus::Downloading).await;
        ctx.replay_queue.notify.notify_waiters();

        if let Err(err) = download_mapset(&ctx, mapset_id).await {
            warn!("{err:?}");
            let content = "Failed to download map. Mirrors are likely down, try again later.";
            let _ = input_channel
                .create_message(
                    &ctx,
                    &crate::util::builder::MessageBuilder::new().content(content),
                )
                .await;
            return Err(err);
        }
        warn!("Finished map download");
    }

    let mut settings_path = config.paths.danser().to_owned();
    settings_path.push(format!("settings/{user}.json"));

    let settings = if settings_path.exists() {
        user.to_string()
    } else {
        "default".to_owned()
    };

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

    tracing::warn!(mods = replay.mods, "replay mods being sent to danser");

    let canonical_path = tokio::fs::canonicalize(&path)
        .await
        .context("failed to canonicalize replay path")?;

    let mut command = Command::new(danser_path);
    warn!("danser dir: {:?}", config.paths.danser());
    warn!("danser path: {:?}", danser_path);
    command
        .current_dir(config.paths.danser())
        .arg("-noupdatecheck")
        .arg("-replay")
        .arg(&canonical_path)
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
    if let Some(pitch) = pitch {
        let pitch_val: f64 = pitch;
        command.args(["-pitch", &pitch_val.to_string()]);
    }

    info!("Started replay processing");

    ctx.replay_queue
        .set_status(ReplayStatus::Rendering(0))
        .await;

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            let err = Report::from(err).wrap_err("failed to start danser command");
            tracing::warn!("{:?}", err);
            let content = "Failed to run danser on the replay";
            let _ = input_channel.error(&ctx, content).await;
            return Err(err);
        }
    };

    let stdout = child.stdout.take().expect("missing stdout on child");
    let stderr = child.stderr.take().expect("missing stderr on child");

    let stdout_reader = tokio::io::BufReader::new(stdout);
    let stderr_reader = tokio::io::BufReader::new(stderr);

    let ctx1 = Arc::clone(&ctx);
    tokio::spawn(async move {
        read_danser_progress(ctx1, stdout_reader).await;
    });

    let ctx2 = Arc::clone(&ctx);
    tokio::spawn(async move {
        read_danser_progress(ctx2, stderr_reader).await;
    });

    let childres = child.wait().await;
    tracing::trace!("Danser finished");

    match childres {
        Ok(status) => {
            if !status.success() {
                let content = "Danser crashed or exited with an error code.";
                let _ = input_channel.error(&ctx, content).await;
                eyre::bail!("danser exited with non-zero status: {status}");
            }
        }
        Err(err) => {
            let err = Report::from(err).wrap_err("failed to run danser command");
            tracing::warn!("{:?}", err);
            let content = "Failed to run danser on the replay";
            let _ = input_channel.error(&ctx, content).await;
            return Err(err);
        }
    }

    warn!("parsed replay mods bits: {}", replay.mods);
    warn!(
        "parsed replay mods: {:?}",
        GameModsLegacy::from_bits(replay.mods)
    );
    info!("Finished replay processing");

    let map_osu_file = match find_beatmap_osu_file(mapset_id, map_id, &map_version).await {
        Ok(osu_file) => osu_file,
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

    // Store the formatted title back so the queue embed can display it
    if let Some(front) = ctx.replay_queue.queue.lock().await.front_mut() {
        front.title = Some(video_title.clone());
    }

    let without_prefix = video_title.trim_start_matches('[');
    let (stars, rest) = without_prefix
        .split_once("⭐] ")
        .unwrap_or(("0.00", without_prefix));
    let (player, rest) = rest.split_once(" | ").unwrap_or(("Unknown", rest));

    let rest = rest.trim_end_matches('%');
    let (rest, acc) = rest.rsplit_once(' ').unwrap_or((rest, "100.00"));

    let (map_name, mods) = if let Some((m, md)) = rest.rsplit_once(" +") {
        (m, md)
    } else {
        (rest, "NM")
    };

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

    let safe_filename = format!(
        "{}-{}-{}-{}-{}_{}_{}",
        sanitize(stars),
        sanitize(player),
        sanitize(map_name),
        sanitize(mods),
        sanitize(acc),
        if replay_hash.len() > 8 {
            &replay_hash[..8]
        } else {
            replay_hash
        },
        timestamp
    );

    let mut old_filepath = config.paths.replays().clone();
    old_filepath.push(format!("{}.mp4", filename));

    let mut new_filepath = config.paths.replays().clone();
    new_filepath.push(format!("{}.mp4", safe_filename));

    if let Err(err) = tokio::fs::rename(&old_filepath, &new_filepath).await {
        warn!("Failed to rename MP4 file: {:?}", err);
        new_filepath = old_filepath;
    }

    info!("Started upload to server");
    ctx.replay_queue
        .set_status(ReplayStatus::Uploading(0))
        .await;
    let beatmap_link = format!("https://osu.ppy.sh/beatmapsets/{}", mapset_id);

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

    let link = match upload_fut.await {
        Ok(res) if res.error != 0 => {
            let err = format!("failed to upload: `{}`", res.text);
            warn!("{err}");
            let _ = input_channel.error(&ctx, err).await;
            let _ = tokio::fs::remove_file(&new_filepath).await;
            eyre::bail!("upload server returned error {}", res.error);
        }
        Ok(res) => res.text,
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
    warn!("upload returned link: {}", link);

    if let Ok(warmup_msg) = output_channel
        .create_message(&ctx, &MessageBuilder::new().content(link.clone()))
        .await
    {
        sleep(Duration::from_millis(1200)).await;
        let _ = warmup_msg.delete(&ctx).await;
    }
    let file_size_bytes = tokio::fs::metadata(&new_filepath)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let upload_mbps = ctx.client().upload_speed_mbps();
    let wait_secs = calculate_warmup_delay_secs(file_size_bytes, upload_mbps);
    warn!("Wait time for dc cache: {}s (calculated at {:.1} Mbps)", wait_secs, upload_mbps);

    let mut remaining = wait_secs;
    while remaining > 0 {
        ctx.replay_queue
            .set_status(ReplayStatus::Uploading(remaining))
            .await;
        ctx.replay_queue.notify.notify_waiters();
        sleep(Duration::from_secs(1)).await;
        remaining = remaining.saturating_sub(1);
    }
    ctx.replay_queue.notify.notify_waiters();

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
        Grade::XH => format!("{}", emojis.grade_xh),
        Grade::X => format!("{}", emojis.grade_x),
        Grade::SH => format!("{}", emojis.grade_sh),
        Grade::S => format!("{}", emojis.grade_s),
        Grade::A => format!("{}", emojis.grade_a),
        Grade::B => format!("{}", emojis.grade_b),
        Grade::C => format!("{}", emojis.grade_c),
        Grade::D => format!("{}", emojis.grade_d),
        _ => format!("{}", emojis.grade_fail),
    };

    let is_perfect_fc = replay.max_combo == max_possible_combo && replay.count_miss == 0;
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

    let embed_builder = EmbedBuilder::new()
        .color(0x96DFE3)
        .title(video_title)
        .url(format!("https://osu.ppy.sh/beatmaps/{map_id}"))
        .fields(vec![
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

    let embed = embed_builder.build();
    let mut components: Vec<twilight_model::channel::message::Component> = vec![];

    let mut options = Vec::new();
    let mut push_opt = |id: &str, label: &str, desc: &str, t: PpTriple, is_default: bool| {
        options.push(
            twilight_model::channel::message::component::SelectMenuOption {
                default: is_default,
                description: Some(desc.to_string()),
                emoji: None,
                label: label.to_string(),
                value: format!("{id}|{is_perfect_fc}|{:.2}|{:.2}|{:.2}", t.pp, t.max_pp, t.nochoke_pp),
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
    let embed_builder = MessageBuilder::new().embed(embed).components(components);
    if let Err(err) = output_channel.create_message(&ctx, &embed_builder).await {
        warn!(
            "{:?}",
            Report::from(err).wrap_err("failed to send replay details embed")
        );
    }

    let link = link.replacen(".mp4", "", 1);
    let content = format!("<@{user}> [Replay]({link})");

    let builder = MessageBuilder::new().content(content);
    if let Some((msg_id, channel_id)) = queue_message {
        let _ = ctx.http.delete_message(channel_id, msg_id).await;
    }

    if let Err(err) = output_channel.create_message(&ctx, &builder).await {
        let err = Report::from(err).wrap_err("failed to send video link");
        warn!("{err:?}");
    }

    // 1. Delete generated .mp4 from Replays folder
    if let Err(err) = tokio::fs::remove_file(&new_filepath).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            warn!("Failed to delete local MP4 file {:?}: {}", new_filepath, err);
        }
    } else {
        warn!("Cleaned up local MP4 file at: {:?}.", new_filepath);
    }

    // 2. Delete .osr from Downloads folder
    if let Err(err) = tokio::fs::remove_file(&path).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            warn!("Failed to delete local OSR file {:?}: {}", path, err);
        }
    } else {
        warn!("Cleaned up local OSR file at: {:?}.", path);
    }

    Ok(())
}

async fn read_danser_progress<R: tokio::io::AsyncRead + Unpin>(
    ctx: std::sync::Arc<crate::core::Context>,
    mut reader: tokio::io::BufReader<R>,
) {
    use tokio::io::AsyncReadExt;

    let mut buf = [0u8; 1024];
    let mut text_buffer = String::new();

    while let Ok(n) = reader.read(&mut buf).await {
        if n == 0 {
            break;
        } // EOF

        let chunk = String::from_utf8_lossy(&buf[..n]);
        text_buffer.push_str(&chunk);

        let mut lines: Vec<&str> = text_buffer.split(|c| c == '\r' || c == '\n').collect();
        let incomplete = lines.pop().unwrap_or("").to_string();

        for line in lines {
            if let Some(idx) = line.find("Progress: ") {
                let remainder = &line[idx + 10..];
                if let Some(pct_str) = remainder.split('%').next() {
                    if let Ok(mut progress) = pct_str.parse::<u8>() {
                        progress = progress.min(100);
                        *ctx.replay_queue.status.lock().await =
                            crate::core::ReplayStatus::Rendering(progress);
                        ctx.replay_queue.notify.notify_waiters();
                    }
                }
            }
        }
        text_buffer = incomplete;
    }
}

#[derive(Debug)]
struct MapsetDownloadError {
    kitsu: Report,
    nerinyan: Report,
    catboy: Report,
    chimu: Report,
}

impl Display for MapsetDownloadError {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(
            f,
            "failed to download mapset:\n\
            kitsu: {:?}\n\
            nerinyan: {:?}\n\
            catboy: {:?}\n\
            chimu: {:?}",
            self.kitsu, self.nerinyan, self.catboy, self.chimu
        )
    }
}

impl StdError for MapsetDownloadError {
    #[inline]
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        None
    }
}

async fn download_mapset(ctx: &Context, mapset_id: u32) -> Result<()> {
    let bytes = request_mapset(ctx, mapset_id).await?;
    let mut out_path = BotConfig::get().paths.songs();
    out_path.push(mapset_id.to_string());

    tokio::task::spawn_blocking(move || -> Result<()> {
        let cursor = Cursor::new(bytes);
        let mut archive = ZipArchive::new(cursor).context("failed to create zip archive")?;
        archive
            .extract(&out_path)
            .with_context(|| format!("failed to extract zip archive at `{out_path:?}`"))?;
        Ok(())
    })
    .await
    .context("blocking zip extraction task panicked")?
}

async fn request_mapset(ctx: &Context, mapset_id: u32) -> Result<Bytes> {
    let kitsu = match ctx.client().download_kitsu_mapset(mapset_id).await {
        Ok(bytes) => {
            warn!("osu.direct success");
            return Ok(bytes);
        }
        Err(err) => {
            warn!("osu.direct failed: {err}");
            err
        }
    };
    let nerinyan = match ctx.client().download_nerinyan_mapset(mapset_id).await {
        Ok(bytes) => {
            warn!("nerinyan success");
            return Ok(bytes);
        }
        Err(err) => {
            warn!("nerinyan failed: {err}");
            err
        }
    };
    let catboy = match ctx.client().download_catboy_mapset(mapset_id).await {
        Ok(bytes) => {
            warn!("catboy/nekoha success");
            return Ok(bytes);
        }
        Err(err) => {
            warn!("catboy/nekoha failed: {err}");
            err
        }
    };
    let chimu = match ctx.client().download_chimu_mapset(mapset_id).await {
        Ok(bytes) => {
            warn!("chimu success");
            return Ok(bytes);
        }
        Err(err) => {
            warn!("chimu failed: {err}");
            err
        }
    };

    Err(Report::from(MapsetDownloadError {
        kitsu,
        nerinyan,
        catboy,
        chimu,
    }))
}

struct TitleResult {
    title: String,
    pp_v4: PpTriple,
    pp_latest: Option<PpTriple>,
    pp_2025: Option<PpTriple>,
    pp_2024: Option<PpTriple>,
    pp_2022: Option<PpTriple>,
    pp_2021: Option<PpTriple>,
    max_possible_combo: u16,
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
) -> Option<f64> {
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

    cmd.arg(map_path);

    if let Ok(output) = cmd.output().await {
        if output.status.success() {
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                if let Some(pp) = json
                    .get("performance_attributes")
                    .and_then(|attrs| attrs.get("pp"))
                    .and_then(|pp| pp.as_f64())
                {
                    return Some(pp);
                } else {
                    tracing::warn!("Failed to parse PP from JSON: {:?}", json);
                }
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
    } else {
        tracing::warn!("Failed to spawn command");
    }
    None
}

async fn calc_latest_pp(
    map_path: &std::path::Path,
    replay: &crate::core::replay_queue::data::ReplaySlim,
    max_combo: u32,
    is_lazer: bool,
) -> Option<PpTriple> {
    let dotnet_path = std::env::var("DOTNET_PATH").unwrap_or_else(|_| {
        if let Ok(output) = std::process::Command::new("dotnet").arg("--version").output() {
            if output.status.success() {
                return "dotnet".to_string();
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            let user_dotnet = std::path::PathBuf::from(home).join(".dotnet/dotnet");
            if user_dotnet.exists() {
                return user_dotnet.to_string_lossy().into_owned();
            }
        }
        for p in &["/usr/bin/dotnet", "/usr/local/bin/dotnet", "/app/dotnet/dotnet"] {
            if std::path::Path::new(p).exists() {
                return p.to_string();
            }
        }
        "dotnet".to_string()
    });
    let perf_calc_path = std::env::var("PERF_CALC_PATH").unwrap_or_else(|_| {
        let base = crate::core::BotConfig::get().paths.folders();
        let net10 = base.join("osu-tools/PerformanceCalculator/bin/Release/net10.0/PerformanceCalculator.dll");
        if net10.exists() {
            net10.to_string_lossy().into_owned()
        } else {
            let net8 = base.join("osu-tools/PerformanceCalculator/bin/Release/net8.0/PerformanceCalculator.dll");
            net8.to_string_lossy().into_owned()
        }
    });

    let dotnet_exists = if dotnet_path == "dotnet" {
        true
    } else {
        std::path::Path::new(&dotnet_path).exists()
    };

    if !dotnet_exists || !std::path::Path::new(&perf_calc_path).exists() {
        return None;
    }

    let (pp, max_pp, nochoke_pp) = tokio::join!(
        calc_single_latest_pp(
            &dotnet_path,
            &perf_calc_path,
            map_path,
            replay,
            max_combo,
            SimulateMode::Actual,
            is_lazer
        ),
        calc_single_latest_pp(
            &dotnet_path,
            &perf_calc_path,
            map_path,
            replay,
            max_combo,
            SimulateMode::Max,
            is_lazer
        ),
        calc_single_latest_pp(
            &dotnet_path,
            &perf_calc_path,
            map_path,
            replay,
            max_combo,
            SimulateMode::NoChoke,
            is_lazer
        )
    );

    match (pp, max_pp, nochoke_pp) {
        (Some(pp), Some(max_pp), Some(nochoke_pp)) => Some(PpTriple {
            pp,
            nochoke_pp,
            max_pp,
        }),
        _ => None,
    }
}

async fn create_title(
    replay: &ReplaySlim,
    map_path: PathBuf,
    map_title: &str,
    is_lazer: bool,
) -> Result<TitleResult> {
    let map = Beatmap::from_path(&map_path)
        .with_context(|| format!("failed to parse map at {map_path:?}"))?;

    let diff_attrs = rosu_pp::Difficulty::new()
        .mods(replay.mods as u32)
        .lazer(is_lazer)
        .calculate(&map);

    let stars = diff_attrs.stars();
    let max_possible_combo = diff_attrs.max_combo();

    let mut perf = rosu_pp::Performance::new(diff_attrs.clone())
        .mods(replay.mods as u32)
        .n300(replay.count_300 as u32)
        .n100(replay.count_100 as u32)
        .n50(replay.count_50 as u32)
        .misses(replay.count_miss as u32)
        .combo(replay.max_combo as u32)
        .lazer(is_lazer);

    if !is_lazer && replay.score > 0 {
        perf = perf.legacy_total_score(replay.score);
    }
    let pp = perf.calculate().pp();

    let max_pp = rosu_pp::Performance::new(diff_attrs.clone())
        .mods(replay.mods as u32)
        .lazer(is_lazer)
        .calculate()
        .pp();

    let nochoke_perf = rosu_pp::Performance::new(diff_attrs)
        .mods(replay.mods as u32)
        .n300((replay.count_300 + replay.count_miss) as u32)
        .n100(replay.count_100 as u32)
        .n50(replay.count_50 as u32)
        .misses(0)
        .combo(max_possible_combo)
        .lazer(is_lazer);

    let nochoke_pp = nochoke_perf.calculate().pp();

    let pp_v4 = PpTriple {
        pp,
        nochoke_pp,
        max_pp,
    };

    let stars = (stars * 100.0).round() / 100.0;

    let pp_latest = calc_latest_pp(&map_path, replay, max_possible_combo as u32, is_lazer).await;

    let pp_2025 = if !is_lazer {
        if let Ok(map_2025) = rosu_pp_2025::Beatmap::from_path(&map_path) {
            let diff_2025 = rosu_pp_2025::Difficulty::new()
                .mods(replay.mods as u32)
                .calculate(&map_2025);
            let pp = rosu_pp_2025::Performance::new(diff_2025.clone())
                .mods(replay.mods as u32)
                .n300(replay.count_300 as u32)
                .n100(replay.count_100 as u32)
                .n50(replay.count_50 as u32)
                .misses(replay.count_miss as u32)
                .combo(replay.max_combo as u32)
                .calculate()
                .pp();
            let max_pp = rosu_pp_2025::Performance::new(diff_2025.clone())
                .mods(replay.mods as u32)
                .calculate()
                .pp();
            let nochoke_pp = rosu_pp_2025::Performance::new(diff_2025)
                .mods(replay.mods as u32)
                .n300((replay.count_300 + replay.count_miss) as u32)
                .n100(replay.count_100 as u32)
                .n50(replay.count_50 as u32)
                .misses(0)
                .combo(max_possible_combo)
                .calculate()
                .pp();
            Some(PpTriple {
                pp,
                nochoke_pp,
                max_pp,
            })
        } else {
            None
        }
    } else {
        None
    };

    let pp_2024 = if !is_lazer {
        if let Ok(map_2024) = rosu_pp_2024::Beatmap::from_path(&map_path) {
            let diff_2024 = rosu_pp_2024::Difficulty::new()
                .mods(replay.mods as u32)
                .calculate(&map_2024);
            let pp = rosu_pp_2024::Performance::new(diff_2024.clone())
                .mods(replay.mods as u32)
                .n300(replay.count_300 as u32)
                .n100(replay.count_100 as u32)
                .n50(replay.count_50 as u32)
                .misses(replay.count_miss as u32)
                .combo(replay.max_combo as u32)
                .calculate()
                .pp();
            let max_pp = rosu_pp_2024::Performance::new(diff_2024.clone())
                .mods(replay.mods as u32)
                .calculate()
                .pp();
            let nochoke_pp = rosu_pp_2024::Performance::new(diff_2024)
                .mods(replay.mods as u32)
                .n300((replay.count_300 + replay.count_miss) as u32)
                .n100(replay.count_100 as u32)
                .n50(replay.count_50 as u32)
                .misses(0)
                .combo(max_possible_combo)
                .calculate()
                .pp();
            Some(PpTriple {
                pp,
                nochoke_pp,
                max_pp,
            })
        } else {
            None
        }
    } else {
        None
    };

    let pp_2022 = if !is_lazer {
        if let Ok(map_2022) = rosu_pp_2022::Beatmap::from_path(&map_path) {
            let pp = rosu_pp_2022::AnyPP::new(&map_2022)
                .mods(replay.mods as u32)
                .n300(replay.count_300 as usize)
                .n100(replay.count_100 as usize)
                .n50(replay.count_50 as usize)
                .n_misses(replay.count_miss as usize)
                .combo(replay.max_combo as usize)
                .calculate()
                .pp();
            let max_pp = rosu_pp_2022::AnyPP::new(&map_2022)
                .mods(replay.mods as u32)
                .calculate()
                .pp();
            let nochoke_pp = rosu_pp_2022::AnyPP::new(&map_2022)
                .mods(replay.mods as u32)
                .n300((replay.count_300 + replay.count_miss) as usize)
                .n100(replay.count_100 as usize)
                .n50(replay.count_50 as usize)
                .n_misses(0)
                .combo(max_possible_combo as usize)
                .calculate()
                .pp();
            Some(PpTriple {
                pp,
                nochoke_pp,
                max_pp,
            })
        } else {
            None
        }
    } else {
        None
    };

    let pp_2021 = if !is_lazer {
        if let Ok(map_2021) = rosu_pp_2021::Beatmap::from_path(&map_path) {
            let diff_2021 = rosu_pp_2021::Difficulty::new()
                .mods(replay.mods as u32)
                .calculate(&map_2021);
            let pp = rosu_pp_2021::Performance::new(diff_2021.clone())
                .mods(replay.mods as u32)
                .n300(replay.count_300 as u32)
                .n100(replay.count_100 as u32)
                .n50(replay.count_50 as u32)
                .misses(replay.count_miss as u32)
                .combo(replay.max_combo as u32)
                .calculate()
                .pp();
            let max_pp = rosu_pp_2021::Performance::new(diff_2021.clone())
                .mods(replay.mods as u32)
                .calculate()
                .pp();
            let nochoke_pp = rosu_pp_2021::Performance::new(diff_2021)
                .mods(replay.mods as u32)
                .n300((replay.count_300 + replay.count_miss) as u32)
                .n100(replay.count_100 as u32)
                .n50(replay.count_50 as u32)
                .misses(0)
                .combo(max_possible_combo)
                .calculate()
                .pp();
            Some(PpTriple {
                pp,
                nochoke_pp,
                max_pp,
            })
        } else {
            None
        }
    } else {
        None
    };

    let player_name = replay.player_name.as_ref().map(|user| user.clone());
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
        pp_v4,
        pp_latest,
        pp_2025,
        pp_2024,
        pp_2022,
        pp_2021,
        max_possible_combo: max_possible_combo.try_into().unwrap_or(0),
    })
}

async fn find_beatmap_osu_file(mapset_id: u32, map_id: u32, version: &str) -> Result<String> {
    let mut items_dir = BotConfig::get().paths.songs();
    items_dir.push(mapset_id.to_string());

    let mut entries = tokio::fs::read_dir(&items_dir)
        .await
        .with_context(|| format!("failed to read songs dir at {items_dir:?}"))?;

    let mut candidate_by_version_content = None;
    let mut candidate_by_filename = None;
    let mut any_osu = None;

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

        if any_osu.is_none() {
            any_osu = Some(file_name.clone());
        }

        if file_name.to_lowercase().ends_with(&target_diff_suffix) {
            candidate_by_filename = Some(file_name.clone());
        }

        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            let mut file_beatmap_id = None;
            let mut file_version = None;

            for line in content.lines() {
                if line.starts_with('[')
                    && !line.starts_with("[General]")
                    && !line.starts_with("[Metadata]")
                    && !line.starts_with("[Editor]")
                {
                    break;
                }
                if let Some(rest) = line.strip_prefix("BeatmapID:") {
                    if let Ok(id) = rest.trim().parse::<u32>() {
                        file_beatmap_id = Some(id);
                    }
                } else if let Some(rest) = line.strip_prefix("Version:") {
                    file_version = Some(rest.trim().to_string());
                }
            }

            if let Some(id) = file_beatmap_id {
                if id == map_id {
                    return Ok(file_name);
                }
            }

            if let Some(ref v) = file_version {
                if v.eq_ignore_ascii_case(version) {
                    candidate_by_version_content = Some(file_name.clone());
                }
            }
        }
    }

    if let Some(f) = candidate_by_version_content {
        return Ok(f);
    }
    if let Some(f) = candidate_by_filename {
        return Ok(f);
    }
    if let Some(f) = any_osu {
        return Ok(f);
    }

    eyre::bail!("no matching .osu file found in {items_dir:?} for map_id {map_id} ({version})")
}

fn calculate_warmup_delay_secs(file_size_bytes: u64, upload_mbps: f64) -> u64 {
    const EFFICIENCY: f64 = 0.85;
    const FRACTION: f64 = 0.90;
    const MIN_WAIT_SECS: f64 = 2.0;
    const EXTRA_BUFFER_SECS: f64 = 4.0;
    const MAX_WAIT_SECS: f64 = 60.0;

    let effective_mbps = upload_mbps.max(1.0);
    let bytes_per_sec = (effective_mbps * 1_000_000.0 / 8.0) * EFFICIENCY;
    let full_transfer_secs = file_size_bytes as f64 / bytes_per_sec;
    let wait_secs = (full_transfer_secs * FRACTION) + EXTRA_BUFFER_SECS;

    wait_secs.clamp(MIN_WAIT_SECS, MAX_WAIT_SECS).ceil() as u64
}
