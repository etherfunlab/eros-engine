// SPDX-License-Identifier: AGPL-3.0-only
//! Pipeline module — shared helpers (`compute_signals_for_session`,
//! `log_openrouter_usage`) plus the chat / post-process / dreaming
//! submodules. The streaming chat entry point is `stream::run_stream`.

pub mod dreaming;
pub mod handlers;
pub mod post_process;
pub mod stream;
pub mod sync;

use uuid::Uuid;

use eros_engine_core::affinity::Affinity;
use eros_engine_core::types::ConversationSignals;

use crate::error::AppError;

/// Emit one structured `openrouter: call completed` log line per call.
/// Token / cost fields are best-effort parses off the opaque `usage`
/// JSON — missing fields silently drop out of the line. Called from
/// every codepath that owns the result of `OpenRouterClient::execute`
/// (chat, dreaming, post_process), keeping `docs/llm-audit.md`'s
/// "background paths emit usage only as tracing fields" claim honest.
pub(super) fn log_openrouter_usage(
    task: &str,
    session_id: Option<Uuid>,
    resp: &eros_engine_llm::openrouter::ChatResponse,
) {
    let usage_ref = resp.usage.as_ref();
    let prompt_tokens = usage_ref
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_u64());
    let completion_tokens = usage_ref
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_u64());
    let total_tokens = usage_ref
        .and_then(|u| u.get("total_tokens"))
        .and_then(|v| v.as_u64());
    let cost = usage_ref
        .and_then(|u| u.get("cost"))
        .and_then(|v| v.as_f64());
    tracing::info!(
        task = task,
        session = ?session_id,
        generation_id = ?resp.generation_id,
        model = ?resp.model,
        prompt_tokens = ?prompt_tokens,
        completion_tokens = ?completion_tokens,
        total_tokens = ?total_tokens,
        cost = ?cost,
        "openrouter: call completed"
    );
}

pub async fn compute_signals_for_session(
    pool: &sqlx::PgPool,
    session_id: Uuid,
    affinity: &Affinity,
) -> Result<ConversationSignals, AppError> {
    let message_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM engine.chat_messages WHERE session_id = $1 AND role = 'user'",
    )
    .bind(session_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    let last_time: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT MAX(sent_at) FROM engine.chat_messages WHERE session_id = $1 AND role = 'user'",
    )
    .bind(session_id)
    .fetch_optional(pool)
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
