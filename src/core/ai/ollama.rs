use std::time::Duration;

use eyre::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::AiMessage;

pub struct OllamaClient {
    client: Client,
    url: String,
    model: String,
}

#[derive(Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    messages: &'a [AiMessage],
    stream: bool,
}

#[derive(Deserialize)]
struct OllamaResponse {
    message: OllamaResponseMessage,
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

        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("failed to build AI HTTP client");

        Self { client, url, model }
    }

    pub async fn ask(&self, messages: &[AiMessage]) -> Result<String> {
        let request = OllamaRequest {
            model: &self.model,
            messages,
            stream: false,
        };

        let response = self
            .client
            .post(format!("{}/api/chat", self.url.trim_end_matches('/')))
            .json(&request)
            .send()
            .await
            .context("failed to contact Ollama")?
            .error_for_status()
            .context("Ollama returned an error")?
            .json::<OllamaResponse>()
            .await
            .context("failed to deserialize Ollama response")?;

        Ok(response.message.content)
    }
}
