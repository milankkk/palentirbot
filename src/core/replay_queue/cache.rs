use std::{collections::HashMap, path::Path, sync::Arc};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

const RENDER_CACHE_PATH: &str = "data/render_cache.json";
const MAX_CACHE_ENTRIES: usize = 500;
const CACHE_TTL_SECS: i64 = 6 * 86_400 + 12 * 3_600; // slightly under 7-day caddy retention

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
        let Ok(bytes) = tokio::fs::read(RENDER_CACHE_PATH).await else {
            return HashMap::new();
        };
        serde_json::from_slice::<HashMap<String, CachedRender>>(&bytes).unwrap_or_default()
    }

    async fn write_to_disk(entries: &HashMap<String, CachedRender>) -> eyre::Result<()> {
        let path = Path::new(RENDER_CACHE_PATH);
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let json = serde_json::to_vec(entries)?;
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, json).await?;
        tokio::fs::rename(&tmp, path).await?;
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
    replay_hash: &str,
    settings_bytes: &[u8],
    start: u32,
    end: u32,
    pitch: Option<f64>,
    skin: Option<&str>,
    skip_intro: bool,
    offset_ms: Option<i32>,
) -> Option<String> {
    if replay_hash.trim().is_empty() {
        return None;
    }
    let settings_fp = format!("{}:{:016x}", settings_bytes.len(), fnv1a_64(settings_bytes));
    let pitch_bits = pitch.map(f64::to_bits).unwrap_or(0);
    let skin_norm = skin.unwrap_or("");
    let off = offset_ms.unwrap_or(0);
    Some(format!(
        "{replay_hash}|{settings_fp}|{start}|{end}|{pitch_bits}|{skin_norm}|{skip_intro}|{off}"
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
        assert!(build_render_cache_key("", b"{}", 0, 0, None, None, false, None).is_none());

        let k1 = build_render_cache_key("abc", b"{\"a\":1}", 10, 20, Some(1.5), Some("MySkin"), true, Some(-5)).unwrap();
        let k2 = build_render_cache_key("abc", b"{\"a\":1}", 10, 20, Some(1.5), Some("MySkin"), true, Some(-5)).unwrap();
        let k3 = build_render_cache_key("abc", b"{\"a\":2}", 10, 20, Some(1.5), Some("MySkin"), true, Some(-5)).unwrap();
        let k4 = build_render_cache_key("abc", b"{\"a\":1}", 10, 20, Some(1.5), Some("Other"), true, Some(-5)).unwrap();

        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
        assert_ne!(k1, k4);
    }
}
