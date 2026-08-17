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

use eros_engine_core::scope::{AffinityAxis, AffinityScope, MemoryScope};
use eros_engine_llm::model_config::StyleKey;
use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
use eros_engine_store::persona::PersonaRepo;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, StreamPreError};
use crate::pipeline::stream::{replay_stream, run_stream, PersistedUserMessage, ProtocolFrame};
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

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AffinityScopeName {
    Full,
    BondAndChemistry,
    Bond,
    Chemistry,
    None,
}

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
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
#[derive(Debug, Clone, Default, Deserialize, utoipa::ToSchema)]
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
    /// Optional id of a `chat_messages` row in this session to anchor context on.
    /// When valid, history rewinds to (and includes) that message; when present
    /// but unresolvable, history is dropped for this turn (recorded in metadata).
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

fn validate_payload(req: &StreamSendRequest) -> Result<(), AppError> {
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
fn validate_image_capability(
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
    Json(mut req): Json<StreamSendRequest>,
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
    let audit = validate_llm_audit(req.audit.take()).map_err(|e| {
        AppError::StreamPre(StreamPreError {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_payload",
            message: e.to_string(),
            user_message: "请求无效".into(),
            original_user_message_id: None,
        })
    })?;

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
    // The loaded persona is threaded into `run_stream` below so the turn does
    // not re-issue the same `load_companion` query inside the generator.
    let persona_repo = PersonaRepo { pool: &state.pool };
    let persona = persona_repo
        .load_companion(instance_id)
        .await?
        .ok_or_else(|| AppError::NotFound("instance not found".into()))?;
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

    // Resolve the optional reply anchor BEFORE the upsert so the outcome can be
    // recorded in the new row's metadata. A present-but-unresolvable id does not
    // fail the request — we drop history for this turn and flag it (DropHistory).
    use eros_engine_core::types::HistoryAnchor;
    let history_anchor = match req.reply_to_message_id {
        None => HistoryAnchor::Latest,
        Some(id) => match chat_repo.message_sent_at_in_session(session_id, id).await? {
            Some(sent_at) => HistoryAnchor::At {
                message_id: id,
                sent_at,
            },
            None => HistoryAnchor::DropHistory,
        },
    };

    // Build metadata: conditionally include tips_amount_usd, tier, and image_url.
    // tier is omitted entirely (not written as null) when absent — keeps JSONB shape sparse.
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
    match history_anchor {
        HistoryAnchor::At { message_id, .. } => {
            meta_map.insert("reply_to_message_id".into(), serde_json::json!(message_id));
        }
        HistoryAnchor::DropHistory => {
            meta_map.insert("reply_to_error".into(), serde_json::json!("not_found"));
        }
        HistoryAnchor::Latest => {}
    }
    let persisted_metadata: Option<serde_json::Value> = if meta_map.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(meta_map))
    };

    let (persisted_content, persisted_role) = match req.tips_amount_usd {
        Some(amount) if req.content.is_empty() => (
            format!("(打赏 ${})", crate::prompt::fmt_amount(amount)),
            "gift_user",
        ),
        Some(_) => (req.content.clone(), "gift_user"),
        None => (req.content.clone(), "user"),
    };
    let outcome = chat_repo
        .upsert_user_message_idempotent(
            session_id,
            &persisted_content,
            &req.client_msg_id,
            persisted_role,
            persisted_metadata.as_ref(),
        )
        .await?;
    // Resolve the proto stream. Replay returns early with a boxed stream;
    // Inserted continues to run_stream below. Boxing erases the concrete type
    // so both branches can feed the same SSE wrapper.
    let proto: std::pin::Pin<Box<dyn futures_util::Stream<Item = ProtocolFrame> + Send>> =
        match outcome {
            UpsertUserOutcome::Inserted { message_id } => {
                let state_arc = Arc::new(state.clone());
                let memory_scope = req.memory_scope.unwrap_or_default();
                let affinity_scope = req
                    .affinity_scope
                    .as_ref()
                    .map(AffinityScopeDto::resolve)
                    .unwrap_or_default();
                let user_msg = PersistedUserMessage {
                    user_message_id: message_id,
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
                    history_anchor,
                };
                Box::pin(run_stream(state_arc, user_msg, Some(persona)))
            }
            UpsertUserOutcome::DuplicateInProgress { user_message_id } => {
                return Err(AppError::StreamPre(StreamPreError {
                    status: StatusCode::CONFLICT,
                    code: "duplicate_in_progress",
                    message: "same (session_id, client_msg_id) is still generating".into(),
                    user_message: "上一条消息正在处理中，请稍候".into(),
                    original_user_message_id: Some(user_message_id),
                }));
            }
            UpsertUserOutcome::Replay {
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
                    model: Some("primary".into()),
                    usage: Some(serde_json::json!({"prompt_tokens":1,"completion_tokens":2,"total_tokens":3})),
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
