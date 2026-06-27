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
    fn resolve(&self) -> AffinityScope {
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

#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImageMode {
    #[default]
    TextImage,
    ImageOnly,
}

#[derive(Debug, Clone, Default, Deserialize, utoipa::ToSchema)]
pub struct ImageReplyParams {
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub mode: ImageMode,
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub style: Option<StyleKey>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub image_prompt: Option<String>,
    #[serde(default)]
    pub aspect_ratio: Option<String>,
    #[serde(default)]
    pub resolution: Option<String>,
    #[serde(default)]
    pub face_ref_url: Option<String>,
    /// Optional URL of the previously generated image, for iteration. Selected
    /// when the PDE chooses `image_ref = previous`. Same validation as
    /// `face_ref_url`; the engine never fetches it — it is embedded in the
    /// OpenRouter body and fetched by the image provider at generation time, so
    /// clients backed by a private store should pass a short-lived signed URL.
    #[serde(default)]
    pub prev_image_url: Option<String>,
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
    // Content may be empty only when a tip, an image_url, or a forced ImageOnly turn is attached.
    let image_only = req
        .image
        .as_ref()
        .is_some_and(|i| i.force && i.mode == ImageMode::ImageOnly);
    if req.content.is_empty()
        && req.tips_amount_usd.is_none()
        && req.image_url.is_none()
        && !image_only
    {
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
        if let Some(url) = img.face_ref_url.as_deref() {
            if !image_url_is_valid(url) {
                return Err(AppError::StreamPre(StreamPreError {
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                    code: "unprocessable",
                    message: "face_ref_url must be an absolute http(s) URL".into(),
                    user_message: "脸部参考图链接无效".into(),
                    original_user_message_id: None,
                }));
            }
        }
        if let Some(url) = img.prev_image_url.as_deref() {
            if !image_url_is_valid(url) {
                return Err(AppError::StreamPre(StreamPreError {
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                    code: "unprocessable",
                    message: "prev_image_url must be an absolute http(s) URL".into(),
                    user_message: "上一张图片链接无效".into(),
                    original_user_message_id: None,
                }));
            }
        }
        if let Some(ar) = img.aspect_ratio.as_deref() {
            if !matches!(ar, "1:1" | "3:4" | "4:3" | "9:16" | "16:9") {
                return Err(AppError::StreamPre(StreamPreError {
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                    code: "unprocessable",
                    message: "unsupported aspect_ratio".into(),
                    user_message: "不支持的画幅比例".into(),
                    original_user_message_id: None,
                }));
            }
        }
        if let Some(res) = img.resolution.as_deref() {
            let ok = res.len() <= 16
                && res.split_once('x').is_some_and(|(w, h)| {
                    !w.is_empty()
                        && !h.is_empty()
                        && w.bytes().all(|b| b.is_ascii_digit())
                        && h.bytes().all(|b| b.is_ascii_digit())
                });
            if !ok {
                return Err(AppError::StreamPre(StreamPreError {
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                    code: "unprocessable",
                    message: "resolution must look like WxH".into(),
                    user_message: "分辨率格式无效".into(),
                    original_user_message_id: None,
                }));
            }
        }
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
        (status = 200, description = "SSE event stream (text/event-stream)", content_type = "text/event-stream"),
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
    let persona_repo = PersonaRepo { pool: &state.pool };
    persona_repo
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
                };
                Box::pin(run_stream(state_arc, user_msg))
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

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SetImageUrlRequest {
    pub url: String,
}

/// Write back a generated image URL to an existing assistant message.
#[utoipa::path(
    post,
    path = "/comp/chat/{session_id}/message/{message_id}/image",
    tag = "companion",
    params(
        ("session_id" = Uuid, Path, description = "Chat session id"),
        ("message_id" = Uuid, Path, description = "Assistant message id"),
    ),
    request_body = SetImageUrlRequest,
    responses(
        (status = 204, description = "stored"),
        (status = 403, body = StreamPreErrorBody),
        (status = 404, body = StreamPreErrorBody),
        (status = 422, body = StreamPreErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn set_generated_image_url(
    State(state): State<AppState>,
    Path((session_id, message_id)): Path<(Uuid, Uuid)>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<SetImageUrlRequest>,
) -> Result<StatusCode, AppError> {
    if !image_url_is_valid(&req.url) {
        return Err(AppError::StreamPre(StreamPreError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "unprocessable",
            message: "url must be an absolute http(s) URL".into(),
            user_message: "图片链接无效".into(),
            original_user_message_id: None,
        }));
    }
    let chat_repo = ChatRepo { pool: &state.pool };
    // Ownership gate — mirrors send_message_stream exactly.
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
    let rows = chat_repo
        .set_assistant_image_url(session_id, message_id, &req.url)
        .await?;
    if rows == 0 {
        return Err(AppError::StreamPre(StreamPreError {
            status: StatusCode::NOT_FOUND,
            code: "message_not_found",
            message: "assistant message not found in session".into(),
            user_message: "消息不存在".into(),
            original_user_message_id: None,
        }));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(send_message_stream))
        .routes(routes!(set_generated_image_url))
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let session = chat_repo
            .create_session(uuid::Uuid::new_v4(), uuid::Uuid::new_v4())
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
    async fn user_row_omits_scope_raw_keys_when_request_fields_are_none(pool: sqlx::PgPool) {
        use eros_engine_store::chat::ChatRepo;
        let chat_repo = ChatRepo { pool: &pool };
        let session = chat_repo
            .create_session(uuid::Uuid::new_v4(), uuid::Uuid::new_v4())
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

    // --- SetImageUrlRequest / writeback validation ---

    #[test]
    fn writeback_rejects_bad_url() {
        assert!(!image_url_is_valid("not-a-url"));
        assert!(image_url_is_valid("https://cdn.example/x.png"));
    }

    #[test]
    fn set_image_url_request_deserializes() {
        let v: SetImageUrlRequest =
            serde_json::from_str(r#"{"url":"https://cdn.example/x.png"}"#).unwrap();
        assert_eq!(v.url, "https://cdn.example/x.png");
    }

    #[test]
    fn set_image_url_request_rejects_missing_url_field() {
        // url is required (no default); missing field → deserialization error.
        assert!(serde_json::from_str::<SetImageUrlRequest>(r#"{}"#).is_err());
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
    fn validate_allows_image_only_empty_content() {
        let mut req = minimal_req();
        req.content = String::new();
        req.image = Some(ImageReplyParams {
            force: true,
            mode: ImageMode::ImageOnly,
            ..Default::default()
        });
        assert!(validate_payload(&req).is_ok());
    }

    #[test]
    fn validate_rejects_bad_face_ref_and_aspect() {
        let mut req = minimal_req();
        req.image = Some(ImageReplyParams {
            face_ref_url: Some("ftp://x".into()),
            ..Default::default()
        });
        assert!(validate_payload(&req).is_err());
        let mut req2 = minimal_req();
        req2.image = Some(ImageReplyParams {
            aspect_ratio: Some("2:5".into()),
            ..Default::default()
        });
        assert!(validate_payload(&req2).is_err());
    }

    #[test]
    fn validate_rejects_bad_prev_image_url() {
        let mut req = minimal_req();
        req.image = Some(ImageReplyParams {
            prev_image_url: Some("ftp://nope".into()),
            ..Default::default()
        });
        assert!(validate_payload(&req).is_err());

        // a valid absolute https URL is accepted
        let mut ok = minimal_req();
        ok.image = Some(ImageReplyParams {
            prev_image_url: Some("https://example.test/a.png".into()),
            ..Default::default()
        });
        assert!(validate_payload(&ok).is_ok());
    }
}
