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
const HEADER_TITLE: &str = "X-OpenRouter-Title";
const HEADER_CATEGORIES: &str = "X-OpenRouter-Categories";

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
    /// Optional sampling knobs. `None` ⇒ the wire param is omitted, so a
    /// deployment that sets none produces a byte-identical body to today.
    pub top_p: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub presence_penalty: Option<f32>,
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
    /// PDE-only: OpenRouter `response_format` (e.g. a json_schema object).
    /// `None` ⇒ omitted. Opaque passthrough; the caller builds the schema.
    pub response_format: Option<serde_json::Value>,
}

/// One-shot multimodal *describe* request. Used only by the `chat_vision`
/// pipeline stage. Builds an OpenRouter user message whose `content` is a block
/// array (text instruction + one image_url). Keeps `ChatMessage` text-only.
#[derive(Debug, Clone, Default)]
pub struct VisionRequest {
    pub model: String,
    pub fallback_model: Vec<String>,
    pub system_prompt: String,
    pub image_url: String,
    /// User's own caption (becomes the text block when non-blank).
    pub caption: Option<String>,
    pub temperature: f32,
    pub max_tokens: u32,
    pub reasoning: Option<ReasoningConfig>,
}

/// Build the OpenRouter wire body for one vision attempt against `model`. Pure
/// (no I/O) so the block shape is unit-testable. A non-blank `caption` becomes
/// the text block; otherwise a default describe instruction is used.
fn build_vision_body(req: &VisionRequest, model: &str) -> serde_json::Value {
    let text = match req.caption.as_deref().map(str::trim) {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => "请描述这张图片的内容。".to_string(),
    };
    let mut body = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "system", "content": req.system_prompt },
            { "role": "user", "content": [
                { "type": "text", "text": text },
                { "type": "image_url", "image_url": { "url": req.image_url } }
            ]}
        ],
        "temperature": req.temperature,
        "max_tokens": req.max_tokens,
        "stream": false,
    });
    if let Some(r) = &req.reasoning {
        if let Ok(v) = serde_json::to_value(r) {
            body["reasoning"] = v;
        }
    }
    body
}

/// One-shot image-generation request. The model is expected to return
/// generated images in `message.images[]` on the wire response.
#[derive(Debug, Clone, Default)]
pub struct ImageGenRequest {
    pub model: String,
    /// Ordered fallback chain (empty = no fallback).
    pub fallback_model: Vec<String>,
    pub prompt: String,
    /// Optional face/style reference image URL (image2image hint).
    pub face_ref_url: Option<String>,
    /// Model-specific aspect ratio hint (e.g. `"3:4"`). Folded into the
    /// text instruction when present.
    pub aspect_ratio: Option<String>,
    /// Model-specific resolution hint (e.g. `"1024x1024"`). Folded into
    /// the text instruction when present.
    pub resolution: Option<String>,
    pub max_tokens: u32,
}

/// Response from a successful image-generation call.
#[derive(Debug, Clone, Default)]
pub struct ImageGenResponse {
    /// Base64 data-URLs extracted from `message.images[]`.
    pub images: Vec<String>,
    /// Optional text accompanying the images (from `message.content`).
    pub text: Option<String>,
    /// OpenRouter response `id` — opaque generation handle.
    pub generation_id: Option<String>,
    /// Model actually served (may differ from request when fallback hit).
    pub model: Option<String>,
    /// OpenRouter `usage` block — tokens / cost. Opaque to engine.
    pub usage: Option<serde_json::Value>,
    /// `finish_reason` from the first choice in the wire response.
    pub finish_reason: Option<String>,
}

/// Pull all image URLs out of the first choice's `message.images[]`.
fn images_from_wire(parsed: &WireResponse) -> Vec<String> {
    parsed
        .choices
        .first()
        .and_then(|c| c.message.images.as_ref())
        .map(|imgs| imgs.iter().map(|i| i.image_url.url.clone()).collect())
        .unwrap_or_default()
}

/// Build the OpenRouter wire body for one image-gen attempt. Pure (no I/O).
fn build_image_body(req: &ImageGenRequest, model: &str) -> serde_json::Value {
    // Aspect ratio / resolution are model-specific; fold them into the text
    // instruction as a best-effort hint (works across image models).
    let mut text = req.prompt.clone();
    if let Some(ar) = req.aspect_ratio.as_deref().filter(|s| !s.is_empty()) {
        text.push_str(&format!("\n(aspect ratio: {ar})"));
    }
    if let Some(res) = req.resolution.as_deref().filter(|s| !s.is_empty()) {
        text.push_str(&format!("\n(resolution: {res})"));
    }
    let mut content = vec![serde_json::json!({ "type": "text", "text": text })];
    if let Some(face) = req.face_ref_url.as_deref().filter(|s| !s.is_empty()) {
        content.push(serde_json::json!({
            "type": "image_url",
            "image_url": { "url": face }
        }));
    }
    serde_json::json!({
        "model": model,
        "modalities": ["image", "text"],
        "messages": [ { "role": "user", "content": content } ],
        "max_tokens": req.max_tokens,
        "stream": false,
    })
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
    /// `finish_reason` from the first choice in the wire response.
    /// `None` when the upstream omits it (most normal completions).
    /// Present as `"content_filter"` when Gemini/OpenAI mid-response
    /// safety truncation fires; callers can gate on this value.
    pub finish_reason: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// OpenRouter provider routing preferences. Only `ignore` is used today;
/// `allow_fallbacks` is omitted so OpenRouter applies its default (true),
/// i.e. the model is still served by a healthy provider.
#[derive(Debug, Serialize)]
struct ProviderPrefs<'a> {
    ignore: &'a [String],
}

#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f32>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<ProviderPrefs<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<&'a serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct WireImageUrl {
    url: String,
}

#[derive(Debug, Deserialize)]
struct WireImage {
    image_url: WireImageUrl,
}

#[derive(Debug, Deserialize)]
struct WireMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    images: Option<Vec<WireImage>>,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
    message: WireMessage,
    #[serde(default)]
    finish_reason: Option<String>,
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
    /// Sent as `X-OpenRouter-Title`. Display name in OpenRouter's app
    /// analytics. (OpenRouter also accepts the legacy `X-Title` alias; we
    /// send the current canonical name.)
    pub title: Option<String>,
    /// Sent as `X-OpenRouter-Categories`. Comma-separated marketplace
    /// categories for OpenRouter's app directory. Passed through verbatim;
    /// OpenRouter silently ignores unrecognised values, so the engine does
    /// no validation. Only takes effect when paired with `referer`.
    pub categories: Option<String>,
}

#[derive(Clone)]
pub struct OpenRouterClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    /// OpenRouter provider slugs sent as `provider.ignore` on every call.
    /// Empty by default; set at boot via [`OpenRouterClient::with_ignore_providers`].
    ignore_providers: Vec<String>,
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
        if let Some(ref categories) = attribution.categories {
            match reqwest::header::HeaderValue::from_str(categories) {
                Ok(v) => {
                    headers.insert(HEADER_CATEGORIES, v);
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    header = HEADER_CATEGORIES,
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
            ignore_providers: Vec::new(),
        }
    }

    /// Set the global provider-exclusion list (issue #84). Consuming builder so
    /// boot can chain it: `OpenRouterClient::new(key, attr).with_ignore_providers(list)`.
    /// Sent as `provider.ignore` on every outbound call.
    pub fn with_ignore_providers(mut self, providers: Vec<String>) -> Self {
        self.ignore_providers = providers;
        self
    }

    /// Build the `ProviderPrefs` for a wire body, or `None` when the exclusion
    /// list is empty (so the `provider` key is omitted entirely).
    fn provider_prefs(&self) -> Option<ProviderPrefs<'_>> {
        if self.ignore_providers.is_empty() {
            None
        } else {
            Some(ProviderPrefs {
                ignore: &self.ignore_providers,
            })
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
        // Latest recoverable byte-BPE garble seen while walking the chain, kept
        // separately from `last_err` so a LATER non-garble failure (transport /
        // status / decode) can't discard a repairable earlier garble. Tuple:
        // (model, raw, finish_reason).
        let mut last_garbled: Option<(String, String, Option<String>)> = None;
        for (i, model) in candidates.iter().enumerate() {
            match self
                .call_once(
                    model,
                    &req.messages,
                    req.temperature,
                    req.max_tokens,
                    req.top_p,
                    req.frequency_penalty,
                    req.presence_penalty,
                    req.user.as_deref(),
                    req.session_id.as_deref(),
                    req.metadata.as_ref(),
                    req.reasoning.as_ref(),
                    req.response_format.as_ref(),
                )
                .await
            {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    if let LlmError::Garbled {
                        model,
                        raw,
                        finish_reason,
                    } = &e
                    {
                        // Retain only a COMPLETE garble for last-resort salvage. A
                        // length-truncated garble is incomplete; repairing it would
                        // hand partial content to a structured caller as if complete.
                        if finish_reason.as_deref() != Some("length") {
                            last_garbled =
                                Some((model.clone(), raw.clone(), finish_reason.clone()));
                        }
                    }
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

        // Chain exhausted with no clean success. If ANY candidate returned
        // recoverable garble, repair it and return clean (if imperfect) text
        // rather than surfacing a hard failure or raw glyphs — even when a later
        // candidate failed differently. generation_id/usage are unavailable here.
        if let Some((model, raw, finish_reason)) = last_garbled {
            tracing::error!(
                %model,
                "openrouter: all candidates failed; returning repaired last garbled attempt"
            );
            return Ok(ChatResponse {
                reply: clean_response(crate::byte_bpe::repair_byte_bpe(&raw).trim()),
                generation_id: None,
                model: Some(model),
                usage: None,
                // Preserve the upstream finish_reason (e.g. "content_filter") so
                // downstream validity gates still see the safety signal.
                finish_reason,
            });
        }
        Err(last_err.unwrap_or_else(|| LlmError::Config("openrouter: no models configured".into())))
    }

    /// Execute a one-shot vision describe, walking the candidate chain
    /// (`model` + `fallback_model`) sequentially. First success wins. Mirrors
    /// `execute`'s chain semantics. Returns the model's text reply (expected
    /// JSON; parsing is the caller's job).
    pub async fn execute_vision(&self, req: VisionRequest) -> Result<ChatResponse, LlmError> {
        let candidates: Vec<&str> = std::iter::once(req.model.as_str())
            .chain(req.fallback_model.iter().map(String::as_str))
            .filter(|s| !s.is_empty())
            .collect();
        if candidates.is_empty() {
            return Err(LlmError::Config(
                "openrouter: vision has no models configured".into(),
            ));
        }
        if self.api_key.is_empty() {
            return Err(LlmError::Config("openrouter: api key not set".into()));
        }

        let mut last_err: Option<LlmError> = None;
        // Latest recoverable garble, kept separate so a later non-garble failure
        // can't discard a repairable earlier garble (mirrors `execute`). Tuple:
        // (model, raw, finish_reason).
        let mut last_garbled: Option<(String, String, Option<String>)> = None;
        for model in &candidates {
            let mut body = build_vision_body(&req, model);
            if let Some(prefs) = self.provider_prefs() {
                if let Ok(v) = serde_json::to_value(&prefs) {
                    body["provider"] = v;
                }
            }
            let resp = match self
                .http
                .post(&self.base_url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(model = %model, error = %e, "openrouter: vision attempt failed (transport); next");
                    last_err = Some(e.into());
                    continue;
                }
            };
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                tracing::warn!(model = %model, %status, "openrouter: vision attempt failed (status); next");
                last_err = Some(LlmError::Status(status, text));
                continue;
            }
            let parsed: WireResponse = match resp.json().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(model = %model, error = %e, "openrouter: vision attempt failed (decode); next");
                    last_err = Some(e.into());
                    continue;
                }
            };
            let first_choice = parsed.choices.into_iter().next();
            let raw = first_choice
                .as_ref()
                .and_then(|c| c.message.content.clone())
                .unwrap_or_default();
            let finish_reason = first_choice.and_then(|c| c.finish_reason);
            if crate::byte_bpe::looks_byte_garbled(&raw) {
                tracing::error!(model = %model, "openrouter: vision byte-BPE garbled; advancing candidate chain");
                // Retain only a COMPLETE garble for last-resort salvage; a
                // length-truncated garble is incomplete, so route it to last_err
                // (the caller fails open) rather than salvaging partial JSON.
                if finish_reason.as_deref() == Some("length") {
                    last_err = Some(LlmError::Garbled {
                        model: model.to_string(),
                        raw,
                        finish_reason,
                    });
                } else {
                    last_garbled = Some((model.to_string(), raw, finish_reason));
                }
                continue;
            }
            return Ok(ChatResponse {
                reply: clean_response(raw.trim()),
                generation_id: parsed.id,
                model: parsed.model,
                usage: parsed.usage,
                finish_reason,
            });
        }
        // Exhausted with no clean describe. If any candidate returned recoverable
        // garble, repair it so `run_vision` can still parse a describe JSON
        // (Ġ/Ċ-only garble round-trips to valid JSON) instead of dropping to the
        // text-only path — even when a later candidate failed differently.
        if let Some((model, raw, finish_reason)) = last_garbled {
            tracing::error!(
                %model,
                "openrouter: all vision candidates failed; returning repaired last garbled attempt"
            );
            return Ok(ChatResponse {
                reply: clean_response(crate::byte_bpe::repair_byte_bpe(&raw).trim()),
                generation_id: None,
                model: Some(model),
                usage: None,
                // Preserve the upstream finish_reason (e.g. "content_filter") so
                // run_vision's validity gate still sees the safety signal.
                finish_reason,
            });
        }
        Err(last_err.unwrap_or_else(|| LlmError::Config("openrouter: vision no models".into())))
    }

    /// One-shot image-generation call. Walks `[model] + fallback_model` on
    /// transport failure OR a zero-image success. Returns the first attempt that
    /// yields ≥1 image. Non-streaming.
    pub async fn execute_image(&self, req: ImageGenRequest) -> Result<ImageGenResponse, LlmError> {
        let candidates: Vec<&str> = std::iter::once(req.model.as_str())
            .chain(req.fallback_model.iter().map(String::as_str))
            .filter(|s| !s.is_empty())
            .collect();
        if candidates.is_empty() {
            return Err(LlmError::Config("openrouter: image-gen has no models".into()));
        }
        if self.api_key.is_empty() {
            return Err(LlmError::Config("openrouter: api key not set".into()));
        }
        let mut last_err: Option<LlmError> = None;
        for model in &candidates {
            let mut body = build_image_body(&req, model);
            if let Some(prefs) = self.provider_prefs() {
                if let Ok(v) = serde_json::to_value(&prefs) {
                    body["provider"] = v;
                }
            }
            let resp = match self
                .http
                .post(&self.base_url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(model = %model, error = %e, "openrouter: image attempt failed (transport); next");
                    last_err = Some(e.into());
                    continue;
                }
            };
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                tracing::warn!(model = %model, %status, "openrouter: image attempt failed (status); next");
                last_err = Some(LlmError::Status(status, text));
                continue;
            }
            let parsed: WireResponse = match resp.json().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(model = %model, error = %e, "openrouter: image attempt failed (decode); next");
                    last_err = Some(e.into());
                    continue;
                }
            };
            let images = images_from_wire(&parsed);
            if images.is_empty() {
                tracing::warn!(model = %model, "openrouter: image attempt returned zero images; next");
                last_err = Some(LlmError::Provider("image-gen returned no images".into()));
                continue;
            }
            let first = parsed.choices.into_iter().next();
            let text = first.as_ref().and_then(|c| c.message.content.clone());
            let finish_reason = first.and_then(|c| c.finish_reason);
            return Ok(ImageGenResponse {
                images,
                text,
                generation_id: parsed.id,
                model: parsed.model,
                usage: parsed.usage,
                finish_reason,
            });
        }
        Err(last_err.unwrap_or_else(|| LlmError::Config("openrouter: image-gen no models".into())))
    }

    #[allow(clippy::too_many_arguments)]
    async fn call_once(
        &self,
        model: &str,
        messages: &[ChatMessage],
        temperature: f32,
        max_tokens: u32,
        top_p: Option<f32>,
        frequency_penalty: Option<f32>,
        presence_penalty: Option<f32>,
        req_user: Option<&str>,
        req_session_id: Option<&str>,
        req_metadata: Option<&serde_json::Map<String, serde_json::Value>>,
        req_reasoning: Option<&ReasoningConfig>,
        req_response_format: Option<&serde_json::Value>,
    ) -> Result<ChatResponse, LlmError> {
        if self.api_key.is_empty() {
            return Err(LlmError::Config("openrouter: api key not set".into()));
        }

        let wire = WireRequest {
            model,
            messages,
            temperature,
            top_p,
            frequency_penalty,
            presence_penalty,
            max_tokens,
            stream: false,
            user: req_user,
            session_id: req_session_id,
            metadata: req_metadata,
            reasoning: req_reasoning,
            provider: self.provider_prefs(),
            response_format: req_response_format,
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
        let first_choice = parsed.choices.into_iter().next();
        let raw = first_choice
            .as_ref()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();
        let finish_reason = first_choice.and_then(|c| c.finish_reason);
        if crate::byte_bpe::looks_byte_garbled(&raw) {
            tracing::error!(
                model,
                generation_id = ?parsed.id,
                "openrouter: byte-BPE garbled completion; advancing candidate chain"
            );
            return Err(LlmError::Garbled {
                model: model.to_string(),
                raw,
                finish_reason,
            });
        }
        Ok(ChatResponse {
            reply: clean_response(raw.trim()),
            generation_id: parsed.id,
            model: parsed.model,
            usage: parsed.usage,
            finish_reason,
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
            top_p: req.top_p,
            frequency_penalty: req.frequency_penalty,
            presence_penalty: req.presence_penalty,
            max_tokens: req.max_tokens,
            stream: true,
            user: req.user.as_deref(),
            session_id: req.session_id.as_deref(),
            metadata: req.metadata.as_ref(),
            reasoning: req.reasoning.as_ref(),
            provider: self.provider_prefs(),
            response_format: None,
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
            .and(header("X-OpenRouter-Title", "Eros"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            AppAttribution {
                referer: Some("https://eros.example".into()),
                title: Some("Eros".into()),
                categories: Some("roleplay,general-chat".into()),
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

        // Categories is checked on the raw received value rather than via
        // wiremock's `header` matcher: that matcher splits the received value
        // on commas, so a comma-joined string would never compare equal. We
        // want to prove the verbatim comma-separated string reaches the wire.
        let reqs = server.received_requests().await.unwrap_or_default();
        let categories = reqs
            .iter()
            .find_map(|r| r.headers.get("x-openrouter-categories"))
            .expect("X-OpenRouter-Categories header present");
        assert_eq!(
            categories.to_str().expect("header is valid utf-8"),
            "roleplay,general-chat"
        );
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
                req.headers.get("x-openrouter-title").is_none(),
                "X-OpenRouter-Title must be absent when unset"
            );
            assert!(
                req.headers.get("x-openrouter-categories").is_none(),
                "X-OpenRouter-Categories must be absent when unset"
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
                categories: Some("still\nbad".into()),
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
                req.headers.get("x-openrouter-title").is_none(),
                "X-OpenRouter-Title must be dropped"
            );
            assert!(
                req.headers.get("x-openrouter-categories").is_none(),
                "X-OpenRouter-Categories must be dropped"
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
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            max_tokens: req.max_tokens,
            stream: false,
            user: req.user.as_deref(),
            session_id: req.session_id.as_deref(),
            metadata: req.metadata.as_ref(),
            reasoning: None,
            provider: None,
            response_format: None,
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
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            max_tokens: req.max_tokens,
            stream: false,
            user: req.user.as_deref(),
            session_id: req.session_id.as_deref(),
            metadata: req.metadata.as_ref(),
            reasoning: None,
            provider: None,
            response_format: None,
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
    fn build_vision_body_has_text_and_image_blocks() {
        let req = VisionRequest {
            model: "ignored".into(),
            system_prompt: "sys".into(),
            image_url: "https://x/y.png".into(),
            caption: Some("看看这个".into()),
            temperature: 0.2,
            max_tokens: 400,
            ..Default::default()
        };
        let body = build_vision_body(&req, "vision-model");
        assert_eq!(body["model"], "vision-model");
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "sys");
        let content = &body["messages"][1]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "看看这个");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "https://x/y.png");
    }

    #[test]
    fn build_vision_body_defaults_text_when_caption_blank() {
        let req = VisionRequest {
            image_url: "https://x/y.png".into(),
            caption: None,
            max_tokens: 1,
            ..Default::default()
        };
        let body = build_vision_body(&req, "m");
        assert_eq!(
            body["messages"][1]["content"][0]["text"],
            "请描述这张图片的内容。"
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
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            max_tokens: 16,
            stream: false,
            user: None,
            session_id: None,
            metadata: None,
            reasoning: Some(&cfg),
            provider: None,
            response_format: None,
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
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            max_tokens: 16,
            stream: false,
            user: None,
            session_id: None,
            metadata: None,
            reasoning: None,
            provider: None,
            response_format: None,
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

    #[test]
    fn wire_request_serializes_response_format_only_when_present() {
        let messages: Vec<ChatMessage> = vec![];
        let rf = serde_json::json!({"type": "json_schema"});
        let wire = WireRequest {
            model: "m",
            messages: &messages,
            temperature: 0.0,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            max_tokens: 16,
            stream: false,
            user: None,
            session_id: None,
            metadata: None,
            reasoning: None,
            provider: None,
            response_format: Some(&rf),
        };
        let s = serde_json::to_string(&wire).unwrap();
        assert!(
            s.contains("\"response_format\":{\"type\":\"json_schema\"}"),
            "{s}"
        );

        let wire_none = WireRequest {
            model: "m",
            messages: &messages,
            temperature: 0.0,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            max_tokens: 16,
            stream: false,
            user: None,
            session_id: None,
            metadata: None,
            reasoning: None,
            provider: None,
            response_format: None,
        };
        let s_none = serde_json::to_string(&wire_none).unwrap();
        assert!(
            !s_none.contains("response_format"),
            "absent ⇒ omitted: {s_none}"
        );
    }

    #[test]
    fn wire_request_omits_provider_when_no_ignore_list() {
        let wire = WireRequest {
            model: "x/y",
            messages: &[],
            temperature: 0.8,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            max_tokens: 100,
            stream: false,
            user: None,
            session_id: None,
            metadata: None,
            reasoning: None,
            provider: None,
            response_format: None,
        };
        let body = serde_json::to_value(&wire).unwrap();
        assert!(
            body.get("provider").is_none(),
            "provider key must be omitted when None"
        );
    }

    #[test]
    fn wire_request_emits_provider_ignore_when_set() {
        let ignore = vec!["BadHost".to_string()];
        let wire = WireRequest {
            model: "x/y",
            messages: &[],
            temperature: 0.8,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            max_tokens: 100,
            stream: false,
            user: None,
            session_id: None,
            metadata: None,
            reasoning: None,
            provider: Some(ProviderPrefs { ignore: &ignore }),
            response_format: None,
        };
        let body = serde_json::to_value(&wire).unwrap();
        assert_eq!(body["provider"]["ignore"][0], "BadHost");
    }

    #[test]
    fn with_ignore_providers_sets_prefs() {
        let c = OpenRouterClient::with_base_url(
            "k".into(),
            AppAttribution::default(),
            "http://localhost".into(),
        )
        .with_ignore_providers(vec!["BadHost".into()]);
        let prefs = c.provider_prefs().expect("prefs present");
        assert_eq!(prefs.ignore, ["BadHost"]);
    }

    /// Garbled string used in garble-guard tests. `Ġ`/`Ċ` density is 2/12 ≈ 16.7 % >> 3 % threshold.
    fn garbled_content() -> serde_json::Value {
        serde_json::json!({
            "choices": [{ "message": { "content": "Hi\u{0120}there\u{010A}bye" } }]
        })
    }

    #[tokio::test]
    async fn execute_falls_back_past_a_garbled_primary() {
        let server = MockServer::start().await;
        // Primary "p" returns garbled content; fallback "f1" returns clean.
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "p"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(garbled_content()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "f1"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "f1",
                "choices": [{ "message": { "content": "hi there" } }]
            })))
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
            .expect("fallback past garbled primary succeeds");
        assert_eq!(resp.reply, "hi there");
        // The served model field comes from the fallback wire response.
        assert_eq!(resp.model.as_deref(), Some("f1"));
    }

    #[tokio::test]
    async fn execute_repairs_when_all_candidates_garbled() {
        let server = MockServer::start().await;
        // Both primary "p" and fallback "f1" return garbled content.
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "p"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(garbled_content()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "f1"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(garbled_content()))
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
            .expect("all-garbled chain returns repaired Ok rather than Err");
        // repair_byte_bpe("HiĠthereĊbye") → "Hi there\nbye"; clean_response trims but
        // does not alter interior spaces/newlines → "Hi there\nbye".
        assert_eq!(resp.reply, "Hi there\nbye");
        assert!(
            resp.generation_id.is_none(),
            "no generation_id when repaired"
        );
        assert!(resp.usage.is_none(), "no usage when repaired");
        // model carried from the last Garbled error — which is "f1" (the last candidate).
        assert_eq!(resp.model.as_deref(), Some("f1"));
    }

    #[tokio::test]
    async fn execute_returns_repaired_garble_even_when_later_candidate_fails() {
        let server = MockServer::start().await;
        // Primary "p" returns recoverable garble; fallback "f1" then fails with a
        // non-garble status error. The salvage must still return p's repaired text
        // (issue #84, Codex P2b) rather than surfacing f1's error.
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "p"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(garbled_content()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "f1"}),
            ))
            .respond_with(ResponseTemplate::new(500))
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
            .expect("earlier garble salvaged despite later non-garble failure");
        assert_eq!(resp.reply, "Hi there\nbye");
        // The repaired text comes from the FIRST (garbled) candidate "p".
        assert_eq!(resp.model.as_deref(), Some("p"));
    }

    #[tokio::test]
    async fn execute_preserves_finish_reason_when_salvaging_garble() {
        let server = MockServer::start().await;
        // A garbled completion whose upstream finish_reason is "content_filter".
        // The salvage must carry that safety signal through (issue #84, Codex P1
        // round 4) so downstream validity gates can still reject filtered content.
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": { "content": "Hi\u{0120}there\u{010A}bye" },
                    "finish_reason": "content_filter"
                }]
            })))
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
                fallback_model: vec![],
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect("garbled response is salvaged");
        assert_eq!(resp.reply, "Hi there\nbye");
        assert_eq!(
            resp.finish_reason.as_deref(),
            Some("content_filter"),
            "the upstream safety finish_reason must survive the garble salvage"
        );
    }

    #[tokio::test]
    async fn execute_does_not_salvage_length_truncated_garble() {
        let server = MockServer::start().await;
        // A garbled completion that is ALSO length-truncated (incomplete). It must
        // NOT be salvaged — repairing partial content and returning it as a success
        // would mislead structured callers (issue #84, Codex round-6 P2).
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": { "content": "Hi\u{0120}there\u{010A}bye" },
                    "finish_reason": "length"
                }]
            })))
            .expect(1)
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
                fallback_model: vec![],
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect_err("length-truncated garble must NOT be salvaged");
        assert!(
            matches!(err, LlmError::Garbled { .. }),
            "expected the Garbled error to surface (caller fails open), got {err:?}"
        );
    }

    #[tokio::test]
    async fn execute_vision_repairs_when_all_candidates_garbled() {
        let server = MockServer::start().await;
        // Single vision candidate returns a GARBLED describe JSON. The last-resort
        // guard must repair it (Ġ/Ċ → space/newline) so the recoverable JSON is
        // returned as Ok rather than dropped to the text-only path — mirrors
        // execute()'s last-resort for chat (issue #84, Codex P2).
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "vp"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": {
                    "content": "{\u{010A}\u{0120}\u{0120}\"description\":\u{0120}\"a\u{0120}cat\"\u{010A}}"
                }}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            AppAttribution::default(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let resp = client
            .execute_vision(VisionRequest {
                model: "vp".into(),
                fallback_model: vec![],
                system_prompt: "describe".into(),
                image_url: "https://example/x.png".into(),
                caption: None,
                temperature: 0.0,
                max_tokens: 64,
                reasoning: None,
            })
            .await
            .expect("garbled vision is repaired into Ok, not dropped");
        // The repaired reply must parse as valid JSON with the recovered field —
        // proving the salvage that the pre-fix code discarded.
        let v: serde_json::Value =
            serde_json::from_str(&resp.reply).expect("repaired describe parses as JSON");
        assert_eq!(v["description"], "a cat");
        assert!(
            resp.generation_id.is_none(),
            "no generation_id when repaired"
        );
        assert_eq!(resp.model.as_deref(), Some("vp"));
    }

    #[test]
    fn wire_request_serializes_sampling_params_when_set() {
        let messages = vec![ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        }];
        let wire = WireRequest {
            model: "m",
            messages: &messages,
            temperature: 0.8,
            top_p: Some(0.9),
            frequency_penalty: Some(0.4),
            presence_penalty: Some(0.2),
            max_tokens: 16,
            stream: false,
            user: None,
            session_id: None,
            metadata: None,
            reasoning: None,
            provider: None,
            response_format: None,
        };
        let s = serde_json::to_string(&wire).unwrap();
        assert!(s.contains("\"top_p\":0.9"), "{s}");
        assert!(s.contains("\"frequency_penalty\":0.4"), "{s}");
        assert!(s.contains("\"presence_penalty\":0.2"), "{s}");
    }

    #[test]
    fn wire_request_omits_sampling_params_when_none() {
        let messages = vec![ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        }];
        let wire = WireRequest {
            model: "m",
            messages: &messages,
            temperature: 0.8,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            max_tokens: 16,
            stream: false,
            user: None,
            session_id: None,
            metadata: None,
            reasoning: None,
            provider: None,
            response_format: None,
        };
        let s = serde_json::to_string(&wire).unwrap();
        assert!(!s.contains("top_p"), "unset top_p must be omitted: {s}");
        assert!(
            !s.contains("frequency_penalty"),
            "unset frequency_penalty must be omitted: {s}"
        );
        assert!(
            !s.contains("presence_penalty"),
            "unset presence_penalty must be omitted: {s}"
        );
    }

    #[test]
    fn image_body_has_modalities_and_optional_face_ref() {
        let req = ImageGenRequest {
            model: "m".into(),
            fallback_model: vec![],
            prompt: "a cat".into(),
            face_ref_url: None,
            aspect_ratio: Some("3:4".into()),
            resolution: None,
            max_tokens: 4096,
        };
        let body = build_image_body(&req, "m");
        assert_eq!(body["modalities"], serde_json::json!(["image", "text"]));
        let content = &body["messages"][0]["content"];
        // text-only content block when no face ref
        assert_eq!(content.as_array().unwrap().len(), 1);
        assert!(content[0]["text"].as_str().unwrap().contains("a cat"));
        assert!(content[0]["text"].as_str().unwrap().contains("3:4"));

        let req2 = ImageGenRequest { face_ref_url: Some("https://x/a.png".into()), ..req };
        let body2 = build_image_body(&req2, "m");
        let content2 = &body2["messages"][0]["content"];
        assert_eq!(content2.as_array().unwrap().len(), 2);
        assert_eq!(content2[1]["type"], "image_url");
        assert_eq!(content2[1]["image_url"]["url"], "https://x/a.png");
    }

    #[test]
    fn image_response_parses_data_url_from_images_array() {
        let wire = serde_json::json!({
            "id": "gen_1",
            "model": "served-model",
            "usage": {"total_tokens": 1},
            "choices": [{
                "message": {
                    "content": "here you go",
                    "images": [{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}]
                },
                "finish_reason": "stop"
            }]
        });
        let parsed: WireResponse = serde_json::from_value(wire).unwrap();
        let imgs = images_from_wire(&parsed);
        assert_eq!(imgs, vec!["data:image/png;base64,AAAA".to_string()]);
    }
}
