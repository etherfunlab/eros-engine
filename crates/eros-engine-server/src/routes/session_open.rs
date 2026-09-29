// SPDX-License-Identifier: AGPL-3.0-only
//! POST /v2/comp/session/{session_id}/open — the client reports that the user
//! just entered a session. On a user-local holiday nobody has spoken on yet,
//! the persona speaks first.
//!
//! Spec: docs/superpowers/specs/2026-09-26-user-locale-and-holiday-greeting-design.md §4.2

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_core::scope::MemoryScope;
use eros_engine_store::chat::{ChatMessage, ChatRepo};

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, StreamPreError};
use crate::holiday::{self, UserLocale};
use crate::pipeline::proactive::{greet, Greeting};
use crate::prompt::NowContext;
use crate::routes::companion::{
    validate_llm_audit, validate_prompt_traits, LlmAuditDto, PromptTraitDto,
};
use crate::routes::companion_stream::{
    resolve_text_turn, validate_tier, AffinityScopeDto, StreamPreErrorBody,
};
use crate::state::AppState;

/// Same per-user in-flight cap as every other LLM entry point.
const CONCURRENT_STREAMS_PER_USER: u32 = 3;

#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct OpenSessionRequest {
    /// The user's IANA timezone. Without a valid one there is no user-local
    /// date and `greeting` is `null`.
    #[serde(default)]
    pub user_timezone: Option<String>,
    /// ISO 3166-1 alpha-2 country — same rules as the chat body.
    #[serde(default)]
    pub user_country: Option<String>,
    /// ISO 3166-2 subdivision without the country prefix — same rules as the
    /// chat body.
    #[serde(default)]
    pub user_region: Option<String>,
    /// Same as the chat body: model routing and trait gating for the greeting.
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
pub struct OpenSessionResponse {
    /// The persona's first message today, or `null` when it does not speak first.
    pub greeting: Option<GreetingDto>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GreetingDto {
    pub message_id: Uuid,
    pub content: String,
    pub sent_at: DateTime<Utc>,
}

impl From<ChatMessage> for GreetingDto {
    fn from(m: ChatMessage) -> Self {
        Self {
            message_id: m.id,
            content: m.content,
            sent_at: m.sent_at,
        }
    }
}

/// Tell the engine the user just entered this session.
///
/// When it is a holiday in the user's timezone and nothing has been said in
/// the session since the user's local midnight, the persona speaks first: one
/// assistant message is generated, persisted (unread, like any reply) and
/// returned. Otherwise `greeting` is `null`. Idempotent per session and
/// user-local date — a repeat call returns the same greeting.
#[utoipa::path(
    post,
    path = "/v2/comp/session/{session_id}/open",
    tag = "companion",
    params(("session_id" = Uuid, Path, description = "Chat session id")),
    request_body = OpenSessionRequest,
    responses(
        (status = 200, body = OpenSessionResponse),
        (status = 400, body = StreamPreErrorBody),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, body = StreamPreErrorBody),
        (status = 404, body = StreamPreErrorBody),
        (status = 409, body = StreamPreErrorBody),
        (status = 429, description = "per-user in-flight cap reached"),
        (status = "5XX", description = "companion chain exhausted; the provider's own status \
            passes through, same body as the image endpoints. Nothing is persisted.")
    ),
    security(("bearer" = []))
)]
async fn open_session(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<OpenSessionRequest>,
) -> Result<Json<OpenSessionResponse>, AppError> {
    open_at(&state, session_id, user_id, req, Utc::now())
        .await
        .map(Json)
}

fn no_greeting() -> Result<OpenSessionResponse, AppError> {
    Ok(OpenSessionResponse { greeting: None })
}

/// The handler with its clock injected, so tests can pin a holiday.
pub(crate) async fn open_at(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    req: OpenSessionRequest,
    now: DateTime<Utc>,
) -> Result<OpenSessionResponse, AppError> {
    validate_tier(req.tier.as_deref())?;
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
    let (_session, persona, instance_id) = resolve_text_turn(state, session_id, user_id).await?;

    let locale = UserLocale::resolve(
        req.user_timezone.as_deref(),
        req.user_country.as_deref(),
        req.user_region.as_deref(),
    );
    let (Some(tz), Some(local_date)) = (locale.timezone, locale.local_date(now)) else {
        return no_greeting();
    };
    let holidays = holiday::holidays_on(local_date, locale.country, locale.region);
    if holidays.is_empty() {
        return no_greeting();
    }

    let chat_repo = ChatRepo { pool: &state.pool };
    if let Some(row) = chat_repo
        .proactive_greeting_on(session_id, &local_date.to_string())
        .await?
    {
        return Ok(OpenSessionResponse {
            greeting: Some(row.into()),
        });
    }
    if chat_repo
        .has_message_since(session_id, holiday::local_midnight_utc(tz, local_date))
        .await?
    {
        // A concurrent caller's greeting can land between the lookup above
        // and this check (its own insert counts as "a message since" too):
        // re-check for that winner's row rather than reporting a bare null.
        return Ok(OpenSessionResponse {
            greeting: chat_repo
                .proactive_greeting_on(session_id, &local_date.to_string())
                .await?
                .map(Into::into),
        });
    }

    let _guard = state
        .stream_slots
        .try_acquire(user_id, CONCURRENT_STREAMS_PER_USER)
        .ok_or_else(|| AppError::TooManyRequests("per-user in-flight cap reached".into()))?;
    let mut metadata = serde_json::Map::new();
    holiday::record_raw(
        &mut metadata,
        req.user_timezone.as_deref(),
        req.user_country.as_deref(),
        req.user_region.as_deref(),
    );
    let now_ctx = NowContext::for_user(&locale, now);
    let greeting = greet(
        state,
        Greeting {
            session_id,
            user_id,
            instance_id,
            persona: &persona,
            now: &now_ctx,
            local_date,
            holidays,
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
    Ok(OpenSessionResponse {
        greeting: greeting.map(Into::into),
    })
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(open_session))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use chrono::TimeZone;
    use serde_json::json;
    use sqlx::PgPool;
    use std::sync::Arc;
    use wiremock::matchers::{body_partial_json, body_string_contains, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{
        build_router, mint_test_jwt, seed_genome, seed_instance, seed_session, send_request,
    };

    /// 2026-09-25 04:00 in Taipei: 中秋节, and TW's Mid-Autumn Festival.
    fn mid_autumn_morning() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 24, 20, 0, 0).unwrap()
    }

    /// Taipei's local midnight on that day.
    fn taipei_midnight() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 24, 16, 0, 0).unwrap()
    }

    fn taipei() -> OpenSessionRequest {
        OpenSessionRequest {
            user_timezone: Some("Asia/Taipei".into()),
            user_country: Some("TW".into()),
            ..Default::default()
        }
    }

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

    /// Companion mock keyed on the seeded genome's system prompt.
    async fn mount_companion(mock: &MockServer, reply: &str) {
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("you are a companion"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "gen-greet",
                "model": "served/companion",
                "choices": [{"message": {"content": reply}}],
            })))
            .mount(mock)
            .await;
    }

    async fn companion_calls(mock: &MockServer) -> Vec<serde_json::Value> {
        mock.received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    /// A session whose two history rows were both sent at `at`.
    async fn session_with_history(pool: &PgPool, user_id: Uuid, at: DateTime<Utc>) -> Uuid {
        let genome_id = seed_genome(pool, "Aria").await;
        let instance_id = seed_instance(pool, genome_id, user_id).await;
        let session_id = seed_session(pool, user_id, instance_id).await;
        for (role, content) in [("user", "晚安"), ("assistant", "晚安，明天见")] {
            sqlx::query(
                "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(session_id)
            .bind(role)
            .bind(content)
            .bind(at)
            .execute(pool)
            .await
            .unwrap();
        }
        session_id
    }

    async fn proactive_rows(pool: &PgPool, session_id: Uuid) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
             WHERE session_id = $1 AND assistant_action_type = 'proactive'",
        )
        .bind(session_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn greets_on_a_holiday_nobody_has_spoken_on(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = session_with_history(
            &pool,
            user_id,
            mid_autumn_morning() - chrono::Duration::days(2),
        )
        .await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "中秋快乐呀，今天有吃月饼吗").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let out = open_at(&state, session_id, user_id, taipei(), mid_autumn_morning())
            .await
            .unwrap();
        let g = out.greeting.expect("a greeting");
        assert_eq!(g.content, "中秋快乐呀，今天有吃月饼吗");

        let (action, umid, read_at, meta): (
            Option<String>,
            Option<Uuid>,
            Option<DateTime<Utc>>,
            serde_json::Value,
        ) = sqlx::query_as(
            "SELECT assistant_action_type, user_message_id, read_at, metadata \
             FROM engine.chat_messages WHERE id = $1",
        )
        .bind(g.message_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(action.as_deref(), Some("proactive"));
        assert_eq!(umid, None);
        assert_eq!(read_at, None, "unread like any reply");
        assert_eq!(meta["proactive"], json!("holiday"));
        assert_eq!(meta["local_date"], json!("2026-09-25"));
        assert_eq!(meta["holidays"], json!(["中秋节", "Mid-Autumn Festival"]));
        assert_eq!(meta["user_timezone"], json!("Asia/Taipei"));
        assert_eq!(meta["user_country"], json!("TW"));
        assert_eq!(meta["prompt_traits"], json!([]));

        // The holiday is a fact in [now]; the engine's cue closes the request
        // as a user turn.
        let calls = companion_calls(&mock).await;
        assert_eq!(calls.len(), 1);
        let messages = calls[0]["messages"].as_array().unwrap();
        let system = messages[0]["content"].as_str().unwrap();
        assert!(
            system.contains("对方那边今天是中秋节、Mid-Autumn Festival"),
            "{system}"
        );
        let last = messages.last().unwrap();
        assert_eq!(last["role"], json!("user"));
        assert_eq!(last["content"], json!(crate::pipeline::proactive::OPEN_CUE));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn no_timezone_or_no_holiday_means_no_greeting_and_no_call(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = session_with_history(
            &pool,
            user_id,
            mid_autumn_morning() - chrono::Duration::days(2),
        )
        .await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "不该被调用").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let no_tz = OpenSessionRequest {
            user_country: Some("TW".into()),
            ..Default::default()
        };
        let out = open_at(&state, session_id, user_id, no_tz, mid_autumn_morning())
            .await
            .unwrap();
        assert!(out.greeting.is_none());

        // 2026-09-22 in Taipei: no lunar festival, no TW holiday, no fixed date.
        // Fresh session with history well before this day's Taipei midnight, so
        // only the holiday gate (not `has_message_since`) can produce the null.
        let plain_day = Utc.with_ymd_and_hms(2026, 9, 21, 20, 0, 0).unwrap();
        let plain_day_session =
            session_with_history(&pool, user_id, plain_day - chrono::Duration::days(2)).await;
        let out = open_at(&state, plain_day_session, user_id, taipei(), plain_day)
            .await
            .unwrap();
        assert!(out.greeting.is_none());
        assert!(companion_calls(&mock).await.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_message_since_local_midnight_means_no_greeting(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = session_with_history(
            &pool,
            user_id,
            taipei_midnight() + chrono::Duration::hours(1),
        )
        .await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "不该被调用").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let out = open_at(&state, session_id, user_id, taipei(), mid_autumn_morning())
            .await
            .unwrap();
        assert!(out.greeting.is_none());
        assert!(companion_calls(&mock).await.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_message_just_before_local_midnight_does_not_block(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = session_with_history(
            &pool,
            user_id,
            taipei_midnight() - chrono::Duration::minutes(1),
        )
        .await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "中秋快乐").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let out = open_at(&state, session_id, user_id, taipei(), mid_autumn_morning())
            .await
            .unwrap();
        assert!(out.greeting.is_some());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_second_open_returns_the_same_greeting_without_calling_again(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = session_with_history(
            &pool,
            user_id,
            mid_autumn_morning() - chrono::Duration::days(2),
        )
        .await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "中秋快乐").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let first = open_at(&state, session_id, user_id, taipei(), mid_autumn_morning())
            .await
            .unwrap()
            .greeting
            .unwrap();
        let later = mid_autumn_morning() + chrono::Duration::hours(3);
        let second = open_at(&state, session_id, user_id, taipei(), later)
            .await
            .unwrap()
            .greeting
            .unwrap();
        assert_eq!(first.message_id, second.message_id);
        assert_eq!(companion_calls(&mock).await.len(), 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn concurrent_opens_write_one_greeting(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = session_with_history(
            &pool,
            user_id,
            mid_autumn_morning() - chrono::Duration::days(2),
        )
        .await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "中秋快乐").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let (a, b) = tokio::join!(
            open_at(&state, session_id, user_id, taipei(), mid_autumn_morning()),
            open_at(&state, session_id, user_id, taipei(), mid_autumn_morning()),
        );
        let (a, b) = (a.unwrap().greeting.unwrap(), b.unwrap().greeting.unwrap());
        assert_eq!(a.message_id, b.message_id);
        assert_eq!(proactive_rows(&pool, session_id).await, 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_blank_reply_writes_nothing(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = session_with_history(
            &pool,
            user_id,
            mid_autumn_morning() - chrono::Duration::days(2),
        )
        .await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "   ").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let out = open_at(&state, session_id, user_id, taipei(), mid_autumn_morning())
            .await
            .unwrap();
        assert!(out.greeting.is_none());
        assert_eq!(proactive_rows(&pool, session_id).await, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_failed_call_surfaces_upstream_and_writes_nothing(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = session_with_history(
            &pool,
            user_id,
            mid_autumn_morning() - chrono::Duration::days(2),
        )
        .await;
        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let err = open_at(&state, session_id, user_id, taipei(), mid_autumn_morning())
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Upstream(_)), "{err:?}");
        assert_eq!(proactive_rows(&pool, session_id).await, 0);
    }

    /// `with_companion` chained `companion` → `backup`, plus one neutral
    /// `output_regex` rule on `rule_model` that deletes `[note: …]`.
    fn with_note_rule(state: AppState, mock_uri: &str, rule_model: &str) -> AppState {
        let mut state = with_companion(state, mock_uri);
        let cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(&format!(
            r#"
            [tasks.chat_companion]
            model = "companion"
            fallback = ["backup"]

            [[tasks.chat_companion.output_regex]]
            models = ["{rule_model}"]
            pattern = '\s*\[note:[^\]]*\]'
            "#
        ))
        .unwrap();
        state.output_regex = Arc::new(cfg.compile_output_regex().expect("rule compiles"));
        state.model_config = Arc::new(cfg);
        state
    }

    async fn greeting_row_content(pool: &PgPool, session_id: Uuid) -> String {
        sqlx::query_scalar(
            "SELECT content FROM engine.chat_messages \
             WHERE session_id = $1 AND assistant_action_type = 'proactive'",
        )
        .bind(session_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_greeting_goes_through_output_regex(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = session_with_history(
            &pool,
            user_id,
            mid_autumn_morning() - chrono::Duration::days(2),
        )
        .await;
        let mock = MockServer::start().await;
        // The mock reports "served/companion"; the rule is keyed on the chain
        // hop's config id, as on the stream path.
        mount_companion(&mock, "中秋快乐呀 [note: a marker]").await;
        let state = with_note_rule(test_state(pool.clone()), &mock.uri(), "companion");

        let out = open_at(&state, session_id, user_id, taipei(), mid_autumn_morning())
            .await
            .unwrap();
        assert_eq!(out.greeting.expect("a greeting").content, "中秋快乐呀");
        assert_eq!(greeting_row_content(&pool, session_id).await, "中秋快乐呀");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_greeting_that_strips_to_empty_writes_nothing(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = session_with_history(
            &pool,
            user_id,
            mid_autumn_morning() - chrono::Duration::days(2),
        )
        .await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "[note: nothing else]").await;
        let state = with_note_rule(test_state(pool.clone()), &mock.uri(), "companion");

        let out = open_at(&state, session_id, user_id, taipei(), mid_autumn_morning())
            .await
            .unwrap();
        assert!(out.greeting.is_none());
        assert_eq!(proactive_rows(&pool, session_id).await, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_fallback_greeting_takes_the_fallbacks_rules(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = session_with_history(
            &pool,
            user_id,
            mid_autumn_morning() - chrono::Duration::days(2),
        )
        .await;
        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_partial_json(json!({"model": "companion"})))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_partial_json(json!({"model": "backup"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "gen-backup",
                "model": "served/backup",
                "choices": [{"message": {"content": "中秋快乐 [note: a marker]"}}],
            })))
            .mount(&mock)
            .await;
        let state = with_note_rule(test_state(pool.clone()), &mock.uri(), "backup");

        let out = open_at(&state, session_id, user_id, taipei(), mid_autumn_morning())
            .await
            .unwrap();
        assert_eq!(out.greeting.expect("a greeting").content, "中秋快乐");
    }

    fn open_req(session_id: Uuid, jwt: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(format!("/v2/comp/session/{session_id}/open"))
            .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_route_is_wired_owned_and_text_only(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let mut app = build_router(test_state(pool.clone()));

        let (status, body) = send_request(
            &mut app,
            open_req(session_id, &mint_test_jwt(user_id), json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"greeting": null}));

        let (status, _) = send_request(
            &mut app,
            open_req(session_id, &mint_test_jwt(Uuid::new_v4()), json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        sqlx::query("UPDATE engine.chat_sessions SET channel = 'voice' WHERE id = $1")
            .bind(session_id)
            .execute(&pool)
            .await
            .unwrap();
        let (status, body) = send_request(
            &mut app,
            open_req(session_id, &mint_test_jwt(user_id), json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], json!("wrong_channel"));
    }

    /// `prompt_traits`/`audit` validation failures render in the same shape
    /// as the chat stream's 400s (`StreamPreErrorBody`), not the bare
    /// `bad_request` body `validate_prompt_traits` raises on its own.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn open_400s_on_invalid_prompt_traits_in_the_chat_shape(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let mut app = build_router(test_state(pool.clone()));

        let (status, body) = send_request(
            &mut app,
            open_req(
                session_id,
                &mint_test_jwt(user_id),
                json!({"prompt_traits": [{"tag": "Bad", "text": "hi"}]}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], json!("invalid_payload"));
    }

    #[test]
    fn openapi_lists_the_open_route() {
        let api = crate::routes::openapi_for_tests();
        assert!(api["paths"]["/v2/comp/session/{session_id}/open"]["post"].is_object());
    }
}
