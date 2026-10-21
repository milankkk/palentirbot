use std::collections::HashMap;

use eyre::{Context as _, Result};
use serde::Serialize;
use tokio::sync::RwLock;
use twilight_model::id::{marker::ChannelMarker, Id};

mod ollama;
mod queue;

pub use ollama::{LastGenerationStats, OllamaClient, DEFAULT_NUM_CTX};
pub use queue::{AiQueue, AiQueuePermit, AiQueueTicket, DEFAULT_MAX_AI_QUEUE_SIZE};

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

        Self {
            ollama: OllamaClient::new(),
            queue: AiQueue::new(max_queue_size),
            cached_prompt: RwLock::new(prompt),
            context_cutoffs: RwLock::new(HashMap::new()),
        }
    }

    /// Access the FIFO queue controlling generation concurrency.
    pub fn queue(&self) -> &AiQueue {
        &self.queue
    }

    /// Total number of AI requests currently executing or waiting in line.
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    pub async fn ask(&self, messages: &[AiMessage]) -> Result<String> {
        self.ollama.ask(messages).await
    }

    /// Return statistics from the most recent generation evaluated by Ollama.
    pub async fn last_stats(&self) -> Option<LastGenerationStats> {
        self.ollama.last_stats().await
    }

    /// The model identifier currently configured.
    pub fn model_name(&self) -> &str {
        self.ollama.model_name()
    }

    /// Total context window size in tokens configured for Ollama.
    pub fn num_ctx(&self) -> u32 {
        self.ollama.num_ctx()
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
        self.context_cutoffs
            .write()
            .await
            .insert(channel_id, now);
    }

    /// Return the cutoff timestamp for a channel, if any.
    pub async fn get_cutoff(&self, channel_id: Id<ChannelMarker>) -> Option<i64> {
        self.context_cutoffs.read().await.get(&channel_id).copied()
    }

    /// Read the prompt from disk or env var.
    async fn load_prompt_from_disk() -> Result<String> {
        let source = std::env::var(SYSTEM_PROMPT_ENV)
            .unwrap_or_else(|_| "prompt.txt".to_owned());

        if source.ends_with(".txt") {
            tokio::fs::read_to_string(&source)
                .await
                .with_context(|| format!("failed to read prompt file `{source}`"))
        } else {
            Ok(source)
        }
    }
}
