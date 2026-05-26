// SPDX-License-Identifier: AGPL-3.0-only
//! Chat session + message persistence.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChatSession {
    pub id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Option<Uuid>,
    pub lead_score: f64,
    pub is_converted: bool,
    pub last_active_at: DateTime<Utc>,
    pub metadata: serde_json::Value,
    /// Set by the dreaming-lite sweeper after a classification pass.
    /// `None` means the session is still eligible for the next sweep tick.
    pub classified_at: Option<DateTime<Utc>>,
    /// Set by the dreaming-lite picker when it claims a session for
    /// processing — the claim sentinel that makes multi-instance
    /// sweepers safe via `FOR UPDATE SKIP LOCKED`. A non-NULL value
    /// older than `DREAMING_CLAIM_STALE_SECS` is treated as a crashed
    /// worker and re-claimable. Cleared implicitly by `classified_at`
    /// being set on a successful pass.
    pub classification_claimed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChatMessage {
    pub id: Uuid,
    pub session_id: Uuid,
    pub role: String,
    pub content: String,
    pub sent_at: DateTime<Utc>,

    // Streaming + idempotency metadata (added in migration 0012).
    #[serde(default)]
    pub client_msg_id: Option<String>,
    #[serde(default)]
    pub ghost_decision: bool,
    #[serde(default)]
    pub user_message_id: Option<Uuid>,
    #[serde(default)]
    pub continues_from_message_id: Option<Uuid>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub usage: Option<serde_json::Value>,
    #[serde(default)]
    pub generation_id: Option<String>,
    #[serde(default)]
    pub assistant_action_type: Option<String>,
}

/// Projection-narrowed `ChatMessage` for BFF / UI-rendering paths that
/// don't need `extracted_facts`, idempotency keys, or SSE metadata.
/// Carries only the columns a chat-history viewer renders.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ChatMessageSlim {
    pub id: Uuid,
    pub role: String,
    pub content: String,
    pub sent_at: DateTime<Utc>,
    /// Client-supplied message id forwarded during streaming (idempotency
    /// key). NULL for rows that never carried one (e.g. assistant turns).
    pub client_msg_id: Option<String>,
    /// Structured tip amount extracted from `metadata->>'tips_amount_usd'`.
    /// Present on `role='gift_user'` rows that carry tip metadata; NULL on
    /// all other rows. Lets BFF / FE render tips as a structured field
    /// instead of parsing the `(打赏 $X)` content marker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tips_amount_usd: Option<f64>,
}

pub struct ChatRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> ChatRepo<'a> {
    /// Create a new chat session for `user_id` × `instance_id`.
    pub async fn create_session(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
    ) -> Result<ChatSession, sqlx::Error> {
        self.create_session_with_metadata(user_id, instance_id, serde_json::json!({}))
            .await
    }

    /// Create a session and seed `metadata` as the JSONB column. Used by
    /// callers that need session-scoped flags (e.g. `is_demo`) the
    /// pipeline reads later.
    pub async fn create_session_with_metadata(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
        metadata: serde_json::Value,
    ) -> Result<ChatSession, sqlx::Error> {
        sqlx::query_as::<_, ChatSession>(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, metadata) \
             VALUES ($1, $2, $3) \
             RETURNING *",
        )
        .bind(user_id)
        .bind(instance_id)
        .bind(metadata)
        .fetch_one(self.pool)
        .await
    }

    /// Look up a session by id.
    pub async fn get_session(&self, session_id: Uuid) -> Result<Option<ChatSession>, sqlx::Error> {
        sqlx::query_as::<_, ChatSession>("SELECT * FROM engine.chat_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(self.pool)
            .await
    }

    /// Resume the most recent session for a user×instance pair, or create a new one.
    pub async fn create_or_resume(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
    ) -> Result<ChatSession, sqlx::Error> {
        if let Some(existing) = sqlx::query_as::<_, ChatSession>(
            "SELECT * FROM engine.chat_sessions \
             WHERE user_id = $1 AND instance_id = $2 \
             ORDER BY last_active_at DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_optional(self.pool)
        .await?
        {
            sqlx::query("UPDATE engine.chat_sessions SET last_active_at = now() WHERE id = $1")
                .bind(existing.id)
                .execute(self.pool)
                .await?;
            return Ok(existing);
        }
        self.create_session(user_id, instance_id).await
    }

    /// Resume the most-recent session for `(user_id, instance_id)`, bumping
    /// `last_active_at` in the same statement. Returns `None` when none
    /// exists (caller then creates). Folds the former SELECT-latest +
    /// separate UPDATE into one round-trip. Callers consume only `id`
    /// (immutable), so returning the post-bump row is immaterial.
    pub async fn resume_latest_session(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
    ) -> Result<Option<ChatSession>, sqlx::Error> {
        sqlx::query_as::<_, ChatSession>(
            "UPDATE engine.chat_sessions SET last_active_at = now() \
             WHERE id = ( \
                 SELECT id FROM engine.chat_sessions \
                 WHERE user_id = $1 AND instance_id = $2 \
                 ORDER BY last_active_at DESC \
                 LIMIT 1 \
             ) \
             RETURNING *",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_optional(self.pool)
        .await
    }

    /// Append a message to a session and bump `last_active_at`.
    pub async fn append_message(
        &self,
        session_id: Uuid,
        role: &str,
        content: &str,
    ) -> Result<Uuid, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(session_id)
        .bind(role)
        .bind(content)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("UPDATE engine.chat_sessions SET last_active_at = now() WHERE id = $1")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Fetch chat history in chronological (ascending) order.
    pub async fn history(
        &self,
        session_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ChatMessage>, sqlx::Error> {
        // We pull DESC + LIMIT/OFFSET (most recent N messages, paged
        // backwards), then reverse to ASC for the caller. This matches the
        // gateway's `get_history` semantics.
        let mut rows = sqlx::query_as::<_, ChatMessage>(
            "SELECT * FROM engine.chat_messages \
             WHERE session_id = $1 \
             ORDER BY sent_at DESC \
             LIMIT $2 OFFSET $3",
        )
        .bind(session_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(self.pool)
        .await?;
        rows.reverse();
        Ok(rows)
    }

    /// Projection-narrowed read used by BFF endpoints (and any caller that
    /// doesn't need `extracted_facts` / idempotency / SSE metadata). Same
    /// DESC+reverse trick as `history()` so the result is chronological.
    /// Uses the existing `(session_id, sent_at DESC)` index — no migration.
    pub async fn history_slim(
        &self,
        session_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ChatMessageSlim>, sqlx::Error> {
        let mut rows = sqlx::query_as::<_, ChatMessageSlim>(
            "SELECT id, role, content, sent_at, client_msg_id, \
                    (metadata->>'tips_amount_usd')::float8 AS tips_amount_usd \
             FROM engine.chat_messages \
             WHERE session_id = $1 \
             ORDER BY sent_at DESC \
             LIMIT $2 OFFSET $3",
        )
        .bind(session_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(self.pool)
        .await?;
        rows.reverse();
        Ok(rows)
    }

    /// All sessions belonging to a user, most-recently-active first.
    pub async fn list_sessions(&self, user_id: Uuid) -> Result<Vec<ChatSession>, sqlx::Error> {
        sqlx::query_as::<_, ChatSession>(
            "SELECT * FROM engine.chat_sessions \
             WHERE user_id = $1 \
             ORDER BY last_active_at DESC",
        )
        .bind(user_id)
        .fetch_all(self.pool)
        .await
    }
}

/// Audit metadata for a filtered-success assistant row. Threaded through
/// `AssistantInsert::filter_audit` and bound into the five `chat_messages`
/// audit columns. `None` ⇒ all five columns are NULL. See
/// docs/superpowers/specs/2026-05-26-tip-role-and-filter-audit-design.md §4.
#[derive(Debug, Clone)]
pub struct FilterAudit {
    pub pre_filter_content: String,
    pub filter_model: String,
    pub filter_triggers: serde_json::Value,
    pub f_client_msg_id: String,
    pub f_generation_id: Option<String>,
}

/// One assistant row to insert in a burst.
#[derive(Debug, Clone)]
pub struct AssistantInsert {
    pub id: Uuid,
    pub content: String,
    pub assistant_action_type: String, // "reply" | "gift_reaction"
    pub continues_from_message_id: Option<Uuid>,
    pub truncated: bool,
    pub model: Option<String>,
    pub usage: Option<serde_json::Value>,
    pub generation_id: Option<String>,
    pub filter_audit: Option<FilterAudit>,
}

/// Outcome of `upsert_user_message_idempotent`. The application uses this
/// to decide between normal processing, replay, and 409.
#[derive(Debug)]
pub enum UpsertUserOutcome {
    /// First time seeing `(session_id, client_msg_id)`.
    Inserted { message_id: Uuid },
    /// Original request completed. Caller should synthesise SSE frames from
    /// the persisted rows (assistant_chain may be empty for a ghost outcome).
    Replay {
        user_message_id: Uuid,
        ghost: bool,
        assistant_chain: Vec<ChatMessage>,
    },
    /// Same key seen, but no assistant row and no ghost flag — the original
    /// request is still in flight. Caller should return HTTP 409.
    DuplicateInProgress { user_message_id: Uuid },
}

impl<'a> ChatRepo<'a> {
    /// Insert a user message keyed by `client_msg_id` with permanent
    /// idempotency. The partial unique index on `(session_id, client_msg_id)`
    /// has no time component, so deduplication is permanent: any prior row
    /// with the same key is a replay candidate. A future janitor can GC old
    /// rows, but the application treats any prior `(session_id, client_msg_id)`
    /// row as authoritative. Resolves the outcome under one short-lived
    /// transaction so the dedup decision and write happen against a consistent
    /// snapshot.
    pub async fn upsert_user_message_idempotent(
        &self,
        session_id: Uuid,
        content: &str,
        client_msg_id: &str,
        role: &str,
        metadata: Option<&serde_json::Value>,
    ) -> Result<UpsertUserOutcome, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        // Widened role filter: tip path writes 'gift_user', and idempotency is
        // keyed on (session_id, client_msg_id) regardless of which user-side
        // role was originally persisted.
        let existing: Option<ChatMessage> = sqlx::query_as::<_, ChatMessage>(
            "SELECT * FROM engine.chat_messages \
             WHERE session_id = $1 AND client_msg_id = $2 \
               AND role IN ('user', 'gift_user') \
             LIMIT 1",
        )
        .bind(session_id)
        .bind(client_msg_id)
        .fetch_optional(&mut *tx)
        .await?;

        if let Some(row) = existing {
            let assistant_chain: Vec<ChatMessage> = sqlx::query_as::<_, ChatMessage>(
                "SELECT * FROM engine.chat_messages \
                 WHERE user_message_id = $1 AND role = 'assistant' \
                 ORDER BY sent_at ASC",
            )
            .bind(row.id)
            .fetch_all(&mut *tx)
            .await?;

            tx.commit().await?;

            return Ok(if !assistant_chain.is_empty() {
                UpsertUserOutcome::Replay {
                    user_message_id: row.id,
                    ghost: false,
                    assistant_chain,
                }
            } else if row.ghost_decision {
                UpsertUserOutcome::Replay {
                    user_message_id: row.id,
                    ghost: true,
                    assistant_chain: vec![],
                }
            } else {
                UpsertUserOutcome::DuplicateInProgress {
                    user_message_id: row.id,
                }
            });
        }

        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages \
                 (session_id, role, content, client_msg_id, metadata) \
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(session_id)
        .bind(role)
        .bind(content)
        .bind(client_msg_id)
        .bind(metadata)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("UPDATE engine.chat_sessions SET last_active_at = now() WHERE id = $1")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(UpsertUserOutcome::Inserted { message_id: id })
    }

    /// Mark a user message as having received a `ghost` decision from the
    /// pipeline. Idempotent — re-marking is a no-op.
    pub async fn mark_user_message_ghosted(
        &self,
        user_message_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.chat_messages SET ghost_decision = true \
             WHERE id = $1 AND role = 'user' AND ghost_decision = false",
        )
        .bind(user_message_id)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Persist a burst of assistant messages keyed back to the driving user
    /// message. Caller picks the ULID-shaped `id` so the streamed `meta.message_id`
    /// matches the DB row. Bumps `last_active_at` once at the end.
    pub async fn insert_assistant_batch(
        &self,
        session_id: Uuid,
        user_message_id: Uuid,
        rows: &[AssistantInsert],
    ) -> Result<(), sqlx::Error> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for row in rows {
            let (pre_filter, filter_model, filter_triggers, f_client_msg_id, f_generation_id) =
                match &row.filter_audit {
                    Some(a) => (
                        Some(a.pre_filter_content.as_str()),
                        Some(a.filter_model.as_str()),
                        Some(&a.filter_triggers),
                        Some(a.f_client_msg_id.as_str()),
                        a.f_generation_id.as_deref(),
                    ),
                    None => (None, None, None, None, None),
                };
            sqlx::query(
                "INSERT INTO engine.chat_messages \
                   (id, session_id, role, content, user_message_id, \
                    continues_from_message_id, truncated, model, usage, generation_id, \
                    assistant_action_type, \
                    pre_filter_content, filter_model, filter_triggers, \
                    f_client_msg_id, f_generation_id) \
                 VALUES ($1, $2, 'assistant', $3, $4, $5, $6, $7, $8, $9, $10, \
                         $11, $12, $13, $14, $15)",
            )
            .bind(row.id)
            .bind(session_id)
            .bind(&row.content)
            .bind(user_message_id)
            .bind(row.continues_from_message_id)
            .bind(row.truncated)
            .bind(&row.model)
            .bind(&row.usage)
            .bind(&row.generation_id)
            .bind(&row.assistant_action_type)
            .bind(pre_filter)
            .bind(filter_model)
            .bind(filter_triggers)
            .bind(f_client_msg_id)
            .bind(f_generation_id)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("UPDATE engine.chat_sessions SET last_active_at = now() WHERE id = $1")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "./migrations")]
    async fn create_then_retrieve_session(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let s = repo.create_session(user_id, instance_id).await.unwrap();
        let loaded = repo.get_session(s.id).await.unwrap().unwrap();
        assert_eq!(loaded.user_id, user_id);
        assert_eq!(loaded.instance_id, Some(instance_id));
        assert_eq!(loaded.lead_score, 0.0);
        assert!(!loaded.is_converted);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn append_message_and_history_roundtrip(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let s = repo.create_session(user_id, instance_id).await.unwrap();

        repo.append_message(s.id, "user", "hello").await.unwrap();
        repo.append_message(s.id, "assistant", "hi there")
            .await
            .unwrap();
        repo.append_message(s.id, "user", "how are you?")
            .await
            .unwrap();

        let history = repo.history(s.id, 50, 0).await.unwrap();
        assert_eq!(history.len(), 3);
        // Chronological: first appended first.
        assert_eq!(history[0].role, "user");
        assert_eq!(history[0].content, "hello");
        assert_eq!(history[1].role, "assistant");
        assert_eq!(history[2].content, "how are you?");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn history_slim_returns_role_content_sent_at_in_order(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let s = repo.create_session(user_id, instance_id).await.unwrap();

        repo.append_message(s.id, "user", "alpha").await.unwrap();
        repo.append_message(s.id, "assistant", "beta")
            .await
            .unwrap();
        repo.append_message(s.id, "user", "gamma").await.unwrap();

        let slim = repo.history_slim(s.id, 50, 0).await.unwrap();
        assert_eq!(slim.len(), 3);
        // Chronological order: oldest first (matches history()).
        assert_eq!(slim[0].role, "user");
        assert_eq!(slim[0].content, "alpha");
        assert_eq!(slim[1].role, "assistant");
        assert_eq!(slim[1].content, "beta");
        assert_eq!(slim[2].role, "user");
        assert_eq!(slim[2].content, "gamma");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn history_slim_respects_limit_and_offset(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let s = repo.create_session(user_id, instance_id).await.unwrap();
        for n in 0..5 {
            repo.append_message(s.id, "user", &format!("m{n}"))
                .await
                .unwrap();
        }

        // Most-recent 2, reversed to ASC — should be ["m3", "m4"].
        let page = repo.history_slim(s.id, 2, 0).await.unwrap();
        assert_eq!(
            page.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            vec!["m3", "m4"]
        );

        // offset=2 → next-most-recent 2, reversed — should be ["m1", "m2"].
        let page = repo.history_slim(s.id, 2, 2).await.unwrap();
        assert_eq!(
            page.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            vec!["m1", "m2"]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn list_sessions_for_user(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let other_user = Uuid::new_v4();

        let i1 = Uuid::new_v4();
        let i2 = Uuid::new_v4();
        let i3 = Uuid::new_v4();

        repo.create_session(user_id, i1).await.unwrap();
        repo.create_session(user_id, i2).await.unwrap();
        repo.create_session(other_user, i3).await.unwrap();

        let sessions = repo.list_sessions(user_id).await.unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().all(|s| s.user_id == user_id));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn create_or_resume_returns_existing(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let first = repo.create_session(user_id, instance_id).await.unwrap();
        let resumed = repo.create_or_resume(user_id, instance_id).await.unwrap();
        assert_eq!(first.id, resumed.id);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_user_message_idempotent_first_insert(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let s = repo.create_session(user_id, instance_id).await.unwrap();

        let outcome = repo
            .upsert_user_message_idempotent(s.id, "hello", "01J0000000000000000000000A", "user", None)
            .await
            .unwrap();
        match outcome {
            UpsertUserOutcome::Inserted { message_id } => {
                assert_ne!(message_id, Uuid::nil());
            }
            other => panic!("expected Inserted, got {other:?}"),
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_user_message_idempotent_replay_after_done(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let s = repo.create_session(user_id, instance_id).await.unwrap();

        let first = match repo
            .upsert_user_message_idempotent(s.id, "hello", "01J0000000000000000000000A", "user", None)
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            o => panic!("expected Inserted, got {o:?}"),
        };

        repo.insert_assistant_batch(
            s.id,
            first,
            &[AssistantInsert {
                id: Uuid::new_v4(),
                content: "hi back".into(),
                assistant_action_type: "reply".into(),
                continues_from_message_id: None,
                truncated: false,
                model: Some("x-ai/grok-4-fast".into()),
                usage: Some(
                    serde_json::json!({"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}),
                ),
                generation_id: Some("gen-1".into()),
                filter_audit: None,
            }],
        )
        .await
        .unwrap();

        let outcome = repo
            .upsert_user_message_idempotent(s.id, "hello", "01J0000000000000000000000A", "user", None)
            .await
            .unwrap();
        match outcome {
            UpsertUserOutcome::Replay {
                user_message_id,
                ghost,
                assistant_chain,
            } => {
                assert_eq!(user_message_id, first);
                assert!(!ghost);
                assert_eq!(assistant_chain.len(), 1);
                assert_eq!(assistant_chain[0].content, "hi back");
            }
            other => panic!("expected Replay, got {other:?}"),
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_user_message_idempotent_409_when_no_assistant_and_not_ghost(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let s = repo.create_session(user_id, instance_id).await.unwrap();

        let first = match repo
            .upsert_user_message_idempotent(s.id, "hello", "01J0000000000000000000000A", "user", None)
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            o => panic!("expected Inserted, got {o:?}"),
        };

        match repo
            .upsert_user_message_idempotent(s.id, "hello", "01J0000000000000000000000A", "user", None)
            .await
            .unwrap()
        {
            UpsertUserOutcome::DuplicateInProgress { user_message_id } => {
                assert_eq!(user_message_id, first);
            }
            other => panic!("expected DuplicateInProgress, got {other:?}"),
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_user_message_idempotent_replay_when_ghost(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let s = repo.create_session(user_id, instance_id).await.unwrap();

        let first = match repo
            .upsert_user_message_idempotent(s.id, "hello", "01J0000000000000000000000A", "user", None)
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            o => panic!("expected Inserted, got {o:?}"),
        };
        repo.mark_user_message_ghosted(first).await.unwrap();

        match repo
            .upsert_user_message_idempotent(s.id, "hello", "01J0000000000000000000000A", "user", None)
            .await
            .unwrap()
        {
            UpsertUserOutcome::Replay {
                ghost,
                assistant_chain,
                ..
            } => {
                assert!(ghost);
                assert!(assistant_chain.is_empty());
            }
            other => panic!("expected Replay, got {other:?}"),
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn resume_latest_session_returns_latest_and_bumps(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();

        // no session yet → None
        assert!(repo
            .resume_latest_session(user_id, instance_id)
            .await
            .unwrap()
            .is_none());

        // two sessions with explicit last_active_at so "latest" is deterministic
        let _older = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, last_active_at) \
             VALUES ($1, $2, now() - interval '1 hour') RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let newer = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, last_active_at) \
             VALUES ($1, $2, now() - interval '1 minute') RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let before: DateTime<Utc> =
            sqlx::query_scalar("SELECT last_active_at FROM engine.chat_sessions WHERE id = $1")
                .bind(newer)
                .fetch_one(&pool)
                .await
                .unwrap();

        let resumed = repo
            .resume_latest_session(user_id, instance_id)
            .await
            .unwrap()
            .expect("resume the most-recent session");
        assert_eq!(resumed.id, newer, "must resume the most-recent session");
        assert!(
            resumed.last_active_at >= before,
            "last_active_at must be bumped"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_user_message_writes_role_user_and_no_metadata(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = repo
            .create_session(Uuid::new_v4(), Uuid::new_v4())
            .await
            .unwrap();
        let outcome = repo
            .upsert_user_message_idempotent(
                s.id,
                "hi",
                "01J0000000000000000000000A",
                "user",
                None,
            )
            .await
            .unwrap();
        match outcome {
            UpsertUserOutcome::Inserted { .. } => {}
            other => panic!("expected Inserted, got {:?}", other),
        }
        let (role, metadata): (String, Option<serde_json::Value>) = sqlx::query_as(
            "SELECT role, metadata FROM engine.chat_messages WHERE session_id = $1",
        )
        .bind(s.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(role, "user");
        assert!(metadata.is_none(), "metadata should be NULL on plain user rows");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_user_message_writes_gift_user_role_and_tip_metadata(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = repo
            .create_session(Uuid::new_v4(), Uuid::new_v4())
            .await
            .unwrap();
        let meta = serde_json::json!({ "tips_amount_usd": 20.0 });
        repo.upsert_user_message_idempotent(
            s.id,
            "(打赏 $20)",
            "01J0000000000000000000000B",
            "gift_user",
            Some(&meta),
        )
        .await
        .unwrap();
        let (role, metadata): (String, serde_json::Value) = sqlx::query_as(
            "SELECT role, metadata FROM engine.chat_messages WHERE session_id = $1",
        )
        .bind(s.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(role, "gift_user");
        assert_eq!(metadata["tips_amount_usd"].as_f64(), Some(20.0));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_user_message_replay_finds_gift_user_row(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = repo
            .create_session(Uuid::new_v4(), Uuid::new_v4())
            .await
            .unwrap();
        let meta = serde_json::json!({ "tips_amount_usd": 5.0 });
        repo.upsert_user_message_idempotent(
            s.id,
            "(打赏 $5)",
            "01J0000000000000000000000C",
            "gift_user",
            Some(&meta),
        )
        .await
        .unwrap();
        // Second call with the same client_msg_id is a replay candidate even
        // though the original wrote role='gift_user'. The widened role filter
        // in the dedup lookup is what we're exercising.
        let outcome = repo
            .upsert_user_message_idempotent(
                s.id,
                "(打赏 $5)",
                "01J0000000000000000000000C",
                "gift_user",
                Some(&meta),
            )
            .await
            .unwrap();
        match outcome {
            UpsertUserOutcome::DuplicateInProgress { .. } => {}
            other => panic!("expected DuplicateInProgress, got {:?}", other),
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn assistant_batch_round_trips_filter_audit(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = repo
            .create_session(Uuid::new_v4(), Uuid::new_v4())
            .await
            .unwrap();
        let user_msg_id = match repo
            .upsert_user_message_idempotent(
                s.id,
                "hi",
                "01J0000000000000000000001A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("{other:?}"),
        };

        let triggers = serde_json::json!({
            "random": { "p": 0.3, "draw": 0.18 },
            "models": "deepseek/deepseek-v4-flash",
            "traits": ["nsfw_boost"]
        });
        let row = AssistantInsert {
            id: Uuid::new_v4(),
            content: "filtered reply".into(),
            assistant_action_type: "reply".into(),
            continues_from_message_id: None,
            truncated: false,
            model: Some("anthropic/claude-sonnet-4.6".into()),
            usage: None,
            generation_id: Some("gen_chat_xyz".into()),
            filter_audit: Some(FilterAudit {
                pre_filter_content: "raw reply".into(),
                filter_model: "anthropic/claude-haiku-4.5".into(),
                filter_triggers: triggers.clone(),
                f_client_msg_id: "f_01J0000000000000000000001Z".into(),
                f_generation_id: Some("gen_filter_abc".into()),
            }),
        };
        repo.insert_assistant_batch(s.id, user_msg_id, &[row])
            .await
            .unwrap();

        let (
            content,
            pre_filter,
            filter_model,
            filter_triggers,
            f_client,
            f_gen,
        ): (
            String,
            Option<String>,
            Option<String>,
            Option<serde_json::Value>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT content, pre_filter_content, filter_model, filter_triggers, \
                    f_client_msg_id, f_generation_id \
             FROM engine.chat_messages WHERE role = 'assistant' AND session_id = $1",
        )
        .bind(s.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(content, "filtered reply");
        assert_eq!(pre_filter.as_deref(), Some("raw reply"));
        assert_eq!(filter_model.as_deref(), Some("anthropic/claude-haiku-4.5"));
        assert_eq!(filter_triggers, Some(triggers));
        assert_eq!(f_client.as_deref(), Some("f_01J0000000000000000000001Z"));
        assert_eq!(f_gen.as_deref(), Some("gen_filter_abc"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn assistant_batch_filter_audit_columns_default_null(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = repo
            .create_session(Uuid::new_v4(), Uuid::new_v4())
            .await
            .unwrap();
        let user_msg_id = match repo
            .upsert_user_message_idempotent(
                s.id,
                "hi",
                "01J0000000000000000000002A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("{other:?}"),
        };
        let row = AssistantInsert {
            id: Uuid::new_v4(),
            content: "plain reply".into(),
            assistant_action_type: "reply".into(),
            continues_from_message_id: None,
            truncated: false,
            model: Some("anthropic/claude-sonnet-4.6".into()),
            usage: None,
            generation_id: Some("gen_chat_xyz".into()),
            filter_audit: None,
        };
        repo.insert_assistant_batch(s.id, user_msg_id, &[row])
            .await
            .unwrap();
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
             WHERE role='assistant' AND session_id=$1 \
               AND pre_filter_content IS NULL AND filter_model IS NULL \
               AND filter_triggers IS NULL AND f_client_msg_id IS NULL \
               AND f_generation_id IS NULL",
        )
        .bind(s.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0019_adds_chat_messages_columns(pool: PgPool) {
        // Probe the six new columns by name. Each query should succeed (NULL on
        // legacy rows). If any column is missing, the SELECT errors out.
        for col in [
            "metadata",
            "pre_filter_content",
            "filter_model",
            "filter_triggers",
            "f_client_msg_id",
            "f_generation_id",
        ] {
            let q = format!("SELECT {col} FROM engine.chat_messages LIMIT 0");
            sqlx::query(&q).execute(&pool).await.unwrap_or_else(|e| {
                panic!("expected column {col} on engine.chat_messages: {e}")
            });
        }
        // Indexes exist.
        let idx_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_indexes \
             WHERE schemaname = 'engine' \
               AND tablename = 'chat_messages' \
               AND indexname IN ('chat_messages_tips_amount_idx',
                                 'chat_messages_f_client_msg_id_uidx')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(idx_count, 2, "both new indexes should exist");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn assistant_batch_filter_audit_with_none_generation_id_writes_null(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = repo
            .create_session(Uuid::new_v4(), Uuid::new_v4())
            .await
            .unwrap();
        let user_msg_id = match repo
            .upsert_user_message_idempotent(
                s.id,
                "hi",
                "01J0000000000000000000003A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("{other:?}"),
        };
        let row = AssistantInsert {
            id: Uuid::new_v4(),
            content: "filtered no gen".into(),
            assistant_action_type: "reply".into(),
            continues_from_message_id: None,
            truncated: false,
            model: Some("anthropic/claude-sonnet-4.6".into()),
            usage: None,
            generation_id: Some("gen_chat_xyz".into()),
            filter_audit: Some(FilterAudit {
                pre_filter_content: "raw".into(),
                filter_model: "anthropic/claude-haiku-4.5".into(),
                filter_triggers: serde_json::json!({}),
                f_client_msg_id: "f_01J0000000000000000000003Z".into(),
                f_generation_id: None,
            }),
        };
        repo.insert_assistant_batch(s.id, user_msg_id, &[row])
            .await
            .unwrap();
        let (pre_filter, f_gen): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT pre_filter_content, f_generation_id \
             FROM engine.chat_messages \
             WHERE role='assistant' AND session_id=$1",
        )
        .bind(s.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(pre_filter.as_deref(), Some("raw"));
        assert!(f_gen.is_none(), "f_generation_id should be NULL when None inside Some(FilterAudit)");
    }
}
