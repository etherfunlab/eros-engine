// SPDX-License-Identifier: AGPL-3.0-only
//! POST /v2/comp/session/{session_id}/events — the caller asks for something
//! to happen in the persona's day; the engine decides whether and what, and
//! the persona tells the user (spec
//! docs/superpowers/specs/2026-10-13-life-events-design.md §5).

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_core::events::{self, DrawInput, EventKind};
use eros_engine_core::scope::MemoryScope;
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::{ChatMessage, ChatRepo, EventRecord};

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, StreamPreError};
use crate::holiday::{self, UserLocale};
use crate::pipeline::handlers::recall_query_text;
use crate::pipeline::proactive::{greet, Greeting, Occasion};
use crate::prompt::NowContext;
use crate::routes::companion::{
    validate_llm_audit, validate_prompt_traits, LlmAuditDto, PromptTraitDto,
};
use crate::routes::companion_stream::{
    resolve_text_turn, validate_tier, AffinityScopeDto, StreamPreErrorBody,
};
use crate::routes::session_open::GreetingDto;
use crate::state::AppState;

/// Same per-user in-flight cap as every other LLM entry point.
const CONCURRENT_STREAMS_PER_USER: u32 = 3;
const MAX_EVENT_KEY_CHARS: usize = 128;

/// A kind of life event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventKindDto {
    TempWork,
    ExMessage,
    WeatherChange,
    PoliceEncounter,
    AnimalEncounter,
}

impl From<EventKindDto> for EventKind {
    fn from(k: EventKindDto) -> Self {
        match k {
            EventKindDto::TempWork => EventKind::TempWork,
            EventKindDto::ExMessage => EventKind::ExMessage,
            EventKindDto::WeatherChange => EventKind::WeatherChange,
            EventKindDto::PoliceEncounter => EventKind::PoliceEncounter,
            EventKindDto::AnimalEncounter => EventKind::AnimalEncounter,
        }
    }
}

impl From<EventKind> for EventKindDto {
    fn from(k: EventKind) -> Self {
        match k {
            EventKind::TempWork => EventKindDto::TempWork,
            EventKind::ExMessage => EventKindDto::ExMessage,
            EventKind::WeatherChange => EventKindDto::WeatherChange,
            EventKind::PoliceEncounter => EventKindDto::PoliceEncounter,
            EventKind::AnimalEncounter => EventKindDto::AnimalEncounter,
        }
    }
}

/// A life event: what happened, when it started and ends, and the message
/// that told it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct EventView {
    pub kind: EventKindDto,
    pub started_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub message_id: Uuid,
}

impl From<&EventRecord> for EventView {
    fn from(e: &EventRecord) -> Self {
        Self {
            kind: e.kind.into(),
            started_at: e.started_at,
            ends_at: e.ends_at,
            message_id: e.message_id,
        }
    }
}

#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct DrawEventRequest {
    /// The caller's idempotency key, scoped to the session: the same key
    /// returns the same event. 1–128 chars of `[A-Za-z0-9_.:-]`.
    pub event_key: String,
    /// The kinds that may be drawn. Absent: all of them.
    #[serde(default)]
    pub kinds: Option<Vec<EventKindDto>>,
    /// As on `/open`: the user's IANA timezone, for `[now]`.
    #[serde(default)]
    pub user_timezone: Option<String>,
    #[serde(default)]
    pub user_country: Option<String>,
    #[serde(default)]
    pub user_region: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub prompt_traits: Option<Vec<PromptTraitDto>>,
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub memory_scope: Option<MemoryScope>,
    #[serde(default)]
    pub affinity_scope: Option<AffinityScopeDto>,
    #[serde(default)]
    pub audit: Option<LlmAuditDto>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DrawEventResponse {
    /// The event drawn (or the key's existing one); `null` when nothing
    /// happened.
    pub event: Option<EventView>,
    /// The persona's message telling it; `null` with `event`.
    pub message: Option<GreetingDto>,
}

impl DrawEventResponse {
    fn none() -> Self {
        Self {
            event: None,
            message: None,
        }
    }

    fn from_row(row: Option<ChatMessage>) -> Self {
        match row.as_ref().and_then(EventRecord::from_message) {
            Some(e) => Self {
                event: Some(EventView::from(&e)),
                message: row.map(Into::into),
            },
            None => Self::none(),
        }
    }
}

/// Ask for something to happen in the persona's day.
///
/// The engine draws one kind of event, or none (an event is under way, or
/// every allowed kind is cooling down). The persona then tells the user in
/// one proactive message, persisted like any reply and returned with the
/// event. The same `event_key` returns the same event.
#[utoipa::path(
    post,
    path = "/v2/comp/session/{session_id}/events",
    tag = "companion",
    params(("session_id" = Uuid, Path, description = "Chat session id")),
    request_body = DrawEventRequest,
    responses(
        (status = 200, body = DrawEventResponse),
        (status = 400, body = StreamPreErrorBody),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, body = StreamPreErrorBody),
        (status = 404, body = StreamPreErrorBody),
        (status = 409, body = StreamPreErrorBody),
        (status = 422, description = "unknown kind or another malformed body field"),
        (status = 429, description = "per-user in-flight cap reached"),
        (status = "5XX", description = "companion chain exhausted; the provider's own status \
            passes through. Nothing is persisted.")
    ),
    security(("bearer" = []))
)]
async fn draw_event(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<DrawEventRequest>,
) -> Result<Json<DrawEventResponse>, AppError> {
    let roll = rand::thread_rng().gen::<f64>();
    draw_event_at(&state, session_id, user_id, req, Utc::now(), roll)
        .await
        .map(Json)
}

fn invalid_payload(message: String) -> AppError {
    AppError::StreamPre(StreamPreError {
        status: StatusCode::BAD_REQUEST,
        code: "invalid_payload",
        message,
        user_message: "请求无效".into(),
        original_user_message_id: None,
    })
}

/// The handler with its clock and roll injected, for tests.
pub(crate) async fn draw_event_at(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    req: DrawEventRequest,
    now: DateTime<Utc>,
    roll: f64,
) -> Result<DrawEventResponse, AppError> {
    validate_tier(req.tier.as_deref())?;
    let prompt_traits = validate_prompt_traits(req.prompt_traits.as_deref().unwrap_or(&[]))
        .map_err(|e| invalid_payload(e.to_string()))?;
    let audit =
        validate_llm_audit(req.audit.clone()).map_err(|e| invalid_payload(e.to_string()))?;
    let key = req.event_key.clone();
    let well_formed = (1..=MAX_EVENT_KEY_CHARS).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'));
    if !well_formed {
        return Err(invalid_payload(format!(
            "event_key must be 1..={MAX_EVENT_KEY_CHARS} chars of [A-Za-z0-9_.:-]"
        )));
    }
    let (_session, persona, instance_id) = resolve_text_turn(state, session_id, user_id).await?;

    let chat_repo = ChatRepo { pool: &state.pool };
    if let Some(row) = chat_repo.event_message(session_id, &key).await? {
        return Ok(DrawEventResponse::from_row(Some(row)));
    }

    let needs = if state.model_config.pde_enabled() {
        crate::pipeline::handlers::load_needs(&state.pool, instance_id, None, now).await?
    } else {
        None
    };
    let mut affinity = AffinityRepo { pool: &state.pool }
        .load_or_create(session_id, user_id, instance_id)
        .await?;
    affinity.apply_time_decay();
    affinity.refresh_endpoints(&state.config.affinity_tuning);
    let history: Vec<events::PastEvent> = chat_repo
        .recent_events(instance_id, now - events::longest_cooldown())
        .await?
        .iter()
        .map(EventRecord::past)
        .collect();
    let allowed: Vec<EventKind> = match &req.kinds {
        Some(kinds) => kinds.iter().map(|k| (*k).into()).collect(),
        None => EventKind::ALL.to_vec(),
    };
    let Some(kind) = events::draw(
        &DrawInput {
            now,
            needs,
            intimacy_rung: affinity.intimacy_rung(),
            history: &history,
            allowed: &allowed,
        },
        roll,
    ) else {
        return Ok(DrawEventResponse::none());
    };

    let _guard = state
        .stream_slots
        .try_acquire(user_id, CONCURRENT_STREAMS_PER_USER)
        .ok_or_else(|| AppError::TooManyRequests("per-user in-flight cap reached".into()))?;
    let locale = UserLocale::resolve(
        req.user_timezone.as_deref(),
        req.user_country.as_deref(),
        req.user_region.as_deref(),
    );
    let mut metadata = serde_json::Map::new();
    holiday::record_raw(
        &mut metadata,
        req.user_timezone.as_deref(),
        req.user_country.as_deref(),
        req.user_region.as_deref(),
    );
    let recall_query = chat_repo
        .latest_user_message(session_id)
        .await?
        .as_ref()
        .map(recall_query_text)
        .unwrap_or_default();
    let now_ctx = NowContext::for_user(&locale, now);
    let row = greet(
        state,
        Greeting {
            session_id,
            user_id,
            instance_id,
            persona: &persona,
            now: &now_ctx,
            occasion: Occasion::Event {
                event_key: key,
                kind,
            },
            recall_query,
            since: now,
            tier: req.tier,
            prompt_traits,
            memory_scope: req.memory_scope.unwrap_or_default(),
            affinity_scope: req
                .affinity_scope
                .as_ref()
                .map(AffinityScopeDto::resolve)
                .unwrap_or_default(),
            audit,
            metadata,
        },
    )
    .await?;
    Ok(DrawEventResponse::from_row(row))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(draw_event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlx::PgPool;
    use std::sync::Arc;
    use wiremock::matchers::{body_string_contains, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{seed_genome, seed_instance, seed_session};

    fn with_companion(mut state: AppState, mock_uri: &str) -> AppState {
        state.model_config = Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"companion\"\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{mock_uri}/api/v1/chat/completions"),
            ),
        );
        state
    }

    async fn mount_companion(mock: &MockServer, reply: &str) {
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("you are a companion"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "gen-event",
                "model": "served/companion",
                "choices": [{"message": {"content": reply}}],
            })))
            .mount(mock)
            .await;
    }

    async fn calls(mock: &MockServer) -> Vec<serde_json::Value> {
        mock.received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    /// A persona (`seed_genome`'s prompt is "you are a companion") with one
    /// text session; returns (instance, session).
    async fn setup(pool: &PgPool, user_id: Uuid) -> (Uuid, Uuid) {
        let genome_id = seed_genome(pool, "Aria").await;
        let instance_id = seed_instance(pool, genome_id, user_id).await;
        let session_id = seed_session(pool, user_id, instance_id).await;
        (instance_id, session_id)
    }

    fn req(key: &str, kinds: Option<Vec<EventKindDto>>) -> DrawEventRequest {
        DrawEventRequest {
            event_key: key.into(),
            kinds,
            user_timezone: Some("Asia/Taipei".into()),
            ..Default::default()
        }
    }

    async fn event_rows(pool: &PgPool, session_id: Uuid) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
             WHERE session_id = $1 AND metadata->>'proactive' = 'event'",
        )
        .bind(session_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_draw_tells_the_event_and_returns_it(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "刚被老板叫去加班，晚点聊").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let now = Utc::now() - chrono::Duration::minutes(1);

        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k1", Some(vec![EventKindDto::TempWork])),
            now,
            0.5,
        )
        .await
        .unwrap();
        let e = out.event.expect("an event");
        let m = out.message.expect("a message");
        assert_eq!(e.kind, EventKindDto::TempWork);
        assert_eq!(e.message_id, m.message_id);
        assert_eq!(m.content, "刚被老板叫去加班，晚点聊");
        assert_eq!(
            m.occasion,
            crate::routes::session_open::GreetingOccasion::Event
        );
        assert_eq!((e.ends_at - e.started_at).num_hours(), 4);
        assert!(e.started_at > now, "the event's time is when it is written");

        let meta: serde_json::Value =
            sqlx::query_scalar("SELECT metadata FROM engine.chat_messages WHERE id = $1")
                .bind(m.message_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(meta["proactive"], json!("event"));
        assert_eq!(meta["event_key"], json!("k1"));
        assert_eq!(meta["event"]["kind"], json!("temp_work"));
        assert!(meta["event"]["ends_at"].is_string());
        assert_eq!(meta["user_timezone"], json!("Asia/Taipei"));

        let c = calls(&mock).await;
        assert_eq!(c.len(), 1);
        let msgs = c[0]["messages"].as_array().unwrap();
        let last = msgs.last().unwrap();
        assert_eq!(last["role"], json!("user"));
        assert_eq!(
            last["content"],
            json!("（你刚临时接到一份工作，得马上去处理。）")
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_same_key_returns_the_same_event(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "外面突然下大雨了").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let kinds = Some(vec![EventKindDto::WeatherChange]);
        let first = draw_event_at(
            &state,
            session_id,
            user_id,
            req("same", kinds.clone()),
            Utc::now(),
            0.1,
        )
        .await
        .unwrap();
        let again = draw_event_at(
            &state,
            session_id,
            user_id,
            req("same", kinds),
            Utc::now(),
            0.9,
        )
        .await
        .unwrap();
        assert_eq!(
            first.message.as_ref().map(|m| m.message_id),
            again.message.as_ref().map(|m| m.message_id)
        );
        assert!(again.event.is_some());
        assert_eq!(calls(&mock).await.len(), 1, "no second generation");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn nothing_is_drawn_while_an_event_is_under_way(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "路上遇到一只小猫").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let first = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k1", Some(vec![EventKindDto::AnimalEncounter])),
            Utc::now(),
            0.1,
        )
        .await
        .unwrap();
        assert!(first.event.is_some());
        let second = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k2", None),
            Utc::now(),
            0.1,
        )
        .await
        .unwrap();
        assert!(second.event.is_none() && second.message.is_none());
        assert_eq!(calls(&mock).await.len(), 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_event_in_another_session_blocks_the_draw(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id) = setup(&pool, user_id).await;
        let other = seed_session(&pool, user_id, instance_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "被警察拦下来问话了").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        assert!(draw_event_at(
            &state,
            other,
            user_id,
            req("k1", Some(vec![EventKindDto::PoliceEncounter])),
            Utc::now(),
            0.1
        )
        .await
        .unwrap()
        .event
        .is_some());
        let here = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k2", None),
            Utc::now(),
            0.1,
        )
        .await
        .unwrap();
        assert!(
            here.event.is_none(),
            "the persona is already in the middle of something"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_kind_in_cooldown_is_not_drawn(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id) = setup(&pool, user_id).await;
        // A weather change that started 5 hours ago and ended an hour ago:
        // over, but still inside its 8-hour cooldown.
        let started = Utc::now() - chrono::Duration::hours(5);
        let repo = ChatRepo { pool: &pool };
        repo.insert_event_message(
            instance_id,
            session_id,
            started,
            &eros_engine_store::chat::ProactiveInsert {
                id: Uuid::new_v4(),
                content: "下雨了".into(),
                generation_id: None,
                metadata: json!({"proactive": "event", "event_key": "old",
                    "event": {"kind": "weather_change",
                              "ends_at": (started + chrono::Duration::hours(4)).to_rfc3339()}}),
                llm_attempts: None,
                gateway_errors: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let mock = MockServer::start().await;
        mount_companion(&mock, "又下雨了").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k1", Some(vec![EventKindDto::WeatherChange])),
            Utc::now(),
            0.1,
        )
        .await
        .unwrap();
        assert!(out.event.is_none());
        assert!(calls(&mock).await.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn ex_message_needs_closeness(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "前任给我发消息了").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        // A fresh session sits on rung 1; the ex only writes from rung 2.
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k1", Some(vec![EventKindDto::ExMessage])),
            Utc::now(),
            0.5,
        )
        .await
        .unwrap();
        assert!(out.event.is_none() && out.message.is_none());
        assert!(calls(&mock).await.is_empty());
        assert_eq!(event_rows(&pool, session_id).await, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_empty_allowlist_draws_nothing(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k1", Some(vec![])),
            Utc::now(),
            0.5,
        )
        .await
        .unwrap();
        assert!(out.event.is_none());
        assert!(calls(&mock).await.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_blank_reply_leaves_no_event(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "   ").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let out = draw_event_at(
            &state,
            session_id,
            user_id,
            req("k1", Some(vec![EventKindDto::TempWork])),
            Utc::now(),
            0.5,
        )
        .await
        .unwrap();
        assert!(out.event.is_none() && out.message.is_none());
        assert_eq!(event_rows(&pool, session_id).await, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_malformed_key_is_rejected(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let state = test_state(pool.clone());
        for key in ["", "has space", &"x".repeat(129)] {
            let err = draw_event_at(&state, session_id, user_id, req(key, None), Utc::now(), 0.5)
                .await
                .err()
                .unwrap_or_else(|| panic!("{key:?} accepted"));
            assert!(
                matches!(
                    err,
                    AppError::StreamPre(StreamPreError {
                        status: StatusCode::BAD_REQUEST,
                        ..
                    })
                ),
                "{key:?}"
            );
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_session_gate_applies(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = setup(&pool, user_id).await;
        let state = test_state(pool.clone());
        let status = |e: AppError| match e {
            AppError::StreamPre(p) => p.status,
            other => panic!("{other:?}"),
        };
        let stranger = draw_event_at(
            &state,
            session_id,
            Uuid::new_v4(),
            req("k", None),
            Utc::now(),
            0.5,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(status(stranger), StatusCode::FORBIDDEN);
        let missing = draw_event_at(
            &state,
            Uuid::new_v4(),
            user_id,
            req("k", None),
            Utc::now(),
            0.5,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(status(missing), StatusCode::NOT_FOUND);
    }

    #[test]
    fn an_unknown_kind_fails_deserialization() {
        assert!(serde_json::from_value::<DrawEventRequest>(
            json!({"event_key": "k", "kinds": ["dragon"]})
        )
        .is_err());
        assert!(
            serde_json::from_value::<DrawEventRequest>(json!({"kinds": []})).is_err(),
            "event_key is required"
        );
    }
}
