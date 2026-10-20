// SPDX-License-Identifier: AGPL-3.0-only
//! POST /v2/comp/session/{session_id}/life-choices/{choice_id} — the user
//! answers the persona's ask for space: accept or decline. Either way the
//! relationship drops one tier on both lines; an accept also starts a
//! separation. The persona answers in one proactive message, and the cost is
//! applied with the answer, not with the message (spec
//! docs/superpowers/specs/2026-10-19-life-events-shopping-gatherings-and-ask-for-space-design.md
//! §3.4a, §3.4b, §3.5).

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_core::events::EventKind;
use eros_engine_core::persona::CompanionPersona;
use eros_engine_core::scope::MemoryScope;
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::{ChatMessage, ChatRepo};
use eros_engine_store::events::{EventRepo, NewEvent};
use eros_engine_store::life_choices::LifeChoiceRepo;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, StreamPreError};
use crate::holiday::{self, UserLocale};
use crate::pipeline::handlers::recall_query_text;
use crate::pipeline::proactive::{greet, Greeting, LifeChoiceCue, Occasion};
use crate::prompt::NowContext;
use crate::routes::companion::{
    validate_llm_audit, validate_prompt_traits, LlmAuditDto, PromptTraitDto,
};
use crate::routes::companion_stream::{
    resolve_text_turn, validate_tier, AffinityScopeDto, StreamPreErrorBody,
};
use crate::routes::dto::AffinitySnapshot;
use crate::routes::session_events::EventView;
use crate::routes::session_open::GreetingDto;
use crate::state::AppState;

/// Same per-user in-flight cap as every other LLM entry point.
const CONCURRENT_STREAMS_PER_USER: u32 = 3;

/// The separation lasts this many hours for each tier of the gap stored when
/// she asked: a gap of 2, 3 or 4 is 12, 18 or 24 hours.
const SEPARATION_HOURS_PER_TIER: i64 = 6;

/// The user's answer to the ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LifeChoiceAnswer {
    /// Agree to the separation.
    Accept,
    /// Refuse it.
    Decline,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct LifeChoiceRespondRequest {
    pub response: LifeChoiceAnswer,
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
pub struct LifeChoiceRespondResponse {
    /// The choice's new status: `accepted` or `declined`.
    pub status: String,
    /// What she says back: the goodbye after an accept, the hurt reply after
    /// a decline. `null` when the generation failed, came back blank or a
    /// newer message landed first; the answer and its costs stand either way.
    pub message: Option<GreetingDto>,
    /// The relationship after the drop.
    pub affinity: AffinitySnapshot,
    /// The separation an accept started; `null` after a decline.
    pub event: Option<EventView>,
}

fn not_found() -> AppError {
    AppError::StreamPre(StreamPreError {
        status: StatusCode::NOT_FOUND,
        code: "life_choice_not_found",
        message: "life choice not found in this session".into(),
        user_message: "找不到这个请求".into(),
        original_user_message_id: None,
    })
}

fn closed() -> AppError {
    AppError::StreamPre(StreamPreError {
        status: StatusCode::CONFLICT,
        code: "life_choice_closed",
        message: "already resolved or expired".into(),
        user_message: "这个请求已经处理过或已过期".into(),
        original_user_message_id: None,
    })
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

/// Answer her ask for space.
///
/// One transaction answers the choice and pays for it. Accept: the choice
/// becomes `accepted`, a `taking_space` separation starts now and lasts 6 hours
/// per tier of the gap she asked at (12, 18 or 24 hours; she is offline for
/// it, and an incident under way ends), and bond and chemistry each drop one
/// tier. Decline: the choice becomes `declined` and the two lines drop one
/// tier, with no separation. Either way she then answers in one proactive
/// message, persisted like any reply and returned; if that generation fails
/// the call still succeeds with `message: null`.
///
/// `409` when the choice is no longer pending: answered already (an ordinary
/// message from the user counts, and the first writer wins), or lapsed 24
/// hours after she asked. `404` when the choice is not in this session.
#[utoipa::path(
    post,
    path = "/v2/comp/session/{session_id}/life-choices/{choice_id}",
    tag = "companion",
    params(
        ("session_id" = Uuid, Path, description = "Chat session id"),
        ("choice_id" = Uuid, Path, description = "The choice's id")
    ),
    request_body = LifeChoiceRespondRequest,
    responses(
        (status = 200, body = LifeChoiceRespondResponse),
        (status = 400, body = StreamPreErrorBody),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, body = StreamPreErrorBody),
        (status = 404, body = StreamPreErrorBody),
        (status = 409, body = StreamPreErrorBody),
        (status = 422, description = "`response` is not `accept` or `decline`, or another \
            malformed body field"),
        (status = 429, description = "per-user in-flight cap reached")
    ),
    security(("bearer" = []))
)]
async fn respond_to_life_choice(
    State(state): State<AppState>,
    Path((session_id, choice_id)): Path<(Uuid, Uuid)>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<LifeChoiceRespondRequest>,
) -> Result<Json<LifeChoiceRespondResponse>, AppError> {
    respond_at(&state, session_id, choice_id, user_id, req, Utc::now())
        .await
        .map(Json)
}

/// The handler with its clock injected, for tests.
pub(crate) async fn respond_at(
    state: &AppState,
    session_id: Uuid,
    choice_id: Uuid,
    user_id: Uuid,
    req: LifeChoiceRespondRequest,
    now: DateTime<Utc>,
) -> Result<LifeChoiceRespondResponse, AppError> {
    validate_tier(req.tier.as_deref())?;
    let prompt_traits = validate_prompt_traits(req.prompt_traits.as_deref().unwrap_or(&[]))
        .map_err(|e| invalid_payload(e.to_string()))?;
    let audit =
        validate_llm_audit(req.audit.clone()).map_err(|e| invalid_payload(e.to_string()))?;
    let (_session, persona, instance_id) =
        resolve_text_turn(state, session_id, user_id, "life choices").await?;

    (LifeChoiceRepo { pool: &state.pool })
        .get(choice_id)
        .await?
        .filter(|c| c.session_id == session_id)
        .ok_or_else(not_found)?;

    // Taken before anything is written: a call over the cap is refused with
    // the choice still open, not answered and then left without her reply.
    let _guard = state
        .stream_slots
        .try_acquire(user_id, CONCURRENT_STREAMS_PER_USER)
        .ok_or_else(|| AppError::TooManyRequests("per-user in-flight cap reached".into()))?;

    // The answer and its costs are one transaction. The resolve goes first:
    // it is the race guard, and a loser (answered, converted by a message or
    // lapsed) writes nothing. The separation comes before the drop.
    let (status, resolution) = match req.response {
        LifeChoiceAnswer::Accept => ("accepted", "accept"),
        LifeChoiceAnswer::Decline => ("declined", "decline"),
    };
    let mut tx = state.pool.begin().await?;
    let Some(choice) = LifeChoiceRepo::resolve_in_tx(&mut tx, choice_id, status, now).await? else {
        return Err(closed());
    };
    let separation = match req.response {
        LifeChoiceAnswer::Accept => {
            let lasts = Duration::hours(SEPARATION_HOURS_PER_TIER * i64::from(choice.tier_gap));
            let event = EventRepo::insert_preempting_in_tx(
                &mut tx,
                &NewEvent {
                    instance_id,
                    session_id,
                    event_key: &format!("life_choice:{}:taking_space", choice.id),
                    kind: EventKind::TakingSpace,
                    started_at: now,
                    ends_at: now + lasts,
                },
            )
            .await?;
            LifeChoiceRepo::set_event_in_tx(&mut tx, choice.id, event.id).await?;
            Some((event, lasts))
        }
        LifeChoiceAnswer::Decline => None,
    };
    let affinity = AffinityRepo::persist_life_choice_drop_in_tx(
        &mut tx,
        session_id,
        choice.id,
        resolution,
        &state.config.affinity_tuning,
    )
    .await?;
    tx.commit().await?;

    // Her reply is the last step and cannot undo the answer: whatever goes
    // wrong here (the provider, a blank reply, a newer message) leaves the
    // resolution, the separation and the drop in place, and she says nothing.
    let cue = match &separation {
        Some((_, lasts)) => LifeChoiceCue::Goodbye {
            hours: lasts.num_hours(),
        },
        None => LifeChoiceCue::Hurt,
    };
    let message = match say(
        state,
        session_id,
        user_id,
        instance_id,
        &persona,
        &req,
        cue,
        now,
        prompt_traits,
        audit,
    )
    .await
    {
        Ok(m) => m.map(GreetingDto::from),
        Err(e) => {
            tracing::warn!(%session_id, choice_id = %choice.id, "life-choice reply failed: {e}");
            None
        }
    };

    let traits = crate::prompt::persona_traits(&persona);
    Ok(LifeChoiceRespondResponse {
        status: status.into(),
        message,
        affinity: AffinitySnapshot::from(affinity),
        event: separation.map(|(e, _)| EventView::new(&e, &traits)),
    })
}

/// Generate and persist what she says about the answer. It yields to a
/// message the user sent since the request began.
#[allow(clippy::too_many_arguments)]
async fn say(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    persona: &CompanionPersona,
    req: &LifeChoiceRespondRequest,
    cue: LifeChoiceCue,
    now: DateTime<Utc>,
    prompt_traits: Vec<eros_engine_core::types::PromptTrait>,
    audit: Option<eros_engine_core::types::LlmAudit>,
) -> Result<Option<ChatMessage>, AppError> {
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
    let recall_query = (ChatRepo { pool: &state.pool })
        .latest_user_message(session_id)
        .await?
        .as_ref()
        .map(recall_query_text)
        .unwrap_or_default();
    greet(
        state,
        Greeting {
            session_id,
            user_id,
            instance_id,
            persona,
            now: &NowContext::for_user(&locale, now),
            occasion: Occasion::LifeChoice(cue),
            recall_query,
            since: now,
            tier: req.tier.clone(),
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
    .await
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(respond_to_life_choice))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::SubsecRound;
    use serde_json::json;
    use sqlx::PgPool;
    use std::sync::Arc;
    use wiremock::matchers::{body_string_contains, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{
        seed_byoc_session, seed_genome, seed_group_session, seed_instance, seed_session,
        session_footprint,
    };
    use crate::routes::session_events::EventKindDto;
    use crate::routes::session_open::GreetingOccasion;

    const GOODBYE: &str = "好，那我们先各自安静一下。";
    const HURT: &str = "你不愿意啊……我有点难过。";
    const GOODBYE_CUE_18: &str =
        "（对方同意了你提出的分开请求，接下来 18 小时你们不联系。你跟对方道别。）";
    const HURT_CUE: &str = "（对方不同意你提出的分开请求，你心里不好受。）";

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

    /// A mock for the persona's own call ("you are a companion"): `Some(text)`
    /// as her reply, `None` as a provider failure.
    async fn companion_mock(reply: Option<&str>) -> MockServer {
        let mock = MockServer::start().await;
        let template = match reply {
            Some(text) => ResponseTemplate::new(200).set_body_json(json!({
                "id": "gen-choice",
                "model": "served/companion",
                "choices": [{"message": {"content": text}}],
            })),
            None => ResponseTemplate::new(500).set_body_string("provider down"),
        };
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("you are a companion"))
            .respond_with(template)
            .mount(&mock)
            .await;
        mock
    }

    async fn calls(mock: &MockServer) -> Vec<serde_json::Value> {
        mock.received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    /// A time in the recent past, whole microseconds, so what Postgres stores
    /// is what the test compares.
    fn recent(minutes_ago: i64) -> DateTime<Utc> {
        (Utc::now() - Duration::minutes(minutes_ago)).trunc_subsecs(6)
    }

    fn answer(response: LifeChoiceAnswer) -> LifeChoiceRespondRequest {
        LifeChoiceRespondRequest {
            response,
            user_timezone: Some("Asia/Taipei".into()),
            user_country: None,
            user_region: None,
            tier: None,
            prompt_traits: None,
            memory_scope: None,
            affinity_scope: None,
            audit: None,
        }
    }

    struct Scene {
        pool: PgPool,
        user_id: Uuid,
        instance_id: Uuid,
        session_id: Uuid,
    }

    /// A text session whose bond line sits at tier 3 (0.45) and chemistry line
    /// at tier 4 (0.70).
    async fn scene(pool: &PgPool) -> Scene {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(pool, "Aria").await;
        let instance_id = seed_instance(pool, genome_id, user_id).await;
        let session_id = seed_session(pool, user_id, instance_id).await;
        AffinityRepo { pool }
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE engine.companion_affinity \
                SET trust = 0.45, intrigue = 0.45, intimacy = 0.70, tension = 0.70, \
                    bond_tier = 3, chem_tier = 4, updated_at = now() \
              WHERE session_id = $1",
        )
        .bind(session_id)
        .execute(pool)
        .await
        .unwrap();
        Scene {
            pool: pool.clone(),
            user_id,
            instance_id,
            session_id,
        }
    }

    impl Scene {
        /// A pending choice asked at `asked_at` with `tier_gap`; returns its id.
        async fn ask(&self, tier_gap: i16, asked_at: DateTime<Utc>) -> Uuid {
            sqlx::query_scalar(
                "INSERT INTO engine.life_choices (session_id, kind, tier_gap, asked_at) \
                 VALUES ($1, 'ask_for_space', $2, $3) RETURNING id",
            )
            .bind(self.session_id)
            .bind(tier_gap)
            .bind(asked_at)
            .fetch_one(&self.pool)
            .await
            .unwrap()
        }

        async fn respond(
            &self,
            state: &AppState,
            choice: Uuid,
            response: LifeChoiceAnswer,
            now: DateTime<Utc>,
        ) -> Result<LifeChoiceRespondResponse, AppError> {
            respond_at(
                state,
                self.session_id,
                choice,
                self.user_id,
                answer(response),
                now,
            )
            .await
        }

        /// (status, resolved_at, event_id) of the choice.
        async fn row(&self, choice: Uuid) -> (String, Option<DateTime<Utc>>, Option<Uuid>) {
            sqlx::query_as(
                "SELECT status, resolved_at, event_id FROM engine.life_choices WHERE id = $1",
            )
            .bind(choice)
            .fetch_one(&self.pool)
            .await
            .unwrap()
        }

        /// (bond_tier, chem_tier) as stored.
        async fn tiers(&self) -> (i16, i16) {
            sqlx::query_as(
                "SELECT bond_tier, chem_tier FROM engine.companion_affinity WHERE session_id = $1",
            )
            .bind(self.session_id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
        }

        /// The `context` of every `life_choice` affinity event of the session.
        async fn drops(&self) -> Vec<serde_json::Value> {
            sqlx::query_scalar(
                "SELECT e.context FROM engine.companion_affinity_events e \
                   JOIN engine.companion_affinity a ON a.id = e.affinity_id \
                  WHERE a.session_id = $1 AND e.event_type = 'life_choice'",
            )
            .bind(self.session_id)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// (id, kind, event_key, session_id, started_at, ends_at, message_id)
        /// of the instance's events, oldest first.
        #[allow(clippy::type_complexity)]
        async fn events(
            &self,
        ) -> Vec<(
            Uuid,
            String,
            String,
            Option<Uuid>,
            DateTime<Utc>,
            DateTime<Utc>,
            Option<Uuid>,
        )> {
            sqlx::query_as(
                "SELECT id, kind, event_key, session_id, started_at, ends_at, message_id \
                   FROM engine.persona_events WHERE instance_id = $1 \
                  ORDER BY started_at, id",
            )
            .bind(self.instance_id)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// Every assistant row of the session written as a life choice.
        async fn life_choice_rows(&self) -> Vec<(Uuid, String, serde_json::Value)> {
            sqlx::query_as(
                "SELECT id, content, metadata FROM engine.chat_messages \
                  WHERE session_id = $1 AND role = 'assistant' \
                    AND metadata->>'proactive' = 'life_choice'",
            )
            .bind(self.session_id)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }
    }

    /// Accept: the choice is answered, the separation starts now and lasts
    /// 6 hours per tier of the gap stored at ask time, both lines drop a tier
    /// in the same transaction, and she says goodbye.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn accept_writes_separation_drop_and_goodbye(pool: PgPool) {
        let mock = companion_mock(Some(GOODBYE)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        for (gap, hours) in [(2_i16, 12_i64), (4, 24)] {
            let s = scene(&pool).await;
            let asked = recent(30);
            let choice = s.ask(gap, asked).await;
            assert_eq!(s.tiers().await, (3, 4));
            let now = recent(0);

            let out = s
                .respond(&state, choice, LifeChoiceAnswer::Accept, now)
                .await
                .unwrap();

            assert_eq!(out.status, "accepted");
            // The separation: now, 6 h per tier of the gap, no message of its
            // own, in the choice's session.
            let events = s.events().await;
            assert_eq!(events.len(), 1, "{events:?}");
            let (event_id, kind, key, session, started, ends, message) = events[0].clone();
            assert_eq!(kind, "taking_space");
            assert_eq!(key, format!("life_choice:{choice}:taking_space"));
            assert_eq!(session, Some(s.session_id));
            assert_eq!(started, now, "wall-clock: the user acted now");
            assert_eq!(ends - started, Duration::hours(6 * i64::from(gap)));
            assert_eq!(ends - started, Duration::hours(hours));
            assert_eq!(message, None);
            let view = out.event.expect("the separation is returned");
            assert_eq!(view.kind, EventKindDto::TakingSpace);
            assert_eq!((view.started_at, view.ends_at), (started, ends));
            assert!(view.offline, "she is unreachable while it lasts");
            assert_eq!(view.message_id, None);

            // The choice: answered at `now`, linked to the event.
            assert_eq!(
                s.row(choice).await,
                ("accepted".into(), Some(now), Some(event_id))
            );

            // The cost: one tier on both lines, one `life_choice` event.
            assert_eq!(s.tiers().await, (2, 3));
            assert_eq!(
                (out.affinity.bond_tier, out.affinity.chem_tier),
                (2, 3),
                "the response carries the post-drop state"
            );
            assert_eq!(
                s.drops().await,
                vec![json!({"life_choice": "accept", "choice_id": choice})]
            );

            // Her goodbye: a proactive row, the choice's occasion.
            let said = out.message.expect("a goodbye");
            assert_eq!(said.content, GOODBYE);
            assert_eq!(said.occasion, GreetingOccasion::LifeChoice);
            let rows = s.life_choice_rows().await;
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].0, said.message_id);
            assert_eq!(rows[0].2["proactive"], json!("life_choice"));
            assert_eq!(rows[0].2["user_timezone"], json!("Asia/Taipei"));
        }

        // The cue is a stage line that states the length of the separation.
        let c = calls(&mock).await;
        assert_eq!(c.len(), 2);
        for (call, hours) in c.iter().zip([12, 24]) {
            let last = call["messages"].as_array().unwrap().last().unwrap().clone();
            assert_eq!(last["role"], json!("user"));
            assert_eq!(
                last["content"],
                json!(format!(
                    "（对方同意了你提出的分开请求，接下来 {hours} 小时你们不联系。你跟对方道别。）"
                ))
            );
        }
    }

    /// A gap of 3 is 18 hours, and an incident under way ends where the
    /// separation starts.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn accept_caps_a_running_incident_and_names_18_hours_for_a_gap_of_3(pool: PgPool) {
        let mock = companion_mock(Some(GOODBYE)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask(3, recent(30)).await;
        let now = recent(0);
        sqlx::query(
            "INSERT INTO engine.persona_events \
               (instance_id, session_id, event_key, kind, started_at, ends_at) \
             VALUES ($1, $2, 'reading', 'reading', $3, $4)",
        )
        .bind(s.instance_id)
        .bind(s.session_id)
        .bind(now - Duration::minutes(20))
        .bind(now + Duration::minutes(100))
        .execute(&pool)
        .await
        .unwrap();

        let out = s
            .respond(&state, choice, LifeChoiceAnswer::Accept, now)
            .await
            .unwrap();

        let ev = out.event.expect("separation");
        assert_eq!(ev.ends_at - ev.started_at, Duration::hours(18));
        let events = s.events().await;
        let reading = events.iter().find(|e| e.1 == "reading").unwrap();
        assert_eq!(
            reading.5, now,
            "the incident ends where the separation starts"
        );
        let last = calls(&mock).await[0]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(last["content"], json!(GOODBYE_CUE_18));
    }

    /// Decline: the lines drop, nothing is separated, and she is hurt.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn decline_drops_without_an_event(pool: PgPool) {
        let mock = companion_mock(Some(HURT)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask(3, recent(30)).await;
        let now = recent(0);

        let out = s
            .respond(&state, choice, LifeChoiceAnswer::Decline, now)
            .await
            .unwrap();

        assert_eq!(out.status, "declined");
        assert!(out.event.is_none());
        assert!(s.events().await.is_empty(), "no separation");
        assert_eq!(s.row(choice).await, ("declined".into(), Some(now), None));
        assert_eq!(s.tiers().await, (2, 3));
        assert_eq!((out.affinity.bond_tier, out.affinity.chem_tier), (2, 3));
        assert_eq!(
            s.drops().await,
            vec![json!({"life_choice": "decline", "choice_id": choice})]
        );

        let said = out.message.expect("a hurt reply");
        assert_eq!(said.content, HURT);
        assert_eq!(said.occasion, GreetingOccasion::LifeChoice);
        let rows = s.life_choice_rows().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2["proactive"], json!("life_choice"));
        let c = calls(&mock).await;
        assert_eq!(c.len(), 1);
        let last = c[0]["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["content"], json!(HURT_CUE));
    }

    /// The first writer wins: a choice an ordinary message already converted
    /// answers 409, and nothing of the second answer lands.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn resolve_loses_to_a_faster_message(pool: PgPool) {
        let mock = companion_mock(Some(GOODBYE)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask(3, recent(30)).await;
        let flipped = recent(1);
        sqlx::query(
            "UPDATE engine.life_choices SET status = 'ignored', resolved_at = $2 WHERE id = $1",
        )
        .bind(choice)
        .bind(flipped)
        .execute(&pool)
        .await
        .unwrap();

        for response in [LifeChoiceAnswer::Accept, LifeChoiceAnswer::Decline] {
            let err = s
                .respond(&state, choice, response, recent(0))
                .await
                .expect_err("the answer lost");
            let AppError::StreamPre(e) = err else {
                panic!("expected a coded error")
            };
            assert_eq!(
                (e.status, e.code),
                (StatusCode::CONFLICT, "life_choice_closed")
            );
            assert_eq!(e.message, "already resolved or expired");
        }
        assert_eq!(
            s.row(choice).await,
            ("ignored".into(), Some(flipped), None),
            "untouched"
        );
        assert!(s.events().await.is_empty());
        assert_eq!(s.tiers().await, (3, 4), "no drop");
        assert!(s.drops().await.is_empty());
        assert!(s.life_choice_rows().await.is_empty());
        assert!(calls(&mock).await.is_empty(), "and nothing generated");
    }

    /// The race itself, on the production path: a message's conversion holds
    /// the choice's row lock while the answer arrives. The answer waits, then
    /// finds the choice converted and writes nothing.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_answer_that_waits_on_a_converting_message_loses(pool: PgPool) {
        let mock = companion_mock(Some(GOODBYE)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = Arc::new(scene(&pool).await);
        let choice = s.ask(3, recent(30)).await;
        let now = recent(0);

        let mut converting = pool.begin().await.unwrap();
        LifeChoiceRepo::resolve_in_tx(&mut converting, choice, "ignored", now)
            .await
            .unwrap()
            .expect("the message won the lock");
        let answering = tokio::spawn({
            let (s, state) = (s.clone(), state.clone());
            async move {
                s.respond(&state, choice, LifeChoiceAnswer::Accept, now)
                    .await
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!answering.is_finished(), "the answer waits for the lock");
        converting.commit().await.unwrap();

        let err = answering.await.unwrap().expect_err("the answer lost");
        let AppError::StreamPre(e) = err else {
            panic!("expected a coded error")
        };
        assert_eq!(e.status, StatusCode::CONFLICT);
        assert_eq!(s.row(choice).await, ("ignored".into(), Some(now), None));
        assert!(s.events().await.is_empty());
        assert_eq!(s.tiers().await, (3, 4));
        assert!(s.drops().await.is_empty());
        assert!(calls(&mock).await.is_empty());
    }

    /// A choice that lapsed (24 hours past its ask, the instant included) is
    /// closed; one microsecond earlier it still answers.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_expired_choice_is_409(pool: PgPool) {
        let mock = companion_mock(Some(HURT)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let micro = Duration::microseconds(1);
        let asked = recent(60 * 24 + 30);
        let expiry = asked + Duration::hours(24);

        for (at, answers) in [
            (expiry + micro, false),
            (expiry, false),
            (expiry - micro, true),
        ] {
            let s = scene(&pool).await;
            let choice = s.ask(3, asked).await;
            let out = s
                .respond(&state, choice, LifeChoiceAnswer::Decline, at)
                .await;
            if answers {
                assert_eq!(out.expect("still open").status, "declined");
            } else {
                let AppError::StreamPre(e) = out.expect_err("lapsed") else {
                    panic!("expected a coded error")
                };
                assert_eq!(e.status, StatusCode::CONFLICT, "at {at}");
                assert_eq!(s.row(choice).await, ("pending".into(), None, None));
                assert_eq!(s.tiers().await, (3, 4), "at {at}");
                assert!(s.drops().await.is_empty());
            }
        }
        assert_eq!(calls(&mock).await.len(), 1, "only the open one spoke");
    }

    /// A failed goodbye does not undo the answer: the resolution, the
    /// separation and the drop are committed before she speaks.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_failed_goodbye_keeps_the_resolution(pool: PgPool) {
        let failing = companion_mock(None).await;
        let state = with_companion(test_state(pool.clone()), &failing.uri());
        let s = scene(&pool).await;
        let choice = s.ask(3, recent(30)).await;
        let now = recent(0);

        let out = s
            .respond(&state, choice, LifeChoiceAnswer::Accept, now)
            .await
            .expect("still a success");

        assert_eq!(out.status, "accepted");
        assert!(out.message.is_none(), "she could not speak");
        assert!(out.event.is_some());
        assert_eq!(calls(&failing).await.len(), 1, "she was asked");
        let events = s.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(
            s.row(choice).await,
            ("accepted".into(), Some(now), Some(events[0].0))
        );
        assert_eq!(s.tiers().await, (2, 3));
        assert_eq!(s.drops().await.len(), 1);
        assert!(s.life_choice_rows().await.is_empty(), "no message row");
    }

    /// A blank hurt reply is the same: 200, `message: null`, the drop stands.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_blank_hurt_reply_keeps_the_resolution(pool: PgPool) {
        let blank = companion_mock(Some("  ")).await;
        let state = with_companion(test_state(pool.clone()), &blank.uri());
        let s = scene(&pool).await;
        let choice = s.ask(3, recent(30)).await;
        let now = recent(0);

        let out = s
            .respond(&state, choice, LifeChoiceAnswer::Decline, now)
            .await
            .expect("still a success");

        assert_eq!(out.status, "declined");
        assert!(out.message.is_none());
        assert_eq!(s.row(choice).await, ("declined".into(), Some(now), None));
        assert_eq!(s.tiers().await, (2, 3));
        assert!(s.life_choice_rows().await.is_empty());
    }

    /// The answer and its costs are one transaction: a separation that cannot
    /// be written (its key is taken) undoes the resolution and the drop, and
    /// the user is told so rather than charged for nothing.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_failed_separation_undoes_the_whole_answer(pool: PgPool) {
        let mock = companion_mock(Some(GOODBYE)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask(3, recent(30)).await;
        let now = recent(0);
        let (started, ends) = (now - Duration::minutes(20), now + Duration::minutes(40));
        sqlx::query(
            "INSERT INTO engine.persona_events \
               (instance_id, session_id, event_key, kind, started_at, ends_at) \
             VALUES ($1, $2, $3, 'taking_space', $4, $5)",
        )
        .bind(s.instance_id)
        .bind(s.session_id)
        .bind(format!("life_choice:{choice}:taking_space"))
        .bind(started)
        .bind(ends)
        .execute(&pool)
        .await
        .unwrap();

        let out = s
            .respond(&state, choice, LifeChoiceAnswer::Accept, now)
            .await;

        assert!(out.is_err(), "the key is taken");
        assert_eq!(s.row(choice).await, ("pending".into(), None, None));
        assert_eq!(s.tiers().await, (3, 4), "no drop");
        assert!(s.drops().await.is_empty());
        let events = s.events().await;
        assert_eq!(events.len(), 1, "no second row");
        assert_eq!(events[0].5, ends, "and the cap went down with it");
        assert!(calls(&mock).await.is_empty());
    }

    /// A choice that is not in the session is a 404, whoever asked and
    /// wherever it lives; the other session's choice stays pending.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_choice_of_another_session_is_404(pool: PgPool) {
        let mock = companion_mock(Some(GOODBYE)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let other = scene(&pool).await;
        let theirs = other.ask(3, recent(30)).await;
        let status = |e: AppError| match e {
            AppError::StreamPre(p) => (p.status, p.code),
            other => panic!("{other:?}"),
        };

        let wrong_session = s
            .respond(&state, theirs, LifeChoiceAnswer::Accept, recent(0))
            .await
            .err()
            .unwrap();
        assert_eq!(
            status(wrong_session),
            (StatusCode::NOT_FOUND, "life_choice_not_found")
        );
        let unknown = s
            .respond(&state, Uuid::new_v4(), LifeChoiceAnswer::Accept, recent(0))
            .await
            .err()
            .unwrap();
        assert_eq!(status(unknown).0, StatusCode::NOT_FOUND);
        assert_eq!(other.row(theirs).await.0, "pending");
        assert_eq!(other.tiers().await, (3, 4));
        assert!(calls(&mock).await.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_session_gate_applies(pool: PgPool) {
        let s = scene(&pool).await;
        let choice = s.ask(3, recent(30)).await;
        let state = test_state(pool.clone());
        let status = |e: AppError| match e {
            AppError::StreamPre(p) => p.status,
            other => panic!("{other:?}"),
        };
        let stranger = respond_at(
            &state,
            s.session_id,
            choice,
            Uuid::new_v4(),
            answer(LifeChoiceAnswer::Accept),
            recent(0),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(status(stranger), StatusCode::FORBIDDEN);
        let missing = respond_at(
            &state,
            Uuid::new_v4(),
            choice,
            s.user_id,
            answer(LifeChoiceAnswer::Accept),
            recent(0),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(status(missing), StatusCode::NOT_FOUND);
        assert_eq!(s.row(choice).await.0, "pending");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_byoc_session_is_refused(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_, session_id) = seed_byoc_session(&pool, user_id).await;
        let before = session_footprint(&pool, session_id).await;
        let err = respond_at(
            &test_state(pool.clone()),
            session_id,
            Uuid::new_v4(),
            user_id,
            answer(LifeChoiceAnswer::Accept),
            recent(0),
        )
        .await
        .err()
        .unwrap();
        let AppError::StreamPre(e) = err else {
            panic!("expected a coded error")
        };
        assert_eq!(
            (e.status, e.code),
            (StatusCode::BAD_REQUEST, "byoc_unsupported")
        );
        assert_eq!(session_footprint(&pool, session_id).await, before);
    }

    mod http {
        use super::*;
        use axum::body::Body;
        use axum::http::{header, Request};

        use crate::routes::companion::testutil::{build_router, mint_test_jwt, send_request};

        fn respond_req(
            session_id: Uuid,
            choice_id: Uuid,
            jwt: &str,
            body: serde_json::Value,
        ) -> Request<Body> {
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/v2/comp/session/{session_id}/life-choices/{choice_id}"
                ))
                .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        }

        /// The route is wired behind auth; anything but `accept` or `decline`
        /// is refused at deserialization before any state is touched.
        #[sqlx::test(migrations = "../eros-engine-store/migrations")]
        async fn the_route_is_wired_authed_and_reads_only_two_answers(pool: PgPool) {
            let mock = companion_mock(Some(HURT)).await;
            let s = scene(&pool).await;
            let choice = s.ask(3, recent(30)).await;
            let mut app = build_router(with_companion(test_state(pool.clone()), &mock.uri()));
            let jwt = mint_test_jwt(s.user_id);

            let (status, _) = send_request(
                &mut app,
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/v2/comp/session/{}/life-choices/{choice}",
                        s.session_id
                    ))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(json!({"response": "accept"}).to_string()))
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);

            for bad in [
                json!({}),
                json!({"response": "ignore"}),
                json!({"response": "Accept"}),
            ] {
                let (status, resp) = send_request(
                    &mut app,
                    respond_req(s.session_id, choice, &jwt, bad.clone()),
                )
                .await;
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {resp}");
            }
            // A `response` that is no string at all is refused too (serde_json
            // reads it as a syntax error, so it is a 400).
            for bad in [json!({"response": null}), json!({"response": 1})] {
                let (status, _) =
                    send_request(&mut app, respond_req(s.session_id, choice, &jwt, bad)).await;
                assert!(status.is_client_error(), "{status}");
            }
            assert_eq!(s.row(choice).await.0, "pending", "nothing was touched");

            let (status, body) = send_request(
                &mut app,
                respond_req(s.session_id, choice, &jwt, json!({"response": "decline"})),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["status"], json!("declined"));
            assert_eq!(body["event"], json!(null));
            assert_eq!(body["message"]["content"], json!(HURT));
            assert_eq!(body["message"]["occasion"], json!("life_choice"));
            assert_eq!(body["affinity"]["bond_tier"], json!(2));
            assert_eq!(body["affinity"]["chem_tier"], json!(3));

            let (status, body) = send_request(
                &mut app,
                respond_req(s.session_id, choice, &jwt, json!({"response": "decline"})),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["code"], json!("life_choice_closed"));
        }

        /// A voice or group session is not where she asks: 409 wrong_channel,
        /// like the events route, with nothing written.
        #[sqlx::test(migrations = "../eros-engine-store/migrations")]
        async fn voice_and_group_sessions_are_wrong_channel(pool: PgPool) {
            let s = scene(&pool).await;
            let choice = s.ask(3, recent(30)).await;
            let mut app = build_router(test_state(pool.clone()));
            let jwt = mint_test_jwt(s.user_id);

            sqlx::query("UPDATE engine.chat_sessions SET channel = 'voice' WHERE id = $1")
                .bind(s.session_id)
                .execute(&pool)
                .await
                .unwrap();
            let group = seed_group_session(&pool, s.user_id).await;
            for session in [s.session_id, group] {
                let (status, body) = send_request(
                    &mut app,
                    respond_req(session, choice, &jwt, json!({"response": "accept"})),
                )
                .await;
                assert_eq!(status, StatusCode::CONFLICT, "{body}");
                assert_eq!(body["code"], json!("wrong_channel"));
            }
            assert_eq!(s.row(choice).await.0, "pending");
            assert_eq!(s.tiers().await, (3, 4));
            assert!(s.events().await.is_empty());
        }
    }
}
