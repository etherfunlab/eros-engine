// SPDX-License-Identifier: AGPL-3.0-only
//! Pipeline module — shared helpers (`compute_signals_for_session`,
//! `log_openrouter_usage`) plus the chat / post-process / dreaming
//! submodules. The streaming chat entry point is `stream::run_stream`.

pub mod dreaming;
pub mod handlers;
pub mod post_process;
pub mod snapshot;
pub mod stream;
pub mod voice;

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
    // One aggregate roundtrip instead of two separate scans of the same rows.
    // Fail-open matches the previous per-query behavior: any error collapses to
    // count 0 / last_time None (→ 0.0 hours). `COUNT(*)` is never NULL;
    // `MAX(sent_at)` is NULL for an empty session.
    let (message_count, last_time): (i64, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT COUNT(*), MAX(sent_at) FROM engine.chat_messages \
             WHERE session_id = $1 AND role IN ('user', 'gift_user')",
    )
    .bind(session_id)
    .fetch_one(pool)
    .await
    .unwrap_or((0, None));

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

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;
    use uuid::Uuid;

    fn fixture_affinity(session_id: Uuid, user_id: Uuid, instance_id: Uuid) -> Affinity {
        let now = chrono::Utc::now();
        Affinity {
            id: Uuid::new_v4(),
            session_id,
            user_id,
            instance_id,
            warmth: 0.3,
            trust: 0.2,
            intrigue: 0.5,
            intimacy: 0.0,
            patience: 0.5,
            tension: 0.1,
            ghost_streak: 0,
            last_ghost_at: None,
            total_ghosts: 0,
            relationship_label: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Seed a minimal persona_genome → persona_instance → chat_session chain so
    /// the chat_messages FK is satisfied. Returns (genome_id, instance_id, session_id).
    async fn seed_session(pool: &PgPool, user_id: Uuid) -> (Uuid, Uuid, Uuid) {
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('SignalsTest', 'sp', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id)
        .bind(user_id)
        .fetch_one(pool)
        .await
        .unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(pool)
        .await
        .unwrap();
        (genome_id, instance_id, session_id)
    }

    /// `user` turns and tip `gift_user` rows both count toward the conversation
    /// signal. gift_user is tip-only now (the legacy in-app Gift Event endpoint
    /// was removed), so there is no longer a legacy `gift_user` row to exclude.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn signals_count_includes_gift_user_tip_rows(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_genome_id, instance_id, session_id) = seed_session(&pool, user_id).await;

        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, metadata) VALUES \
                 ($1, 'user', 'hi', NULL), \
                 ($1, 'gift_user', '(打赏 $20)', '{\"tips_amount_usd\": 20.0}'::jsonb)",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        let aff = fixture_affinity(session_id, user_id, instance_id);
        let signals = compute_signals_for_session(&pool, session_id, &aff)
            .await
            .unwrap();

        assert_eq!(
            signals.message_count, 2,
            "user + gift_user (tip) both count"
        );
    }

    /// Pure `user` rows still count normally (baseline regression).
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn signals_count_user_only_rows(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_genome_id, _instance_id, session_id) = seed_session(&pool, user_id).await;

        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content) \
             VALUES ($1, 'user', 'hello'), ($1, 'user', 'world')",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        let aff = fixture_affinity(session_id, user_id, _instance_id);
        let signals = compute_signals_for_session(&pool, session_id, &aff)
            .await
            .unwrap();

        assert_eq!(signals.message_count, 2, "two user rows must yield count 2");
    }

    /// Characterization: `hours_since_last_message` derives from the MAX(sent_at)
    /// of the user/gift_user rows. Pins the MAX half of the signal before COUNT
    /// and MAX are merged into a single aggregate query (the COUNT half is
    /// already covered by `signals_count_*`).
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn signals_hours_since_reflects_latest_user_row(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_genome_id, instance_id, session_id) = seed_session(&pool, user_id).await;

        // Two user rows; MAX(sent_at) = the newer one (~1h ago).
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) VALUES \
                 ($1, 'user', 'older', now() - interval '3 hours'), \
                 ($1, 'user', 'newer', now() - interval '1 hour')",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        let aff = fixture_affinity(session_id, user_id, instance_id);
        let signals = compute_signals_for_session(&pool, session_id, &aff)
            .await
            .unwrap();

        assert_eq!(signals.message_count, 2);
        assert!(
            (signals.hours_since_last_message - 1.0).abs() < 0.1,
            "hours_since_last_message must track MAX(sent_at) (~1.0), got {}",
            signals.hours_since_last_message
        );
    }

    /// Characterization: an empty session yields count 0 and 0.0 hours (the
    /// `MAX(sent_at) IS NULL` / no-rows fail-open path). Pins the empty-session
    /// behavior before the COUNT+MAX merge.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn signals_empty_session_zero_count_zero_hours(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_genome_id, instance_id, session_id) = seed_session(&pool, user_id).await;

        let aff = fixture_affinity(session_id, user_id, instance_id);
        let signals = compute_signals_for_session(&pool, session_id, &aff)
            .await
            .unwrap();

        assert_eq!(signals.message_count, 0);
        assert_eq!(signals.hours_since_last_message, 0.0);
    }
}
