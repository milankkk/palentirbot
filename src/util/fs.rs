use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use eyre::{Context, Report, Result};
use serde::Serialize;
use tokio::{fs, io::AsyncWriteExt};

static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

fn unique_tmp_path(path: &Path) -> PathBuf {
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("tmp");
    path.with_file_name(format!(".{name}.{pid}_{seq}.tmp"))
}

/// Atomically write `bytes` to `path` via a unique sibling temp file and `sync_all`.
pub async fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create directory {parent:?}"))?;
    }

    let tmp = unique_tmp_path(path);
    let res: Result<()> = async {
        let mut file = fs::File::create(&tmp)
            .await
            .with_context(|| format!("failed to create temp file {tmp:?}"))?;
        file.write_all(bytes)
            .await
            .with_context(|| format!("failed to write temp file {tmp:?}"))?;
        file.sync_all()
            .await
            .with_context(|| format!("failed to sync temp file {tmp:?}"))?;
        drop(file);
        fs::rename(&tmp, path)
            .await
            .with_context(|| format!("failed to atomically replace {path:?}"))?;
        Ok(())
    }
    .await;

    if res.is_err() {
        let _ = fs::remove_file(&tmp).await;
    }

    res
}

/// Atomically serialize `value` as JSON and write it to `path`.
pub async fn write_json_atomic<T: Serialize + ?Sized>(
    path: &Path,
    value: &T,
    pretty: bool,
) -> Result<()> {
    let bytes = if pretty {
        serde_json::to_vec_pretty(value)
    } else {
        serde_json::to_vec(value)
    }
    .with_context(|| format!("failed to serialize JSON for {path:?}"))?;

    write_bytes_atomic(path, &bytes).await
}

/// Synchronously and atomically write `bytes` to `path` via a unique sibling temp file.
pub fn write_bytes_atomic_sync(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {parent:?}"))?;
    }

    let tmp = unique_tmp_path(path);
    let res: Result<()> = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .with_context(|| format!("failed to open temp file {tmp:?}"))?;
        file.write_all(bytes)
            .with_context(|| format!("failed to write temp file {tmp:?}"))?;
        file.sync_all()
            .with_context(|| format!("failed to sync temp file {tmp:?}"))?;
        drop(file);
        std::fs::rename(&tmp, path)
            .with_context(|| format!("failed to atomically replace {path:?}"))?;
        Ok(())
    })();

    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }

    res
}

/// Atomically copy `src` to `dst` via a unique temp file in `dst`'s directory.
pub async fn copy_atomic(src: &Path, dst: &Path) -> Result<()> {
    let tmp = unique_tmp_path(dst);
    fs::copy(src, &tmp)
        .await
        .with_context(|| format!("failed to copy {src:?} to {tmp:?}"))?;

    if let Err(err) = fs::rename(&tmp, dst).await {
        let _ = fs::remove_file(&tmp).await;
        return Err(Report::from(err).wrap_err(format!("failed to replace {dst:?}")));
    }

    Ok(())
}
