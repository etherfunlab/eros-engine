// SPDX-License-Identifier: AGPL-3.0-only
//! POST /comp/voice/{session_id}/turn/stream — lean voice-channel turn.
//!
//! Spec: docs/superpowers/specs/2026-07-07-voice-call-parts-design.md

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures_util::Stream;
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_store::chat::{ChatRepo, VoiceUserInsert};

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, StreamPreError};
use crate::pipeline::stream::ProtocolFrame;
use crate::pipeline::voice::{run_voice_turn, VoiceTurn};
use crate::state::AppState;

const MAX_CONTENT_CHARS: usize = 4096;
const MIN_CLIENT_MSG_ID_LEN: usize = 26;
const MAX_CLIENT_MSG_ID_LEN: usize = 36;
const CONCURRENT_STREAMS_PER_USER: u32 = 3;
const SSE_KEEPALIVE_SECS: u64 = 15;

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct VoiceTurnRequest {
    pub content: String,
    pub client_msg_id: String,
}

fn pre(status: StatusCode, code: &'static str, message: &str, user_message: &str) -> AppError {
    AppError::StreamPre(StreamPreError {
        status,
        code,
        message: message.into(),
        user_message: user_message.into(),
        original_user_message_id: None,
    })
}

fn validate(req: &VoiceTurnRequest) -> Result<(), AppError> {
    if req.content.is_empty() {
        return Err(pre(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unprocessable",
            "content must not be empty",
            "请输入一条消息",
        ));
    }
    if req.content.chars().count() > MAX_CONTENT_CHARS {
        return Err(pre(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unprocessable",
            "content too long",
            "消息过长，请缩短后重试",
        ));
    }
    let n = req.client_msg_id.len();
    let bad_chars = req
        .client_msg_id
        .chars()
        .any(|c| c.is_whitespace() || !c.is_ascii() || !c.is_ascii_graphic());
    if !(MIN_CLIENT_MSG_ID_LEN..=MAX_CLIENT_MSG_ID_LEN).contains(&n) || bad_chars {
        return Err(pre(
            StatusCode::BAD_REQUEST,
            "invalid_payload",
            "client_msg_id invalid",
            "请求无效",
        ));
    }
    Ok(())
}

#[utoipa::path(
    post,
    path = "/comp/voice/{session_id}/turn/stream",
    tag = "companion",
    params(("session_id" = Uuid, Path, description = "Chat session id")),
    request_body = VoiceTurnRequest,
    responses(
        (status = 200, description = "SSE event stream (text/event-stream)", content_type = "text/event-stream"),
        (status = 400, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 404, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 422, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 429, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 501, body = crate::routes::companion_stream::StreamPreErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn voice_turn_stream(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<VoiceTurnRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    validate(&req)?;

    let chat_repo = ChatRepo { pool: &state.pool };
    let session = chat_repo.get_session(session_id).await?.ok_or_else(|| {
        pre(
            StatusCode::NOT_FOUND,
            "session_not_found",
            "session not found",
            "会话不存在",
        )
    })?;
    if session.user_id != user_id {
        return Err(pre(
            StatusCode::FORBIDDEN,
            "session_forbidden",
            "session not owned by JWT user",
            "无权访问该会话",
        ));
    }
    let instance_id = session.instance_id.ok_or_else(|| {
        pre(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "session has no instance_id",
            "服务出现问题，请稍后再试",
        )
    })?;

    // Voice is opt-in: no [tasks.chat_voice] ⇒ 501.
    let resolved = state.model_config.resolve_voice().ok_or_else(|| {
        pre(
            StatusCode::NOT_IMPLEMENTED,
            "voice_disabled",
            "voice is not configured on this deployment",
            "该服务未启用语音",
        )
    })?;

    let guard = state
        .stream_slots
        .try_acquire(user_id, CONCURRENT_STREAMS_PER_USER)
        .ok_or_else(|| {
            pre(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "per-user stream cap reached",
                "请求过多，请稍后再试",
            )
        })?;

    // Persist the user turn (idempotent on (session_id, client_msg_id)). A
    // duplicate client_msg_id must NOT trigger a second assistant reply /
    // second upstream LLM call — return 409 instead of regenerating.
    let user_message_id = match chat_repo
        .insert_voice_user_message(session_id, &req.content, &req.client_msg_id)
        .await?
    {
        VoiceUserInsert::Inserted(id) => id,
        VoiceUserInsert::Duplicate(_) => {
            return Err(pre(
                StatusCode::CONFLICT,
                "duplicate",
                "duplicate client_msg_id for this session",
                "这条消息已在处理",
            ));
        }
    };

    let turn = VoiceTurn {
        session_id,
        instance_id,
        user_message_id,
    };
    let proto = run_voice_turn(Arc::new(state.clone()), turn, resolved);

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
    OpenApiRouter::new().routes(routes!(voice_turn_stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request};
    use axum::Router;
    use eros_engine_llm::model_config::ModelConfig;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;
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

    async fn seed(pool: &PgPool, user_id: Uuid) -> Uuid {
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('V','p','{}'::jsonb) RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1,$2) RETURNING id",
        ).bind(genome_id).bind(user_id).fetch_one(pool).await.unwrap();
        sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1,$2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn with_voice(mut state: AppState) -> AppState {
        state.model_config = Arc::new(
            ModelConfig::from_toml_str("[tasks.chat_voice]\nmodel = \"primary\"\n").unwrap(),
        );
        state
    }

    async fn post_voice(
        app: &mut Router,
        session_id: Uuid,
        jwt: &str,
        body: serde_json::Value,
    ) -> axum::http::Response<Body> {
        let req = Request::builder()
            .method("POST")
            .uri(format!("/comp/voice/{session_id}/turn/stream"))
            .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        app.call(req).await.unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn voice_422_when_content_empty(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = seed(&pool, user_id).await;
        let mut app = build_router(with_voice(crate::routes::companion::test_state(pool)));
        let jwt = mint_jwt(user_id);
        let resp = post_voice(
            &mut app,
            session_id,
            &jwt,
            json!({"content":"","client_msg_id":"01J2222222222222222222222A"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn voice_501_when_task_absent(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = seed(&pool, user_id).await;
        // Default test_state has no chat_voice task.
        let mut app = build_router(crate::routes::companion::test_state(pool));
        let jwt = mint_jwt(user_id);
        let resp = post_voice(
            &mut app,
            session_id,
            &jwt,
            json!({"content":"hi","client_msg_id":"01J2222222222222222222222A"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn voice_403_when_not_owner(pool: PgPool) {
        let owner = Uuid::new_v4();
        let session_id = seed(&pool, owner).await;
        let mut app = build_router(with_voice(crate::routes::companion::test_state(pool)));
        let other = mint_jwt(Uuid::new_v4());
        let resp = post_voice(
            &mut app,
            session_id,
            &other,
            json!({"content":"hi","client_msg_id":"01J2222222222222222222222A"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn voice_409_on_duplicate_client_msg_id(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = seed(&pool, user_id).await;
        let client_msg_id = "01J2222222222222222222222A";

        // Pre-seed a user voice row with this client_msg_id, as if a first
        // request already landed it.
        let chat_repo = ChatRepo { pool: &pool };
        match chat_repo
            .insert_voice_user_message(session_id, "hi", client_msg_id)
            .await
            .unwrap()
        {
            VoiceUserInsert::Inserted(_) => {}
            other => panic!("expected Inserted, got {other:?}"),
        }

        let mut app = build_router(with_voice(crate::routes::companion::test_state(pool)));
        let jwt = mint_jwt(user_id);
        // No mock OpenRouter configured — the 409 must happen before any
        // generation is attempted.
        let resp = post_voice(
            &mut app,
            session_id,
            &jwt,
            json!({"content":"hi","client_msg_id": client_msg_id}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }
}
