use std::collections::HashMap;
use std::time::Duration;

use eyre::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use twilight_model::id::{marker::ChannelMarker, Id};

use super::AiMessage;

pub const DEFAULT_NUM_CTX: u32 = 40_960;
pub const DEFAULT_REPEAT_PENALTY: f32 = 1.15;
pub const DEFAULT_REPEAT_LAST_N: i32 = 256;
pub const DEFAULT_PRESENCE_PENALTY: f32 = 0.0;
pub const DEFAULT_FREQUENCY_PENALTY: f32 = 0.0;
pub const DEFAULT_TEMPERATURE: f32 = 0.8;
pub const DEFAULT_TOP_P: f32 = 0.9;
pub const DEFAULT_NUM_PREDICT: i32 = 1024;

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
    num_predict: i32,
    repeat_penalty: f32,
    repeat_last_n: i32,
    presence_penalty: f32,
    frequency_penalty: f32,
    temperature: f32,
    top_p: f32,
    last_stats: tokio::sync::RwLock<HashMap<Id<ChannelMarker>, LastGenerationStats>>,
}

fn is_zero_f32(val: &f32) -> bool {
    *val == 0.0
}

#[derive(Serialize)]
struct OllamaOptions {
    num_ctx: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<i32>,
    repeat_penalty: f32,
    repeat_last_n: i32,
    #[serde(skip_serializing_if = "is_zero_f32")]
    presence_penalty: f32,
    #[serde(skip_serializing_if = "is_zero_f32")]
    frequency_penalty: f32,
    temperature: f32,
    top_p: f32,
}

#[derive(Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    messages: &'a [AiMessage],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    think: Option<bool>,
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
    #[allow(dead_code)]
    #[serde(default)]
    thinking: Option<String>,
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

        let num_predict = std::env::var("OLLAMA_NUM_PREDICT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_NUM_PREDICT);

        let repeat_penalty = std::env::var("OLLAMA_REPEAT_PENALTY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_REPEAT_PENALTY);

        let repeat_last_n = std::env::var("OLLAMA_REPEAT_LAST_N")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_REPEAT_LAST_N);

        let presence_penalty = std::env::var("OLLAMA_PRESENCE_PENALTY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_PRESENCE_PENALTY);

        let frequency_penalty = std::env::var("OLLAMA_FREQUENCY_PENALTY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_FREQUENCY_PENALTY);

        let temperature = std::env::var("OLLAMA_TEMPERATURE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_TEMPERATURE);

        let top_p = std::env::var("OLLAMA_TOP_P")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_TOP_P);

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
            num_predict,
            repeat_penalty,
            repeat_last_n,
            presence_penalty,
            frequency_penalty,
            temperature,
            top_p,
            last_stats: tokio::sync::RwLock::new(HashMap::new()),
        }
    }

    pub fn num_ctx(&self) -> u32 {
        self.num_ctx
    }

    pub fn num_predict(&self) -> i32 {
        self.num_predict
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
            think: Some(false),
            options: OllamaOptions {
                num_ctx: self.num_ctx,
                num_predict: Some(self.num_predict),
                repeat_penalty: self.repeat_penalty,
                repeat_last_n: self.repeat_last_n,
                presence_penalty: self.presence_penalty,
                frequency_penalty: self.frequency_penalty,
                temperature: self.temperature,
                top_p: self.top_p,
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
            let error_bytes = response.bytes().await.unwrap_or_default();
            let error_text = String::from_utf8_lossy(&error_bytes);
            eyre::bail!("Ollama returned HTTP {status}: {error_text}");
        }

        // Lossy UTF-8 decode handles models/tokenizers that output broken or raw byte tokens,
        // preventing serde_json from failing with "invalid utf-8 sequence".
        let raw_bytes = response
            .bytes()
            .await
            .context("failed to read Ollama response bytes")?;
        let body_str = String::from_utf8_lossy(&raw_bytes);
        let response: OllamaResponse = serde_json::from_str(&body_str)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_non_utf8_bytes_in_response() {
        // Construct a JSON payload that contains invalid non-UTF-8 bytes inside content
        let mut raw = Vec::new();
        raw.extend_from_slice(br#"{"message":{"role":"assistant","content":"hello "#);
        raw.extend_from_slice(&[0xff, 0xfe, 0x80]); // Invalid UTF-8 byte sequences
        raw.extend_from_slice(br#" world"}}"#);

        // Standard serde_json::from_slice will fail on invalid UTF-8
        let direct_res: Result<OllamaResponse, _> = serde_json::from_slice(&raw);
        assert!(direct_res.is_err(), "direct from_slice should fail on invalid UTF-8");

        // Our String::from_utf8_lossy method safely replaces invalid bytes and succeeds
        let body_str = String::from_utf8_lossy(&raw);
        let lossy_res: Result<OllamaResponse, _> = serde_json::from_str(&body_str);
        assert!(lossy_res.is_ok(), "lossy from_str should succeed");
        let parsed = lossy_res.unwrap();
        assert!(parsed.message.content.starts_with("hello "));
        assert!(parsed.message.content.ends_with(" world"));
    }
}
