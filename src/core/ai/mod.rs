use eyre::Result;
use serde::Serialize;

mod ollama;

pub use ollama::OllamaClient;

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

pub struct AiClient {
    ollama: OllamaClient,
}

impl AiClient {
    pub fn new() -> Self {
        Self {
            ollama: OllamaClient::new(),
        }
    }

    pub async fn ask(&self, messages: &[AiMessage]) -> Result<String> {
        self.ollama.ask(messages).await
    }
}
