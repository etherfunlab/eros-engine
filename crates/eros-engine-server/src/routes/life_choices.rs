// SPDX-License-Identifier: AGPL-3.0-only
//! POST /v2/comp/session/{session_id}/life-choices/{choice_id} — the user
//! answers what the persona asked: accept or decline. What an answer does
//! depends on the choice's kind.
//!
//! - `confess`: an accept moves the status (`none` and `ex` to `dating`,
//!   `partner` to `fwb`, `peer` to `situationship`) and ends the incident
//!   under way; a decline keeps the status and drops chemistry one tier.
//! - `partner_up`: an accept makes them `partner` and ends the incident under
//!   way; a decline makes them `peer`. Neither drops anything.
//! - `propose`: an accept makes them `engaged` and ends the incident under
//!   way; a decline keeps the status and drops both lines one tier.
//! - `break_up`: an accept makes them `ex` and ends the cold war; a decline
//!   keeps the status and the cold war, starting one if none was on. Neither
//!   drops anything.
//! - `sleep_together`: an accept starts an hour of sex together and costs
//!   nothing; a decline drops chemistry one tier.
//! - `fall_asleep_together`: an accept starts one to four hours asleep
//!   together and costs nothing; a decline drops chemistry one tier.
//! - `travel_together`: an accept starts a trip; a decline changes nothing.
//! - `bathe_together`: an accept starts a 30-minute shower or bath together,
//!   one or the other at even odds; a decline drops chemistry one tier.
//! - `ask_for_space`: retired; a stored one is not answered (`409`).
//!
//! A status kind answered once the status is no longer one it is asked from
//! is closed (`409`) and writes nothing. The persona answers in one proactive
//! message, and the cost is applied with the answer, not with the message
//! (spec
//! docs/superpowers/specs/2026-10-19-life-events-shopping-gatherings-and-ask-for-space-design.md
//! §3.4; romantic kinds:
//! docs/superpowers/specs/2026-10-30-give-needs-romantic-choices-and-answer-receipts-design.md
//! §4.5; status kinds: docs/superpowers/specs/2026-11-23-affinity-50-design.md
//! §6.2, §7.2, §7.3).

use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_core::affinity::{Affinity, AffinityLine};
use eros_engine_core::events::{self, EventKind};
use eros_engine_core::persona::CompanionPersona;
use eros_engine_core::relationship::settle;
use eros_engine_core::scope::MemoryScope;
use eros_engine_store::affinity::{AffinityRepo, RelationshipCause};
use eros_engine_store::chat::ChatRepo;
use eros_engine_store::events::{EventRepo, NewEvent};
use eros_engine_store::life_choices::{LifeChoiceKind, LifeChoiceRecord, LifeChoiceRepo};
use std::collections::HashMap;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, StreamPreError};
use crate::holiday::{self, UserLocale};
use crate::pipeline::handlers::recall_query_text;
use crate::pipeline::proactive::{greet, greet_stream, Greeting, LifeChoiceCue, Occasion};
use crate::pipeline::stream::{ProtocolFrame, StreamErrorCode};
use crate::prompt::NowContext;
use crate::routes::companion::{
    validate_llm_audit, validate_prompt_traits, LlmAuditDto, PromptTraitDto,
};
use crate::routes::companion_stream::{
    resolve_text_turn, validate_tier, AffinityScopeDto, StreamPreErrorBody,
};
use crate::routes::dto::{sse_response, wants_stream, AffinitySnapshot};
use crate::routes::session_events::EventView;
use crate::routes::session_open::GreetingDto;
use crate::state::AppState;

/// Same per-user in-flight cap as every other LLM entry point.
const CONCURRENT_STREAMS_PER_USER: u32 = 3;

/// The user's answer to the ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LifeChoiceAnswer {
    /// Agree to what she asked.
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
    /// Answer as a server-sent event stream instead of JSON; an
    /// `Accept: text/event-stream` header asks for the same.
    #[serde(default)]
    pub stream: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LifeChoiceRespondResponse {
    /// The choice's new status: `accepted` or `declined`.
    pub status: String,
    /// What she says back: her answer to the user's yes or no. `null` when
    /// the generation failed, came back blank or a newer message landed
    /// first; the answer and its costs stand either way.
    pub message: Option<GreetingDto>,
    /// The relationship after the answer: a decline of `confess`,
    /// `sleep_together`, `fall_asleep_together` or `bathe_together` drops
    /// chemistry one tier and a declined `propose` both lines;
    /// `relationship_status` and `cold_war` carry what a `confess`,
    /// `partner_up`, `propose` or `break_up` answer moved.
    pub affinity: AffinitySnapshot,
    /// The trip a `travel_together` accept started, the shower or bath a
    /// `bathe_together` accept started, the `having_sex_together` a
    /// `sleep_together` accept started, or the `asleep_together` a
    /// `fall_asleep_together` accept started. `null` after any decline and
    /// for every `confess`, `partner_up`, `propose` or `break_up` answer.
    pub event: Option<EventView>,
}

/// The choice a proactive row asked and how it stands (spec 2026-10-30 §3),
/// carried on her ask row in both history routes so any client can draw the
/// user's answer between her ask and her reply.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct LifeChoiceReceiptDto {
    pub id: Uuid,
    /// `confess`, `partner_up`, `propose`, `break_up`, `sleep_together`,
    /// `fall_asleep_together`, `travel_together` or `bathe_together`; on
    /// older rows, the retired `ask_for_space`.
    pub kind: String,
    /// `pending`, `accepted`, `declined`, `ignored`, or `expired` for a
    /// `pending` choice past its 24 hours.
    pub status: String,
    /// `null` while `pending` or `expired`. For `accepted` / `declined`,
    /// when the answer landed; for `ignored`, the `sent_at` of the user
    /// message that answered her in words.
    pub resolved_at: Option<DateTime<Utc>>,
}

impl LifeChoiceReceiptDto {
    fn at(c: &LifeChoiceRecord, now: DateTime<Utc>) -> Self {
        Self {
            id: c.id,
            kind: c.kind.clone(),
            status: c.status_at(now).to_owned(),
            resolved_at: c.resolved_at,
        }
    }
}

/// `message_id → receipt` for the rows of one history page: one lookup.
pub(crate) async fn receipts_for(
    pool: &sqlx::PgPool,
    message_ids: &[Uuid],
    now: DateTime<Utc>,
) -> Result<HashMap<Uuid, LifeChoiceReceiptDto>, sqlx::Error> {
    Ok((LifeChoiceRepo { pool })
        .by_message_ids(message_ids)
        .await?
        .iter()
        .filter_map(|c| Some((c.message_id?, LifeChoiceReceiptDto::at(c, now))))
        .collect())
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

/// Answer what she asked.
///
/// One transaction answers the choice and pays for it; what an answer does
/// depends on the choice's `kind`. An accept makes the choice `accepted`, a
/// decline `declined`.
///
/// `confess`: an accept moves the relationship status by where it stands:
/// `none` or `ex` to `dating`, `partner` to `fwb`, `peer` to `situationship`;
/// an incident under way ends, and it costs nothing. A decline keeps the
/// status and drops chemistry one tier.
///
/// `partner_up`: an accept makes the status `partner` and a decline `peer`;
/// an accept ends an incident under way. Neither costs anything.
///
/// `propose`: an accept makes the status `engaged` and costs nothing; an
/// incident under way ends. A decline keeps the status and drops bond and
/// chemistry one tier each.
///
/// `break_up`: an accept makes the status `ex` and ends the cold war. A
/// decline keeps the status and the cold war, starting one now if none was
/// on. Neither costs anything.
///
/// `sleep_together`: an accept starts a `having_sex_together` incident of one
/// hour now, costing nothing; an incident under way ends. A decline drops
/// chemistry one tier.
///
/// `fall_asleep_together`: an accept starts an `asleep_together` incident of
/// one to four hours now, costing nothing; she keeps replying, and an incident
/// under way ends. A decline drops chemistry one tier.
///
/// `travel_together`: an accept starts a `traveling_together` background of
/// two to three days now; she keeps replying, and an incident under way ends.
/// Neither answer costs anything.
///
/// `bathe_together`: an accept starts a `showering_together` or
/// `bathing_together` incident of 30 minutes now, one or the other at even
/// odds, costing nothing; an incident under way ends. A decline drops
/// chemistry one tier.
///
/// Either way she then answers in one proactive message, persisted like any
/// reply and returned; if that generation fails the call still succeeds with
/// `message: null`. `event` is `null` for every answer but a
/// `travel_together`, `bathe_together`, `sleep_together` or
/// `fall_asleep_together` accept.
///
/// `409` when the choice is no longer pending: answered already (an ordinary
/// message from the user counts, and the first writer wins), or lapsed 24
/// hours after she asked; when the status has moved to one the kind is not
/// asked from (`confess`, `partner_up`, `propose`, `break_up`), with nothing
/// written; and for the retired `ask_for_space`. `404` when the choice is not
/// in this session.
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
        (status = 200, description = "The answer and her reply. With `stream: true` in the body \
            or `Accept: text/event-stream`, a server-sent event stream instead: a `resolved` \
            frame first (`{type, status, affinity, event?}`, sent once the answer is \
            committed), then the chat stream's frames for her reply: `meta` → `delta`… → \
            `done` → `final`, or `final` alone when she says nothing. An `error` frame after \
            `resolved` leaves the answer in place and is never retryable.",
            content(
                (LifeChoiceRespondResponse = "application/json"),
                (String = "text/event-stream")
            )),
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
    headers: HeaderMap,
    Json(req): Json<LifeChoiceRespondRequest>,
) -> Result<Response, AppError> {
    let now = Utc::now();
    if wants_stream(&headers, req.stream) {
        return respond_stream_at(&state, session_id, choice_id, user_id, req, now).await;
    }
    Ok(Json(respond_at(&state, session_id, choice_id, user_id, req, now).await?).into_response())
}

/// The answer, committed, and what she is about to say about it.
struct Resolved {
    /// The user's in-flight slot.
    guard: crate::state::StreamSlotGuard,
    choice_id: Uuid,
    status: &'static str,
    affinity: AffinitySnapshot,
    event: Option<EventView>,
    /// `None` when her reply could not even be set up; the answer stands.
    greeting: Option<Greeting>,
}

/// The JSON handler with its clock injected, for tests.
pub(crate) async fn respond_at(
    state: &AppState,
    session_id: Uuid,
    choice_id: Uuid,
    user_id: Uuid,
    req: LifeChoiceRespondRequest,
    now: DateTime<Utc>,
) -> Result<LifeChoiceRespondResponse, AppError> {
    let r = resolve_choice(state, session_id, choice_id, user_id, req, now).await?;
    let _guard = r.guard;
    // Her reply is the last step and cannot undo the answer: whatever goes
    // wrong here (the provider, a blank reply, a newer message) leaves the
    // resolution, the event, the status and the drop in place, and she says
    // nothing.
    let message = match r.greeting {
        Some(g) => match greet(state, g).await {
            Ok(m) => m.map(GreetingDto::from),
            Err(e) => {
                tracing::warn!(%session_id, choice_id = %r.choice_id, "life-choice reply failed: {e}");
                None
            }
        },
        None => None,
    };
    Ok(LifeChoiceRespondResponse {
        status: r.status.into(),
        message,
        affinity: r.affinity,
        event: r.event,
    })
}

/// The SSE handler (spec 2026-11-10-proactive-reply-streaming-design.md
/// §4): `resolved` first, then her reply as it is generated. A failure after
/// `resolved` is an `error` frame that is never retryable: the answer is in.
pub(crate) async fn respond_stream_at(
    state: &AppState,
    session_id: Uuid,
    choice_id: Uuid,
    user_id: Uuid,
    req: LifeChoiceRespondRequest,
    now: DateTime<Utc>,
) -> Result<Response, AppError> {
    let r = resolve_choice(state, session_id, choice_id, user_id, req, now).await?;
    let state = state.clone();
    Ok(sse_response(async_stream::stream! {
        // Held while she speaks; a dropped connection drops it with the stream.
        let _guard = r.guard;
        yield ProtocolFrame::Resolved {
            status: r.status.into(),
            affinity: Box::new(r.affinity),
            event: r.event,
        };
        match r.greeting {
            Some(g) => {
                for await frame in greet_stream(state, g, Some(false)) {
                    yield frame;
                }
            }
            None => {
                yield ProtocolFrame::Error {
                    code: StreamErrorCode::Internal,
                    retryable: false,
                    message: "internal error".into(),
                    user_message: "服务出现问题，请稍后再试".into(),
                    upstream_status: None,
                    provider_code: None,
                };
            }
        }
    }))
}

/// Validate the request and commit the answer with its costs.
async fn resolve_choice(
    state: &AppState,
    session_id: Uuid,
    choice_id: Uuid,
    user_id: Uuid,
    req: LifeChoiceRespondRequest,
    now: DateTime<Utc>,
) -> Result<Resolved, AppError> {
    validate_tier(req.tier.as_deref())?;
    let prompt_traits = validate_prompt_traits(req.prompt_traits.as_deref().unwrap_or(&[]))
        .map_err(|e| invalid_payload(e.to_string()))?;
    let audit =
        validate_llm_audit(req.audit.clone()).map_err(|e| invalid_payload(e.to_string()))?;
    let (_session, persona, instance_id) =
        resolve_text_turn(state, session_id, user_id, "life choices").await?;

    let asked = (LifeChoiceRepo { pool: &state.pool })
        .get(choice_id)
        .await?
        .filter(|c| c.session_id == session_id)
        .ok_or_else(not_found)?;
    // The kind is fixed at ask time; one this build cannot read is a fault in
    // the stored data, refused before anything is written.
    let kind = LifeChoiceKind::parse(&asked.kind)
        .ok_or_else(|| AppError::Internal("stored life choice kind unreadable".into()))?;

    // A retired ask is not answered: a row still pending lapses on its own.
    if kind == LifeChoiceKind::AskForSpace {
        return Err(closed());
    }

    // Taken before anything is written: a call over the cap is refused with
    // the choice still open, not answered and then left without her reply.
    let guard = state
        .stream_slots
        .try_acquire(user_id, CONCURRENT_STREAMS_PER_USER)
        .ok_or_else(|| AppError::TooManyRequests("per-user in-flight cap reached".into()))?;

    // The answer and its costs are one transaction. The resolve goes first:
    // it is the race guard, and a loser (answered, converted by a message or
    // lapsed) writes nothing. Then the event, which takes the instance's lock,
    // then the status and the drop, under the affinity row's lock, in the
    // order every writer of the two takes them.
    let accepted = req.response == LifeChoiceAnswer::Accept;
    let (status, resolution) = match req.response {
        LifeChoiceAnswer::Accept => ("accepted", "accept"),
        LifeChoiceAnswer::Decline => ("declined", "decline"),
    };
    let mut tx = state.pool.begin().await?;
    let Some(choice) = LifeChoiceRepo::resolve_in_tx(&mut tx, choice_id, status, now).await? else {
        return Err(closed());
    };
    // The event an accept starts: a trip, a shower or a bath, sex, or falling
    // asleep together. A running incident ends; a background keeps going.
    let placed = match (kind, req.response) {
        (LifeChoiceKind::TravelTogether, LifeChoiceAnswer::Accept) => Some((
            EventKind::TravelingTogether,
            events::lasts(EventKind::TravelingTogether, rand::random()),
        )),
        (LifeChoiceKind::BatheTogether, LifeChoiceAnswer::Accept) => {
            // A shower or a bath, at even odds.
            let washing = if rand::random::<bool>() {
                EventKind::ShoweringTogether
            } else {
                EventKind::BathingTogether
            };
            Some((washing, events::lasts(washing, rand::random())))
        }
        (LifeChoiceKind::SleepTogether, LifeChoiceAnswer::Accept) => Some((
            EventKind::HavingSexTogether,
            events::lasts(EventKind::HavingSexTogether, rand::random()),
        )),
        (LifeChoiceKind::FallAsleepTogether, LifeChoiceAnswer::Accept) => Some((
            EventKind::AsleepTogether,
            events::lasts(EventKind::AsleepTogether, rand::random()),
        )),
        _ => None,
    };
    let started = match placed {
        Some((event_kind, lasts)) => {
            let event = EventRepo::insert_preempting_in_tx(
                &mut tx,
                &NewEvent {
                    instance_id,
                    session_id,
                    event_key: &format!("life_choice:{}:{}", choice.id, event_kind.as_str()),
                    kind: event_kind,
                    started_at: now,
                    ends_at: now + lasts,
                },
            )
            .await?;
            LifeChoiceRepo::set_event_in_tx(&mut tx, choice.id, event.id).await?;
            Some((event, lasts))
        }
        None => None,
    };
    // An accept that places nothing still ends what she was doing; an
    // accepted break-up leaves it.
    if accepted
        && matches!(
            kind,
            LifeChoiceKind::Confess | LifeChoiceKind::PartnerUp | LifeChoiceKind::Propose
        )
    {
        EventRepo::end_foreground_in_tx(&mut tx, instance_id, now).await?;
    }
    // The status the answer moves, settled from where it stands under the
    // row's lock. A status the kind is no longer asked from (it moved while
    // the ask waited) closes the answer: the dropped transaction writes
    // nothing. Before the drop, so the drop's read already holds the move.
    if let Some(ask) = kind.status_ask() {
        let from = AffinityRepo::relationship_for_update_in_tx(&mut tx, session_id)
            .await?
            .ok_or_else(|| AppError::Internal("life choice without an affinity row".into()))?;
        let Some(to) = settle(ask, from, accepted, now) else {
            return Err(closed());
        };
        AffinityRepo::write_relationship_in_tx(
            &mut tx,
            session_id,
            from,
            to,
            RelationshipCause::Choice(choice.id),
        )
        .await?;
    }
    // The lines a decline costs; an accept costs none.
    let lines: Option<&[AffinityLine]> = match (kind, req.response) {
        (_, LifeChoiceAnswer::Accept) => None,
        (
            LifeChoiceKind::SleepTogether
            | LifeChoiceKind::FallAsleepTogether
            | LifeChoiceKind::Confess
            | LifeChoiceKind::BatheTogether,
            LifeChoiceAnswer::Decline,
        ) => Some(&[AffinityLine::Chemistry]),
        (LifeChoiceKind::Propose, LifeChoiceAnswer::Decline) => {
            Some(&[AffinityLine::Bond, AffinityLine::Chemistry])
        }
        (
            LifeChoiceKind::TravelTogether | LifeChoiceKind::PartnerUp | LifeChoiceKind::BreakUp,
            LifeChoiceAnswer::Decline,
        ) => None,
        (LifeChoiceKind::AskForSpace, LifeChoiceAnswer::Decline) => {
            unreachable!("a retired ask_for_space is refused before the answer")
        }
    };
    let dropped = match lines {
        Some(lines) => Some(
            AffinityRepo::persist_life_choice_drop_in_tx(
                &mut tx,
                session_id,
                choice.id,
                resolution,
                lines,
                &state.config.affinity_tuning,
            )
            .await?,
        ),
        None => None,
    };
    tx.commit().await?;
    let affinity = match dropped {
        Some(a) => a,
        // Nothing changed: the relationship as it stands, read the way every
        // snapshot is. A choice is asked only once the row exists.
        None => match (AffinityRepo { pool: &state.pool })
            .load(session_id)
            .await?
        {
            Some(mut a) => {
                a.apply_time_decay();
                a.refresh_endpoints(&state.config.affinity_tuning);
                a
            }
            None => Affinity::unrecorded(),
        },
    };

    let cue = match (kind, &started) {
        (LifeChoiceKind::TravelTogether, Some((_, lasts))) => LifeChoiceCue::TravelAgreed {
            hours: lasts.num_hours(),
        },
        (LifeChoiceKind::BatheTogether, Some((e, _))) if e.kind == EventKind::ShoweringTogether => {
            LifeChoiceCue::ShowerAgreed
        }
        (kind, _) => LifeChoiceCue::Answered { kind, accepted },
    };
    let greeting = match say(
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
        Ok(g) => Some(g),
        Err(e) => {
            tracing::warn!(%session_id, choice_id = %choice.id, "life-choice reply failed: {e}");
            None
        }
    };

    let traits = crate::prompt::persona_traits(&persona);
    Ok(Resolved {
        guard,
        choice_id: choice.id,
        status,
        affinity: AffinitySnapshot::from(affinity),
        event: started.map(|(e, _)| EventView::new(&e, &traits)),
        greeting,
    })
}

/// What she says about the answer, ready to generate. Its insert yields to a
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
) -> Result<Greeting, AppError> {
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
    Ok(Greeting {
        session_id,
        user_id,
        instance_id,
        persona: persona.clone(),
        now: NowContext::for_user(&locale, now),
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
        message_id: None,
    })
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(respond_to_life_choice))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, SubsecRound};
    use eros_engine_core::relationship::RelationshipStatus;
    use serde_json::json;
    use sqlx::PgPool;
    use std::sync::Arc;
    use wiremock::matchers::{body_string_contains, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{
        frame_types, read_sse, seed_byoc_session, seed_genome, seed_group_session, seed_instance,
        seed_session, session_footprint, upstream_sse,
    };
    use crate::routes::session_open::GreetingOccasion;

    const GOODBYE: &str = "好，那我们先各自安静一下。";
    const HURT: &str = "你不愿意啊……我有点难过。";
    const SLEEP_ACCEPT_CUE: &str = "（对方答应了你的邀请。）";
    const SLEEP_DECLINE_CUE: &str = "（对方拒绝了你的邀请，你有点失落。）";
    const PROPOSE_ACCEPT_CUE: &str = "（对方答应了你的求婚。）";
    const PROPOSE_DECLINE_CUE: &str = "（对方拒绝了你的求婚，你心里很难受。）";

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
            stream: false,
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
        /// A pending `ask_for_space` asked at `asked_at` with `tier_gap`;
        /// returns its id.
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

        /// A pending choice of a kind that carries no gap (the 0083 CHECK
        /// wants it NULL); returns its id.
        async fn ask_kind(&self, kind: &str, asked_at: DateTime<Utc>) -> Uuid {
            sqlx::query_scalar(
                "INSERT INTO engine.life_choices (session_id, kind, tier_gap, asked_at) \
                 VALUES ($1, $2, NULL, $3) RETURNING id",
            )
            .bind(self.session_id)
            .bind(kind)
            .bind(asked_at)
            .fetch_one(&self.pool)
            .await
            .unwrap()
        }

        /// (trust, intrigue, intimacy, tension) as stored.
        async fn axes(&self) -> (f64, f64, f64, f64) {
            sqlx::query_as(
                "SELECT trust, intrigue, intimacy, tension \
                   FROM engine.companion_affinity WHERE session_id = $1",
            )
            .bind(self.session_id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
        }

        /// How many affinity events of any type the session has.
        async fn affinity_event_count(&self) -> i64 {
            sqlx::query_scalar(
                "SELECT count(*) FROM engine.companion_affinity_events e \
                   JOIN engine.companion_affinity a ON a.id = e.affinity_id \
                  WHERE a.session_id = $1",
            )
            .bind(self.session_id)
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

        /// Set the session's relationship status and cold war directly.
        async fn set_status(&self, status: &str, cold_war_since: Option<DateTime<Utc>>) {
            sqlx::query(
                "UPDATE engine.companion_affinity \
                    SET relationship_status = $2, cold_war_since = $3 \
                  WHERE session_id = $1",
            )
            .bind(self.session_id)
            .bind(status)
            .bind(cold_war_since)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        /// (relationship_status, cold_war_since) as stored.
        async fn relationship(&self) -> (String, Option<DateTime<Utc>>) {
            sqlx::query_as(
                "SELECT relationship_status, cold_war_since \
                   FROM engine.companion_affinity WHERE session_id = $1",
            )
            .bind(self.session_id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
        }

        /// The `context` of every `relationship` affinity event of the session.
        async fn relationship_events(&self) -> Vec<serde_json::Value> {
            sqlx::query_scalar(
                "SELECT e.context FROM engine.companion_affinity_events e \
                   JOIN engine.companion_affinity a ON a.id = e.affinity_id \
                  WHERE a.session_id = $1 AND e.event_type = 'relationship' \
                  ORDER BY e.created_at",
            )
            .bind(self.session_id)
            .fetch_all(&self.pool)
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
        /// One event of `kind` under way from `started_at` to `ends_at`.
        async fn put(&self, kind: &str, started_at: DateTime<Utc>, ends_at: DateTime<Utc>) {
            sqlx::query(
                "INSERT INTO engine.persona_events \
                   (instance_id, session_id, event_key, kind, started_at, ends_at) \
                 VALUES ($1, $2, $3, $3, $4, $5)",
            )
            .bind(self.instance_id)
            .bind(self.session_id)
            .bind(kind)
            .bind(started_at)
            .bind(ends_at)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        /// When the event of `kind` ends.
        async fn ends_at(&self, kind: &str) -> DateTime<Utc> {
            self.events()
                .await
                .into_iter()
                .find(|e| e.1 == kind)
                .unwrap_or_else(|| panic!("no {kind}"))
                .5
        }

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

    /// The last `user` turn of the companion request: the stage cue.
    fn last_user_turn(call: &serde_json::Value) -> serde_json::Value {
        let last = call["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["role"], json!("user"));
        last["content"].clone()
    }

    /// A romantic accept costs nothing: no tier change, nothing written to the
    /// lines or the drop log; an accepted `propose` moves the status alone,
    /// with its one `relationship` event. She says yes-cue back and the
    /// response carries the relationship as it stands. Only `propose` places
    /// no event; `sleep_together`'s has its own test.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_romantic_accept_costs_nothing(pool: PgPool) {
        let mock = companion_mock(Some("好呀。")).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let repo = AffinityRepo { pool: &pool };

        for kind in ["sleep_together", "propose"] {
            let s = scene(&pool).await;
            s.set_status("dating", None).await;
            let choice = s.ask_kind(kind, recent(30)).await;
            let stored = repo.load(s.session_id).await.unwrap().expect("row");
            let events_before = s.affinity_event_count().await;
            let now = recent(0);

            let out = s
                .respond(&state, choice, LifeChoiceAnswer::Accept, now)
                .await
                .unwrap();

            assert_eq!(out.status, "accepted", "{kind}");
            if kind == "propose" {
                assert!(out.event.is_none(), "{kind}: no event");
                assert!(s.events().await.is_empty(), "{kind}: no persona event");
                assert_eq!(s.row(choice).await, ("accepted".into(), Some(now), None));
            } else {
                let (status, resolved_at, _) = s.row(choice).await;
                assert_eq!((status.as_str(), resolved_at), ("accepted", Some(now)));
            }
            // An accepted proposal's `relationship` event is the one row it adds.
            let moved = i64::from(kind == "propose");
            assert_eq!(
                s.affinity_event_count().await,
                events_before + moved,
                "{kind}"
            );
            assert!(s.drops().await.is_empty(), "{kind}");

            // The lines are untouched, `updated_at` included.
            let after = repo.load(s.session_id).await.unwrap().expect("row");
            assert_eq!(s.tiers().await, (3, 4), "{kind}");
            assert_eq!(s.axes().await, (0.45, 0.45, 0.70, 0.70), "{kind}");
            assert_eq!(after.updated_at, stored.updated_at, "{kind}");

            // The response is the relationship as it was, read the way every
            // snapshot is: decayed, endpoints refreshed; engaged after a
            // proposal.
            let mut expected = stored.clone();
            if kind == "propose" {
                expected.relationship.status = RelationshipStatus::Engaged;
            }
            expected.apply_time_decay();
            expected.refresh_endpoints(&state.config.affinity_tuning);
            assert_eq!(
                serde_json::to_value(&out.affinity).unwrap(),
                serde_json::to_value(AffinitySnapshot::from(expected)).unwrap(),
                "{kind}"
            );

            let said = out.message.expect("a reply");
            assert_eq!(said.content, "好呀。");
            assert_eq!(said.occasion, GreetingOccasion::LifeChoice);
            assert_eq!(s.life_choice_rows().await.len(), 1, "{kind}");
        }

        let c = calls(&mock).await;
        assert_eq!(c.len(), 2);
        assert_eq!(last_user_turn(&c[0]), json!(SLEEP_ACCEPT_CUE));
        assert_eq!(last_user_turn(&c[1]), json!(PROPOSE_ACCEPT_CUE));
    }

    /// A romantic accept ends the incident under way, whether it places an
    /// event (`sleep_together`) or nothing (`propose`); a background keeps
    /// running.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_romantic_accept_ends_the_incident_but_not_the_trip(pool: PgPool) {
        let mock = companion_mock(Some("好呀。")).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        for kind in ["sleep_together", "propose"] {
            let s = scene(&pool).await;
            s.set_status("dating", None).await;
            let choice = s.ask_kind(kind, recent(30)).await;
            let now = recent(0);
            let trip_ends = now + Duration::days(1);
            s.put(
                "reading",
                now - Duration::minutes(20),
                now + Duration::minutes(100),
            )
            .await;
            s.put("short_trip", now - Duration::days(1), trip_ends)
                .await;

            let out = s
                .respond(&state, choice, LifeChoiceAnswer::Accept, now)
                .await
                .unwrap();

            match kind {
                "propose" => assert!(out.event.is_none(), "{kind}: nothing placed"),
                _ => assert_eq!(
                    serde_json::to_value(out.event.expect("the event")).unwrap()["kind"],
                    json!("having_sex_together"),
                    "{kind}"
                ),
            }
            assert_eq!(s.ends_at("reading").await, now, "{kind}: the incident ends");
            assert_eq!(
                s.ends_at("short_trip").await,
                trip_ends,
                "{kind}: the trip goes on"
            );
        }
    }

    /// A decline leaves the incident under way alone.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn declining_sleep_together_leaves_the_incident(pool: PgPool) {
        let mock = companion_mock(Some(HURT)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask_kind("sleep_together", recent(30)).await;
        let now = recent(0);
        let reading_ends = now + Duration::minutes(100);
        s.put("reading", now - Duration::minutes(20), reading_ends)
            .await;

        s.respond(&state, choice, LifeChoiceAnswer::Decline, now)
            .await
            .unwrap();

        assert_eq!(s.ends_at("reading").await, reading_ends);
    }

    /// A bath together is an incident too: going to bed ends it.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn accepting_sleep_together_ends_a_bath_together(pool: PgPool) {
        let mock = companion_mock(Some("好呀。")).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask_kind("sleep_together", recent(30)).await;
        let now = recent(0);
        s.put(
            "bathing_together",
            now - Duration::minutes(10),
            now + Duration::minutes(20),
        )
        .await;

        s.respond(&state, choice, LifeChoiceAnswer::Accept, now)
            .await
            .unwrap();

        assert_eq!(s.ends_at("bathing_together").await, now);
        let sex: Vec<_> = s
            .events()
            .await
            .into_iter()
            .filter(|e| e.1 == "having_sex_together")
            .collect();
        assert_eq!(sex.len(), 1, "{sex:?}");
        assert_eq!(sex[0].4, now, "it starts where the bath ends");
    }

    /// An accepted trip starts now, lasts two to three days and costs nothing.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn accepting_a_trip_starts_it_and_costs_nothing(pool: PgPool) {
        let mock = companion_mock(Some("太好了！")).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask_kind("travel_together", recent(30)).await;
        let axes = s.axes().await;
        let now = recent(0);

        let out = s
            .respond(&state, choice, LifeChoiceAnswer::Accept, now)
            .await
            .unwrap();

        assert_eq!(out.status, "accepted");
        let event = serde_json::to_value(out.event.expect("the trip")).unwrap();
        assert_eq!(event["kind"], json!("traveling_together"));
        assert_eq!(event["offline"], json!(false));
        let events = s.events().await;
        assert_eq!(events.len(), 1);
        let (id, kind, key, _, started_at, ends_at, message_id) = events[0].clone();
        assert_eq!(kind, "traveling_together");
        assert_eq!(key, format!("life_choice:{choice}:traveling_together"));
        assert_eq!(started_at, now);
        assert_eq!(message_id, None);
        let hours = (ends_at - started_at).num_hours();
        assert!((48..=72).contains(&hours), "{hours} hours");
        assert_eq!(
            s.row(choice).await,
            ("accepted".into(), Some(now), Some(id))
        );

        assert_eq!(s.axes().await, axes);
        assert_eq!(s.tiers().await, (3, 4));
        assert!(s.drops().await.is_empty());

        assert_eq!(out.message.expect("a reply").content, "太好了！");
        let span = crate::prompt::span_text(Duration::hours(hours));
        let cue = last_user_turn(&calls(&mock).await[0]);
        assert!(cue.as_str().unwrap().contains(&span), "{cue}");
    }

    /// Turning a trip down costs nothing: she is only disappointed.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn declining_a_trip_costs_nothing(pool: PgPool) {
        let mock = companion_mock(Some("好吧。")).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask_kind("travel_together", recent(30)).await;
        let axes = s.axes().await;
        let now = recent(0);

        let out = s
            .respond(&state, choice, LifeChoiceAnswer::Decline, now)
            .await
            .unwrap();

        assert_eq!(out.status, "declined");
        assert!(out.event.is_none());
        assert!(s.events().await.is_empty());
        assert_eq!(s.row(choice).await, ("declined".into(), Some(now), None));
        assert_eq!(s.axes().await, axes);
        assert_eq!(s.tiers().await, (3, 4));
        assert!(s.drops().await.is_empty());
        assert_eq!(out.message.expect("a reply").content, "好吧。");
    }

    /// An accepted bath places a 30-minute shower or bath together, one or
    /// the other at even odds, and costs nothing; her reply's cue names the
    /// one placed.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn accepting_a_bath_places_a_shower_or_a_bath(pool: PgPool) {
        let mock = companion_mock(Some("来吧。")).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let mut seen = std::collections::BTreeSet::new();
        // Both kinds turn up within 64 accepts but for a 2^-63 chance.
        for _ in 0..64 {
            let s = scene(&pool).await;
            let choice = s.ask_kind("bathe_together", recent(30)).await;
            let axes = s.axes().await;
            let now = recent(0);

            let out = s
                .respond(&state, choice, LifeChoiceAnswer::Accept, now)
                .await
                .unwrap();

            assert_eq!(out.status, "accepted");
            let event = serde_json::to_value(out.event.expect("the event")).unwrap();
            assert_eq!(event["offline"], json!(false));
            let events = s.events().await;
            assert_eq!(events.len(), 1);
            let (id, kind, key, _, started_at, ends_at, _) = events[0].clone();
            assert_eq!(event["kind"], json!(kind));
            assert_eq!(key, format!("life_choice:{choice}:{kind}"));
            assert_eq!(started_at, now);
            assert_eq!(ends_at - started_at, Duration::minutes(30));
            assert_eq!(
                s.row(choice).await,
                ("accepted".into(), Some(now), Some(id))
            );
            assert_eq!(s.axes().await, axes);
            assert!(s.drops().await.is_empty());
            let cue = match kind.as_str() {
                "bathing_together" => "（对方答应了和你一起洗澡，你们现在一起泡在浴缸里。）",
                "showering_together" => "（对方答应了和你一起洗澡，你们现在一起站在花洒下。）",
                other => panic!("a bath accept placed {other}"),
            };
            let c = calls(&mock).await;
            assert_eq!(last_user_turn(c.last().unwrap()), json!(cue), "{kind}");
            seen.insert(kind);
            if seen.len() == 2 {
                break;
            }
        }
        assert_eq!(seen.len(), 2, "both a shower and a bath: {seen:?}");
    }

    /// An accepted invitation to bed places `having_sex_together`, as the
    /// user's accepted `sex_together` does: one hour from now, linked to the
    /// choice and returned as `event`. Her reply reads it as under way, and the
    /// draw no longer reads her as a virgin.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn accepting_sleep_together_places_having_sex_together(pool: PgPool) {
        let mock = companion_mock(Some("好呀。")).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask_kind("sleep_together", recent(30)).await;
        let now = recent(0);

        let out = s
            .respond(&state, choice, LifeChoiceAnswer::Accept, now)
            .await
            .unwrap();

        assert_eq!(out.status, "accepted");
        let event = serde_json::to_value(out.event.expect("the event")).unwrap();
        assert_eq!(event["kind"], json!("having_sex_together"));
        assert_eq!(event["offline"], json!(false));
        let events = s.events().await;
        assert_eq!(events.len(), 1, "{events:?}");
        let (id, kind, key, _, started_at, ends_at, message_id) = events[0].clone();
        assert_eq!(kind, "having_sex_together");
        assert_eq!(key, format!("life_choice:{choice}:having_sex_together"));
        assert_eq!(started_at, now);
        assert_eq!(ends_at - started_at, Duration::hours(1));
        assert_eq!(message_id, None);
        assert_eq!(
            s.row(choice).await,
            ("accepted".into(), Some(now), Some(id))
        );
        assert!(EventRepo { pool: &pool }
            .ever(s.instance_id, &EventKind::SEX)
            .await
            .unwrap());

        let c = calls(&mock).await;
        assert_eq!(last_user_turn(&c[0]), json!(SLEEP_ACCEPT_CUE));
        assert!(
            c[0].to_string().contains("和对方上床"),
            "her reply reads the event as under way"
        );
    }

    /// Turning the bath down costs chemistry one tier, as the bed does.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn declining_a_bath_drops_chemistry(pool: PgPool) {
        let mock = companion_mock(Some(HURT)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask_kind("bathe_together", recent(30)).await;
        let (trust, intrigue, _, _) = s.axes().await;
        let now = recent(0);

        let out = s
            .respond(&state, choice, LifeChoiceAnswer::Decline, now)
            .await
            .unwrap();

        assert_eq!(out.status, "declined");
        assert!(out.event.is_none());
        assert!(s.events().await.is_empty());
        let (t, i, _, _) = s.axes().await;
        assert_eq!((t, i), (trust, intrigue), "the bond axes did not move");
        assert_eq!(s.tiers().await, (3, 3), "chemistry one tier lower");
        assert_eq!(
            s.drops().await,
            vec![json!({"life_choice": "decline", "choice_id": choice})]
        );
        assert_eq!(
            last_user_turn(&calls(&mock).await[0]),
            json!("（对方没有答应和你一起洗澡，你有点失落。）")
        );
    }

    /// Declining the invitation costs chemistry one tier and nothing else:
    /// the bond line keeps its axes, to the bit.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn declining_sleep_together_drops_chemistry_only(pool: PgPool) {
        let mock = companion_mock(Some(HURT)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask_kind("sleep_together", recent(30)).await;
        let (trust, intrigue, intimacy, tension) = s.axes().await;
        let now = recent(0);

        let out = s
            .respond(&state, choice, LifeChoiceAnswer::Decline, now)
            .await
            .unwrap();

        assert_eq!(out.status, "declined");
        assert!(out.event.is_none());
        assert!(s.events().await.is_empty());
        assert_eq!(s.row(choice).await, ("declined".into(), Some(now), None));
        let (t, i, m, n) = s.axes().await;
        assert_eq!((t, i), (trust, intrigue), "the bond axes did not move");
        assert!(m < intimacy && n < tension, "chemistry fell");
        assert_eq!(
            s.tiers().await,
            (3, 3),
            "chemistry one tier lower, bond kept"
        );
        assert_eq!((out.affinity.bond_tier, out.affinity.chem_tier), (3, 3));
        assert_eq!(
            s.drops().await,
            vec![json!({"life_choice": "decline", "choice_id": choice})]
        );

        assert_eq!(out.message.expect("a reply").content, HURT);
        let c = calls(&mock).await;
        assert_eq!(c.len(), 1);
        assert_eq!(last_user_turn(&c[0]), json!(SLEEP_DECLINE_CUE));
    }

    /// Declining a proposal costs what v1's decline costs: both lines, one
    /// tier each.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn declining_a_proposal_drops_both_lines(pool: PgPool) {
        let mock = companion_mock(Some(HURT)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        s.set_status("dating", None).await;
        let choice = s.ask_kind("propose", recent(30)).await;
        let now = recent(0);

        let out = s
            .respond(&state, choice, LifeChoiceAnswer::Decline, now)
            .await
            .unwrap();

        assert_eq!(out.status, "declined");
        assert!(out.event.is_none());
        assert!(s.events().await.is_empty());
        assert_eq!(s.row(choice).await, ("declined".into(), Some(now), None));
        assert_eq!(s.tiers().await, (2, 3));
        assert_eq!((out.affinity.bond_tier, out.affinity.chem_tier), (2, 3));
        assert_eq!(
            s.drops().await,
            vec![json!({"life_choice": "decline", "choice_id": choice})]
        );
        assert_eq!(out.message.expect("a reply").content, HURT);
        let c = calls(&mock).await;
        assert_eq!(c.len(), 1);
        assert_eq!(last_user_turn(&c[0]), json!(PROPOSE_DECLINE_CUE));
    }

    /// Spec 2026-11-23-affinity-50 §6.2: an accepted confession moves the
    /// status by where it stood, ends the incident under way and costs
    /// nothing; the move is one `relationship` event naming the choice.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn accepting_a_confession_moves_the_status(pool: PgPool) {
        let mock = companion_mock(Some("好呀。")).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        for (from, to) in [
            ("none", "dating"),
            ("ex", "dating"),
            ("partner", "fwb"),
            ("peer", "situationship"),
        ] {
            let s = scene(&pool).await;
            s.set_status(from, None).await;
            let choice = s.ask_kind("confess", recent(30)).await;
            let now = recent(0);
            s.put(
                "reading",
                now - Duration::minutes(20),
                now + Duration::minutes(100),
            )
            .await;

            let out = s
                .respond(&state, choice, LifeChoiceAnswer::Accept, now)
                .await
                .unwrap();

            assert_eq!(out.status, "accepted", "{from}");
            assert!(out.event.is_none(), "{from}: nothing placed");
            assert_eq!(
                s.row(choice).await,
                ("accepted".into(), Some(now), None),
                "{from}"
            );
            assert_eq!(s.relationship().await, (to.to_string(), None), "{from}");
            assert_eq!(
                s.relationship_events().await,
                vec![json!({"from": from, "to": to, "choice_id": choice})],
                "{from}"
            );
            assert!(s.drops().await.is_empty(), "{from}: nothing drops");
            assert_eq!(s.tiers().await, (3, 4), "{from}");
            assert_eq!(s.ends_at("reading").await, now, "{from}: the incident ends");
            assert_eq!(out.affinity.relationship_status, to, "{from}");
            assert!(!out.affinity.cold_war, "{from}");
        }
        let c = calls(&mock).await;
        assert_eq!(c.len(), 4);
        for call in &c {
            assert_eq!(last_user_turn(call), json!("（对方答应了你的告白。）"));
        }
    }

    /// A declined confession keeps the status and costs chemistry one tier,
    /// as a declined night together does.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn declining_a_confession_keeps_the_status_and_drops_chemistry(pool: PgPool) {
        let mock = companion_mock(Some(HURT)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        s.set_status("peer", None).await;
        let choice = s.ask_kind("confess", recent(30)).await;
        let (trust, intrigue, _, _) = s.axes().await;
        let now = recent(0);

        let out = s
            .respond(&state, choice, LifeChoiceAnswer::Decline, now)
            .await
            .unwrap();

        assert_eq!(out.status, "declined");
        assert!(out.event.is_none());
        assert_eq!(s.row(choice).await, ("declined".into(), Some(now), None));
        let (t, i, _, _) = s.axes().await;
        assert_eq!((t, i), (trust, intrigue), "the bond axes did not move");
        assert_eq!(s.tiers().await, (3, 3), "chemistry one tier lower");
        assert_eq!(
            s.drops().await,
            vec![json!({"life_choice": "decline", "choice_id": choice})]
        );
        assert_eq!(s.relationship().await, ("peer".into(), None));
        assert!(s.relationship_events().await.is_empty());
        assert_eq!(out.affinity.relationship_status, "peer");
        assert_eq!(
            last_user_turn(&calls(&mock).await[0]),
            json!("（对方拒绝了你的告白，你心里很难受。）")
        );
    }

    /// An accepted partnership makes them partners, ends the incident under
    /// way and costs nothing.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn accepting_a_partnership_makes_them_partners(pool: PgPool) {
        let mock = companion_mock(Some("好呀。")).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask_kind("partner_up", recent(30)).await;
        let axes = s.axes().await;
        let now = recent(0);
        s.put(
            "reading",
            now - Duration::minutes(20),
            now + Duration::minutes(100),
        )
        .await;

        let out = s
            .respond(&state, choice, LifeChoiceAnswer::Accept, now)
            .await
            .unwrap();

        assert_eq!(out.status, "accepted");
        assert!(out.event.is_none());
        assert_eq!(s.relationship().await, ("partner".into(), None));
        assert_eq!(
            s.relationship_events().await,
            vec![json!({"from": "none", "to": "partner", "choice_id": choice})]
        );
        assert_eq!(s.axes().await, axes);
        assert!(s.drops().await.is_empty());
        assert_eq!(s.ends_at("reading").await, now, "the incident ends");
        assert_eq!(out.affinity.relationship_status, "partner");
        assert_eq!(
            last_user_turn(&calls(&mock).await[0]),
            json!("（对方答应了和你结成伙伴。）")
        );
    }

    /// A declined partnership makes them peers and drops nothing.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn declining_a_partnership_makes_them_peers_without_a_drop(pool: PgPool) {
        let mock = companion_mock(Some(HURT)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask_kind("partner_up", recent(30)).await;
        let axes = s.axes().await;
        let now = recent(0);

        let out = s
            .respond(&state, choice, LifeChoiceAnswer::Decline, now)
            .await
            .unwrap();

        assert_eq!(out.status, "declined");
        assert!(out.event.is_none());
        assert_eq!(s.row(choice).await, ("declined".into(), Some(now), None));
        assert_eq!(s.axes().await, axes);
        assert_eq!(s.tiers().await, (3, 4), "both lines kept");
        assert!(s.drops().await.is_empty());
        assert_eq!(s.relationship().await, ("peer".into(), None));
        assert_eq!(
            s.relationship_events().await,
            vec![json!({"from": "none", "to": "peer", "choice_id": choice})]
        );
        assert_eq!(out.affinity.relationship_status, "peer");
        assert_eq!(
            last_user_turn(&calls(&mock).await[0]),
            json!("（对方没有答应和你结成伙伴，你有点失落。）")
        );
    }

    /// An accepted break-up makes an ex and ends the cold war; it costs
    /// nothing and leaves what she was doing alone.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn accepting_a_break_up_makes_an_ex_and_ends_the_cold_war(pool: PgPool) {
        let mock = companion_mock(Some("好。")).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        s.set_status("dating", Some(recent(60))).await;
        let choice = s.ask_kind("break_up", recent(30)).await;
        let now = recent(0);
        let reading_ends = now + Duration::minutes(100);
        s.put("reading", now - Duration::minutes(20), reading_ends)
            .await;

        let out = s
            .respond(&state, choice, LifeChoiceAnswer::Accept, now)
            .await
            .unwrap();

        assert_eq!(out.status, "accepted");
        assert!(out.event.is_none());
        assert_eq!(s.row(choice).await, ("accepted".into(), Some(now), None));
        assert_eq!(s.relationship().await, ("ex".into(), None));
        assert_eq!(
            s.relationship_events().await,
            vec![json!({"from": "dating", "to": "ex", "cold_war": "ended", "choice_id": choice})]
        );
        assert_eq!(s.tiers().await, (3, 4));
        assert!(s.drops().await.is_empty());
        assert_eq!(s.ends_at("reading").await, reading_ends);
        assert_eq!(out.affinity.relationship_status, "ex");
        assert!(!out.affinity.cold_war);
        assert_eq!(
            last_user_turn(&calls(&mock).await[0]),
            json!("（对方同意了分手。）")
        );
    }

    /// A refused break-up keeps the status and the cold war: one starts now
    /// when none was on, one under way keeps its start. Nothing drops.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn declining_a_break_up_starts_a_cold_war(pool: PgPool) {
        let mock = companion_mock(Some(HURT)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let earlier = recent(60);
        for under_way in [None, Some(earlier)] {
            let s = scene(&pool).await;
            s.set_status("dating", under_way).await;
            let choice = s.ask_kind("break_up", recent(30)).await;
            let now = recent(0);

            let out = s
                .respond(&state, choice, LifeChoiceAnswer::Decline, now)
                .await
                .unwrap();

            assert_eq!(out.status, "declined", "{under_way:?}");
            assert!(out.event.is_none());
            assert_eq!(s.row(choice).await, ("declined".into(), Some(now), None));
            let since = under_way.unwrap_or(now);
            assert_eq!(s.relationship().await, ("dating".into(), Some(since)));
            let events = match under_way {
                None => vec![json!({
                    "from": "dating", "to": "dating", "cold_war": "entered", "choice_id": choice
                })],
                Some(_) => vec![],
            };
            assert_eq!(s.relationship_events().await, events, "{under_way:?}");
            assert_eq!(s.tiers().await, (3, 4), "{under_way:?}");
            assert!(s.drops().await.is_empty(), "{under_way:?}");
            assert_eq!(out.affinity.relationship_status, "dating");
            assert!(out.affinity.cold_war, "{under_way:?}");
        }
        for call in &calls(&mock).await {
            assert_eq!(
                last_user_turn(call),
                json!("（对方不同意分手，你们还在冷战。）")
            );
        }
    }

    /// An accepted proposal makes a couple engaged and costs nothing.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_accepted_proposal_makes_them_engaged(pool: PgPool) {
        let mock = companion_mock(Some("好呀。")).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        for from in ["dating", "fwb", "situationship"] {
            let s = scene(&pool).await;
            s.set_status(from, None).await;
            let choice = s.ask_kind("propose", recent(30)).await;
            let now = recent(0);

            let out = s
                .respond(&state, choice, LifeChoiceAnswer::Accept, now)
                .await
                .unwrap();

            assert_eq!(out.status, "accepted", "{from}");
            assert_eq!(s.relationship().await, ("engaged".into(), None), "{from}");
            assert_eq!(
                s.relationship_events().await,
                vec![json!({"from": from, "to": "engaged", "choice_id": choice})],
                "{from}"
            );
            assert_eq!(s.tiers().await, (3, 4), "{from}");
            assert!(s.drops().await.is_empty(), "{from}");
            assert_eq!(out.affinity.relationship_status, "engaged", "{from}");
        }
    }

    /// Spec 2026-11-23-affinity-50 §7.2: an answer that arrives once the
    /// status is no longer one the kind is asked from is closed, and the
    /// whole answer is undone, the cap on the incident under way included.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_answer_after_the_status_moved_is_409(pool: PgPool) {
        let mock = companion_mock(Some("好呀。")).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        s.set_status("dating", None).await;
        let choice = s.ask_kind("propose", recent(30)).await;
        s.set_status("ex", None).await;
        let now = recent(0);
        let reading_ends = now + Duration::minutes(100);
        s.put("reading", now - Duration::minutes(20), reading_ends)
            .await;

        for response in [LifeChoiceAnswer::Accept, LifeChoiceAnswer::Decline] {
            let AppError::StreamPre(e) = s
                .respond(&state, choice, response, now)
                .await
                .expect_err("the status moved")
            else {
                panic!("expected a coded error")
            };
            assert_eq!(
                (e.status, e.code),
                (StatusCode::CONFLICT, "life_choice_closed")
            );
        }
        assert_eq!(s.row(choice).await, ("pending".into(), None, None));
        assert_eq!(s.relationship().await, ("ex".into(), None));
        assert_eq!(s.affinity_event_count().await, 0, "no affinity event");
        assert_eq!(s.tiers().await, (3, 4));
        let events = s.events().await;
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].5, reading_ends, "the cap went down with it");
        assert!(s.life_choice_rows().await.is_empty());
        assert!(calls(&mock).await.is_empty());
    }

    /// Spec 2026-11-23-affinity-50 §7.3: an accepted ask to fall asleep
    /// together places `asleep_together`, as the user's own ask does; a
    /// decline costs chemistry one tier. Neither moves the status.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn accepting_fall_asleep_together_places_asleep_together(pool: PgPool) {
        let mock = companion_mock(Some("好呀。")).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());

        let s = scene(&pool).await;
        let choice = s.ask_kind("fall_asleep_together", recent(30)).await;
        let axes = s.axes().await;
        let now = recent(0);
        let out = s
            .respond(&state, choice, LifeChoiceAnswer::Accept, now)
            .await
            .unwrap();

        assert_eq!(out.status, "accepted");
        let event = serde_json::to_value(out.event.expect("the event")).unwrap();
        assert_eq!(event["kind"], json!("asleep_together"));
        let events = s.events().await;
        assert_eq!(events.len(), 1, "{events:?}");
        let (id, kind, key, _, started_at, ends_at, _) = events[0].clone();
        assert_eq!(kind, "asleep_together");
        assert_eq!(key, format!("life_choice:{choice}:asleep_together"));
        assert_eq!(started_at, now);
        let hours = (ends_at - started_at).num_hours();
        assert!((1..=4).contains(&hours), "{hours} hours");
        assert_eq!(
            s.row(choice).await,
            ("accepted".into(), Some(now), Some(id))
        );
        assert_eq!(s.axes().await, axes);
        assert!(s.drops().await.is_empty());
        assert_eq!(s.relationship().await, ("none".into(), None));
        assert!(s.relationship_events().await.is_empty());
        assert_eq!(
            last_user_turn(&calls(&mock).await[0]),
            json!("（对方答应了，你们现在一起躺下睡觉。）")
        );

        let s = scene(&pool).await;
        let choice = s.ask_kind("fall_asleep_together", recent(30)).await;
        let (trust, intrigue, _, _) = s.axes().await;
        let now = recent(0);
        let out = s
            .respond(&state, choice, LifeChoiceAnswer::Decline, now)
            .await
            .unwrap();

        assert_eq!(out.status, "declined");
        assert!(out.event.is_none());
        assert!(s.events().await.is_empty());
        let (t, i, _, _) = s.axes().await;
        assert_eq!((t, i), (trust, intrigue), "the bond axes did not move");
        assert_eq!(s.tiers().await, (3, 3), "chemistry one tier lower");
        assert_eq!(
            s.drops().await,
            vec![json!({"life_choice": "decline", "choice_id": choice})]
        );
        assert_eq!(
            last_user_turn(calls(&mock).await.last().unwrap()),
            json!("（对方拒绝了你的邀请，你有点失落。）")
        );
    }

    /// Spec 2026-11-23-affinity-50 §7.5: `ask_for_space` is retired, and a
    /// stored one still pending is not answered: 409, nothing written.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_ask_for_space_answer_is_409(pool: PgPool) {
        let mock = companion_mock(Some(HURT)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask(3, recent(30)).await;

        for response in [LifeChoiceAnswer::Accept, LifeChoiceAnswer::Decline] {
            let AppError::StreamPre(e) = s
                .respond(&state, choice, response, recent(0))
                .await
                .expect_err("retired")
            else {
                panic!("expected a coded error")
            };
            assert_eq!(
                (e.status, e.code),
                (StatusCode::CONFLICT, "life_choice_closed")
            );
        }
        assert_eq!(s.row(choice).await, ("pending".into(), None, None));
        assert_eq!(s.tiers().await, (3, 4));
        assert_eq!(s.affinity_event_count().await, 0);
        assert!(s.events().await.is_empty());
        assert!(s.life_choice_rows().await.is_empty());
        assert!(calls(&mock).await.is_empty());
    }

    /// A stored kind this build does not know is refused as a server fault
    /// before anything is written: the choice stays pending, no cost lands.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_unknown_kind_is_a_500_and_writes_nothing(pool: PgPool) {
        let mock = companion_mock(Some(HURT)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask_kind("mystery", recent(30)).await;

        for response in [LifeChoiceAnswer::Accept, LifeChoiceAnswer::Decline] {
            let err = s
                .respond(&state, choice, response, recent(0))
                .await
                .expect_err("an unreadable kind");
            assert!(matches!(err, AppError::Internal(_)), "{err:?}");
        }
        assert_eq!(s.row(choice).await, ("pending".into(), None, None));
        assert_eq!(s.tiers().await, (3, 4));
        assert!(s.events().await.is_empty());
        assert!(s.drops().await.is_empty());
        assert!(calls(&mock).await.is_empty());
    }

    /// The first writer wins: a choice an ordinary message already converted
    /// answers 409, and nothing of the second answer lands.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn resolve_loses_to_a_faster_message(pool: PgPool) {
        let mock = companion_mock(Some(GOODBYE)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask_kind("travel_together", recent(30)).await;
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
        let choice = s.ask_kind("travel_together", recent(30)).await;
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
            let choice = s.ask_kind("travel_together", asked).await;
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

    /// The stream leads with `resolved`, the answer and what it placed as
    /// committed before she speaks, then her reply.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn life_choice_stream_leads_with_resolved(pool: PgPool) {
        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("you are a companion"))
            .respond_with(upstream_sse(&["好，那我们", "先各自安静一下。"]))
            .mount(&mock)
            .await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask_kind("travel_together", recent(30)).await;
        let now = recent(0);

        let resp = respond_stream_at(
            &state,
            s.session_id,
            choice,
            s.user_id,
            answer(LifeChoiceAnswer::Accept),
            now,
        )
        .await
        .unwrap();
        let frames = read_sse(resp).await;
        assert_eq!(
            frame_types(&frames),
            ["resolved", "meta", "delta", "delta", "done", "final"]
        );
        let resolved = &frames[0];
        assert_eq!(resolved["status"], "accepted");
        assert_eq!(resolved["event"]["kind"], "traveling_together");
        assert_eq!(frames[4]["occasion"], "life_choice");
        assert_eq!(s.row(choice).await.0, "accepted");
        let rows = s.life_choice_rows().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, GOODBYE);
    }

    /// A decline starts no event: `resolved` has no `event` key.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn life_choice_stream_decline_has_no_event(pool: PgPool) {
        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(upstream_sse(&[HURT]))
            .mount(&mock)
            .await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask_kind("travel_together", recent(30)).await;

        let resp = respond_stream_at(
            &state,
            s.session_id,
            choice,
            s.user_id,
            answer(LifeChoiceAnswer::Decline),
            recent(0),
        )
        .await
        .unwrap();
        let frames = read_sse(resp).await;
        assert_eq!(
            frame_types(&frames),
            ["resolved", "meta", "delta", "done", "final"]
        );
        assert_eq!(frames[0]["status"], "declined");
        assert!(frames[0].get("event").is_none());
    }

    /// Her reply failing cannot undo the answer, and the request cannot be
    /// made again: the error is never retryable.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn life_choice_stream_reply_failure_keeps_answer(pool: PgPool) {
        let failing = companion_mock(None).await;
        let state = with_companion(test_state(pool.clone()), &failing.uri());
        let s = scene(&pool).await;
        let choice = s.ask_kind("travel_together", recent(30)).await;

        let resp = respond_stream_at(
            &state,
            s.session_id,
            choice,
            s.user_id,
            answer(LifeChoiceAnswer::Accept),
            recent(0),
        )
        .await
        .unwrap();
        let frames = read_sse(resp).await;
        assert_eq!(frame_types(&frames), ["resolved", "error"]);
        assert_eq!(frames[1]["retryable"], false);
        assert_eq!(s.row(choice).await.0, "accepted");
        assert!(s.life_choice_rows().await.is_empty(), "no message row");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn life_choice_stream_closed_is_409(pool: PgPool) {
        let mock = companion_mock(Some(GOODBYE)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask_kind("travel_together", recent(30)).await;
        s.respond(&state, choice, LifeChoiceAnswer::Accept, recent(0))
            .await
            .unwrap();

        let err = respond_stream_at(
            &state,
            s.session_id,
            choice,
            s.user_id,
            answer(LifeChoiceAnswer::Accept),
            recent(0),
        )
        .await
        .expect_err("a plain status, not a stream");
        assert!(
            matches!(&err, AppError::StreamPre(e) if e.status == StatusCode::CONFLICT),
            "{err:?}"
        );
    }

    /// The answer and what it places are one transaction: a trip that cannot
    /// be written (its key is taken, here by an incident under way) undoes the
    /// resolution and the incident's cap, and the user is told so rather than
    /// answered for nothing.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_failed_trip_undoes_the_whole_answer(pool: PgPool) {
        let mock = companion_mock(Some(GOODBYE)).await;
        let state = with_companion(test_state(pool.clone()), &mock.uri());
        let s = scene(&pool).await;
        let choice = s.ask_kind("travel_together", recent(30)).await;
        let now = recent(0);
        let (started, ends) = (now - Duration::minutes(20), now + Duration::minutes(40));
        sqlx::query(
            "INSERT INTO engine.persona_events \
               (instance_id, session_id, event_key, kind, started_at, ends_at) \
             VALUES ($1, $2, $3, 'reading', $4, $5)",
        )
        .bind(s.instance_id)
        .bind(s.session_id)
        .bind(format!("life_choice:{choice}:traveling_together"))
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
        let theirs = other.ask_kind("travel_together", recent(30)).await;
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
        let choice = s.ask_kind("travel_together", recent(30)).await;
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
            s.set_status("dating", None).await;
            let choice = s.ask_kind("propose", recent(30)).await;
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
            let choice = s.ask_kind("travel_together", recent(30)).await;
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
