// SPDX-License-Identifier: AGPL-3.0-only
//! OpenRouter chat-completions client. Thin HTTP wrapper around
//! `POST https://openrouter.ai/api/v1/chat/completions`.
//!
//! Returns plain-text reply only; no JSON evaluation.

use serde::{Deserialize, Serialize};

use crate::error::LlmError;

const BASE_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub fallback_model: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub temperature: f32,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatResponse {
    pub reply: String,
}

#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    temperature: f32,
    max_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct WireMessage {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
    message: WireMessage,
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    choices: Vec<WireChoice>,
}

#[derive(Clone)]
pub struct OpenRouterClient {
    http: reqwest::Client,
    api_key: String,
}

impl OpenRouterClient {
    pub fn new(api_key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key,
        }
    }

    /// Execute a chat completion. If `fallback_model` is set and the primary
    /// call fails (transport error or non-2xx status), retry once with the
    /// fallback model.
    pub async fn execute(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        match self
            .call_once(&req.model, &req.messages, req.temperature, req.max_tokens)
            .await
        {
            Ok(reply) => Ok(ChatResponse { reply }),
            Err(primary_err) => {
                if let Some(fallback) = req.fallback_model.as_deref() {
                    tracing::warn!(
                        primary = %req.model,
                        fallback = %fallback,
                        error = %primary_err,
                        "openrouter: primary failed, retrying with fallback"
                    );
                    let reply = self
                        .call_once(fallback, &req.messages, req.temperature, req.max_tokens)
                        .await?;
                    Ok(ChatResponse { reply })
                } else {
                    Err(primary_err)
                }
            }
        }
    }

    async fn call_once(
        &self,
        model: &str,
        messages: &[ChatMessage],
        temperature: f32,
        max_tokens: u32,
    ) -> Result<String, LlmError> {
        if self.api_key.is_empty() {
            return Err(LlmError::Config("openrouter: api key not set".into()));
        }

        let wire = WireRequest {
            model,
            messages,
            temperature,
            max_tokens,
        };

        let resp = self
            .http
            .post(BASE_URL)
            .bearer_auth(&self.api_key)
            .json(&wire)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(LlmError::Status(status, text));
        }

        let parsed: WireResponse = resp.json().await?;
        let raw = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();
        Ok(clean_response(raw.trim()))
    }
}

/// Strip markdown fences and surrounding whitespace so a plain-text model
/// output is preserved verbatim.
pub fn clean_response(raw: &str) -> String {
    let mut s = raw.trim();

    // Remove a leading ```...``` fence if present.
    if let Some(stripped) = s.strip_prefix("```") {
        // Drop the language tag if any (e.g. ```text)
        let after_lang = stripped.split_once('\n').map(|x| x.1).unwrap_or(stripped);
        if let Some(inner) = after_lang.rsplit_once("```") {
            s = inner.0.trim();
        } else {
            s = after_lang.trim();
        }
    }

    // Strip surrounding quotes ("reply" or 「reply」)
    let s = s.trim().trim_matches('"');
    let s = s.trim_matches(|c| c == '「' || c == '」');

    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_response_strips_code_fence() {
        let out = clean_response("```text\n你好呀\n```");
        assert_eq!(out, "你好呀");
    }

    #[test]
    fn test_clean_response_strips_language_less_fence() {
        let out = clean_response("```\n哈哈\n```");
        assert_eq!(out, "哈哈");
    }

    #[test]
    fn test_clean_response_strips_quotes() {
        assert_eq!(clean_response("\"hi there\""), "hi there");
        assert_eq!(clean_response("「你好」"), "你好");
    }

    #[test]
    fn test_clean_response_passthrough_plain() {
        assert_eq!(clean_response("hello"), "hello");
    }
}
