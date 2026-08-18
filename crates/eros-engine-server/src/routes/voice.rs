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

use ulid::Ulid;

use eros_engine_core::scope::MemoryScope;
use eros_engine_store::chat::{ChatRepo, LatestTurnLookup, VoiceUserInsert};
use eros_engine_store::persona::PersonaRepo;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, StreamPreError};
use crate::pipeline::stream::{ulid_string, ProtocolFrame};
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
    /// Which affinity halves feed this turn's relationship line — same field
    /// name, value space, and default (`bond`) as the chat message stream:
    /// `"full" | "bond_and_chemistry" | "bond" | "chemistry" | "none"`, or an
    /// array of axis names such as `["warmth", "trust"]`. Axes flatten to the
    /// two injectable halves (any bond axis ⇒ bond half, any chemistry axis ⇒
    /// chemistry half).
    #[serde(default)]
    pub affinity_scope: Option<crate::routes::companion_stream::AffinityScopeDto>,
    /// How much long-term memory this turn may use — same field name, enum,
    /// wire values, and default (`neutral_and_relationship`) as the chat
    /// stream. The first successfully-assembling turn's resolved insight
    /// tier is frozen into the bootstrap snapshot and never changes for the
    /// rest of the call.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub memory_scope: Option<MemoryScope>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct VoiceInterruptRequest {
    /// The `client_msg_id` of the turn being interrupted. MUST be the
    /// session's latest user turn — you can only barge in on what is
    /// currently being spoken.
    pub client_msg_id: String,
    /// What TTS actually played, verbatim. MAY be empty (the user cut in
    /// before the first audible word), in which case no assistant content is
    /// written and only the interrupt marker records the turn.
    pub spoken_text: String,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct VoiceInterruptResponse {
    /// The assistant row that now holds the spoken text, or `null` when
    /// nothing was played and no reply row exists.
    pub message_id: Option<String>,
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

fn validate_interrupt(req: &VoiceInterruptRequest) -> Result<(), AppError> {
    if req.spoken_text.chars().count() > MAX_CONTENT_CHARS {
        return Err(pre(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unprocessable",
            "spoken_text too long",
            "请求无效",
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
        (status = 200, description = "SSE event stream (text/event-stream). Lean frame set: \
            `delta`* then a terminal `done` (or a single `error`). Unlike the chat message \
            stream, the voice turn emits no `meta` frame and no `action_type`.", content_type = "text/event-stream"),
        (status = 400, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 404, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 409, body = crate::routes::companion_stream::StreamPreErrorBody),
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
    // Channel-scoped enforcement: the voice endpoint writes `channel = 'voice'`
    // messages, so it must only operate on a voice-channel session. Reject a
    // text (or otherwise non-voice) session here — before persisting any turn —
    // so voice messages never leak into a text conversation's history. Voice
    // clients obtain a voice session via `chat/start` with `channel: "voice"`.
    if session.channel != "voice" {
        return Err(pre(
            StatusCode::CONFLICT,
            "wrong_channel",
            "session is not a voice-channel session",
            "该会话不是语音会话",
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

    // Reject a session whose persona instance is missing/inactive BEFORE
    // acquiring a slot or persisting the user turn — mirrors the text stream so
    // a recoverable precondition failure never leaves an orphaned user row.
    let persona_repo = PersonaRepo { pool: &state.pool };
    persona_repo
        .load_companion(instance_id)
        .await?
        .ok_or_else(|| AppError::NotFound("instance not found".into()))?;

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

    // Resolution: the request field, else the chat-parity default.
    let affinity_scope = req
        .affinity_scope
        .as_ref()
        .map(|dto| dto.resolve())
        .unwrap_or_default(); // bond — chat parity
                              // Pre-resolve raw snapshot on the user row, mirroring the chat stream's
                              // sparse `_raw` keys.
    let mut raw = serde_json::Map::new();
    if let Some(ms) = req.memory_scope.as_ref() {
        raw.insert(
            "memory_scope_raw".into(),
            serde_json::to_value(ms).expect("MemoryScope serializes"),
        );
    }
    if let Some(asd) = req.affinity_scope.as_ref() {
        raw.insert(
            "affinity_scope_raw".into(),
            serde_json::to_value(asd).expect("AffinityScopeDto serializes"),
        );
    }
    let raw_metadata = (!raw.is_empty()).then_some(serde_json::Value::Object(raw));

    // Persist the user turn (idempotent on (session_id, client_msg_id)).
    // A duplicate is only a conflict when the turn actually PRODUCED something:
    // an existing reply (retrying would double-bill) or a deliberate barge-in
    // (nothing to repair). A duplicate with neither is an abnormal disconnect —
    // or an upstream failure, which already told the client `retryable: true` —
    // so it regenerates against the persisted user row.
    let (user_message_id, persisted_content) = match chat_repo
        .insert_voice_user_message(
            session_id,
            &req.content,
            &req.client_msg_id,
            raw_metadata.as_ref(),
        )
        .await?
    {
        VoiceUserInsert::Inserted(id) => (id, req.content),
        VoiceUserInsert::Duplicate(id) => {
            // `insert_voice_user_message`'s conflict lookup is deliberately
            // role-agnostic (see its doc comment) — the colliding row can be a
            // `gift_user` tip row, or even a text-channel `user` row reached
            // via `message/stream` against this same session id, since that
            // route has no channel guard. `voice_turn_repair_state` is scoped
            // to `role = 'user' AND channel = 'voice'` and returns `None` for
            // anything else, so such a collision falls back to the ordinary
            // unconditional 409 instead of being treated as a repairable
            // voice turn (which would regenerate against the wrong row).
            let st = chat_repo.voice_turn_repair_state(id).await?;
            let Some(st) = st else {
                return Err(pre(
                    StatusCode::CONFLICT,
                    "duplicate",
                    "duplicate client_msg_id for this session",
                    "这条消息已在处理",
                ));
            };
            if st.has_reply || st.interrupted {
                return Err(pre(
                    StatusCode::CONFLICT,
                    "duplicate",
                    "duplicate client_msg_id for this session",
                    "这条消息已在处理",
                ));
            }
            if st.content != req.content {
                // Deliberately omits the differing text — this repo forbids
                // logging chat content. Do not "improve" this by adding the
                // diff between `st.content` and `req.content`; that would leak
                // user utterances into the logs.
                tracing::warn!(
                    session_id = %session_id,
                    "voice: repair retry carried different content; using the persisted turn",
                );
            }
            // The persisted utterance is authoritative — the engine never
            // rewrites history, and the per-turn recall embeds this text as its
            // query, so a repair must not let the same turn's recall drift.
            (id, st.content)
        }
    };

    let turn = VoiceTurn {
        session_id,
        instance_id,
        user_id,
        user_message_id,
        // Verbatim: the per-turn recall embeds it as the query text (voice has
        // no input-filter rewrite). Already persisted above as the latest
        // history row — the wire messages still come from there.
        content: persisted_content,
        affinity_scope,
        memory_scope: req.memory_scope.unwrap_or_default(),
        // Handed over from the row loaded above — the pipeline reads the
        // `voice_bootstrap` marker from it instead of re-querying the session.
        session_metadata: session.metadata,
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

/// Report a deliberate barge-in on the turn currently being spoken.
///
/// Stopping generation is NOT this endpoint's job — aborting the SSE already
/// does that (the stream generator is dropped, which closes the upstream
/// connection). This endpoint records what the user actually HEARD, which the
/// aborted generator can no longer do: its persist step sits after the
/// streaming loop and never runs.
///
/// Deliberately has no `501 voice_disabled` gate — it makes no LLM call, and
/// gating it on `[tasks.chat_voice]` would make an in-flight call's interrupt
/// fail if the deployment's config changed mid-call.
#[utoipa::path(
    post,
    path = "/comp/voice/{session_id}/turn/interrupt",
    tag = "companion",
    params(("session_id" = Uuid, Path, description = "Voice-channel session id")),
    request_body = VoiceInterruptRequest,
    responses(
        (status = 200, body = VoiceInterruptResponse),
        (status = 400, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 404, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 409, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 422, body = crate::routes::companion_stream::StreamPreErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn voice_turn_interrupt(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<VoiceInterruptRequest>,
) -> Result<Json<VoiceInterruptResponse>, AppError> {
    validate_interrupt(&req)?;

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
    if session.channel != "voice" {
        return Err(pre(
            StatusCode::CONFLICT,
            "wrong_channel",
            "session is not a voice-channel session",
            "该会话不是语音会话",
        ));
    }

    let user_message_id = match chat_repo
        .resolve_latest_voice_user_turn(session_id, &req.client_msg_id)
        .await?
    {
        LatestTurnLookup::Latest(id) => id,
        LatestTurnLookup::NotFound => {
            return Err(pre(
                StatusCode::NOT_FOUND,
                "turn_not_found",
                "no such client_msg_id in this session",
                "该消息不存在",
            ));
        }
        // Guarding this is what stops a client rewriting the content of ANY
        // past assistant reply through the upsert below.
        LatestTurnLookup::Stale => {
            return Err(pre(
                StatusCode::CONFLICT,
                "not_latest_turn",
                "only the session's latest turn can be interrupted",
                "该回合已结束",
            ));
        }
    };

    // TOCTOU: the turn resolved as "latest" above can stop being latest
    // before the upsert below runs (a new turn lands in between). This is
    // safe to leave as-is — it only WIDENS the legitimate window in which a
    // barge-in on the just-resolved turn is accepted, it can never let a
    // client reach an OLDER turn than the one it named (the `Stale` check
    // above already rejected that). Do not "fix" this by re-checking
    // latest-ness right before the upsert.
    let message_id = chat_repo
        .upsert_voice_interrupt(
            session_id,
            user_message_id,
            Ulid::new().into(),
            &req.spoken_text,
        )
        .await?
        .map(|id| ulid_string(Ulid::from(id)));

    Ok(Json(VoiceInterruptResponse { message_id }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(voice_turn_stream))
        .routes(routes!(voice_turn_interrupt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::companion::testutil::seed_persona_instance;
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

    /// Seed a voice-channel session (the voice endpoint's valid input).
    async fn seed(pool: &PgPool, user_id: Uuid) -> Uuid {
        seed_channel(pool, user_id, "voice").await
    }

    /// Seed a session on an explicit `channel` for the voice endpoint's
    /// channel-scoping tests.
    async fn seed_channel(pool: &PgPool, user_id: Uuid, channel: &str) -> Uuid {
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
            "INSERT INTO engine.chat_sessions (user_id, instance_id, channel) VALUES ($1,$2,$3) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .bind(channel)
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

    /// `memory_scope` is a typed enum on the wire: an unknown value is a
    /// payload error, not a silent fallback to the default.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn voice_422_when_memory_scope_invalid(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = seed(&pool, user_id).await;
        let mut app = build_router(with_voice(crate::routes::companion::test_state(pool)));
        let jwt = mint_jwt(user_id);
        let resp = post_voice(
            &mut app,
            session_id,
            &jwt,
            json!({
                "content": "hi",
                "client_msg_id": "01J2222222222222222222222A",
                "memory_scope": "everything"
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// Byte-anchors for the two relationship-line halves at a fresh
    /// (all-default) affinity row — Acquaintance bond text / Spark chemistry
    /// text. Distinctive substrings that appear nowhere else in the prompt.
    const BOND_LINE_MARK: &str = "still getting to know each other";
    const CHEM_LINE_MARK: &str = "unspoken spark";

    /// Seed a default-tier affinity row for the session's (user, instance)
    /// pair so the relationship line renders at all — the voice pipeline
    /// resolves affinity by user × instance, not by session.
    async fn seed_affinity(pool: &PgPool, session_id: Uuid, user_id: Uuid) {
        sqlx::query(
            "INSERT INTO engine.companion_affinity (session_id, user_id, instance_id) \
             SELECT $1, $2, instance_id FROM engine.chat_sessions WHERE id = $1",
        )
        .bind(session_id)
        .bind(user_id)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Drive one mocked voice turn to completion and return the upstream
    /// chat-completions request as its wire JSON string — the caller asserts
    /// on the DB rows it left behind AND on the prompt actually served, so
    /// the audit metadata can never drift from the prompt unnoticed.
    async fn run_mocked_turn(
        pool: &PgPool,
        session_id: Uuid,
        user_id: Uuid,
        body: serde_json::Value,
    ) -> String {
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\
                         \"id\":\"gen-scope-test\",\"model\":\"primary\"}\n\n\
                         data: [DONE]\n\n",
                        "text/event-stream",
                    ),
            )
            .mount(&mock)
            .await;
        let mut state = with_voice(crate::routes::companion::test_state(pool.clone()));
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        let mut app = build_router(state);
        let resp = post_voice(&mut app, session_id, &mint_jwt(user_id), body).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024)
            .await
            .unwrap();
        let sse_text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            !sse_text.contains("\"type\":\"error\""),
            "no error frame expected: {sse_text}"
        );
        let received = mock
            .received_requests()
            .await
            .expect("recording enabled by default");
        let last = received.last().expect("at least one upstream request");
        serde_json::from_slice::<serde_json::Value>(&last.body)
            .unwrap()
            .to_string()
    }

    /// Requirement: the two vocabularies must not cross. The legacy value
    /// `"both"` under the NEW field name is a payload error.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn voice_422_when_affinity_scope_uses_legacy_value(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = seed(&pool, user_id).await;
        let mut app = build_router(with_voice(crate::routes::companion::test_state(pool)));
        let resp = post_voice(
            &mut app,
            session_id,
            &mint_jwt(user_id),
            json!({
                "content": "hi",
                "client_msg_id": "01J2222222222222222222222A",
                "affinity_scope": "both"
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// Neither field ⇒ chat-parity default `bond`, and NO raw keys on the
    /// user row (sparse audit).
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn omitted_scopes_default_to_bond_with_no_raw_audit(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = seed(&pool, user_id).await;
        seed_affinity(&pool, session_id, user_id).await;
        let wire = run_mocked_turn(
            &pool,
            session_id,
            user_id,
            json!({"content": "hi", "client_msg_id": "01J9SCOPEDFLT0000000000001"}),
        )
        .await;
        assert!(
            wire.contains(BOND_LINE_MARK) && !wire.contains(CHEM_LINE_MARK),
            "default bond must serve the bond half only: {wire}"
        );
        let scope: serde_json::Value = sqlx::query_scalar(
            "SELECT metadata->'affinity_scope' \
             FROM engine.chat_messages WHERE session_id = $1 AND role = 'assistant'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            scope,
            serde_json::json!({
                "warmth": true, "trust": false, "intrigue": false,
                "intimacy": true, "patience": false, "tension": true
            }),
            "omitted ⇒ chat default bond"
        );
        let user_meta_null: bool = sqlx::query_scalar(
            "SELECT metadata IS NULL FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'user'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(user_meta_null, "no fields sent ⇒ no raw audit keys");
    }

    /// The axes-array shape resolves (chemistry half from a chemistry axis)
    /// and is echoed verbatim as the raw audit value.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn affinity_scope_axes_array_resolves_and_echoes_raw(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = seed(&pool, user_id).await;
        seed_affinity(&pool, session_id, user_id).await;
        let wire = run_mocked_turn(
            &pool,
            session_id,
            user_id,
            json!({
                "content": "hi",
                "client_msg_id": "01J9SCOPEAXES0000000000001",
                "affinity_scope": ["trust"],
                "memory_scope": "none"
            }),
        )
        .await;
        // trust ∈ chemistry half ⇒ the chemistry text alone reaches the model.
        assert!(
            wire.contains(CHEM_LINE_MARK) && !wire.contains(BOND_LINE_MARK),
            "[\"trust\"] must serve the chemistry half only: {wire}"
        );
        let scope: serde_json::Value = sqlx::query_scalar(
            "SELECT metadata->'affinity_scope' \
             FROM engine.chat_messages WHERE session_id = $1 AND role = 'assistant'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            scope,
            serde_json::json!({
                "warmth": false, "trust": true, "intrigue": false,
                "intimacy": false, "patience": false, "tension": false
            })
        );
        let (raw_scope, raw_memory): (Option<serde_json::Value>, Option<String>) = sqlx::query_as(
            "SELECT metadata->'affinity_scope_raw', metadata->>'memory_scope_raw' \
             FROM engine.chat_messages WHERE session_id = $1 AND role = 'user'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(raw_scope, Some(serde_json::json!(["trust"])));
        assert_eq!(raw_memory.as_deref(), Some("none"));
    }

    /// The field is gone, so an unknown-value `relationship_scope` is now just
    /// an ignored unknown key — NOT a 422. Locks in that the removal did not
    /// leave a validation stub behind, and that such a request falls to the
    /// `bond` default.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn legacy_relationship_scope_is_ignored_and_defaults_to_bond(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = seed(&pool, user_id).await;
        run_mocked_turn(
            &pool,
            session_id,
            user_id,
            json!({
                "content": "hi",
                "client_msg_id": "01J2222222222222222222222A",
                "relationship_scope": "romance"
            }),
        )
        .await;

        let scope: serde_json::Value = sqlx::query_scalar(
            "SELECT metadata->'affinity_scope' FROM engine.chat_messages \
             WHERE role = 'assistant' AND session_id = $1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(scope["warmth"], serde_json::json!(true));
        assert_eq!(scope["intimacy"], serde_json::json!(true));
        assert_eq!(scope["tension"], serde_json::json!(true));
        assert_eq!(scope["trust"], serde_json::json!(false));
        assert_eq!(scope["intrigue"], serde_json::json!(false));
        assert_eq!(scope["patience"], serde_json::json!(false));
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
        let user_message_id = match chat_repo
            .insert_voice_user_message(session_id, "hi", client_msg_id, None)
            .await
            .unwrap()
        {
            VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };
        // A duplicate is only a conflict when the turn PRODUCED something.
        // Seed the reply so this test still pins the double-billing guard
        // rather than the (now relaxed) unconditional rule.
        chat_repo
            .insert_voice_assistant_message(
                session_id,
                user_message_id,
                Uuid::new_v4(),
                "hello back",
                None,
                None,
                None,
                false,
                None,
                None,
                None,
            )
            .await
            .unwrap();

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

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn duplicate_regenerates_when_turn_has_no_reply(pool: PgPool) {
        // Abnormal disconnect: user row persisted, no reply, no marker.
        let user_id = Uuid::new_v4();
        let session_id = seed(&pool, user_id).await;
        seed_user_turn(&pool, session_id, "01J9REPAIR0000000000000001").await;
        let mut app = build_router(with_voice(crate::routes::companion::test_state(
            pool.clone(),
        )));

        let resp = post_voice(
            &mut app,
            session_id,
            &mint_jwt(user_id),
            json!({
                "content": "hello", "client_msg_id": "01J9REPAIR0000000000000001"
            }),
        )
        .await;
        assert_ne!(
            resp.status(),
            StatusCode::CONFLICT,
            "a turn with no reply and no interrupt marker must be repairable"
        );

        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages WHERE session_id = $1 AND role='user'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            n, 1,
            "repair reuses the persisted user row, never adds a second"
        );
    }

    /// `insert_voice_user_message`'s conflict lookup is deliberately
    /// role-agnostic (see its doc comment: "the conflicting row may be a
    /// `user` OR a `gift_user` (tip) row"). Before the repair relaxation that
    /// only ever produced a 409, so the role-agnosticism was harmless. Now
    /// that a duplicate with no reply and no interrupt marker regenerates,
    /// a `client_msg_id` collision against a NON-voice-user row (a
    /// `gift_user` tip row, seeded here — the realistic case per the plan;
    /// also possible via a text-channel `user` row, since `message/stream`
    /// has no channel guard against being pointed at this session id) must
    /// still fall back to the ordinary unconditional 409, never regenerate.
    /// Regenerating would embed the tip marker text as the recall query and
    /// write a `channel='voice'` assistant row whose `user_message_id`
    /// points at a tip row.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn voice_409_and_no_regen_on_non_voice_user_row_collision(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = seed(&pool, user_id).await;
        let client_msg_id = "01J9GIFTCOLLIDE00000000001";

        // Seed a gift_user (tip) row directly, colliding on
        // (session_id, client_msg_id) — the same collision surface
        // `insert_voice_user_message`'s ON CONFLICT resolves against.
        sqlx::query(
            "INSERT INTO engine.chat_messages \
                (session_id, role, content, client_msg_id, metadata) \
             VALUES ($1, 'gift_user', '(打赏 $20)', $2, \
                     '{\"tips_amount_usd\": 20.0}'::jsonb)",
        )
        .bind(session_id)
        .bind(client_msg_id)
        .execute(&pool)
        .await
        .unwrap();

        let resp = post_voice(
            &mut build_router(with_voice(crate::routes::companion::test_state(
                pool.clone(),
            ))),
            session_id,
            &mint_jwt(user_id),
            json!({"content": "hello", "client_msg_id": client_msg_id}),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::CONFLICT,
            "a client_msg_id collision with a non-voice-user row must not be treated as repairable"
        );

        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages WHERE session_id = $1 AND role = 'assistant'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 0, "must not have regenerated an assistant reply");
    }

    /// A 512-dim one-hot vector — a query embedded at the same seed is the
    /// exact nearest neighbour of a row stored at it. Mirrors
    /// `pipeline::voice::tests::unit_embedding`.
    fn unit_embedding(seed: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; 512];
        v[seed % 512] = 1.0;
        v
    }

    /// An `EmbeddingRouter` whose read backend is a wiremock server. Mirrors
    /// `pipeline::voice::tests::embed_router`.
    fn embed_router(uri: &str) -> eros_engine_llm::embedding::EmbeddingRouter {
        let cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(&format!(
            "[providers.mockembed]\nembeddings = \"{uri}/v1/embeddings\"\n\
             [tasks.embedding]\nmodel = \"mock-embed@mockembed\"\n"
        ))
        .expect("embedding provider config parses");
        eros_engine_llm::embedding::EmbeddingRouter::from_config_with(&cfg, |k| {
            (k == "MOCKEMBED_API_KEY").then(|| "test-key".to_string())
        })
        .expect("mock embedding router builds")
    }

    /// The route-level counterpart to
    /// `pipeline::voice::tests::repair_uses_persisted_content_not_the_retry_body`:
    /// that test proves `run_voice_turn` respects whatever `VoiceTurn.content`
    /// it is handed, but never reaches the `Duplicate` arm where the ACTUAL
    /// choice between `st.content` (persisted) and `req.content` (retry body)
    /// is made. This drives the real HTTP route end to end — orphaned turn,
    /// then a retry whose body carries DIFFERENT content — and inspects the
    /// upstream request the route's regeneration actually issued.
    ///
    /// Same discrimination technique as the pipeline test: the embed mock
    /// answers ONLY a query built from the persisted text (any other query,
    /// e.g. the retry body's, 404s and recall degrades to nothing), and the
    /// one memory row it can find carries a marker that appears nowhere else
    /// in the prompt — so a populated recall block in the CAPTURED upstream
    /// request is proof the persisted text, not the retry's, drove the
    /// search. Reverting `content: persisted_content` back to
    /// `content: req.content` at the route's `Duplicate` arm makes this test
    /// fail (verified — see the task report's sabotage transcript).
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn duplicate_regenerates_using_persisted_content_not_retry_body(pool: PgPool) {
        use eros_engine_store::memory::{MemoryLayer, MemoryRepo};
        use wiremock::matchers::{body_partial_json, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let user_id = Uuid::new_v4();
        let session_id = seed(&pool, user_id).await;

        let persisted_text = "persisted words";
        let retry_text = "different words";
        let client_msg_id = "01J9CONTENT0000000000000003";

        // Abnormal disconnect: the FIRST attempt's utterance landed, nothing
        // else — no reply, no marker.
        let chat_repo = ChatRepo { pool: &pool };
        match chat_repo
            .insert_voice_user_message(session_id, persisted_text, client_msg_id, None)
            .await
            .unwrap()
        {
            VoiceUserInsert::Inserted(_) => {}
            other => panic!("expected Inserted, got {other:?}"),
        }

        // The embed mock answers ONLY a query built from the persisted text.
        let embed = MockServer::start().await;
        Mock::given(wm_path("/v1/embeddings"))
            .and(body_partial_json(
                serde_json::json!({ "input": [persisted_text] }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [ { "embedding": unit_embedding(7) } ] }),
            ))
            .mount(&embed)
            .await;

        // One memory row, findable only by the persisted-text query's vector.
        MemoryRepo { pool: &pool }
            .upsert(
                MemoryLayer::Profile,
                session_id,
                user_id,
                None,
                "TOKYO_MARKER_SNIPPET",
                &unit_embedding(7),
                Some("preference"),
                None,
            )
            .await
            .unwrap();

        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\
                         \"id\":\"gen-route-repair\",\"model\":\"primary\"}\n\n\
                         data: [DONE]\n\n",
                        "text/event-stream",
                    ),
            )
            .mount(&mock)
            .await;

        let mut state = with_voice(crate::routes::companion::test_state(pool.clone()));
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        state.embed = Arc::new(embed_router(&embed.uri()));
        let mut app = build_router(state);

        // The RETRY: same client_msg_id, but DIFFERENT content — must be
        // discarded in favour of the persisted row.
        let resp = post_voice(
            &mut app,
            session_id,
            &mint_jwt(user_id),
            json!({ "content": retry_text, "client_msg_id": client_msg_id }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // Drive the SSE body to completion so the mocked upstream is actually
        // hit (the pipeline's async_stream! only runs as the body is polled).
        let bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024)
            .await
            .unwrap();
        let sse_text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            !sse_text.contains("\"type\":\"error\""),
            "no error frame expected: {sse_text}"
        );

        let received = mock
            .received_requests()
            .await
            .expect("recording enabled by default");
        let last = received.last().expect("at least one upstream request");
        let wire: serde_json::Value = serde_json::from_slice(&last.body).unwrap();
        let wire_str = wire.to_string();

        assert!(
            wire_str.contains("TOKYO_MARKER_SNIPPET"),
            "recall must have used the PERSISTED text as its query — a wrong \
             query would 404 the embed mock and leave no recall block: {wire_str}"
        );
        assert!(
            wire_str.contains(persisted_text),
            "the persisted utterance must reach the upstream request: {wire_str}"
        );
        assert!(
            !wire_str.contains(retry_text),
            "the retry body's text must never reach the upstream request: {wire_str}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn duplicate_still_409_after_interrupt(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = seed(&pool, user_id).await;
        let uid = seed_user_turn(&pool, session_id, "01J9REPAIR0000000000000002").await;
        sqlx::query(
            "UPDATE engine.chat_messages \
                SET metadata = COALESCE(metadata,'{}'::jsonb) || '{\"voice_interrupt\": true}'::jsonb \
              WHERE id = $1",
        ).bind(uid).execute(&pool).await.unwrap();
        let mut app = build_router(with_voice(crate::routes::companion::test_state(pool)));

        let resp = post_voice(
            &mut app,
            session_id,
            &mint_jwt(user_id),
            json!({
                "content": "hello", "client_msg_id": "01J9REPAIR0000000000000002"
            }),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::CONFLICT,
            "a deliberate barge-in has nothing to repair"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn voice_409_when_session_not_voice_channel(pool: PgPool) {
        // The voice endpoint must only operate on voice-channel sessions.
        // `seed` creates a default (text-channel) session; posting a voice turn
        // to it must be rejected pre-stream (409) WITHOUT persisting a user row,
        // so voice turns never leak into a text conversation.
        let user_id = Uuid::new_v4();
        let session_id = seed_channel(&pool, user_id, "text").await;
        let mut app = build_router(with_voice(crate::routes::companion::test_state(
            pool.clone(),
        )));
        let jwt = mint_jwt(user_id);
        let resp = post_voice(
            &mut app,
            session_id,
            &jwt,
            json!({"content":"hi","client_msg_id":"01J2222222222222222222222A"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        // No user row persisted — the rejection happened before the insert.
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.chat_messages WHERE session_id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 0, "wrong-channel rejection must not persist a turn");
    }

    async fn post_interrupt(
        app: &mut Router,
        session_id: Uuid,
        jwt: &str,
        body: serde_json::Value,
    ) -> axum::http::Response<Body> {
        let req = Request::builder()
            .method("POST")
            .uri(format!("/comp/voice/{session_id}/turn/interrupt"))
            .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        app.call(req).await.unwrap()
    }

    /// Persist a voice user turn directly, returning its id.
    async fn seed_user_turn(pool: &PgPool, session_id: Uuid, cid: &str) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, channel, client_msg_id) \
             VALUES ($1,'user','hello','voice',$2) RETURNING id",
        )
        .bind(session_id)
        .bind(cid)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn interrupt_persists_spoken_text(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = seed(&pool, user_id).await;
        seed_user_turn(&pool, session_id, "01J9INTERRUPT0000000000001").await;
        let mut app = build_router(with_voice(crate::routes::companion::test_state(
            pool.clone(),
        )));

        let resp = post_interrupt(
            &mut app,
            session_id,
            &mint_jwt(user_id),
            json!({
                "client_msg_id": "01J9INTERRUPT0000000000001",
                "spoken_text": "你今天过得"
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let content: String = sqlx::query_scalar(
            "SELECT content FROM engine.chat_messages \
              WHERE session_id = $1 AND role = 'assistant'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(content, "你今天过得");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn interrupt_with_empty_text_writes_marker_only(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = seed(&pool, user_id).await;
        let uid = seed_user_turn(&pool, session_id, "01J9INTERRUPT0000000000002").await;
        let mut app = build_router(with_voice(crate::routes::companion::test_state(
            pool.clone(),
        )));

        let resp = post_interrupt(
            &mut app,
            session_id,
            &mint_jwt(user_id),
            json!({
                "client_msg_id": "01J9INTERRUPT0000000000002",
                "spoken_text": ""
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages WHERE session_id = $1 AND role='assistant'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 0);
        let marked: bool = sqlx::query_scalar(
            "SELECT COALESCE(metadata->>'voice_interrupt','false')::bool \
               FROM engine.chat_messages WHERE id = $1",
        )
        .bind(uid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(marked);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn interrupt_is_idempotent(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = seed(&pool, user_id).await;
        seed_user_turn(&pool, session_id, "01J9INTERRUPT0000000000003").await;
        let mut app = build_router(with_voice(crate::routes::companion::test_state(
            pool.clone(),
        )));
        let jwt = mint_jwt(user_id);
        let body = json!({"client_msg_id":"01J9INTERRUPT0000000000003","spoken_text":"半句"});

        assert_eq!(
            post_interrupt(&mut app, session_id, &jwt, body.clone())
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            post_interrupt(&mut app, session_id, &jwt, body)
                .await
                .status(),
            StatusCode::OK
        );

        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages WHERE session_id = $1 AND role='assistant'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1, "repeat interrupts must not multiply rows");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn interrupt_409_when_not_latest_turn(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = seed(&pool, user_id).await;
        seed_user_turn(&pool, session_id, "01J9INTERRUPT0000000000004").await;
        seed_user_turn(&pool, session_id, "01J9INTERRUPT0000000000005").await;
        let mut app = build_router(with_voice(crate::routes::companion::test_state(
            pool.clone(),
        )));

        let resp = post_interrupt(
            &mut app,
            session_id,
            &mint_jwt(user_id),
            json!({
                "client_msg_id": "01J9INTERRUPT0000000000004",
                "spoken_text": "rewrite attempt"
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages WHERE session_id = $1 AND role='assistant'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 0, "a stale interrupt must not touch history");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn interrupt_404_when_client_msg_id_unknown(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = seed(&pool, user_id).await;
        let mut app = build_router(with_voice(crate::routes::companion::test_state(pool)));
        let resp = post_interrupt(
            &mut app,
            session_id,
            &mint_jwt(user_id),
            json!({
                "client_msg_id": "01J9INTERRUPT0000000000009", "spoken_text": "x"
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn interrupt_403_when_not_owner(pool: PgPool) {
        let owner = Uuid::new_v4();
        let session_id = seed(&pool, owner).await;
        seed_user_turn(&pool, session_id, "01J9INTERRUPT0000000000006").await;
        let mut app = build_router(with_voice(crate::routes::companion::test_state(pool)));
        let resp = post_interrupt(
            &mut app,
            session_id,
            &mint_jwt(Uuid::new_v4()),
            json!({
                "client_msg_id": "01J9INTERRUPT0000000000006", "spoken_text": "x"
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn interrupt_409_when_session_not_voice_channel(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = seed_channel(&pool, user_id, "text").await;
        let mut app = build_router(with_voice(crate::routes::companion::test_state(pool)));
        let resp = post_interrupt(
            &mut app,
            session_id,
            &mint_jwt(user_id),
            json!({
                "client_msg_id": "01J9INTERRUPT0000000000007", "spoken_text": "x"
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn voice_404_when_persona_instance_inactive(pool: PgPool) {
        // A session whose persona instance is not active must be rejected
        // pre-stream (404) WITHOUT persisting a user row. Archival is the only
        // way to reach this state — 0040 gave chat_sessions.instance_id a real
        // FK, so a dangling id can no longer be written and a deleted instance
        // takes its sessions with it (ON DELETE CASCADE). That matches
        // production: the engine only ever flips status to 'archived', it never
        // DELETEs an instance. load_companion filters status = 'active', so
        // this lands on the same 404 branch the dangling case used to take.
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, channel) \
             VALUES ($1, $2, 'voice') RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE engine.persona_instances SET status = 'archived' WHERE id = $1")
            .bind(instance_id)
            .execute(&pool)
            .await
            .unwrap();

        let mut app = build_router(with_voice(crate::routes::companion::test_state(
            pool.clone(),
        )));
        let jwt = mint_jwt(user_id);
        let cmid = "01J3333333333333333333333A";
        let resp = post_voice(
            &mut app,
            session_id,
            &jwt,
            json!({"content":"hi","client_msg_id": cmid}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // The pre-stream 404 must not have persisted a user row.
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages WHERE client_msg_id = $1",
        )
        .bind(cmid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            n, 0,
            "no user row should be persisted when the instance is missing"
        );
    }
}
