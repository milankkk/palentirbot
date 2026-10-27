use std::collections::HashMap;

use eyre::{Context as _, Result};
use serde::Serialize;
use tokio::sync::RwLock;
use twilight_interactions::command::{CommandOption, CreateOption};
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker},
    Id,
};

pub mod commands;
pub mod ollama;
pub mod queue;
pub mod search;

pub use ollama::{GenerationResult, LastGenerationStats, OllamaClient, StreamResult};
pub use queue::{AiQueue, DEFAULT_MAX_AI_QUEUE_SIZE};
pub use search::SearxClient;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, CommandOption, CreateOption)]
pub enum AiMode {
    #[default]
    #[option(name = "Thinking (Deep reasoning with live thoughts)", value = "thinking")]
    Thinking,
    #[option(name = "Fast (Quick direct response without reasoning)", value = "fast")]
    Fast,
}

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
/// - 0: Off (when disabled in server settings or AI_SAFESEARCH=0)
/// - 1: Moderate (default)
/// - 2: Strict
pub fn safesearch_level_for_guild(
    ctx: &crate::core::Context,
    guild_id: Option<Id<GuildMarker>>,
) -> u8 {
    let env_var = std::env::var("AI_SAFESEARCH").ok();
    let server_safesearch = guild_id
        .and_then(|gid| ctx.guild_settings(gid, |s| s.ai_safesearch))
        .unwrap_or(true);

    compute_safesearch_level(env_var.as_deref(), server_safesearch)
}

/// Pure helper for calculating effective SafeSearch level.
pub fn compute_safesearch_level(env_var: Option<&str>, server_safesearch: bool) -> u8 {
    let env_level: u8 = env_var
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    if !server_safesearch || env_level == 0 {
        0
    } else {
        env_level.max(1)
    }
}

const SYSTEM_PROMPT_ENV: &str = "AI_SYSTEM_PROMPT";

pub const SEARCH_SYSTEM_PROMPT: &str = r#"
[Web & Media Search Tools]
You have access to real-time search tools ONLY when needed:
- <search>specific search query</search> : Search the web for current events, live facts, recent osu! news/scores/tournaments/players, or facts you genuinely do not know.
- <browse>https://...</browse> : Read the full text of a specific URL from search results if snippets are insufficient.
- <search_image>specific image query</search_image> : Find a picture ONLY when the user explicitly asks for an image/picture/photo or asks what something looks like.
- <search_audio>specific audio query</search_audio> : Find audio ONLY when the user explicitly asks for an audio/sound file.
- <attach>direct_media_url</attach> : Attach one chosen direct image or audio URL from the search results to your final reply.

RULES FOR SEARCHING:
1. DO NOT search for casual conversation, greetings, jokes, opinions, or core osu! knowledge you already know (e.g., you already know peppy / Dean Herbert created osu!, what pp, beatmaps, mods, and danser are).
2. When you DO search, make the query specific. Because you are in an osu! Discord server, if a user asks about a player, mapper, beatmap, or community figure, include "osu!" in the query (e.g., <search>peppy Dean Herbert osu!</search> instead of just <search>peppy</search>).
3. NEVER say "Based on your query", "According to search results", or mention search tags in your final response. Stay 100% in your persona and answer naturally."#;

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
    /// Per-server cutoff timestamps (unix seconds). Messages created at or before
    /// this instant are excluded from the AI's conversation history across the entire guild.
    server_context_cutoffs: RwLock<HashMap<Id<GuildMarker>, i64>>,
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
            server_context_cutoffs: RwLock::new(HashMap::new()),
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

    /// Return the cached system-prompt template (with search instructions appended only if not already present).
    pub async fn get_prompt(&self) -> String {
        let base = self.cached_prompt.read().await.clone();
        if self.search.is_enabled() && !base.contains("<search>") {
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

    /// Mark a cutoff for an entire server — messages older than or equal to `now` in this guild will be ignored.
    pub async fn clear_server_context(&self, guild_id: Id<GuildMarker>) {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        self.server_context_cutoffs
            .write()
            .await
            .insert(guild_id, now);
    }

    /// Return the effective cutoff timestamp for a channel/guild, if any.
    pub async fn get_cutoff(&self, channel_id: Id<ChannelMarker>, guild_id: Option<Id<GuildMarker>>) -> Option<i64> {
        let channel_cutoff = self.context_cutoffs.read().await.get(&channel_id).copied();
        let server_cutoff = match guild_id {
            Some(gid) => self.server_context_cutoffs.read().await.get(&gid).copied(),
            None => None,
        };
        match (channel_cutoff, server_cutoff) {
            (Some(c), Some(s)) => Some(c.max(s)),
            (Some(c), None) => Some(c),
            (None, Some(s)) => Some(s),
            (None, None) => None,
        }
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
    fn test_compute_safesearch_level_default_on() {
        // Defaults to 1 (Moderate / ON) when unset
        assert_eq!(compute_safesearch_level(None, true), 1);
        // Explicitly set to 2 (Strict)
        assert_eq!(compute_safesearch_level(Some("2"), true), 2);
        // Explicitly disabled via env var "0"
        assert_eq!(compute_safesearch_level(Some("0"), true), 0);
        // Disabled via server settings
        assert_eq!(compute_safesearch_level(None, false), 0);
        assert_eq!(compute_safesearch_level(Some("2"), false), 0);
    }
}
