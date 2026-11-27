// SPDX-License-Identifier: AGPL-3.0-only
//! `POST /comp/chat/{session_id}/messages/speech`: her stored chat messages,
//! prepared for a text-to-speech voice.
//!
//! Spec: docs/superpowers/specs/2026-11-27-chat-speech-markup-design.md

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::Json;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::byoc::LoadedInstance;
use crate::error::{AppError, StreamPreError};
use crate::pipeline::speech::{run_speech, SpeechJob};
use crate::pipeline::stream::ProtocolFrame;
use crate::state::AppState;

/// How many messages one request may name.
const MAX_MESSAGES: usize = 8;
/// A request holds one of the caller's stream slots, as a voice turn does.
const CONCURRENT_STREAMS_PER_USER: u32 = 3;

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SpeechRequest {
    /// 1 to 8 distinct ids of her messages in this session.
    pub message_ids: Vec<Uuid>,
    /// `true` lets the model rephrase her line for the ear; by default her
    /// words are kept and only tags are added.
    #[serde(default)]
    pub rewrite: bool,
    /// `true` (or an `Accept: text/event-stream` header) answers as a stream.
    #[serde(default)]
    pub stream: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SpeechResponse {
    /// One per requested id, in request order.
    pub items: Vec<SpeechItem>,
}

/// A marked-up line, or why it could not be made.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum SpeechItem {
    Text {
        message_id: Uuid,
        /// The line with inline audio tags.
        text: String,
        /// The model stopped early; the text is incomplete.
        truncated: bool,
        model: Option<String>,
        usage: Option<serde_json::Value>,
        generation_id: Option<String>,
    },
    Error {
        message_id: Uuid,
        error: SpeechItemError,
    },
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SpeechItemError {
    /// `not_found`, `not_speakable` or `generation_failed`.
    pub code: String,
    pub message: String,
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

fn validate(req: &SpeechRequest) -> Result<(), AppError> {
    let n = req.message_ids.len();
    let distinct = req
        .message_ids
        .iter()
        .collect::<std::collections::HashSet<_>>()
        .len();
    if n == 0 || n > MAX_MESSAGES || distinct != n {
        return Err(pre(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unprocessable",
            "message_ids must name 1 to 8 distinct messages",
            "请求无效",
        ));
    }
    Ok(())
}

#[utoipa::path(
    post,
    path = "/comp/chat/{session_id}/messages/speech",
    tag = "companion",
    params(("session_id" = Uuid, Path, description = "Chat session id")),
    request_body = SpeechRequest,
    responses(
        (status = 200, body = SpeechResponse, description = "Each requested message as a line with \
            inline audio tags, or its own error. With `stream: true` or `Accept: text/event-stream`: \
            per item `delta`… then `done` (carrying `text`), or `item_error`; then one `final`. \
            Frames of different items interleave and name their message by `message_id`."),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 404, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 409, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 400, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 422, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 429, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 501, body = crate::routes::companion_stream::StreamPreErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn speech_markup(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    headers: HeaderMap,
    Json(req): Json<SpeechRequest>,
) -> Result<Response, AppError> {
    validate(&req)?;
    let (_, instance_id) =
        crate::routes::companion_stream::resolve_send_session(&state, session_id, user_id).await?;
    let resolved = state.model_config.resolve_speech().ok_or_else(|| {
        pre(
            StatusCode::NOT_IMPLEMENTED,
            "speech_disabled",
            "speech markup is not configured on this deployment",
            "该服务未启用语音朗读",
        )
    })?;
    match crate::byoc::load_instance(&state, instance_id).await? {
        Some(LoadedInstance::Genome(_)) => {}
        Some(LoadedInstance::Byoc(_)) => return Err(crate::byoc::unsupported("speech markup")),
        None => return Err(AppError::NotFound("instance not found".into())),
    }
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
    let relationship = crate::pipeline::activity::turn_session(&state.pool, session_id)
        .await?
        .relationship;
    let ids = req.message_ids.clone();
    let frames = run_speech(
        Arc::new(state.clone()),
        SpeechJob {
            session_id,
            relationship,
            instance_id,
            message_ids: req.message_ids,
            rewrite: req.rewrite,
        },
        resolved,
    );
    if crate::routes::dto::wants_stream(&headers, req.stream) {
        return Ok(crate::routes::dto::sse_response(async_stream::stream! {
            // Held while the items run; a dropped connection drops it.
            let _guard = guard;
            futures_util::pin_mut!(frames);
            while let Some(frame) = frames.next().await {
                yield frame;
            }
        }));
    }

    let _guard = guard;
    futures_util::pin_mut!(frames);
    let mut by_id: HashMap<String, SpeechItem> = HashMap::new();
    while let Some(frame) = frames.next().await {
        let (key, item) = match frame {
            ProtocolFrame::SpeechDone {
                message_id,
                text,
                truncated,
                usage,
                generation_id,
                model,
            } => {
                let id = message_id.parse().expect("a source message id");
                let item = SpeechItem::Text {
                    message_id: id,
                    text,
                    truncated,
                    model,
                    usage,
                    generation_id,
                };
                (message_id, item)
            }
            ProtocolFrame::ItemError {
                message_id,
                code,
                message,
            } => {
                let id = message_id.parse().expect("a source message id");
                let item = SpeechItem::Error {
                    message_id: id,
                    error: SpeechItemError {
                        code: code.into(),
                        message,
                    },
                };
                (message_id, item)
            }
            ProtocolFrame::Error { message, .. } => return Err(AppError::Internal(message)),
            _ => continue,
        };
        by_id.insert(key, item);
    }
    let items = ids
        .iter()
        .filter_map(|id| by_id.remove(&id.to_string()))
        .collect();
    Ok(Json(SpeechResponse { items }).into_response())
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(speech_markup))
}

#[cfg(test)]
mod tests {
    use crate::routes::companion::testutil::{
        read_sse, seed_byoc_instance_with_key, seed_persona_instance,
    };
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request, StatusCode};
    use axum::Router;
    use eros_engine_llm::model_config::ModelConfig;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::{json, Value};
    use sqlx::PgPool;
    use std::sync::Arc;
    use tower::Service;
    use uuid::Uuid;
    use wiremock::matchers::{body_string_contains, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::state::AppState;

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

    const SPEECH: &str = "[tasks.chat_speech]\nmodel = \"speech\"\nmax_tokens = 400\n";

    fn with_speech(pool: PgPool, mock: &MockServer, toml: &str) -> AppState {
        let mut state = crate::routes::companion::test_state(pool);
        state.model_config = Arc::new(ModelConfig::from_toml_str(toml).unwrap());
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        state
    }

    /// One upstream completion: `text`, ending on `finish` when given.
    fn sse(text: &str, finish: Option<&str>) -> String {
        let mut choice = json!({"delta": {"content": text}});
        if let Some(f) = finish {
            choice["finish_reason"] = json!(f);
        }
        let chunk = json!({
            "id": "gen-speech", "model": "served/speech", "choices": [choice],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        });
        format!("data: {chunk}\n\ndata: [DONE]\n\n")
    }

    /// What the request body holds for the line it prepares (the JSON
    /// escape of the newline included), so a line quoted in another item's
    /// transcript does not match.
    fn preparing(line: &str) -> String {
        format!("The line to prepare:\\n{line}")
    }

    /// The model answers `reply` to a request that prepares `line`.
    async fn answer(mock: &MockServer, line: &str, reply: &str, finish: Option<&str>) {
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains(preparing(line)))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse(reply, finish), "text/event-stream"),
            )
            .mount(mock)
            .await;
    }

    async fn fail(mock: &MockServer, line: &str) {
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains(preparing(line)))
            .respond_with(ResponseTemplate::new(500).set_body_string("down"))
            .mount(mock)
            .await;
    }

    /// The bodies the model received, as JSON.
    async fn calls(mock: &MockServer) -> Vec<Value> {
        mock.received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    /// A session of `channel` for a stored persona; (user, instance, session).
    async fn scene(pool: &PgPool, channel: &str) -> (Uuid, Uuid, Uuid) {
        let user = Uuid::new_v4();
        let instance = seed_persona_instance(pool, user).await;
        let session = session_for(pool, user, instance, channel).await;
        (user, instance, session)
    }

    async fn session_for(pool: &PgPool, user: Uuid, instance: Uuid, channel: &str) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, channel) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(user)
        .bind(instance)
        .bind(channel)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// A row of `session`, `mins` minutes ago.
    async fn row(
        pool: &PgPool,
        session: Uuid,
        role: &str,
        content: &str,
        metadata: Option<Value>,
        mins: i32,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, metadata, sent_at) \
             VALUES ($1, $2, $3, $4, now() - make_interval(mins => $5)) RETURNING id",
        )
        .bind(session)
        .bind(role)
        .bind(content)
        .bind(metadata)
        .bind(mins)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn post(
        app: &mut Router,
        session: Uuid,
        user: Uuid,
        body: Value,
        accept_sse: bool,
    ) -> axum::response::Response {
        let mut req = Request::builder()
            .method("POST")
            .uri(format!("/comp/chat/{session}/messages/speech"))
            .header(header::AUTHORIZATION, format!("Bearer {}", mint_jwt(user)))
            .header(header::CONTENT_TYPE, "application/json");
        if accept_sse {
            req = req.header(header::ACCEPT, "text/event-stream");
        }
        app.call(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
    }

    async fn json_of(resp: axum::response::Response) -> Value {
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn malformed_requests_are_422(pool: PgPool) {
        let mock = MockServer::start().await;
        let (user, _, session) = scene(&pool, "text").await;
        let mut app = build_router(with_speech(pool, &mock, SPEECH));
        let id = Uuid::new_v4();
        let nine: Vec<Uuid> = (0..9).map(|_| Uuid::new_v4()).collect();
        for body in [
            json!({"message_ids": []}),
            json!({"message_ids": nine}),
            json!({"message_ids": [id, id]}),
            json!({"message_ids": ["not-a-uuid"]}),
        ] {
            let resp = post(&mut app, session, user, body.clone(), false).await;
            assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        }
        assert!(calls(&mock).await.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn sessions_the_route_does_not_serve(pool: PgPool) {
        let mock = MockServer::start().await;
        let (user, instance, text) = scene(&pool, "text").await;
        let id = row(&pool, text, "assistant", "LINE", None, 1).await;
        let body = json!({"message_ids": [id]});

        let mut off = build_router(with_speech(
            pool.clone(),
            &mock,
            "[tasks.chat_companion]\nmodel = \"m\"\n",
        ));
        let resp = post(&mut off, text, user, body.clone(), false).await;
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        assert_eq!(json_of(resp).await["code"], "speech_disabled");

        let mut app = build_router(with_speech(pool.clone(), &mock, SPEECH));
        let group: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, channel) VALUES ($1, 'group') RETURNING id",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        for (channel, s) in [
            ("voice", session_for(&pool, user, instance, "voice").await),
            ("group", group),
        ] {
            let resp = post(&mut app, s, user, body.clone(), false).await;
            assert_eq!(resp.status(), StatusCode::CONFLICT, "{channel}");
        }
        let resp = post(&mut app, Uuid::new_v4(), user, body.clone(), false).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let resp = post(&mut app, text, Uuid::new_v4(), body.clone(), false).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let (byoc, _key) = seed_byoc_instance_with_key(&pool, user).await;
        let s = session_for(&pool, user, byoc, "text").await;
        let resp = post(&mut app, s, user, body, false).await;
        assert_eq!(json_of(resp).await["code"], "byoc_unsupported");
        assert!(calls(&mock).await.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn one_message_comes_back_marked_up(pool: PgPool) {
        let mock = MockServer::start().await;
        answer(&mock, "LINE-OK", "  [whispers] LINE-OK  ", None).await;
        let (user, _, session) = scene(&pool, "text").await;
        let id = row(&pool, session, "assistant", "LINE-OK", None, 1).await;
        let mut app = build_router(with_speech(pool.clone(), &mock, SPEECH));

        let resp = post(&mut app, session, user, json!({"message_ids": [id]}), false).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_of(resp).await;
        let item = &body["items"][0];
        assert_eq!(item["message_id"], id.to_string());
        assert_eq!(item["text"], "[whispers] LINE-OK");
        assert_eq!(item["truncated"], false);
        assert_eq!(item["generation_id"], "gen-speech");

        let c = calls(&mock).await;
        assert_eq!(c.len(), 1);
        let messages = c[0]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert!(messages[0]["content"]
            .as_str()
            .unwrap()
            .starts_with("you are a companion"));
        assert!(messages[1]["content"]
            .as_str()
            .unwrap()
            .ends_with("LINE-OK"));

        let tasks: Vec<String> =
            sqlx::query_scalar("SELECT task FROM engine.llm_generations WHERE session_id = $1")
                .bind(session)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(tasks, ["chat_speech"]);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_request_carries_tone_scope_and_context(pool: PgPool) {
        use eros_engine_core::scope::AffinityScope;
        let mock = MockServer::start().await;
        answer(&mock, "LINE-TONED", "LINE-TONED", None).await;
        answer(&mock, "LINE-PLAIN", "LINE-PLAIN", None).await;
        let (user, instance, session) = scene(&pool, "text").await;
        sqlx::query(
            "INSERT INTO engine.companion_affinity (session_id, user_id, instance_id, intimacy, tension) \
             VALUES ($1, $2, $3, 0.7, 0.7)",
        )
        .bind(session)
        .bind(user)
        .bind(instance)
        .execute(&pool)
        .await
        .unwrap();
        for (i, content) in ["EARLY-1", "EARLY-2", "EARLY-3", "EARLY-4", "EARLY-5"]
            .iter()
            .enumerate()
        {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            row(&pool, session, role, content, None, 20 - i as i32).await;
        }
        let toned = row(
            &pool,
            session,
            "assistant",
            "LINE-TONED",
            Some(json!({"tone": "erotic", "affinity_scope": serde_json::to_value(AffinityScope::full()).unwrap()})),
            10,
        )
        .await;
        let plain = row(&pool, session, "assistant", "LINE-PLAIN", None, 9).await;
        let mut app = build_router(with_speech(pool.clone(), &mock, SPEECH));
        let resp = post(
            &mut app,
            session,
            user,
            json!({"message_ids": [toned, plain]}),
            false,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let c = calls(&mock).await;
        let of = |line: &str| {
            c.iter()
                .find(|b| {
                    b["messages"][1]["content"]
                        .as_str()
                        .unwrap()
                        .ends_with(line)
                })
                .unwrap_or_else(|| panic!("a call for {line}"))
                .clone()
        };
        let t = of("LINE-TONED");
        let system = t["messages"][0]["content"].as_str().unwrap();
        assert!(
            system.contains("She said this in the grip of a feeling"),
            "{system}"
        );
        assert!(system.contains("She wants the other person"), "{system}");
        assert!(
            system.contains("gently alluring"),
            "tier-4 chemistry half: {system}"
        );
        let user_msg = t["messages"][1]["content"].as_str().unwrap();
        assert!(
            !user_msg.contains("EARLY-1"),
            "five earlier rows, four kept: {user_msg}"
        );
        for kept in ["EARLY-2", "EARLY-3", "EARLY-4", "EARLY-5"] {
            assert!(user_msg.contains(kept), "{kept}: {user_msg}");
        }

        let p = of("LINE-PLAIN");
        let system = p["messages"][0]["content"].as_str().unwrap();
        assert!(!system.contains("She said this"), "{system}");
        assert!(
            !system.contains("gently alluring"),
            "the default scope has no chemistry: {system}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_date_sub_session_reads_its_parents_affinity(pool: PgPool) {
        let mock = MockServer::start().await;
        answer(&mock, "LINE-DATE", "LINE-DATE", None).await;
        let (user, instance, parent) = scene(&pool, "text").await;
        sqlx::query(
            "INSERT INTO engine.companion_affinity (session_id, user_id, instance_id, trust, intrigue) \
             VALUES ($1, $2, $3, 0.95, 0.95)",
        )
        .bind(parent)
        .bind(user)
        .bind(instance)
        .execute(&pool)
        .await
        .unwrap();
        let sub = session_for(&pool, user, instance, "activity").await;
        sqlx::query(
            "INSERT INTO engine.activities (session_id, sub_session_id, duration, place, content) \
             VALUES ($1, $2, 'short', 'park', 'a walk')",
        )
        .bind(parent)
        .bind(sub)
        .execute(&pool)
        .await
        .unwrap();
        let id = row(&pool, sub, "assistant", "LINE-DATE", None, 1).await;
        let mut app = build_router(with_speech(pool.clone(), &mock, SPEECH));
        let resp = post(&mut app, sub, user, json!({"message_ids": [id]}), false).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let c = calls(&mock).await;
        let system = c[0]["messages"][0]["content"].as_str().unwrap();
        assert!(
            system.contains("know each other inside out"),
            "the parent's soulmate bond: {system}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn item_errors_do_not_fail_the_request(pool: PgPool) {
        let mock = MockServer::start().await;
        answer(&mock, "LINE-OK", "LINE-OK", None).await;
        fail(&mock, "LINE-FAIL").await;
        let (user, instance, session) = scene(&pool, "text").await;
        let other = session_for(&pool, user, instance, "text").await;
        let ok = row(&pool, session, "assistant", "LINE-OK", None, 5).await;
        let foreign = row(&pool, other, "assistant", "LINE-FOREIGN", None, 5).await;
        let blank = row(&pool, session, "assistant", "  ", None, 4).await;
        let theirs = row(&pool, session, "user", "LINE-USER", None, 3).await;
        let failing = row(&pool, session, "assistant", "LINE-FAIL", None, 2).await;
        let mut app = build_router(with_speech(pool.clone(), &mock, SPEECH));

        let ids = [ok, foreign, blank, theirs, failing];
        let resp = post(&mut app, session, user, json!({"message_ids": ids}), false).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_of(resp).await;
        let items = body["items"].as_array().unwrap();
        let got: Vec<(String, Value)> = items
            .iter()
            .map(|i| {
                (
                    i["message_id"].as_str().unwrap().to_string(),
                    i["error"]["code"].clone(),
                )
            })
            .collect();
        let want: Vec<(String, Value)> = [
            (ok, Value::Null),
            (foreign, json!("not_found")),
            (blank, json!("not_speakable")),
            (theirs, json!("not_speakable")),
            (failing, json!("generation_failed")),
        ]
        .into_iter()
        .map(|(id, code)| (id.to_string(), code))
        .collect();
        assert_eq!(got, want);
        assert_eq!(items[0]["text"], "LINE-OK");
        for i in &items[1..] {
            assert!(i.get("text").is_none(), "{i}");
        }
        for line in ["LINE-FOREIGN", "LINE-USER"] {
            assert!(
                !calls(&mock)
                    .await
                    .iter()
                    .any(|b| b["messages"][1]["content"]
                        .as_str()
                        .unwrap()
                        .ends_with(line)),
                "{line} must reach no model"
            );
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_cut_off_output_is_returned_truncated(pool: PgPool) {
        let mock = MockServer::start().await;
        answer(&mock, "LINE-LONG", "[sighs] LINE-LO", Some("length")).await;
        let (user, _, session) = scene(&pool, "text").await;
        let id = row(&pool, session, "assistant", "LINE-LONG", None, 1).await;
        let mut app = build_router(with_speech(pool.clone(), &mock, SPEECH));
        let body =
            json_of(post(&mut app, session, user, json!({"message_ids": [id]}), false).await).await;
        assert_eq!(body["items"][0]["text"], "[sighs] LINE-LO");
        assert_eq!(body["items"][0]["truncated"], true);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn rewrite_switches_the_mode_sentence(pool: PgPool) {
        use eros_engine_llm::model_config::{SPEECH_MODE_REWRITE, SPEECH_MODE_VERBATIM};
        let mock = MockServer::start().await;
        answer(&mock, "LINE-OK", "LINE-OK", None).await;
        let (user, _, session) = scene(&pool, "text").await;
        let id = row(&pool, session, "assistant", "LINE-OK", None, 1).await;
        let mut app = build_router(with_speech(pool.clone(), &mock, SPEECH));
        post(&mut app, session, user, json!({"message_ids": [id]}), false).await;
        post(
            &mut app,
            session,
            user,
            json!({"message_ids": [id], "rewrite": true}),
            false,
        )
        .await;
        let c = calls(&mock).await;
        let system = |i: usize| c[i]["messages"][0]["content"].as_str().unwrap().to_string();
        assert!(
            system(0).contains(SPEECH_MODE_VERBATIM) && !system(0).contains(SPEECH_MODE_REWRITE)
        );
        assert!(
            system(1).contains(SPEECH_MODE_REWRITE) && !system(1).contains(SPEECH_MODE_VERBATIM)
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn nothing_is_written_but_generations(pool: PgPool) {
        let mock = MockServer::start().await;
        answer(&mock, "LINE-OK", "LINE-OK", None).await;
        let (user, instance, session) = scene(&pool, "text").await;
        sqlx::query(
            "INSERT INTO engine.companion_affinity (session_id, user_id, instance_id) VALUES ($1, $2, $3)",
        )
        .bind(session)
        .bind(user)
        .bind(instance)
        .execute(&pool)
        .await
        .unwrap();
        let id = row(&pool, session, "assistant", "LINE-OK", None, 1).await;
        let snapshot = || {
            let pool = pool.clone();
            async move {
                let rows: Value = sqlx::query_scalar(
                    "SELECT coalesce(jsonb_agg(to_jsonb(m) ORDER BY id), '[]') FROM engine.chat_messages m WHERE session_id = $1",
                )
                .bind(session)
                .fetch_one(&pool)
                .await
                .unwrap();
                let affinity: Value = sqlx::query_scalar(
                    "SELECT to_jsonb(a) FROM engine.companion_affinity a WHERE session_id = $1",
                )
                .bind(session)
                .fetch_one(&pool)
                .await
                .unwrap();
                let sess: Value = sqlx::query_scalar(
                    "SELECT to_jsonb(s) FROM engine.chat_sessions s WHERE id = $1",
                )
                .bind(session)
                .fetch_one(&pool)
                .await
                .unwrap();
                (rows, affinity, sess)
            }
        };
        let before = snapshot().await;
        let mut app = build_router(with_speech(pool.clone(), &mock, SPEECH));
        let resp = post(&mut app, session, user, json!({"message_ids": [id]}), false).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(snapshot().await, before);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_stream_names_each_item_and_ends_with_final(pool: PgPool) {
        let mock = MockServer::start().await;
        answer(&mock, "LINE-A", " [laughs] LINE-A", None).await;
        answer(&mock, "LINE-B", "LINE-B", None).await;
        let (user, _, session) = scene(&pool, "text").await;
        let a = row(&pool, session, "assistant", "LINE-A", None, 2).await;
        let b = row(&pool, session, "assistant", "LINE-B", None, 1).await;
        let missing = Uuid::new_v4();
        let mut app = build_router(with_speech(pool.clone(), &mock, SPEECH));

        for (body, accept) in [
            (
                json!({"message_ids": [a, b, missing], "stream": true}),
                false,
            ),
            (json!({"message_ids": [a, b, missing]}), true),
        ] {
            let resp = post(&mut app, session, user, body, accept).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let frames = read_sse(resp).await;
            assert_eq!(
                frames.last().unwrap(),
                &json!({"type": "final"}),
                "{frames:?}"
            );
            assert!(frames.iter().all(|f| f["type"] != "meta"), "{frames:?}");
            for id in [a, b] {
                let id = id.to_string();
                let deltas: String = frames
                    .iter()
                    .filter(|f| f["type"] == "delta" && f["message_id"] == id)
                    .map(|f| f["content"].as_str().unwrap())
                    .collect();
                let done: Vec<&Value> = frames
                    .iter()
                    .filter(|f| f["type"] == "done" && f["message_id"] == id)
                    .collect();
                assert_eq!(done.len(), 1, "{frames:?}");
                assert_eq!(done[0]["text"], deltas.trim(), "{frames:?}");
                assert_eq!(done[0]["truncated"], false);
            }
            let errors: Vec<&Value> = frames
                .iter()
                .filter(|f| f["type"] == "item_error")
                .collect();
            assert_eq!(errors.len(), 1, "{frames:?}");
            assert_eq!(errors[0]["message_id"], missing.to_string());
            assert_eq!(errors[0]["code"], "not_found");
        }
    }
}
