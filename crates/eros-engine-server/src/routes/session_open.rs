// SPDX-License-Identifier: AGPL-3.0-only
//! POST /v2/comp/session/{session_id}/open — the client reports that the user
//! just entered a session. On the persona's birthday or a user-local holiday
//! nobody has spoken on yet, or on an occasion the client names, the persona
//! speaks first.
//!
//! Specs: docs/superpowers/specs/2026-09-26-user-locale-and-holiday-greeting-design.md §4.2,
//! docs/superpowers/specs/2026-10-03-open-occasions-design.md §3–§4,
//! docs/superpowers/specs/2026-10-10-persona-origin-and-birthday-design.md §5

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_core::persona::CompanionPersona;
use eros_engine_core::scope::MemoryScope;
use eros_engine_store::chat::{ChatMessage, ChatRepo};

use crate::auth::middleware::AuthUser;
use crate::birthday::Birthday;
use crate::error::{AppError, StreamPreError};
use crate::holiday::{self, UserLocale};
use crate::pipeline::handlers::recall_query_text;
use crate::pipeline::proactive::{greet, Greeting, Occasion};
use crate::prompt::{meta_str, persona_clock_tz, NowContext};
use crate::routes::companion::{
    validate_llm_audit, validate_prompt_traits, LlmAuditDto, PromptTraitDto,
};
use crate::routes::companion_stream::{
    resolve_text_turn, validate_tier, AffinityScopeDto, StreamPreErrorBody,
};
use crate::state::AppState;

/// Same per-user in-flight cap as every other LLM entry point.
const CONCURRENT_STREAMS_PER_USER: u32 = 3;
const MAX_OPEN_KEY_CHARS: usize = 128;

#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct OpenSessionRequest {
    /// The user's IANA timezone. Without a valid one there is no user-local
    /// date and no holiday greeting. It is also the persona clock's fallback
    /// when the persona has no timezone of its own.
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
    /// Ask the persona to speak first for this reason. Absent: the engine
    /// decides, and speaks first only on the persona's birthday or a
    /// user-local holiday.
    #[serde(default)]
    pub occasion: Option<OpenOccasion>,
    /// The caller's idempotency key for `occasion`, scoped to the session:
    /// the same key returns the same message. Required with `occasion`;
    /// 1–128 chars of `[A-Za-z0-9_.:-]`.
    #[serde(default)]
    pub open_key: Option<String>,
}

/// A caller-named reason for the persona to speak first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpenOccasion {
    /// The session has no messages yet.
    FirstMeet,
    /// The user has spoken in this session before; the engine tells the
    /// persona how many days ago.
    Returning,
    /// No particular reason.
    JustOpened,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct OpenSessionResponse {
    /// The persona's first message, or `null` when it does not speak first.
    pub greeting: Option<GreetingDto>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GreetingDto {
    pub message_id: Uuid,
    pub content: String,
    pub sent_at: DateTime<Utc>,
    pub occasion: GreetingOccasion,
}

/// Why the persona spoke first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GreetingOccasion {
    Holiday,
    Birthday,
    FirstMeet,
    Returning,
    JustOpened,
}

impl From<ChatMessage> for GreetingDto {
    fn from(m: ChatMessage) -> Self {
        // Every proactive row records `metadata.proactive`; holiday greetings
        // were the only kind before caller-named occasions.
        let occasion = m
            .metadata
            .as_ref()
            .and_then(|v| v.get("proactive"))
            .and_then(|v| GreetingOccasion::deserialize(v).ok())
            .unwrap_or(GreetingOccasion::Holiday);
        Self {
            message_id: m.id,
            content: m.content,
            sent_at: m.sent_at,
            occasion,
        }
    }
}

/// Tell the engine the user just entered this session.
///
/// Without `occasion` the engine decides: when it is a holiday in the user's
/// timezone and nobody has spoken in the session since the user's local
/// midnight, the persona speaks first. With `occasion` the caller decides,
/// and the persona speaks first when the occasion is true of the session.
/// Either way one assistant message is generated, persisted (unread, like any
/// reply) and returned; otherwise `greeting` is `null`. Idempotent per
/// session and user-local date for the holiday, per session and `open_key`
/// for an occasion.
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
        (status = 422, description = "unknown `occasion` or another malformed body field"),
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

fn invalid_payload(message: String) -> AppError {
    AppError::StreamPre(StreamPreError {
        status: StatusCode::BAD_REQUEST,
        code: "invalid_payload",
        message,
        user_message: "请求无效".into(),
        original_user_message_id: None,
    })
}

/// `occasion` and `open_key` come together or not at all.
fn validate_opener(
    occasion: Option<OpenOccasion>,
    open_key: Option<String>,
) -> Result<Option<(OpenOccasion, String)>, AppError> {
    match (occasion, open_key) {
        (None, None) => Ok(None),
        (Some(_), None) => Err(invalid_payload("occasion requires open_key".into())),
        (None, Some(_)) => Err(invalid_payload("open_key requires occasion".into())),
        (Some(occasion), Some(key)) => {
            let well_formed = (1..=MAX_OPEN_KEY_CHARS).contains(&key.len())
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'));
            if !well_formed {
                return Err(invalid_payload(format!(
                    "open_key must be 1..={MAX_OPEN_KEY_CHARS} chars of [A-Za-z0-9_.:-]"
                )));
            }
            Ok(Some((occasion, key)))
        }
    }
}

/// What the route does once it has decided whether the persona speaks.
enum Next {
    /// Answer without generating: an existing message, or `null`.
    Answer(OpenSessionResponse),
    /// Generate for this occasion; `since` is the cutoff the insert re-checks.
    Speak {
        occasion: Occasion,
        recall_query: String,
        since: DateTime<Utc>,
    },
}

fn answer(row: Option<ChatMessage>) -> Next {
    Next::Answer(OpenSessionResponse {
        greeting: row.map(Into::into),
    })
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
        .map_err(|e| invalid_payload(e.to_string()))?;
    let audit =
        validate_llm_audit(req.audit.clone()).map_err(|e| invalid_payload(e.to_string()))?;
    let opener = validate_opener(req.occasion, req.open_key.clone())?;
    let (_session, persona, instance_id) = resolve_text_turn(state, session_id, user_id).await?;

    let locale = UserLocale::resolve(
        req.user_timezone.as_deref(),
        req.user_country.as_deref(),
        req.user_region.as_deref(),
    );
    let chat_repo = ChatRepo { pool: &state.pool };
    let next = match opener {
        None => engine_occasion(&chat_repo, session_id, &persona, locale, now).await?,
        Some((occasion, open_key)) => {
            caller_occasion(&chat_repo, session_id, occasion, open_key, now).await?
        }
    };
    let (occasion, recall_query, since) = match next {
        Next::Answer(resp) => return Ok(resp),
        Next::Speak {
            occasion,
            recall_query,
            since,
        } => (occasion, recall_query, since),
    };

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
            occasion,
            recall_query,
            since,
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

/// The engine's own call: speak first on the persona's birthday (persona
/// clock), else on a user-local holiday, when nobody has spoken since that
/// local midnight. A caller-named opener since then counts as having spoken.
async fn engine_occasion(
    chat_repo: &ChatRepo<'_>,
    session_id: Uuid,
    persona: &CompanionPersona,
    locale: UserLocale,
    now: DateTime<Utc>,
) -> Result<Next, AppError> {
    let persona_tz = persona_clock_tz(meta_str(persona, "timezone"), locale.timezone);
    let persona_date = now.with_timezone(&persona_tz).date_naive();
    if meta_str(persona, "birthday")
        .and_then(Birthday::parse)
        .is_some_and(|b| b.falls_on(persona_date))
    {
        return dated_occasion(
            chat_repo,
            session_id,
            persona_tz,
            persona_date,
            Occasion::Birthday {
                local_date: persona_date,
            },
            "生日".into(),
        )
        .await;
    }

    let (Some(tz), Some(local_date)) = (locale.timezone, locale.local_date(now)) else {
        return Ok(answer(None));
    };
    let holidays = holiday::holidays_on(local_date, locale.country, locale.region);
    if holidays.is_empty() {
        return Ok(answer(None));
    }
    let recall_query = holidays.join("、");
    dated_occasion(
        chat_repo,
        session_id,
        tz,
        local_date,
        Occasion::Holiday {
            local_date,
            holidays,
        },
        recall_query,
    )
    .await
}

/// A dated engine greeting: the row already written for `local_date`, `null`
/// when anyone spoke since that local midnight, else speak.
async fn dated_occasion(
    chat_repo: &ChatRepo<'_>,
    session_id: Uuid,
    tz: Tz,
    local_date: NaiveDate,
    occasion: Occasion,
    recall_query: String,
) -> Result<Next, AppError> {
    let since = holiday::local_midnight_utc(tz, local_date);
    let date = local_date.to_string();
    if let Some(row) = chat_repo.proactive_greeting_on(session_id, &date).await? {
        return Ok(answer(Some(row)));
    }
    if chat_repo.has_message_since(session_id, since).await? {
        // A concurrent caller's greeting can land between the lookup above
        // and this check (its own insert counts as "a message since" too):
        // re-check for that winner's row rather than reporting a bare null.
        return Ok(answer(
            chat_repo.proactive_greeting_on(session_id, &date).await?,
        ));
    }
    Ok(Next::Speak {
        occasion,
        recall_query,
        since,
    })
}

/// The caller's call: the key's existing message, `null` when the occasion
/// is not true of this session, or the occasion to speak for. The insert's
/// cutoff is the request's start, so a user who speaks meanwhile wins.
async fn caller_occasion(
    chat_repo: &ChatRepo<'_>,
    session_id: Uuid,
    occasion: OpenOccasion,
    open_key: String,
    now: DateTime<Utc>,
) -> Result<Next, AppError> {
    if let Some(row) = chat_repo.open_message(session_id, &open_key).await? {
        return Ok(answer(Some(row)));
    }
    // When a precondition fails, re-read the key: a same-key call can commit
    // between the lookup above and the precondition read (its own row makes
    // the session non-empty), and its row is this call's answer too.
    let last_user = chat_repo.latest_user_message(session_id).await?;
    let occasion = match occasion {
        OpenOccasion::FirstMeet => {
            if !chat_repo.history(session_id, 1, 0).await?.is_empty() {
                return Ok(answer(chat_repo.open_message(session_id, &open_key).await?));
            }
            Occasion::FirstMeet { open_key }
        }
        OpenOccasion::Returning => {
            let Some(last) = &last_user else {
                return Ok(answer(chat_repo.open_message(session_id, &open_key).await?));
            };
            Occasion::Returning {
                open_key,
                gap_days: (now - last.sent_at).num_days().max(0),
            }
        }
        OpenOccasion::JustOpened => Occasion::JustOpened { open_key },
    };
    Ok(Next::Speak {
        occasion,
        recall_query: last_user
            .as_ref()
            .map(recall_query_text)
            .unwrap_or_default(),
        since: now,
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
        assert_eq!(g.occasion, GreetingOccasion::Holiday);

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

    /// A greeting written for a different user-local date must not block
    /// today's — e.g. the user's timezone
    /// changed between opens. 2027-02-05 12:30 UTC is 2027-02-06 (春节) in
    /// Kiritimati (UTC+14) and 2027-02-05 (除夕) in Shanghai (UTC+8).
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_greeting_for_a_different_local_date_does_not_block_todays(pool: PgPool) {
        let user_id = Uuid::new_v4();
        // History well before either timezone's relevant local midnight
        // (Shanghai's, the earlier of the two: 2027-02-04 16:00 UTC).
        let session_id = session_with_history(
            &pool,
            user_id,
            Utc.with_ymd_and_hms(2027, 2, 2, 0, 0, 0).unwrap(),
        )
        .await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "过年好").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let at = Utc.with_ymd_and_hms(2027, 2, 5, 12, 30, 0).unwrap();
        let kiritimati = OpenSessionRequest {
            user_timezone: Some("Pacific/Kiritimati".into()),
            user_country: Some("CN".into()),
            ..Default::default()
        };
        let first = open_at(&state, session_id, user_id, kiritimati, at)
            .await
            .unwrap()
            .greeting
            .expect("Kiritimati's holiday greeting");

        let shanghai = OpenSessionRequest {
            user_timezone: Some("Asia/Shanghai".into()),
            user_country: Some("CN".into()),
            ..Default::default()
        };
        let second = open_at(&state, session_id, user_id, shanghai, at)
            .await
            .unwrap()
            .greeting
            .expect("Shanghai's own greeting must not be blocked by Kiritimati's");

        assert_ne!(first.message_id, second.message_id);
        assert_eq!(companion_calls(&mock).await.len(), 2);
        let mut local_dates: Vec<String> = sqlx::query_scalar(
            "SELECT metadata->>'local_date' FROM engine.chat_messages \
             WHERE session_id = $1 AND assistant_action_type = 'proactive'",
        )
        .bind(session_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        local_dates.sort();
        assert_eq!(local_dates, vec!["2027-02-05", "2027-02-06"]);
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

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_greeting_after_a_garbled_hop_takes_the_serving_hops_rules(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = session_with_history(
            &pool,
            user_id,
            mid_autumn_morning() - chrono::Duration::days(2),
        )
        .await;
        let mock = MockServer::start().await;
        // A garbled completion advances the chain without recording a failure,
        // and the backup reports the primary's id: only the hop `execute`
        // served says whose rules apply.
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_partial_json(json!({"model": "companion"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "Hi\u{0120}there\u{010A}bye"}}],
            })))
            .mount(&mock)
            .await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_partial_json(json!({"model": "backup"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "gen-backup",
                "model": "companion",
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

    fn opener(occasion: OpenOccasion, key: &str) -> OpenSessionRequest {
        OpenSessionRequest {
            occasion: Some(occasion),
            open_key: Some(key.into()),
            ..Default::default()
        }
    }

    async fn empty_session(pool: &PgPool, user_id: Uuid) -> Uuid {
        let genome_id = seed_genome(pool, "Aria").await;
        let instance_id = seed_instance(pool, genome_id, user_id).await;
        seed_session(pool, user_id, instance_id).await
    }

    fn last_message(call: &serde_json::Value) -> &serde_json::Value {
        call["messages"].as_array().unwrap().last().unwrap()
    }

    async fn row_metadata(pool: &PgPool, id: Uuid) -> serde_json::Value {
        sqlx::query_scalar("SELECT metadata FROM engine.chat_messages WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn first_meet_opens_an_empty_session(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = empty_session(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "你好呀，第一次见").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let g = open_at(
            &state,
            session_id,
            user_id,
            opener(OpenOccasion::FirstMeet, "visit-1"),
            Utc::now(),
        )
        .await
        .unwrap()
        .greeting
        .expect("an opener");
        assert_eq!(g.content, "你好呀，第一次见");
        assert_eq!(g.occasion, GreetingOccasion::FirstMeet);

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
        assert_eq!(meta["proactive"], json!("first_meet"));
        assert_eq!(meta["open_key"], json!("visit-1"));
        assert_eq!(meta["prompt_traits"], json!([]));
        assert!(meta.get("memory_scope").is_some());
        assert!(meta.get("affinity_scope").is_some());
        assert!(
            meta.get("local_date").is_none(),
            "never meets the holiday index"
        );
        assert!(meta.get("gap_days").is_none());

        let calls = companion_calls(&mock).await;
        assert_eq!(calls.len(), 1);
        assert_eq!(
            last_message(&calls[0])["content"],
            json!("（你们还没聊过。对方刚打开和你的聊天，还没说话。）")
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn returning_states_the_gap_in_days(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let now = Utc::now();
        let session_id =
            session_with_history(&pool, user_id, now - chrono::Duration::hours(80)).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "好久不见").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let g = open_at(
            &state,
            session_id,
            user_id,
            opener(OpenOccasion::Returning, "visit-1"),
            now,
        )
        .await
        .unwrap()
        .greeting
        .expect("an opener");
        assert_eq!(g.occasion, GreetingOccasion::Returning);
        let meta = row_metadata(&pool, g.message_id).await;
        assert_eq!(meta["proactive"], json!("returning"));
        assert_eq!(meta["gap_days"], json!(3));
        let calls = companion_calls(&mock).await;
        assert_eq!(
            last_message(&calls[0])["content"],
            json!("（对方隔了 3 天又打开和你的聊天，还没说话。）")
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn returning_within_a_day_says_less_than_a_day(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let now = Utc::now();
        let session_id =
            session_with_history(&pool, user_id, now - chrono::Duration::hours(5)).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "又来啦").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let g = open_at(
            &state,
            session_id,
            user_id,
            opener(OpenOccasion::Returning, "visit-1"),
            now,
        )
        .await
        .unwrap()
        .greeting
        .expect("an opener");
        assert_eq!(
            row_metadata(&pool, g.message_id).await["gap_days"],
            json!(0)
        );
        let calls = companion_calls(&mock).await;
        assert_eq!(
            last_message(&calls[0])["content"],
            json!("（对方隔了不到一天又打开和你的聊天，还没说话。）")
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn just_opened_takes_the_plain_cue(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let now = Utc::now();
        let session_id =
            session_with_history(&pool, user_id, now - chrono::Duration::days(2)).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "在干嘛呢").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let g = open_at(
            &state,
            session_id,
            user_id,
            opener(OpenOccasion::JustOpened, "visit-1"),
            now,
        )
        .await
        .unwrap()
        .greeting
        .expect("an opener");
        assert_eq!(g.occasion, GreetingOccasion::JustOpened);
        let meta = row_metadata(&pool, g.message_id).await;
        assert_eq!(meta["proactive"], json!("just_opened"));
        assert!(meta.get("gap_days").is_none());
        let calls = companion_calls(&mock).await;
        assert_eq!(
            last_message(&calls[0])["content"],
            json!(crate::pipeline::proactive::OPEN_CUE)
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_occasion_that_is_not_true_means_no_opener_and_no_call(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let now = Utc::now();
        let talked = session_with_history(&pool, user_id, now - chrono::Duration::days(2)).await;
        let empty = empty_session(&pool, user_id).await;
        // A session whose only row is a holiday greeting: the persona already
        // spoke (not a first meeting) and the user never did (not a return).
        let greeted_only = empty_session(&pool, user_id).await;
        sqlx::query(
            "INSERT INTO engine.chat_messages \
               (session_id, role, content, assistant_action_type, metadata) \
             VALUES ($1, 'assistant', '中秋快乐', 'proactive', \
                     '{\"proactive\": \"holiday\", \"local_date\": \"2026-09-25\"}'::jsonb)",
        )
        .bind(greeted_only)
        .execute(&pool)
        .await
        .unwrap();
        let mock = MockServer::start().await;
        mount_companion(&mock, "不该被调用").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        for (session_id, occasion) in [
            (talked, OpenOccasion::FirstMeet),
            (empty, OpenOccasion::Returning),
            (greeted_only, OpenOccasion::FirstMeet),
            (greeted_only, OpenOccasion::Returning),
        ] {
            let out = open_at(
                &state,
                session_id,
                user_id,
                opener(occasion, "visit-1"),
                now,
            )
            .await
            .unwrap();
            assert!(out.greeting.is_none(), "{occasion:?} on {session_id}");
        }
        assert!(companion_calls(&mock).await.is_empty());
        assert_eq!(proactive_rows(&pool, talked).await, 0);
        assert_eq!(proactive_rows(&pool, empty).await, 0);
        assert_eq!(proactive_rows(&pool, greeted_only).await, 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_same_key_returns_the_same_opener_and_a_new_key_speaks_again(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id =
            session_with_history(&pool, user_id, Utc::now() - chrono::Duration::days(2)).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "在干嘛呢").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let first = open_at(
            &state,
            session_id,
            user_id,
            opener(OpenOccasion::JustOpened, "visit-1"),
            Utc::now(),
        )
        .await
        .unwrap()
        .greeting
        .unwrap();
        // The key wins over the occasion this call names.
        let again = open_at(
            &state,
            session_id,
            user_id,
            opener(OpenOccasion::Returning, "visit-1"),
            Utc::now(),
        )
        .await
        .unwrap()
        .greeting
        .unwrap();
        assert_eq!(again.message_id, first.message_id);
        assert_eq!(again.occasion, GreetingOccasion::JustOpened);
        assert_eq!(companion_calls(&mock).await.len(), 1);

        // `now` is taken after the first opener landed, so its row is before
        // this request's cutoff.
        let next = open_at(
            &state,
            session_id,
            user_id,
            opener(OpenOccasion::JustOpened, "visit-2"),
            Utc::now(),
        )
        .await
        .unwrap()
        .greeting
        .expect("a new key speaks again");
        assert_ne!(next.message_id, first.message_id);
        assert_eq!(companion_calls(&mock).await.len(), 2);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn concurrent_opens_with_one_key_write_one_opener(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let now = Utc::now();
        let session_id =
            session_with_history(&pool, user_id, now - chrono::Duration::days(2)).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "在干嘛呢").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let (a, b) = tokio::join!(
            open_at(
                &state,
                session_id,
                user_id,
                opener(OpenOccasion::JustOpened, "visit-1"),
                now
            ),
            open_at(
                &state,
                session_id,
                user_id,
                opener(OpenOccasion::JustOpened, "visit-1"),
                now
            ),
        );
        let (a, b) = (a.unwrap().greeting.unwrap(), b.unwrap().greeting.unwrap());
        assert_eq!(a.message_id, b.message_id);
        assert_eq!(proactive_rows(&pool, session_id).await, 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_failed_opener_writes_nothing_and_leaves_the_key_free(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let now = Utc::now();
        let session_id =
            session_with_history(&pool, user_id, now - chrono::Duration::days(2)).await;
        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let err = open_at(
            &state,
            session_id,
            user_id,
            opener(OpenOccasion::JustOpened, "visit-1"),
            now,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Upstream(_)), "{err:?}");
        assert_eq!(proactive_rows(&pool, session_id).await, 0);
        let repo = ChatRepo { pool: &pool };
        assert!(
            repo.open_message(session_id, "visit-1")
                .await
                .unwrap()
                .is_none(),
            "the next open under this key retries"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_opener_today_blocks_the_holiday_greeting(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = session_with_history(
            &pool,
            user_id,
            mid_autumn_morning() - chrono::Duration::days(2),
        )
        .await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "在干嘛呢").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        open_at(
            &state,
            session_id,
            user_id,
            opener(OpenOccasion::JustOpened, "visit-1"),
            mid_autumn_morning(),
        )
        .await
        .unwrap()
        .greeting
        .expect("the opener");
        let later = mid_autumn_morning() + chrono::Duration::hours(1);
        let out = open_at(&state, session_id, user_id, taipei(), later)
            .await
            .unwrap();
        assert!(out.greeting.is_none(), "the persona already spoke today");
        assert_eq!(companion_calls(&mock).await.len(), 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_holiday_greeting_does_not_block_an_opener(pool: PgPool) {
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

        open_at(&state, session_id, user_id, taipei(), mid_autumn_morning())
            .await
            .unwrap()
            .greeting
            .expect("the holiday greeting");
        // A `now` before the greeting's own sent_at: the greeting sits inside
        // this request's cutoff, so only the holiday exclusion lets it pass.
        let g = open_at(
            &state,
            session_id,
            user_id,
            opener(OpenOccasion::JustOpened, "visit-1"),
            mid_autumn_morning() + chrono::Duration::hours(1),
        )
        .await
        .unwrap()
        .greeting
        .expect("an opener under a new key");
        assert_eq!(g.occasion, GreetingOccasion::JustOpened);
        assert_eq!(proactive_rows(&pool, session_id).await, 2);
    }

    /// Replace the session's genome `art_metadata`.
    async fn set_art(pool: &PgPool, session_id: Uuid, art: serde_json::Value) {
        sqlx::query(
            "UPDATE engine.persona_genomes SET art_metadata = $2 WHERE id = ( \
               SELECT pi.genome_id FROM engine.chat_sessions s \
               JOIN engine.persona_instances pi ON pi.id = s.instance_id \
               WHERE s.id = $1)",
        )
        .bind(session_id)
        .bind(art)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Born 09-25 on Taipei's clock — the day `mid_autumn_morning` falls on there.
    fn taipei_birthday() -> serde_json::Value {
        json!({ "timezone": "Asia/Taipei", "birthday": "09-25" })
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn greets_on_the_personas_birthday_without_a_user_timezone(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = session_with_history(
            &pool,
            user_id,
            mid_autumn_morning() - chrono::Duration::days(2),
        )
        .await;
        set_art(&pool, session_id, taipei_birthday()).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "今天是我生日哦").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let g = open_at(
            &state,
            session_id,
            user_id,
            OpenSessionRequest::default(),
            mid_autumn_morning(),
        )
        .await
        .unwrap()
        .greeting
        .expect("a birthday greeting");
        assert_eq!(g.occasion, GreetingOccasion::Birthday);
        let meta = row_metadata(&pool, g.message_id).await;
        assert_eq!(meta["proactive"], json!("birthday"));
        assert_eq!(meta["local_date"], json!("2026-09-25"));

        let calls = companion_calls(&mock).await;
        assert_eq!(calls.len(), 1);
        let messages = calls[0]["messages"].as_array().unwrap();
        let system = messages[0]["content"].as_str().unwrap();
        assert!(system.contains("今天是你的生日。"), "{system}");
        assert_eq!(
            messages.last().unwrap()["content"],
            json!(crate::pipeline::proactive::OPEN_CUE)
        );

        let again = open_at(
            &state,
            session_id,
            user_id,
            OpenSessionRequest::default(),
            mid_autumn_morning() + chrono::Duration::hours(1),
        )
        .await
        .unwrap()
        .greeting
        .expect("the same greeting");
        assert_eq!(again.message_id, g.message_id);
        assert_eq!(companion_calls(&mock).await.len(), 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_birthday_outranks_a_holiday_on_the_same_day(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = session_with_history(
            &pool,
            user_id,
            mid_autumn_morning() - chrono::Duration::days(2),
        )
        .await;
        set_art(&pool, session_id, taipei_birthday()).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "今天也是我生日").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let g = open_at(&state, session_id, user_id, taipei(), mid_autumn_morning())
            .await
            .unwrap()
            .greeting
            .expect("a greeting");
        assert_eq!(g.occasion, GreetingOccasion::Birthday);
        assert_eq!(proactive_rows(&pool, session_id).await, 1);
    }

    /// A Los Angeles user. `OpenSessionRequest` is not `Clone`, so build anew.
    fn la() -> OpenSessionRequest {
        OpenSessionRequest {
            user_timezone: Some("America/Los_Angeles".into()),
            user_country: Some("US".into()),
            ..Default::default()
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_birthday_is_dated_on_the_persona_clock(pool: PgPool) {
        // Taipei is on 09-25; Los Angeles is still on 09-24, which has no
        // holiday for a US user.
        let user_id = Uuid::new_v4();
        let mock = MockServer::start().await;
        mount_companion(&mock, "生日快乐给自己").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let past = mid_autumn_morning() - chrono::Duration::days(2);

        let taipei_persona = session_with_history(&pool, user_id, past).await;
        set_art(&pool, taipei_persona, taipei_birthday()).await;
        let g = open_at(&state, taipei_persona, user_id, la(), mid_autumn_morning())
            .await
            .unwrap()
            .greeting
            .expect("the persona's own clock says today");
        assert_eq!(g.occasion, GreetingOccasion::Birthday);

        for art in [
            json!({ "birthday": "09-25" }),
            json!({ "timezone": "Not/AZone", "birthday": "09-25" }),
        ] {
            let s = session_with_history(&pool, user_id, past).await;
            set_art(&pool, s, art.clone()).await;
            let out = open_at(&state, s, user_id, la(), mid_autumn_morning())
                .await
                .unwrap();
            assert!(out.greeting.is_none(), "user clock says 09-24: {art}");
        }
        assert_eq!(companion_calls(&mock).await.len(), 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn only_a_message_since_the_personas_midnight_blocks_the_birthday(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let mock = MockServer::start().await;
        mount_companion(&mock, "今天是我生日").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let spoke_today = session_with_history(
            &pool,
            user_id,
            taipei_midnight() + chrono::Duration::hours(1),
        )
        .await;
        set_art(&pool, spoke_today, taipei_birthday()).await;
        let out = open_at(
            &state,
            spoke_today,
            user_id,
            OpenSessionRequest::default(),
            mid_autumn_morning(),
        )
        .await
        .unwrap();
        assert!(out.greeting.is_none());
        assert!(companion_calls(&mock).await.is_empty());

        let spoke_last_night = session_with_history(
            &pool,
            user_id,
            taipei_midnight() - chrono::Duration::minutes(1),
        )
        .await;
        set_art(&pool, spoke_last_night, taipei_birthday()).await;
        let g = open_at(
            &state,
            spoke_last_night,
            user_id,
            OpenSessionRequest::default(),
            mid_autumn_morning(),
        )
        .await
        .unwrap()
        .greeting
        .expect("yesterday's message does not block");
        assert_eq!(g.occasion, GreetingOccasion::Birthday);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_caller_occasion_on_the_birthday_keeps_its_own_occasion(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = session_with_history(
            &pool,
            user_id,
            mid_autumn_morning() - chrono::Duration::days(2),
        )
        .await;
        set_art(&pool, session_id, taipei_birthday()).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "在干嘛呢").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        open_at(
            &state,
            session_id,
            user_id,
            OpenSessionRequest::default(),
            mid_autumn_morning(),
        )
        .await
        .unwrap()
        .greeting
        .expect("the birthday greeting");
        // A `now` before the greeting's own sent_at: the greeting sits inside
        // this request's cutoff, so only the birthday exemption lets it pass.
        let g = open_at(
            &state,
            session_id,
            user_id,
            opener(OpenOccasion::JustOpened, "visit-1"),
            mid_autumn_morning() + chrono::Duration::hours(1),
        )
        .await
        .unwrap()
        .greeting
        .expect("an opener");
        assert_eq!(g.occasion, GreetingOccasion::JustOpened);
        assert_eq!(proactive_rows(&pool, session_id).await, 2);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_recall_query_is_the_latest_user_message(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let now = Utc::now();
        let talked = session_with_history(&pool, user_id, now - chrono::Duration::days(2)).await;
        let empty = empty_session(&pool, user_id).await;
        let repo = ChatRepo { pool: &pool };

        let Next::Speak { recall_query, .. } = caller_occasion(
            &repo,
            talked,
            OpenOccasion::JustOpened,
            "visit-1".into(),
            now,
        )
        .await
        .unwrap() else {
            panic!("just_opened always speaks");
        };
        assert_eq!(recall_query, "晚安");

        let Next::Speak { recall_query, .. } =
            caller_occasion(&repo, empty, OpenOccasion::FirstMeet, "visit-1".into(), now)
                .await
                .unwrap()
        else {
            panic!("an empty session is a first meeting");
        };
        assert!(recall_query.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_user_row_stamped_after_now_is_a_zero_day_gap(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let now = Utc::now();
        let session_id =
            session_with_history(&pool, user_id, now + chrono::Duration::minutes(5)).await;
        let repo = ChatRepo { pool: &pool };

        let Next::Speak { occasion, .. } = caller_occasion(
            &repo,
            session_id,
            OpenOccasion::Returning,
            "visit-1".into(),
            now,
        )
        .await
        .unwrap() else {
            panic!("the user has spoken here");
        };
        assert!(
            matches!(occasion, Occasion::Returning { gap_days: 0, .. }),
            "never a negative gap"
        );
    }

    /// A same-key opener that commits between a call's key lookup and its
    /// precondition read is that call's answer, never a bare `null`. The
    /// checker loops across the writer's commit, so the commit lands inside
    /// one of its lookup → precondition windows.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_same_key_opener_landing_mid_check_is_returned_not_null(pool: PgPool) {
        use std::sync::atomic::{AtomicBool, Ordering};

        let user_id = Uuid::new_v4();
        let session_id = empty_session(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_companion(&mock, "你好呀").await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let repo = ChatRepo { pool: &pool };
        let now = Utc::now();
        let done = AtomicBool::new(false);

        let writer = async {
            let out = open_at(
                &state,
                session_id,
                user_id,
                opener(OpenOccasion::FirstMeet, "visit-1"),
                now,
            )
            .await;
            done.store(true, Ordering::SeqCst);
            out
        };
        let checker = async {
            let mut nulls = 0;
            loop {
                let writer_done = done.load(Ordering::SeqCst);
                let next = caller_occasion(
                    &repo,
                    session_id,
                    OpenOccasion::FirstMeet,
                    "visit-1".into(),
                    now,
                )
                .await
                .unwrap();
                match next {
                    Next::Answer(OpenSessionResponse { greeting: Some(_) }) => break,
                    Next::Answer(OpenSessionResponse { greeting: None }) => nulls += 1,
                    Next::Speak { .. } if writer_done => break,
                    Next::Speak { .. } => tokio::task::yield_now().await,
                }
            }
            nulls
        };
        let (written, nulls) = tokio::join!(writer, checker);
        assert!(written.unwrap().greeting.is_some(), "the writer's opener");
        assert_eq!(nulls, 0, "the same key's opener was reported as null");
    }

    #[test]
    fn open_key_is_one_to_128_safe_chars() {
        let ok = |key: &str| validate_opener(Some(OpenOccasion::JustOpened), Some(key.into()));
        assert!(ok(&"k".repeat(128)).is_ok());
        assert!(ok("visit-01J9:a.b_c").is_ok());
        assert!(ok(&"k".repeat(129)).is_err());
        assert!(ok("").is_err());
        assert!(ok("visit 1").is_err());
        assert!(ok("访问").is_err());
        assert!(validate_opener(None, None).unwrap().is_none());
        assert!(validate_opener(Some(OpenOccasion::JustOpened), None).is_err());
        assert!(validate_opener(None, Some("visit-1".into())).is_err());
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

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn occasion_and_open_key_come_together(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let session_id = empty_session(&pool, user_id).await;
        let mut app = build_router(test_state(pool.clone()));
        let jwt = mint_test_jwt(user_id);

        for body in [
            json!({"occasion": "just_opened"}),
            json!({"open_key": "visit-1"}),
            json!({"occasion": "just_opened", "open_key": "visit 1"}),
        ] {
            let (status, resp) =
                send_request(&mut app, open_req(session_id, &jwt, body.clone())).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(resp["code"], json!("invalid_payload"), "{body}");
        }
        let (status, _) = send_request(
            &mut app,
            open_req(
                session_id,
                &jwt,
                json!({"occasion": "holiday", "open_key": "visit-1"}),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "the holiday is the engine's call"
        );
    }

    #[test]
    fn openapi_lists_the_open_route() {
        let api = crate::routes::openapi_for_tests();
        assert!(api["paths"]["/v2/comp/session/{session_id}/open"]["post"].is_object());
    }
}
