// SPDX-License-Identifier: AGPL-3.0-only
//! OpenRouter chat-completions client. Thin HTTP wrapper around
//! `POST https://openrouter.ai/api/v1/chat/completions`.
//!
//! Returns plain-text reply only; no JSON evaluation.

use serde::{Deserialize, Serialize};

use crate::error::LlmError;

const BASE_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

/// OpenRouter app-attribution header names. Pinned to the current
/// `https://openrouter.ai/docs/app-attribution` spec. If OpenRouter
/// renames either header in the future, update the value here; today's
/// names become legacy and (if a transition window applies) get added as
/// a parallel alias below.
const HEADER_REFERER: &str = "HTTP-Referer";
const HEADER_TITLE: &str = "X-Title";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub model: String,
    pub fallback_model: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub temperature: f32,
    pub max_tokens: u32,
    /// Opaque OpenRouter wire passthrough — `user` field. Engine never
    /// inspects this; callers are responsible for hashing PII out.
    pub user: Option<String>,
    /// Opaque OpenRouter wire passthrough — caller's session/conversation
    /// grouping id. Distinct from the engine's URL-path `session_id`.
    pub session_id: Option<String>,
    /// Opaque OpenRouter wire passthrough — analytics dimensions. Caps
    /// (≤16 keys, key ≤64 chars, value ≤512 chars) are enforced at the
    /// HTTP boundary, not here.
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatResponse {
    pub reply: String,
    /// OpenRouter response `id` — opaque generation handle.
    pub generation_id: Option<String>,
    /// Model actually served (may differ from request when fallback hit).
    pub model: Option<String>,
    /// OpenRouter `usage` block — tokens / cost. Opaque to engine;
    /// caller deserialises as needed.
    pub usage: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    temperature: f32,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<&'a serde_json::Map<String, serde_json::Value>>,
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
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<serde_json::Value>,
    choices: Vec<WireChoice>,
}

/// App-Attribution headers sent on every outbound OpenRouter call.
/// Skipping `None` fields avoids emitting blank-but-set headers, which
/// OpenRouter would record as a real attribution. Both `None` reverts
/// to today's no-header behaviour.
#[derive(Debug, Clone, Default)]
pub struct AppAttribution {
    /// Sent as `HTTP-Referer`. Identifies the deploying app to OpenRouter.
    pub referer: Option<String>,
    /// Sent as `X-Title`. Display name in OpenRouter's app analytics.
    pub title: Option<String>,
}

#[derive(Clone)]
pub struct OpenRouterClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl OpenRouterClient {
    /// Production constructor. Pins to OpenRouter's canonical URL and
    /// bakes attribution headers into the shared reqwest client at boot.
    pub fn new(api_key: String, attribution: AppAttribution) -> Self {
        Self::with_base_url(api_key, attribution, BASE_URL.to_string())
    }

    /// Low-level constructor that lets callers override the OpenRouter
    /// endpoint. Intended for integration tests (workspace-internal and
    /// downstream) that wire a wiremock or fake server in front of the
    /// client. Production code should use `new`, which pins to OpenRouter's
    /// canonical URL.
    pub fn with_base_url(api_key: String, attribution: AppAttribution, base_url: String) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(ref referer) = attribution.referer {
            match reqwest::header::HeaderValue::from_str(referer) {
                Ok(v) => {
                    headers.insert(HEADER_REFERER, v);
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    header = HEADER_REFERER,
                    "openrouter: dropping invalid attribution value"
                ),
            }
        }
        if let Some(ref title) = attribution.title {
            match reqwest::header::HeaderValue::from_str(title) {
                Ok(v) => {
                    headers.insert(HEADER_TITLE, v);
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    header = HEADER_TITLE,
                    "openrouter: dropping invalid attribution value"
                ),
            }
        }
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .expect("reqwest client build never fails with empty config");
        Self {
            http,
            api_key,
            base_url,
        }
    }

    /// Execute a chat completion. If `fallback_model` is set and the primary
    /// call fails (transport error or non-2xx status), retry once with the
    /// fallback model. Audit passthrough fields ride along on both attempts.
    pub async fn execute(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        match self
            .call_once(
                &req.model,
                &req.messages,
                req.temperature,
                req.max_tokens,
                req.user.as_deref(),
                req.session_id.as_deref(),
                req.metadata.as_ref(),
            )
            .await
        {
            Ok(resp) => Ok(resp),
            Err(primary_err) => {
                if let Some(fallback) = req.fallback_model.as_deref() {
                    tracing::warn!(
                        primary = %req.model,
                        fallback = %fallback,
                        error = %primary_err,
                        "openrouter: primary failed, retrying with fallback"
                    );
                    self.call_once(
                        fallback,
                        &req.messages,
                        req.temperature,
                        req.max_tokens,
                        req.user.as_deref(),
                        req.session_id.as_deref(),
                        req.metadata.as_ref(),
                    )
                    .await
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
        req_user: Option<&str>,
        req_session_id: Option<&str>,
        req_metadata: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<ChatResponse, LlmError> {
        if self.api_key.is_empty() {
            return Err(LlmError::Config("openrouter: api key not set".into()));
        }

        let wire = WireRequest {
            model,
            messages,
            temperature,
            max_tokens,
            user: req_user,
            session_id: req_session_id,
            metadata: req_metadata,
        };

        let resp = self
            .http
            .post(&self.base_url)
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
            .iter()
            .next()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();
        Ok(ChatResponse {
            reply: clean_response(raw.trim()),
            generation_id: parsed.id,
            model: parsed.model,
            usage: parsed.usage,
        })
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

    use wiremock::matchers::{header, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ok_response() -> serde_json::Value {
        serde_json::json!({
            "choices": [{ "message": { "content": "ok" } }]
        })
    }

    #[tokio::test]
    async fn client_sends_app_attribution_headers_when_set() {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(header("HTTP-Referer", "https://eros.example"))
            .and(header("X-Title", "Eros"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            AppAttribution {
                referer: Some("https://eros.example".into()),
                title: Some("Eros".into()),
            },
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let _ = client
            .execute(ChatRequest {
                model: "test/model".into(),
                fallback_model: None,
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect("call succeeds");
    }

    #[tokio::test]
    async fn client_omits_app_attribution_headers_when_default() {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            AppAttribution::default(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let _ = client
            .execute(ChatRequest {
                model: "test/model".into(),
                fallback_model: None,
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect("call succeeds");

        for req in server.received_requests().await.unwrap_or_default() {
            assert!(
                req.headers.get("http-referer").is_none(),
                "HTTP-Referer must be absent when unset"
            );
            assert!(
                req.headers.get("x-title").is_none(),
                "X-Title must be absent when unset"
            );
        }
    }

    #[tokio::test]
    async fn client_drops_invalid_attribution_value() {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            AppAttribution {
                referer: Some("bad\nvalue".into()),
                title: Some("also\rbad".into()),
            },
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let _ = client
            .execute(ChatRequest {
                model: "test/model".into(),
                fallback_model: None,
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect("call succeeds despite dropped header");

        for req in server.received_requests().await.unwrap_or_default() {
            assert!(req.headers.get("http-referer").is_none(), "HTTP-Referer must be dropped");
            assert!(req.headers.get("x-title").is_none(), "X-Title must be dropped");
        }
    }

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

    #[test]
    fn wire_request_omits_audit_fields_when_none() {
        let req = ChatRequest {
            model: "openai/gpt-5.2".into(),
            messages: vec![ChatMessage { role: "user".into(), content: "hi".into() }],
            temperature: 0.0,
            max_tokens: 16,
            ..Default::default()
        };
        let wire = WireRequest {
            model: &req.model,
            messages: &req.messages,
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            user: req.user.as_deref(),
            session_id: req.session_id.as_deref(),
            metadata: req.metadata.as_ref(),
        };
        let s = serde_json::to_string(&wire).unwrap();
        assert!(!s.contains("\"user\":"), "user key must be absent: {s}");
        assert!(!s.contains("\"session_id\":"), "session_id key must be absent: {s}");
        assert!(!s.contains("\"metadata\":"), "metadata key must be absent: {s}");
    }

    #[test]
    fn wire_request_includes_audit_fields_when_set() {
        let mut metadata = serde_json::Map::new();
        metadata.insert("feature".into(), serde_json::Value::String("chat".into()));
        let req = ChatRequest {
            model: "openai/gpt-5.2".into(),
            messages: vec![ChatMessage { role: "user".into(), content: "hi".into() }],
            temperature: 0.0,
            max_tokens: 16,
            user: Some("u_abc".into()),
            session_id: Some("conv_xyz".into()),
            metadata: Some(metadata),
            ..Default::default()
        };
        let wire = WireRequest {
            model: &req.model,
            messages: &req.messages,
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            user: req.user.as_deref(),
            session_id: req.session_id.as_deref(),
            metadata: req.metadata.as_ref(),
        };
        let s = serde_json::to_string(&wire).unwrap();
        assert!(s.contains("\"user\":\"u_abc\""), "{s}");
        assert!(s.contains("\"session_id\":\"conv_xyz\""), "{s}");
        assert!(s.contains("\"metadata\":{\"feature\":\"chat\"}"), "{s}");
    }

    #[tokio::test]
    async fn wire_response_parses_id_model_usage() {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "gen-abc123",
                "model": "openai/gpt-5.2",
                "usage": {
                    "prompt_tokens": 12,
                    "completion_tokens": 8,
                    "total_tokens": 20,
                    "cost": 0.0004
                },
                "choices": [{ "message": { "content": "ok" } }]
            })))
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            AppAttribution::default(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let resp = client
            .execute(ChatRequest {
                model: "openai/gpt-5.2".into(),
                messages: vec![ChatMessage { role: "user".into(), content: "hi".into() }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect("call succeeds");

        assert_eq!(resp.reply, "ok");
        assert_eq!(resp.generation_id.as_deref(), Some("gen-abc123"));
        assert_eq!(resp.model.as_deref(), Some("openai/gpt-5.2"));
        let usage = resp.usage.expect("usage present");
        assert_eq!(usage.get("prompt_tokens").and_then(|v| v.as_u64()), Some(12));
        assert_eq!(usage.get("total_tokens").and_then(|v| v.as_u64()), Some(20));
    }

    #[tokio::test]
    async fn wire_response_handles_missing_id_model_usage() {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "ok" } }]
            })))
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            AppAttribution::default(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let resp = client
            .execute(ChatRequest {
                model: "openai/gpt-5.2".into(),
                messages: vec![ChatMessage { role: "user".into(), content: "hi".into() }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect("call succeeds");

        assert_eq!(resp.reply, "ok");
        assert!(resp.generation_id.is_none());
        assert!(resp.model.is_none());
        assert!(resp.usage.is_none());
    }
}
