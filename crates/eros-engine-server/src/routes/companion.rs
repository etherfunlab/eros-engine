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
//!   reachable through the streaming `/message/stream` path with whatever
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
use eros_engine_core::types::LlmAudit;
use eros_engine_core::types::PromptTrait;
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::{ChatRepo, ChatSession};
use eros_engine_store::insight::{compute_training_level, InsightRepo};
use eros_engine_store::ownership::OwnershipRepo;
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

/// NFT-ownership gate. Returns `Ok(())` immediately if `asset_id` is `None`
/// (legacy seed-persona genome). Otherwise joins persona_ownership with
/// wallet_links (linked=true) and returns 403 on no match.
///
/// Called at chat-start (before create_instance) and at every chat message
/// (sync + async). The join is a single indexed PK lookup followed by an
/// index lookup on wallet_pubkey — sub-ms.
pub(crate) async fn enforce_nft_ownership(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    asset_id: Option<&str>,
) -> Result<(), AppError> {
    let Some(asset_id) = asset_id else {
        return Ok(());
    };
    let owns = OwnershipRepo { pool }
        .owns(user_id, asset_id)
        .await
        .map_err(AppError::from)?;
    if owns {
        Ok(())
    } else {
        Err(AppError::Forbidden("nft_ownership_required".into()))
    }
}

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

/// Caller-supplied prompt-injection fragment. See `docs/prompt-traits.md`.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct PromptTraitDto {
    /// ASCII identifier, regex `^[a-z0-9_]{1,32}$`. Used for logging.
    pub tag: String,
    /// Verbatim text inserted under `【附加指引】` in the system prompt.
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

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GiftEventResponse {
    pub reply: Option<String>,
    pub applied_deltas: AffinityDeltasDto,
    pub relationship_label: Option<String>,
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
/// `POST /bff/v1/comp/chat/start`. Encapsulates instance lookup, NFT
/// ownership gate (with the **exact** ordering both endpoints depend on),
/// and resume-or-create on `chat_sessions`. Caller is responsible for
/// building its own response DTO from the returned `ResolvedSession`.
///
/// **NFT gate ordering (load-bearing):**
///   * Explicit `instance_id` → gate runs AFTER load + owner check, so we
///     never gate a missing or non-owned instance.
///   * `genome_id` only → gate runs BEFORE find-or-create on
///     `persona_instances`, so a non-owner who hits the create-fallback
///     does NOT leave a stray row.
pub(crate) async fn resolve_or_create_session(
    state: &AppState,
    user_id: Uuid,
    req: &StartChatRequest,
) -> Result<ResolvedSession, AppError> {
    let persona_repo = PersonaRepo { pool: &state.pool };
    let chat_repo = ChatRepo { pool: &state.pool };

    let instance_id = match req.instance_id {
        Some(iid) => {
            let companion = persona_repo
                .load_companion(iid)
                .await?
                .ok_or_else(|| AppError::NotFound("instance not found".into()))?;
            if companion.instance.owner_uid != user_id {
                return Err(AppError::Forbidden(
                    "instance not owned by this user".into(),
                ));
            }
            let asset_id_opt = persona_repo
                .get_asset_id_for_genome(companion.instance.genome_id)
                .await?;
            enforce_nft_ownership(&state.pool, user_id, asset_id_opt.as_deref()).await?;
            iid
        }
        None => {
            let genome_id = req
                .genome_id
                .ok_or_else(|| AppError::BadRequest("missing genome_id (or instance_id)".into()))?;

            let genome = persona_repo
                .get_genome(genome_id)
                .await?
                .ok_or_else(|| AppError::NotFound("genome not found".into()))?;
            if !genome.is_active {
                return Err(AppError::BadRequest("genome is not active".into()));
            }

            let asset_id_opt = persona_repo.get_asset_id_for_genome(genome_id).await?;
            enforce_nft_ownership(&state.pool, user_id, asset_id_opt.as_deref()).await?;

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

    let companion = persona_repo
        .load_companion(instance_id)
        .await?
        .ok_or_else(|| AppError::NotFound("persona not loadable".into()))?;

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

    Ok(ResolvedSession {
        session_id,
        instance_id,
        persona_name: companion.genome.name,
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
        .routes(routes!(get_history))
        .routes(routes!(list_sessions))
        .routes(routes!(get_profile))
        .routes(routes!(event_gift))
}

// ────────────────────────────────────────────────────────────────────
// Test helpers — visible to other modules' #[cfg(test)] blocks so the
// /s2s/* integration tests in routes/s2s.rs can reuse `test_state`.
// Lives outside the inner `tests` module on purpose; Rust's visibility
// rules don't let a sibling module reach into a private `mod tests`.
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
        },
        openrouter: Arc::new(eros_engine_llm::openrouter::OpenRouterClient::new(
            "stub".into(),
            eros_engine_llm::openrouter::AppAttribution::default(),
        )),
        voyage: Arc::new(eros_engine_llm::voyage::VoyageClient::new("stub".into())),
        model_config: Arc::new(eros_engine_llm::model_config::ModelConfig::default()),
        stream_slots: std::sync::Arc::new(crate::state::StreamSlots::default()),
        // s2s middleware is opted-out in companion tests (no secret
        // configured → /s2s/* returns 401). The s2s integration tests
        // in routes/s2s.rs override `marketplace_s2s_secret` after
        // calling this helper.
        marketplace_svc_url: None,
        marketplace_s2s_secret: None,
        marketplace_s2s_secret_previous: None,
        http_client: reqwest::Client::new(),
    }
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
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata, is_active) \
             VALUES ($1, 'you are a companion', '{}'::jsonb, true) RETURNING id",
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
    async fn enforce_passes_for_legacy_genome(pool: PgPool) {
        let user = Uuid::new_v4();
        let res = enforce_nft_ownership(&pool, user, None).await;
        assert!(res.is_ok(), "asset_id=None must always pass");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn enforce_rejects_when_not_owner(pool: PgPool) {
        let user = Uuid::new_v4();
        let res =
            enforce_nft_ownership(&pool, user, Some("11111111111111111111111111111111")).await;
        match res {
            Err(AppError::Forbidden(_)) => {}
            other => panic!("expected Forbidden, got {other:?}"),
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn enforce_passes_for_owner(pool: PgPool) {
        use chrono::Utc;
        use eros_engine_store::ownership::OwnershipRepo;
        use eros_engine_store::wallets::WalletLinkRepo;

        let user = Uuid::new_v4();
        let wallet = "BvHvbHBeF2zXa1pT5eExMzTAydPGFTyhqMAbPyuMTfQt";
        let asset = "11111111111111111111111111111131";
        WalletLinkRepo { pool: &pool }
            .upsert(user, wallet, true, Utc::now())
            .await
            .unwrap();
        OwnershipRepo { pool: &pool }
            .upsert(asset, "p-1", wallet, Utc::now())
            .await
            .unwrap();

        assert!(enforce_nft_ownership(&pool, user, Some(asset))
            .await
            .is_ok());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn start_chat_403_on_unowned_nft_genome(pool: PgPool) {
        // Seed an NFT-backed genome whose asset_id no one currently owns.
        let genome_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.persona_genomes
                (id, name, system_prompt, art_metadata, is_active, asset_id)
             VALUES ($1, 'NftGenome', 'p', '{}'::jsonb, true,
                     '11111111111111111111111111111131')",
        )
        .bind(genome_id)
        .execute(&pool)
        .await
        .unwrap();

        let state = test_state(pool.clone());
        let mut app = build_router(state);
        let token = mint_test_jwt(Uuid::new_v4());

        let body = serde_json::to_vec(&serde_json::json!({ "genome_id": genome_id })).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/comp/chat/start")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, _resp) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Crucially: NO instance row was created.
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM engine.persona_instances WHERE genome_id = $1")
                .bind(genome_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            count.0, 0,
            "non-owner must not create a hidden persona_instance"
        );
    }

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
    async fn message_403_after_unlink(pool: PgPool) {
        use chrono::Utc;
        use eros_engine_store::ownership::OwnershipRepo;
        use eros_engine_store::wallets::WalletLinkRepo;

        // Setup: NFT genome, owner, started a session.
        let user = Uuid::new_v4();
        let wallet = "BvHvbHBeF2zXa1pT5eExMzTAydPGFTyhqMAbPyuMTfQt";
        let asset = "11111111111111111111111111111131";
        let genome_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.persona_genomes
                (id, name, system_prompt, art_metadata, is_active, asset_id)
             VALUES ($1, 'NftGenome', 'p', '{}'::jsonb, true, $2)",
        )
        .bind(genome_id)
        .bind(asset)
        .execute(&pool)
        .await
        .unwrap();
        WalletLinkRepo { pool: &pool }
            .upsert(user, wallet, true, Utc::now())
            .await
            .unwrap();
        OwnershipRepo { pool: &pool }
            .upsert(asset, "p-1", wallet, Utc::now())
            .await
            .unwrap();

        let state = test_state(pool.clone());
        let mut app = build_router(state);
        let token = mint_test_jwt(user);

        // Start a chat (passes the gate).
        let body = serde_json::to_vec(&serde_json::json!({ "genome_id": genome_id })).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/comp/chat/start")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, resp) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK, "start should succeed: {resp}");
        let session_id = resp["session_id"].as_str().unwrap().to_string();

        // Unlink the wallet — ownership chain now broken.
        WalletLinkRepo { pool: &pool }
            .upsert(user, wallet, false, Utc::now())
            .await
            .unwrap();

        // Sending a message should now 403. The per-message NFT recheck
        // lives on the streaming endpoint (the sync endpoint is gone); the
        // ownership failure is returned before any SSE body is produced, so
        // we can assert on the pre-stream status directly.
        let body = serde_json::to_vec(&serde_json::json!({
            "content": "hi",
            "client_msg_id": "01J4444444444444444444444A",
        }))
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri(format!("/comp/chat/{session_id}/message/stream"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, _resp) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
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
    async fn resolve_or_create_session_nft_gate_blocks_unowned_genome(pool: PgPool) {
        // genome_id branch: NFT gate must run BEFORE we look for an instance.
        let user_id = Uuid::new_v4();
        let genome_id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata, is_active, asset_id) \
             VALUES ('NftPersona', 'sys', '{}'::jsonb, true, 'asset-x') RETURNING id",
        )
        .fetch_one(&pool).await.unwrap();
        let state = test_state(pool.clone());

        let req = StartChatRequest {
            instance_id: None,
            genome_id: Some(genome_id),
            is_demo: None,
        };
        let err = resolve_or_create_session(&state, user_id, &req)
            .await
            .expect_err("should reject unowned NFT-gated genome");
        match err {
            AppError::Forbidden(msg) => {
                assert!(msg.contains("nft_ownership_required"), "msg={msg}")
            }
            other => panic!("expected Forbidden, got {other:?}"),
        }

        // Confirm no orphan persona_instances row was created — this is what
        // makes "gate before find-or-create" load-bearing.
        let leftover: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM engine.persona_instances WHERE genome_id = $1 AND owner_uid = $2",
        )
        .bind(genome_id)
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(leftover, 0, "NFT gate must reject before create_instance");
    }
}
