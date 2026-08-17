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
//!   (`ChatRepo` / `AffinityRepo` / `PersonaRepo` / `HumanInsightRepo`).
//! - The credit ledger is gone in OSS — tipping is handled inline on the
//!   streaming `/message/stream` path via `tips_amount_usd`, not through a
//!   separate credit-spending endpoint.
//! - `/comp/user/{user_id}/profile` returns the flat, typed `human_insights`
//!   row (city/occupation/interests/... — see `ProfileResponse`), not the
//!   legacy freeform `companion_insights` JSON blob.

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
use eros_engine_store::human_insight::HumanInsightRepo;
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
    /// read by the affinity pipeline to apply `AFFINITY_DEMO_BOOST` instead
    /// of the global value, so meters move visibly within the turn budget.
    /// Ignored when resuming an existing session.
    #[serde(default)]
    pub is_demo: Option<bool>,
    /// Conversation channel for the session: `"text"` (default) or `"voice"`.
    /// Start/resume is channel-scoped — a voice-channel start never resumes a
    /// text session and vice versa, so the two conversations stay isolated.
    #[serde(default)]
    pub channel: Option<String>,
    /// When `true`, skip resume entirely and always create a fresh session
    /// (returns `is_new: true`), even if a resumable one exists for this
    /// user × instance × channel. Default `false`/omitted keeps the normal
    /// resume-or-create behavior. Intended for callers (e.g. voice calls)
    /// that want one session per call rather than a continued conversation.
    #[serde(default)]
    pub force_new: Option<bool>,
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
    /// Conversation-flavor marker: `"product_qa"` = out-of-character product
    /// answer (excluded from companion context). Omitted for normal turns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
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
    pub is_converted: bool,
    pub last_active_at: DateTime<Utc>,
    /// Conversation channel ('text' or 'voice'); start/resume is channel-scoped.
    pub channel: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ListSessionsResponse {
    pub user_id: Uuid,
    pub sessions: Vec<SessionListEntry>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ProfileResponse {
    pub user_id: Uuid,
    pub city: Option<String>,
    pub location: Option<String>,
    pub hometown: Option<String>,
    pub nationality: Option<String>,
    pub occupation: Option<String>,
    pub mbti_guess: Option<String>,
    pub love_values: Option<String>,
    pub emotional_needs: Option<String>,
    pub life_rhythm: Option<String>,
    pub interests: Vec<String>,
    pub personality_traits: Vec<String>,
    pub preferred_gender: Option<String>,
    pub age_min: Option<i32>,
    pub age_max: Option<i32>,
    pub deal_breakers: Vec<String>,
    pub education: Option<String>,
    pub family: Option<String>,
    pub relationship_history: Option<String>,
    pub social_pattern: Option<String>,
    pub future_plans: Option<String>,
    pub finance_status: Option<String>,
    /// None when the user has no insights row yet.
    pub updated_at: Option<DateTime<Utc>>,
}

impl ProfileResponse {
    fn from_row(
        user_id: Uuid,
        row: Option<eros_engine_store::human_insight::HumanInsightsRow>,
    ) -> Self {
        match row {
            Some(r) => Self {
                user_id,
                city: r.city,
                location: r.location,
                hometown: r.hometown,
                nationality: r.nationality,
                occupation: r.occupation,
                mbti_guess: r.mbti_guess,
                love_values: r.love_values,
                emotional_needs: r.emotional_needs,
                life_rhythm: r.life_rhythm,
                interests: r.interests,
                personality_traits: r.personality_traits,
                preferred_gender: r.preferred_gender,
                age_min: r.age_min,
                age_max: r.age_max,
                deal_breakers: r.deal_breakers,
                education: r.education,
                family: r.family,
                relationship_history: r.relationship_history,
                social_pattern: r.social_pattern,
                future_plans: r.future_plans,
                finance_status: r.finance_status,
                updated_at: Some(r.updated_at),
            },
            None => Self {
                user_id,
                city: None,
                location: None,
                hometown: None,
                nationality: None,
                occupation: None,
                mbti_guess: None,
                love_values: None,
                emotional_needs: None,
                life_rhythm: None,
                interests: vec![],
                personality_traits: vec![],
                preferred_gender: None,
                age_min: None,
                age_max: None,
                deal_breakers: vec![],
                education: None,
                family: None,
                relationship_history: None,
                social_pattern: None,
                future_plans: None,
                finance_status: None,
                updated_at: None,
            },
        }
    }
}

/// Typed `character_insights` profile for one relationship. All fields are
/// null/empty until the character chain has produced its first extraction.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CharacterProfileResponse {
    pub instance_id: Uuid,
    pub location: Option<String>,
    pub occupation: Option<String>,
    pub current_situation: Option<String>,
    pub desires: Option<String>,
    pub vulnerabilities: Option<String>,
    pub habits: Option<String>,
    pub personal_values: Option<String>,
    pub likes: Vec<String>,
    pub dislikes: Vec<String>,
    pub relationships: Vec<String>,
    /// None when the character has no insights row yet.
    pub updated_at: Option<DateTime<Utc>>,
}

impl CharacterProfileResponse {
    fn from_row(
        instance_id: Uuid,
        row: Option<eros_engine_store::character_insight::CharacterInsightsRow>,
    ) -> Self {
        match row {
            Some(r) => Self {
                instance_id,
                location: r.location,
                occupation: r.occupation,
                current_situation: r.current_situation,
                desires: r.desires,
                vulnerabilities: r.vulnerabilities,
                habits: r.habits,
                personal_values: r.personal_values,
                likes: r.likes,
                dislikes: r.dislikes,
                relationships: r.relationships,
                updated_at: Some(r.updated_at),
            },
            None => Self {
                instance_id,
                location: None,
                occupation: None,
                current_situation: None,
                desires: None,
                vulnerabilities: None,
                habits: None,
                personal_values: None,
                likes: vec![],
                dislikes: vec![],
                relationships: vec![],
                updated_at: None,
            },
        }
    }
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

    let channel = match req.channel.as_deref() {
        None | Some("text") => "text",
        Some("voice") => "voice",
        Some(other) => return Err(AppError::BadRequest(format!("invalid channel: {other}"))),
    };

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
    // create a fresh one. Only `id` is consumed downstream. `force_new` skips
    // the resume lookup entirely so the match below always falls to create.
    let resumed = if req.force_new.unwrap_or(false) {
        None
    } else {
        chat_repo
            .resume_latest_session(user_id, instance_id, channel)
            .await?
    };
    let (session_id, is_new) = match resumed {
        Some(s) => (s.id, false),
        None => {
            let metadata = if req.is_demo.unwrap_or(false) {
                serde_json::json!({ "is_demo": true })
            } else {
                serde_json::json!({})
            };
            let s = chat_repo
                .create_session_with_metadata(user_id, instance_id, metadata, channel)
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
            channel: m.channel,
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
            is_converted: s.is_converted,
            last_active_at: s.last_active_at,
            channel: s.channel,
        })
        .collect();
    Ok(Json(ListSessionsResponse {
        user_id,
        sessions: entries,
    }))
}

/// Typed human_insights profile for the JWT user. The path `user_id` MUST
/// match the JWT's user_id; mismatch returns 403.
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
    let repo = HumanInsightRepo { pool: &state.pool };
    let row = repo.load(user_id).await?;
    Ok(Json(ProfileResponse::from_row(user_id, row)))
}

/// Typed `character_insights` profile for one relationship. The instance's
/// `owner_uid` MUST match the JWT's user_id; mismatch returns 403, and an
/// unknown or archived instance returns 404.
#[utoipa::path(
    get,
    path = "/comp/instance/{instance_id}/profile",
    tag = "companion",
    params(("instance_id" = Uuid, Path, description = "Persona instance id owned by the JWT user")),
    responses(
        (status = 200, body = CharacterProfileResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "instance is not owned by the JWT user"),
        (status = 404, description = "no such active instance")
    ),
    security(("bearer" = []))
)]
async fn get_character_profile(
    State(state): State<AppState>,
    Path(instance_id): Path<Uuid>,
    Extension(AuthUser(jwt_user)): Extension<AuthUser>,
) -> Result<Json<CharacterProfileResponse>, AppError> {
    // The path key is an instance, not a user, so ownership is read through
    // the instance rather than compared against the path.
    let gate = PersonaRepo { pool: &state.pool }
        .load_instance_gate(instance_id)
        .await?
        .ok_or_else(|| AppError::NotFound("no such instance".into()))?;
    if gate.owner_uid != jwt_user {
        return Err(AppError::Forbidden("not your data".into()));
    }

    let row = eros_engine_store::character_insight::CharacterInsightRepo { pool: &state.pool }
        .load(instance_id)
        .await?;
    Ok(Json(CharacterProfileResponse::from_row(instance_id, row)))
}

// ─── Router ─────────────────────────────────────────────────────────

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(start_chat))
        .routes(routes!(get_history))
        .routes(routes!(list_sessions))
        .routes(routes!(get_profile))
        .routes(routes!(get_character_profile))
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
            // Default tuning: at a fresh seed (tier 1, no counterpart penalty)
            // rule deltas land 1:1.
            affinity_tuning: eros_engine_core::affinity::AffinityTuning::default(),
            bind_addr: "127.0.0.1:0".into(),
            // Sweeper disabled in tests — unit tests don't spawn it
            // and the fields are just for AppState completeness.
            dreaming_tick: std::time::Duration::ZERO,
            dreaming_idle_threshold: std::time::Duration::from_secs(1800),
            dreaming_claim_stale_threshold: std::time::Duration::from_secs(600),
            // Voice ingestion ON in tests — matches the production default.
            dreaming_voice_disabled: false,
            openrouter_usage_hidden_keys: std::collections::HashSet::new(),
            // Snapshot sweeper disabled in tests — same rationale as dreaming.
            snapshot: crate::state::SnapshotConfig {
                disabled: true,
                cron: "0 0 23 * * *".into(),
                tz: chrono_tz::Asia::Singapore,
            },
            prompt_log_dir: None,
            world: crate::state::parse_world_config(None, None, None, None, None, None),
        },
        openrouter: Arc::new(eros_engine_llm::openrouter::OpenRouterClient::new(
            "stub".into(),
        )),
        embed: Arc::new(
            eros_engine_llm::embedding::EmbeddingRouter::from_config_with(
                &eros_engine_llm::model_config::ModelConfig::default(),
                |k| (k == "VOYAGE_API_KEY").then(|| "test-key".to_string()),
            )
            .expect("test embedding router: default config resolves to Voyage with a test key"),
        ),
        model_config: Arc::new(eros_engine_llm::model_config::ModelConfig::default()),
        output_regex: std::sync::Arc::new(Vec::new()),
        stream_slots: std::sync::Arc::new(crate::state::StreamSlots::default()),
        world_configured: false,
        stories_configured: false,
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

    /// Genome + active instance in one step, returning the instance id.
    /// Since 0040 `chat_sessions.instance_id` carries a real FK, so tests that
    /// only need "some valid instance" must persist one instead of fabricating
    /// a bare `Uuid::new_v4()`. The genome name is randomised so repeat calls
    /// never collide on `persona_instances UNIQUE(genome_id, owner_uid)`.
    pub(crate) async fn seed_persona_instance(pool: &PgPool, owner: Uuid) -> Uuid {
        let genome_id = seed_genome(pool, &format!("seed-{}", Uuid::new_v4())).await;
        seed_instance(pool, genome_id, owner).await
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

    // ─── Test 3b: force_new bypasses resume; default still resumes ──

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn start_force_new_creates_fresh_session_when_one_exists(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Echo").await;
        let state = test_state(pool.clone());
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        // 1. Plain start creates the first session.
        let body = serde_json::to_vec(&json!({ "genome_id": genome_id })).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/comp/chat/start")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, first) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK, "body={first}");
        assert_eq!(first["is_new"], true);
        let first_id = first["session_id"].as_str().unwrap().to_string();

        // 2. force_new: true must NOT resume — it always creates a fresh session.
        let body =
            serde_json::to_vec(&json!({ "genome_id": genome_id, "force_new": true })).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/comp/chat/start")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, second) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK, "body={second}");
        assert_eq!(second["is_new"], true);
        let second_id = second["session_id"].as_str().unwrap().to_string();
        assert_ne!(
            second_id, first_id,
            "force_new must create a session distinct from the resumable one"
        );

        // 3. A plain (non-force) start afterward resumes the LATEST session —
        //    the one force_new just created, bumped ahead by last_active_at —
        //    not a third fresh one.
        let body = serde_json::to_vec(&json!({ "genome_id": genome_id })).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/comp/chat/start")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, third) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK, "body={third}");
        assert_eq!(third["is_new"], false);
        assert_eq!(third["session_id"].as_str().unwrap(), second_id);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn start_force_new_on_voice_channel(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Nyx").await;
        let state = test_state(pool.clone());
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        // 1. Plain voice start creates the first voice session.
        let body =
            serde_json::to_vec(&json!({ "genome_id": genome_id, "channel": "voice" })).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/comp/chat/start")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, first) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK, "body={first}");
        assert_eq!(first["is_new"], true);
        let first_id = first["session_id"].as_str().unwrap().to_string();

        // 2. force_new on voice must create a fresh voice session, not resume.
        let body = serde_json::to_vec(
            &json!({ "genome_id": genome_id, "channel": "voice", "force_new": true }),
        )
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/comp/chat/start")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, second) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK, "body={second}");
        assert_eq!(second["is_new"], true);
        let second_id_str = second["session_id"].as_str().unwrap();
        assert_ne!(second_id_str, first_id);
        let second_id = Uuid::parse_str(second_id_str).unwrap();

        // The fresh session must land on the voice channel, not silently
        // reset to text.
        let channel: String =
            sqlx::query_scalar("SELECT channel FROM engine.chat_sessions WHERE id = $1")
                .bind(second_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(channel, "voice");
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

    // ─── Test 4b: GET /sessions exposes channel for text vs voice ───

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn get_sessions_exposes_channel_text_and_voice(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Nyx").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;

        // seed_session creates a plain (text-channel-default) session.
        let text_session = seed_session(&pool, user_id, instance_id).await;
        let voice_session: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, channel) \
             VALUES ($1, $2, 'voice') RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let state = test_state(pool.clone());
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let req = Request::builder()
            .uri(format!("/comp/chat/{user_id}/sessions"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let (status, body) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK, "got body: {body}");

        let sessions = body["sessions"].as_array().expect("sessions array");
        assert_eq!(sessions.len(), 2);

        let channel_of = |id: Uuid| -> Option<String> {
            sessions
                .iter()
                .find(|s| s["session_id"].as_str() == Some(id.to_string().as_str()))
                .and_then(|s| s["channel"].as_str())
                .map(str::to_string)
        };
        assert_eq!(channel_of(text_session), Some("text".to_string()));
        assert_eq!(channel_of(voice_session), Some("voice".to_string()));
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
            channel: None,
            force_new: None,
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
            channel: None,
            force_new: None,
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
            channel: None,
            force_new: None,
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
            channel: None,
            force_new: None,
        };
        let err = resolve_or_create_session(&state, user_id, &req)
            .await
            .expect_err("archived instance must 404");
        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
    }

    // ─── Test 5: ProfileResponse::from_row pins the column mapping ──
    //
    // Pure unit test (no DB): every Option<String> field gets a distinct
    // value (its own field name), so any accidental transposition between
    // `HumanInsightsRow` and `ProfileResponse` fails an assertion here.

    #[test]
    fn profile_response_from_row_maps_fields_by_name() {
        use eros_engine_store::human_insight::HumanInsightsRow;

        let user_id = Uuid::new_v4();
        let updated_at = Utc::now();
        let row = HumanInsightsRow {
            user_id,
            city: Some("city".into()),
            location: Some("location".into()),
            hometown: Some("hometown".into()),
            nationality: Some("nationality".into()),
            occupation: Some("occupation".into()),
            mbti_guess: Some("mbti_guess".into()),
            love_values: Some("love_values".into()),
            emotional_needs: Some("emotional_needs".into()),
            life_rhythm: Some("life_rhythm".into()),
            interests: vec!["interests_0".into(), "interests_1".into()],
            personality_traits: vec!["personality_traits_0".into()],
            preferred_gender: Some("preferred_gender".into()),
            age_min: Some(21),
            age_max: Some(35),
            deal_breakers: vec!["deal_breakers_0".into(), "deal_breakers_1".into()],
            education: Some("education".into()),
            family: Some("family".into()),
            relationship_history: Some("relationship_history".into()),
            social_pattern: Some("social_pattern".into()),
            future_plans: Some("future_plans".into()),
            finance_status: Some("finance_status".into()),
            updated_at,
        };

        let resp = ProfileResponse::from_row(user_id, Some(row.clone()));
        assert_eq!(resp.user_id, user_id);
        assert_eq!(resp.city.as_deref(), Some("city"));
        assert_eq!(resp.location.as_deref(), Some("location"));
        assert_eq!(resp.hometown.as_deref(), Some("hometown"));
        assert_eq!(resp.nationality.as_deref(), Some("nationality"));
        assert_eq!(resp.occupation.as_deref(), Some("occupation"));
        assert_eq!(resp.mbti_guess.as_deref(), Some("mbti_guess"));
        assert_eq!(resp.love_values.as_deref(), Some("love_values"));
        assert_eq!(resp.emotional_needs.as_deref(), Some("emotional_needs"));
        assert_eq!(resp.life_rhythm.as_deref(), Some("life_rhythm"));
        assert_eq!(resp.interests, vec!["interests_0", "interests_1"]);
        assert_eq!(resp.personality_traits, vec!["personality_traits_0"]);
        assert_eq!(resp.preferred_gender.as_deref(), Some("preferred_gender"));
        assert_eq!(resp.age_min, Some(21));
        assert_eq!(resp.age_max, Some(35));
        assert_eq!(
            resp.deal_breakers,
            vec!["deal_breakers_0", "deal_breakers_1"]
        );
        assert_eq!(resp.education.as_deref(), Some("education"));
        assert_eq!(resp.family.as_deref(), Some("family"));
        assert_eq!(
            resp.relationship_history.as_deref(),
            Some("relationship_history")
        );
        assert_eq!(resp.social_pattern.as_deref(), Some("social_pattern"));
        assert_eq!(resp.future_plans.as_deref(), Some("future_plans"));
        assert_eq!(resp.finance_status.as_deref(), Some("finance_status"));
        assert_eq!(resp.updated_at, Some(row.updated_at));

        // None row: every field falls back to its empty/None default.
        let empty = ProfileResponse::from_row(user_id, None);
        assert_eq!(empty.user_id, user_id);
        assert_eq!(empty.city, None);
        assert_eq!(empty.location, None);
        assert_eq!(empty.hometown, None);
        assert_eq!(empty.nationality, None);
        assert_eq!(empty.occupation, None);
        assert_eq!(empty.mbti_guess, None);
        assert_eq!(empty.love_values, None);
        assert_eq!(empty.emotional_needs, None);
        assert_eq!(empty.life_rhythm, None);
        assert_eq!(empty.interests, Vec::<String>::new());
        assert_eq!(empty.personality_traits, Vec::<String>::new());
        assert_eq!(empty.preferred_gender, None);
        assert_eq!(empty.age_min, None);
        assert_eq!(empty.age_max, None);
        assert_eq!(empty.deal_breakers, Vec::<String>::new());
        assert_eq!(empty.education, None);
        assert_eq!(empty.family, None);
        assert_eq!(empty.relationship_history, None);
        assert_eq!(empty.social_pattern, None);
        assert_eq!(empty.future_plans, None);
        assert_eq!(empty.finance_status, None);
        assert_eq!(empty.updated_at, None);
    }

    // ─── Test 6: GET /comp/instance/{instance_id}/profile ───────────

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn character_profile_returns_the_row_for_its_owner(pool: PgPool) {
        let owner = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Vita").await;
        let instance_id = seed_instance(&pool, genome_id, owner).await;
        eros_engine_store::character_insight::CharacterInsightRepo { pool: &pool }
            .apply_extraction(
                instance_id,
                &serde_json::json!({ "location": "公司", "likes": ["雨天"] }),
            )
            .await
            .unwrap();

        let state = test_state(pool.clone());
        let mut router = testutil::build_router(state);
        let req = Request::builder()
            .uri(format!("/comp/instance/{instance_id}/profile"))
            .header(
                "authorization",
                format!("Bearer {}", testutil::mint_test_jwt(owner)),
            )
            .body(Body::empty())
            .unwrap();
        let (status, body) = testutil::send_request(&mut router, req).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["location"], "公司");
        assert_eq!(body["likes"][0], "雨天");
        assert!(body["updated_at"].is_string());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn character_profile_403s_for_a_non_owner(pool: PgPool) {
        let owner = Uuid::new_v4();
        let intruder = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Vita").await;
        let instance_id = seed_instance(&pool, genome_id, owner).await;

        let state = test_state(pool.clone());
        let mut router = testutil::build_router(state);
        let req = Request::builder()
            .uri(format!("/comp/instance/{instance_id}/profile"))
            .header(
                "authorization",
                format!("Bearer {}", testutil::mint_test_jwt(intruder)),
            )
            .body(Body::empty())
            .unwrap();
        let (status, _) = testutil::send_request(&mut router, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn character_profile_404s_for_an_unknown_instance(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let state = test_state(pool.clone());
        let mut router = testutil::build_router(state);
        let req = Request::builder()
            .uri(format!("/comp/instance/{}/profile", Uuid::new_v4()))
            .header(
                "authorization",
                format!("Bearer {}", testutil::mint_test_jwt(user_id)),
            )
            .body(Body::empty())
            .unwrap();
        let (status, body) = testutil::send_request(&mut router, req).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body["error"], "not_found",
            "must be the handler's gate 404, not axum's unmatched-route 404"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn character_profile_is_all_null_before_the_first_extraction(pool: PgPool) {
        let owner = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Vita").await;
        let instance_id = seed_instance(&pool, genome_id, owner).await;

        let state = test_state(pool.clone());
        let mut router = testutil::build_router(state);
        let req = Request::builder()
            .uri(format!("/comp/instance/{instance_id}/profile"))
            .header(
                "authorization",
                format!("Bearer {}", testutil::mint_test_jwt(owner)),
            )
            .body(Body::empty())
            .unwrap();
        let (status, body) = testutil::send_request(&mut router, req).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body["location"].is_null());
        assert_eq!(body["likes"], serde_json::json!([]));
        assert!(body["updated_at"].is_null(), "no row yet ⇒ null stamp");
    }
}
