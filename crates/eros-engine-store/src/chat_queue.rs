// SPDX-License-Identifier: AGPL-3.0-only
//! Queue store for the v2 async chat turn endpoint
//! (spec: docs/superpowers/specs/2026-08-20-async-chat-endpoint-design.md).

use sqlx::PgPool;
use uuid::Uuid;

use crate::chat::{upsert_user_message_in_tx, UpsertUserOutcome};

pub struct ChatQueueRepo<'a> {
    pub pool: &'a PgPool,
}

/// Outcome of an enqueue attempt, resolved against both the message table's
/// permanent idempotency and the queue row's lifecycle.
#[derive(Debug)]
pub enum EnqueueOutcome {
    Queued {
        user_message_id: Uuid,
        queue_id: Uuid,
    },
    AlreadyQueued {
        user_message_id: Uuid,
    },
    AlreadyCompleted {
        user_message_id: Uuid,
    },
    Failed {
        user_message_id: Uuid,
    },
    DepthExceeded,
}

impl<'a> ChatQueueRepo<'a> {
    /// User row + queue row in ONE transaction. A user row without a queue row
    /// would be a message in history nothing will ever answer — the exact hole
    /// the spec forbids (§4 Atomicity).
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_user_message(
        &self,
        session_id: Uuid,
        content: &str,
        client_msg_id: &str,
        role: &str,
        metadata: Option<&serde_json::Value>,
        user_id: Uuid,
        params: &serde_json::Value,
        pending_cap: i64,
    ) -> Result<EnqueueOutcome, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let outcome =
            upsert_user_message_in_tx(&mut tx, session_id, content, client_msg_id, role, metadata)
                .await?;
        match outcome {
            UpsertUserOutcome::Inserted { message_id } => {
                let pending: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM engine.chat_turn_queue \
                     WHERE session_id = $1 AND status IN ('pending','claimed')",
                )
                .bind(session_id)
                .fetch_one(&mut *tx)
                .await?;
                if pending >= pending_cap {
                    tx.rollback().await?;
                    return Ok(EnqueueOutcome::DepthExceeded);
                }
                let queue_id: Uuid = sqlx::query_scalar(
                    "INSERT INTO engine.chat_turn_queue \
                         (session_id, user_message_id, user_id, params) \
                     VALUES ($1, $2, $3, $4) RETURNING id",
                )
                .bind(session_id)
                .bind(message_id)
                .bind(user_id)
                .bind(params)
                .fetch_one(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(EnqueueOutcome::Queued {
                    user_message_id: message_id,
                    queue_id,
                })
            }
            UpsertUserOutcome::Replay {
                user_message_id, ..
            } => {
                tx.commit().await?;
                Ok(EnqueueOutcome::AlreadyCompleted { user_message_id })
            }
            UpsertUserOutcome::DuplicateInProgress { user_message_id } => {
                let status: Option<String> = sqlx::query_scalar(
                    "SELECT status FROM engine.chat_turn_queue WHERE user_message_id = $1",
                )
                .bind(user_message_id)
                .fetch_optional(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(match status.as_deref() {
                    Some("failed") => EnqueueOutcome::Failed { user_message_id },
                    // pending / claimed → queued; no queue row means the turn is
                    // mid-flight on the STREAM path — same "being handled" answer.
                    _ => EnqueueOutcome::AlreadyQueued { user_message_id },
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;
    use uuid::Uuid;

    async fn seed_session(pool: &PgPool) -> (Uuid, Uuid) {
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('Q', 'p', '{}'::jsonb) RETURNING id",
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
        (user_id, session_id)
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn enqueue_inserts_user_row_and_queue_row_atomically(pool: PgPool) {
        let (user_id, session_id) = seed_session(&pool).await;
        let repo = ChatQueueRepo { pool: &pool };
        let out = repo
            .enqueue_user_message(
                session_id,
                "hi",
                "01JQ000000000000000000001A",
                "user",
                None,
                user_id,
                &serde_json::json!({}),
                20,
            )
            .await
            .unwrap();
        let EnqueueOutcome::Queued {
            user_message_id,
            queue_id,
        } = out
        else {
            panic!("expected Queued, got {out:?}");
        };
        let (qs, qmid): (String, Uuid) = sqlx::query_as(
            "SELECT status, user_message_id FROM engine.chat_turn_queue WHERE id = $1",
        )
        .bind(queue_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(qs, "pending");
        assert_eq!(qmid, user_message_id);
        let content: String =
            sqlx::query_scalar("SELECT content FROM engine.chat_messages WHERE id = $1")
                .bind(user_message_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(content, "hi");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn enqueue_duplicate_key_maps_to_already_queued(pool: PgPool) {
        let (user_id, session_id) = seed_session(&pool).await;
        let repo = ChatQueueRepo { pool: &pool };
        let first = repo
            .enqueue_user_message(
                session_id,
                "hi",
                "01JQ000000000000000000002A",
                "user",
                None,
                user_id,
                &serde_json::json!({}),
                20,
            )
            .await
            .unwrap();
        assert!(matches!(first, EnqueueOutcome::Queued { .. }));
        // Webhook redelivery: same key again while the queue row is pending.
        let second = repo
            .enqueue_user_message(
                session_id,
                "hi",
                "01JQ000000000000000000002A",
                "user",
                None,
                user_id,
                &serde_json::json!({}),
                20,
            )
            .await
            .unwrap();
        assert!(matches!(second, EnqueueOutcome::AlreadyQueued { .. }));
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.chat_turn_queue WHERE session_id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 1, "redelivery must not add a second queue row");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn enqueue_completed_and_failed_keys_map_to_their_outcomes(pool: PgPool) {
        use crate::chat::{AssistantInsert, ChatRepo, UpsertUserOutcome};
        let (user_id, session_id) = seed_session(&pool).await;
        let repo = ChatQueueRepo { pool: &pool };
        let chat = ChatRepo { pool: &pool };

        // Completed: user row + assistant row exist (came via the stream path).
        let done_uid = match chat
            .upsert_user_message_idempotent(
                session_id,
                "done msg",
                "01JQ000000000000000000003A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };
        chat.insert_assistant_batch(
            session_id,
            done_uid,
            &[AssistantInsert {
                id: ulid::Ulid::new().into(),
                content: "served".into(),
                assistant_action_type: "reply".into(),
                continues_from_message_id: None,
                truncated: false,
                model: None,
                usage: None,
                generation_id: None,
                filter_audit: None,
                metadata: None,
                llm_attempts: None,
                gateway_errors: None,
            }],
        )
        .await
        .unwrap();
        let out = repo
            .enqueue_user_message(
                session_id,
                "done msg",
                "01JQ000000000000000000003A",
                "user",
                None,
                user_id,
                &serde_json::json!({}),
                20,
            )
            .await
            .unwrap();
        assert!(matches!(out, EnqueueOutcome::AlreadyCompleted { .. }));

        // Failed: queue row exists in terminal 'failed'.
        let queued = repo
            .enqueue_user_message(
                session_id,
                "will fail",
                "01JQ000000000000000000004A",
                "user",
                None,
                user_id,
                &serde_json::json!({}),
                20,
            )
            .await
            .unwrap();
        let EnqueueOutcome::Queued { queue_id, .. } = queued else {
            unreachable!()
        };
        sqlx::query("UPDATE engine.chat_turn_queue SET status = 'failed' WHERE id = $1")
            .bind(queue_id)
            .execute(&pool)
            .await
            .unwrap();
        let out = repo
            .enqueue_user_message(
                session_id,
                "will fail",
                "01JQ000000000000000000004A",
                "user",
                None,
                user_id,
                &serde_json::json!({}),
                20,
            )
            .await
            .unwrap();
        assert!(matches!(out, EnqueueOutcome::Failed { .. }));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn enqueue_depth_cap_rejects_and_rolls_back_the_user_row(pool: PgPool) {
        let (user_id, session_id) = seed_session(&pool).await;
        let repo = ChatQueueRepo { pool: &pool };
        for i in 0..2 {
            let key = format!("01JQ00000000000000000001{i}A");
            assert!(matches!(
                repo.enqueue_user_message(
                    session_id,
                    "spam",
                    &key,
                    "user",
                    None,
                    user_id,
                    &serde_json::json!({}),
                    2,
                )
                .await
                .unwrap(),
                EnqueueOutcome::Queued { .. }
            ));
        }
        let out = repo
            .enqueue_user_message(
                session_id,
                "over cap",
                "01JQ000000000000000000019A",
                "user",
                None,
                user_id,
                &serde_json::json!({}),
                2,
            )
            .await
            .unwrap();
        assert!(matches!(out, EnqueueOutcome::DepthExceeded));
        // The rejected request must persist NOTHING — neither queue nor message row.
        let msgs: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
             WHERE session_id = $1 AND client_msg_id = '01JQ000000000000000000019A'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(msgs, 0, "depth rejection must roll back the user row");
    }
}
