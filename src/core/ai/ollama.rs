use std::collections::HashMap;
use std::time::Duration;

use eyre::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use twilight_model::id::{marker::ChannelMarker, Id};

use super::AiMessage;

pub const DEFAULT_MODEL: &str = "huihui_ai/qwen3.5-abliterated:9b";
/// Kept small enough that weights + KV cache fit in 8 GB of VRAM (no CPU spill / reloads).
pub const DEFAULT_NUM_CTX: u32 = 12_288;
/// How long Ollama keeps the model resident after a request.
pub const DEFAULT_KEEP_ALIVE: &str = "30m";
pub const DEFAULT_REPEAT_PENALTY: f32 = 1.15;
pub const DEFAULT_REPEAT_LAST_N: i32 = 256;
pub const DEFAULT_PRESENCE_PENALTY: f32 = 0.3;
pub const DEFAULT_FREQUENCY_PENALTY: f32 = 0.1;
pub const DEFAULT_TEMPERATURE: f32 = 0.6;
pub const DEFAULT_TOP_P: f32 = 0.95;
pub const DEFAULT_NUM_PREDICT: i32 = 2048;
pub const DEFAULT_TOP_K: u32 = 20;
pub const DEFAULT_MIN_P: f32 = 0.0;

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
    top_k: u32,
    min_p: f32,
    think_enabled: bool,
    keep_alive: String,
    search: super::search::WebSearchClient,
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
    top_k: u32,
    #[serde(skip_serializing_if = "is_zero_f32")]
    min_p: f32,
}

static WEB_SEARCH_TOOL: once_cell::sync::Lazy<[serde_json::Value; 1]> = once_cell::sync::Lazy::new(|| {
    [serde_json::json!({
        "type": "function",
        "function": {
            "name": "web_search",
            "description": "Search Google / DuckDuckGo for live facts, current events, tournament winners, scores, news, or player lore",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The exact search query to look up on the web"
                    }
                },
                "required": ["query"]
            }
        }
    })]
});

#[derive(Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    messages: &'a [AiMessage],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    think: Option<bool>,
    keep_alive: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [serde_json::Value]>,
    options: OllamaOptions,
}

#[derive(Deserialize, Debug, Clone)]
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

#[derive(Deserialize, Debug, Clone)]
struct OllamaResponseMessage {
    content: String,
    #[allow(dead_code)]
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Deserialize, Debug, Clone)]
struct OllamaToolCall {
    function: OllamaFunctionCall,
}

#[derive(Deserialize, Debug, Clone)]
struct OllamaFunctionCall {
    name: String,
    arguments: serde_json::Value,
}

impl OllamaClient {
    pub fn new() -> Self {
        let url =
            std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".to_owned());

        let model =
            std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned());

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

        let top_k = std::env::var("OLLAMA_TOP_K")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_TOP_K);

        let min_p = std::env::var("OLLAMA_MIN_P")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MIN_P);

        let think_enabled = std::env::var("OLLAMA_THINK_ENABLED")
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
            .unwrap_or(true);

        let keep_alive = std::env::var("OLLAMA_KEEP_ALIVE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_KEEP_ALIVE.to_owned());

        let timeout_secs = std::env::var("OLLAMA_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(180);

        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .expect("failed to build AI HTTP client");

        let search = super::search::WebSearchClient::new();

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
            top_k,
            min_p,
            think_enabled,
            keep_alive,
            search,
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
        let mut working_messages = messages.to_vec();

        // 1. If the user pasted a direct http/https link, auto-browse it into system context
        if let Some(last_user) = working_messages.iter().rev().find(|m| m.role == "user") {
            if let Some(url) = super::search::extract_first_url(&last_user.content) {
                if let Ok(page_text) = self.search.browse_url(&url, 2_000).await {
                    if !page_text.is_empty() {
                        if let Some(sys) = working_messages.first_mut() {
                            if sys.role == "system" {
                                sys.content.push_str(&format!(
                                    "\n\n[Webpage Content from {url}]:\n{page_text}"
                                ));
                            }
                        }
                    }
                }
            }
        }

        // 2. First pass with native tool support enabled
        let tools_slice: &[serde_json::Value] = &*WEB_SEARCH_TOOL;
        let response = self
            .send_chat_request(&working_messages, self.think_enabled, Some(tools_slice), 350)
            .await?;
        self.record_stats(channel_id, &response).await;

        let mut search_context = String::new();

        // Check native tool calls from Ollama
        if let Some(ref tool_calls) = response.message.tool_calls {
            for call in tool_calls {
                if call.function.name == "web_search" {
                    let query = if let Some(q_str) = call.function.arguments.get("query").and_then(|v| v.as_str()) {
                        q_str.to_owned()
                    } else if let Some(q_raw) = call.function.arguments.as_str() {
                        q_raw.to_owned()
                    } else {
                        String::new()
                    };

                    let clean_query = query.trim();
                    if !clean_query.is_empty() {
                        tracing::info!(%channel_id, query = %clean_query, "Executing AI web search via tool call");
                        if let Ok(results) = self.search.search(clean_query).await {
                            search_context.push_str(&super::search::format_search_results(
                                clean_query, &results, 4,
                            ));
                        }
                    }
                }
            }
        }

        // Fallback: Also check if the model wrote <search> or <browse> tags in thinking or content
        let combined_text = format!(
            "{}\n{}",
            response.message.thinking.as_deref().unwrap_or(""),
            response.message.content
        );
        let fallback_actions = super::search::extract_search_actions(&combined_text, 2);
        for action in fallback_actions {
            match action {
                super::search::AiSearchAction::Search(query) => {
                    if !search_context.contains(&query) {
                        tracing::info!(%channel_id, %query, "Executing AI web search via fallback tag");
                        if let Ok(results) = self.search.search(&query).await {
                            search_context.push_str(&super::search::format_search_results(
                                &query, &results, 4,
                            ));
                        }
                    }
                }
                super::search::AiSearchAction::Browse(url) => {
                    tracing::info!(%channel_id, %url, "Executing AI URL browse via fallback tag");
                    if let Ok(text) = self.search.browse_url(&url, 1_800).await {
                        search_context.push_str(&format!("Content from {url}:\n{text}\n"));
                    }
                }
            }
        }

        // 3. If web search or browsing was performed, inject the facts and synthesize final answer
        if !search_context.is_empty() {
            let mut followup_messages = working_messages;
            if let Some(first) = followup_messages.first_mut() {
                if first.role == "system" {
                    first.content.push_str(&format!(
                        "\n\n[Live Web Search Results]:\n{search_context}\nAnswer the user's question using these verified facts with your normal witty Discord tone, never sound like a formal report."
                    ));
                }
            }
            let final_resp = self
                .send_chat_request(&followup_messages, false, None, 350)
                .await?;
            self.record_stats(channel_id, &final_resp).await;
            return Ok(final_resp.message.content);
        }

        // 4. If no search was needed and the model already generated a non-empty response, return it
        if !response.message.content.trim().is_empty() {
            return Ok(response.message.content);
        }

        // 5. If content was empty (e.g. bounded thinking pass), synthesize final answer with think=false
        let mut followup_messages = working_messages;
        if let Some(first) = followup_messages.first_mut() {
            if first.role == "system" {
                if let Some(thinking) = response
                    .message
                    .thinking
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                {
                    first.content.push_str(
                        "\n\n[Internal reasoning notes — use these ideas to write your final 1-2 sentence Discord reply, do NOT repeat verbatim]:\n",
                    );
                    first.content.push_str(thinking);
                }
            }
        }

        let fallback = self
            .send_chat_request(&followup_messages, false, None, 350)
            .await?;
        self.record_stats(channel_id, &fallback).await;
        Ok(fallback.message.content)
    }

    async fn record_stats(&self, channel_id: Id<ChannelMarker>, response: &OllamaResponse) {
        if let Some(prompt_tokens) = response.prompt_eval_count {
            let stats = LastGenerationStats {
                prompt_tokens,
                response_tokens: response.eval_count.unwrap_or(0),
                eval_duration_ms: response.eval_duration.map(|ns| ns / 1_000_000).unwrap_or(0),
                total_duration_ms: response.total_duration.map(|ns| ns / 1_000_000).unwrap_or(0),
            };
            self.last_stats.write().await.insert(channel_id, stats);
        }
    }

    async fn send_chat_request(
        &self,
        messages: &[AiMessage],
        think: bool,
        tools: Option<&[serde_json::Value]>,
        num_predict: i32,
    ) -> Result<OllamaResponse> {
        let request = OllamaRequest {
            model: &self.model,
            messages,
            stream: false,
            think: Some(think),
            keep_alive: &self.keep_alive,
            tools,
            options: OllamaOptions {
                num_ctx: self.num_ctx,
                num_predict: Some(num_predict),
                repeat_penalty: self.repeat_penalty,
                repeat_last_n: self.repeat_last_n,
                presence_penalty: self.presence_penalty,
                frequency_penalty: self.frequency_penalty,
                temperature: self.temperature,
                top_p: self.top_p,
                top_k: self.top_k,
                min_p: self.min_p,
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
        let parsed: OllamaResponse =
            serde_json::from_str(&body_str).context("failed to deserialize Ollama response")?;

        Ok(parsed)
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
