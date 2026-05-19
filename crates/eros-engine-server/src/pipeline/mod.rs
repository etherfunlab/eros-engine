// SPDX-License-Identifier: AGPL-3.0-only
//! Pipeline orchestrator — pre-process → PDE → handler dispatch → chat
//! exec → save assistant message → spawn post-process.
//!
//! Ported from `eros-gateway/src/engine/main.rs` with these OSS changes:
//!
//! - All DB I/O routed through `eros-engine-store` repos.
//! - Ghost-streak reset on Reply / Proactive / GiftReaction is performed
//!   here before persistence; the store crate's `persist_with_event`
//!   intentionally does not touch the streak.
//! - Ghost path calls `AffinityRepo::record_ghost` rather than going
//!   through `persist_with_event`.
//! - `state.chat_engine` → `state.openrouter`.

pub mod dreaming;
pub mod handlers;
pub mod post_process;
pub mod sync;

use uuid::Uuid;

use eros_engine_core::affinity::Affinity;
use eros_engine_core::types::{
    ActionType, ChatResponse, ConversationSignals, DecisionInput, Event,
};
use eros_engine_core::{pde, persona::CompanionPersona};
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::ChatRepo;
use eros_engine_store::persona::PersonaRepo;

use crate::error::AppError;
use crate::state::AppState;
use handlers::{ActionHandler, GhostHandler, GiftHandler, ProactiveHandler, ReplyHandler};

/// Primary entry point for the companion engine.
pub async fn run(
    state: &AppState,
    session_id: Uuid,
    event: Event,
) -> Result<Option<ChatResponse>, AppError> {
    // 1. Resolve user + persona instance for the session.
    let (user_id, instance_id) = load_session_ids(state, session_id).await?;

    // 2. Persona.
    let persona_repo = PersonaRepo { pool: &state.pool };
    let persona: CompanionPersona = persona_repo
        .load_companion(instance_id)
        .await?
        .ok_or_else(|| AppError::NotFound("persona instance not found".into()))?;

    // 3. Affinity (with time decay).
    let affinity = load_affinity_with_decay(state, session_id, user_id, instance_id).await?;

    // 4. Conversation signals.
    let signals = compute_signals(state, session_id, &affinity).await?;

    // 5. Build decision input.
    let input = DecisionInput {
        event: event.clone(),
        affinity,
        persona,
        signals,
    };

    // 6. PDE decision (rules only for now). Ghost evaluation happens
    // internally inside `pde::decide` via `eros_engine_core::ghost`.
    let plan = pde::decide(&input);

    tracing::info!(
        "engine: session={session_id} action={:?} style={:?}",
        plan.action_type,
        plan.reply_style,
    );

    if let Event::UserMessage {
        prompt_traits,
        audit,
        ..
    } = &event
    {
        if !prompt_traits.is_empty() {
            let tags: Vec<&str> = prompt_traits.iter().map(|t| t.tag.as_str()).collect();
            tracing::info!(
                session = %session_id,
                traits_count = prompt_traits.len(),
                trait_tags = ?tags,
                "engine: prompt_traits applied"
            );
        }
        if let Some(a) = audit {
            let metadata_keys: Vec<&str> = a
                .metadata
                .as_ref()
                .map(|m| m.keys().map(String::as_str).collect())
                .unwrap_or_default();
            tracing::info!(
                session = %session_id,
                audit_user_present = a.user.is_some(),
                audit_session_present = a.session_id.is_some(),
                audit_metadata_keys = ?metadata_keys,
                "engine: llm audit applied"
            );
        }
    }

    // 7. Dispatch to handler. The Gift branch passes `plan.affinity_deltas`
    // through; T11 will replace this with deltas supplied directly by the
    // `/comp/chat/{id}/event/gift` route's request body, since the OSS
    // engine has no credit ledger to look them up from.
    let chat_req = match plan.action_type {
        ActionType::Reply => {
            ReplyHandler {
                state,
                session_id,
                user_id,
                instance_id,
            }
            .handle(&input, &plan)
            .await?
        }
        ActionType::Ghost => {
            GhostHandler { state, session_id }
                .handle(&input, &plan)
                .await?
        }
        ActionType::Proactive => {
            ProactiveHandler { state, session_id }
                .handle(&input, &plan)
                .await?
        }
        ActionType::GiftReaction => {
            GiftHandler {
                state,
                session_id,
                user_id,
                instance_id,
                deltas: plan.affinity_deltas.clone(),
                pending: vec![], // TODO(T11): inject from request body
            }
            .handle(&input, &plan)
            .await?
        }
    };

    // 8. Execute chat stage only if a handler produced a request.
    // Convert the LLM crate's ChatResponse → core's ChatResponse so the
    // engine's public API stays decoupled from the openrouter wire shape.
    let response: Option<ChatResponse> = match chat_req {
        Some(req) => {
            let llm_resp = state
                .openrouter
                .execute(req)
                .await
                .map_err(|e| AppError::Internal(format!("openrouter: {e}")))?;

            // Tracing-only usage line. Covers sync, async (background
            // tokio::spawn calls pipeline::run too), dreaming, post_process
            // — every codepath that reaches this point in the orchestrator.
            // Token / cost fields are best-effort parses off the opaque
            // usage JSON; missing fields silently drop out of the log line.
            let usage_ref = llm_resp.usage.as_ref();
            let prompt_tokens =
                usage_ref.and_then(|u| u.get("prompt_tokens")).and_then(|v| v.as_u64());
            let completion_tokens = usage_ref
                .and_then(|u| u.get("completion_tokens"))
                .and_then(|v| v.as_u64());
            let total_tokens =
                usage_ref.and_then(|u| u.get("total_tokens")).and_then(|v| v.as_u64());
            let cost = usage_ref.and_then(|u| u.get("cost")).and_then(|v| v.as_f64());
            tracing::info!(
                session = %session_id,
                generation_id = ?llm_resp.generation_id,
                model = ?llm_resp.model,
                prompt_tokens = ?prompt_tokens,
                completion_tokens = ?completion_tokens,
                total_tokens = ?total_tokens,
                cost = ?cost,
                "openrouter: call completed"
            );

            let chat_repo = ChatRepo { pool: &state.pool };
            chat_repo
                .append_message(session_id, "assistant", &llm_resp.reply)
                .await?;
            Some(ChatResponse {
                reply: llm_resp.reply,
                generation_id: llm_resp.generation_id,
                model: llm_resp.model,
                usage: llm_resp.usage,
            })
        }
        None => None,
    };

    // 9. Reset ghost streak — caller responsibility per store crate split.
    // The store's `persist_with_event` intentionally doesn't touch the
    // streak; the policy of "any non-Ghost action clears the streak" lives
    // here in the pipeline. Ghost increments are handled by `record_ghost`
    // inside post_process.
    if !matches!(plan.action_type, ActionType::Ghost) {
        if let Err(e) = clear_ghost_streak(state, session_id).await {
            tracing::warn!("ghost streak reset failed: {e}");
        }
    }

    // 10. Spawn post-process. AppState is `Clone`; the openrouter / voyage
    // / model_config inside it are `Arc`-wrapped so the clone is cheap and
    // the spawned future is `'static` (no `&'a` borrows leak in).
    let state_bg = state.clone();
    let plan_bg = plan.clone();
    let response_bg = response.clone();
    let event_bg = event;
    tokio::spawn(async move {
        post_process::run(
            state_bg,
            session_id,
            user_id,
            instance_id,
            event_bg,
            plan_bg,
            response_bg,
        )
        .await;
    });

    Ok(response)
}

async fn load_session_ids(state: &AppState, session_id: Uuid) -> Result<(Uuid, Uuid), AppError> {
    let chat_repo = ChatRepo { pool: &state.pool };
    let session = chat_repo
        .get_session(session_id)
        .await?
        .ok_or_else(|| AppError::NotFound("session not found".into()))?;
    let instance_id = session
        .instance_id
        .ok_or_else(|| AppError::Internal("session has no instance".into()))?;
    Ok((session.user_id, instance_id))
}

async fn load_affinity_with_decay(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
) -> Result<Affinity, AppError> {
    let repo = AffinityRepo { pool: &state.pool };
    let mut affinity = repo
        .load_or_create(session_id, user_id, instance_id)
        .await?;
    affinity.apply_time_decay();
    Ok(affinity)
}

async fn compute_signals(
    state: &AppState,
    session_id: Uuid,
    affinity: &Affinity,
) -> Result<ConversationSignals, AppError> {
    let message_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM engine.chat_messages WHERE session_id = $1 AND role = 'user'",
    )
    .bind(session_id)
    .fetch_one(&state.pool)
    .await
    .unwrap_or(0);

    let last_time: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT MAX(sent_at) FROM engine.chat_messages WHERE session_id = $1 AND role = 'user'",
    )
    .bind(session_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();

    let hours_since_last_message = last_time
        .map(|t| (chrono::Utc::now() - t).num_minutes() as f64 / 60.0)
        .unwrap_or(0.0);

    let hours_since_last_ghost = affinity
        .last_ghost_at
        .map(|t| (chrono::Utc::now() - t).num_minutes() as f64 / 60.0);

    Ok(ConversationSignals {
        message_count,
        hours_since_last_message,
        ghost_streak: affinity.ghost_streak,
        hours_since_last_ghost,
    })
}

/// Reset `ghost_streak` to 0 for the affinity row tied to this session.
/// Idempotent: the WHERE clause skips the UPDATE when streak is already 0
/// so the unconditional call from `pipeline::run` doesn't spam writes.
async fn clear_ghost_streak(state: &AppState, session_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE engine.companion_affinity \
         SET ghost_streak = 0, updated_at = now() \
         WHERE session_id = $1 AND ghost_streak <> 0",
    )
    .bind(session_id)
    .execute(&state.pool)
    .await?;
    Ok(())
}
