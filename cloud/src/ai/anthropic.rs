//! Minimal Anthropic Messages API client (raw HTTP via reqwest).

use serde::{Deserialize, Serialize};
use serde_json::Value;

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
pub const MODEL: &str = "claude-opus-4-7";

#[derive(Debug, thiserror::Error)]
pub enum AnthropicError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api error ({status}): {message}")]
    Api { status: u16, message: String },
    #[error("missing field: {0}")]
    MissingField(&'static str),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String, // "user" or "assistant"
    pub content: Value, // String or [content blocks]
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateRequest<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub system: &'a str,
    pub messages: &'a [Message],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<&'a [Value]>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateResponse {
    pub content: Vec<Value>,
    pub stop_reason: Option<String>,
    pub usage: Option<Value>,
}

pub struct AnthropicClient {
    http: reqwest::Client,
    api_key: String,
}

impl AnthropicClient {
    pub fn new(api_key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key,
        }
    }

    pub async fn create_message(
        &self,
        system: &str,
        messages: &[Message],
        tools: Option<&[Value]>,
    ) -> Result<CreateResponse, AnthropicError> {
        let body = CreateRequest {
            model: MODEL,
            max_tokens: 4096,
            system,
            messages,
            tools,
        };
        let resp = self
            .http
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(AnthropicError::Api {
                status: status.as_u16(),
                message: text,
            });
        }
        let parsed: CreateResponse = resp.json().await?;
        Ok(parsed)
    }
}
