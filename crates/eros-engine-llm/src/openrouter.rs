// SPDX-License-Identifier: AGPL-3.0-only
//! OpenRouter chat-completions client. Thin HTTP wrapper around
//! `POST https://openrouter.ai/api/v1/chat/completions`.
//!
//! Returns plain-text reply only; no JSON evaluation.

use serde::{Deserialize, Serialize};

use crate::error::LlmError;
use crate::model_config::ReasoningConfig;

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
    /// Ordered fallback chain (empty = no fallback). Singular-named
    /// despite being a Vec because semantically the chain resolves to
    /// ONE actually-served model per call — entries are sequentially
    /// tried candidates, not parallel fan-out.
    pub fallback_model: Vec<String>,
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
    /// Reasoning config forwarded to OpenRouter. `None` → omit the param;
    /// `Some(cfg)` → send the `reasoning` object verbatim.
    pub reasoning: Option<ReasoningConfig>,
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

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    temperature: f32,
    max_tokens: u32,
    #[serde(skip_serializing_if = "is_false")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<&'a serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<&'a ReasoningConfig>,
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

// ── SSE streaming types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct UsageBlock {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    /// OpenRouter sometimes includes a `cost` field (USD). Kept here so
    /// callers that want to log it have access; the spec's `done.usage`
    /// schema only requires the three token counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct DeltaChunk {
    pub content: Option<String>,
    pub finish_reason: Option<String>,
    pub usage: Option<UsageBlock>,
    pub generation_id: Option<String>,
    pub model: Option<String>,
}

/// Opaque wrapper around a boxed SSE delta stream. Implements [`Debug`] so
/// callers can use `.expect_err()` / `.unwrap()` in tests without the
/// underlying `dyn Stream` trait-object imposing a `Debug` bound.
pub struct DeltaStream(pub futures_util::stream::BoxStream<'static, Result<DeltaChunk, LlmError>>);

impl std::fmt::Debug for DeltaStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeltaStream").finish_non_exhaustive()
    }
}

impl futures_util::Stream for DeltaStream {
    type Item = Result<DeltaChunk, LlmError>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::pin::Pin;
        Pin::new(&mut self.0).poll_next(cx)
    }
}

#[derive(Debug, Deserialize, Default)]
struct WireStreamDelta {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct WireStreamChoice {
    #[serde(default)]
    delta: WireStreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireStreamFrame {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<UsageBlock>,
    #[serde(default)]
    choices: Vec<WireStreamChoice>,
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

    /// Execute a chat completion, walking the candidate chain
    /// (`req.model` + `req.fallback_model` entries) sequentially.
    /// First success wins; each failure is logged at warn level.
    /// Empty model strings are filtered out so a misconfigured TOML
    /// (e.g. `model = ""` or `fallback = [""]`) is caught locally as
    /// `LlmError::Config` rather than producing a remote 400.
    /// Audit passthrough fields ride along on every attempt.
    pub async fn execute(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        let candidates: Vec<&str> = std::iter::once(req.model.as_str())
            .chain(req.fallback_model.iter().map(String::as_str))
            .filter(|s| !s.is_empty())
            .collect();
        if candidates.is_empty() {
            return Err(LlmError::Config(
                "openrouter: no models configured (primary empty, no fallbacks)".into(),
            ));
        }

        let mut last_err: Option<LlmError> = None;
        for (i, model) in candidates.iter().enumerate() {
            match self
                .call_once(
                    model,
                    &req.messages,
                    req.temperature,
                    req.max_tokens,
                    req.user.as_deref(),
                    req.session_id.as_deref(),
                    req.metadata.as_ref(),
                    req.reasoning.as_ref(),
                )
                .await
            {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    let remaining = candidates.len() - i - 1;
                    let msg = if remaining == 0 {
                        "openrouter: all candidates exhausted"
                    } else if i == 0 {
                        "openrouter: primary failed, trying fallbacks"
                    } else {
                        "openrouter: fallback failed, trying next"
                    };
                    if i == 0 {
                        tracing::warn!(
                            primary = %req.model,
                            error = %e,
                            fallbacks_remaining = remaining,
                            "{msg}"
                        );
                    } else {
                        tracing::warn!(
                            primary = %req.model,
                            fallback = %model,
                            fallback_index = i - 1,
                            error = %e,
                            fallbacks_remaining = remaining,
                            "{msg}"
                        );
                    }
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| LlmError::Config("openrouter: no models configured".into())))
    }

    #[allow(clippy::too_many_arguments)]
    async fn call_once(
        &self,
        model: &str,
        messages: &[ChatMessage],
        temperature: f32,
        max_tokens: u32,
        req_user: Option<&str>,
        req_session_id: Option<&str>,
        req_metadata: Option<&serde_json::Map<String, serde_json::Value>>,
        req_reasoning: Option<&ReasoningConfig>,
    ) -> Result<ChatResponse, LlmError> {
        if self.api_key.is_empty() {
            return Err(LlmError::Config("openrouter: api key not set".into()));
        }

        let wire = WireRequest {
            model,
            messages,
            temperature,
            max_tokens,
            stream: false,
            user: req_user,
            session_id: req_session_id,
            metadata: req_metadata,
            reasoning: req_reasoning,
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
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();
        Ok(ChatResponse {
            reply: clean_response(raw.trim()),
            generation_id: parsed.id,
            model: parsed.model,
            usage: parsed.usage,
        })
    }

    /// Open a streaming chat completion against a single model. Fallback
    /// chain handling is the caller's responsibility (pipeline layer).
    pub async fn execute_stream(&self, req: ChatRequest) -> Result<DeltaStream, LlmError> {
        use eventsource_stream::Eventsource;
        use futures_util::StreamExt;

        if self.api_key.is_empty() {
            return Err(LlmError::Config("openrouter: api key not set".into()));
        }
        if req.model.is_empty() {
            return Err(LlmError::Config(
                "openrouter: execute_stream requires a non-empty model".into(),
            ));
        }

        // Mirror the sync `call_once` wire: a hand-rolled `json!` here once
        // serialised unset audit fields as `user: null`, which OpenRouter
        // rejects (400 "expected string, received null"). Sharing WireRequest
        // keeps the skip-None behaviour and stops the two paths from drifting.
        let wire = WireRequest {
            model: &req.model,
            messages: &req.messages,
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            stream: true,
            user: req.user.as_deref(),
            session_id: req.session_id.as_deref(),
            metadata: req.metadata.as_ref(),
            reasoning: req.reasoning.as_ref(),
        };

        let resp = self
            .http
            .post(&self.base_url)
            .bearer_auth(&self.api_key)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&wire)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(LlmError::Status(status, text));
        }

        let stream = resp
            .bytes_stream()
            .eventsource()
            .filter_map(|ev| async move {
                match ev {
                    Ok(e) => {
                        if e.data == "[DONE]" {
                            return None;
                        }
                        match serde_json::from_str::<WireStreamFrame>(&e.data) {
                            Ok(frame) => {
                                let choice = frame.choices.into_iter().next().unwrap_or_default();
                                Some(Ok(DeltaChunk {
                                    content: choice.delta.content.filter(|s| !s.is_empty()),
                                    finish_reason: choice.finish_reason,
                                    usage: frame.usage,
                                    generation_id: frame.id,
                                    model: frame.model,
                                }))
                            }
                            Err(_) => Some(Err(LlmError::StreamParse(
                                e.data.chars().take(256).collect(),
                            ))),
                        }
                    }
                    Err(e) => Some(Err(LlmError::Stream(e.to_string()))),
                }
            });

        Ok(DeltaStream(stream.boxed()))
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
                fallback_model: Vec::new(),
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
                fallback_model: Vec::new(),
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
                fallback_model: Vec::new(),
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
            assert!(
                req.headers.get("http-referer").is_none(),
                "HTTP-Referer must be dropped"
            );
            assert!(
                req.headers.get("x-title").is_none(),
                "X-Title must be dropped"
            );
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
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            }],
            temperature: 0.0,
            max_tokens: 16,
            ..Default::default()
        };
        let wire = WireRequest {
            model: &req.model,
            messages: &req.messages,
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            stream: false,
            user: req.user.as_deref(),
            session_id: req.session_id.as_deref(),
            metadata: req.metadata.as_ref(),
            reasoning: None,
        };
        let s = serde_json::to_string(&wire).unwrap();
        assert!(!s.contains("\"user\":"), "user key must be absent: {s}");
        assert!(
            !s.contains("\"session_id\":"),
            "session_id key must be absent: {s}"
        );
        assert!(
            !s.contains("\"metadata\":"),
            "metadata key must be absent: {s}"
        );
    }

    #[test]
    fn wire_request_includes_audit_fields_when_set() {
        let mut metadata = serde_json::Map::new();
        metadata.insert("feature".into(), serde_json::Value::String("chat".into()));
        let req = ChatRequest {
            model: "openai/gpt-5.2".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            }],
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
            stream: false,
            user: req.user.as_deref(),
            session_id: req.session_id.as_deref(),
            metadata: req.metadata.as_ref(),
            reasoning: None,
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

        assert_eq!(resp.reply, "ok");
        assert_eq!(resp.generation_id.as_deref(), Some("gen-abc123"));
        assert_eq!(resp.model.as_deref(), Some("openai/gpt-5.2"));
        let usage = resp.usage.expect("usage present");
        assert_eq!(
            usage.get("prompt_tokens").and_then(|v| v.as_u64()),
            Some(12)
        );
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

        assert_eq!(resp.reply, "ok");
        assert!(resp.generation_id.is_none());
        assert!(resp.model.is_none());
        assert!(resp.usage.is_none());
    }

    #[tokio::test]
    async fn execute_falls_back_on_primary_failure() {
        let server = MockServer::start().await;
        // Primary "p" returns 500; fallback "f1" returns 200.
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "p"}),
            ))
            .respond_with(ResponseTemplate::new(500).set_body_string("primary down"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "f1"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            AppAttribution::default(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let resp = client
            .execute(ChatRequest {
                model: "p".into(),
                fallback_model: vec!["f1".into()],
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect("fallback succeeds");
        assert_eq!(resp.reply, "ok");
    }

    #[tokio::test]
    async fn execute_walks_full_fallback_chain() {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "p"}),
            ))
            .respond_with(ResponseTemplate::new(500).set_body_string("p down"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "f1"}),
            ))
            .respond_with(ResponseTemplate::new(500).set_body_string("f1 down"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "f2"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            AppAttribution::default(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let resp = client
            .execute(ChatRequest {
                model: "p".into(),
                fallback_model: vec!["f1".into(), "f2".into()],
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect("second fallback succeeds");
        assert_eq!(resp.reply, "ok");
    }

    #[tokio::test]
    async fn execute_returns_last_error_when_all_fail() {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("nope"))
            .expect(2)
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            AppAttribution::default(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let err = client
            .execute(ChatRequest {
                model: "p".into(),
                fallback_model: vec!["f1".into()],
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect_err("all fail");
        assert!(
            matches!(err, LlmError::Status(s, _) if s.as_u16() == 500),
            "expected last 500, got {err:?}"
        );
    }

    #[tokio::test]
    async fn execute_returns_config_err_when_chain_empty() {
        // No mocks — empty primary + empty fallback chain must short-circuit
        // BEFORE any HTTP request reaches the server.
        let server = MockServer::start().await;
        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            AppAttribution::default(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let err = client
            .execute(ChatRequest {
                model: String::new(),
                fallback_model: Vec::new(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect_err("empty chain must Err");
        assert!(
            matches!(err, LlmError::Config(_)),
            "expected Config error, got {err:?}"
        );
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "no HTTP request should have been made"
        );
    }

    #[tokio::test]
    async fn execute_skips_empty_string_candidates() {
        let server = MockServer::start().await;
        // Only "x" should be hit; primary "" must be filtered out before
        // any HTTP call is attempted.
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "x"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            AppAttribution::default(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let resp = client
            .execute(ChatRequest {
                model: String::new(),
                fallback_model: vec!["x".into()],
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect("non-empty fallback succeeds");
        assert_eq!(resp.reply, "ok");
    }

    #[tokio::test]
    async fn execute_stream_yields_deltas_then_terminal_usage() {
        use futures_util::StreamExt;

        let server = MockServer::start().await;
        // Two delta frames + a terminal frame with usage + the `[DONE]`
        // sentinel. Crucially, the body chunks arrive as a single raw text
        // body — wiremock does not flush per-chunk, but the eventsource-stream
        // parser tolerates the whole body arriving at once because it splits
        // on the wire-level `\n\n` boundary itself.
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"好\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5},\"id\":\"gen-xyz\",\"model\":\"x-ai/grok-4-fast\"}\n\n\
data: [DONE]\n\n";

        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"stream": true}),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            AppAttribution::default(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );

        let mut stream = client
            .execute_stream(ChatRequest {
                model: "x-ai/grok-4-fast".into(),
                fallback_model: Vec::new(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect("stream opens");

        let mut contents = Vec::new();
        let mut last_usage: Option<UsageBlock> = None;
        let mut last_gen_id: Option<String> = None;
        let mut last_model: Option<String> = None;
        while let Some(item) = stream.next().await {
            let chunk = item.expect("delta chunk parses");
            if let Some(c) = chunk.content {
                contents.push(c);
            }
            if chunk.usage.is_some() {
                last_usage = chunk.usage;
            }
            if chunk.generation_id.is_some() {
                last_gen_id = chunk.generation_id;
            }
            if chunk.model.is_some() {
                last_model = chunk.model;
            }
        }
        assert_eq!(contents, vec!["你".to_string(), "好".to_string()]);
        let u = last_usage.expect("usage present on terminal chunk");
        assert_eq!(u.prompt_tokens, 3);
        assert_eq!(u.completion_tokens, 2);
        assert_eq!(u.total_tokens, 5);
        assert_eq!(last_gen_id.as_deref(), Some("gen-xyz"));
        assert_eq!(last_model.as_deref(), Some("x-ai/grok-4-fast"));
    }

    #[tokio::test]
    async fn execute_stream_omits_null_audit_fields() {
        use futures_util::StreamExt;

        // Regression: the streaming wire used to be built with the `json!`
        // macro, which serialised unset audit fields as `user: null`.
        // OpenRouter rejects that with 400 "user: Invalid input: expected
        // string, received null", so absent fields MUST be omitted — same
        // skip-None behaviour as the sync `call_once` path.
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw("data: [DONE]\n\n", "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            AppAttribution::default(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );

        let mut stream = client
            .execute_stream(ChatRequest {
                model: "minimax/minimax-m2".into(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                // user / session_id / metadata default to None.
                ..Default::default()
            })
            .await
            .expect("stream opens");
        while stream.next().await.is_some() {}

        let reqs = server.received_requests().await.expect("requests recorded");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).expect("body is json");
        let obj = body.as_object().expect("body is a json object");
        assert_eq!(obj.get("stream"), Some(&serde_json::Value::Bool(true)));
        assert!(
            !obj.contains_key("user"),
            "user key must be absent (not null): {body}"
        );
        assert!(
            !obj.contains_key("session_id"),
            "session_id key must be absent: {body}"
        );
        assert!(
            !obj.contains_key("metadata"),
            "metadata key must be absent: {body}"
        );
    }

    #[tokio::test]
    async fn execute_stream_returns_status_error_when_upstream_4xx() {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate-limited"))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            AppAttribution::default(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let err = client
            .execute_stream(ChatRequest {
                model: "x-ai/grok-4-fast".into(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect_err("4xx → Err before any stream yielded");
        assert!(
            matches!(err, LlmError::Status(s, _) if s.as_u16() == 429),
            "expected Status(429), got {err:?}"
        );
    }

    #[test]
    fn wire_request_serializes_reasoning_enabled_flag() {
        let messages = vec![ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        }];
        // Some(cfg) -> nested object; absent inner fields are omitted.
        let cfg = ReasoningConfig {
            enabled: Some(false),
            exclude: None,
        };
        let wire = WireRequest {
            model: "m",
            messages: &messages,
            temperature: 0.0,
            max_tokens: 16,
            stream: false,
            user: None,
            session_id: None,
            metadata: None,
            reasoning: Some(&cfg),
        };
        let s = serde_json::to_string(&wire).unwrap();
        assert!(
            s.contains("\"reasoning\":{\"enabled\":false}"),
            "reasoning must serialize as a nested object: {s}"
        );

        // None -> key omitted entirely
        let wire_none = WireRequest {
            model: "m",
            messages: &messages,
            temperature: 0.0,
            max_tokens: 16,
            stream: false,
            user: None,
            session_id: None,
            metadata: None,
            reasoning: None,
        };
        let s_none = serde_json::to_string(&wire_none).unwrap();
        assert!(
            !s_none.contains("\"reasoning\""),
            "absent reasoning must be omitted: {s_none}"
        );
    }

    #[tokio::test]
    async fn execute_stream_yields_parse_error_on_bad_frame() {
        use futures_util::StreamExt;
        let server = MockServer::start().await;
        let body = "data: not-json\n\n";
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            AppAttribution::default(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let mut stream = client
            .execute_stream(ChatRequest {
                model: "x-ai/grok-4-fast".into(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .unwrap();
        let item = stream.next().await.expect("at least one item");
        assert!(
            matches!(item, Err(LlmError::StreamParse(_))),
            "expected StreamParse error, got {item:?}"
        );
    }
}
