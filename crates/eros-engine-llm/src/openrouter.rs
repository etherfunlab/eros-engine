// SPDX-License-Identifier: AGPL-3.0-only
//! OpenRouter chat-completions client. Thin HTTP wrapper around
//! `POST https://openrouter.ai/api/v1/chat/completions`.
//!
//! Returns plain-text reply only; no JSON evaluation.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::LlmError;
use crate::model_config::ReasoningConfig;

const BASE_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

/// Max TCP+TLS establishment time for any OpenRouter call.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// How long an idle pooled connection is kept for reuse.
const POOL_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
/// Max gap between SSE *bytes* before a live stream is declared dead.
const STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// Marker prefix of the idle-watchdog error message. Pub so failure
/// classification can recognize an idle timeout after the message has been
/// stringified through the SSE layer's `Transport error:` wrapper into
/// `LlmError::Stream` (issue #188 — `GatewayKind::IdleTimeout` stays distinct
/// from `GatewayKind::Transport`).
pub const STREAM_IDLE_TIMEOUT_MSG: &str = "openrouter stream idle timeout";

/// Gap-bound a fallible stream: an idle period longer than `idle` between
/// items becomes an io TimedOut error item. Applied to the raw BYTES stream
/// (before SSE parsing) deliberately: OpenRouter's `: OPENROUTER PROCESSING`
/// comment keepalives count as bytes and reset the timer, so a reasoning
/// model thinking for minutes stays alive while a dead peer trips it.
fn idle_bounded<S, T, E>(
    s: S,
    idle: std::time::Duration,
) -> impl futures_util::Stream<Item = Result<T, std::io::Error>>
where
    S: futures_util::Stream<Item = Result<T, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    use tokio_stream::StreamExt as _;
    s.timeout(idle).map(move |r| match r {
        Ok(Ok(b)) => Ok(b),
        Ok(Err(e)) => Err(std::io::Error::other(e)),
        Err(_elapsed) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "{STREAM_IDLE_TIMEOUT_MSG}: no bytes for {}s",
                idle.as_secs()
            ),
        )),
    })
}

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
    /// Optional sampling knobs, resolved from the task block. `None` on a
    /// field ⇒ that wire param is omitted, so a deployment that sets none
    /// produces a byte-identical body to today.
    pub sampling: crate::model_config::Sampling,
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
    /// Engine task this call serves (`chat_companion`, `pde_decision`, …) —
    /// config routing ONLY, never serialized to the wire. Selects which
    /// `[[providers.<name>.body]]` rules apply. `None` (the `Default`) ⇒ no
    /// task-scoped rule matches; unscoped rules still apply.
    pub task: Option<String>,
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
    /// Optional sampling knobs (issue #246). The describe call is an ordinary
    /// chat/completions request in every respect but its `messages` shape.
    pub sampling: crate::model_config::Sampling,
}

/// Task name the vision pre-stage matches body rules under. `VisionRequest`
/// carries no `task` field because the stage is single-purpose — there is
/// exactly one task that posts this body shape.
const VISION_TASK: &str = "chat_vision";

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
    // Sampling knobs are omitted entirely when unset, mirroring WireRequest's
    // `skip_serializing_if` — an untuned deployment must keep producing a
    // byte-identical body (issue #246).
    let mut put = |k: &str, v: Option<f32>| {
        if let Some(x) = v {
            body[k] = serde_json::json!(x);
        }
    };
    put("top_p", req.sampling.top_p);
    put("frequency_penalty", req.sampling.frequency_penalty);
    put("presence_penalty", req.sampling.presence_penalty);
    put("repetition_penalty", req.sampling.repetition_penalty);
    body
}

/// Strip the OpenRouter-specific fields `build_vision_body` bakes in
/// unconditionally, for a custom `[providers]` endpoint (spec §4). Mirrors
/// `WireRequest::for_endpoint`'s drop list, but operates on the raw
/// `serde_json::Value` since the vision body isn't a typed wire struct. A
/// named helper (rather than inlining the strip at each call site) so the
/// `execute_vision` production path and its subset-lock test can never drift
/// apart.
fn strip_openrouter_vision_fields(body: &mut serde_json::Value) {
    if let Some(o) = body.as_object_mut() {
        o.remove("reasoning");
    }
}

/// Max chars kept from a raw provider error body in ordinary logs. Short by
/// design: logs are for triage, not forensics — full error forensics live in
/// OpenRouter's own logs, joined on `generation_id`.
const ERROR_PREVIEW_MAX: usize = 200;

/// Flatten newlines and cap a string to [`ERROR_PREVIEW_MAX`] chars so it is a
/// single bounded log line. An ellipsis marks truncation.
fn body_preview(s: &str) -> String {
    let flat = s.trim().replace('\r', "\\r").replace('\n', "\\n");
    if flat.chars().count() <= ERROR_PREVIEW_MAX {
        flat
    } else {
        flat.chars().take(ERROR_PREVIEW_MAX).collect::<String>() + "…"
    }
}

/// `Retry-After` as whole seconds. The delta-seconds form is honoured; the
/// HTTP-date form returns `None` rather than being resolved against a clock —
/// the engine does not act on this value, it only records and forwards it.
pub fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u32> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

/// A provider error body, parsed into parts instead of flattened to a string.
///
/// `Display` reproduces exactly what `scrub_error_body` used to return, so every
/// log line and every existing assertion is byte-identical; the named fields are
/// what the audit columns read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedErrorBody {
    /// OpenRouter `error.code` (numeric) or Venice `error.code` (string),
    /// rendered as its JSON text so a numeric `529` and a string
    /// `"MODEL_OVERLOADED"` stay distinguishable.
    pub code: Option<String>,
    /// OpenRouter `metadata.error_type`, or the OpenAI-compatible `error.type`.
    pub error_type: Option<String>,
    /// OpenRouter `metadata.provider_code` — the provider's own upstream code.
    pub provider_code: Option<String>,
    /// Bounded, single-line, prompt-free.
    pub message: String,
}

impl ParsedErrorBody {
    /// For call sites that have prose rather than an error envelope.
    pub fn message_only(s: &str) -> Self {
        Self {
            message: s.to_string(),
            ..Default::default()
        }
    }
}

impl std::fmt::Display for ParsedErrorBody {
    /// `message` is already the fully assembled, bounded, single-line string
    /// that `scrub_error_body` used to return — code and metadata are folded
    /// into it by `parse_error_body`. The named fields are a parallel view for
    /// the audit columns, not extra text to append here.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Turn a raw provider error body into parts instead of flattening it to a
/// string. Best-effort parses the OpenRouter `{"error":{code,message,metadata}}`
/// envelope and keeps `code` (as `serde_json::Value` — codes are sometimes
/// strings, not ints), `metadata.error_type` / `metadata.provider_code`, a
/// length-capped `message`, and — from a moderation `metadata` block —
/// `provider_name` + `reasons`. It deliberately DROPS `metadata.flagged_input`,
/// which is an excerpt of the user's flagged prompt that a moderation rejection
/// echoes back (logging it would leak raw chat content). Also handles Venice's
/// two shapes: the OpenAI-compatible envelope (semantic name in string `code`,
/// family in `type`, no `metadata`) and the bare `{"error": "..."}` string form.
/// Non-envelope bodies fall back to a plain length-capped preview.
///
/// `pub`: `ParsedErrorBody` sits in `LlmError`, a `pub` type, so this must be
/// too. The voyage and embeddings clients also reuse it for their own
/// status-error bodies (issue #188) — the envelope parse simply falls through
/// to the capped preview for non-OpenRouter shapes.
pub fn parse_error_body(raw: &str) -> ParsedErrorBody {
    #[derive(Deserialize)]
    struct Env {
        error: ErrField,
    }
    // Venice returns either a bare string or an object under `error`.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ErrField {
        Text(String),
        Body(Box<ErrBody>),
    }
    #[derive(Deserialize, Default)]
    struct ErrBody {
        #[serde(default)]
        code: Option<serde_json::Value>,
        #[serde(default)]
        message: Option<String>,
        /// OpenAI-compatible family name (Venice). OpenRouter puts its
        /// equivalent under `metadata.error_type`, read below.
        #[serde(default, rename = "type")]
        ty: Option<String>,
        #[serde(default)]
        metadata: Option<serde_json::Value>,
    }

    let Ok(env) = serde_json::from_str::<Env>(raw) else {
        return ParsedErrorBody {
            message: body_preview(raw),
            ..Default::default()
        };
    };
    let body = match env.error {
        ErrField::Text(t) => ErrBody {
            message: Some(t),
            ..Default::default()
        },
        ErrField::Body(b) => *b,
    };

    let code = body.code.map(|c| c.to_string());
    let meta = body.metadata.as_ref();
    let meta_str = |key: &str| -> Option<String> {
        meta.and_then(|m| m.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let error_type = meta_str("error_type").or(body.ty);
    let provider_code = meta_str("provider_code");

    // Assemble the human-readable parts, then run the WHOLE string through
    // body_preview once. provider_name / reasons are provider-controlled and
    // could carry newlines or be arbitrarily long, so the single final
    // flatten+cap is what upholds the "bounded, single-line" guarantee for
    // every field — not just the message. flagged_input is never read: it is
    // the user's own prompt excerpt and must not reach a log or an audit row.
    let mut out = format!(
        "code={}: {}",
        code.as_deref().unwrap_or("?"),
        body.message.as_deref().unwrap_or("")
    );
    if let Some(p) = meta_str("provider_name") {
        out.push_str(&format!(" [provider={p}]"));
    }
    if let Some(reasons) = meta
        .and_then(|m| m.get("reasons"))
        .and_then(|v| v.as_array())
    {
        let joined: Vec<&str> = reasons.iter().filter_map(|r| r.as_str()).collect();
        if !joined.is_empty() {
            out.push_str(&format!(" [moderation_reasons={}]", joined.join(",")));
        }
    }

    ParsedErrorBody {
        code,
        error_type,
        provider_code,
        message: body_preview(&out),
    }
}

/// A 200 body that failed to decode as a chat/vision completion: if it is in
/// fact an OpenRouter error envelope (`{"error":...}` with no `choices`),
/// surface its scrubbed message as a `Provider` error so the candidate chain
/// advances with a useful, redacted reason; otherwise the ordinary `Decode`
/// error (whose `Display` carries only a serde offset, never the body).
fn decode_or_api_error(body: &str, err: serde_json::Error) -> LlmError {
    let is_api_error = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").cloned())
        .is_some();
    if is_api_error {
        LlmError::Provider(ParsedErrorBody::message_only(&format!(
            "openrouter 200 error body: {}",
            parse_error_body(body)
        )))
    } else {
        LlmError::Decode(err)
    }
}

/// Build the `ParsedErrorBody` for a mid-stream provider error frame (a
/// top-level `error` object on an otherwise-200 SSE stream). The `"mid-stream
/// error: "` marker is the only thing that distinguishes this from a 200-body
/// error envelope caught by `decode_or_api_error` — nothing was sent there,
/// whereas here the provider started streaming and then failed, so partial
/// content may already be out. Both classify as `Provider` errors at
/// `http_status: 200`, so without the marker nothing tells them apart. No
/// vendor name in the marker: this client also serves Venice and any custom
/// OpenAI-compatible endpoint via the `@provider` suffix, and `"openrouter
/// ..."` on a Venice stream would simply be false.
fn mid_stream_error_body(code: Option<&serde_json::Value>, message: &str) -> ParsedErrorBody {
    let code = code.map(|c| c.to_string());
    ParsedErrorBody {
        code: code.clone(),
        message: body_preview(&format!(
            "mid-stream error: code={}: {}",
            code.as_deref().unwrap_or("?"),
            message
        )),
        ..Default::default()
    }
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
    /// Every hop that failed before the served one. Populated on success too:
    /// a turn that recovered on the second model still has to report what the
    /// first one said, which used to leave no trace anywhere.
    #[serde(default)]
    pub failures: Vec<crate::failure::AttemptFailure>,
}

fn is_false(b: &bool) -> bool {
    !*b
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
    #[serde(skip_serializing_if = "Option::is_none")]
    repetition_penalty: Option<f32>,
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
    response_format: Option<&'a serde_json::Value>,
}

impl<'a> WireRequest<'a> {
    /// Strip the OpenRouter-specific fields for a custom endpoint (spec §4):
    /// custom providers receive a strict OpenAI chat-completions subset. All
    /// three fields carry `skip_serializing_if`, so `None` removes them from
    /// the wire entirely — one serialization path, no drift. NOTE: when
    /// adding a field to WireRequest, decide its fate here; the
    /// `custom_endpoint_wire_is_strict_openai_subset` test enforces it.
    ///
    /// Body rules merge AFTER this strip (spec 2026-08-02-provider-body-params
    /// §4): a custom provider's declared `[[providers.<name>.body]]` params
    /// may deliberately reintroduce a vendor field this strip just removed
    /// (e.g. its own `reasoning` shape) — that is the supported way to send
    /// provider-specific params, not a leak. The strict-subset lock
    /// (`custom_endpoint_wire_is_strict_openai_subset`) only covers the
    /// no-rules default.
    fn for_endpoint(mut self, ep: &Endpoint<'_>) -> Self {
        if ep.name.is_some() {
            self.session_id = None;
            self.metadata = None;
            self.reasoning = None;
        }
        self
    }
}

/// True when `rule` applies to a call serving `task` (spec
/// 2026-08-02-provider-body-params): no `tasks` list ⇒ applies always;
/// a task-scoped rule requires an exact, case-sensitive name match.
fn rule_matches(rule: &crate::model_config::BodyRule, task: Option<&str>) -> bool {
    match (&rule.tasks, task) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(list), Some(t)) => list.iter().any(|x| x == t),
    }
}

/// Merge every matching rule's params into the serialized wire body:
/// top-level shallow, declaration order, later wins — and the merged params
/// win over engine-built fields (that ordering is what makes the
/// `[providers.openrouter]` `reasoning` override `[tasks.*].reasoning`).
/// Structural keys (`model`/`messages`/`stream`) were refused at boot. Pure.
fn apply_body_rules(
    body: &mut serde_json::Map<String, serde_json::Value>,
    rules: &[crate::model_config::BodyRule],
    task: Option<&str>,
) {
    for rule in rules.iter().filter(|r| rule_matches(r, task)) {
        for (k, v) in &rule.params {
            body.insert(k.clone(), v.clone());
        }
    }
}

#[derive(Debug, Deserialize)]
struct WireMessage {
    #[serde(default)]
    content: Option<String>,
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

/// Top-level `error` object OpenRouter embeds in an HTTP-200 SSE data frame
/// when a provider fails mid-stream (docs: "API Streaming — error handling").
/// `code` is upstream-defined (int or string) — kept opaque.
#[derive(Debug, Deserialize)]
struct WireStreamError {
    #[serde(default)]
    code: Option<serde_json::Value>,
    #[serde(default)]
    message: String,
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
    #[serde(default)]
    error: Option<WireStreamError>,
}

#[derive(Clone)]
pub struct OpenRouterClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    /// Custom `[providers]` endpoints, name → endpoint. Empty by default;
    /// installed at boot via [`OpenRouterClient::with_providers`]. Arc so the
    /// client's Clone stays cheap.
    providers: Arc<HashMap<String, crate::provider::ProviderEndpoint>>,
    /// `[providers].openrouter.body` rules for the built-in endpoint. Empty
    /// by default; installed at boot via
    /// [`OpenRouterClient::with_openrouter_body_rules`]. Custom `[providers]`
    /// endpoints carry their own rules on `ProviderEndpoint.body_rules`
    /// instead.
    openrouter_body_rules: Vec<crate::model_config::BodyRule>,
    /// HTTP client WITHOUT the `[providers].openrouter.headers` default
    /// headers, used for every custom-provider post. Those default headers
    /// are baked into `http` at boot via [`OpenRouterClient::with_openrouter_headers`]
    /// and cannot be withdrawn per-request; custom `[providers]` endpoints
    /// instead carry their own declared headers per-request (spec §3). Same
    /// connect/pool bounds as `http`.
    plain_http: reqwest::Client,
}

impl OpenRouterClient {
    /// Production constructor. Pins to OpenRouter's canonical URL.
    pub fn new(api_key: String) -> Self {
        Self::with_base_url(api_key, BASE_URL.to_string())
    }

    /// Low-level constructor that lets callers override the OpenRouter
    /// endpoint. Intended for integration tests (workspace-internal and
    /// downstream) that wire a wiremock or fake server in front of the
    /// client. Production code should use `new`, which pins to OpenRouter's
    /// canonical URL.
    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        // http and plain_http start identical; with_openrouter_headers
        // rebuilds `http` with default headers when the config declares any.
        //
        // connect/pool bounds only — deliberately NO global `.timeout()` or
        // client-level read timeout: both would also bound non-streaming calls
        // (image generation legitimately spends its whole wall-time before the
        // first body byte). Stream liveness is `idle_bounded`'s job.
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .build()
            .expect("reqwest client build never fails with empty config");
        let plain_http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .build()
            .expect("reqwest client build never fails with empty config");
        Self {
            http,
            plain_http,
            api_key,
            base_url,
            providers: Arc::new(HashMap::new()),
            openrouter_body_rules: Vec::new(),
        }
    }

    /// Install `[providers].openrouter.headers` as default headers on the
    /// built-in-endpoint client. Consuming builder, boot-chained. `plain_http`
    /// (custom providers) is untouched — their headers ride per-request.
    pub fn with_openrouter_headers(mut self, headers: reqwest::header::HeaderMap) -> Self {
        if !headers.is_empty() {
            self.http = reqwest::Client::builder()
                .default_headers(headers)
                .connect_timeout(CONNECT_TIMEOUT)
                .pool_idle_timeout(POOL_IDLE_TIMEOUT)
                .build()
                .expect("reqwest client build never fails with static config");
        }
        self
    }

    /// Override the built-in chat URL from `[providers].openrouter.chat`.
    /// `None` keeps the pinned default. Replaces the removed
    /// OPENROUTER_BASE_URL env var.
    pub fn with_openrouter_chat_url(mut self, url: Option<String>) -> Self {
        if let Some(u) = url {
            self.base_url = u;
        }
        self
    }

    /// Install the `[providers]` endpoint map (multi-provider spec §3).
    /// Consuming builder, chained at boot.
    pub fn with_providers(
        mut self,
        providers: HashMap<String, crate::provider::ProviderEndpoint>,
    ) -> Self {
        self.providers = Arc::new(providers);
        self
    }

    /// Install `[providers].openrouter.body` rules for the built-in endpoint
    /// (custom providers carry theirs on `ProviderEndpoint`). Consuming
    /// builder, boot-chained.
    pub fn with_openrouter_body_rules(mut self, rules: Vec<crate::model_config::BodyRule>) -> Self {
        self.openrouter_body_rules = rules;
        self
    }
}

/// A resolved posting target: where one candidate's request goes.
/// `name: None` ⇒ the built-in OpenRouter endpoint (config-driven default
/// headers ride on `http`, full wire); `Some(name)` ⇒ a `[providers]` entry
/// (plain client, its own declared headers threaded per-request, strict
/// OpenAI wire subset, audit model suffixed `@name`).
struct Endpoint<'a> {
    url: &'a str,
    api_key: &'a str,
    http: &'a reqwest::Client,
    name: Option<&'a str>,
    /// `None` for the built-in endpoint (default headers already ride on
    /// `http`); `Some(&ep.headers)` for a custom `[providers]` endpoint,
    /// applied per-request since `plain_http` carries no default headers.
    headers: Option<&'a reqwest::header::HeaderMap>,
}

impl OpenRouterClient {
    /// Split a candidate slug and resolve where it posts (spec §3). The
    /// api-key emptiness guard lives HERE, per endpoint, rather than at the
    /// head of each execute method — a mixed chain must check the key of the
    /// endpoint each candidate actually uses. Unknown provider ⇒
    /// `LlmError::Config`, which advances the caller's candidate chain
    /// instead of panicking (unreachable for config slugs post-boot, but
    /// `execute_stream_as` receives arbitrary server strings).
    ///
    /// `openrouter` is a reserved alias for "no suffix" (spec §3/§4): a slug
    /// suffixed `@openrouter` must be byte-for-byte equivalent to the same
    /// slug with no suffix at all — built-in endpoint, attributed `http`
    /// client, full OpenRouter wire, no `[providers]` lookup. It is handled
    /// in THIS arm, not `Some(p)`, precisely so it never falls into the
    /// custom-provider path (plain client, stripped wire, audit suffix).
    fn resolve_endpoint<'s>(&'s self, slug: &str) -> Result<(String, Endpoint<'s>), LlmError> {
        let (bare, provider) = crate::provider::split_model_slug(slug)
            .map_err(|e| LlmError::Config(format!("openrouter: {e}")))?;
        match provider {
            None | Some("openrouter") => {
                if self.api_key.is_empty() {
                    return Err(LlmError::Config("openrouter: api key not set".into()));
                }
                Ok((
                    bare,
                    Endpoint {
                        url: &self.base_url,
                        api_key: &self.api_key,
                        http: &self.http,
                        name: None,
                        headers: None,
                    },
                ))
            }
            Some(p) => {
                let (name, ep) = self.providers.get_key_value(p).ok_or_else(|| {
                    LlmError::Config(format!(
                        "openrouter: model slug `{slug}` names undeclared provider `{p}`"
                    ))
                })?;
                if ep.api_key.is_empty() {
                    return Err(LlmError::Config(format!(
                        "openrouter: provider `{p}`: api key not set"
                    )));
                }
                Ok((
                    bare,
                    Endpoint {
                        url: &ep.base_url,
                        api_key: &ep.api_key,
                        http: &self.plain_http,
                        name: Some(name.as_str()),
                        headers: Some(&ep.headers),
                    },
                ))
            }
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

        let mut failures: Vec<crate::failure::AttemptFailure> = Vec::new();
        let task = req.task.as_deref().unwrap_or("");
        // Latest recoverable byte-BPE garble seen while walking the chain, kept
        // separately from `failures` so a LATER non-garble failure (transport /
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
                    req.sampling,
                    req.user.as_deref(),
                    req.session_id.as_deref(),
                    req.metadata.as_ref(),
                    req.reasoning.as_ref(),
                    req.response_format.as_ref(),
                    req.task.as_deref(),
                )
                .await
            {
                Ok(mut resp) => {
                    resp.failures = failures;
                    return Ok(resp);
                }
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
                    // A garble is a content verdict — the call succeeded and
                    // was billed — so it belongs to the caller's coarse marker
                    // and to NEITHER column (spec §2).
                    if crate::failure::AttemptFailure::should_record(&e) {
                        failures.push(crate::failure::AttemptFailure::from_llm_error(
                            task, model, &e,
                        ));
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
                failures: failures.clone(),
            });
        }
        Err(LlmError::Chain { failures })
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
        let mut failures: Vec<crate::failure::AttemptFailure> = Vec::new();
        // `VisionRequest` carries no `task` field (see `VISION_TASK` above) —
        // the vision pre-stage is single-purpose, so its task name is fixed.
        let task = VISION_TASK;
        // Latest recoverable garble, kept separate so a later non-garble failure
        // can't discard a repairable earlier garble (mirrors `execute`). Tuple:
        // (model, raw, finish_reason).
        let mut last_garbled: Option<(String, String, Option<String>)> = None;
        for model in &candidates {
            let (bare_model, ep) = match self.resolve_endpoint(model) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(model = %model, error = %e, "openrouter: vision attempt failed (endpoint); next");
                    failures.push(crate::failure::AttemptFailure::from_llm_error(
                        task, model, &e,
                    ));
                    continue;
                }
            };
            let mut body = build_vision_body(&req, &bare_model);
            if ep.name.is_some() {
                // Custom `[providers]` endpoints get a strict OpenAI subset
                // (spec §4) — `reasoning` is an OpenRouter-specific extension
                // that `build_vision_body` bakes in unconditionally, so strip
                // it back out here, mirroring `WireRequest::for_endpoint`.
                strip_openrouter_vision_fields(&mut body);
            }
            // Body rules reach the vision pre-stage too (issue #225). Applied
            // AFTER the subset strip, mirroring `call_once`'s strip-then-merge
            // order: that is what lets a rule on a custom endpoint put back an
            // extension the strip removed. `messages` (a block array here, not
            // the chat shape) can't be clobbered — it is refused at boot along
            // with `model`/`stream`.
            let rules: &[crate::model_config::BodyRule] = match ep.name {
                None => &self.openrouter_body_rules,
                Some(p) => self
                    .providers
                    .get(p)
                    .map(|e| e.body_rules.as_slice())
                    .unwrap_or(&[]),
            };
            if let Some(map) = body.as_object_mut() {
                apply_body_rules(map, rules, Some(VISION_TASK));
            }
            let mut builder = ep.http.post(ep.url).bearer_auth(ep.api_key);
            if let Some(h) = ep.headers {
                builder = builder.headers(h.clone());
            }
            let resp = match builder.json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(model = %model, error = %e, "openrouter: vision attempt failed (transport); next");
                    let e: LlmError = e.into();
                    failures.push(crate::failure::AttemptFailure::from_llm_error(
                        task, model, &e,
                    ));
                    continue;
                }
            };
            let status = resp.status();
            if !status.is_success() {
                let retry_after = retry_after_secs(resp.headers());
                let text = resp.text().await.unwrap_or_default();
                tracing::warn!(model = %model, %status, "openrouter: vision attempt failed (status); next");
                let e = LlmError::Status(status, parse_error_body(&text), retry_after);
                failures.push(crate::failure::AttemptFailure::from_llm_error(
                    task, model, &e,
                ));
                continue;
            }
            let body = match resp.text().await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(model = %model, error = %e, "openrouter: vision attempt failed (transport); next");
                    let e: LlmError = e.into();
                    failures.push(crate::failure::AttemptFailure::from_llm_error(
                        task, model, &e,
                    ));
                    continue;
                }
            };
            let parsed: WireResponse = match serde_json::from_str::<WireResponse>(&body) {
                Ok(p) => p,
                Err(e) => {
                    let err = decode_or_api_error(&body, e);
                    tracing::warn!(model = %model, error = %err, "openrouter: vision attempt failed (decode); next");
                    failures.push(crate::failure::AttemptFailure::from_llm_error(
                        task, model, &err,
                    ));
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
                // Nothing is pushed: a garble is a content verdict, the call
                // succeeded and was billed, and neither column owns it
                // (spec §2 — the rule is `AttemptFailure::should_record`).
                // Retain only a COMPLETE garble for last-resort salvage; a
                // length-truncated garble is incomplete, so repairing it would
                // hand partial JSON to the caller as if it were whole.
                if finish_reason.as_deref() != Some("length") {
                    last_garbled = Some((model.to_string(), raw, finish_reason));
                }
                continue;
            }
            // §6: a custom row self-identifies as <echo>@<provider> so a failed
            // eros-audit join on generation_id explains itself from the model
            // column. OpenRouter rows are byte-identical to before. The echo /
            // bare id is escaped so a literal `@` in it doesn't produce a
            // second unescaped `@`, which `split_model_slug` would reject.
            let model_out = match ep.name {
                None => parsed.model,
                Some(p) => Some(match parsed.model {
                    Some(echo) => format!("{}@{p}", crate::provider::escape_model_id(&echo)),
                    None => format!("{}@{p}", crate::provider::escape_model_id(&bare_model)),
                }),
            };
            return Ok(ChatResponse {
                reply: clean_response(raw.trim()),
                generation_id: parsed.id,
                model: model_out,
                usage: parsed.usage,
                finish_reason,
                failures,
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
                failures: failures.clone(),
            });
        }
        Err(LlmError::Chain { failures })
    }

    #[allow(clippy::too_many_arguments)]
    async fn call_once(
        &self,
        model: &str,
        messages: &[ChatMessage],
        temperature: f32,
        max_tokens: u32,
        sampling: crate::model_config::Sampling,
        req_user: Option<&str>,
        req_session_id: Option<&str>,
        req_metadata: Option<&serde_json::Map<String, serde_json::Value>>,
        req_reasoning: Option<&ReasoningConfig>,
        req_response_format: Option<&serde_json::Value>,
        req_task: Option<&str>,
    ) -> Result<ChatResponse, LlmError> {
        let (bare_model, ep) = self.resolve_endpoint(model)?;
        let wire = WireRequest {
            model: &bare_model,
            messages,
            temperature,
            top_p: sampling.top_p,
            frequency_penalty: sampling.frequency_penalty,
            presence_penalty: sampling.presence_penalty,
            repetition_penalty: sampling.repetition_penalty,
            max_tokens,
            stream: false,
            user: req_user,
            session_id: req_session_id,
            metadata: req_metadata,
            reasoning: req_reasoning,
            response_format: req_response_format,
        }
        .for_endpoint(&ep);

        let rules: &[crate::model_config::BodyRule] = match ep.name {
            None => &self.openrouter_body_rules,
            Some(p) => self
                .providers
                .get(p)
                .map(|e| e.body_rules.as_slice())
                .unwrap_or(&[]),
        };
        let mut builder = ep.http.post(ep.url).bearer_auth(ep.api_key);
        if let Some(h) = ep.headers {
            builder = builder.headers(h.clone());
        }
        let resp = if rules.iter().any(|r| rule_matches(r, req_task)) {
            let mut v = serde_json::to_value(&wire)
                .map_err(|e| LlmError::Config(format!("openrouter: wire serialize: {e}")))?;
            let map = v
                .as_object_mut()
                .expect("WireRequest always serializes to a JSON object");
            apply_body_rules(map, rules, req_task);
            builder.json(&v).send().await?
        } else {
            builder.json(&wire).send().await?
        };

        let status = resp.status();
        if !status.is_success() {
            let retry_after = retry_after_secs(resp.headers());
            let text = resp.text().await.unwrap_or_default();
            return Err(LlmError::Status(
                status,
                parse_error_body(&text),
                retry_after,
            ));
        }

        // Read as text so a 200 body that is actually an error envelope
        // (`{"error":...}` with no `choices`) surfaces the provider message
        // instead of a bare "missing field choices" decode error.
        let body = resp.text().await?;
        let parsed: WireResponse = match serde_json::from_str::<WireResponse>(&body) {
            Ok(p) => p,
            Err(e) => return Err(decode_or_api_error(&body, e)),
        };
        let first_choice = parsed.choices.into_iter().next();
        let raw = first_choice
            .as_ref()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();
        let finish_reason = first_choice.and_then(|c| c.finish_reason);
        // A non-stream completion that finished with finish_reason="error" is a
        // mid-generation provider death (Batch A fixed the streaming path only).
        // Fail the attempt so `execute`'s chain advances rather than returning a
        // partial reply that callers' validity gates would accept as complete.
        if finish_reason.as_deref() == Some("error") {
            return Err(LlmError::Provider(ParsedErrorBody::message_only(
                "openrouter: non-stream completion finished with finish_reason=error",
            )));
        }
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
        // §6: a custom row self-identifies as <echo>@<provider> so a failed
        // eros-audit join on generation_id explains itself from the model
        // column. OpenRouter rows are byte-identical to before. The echo /
        // bare id is escaped so a literal `@` in it doesn't produce a
        // second unescaped `@`, which `split_model_slug` would reject.
        let model_out = match ep.name {
            None => parsed.model,
            Some(p) => Some(match parsed.model {
                Some(echo) => format!("{}@{p}", crate::provider::escape_model_id(&echo)),
                None => format!("{}@{p}", crate::provider::escape_model_id(&bare_model)),
            }),
        };
        Ok(ChatResponse {
            reply: clean_response(raw.trim()),
            generation_id: parsed.id,
            model: model_out,
            usage: parsed.usage,
            finish_reason,
            failures: Vec::new(),
        })
    }

    /// Open a streaming chat completion against a single model. Fallback
    /// chain handling is the caller's responsibility (pipeline layer). Owns
    /// its `ChatRequest`; retained as the stable public entry point. In-tree
    /// fallback loops use [`execute_stream_as`](Self::execute_stream_as) to
    /// avoid re-cloning the prompt per attempt.
    pub async fn execute_stream(&self, req: ChatRequest) -> Result<DeltaStream, LlmError> {
        let model = req.model.clone();
        self.execute_stream_as(&req, &model).await
    }

    /// Like [`execute_stream`](Self::execute_stream) but borrows the request and
    /// takes the served `model` separately, so a fallback chain can retry the
    /// same (large) prompt against each candidate without deep-cloning it per
    /// attempt. `req.model` / `req.fallback_model` are ignored — `model` is the
    /// one actually sent.
    pub async fn execute_stream_as(
        &self,
        req: &ChatRequest,
        model: &str,
    ) -> Result<DeltaStream, LlmError> {
        use eventsource_stream::Eventsource;
        use futures_util::StreamExt;

        if model.is_empty() {
            return Err(LlmError::Config(
                "openrouter: execute_stream requires a non-empty model".into(),
            ));
        }

        // Mirror the sync `call_once` wire: a hand-rolled `json!` here once
        // serialised unset audit fields as `user: null`, which OpenRouter
        // rejects (400 "expected string, received null"). Sharing WireRequest
        // keeps the skip-None behaviour and stops the two paths from drifting.
        let (bare_model, ep) = self.resolve_endpoint(model)?;
        let wire = WireRequest {
            model: &bare_model,
            messages: &req.messages,
            temperature: req.temperature,
            top_p: req.sampling.top_p,
            frequency_penalty: req.sampling.frequency_penalty,
            presence_penalty: req.sampling.presence_penalty,
            repetition_penalty: req.sampling.repetition_penalty,
            max_tokens: req.max_tokens,
            stream: true,
            user: req.user.as_deref(),
            session_id: req.session_id.as_deref(),
            metadata: req.metadata.as_ref(),
            reasoning: req.reasoning.as_ref(),
            response_format: None,
        }
        .for_endpoint(&ep);

        let rules: &[crate::model_config::BodyRule] = match ep.name {
            None => &self.openrouter_body_rules,
            Some(p) => self
                .providers
                .get(p)
                .map(|e| e.body_rules.as_slice())
                .unwrap_or(&[]),
        };
        let req_task = req.task.as_deref();
        let started = std::time::Instant::now();
        let mut builder = ep
            .http
            .post(ep.url)
            .bearer_auth(ep.api_key)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        if let Some(h) = ep.headers {
            builder = builder.headers(h.clone());
        }
        let resp = if rules.iter().any(|r| rule_matches(r, req_task)) {
            let mut v = serde_json::to_value(&wire)
                .map_err(|e| LlmError::Config(format!("openrouter: wire serialize: {e}")))?;
            let map = v
                .as_object_mut()
                .expect("WireRequest always serializes to a JSON object");
            apply_body_rules(map, rules, req_task);
            builder.json(&v).send().await?
        } else {
            builder.json(&wire).send().await?
        };

        let status = resp.status();
        if !status.is_success() {
            let retry_after = retry_after_secs(resp.headers());
            let text = resp.text().await.unwrap_or_default();
            return Err(LlmError::Status(
                status,
                parse_error_body(&text),
                retry_after,
            ));
        }

        // Observability: connect+headers latency and the negotiated HTTP
        // version (should read HTTP/2.0 post-Batch-A3). Prompt content is never
        // logged — only the model id and timing.
        tracing::debug!(
            target: "openrouter_stream",
            model = %model,
            headers_ms = started.elapsed().as_millis() as u64,
            http_version = ?resp.version(),
            "stream opened"
        );

        // Capture the authoritative generation id from the X-Generation-Id
        // header the moment headers arrive, so a stream that dies before its
        // first id-bearing body chunk still yields an audit handle. Prepended
        // as a synthetic first chunk; the pipeline's "latest non-None wins"
        // accumulation adopts it, and a later body `id` (identical) overwrites.
        let header_gen_id = resp
            .headers()
            .get("x-generation-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let head = futures_util::stream::iter(header_gen_id.map(|id| {
            Ok(DeltaChunk {
                generation_id: Some(id),
                ..Default::default()
            })
        }));

        let stream = idle_bounded(resp.bytes_stream(), STREAM_IDLE_TIMEOUT)
            .eventsource()
            .filter_map(|ev| async move {
                match ev {
                    Ok(e) => {
                        if e.data == "[DONE]" {
                            return None;
                        }
                        match serde_json::from_str::<WireStreamFrame>(&e.data) {
                            Ok(frame) => {
                                // A mid-stream provider failure arrives as a
                                // normal-looking 200 SSE frame with a top-level
                                // `error` (and/or finish_reason:"error"). It
                                // must fail the attempt so the pipeline's
                                // fallback chain runs — NOT parse as an
                                // all-None chunk that lets a partial reply
                                // persist as a clean success.
                                if let Some(err) = frame.error {
                                    // The provider spoke inside a 200 stream.
                                    // Keep the code structured — this used to
                                    // be format!("code={:?}") into a String,
                                    // which destroyed it.
                                    return Some(Err(LlmError::Provider(mid_stream_error_body(
                                        err.code.as_ref(),
                                        &err.message,
                                    ))));
                                }
                                let choice = frame.choices.into_iter().next().unwrap_or_default();
                                if choice.finish_reason.as_deref() == Some("error") {
                                    return Some(Err(LlmError::Provider(
                                        ParsedErrorBody::message_only(
                                            "openrouter stream terminated with finish_reason=error",
                                        ),
                                    )));
                                }
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

        Ok(DeltaStream(head.chain(stream).boxed()))
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

    fn rule(tasks: Option<&[&str]>, params: serde_json::Value) -> crate::model_config::BodyRule {
        crate::model_config::BodyRule {
            tasks: tasks.map(|t| t.iter().map(|s| s.to_string()).collect()),
            params: params.as_object().unwrap().clone(),
        }
    }

    #[test]
    fn rule_matching_semantics() {
        let scoped = rule(Some(&["chat_companion"]), serde_json::json!({"a": 1}));
        let open = rule(None, serde_json::json!({"b": 2}));
        assert!(rule_matches(&scoped, Some("chat_companion")));
        assert!(!rule_matches(&scoped, Some("pde_decision")));
        assert!(
            !rule_matches(&scoped, None),
            "task-scoped rule never matches a task-less request"
        );
        assert!(rule_matches(&open, Some("anything")));
        assert!(
            rule_matches(&open, None),
            "unscoped rule applies even without a task"
        );
    }

    #[test]
    fn apply_body_rules_merges_in_order_later_wins() {
        let mut body = serde_json::json!({"model": "m", "temperature": 0.5})
            .as_object()
            .unwrap()
            .clone();
        let rules = [
            rule(
                None,
                serde_json::json!({"venice_parameters": {"x": 1}, "temperature": 0.9}),
            ),
            rule(
                Some(&["chat_companion"]),
                serde_json::json!({"venice_parameters": {"x": 2}}),
            ),
            rule(Some(&["pde_decision"]), serde_json::json!({"never": true})),
        ];
        apply_body_rules(&mut body, &rules, Some("chat_companion"));
        assert_eq!(
            body["venice_parameters"]["x"], 2,
            "later matching rule wins"
        );
        assert_eq!(
            body["temperature"], 0.9,
            "params win over engine-built fields"
        );
        assert_eq!(body["model"], "m", "untouched keys survive");
        assert!(body.get("never").is_none(), "non-matching rule skipped");
    }

    #[test]
    fn body_rules_override_max_tokens_and_sampling() {
        // Companion lock to the test above, which only covers `temperature`.
        // `[[providers.<name>.body]]` params beat every engine-built wire
        // field; only the three engine-owned structural keys
        // (model/messages/stream) are exempt, and those are refused at boot.
        // Issue #246 leans on this being true for max_tokens and the four
        // sampling knobs, so it is locked rather than assumed.
        // Every key the rule touches is already present, so this proves
        // OVERRIDE rather than mere insertion.
        let mut body = serde_json::json!({
            "model": "m",
            "temperature": 0.5,
            "max_tokens": 200,
            "top_p": 0.9,
            "frequency_penalty": 0.0,
            "presence_penalty": 0.0,
            "repetition_penalty": 1.0
        })
        .as_object()
        .unwrap()
        .clone();
        let rules = [rule(
            Some(&["chat_image_prompt_compose"]),
            serde_json::json!({
                "temperature": 0.11,
                "max_tokens": 900,
                "top_p": 0.5,
                "frequency_penalty": 0.75,
                "presence_penalty": -0.25,
                "repetition_penalty": 1.25
            }),
        )];
        apply_body_rules(&mut body, &rules, Some("chat_image_prompt_compose"));
        // All six overridable keys the docs name, not a sample of them.
        assert_eq!(body["temperature"], 0.11);
        assert_eq!(body["max_tokens"], 900);
        assert_eq!(body["top_p"], 0.5);
        assert_eq!(body["frequency_penalty"], 0.75);
        assert_eq!(body["presence_penalty"], -0.25);
        assert_eq!(body["repetition_penalty"], 1.25);
        assert_eq!(body["model"], "m", "engine-owned key untouched");
    }

    #[tokio::test]
    async fn client_sends_configured_openrouter_headers() {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(header("HTTP-Referer", "https://eros.example"))
            .and(header("X-OpenRouter-Title", "Eros"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&server)
            .await;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("HTTP-Referer", "https://eros.example".parse().unwrap());
        headers.insert("X-OpenRouter-Title", "Eros".parse().unwrap());
        headers.insert(
            "X-OpenRouter-Categories",
            "roleplay,general-chat".parse().unwrap(),
        );
        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            format!("{}/api/v1/chat/completions", server.uri()),
        )
        .with_openrouter_headers(headers);
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
        assert_eq!(categories.to_str().unwrap(), "roleplay,general-chat");
    }

    #[tokio::test]
    async fn client_omits_headers_when_none_configured() {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
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
    async fn custom_provider_receives_declared_headers_only() {
        let server = MockServer::start().await;
        Mock::given(path("/v1/chat/completions"))
            .and(header("X-Team", "companion"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&server)
            .await;
        let mut ep_headers = reqwest::header::HeaderMap::new();
        ep_headers.insert("X-Team", "companion".parse().unwrap());
        let mut providers = HashMap::new();
        providers.insert(
            "venice".to_string(),
            crate::provider::ProviderEndpoint {
                base_url: format!("{}/v1/chat/completions", server.uri()),
                api_key: "vk".into(),
                headers: ep_headers,
                body_rules: Vec::new(),
            },
        );
        let client = OpenRouterClient::with_base_url("test-key".into(), "http://unused/".into())
            .with_providers(providers);
        let _ = client
            .execute(ChatRequest {
                model: "some-model@venice".into(),
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
            repetition_penalty: None,
            max_tokens: req.max_tokens,
            stream: false,
            user: req.user.as_deref(),
            session_id: req.session_id.as_deref(),
            metadata: req.metadata.as_ref(),
            reasoning: None,
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
            repetition_penalty: None,
            max_tokens: req.max_tokens,
            stream: false,
            user: req.user.as_deref(),
            session_id: req.session_id.as_deref(),
            metadata: req.metadata.as_ref(),
            reasoning: None,
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
        match err {
            LlmError::Chain { failures } => {
                let last = failures.last().expect("at least one failure");
                match last {
                    crate::failure::AttemptFailure::Upstream(a) => {
                        assert_eq!(a.http_status, 500, "expected last 500, got {a:?}")
                    }
                    other => panic!("expected Upstream, got {other:?}"),
                }
            }
            other => panic!("expected Chain, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_reports_the_failed_hop_even_when_a_fallback_recovers() {
        // The whole point: a turn that recovered used to leave no trace at all.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::body_string_contains("primary/m"))
            .respond_with(
                wiremock::ResponseTemplate::new(529)
                    .set_body_string(r#"{"error":{"code":529,"message":"Overloaded"}}"#),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::body_string_contains("fallback/m"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string(r#"{"choices":[{"message":{"content":"hi"}}]}"#),
            )
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url("k".into(), server.uri());
        let resp = client
            .execute(ChatRequest {
                model: "primary/m".into(),
                fallback_model: vec!["fallback/m".into()],
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "x".into(),
                }],
                task: Some("chat_companion".into()),
                ..Default::default()
            })
            .await
            .expect("fallback should recover");

        assert_eq!(resp.reply, "hi");
        assert_eq!(resp.failures.len(), 1, "the 529 hop must be reported");
        match &resp.failures[0] {
            crate::failure::AttemptFailure::Upstream(a) => {
                assert_eq!(a.http_status, 529);
                assert_eq!(a.model, "primary/m");
                assert_eq!(a.task, "chat_companion");
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_returns_chain_error_carrying_every_hop() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(
                wiremock::ResponseTemplate::new(503)
                    .set_body_string(r#"{"error":{"code":503,"message":"no provider"}}"#),
            )
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url("k".into(), server.uri());
        let err = client
            .execute(ChatRequest {
                model: "a/m".into(),
                fallback_model: vec!["b/m".into(), "c/m".into()],
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "x".into(),
                }],
                task: Some("chat_companion".into()),
                ..Default::default()
            })
            .await
            .expect_err("all candidates fail");

        match err {
            LlmError::Chain { failures } => {
                assert_eq!(failures.len(), 3, "one entry per hop");
                for f in &failures {
                    match f {
                        crate::failure::AttemptFailure::Upstream(a) => {
                            assert_eq!(a.http_status, 503)
                        }
                        other => panic!("expected Upstream, got {other:?}"),
                    }
                }
            }
            other => panic!("expected Chain, got {other:?}"),
        }
    }

    // ─── B-err1: bounded + redacted provider error body ─────────────────────

    #[test]
    fn body_preview_caps_and_flattens() {
        assert_eq!(body_preview("  hi\nthere\r "), "hi\\nthere");
        let long: String = "x".repeat(ERROR_PREVIEW_MAX + 50);
        let out = body_preview(&long);
        assert_eq!(
            out.chars().count(),
            ERROR_PREVIEW_MAX + 1,
            "capped + ellipsis"
        );
        assert!(out.ends_with('…'));
    }

    #[test]
    fn scrub_error_body_drops_moderation_flagged_input() {
        // The user's flagged prompt excerpt must never survive into the log line.
        let raw = serde_json::json!({
            "error": {
                "code": "moderation",
                "message": "flagged",
                "metadata": {
                    "reasons": ["sexual"],
                    "flagged_input": "SECRET USER PROMPT TEXT",
                    "provider_name": "SomeProvider",
                    "model_slug": "some/model",
                }
            }
        })
        .to_string();
        let out = parse_error_body(&raw).to_string();
        assert!(
            !out.contains("SECRET USER PROMPT TEXT"),
            "flagged_input leaked: {out}"
        );
        assert!(out.contains("code=\"moderation\""), "keeps code: {out}");
        assert!(
            out.contains("provider=SomeProvider"),
            "keeps provider: {out}"
        );
        assert!(
            out.contains("moderation_reasons=sexual"),
            "keeps reasons: {out}"
        );
    }

    #[test]
    fn scrub_error_body_bounds_and_flattens_hostile_metadata() {
        // provider_name/reasons are provider-controlled: a newline-laden, very
        // long value must not defeat the single-line, bounded guarantee.
        let evil = format!("{}\n{}", "A".repeat(300), "B".repeat(300));
        let raw = serde_json::json!({
            "error": {
                "code": 500,
                "message": "boom",
                "metadata": { "provider_name": evil, "reasons": ["x\ny"] }
            }
        })
        .to_string();
        let out = parse_error_body(&raw).to_string();
        assert!(!out.contains('\n'), "must be single-line: {out:?}");
        assert!(
            out.chars().count() <= ERROR_PREVIEW_MAX + 1,
            "must be bounded, got {} chars",
            out.chars().count()
        );
    }

    #[test]
    fn scrub_error_body_handles_numeric_code_and_non_envelope() {
        // Numeric code (Value, not i64-restricted) round-trips.
        let raw = serde_json::json!({"error": {"code": 402, "message": "no credits"}}).to_string();
        let out = parse_error_body(&raw).to_string();
        assert!(out.contains("code=402"), "{out}");
        assert!(out.contains("no credits"), "{out}");
        // Non-envelope junk falls back to a bounded preview.
        let junk: String = "boom ".repeat(100);
        let out = parse_error_body(&junk).to_string();
        assert!(
            out.chars().count() <= ERROR_PREVIEW_MAX + 1,
            "bounded: {}",
            out.len()
        );
    }

    #[test]
    fn parse_error_body_extracts_openrouter_metadata_fields() {
        // error_type and metadata.provider_code were never extracted before —
        // scrub_error_body read only code / message / provider_name / reasons.
        let raw = serde_json::json!({
            "error": {
                "code": 529,
                "message": "Overloaded",
                "metadata": {
                    "error_type": "overloaded",
                    "provider_code": "anthropic:overloaded_error"
                }
            }
        })
        .to_string();
        let p = parse_error_body(&raw);
        assert_eq!(p.code.as_deref(), Some("529"));
        assert_eq!(p.error_type.as_deref(), Some("overloaded"));
        assert_eq!(
            p.provider_code.as_deref(),
            Some("anthropic:overloaded_error")
        );
    }

    #[test]
    fn parse_error_body_reads_venice_openai_compatible_shape() {
        // Venice's OpenAI-compatible envelope puts the semantic name in `code`
        // (a string) and the family in `type`. No `metadata` at all.
        let raw = serde_json::json!({
            "error": {
                "message": "The model is currently overloaded",
                "type": "rate_limit_error",
                "param": null,
                "code": "MODEL_OVERLOADED"
            }
        })
        .to_string();
        let p = parse_error_body(&raw);
        assert_eq!(p.code.as_deref(), Some("\"MODEL_OVERLOADED\""));
        assert_eq!(p.error_type.as_deref(), Some("rate_limit_error"));
        assert!(p.message.contains("overloaded"), "{}", p.message);
    }

    #[test]
    fn parse_error_body_reads_venice_bare_string_shape() {
        let raw = serde_json::json!({ "error": "Authentication failed" }).to_string();
        let p = parse_error_body(&raw);
        assert_eq!(p.code, None);
        assert!(p.message.contains("Authentication failed"), "{}", p.message);
    }

    #[test]
    fn parse_error_body_display_matches_legacy_scrub_output() {
        // The Display impl is what every existing log line and assertion sees.
        let raw = serde_json::json!({
            "error": {
                "code": "moderation",
                "message": "flagged",
                "metadata": {
                    "reasons": ["sexual"],
                    "flagged_input": "SECRET USER PROMPT TEXT",
                    "provider_name": "SomeProvider"
                }
            }
        })
        .to_string();
        let out = parse_error_body(&raw).to_string();
        assert!(!out.contains("SECRET USER PROMPT TEXT"), "leaked: {out}");
        assert!(out.contains("code=\"moderation\""), "{out}");
        assert!(out.contains("provider=SomeProvider"), "{out}");
        assert!(out.contains("moderation_reasons=sexual"), "{out}");
    }

    #[test]
    fn parse_error_body_message_only_keeps_the_text_and_no_code() {
        let p = ParsedErrorBody::message_only("stream terminated with finish_reason=error");
        assert_eq!(p.code, None);
        assert_eq!(p.error_type, None);
        assert_eq!(p.provider_code, None);
        assert_eq!(p.message, "stream terminated with finish_reason=error");
    }

    #[test]
    fn mid_stream_error_message_keeps_a_marker_and_the_code() {
        // The marker is the only thing separating "the provider died mid-stream,
        // partial content may already be out" from "the provider returned an error
        // envelope with a 200 and nothing was sent" — both are Provider errors that
        // classify as upstream at http_status 200.
        let body = mid_stream_error_body(Some(&serde_json::json!(529)), "Overloaded");
        assert_eq!(body.to_string(), "mid-stream error: code=529: Overloaded");
    }

    #[test]
    fn mid_stream_error_message_with_no_code() {
        let body = mid_stream_error_body(None, "provider blew up");
        assert_eq!(
            body.to_string(),
            "mid-stream error: code=?: provider blew up"
        );
    }

    #[test]
    fn parse_error_body_display_is_byte_identical_to_the_legacy_format() {
        // scrub_error_body's output shape is an operator-facing log format. The
        // three inherited tests use .contains(), which cannot see a dropped
        // bracket — this one pins the whole string.
        let raw = serde_json::json!({
            "error": {
                "code": 429,
                "message": "slow down",
                "metadata": { "provider_name": "SomeProvider", "reasons": ["sexual", "violence"] }
            }
        })
        .to_string();
        assert_eq!(
            parse_error_body(&raw).to_string(),
            "code=429: slow down [provider=SomeProvider] [moderation_reasons=sexual,violence]"
        );
    }

    #[test]
    fn parse_error_body_drops_non_string_moderation_reasons() {
        let raw = serde_json::json!({
            "error": { "code": 403, "message": "no", "metadata": { "reasons": [{"x": 1}] } }
        })
        .to_string();
        let out = parse_error_body(&raw).to_string();
        assert_eq!(
            out, "code=403: no",
            "a non-string reason contributes nothing"
        );
    }

    #[test]
    fn decode_or_api_error_surfaces_embedded_error() {
        // A 200 body that is really an error envelope → Provider (chain advances
        // with a useful, redacted reason), not a bare Decode.
        let body =
            serde_json::json!({"error": {"code": 400, "message": "bad request"}}).to_string();
        let err = serde_json::from_str::<WireResponse>(&body).expect_err("no choices");
        match decode_or_api_error(&body, err) {
            LlmError::Provider(msg) => assert!(msg.to_string().contains("bad request"), "{msg}"),
            other => panic!("expected Provider, got {other:?}"),
        }
        // Genuine junk stays a Decode error (no body leak — Display is a serde offset).
        let junk = "not json at all";
        let err = serde_json::from_str::<WireResponse>(junk).expect_err("bad json");
        assert!(matches!(
            decode_or_api_error(junk, err),
            LlmError::Decode(_)
        ));
    }

    #[tokio::test]
    async fn call_once_status_body_is_scrubbed_in_error() {
        // A moderation 403 with flagged_input must reach the caller's error
        // (hence logs) with the prompt excerpt stripped.
        let server = MockServer::start().await;
        let moderation = serde_json::json!({
            "error": {
                "code": "moderation",
                "message": "input flagged",
                "metadata": { "reasons": ["harassment"], "flagged_input": "RAW USER CHAT" }
            }
        });
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(403).set_body_json(moderation))
            .mount(&server)
            .await;
        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let err = client
            .execute(ChatRequest {
                model: "m".into(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect_err("403 fails the chain");
        let shown = match &err {
            LlmError::Chain { failures } => match failures.last() {
                Some(crate::failure::AttemptFailure::Upstream(a)) => a.message.clone(),
                other => panic!("expected Upstream, got {other:?}"),
            },
            other => panic!("expected Chain, got {other:?}"),
        };
        assert!(
            !shown.contains("RAW USER CHAT"),
            "flagged_input leaked into error: {shown}"
        );
        assert!(shown.contains("moderation_reasons=harassment"), "{shown}");
    }

    // ─── B-err2: non-stream finish_reason=="error" fails the attempt ─────────

    #[tokio::test]
    async fn call_once_finish_reason_error_advances_chain() {
        let server = MockServer::start().await;
        // Primary returns 200 with finish_reason:"error" (mid-generation death);
        // fallback returns a clean reply.
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "p"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "partial" }, "finish_reason": "error" }]
            })))
            .mount(&server)
            .await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "f"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .mount(&server)
            .await;
        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let resp = client
            .execute(ChatRequest {
                model: "p".into(),
                fallback_model: vec!["f".into()],
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect("fallback serves the clean reply");
        assert_eq!(
            resp.reply, "ok",
            "the finish_reason=error partial must not be returned"
        );
    }

    // ─── B-err3: 200 body that is an error envelope ─────────────────────────

    #[tokio::test]
    async fn call_once_200_error_envelope_becomes_provider_error() {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "error": { "code": 500, "message": "provider exploded" }
            })))
            .mount(&server)
            .await;
        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let err = client
            .execute(ChatRequest {
                model: "m".into(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect_err("a 200 error envelope must fail, not decode-silently");
        match err {
            LlmError::Chain { failures } => match failures.last() {
                Some(crate::failure::AttemptFailure::Upstream(a)) => {
                    assert_eq!(a.http_status, 200, "mid-stream error rides a 200: {a:?}");
                    assert!(
                        a.message.contains("provider exploded"),
                        "expected the embedded message, got {a:?}"
                    );
                }
                other => panic!("expected Upstream, got {other:?}"),
            },
            other => panic!("expected Chain, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_returns_config_err_when_chain_empty() {
        // No mocks — empty primary + empty fallback chain must short-circuit
        // BEFORE any HTTP request reaches the server.
        let server = MockServer::start().await;
        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
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
            matches!(err, LlmError::Status(s, _, _) if s.as_u16() == 429),
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
            repetition_penalty: None,
            max_tokens: 16,
            stream: false,
            user: None,
            session_id: None,
            metadata: None,
            reasoning: Some(&cfg),
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
            repetition_penalty: None,
            max_tokens: 16,
            stream: false,
            user: None,
            session_id: None,
            metadata: None,
            reasoning: None,
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

    #[tokio::test(start_paused = true)]
    async fn idle_bounded_times_out_on_stalled_stream() {
        use futures_util::StreamExt;
        // One item arrives, then the stream stalls forever: the watchdog must
        // pass the item through, then yield a TimedOut error (paused tokio
        // time auto-advances, so this runs in microseconds).
        let inner = futures_util::stream::iter([Ok::<_, std::convert::Infallible>("chunk")])
            .chain(futures_util::stream::pending());
        let mut s = std::pin::pin!(idle_bounded(inner, std::time::Duration::from_millis(50)));
        let first = s.next().await.expect("first item");
        assert_eq!(first.expect("passthrough"), "chunk");
        let second = s.next().await.expect("watchdog fires");
        let err = second.expect_err("stalled gap must error");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(err.to_string().contains("idle timeout"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn idle_bounded_passes_healthy_stream_untouched() {
        use futures_util::StreamExt;
        let inner =
            futures_util::stream::iter(["a", "b", "c"].map(Ok::<_, std::convert::Infallible>));
        let s = std::pin::pin!(idle_bounded(inner, std::time::Duration::from_millis(50)));
        let items: Vec<&str> = s
            .map(|r| r.expect("no timeout on a live stream"))
            .collect()
            .await;
        assert_eq!(items, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn execute_stream_surfaces_mid_stream_error_frame() {
        use futures_util::StreamExt;
        // OpenRouter signals a mid-stream provider failure on an HTTP-200 SSE
        // stream as a data frame with a top-level `error` object (plus a
        // finish_reason:"error" choice). It must surface as Err, not parse as
        // an all-None chunk that lets a partial reply persist as success.
        let server = MockServer::start().await;
        let body = "\
data: {\"id\":\"gen-1\",\"choices\":[{\"delta\":{\"content\":\"部分\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":\"error\"}],\"error\":{\"code\":502,\"message\":\"provider disconnected\"}}\n\n\
data: [DONE]\n\n";
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

        let first = stream.next().await.expect("delta arrives");
        let chunk = first.expect("first frame is a normal delta");
        assert_eq!(chunk.content.as_deref(), Some("部分"));

        let second = stream.next().await.expect("error frame arrives");
        match second {
            Err(LlmError::Provider(msg)) => {
                assert!(
                    msg.to_string().contains("provider disconnected"),
                    "error message carries the upstream detail: {msg}"
                );
            }
            other => panic!("expected Provider error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_stream_surfaces_finish_reason_error_without_error_object() {
        use futures_util::StreamExt;
        // Some providers set finish_reason:"error" without the top-level error
        // object; that terminal frame must also fail the attempt.
        let server = MockServer::start().await;
        let body = "\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"error\"}]}\n\n\
data: [DONE]\n\n";
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
        let item = stream.next().await.expect("terminal frame arrives");
        assert!(
            matches!(item, Err(LlmError::Provider(_))),
            "finish_reason=error must surface as Provider error, got {item:?}"
        );
    }

    #[tokio::test]
    async fn execute_stream_as_sends_the_passed_model_not_req_model() {
        use futures_util::StreamExt;
        // The borrowed request's own `model` is ignored — the `model` argument
        // is what reaches the wire (the fallback-chain contract).
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "actual/served", "stream": true}),
            ))
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
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let req = ChatRequest {
            model: "ignored/primary".into(),
            fallback_model: vec!["also/ignored".into()],
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            }],
            temperature: 0.0,
            max_tokens: 16,
            ..Default::default()
        };
        let mut stream = client
            .execute_stream_as(&req, "actual/served")
            .await
            .expect("stream opens");
        // Drain (single [DONE]).
        while stream.next().await.is_some() {}
    }

    #[tokio::test]
    async fn stream_custom_provider_gets_bare_model_and_subset() {
        let server_b = MockServer::start().await;
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        Mock::given(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse, "text/event-stream"),
            )
            .expect(1)
            .mount(&server_b)
            .await;
        let mut openrouter_headers = reqwest::header::HeaderMap::new();
        openrouter_headers.insert("HTTP-Referer", "https://eros.example".parse().unwrap());
        let client =
            OpenRouterClient::with_base_url("or-key".into(), "https://unused.test/v1".into())
                .with_openrouter_headers(openrouter_headers)
                .with_providers(std::collections::HashMap::from([(
                    "venice".to_string(),
                    crate::provider::ProviderEndpoint {
                        base_url: format!("{}/v1/chat/completions", server_b.uri()),
                        api_key: "v-key".into(),
                        headers: reqwest::header::HeaderMap::new(),
                        body_rules: Vec::new(),
                    },
                )]));
        let req = ChatRequest {
            model: String::new(), // placeholder; execute_stream_as takes the model separately
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            }],
            temperature: 0.0,
            max_tokens: 16,
            session_id: Some("sess".into()),
            ..Default::default()
        };
        let mut s = client
            .execute_stream_as(&req, "venice-model@venice")
            .await
            .expect("stream opens");
        use futures_util::StreamExt as _;
        while s.next().await.is_some() {}

        let r = &server_b.received_requests().await.unwrap()[0];
        assert_eq!(
            r.headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer v-key"
        );
        assert!(r.headers.get("HTTP-Referer").is_none());
        let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(body["model"], "venice-model");
        assert_eq!(body["stream"], true);
        for k in ["session_id", "metadata", "reasoning"] {
            assert!(
                body.get(k).is_none(),
                "body field {k} leaked into the stream wire"
            );
        }
    }

    // ─── B1: X-Generation-Id header capture ─────────────────────────────────

    #[tokio::test]
    async fn execute_stream_prepends_generation_id_from_header() {
        use futures_util::StreamExt;
        // Header carries the id; the body frames carry none. The synthetic first
        // chunk must surface the header id so audit has a handle even if the
        // stream dies before any body id.
        let server = MockServer::start().await;
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .insert_header("x-generation-id", "gen-hdr-1")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&server)
            .await;
        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
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
        let first = stream
            .next()
            .await
            .expect("synthetic first chunk")
            .expect("ok");
        assert_eq!(first.generation_id.as_deref(), Some("gen-hdr-1"));
        assert!(
            first.content.is_none(),
            "synthetic chunk carries no content"
        );
    }

    #[tokio::test]
    async fn execute_stream_no_header_no_synthetic_chunk() {
        use futures_util::StreamExt;
        // Without the header, the first chunk is the real body delta (no
        // spurious empty synthetic chunk).
        let server = MockServer::start().await;
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&server)
            .await;
        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
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
        let first = stream.next().await.expect("first chunk").expect("ok");
        assert_eq!(
            first.content.as_deref(),
            Some("hi"),
            "first chunk is the real delta"
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
            repetition_penalty: None,
            max_tokens: 16,
            stream: false,
            user: None,
            session_id: None,
            metadata: None,
            reasoning: None,
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
            repetition_penalty: None,
            max_tokens: 16,
            stream: false,
            user: None,
            session_id: None,
            metadata: None,
            reasoning: None,
            response_format: None,
        };
        let s_none = serde_json::to_string(&wire_none).unwrap();
        assert!(
            !s_none.contains("response_format"),
            "absent ⇒ omitted: {s_none}"
        );
    }

    // ---- multi-provider routing: resolve_endpoint (spec §3) ----

    fn client_with_venice() -> OpenRouterClient {
        OpenRouterClient::with_base_url("or-key".into(), "https://openrouter.test/v1".into())
            .with_providers(std::collections::HashMap::from([(
                "venice".to_string(),
                crate::provider::ProviderEndpoint {
                    base_url: "https://venice.test/v1/chat/completions".into(),
                    api_key: "v-key".into(),
                    headers: reqwest::header::HeaderMap::new(),
                    body_rules: Vec::new(),
                },
            )]))
    }

    #[test]
    fn resolve_no_suffix_is_openrouter() {
        let c = client_with_venice();
        let (bare, ep) = c.resolve_endpoint("x-ai/grok-4.20").unwrap();
        assert_eq!(bare, "x-ai/grok-4.20");
        assert_eq!(ep.url, "https://openrouter.test/v1");
        assert_eq!(ep.api_key, "or-key");
        assert!(ep.name.is_none());
    }

    #[test]
    fn resolve_suffix_hits_custom_endpoint() {
        let c = client_with_venice();
        let (bare, ep) = c.resolve_endpoint("some-slug@venice").unwrap();
        assert_eq!(bare, "some-slug");
        assert_eq!(ep.url, "https://venice.test/v1/chat/completions");
        assert_eq!(ep.api_key, "v-key");
        assert_eq!(ep.name, Some("venice"));
    }

    #[test]
    fn resolve_escaped_at_stays_openrouter() {
        let c = client_with_venice();
        let (bare, ep) = c.resolve_endpoint("weird\\@vendor/m").unwrap();
        assert_eq!(bare, "weird@vendor/m");
        assert!(ep.name.is_none());
    }

    #[test]
    fn resolve_at_openrouter_suffix_matches_no_suffix() {
        // Critical-finding regression (spec §3/§4): `@openrouter` must be
        // byte-for-byte equivalent to no suffix at all — built-in endpoint,
        // no `[providers]` lookup. `client_with_venice` declares ONLY
        // `venice`, no `openrouter` entry, so a resolution that (bugly)
        // fell through to the `Some(p)` custom-provider arm would fail here
        // with "names undeclared provider `openrouter`" — this test proves
        // it does not.
        let c = client_with_venice();
        let (bare_plain, ep_plain) = c.resolve_endpoint("x-ai/grok-4.20").unwrap();
        let (bare_alias, ep_alias) = c.resolve_endpoint("x-ai/grok-4.20@openrouter").unwrap();
        assert_eq!(bare_alias, bare_plain);
        assert_eq!(ep_alias.url, ep_plain.url);
        assert_eq!(ep_alias.api_key, ep_plain.api_key);
        assert!(ep_alias.name.is_none());
        assert!(ep_alias.headers.is_none());
    }

    #[tokio::test]
    async fn execute_at_openrouter_suffix_hits_built_in_endpoint_full_wire() {
        // Critical-finding regression test: `@openrouter` on a chat slug must
        // resolve through the SAME path as no suffix (spec §3/§4) — built-in
        // endpoint, bare model on the wire — never the custom-provider
        // subset. No `[providers]` map is installed at all, proving the
        // alias needs no `[providers].openrouter` entry to work.
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&server)
            .await;
        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let _ = client
            .execute(ChatRequest {
                model: "test/model@openrouter".into(),
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

        let reqs = server.received_requests().await.unwrap_or_default();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(
            body["model"], "test/model",
            "wire model must be the bare id — the @openrouter suffix must not reach the wire"
        );
    }

    #[tokio::test]
    async fn builtin_wire_has_no_provider_key() {
        // ProviderPrefs was removed (spec 2026-08-02-provider-body-params §3):
        // the engine never builds a `provider` object; deployers who need
        // OpenRouter routing prefs declare them as [[providers.openrouter.body]]
        // params. Locks the built-in wire against a silent re-introduction.
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&server)
            .await;
        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        client
            .execute(ChatRequest {
                model: "m".into(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                max_tokens: 5,
                ..Default::default()
            })
            .await
            .unwrap();
        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(body.get("provider").is_none(), "provider key must be gone");
    }

    #[test]
    fn resolve_unknown_provider_is_config_error() {
        let c = client_with_venice();
        assert!(matches!(
            c.resolve_endpoint("m@nope"),
            Err(LlmError::Config(_))
        ));
    }

    #[test]
    fn resolve_empty_openrouter_key_still_guards() {
        // The old head-of-method guard moved here; same error, same wording.
        let c = OpenRouterClient::with_base_url(String::new(), "https://openrouter.test/v1".into());
        assert!(matches!(c.resolve_endpoint("m"), Err(LlmError::Config(_))));
    }

    #[test]
    fn resolve_empty_custom_key_guards() {
        let c =
            OpenRouterClient::with_base_url("or-key".into(), "https://openrouter.test/v1".into())
                .with_providers(std::collections::HashMap::from([(
                    "venice".to_string(),
                    crate::provider::ProviderEndpoint {
                        base_url: "https://venice.test/v1".into(),
                        api_key: String::new(),
                        headers: reqwest::header::HeaderMap::new(),
                        body_rules: Vec::new(),
                    },
                )]));
        assert!(matches!(
            c.resolve_endpoint("m@venice"),
            Err(LlmError::Config(_))
        ));
    }

    #[test]
    fn with_openrouter_chat_url_overrides_and_none_keeps() {
        let c = client_with_venice().with_openrouter_chat_url(Some("https://proxy.test/v1".into()));
        assert_eq!(
            c.resolve_endpoint("m").unwrap().1.url,
            "https://proxy.test/v1"
        );
        let c2 = client_with_venice().with_openrouter_chat_url(None);
        assert_eq!(
            c2.resolve_endpoint("m").unwrap().1.url,
            "https://openrouter.test/v1"
        );
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
        // Spec §2: the garbled hop's call SUCCEEDED and was billed, so it is a
        // content verdict owned by the caller's coarse marker. It must not
        // appear in either column — least of all as a `decode` gateway error,
        // which would say our path to the provider broke when it did not.
        assert!(
            resp.failures.is_empty(),
            "a garble belongs to neither column: {:?}",
            resp.failures
        );
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
        match err {
            // The chain still dies — the caller fails open on the `Err`. What
            // the garble does NOT do is land in a column: the call succeeded
            // and was billed, so it is a content verdict owned by the caller's
            // coarse marker (spec §2, `AttemptFailure::should_record`).
            LlmError::Chain { failures } => assert!(
                failures.is_empty(),
                "a garble belongs to neither column: {failures:?}"
            ),
            other => panic!("expected Chain, got {other:?}"),
        }
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
                sampling: crate::model_config::Sampling::default(),
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

    #[tokio::test]
    async fn execute_vision_reports_the_failed_hop_even_when_a_fallback_recovers() {
        // Vision variant of execute_reports_the_failed_hop_even_when_a_fallback_recovers.
        // execute_vision has its own (non-call_once) control flow, so this must be
        // exercised separately, not just inferred from the chat test. 502 (not
        // chat's 529) so a copy-paste that accidentally hit the chat path fails
        // loudly instead of passing silently.
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "vision-primary/m"}),
            ))
            .respond_with(
                ResponseTemplate::new(502)
                    .set_body_string(r#"{"error":{"code":502,"message":"Bad Gateway"}}"#),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "vision-fallback/m"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "{\"description\":\"a cat\"}" } }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "k".into(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let resp = client
            .execute_vision(VisionRequest {
                model: "vision-primary/m".into(),
                fallback_model: vec!["vision-fallback/m".into()],
                system_prompt: "describe".into(),
                image_url: "https://example/x.png".into(),
                caption: None,
                temperature: 0.0,
                max_tokens: 64,
                reasoning: None,
                sampling: crate::model_config::Sampling::default(),
            })
            .await
            .expect("fallback should recover");

        assert_eq!(resp.reply, "{\"description\":\"a cat\"}");
        assert_eq!(resp.failures.len(), 1, "the 502 hop must be reported");
        match &resp.failures[0] {
            crate::failure::AttemptFailure::Upstream(a) => {
                assert_eq!(a.http_status, 502);
                assert_eq!(a.model, "vision-primary/m");
                assert_eq!(a.task, VISION_TASK);
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_vision_returns_chain_error_carrying_every_hop() {
        // Vision variant of execute_returns_chain_error_carrying_every_hop.
        // Vision-shaped model slugs (va/vb/vc, not chat's a/b/c) so a copy-paste
        // that mixed up the two chains would show up in the failed model name.
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(503)
                    .set_body_string(r#"{"error":{"code":503,"message":"no provider"}}"#),
            )
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "k".into(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let err = client
            .execute_vision(VisionRequest {
                model: "va/m".into(),
                fallback_model: vec!["vb/m".into(), "vc/m".into()],
                system_prompt: "describe".into(),
                image_url: "https://example/x.png".into(),
                caption: None,
                temperature: 0.0,
                max_tokens: 64,
                reasoning: None,
                sampling: crate::model_config::Sampling::default(),
            })
            .await
            .expect_err("all candidates fail");

        match err {
            LlmError::Chain { failures } => {
                assert_eq!(failures.len(), 3, "one entry per hop");
                for f in &failures {
                    match f {
                        crate::failure::AttemptFailure::Upstream(a) => {
                            assert_eq!(a.http_status, 503)
                        }
                        other => panic!("expected Upstream, got {other:?}"),
                    }
                }
            }
            other => panic!("expected Chain, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_vision_salvage_return_carries_the_failures_too() {
        // Mirrors execute_returns_repaired_garble_even_when_later_candidate_fails:
        // primary "vp" returns recoverable garble, fallback "vf1" then fails with a
        // non-garble status error. The salvage must still return vp's repaired text
        // AND report vf1's hop in `failures` — this exercises the
        // `failures: failures.clone()` on execute_vision's salvage return, which
        // was previously verified only by reading the code.
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "vp"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(garbled_content()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"model": "vf1"}),
            ))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenRouterClient::with_base_url(
            "test-key".into(),
            format!("{}/api/v1/chat/completions", server.uri()),
        );
        let resp = client
            .execute_vision(VisionRequest {
                model: "vp".into(),
                fallback_model: vec!["vf1".into()],
                system_prompt: "describe".into(),
                image_url: "https://example/x.png".into(),
                caption: None,
                temperature: 0.0,
                max_tokens: 64,
                reasoning: None,
                sampling: crate::model_config::Sampling::default(),
            })
            .await
            .expect("earlier garble salvaged despite later non-garble failure");
        assert_eq!(resp.reply, "Hi there\nbye");
        assert_eq!(resp.model.as_deref(), Some("vp"));
        // vf1's 500 rides the salvage return; vp's garble does NOT — the call
        // succeeded and was billed, so it is a content verdict and belongs to
        // neither column (spec §2).
        assert_eq!(
            resp.failures.len(),
            1,
            "only the non-garble hop is recordable: {:?}",
            resp.failures
        );
        match &resp.failures[0] {
            crate::failure::AttemptFailure::Upstream(a) => assert_eq!(a.http_status, 500),
            other => panic!("expected the 500 hop, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn vision_custom_provider_bare_model_no_prefs_suffixed_audit() {
        let server_b = MockServer::start().await;
        Mock::given(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "v-gen-2",
                "model": "venice-vision-echo",
                "choices": [{ "message": { "content": "{\"desc\":\"a cat\"}" } }]
            })))
            .expect(1)
            .mount(&server_b)
            .await;
        let client =
            OpenRouterClient::with_base_url("or-key".into(), "https://unused.test/v1".into())
                .with_providers(std::collections::HashMap::from([(
                    "venice".to_string(),
                    crate::provider::ProviderEndpoint {
                        base_url: format!("{}/v1/chat/completions", server_b.uri()),
                        api_key: "v-key".into(),
                        headers: reqwest::header::HeaderMap::new(),
                        body_rules: Vec::new(),
                    },
                )]));
        let resp = client
            .execute_vision(VisionRequest {
                model: "vis@venice".into(),
                fallback_model: vec![],
                system_prompt: "describe".into(),
                image_url: "data:image/png;base64,AAAA".into(),
                caption: None,
                temperature: 0.0,
                max_tokens: 64,
                reasoning: Some(ReasoningConfig {
                    enabled: Some(false),
                    ..Default::default()
                }),
                sampling: crate::model_config::Sampling::default(),
            })
            .await
            .expect("vision call succeeds");
        assert_eq!(resp.model.as_deref(), Some("venice-vision-echo@venice"));

        let r = &server_b.received_requests().await.unwrap()[0];
        assert_eq!(
            r.headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer v-key"
        );
        let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(body["model"], "vis");
        assert!(
            body.get("provider").is_none(),
            "provider prefs leaked to custom vision"
        );
        assert!(
            body.get("reasoning").is_none(),
            "OpenRouter-only reasoning object leaked to custom vision"
        );
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
            repetition_penalty: Some(1.15),
            max_tokens: 16,
            stream: false,
            user: None,
            session_id: None,
            metadata: None,
            reasoning: None,
            response_format: None,
        };
        let s = serde_json::to_string(&wire).unwrap();
        assert!(s.contains("\"top_p\":0.9"), "{s}");
        assert!(s.contains("\"frequency_penalty\":0.4"), "{s}");
        assert!(s.contains("\"presence_penalty\":0.2"), "{s}");
        assert!(s.contains("\"repetition_penalty\":1.15"), "{s}");
    }

    #[test]
    fn unset_sampling_serializes_byte_identically_to_the_pre_246_body() {
        // Stronger than the key-absence checks below: #246 promises a
        // deployment that sets no sampling knob keeps producing the EXACT
        // body it produced before. Key absence alone would still pass if
        // field order or numeric formatting drifted, so compare bytes.
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
            repetition_penalty: None,
            max_tokens: 16,
            stream: false,
            user: None,
            session_id: None,
            metadata: None,
            reasoning: None,
            response_format: None,
        };
        assert_eq!(
            serde_json::to_string(&wire).unwrap(),
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"temperature":0.8,"max_tokens":16}"#
        );
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
            repetition_penalty: None,
            max_tokens: 16,
            stream: false,
            user: None,
            session_id: None,
            metadata: None,
            reasoning: None,
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
        assert!(
            !s.contains("repetition_penalty"),
            "unset repetition_penalty must be omitted: {s}"
        );
    }

    #[test]
    fn vision_body_carries_sampling_when_set_and_omits_when_unset() {
        let base = VisionRequest {
            model: "m".into(),
            fallback_model: vec![],
            system_prompt: "sys".into(),
            image_url: "https://x/y.png".into(),
            caption: None,
            temperature: 0.5,
            max_tokens: 10,
            reasoning: None,
            sampling: crate::model_config::Sampling::default(),
        };

        let bare = build_vision_body(&base, "m");
        let o = bare.as_object().unwrap();
        for k in [
            "top_p",
            "frequency_penalty",
            "presence_penalty",
            "repetition_penalty",
        ] {
            assert!(!o.contains_key(k), "unset {k} must be omitted: {bare}");
        }

        let tuned = VisionRequest {
            sampling: crate::model_config::Sampling {
                top_p: Some(0.9),
                frequency_penalty: Some(0.4),
                presence_penalty: Some(0.2),
                repetition_penalty: Some(1.15),
            },
            ..base
        };
        let body = build_vision_body(&tuned, "m");
        // Compared through the f32→f64 widening `serde_json::json!` applies.
        // The vision body is a hand-built `Value`, so every float in it takes
        // that widening — `temperature` already did before this change. The
        // typed chat path formats f32 with ryu instead and emits `0.9`; the
        // difference is representational only, both parse to the same float.
        let f = |v: f32| serde_json::json!(v);
        assert_eq!(body["top_p"], f(0.9));
        assert_eq!(body["frequency_penalty"], f(0.4));
        assert_eq!(body["presence_penalty"], f(0.2));
        assert_eq!(body["repetition_penalty"], f(1.15));
    }

    #[test]
    fn chat_request_default_still_works_with_sampling() {
        // The new grouped field must not break `..Default::default()`, which
        // every ChatRequest literal in the server crate relies on.
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            }],
            temperature: 0.5,
            max_tokens: 10,
            sampling: crate::model_config::Sampling {
                top_p: Some(0.9),
                repetition_penalty: Some(1.15),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(req.sampling.repetition_penalty, Some(1.15));
        assert_eq!(req.sampling.frequency_penalty, None);
        assert_eq!(
            ChatRequest::default().sampling,
            crate::model_config::Sampling::default()
        );
    }

    // ---- multi-provider routing: non-stream wire (spec §4/§6) ----

    /// A WireRequest with EVERY optional field set — the worst case for leaks.
    /// Returns owned parts; tests borrow from them.
    fn full_wire_parts() -> (
        Vec<ChatMessage>,
        serde_json::Map<String, serde_json::Value>,
        ReasoningConfig,
        serde_json::Value,
    ) {
        let messages = vec![ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        }];
        let mut meta = serde_json::Map::new();
        meta.insert("k".into(), serde_json::json!("v"));
        let reasoning = ReasoningConfig {
            enabled: Some(false),
            ..Default::default()
        };
        let response_format = serde_json::json!({ "type": "json_object" });
        (messages, meta, reasoning, response_format)
    }

    #[test]
    fn custom_endpoint_wire_is_strict_openai_subset() {
        // THE allow-list lock (spec §4): a custom-endpoint body's keys must be
        // a SUBSET of the OpenAI chat-completions surface. Subset, not
        // equality — several fields are legitimately skipped when None/false.
        // If this test fails on a field you just added to WireRequest, add it
        // to WireRequest::for_endpoint's drop list (or the allow list, if it
        // is standard OpenAI).
        let (messages, meta, reasoning, response_format) = full_wire_parts();
        let http = reqwest::Client::new();
        let ep = Endpoint {
            url: "https://x",
            api_key: "k",
            http: &http,
            name: Some("venice"),
            headers: None,
        };
        let wire = WireRequest {
            model: "m",
            messages: &messages,
            temperature: 0.5,
            top_p: Some(0.9),
            frequency_penalty: Some(0.1),
            presence_penalty: Some(0.1),
            repetition_penalty: Some(1.15),
            max_tokens: 10,
            stream: true,
            user: Some("u"),
            session_id: Some("s"),
            metadata: Some(&meta),
            reasoning: Some(&reasoning),
            response_format: Some(&response_format),
        }
        .for_endpoint(&ep);
        let v = serde_json::to_value(&wire).unwrap();
        const ALLOW: [&str; 11] = [
            "model",
            "messages",
            "temperature",
            "top_p",
            "frequency_penalty",
            "presence_penalty",
            // Standard OpenAI-format field, not an OpenRouter extension — it
            // belongs in the allow list, not for_endpoint's drop list (#246).
            "repetition_penalty",
            "max_tokens",
            "stream",
            "user",
            "response_format",
        ];
        for k in v.as_object().unwrap().keys() {
            assert!(
                ALLOW.contains(&k.as_str()),
                "OpenRouter-specific field `{k}` leaked to a custom provider"
            );
        }
    }

    #[test]
    fn custom_endpoint_vision_body_is_strict_openai_subset() {
        // Finding 1 (final review): the WireRequest lock above only covers
        // the typed chat/stream path (`call_once` / `execute_stream_as`).
        // `execute_vision` instead builds a raw `serde_json::Value` via
        // `build_vision_body` and strips OpenRouter-only fields with
        // `strip_openrouter_vision_fields` — the exact helper `execute_vision`
        // calls for a custom endpoint — so it needs its own lock, driving both
        // together the same way production does. This can't drift from
        // `execute_vision`'s custom-endpoint path because both call the same
        // helper.
        let req = VisionRequest {
            model: "ignored".into(),
            fallback_model: vec!["ignored-2".into()],
            system_prompt: "sys".into(),
            image_url: "https://x/y.png".into(),
            caption: Some("a caption".into()),
            temperature: 0.5,
            max_tokens: 10,
            reasoning: Some(ReasoningConfig {
                enabled: Some(false),
                ..Default::default()
            }),
            // Set, so the ALLOW lock below actually exercises them — all four
            // are standard OpenAI fields and must survive the strip (#246).
            sampling: crate::model_config::Sampling {
                top_p: Some(0.9),
                frequency_penalty: Some(0.1),
                presence_penalty: Some(0.1),
                repetition_penalty: Some(1.15),
            },
        };
        let mut body = build_vision_body(&req, "m");
        strip_openrouter_vision_fields(&mut body);
        const ALLOW: [&str; 11] = [
            "model",
            "messages",
            "temperature",
            "top_p",
            "frequency_penalty",
            "presence_penalty",
            // Standard OpenAI-format field, not an OpenRouter extension — it
            // belongs in the allow list, not for_endpoint's drop list (#246).
            "repetition_penalty",
            "max_tokens",
            "stream",
            "user",
            "response_format",
        ];
        for k in body.as_object().unwrap().keys() {
            assert!(
                ALLOW.contains(&k.as_str()),
                "OpenRouter-specific field `{k}` leaked to a custom provider via the vision body"
            );
        }
    }

    #[test]
    fn openrouter_endpoint_wire_keeps_all_fields() {
        // Regression lock: for_endpoint on the built-in endpoint drops NOTHING.
        let (messages, meta, reasoning, response_format) = full_wire_parts();
        let http = reqwest::Client::new();
        let ep = Endpoint {
            url: "https://x",
            api_key: "k",
            http: &http,
            name: None,
            headers: None,
        };
        let wire = WireRequest {
            model: "m",
            messages: &messages,
            temperature: 0.5,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            repetition_penalty: None,
            max_tokens: 10,
            stream: false,
            user: None,
            session_id: Some("s"),
            metadata: Some(&meta),
            reasoning: Some(&reasoning),
            response_format: Some(&response_format),
        }
        .for_endpoint(&ep);
        let v = serde_json::to_value(&wire).unwrap();
        let obj = v.as_object().unwrap();
        for k in ["session_id", "metadata", "reasoning"] {
            assert!(
                obj.contains_key(k),
                "`{k}` must survive on the OpenRouter path"
            );
        }
    }

    #[tokio::test]
    async fn custom_provider_gets_bare_model_own_key_no_attribution() {
        let server_b = MockServer::start().await;
        Mock::given(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "v-gen-1",
                "model": "venice-echo",
                "choices": [{ "message": { "content": "ok" } }]
            })))
            .expect(1)
            .mount(&server_b)
            .await;
        // Headers configured — none of it may reach the custom host.
        let mut openrouter_headers = reqwest::header::HeaderMap::new();
        openrouter_headers.insert("HTTP-Referer", "https://eros.example".parse().unwrap());
        openrouter_headers.insert("X-OpenRouter-Title", "Eros".parse().unwrap());
        openrouter_headers.insert(
            "X-OpenRouter-Categories",
            "roleplay,general-chat".parse().unwrap(),
        );
        let client = OpenRouterClient::with_base_url(
            "or-key".into(),
            "https://unused-openrouter.test/v1".into(),
        )
        .with_openrouter_headers(openrouter_headers)
        .with_providers(std::collections::HashMap::from([(
            "venice".to_string(),
            crate::provider::ProviderEndpoint {
                base_url: format!("{}/v1/chat/completions", server_b.uri()),
                api_key: "v-key".into(),
                headers: reqwest::header::HeaderMap::new(),
                body_rules: Vec::new(),
            },
        )]));
        let mut meta = serde_json::Map::new();
        meta.insert("k".into(), serde_json::json!("v"));
        let resp = client
            .execute(ChatRequest {
                model: "venice-model@venice".into(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.5,
                max_tokens: 16,
                session_id: Some("sess".into()),
                metadata: Some(meta),
                reasoning: Some(ReasoningConfig {
                    enabled: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .expect("custom call succeeds");

        // §6: audit model = upstream echo + @provider; generation_id verbatim.
        assert_eq!(resp.model.as_deref(), Some("venice-echo@venice"));
        assert_eq!(resp.generation_id.as_deref(), Some("v-gen-1"));

        let req = &server_b.received_requests().await.unwrap()[0];
        // Bearer is the provider's own key.
        assert_eq!(
            req.headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer v-key"
        );
        // No attribution headers on a custom host (spec §4).
        for h in [
            "HTTP-Referer",
            "X-OpenRouter-Title",
            "X-OpenRouter-Categories",
        ] {
            assert!(req.headers.get(h).is_none(), "header {h} leaked");
        }
        // Bare model id + none of the three OpenRouter-only body fields.
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["model"], "venice-model");
        for k in ["session_id", "metadata", "reasoning"] {
            assert!(body.get(k).is_none(), "body field {k} leaked");
        }
    }

    #[tokio::test]
    async fn custom_echo_with_literal_at_is_escaped_in_audit_slug() {
        let server_b = MockServer::start().await;
        Mock::given(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "v-gen-3",
                "model": "weird@vendor/m",
                "choices": [{ "message": { "content": "ok" } }]
            })))
            .expect(1)
            .mount(&server_b)
            .await;
        let client =
            OpenRouterClient::with_base_url("or-key".into(), "https://unused.test/v1".into())
                .with_providers(std::collections::HashMap::from([(
                    "venice".to_string(),
                    crate::provider::ProviderEndpoint {
                        base_url: format!("{}/v1/chat/completions", server_b.uri()),
                        api_key: "v-key".into(),
                        headers: reqwest::header::HeaderMap::new(),
                        body_rules: Vec::new(),
                    },
                )]));
        let resp = client
            .execute(ChatRequest {
                model: "weird\\@vendor/m@venice".into(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect("custom call succeeds");
        // The echoed id contains a literal `@`; it must be escaped before the
        // `@venice` suffix so the persisted slug has exactly one unescaped
        // `@` and is parseable again.
        assert_eq!(resp.model.as_deref(), Some("weird\\@vendor/m@venice"));
        assert_eq!(
            crate::provider::bare_model_id(resp.model.as_deref().unwrap()),
            "weird@vendor/m"
        );
    }

    #[tokio::test]
    async fn custom_echo_missing_falls_back_to_bare_at_name() {
        let server_b = MockServer::start().await;
        Mock::given(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&server_b)
            .await;
        let client =
            OpenRouterClient::with_base_url("or-key".into(), "https://unused.test/v1".into())
                .with_providers(std::collections::HashMap::from([(
                    "venice".to_string(),
                    crate::provider::ProviderEndpoint {
                        base_url: format!("{}/v1/chat/completions", server_b.uri()),
                        api_key: "v-key".into(),
                        headers: reqwest::header::HeaderMap::new(),
                        body_rules: Vec::new(),
                    },
                )]));
        let resp = client
            .execute(ChatRequest {
                model: "vm@venice".into(),
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
        // ok_response() has no "model" field → bare@name.
        assert_eq!(resp.model.as_deref(), Some("vm@venice"));
    }

    #[tokio::test]
    async fn mixed_chain_falls_back_from_custom_to_openrouter() {
        let server_a = MockServer::start().await; // built-in
        let server_b = MockServer::start().await; // venice — down
        Mock::given(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server_b)
            .await;
        Mock::given(path("/or/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&server_a)
            .await;
        let client = OpenRouterClient::with_base_url(
            "or-key".into(),
            format!("{}/or/chat/completions", server_a.uri()),
        )
        .with_providers(std::collections::HashMap::from([(
            "venice".to_string(),
            crate::provider::ProviderEndpoint {
                base_url: format!("{}/v1/chat/completions", server_b.uri()),
                api_key: "v-key".into(),
                headers: reqwest::header::HeaderMap::new(),
                body_rules: Vec::new(),
            },
        )]));
        let resp = client
            .execute(ChatRequest {
                model: "m@venice".into(),
                fallback_model: vec!["or/fallback".into()],
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect("fallback serves");
        assert_eq!(resp.reply, "ok");
        // The OpenRouter attempt used its own key and the fallback's bare id.
        let req_a = &server_a.received_requests().await.unwrap()[0];
        assert_eq!(
            req_a
                .headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer or-key"
        );
        let body: serde_json::Value = serde_json::from_slice(&req_a.body).unwrap();
        assert_eq!(body["model"], "or/fallback");
    }

    #[tokio::test]
    async fn unknown_provider_advances_the_chain() {
        let server_a = MockServer::start().await;
        Mock::given(path("/or/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&server_a)
            .await;
        let client = OpenRouterClient::with_base_url(
            "or-key".into(),
            format!("{}/or/chat/completions", server_a.uri()),
        );
        let resp = client
            .execute(ChatRequest {
                model: "m@nope".into(),
                fallback_model: vec!["good/m".into()],
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                temperature: 0.0,
                max_tokens: 16,
                ..Default::default()
            })
            .await
            .expect("chain advances past the unresolvable candidate");
        assert_eq!(resp.reply, "ok");
    }

    // ---- [[providers.*.body]] merge (spec 2026-08-02-provider-body-params) ----

    #[tokio::test]
    async fn openrouter_body_rule_applies_for_its_task_and_overrides_reasoning() {
        let mock = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&mock)
            .await;
        let client = OpenRouterClient::with_base_url(
            "k".into(),
            format!("{}/api/v1/chat/completions", mock.uri()),
        )
        .with_openrouter_body_rules(vec![rule(
            Some(&["chat_companion"]),
            serde_json::json!({"transforms": ["middle-out"], "reasoning": {"max_tokens": 64}}),
        )]);
        client
            .execute(ChatRequest {
                model: "m".into(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                max_tokens: 5,
                reasoning: Some(ReasoningConfig {
                    enabled: Some(true),
                    ..Default::default()
                }),
                task: Some("chat_companion".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(body["transforms"][0], "middle-out");
        assert_eq!(
            body["reasoning"]["max_tokens"], 64,
            "providers-block reasoning must beat [tasks.*] reasoning"
        );
        assert!(body.get("enabled").is_none());
    }

    #[tokio::test]
    async fn body_rule_skipped_for_unlisted_task() {
        // Identical setup to the previous test, but the request serves a task
        // the rule does not list — nothing merges, and the request's own
        // reasoning config survives untouched.
        let mock = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&mock)
            .await;
        let client = OpenRouterClient::with_base_url(
            "k".into(),
            format!("{}/api/v1/chat/completions", mock.uri()),
        )
        .with_openrouter_body_rules(vec![rule(
            Some(&["chat_companion"]),
            serde_json::json!({"transforms": ["middle-out"]}),
        )]);
        client
            .execute(ChatRequest {
                model: "m".into(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                max_tokens: 5,
                reasoning: Some(ReasoningConfig {
                    enabled: Some(true),
                    ..Default::default()
                }),
                task: Some("pde_decision".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
        assert!(
            body.get("transforms").is_none(),
            "unlisted task must not merge"
        );
        assert_eq!(
            body["reasoning"]["enabled"], true,
            "request's own reasoning survives"
        );
    }

    #[tokio::test]
    async fn custom_provider_strips_then_merges() {
        let mock = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&mock)
            .await;
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "venice".to_string(),
            crate::provider::ProviderEndpoint {
                base_url: format!("{}/api/v1/chat/completions", mock.uri()),
                api_key: "vk".into(),
                headers: reqwest::header::HeaderMap::new(),
                body_rules: vec![rule(
                    None,
                    serde_json::json!({
                        "venice_parameters": {"include_venice_system_prompt": false},
                        "reasoning": {"strip_thinking_response": true},
                    }),
                )],
            },
        );
        let client = OpenRouterClient::with_base_url("k".into(), "http://unused".into())
            .with_providers(providers);
        let mut meta = serde_json::Map::new();
        meta.insert("k".into(), serde_json::json!("v"));
        client
            .execute(ChatRequest {
                model: "m@venice".into(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                max_tokens: 5,
                session_id: Some("s".into()),
                metadata: Some(meta),
                reasoning: Some(ReasoningConfig {
                    enabled: Some(true),
                    ..Default::default()
                }),
                task: Some("chat_companion".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(
            body["venice_parameters"]["include_venice_system_prompt"],
            serde_json::Value::Bool(false)
        );
        assert_eq!(
            body["reasoning"]["strip_thinking_response"], true,
            "the RULE's reasoning shape — the request's was stripped first"
        );
        assert!(
            body.get("session_id").is_none(),
            "strip still runs before merge"
        );
        assert!(body.get("metadata").is_none());
    }

    #[tokio::test]
    async fn body_rules_apply_on_stream_path() {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::path("/api/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_raw("data: [DONE]\n\n", "text/event-stream"),
            )
            .mount(&mock)
            .await;
        let client = OpenRouterClient::with_base_url(
            "k".into(),
            format!("{}/api/v1/chat/completions", mock.uri()),
        )
        .with_openrouter_body_rules(vec![rule(
            None,
            serde_json::json!({"transforms": ["middle-out"]}),
        )]);
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            }],
            max_tokens: 5,
            task: Some("chat_companion".into()),
            ..Default::default()
        };
        let _stream = client.execute_stream_as(&req, "m").await.unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(body["transforms"][0], "middle-out");
        assert_eq!(
            body["stream"], true,
            "structural key untouched by the merge"
        );
    }

    #[tokio::test]
    async fn fallback_chain_resolves_rules_per_attempt() {
        // Spec §2 per-attempt resolution: primary @venice fails (500), chain
        // advances to built-in OpenRouter — each attempt carries ITS OWN
        // provider's rules, never the other's.
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::path("/venice/chat"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&mock)
            .await;
        wiremock::Mock::given(wiremock::matchers::path("/or/chat"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!(
                    {"choices": [{"message": {"content": "ok"}}]}
                )),
            )
            .mount(&mock)
            .await;
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "venice".to_string(),
            crate::provider::ProviderEndpoint {
                base_url: format!("{}/venice/chat", mock.uri()),
                api_key: "vk".into(),
                headers: reqwest::header::HeaderMap::new(),
                body_rules: vec![rule(
                    None,
                    serde_json::json!({"venice_parameters": {"x": 1}}),
                )],
            },
        );
        let client = OpenRouterClient::with_base_url("k".into(), format!("{}/or/chat", mock.uri()))
            .with_providers(providers)
            .with_openrouter_body_rules(vec![rule(
                None,
                serde_json::json!({"transforms": ["middle-out"]}),
            )]);
        client
            .execute(ChatRequest {
                model: "m@venice".into(),
                fallback_model: vec!["m".into()],
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                max_tokens: 5,
                task: Some("chat_companion".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let reqs = mock.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 2, "venice attempt then openrouter fallback");
        let venice: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(venice["venice_parameters"]["x"], 1);
        assert!(venice.get("transforms").is_none());
        let or: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap();
        assert_eq!(or["transforms"][0], "middle-out");
        assert!(or.get("venice_parameters").is_none());
    }

    #[tokio::test]
    async fn no_rules_wire_key_set_is_unchanged() {
        // No rules configured ⇒ the serialized path is untouched and the key
        // set is exactly today's minimal wire for this request shape.
        let mock = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response()))
            .expect(1)
            .mount(&mock)
            .await;
        let client = OpenRouterClient::with_base_url(
            "k".into(),
            format!("{}/api/v1/chat/completions", mock.uri()),
        );
        client
            .execute(ChatRequest {
                model: "m".into(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                max_tokens: 5,
                task: Some("chat_companion".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
        let mut keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["max_tokens", "messages", "model", "temperature"],
            "task must never serialize; no rules ⇒ no extra keys"
        );
    }

    // ---- body rules on the vision pre-stage (issue #225) ----

    fn vision_req() -> VisionRequest {
        VisionRequest {
            model: "vis/m".into(),
            fallback_model: vec![],
            system_prompt: "describe".into(),
            image_url: "https://x/y.png".into(),
            caption: Some("a caption".into()),
            temperature: 0.0,
            max_tokens: 64,
            reasoning: Some(ReasoningConfig {
                enabled: Some(false),
                ..Default::default()
            }),
            sampling: crate::model_config::Sampling::default(),
        }
    }

    fn vision_ok() -> serde_json::Value {
        serde_json::json!({
            "id": "v-gen",
            "model": "vis/m",
            "choices": [{ "message": { "content": "{\"desc\":\"a cat\"}" } }]
        })
    }

    #[tokio::test]
    async fn vision_body_rule_reaches_the_wire_and_beats_task_reasoning() {
        // The gap issue #225 closes: a deployer knob such as `reasoning_effort`
        // (a top-level field, NOT part of the `reasoning` object) had no way to
        // reach the vision pre-stage. It now merges, and — as on the chat path
        // — a rule's params beat the engine-built fields.
        let mock = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vision_ok()))
            .expect(1)
            .mount(&mock)
            .await;
        let client = OpenRouterClient::with_base_url(
            "k".into(),
            format!("{}/api/v1/chat/completions", mock.uri()),
        )
        .with_openrouter_body_rules(vec![rule(
            Some(&["chat_vision"]),
            serde_json::json!({"reasoning_effort": "none", "reasoning": {"enabled": true}}),
        )]);
        client.execute_vision(vision_req()).await.unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(body["reasoning_effort"], "none");
        assert_eq!(
            body["reasoning"]["enabled"], true,
            "rule params beat [tasks.chat_vision].reasoning"
        );
    }

    #[tokio::test]
    async fn vision_body_rule_cannot_flatten_the_block_array_messages() {
        // Vision's `messages` is structurally unlike chat's — the user turn's
        // `content` is a block array. `messages` is engine-owned and refused at
        // boot (`validate_providers`), so no rule can reach it; this pins that
        // the merge leaves the block array intact even when the rule is
        // otherwise the widest possible (untargeted, top-level keys).
        let mock = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vision_ok()))
            .expect(1)
            .mount(&mock)
            .await;
        let client = OpenRouterClient::with_base_url(
            "k".into(),
            format!("{}/api/v1/chat/completions", mock.uri()),
        )
        .with_openrouter_body_rules(vec![rule(
            None,
            serde_json::json!({"provider": {"sort": "price"}}),
        )]);
        client.execute_vision(vision_req()).await.unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(body["provider"]["sort"], "price", "untargeted rule applies");
        assert_eq!(body["messages"][0]["role"], "system");
        let blocks = body["messages"][1]["content"]
            .as_array()
            .expect("user content stays a block array");
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "a caption");
        assert_eq!(blocks[1]["type"], "image_url");
        assert_eq!(blocks[1]["image_url"]["url"], "https://x/y.png");
    }

    #[tokio::test]
    async fn vision_body_rule_skipped_for_unlisted_task() {
        // A rule scoped to a chat task must not bleed into the vision stage.
        let mock = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vision_ok()))
            .expect(1)
            .mount(&mock)
            .await;
        let client = OpenRouterClient::with_base_url(
            "k".into(),
            format!("{}/api/v1/chat/completions", mock.uri()),
        )
        .with_openrouter_body_rules(vec![rule(
            Some(&["chat_companion"]),
            serde_json::json!({"reasoning_effort": "none"}),
        )]);
        client.execute_vision(vision_req()).await.unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
        assert!(
            body.get("reasoning_effort").is_none(),
            "unlisted task must not merge"
        );
        assert_eq!(
            body["reasoning"]["enabled"], false,
            "request's own reasoning survives"
        );
    }

    #[tokio::test]
    async fn vision_custom_provider_strips_then_merges() {
        // Custom endpoints strip OpenRouter-only fields first, then merge —
        // so a rule on that provider can deliberately put `reasoning` back,
        // exactly as `custom_provider_strips_then_merges` pins for chat.
        let mock = MockServer::start().await;
        Mock::given(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vision_ok()))
            .expect(1)
            .mount(&mock)
            .await;
        let client =
            OpenRouterClient::with_base_url("or-key".into(), "https://unused.test/v1".into())
                .with_providers(std::collections::HashMap::from([(
                    "venice".to_string(),
                    crate::provider::ProviderEndpoint {
                        base_url: format!("{}/v1/chat/completions", mock.uri()),
                        api_key: "v-key".into(),
                        headers: reqwest::header::HeaderMap::new(),
                        body_rules: vec![rule(
                            Some(&["chat_vision"]),
                            serde_json::json!({
                                "venice_parameters": {"x": 1},
                                "reasoning": {"strip_thinking_response": true},
                            }),
                        )],
                    },
                )]));
        client
            .execute_vision(VisionRequest {
                model: "vis@venice".into(),
                ..vision_req()
            })
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(body["model"], "vis", "bare model on a custom endpoint");
        assert_eq!(body["venice_parameters"]["x"], 1);
        assert_eq!(
            body["reasoning"]["strip_thinking_response"], true,
            "a rule may put back what the subset strip removed"
        );
        assert!(
            body["reasoning"].get("enabled").is_none(),
            "the stripped [tasks.chat_vision].reasoning must not resurface"
        );
    }

    #[tokio::test]
    async fn vision_openrouter_rules_do_not_leak_to_a_custom_provider() {
        // Mirrors `each_endpoint_gets_only_its_own_rules` for the vision path.
        let mock = MockServer::start().await;
        Mock::given(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vision_ok()))
            .expect(1)
            .mount(&mock)
            .await;
        let client =
            OpenRouterClient::with_base_url("or-key".into(), "https://unused.test/v1".into())
                .with_providers(std::collections::HashMap::from([(
                    "venice".to_string(),
                    crate::provider::ProviderEndpoint {
                        base_url: format!("{}/v1/chat/completions", mock.uri()),
                        api_key: "v-key".into(),
                        headers: reqwest::header::HeaderMap::new(),
                        body_rules: Vec::new(),
                    },
                )]))
                .with_openrouter_body_rules(vec![rule(
                    None,
                    serde_json::json!({"reasoning_effort": "none"}),
                )]);
        client
            .execute_vision(VisionRequest {
                model: "vis@venice".into(),
                ..vision_req()
            })
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
        assert!(
            body.get("reasoning_effort").is_none(),
            "openrouter-entry rules must not reach a custom provider's vision call"
        );
    }
}
