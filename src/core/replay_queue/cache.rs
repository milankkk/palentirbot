use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::core::BotConfig;

const MAX_CACHE_ENTRIES: usize = 500;
const CACHE_TTL_SECS: i64 = 6 * 86_400 + 12 * 3_600; // slightly under 7-day caddy retention

fn render_cache_path() -> PathBuf {
    BotConfig::try_get()
        .map(|cfg| cfg.paths.folders().join("render_cache.json"))
        .unwrap_or_else(|| PathBuf::from("data/render_cache.json"))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedRender {
    pub watch_url: String,
    #[serde(default)]
    pub video_url: Option<String>,
    pub created_at: i64,
}

pub struct RenderCache {
    entries: RwLock<HashMap<String, CachedRender>>,
    save_lock: Mutex<()>,
}

impl RenderCache {
    pub async fn load() -> Self {
        let mut entries = Self::read_from_disk().await;
        Self::prune_expired(&mut entries, now_unix());
        Self {
            entries: RwLock::new(entries),
            save_lock: Mutex::new(()),
        }
    }

    pub async fn get(&self, key: &str) -> Option<CachedRender> {
        let now = now_unix();
        let entry = self.entries.read().await.get(key).cloned()?;
        if now.saturating_sub(entry.created_at) > CACHE_TTL_SECS {
            return None;
        }
        Some(entry)
    }

    pub async fn remove(&self, key: &str) {
        let _save_guard = self.save_lock.lock().await;
        let snapshot = {
            let mut guard = self.entries.write().await;
            if guard.remove(key).is_none() {
                return;
            }
            guard.clone()
        };
        if let Err(err) = Self::write_to_disk(&snapshot).await {
            tracing::warn!(?err, "failed to persist render cache after removal");
        }
    }

    pub async fn insert(&self, key: String, entry: CachedRender) {
        let _save_guard = self.save_lock.lock().await;
        let now = now_unix();
        let snapshot = {
            let mut guard = self.entries.write().await;
            Self::prune_expired(&mut guard, now);
            guard.insert(key, entry);
            if guard.len() > MAX_CACHE_ENTRIES {
                let overflow = guard.len() - MAX_CACHE_ENTRIES;
                let mut oldest: Vec<(String, i64)> = guard
                    .iter()
                    .map(|(k, v)| (k.clone(), v.created_at))
                    .collect();
                oldest.sort_by_key(|(_, ts)| *ts);
                for (old_k, _) in oldest.into_iter().take(overflow) {
                    guard.remove(&old_k);
                }
            }
            guard.clone()
        };
        if let Err(err) = Self::write_to_disk(&snapshot).await {
            tracing::warn!(?err, "failed to persist render cache");
        }
    }

    fn prune_expired(entries: &mut HashMap<String, CachedRender>, now: i64) {
        entries.retain(|_, v| now.saturating_sub(v.created_at) <= CACHE_TTL_SECS);
    }

    async fn read_from_disk() -> HashMap<String, CachedRender> {
        let Ok(bytes) = tokio::fs::read(render_cache_path()).await else {
            return HashMap::new();
        };
        serde_json::from_slice::<HashMap<String, CachedRender>>(&bytes).unwrap_or_default()
    }

    async fn write_to_disk(entries: &HashMap<String, CachedRender>) -> eyre::Result<()> {
        let path = render_cache_path();
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let json = serde_json::to_vec(entries)?;
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, json).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }
}

fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Compute a deterministic 64-bit FNV-1a digest of byte slices.
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[allow(clippy::too_many_arguments)]
pub fn build_render_cache_key(
    osr_bytes: &[u8],
    beatmap_hash: &str,
    settings_bytes: &[u8],
    start: u32,
    end: u32,
    pitch: Option<f64>,
    skin: Option<&str>,
    skip_intro: bool,
    offset_ms: Option<i32>,
) -> Option<String> {
    if osr_bytes.is_empty() || beatmap_hash.trim().is_empty() {
        return None;
    }
    let replay_fp = format!(
        "{}:{}:{}",
        osr_bytes.len(),
        super::process::md5_hex(osr_bytes),
        beatmap_hash.trim().to_ascii_lowercase()
    );
    let settings_fp = format!("{}:{:016x}", settings_bytes.len(), fnv1a_64(settings_bytes));
    let pitch_str = pitch
        .map(|v| format!("{:016x}", v.to_bits()))
        .unwrap_or_else(|| "_".to_owned());
    let skin_norm = skin.unwrap_or("");
    let off_str = offset_ms
        .map(|v| v.to_string())
        .unwrap_or_else(|| "_".to_owned());
    Some(format!(
        "{replay_fp}|{settings_fp}|{start}|{end}|{pitch_str}|{skin_norm}|{skip_intro}|{off_str}"
    ))
}

#[allow(dead_code)]
pub fn new_shared_render_cache_empty() -> Arc<RenderCache> {
    Arc::new(RenderCache {
        entries: RwLock::new(HashMap::new()),
        save_lock: Mutex::new(()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_render_cache_key_determinism_and_uniqueness() {
        assert!(build_render_cache_key(b"", "abc", b"{}", 0, 0, None, None, false, None).is_none());
        assert!(build_render_cache_key(b"osr", "", b"{}", 0, 0, None, None, false, None).is_none());

        let k1 = build_render_cache_key(
            b"replay-bytes-1",
            "abc",
            b"{\"a\":1}",
            10,
            20,
            Some(1.5),
            Some("MySkin"),
            true,
            Some(-5),
        )
        .unwrap();
        let k2 = build_render_cache_key(
            b"replay-bytes-1",
            "abc",
            b"{\"a\":1}",
            10,
            20,
            Some(1.5),
            Some("MySkin"),
            true,
            Some(-5),
        )
        .unwrap();
        let k3 = build_render_cache_key(
            b"replay-bytes-1",
            "abc",
            b"{\"a\":2}",
            10,
            20,
            Some(1.5),
            Some("MySkin"),
            true,
            Some(-5),
        )
        .unwrap();
        let k4 = build_render_cache_key(
            b"replay-bytes-1",
            "abc",
            b"{\"a\":1}",
            10,
            20,
            Some(1.5),
            Some("Other"),
            true,
            Some(-5),
        )
        .unwrap();
        let k_none_off = build_render_cache_key(
            b"replay-bytes-1",
            "abc",
            b"{\"a\":1}",
            10,
            20,
            None,
            None,
            false,
            None,
        )
        .unwrap();
        let k_zero_off = build_render_cache_key(
            b"replay-bytes-1",
            "abc",
            b"{\"a\":1}",
            10,
            20,
            None,
            None,
            false,
            Some(0),
        )
        .unwrap();
        let k_other_bytes = build_render_cache_key(
            b"replay-bytes-2",
            "abc",
            b"{\"a\":1}",
            10,
            20,
            None,
            None,
            false,
            None,
        )
        .unwrap();

        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
        assert_ne!(k1, k4);
        assert_ne!(k_none_off, k_zero_off);
        assert_ne!(k_none_off, k_other_bytes);
    }
}
