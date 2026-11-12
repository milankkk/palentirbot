use std::collections::HashMap;

use eyre::{Context as _, Result};
use serde::Serialize;
use tokio::sync::RwLock;
use twilight_model::id::{marker::ChannelMarker, Id};

mod ollama;
mod queue;
pub mod search;

pub use ollama::{LastGenerationStats, OllamaClient};
pub use queue::{AiQueue, DEFAULT_MAX_AI_QUEUE_SIZE};
#[allow(unused_imports)]
pub use search::WebSearchClient;

#[derive(Debug, Clone, Serialize)]
pub struct AiMessage {
    pub role: String,
    pub content: String,
}

impl AiMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_owned(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_owned(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_owned(),
            content: content.into(),
        }
    }
}

const SYSTEM_PROMPT_ENV: &str = "AI_SYSTEM_PROMPT";
const CUTOFFS_FILE_PATH: &str = "data/ai_cutoffs.json";

pub struct AiClient {
    ollama: OllamaClient,
    queue: AiQueue,
    /// The cached raw prompt template (with `{username}` / `{user_id}` placeholders).
    cached_prompt: RwLock<String>,
    /// Per-channel cutoff timestamps (unix seconds). Messages created at or before
    /// this instant are excluded from the AI's conversation history, effectively
    /// "clearing" the context for that channel.
    context_cutoffs: RwLock<HashMap<Id<ChannelMarker>, i64>>,
}

impl AiClient {
    pub async fn new() -> Self {
        let prompt = Self::load_prompt_from_disk()
            .await
            .expect("failed to load AI system prompt on startup");

        let max_queue_size = std::env::var("AI_MAX_QUEUE_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_AI_QUEUE_SIZE);

        let cutoffs = Self::load_cutoffs_from_disk().await;

        Self {
            ollama: OllamaClient::new(),
            queue: AiQueue::new(max_queue_size),
            cached_prompt: RwLock::new(prompt),
            context_cutoffs: RwLock::new(cutoffs),
        }
    }

    /// Access the FIFO queue controlling generation concurrency.
    pub fn queue(&self) -> &AiQueue {
        &self.queue
    }

    /// Total number of AI requests currently executing or waiting in line.
    pub async fn queue_len(&self) -> usize {
        self.queue.len().await
    }

    pub async fn ask(&self, channel_id: Id<ChannelMarker>, messages: &[AiMessage]) -> Result<String> {
        self.ollama.ask(channel_id, messages).await
    }

    /// Return statistics from the most recent generation evaluated by Ollama for this channel.
    pub async fn last_stats(&self, channel_id: Id<ChannelMarker>) -> Option<LastGenerationStats> {
        self.ollama.last_stats(channel_id).await
    }

    /// The model identifier currently configured.
    pub fn model_name(&self) -> &str {
        self.ollama.model_name()
    }

    /// Total context window size in tokens configured for Ollama.
    pub fn num_ctx(&self) -> u32 {
        self.ollama.num_ctx()
    }

    /// Maximum generated tokens limit configured for Ollama.
    pub fn num_predict(&self) -> i32 {
        self.ollama.num_predict()
    }

    /// Return the cached system-prompt template.
    pub async fn get_prompt(&self) -> String {
        self.cached_prompt.read().await.clone()
    }

    /// Re-read the prompt from disk (or inline env var) and update the cache.
    pub async fn reload_prompt(&self) -> Result<()> {
        let new_prompt = Self::load_prompt_from_disk().await?;
        *self.cached_prompt.write().await = new_prompt;
        Ok(())
    }

    /// Mark a cutoff for a channel — messages older than or equal to `now` will be ignored.
    pub async fn clear_context(&self, channel_id: Id<ChannelMarker>) {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let snapshot = {
            let mut guard = self.context_cutoffs.write().await;
            guard.insert(channel_id, now);
            guard.clone()
        };
        if let Err(err) = Self::save_cutoffs_to_disk(&snapshot).await {
            tracing::warn!(?err, "failed to persist AI context cutoffs to disk");
        }
    }

    /// Return the cutoff timestamp for a channel, if any.
    pub async fn get_cutoff(&self, channel_id: Id<ChannelMarker>) -> Option<i64> {
        self.context_cutoffs.read().await.get(&channel_id).copied()
    }

    async fn load_cutoffs_from_disk() -> HashMap<Id<ChannelMarker>, i64> {
        let Ok(bytes) = tokio::fs::read(CUTOFFS_FILE_PATH).await else {
            return HashMap::new();
        };
        let Ok(raw) = serde_json::from_slice::<HashMap<String, i64>>(&bytes) else {
            return HashMap::new();
        };
        raw.into_iter()
            .filter_map(|(k, v)| k.parse::<u64>().ok().and_then(Id::new_checked).map(|id| (id, v)))
            .collect()
    }

    async fn save_cutoffs_to_disk(cutoffs: &HashMap<Id<ChannelMarker>, i64>) -> Result<()> {
        let raw: HashMap<String, i64> = cutoffs
            .iter()
            .map(|(k, &v)| (k.get().to_string(), v))
            .collect();
        let json = serde_json::to_vec_pretty(&raw)?;
        if let Some(parent) = std::path::Path::new(CUTOFFS_FILE_PATH).parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        tokio::fs::write(CUTOFFS_FILE_PATH, json).await?;
        Ok(())
    }

    /// Read the prompt from disk or env var.
    ///
    /// Uses lossy UTF-8 decoding to handle any non-UTF-8 characters in the prompt
    /// file without panicking or failing on startup.
    async fn load_prompt_from_disk() -> Result<String> {
        let source = std::env::var(SYSTEM_PROMPT_ENV)
            .unwrap_or_else(|_| "prompt.txt".to_owned());

        if source.ends_with(".txt") {
            let bytes = tokio::fs::read(&source)
                .await
                .with_context(|| format!("failed to read prompt file `{source}`"))?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        } else {
            Ok(source)
        }
    }
}
