// SPDX-License-Identifier: AGPL-3.0-only
//! POST /comp/chat/{session_id}/message/stream — SSE streaming chat.
//!
//! Spec: docs/superpowers/specs/2026-05-19-sse-streaming-chat-0.2-design.md

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_core::persona::CompanionPersona;
use eros_engine_core::scope::{AffinityAxis, AffinityScope, MemoryScope};
use eros_engine_core::types::QuotedMessage;
use eros_engine_llm::model_config::StyleKey;
use eros_engine_store::chat::{ChatRepo, ChatSession};
use eros_engine_store::chat_queue::{ChatQueueRepo, ClaimedEnqueueOutcome, ClaimedTurn};
use eros_engine_store::persona::PersonaRepo;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, StreamPreError};
use crate::pipeline::stream::{
    replay_stream, PersistedUserMessage, ProtocolFrame, StreamErrorCode,
};
use crate::routes::companion::{
    validate_llm_audit, validate_prompt_traits, LlmAuditDto, PromptTraitDto,
};
use crate::state::AppState;

const MAX_CONTENT_CHARS: usize = 4096;
const MAX_TIER_LEN: usize = 32;
const MAX_IMAGE_URL_LEN: usize = 2048;
const MAX_TIP_USD: f64 = 1_000_000.0;
const MIN_CLIENT_MSG_ID_LEN: usize = 26;
const MAX_CLIENT_MSG_ID_LEN: usize = 36;
const CONCURRENT_STREAMS_PER_USER: u32 = 3;
const SSE_KEEPALIVE_SECS: u64 = 15;

#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AffinityScopeName {
    Full,
    BondAndChemistry,
    Bond,
    Chemistry,
    None,
}

#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum AffinityScopeDto {
    Named(AffinityScopeName),
    #[schema(value_type = Vec<String>)]
    Axes(Vec<AffinityAxis>),
}

impl AffinityScopeDto {
    pub(crate) fn resolve(&self) -> AffinityScope {
        match self {
            // bond (warmth+intimacy+tension) ∪ chemistry (trust+intrigue+patience)
            // covers all six axes — identical to Full.
            AffinityScopeDto::Named(AffinityScopeName::Full)
            | AffinityScopeDto::Named(AffinityScopeName::BondAndChemistry) => AffinityScope::full(),
            AffinityScopeDto::Named(AffinityScopeName::Bond) => AffinityScope::bond(),
            AffinityScopeDto::Named(AffinityScopeName::Chemistry) => AffinityScope::chemistry(),
            AffinityScopeDto::Named(AffinityScopeName::None) => AffinityScope::none(),
            AffinityScopeDto::Axes(axes) => AffinityScope::from_axes(axes),
        }
    }
}

/// Per-turn image parameters. Presence of this block signals the consumer
/// handles images this turn (it drives image-action availability). The engine
/// composes the prompt and emits a single `image_request` frame — it never
/// draws on the chat stream.
///
/// `force = true` overrides the judge and makes the turn `reply_image` —
/// image only, no text reply. `content` follows the ordinary non-empty rule
/// on forced turns, and forcing requires `[tasks.chat_image_prompt_compose]`
/// to be configured (422 otherwise). A leftover `mode` key from the pre-1.0.1
/// contract deserializes and is silently ignored (spec 2026-08-03 §1).
///
/// Reference-image URLs travel on the `image_request` frame; the engine
/// never draws, so there is no engine-side draw request carrying them.
#[derive(Debug, Clone, Default, Deserialize, Serialize, utoipa::ToSchema)]
pub struct ImageReplyParams {
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub style: Option<StyleKey>,
    #[serde(default)]
    pub aspect_ratio: Option<String>,
    /// Which `[tasks.chat_image_prompt_compose].filter_prompt` variant to use
    /// for this turn: an index (`"0"`, `"1"`) for the array shape, or a key
    /// (`"a"`, `"b"`) for the table shape. Ignored when that task configures a
    /// single plain prompt.
    ///
    /// `"raw"` carries no special meaning — it selects a prompt only if a
    /// deployment actually configures one under that key.
    ///
    /// An unknown index/key falls back to the engine's built-in composer
    /// prompt — it is never an error.
    #[serde(default)]
    pub prompt_variant: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct StreamSendRequest {
    pub content: String,
    pub client_msg_id: String,
    #[serde(default)]
    pub prompt_traits: Option<Vec<PromptTraitDto>>,
    #[serde(default)]
    pub audit: Option<LlmAuditDto>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub memory_scope: Option<MemoryScope>,
    #[serde(default)]
    pub affinity_scope: Option<AffinityScopeDto>,
    #[serde(default)]
    pub tips_amount_usd: Option<f64>,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub image: Option<ImageReplyParams>,
    /// Optional id of a `chat_messages` row in this session that this turn
    /// quotes. When it resolves, the quoted line is rendered into the prompt's
    /// `[quote]` block; when present but unresolvable the turn runs without one
    /// (recorded in metadata). History is never truncated either way.
    #[serde(default)]
    pub reply_to_message_id: Option<Uuid>,
}

/// Pre-stream error body per spec §1.3. Schema-only struct for utoipa;
/// runtime renders the same shape via StreamPreError.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct StreamPreErrorBody {
    pub code: String,
    pub message: String,
    pub user_message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_user_message_id: Option<String>,
}

/// True when `url` is an absolute http(s) URL with a non-empty host, no
/// whitespace anywhere, and within the length cap. Dependency-free: we never
/// dereference the URL (it is forwarded to the vision model), so we only require
/// a plausible, whitespace-free absolute URL — not full RFC-3986 parsing.
fn image_url_is_valid(url: &str) -> bool {
    if url.is_empty() || url.len() > MAX_IMAGE_URL_LEN {
        return false;
    }
    // A URL never contains whitespace — reject it anywhere (host, path, query),
    // not just the host segment.
    if url.chars().any(char::is_whitespace) {
        return false;
    }
    let rest = match url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        Some(r) => r,
        None => return false,
    };
    // Require a non-empty host (reject "https://").
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.is_empty()
}

pub(crate) fn validate_payload(req: &StreamSendRequest) -> Result<(), AppError> {
    // Content may be empty only when a tip or an image_url is attached. A
    // forced image turn gets no exemption: the composer's strongest input is
    // the user's message (spec 2026-08-03 §2.3).
    if req.content.is_empty() && req.tips_amount_usd.is_none() && req.image_url.is_none() {
        return Err(AppError::StreamPre(StreamPreError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "unprocessable",
            message: "content must not be empty".into(),
            user_message: "请输入一条消息".into(),
            original_user_message_id: None,
        }));
    }
    if let Some(amount) = req.tips_amount_usd {
        if !amount.is_finite() || amount <= 0.0 || amount > MAX_TIP_USD {
            return Err(AppError::StreamPre(StreamPreError {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                code: "unprocessable",
                message: format!("tips_amount_usd must be a finite value in (0, {MAX_TIP_USD}]"),
                user_message: "打赏金额无效".into(),
                original_user_message_id: None,
            }));
        }
    }
    if req.content.chars().count() > MAX_CONTENT_CHARS {
        return Err(AppError::StreamPre(StreamPreError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "unprocessable",
            message: format!("content exceeds {MAX_CONTENT_CHARS} chars"),
            user_message: "消息过长，请缩短后重试".into(),
            original_user_message_id: None,
        }));
    }
    let id_len = req.client_msg_id.len();
    if !(MIN_CLIENT_MSG_ID_LEN..=MAX_CLIENT_MSG_ID_LEN).contains(&id_len) {
        return Err(AppError::StreamPre(StreamPreError {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_payload",
            message: format!(
                "client_msg_id must be {MIN_CLIENT_MSG_ID_LEN}..={MAX_CLIENT_MSG_ID_LEN} chars"
            ),
            user_message: "请求无效".into(),
            original_user_message_id: None,
        }));
    }
    if req
        .client_msg_id
        .chars()
        .any(|c| c.is_whitespace() || !c.is_ascii() || !c.is_ascii_graphic())
    {
        return Err(AppError::StreamPre(StreamPreError {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_payload",
            message: "client_msg_id must be ASCII printable, no whitespace".into(),
            user_message: "请求无效".into(),
            original_user_message_id: None,
        }));
    }
    if let Some(tier) = req.tier.as_deref() {
        let ok = (1..=MAX_TIER_LEN).contains(&tier.len())
            && tier
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'));
        if !ok {
            return Err(AppError::StreamPre(StreamPreError {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_payload",
                message: format!("tier must match [a-z0-9_]{{1,{MAX_TIER_LEN}}}"),
                user_message: "请求无效".into(),
                original_user_message_id: None,
            }));
        }
    }
    if let Some(url) = req.image_url.as_deref() {
        if req.tips_amount_usd.is_some() {
            return Err(AppError::StreamPre(StreamPreError {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                code: "unprocessable",
                message: "image_url cannot be combined with tips_amount_usd".into(),
                user_message: "图片消息暂不支持同时打赏".into(),
                original_user_message_id: None,
            }));
        }
        if !image_url_is_valid(url) {
            return Err(AppError::StreamPre(StreamPreError {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                code: "unprocessable",
                message: format!("image_url must be an absolute http(s) URL with a host and <= {MAX_IMAGE_URL_LEN} chars"),
                user_message: "图片链接无效".into(),
                original_user_message_id: None,
            }));
        }
    }
    if let Some(img) = req.image.as_ref() {
        if img.force && req.tips_amount_usd.is_some() {
            return Err(AppError::StreamPre(StreamPreError {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                code: "unprocessable",
                message: "forced image cannot be combined with tips_amount_usd".into(),
                user_message: "打赏消息暂不支持图片回复".into(),
                original_user_message_id: None,
            }));
        }
        if let Some(ar) = img.aspect_ratio.as_deref() {
            if !aspect_ratio_supported(ar) {
                return Err(AppError::StreamPre(StreamPreError {
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                    code: "unprocessable",
                    message: "unsupported aspect_ratio".into(),
                    user_message: "不支持的画幅比例".into(),
                    original_user_message_id: None,
                }));
            }
        }
    }
    Ok(())
}

/// The aspect-ratio allow-list shared by the chat stream and the persona
/// compose endpoint.
pub(crate) fn aspect_ratio_supported(ar: &str) -> bool {
    matches!(ar, "1:1" | "3:4" | "4:3" | "9:16" | "16:9")
}

/// `image.force` demands the composer (spec 2026-08-03 §2.3): with no
/// `[tasks.chat_image_prompt_compose]` nothing can write the prompt, so the
/// request is refused pre-stream instead of degrading to a generic portrait.
/// Separate from `validate_payload`, which stays pure/config-free.
pub(crate) fn validate_image_capability(
    req: &StreamSendRequest,
    model_config: &eros_engine_llm::model_config::ModelConfig,
) -> Result<(), AppError> {
    if req.image.as_ref().is_some_and(|i| i.force)
        && !model_config.has_task("chat_image_prompt_compose")
    {
        return Err(AppError::StreamPre(StreamPreError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "unprocessable",
            message: "image.force requires [tasks.chat_image_prompt_compose] to be configured"
                .into(),
            user_message: "该服务未启用图片回复".into(),
            original_user_message_id: None,
        }));
    }
    Ok(())
}

/// Session-load / ownership / voice-channel / instance_id / persona-exists
/// pre-flight shared by the stream handler (and, in turn, its async sibling).
pub(crate) async fn resolve_text_turn(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
) -> Result<(ChatSession, CompanionPersona, Uuid), AppError> {
    let chat_repo = ChatRepo { pool: &state.pool };
    let session = chat_repo.get_session(session_id).await?.ok_or_else(|| {
        AppError::StreamPre(StreamPreError {
            status: StatusCode::NOT_FOUND,
            code: "session_not_found",
            message: "session not found".into(),
            user_message: "会话不存在".into(),
            original_user_message_id: None,
        })
    })?;
    if session.user_id != user_id {
        return Err(AppError::StreamPre(StreamPreError {
            status: StatusCode::FORBIDDEN,
            code: "session_forbidden",
            message: "session not owned by JWT user".into(),
            user_message: "无权访问该会话".into(),
            original_user_message_id: None,
        }));
    }
    // Channel-scoped, mirroring the voice endpoint's own gate: this handler
    // writes text-channel rows, so it must not operate on a voice session.
    // Without this, a text turn lands in a voice conversation's history and
    // the two channels interleave in a single transcript — and it is what let
    // a text `client_msg_id` collide with the voice barge-in lookups
    // (2026-08-11-voice-barge-in-interrupt-design.md). Those lookups now filter
    // `channel = 'voice'` themselves, so this is defence in depth on the
    // writing side rather than the only guard.
    if session.channel == "voice" {
        return Err(AppError::StreamPre(StreamPreError {
            status: StatusCode::CONFLICT,
            code: "wrong_channel",
            message: "session is a voice-channel session".into(),
            user_message: "该会话是语音会话".into(),
            original_user_message_id: None,
        }));
    }
    let instance_id = session.instance_id.ok_or_else(|| {
        AppError::StreamPre(StreamPreError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            message: "session has no instance_id".into(),
            user_message: "服务出现问题，请稍后再试".into(),
            original_user_message_id: None,
        })
    })?;
    // Verify the instance still exists and is active (404 otherwise) before
    // opening the stream. (Previously this load also fed the NFT-ownership gate.)
    // The loaded persona is threaded into the detached drive task's
    // `run_stream` call below so the turn does not re-issue the same
    // `load_companion` query inside the generator.
    let persona_repo = PersonaRepo { pool: &state.pool };
    let persona = persona_repo
        .load_companion(instance_id)
        .await?
        .ok_or_else(|| AppError::NotFound("instance not found".into()))?;
    Ok((session, persona, instance_id))
}

/// Resolve the caller's `reply_to_message_id` into the message it quotes. A
/// present-but-unresolvable id does not fail the request and does not touch
/// history — the turn simply runs without a `[quote]` block, and
/// `build_user_row_metadata` records `reply_to_error` on the persisted row.
pub(crate) async fn resolve_quote(
    chat_repo: &ChatRepo<'_>,
    session_id: Uuid,
    reply_to: Option<Uuid>,
) -> Result<Option<QuotedMessage>, AppError> {
    let Some(id) = reply_to else {
        return Ok(None);
    };
    Ok(chat_repo
        .message_by_id_in_session(session_id, id)
        .await?
        .map(|m| QuotedMessage {
            message_id: m.id,
            role: m.role,
            content: m.content,
            sent_at: m.sent_at,
        }))
}

// Build metadata: conditionally include tips_amount_usd, tier, and image_url.
// tier is omitted entirely (not written as null) when absent — keeps JSONB shape sparse.
pub(crate) fn build_user_row_metadata(
    req: &StreamSendRequest,
    quote: Option<&QuotedMessage>,
) -> Option<serde_json::Value> {
    let mut meta_map = serde_json::Map::new();
    if let Some(amount) = req.tips_amount_usd {
        meta_map.insert("tips_amount_usd".into(), serde_json::json!(amount));
    }
    if let Some(t) = req.tier.as_deref() {
        meta_map.insert("tier".into(), serde_json::json!(t));
    }
    if let Some(url) = req.image_url.as_deref() {
        meta_map.insert("image_url".into(), serde_json::json!(url));
    }
    // Pre-validation, pre-resolve raw snapshot of what the frontend sent.
    // The `_raw` suffix distinguishes these from the post-resolve `memory_scope`
    // / `affinity_scope` / `prompt_traits` written on the matching assistant row.
    // An operator diffing the two can spot allow-list misconfiguration or
    // frontend/backend shape drift.
    if let Some(ms) = req.memory_scope.as_ref() {
        meta_map.insert(
            "memory_scope_raw".into(),
            serde_json::to_value(ms).expect("MemoryScope serializes"),
        );
    }
    if let Some(asd) = req.affinity_scope.as_ref() {
        meta_map.insert(
            "affinity_scope_raw".into(),
            serde_json::to_value(asd).expect("AffinityScopeDto serializes"),
        );
    }
    if let Some(pt) = req.prompt_traits.as_ref() {
        // PromptTraitDto does not derive Serialize (lives in companion.rs).
        // Hand-build the JSON shape — `{tag, text}` per element — so an empty
        // input vec round-trips as `[]` (not omitted).
        let arr: Vec<serde_json::Value> = pt
            .iter()
            .map(|t| serde_json::json!({"tag": t.tag, "text": t.text}))
            .collect();
        meta_map.insert("prompt_traits_raw".into(), serde_json::Value::Array(arr));
    }
    // A quote that resolved is the anchor the history routes hand back; one the
    // caller asked for but we could not find leaves an audit-only error marker.
    match (quote, req.reply_to_message_id) {
        (Some(q), _) => {
            meta_map.insert(
                "reply_to_message_id".into(),
                serde_json::json!(q.message_id),
            );
        }
        (None, Some(_)) => {
            meta_map.insert("reply_to_error".into(), serde_json::json!("not_found"));
        }
        (None, None) => {}
    }
    if meta_map.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(meta_map))
    }
}

pub(crate) fn persisted_content_role(req: &StreamSendRequest) -> (String, &'static str) {
    match req.tips_amount_usd {
        Some(amount) if req.content.is_empty() => (
            format!("(打赏 ${})", crate::prompt::fmt_amount(amount)),
            "gift_user",
        ),
        Some(_) => (req.content.clone(), "gift_user"),
        None => (req.content.clone(), "user"),
    }
}

#[utoipa::path(
    post,
    path = "/comp/chat/{session_id}/message/stream",
    tag = "companion",
    params(("session_id" = Uuid, Path, description = "Chat session id")),
    request_body = StreamSendRequest,
    responses(
        (status = 200, description = "SSE event stream (text/event-stream). The `meta` frame's \
            `action_type` is one of `reply` | `ghost` | `reply_image` | `reply_text_image` | \
            `product_qa`. \
            Note the asymmetry: a plain-text `reply_text` turn is reported as `reply` (there is no \
            `reply_text` on the wire), while the text+image variant keeps its full \
            `reply_text_image` name. `product_qa` marks an out-of-character product answer \
            (excluded from companion context; also reported on replay). Clients must tolerate \
            unknown `action_type` values.", content_type = "text/event-stream"),
        (status = 400, body = StreamPreErrorBody),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, body = StreamPreErrorBody),
        (status = 404, body = StreamPreErrorBody),
        (status = 409, body = StreamPreErrorBody),
        (status = 422, body = StreamPreErrorBody),
        (status = 429, body = StreamPreErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn send_message_stream(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<StreamSendRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    // Validate payload first — before any DB call — so 422/400 never waste a roundtrip.
    validate_payload(&req)?;
    validate_image_capability(&req, &state.model_config)?;
    let prompt_traits = validate_prompt_traits(req.prompt_traits.as_deref().unwrap_or(&[]))
        .map_err(|e| {
            AppError::StreamPre(StreamPreError {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_payload",
                message: e.to_string(),
                user_message: "请求无效".into(),
                original_user_message_id: None,
            })
        })?;
    let audit = validate_llm_audit(req.audit.clone()).map_err(|e| {
        AppError::StreamPre(StreamPreError {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_payload",
            message: e.to_string(),
            user_message: "请求无效".into(),
            original_user_message_id: None,
        })
    })?;

    let (_session, persona, instance_id) = resolve_text_turn(&state, session_id, user_id).await?;
    let chat_repo = ChatRepo { pool: &state.pool };
    // Acquire a stream slot. `StreamSlotGuard` is now `'static` (holds Arc),
    // so it can be moved into the SSE body below.
    let guard = state
        .stream_slots
        .try_acquire(user_id, CONCURRENT_STREAMS_PER_USER)
        .ok_or_else(|| {
            AppError::StreamPre(StreamPreError {
                status: StatusCode::TOO_MANY_REQUESTS,
                code: "rate_limited",
                message: format!("per-user stream cap reached ({CONCURRENT_STREAMS_PER_USER})"),
                user_message: "请求过多，请稍后再试".into(),
                original_user_message_id: None,
            })
        })?;
    // Held here as an Option: the Claimed arm below moves it into the
    // detached drive task (spec §8 — the cap bounds generations, not
    // connections), so the SSE body's own guard slot is `None` on that path
    // and `Some` only on Replay, where the body is still the sole owner.
    let mut guard = Some(guard);

    // Resolve the quoted message BEFORE the upsert so the outcome can be
    // recorded in the new row's metadata. A present-but-unresolvable id does not
    // fail the request — the turn just runs without a [quote] block.
    let quote = resolve_quote(&chat_repo, session_id, req.reply_to_message_id).await?;

    let persisted_metadata = build_user_row_metadata(&req, quote.as_ref());
    let (persisted_content, persisted_role) = persisted_content_role(&req);
    let params_value = serde_json::to_value(crate::routes::companion_async::QueuedTurnParams {
        tier: req.tier.clone(),
        memory_scope: req.memory_scope,
        affinity_scope: req.affinity_scope.clone(),
        prompt_traits: req.prompt_traits.clone(),
        audit: req.audit.clone(),
        tips_amount_usd: req.tips_amount_usd,
        image_url: req.image_url.clone(),
        image: req.image.clone(),
        reply_to_message_id: req.reply_to_message_id,
    })
    .expect("QueuedTurnParams serializes");
    let queue_repo = ChatQueueRepo { pool: &state.pool };
    let outcome = queue_repo
        .enqueue_user_message_claimed(
            session_id,
            &persisted_content,
            &req.client_msg_id,
            persisted_role,
            persisted_metadata.as_ref(),
            user_id,
            &params_value,
        )
        .await?;
    // Resolve the proto stream. Replay returns early with a boxed stream;
    // Claimed drives the turn on a detached task and taps its frames.
    // Boxing erases the concrete type so both branches can feed the same
    // SSE wrapper.
    let proto: std::pin::Pin<Box<dyn futures_util::Stream<Item = ProtocolFrame> + Send>> =
        match outcome {
            ClaimedEnqueueOutcome::Claimed {
                user_message_id,
                queue_id,
            } => {
                let memory_scope = req.memory_scope.unwrap_or_default();
                let affinity_scope = req
                    .affinity_scope
                    .as_ref()
                    .map(AffinityScopeDto::resolve)
                    .unwrap_or_default();
                let user_msg = PersistedUserMessage {
                    user_message_id,
                    session_id,
                    user_id,
                    instance_id,
                    content: persisted_content.clone(),
                    prompt_traits: prompt_traits.clone(),
                    audit: audit.clone(),
                    tier: req.tier.clone(),
                    memory_scope,
                    affinity_scope,
                    tips_amount_usd: req.tips_amount_usd,
                    image_url: req.image_url.clone(),
                    image: req.image.clone(),
                    quote: quote.clone(),
                };
                let turn = ClaimedTurn {
                    queue_id,
                    session_id,
                    user_message_id,
                    user_id,
                    attempts: 1,
                    params: Some(params_value),
                };
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ProtocolFrame>();
                // The detached task owns the generation (spec §3): the SSE
                // body below is only a tap, so a client disconnect never
                // cancels the drive. The task also owns the StreamSlotGuard
                // (spec §8: the cap bounds generations, not connections) and
                // settles TerminalOnFailure (spec §6: no background retry).
                let drive_state = state.clone();
                let guard = guard.take();
                tokio::spawn(async move {
                    let _guard = guard;
                    // Held only until the timeout arm below has a chance to
                    // use it — see the explicit `drop` right after this
                    // `match`. Without this clone, the timeout arm would have
                    // nothing to send through: `tx` itself is moved into
                    // `drive_to_exhaustion` and dropped with that future the
                    // instant the timeout fires, which would end the SSE body
                    // on bare EOF, indistinguishable from success, even
                    // though a `system_error` row lands in history.
                    let timeout_tx = tx.clone();
                    let outcome = match tokio::time::timeout(
                        drive_state.config.chat_queue.generation_timeout,
                        crate::pipeline::chat_queue::drive_to_exhaustion(
                            Arc::new(drive_state.clone()),
                            user_msg,
                            Some(persona),
                            Some(tx),
                        ),
                    )
                    .await
                    {
                        Ok(o) => o,
                        Err(_) => {
                            let _ = timeout_tx.send(ProtocolFrame::Error {
                                code: StreamErrorCode::GenerationTimeout,
                                retryable: true,
                                message: "generation timeout".into(),
                                user_message: "生成超时，请稍后再试".into(),
                                upstream_status: None,
                                provider_code: None,
                            });
                            crate::pipeline::chat_queue::TurnOutcome::Failure(
                                "generation timeout".into(),
                            )
                        }
                    };
                    // `UnboundedSender::send` enqueues synchronously, so the
                    // timeout arm's Error frame (if any) is already buffered
                    // by this point — dropping the clone here, rather than
                    // holding it to the end of the task, restores the
                    // success-path behavior the tests document: the SSE body
                    // closes the instant the drive future returns, not after
                    // `settle_turn`'s DB write and `notify_one`.
                    drop(timeout_tx);
                    crate::pipeline::chat_queue::settle_turn(
                        &drive_state,
                        &turn,
                        outcome,
                        crate::pipeline::chat_queue::SettleMode::TerminalOnFailure,
                    )
                    .await;
                    // Queued async turns on this session become claimable now.
                    drive_state.chat_queue_notify.notify_one();
                });
                Box::pin(async_stream::stream! {
                    while let Some(frame) = rx.recv().await {
                        yield frame;
                    }
                })
            }
            ClaimedEnqueueOutcome::DuplicateInProgress { user_message_id } => {
                return Err(AppError::StreamPre(StreamPreError {
                    status: StatusCode::CONFLICT,
                    code: "duplicate_in_progress",
                    message: "same (session_id, client_msg_id) is still generating".into(),
                    user_message: "上一条消息正在处理中，请稍候".into(),
                    original_user_message_id: Some(user_message_id),
                }));
            }
            ClaimedEnqueueOutcome::Replay {
                ghost,
                assistant_chain,
                ..
            } => {
                let state_arc = Arc::new(state.clone());
                Box::pin(replay_stream(
                    state_arc,
                    session_id,
                    user_id,
                    ghost,
                    assistant_chain,
                ))
            }
        };

    // Move the StreamSlotGuard into the stream so it is released only when
    // the response body finishes. `StreamSlotGuard` holds `Arc<StreamSlots>`
    // so it satisfies the `'static` bound required by axum's `Sse`.
    let sse = futures_util::StreamExt::map(
        async_stream::stream! {
            let _guard = guard;
            futures_util::pin_mut!(proto);
            while let Some(frame) = futures_util::StreamExt::next(&mut proto).await {
                yield frame;
            }
        },
        |frame: ProtocolFrame| {
            let json =
                serde_json::to_string(&frame).expect("ProtocolFrame serialization is infallible");
            Ok::<_, Infallible>(Event::default().data(json))
        },
    );

    Ok(Sse::new(sse).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(SSE_KEEPALIVE_SECS))
            .text("ping"),
    ))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(send_message_stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::companion::testutil::seed_persona_instance;
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request};
    use axum::Router;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::{json, Value};
    use sqlx::PgPool;
    use tower::Service;

    fn req_with_tier(tier: Option<&str>) -> StreamSendRequest {
        StreamSendRequest {
            content: "hi".into(),
            client_msg_id: "01J5555555555555555555555A".into(),
            prompt_traits: None,
            audit: None,
            tier: tier.map(String::from),
            memory_scope: None,
            affinity_scope: None,
            tips_amount_usd: None,
            image_url: None,
            image: None,
            reply_to_message_id: None,
        }
    }

    fn req_tip(amount: Option<f64>, content: &str) -> StreamSendRequest {
        StreamSendRequest {
            content: content.into(),
            client_msg_id: "01J5555555555555555555555A".into(),
            prompt_traits: None,
            audit: None,
            tier: None,
            memory_scope: None,
            affinity_scope: None,
            tips_amount_usd: amount,
            image_url: None,
            image: None,
            reply_to_message_id: None,
        }
    }

    #[test]
    fn validate_payload_tip_allows_empty_content() {
        assert!(validate_payload(&req_tip(Some(20.0), "")).is_ok());
    }

    #[test]
    fn validate_payload_rejects_empty_content_without_tip() {
        assert!(validate_payload(&req_tip(None, "")).is_err());
    }

    #[test]
    fn validate_payload_rejects_bad_tip_amounts() {
        assert!(validate_payload(&req_tip(Some(0.0), "")).is_err());
        assert!(validate_payload(&req_tip(Some(-5.0), "")).is_err());
        assert!(validate_payload(&req_tip(Some(2_000_000.0), "")).is_err());
        assert!(validate_payload(&req_tip(Some(f64::NAN), "")).is_err());
        assert!(validate_payload(&req_tip(Some(f64::INFINITY), "")).is_err());
    }

    #[test]
    fn validate_payload_accepts_wellformed_tip() {
        assert!(validate_payload(&req_tip(Some(2.0), "")).is_ok());
        assert!(validate_payload(&req_tip(Some(20000.0), "")).is_ok());
        assert!(validate_payload(&req_tip(Some(1_000_000.0), "")).is_ok());
    }

    #[test]
    fn affinity_scope_dto_resolves_named_and_array() {
        let named: AffinityScopeDto = serde_json::from_str("\"chemistry\"").unwrap();
        assert_eq!(named.resolve(), AffinityScope::chemistry());
        let bond: AffinityScopeDto = serde_json::from_str("\"bond\"").unwrap();
        assert_eq!(bond.resolve(), AffinityScope::bond());
        let full: AffinityScopeDto = serde_json::from_str("\"full\"").unwrap();
        assert_eq!(full.resolve(), AffinityScope::full());
        let bac: AffinityScopeDto = serde_json::from_str("\"bond_and_chemistry\"").unwrap();
        assert_eq!(bac.resolve(), AffinityScope::full());
        let none: AffinityScopeDto = serde_json::from_str("\"none\"").unwrap();
        assert!(none.resolve().is_empty());
        let arr: AffinityScopeDto = serde_json::from_str("[\"warmth\",\"trust\"]").unwrap();
        assert_eq!(
            arr.resolve(),
            AffinityScope::from_axes(&[AffinityAxis::Warmth, AffinityAxis::Trust])
        );
        let empty: AffinityScopeDto = serde_json::from_str("[]").unwrap();
        assert!(empty.resolve().is_empty());
        assert!(serde_json::from_str::<AffinityScopeDto>("\"bogus\"").is_err());
    }

    #[test]
    fn validate_payload_accepts_wellformed_and_absent_tier() {
        assert!(validate_payload(&req_with_tier(Some("gold"))).is_ok());
        assert!(validate_payload(&req_with_tier(Some("free_2"))).is_ok());
        assert!(validate_payload(&req_with_tier(None)).is_ok());
    }

    #[test]
    fn validate_payload_rejects_malformed_tier() {
        // uppercase, punctuation, and over-length are all rejected.
        assert!(validate_payload(&req_with_tier(Some("Gold"))).is_err());
        assert!(validate_payload(&req_with_tier(Some("gold!"))).is_err());
        assert!(validate_payload(&req_with_tier(Some(""))).is_err());
        assert!(validate_payload(&req_with_tier(Some(&"x".repeat(MAX_TIER_LEN + 1)))).is_err());
    }

    fn mint_jwt(uid: Uuid) -> String {
        let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp();
        encode(
            &Header::default(),
            &json!({ "sub": uid.to_string(), "exp": exp }),
            &EncodingKey::from_secret(crate::routes::companion::TEST_SECRET.as_ref()),
        )
        .unwrap()
    }

    fn build_router(state: AppState) -> Router {
        let (axum, _api) = crate::routes::router(state.clone()).split_for_parts();
        axum.with_state(state)
    }

    // Mirrors pipeline::chat_queue's own `wait_for_status` worker-test helper
    // (cross-module test helpers are not shared): the detached drive task's
    // settle_turn() write races the SSE body's completion, so tests must poll
    // for the terminal status rather than reading once.
    async fn wait_for_status(pool: &PgPool, queue_id: Uuid, wanted: &[&str]) -> String {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let s: String =
                    sqlx::query_scalar("SELECT status FROM engine.chat_turn_queue WHERE id = $1")
                        .bind(queue_id)
                        .fetch_one(pool)
                        .await
                        .unwrap();
                if wanted.contains(&s.as_str()) {
                    return s;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("turn should reach a terminal status")
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn stream_turn_rides_the_queue_and_settles_done(pool: PgPool) {
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2,\"total_tokens\":4},\"id\":\"gen-q\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let chat_repo = ChatRepo { pool: &pool };
        let session_id = chat_repo
            .create_session(user_id, instance_id)
            .await
            .unwrap()
            .id;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        let mut app = build_router(state);
        let token = mint_jwt(user_id);
        let body = serde_json::to_vec(&json!({
            "content": "hi",
            "client_msg_id": "01J4444444444444444444444Q"
        }))
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri(format!("/comp/chat/{session_id}/message/stream"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Capture the body (rather than discard it) and assert the frame
        // sequence: without this, nothing in the suite proves frames survive
        // the mpsc hop into the SSE body — a tap forward regressed to a
        // no-op would stay green.
        let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body_text = std::str::from_utf8(&body).unwrap();
        let meta_at = body_text
            .find("\"type\":\"meta\"")
            .expect("meta frame reaches the SSE body");
        let done_at = body_text
            .find("\"type\":\"done\"")
            .expect("done frame reaches the SSE body");
        let final_at = body_text
            .find("\"type\":\"final\"")
            .expect("final frame reaches the SSE body");
        assert!(
            meta_at < done_at && done_at < final_at,
            "frame order preserved through the tap"
        );
        // Delta frames carry the actual generated text — a tap that forwarded
        // only the control frames would still pass the ordering assertions
        // above. Deltas only exist on the reply path, so gate on it (PDE may
        // ghost the turn, and a ghost yields Meta → Done → Final with no
        // Delta).
        if body_text.contains("\"action_type\":\"reply\"") {
            let delta_at = body_text
                .find("\"type\":\"delta\"")
                .expect("delta frames reach the SSE body");
            assert!(
                meta_at < delta_at && delta_at < done_at,
                "delta sits between meta and done"
            );
            assert!(
                body_text.contains("hi") && body_text.contains(" there"),
                "the generated text reaches the client; got {body_text}"
            );
        }

        // The detached drive task's settle_turn() write races the SSE body's
        // completion: the tap channel closes (ending the body) the instant
        // drive_to_exhaustion returns, which is BEFORE settle_turn's UPDATE
        // lands. Poll for the terminal status rather than reading once —
        // mirrors pipeline::chat_queue's own wait_for_status test helper,
        // used for the identical spawn-then-settle race on the worker path.
        let (status, attempts): (String, i32) = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            async {
                loop {
                    let row: Option<(String, i32)> = sqlx::query_as(
                        "SELECT q.status, q.attempts FROM engine.chat_turn_queue q \
                         JOIN engine.chat_messages m ON m.id = q.user_message_id \
                         WHERE m.session_id = $1 AND m.client_msg_id = '01J4444444444444444444444Q'",
                    )
                    .bind(session_id)
                    .fetch_optional(&pool)
                    .await
                    .unwrap();
                    if let Some((status, attempts)) = row {
                        if status == "done" || status == "failed" {
                            return (status, attempts);
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            },
        )
        .await
        .expect("queue row should reach a terminal status");
        assert_eq!(status, "done", "a served stream turn settles its queue row");
        assert_eq!(
            attempts, 1,
            "born claimed: the handler drive is the only attempt"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn generation_timeout_surfaces_terminal_error_frame(pool: PgPool) {
        // Proves the detached task's timeout arm no longer ends the SSE body
        // on bare EOF (indistinguishable from success) while a system_error
        // row lands in history — it now sends a terminal Error frame through
        // a held-open sender clone before settling the turn.
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream")
                    // 10s comfortably outlasts the 100ms generation_timeout
                    // below no matter how much setup work runs first — the
                    // timeout arm always wins this race.
                    .set_delay(std::time::Duration::from_secs(10)),
            )
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let chat_repo = ChatRepo { pool: &pool };
        let session_id = chat_repo
            .create_session(user_id, instance_id)
            .await
            .unwrap()
            .id;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        // Shrink the whole-turn wall-clock budget so the detached task's
        // timeout arm fires long before the mock's 10s delayed response.
        state.config.chat_queue.generation_timeout = std::time::Duration::from_millis(100);
        let mut app = build_router(state);
        let token = mint_jwt(user_id);
        let body = serde_json::to_vec(&json!({
            "content": "hi",
            "client_msg_id": "01J4444444444444444444444T"
        }))
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri(format!("/comp/chat/{session_id}/message/stream"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body_text = std::str::from_utf8(&bytes).unwrap();
        assert!(
            body_text.contains("\"type\":\"error\""),
            "no error frame in body: {body_text}"
        );
        assert!(
            body_text.contains("generation timeout"),
            "error frame missing timeout message: {body_text}"
        );
        assert!(
            body_text.contains("\"code\":\"generation_timeout\""),
            "error frame missing generation_timeout code: {body_text}"
        );

        let queue_id: Uuid = sqlx::query_scalar(
            "SELECT q.id FROM engine.chat_turn_queue q \
             JOIN engine.chat_messages m ON m.id = q.user_message_id \
             WHERE m.session_id = $1 AND m.client_msg_id = '01J4444444444444444444444T'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let status = wait_for_status(&pool, queue_id, &["done", "failed"]).await;
        assert_eq!(
            status, "failed",
            "a timed-out turn goes terminal, not silently done"
        );
        let notices: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'system_error'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(notices, 1, "terminal timeout leaves exactly one signal row");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn disconnect_mid_generation_still_persists_and_settles(pool: PgPool) {
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2,\"total_tokens\":4},\"id\":\"gen-q\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream")
                    // Keeps the generation in flight past the point where we
                    // drop the response below, proving the detached task
                    // survives a client disconnect mid-generation (spec §3).
                    .set_delay(std::time::Duration::from_millis(500)),
            )
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let chat_repo = ChatRepo { pool: &pool };
        let session_id = chat_repo
            .create_session(user_id, instance_id)
            .await
            .unwrap()
            .id;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        let mut app = build_router(state);
        let token = mint_jwt(user_id);
        let body = serde_json::to_vec(&json!({
            "content": "hi",
            "client_msg_id": "01J4444444444444444444444D"
        }))
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri(format!("/comp/chat/{session_id}/message/stream"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        drop(resp); // client disconnects: the SSE body (the tap receiver) is dropped

        // The detached task must finish the turn with nobody listening.
        let queue_id: Uuid = sqlx::query_scalar(
            "SELECT q.id FROM engine.chat_turn_queue q \
             JOIN engine.chat_messages m ON m.id = q.user_message_id \
             WHERE m.session_id = $1 AND m.client_msg_id = '01J4444444444444444444444D'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let status = wait_for_status(&pool, queue_id, &["done", "failed"]).await;
        assert_eq!(status, "done");
        let user_message_id: Uuid =
            sqlx::query_scalar("SELECT user_message_id FROM engine.chat_turn_queue WHERE id = $1")
                .bind(queue_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let assistant: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
             WHERE user_message_id = $1 AND role = 'assistant'",
        )
        .bind(user_message_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let ghosted: bool =
            sqlx::query_scalar("SELECT ghost_decision FROM engine.chat_messages WHERE id = $1")
                .bind(user_message_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            assistant > 0 || ghosted,
            "the reply must persist (or ghost) after the client is gone"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn failed_stream_turn_goes_terminal_without_retry(pool: PgPool) {
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Zero-Done failure recipe, adapted from pipeline::chat_queue's
        // `failing_turn_releases_then_goes_terminal_with_system_error` to
        // enter through the HTTP router instead of `drain_once`. See that
        // test's comment for why product_qa — not the plain chat_companion
        // path — is the way to reach zero-Done+Error (Failure) through the
        // real pipeline: chat_companion's per-attempt loop always persists
        // and yields a Done frame (⇒ Success) before chain exhaustion runs,
        // and a cleared phrase table only matters for that path anyway —
        // product_qa's own hand-rolled exhaustion arm persists nothing and
        // emits only an Error frame.
        sqlx::query(
            "UPDATE engine.error_handling_config \
             SET payload = '[]'::jsonb \
             WHERE kind = 'chat_stream_failure_fallback_phrases'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let mock = MockServer::start().await;
        let verdict = serde_json::json!({ "action": "product_qa", "inner_state": "" }).to_string();
        let judge_body = serde_json::json!({
            "id": "gj", "model": "pde/judge",
            "choices": [{"message": {"content": verdict}}],
        });
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_string_contains("pde/judge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(judge_body))
            .mount(&mock)
            .await;
        // Both product_qa executor candidates fail — the chain exhausts.
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_string_contains("qa/exec-a"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_string_contains("qa/exec-b"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let chat_repo = ChatRepo { pool: &pool };
        let session_id = chat_repo
            .create_session(user_id, instance_id)
            .await
            .unwrap()
            .id;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel=\"deepseek/x\"\n\
                 [tasks.pde_decision]\nmodel=\"pde/judge\"\nfilter_prompt=\"Decide the action and inner_state.\"\n\
                 [tasks.chat_product_qa]\nmodel=\"qa/exec-a\"\nfallback=[\"qa/exec-b\"]\nretry_depth=1\n\
                 filter_prompt=\"Answer using the product docs below.\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let mut app = build_router(state);
        let token = mint_jwt(user_id);
        let body = serde_json::to_vec(&json!({
            "content": "这个产品能用几年",
            "client_msg_id": "01J4444444444444444444444F"
        }))
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri(format!("/comp/chat/{session_id}/message/stream"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();

        let queue_id: Uuid = sqlx::query_scalar(
            "SELECT q.id FROM engine.chat_turn_queue q \
             JOIN engine.chat_messages m ON m.id = q.user_message_id \
             WHERE m.session_id = $1 AND m.client_msg_id = '01J4444444444444444444444F'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let status = wait_for_status(&pool, queue_id, &["done", "failed"]).await;
        assert_eq!(status, "failed", "stream turns never release for retry");
        let attempts: i32 =
            sqlx::query_scalar("SELECT attempts FROM engine.chat_turn_queue WHERE id = $1")
                .bind(queue_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(attempts, 1, "terminal on the first failure — no ladder");
        let notices: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'system_error'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(notices, 1, "terminal failure leaves exactly one signal row");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn stream_422_when_content_empty(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('S', 'p', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let state = crate::routes::companion::test_state(pool);
        let mut app = build_router(state);
        let token = mint_jwt(user_id);
        let body =
            serde_json::to_vec(&json!({"content":"","client_msg_id":"01J2222222222222222222222A"}))
                .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri(format!("/comp/chat/{session_id}/message/stream"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["code"], "unprocessable");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn stream_409_when_session_is_voice_channel(pool: PgPool) {
        // The text stream writes text-channel rows, so it must refuse a voice
        // session — otherwise a text turn lands in a voice conversation's
        // history and the two channels interleave in one transcript. Rejected
        // BEFORE any row is persisted.
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('S', 'p', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, channel) \
             VALUES ($1, $2, 'voice') RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let mut app = build_router(crate::routes::companion::test_state(pool.clone()));
        let token = mint_jwt(user_id);
        let body = serde_json::to_vec(
            &json!({"content":"typed","client_msg_id":"01J2222222222222222222222V"}),
        )
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri(format!("/comp/chat/{session_id}/message/stream"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["code"], "wrong_channel");

        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.chat_messages WHERE session_id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rows, 0, "a rejected text turn must not persist any row");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn stream_422_when_forced_image_without_composer(pool: PgPool) {
        // spec 2026-08-03 §2.3: image.force with no [tasks.chat_image_prompt_compose]
        // is refused pre-stream — no user row may be persisted.
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('S', 'p', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // Default test_state carries no chat_image_prompt_compose task.
        let state = crate::routes::companion::test_state(pool.clone());
        let mut app = build_router(state);
        let token = mint_jwt(user_id);
        let body = serde_json::to_vec(&json!({
            "content": "draw me",
            "client_msg_id": "01J5555555555555555555555A",
            "image": {"force": true}
        }))
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri(format!("/comp/chat/{session_id}/message/stream"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["code"], "unprocessable");

        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.chat_messages WHERE session_id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 0, "pre-stream rejection must not persist a user row");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn stream_400_when_tier_malformed(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('S', 'p', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let state = crate::routes::companion::test_state(pool);
        let mut app = build_router(state);
        let token = mint_jwt(user_id);
        let body = serde_json::to_vec(&json!({
            "content":"hi",
            "client_msg_id":"01J4444444444444444444444A",
            "tier":"Gold!"
        }))
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri(format!("/comp/chat/{session_id}/message/stream"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["code"], "invalid_payload");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn stream_replay_emits_same_frames_for_repeat_client_msg_id(pool: PgPool) {
        use eros_engine_store::chat::{AssistantInsert, ChatRepo, UpsertUserOutcome};
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('R', 'p', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        ).bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // Pre-seed an original-request outcome.
        let chat_repo = ChatRepo { pool: &pool };
        let user_msg_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01J3333333333333333333333A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };
        // Since 0060 chat_messages.generation_id FK-references
        // engine.llm_generations. Release A's write path creates the parent;
        // this test writes the child row directly, so it seeds one itself.
        sqlx::query(
            "INSERT INTO engine.llm_generations (generation_id, task) \
             VALUES ('gen-1', 'chat_companion')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let assistant_uuid: Uuid = ulid::Ulid::new().into();
        chat_repo
            .insert_assistant_batch(
                session_id,
                user_msg_id,
                &[AssistantInsert {
                    llm_attempts: None,
                    gateway_errors: None,
                    id: assistant_uuid,
                    content: "replayed reply".into(),
                    assistant_action_type: "reply".into(),
                    continues_from_message_id: None,
                    truncated: false,
                    generation_id: Some("gen-1".into()),
                    filter_audit: None,
                    metadata: None,
                }],
            )
            .await
            .unwrap();

        let state = crate::routes::companion::test_state(pool);
        let mut app = build_router(state);
        let token = mint_jwt(user_id);
        let body = serde_json::to_vec(
            &json!({"content":"hi","client_msg_id":"01J3333333333333333333333A"}),
        )
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri(format!("/comp/chat/{session_id}/message/stream"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.starts_with("text/event-stream"));
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body_text = std::str::from_utf8(&bytes).unwrap();
        // The replayed delta carries the persisted content verbatim.
        assert!(body_text.contains("replayed reply"), "body: {body_text}");
        // And the final frame closes the stream.
        assert!(
            body_text.contains("\"type\":\"final\""),
            "body: {body_text}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn user_row_writes_scope_raw_keys_when_request_carries_them(pool: sqlx::PgPool) {
        use eros_engine_core::scope::MemoryScope;
        use eros_engine_store::chat::ChatRepo;
        // Build raw metadata bag mirroring what the route handler would build
        // for a request with all three new fields populated.
        let mut meta_map = serde_json::Map::new();
        let ms = MemoryScope::NeutralOnly;
        let asd_value: serde_json::Value = serde_json::json!("chemistry");
        let pt_value: serde_json::Value = serde_json::json!([
            {"tag": "nsfw_boost", "text": "be daring"}
        ]);
        meta_map.insert("memory_scope_raw".into(), serde_json::to_value(ms).unwrap());
        meta_map.insert("affinity_scope_raw".into(), asd_value);
        meta_map.insert("prompt_traits_raw".into(), pt_value);
        let persisted = serde_json::Value::Object(meta_map);

        let chat_repo = ChatRepo { pool: &pool };
        let user_id = uuid::Uuid::new_v4();
        let session = chat_repo
            .create_session(user_id, seed_persona_instance(&pool, user_id).await)
            .await
            .unwrap();
        chat_repo
            .upsert_user_message_idempotent(
                session.id,
                "hi",
                "01J0000000000000000070001A",
                "user",
                Some(&persisted),
            )
            .await
            .unwrap();

        let stored: serde_json::Value =
            sqlx::query_scalar("SELECT metadata FROM engine.chat_messages WHERE session_id = $1")
                .bind(session.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            stored["memory_scope_raw"],
            serde_json::json!("neutral_only")
        );
        assert_eq!(stored["affinity_scope_raw"], serde_json::json!("chemistry"));
        assert_eq!(stored["prompt_traits_raw"][0]["tag"], "nsfw_boost");
        assert_eq!(stored["prompt_traits_raw"][0]["text"], "be daring");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn resolve_quote_carries_content_and_scopes_by_session(pool: PgPool) {
        use eros_engine_store::chat::ChatRepo;
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('S', 'p', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let mut sessions = Vec::new();
        for _ in 0..2 {
            sessions.push(
                sqlx::query_scalar::<_, Uuid>(
                    "INSERT INTO engine.chat_sessions (user_id, instance_id) \
                     VALUES ($1, $2) RETURNING id",
                )
                .bind(user_id)
                .bind(instance_id)
                .fetch_one(&pool)
                .await
                .unwrap(),
            );
        }
        let chat_repo = ChatRepo { pool: &pool };
        let msg = chat_repo
            .append_message(sessions[0], "assistant", "那我们礼拜六去看展")
            .await
            .unwrap();

        // No id at all → no quote, and no DB round-trip to make one.
        assert!(resolve_quote(&chat_repo, sessions[0], None)
            .await
            .unwrap()
            .is_none());

        // Resolvable → the whole line, not just its id: the prompt renders the
        // text, so resolution has to fetch it here.
        let q = resolve_quote(&chat_repo, sessions[0], Some(msg))
            .await
            .unwrap()
            .expect("in-session id resolves");
        assert_eq!(q.message_id, msg);
        assert_eq!(q.role, "assistant");
        assert_eq!(q.content, "那我们礼拜六去看展");

        // Real id, wrong session → None (never leaks another session's text).
        assert!(resolve_quote(&chat_repo, sessions[1], Some(msg))
            .await
            .unwrap()
            .is_none());
        // Unknown id → None, not an error.
        assert!(resolve_quote(&chat_repo, sessions[0], Some(Uuid::new_v4()))
            .await
            .unwrap()
            .is_none());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn stream_records_reply_to_message_id_in_metadata(pool: PgPool) {
        use eros_engine_store::chat::ChatRepo;
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('S', 'p', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // An earlier message to quote.
        let chat_repo = ChatRepo { pool: &pool };
        let quoted = chat_repo
            .append_message(session_id, "assistant", "earlier line")
            .await
            .unwrap();

        let state = crate::routes::companion::test_state(pool.clone());
        let mut app = build_router(state);
        let token = mint_jwt(user_id);

        // (a) valid quote → metadata.reply_to_message_id set, 200.
        let body = serde_json::to_vec(&json!({
            "content": "replying", "client_msg_id": "01J7777777777777777777777A",
            "reply_to_message_id": quoted.to_string()
        }))
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri(format!("/comp/chat/{session_id}/message/stream"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let meta: Value = sqlx::query_scalar(
            "SELECT metadata FROM engine.chat_messages WHERE client_msg_id = '01J7777777777777777777777A'",
        ).fetch_one(&pool).await.unwrap();
        assert_eq!(meta["reply_to_message_id"], json!(quoted.to_string()));
        assert!(meta.get("reply_to_error").is_none());

        // (b) invalid quote (random id) → metadata.reply_to_error, still 200.
        let body = serde_json::to_vec(&json!({
            "content": "replying2", "client_msg_id": "01J8888888888888888888888A",
            "reply_to_message_id": Uuid::new_v4().to_string()
        }))
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri(format!("/comp/chat/{session_id}/message/stream"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let meta: Value = sqlx::query_scalar(
            "SELECT metadata FROM engine.chat_messages WHERE client_msg_id = '01J8888888888888888888888A'",
        ).fetch_one(&pool).await.unwrap();
        assert_eq!(meta["reply_to_error"], json!("not_found"));
        assert!(meta.get("reply_to_message_id").is_none());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn user_row_omits_scope_raw_keys_when_request_fields_are_none(pool: sqlx::PgPool) {
        use eros_engine_store::chat::ChatRepo;
        let chat_repo = ChatRepo { pool: &pool };
        let user_id = uuid::Uuid::new_v4();
        let session = chat_repo
            .create_session(user_id, seed_persona_instance(&pool, user_id).await)
            .await
            .unwrap();
        // None of the three optional fields present, no tip, no tier → meta_map
        // empty → metadata = None.
        chat_repo
            .upsert_user_message_idempotent(
                session.id,
                "hi",
                "01J0000000000000000070002A",
                "user",
                None,
            )
            .await
            .unwrap();

        let stored: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT metadata FROM engine.chat_messages WHERE session_id = $1")
                .bind(session.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            stored.is_none(),
            "metadata must be NULL when no fields present"
        );
    }
}

#[cfg(test)]
mod validate_payload_tests {
    use super::*;

    fn base() -> StreamSendRequest {
        StreamSendRequest {
            content: "hi".into(),
            client_msg_id: "01J0000000000000000000000A".into(),
            prompt_traits: None,
            audit: None,
            tier: None,
            memory_scope: None,
            affinity_scope: None,
            tips_amount_usd: None,
            image_url: None,
            image: None,
            reply_to_message_id: None,
        }
    }

    #[test]
    fn empty_content_ok_with_image() {
        let mut r = base();
        r.content = String::new();
        r.image_url = Some("https://x/y.png".into());
        assert!(validate_payload(&r).is_ok());
    }

    #[test]
    fn empty_content_rejected_without_image_or_tip() {
        let mut r = base();
        r.content = String::new();
        assert!(validate_payload(&r).is_err());
    }

    #[test]
    fn bad_image_url_scheme_rejected() {
        let mut r = base();
        r.image_url = Some("ftp://x/y.png".into());
        assert!(validate_payload(&r).is_err());
    }

    #[test]
    fn good_https_image_url_ok() {
        let mut r = base();
        r.image_url = Some("https://x/y.png".into());
        assert!(validate_payload(&r).is_ok());
    }

    #[test]
    fn http_scheme_image_url_ok() {
        let mut r = base();
        r.image_url = Some("http://x/y.png".into());
        assert!(validate_payload(&r).is_ok());
    }

    #[test]
    fn over_length_image_url_rejected() {
        let mut r = base();
        // 2048 is the max; build a URL longer than that.
        let long = format!("https://x/{}", "a".repeat(MAX_IMAGE_URL_LEN));
        assert!(long.len() > MAX_IMAGE_URL_LEN);
        r.image_url = Some(long);
        assert!(validate_payload(&r).is_err());
    }

    #[test]
    fn image_url_no_host_rejected() {
        let mut r = base();
        r.content = String::new();
        r.image_url = Some("https://".into());
        assert!(validate_payload(&r).is_err());
    }

    #[test]
    fn image_url_whitespace_host_rejected() {
        let mut r = base();
        r.image_url = Some("https:// example.com/y.png".into());
        assert!(validate_payload(&r).is_err());
    }

    #[test]
    fn image_url_is_valid_unit() {
        assert!(image_url_is_valid("https://x/y.png"));
        assert!(image_url_is_valid("http://example.com/a.jpg"));
        assert!(!image_url_is_valid("https://"));
        assert!(!image_url_is_valid("http:// "));
        assert!(!image_url_is_valid("ftp://x/y.png"));
        assert!(!image_url_is_valid(""));
        assert!(!image_url_is_valid("https://example.com/a b.png"));
    }

    #[test]
    fn image_url_with_space_in_path_rejected() {
        let mut r = base();
        r.image_url = Some("https://example.com/a b.png".into());
        assert!(validate_payload(&r).is_err());
    }

    #[test]
    fn tip_plus_image_rejected() {
        let mut r = base();
        r.tips_amount_usd = Some(1.0);
        r.image_url = Some("https://x/y.png".into());
        assert!(validate_payload(&r).is_err());
    }

    fn minimal_req() -> StreamSendRequest {
        StreamSendRequest {
            content: "hi".into(),
            client_msg_id: "01J0000000000000000000000A".into(),
            prompt_traits: None,
            audit: None,
            tier: None,
            memory_scope: None,
            affinity_scope: None,
            tips_amount_usd: None,
            image_url: None,
            image: None,
            reply_to_message_id: None,
        }
    }

    #[test]
    fn validate_rejects_force_image_with_tip() {
        let mut req = minimal_req();
        req.tips_amount_usd = Some(5.0);
        req.image = Some(ImageReplyParams {
            force: true,
            ..Default::default()
        });
        assert!(validate_payload(&req).is_err());
    }

    #[test]
    fn image_capability_rejects_force_without_composer() {
        let mut req = minimal_req();
        req.image = Some(ImageReplyParams {
            force: true,
            ..Default::default()
        });
        let cfg = eros_engine_llm::model_config::ModelConfig::default();
        assert!(validate_image_capability(&req, &cfg).is_err());
    }

    #[test]
    fn image_capability_allows_force_with_composer() {
        let mut req = minimal_req();
        req.image = Some(ImageReplyParams {
            force: true,
            ..Default::default()
        });
        let cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(
            "[tasks.chat_image_prompt_compose]\nmodel = \"composer\"\n",
        )
        .unwrap();
        assert!(validate_image_capability(&req, &cfg).is_ok());
    }

    #[test]
    fn image_capability_ignores_unforced_image_block() {
        let mut req = minimal_req();
        req.image = Some(ImageReplyParams::default());
        let cfg = eros_engine_llm::model_config::ModelConfig::default();
        assert!(validate_image_capability(&req, &cfg).is_ok());
    }

    #[test]
    fn validate_rejects_forced_image_with_empty_content() {
        // spec 2026-08-03 §2.3: the force+image_only empty-content exemption is
        // gone — content follows the ordinary non-empty rule on forced turns.
        let mut req = minimal_req();
        req.content = String::new();
        req.image = Some(ImageReplyParams {
            force: true,
            ..Default::default()
        });
        assert!(validate_payload(&req).is_err());
    }

    #[test]
    fn leftover_mode_key_deserializes_and_is_ignored() {
        // Consumers may still send the deleted `mode` field; no
        // deny_unknown_fields, so it must parse and change nothing (spec §1).
        let p: ImageReplyParams =
            serde_json::from_str(r#"{"force":true,"mode":"image_only"}"#).unwrap();
        assert!(p.force);
    }

    #[test]
    fn validate_rejects_bad_aspect() {
        let mut req = minimal_req();
        req.image = Some(ImageReplyParams {
            aspect_ratio: Some("2:5".into()),
            ..Default::default()
        });
        assert!(validate_payload(&req).is_err());
    }
}
