// SPDX-License-Identifier: AGPL-3.0-only
// TODO(T12): handlers + DTOs become live once main.rs constructs an
// AppState and calls `routes::router(state)`. Until then they're dead
// from the binary's POV; the integration tests in this file exercise
// them directly.
#![allow(dead_code)]

//! Companion HTTP routes (`/comp/*`).
//!
//! Ported from `eros-gateway/src/routes/companion.rs` with these
//! OSS-specific changes:
//!
//! - `user_id` is exclusively sourced from the JWT via the `AuthUser`
//!   request extension. Request DTOs no longer carry `user_id`.
//! - Path-supplied `user_id` (on `/sessions` + `/profile`) MUST equal the
//!   JWT's user_id; mismatch returns 403 Forbidden.
//! - Routes that operate on a `session_id` verify that the session belongs
//!   to the JWT user; otherwise 403 Forbidden.
//! - All DB I/O routes through the `eros-engine-store` repos
//!   (`ChatRepo` / `AffinityRepo` / `PersonaRepo` / `InsightRepo`).
//! - The credit ledger is gone in OSS — the gateway's two legacy
//!   credit-spending endpoints collapse into a single `event/gift`
//!   endpoint that takes
//!   `{ deltas, label, metadata }` from the body and applies them
//!   directly via `AffinityRepo::persist_with_event`. We deliberately
//!   bypass `pipeline::run` for the gift route so the HTTP+DB side-effects
//!   can be tested without a live LLM (the full gift→reply flow remains
//!   reachable through the regular `send_message` path with whatever
//!   client-driven semantics the host app prefers).
//! - `lead_score` / CTA-gating fields are computed from the same store
//!   primitives used by post-process; no inline `companion_insights`
//!   table read.

use axum::{
    extract::{Extension, Path, Query, State},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_core::affinity::AffinityDeltas;
use eros_engine_core::types::Event;
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::{ChatMessage as StoreChatMessage, ChatRepo, ChatSession};
use eros_engine_store::insight::{compute_training_level, InsightRepo};
use eros_engine_store::persona::PersonaRepo;

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::pipeline;
use crate::state::AppState;

// ─── DTOs ───────────────────────────────────────────────────────────

/// Genome row exposed on `GET /comp/personas`. Matches
/// `eros_engine_core::persona::PersonaGenome` field-for-field but with
/// `ToSchema` so utoipa can render it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PersonaGenomeDto {
    pub id: Uuid,
    pub name: String,
    pub system_prompt: String,
    pub tip_personality: Option<String>,
    pub avatar_url: Option<String>,
    #[schema(value_type = Object)]
    pub art_metadata: serde_json::Value,
    pub is_active: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ListPersonasResponse {
    pub personas: Vec<PersonaGenomeDto>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct StartChatRequest {
    /// Optional explicit instance id. If absent, the server picks (or
    /// auto-creates) the user's instance for the supplied genome.
    pub instance_id: Option<Uuid>,
    /// Optional genome id. Required when `instance_id` is absent.
    pub genome_id: Option<Uuid>,
    /// Mark the new session as a demo. Persisted to `metadata.is_demo` and
    /// read by the affinity pipeline to apply `demo_ema_inertia` instead
    /// of the global value, so meters move visibly within the turn budget.
    /// Ignored when resuming an existing session.
    #[serde(default)]
    pub is_demo: Option<bool>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct StartChatResponse {
    pub session_id: Uuid,
    pub instance_id: Uuid,
    pub persona_name: String,
    pub is_new: bool,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SendMessageRequest {
    pub message: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CompanionReplyResponse {
    pub reply: String,
    pub session_id: Uuid,
    pub lead_score: f64,
    pub should_show_cta: bool,
    pub typing_delay_ms: u64,
    pub agent_training_level: f64,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AsyncSendResponse {
    pub status: String,
    pub user_message_id: Uuid,
    pub session_id: Uuid,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CompanionReplyPayload {
    pub reply: String,
    pub message_id: Uuid,
    pub lead_score: f64,
    pub should_show_cta: bool,
    pub agent_training_level: f64,
    pub sent_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PendingCheckResponse {
    pub status: String,
    pub message: Option<CompanionReplyPayload>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct HistoryQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ChatHistoryEntry {
    pub role: String,
    pub content: String,
    pub sent_at: DateTime<Utc>,
    #[schema(value_type = Object)]
    pub extracted_facts: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct HistoryResponse {
    pub session_id: Uuid,
    pub messages: Vec<ChatHistoryEntry>,
    pub total: usize,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SessionListEntry {
    pub session_id: Uuid,
    pub instance_id: Option<Uuid>,
    pub lead_score: f64,
    pub is_converted: bool,
    pub last_active_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ListSessionsResponse {
    pub user_id: Uuid,
    pub sessions: Vec<SessionListEntry>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ProfileResponse {
    pub user_id: Uuid,
    #[schema(value_type = Object)]
    pub companion_insights: Option<serde_json::Value>,
    pub agent_training_level: f64,
}

/// Body for `POST /comp/chat/{session_id}/event/gift`.
///
/// Replaces the gateway's `tip` + `gift` endpoints. The OSS engine has
/// no credit ledger, so the caller supplies the affinity deltas
/// directly. `label` is the human-readable description (e.g. `"rose"`)
/// and is also used as the `chat_messages.content` for the gift turn.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct GiftEventBody {
    pub deltas: AffinityDeltasDto,
    pub label: Option<String>,
    #[schema(value_type = Object)]
    pub metadata: Option<serde_json::Value>,
}

/// Mirror of `eros_engine_core::affinity::AffinityDeltas` with `ToSchema`
/// for OpenAPI emission. Field-for-field conversion both ways.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AffinityDeltasDto {
    #[serde(default)]
    pub warmth: f64,
    #[serde(default)]
    pub trust: f64,
    #[serde(default)]
    pub intrigue: f64,
    #[serde(default)]
    pub intimacy: f64,
    #[serde(default)]
    pub patience: f64,
    #[serde(default)]
    pub tension: f64,
}

impl From<&AffinityDeltas> for AffinityDeltasDto {
    fn from(d: &AffinityDeltas) -> Self {
        Self {
            warmth: d.warmth,
            trust: d.trust,
            intrigue: d.intrigue,
            intimacy: d.intimacy,
            patience: d.patience,
            tension: d.tension,
        }
    }
}

impl From<&AffinityDeltasDto> for AffinityDeltas {
    fn from(d: &AffinityDeltasDto) -> Self {
        Self {
            warmth: d.warmth,
            trust: d.trust,
            intrigue: d.intrigue,
            intimacy: d.intimacy,
            patience: d.patience,
            tension: d.tension,
        }
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GiftEventResponse {
    pub reply: Option<String>,
    pub applied_deltas: AffinityDeltasDto,
    pub relationship_label: Option<String>,
}

// ─── Helpers ────────────────────────────────────────────────────────

/// Verify a session exists and is owned by `user_id`. Returns the session
/// row on success, `404` if missing, `403` if owned by someone else.
async fn require_session_for_user(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
) -> Result<ChatSession, AppError> {
    let repo = ChatRepo { pool: &state.pool };
    let session = repo
        .get_session(session_id)
        .await?
        .ok_or_else(|| AppError::NotFound("session not found".into()))?;
    if session.user_id != user_id {
        return Err(AppError::Forbidden("not your session".into()));
    }
    Ok(session)
}

/// Compute the on-the-fly `agent_training_level` for `user_id` from
/// the engine.companion_insights row (returns 0.0 when no row exists).
async fn read_training_level(state: &AppState, user_id: Uuid) -> f64 {
    let repo = InsightRepo { pool: &state.pool };
    repo.load(user_id)
        .await
        .ok()
        .flatten()
        .map(|row| compute_training_level(&row.insights))
        .unwrap_or(0.0)
}

fn label_to_string(label: Option<eros_engine_core::affinity::RelationshipLabel>) -> Option<String> {
    use eros_engine_core::affinity::RelationshipLabel as L;
    label.map(|l| {
        match l {
            L::Stranger => "stranger",
            L::Romantic => "romantic",
            L::Friend => "friend",
            L::Frenemy => "frenemy",
            L::SlowBurn => "slow_burn",
        }
        .to_string()
    })
}

/// Pseudo-random typing delay in `[800, 2500)` ms. Avoids pulling in
/// the `rand` crate just for this UI nicety — uses the message UUID's
/// low bits as the entropy source.
fn typing_delay_ms_from(seed: Uuid) -> u64 {
    let bytes = seed.as_bytes();
    let n = u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    800 + (n % 1700)
}

// ─── Handlers ───────────────────────────────────────────────────────

/// List active platform-owned persona genomes available for new sessions.
#[utoipa::path(
    get,
    path = "/comp/personas",
    tag = "companion",
    responses(
        (status = 200, body = ListPersonasResponse),
        (status = 401, description = "missing or invalid bearer")
    ),
    security(("bearer" = []))
)]
async fn list_personas(
    State(state): State<AppState>,
    Extension(AuthUser(_user_id)): Extension<AuthUser>,
) -> Result<Json<ListPersonasResponse>, AppError> {
    let repo = PersonaRepo { pool: &state.pool };
    let genomes = repo.list_active().await?;
    let personas = genomes
        .into_iter()
        .map(|g| PersonaGenomeDto {
            id: g.id,
            name: g.name,
            system_prompt: g.system_prompt,
            tip_personality: g.tip_personality,
            avatar_url: g.avatar_url,
            art_metadata: g.art_metadata,
            is_active: g.is_active,
        })
        .collect();
    Ok(Json(ListPersonasResponse { personas }))
}

/// Start (or resume) a chat session for the JWT user.
///
/// Resolution rules:
///   * `instance_id` provided → must belong to the JWT user.
///   * else `genome_id` provided → look up (or auto-create) the user's
///     active instance of that genome.
///   * else: the only active genome the user already has an instance of
///     wins; otherwise 400.
#[utoipa::path(
    post,
    path = "/comp/chat/start",
    tag = "companion",
    request_body = StartChatRequest,
    responses(
        (status = 200, body = StartChatResponse),
        (status = 400, description = "missing genome_id and no existing instance"),
        (status = 401, description = "missing or invalid bearer"),
        (status = 404, description = "instance/genome not found")
    ),
    security(("bearer" = []))
)]
async fn start_chat(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<StartChatRequest>,
) -> Result<Json<StartChatResponse>, AppError> {
    let persona_repo = PersonaRepo { pool: &state.pool };
    let chat_repo = ChatRepo { pool: &state.pool };

    let instance_id = match req.instance_id {
        Some(iid) => {
            // Verify ownership + active status.
            let companion = persona_repo
                .load_companion(iid)
                .await?
                .ok_or_else(|| AppError::NotFound("instance not found".into()))?;
            if companion.instance.owner_uid != user_id {
                return Err(AppError::Forbidden(
                    "instance not owned by this user".into(),
                ));
            }
            iid
        }
        None => {
            let genome_id = req
                .genome_id
                .ok_or_else(|| AppError::BadRequest("missing genome_id (or instance_id)".into()))?;

            // Validate genome exists + is active.
            let genome = persona_repo
                .get_genome(genome_id)
                .await?
                .ok_or_else(|| AppError::NotFound("genome not found".into()))?;
            if !genome.is_active {
                return Err(AppError::BadRequest("genome is not active".into()));
            }

            // Look for an existing active instance for this user×genome.
            let existing: Option<(Uuid,)> = sqlx::query_as(
                "SELECT id FROM engine.persona_instances \
                 WHERE genome_id = $1 AND owner_uid = $2 AND status = 'active'",
            )
            .bind(genome_id)
            .bind(user_id)
            .fetch_optional(&state.pool)
            .await?;

            match existing {
                Some((iid,)) => iid,
                None => persona_repo.create_instance(genome_id, user_id).await?,
            }
        }
    };

    // Load persona for the response payload.
    let companion = persona_repo
        .load_companion(instance_id)
        .await?
        .ok_or_else(|| AppError::NotFound("persona not loadable".into()))?;

    // Resume the most recent session for (user, instance) or create one.
    let existing: Option<ChatSession> = sqlx::query_as::<_, ChatSession>(
        "SELECT * FROM engine.chat_sessions \
         WHERE user_id = $1 AND instance_id = $2 \
         ORDER BY last_active_at DESC LIMIT 1",
    )
    .bind(user_id)
    .bind(instance_id)
    .fetch_optional(&state.pool)
    .await?;

    let (session_id, is_new) = match existing {
        Some(s) => {
            sqlx::query("UPDATE engine.chat_sessions SET last_active_at = now() WHERE id = $1")
                .bind(s.id)
                .execute(&state.pool)
                .await?;
            (s.id, false)
        }
        None => {
            let metadata = if req.is_demo.unwrap_or(false) {
                serde_json::json!({ "is_demo": true })
            } else {
                serde_json::json!({})
            };
            let s = chat_repo
                .create_session_with_metadata(user_id, instance_id, metadata)
                .await?;
            (s.id, true)
        }
    };

    Ok(Json(StartChatResponse {
        session_id,
        instance_id,
        persona_name: companion.genome.name,
        is_new,
    }))
}

/// Send a user message and synchronously return the AI reply.
#[utoipa::path(
    post,
    path = "/comp/chat/{session_id}/message",
    tag = "companion",
    params(("session_id" = Uuid, Path, description = "Chat session id")),
    request_body = SendMessageRequest,
    responses(
        (status = 200, body = CompanionReplyResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "not your session"),
        (status = 404, description = "session not found")
    ),
    security(("bearer" = []))
)]
async fn send_message(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<SendMessageRequest>,
) -> Result<Json<CompanionReplyResponse>, AppError> {
    require_session_for_user(&state, session_id, user_id).await?;

    if req.message.trim().is_empty() {
        return Err(AppError::BadRequest("message must not be empty".into()));
    }

    // Persist the user message up-front so the engine sees it in history.
    let chat_repo = ChatRepo { pool: &state.pool };
    let user_message_id = chat_repo
        .append_message(session_id, "user", &req.message)
        .await?;

    let event = Event::UserMessage {
        content: req.message.clone(),
        message_id: user_message_id,
    };
    let response = pipeline::run(&state, session_id, event).await?;

    let reply_text = response
        .as_ref()
        .map(|r| r.reply.clone())
        .unwrap_or_default();

    let lead_score: f64 =
        sqlx::query_scalar("SELECT lead_score FROM engine.chat_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten()
            .unwrap_or(0.0);

    let training_level = read_training_level(&state, user_id).await;
    let should_show_cta = lead_score >= 7.0 && training_level >= 0.4;

    Ok(Json(CompanionReplyResponse {
        reply: reply_text,
        session_id,
        lead_score,
        should_show_cta,
        typing_delay_ms: typing_delay_ms_from(user_message_id),
        agent_training_level: training_level,
    }))
}

/// Accept a user message and process it in the background. The client
/// then polls `/pending/{message_id}` for the assistant reply.
#[utoipa::path(
    post,
    path = "/comp/chat/{session_id}/message_async",
    tag = "companion",
    params(("session_id" = Uuid, Path, description = "Chat session id")),
    request_body = SendMessageRequest,
    responses(
        (status = 202, body = AsyncSendResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "not your session"),
        (status = 404, description = "session not found")
    ),
    security(("bearer" = []))
)]
async fn send_message_async(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<SendMessageRequest>,
) -> Result<(axum::http::StatusCode, Json<AsyncSendResponse>), AppError> {
    require_session_for_user(&state, session_id, user_id).await?;

    if req.message.trim().is_empty() {
        return Err(AppError::BadRequest("message must not be empty".into()));
    }

    let chat_repo = ChatRepo { pool: &state.pool };
    let user_message_id = chat_repo
        .append_message(session_id, "user", &req.message)
        .await?;

    let state_bg = state.clone();
    let msg_copy = req.message.clone();
    tokio::spawn(async move {
        let event = Event::UserMessage {
            content: msg_copy,
            message_id: user_message_id,
        };
        if let Err(e) = pipeline::run(&state_bg, session_id, event).await {
            tracing::error!("engine pipeline failed for session {session_id}: {e}");
            let repo = ChatRepo {
                pool: &state_bg.pool,
            };
            let _ = repo
                .append_message(session_id, "system_error", &format!("AI reply failed: {e}"))
                .await;
        }
    });

    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(AsyncSendResponse {
            status: "processing".into(),
            user_message_id,
            session_id,
        }),
    ))
}

/// Poll for the AI reply to a user message. Returns `processing` until
/// the assistant or system_error row appears, then `completed` / `error`.
#[utoipa::path(
    get,
    path = "/comp/chat/{session_id}/pending/{message_id}",
    tag = "companion",
    params(
        ("session_id" = Uuid, Path, description = "Chat session id"),
        ("message_id" = Uuid, Path, description = "User message id to poll on")
    ),
    responses(
        (status = 200, body = PendingCheckResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "not your session"),
        (status = 404, description = "session not found")
    ),
    security(("bearer" = []))
)]
async fn check_pending(
    State(state): State<AppState>,
    Path((session_id, message_id)): Path<(Uuid, Uuid)>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
) -> Result<Json<PendingCheckResponse>, AppError> {
    require_session_for_user(&state, session_id, user_id).await?;

    let reply: Option<StoreChatMessage> = sqlx::query_as::<_, StoreChatMessage>(
        "SELECT * FROM engine.chat_messages \
         WHERE session_id = $1 \
           AND sent_at > (SELECT sent_at FROM engine.chat_messages WHERE id = $2) \
           AND role IN ('assistant', 'system_error') \
         ORDER BY sent_at ASC LIMIT 1",
    )
    .bind(session_id)
    .bind(message_id)
    .fetch_optional(&state.pool)
    .await?;

    match reply {
        Some(msg) if msg.role == "assistant" => {
            let lead_score: f64 =
                sqlx::query_scalar("SELECT lead_score FROM engine.chat_sessions WHERE id = $1")
                    .bind(session_id)
                    .fetch_optional(&state.pool)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(0.0);

            let training_level = read_training_level(&state, user_id).await;
            let should_show_cta = lead_score >= 7.0 && training_level >= 0.4;

            Ok(Json(PendingCheckResponse {
                status: "completed".into(),
                message: Some(CompanionReplyPayload {
                    reply: msg.content,
                    message_id: msg.id,
                    lead_score,
                    should_show_cta,
                    agent_training_level: training_level,
                    sent_at: msg.sent_at,
                }),
            }))
        }
        Some(_) => Ok(Json(PendingCheckResponse {
            status: "error".into(),
            message: None,
        })),
        None => Ok(Json(PendingCheckResponse {
            status: "processing".into(),
            message: None,
        })),
    }
}

/// Paginated chat history (oldest-first) for the given session.
#[utoipa::path(
    get,
    path = "/comp/chat/{session_id}/history",
    tag = "companion",
    params(
        ("session_id" = Uuid, Path, description = "Chat session id"),
        ("limit" = Option<i64>, Query, description = "Max rows (default 20, capped at 50)"),
        ("offset" = Option<i64>, Query, description = "Page offset, default 0")
    ),
    responses(
        (status = 200, body = HistoryResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "not your session"),
        (status = 404, description = "session not found")
    ),
    security(("bearer" = []))
)]
async fn get_history(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<HistoryResponse>, AppError> {
    require_session_for_user(&state, session_id, user_id).await?;

    let limit = query.limit.unwrap_or(20).clamp(1, 50);
    let offset = query.offset.unwrap_or(0).max(0);

    let chat_repo = ChatRepo { pool: &state.pool };
    let rows = chat_repo.history(session_id, limit, offset).await?;

    let entries: Vec<ChatHistoryEntry> = rows
        .into_iter()
        .map(|m| ChatHistoryEntry {
            role: m.role,
            content: m.content,
            sent_at: m.sent_at,
            extracted_facts: m.extracted_facts,
        })
        .collect();
    let total = entries.len();

    Ok(Json(HistoryResponse {
        session_id,
        messages: entries,
        total,
    }))
}

/// All sessions for the JWT user. The `{user_id}` path parameter MUST
/// match the JWT's user_id; mismatch returns 403.
#[utoipa::path(
    get,
    path = "/comp/chat/{user_id}/sessions",
    tag = "companion",
    params(("user_id" = Uuid, Path, description = "Owner user id (must equal JWT sub)")),
    responses(
        (status = 200, body = ListSessionsResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "user_id does not match JWT")
    ),
    security(("bearer" = []))
)]
async fn list_sessions(
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
    Extension(AuthUser(jwt_user)): Extension<AuthUser>,
) -> Result<Json<ListSessionsResponse>, AppError> {
    if user_id != jwt_user {
        return Err(AppError::Forbidden("not your data".into()));
    }
    let repo = ChatRepo { pool: &state.pool };
    let sessions = repo.list_sessions(user_id).await?;
    let entries = sessions
        .into_iter()
        .map(|s| SessionListEntry {
            session_id: s.id,
            instance_id: s.instance_id,
            lead_score: s.lead_score,
            is_converted: s.is_converted,
            last_active_at: s.last_active_at,
        })
        .collect();
    Ok(Json(ListSessionsResponse {
        user_id,
        sessions: entries,
    }))
}

/// Companion insights + computed training level for the JWT user. The
/// path `user_id` MUST match the JWT's user_id; mismatch returns 403.
#[utoipa::path(
    get,
    path = "/comp/user/{user_id}/profile",
    tag = "companion",
    params(("user_id" = Uuid, Path, description = "Owner user id (must equal JWT sub)")),
    responses(
        (status = 200, body = ProfileResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "user_id does not match JWT")
    ),
    security(("bearer" = []))
)]
async fn get_profile(
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
    Extension(AuthUser(jwt_user)): Extension<AuthUser>,
) -> Result<Json<ProfileResponse>, AppError> {
    if user_id != jwt_user {
        return Err(AppError::Forbidden("not your data".into()));
    }

    let repo = InsightRepo { pool: &state.pool };
    let row = repo.load(user_id).await?;
    let (insights, training_level) = match row {
        Some(r) => {
            let lvl = compute_training_level(&r.insights);
            (Some(r.insights), lvl)
        }
        None => (None, 0.0),
    };

    Ok(Json(ProfileResponse {
        user_id,
        companion_insights: insights,
        agent_training_level: training_level,
    }))
}

/// Send a gift event to the session.
///
/// Replaces the gateway's `tip` + `gift` endpoints. The OSS engine has
/// no credit ledger / inventory, so the caller supplies the deltas
/// (validated by client UX) and the optional human label. The route
/// inserts a `gift_user` `chat_messages` row and applies the deltas via
/// `AffinityRepo::persist_with_event` (which logs a `gift` row in
/// `companion_affinity_events`). It deliberately does NOT call
/// `pipeline::run` — the LLM-driven gift reaction reply is delegated to
/// the next user message turn so this route's HTTP+DB side-effects can
/// be exercised in tests without a live LLM.
#[utoipa::path(
    post,
    path = "/comp/chat/{session_id}/event/gift",
    tag = "companion",
    params(("session_id" = Uuid, Path, description = "Chat session id")),
    request_body = GiftEventBody,
    responses(
        (status = 200, body = GiftEventResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "not your session"),
        (status = 404, description = "session not found")
    ),
    security(("bearer" = []))
)]
async fn event_gift(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(body): Json<GiftEventBody>,
) -> Result<Json<GiftEventResponse>, AppError> {
    let session = require_session_for_user(&state, session_id, user_id).await?;
    let instance_id = session
        .instance_id
        .ok_or_else(|| AppError::Internal("session has no instance_id".into()))?;

    // 1. Persist the gift turn as a chat message (role = 'gift_user').
    let chat_repo = ChatRepo { pool: &state.pool };
    let label_text = body.label.clone().unwrap_or_else(|| "gift".to_string());
    chat_repo
        .append_message(session_id, "gift_user", &label_text)
        .await?;

    // 2. Apply deltas + emit a `gift` row in companion_affinity_events.
    let affinity_repo = AffinityRepo { pool: &state.pool };
    let mut affinity = affinity_repo
        .load_or_create(session_id, user_id, instance_id)
        .await?;
    let core_deltas: AffinityDeltas = (&body.deltas).into();
    let context = body
        .metadata
        .clone()
        .unwrap_or_else(|| serde_json::json!({ "label": label_text }));
    affinity_repo
        .persist_with_event(
            &mut affinity,
            &core_deltas,
            state.config.ema_inertia,
            "gift",
            context,
        )
        .await?;

    Ok(Json(GiftEventResponse {
        // Reply text is intentionally None here — see route doc-comment.
        reply: None,
        applied_deltas: body.deltas,
        relationship_label: label_to_string(affinity.relationship_label),
    }))
}

// ─── Router ─────────────────────────────────────────────────────────

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_personas))
        .routes(routes!(start_chat))
        .routes(routes!(send_message))
        .routes(routes!(send_message_async))
        .routes(routes!(check_pending))
        .routes(routes!(get_history))
        .routes(routes!(list_sessions))
        .routes(routes!(get_profile))
        .routes(routes!(event_gift))
}

// ────────────────────────────────────────────────────────────────────
// Integration tests
//
// These exercise the route module's HTTP+DB side-effects against a
// live Postgres instance (via `#[sqlx::test]`). They do NOT exercise
// the LLM-driven path: the gift route deliberately bypasses
// `pipeline::run`, and the message routes are not tested here for the
// same reason (full end-to-end LLM testing is the job of T14's deploy
// smoke). Per the T11 spec: "directly insert a chat_messages row +
// call AffinityRepo::persist_with_event in the test, BYPASSING the
// pipeline" — this is exactly what the gift-event test does, and it
// matches the route's chosen implementation strategy.
// ────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    use axum::{
        body::{to_bytes, Body},
        http::{header, Request, StatusCode},
        Router,
    };
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::{json, Value};
    use sqlx::PgPool;
    use std::sync::Arc;
    use tower::Service;

    use crate::auth::supabase::SupabaseJwtValidator;
    use crate::auth::AuthValidator;

    const TEST_SECRET: &str = "test-secret-companion-routes";

    // ─── Test helpers ───────────────────────────────────────────────

    fn mint_test_jwt(uid: Uuid) -> String {
        let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp();
        encode(
            &Header::default(),
            &json!({ "sub": uid.to_string(), "exp": exp }),
            &EncodingKey::from_secret(TEST_SECRET.as_ref()),
        )
        .expect("test jwt encodes")
    }

    fn test_state(pool: PgPool) -> AppState {
        let auth: Arc<dyn AuthValidator> =
            Arc::new(SupabaseJwtValidator::new().with_legacy_secret(TEST_SECRET.into()));
        AppState {
            pool,
            auth,
            config: crate::state::ServerConfig {
                expose_affinity_debug: true,
                ema_inertia: 0.0, // no smoothing → deltas applied 1:1 in tests
                demo_ema_inertia: 0.0,
                bind_addr: "127.0.0.1:0".into(),
                // Sweeper disabled in tests — unit tests don't spawn it
                // and the field is just for AppState completeness.
                dreaming_tick: std::time::Duration::ZERO,
                dreaming_idle_threshold: std::time::Duration::from_secs(1800),
            },
            openrouter: Arc::new(eros_engine_llm::openrouter::OpenRouterClient::new(
                "stub".into(),
            )),
            voyage: Arc::new(eros_engine_llm::voyage::VoyageClient::new("stub".into())),
            model_config: Arc::new(eros_engine_llm::model_config::ModelConfig::default()),
        }
    }

    fn build_router(state: AppState) -> Router {
        let (axum_router, _api) = crate::routes::router(state.clone()).split_for_parts();
        axum_router.with_state(state)
    }

    async fn send_request(router: &mut Router, req: Request<Body>) -> (StatusCode, Value) {
        let resp = router.call(req).await.expect("router call infallible");
        let status = resp.status();
        let body_bytes = to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("read body");
        let json = if body_bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice::<Value>(&body_bytes).unwrap_or(Value::Null)
        };
        (status, json)
    }

    async fn seed_genome(pool: &PgPool, name: &str) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata, is_active) \
             VALUES ($1, 'you are a companion', '{}'::jsonb, true) RETURNING id",
        )
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn seed_session(pool: &PgPool, user_id: Uuid, instance_id: Uuid) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn seed_instance(pool: &PgPool, genome_id: Uuid, owner: Uuid) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id)
        .bind(owner)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    // ─── Test 1: public /healthz still works without bearer ─────────

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn healthz_unauthenticated_returns_200(pool: PgPool) {
        let state = test_state(pool);
        let mut app = build_router(state);

        let req = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let (status, body) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
    }

    // ─── Test 2: protected route rejects requests without bearer ────

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn comp_personas_401_without_bearer(pool: PgPool) {
        let state = test_state(pool);
        let mut app = build_router(state);

        let req = Request::builder()
            .uri("/comp/personas")
            .body(Body::empty())
            .unwrap();
        let (status, _body) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // ─── Test 3: GET /comp/personas returns active genomes ──────────

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn comp_personas_returns_active_genomes(pool: PgPool) {
        let _g = seed_genome(&pool, "Aria").await;
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(Uuid::new_v4());

        let req = Request::builder()
            .uri("/comp/personas")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let (status, body) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<&str> = body["personas"]
            .as_array()
            .expect("array")
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"Aria"), "expected Aria in {names:?}");
    }

    // ─── Test 4: start_chat creates a session for the JWT user ──────

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn start_chat_creates_session_for_jwt_user_id(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Echo").await;
        let state = test_state(pool.clone());
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let body = serde_json::to_vec(&json!({ "genome_id": genome_id })).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/comp/chat/start")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, resp) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK, "got body: {resp}");

        let session_id_str = resp["session_id"].as_str().expect("session_id present");
        let session_id = Uuid::parse_str(session_id_str).unwrap();
        assert_eq!(resp["persona_name"], "Echo");
        assert_eq!(resp["is_new"], true);

        // Verify the session row's user_id matches the JWT, NOT something
        // an attacker could put in the body.
        let row_user_id: Uuid =
            sqlx::query_scalar("SELECT user_id FROM engine.chat_sessions WHERE id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row_user_id, user_id);
    }

    // ─── Test 5: cross-user GET /chat/{user_id}/sessions → 403 ──────

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn get_sessions_403_when_path_user_id_differs_from_jwt(pool: PgPool) {
        let attacker = Uuid::new_v4();
        let victim = Uuid::new_v4();

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(attacker);

        let req = Request::builder()
            .uri(format!("/comp/chat/{victim}/sessions"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let (status, _body) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    // ─── Test 6: event/gift appends gift_user message + emits event ──

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn event_gift_appends_message_and_emits_event(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Nova").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;

        let state = test_state(pool.clone());
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let body = serde_json::to_vec(&json!({
            "deltas": {
                "warmth": 0.2,
                "trust": 0.1,
                "intrigue": 0.0,
                "intimacy": 0.05,
                "patience": 0.0,
                "tension": 0.0
            },
            "label": "rose",
            "metadata": { "source": "test" }
        }))
        .unwrap();

        let req = Request::builder()
            .method("POST")
            .uri(format!("/comp/chat/{session_id}/event/gift"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, resp) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK, "got body: {resp}");

        // chat_messages: a gift_user row was appended with content == label.
        let gift_msg_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'gift_user' AND content = 'rose'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(gift_msg_count, 1);

        // companion_affinity_events: a gift row was emitted.
        let gift_event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM engine.companion_affinity_events e \
             JOIN engine.companion_affinity a ON a.id = e.affinity_id \
             WHERE a.session_id = $1 AND e.event_type = 'gift'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(gift_event_count, 1);

        // Affinity row exists and warmth was bumped (ema_inertia = 0.0
        // in test config → 1:1 apply, plus default 0.3 baseline → 0.5).
        let warmth: f64 = sqlx::query_scalar(
            "SELECT warmth FROM engine.companion_affinity WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!((warmth - 0.5).abs() < 1e-9, "got warmth={warmth}");
    }

    // ─── Bonus: cross-user gift event is forbidden ──────────────────

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn event_gift_403_for_foreign_session(pool: PgPool) {
        let owner = Uuid::new_v4();
        let attacker = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Mira").await;
        let instance_id = seed_instance(&pool, genome_id, owner).await;
        let session_id = seed_session(&pool, owner, instance_id).await;

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(attacker);

        let body = serde_json::to_vec(&json!({
            "deltas": {
                "warmth": 0.5, "trust": 0.0, "intrigue": 0.0,
                "intimacy": 0.0, "patience": 0.0, "tension": 0.0
            },
            "label": "kiss"
        }))
        .unwrap();

        let req = Request::builder()
            .method("POST")
            .uri(format!("/comp/chat/{session_id}/event/gift"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, _body) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    // ─── Bonus: debug affinity endpoint round-trips when enabled ────

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn debug_affinity_returns_vector_for_owner(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Solace").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;

        // Pre-create the affinity row so the debug GET has something to read.
        let repo = AffinityRepo { pool: &pool };
        let _ = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let req = Request::builder()
            .uri(format!("/comp/affinity/{session_id}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let (status, body) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK, "got body: {body}");
        // Defaults from migration: warmth=0.3, intrigue=0.5
        assert!((body["warmth"].as_f64().unwrap() - 0.3).abs() < 1e-9);
        assert!((body["intrigue"].as_f64().unwrap() - 0.5).abs() < 1e-9);
    }
}
