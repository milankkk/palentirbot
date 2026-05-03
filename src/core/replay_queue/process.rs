use std::{
    error::Error as StdError,
    ffi::OsStr,
    fmt::{Display, Formatter, Result as FmtResult},
    fs,
    io::Cursor,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
};
use rosu_v2::prelude::BeatmapExtended;
use rosu_v2::prelude::GameModsLegacy;
use bytes::Bytes;
use eyre::{Context as _, ContextCompat, Report, Result};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::io::AsyncBufReadExt;
use rosu_pp::Beatmap;
use crate::util::builder::EmbedBuilder;
use tokio::time::{sleep, Duration};
use crate::util::MessageExt;

use tokio::{
    process::{ChildStdout, Command},
};
use zip::ZipArchive;
use crate::{
    core::{BotConfig, Context, ReplayStatus},
    util::{builder::MessageBuilder, levenshtein_similarity, ChannelExt},
};


use super::{ReplayData, ReplayQueue, ReplaySlim};

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
            let ReplayData {
                input_channel,
                output_channel,
                pitch,
                path,
                replay,
                time_points,
                user,
                title,
            } = ctx.replay_queue.peek().await;

            let replay_hash = match replay.replay_hash.as_deref() {
                Some(replay_hash) => replay_hash,
                None => {
                    warn!("replay without replay hash");

                    let content = "Could not get the replay hash";
                    let _ = input_channel.error(&ctx, content).await?;

                    ctx.replay_queue.reset_peek().await;
                    continue;
                }
            };

            let mapset_id = match replay.beatmap_hash.as_deref() {
                Some(hash) => match ctx.osu().beatmap().checksum(hash).await {
                    Ok(BeatmapExtended { mapset, .. }) => match mapset {
                        Some(mapset) => mapset.mapset_id,
                        None => {
                            warn!("map without mapset");

                            let content = "The mapset was not received when requesting the map from the osu!api";
                            let _ = input_channel.error(&ctx, content).await?;

                            ctx.replay_queue.reset_peek().await;
                            continue;
                        }
                    },
                    Err(err) => {
                        let context = format!("failed to request map with hash `{hash}`");
                        let err = Report::from(err).wrap_err(context);
                        warn!("{err:?}");

                        let content = "Failed to retrieve map. Maybe it's not submitted?";
                        let _ = input_channel.error(&ctx, content).await?;

                        ctx.replay_queue.reset_peek().await;
                        continue;
                    }
                },
                None => {
                    warn!("missing hash in replay requested by user {user}");

                    let content = "Missing the beatmap hash in the replay file";
                    let _ = input_channel.error(&ctx, content).await?;

                    ctx.replay_queue.reset_peek().await;
                    continue;
                }
            };

            warn!("Started map download");
            ctx.replay_queue.set_status(ReplayStatus::Downloading).await;

            if let Err(err) = download_mapset(&ctx, mapset_id).await {
                warn!("{err:?}");

                let content = "Failed to download map. Mirrors are likely down, try again later.";
                let _ = input_channel.error(&ctx, content).await?;

                ctx.replay_queue.reset_peek().await;
                continue;
            }

            warn!("Finished map download");

            let mut settings_path = config.paths.danser().to_owned();
            settings_path.push(format!("settings/{user}.json"));

            let settings = if settings_path.exists() {
                user.to_string()
            } else {
                "default".to_owned()
            };

            let filename_opt = path
                .file_name()
                .and_then(OsStr::to_str)
                .and_then(|name: &str| name.split('.').next());

            let filename = match filename_opt {
                Some(name) => name,
                None => {
                    warn!("replay path `{path:?}` has an unexpected form");

                    let content = "There was an error resolving the beatmap path";
                    let _ = input_channel.error(&ctx, content).await?;

                    ctx.replay_queue.reset_peek().await;
                    continue;
                }
            };

            let mut command = Command::new(&danser_path);
            warn!("danser dir: {:?}", config.paths.danser());
            warn!("danser path: {:?}", danser_path);
            let path = std::fs::canonicalize(&path)
                .context("failed to canonicalize replay path")?;
            command
                .current_dir(config.paths.danser())
                .arg("-noupdatecheck")
                //.arg("-nodbcheck")
                .arg("-replay")
                .arg(&path)
                .arg("-record")
                .arg("-settings")
                .arg(settings)
                .arg("-quickstart")
                .arg("-out")
                .arg(filename)
                .arg("-preciseprogress")
                .stderr(Stdio::piped())
                .stdout(Stdio::piped());
            

            // conditional args
            if time_points.start != 0 {
                command.args(["-start", &time_points.start.to_string()]);
            }
            if time_points.end != 0 {
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

            match command.spawn() {
                Ok(mut child) => {
                    let stdout = child.stdout.take().expect("missing stdout on child");
                    let stderr = child.stderr.take().expect("missing stderr on child");

                    let stdout_reader = tokio::io::BufReader::new(stdout);
                    let stderr_reader = tokio::io::BufReader::new(stderr);

                    // Spawn background tasks to constantly drain both pipes so they never fill up
                    let ctx1 = ctx.clone();
                    tokio::spawn(async move {
                        read_danser_progress(ctx1, stdout_reader).await;
                    });

                    let ctx2 = ctx.clone();
                    tokio::spawn(async move {
                        read_danser_progress(ctx2, stderr_reader).await;
                    });

                    // Now we can safely wait for the process to finish
                    let childres = child.wait().await;
                    tracing::trace!("Danser finished");

                    if let Err(err) = childres {
                        let err = eyre::Report::from(err).wrap_err("failed to run danser command");
                        tracing::warn!("{:?}", err);
                        let content = "Failed to run danser on the replay";
                        let _ = input_channel.error(&ctx, content).await;
                        ctx.replay_queue.reset_peek().await;
                        continue;
                    }
                }
                Err(err) => {
                    let err = eyre::Report::from(err).wrap_err("failed to start danser command");
                    tracing::warn!("{:?}", err);
                    let content = "Failed to run danser on the replay";
                    let _ = input_channel.error(&ctx, content).await;
                    ctx.replay_queue.reset_peek().await;
                    continue;
                }
            }



            info!("Finished replay processing");

            let title = match get_title() {
                Ok(title) => title,
                Err(err) => {
                    warn!("{err:?}");

                    let content = "Failed to read danser logs";
                    let _ = input_channel.error(&ctx, content).await?;

                    ctx.replay_queue.reset_peek().await;
                    continue;
                }
            };

            let map_osu_file = match get_beatmap_osu_file(mapset_id, &title).await {
                Ok(osu_file) => osu_file,
                Err(err) => {
                    let err = err.wrap_err("failed to get map_osu_file");
                    warn!("{err:?}");

                    let content = "danser did not like the replay file";
                    let _ = input_channel.error(&ctx, content).await?;

                    ctx.replay_queue.reset_peek().await;
                    continue;
                }
            };

            let mut map_path = config.paths.songs();
            map_path.push(format!("{mapset_id}/{map_osu_file}"));

            // --- NEW PARSING LOGIC HERE ---
                        // Extract the song name and difficulty from the `.osu` filename
            let (base_name, difficulty) = map_osu_file
                .strip_suffix(".osu")
                .and_then(|s| s.rsplit_once(" ["))
                .map(|(left, right)| (left.to_string(), right.trim_end_matches(']').to_string()))
                .unwrap_or_else(|| (title.clone(), String::new()));

            // Create a cleaner display title (e.g. "Ava Max - So Am I (Nightcore Cut Ver) [Outcast]")
            let formatted_title = if difficulty.is_empty() {
                base_name
            } else {
                format!("{} [{}]", base_name, difficulty)
            };

            let TitleResult { title: video_title, pp, max_pp, max_possible_combo, stars, acc } =
                match create_title(&replay, map_path.clone(), &title).await {
                    Ok(result) => result,
                    Err(err) => {
                        let err = err.wrap_err("failed to create title");
                        warn!("{err:?}");
                        let content = "There was an error while trying to create the video title";
                        let _ = input_channel.error(&ctx, content).await;
                        ctx.replay_queue.reset_peek().await;
                        continue;
                    }
                };
            // Store the formatted title back so the queue embed can display it
            ctx.replay_queue.queue.lock().await
                .front_mut()
                .map(|d| d.title = Some(video_title.clone()));


            // 1. Parse the components out of: "[7.05⭐] WhiteCat | DECO*27 - First Storm +HDDT 98.5%"
            let without_prefix = video_title.trim_start_matches('[');
            let (stars, rest) = without_prefix.split_once("⭐] ").unwrap_or(("0.00", without_prefix));
            let (player, rest) = rest.split_once(" | ").unwrap_or(("Unknown", rest));

            let rest = rest.trim_end_matches('%');
            let (rest, acc) = rest.rsplit_once(' ').unwrap_or((rest, "100.00"));

            let (map_name, mods) = if let Some((m, md)) = rest.rsplit_once(" +") {
                (m, md)
            } else {
                (rest, "NM")
            };

            // 2. Helper function to safely replace dots/spaces with underscores 
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

            // 3. Combine the sanitized values and add the replay hash at the end!
            // We take the first 8 characters of the replay hash to keep the filename from getting too long.
            //let hash_suffix = if replay_hash.len() > 8 { &replay_hash[..8] } else { replay_hash };
            
            use std::time::{SystemTime, UNIX_EPOCH};

            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

            let safe_filename = format!(
                "{}-{}-{}-{}-{}_{}_{}",
                sanitize(stars),
                sanitize(player),
                sanitize(map_name),
                sanitize(mods),
                sanitize(acc),
                if replay_hash.len() > 8 { &replay_hash[..8] } else { replay_hash },
                timestamp // Forces a 100% unique filename every single time
            );


            // 4. Rename the file locally
            let mut old_filepath = config.paths.replays().clone();
            old_filepath.push(format!("{}.mp4", filename)); 

            let mut new_filepath = config.paths.replays().clone();
            new_filepath.push(format!("{}.mp4", safe_filename)); 

            if let Err(err) = tokio::fs::rename(&old_filepath, &new_filepath).await {
                warn!("Failed to rename MP4 file: {:?}", err);
            }

            info!("Started upload to server");
            ctx.replay_queue.set_status(ReplayStatus::Uploading(0)).await;
            let beatmap_link = format!("https://osu.ppy.sh/beatmapsets/{}", mapset_id);

            // 6. Upload the RENAMED file!
            let upload_fut = ctx.client().upload_video(&video_title, user, &new_filepath, &beatmap_link, &replay_hash);

            let link = match upload_fut.await {
                Ok(res) if res.error == 1 => {
                    let err = format!("failed to upload: `{}`", res.text);
                    warn!("{err}");
                    let _ = input_channel.error(&ctx, err).await?;
                    ctx.replay_queue.reset_peek().await;
                    continue;
                }
                Ok(res) => res.text,
                Err(err) => {
                    let err = err.wrap_err("failed to upload file");
                    warn!("{err:?}");
                    let content = "Failed to upload file";
                    let _ = input_channel.error(&ctx, content).await?;
                    ctx.replay_queue.reset_peek().await;
                    continue;
                }
            };



            info!("Finished upload to server");
            warn!("upload returned link: {}", link);


            if let Ok(mut warmup_msg) = output_channel
                .create_message(&ctx, &MessageBuilder::new().content(link.clone()))
                .await
            {
                sleep(Duration::from_millis(1200)).await;
                let _ = warmup_msg.delete(&ctx).await;
            }

            let file_size_bytes = std::fs::metadata(&new_filepath)?.len();
            let wait_secs = calculate_warmup_delay_secs(file_size_bytes);
            warn!("Wait time for dc cache: {}", wait_secs);
            // Set uploading status with countdown before sleeping
            //ctx.replay_queue.set_status(ReplayStatus::Uploading(wait_secs)).await;
            let mut remaining = wait_secs;
            while remaining > 0 {
                ctx.replay_queue.set_status(ReplayStatus::Uploading(remaining)).await;
                ctx.replay_queue.notify.notify_waiters();
                sleep(Duration::from_secs(1)).await;
                remaining = remaining.saturating_sub(1);
            }
            ctx.replay_queue.notify.notify_waiters();



            //let content = format!("<@{user}> your replay is ready!\n{link}");
            //let watch_link = link.replacen("https://replays.insertdomainname.be/watch/", "https://replays.insertdomainname.be/", 1);
            // Send replay details embed before the link
            let legacy_mods = GameModsLegacy::from_bits(replay.mods);
            let mods_str = if legacy_mods.is_empty() {
                String::new()
            } else {
                format!("+{}", legacy_mods)
            };

            let player = replay.player_name.as_deref().unwrap_or("unknown player");
            let acc = replay.accuracy();

            use twilight_model::channel::message::embed::EmbedField;

            let embed = EmbedBuilder::new()
                //.title(format!("{stars}⭐ {player} | {title} {mods_str} ({acc}%"))
                .title(video_title)
                .url(format!("https://osu.ppy.sh/beatmapsets/{mapset_id}"))
                .fields(vec![
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
                        value: format!("{:.2}pp / {:.2}pp", pp, max_pp),
                    },
                    EmbedField {
                        inline: false,
                        name: "Hits".to_owned(),
                        value: format!(
                            "{} 🔹 | {} ✳️ | {} ⚠️ | {} ❌",
                            replay.count_300, replay.count_100, replay.count_50, replay.count_miss
                        ),
                    },
                ])
                .build();


            let embed_builder = MessageBuilder::new().embed(embed);
            if let Err(err) = output_channel.create_message(&ctx, &embed_builder).await {
                warn!("{:?}", Report::from(err).wrap_err("failed to send replay details embed"));
            }

            // existing code continues below:
            // let content = format!("{user} your replay is ready! ...

            let link = link.replacen(".mp4", "", 1);
            let content = format!("<@{user}> [Replay]({link})");

            let builder = MessageBuilder::new().content(content);


            if let Err(err) = output_channel.create_message(&ctx, &builder).await {
                let err = Report::from(err).wrap_err("failed to send video link");
                warn!("{err:?}");
            }

            ctx.replay_queue.reset_peek().await;
        }
    }
}

async fn read_danser_progress<R: tokio::io::AsyncRead + Unpin>(
    ctx: std::sync::Arc<crate::core::Context>,
    mut reader: tokio::io::BufReader<R>
) {
    use tokio::io::AsyncReadExt;

    let mut buf = [0u8; 1024];
    let mut text_buffer = String::new();

    while let Ok(n) = reader.read(&mut buf).await {
        if n == 0 { break; } // EOF

        let chunk = String::from_utf8_lossy(&buf[..n]);
        text_buffer.push_str(&chunk);

        let mut lines: Vec<&str> = text_buffer.split(|c| c == '\r' || c == '\n').collect();
        let incomplete = lines.pop().unwrap_or("").to_string();

        for line in lines {
            // Find "Progress: X%" exactly as it appeared in your log
            if let Some(idx) = line.find("Progress: ") {
                let remainder = &line[idx + 10..];
                if let Some(pct_str) = remainder.split('%').next() {
                    if let Ok(mut progress) = pct_str.parse::<u8>() {
                        progress = progress.min(100);
                        *ctx.replay_queue.status.lock().await = crate::core::ReplayStatus::Rendering(progress);
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
    chimu: Report,
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
            chimu: {:?}",
            self.kitsu, self.chimu
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
    let cursor = Cursor::new(bytes);

    let mut archive = ZipArchive::new(cursor).context("failed to create zip archive")?;

    let mut out_path = BotConfig::get().paths.songs();
    out_path.push(mapset_id.to_string());

    archive
        .extract(&out_path)
        .with_context(|| format!("failed to extract zip archive at `{out_path:?}`"))
}

async fn request_mapset(ctx: &Context, mapset_id: u32) -> Result<Bytes> {
    let catboy = match ctx.client().download_catboy_mapset(mapset_id).await {
        Ok(bytes) => return Ok(bytes),
        Err(err) => err,
    };
    let kitsu = match ctx.client().download_kitsu_mapset(mapset_id).await {
        Ok(bytes) => return Ok(bytes),
        Err(err) => err,
    };

    let chimu = match ctx.client().download_chimu_mapset(mapset_id).await {
        Ok(bytes) => return Ok(bytes),
        Err(err) => err,
    };  
    let nerinyan = match ctx.client().download_nerinyan_mapset(mapset_id).await {
        Ok(bytes) => return Ok(bytes),
        Err(err) => {
            warn!("nerinyan also failed: {err}");
            err
        }
    };

    Err(Report::from(MapsetDownloadError { kitsu, chimu, nerinyan, catboy }))
}

struct TitleResult {
    title: String,
    pp: f64,
    max_pp: f64,
    max_possible_combo: u32,
    stars: f64,
    acc: f32,
}

async fn create_title(replay: &ReplaySlim, map_path: PathBuf, map_title: &str) -> Result<TitleResult> {
    let map = Beatmap::from_path(&map_path)
        .with_context(|| format!("failed to parse map at {map_path:?}"))?;

    let difficulty = rosu_pp::Difficulty::new()
        .mods(replay.mods as u32)
        .calculate(&map);

    let stars = difficulty.stars();
    let max_possible_combo = difficulty.max_combo();

    let pp = rosu_pp::Performance::new(&map)
        .mods(replay.mods as u32)
        .n300(replay.count_300 as u32)
        .n100(replay.count_100 as u32)
        .n50(replay.count_50 as u32)
        .misses(replay.count_miss as u32)
        .combo(replay.max_combo as u32)
        .calculate()
        .pp();

    let max_pp = rosu_pp::Performance::new(&map)
        .mods(replay.mods as u32)
        .calculate()
        .pp();

    let stars = (stars * 100.0).round() / 100.0;
    let player = replay.player_name.as_deref().unwrap_or("unknown player");
    let acc = replay.accuracy();
    let legacy = GameModsLegacy::from_bits(replay.mods);
    let mods = if legacy.is_empty() {
        String::new()
    } else {
        format!(" +{}", legacy)
    };

    Ok(TitleResult {
        title: format!("[{stars}⭐] {player} | {map_title}{mods} ({acc}%)"),
        pp,
        max_pp,
        max_possible_combo,
        stars,
        acc,
    })
}


async fn get_beatmap_osu_file(mapset_id: u32, map_without_artist: &str) -> Result<String> {
    let mut items_dir = BotConfig::get().paths.songs();
    items_dir.push(mapset_id.to_string());

    let items = fs::read_dir(&items_dir)
        .with_context(|| format!("failed to read items dir at {items_dir:?}"))?;

    let mut max_similarity = 0.0;
    let mut final_file_name = String::new();

    for entry in items {
        match entry {
            Ok(entry) => {
                let file_name = entry.file_name();

                if let Some(file_name) = file_name.to_str().filter(|name| name.ends_with(".osu")) {
                    debug!("COMPARING: {map_without_artist} WITH: {file_name}");

                    let similarity = levenshtein_similarity(map_without_artist, file_name);

                    if similarity > max_similarity {
                        max_similarity = similarity;
                        final_file_name = file_name.to_owned();
                    }
                }
            }
            Err(err) => {
                let context = format!("there was an error while reading files in {items_dir:?}");

                return Err(Report::from(err).wrap_err(context));
            }
        }
    }

    debug!("FINAL TITLE: {final_file_name} SIMILARITY: {max_similarity}");

    Ok(final_file_name)
}

fn get_title() -> Result<String> {
    let mut logs_path = BotConfig::get().paths.danser().to_owned();
    logs_path.push("danser.log");

    let logs = fs::read_to_string(logs_path).context("failed to read danser logs")?;

    let line = logs
        .lines()
        .find(|line| line.contains("Playing:"))
        .context("expected `Playing:` in danser logs")?;

    line.splitn(4, ' ')
        .last()
        .map(str::to_owned)
        .with_context(|| format!("expected at least 5 words in danser log line `{line}`"))
}

fn calculate_warmup_delay_secs(file_size_bytes: u64) -> u64 {
    const UPLOAD_MBPS: f64 = 40.0;
    const EFFICIENCY: f64 = 0.90;
    const FRACTION: f64 = 0.95;
    const MIN_WAIT_SECS: f64 = 5.0;
    const EXTRA_BUFFER_SECS: f64 = 3.0;
    const MAX_WAIT_SECS: f64 = 45.0;

    let bytes_per_sec = (UPLOAD_MBPS * 1_000_000.0 / 8.0) * EFFICIENCY;
    let full_transfer_secs = file_size_bytes as f64 / bytes_per_sec;
    let wait_secs = (full_transfer_secs * FRACTION) + EXTRA_BUFFER_SECS;

    wait_secs.clamp(MIN_WAIT_SECS, MAX_WAIT_SECS).ceil() as u64
}
