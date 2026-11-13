use std::collections::{hash_map::Entry, HashMap, VecDeque};

use eyre::{Context as _, Result};
use serde::Serialize;
use tokio::sync::{Mutex, RwLock};
use twilight_model::id::{
    marker::{ChannelMarker, MessageMarker},
    Id,
};

mod ollama;
pub mod osu_tools;
mod queue;
pub mod search;

pub use ollama::{
    describe_ai_error, normalize_num_predict, AiAttachmentFile, AiPersonaMode, AiReply,
    AiStreamEvent, AiThinkDepth, LastGenerationStats, OllamaClient, RuntimeAiConfig,
};
pub use queue::{AiQueue, DEFAULT_MAX_AI_QUEUE_SIZE};
#[allow(unused_imports)]
pub use search::WebSearchClient;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum SearchMode {
    #[default]
    Auto,
    Force,
    Off,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub images: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<serde_json::Value>,
}

impl AiMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_owned(),
            content: content.into(),
            images: Vec::new(),
            tool_calls: Vec::new(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_owned(),
            content: content.into(),
            images: Vec::new(),
            tool_calls: Vec::new(),
        }
    }

    pub fn user_with_images(content: impl Into<String>, images: Vec<String>) -> Self {
        Self {
            role: "user".to_owned(),
            content: content.into(),
            images,
            tool_calls: Vec::new(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_owned(),
            content: content.into(),
            images: Vec::new(),
            tool_calls: Vec::new(),
        }
    }

    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<serde_json::Value>,
    ) -> Self {
        Self {
            role: "assistant".to_owned(),
            content: content.into(),
            images: Vec::new(),
            tool_calls,
        }
    }

    pub fn tool(content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_owned(),
            content: content.into(),
            images: Vec::new(),
            tool_calls: Vec::new(),
        }
    }
}

/// Encode bytes as standard RFC 4648 base64 for multimodal vision payloads.
pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(TABLE[(triple >> 18) & 0x3F] as char);
        out.push(TABLE[(triple >> 12) & 0x3F] as char);
        out.push(if chunk.len() > 1 { TABLE[(triple >> 6) & 0x3F] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[triple & 0x3F] as char } else { '=' });
    }
    out
}

/// Sanitize untrusted text before inserting into LLM prompts by stripping null
/// bytes, disallowed control characters, and ChatML/special tokenizer delimiters.
pub fn sanitize_llm_text(text: &str) -> String {
    text.chars()
        .filter(|&c| c == '\n' || c == '\r' || c == '\t' || !c.is_control())
        .collect::<String>()
        .replace("<|im_start|>", "")
        .replace("<|im_end|>", "")
        .replace("<|endoftext|>", "")
}

const SYSTEM_PROMPT_ENV: &str = "AI_SYSTEM_PROMPT";
const CUTOFFS_FILE_PATH: &str = "data/ai_cutoffs.json";
const FALLBACK_SYSTEM_PROMPT: &str =
    "You are a helpful, witty Discord assistant. Reply concisely in plain text.";

pub struct AiClient {
    ollama: OllamaClient,
    queue: AiQueue,
    /// The cached raw prompt template (with `{username}` / `{user_id}` placeholders).
    cached_prompt: RwLock<String>,
    /// Per-channel cutoff timestamps (unix seconds). Messages created at or before
    /// this instant are excluded from the AI's conversation history, effectively
    /// "clearing" the context for that channel.
    context_cutoffs: RwLock<HashMap<Id<ChannelMarker>, i64>>,
    cutoffs_save_lock: Mutex<()>,
    /// Bounded FIFO cache mapping `/ask` reply message IDs to their initiating
    /// user prompt (Discord does not store slash command text in channel history).
    slash_turns: RwLock<SlashTurnHistory>,
}

const MAX_SLASH_TURNS: usize = 256;

struct SlashTurnHistory {
    prompts: HashMap<Id<MessageMarker>, String>,
    order: VecDeque<Id<MessageMarker>>,
}

impl AiClient {
    pub async fn new() -> Self {
        let prompt = match Self::load_prompt_from_disk().await {
            Ok(p) => p,
            Err(err) => {
                tracing::error!(?err, "failed to load AI system prompt on startup; using fallback prompt");
                FALLBACK_SYSTEM_PROMPT.to_owned()
            }
        };

        let max_queue_size = std::env::var("AI_MAX_QUEUE_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n: &usize| n > 0)
            .unwrap_or(DEFAULT_MAX_AI_QUEUE_SIZE);

        let cutoffs = Self::load_cutoffs_from_disk().await;
        let ollama = OllamaClient::new();
        let configured_model = ollama.snapshot_config().model;

        match ollama.list_models().await {
            Ok(models) => {
                let model_installed = models.iter().any(|m| {
                    m == &configured_model
                        || m.strip_suffix(":latest") == Some(configured_model.as_str())
                });
                if model_installed {
                    tracing::info!(
                        url = %ollama.url(),
                        model = %configured_model,
                        installed_models = models.len(),
                        "Ollama AI backend reachable"
                    );
                } else {
                    tracing::warn!(
                        url = %ollama.url(),
                        model = %configured_model,
                        installed_models = models.len(),
                        "Ollama AI backend reachable, but configured model was not found in /api/tags"
                    );
                }
            }
            Err(err) => {
                tracing::warn!(
                    ?err,
                    url = %ollama.url(),
                    model = %configured_model,
                    "Ollama AI backend unreachable on startup; AI commands will retry when invoked"
                );
            }
        }

        Self {
            ollama,
            queue: AiQueue::new(max_queue_size),
            cached_prompt: RwLock::new(prompt),
            context_cutoffs: RwLock::new(cutoffs),
            cutoffs_save_lock: Mutex::new(()),
            slash_turns: RwLock::new(SlashTurnHistory {
                prompts: HashMap::new(),
                order: VecDeque::new(),
            }),
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

    pub async fn ask_with_stream(
        &self,
        channel_id: Id<ChannelMarker>,
        messages: &[AiMessage],
        search_mode: SearchMode,
        osu: Option<&rosu_v2::Osu>,
        stream_tx: Option<tokio::sync::mpsc::UnboundedSender<AiStreamEvent>>,
    ) -> Result<AiReply> {
        self.ollama
            .ask_with_stream(channel_id, messages, search_mode, osu, stream_tx)
            .await
    }

    /// Return statistics from the most recent generation evaluated by Ollama for this channel.
    pub async fn last_stats(&self, channel_id: Id<ChannelMarker>) -> Option<LastGenerationStats> {
        self.ollama.last_stats(channel_id).await
    }

    /// The model identifier currently configured.
    pub fn model_name(&self) -> String {
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

    /// Snapshot of runtime-mutable AI configuration.
    pub fn runtime_config(&self) -> RuntimeAiConfig {
        self.ollama.snapshot_config()
    }

    /// Active persona mode override for a channel.
    pub fn channel_mode(&self, channel_id: Id<ChannelMarker>) -> AiPersonaMode {
        self.ollama.channel_mode(channel_id)
    }

    /// Mutate and atomically persist runtime AI configuration to disk.
    pub async fn update_runtime_config<F>(&self, f: F) -> Result<RuntimeAiConfig>
    where
        F: FnOnce(&mut RuntimeAiConfig),
    {
        self.ollama.update_runtime(f).await
    }

    /// Query installed Ollama models via `/api/tags`.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        self.ollama.list_models().await
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

    /// Record the initiating user prompt for a public `/ask` reply message so
    /// subsequent requests in the channel see both sides of the exchange.
    pub async fn record_slash_turn(&self, reply_msg_id: Id<MessageMarker>, user_prompt: String) {
        if user_prompt.trim().is_empty() {
            return;
        }
        let mut guard = self.slash_turns.write().await;
        match guard.prompts.entry(reply_msg_id) {
            Entry::Vacant(slot) => {
                slot.insert(user_prompt);
                guard.order.push_back(reply_msg_id);
                while guard.order.len() > MAX_SLASH_TURNS {
                    if let Some(oldest) = guard.order.pop_front() {
                        guard.prompts.remove(&oldest);
                    }
                }
            }
            Entry::Occupied(mut slot) => {
                slot.insert(user_prompt);
            }
        }
    }

    /// Snapshot recorded `/ask` user prompts keyed by bot reply message ID.
    pub async fn snapshot_slash_turns(&self) -> HashMap<Id<MessageMarker>, String> {
        self.slash_turns.read().await.prompts.clone()
    }

    /// Mark a cutoff for a channel — messages older than or equal to `now` will be ignored.
    pub async fn clear_context(&self, channel_id: Id<ChannelMarker>) {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let _save_guard = self.cutoffs_save_lock.lock().await;
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
        let path = std::path::Path::new(CUTOFFS_FILE_PATH);
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, json).await?;
        tokio::fs::rename(&tmp, path).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
