use std::collections::HashMap;
use std::time::{Duration, Instant};

use eyre::{Context as _, Result};
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use twilight_model::id::{marker::ChannelMarker, Id};

use super::search::{
    download_media, extract_attach_tags, extract_search_tags, fetch_webpage_clean_text,
    find_closed_search_tag, AiSearchTag, SearxClient,
};
use super::{AiMessage, AiMode};

pub const DEFAULT_MODEL: &str = "huihui_ai/deepseek-r1-abliterated:8b-0528-qwen3";
pub const DEFAULT_NUM_CTX: u32 = 40_960;
pub const DEFAULT_REPEAT_PENALTY: f32 = 1.15;
pub const DEFAULT_REPEAT_LAST_N: i32 = 256;
pub const DEFAULT_PRESENCE_PENALTY: f32 = 0.0;
pub const DEFAULT_FREQUENCY_PENALTY: f32 = 0.0;
pub const DEFAULT_TEMPERATURE: f32 = 0.6;
pub const DEFAULT_TOP_P: f32 = 0.95;
pub const DEFAULT_NUM_PREDICT: i32 = 8192;

#[derive(Clone, Debug, Default)]
pub struct LastGenerationStats {
    pub prompt_tokens: usize,
    pub response_tokens: usize,
    pub thinking_tokens: usize,
    pub eval_duration_ms: u64,
    pub total_duration_ms: u64,
    pub load_duration_ms: u64,
}

#[derive(Clone, Debug)]
pub enum AiStatusUpdate {
    LoadingModel,
    GeneratingFast,
    Reasoning {
        elapsed_secs: u64,
        thought_snippet: String,
    },
    WritingResponse,
    Searching {
        query: String,
        category: String,
        safesearch_on: bool,
    },
    DownloadingMedia,
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
    think_enabled: bool,
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

#[derive(Deserialize, Debug, Default)]
pub struct OllamaStreamChunk {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub message: Option<OllamaStreamMessage>,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub prompt_eval_count: Option<usize>,
    #[serde(default)]
    pub eval_count: Option<usize>,
    #[serde(default)]
    pub eval_duration: Option<u64>,
    #[serde(default)]
    pub total_duration: Option<u64>,
    #[serde(default)]
    pub load_duration: Option<u64>,
}

#[derive(Deserialize, Debug, Default)]
pub struct OllamaStreamMessage {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub thinking: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GenerationResult {
    pub content: String,
    pub attachment: Option<(String, Vec<u8>)>,
}

#[derive(Debug, Clone)]
pub enum StreamResult {
    Completed(String),
    SearchIntercepted {
        tag: AiSearchTag,
        partial_thinking: String,
        partial_content: String,
    },
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

        let think_enabled = std::env::var("OLLAMA_THINK_ENABLED")
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
            .unwrap_or(true);

        let timeout_secs = std::env::var("OLLAMA_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(180);

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
            think_enabled,
            last_stats: tokio::sync::RwLock::new(HashMap::new()),
        }
    }
}

impl Default for OllamaClient {
    fn default() -> Self {
        Self::new()
    }
}

impl OllamaClient {
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

    /// Ask Ollama with streaming NDJSON chunks, updating status callback and tracking thinking/load metrics.
    /// When `allow_search_interception` is true, intercepts closing search tags (`</search*>` in thinking or content)
    /// immediately so that search results can be injected in real time while thinking.
    pub async fn ask_stream<F>(
        &self,
        channel_id: Id<ChannelMarker>,
        messages: &[AiMessage],
        allow_search_interception: bool,
        mode: AiMode,
        mut on_status: F,
    ) -> Result<StreamResult>
    where
        F: FnMut(AiStatusUpdate) + Send,
    {
        let is_fast = mode == AiMode::Fast;
        if is_fast {
            on_status(AiStatusUpdate::GeneratingFast);
        } else {
            on_status(AiStatusUpdate::LoadingModel);
        }

        let request = OllamaRequest {
            model: &self.model,
            messages,
            stream: true,
            think: Some(!is_fast && self.think_enabled),
            options: OllamaOptions {
                num_ctx: self.num_ctx,
                num_predict: Some(if is_fast { 1024 } else { self.num_predict }),
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

        let start_time = Instant::now();
        let mut byte_stream = response.bytes_stream();
        let mut buffer = Vec::new();

        let mut accumulated_content = String::new();
        let mut accumulated_thinking = String::new();
        let mut thinking_tokens: usize = 0;
        let mut response_tokens: usize = 0;
        let mut final_stats = LastGenerationStats::default();

        let mut in_reasoning_state = false;
        let mut in_writing_state = false;

        while let Some(chunk_res) = byte_stream.next().await {
            let chunk_bytes = chunk_res.context("error reading Ollama chunk")?;
            buffer.extend_from_slice(&chunk_bytes);

            while let Some(newline_pos) = buffer.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = buffer.drain(..=newline_pos).collect();
                let line_str = String::from_utf8_lossy(&line_bytes);
                let trimmed = line_str.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let chunk: OllamaStreamChunk = match serde_json::from_str(trimmed) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::trace!(?e, "skipping non-json stream line: {trimmed}");
                        continue;
                    }
                };

                if let Some(msg) = chunk.message {
                    // Check if thinking stream is received via `message.thinking`
                    if let Some(ref th) = msg.thinking {
                        if !th.is_empty() {
                            accumulated_thinking.push_str(th);
                            thinking_tokens += 1;
                            in_reasoning_state = true;

                            // Real-time search interception while thinking
                            if allow_search_interception {
                                if let Some(tag) = find_closed_search_tag(&accumulated_thinking) {
                                    return Ok(StreamResult::SearchIntercepted {
                                        tag,
                                        partial_thinking: accumulated_thinking,
                                        partial_content: accumulated_content,
                                    });
                                }
                            }

                            let snippet = format_thought_snippet(&accumulated_thinking);
                            on_status(AiStatusUpdate::Reasoning {
                                elapsed_secs: start_time.elapsed().as_secs(),
                                thought_snippet: snippet,
                            });
                        }
                    }

                    // Check if content is received
                    if !msg.content.is_empty() {
                        accumulated_content.push_str(&msg.content);
                        response_tokens += 1;

                        // Real-time search interception if tag is output in content
                        if allow_search_interception {
                            if let Some(tag) = find_closed_search_tag(&accumulated_content) {
                                return Ok(StreamResult::SearchIntercepted {
                                    tag,
                                    partial_thinking: accumulated_thinking,
                                    partial_content: accumulated_content,
                                });
                            }
                        }

                        // Check inline <think> tags in content
                        let think_open = accumulated_content.find("<think>");
                        let think_close = accumulated_content.find("</think>");

                        if let Some(open_idx) = think_open {
                            if think_close.is_none() {
                                // Still inside <think>...</think>
                                in_reasoning_state = true;
                                thinking_tokens += 1;
                                let thought = &accumulated_content[open_idx + "<think>".len()..];
                                let snippet = format_thought_snippet(thought);
                                on_status(AiStatusUpdate::Reasoning {
                                    elapsed_secs: start_time.elapsed().as_secs(),
                                    thought_snippet: snippet,
                                });
                            } else if in_reasoning_state && !in_writing_state {
                                // Transitioned from thinking to writing
                                in_writing_state = true;
                                in_reasoning_state = false;
                                on_status(AiStatusUpdate::WritingResponse);
                            }
                        } else if in_reasoning_state && !in_writing_state {
                            // message.thinking stopped, message.content started
                            in_writing_state = true;
                            in_reasoning_state = false;
                            on_status(AiStatusUpdate::WritingResponse);
                        } else if !in_writing_state {
                            in_writing_state = true;
                            on_status(AiStatusUpdate::WritingResponse);
                        }
                    }
                }

                if chunk.done {
                    if let Some(prompt_tokens) = chunk.prompt_eval_count {
                        final_stats.prompt_tokens = prompt_tokens;
                    }
                    if let Some(eval_count) = chunk.eval_count {
                        final_stats.response_tokens = eval_count;
                    } else {
                        final_stats.response_tokens = response_tokens;
                    }
                    final_stats.thinking_tokens = thinking_tokens;
                    final_stats.eval_duration_ms =
                        chunk.eval_duration.map(|ns| ns / 1_000_000).unwrap_or(0);
                    final_stats.total_duration_ms =
                        chunk.total_duration.map(|ns| ns / 1_000_000).unwrap_or(0);
                    final_stats.load_duration_ms =
                        chunk.load_duration.map(|ns| ns / 1_000_000).unwrap_or(0);

                    self.last_stats
                        .write()
                        .await
                        .insert(channel_id, final_stats.clone());
                }
            }
        }

        let final_output = format_partial_output(&accumulated_thinking, &accumulated_content);
        Ok(StreamResult::Completed(final_output))
    }

    /// Orchestrate streaming AI response, real-time SearXNG search execution while thinking, and media downloading.
    pub async fn ask_orchestrated<F>(
        &self,
        searx: &SearxClient,
        channel_id: Id<ChannelMarker>,
        initial_messages: &[AiMessage],
        safesearch_level: u8,
        mode: AiMode,
        mut on_status: F,
    ) -> Result<GenerationResult>
    where
        F: FnMut(AiStatusUpdate) + Send,
    {
        let mut conversation = initial_messages.to_vec();

        let mut candidate_media_urls: Vec<String> = Vec::new();
        let mut search_rounds = 0;
        const MAX_SEARCH_ROUNDS: usize = 2;

        let final_raw_answer = loop {
            let allow_search = searx.is_enabled() && search_rounds < MAX_SEARCH_ROUNDS;
            let stream_res = self
                .ask_stream(channel_id, &conversation, allow_search, mode, &mut on_status)
                .await?;

            match stream_res {
                StreamResult::Completed(full_text) => {
                    // Safety net: check if there are any unexecuted search tags in completed text as fallback
                    if allow_search && search_rounds < MAX_SEARCH_ROUNDS {
                        let fallback_tags = extract_search_tags(&full_text);
                        if let Some(first_tag) = fallback_tags.into_iter().next() {
                            search_rounds += 1;
                            let (category_label, query, search_category) = match &first_tag {
                                AiSearchTag::Web(q) => ("web (DuckDuckGo)", q.clone(), "general"),
                                AiSearchTag::Browse(u) => ("webpage", u.clone(), "browse"),
                                AiSearchTag::Image(q) => ("images", q.clone(), "images"),
                                AiSearchTag::Audio(q) => ("audio/media", q.clone(), "music,files,videos"),
                            };

                            on_status(AiStatusUpdate::Searching {
                                query: query.clone(),
                                category: category_label.to_string(),
                                safesearch_on: safesearch_level > 0,
                            });

                            let search_results_text = match &first_tag {
                                AiSearchTag::Browse(url) => {
                                    match fetch_webpage_clean_text(searx.client(), url).await {
                                        Ok(content) => {
                                            format!(
                                                "\n[Webpage Content for \"{url}\"]:\n{content}\n\n[Instructions]: Synthesize this content to answer the user's question accurately in character.\n"
                                            )
                                        }
                                        Err(e) => {
                                            format!(
                                                "\n[Webpage Content for \"{url}\"]: Could not load page ({e}). Use your current knowledge or other search results.\n"
                                            )
                                        }
                                    }
                                }
                                _ => {
                                    self.execute_search_and_collect(
                                        searx,
                                        &first_tag,
                                        &query,
                                        search_category,
                                        safesearch_level,
                                        &mut candidate_media_urls,
                                    )
                                    .await
                                }
                            };

                            conversation.push(AiMessage::assistant(full_text));
                            let follow_up = if mode == AiMode::Fast {
                                format!(
                                    "{search_results_text}\nNow formulate a direct, accurate answer to the user's question based on these findings without hallucinating."
                                )
                            } else {
                                format!(
                                    "{search_results_text}\nNow carefully analyze and verify these findings in your thoughts, use <browse>URL</browse> if deeper article details are needed, close with </think> when done, and formulate your accurate response."
                                )
                            };
                            conversation.push(AiMessage::user(follow_up));
                            continue;
                        }
                    }
                    break full_text;
                }
                StreamResult::SearchIntercepted {
                    tag,
                    partial_thinking,
                    partial_content,
                } => {
                    search_rounds += 1;

                    let (category_label, query, search_category) = match &tag {
                        AiSearchTag::Web(q) => ("web (DuckDuckGo)", q.clone(), "general"),
                        AiSearchTag::Browse(u) => ("webpage", u.clone(), "browse"),
                        AiSearchTag::Image(q) => ("images", q.clone(), "images"),
                        AiSearchTag::Audio(q) => ("audio/media", q.clone(), "music,files,videos"),
                    };

                    on_status(AiStatusUpdate::Searching {
                        query: query.clone(),
                        category: category_label.to_string(),
                        safesearch_on: safesearch_level > 0,
                    });

                    let search_results_text = match &tag {
                        AiSearchTag::Browse(url) => {
                            match fetch_webpage_clean_text(searx.client(), url).await {
                                Ok(content) => {
                                    format!(
                                        "\n[Webpage Content for \"{url}\"]:\n{content}\n\n[Instructions]: Synthesize this content to answer the user's question accurately in character.\n"
                                    )
                                }
                                Err(e) => {
                                    format!(
                                        "\n[Webpage Content for \"{url}\"]: Could not load page ({e}). Use your current knowledge or other search results.\n"
                                    )
                                }
                            }
                        }
                        _ => {
                            self.execute_search_and_collect(
                                searx,
                                &tag,
                                &query,
                                search_category,
                                safesearch_level,
                                &mut candidate_media_urls,
                            )
                            .await
                        }
                    };

                    let tag_str = match &tag {
                        AiSearchTag::Web(q) => format!("<search>{q}</search>"),
                        AiSearchTag::Browse(u) => format!("<browse>{u}</browse>"),
                        AiSearchTag::Image(q) => format!("<search_image>{q}</search_image>"),
                        AiSearchTag::Audio(q) => format!("<search_audio>{q}</search_audio>"),
                    };
                    let assistant_msg = if !partial_content.trim().is_empty() {
                        if !partial_thinking.trim().is_empty() {
                            format!("<think>\n{}\n</think>\n{}\n{}", partial_thinking.trim(), partial_content.trim(), tag_str)
                        } else {
                            format!("{}\n{}", partial_content.trim(), tag_str)
                        }
                    } else if !partial_thinking.trim().is_empty() {
                        format!("<think>\n{}\n</think>\n{}", partial_thinking.trim(), tag_str)
                    } else {
                        tag_str
                    };
                    conversation.push(AiMessage::assistant(assistant_msg));

                    let follow_up = if mode == AiMode::Fast {
                        format!(
                            "{search_results_text}\nNow formulate a direct, accurate answer to the user's question based on these findings without hallucinating."
                        )
                    } else {
                        format!(
                            "{search_results_text}\nNow carefully analyze and verify these findings in your thoughts, use <browse>URL</browse> if deeper article details are needed, close with </think> when done, and formulate your accurate response."
                        )
                    };
                    conversation.push(AiMessage::user(follow_up));
                }
            }
        };

        // Media file downloading & attachment resolution
        let explicit_attaches = extract_attach_tags(&final_raw_answer);
        let mut download_candidates = explicit_attaches;

        // If the model mentioned any candidate media URL directly in its response text, prioritize it
        for cand in &candidate_media_urls {
            if final_raw_answer.contains(cand) && !download_candidates.contains(cand) {
                download_candidates.push(cand.clone());
            }
        }
        // Fall back to remaining search candidate media URLs
        for cand in candidate_media_urls {
            if !download_candidates.contains(&cand) {
                download_candidates.push(cand);
            }
        }

        let mut attachment: Option<(String, Vec<u8>)> = None;

        if !download_candidates.is_empty() {
            on_status(AiStatusUpdate::DownloadingMedia);

            for candidate in download_candidates {
                match download_media(&self.client, &candidate).await {
                    Ok(att) => {
                        attachment = Some(att);
                        break;
                    }
                    Err(e) => {
                        tracing::debug!(?e, url = %candidate, "Media candidate download failed, trying next");
                    }
                }
            }
        }

        Ok(GenerationResult {
            content: final_raw_answer,
            attachment,
        })
    }

    async fn execute_search_and_collect(
        &self,
        searx: &SearxClient,
        tag: &AiSearchTag,
        query: &str,
        category: &str,
        safesearch: u8,
        candidate_media_urls: &mut Vec<String>,
    ) -> String {
        let mut search_results_text = String::new();
        match searx.search(query, category, safesearch).await {
            Ok(results) => match tag {
                AiSearchTag::Web(_) => {
                    search_results_text.push_str(&format!("\n[Web Search Results for \"{query}\"]:\n"));
                    for (i, item) in results.iter().take(5).enumerate() {
                        let title = item.title.as_deref().unwrap_or("No title").trim();
                        let url = item.url.as_deref().unwrap_or("").trim();
                        let snippet = item.content.as_deref().unwrap_or("").trim();
                        search_results_text.push_str(&format!(
                            "{}. Title: \"{title}\" ({url})\n   Snippet: {snippet}\n",
                            i + 1,
                        ));
                    }
                    search_results_text.push_str(
                        "\n[Search Instructions]: Analyze these findings carefully. If you need deeper details from any URL above, emit <browse>URL</browse>. Otherwise, synthesize these verified facts to formulate an accurate, grounded response for the user.\n"
                    );
                }
                AiSearchTag::Image(_) => {
                    search_results_text.push_str(&format!("\n[Image Search Results for \"{query}\"]:\n"));
                    let mut count = 0;
                    for item in results.iter() {
                        let raw_img = item
                            .img_src
                            .as_deref()
                            .or(item.thumbnail_src.as_deref())
                            .unwrap_or("");
                        let normalized = if raw_img.starts_with("//") {
                            format!("https:{raw_img}")
                        } else {
                            raw_img.to_string()
                        };
                        if normalized.is_empty() || !normalized.starts_with("http") {
                            continue;
                        }
                        let lower = normalized.to_ascii_lowercase();
                        if lower.ends_with(".svg") || lower.contains(".svg?") {
                            continue;
                        }
                        let title = item.title.as_deref().unwrap_or("Image").trim();
                        let snippet = item.content.as_deref().unwrap_or("").trim();
                        candidate_media_urls.push(normalized.clone());
                        count += 1;
                        if snippet.is_empty() {
                            search_results_text.push_str(&format!(
                                "{count}. Title: \"{title}\"\n   Direct Image URL: {normalized}\n"
                            ));
                        } else {
                            search_results_text.push_str(&format!(
                                "{count}. Title: \"{title}\" (Description: {snippet})\n   Direct Image URL: {normalized}\n"
                            ));
                        }
                        if count >= 6 {
                            break;
                        }
                    }
                    if count > 0 {
                        search_results_text.push_str(
                            "\n[Instructions for choosing image]:\n\
                             1. In your thinking, evaluate which image from the list best depicts what was requested.\n\
                             2. In your final response, briefly tell the user what picture you found and attach it using: <attach>CHOSEN_URL</attach>\n"
                        );
                    }
                }
                AiSearchTag::Audio(_) => {
                    search_results_text.push_str(&format!("\n[Audio Search Results for \"{query}\"]:\n"));
                    let mut count = 0;
                    for item in results.iter() {
                        let raw_audio = item
                            .audio_src
                            .as_deref()
                            .or(item.url.as_deref())
                            .unwrap_or("");
                        let normalized = if raw_audio.starts_with("//") {
                            format!("https:{raw_audio}")
                        } else {
                            raw_audio.to_string()
                        };
                        if normalized.is_empty() || !normalized.starts_with("http") {
                            continue;
                        }
                        let title = item.title.as_deref().unwrap_or("Audio").trim();
                        candidate_media_urls.push(normalized.clone());
                        count += 1;
                        search_results_text.push_str(&format!(
                            "{count}. Title: \"{title}\"\n   Media URL: {normalized}\n"
                        ));
                        if count >= 6 {
                            break;
                        }
                    }
                    if count > 0 {
                        search_results_text.push_str(
                            "\n[Instructions]: In your thinking, pick the best audio and attach it using: <attach>CHOSEN_URL</attach>\n"
                        );
                    }
                }
                AiSearchTag::Browse(_) => {}
            },
            Err(e) => {
                tracing::warn!(?e, %query, "SearXNG search failed");
                search_results_text.push_str(&format!(
                    "\n[Search for \"{query}\"]: No results found or search failed.\n"
                ));
            }
        }

        if search_results_text.is_empty() {
            search_results_text = format!("\n[Search for \"{query}\"]: No results found.\n");
        }

        search_results_text
    }

    /// Non-streaming ask fallback (Thinking mode).
    pub async fn ask(&self, channel_id: Id<ChannelMarker>, messages: &[AiMessage]) -> Result<String> {
        match self.ask_stream(channel_id, messages, false, AiMode::Thinking, |_| {}).await? {
            StreamResult::Completed(s) => Ok(s),
            StreamResult::SearchIntercepted {
                partial_thinking,
                partial_content,
                ..
            } => Ok(format_partial_output(&partial_thinking, &partial_content)),
        }
    }

    /// Non-streaming fast ask fallback (Fast mode).
    pub async fn ask_fast(&self, channel_id: Id<ChannelMarker>, messages: &[AiMessage]) -> Result<String> {
        match self.ask_stream(channel_id, messages, false, AiMode::Fast, |_| {}).await? {
            StreamResult::Completed(s) => Ok(s),
            StreamResult::SearchIntercepted {
                partial_thinking,
                partial_content,
                ..
            } => Ok(format_partial_output(&partial_thinking, &partial_content)),
        }
    }
}

/// Format partial thinking and content into a single string.
pub fn format_partial_output(thinking: &str, content: &str) -> String {
    let trimmed_content = content.trim();
    let trimmed_thinking = thinking.trim();

    if trimmed_content.is_empty() {
        if trimmed_thinking.is_empty() {
            String::new()
        } else {
            // Model generated thoughts but stopped before outputting content.
            // Salvage the thinking output so it isn't completely stripped!
            trimmed_thinking.to_string()
        }
    } else if !trimmed_thinking.is_empty() && !trimmed_content.contains("<think>") {
        format!("<think>{trimmed_thinking}</think>\n{trimmed_content}")
    } else {
        trimmed_content.to_string()
    }
}

/// Format latest thought stream into clean 250-300 character snippet.
pub fn format_thought_snippet(thought: &str) -> String {
    let collapsed = thought
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    let total_chars = collapsed.chars().count();
    let snippet: String = if total_chars > 280 {
        collapsed.chars().skip(total_chars - 280).collect()
    } else {
        collapsed
    };

    // Sanitize any internal blockquotes or formatting chars
    snippet.replace('\n', " ").replace('`', "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_thought_snippet() {
        let long_thought = "Let me think about this. ".repeat(20);
        let snippet = format_thought_snippet(&long_thought);
        assert!(snippet.chars().count() <= 280);
    }

    #[test]
    fn test_stream_chunk_deserialize() {
        let json = r#"{"model":"deepseek","message":{"role":"assistant","content":"hello","thinking":"reasoning..."},"done":false}"#;
        let chunk: OllamaStreamChunk = serde_json::from_str(json).unwrap();
        assert_eq!(chunk.message.as_ref().unwrap().content, "hello");
        assert_eq!(chunk.message.as_ref().unwrap().thinking.as_deref(), Some("reasoning..."));
        assert!(!chunk.done);
    }

    #[test]
    fn test_format_partial_output() {
        assert_eq!(format_partial_output("thinking", "content"), "<think>thinking</think>\ncontent");
        assert_eq!(format_partial_output("only thoughts", ""), "only thoughts");
        assert_eq!(format_partial_output("", "only content"), "only content");
        assert_eq!(format_partial_output("", ""), "");
    }
}
