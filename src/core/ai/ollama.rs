use std::collections::HashMap;
use std::time::Duration;

use eyre::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use twilight_model::id::{marker::ChannelMarker, Id};

use super::AiMessage;

pub const DEFAULT_NUM_CTX: u32 = 40_960;
pub const DEFAULT_REPEAT_PENALTY: f32 = 1.15;
pub const DEFAULT_TEMPERATURE: f32 = 0.7;

#[derive(Clone, Debug, Default)]
pub struct LastGenerationStats {
    pub prompt_tokens: usize,
    pub response_tokens: usize,
    pub eval_duration_ms: u64,
    #[allow(dead_code)]
    pub total_duration_ms: u64,
}

pub struct OllamaClient {
    client: Client,
    url: String,
    model: String,
    num_ctx: u32,
    repeat_penalty: f32,
    temperature: f32,
    last_stats: tokio::sync::RwLock<HashMap<Id<ChannelMarker>, LastGenerationStats>>,
}

#[derive(Serialize)]
struct OllamaOptions {
    num_ctx: u32,
    repeat_penalty: f32,
    repeat_last_n: i32,
    temperature: f32,
}

#[derive(Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    messages: &'a [AiMessage],
    stream: bool,
    options: OllamaOptions,
}

#[derive(Deserialize)]
struct OllamaResponse {
    message: OllamaResponseMessage,
    #[serde(default)]
    prompt_eval_count: Option<usize>,
    #[serde(default)]
    eval_count: Option<usize>,
    #[serde(default)]
    eval_duration: Option<u64>,
    #[serde(default)]
    total_duration: Option<u64>,
}

#[derive(Deserialize)]
struct OllamaResponseMessage {
    content: String,
}

impl OllamaClient {
    pub fn new() -> Self {
        let url =
            std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".to_owned());

        let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "gpt-oss".to_owned());

        let num_ctx = std::env::var("OLLAMA_NUM_CTX")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_NUM_CTX);

        let repeat_penalty = std::env::var("OLLAMA_REPEAT_PENALTY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_REPEAT_PENALTY);

        let temperature = std::env::var("OLLAMA_TEMPERATURE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_TEMPERATURE);

        let timeout_secs = std::env::var("OLLAMA_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(120);

        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .expect("failed to build AI HTTP client");

        Self {
            client,
            url,
            model,
            num_ctx,
            repeat_penalty,
            temperature,
            last_stats: tokio::sync::RwLock::new(HashMap::new()),
        }
    }

    pub fn num_ctx(&self) -> u32 {
        self.num_ctx
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }

    pub async fn last_stats(&self, channel_id: Id<ChannelMarker>) -> Option<LastGenerationStats> {
        self.last_stats.read().await.get(&channel_id).cloned()
    }

    pub async fn ask(&self, channel_id: Id<ChannelMarker>, messages: &[AiMessage]) -> Result<String> {
        let request = OllamaRequest {
            model: &self.model,
            messages,
            stream: false,
            options: OllamaOptions {
                num_ctx: self.num_ctx,
                repeat_penalty: self.repeat_penalty,
                repeat_last_n: 128,
                temperature: self.temperature,
            },
        };

        let response = self
            .client
            .post(format!("{}/api/chat", self.url.trim_end_matches('/')))
            .json(&request)
            .send()
            .await
            .context("failed to contact Ollama")?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            eyre::bail!("Ollama returned HTTP {status}: {error_text}");
        }

        let response = response
            .json::<OllamaResponse>()
            .await
            .context("failed to deserialize Ollama response")?;

        if let Some(prompt_tokens) = response.prompt_eval_count {
            let stats = LastGenerationStats {
                prompt_tokens,
                response_tokens: response.eval_count.unwrap_or(0),
                eval_duration_ms: response.eval_duration.map(|ns| ns / 1_000_000).unwrap_or(0),
                total_duration_ms: response.total_duration.map(|ns| ns / 1_000_000).unwrap_or(0),
            };
            self.last_stats.write().await.insert(channel_id, stats);
        }

        Ok(response.message.content)
    }
}
