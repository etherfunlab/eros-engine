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
//! - The credit ledger is gone in OSS — tipping is handled inline on the
//!   streaming `/message/stream` path via `tips_amount_usd`, not through a
//!   separate credit-spending endpoint.
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
use eros_engine_core::types::LlmAudit;
use eros_engine_core::types::PromptTrait;
use eros_engine_store::chat::{ChatRepo, ChatSession};
use eros_engine_store::insight::{compute_training_level, InsightRepo};
use eros_engine_store::persona::PersonaRepo;

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::state::AppState;

/// Per-request prompt-injection limits. Conservative defaults; deployers
/// can tighten by editing these consts. Kept in one block so future env
/// overrides land here.
const MAX_PROMPT_TRAITS: usize = 8;
const MAX_PROMPT_TRAIT_TEXT_CHARS: usize = 2000;
const MAX_PROMPT_TRAIT_TAG_LEN: usize = 32;

/// Audit-string caps. Conservative: holds any reasonable hash without
/// inviting raw PII in `user`. No OpenRouter doc requirement; engine-side
/// guard.
const MAX_LLM_AUDIT_STRING_CHARS: usize = 256;
/// OpenRouter documented cap.
const MAX_LLM_AUDIT_METADATA_KEYS: usize = 16;
/// OpenRouter documented cap.
const MAX_LLM_AUDIT_METADATA_KEY_CHARS: usize = 64;
/// OpenRouter documented cap.
const MAX_LLM_AUDIT_METADATA_VALUE_CHARS: usize = 512;

// ─── DTOs ───────────────────────────────────────────────────────────

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

/// Caller-supplied prompt-injection fragment. See `docs/prompt-traits.md`.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct PromptTraitDto {
    /// ASCII identifier, regex `^[a-z0-9_]{1,32}$`. Used for logging.
    pub tag: String,
    /// Verbatim text inserted under `[additional_guidance]` in the system prompt.
    /// 1 ≤ chars ≤ 2000 after trim.
    pub text: String,
}

/// Caller-supplied OpenRouter audit passthrough. All three fields are
/// optional; engine never inspects content. See `docs/llm-audit.md`.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct LlmAuditDto {
    /// Free-form caller identifier (recommended: hash of internal user id).
    /// `chars ≤ 256`. Forwarded as OpenRouter wire `user`.
    #[serde(default)]
    pub user: Option<String>,
    /// Caller-defined session / conversation grouping. Distinct from the
    /// URL path's `session_id`. `chars ≤ 256`. Forwarded as wire
    /// `session_id`.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Up to 16 string-valued key/value pairs. Key regex
    /// `^[A-Za-z0-9_.-]{1,64}$`, value `chars ≤ 512`.
    #[serde(default)]
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

// ─── Helpers ────────────────────────────────────────────────────────

/// Verify a session exists and is owned by `user_id`. Returns the session
/// row on success, `404` if missing, `403` if owned by someone else.
pub(crate) async fn require_session_for_user(
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

/// Validate a caller-supplied list of `PromptTraitDto` and convert to the
/// core `PromptTrait` shape. Empty input is allowed and returns `vec![]`.
///
/// Rules (all violations → `400 BadRequest`):
/// - `traits.len()` ≤ `MAX_PROMPT_TRAITS`
/// - `tag` matches `^[a-z0-9_]+$` and `1..=MAX_PROMPT_TRAIT_TAG_LEN` chars
/// - `text.trim()` non-empty
/// - `text.chars().count()` ≤ `MAX_PROMPT_TRAIT_TEXT_CHARS`
/// - `text` contains no control characters (would break bullet rendering)
pub(crate) fn validate_prompt_traits(
    dtos: &[PromptTraitDto],
) -> Result<Vec<PromptTrait>, AppError> {
    if dtos.len() > MAX_PROMPT_TRAITS {
        return Err(AppError::BadRequest(format!(
            "too many prompt_traits (max {MAX_PROMPT_TRAITS})"
        )));
    }
    let mut out = Vec::with_capacity(dtos.len());
    for (i, dto) in dtos.iter().enumerate() {
        // tag: 1..=MAX bytes, all [a-z0-9_]
        if dto.tag.is_empty() || dto.tag.len() > MAX_PROMPT_TRAIT_TAG_LEN {
            return Err(AppError::BadRequest(format!(
                "prompt_traits[{i}].tag must be 1..={MAX_PROMPT_TRAIT_TAG_LEN} chars"
            )));
        }
        if !dto
            .tag
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
        {
            return Err(AppError::BadRequest(format!(
                "prompt_traits[{i}].tag must match [a-z0-9_]+"
            )));
        }
        // text: non-empty after trim, length-capped by char count (not bytes)
        // of the TRIMMED form so leading/trailing whitespace doesn't eat the
        // budget. Both checks use the same `trimmed` slice — matches the
        // `1 ≤ chars ≤ 2000 (after trim)` contract in docs/prompt-traits.md.
        let trimmed = dto.text.trim();
        if trimmed.is_empty() {
            return Err(AppError::BadRequest(format!(
                "prompt_traits[{i}].text must not be blank"
            )));
        }
        if trimmed.chars().count() > MAX_PROMPT_TRAIT_TEXT_CHARS {
            return Err(AppError::BadRequest(format!(
                "prompt_traits[{i}].text exceeds {MAX_PROMPT_TRAIT_TEXT_CHARS} chars after trim"
            )));
        }
        // text: no characters that would break the single-line bullet
        // rendering in `build_prompt`. `char::is_control` covers
        // \n / \r / \t / DEL / C1 controls; we additionally reject the
        // Unicode LINE SEPARATOR (U+2028) and PARAGRAPH SEPARATOR
        // (U+2029) which are NOT in Cc but DO start a new line.
        if dto
            .text
            .chars()
            .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}')
        {
            return Err(AppError::BadRequest(format!(
                "prompt_traits[{i}].text must not contain line-break or control characters"
            )));
        }
        out.push(PromptTrait {
            tag: dto.tag.clone(),
            text: trimmed.to_string(),
        });
    }
    Ok(out)
}

/// Deployer-controlled suppression of wholesale cost fields from the
/// streaming `/message/stream` response usage block. Operator tracing is
/// unaffected — this only touches the value before it leaves the HTTP layer.
///
/// Remove the configured top-level keys from a `usage` JSON object in
/// place. No-op when `hidden` is empty, when `usage` is `None`, or
/// when the value is not a JSON object. Caller passes the
/// `Option<Value>` by mutable reference so the public response struct
/// is touched at most once per request.
///
/// Only top-level keys are affected; nested sub-keys inside a retained
/// object (e.g. `prompt_tokens.details`) are out of scope — list the
/// parent key to suppress the whole subtree.
pub(crate) fn filter_usage_keys(
    usage: &mut Option<serde_json::Value>,
    hidden: &std::collections::HashSet<String>,
) {
    if hidden.is_empty() {
        return;
    }
    let Some(value) = usage.as_mut() else { return };
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    for key in hidden {
        obj.remove(key);
    }
}

/// Validate a caller-supplied `audit` object against the documented caps.
/// Returns `Ok(None)` when the field is absent. `Err(BadRequest)` for any
/// cap violation — first failure wins so the message points at one cause.
pub(crate) fn validate_llm_audit(dto: Option<LlmAuditDto>) -> Result<Option<LlmAudit>, AppError> {
    let Some(dto) = dto else { return Ok(None) };

    if let Some(ref u) = dto.user {
        if u.chars().count() > MAX_LLM_AUDIT_STRING_CHARS {
            return Err(AppError::BadRequest(format!(
                "audit.user exceeds {MAX_LLM_AUDIT_STRING_CHARS} chars"
            )));
        }
    }
    if let Some(ref s) = dto.session_id {
        if s.chars().count() > MAX_LLM_AUDIT_STRING_CHARS {
            return Err(AppError::BadRequest(format!(
                "audit.session_id exceeds {MAX_LLM_AUDIT_STRING_CHARS} chars"
            )));
        }
    }
    if let Some(ref m) = dto.metadata {
        if m.len() > MAX_LLM_AUDIT_METADATA_KEYS {
            return Err(AppError::BadRequest(format!(
                "audit.metadata exceeds {MAX_LLM_AUDIT_METADATA_KEYS} keys"
            )));
        }
        for (k, v) in m.iter() {
            if k.is_empty() || k.chars().count() > MAX_LLM_AUDIT_METADATA_KEY_CHARS {
                return Err(AppError::BadRequest(format!(
                    "audit.metadata key length must be 1..={MAX_LLM_AUDIT_METADATA_KEY_CHARS}"
                )));
            }
            if !k
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
            {
                return Err(AppError::BadRequest(format!(
                    "audit.metadata key '{k}' must match [A-Za-z0-9_.-]"
                )));
            }
            let s = v.as_str().ok_or_else(|| {
                AppError::BadRequest(format!("audit.metadata['{k}'] must be a string value"))
            })?;
            if s.chars().count() > MAX_LLM_AUDIT_METADATA_VALUE_CHARS {
                return Err(AppError::BadRequest(format!(
                    "audit.metadata['{k}'] exceeds {MAX_LLM_AUDIT_METADATA_VALUE_CHARS} chars"
                )));
            }
        }
    }

    Ok(Some(LlmAudit {
        user: dto.user,
        session_id: dto.session_id,
        metadata: dto.metadata,
    }))
}

// ─── Handlers ───────────────────────────────────────────────────────

/// Output of `resolve_or_create_session`. Carries everything either the
/// canonical `start_chat` or the BFF `bff_start_chat` needs to build its
/// response. `is_new` is `true` when this call **created** the session row,
/// `false` when an existing session was resumed.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedSession {
    pub session_id: Uuid,
    pub instance_id: Uuid,
    pub persona_name: String,
    pub is_new: bool,
}

/// Shared session-resolution flow used by `POST /comp/chat/start` and
/// `POST /bff/v1/comp/chat/start`. Encapsulates instance lookup (with the
/// explicit-`instance_id` owner check) and resume-or-create on
/// `chat_sessions`. Caller is responsible for building its own response DTO
/// from the returned `ResolvedSession`.
pub(crate) async fn resolve_or_create_session(
    state: &AppState,
    user_id: Uuid,
    req: &StartChatRequest,
) -> Result<ResolvedSession, AppError> {
    let persona_repo = PersonaRepo { pool: &state.pool };
    let chat_repo = ChatRepo { pool: &state.pool };

    let (instance_id, persona_name) = match req.instance_id {
        Some(iid) => {
            // Explicit instance: one JOIN read gives owner + genome name
            // (replaces the former double load_companion + asset read).
            let gate = persona_repo
                .load_instance_gate(iid)
                .await?
                .ok_or_else(|| AppError::NotFound("instance not found".into()))?;
            if gate.owner_uid != user_id {
                return Err(AppError::Forbidden(
                    "instance not owned by this user".into(),
                ));
            }
            (iid, gate.genome_name)
        }
        None => {
            let genome_id = req
                .genome_id
                .ok_or_else(|| AppError::BadRequest("missing genome_id (or instance_id)".into()))?;

            // Two independent reads in one latency wave: `genome_id` comes from
            // the request, so the instance lookup does not depend on the gate read.
            let (gate, existing_instance) = tokio::try_join!(
                persona_repo.get_genome_gate(genome_id),
                persona_repo.find_active_instance(genome_id, user_id),
            )?;

            let gate = gate.ok_or_else(|| AppError::NotFound("genome not found".into()))?;

            let iid = match existing_instance {
                Some(iid) => iid,
                // Upsert: create new, or reactivate an archived row (#37).
                None => {
                    persona_repo
                        .ensure_active_instance(genome_id, user_id)
                        .await?
                }
            };
            (iid, gate.name)
        }
    };

    // Resume the latest session (bumping last_active_at in one statement), or
    // create a fresh one. Only `id` is consumed downstream.
    let (session_id, is_new) = match chat_repo
        .resume_latest_session(user_id, instance_id)
        .await?
    {
        Some(s) => (s.id, false),
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

    Ok(ResolvedSession {
        session_id,
        instance_id,
        persona_name,
        is_new,
    })
}

/// Start (or resume) a chat session for the JWT user.
///
/// Resolution rules:
///   * `instance_id` provided → must belong to the JWT user.
///   * else `genome_id` provided → look up (or auto-create) the user's
///     active instance of that genome.
///   * else (neither provided) → 400 Bad Request.
#[utoipa::path(
    post,
    path = "/comp/chat/start",
    tag = "companion",
    request_body = StartChatRequest,
    responses(
        (status = 200, body = StartChatResponse),
        (status = 400, description = "missing genome_id and no existing instance"),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "instance not owned by this user"),
        (status = 404, description = "instance/genome not found")
    ),
    security(("bearer" = []))
)]
async fn start_chat(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<StartChatRequest>,
) -> Result<Json<StartChatResponse>, AppError> {
    let resolved = resolve_or_create_session(&state, user_id, &req).await?;
    Ok(Json(StartChatResponse {
        session_id: resolved.session_id,
        instance_id: resolved.instance_id,
        persona_name: resolved.persona_name,
        is_new: resolved.is_new,
    }))
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
            // Vestigial: the engine.chat_messages.extracted_facts column was
            // dropped in migration 0017. The field stays on this canonical
            // DTO (always null) to preserve the documented OSS API contract.
            extracted_facts: None,
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

// ─── Router ─────────────────────────────────────────────────────────

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(start_chat))
        .routes(routes!(get_history))
        .routes(routes!(list_sessions))
        .routes(routes!(get_profile))
}

// ────────────────────────────────────────────────────────────────────
// Test helpers — visible to other modules' #[cfg(test)] blocks so sibling
// test modules can reuse `test_state`. Lives outside the inner `tests`
// module on purpose; Rust's visibility rules don't let a sibling module
// reach into a private `mod tests`.
// ────────────────────────────────────────────────────────────────────
#[cfg(test)]
pub(crate) const TEST_SECRET: &str = "test-secret-companion-routes";

#[cfg(test)]
pub(crate) fn test_state(pool: sqlx::PgPool) -> AppState {
    use crate::auth::supabase::SupabaseJwtValidator;
    use crate::auth::AuthValidator;
    use std::sync::Arc;

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
            // and the fields are just for AppState completeness.
            dreaming_tick: std::time::Duration::ZERO,
            dreaming_idle_threshold: std::time::Duration::from_secs(1800),
            dreaming_claim_stale_threshold: std::time::Duration::from_secs(600),
            openrouter_usage_hidden_keys: std::collections::HashSet::new(),
            // Snapshot sweeper disabled in tests — same rationale as dreaming.
            snapshot: crate::state::SnapshotConfig {
                disabled: true,
                cron: "0 0 23 * * *".into(),
                tz: chrono_tz::Asia::Singapore,
            },
            prompt_log_dir: None,
        },
        openrouter: Arc::new(eros_engine_llm::openrouter::OpenRouterClient::new(
            "stub".into(),
            eros_engine_llm::openrouter::AppAttribution::default(),
        )),
        voyage: Arc::new(eros_engine_llm::voyage::VoyageClient::new("stub".into())),
        model_config: Arc::new(eros_engine_llm::model_config::ModelConfig::default()),
        output_regex: std::sync::Arc::new(Vec::new()),
        stream_slots: std::sync::Arc::new(crate::state::StreamSlots::default()),
    }
}

// ────────────────────────────────────────────────────────────────────
// Integration tests
//
// These exercise the route module's HTTP+DB side-effects against a
// live Postgres instance (via `#[sqlx::test]`). They do NOT exercise
// the LLM-driven path (full end-to-end LLM testing is the job of the
// deploy smoke); the message/streaming routes are covered by the
// pipeline tests instead.
// ────────────────────────────────────────────────────────────────────

// Test helpers shared with sibling test modules (e.g. routes::bff::companion).
// Lives outside `mod tests` so other modules' `#[cfg(test)]` blocks can reach
// them via `crate::routes::companion::testutil` — `mod tests` is private by convention.
#[cfg(test)]
pub(crate) mod testutil {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
        Router,
    };
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::{json, Value};
    use sqlx::PgPool;
    use tower::Service;
    use uuid::Uuid;

    use crate::state::AppState;

    pub(crate) fn mint_test_jwt(uid: Uuid) -> String {
        let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp();
        encode(
            &Header::default(),
            &json!({ "sub": uid.to_string(), "exp": exp }),
            &EncodingKey::from_secret(super::TEST_SECRET.as_ref()),
        )
        .expect("test jwt encodes")
    }

    pub(crate) fn build_router(state: AppState) -> Router {
        let (axum_router, _api) = crate::routes::router(state.clone()).split_for_parts();
        axum_router.with_state(state)
    }

    pub(crate) async fn send_request(
        router: &mut Router,
        req: Request<Body>,
    ) -> (StatusCode, Value) {
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

    pub(crate) async fn seed_genome(pool: &PgPool, name: &str) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ($1, 'you are a companion', '{}'::jsonb) RETURNING id",
        )
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    pub(crate) async fn seed_session(pool: &PgPool, user_id: Uuid, instance_id: Uuid) -> Uuid {
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

    pub(crate) async fn seed_instance(pool: &PgPool, genome_id: Uuid, owner: Uuid) -> Uuid {
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
}

#[cfg(test)]
mod tests {
    use super::testutil::{
        build_router, mint_test_jwt, seed_genome, seed_instance, seed_session, send_request,
    };
    use super::*;

    use axum::{
        body::Body,
        http::{header, Request, StatusCode},
    };
    use eros_engine_store::affinity::AffinityRepo;
    use serde_json::json;
    use sqlx::PgPool;

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
    async fn protected_route_401_without_bearer(pool: PgPool) {
        let state = test_state(pool);
        let mut app = build_router(state);

        let req = Request::builder()
            .uri(format!("/comp/chat/{}/sessions", Uuid::new_v4()))
            .body(Body::empty())
            .unwrap();
        let (status, _body) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // ─── Test 3: start_chat creates a session for the JWT user ──────

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

    // ─── Test 4: cross-user GET /chat/{user_id}/sessions → 403 ──────

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

    // ─── Bonus: debug affinity endpoint round-trips when enabled ────

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn start_chat_passes_for_legacy_genome(pool: PgPool) {
        // Unchanged path: legacy seed-persona must still work.
        let genome_id = seed_genome(&pool, "Echo").await;
        let user = Uuid::new_v4();
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user);

        let body = serde_json::to_vec(&serde_json::json!({ "genome_id": genome_id })).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/comp/chat/start")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, _) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK);
    }

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

    // ─── Prompt-traits validator unit tests ─────────────────────────

    #[test]
    fn validate_traits_accepts_empty_input() {
        let out = validate_prompt_traits(&[]).expect("empty ok");
        assert!(out.is_empty());
    }

    #[test]
    fn validate_traits_accepts_two_well_formed_entries() {
        let dtos = vec![
            PromptTraitDto {
                tag: "nsfw_boost".into(),
                text: "x".into(),
            },
            PromptTraitDto {
                tag: "politics_open".into(),
                text: "y".into(),
            },
        ];
        let out = validate_prompt_traits(&dtos).expect("ok");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].tag, "nsfw_boost");
    }

    #[test]
    fn validate_traits_rejects_more_than_max() {
        let dtos: Vec<PromptTraitDto> = (0..9)
            .map(|i| PromptTraitDto {
                tag: format!("t{i}"),
                text: "x".into(),
            })
            .collect();
        let err = validate_prompt_traits(&dtos).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn validate_traits_rejects_oversized_text() {
        let big = "a".repeat(2001);
        let dtos = vec![PromptTraitDto {
            tag: "ok".into(),
            text: big,
        }];
        assert!(matches!(
            validate_prompt_traits(&dtos),
            Err(AppError::BadRequest(_))
        ));
    }

    #[test]
    fn validate_traits_rejects_empty_text_after_trim() {
        let dtos = vec![PromptTraitDto {
            tag: "ok".into(),
            text: "   ".into(),
        }];
        assert!(matches!(
            validate_prompt_traits(&dtos),
            Err(AppError::BadRequest(_))
        ));
    }

    #[test]
    fn validate_traits_rejects_invalid_tag_regex() {
        for bad in [
            "",
            "NSFW",
            "with space",
            "too_long_tag_xxxxxxxxxxxxxxxxxxxxxxx",
        ] {
            let dtos = vec![PromptTraitDto {
                tag: bad.into(),
                text: "x".into(),
            }];
            assert!(
                matches!(validate_prompt_traits(&dtos), Err(AppError::BadRequest(_))),
                "tag {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn validate_traits_rejects_text_with_newlines() {
        let dtos = vec![PromptTraitDto {
            tag: "ok".into(),
            text: "first line\nsecond line".into(),
        }];
        assert!(
            matches!(validate_prompt_traits(&dtos), Err(AppError::BadRequest(_))),
            "embedded newline must be rejected so bullet rendering stays safe"
        );
    }

    #[test]
    fn validate_traits_rejects_text_with_unicode_line_separators() {
        for sep in ["a\u{2028}b", "a\u{2029}b"] {
            let dtos = vec![PromptTraitDto {
                tag: "ok".into(),
                text: sep.into(),
            }];
            assert!(
                matches!(validate_prompt_traits(&dtos), Err(AppError::BadRequest(_))),
                "Unicode line separator in text must be rejected: {sep:?}"
            );
        }
    }

    // ─── LlmAudit validator unit tests ──────────────────────────────

    #[test]
    fn validate_llm_audit_none_returns_none() {
        let out = validate_llm_audit(None).expect("None input ok");
        assert!(out.is_none());
    }

    #[test]
    fn validate_llm_audit_full_passes() {
        let mut metadata = serde_json::Map::new();
        metadata.insert("feature".into(), serde_json::Value::String("chat".into()));
        let dto = LlmAuditDto {
            user: Some("u_abc".into()),
            session_id: Some("conv_xyz".into()),
            metadata: Some(metadata),
        };
        let out = validate_llm_audit(Some(dto)).expect("ok").expect("Some");
        assert_eq!(out.user.as_deref(), Some("u_abc"));
        assert_eq!(out.session_id.as_deref(), Some("conv_xyz"));
        assert_eq!(
            out.metadata
                .as_ref()
                .and_then(|m| m.get("feature"))
                .and_then(|v| v.as_str()),
            Some("chat")
        );
    }

    #[test]
    fn validate_llm_audit_rejects_oversized_user() {
        let dto = LlmAuditDto {
            user: Some("x".repeat(MAX_LLM_AUDIT_STRING_CHARS + 1)),
            session_id: None,
            metadata: None,
        };
        assert!(matches!(
            validate_llm_audit(Some(dto)),
            Err(AppError::BadRequest(_))
        ));
    }

    #[test]
    fn validate_llm_audit_rejects_oversized_session_id() {
        let dto = LlmAuditDto {
            user: None,
            session_id: Some("x".repeat(MAX_LLM_AUDIT_STRING_CHARS + 1)),
            metadata: None,
        };
        assert!(matches!(
            validate_llm_audit(Some(dto)),
            Err(AppError::BadRequest(_))
        ));
    }

    #[test]
    fn validate_llm_audit_rejects_too_many_metadata_keys() {
        let mut metadata = serde_json::Map::new();
        for i in 0..(MAX_LLM_AUDIT_METADATA_KEYS + 1) {
            metadata.insert(format!("k{i}"), serde_json::Value::String("v".into()));
        }
        let dto = LlmAuditDto {
            user: None,
            session_id: None,
            metadata: Some(metadata),
        };
        assert!(matches!(
            validate_llm_audit(Some(dto)),
            Err(AppError::BadRequest(_))
        ));
    }

    #[test]
    fn validate_llm_audit_rejects_invalid_metadata_key_regex() {
        let mut metadata = serde_json::Map::new();
        metadata.insert("Bad Key!".into(), serde_json::Value::String("v".into()));
        let dto = LlmAuditDto {
            user: None,
            session_id: None,
            metadata: Some(metadata),
        };
        assert!(matches!(
            validate_llm_audit(Some(dto)),
            Err(AppError::BadRequest(_))
        ));
    }

    #[test]
    fn validate_llm_audit_rejects_oversized_metadata_key() {
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "x".repeat(MAX_LLM_AUDIT_METADATA_KEY_CHARS + 1),
            serde_json::Value::String("v".into()),
        );
        let dto = LlmAuditDto {
            user: None,
            session_id: None,
            metadata: Some(metadata),
        };
        assert!(matches!(
            validate_llm_audit(Some(dto)),
            Err(AppError::BadRequest(_))
        ));
    }

    #[test]
    fn validate_llm_audit_rejects_non_string_metadata_value() {
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "feature".into(),
            serde_json::Value::Number(serde_json::Number::from(123)),
        );
        let dto = LlmAuditDto {
            user: None,
            session_id: None,
            metadata: Some(metadata),
        };
        assert!(matches!(
            validate_llm_audit(Some(dto)),
            Err(AppError::BadRequest(_))
        ));
    }

    #[test]
    fn validate_llm_audit_rejects_oversized_metadata_value() {
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "feature".into(),
            serde_json::Value::String("v".repeat(MAX_LLM_AUDIT_METADATA_VALUE_CHARS + 1)),
        );
        let dto = LlmAuditDto {
            user: None,
            session_id: None,
            metadata: Some(metadata),
        };
        assert!(matches!(
            validate_llm_audit(Some(dto)),
            Err(AppError::BadRequest(_))
        ));
    }

    // ─── filter_usage_keys unit tests ───────────────────────────────

    #[test]
    fn usage_filter_strips_configured_keys() {
        let mut hidden = std::collections::HashSet::new();
        hidden.insert("cost".to_string());
        hidden.insert("cost_details".to_string());
        let mut usage = Some(serde_json::json!({
            "prompt_tokens": 10,
            "completion_tokens": 8,
            "total_tokens": 18,
            "cost": 0.0004,
            "cost_details": { "upstream": 0.0003 }
        }));
        filter_usage_keys(&mut usage, &hidden);
        let out = usage.expect("usage still Some");
        assert_eq!(out.get("prompt_tokens").and_then(|v| v.as_u64()), Some(10));
        assert_eq!(out.get("total_tokens").and_then(|v| v.as_u64()), Some(18));
        assert!(out.get("cost").is_none(), "cost must be stripped");
        assert!(
            out.get("cost_details").is_none(),
            "cost_details must be stripped"
        );
    }

    #[test]
    fn usage_filter_no_op_when_set_empty() {
        let hidden = std::collections::HashSet::new();
        let original = serde_json::json!({"prompt_tokens": 10, "cost": 0.0004});
        let mut usage = Some(original.clone());
        filter_usage_keys(&mut usage, &hidden);
        assert_eq!(usage, Some(original));
    }

    #[test]
    fn usage_filter_no_op_when_usage_is_none() {
        let mut hidden = std::collections::HashSet::new();
        hidden.insert("cost".to_string());
        let mut usage: Option<serde_json::Value> = None;
        filter_usage_keys(&mut usage, &hidden);
        assert!(usage.is_none());
    }

    #[test]
    fn usage_filter_no_op_when_value_not_object() {
        let mut hidden = std::collections::HashSet::new();
        hidden.insert("cost".to_string());
        let mut usage = Some(serde_json::Value::String("opaque".into()));
        filter_usage_keys(&mut usage, &hidden);
        assert_eq!(usage, Some(serde_json::Value::String("opaque".into())));
    }

    // ─── Message-payload validation (formerly exercised via the sync
    //     /message endpoint; now direct validator calls) ───────────────
    //
    // These cover the prompt-trait limits the sync endpoint used to gate.
    // The sync handler is gone (replaced by /message/stream, which calls
    // the same `validate_prompt_traits`), so they assert on the validator
    // directly — no DB / HTTP plumbing required. The exact over/under-cap
    // inputs match the original endpoint tests.

    #[test]
    fn send_message_rejects_too_many_prompt_traits() {
        // 9 traits > MAX_PROMPT_TRAITS (8) → BadRequest.
        let dtos: Vec<PromptTraitDto> = (0..9)
            .map(|i| PromptTraitDto {
                tag: format!("t{i}"),
                text: "x".into(),
            })
            .collect();
        assert!(matches!(
            validate_prompt_traits(&dtos),
            Err(AppError::BadRequest(_))
        ));
    }

    #[test]
    fn send_message_rejects_oversized_trait_text() {
        // 2001 chars > MAX_PROMPT_TRAIT_TEXT_CHARS (2000) → BadRequest.
        let dtos = vec![PromptTraitDto {
            tag: "ok".into(),
            text: "a".repeat(2001),
        }];
        assert!(matches!(
            validate_prompt_traits(&dtos),
            Err(AppError::BadRequest(_))
        ));
    }

    #[test]
    fn send_message_rejects_invalid_tag_regex() {
        // Whitespace + uppercase in tag violates [a-z0-9_]+ → BadRequest.
        let dtos = vec![PromptTraitDto {
            tag: "NSFW Boost".into(),
            text: "x".into(),
        }];
        assert!(matches!(
            validate_prompt_traits(&dtos),
            Err(AppError::BadRequest(_))
        ));
    }

    #[test]
    fn send_message_accepts_missing_prompt_traits_field() {
        // A missing `prompt_traits` field deserialises to None, which the
        // handler converts to an empty slice — the validator must accept it.
        let out = validate_prompt_traits(&[]).expect("empty/missing must be accepted");
        assert!(out.is_empty());
    }

    // ─── resolve_or_create_session parity tests ──────────────────────

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn resolve_or_create_session_returns_resolved_for_legacy_genome(pool: PgPool) {
        // resolve_or_create_session is the extracted core of start_chat. Brand-new
        // user × legacy (asset-less) genome → creates a new instance + new session.
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Vita").await;
        let state = test_state(pool.clone());

        let req = StartChatRequest {
            instance_id: None,
            genome_id: Some(genome_id),
            is_demo: None,
        };
        let resolved = resolve_or_create_session(&state, user_id, &req)
            .await
            .expect("resolve_or_create_session");

        assert!(resolved.is_new);
        assert_eq!(resolved.persona_name, "Vita");
        // A second call with the same input should resume — not create a new session.
        let resumed = resolve_or_create_session(&state, user_id, &req)
            .await
            .expect("resume");
        assert!(!resumed.is_new);
        assert_eq!(resumed.session_id, resolved.session_id);
        assert_eq!(resumed.instance_id, resolved.instance_id);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn resolve_reactivates_archived_instance_genome_path(pool: PgPool) {
        // #37: a user with an ARCHIVED instance for the genome must be able
        // to start a chat again. The create-fallback reactivates instead of
        // 500-ing on UNIQUE(genome_id, owner_uid).
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Vita").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        sqlx::query("UPDATE engine.persona_instances SET status = 'archived' WHERE id = $1")
            .bind(instance_id)
            .execute(&pool)
            .await
            .unwrap();
        let state = test_state(pool.clone());

        let req = StartChatRequest {
            instance_id: None,
            genome_id: Some(genome_id),
            is_demo: None,
        };
        let resolved = resolve_or_create_session(&state, user_id, &req)
            .await
            .expect("must reactivate, not 500");

        // UNIQUE(genome_id, owner_uid) ⇒ the same row is reactivated.
        assert_eq!(resolved.instance_id, instance_id);
        let status: String =
            sqlx::query_scalar("SELECT status FROM engine.persona_instances WHERE id = $1")
                .bind(instance_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "active");
        assert!(resolved.is_new, "no prior session existed");
        assert_eq!(resolved.persona_name, "Vita");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn resolve_instance_path_403_for_non_owner(pool: PgPool) {
        let owner = Uuid::new_v4();
        let intruder = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, owner).await;
        let state = test_state(pool.clone());

        let req = StartChatRequest {
            instance_id: Some(instance_id),
            genome_id: None,
            is_demo: None,
        };
        let err = resolve_or_create_session(&state, intruder, &req)
            .await
            .expect_err("non-owner must be forbidden");
        match err {
            AppError::Forbidden(msg) => assert!(msg.contains("not owned"), "msg={msg}"),
            other => panic!("expected Forbidden, got {other:?}"),
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn resolve_instance_path_404_for_archived_instance(pool: PgPool) {
        // load_instance_gate filters status='active', so an explicit
        // instance_id pointing at an archived instance resolves to 404.
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Mira").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        sqlx::query("UPDATE engine.persona_instances SET status = 'archived' WHERE id = $1")
            .bind(instance_id)
            .execute(&pool)
            .await
            .unwrap();
        let state = test_state(pool.clone());

        let req = StartChatRequest {
            instance_id: Some(instance_id),
            genome_id: None,
            is_demo: None,
        };
        let err = resolve_or_create_session(&state, user_id, &req)
            .await
            .expect_err("archived instance must 404");
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
    }
}
