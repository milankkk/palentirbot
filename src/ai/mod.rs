use std::collections::HashMap;

use eyre::{Context as _, Result};
use serde::Serialize;
use tokio::sync::RwLock;
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker},
    Id,
};

pub mod commands;
pub mod ollama;
pub mod queue;
pub mod search;

pub use ollama::{GenerationResult, LastGenerationStats, OllamaClient};
pub use queue::{AiQueue, DEFAULT_MAX_AI_QUEUE_SIZE};
pub use search::SearxClient;

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

pub const AI_DISABLED_REPLY: &str =
    "ℹ️ The AI module is disabled in this server. An administrator can enable it with `/setup ai enabled:True`.";

/// Checks if AI is globally enabled via the `AI_ENABLED` environment variable (defaults to true).
pub fn is_globally_enabled() -> bool {
    std::env::var("AI_ENABLED")
        .map(|v| !v.eq_ignore_ascii_case("false") && v != "0")
        .unwrap_or(true)
}

/// Checks if AI is active and permitted for this guild (or DM).
pub fn is_enabled_for_guild(ctx: &crate::core::Context, guild_id: Option<Id<GuildMarker>>) -> bool {
    if ctx.ai().is_none() {
        return false;
    }
    match guild_id {
        Some(gid) => ctx.guild_settings(gid, |s| s.ai_enabled).unwrap_or(true),
        None => true, // Enabled in DMs if AI is globally enabled
    }
}

/// Determines the effective SafeSearch level:
/// - 0: Off (default unless overridden via AI_SAFESEARCH or server settings)
/// - 1: Moderate
/// - 2: Strict
pub fn safesearch_level_for_guild(
    ctx: &crate::core::Context,
    guild_id: Option<Id<GuildMarker>>,
) -> u8 {
    let env_level: u8 = std::env::var("AI_SAFESEARCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    if env_level == 0 {
        return 0;
    }

    if let Some(gid) = guild_id {
        let server_safesearch = ctx.guild_settings(gid, |s| s.ai_safesearch).unwrap_or(false);
        if !server_safesearch {
            return 0;
        }
    }

    env_level
}

const SYSTEM_PROMPT_ENV: &str = "AI_SYSTEM_PROMPT";

pub const SEARCH_SYSTEM_PROMPT: &str = r#"

SEARCH & MEDIA RETRIEVAL INSTRUCTIONS:
You have access to real-time search and media retrieval tools using XML tags:
- To search the web for facts, current events, osu! news, or links: write `<search>your query</search>`.
- To search for pictures when the user asks for an image/picture or when a visual diagram/photo directly answers the request: write `<search_image>image query</search_image>`.
- To search for audio or music files/sounds: write `<search_audio>audio query</search_audio>`.
- To attach an image, audio, or video file to your final message: write `<attach>direct_media_url</attach>`. Supported extensions: .jpg, .png, .webp, .gif, .mp3, .ogg, .wav, .mp4.

STRICT RULES:
1. Do NOT search for or attach images/audio/links on normal conversational messages. ONLY use search tags when the user explicitly asks for an image, picture, audio, link, or current real-time information, or when visual/audio media is necessary to fulfill their request.
2. If you output a search tag, wait for the search results before presenting your final answer."#;

pub struct AiClient {
    ollama: OllamaClient,
    search: SearxClient,
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
            search: SearxClient::new(),
            queue: AiQueue::new(max_queue_size),
            cached_prompt: RwLock::new(prompt),
            context_cutoffs: RwLock::new(HashMap::new()),
        }
    }

    /// Access the FIFO queue controlling generation concurrency.
    pub fn queue(&self) -> &AiQueue {
        &self.queue
    }

    /// Access SearXNG search client.
    pub fn search(&self) -> &SearxClient {
        &self.search
    }

    /// Access Ollama client.
    pub fn ollama(&self) -> &OllamaClient {
        &self.ollama
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

    /// Return the cached system-prompt template (with search instructions appended if SearXNG is enabled).
    pub async fn get_prompt(&self) -> String {
        let base = self.cached_prompt.read().await.clone();
        if self.search.is_enabled() {
            format!("{base}\n{SEARCH_SYSTEM_PROMPT}")
        } else {
            base
        }
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
