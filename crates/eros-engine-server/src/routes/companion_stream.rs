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

use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
use eros_engine_store::persona::PersonaRepo;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, StreamPreError};
use crate::pipeline::stream::{run_stream, PersistedUserMessage, ProtocolFrame};
use crate::routes::companion::enforce_nft_ownership;
use crate::state::AppState;

const MAX_CONTENT_CHARS: usize = 4096;
const MIN_CLIENT_MSG_ID_LEN: usize = 26;
const MAX_CLIENT_MSG_ID_LEN: usize = 36;
const CONCURRENT_STREAMS_PER_USER: u32 = 3;
const IDEMPOTENCY_WINDOW_HOURS: i64 = 24;
const SSE_KEEPALIVE_SECS: u64 = 15;

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct StreamSendRequest {
    pub content: String,
    pub client_msg_id: String,
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

fn validate_payload(req: &StreamSendRequest) -> Result<(), AppError> {
    if req.content.is_empty() {
        return Err(AppError::StreamPre(StreamPreError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "unprocessable",
            message: "content must not be empty".into(),
            user_message: "请输入一条消息".into(),
            original_user_message_id: None,
        }));
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
    Json(req): Json<StreamSendRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    // Validate payload first — before any DB call — so 422/400 never waste a roundtrip.
    validate_payload(&req)?;

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
    let persona_repo = PersonaRepo { pool: &state.pool };
    let companion = persona_repo
        .load_companion(instance_id)
        .await?
        .ok_or_else(|| AppError::NotFound("instance not found".into()))?;
    let asset_id_opt = persona_repo
        .get_asset_id_for_genome(companion.instance.genome_id)
        .await?;
    enforce_nft_ownership(&state.pool, user_id, asset_id_opt.as_deref()).await?;

    // Acquire a stream slot. `StreamSlotGuard` is now `'static` (holds Arc),
    // so it can be moved into the SSE body below.
    let guard = state
        .stream_slots
        .try_acquire(user_id, CONCURRENT_STREAMS_PER_USER)
        .ok_or_else(|| {
            AppError::StreamPre(StreamPreError {
                status: StatusCode::TOO_MANY_REQUESTS,
                code: "rate_limited",
                message: format!(
                    "per-user stream cap reached ({CONCURRENT_STREAMS_PER_USER})"
                ),
                user_message: "请求过多，请稍后再试".into(),
                original_user_message_id: None,
            })
        })?;

    let outcome = chat_repo
        .upsert_user_message_idempotent(
            session_id,
            &req.content,
            &req.client_msg_id,
            IDEMPOTENCY_WINDOW_HOURS,
        )
        .await?;
    let user_message_id = match outcome {
        UpsertUserOutcome::Inserted { message_id } => message_id,
        UpsertUserOutcome::DuplicateInProgress { user_message_id } => {
            return Err(AppError::StreamPre(StreamPreError {
                status: StatusCode::CONFLICT,
                code: "duplicate_in_progress",
                message: "same (session_id, client_msg_id) is still generating".into(),
                user_message: "上一条消息正在处理中，请稍候".into(),
                original_user_message_id: Some(user_message_id),
            }));
        }
        UpsertUserOutcome::Replay { .. } => {
            // T15 implements the replay path. Until then, signal 409 so we
            // don't double-bill OpenRouter for repeat requests.
            return Err(AppError::StreamPre(StreamPreError {
                status: StatusCode::CONFLICT,
                code: "duplicate_in_progress",
                message: "replay path not yet implemented (T15)".into(),
                user_message: "请稍后重试".into(),
                original_user_message_id: None,
            }));
        }
    };

    let state_arc = Arc::new(state.clone());
    let user_msg = PersistedUserMessage {
        user_message_id,
        session_id,
        user_id,
        instance_id,
        content: req.content.clone(),
    };

    // Move the StreamSlotGuard into the stream so it is released only when
    // the response body finishes. `StreamSlotGuard` holds `Arc<StreamSlots>`
    // so it satisfies the `'static` bound required by axum's `Sse`.
    let proto = run_stream(state_arc, user_msg);
    let sse = futures_util::StreamExt::map(
        async_stream::stream! {
            let _guard = guard;
            futures_util::pin_mut!(proto);
            while let Some(frame) = futures_util::StreamExt::next(&mut proto).await {
                yield frame;
            }
        },
        |frame: ProtocolFrame| {
            let json = serde_json::to_string(&frame)
                .expect("ProtocolFrame serialization is infallible");
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
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request};
    use axum::Router;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::{json, Value};
    use sqlx::PgPool;
    use tower::Service;

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
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata, is_active) \
             VALUES ('S', 'p', '{}'::jsonb, true) RETURNING id",
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
        .bind(user_id).bind(instance_id).fetch_one(&pool).await.unwrap();

        let state = crate::routes::companion::test_state(pool);
        let mut app = build_router(state);
        let token = mint_jwt(user_id);
        let body = serde_json::to_vec(&json!({"content":"","client_msg_id":"01J2222222222222222222222A"})).unwrap();
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
}
