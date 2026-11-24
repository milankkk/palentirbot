use std::{
    collections::HashMap,
    error::Error as StdError,
    fmt::{Display, Formatter, Result as FmtResult},
    io::{BufWriter, Cursor, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, LazyLock,
    },
};

use bytes::Bytes;
use eyre::{Context as _, Report, Result};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use zip::ZipArchive;

use crate::core::{BotConfig, Context, ReplayStatus};

use super::{danser::run_cancellable, md5::md5_hex};

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

static MAPSET_LOCKS: LazyLock<AsyncMutex<HashMap<u32, Arc<AsyncMutex<()>>>>> =
    LazyLock::new(|| AsyncMutex::new(HashMap::new()));
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
    std::fs::create_dir_all(dest)
        .with_context(|| format!("failed to create extract dir {dest:?}"))?;

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

pub(crate) async fn sweep_stale_temp_dirs(dir: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".tmp_") || name.starts_with(".old_") || name.starts_with(".flatten_tmp_")
        {
            let path = entry.path();
            if tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_dir()) {
                let _ = tokio::fs::remove_dir_all(&path).await;
            }
        }
    }
}

struct TmpExtractGuard {
    tmp_path: PathBuf,
    aborted: Arc<AtomicBool>,
    armed: bool,
}

impl Drop for TmpExtractGuard {
    fn drop(&mut self) {
        if self.armed {
            self.aborted.store(true, Ordering::Release);
            let path = std::mem::take(&mut self.tmp_path);
            if !path.as_os_str().is_empty() {
                tokio::spawn(async move {
                    let _ = tokio::fs::remove_dir_all(&path).await;
                });
            }
        }
    }
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
    let aborted = Arc::new(AtomicBool::new(false));
    let aborted_clone = Arc::clone(&aborted);
    let mut tmp_guard = TmpExtractGuard {
        tmp_path: tmp_path.clone(),
        aborted,
        armed: true,
    };

    let extract_res = tokio::task::spawn_blocking(move || {
        let res = extract_zip_safely(
            &bytes,
            &tmp_clone,
            MAX_UNCOMPRESSED_MAPSET_BYTES,
            MAX_MAPSET_ENTRIES,
        );
        if res.is_err() || aborted_clone.load(Ordering::Acquire) {
            let _ = std::fs::remove_dir_all(&tmp_clone);
        }
        res
    })
    .await
    .context("blocking zip extraction task panicked")?;

    if extract_res.is_err() {
        tmp_guard.armed = false;
        let _ = tokio::fs::remove_dir_all(&tmp_path).await;
        return extract_res;
    }

    if tokio::fs::try_exists(&out_path).await.unwrap_or(false) {
        let old_tmp = songs_root.join(format!(".old_{mapset_id}_{seq}"));
        if tokio::fs::rename(&out_path, &old_tmp).await.is_ok() {
            if let Err(err) = tokio::fs::rename(&tmp_path, &out_path).await {
                let _ = tokio::fs::rename(&old_tmp, &out_path).await;
                let _ = tokio::fs::remove_dir_all(&tmp_path).await;
                tmp_guard.armed = false;
                return Err(err).context("failed moving extracted beatmapset into place");
            }
            tmp_guard.armed = false;
            let _ = tokio::fs::remove_dir_all(&old_tmp).await;
        } else {
            let _ = tokio::fs::remove_dir_all(&out_path).await;
            if let Err(err) = tokio::fs::rename(&tmp_path, &out_path).await {
                let _ = tokio::fs::remove_dir_all(&tmp_path).await;
                tmp_guard.armed = false;
                return Err(err).context("failed replacing cached beatmapset");
            }
            tmp_guard.armed = false;
        }
    } else if let Err(err) = tokio::fs::rename(&tmp_path, &out_path).await {
        let _ = tokio::fs::remove_dir_all(&tmp_path).await;
        tmp_guard.armed = false;
        return Err(err).context("failed moving extracted beatmapset into place");
    }

    tmp_guard.armed = false;
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

fn parse_osu_header(bytes: &[u8]) -> (Option<u32>, Option<String>) {
    let content = String::from_utf8_lossy(bytes);
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
            file_version = Some(rest.trim().to_owned());
        }
    }

    (file_beatmap_id, file_version)
}

async fn find_beatmap_osu_file(
    mapset_id: u32,
    map_id: u32,
    version: &str,
    expected_md5: Option<&str>,
) -> Result<(PathBuf, Vec<u8>, bool)> {
    let items_dir = BotConfig::get().paths.songs().join(mapset_id.to_string());
    let mut entries = tokio::fs::read_dir(&items_dir)
        .await
        .with_context(|| format!("failed to read songs dir at {items_dir:?}"))?;

    let mut candidate_by_id: Option<(PathBuf, Vec<u8>)> = None;
    let mut candidate_by_version: Option<(PathBuf, Vec<u8>)> = None;
    let mut candidate_by_filename: Option<(PathBuf, Vec<u8>)> = None;

    let target_diff_suffix = format!("[{}].osu", version.to_lowercase());

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("osu") {
            continue;
        }
        let Ok(file_name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(bytes) = tokio::fs::read(&path).await else {
            continue;
        };

        let expected = expected_md5.map(str::to_owned);
        let need_fallback = candidate_by_id.is_none();
        let (bytes, md5_matched, header) = tokio::task::spawn_blocking(move || {
            let md5_matched = expected
                .as_deref()
                .is_some_and(|h| md5_hex(&bytes).eq_ignore_ascii_case(h));
            let header = if !md5_matched && need_fallback {
                Some(parse_osu_header(&bytes))
            } else {
                None
            };
            (bytes, md5_matched, header)
        })
        .await
        .context("blocking beatmap inspection task panicked")?;

        if md5_matched {
            return Ok((path, bytes, true));
        }

        let Some((file_beatmap_id, file_version)) = header else {
            continue;
        };

        if file_beatmap_id == Some(map_id) {
            if expected_md5.is_none() {
                return Ok((path, bytes, true));
            }
            candidate_by_id = Some((path, bytes));
            continue;
        }

        if candidate_by_version.is_none()
            && file_version
                .as_deref()
                .is_some_and(|v| v.eq_ignore_ascii_case(version))
        {
            candidate_by_version = Some((path, bytes));
            continue;
        }

        if candidate_by_filename.is_none()
            && file_name.to_lowercase().ends_with(&target_diff_suffix)
        {
            candidate_by_filename = Some((path, bytes));
        }
    }

    if let Some((path, bytes)) = candidate_by_id
        .or(candidate_by_version)
        .or(candidate_by_filename)
    {
        return Ok((path, bytes, expected_md5.is_none()));
    }

    eyre::bail!("no matching .osu file found in {items_dir:?} for map_id {map_id} ({version})")
}

pub(crate) struct ResolvedBeatmap {
    pub mapset_id: u32,
    pub map_id: u32,
    pub map_version: String,
    pub mapset_title: String,
    pub osu_path: PathBuf,
    pub osu_bytes: Vec<u8>,
}

pub(crate) enum BeatmapResolveError {
    Cancelled,
    Failure {
        user_msg: &'static str,
        report: Report,
    },
}

/// Resolve beatmap metadata by MD5, ensure its mapset is cached/fresh, and return the matched `.osu` path + bytes.
pub(crate) async fn resolve_and_ensure_beatmap(
    ctx: &Context,
    beatmap_hash: Option<&str>,
    update_queue_status: bool,
    cancel: Option<(&AtomicBool, &Notify)>,
) -> std::result::Result<ResolvedBeatmap, BeatmapResolveError> {
    let Some(hash) = beatmap_hash else {
        return Err(BeatmapResolveError::Failure {
            user_msg: "Missing the beatmap hash in the replay file.",
            report: eyre::eyre!("missing hash in replay"),
        });
    };

    let map = crate::osu_call!(ctx, |osu| osu.beatmap().checksum(hash)).map_err(|err| {
            let user_msg = if matches!(err, rosu_v2::error::OsuError::NotFound) {
                "Failed to retrieve map. Maybe it's not submitted?"
            } else {
                "Failed to contact the osu! API while looking up the beatmap. Please try again in a moment."
            };
            let report = Report::from(err).wrap_err(format!("failed to request map with hash {hash}"));
            warn!("{report:?}");
            BeatmapResolveError::Failure { user_msg, report }
        })?;

    let Some(mapset) = map.mapset else {
        return Err(BeatmapResolveError::Failure {
            user_msg: "The mapset was not received when requesting the map from the osu! API.",
            report: eyre::eyre!("map without mapset"),
        });
    };

    let mapset_id = mapset.mapset_id;
    let map_id = map.map_id;
    let map_version = map.version;
    let mapset_title = format!("{} - {}", mapset.artist, mapset.title);

    let mapset_path = BotConfig::get().paths.songs().join(mapset_id.to_string());
    let mapset_already_cached = tokio::fs::try_exists(&mapset_path).await.unwrap_or(false);

    if mapset_already_cached {
        tracing::debug!("Mapset {mapset_id} already exists locally, verifying difficulty.");
        if update_queue_status {
            ctx.replay_queue.set_status(ReplayStatus::MapFound).await;
        }
    } else {
        tracing::debug!("Started map download for mapset {mapset_id}");
        if update_queue_status {
            ctx.replay_queue.set_status(ReplayStatus::Downloading).await;
        }
        run_download_step(ctx, mapset_id, false, cancel).await?;
        tracing::debug!("Finished map download for mapset {mapset_id}");
    }

    let mut found = find_beatmap_osu_file(mapset_id, map_id, &map_version, Some(hash)).await;
    let needs_refresh = match &found {
        Ok((_, _, md5_ok)) => !md5_ok && mapset_already_cached,
        Err(_) => mapset_already_cached,
    };

    if needs_refresh {
        tracing::info!(
            mapset_id,
            map_id,
            "Cached mapset missing expected beatmap or checksum mismatch; re-downloading"
        );
        if update_queue_status {
            ctx.replay_queue.set_status(ReplayStatus::Downloading).await;
        }
        run_download_step(ctx, mapset_id, true, cancel).await?;
        found = find_beatmap_osu_file(mapset_id, map_id, &map_version, Some(hash)).await;
    }

    let (osu_path, mut osu_bytes, md5_ok) = found.map_err(|err| {
        let report = err.wrap_err("failed to find beatmap osu file");
        warn!("{report:?}");
        BeatmapResolveError::Failure {
            user_msg: "Could not find matching difficulty file in beatmapset.",
            report,
        }
    })?;

    // Mirrors occasionally serve stale .osz archives for recently updated WIP/Pending maps.
    // If the extracted .osu MD5 does not match the replay's beatmap hash, fetch the authoritative
    // .osu directly from osu.ppy.sh/osu/{map_id} and atomically update the file on disk for danser.
    if !md5_ok && map_id > 0 {
        let official_res = match cancel {
            Some((cancelled, cancel_notify)) => {
                match run_cancellable(
                    cancelled,
                    cancel_notify,
                    ctx.client().download_official_osu_file(map_id),
                )
                .await
                {
                    Some(res) => res,
                    None => return Err(BeatmapResolveError::Cancelled),
                }
            }
            None => ctx.client().download_official_osu_file(map_id).await,
        };

        match official_res {
            Ok(official_bytes) => {
                let official_vec = official_bytes.to_vec();
                let official_md5 = md5_hex(&official_vec);
                if official_md5.eq_ignore_ascii_case(hash) {
                    let mapset_lock = lock_for_mapset(mapset_id).await;
                    let _guard = mapset_lock.lock().await;
                    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
                    let tmp_osu = osu_path.with_extension(format!("tmp_osu_{map_id}_{seq}"));
                    if let Err(err) = tokio::fs::write(&tmp_osu, &official_vec).await {
                        warn!(
                            mapset_id,
                            map_id,
                            ?err,
                            "Failed writing temporary official .osu file"
                        );
                    } else if let Err(err) = tokio::fs::rename(&tmp_osu, &osu_path).await {
                        let _ = tokio::fs::remove_file(&tmp_osu).await;
                        warn!(
                            mapset_id,
                            map_id,
                            ?err,
                            "Failed replacing stale mirror .osu with official file"
                        );
                    } else {
                        tracing::info!(
                            mapset_id,
                            map_id,
                            hash,
                            "Replaced stale mirror .osu with official osu.ppy.sh copy matching replay MD5"
                        );
                        osu_bytes = official_vec;
                    }
                } else {
                    warn!(
                        mapset_id,
                        map_id,
                        expected_md5 = hash,
                        official_md5 = %official_md5,
                        "Mirror .osu and official osu.ppy.sh .osu both differ from replay beatmap MD5"
                    );
                }
            }
            Err(err) => {
                warn!(
                    mapset_id,
                    map_id,
                    expected_md5 = hash,
                    ?err,
                    "Mirror .osu MD5 mismatch and official osu.ppy.sh fallback download failed"
                );
            }
        }
    }

    Ok(ResolvedBeatmap {
        mapset_id,
        map_id,
        map_version,
        mapset_title,
        osu_path,
        osu_bytes,
    })
}

async fn run_download_step(
    ctx: &Context,
    mapset_id: u32,
    force_redownload: bool,
    cancel: Option<(&AtomicBool, &Notify)>,
) -> std::result::Result<(), BeatmapResolveError> {
    let dl_res = match cancel {
        Some((cancelled, cancel_notify)) => {
            match run_cancellable(
                cancelled,
                cancel_notify,
                download_mapset_inner(ctx, mapset_id, force_redownload),
            )
            .await
            {
                Some(res) => res,
                None => return Err(BeatmapResolveError::Cancelled),
            }
        }
        None => download_mapset_inner(ctx, mapset_id, force_redownload).await,
    };

    dl_res.map_err(|err| {
        warn!("{err:?}");
        BeatmapResolveError::Failure {
            user_msg: "Failed to download map. Mirrors are likely down, try again later.",
            report: err,
        }
    })
}
