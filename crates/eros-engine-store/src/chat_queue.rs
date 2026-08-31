// SPDX-License-Identifier: AGPL-3.0-only
//! Queue store for the v2 async chat turn endpoint
//! (spec: docs/superpowers/specs/2026-08-20-async-chat-endpoint-design.md).

use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

use crate::chat::{upsert_user_message_in_tx, ReplayMessage, UpsertUserOutcome};

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

/// Outcome of a born-claimed (stream-path) enqueue. Mirrors
/// `UpsertUserOutcome` so the stream handler keeps its existing replay /
/// duplicate responses; only `Inserted` grows a queue row (spec §4).
#[derive(Debug)]
pub enum ClaimedEnqueueOutcome {
    Claimed {
        user_message_id: Uuid,
        queue_id: Uuid,
    },
    Replay {
        user_message_id: Uuid,
        ghost: bool,
        assistant_chain: Vec<ReplayMessage>,
    },
    DuplicateInProgress {
        user_message_id: Uuid,
    },
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
                    // pending / claimed → queued. A missing queue row is a turn
                    // persisted before stream turns rode the queue (legacy rows)
                    // — same "being handled" answer is still the safe reply.
                    _ => EnqueueOutcome::AlreadyQueued { user_message_id },
                })
            }
        }
    }

    /// Stream-path enqueue: user row + a BORN-CLAIMED queue row in one
    /// transaction (spec §4). The row never passes through `pending` — the
    /// caller drives it directly and it is invisible to `claim_next` while
    /// live, but its `claimed` status still blocks the session's async turns
    /// (one-way serialization, spec §5). No depth cap: the stream path has
    /// its own per-user slot cap.
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_user_message_claimed(
        &self,
        session_id: Uuid,
        content: &str,
        client_msg_id: &str,
        role: &str,
        metadata: Option<&serde_json::Value>,
        user_id: Uuid,
        params: &serde_json::Value,
    ) -> Result<ClaimedEnqueueOutcome, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let outcome =
            upsert_user_message_in_tx(&mut tx, session_id, content, client_msg_id, role, metadata)
                .await?;
        match outcome {
            UpsertUserOutcome::Inserted { message_id } => {
                let queue_id: Uuid = sqlx::query_scalar(
                    "INSERT INTO engine.chat_turn_queue \
                         (session_id, user_message_id, user_id, params, \
                          status, claimed_at, attempts) \
                     VALUES ($1, $2, $3, $4, 'claimed', now(), 1) RETURNING id",
                )
                .bind(session_id)
                .bind(message_id)
                .bind(user_id)
                .bind(params)
                .fetch_one(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(ClaimedEnqueueOutcome::Claimed {
                    user_message_id: message_id,
                    queue_id,
                })
            }
            UpsertUserOutcome::Replay {
                user_message_id,
                ghost,
                assistant_chain,
            } => {
                tx.commit().await?;
                Ok(ClaimedEnqueueOutcome::Replay {
                    user_message_id,
                    ghost,
                    assistant_chain,
                })
            }
            UpsertUserOutcome::DuplicateInProgress { user_message_id } => {
                tx.commit().await?;
                Ok(ClaimedEnqueueOutcome::DuplicateInProgress { user_message_id })
            }
        }
    }
}

/// A claimed queue row, ready for the worker to drive.
#[derive(Debug)]
pub struct ClaimedTurn {
    pub queue_id: Uuid,
    pub session_id: Uuid,
    pub user_message_id: Uuid,
    pub user_id: Uuid,
    pub attempts: i32,
    pub params: Option<serde_json::Value>,
}

/// A stale claim that just went terminal `failed` — the caller owes its
/// session a `system_error` message row.
#[derive(Debug)]
pub struct ReapedTurn {
    pub queue_id: Uuid,
    pub session_id: Uuid,
    pub user_message_id: Uuid,
}

impl<'a> ChatQueueRepo<'a> {
    /// Claim ONE turn: the per-session-newest pending row (strict LIFO,
    /// `created_at DESC, id DESC` — the id tiebreak keeps two same-timestamp
    /// inserts deterministic), from the session whose newest-pending has
    /// waited longest, skipping any session with a claimed row in flight
    /// (per-session serialization). One row per call by design: the NOT
    /// EXISTS guard is evaluated against the statement snapshot, so a
    /// multi-row LIMIT could claim two rows of one session on a
    /// `created_at` tie. The worker loops this until `None`.
    pub async fn claim_next(&self) -> Result<Option<ClaimedTurn>, sqlx::Error> {
        sqlx::query_as::<_, (Uuid, Uuid, Uuid, Uuid, i32, Option<serde_json::Value>)>(
            "UPDATE engine.chat_turn_queue \
             SET status = 'claimed', claimed_at = now(), attempts = attempts + 1 \
             WHERE id = ( \
                 SELECT q.id FROM engine.chat_turn_queue q \
                 WHERE q.status = 'pending' \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM engine.chat_turn_queue c \
                       WHERE c.session_id = q.session_id AND c.status = 'claimed') \
                   AND q.id = ( \
                       SELECT q2.id FROM engine.chat_turn_queue q2 \
                       WHERE q2.session_id = q.session_id AND q2.status = 'pending' \
                       ORDER BY q2.created_at DESC, q2.id DESC LIMIT 1) \
                 ORDER BY q.created_at ASC \
                 LIMIT 1 \
                 FOR UPDATE SKIP LOCKED \
             ) \
             RETURNING id, session_id, user_message_id, user_id, attempts, params",
        )
        .fetch_optional(self.pool)
        .await
        .map(|row| {
            row.map(
                |(queue_id, session_id, user_message_id, user_id, attempts, params)| ClaimedTurn {
                    queue_id,
                    session_id,
                    user_message_id,
                    user_id,
                    attempts,
                    params,
                },
            )
        })
    }

    pub async fn mark_done(&self, queue_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE engine.chat_turn_queue SET status = 'done' WHERE id = $1")
            .bind(queue_id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub async fn release(&self, queue_id: Uuid, error: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.chat_turn_queue \
             SET status = 'pending', claimed_at = NULL, last_error = $2 WHERE id = $1",
        )
        .bind(queue_id)
        .bind(error)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Terminal failure + its consumer-visible signal in ONE transaction: the
    /// `failed` flip and the `system_error` chat row land together or not at
    /// all. Split writes leave orphan states — a `failed` turn with no
    /// visible signal, or a stranded claim that gets a second signal from the
    /// reaper.
    /// Takes neither the session nor the driving message: both come back from
    /// the flip's own `RETURNING`, so the notice cannot be filed against a
    /// session the message does not belong to. The foreign key only checks that
    /// the message exists, not whose it is — reading the pair off one row makes
    /// the mismatch unrepresentable instead of merely unlikely. It also makes
    /// the two writes agree on a queue id that matches nothing: previously that
    /// flipped no row and still wrote a notice.
    pub async fn mark_failed_with_notice(
        &self,
        queue_id: Uuid,
        error: &str,
        notice_content: &str,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let flipped: Option<(Uuid, Uuid)> = sqlx::query_as(
            "UPDATE engine.chat_turn_queue \
             SET status = 'failed', last_error = $2 WHERE id = $1 \
             RETURNING session_id, user_message_id",
        )
        .bind(queue_id)
        .bind(error)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((session_id, user_message_id)) = flipped else {
            tx.commit().await?;
            return Ok(());
        };
        // Bound to the turn that failed, like every assistant row. Without it a
        // client routing replies per driving message has to guess "latest user
        // row in the session", which is wrong the moment another turn arrives
        // in between.
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, user_message_id) \
             VALUES ($1, 'system_error', $2, $3)",
        )
        .bind(session_id)
        .bind(notice_content)
        .bind(user_message_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE engine.chat_sessions SET last_active_at = now() WHERE id = $1")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Stale-claim recovery (worker died mid-turn). One transaction:
    /// exhausted claims go terminal WITH their consumer-visible
    /// `system_error` rows (atomic — a terminal `failed` turn without its
    /// signal must be impossible); the rest go back to `pending`. Returned
    /// rows are for the caller's logging only.
    pub async fn reap_stale(
        &self,
        stale: Duration,
        max_attempts: i32,
        notice_content: &str,
    ) -> Result<Vec<ReapedTurn>, sqlx::Error> {
        let stale_secs = stale.as_secs_f64();
        let mut tx = self.pool.begin().await?;
        let failed: Vec<(Uuid, Uuid, Uuid)> = sqlx::query_as(
            "UPDATE engine.chat_turn_queue \
             SET status = 'failed', last_error = 'stale claim: worker died mid-turn' \
             WHERE status = 'claimed' \
               AND claimed_at < now() - make_interval(secs => $1::float8) \
               AND attempts >= $2 \
             RETURNING id, session_id, user_message_id",
        )
        .bind(stale_secs)
        .bind(max_attempts)
        .fetch_all(&mut *tx)
        .await?;
        if !failed.is_empty() {
            let sessions: Vec<Uuid> = failed.iter().map(|(_, s, _)| *s).collect();
            // Each notice carries the driving message of the turn it belongs to
            // — the RETURNING above already knows it per row. Unnest both arrays
            // in step so the pairing survives the batch.
            let driving: Vec<Uuid> = failed.iter().map(|(_, _, m)| *m).collect();
            sqlx::query(
                "INSERT INTO engine.chat_messages \
                   (session_id, role, content, user_message_id) \
                 SELECT s, 'system_error', $3, m \
                 FROM unnest($1::uuid[], $2::uuid[]) AS t(s, m)",
            )
            .bind(&sessions)
            .bind(&driving)
            .bind(notice_content)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE engine.chat_sessions SET last_active_at = now() WHERE id = ANY($1)",
            )
            .bind(&sessions)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "UPDATE engine.chat_turn_queue \
             SET status = 'pending', claimed_at = NULL, \
                 last_error = 'stale claim: worker died mid-turn' \
             WHERE status = 'claimed' \
               AND claimed_at < now() - make_interval(secs => $1::float8)",
        )
        .bind(stale_secs)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(failed
            .into_iter()
            .map(|(queue_id, session_id, user_message_id)| ReapedTurn {
                queue_id,
                session_id,
                user_message_id,
            })
            .collect())
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

    /// Insert a user message + queue row with an explicit created_at offset
    /// (seconds before now). Returns (user_message_id, queue_id).
    async fn seed_queued(
        pool: &PgPool,
        session_id: Uuid,
        user_id: Uuid,
        content: &str,
        age_secs: i64,
    ) -> (Uuid, Uuid) {
        let mid: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content) \
             VALUES ($1, 'user', $2) RETURNING id",
        )
        .bind(session_id)
        .bind(content)
        .fetch_one(pool)
        .await
        .unwrap();
        let qid: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_turn_queue \
                 (session_id, user_message_id, user_id, created_at) \
             VALUES ($1, $2, $3, now() - make_interval(secs => $4::float8)) \
             RETURNING id",
        )
        .bind(session_id)
        .bind(mid)
        .bind(user_id)
        .bind(age_secs as f64)
        .fetch_one(pool)
        .await
        .unwrap();
        (mid, qid)
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_next_is_lifo_within_a_session(pool: PgPool) {
        let (user_id, session_id) = seed_session(&pool).await;
        let repo = ChatQueueRepo { pool: &pool };
        let (_a_mid, _a) = seed_queued(&pool, session_id, user_id, "A", 30).await;
        let (_b_mid, _b) = seed_queued(&pool, session_id, user_id, "B", 20).await;
        let (c_mid, c_qid) = seed_queued(&pool, session_id, user_id, "C", 10).await;

        let claimed = repo
            .claim_next()
            .await
            .unwrap()
            .expect("something to claim");
        assert_eq!(claimed.queue_id, c_qid, "newest (C) must be claimed first");
        assert_eq!(claimed.user_message_id, c_mid);
        assert_eq!(claimed.attempts, 1, "claim increments attempts");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_next_serializes_per_session_and_parallelizes_across(pool: PgPool) {
        let (user_a, session_a) = seed_session(&pool).await;
        let (user_b, session_b) = seed_session(&pool).await;
        let repo = ChatQueueRepo { pool: &pool };
        seed_queued(&pool, session_a, user_a, "a1", 40).await;
        seed_queued(&pool, session_a, user_a, "a2", 30).await;
        seed_queued(&pool, session_b, user_b, "b1", 20).await;

        let first = repo.claim_next().await.unwrap().expect("first claim");
        assert_eq!(
            first.session_id, session_a,
            "session A's newest waited longest"
        );
        let second = repo.claim_next().await.unwrap().expect("second claim");
        assert_eq!(
            second.session_id, session_b,
            "session A has a claim in flight — only B is claimable"
        );
        assert!(
            repo.claim_next().await.unwrap().is_none(),
            "both sessions have in-flight claims — nothing claimable"
        );
        // Completing A's turn frees its next (a1).
        repo.mark_done(first.queue_id).await.unwrap();
        let third = repo.claim_next().await.unwrap().expect("A free again");
        assert_eq!(third.session_id, session_a);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn release_returns_to_pending_and_keeps_attempts(pool: PgPool) {
        let (user_id, session_id) = seed_session(&pool).await;
        let repo = ChatQueueRepo { pool: &pool };
        seed_queued(&pool, session_id, user_id, "x", 10).await;
        let t = repo.claim_next().await.unwrap().unwrap();
        repo.release(t.queue_id, "upstream 500").await.unwrap();
        let (status, attempts, last_error): (String, i32, Option<String>) = sqlx::query_as(
            "SELECT status, attempts, last_error FROM engine.chat_turn_queue WHERE id = $1",
        )
        .bind(t.queue_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "pending");
        assert_eq!(attempts, 1);
        assert_eq!(last_error.as_deref(), Some("upstream 500"));
        // And it is claimable again.
        assert!(repo.claim_next().await.unwrap().is_some());
    }

    /// #295: each reaped turn's notice names the message that drove it. The
    /// reaper fails a whole batch in one INSERT, so the risk is not "no id" but
    /// a CROSS-PAIRED one — session A's notice carrying session B's driving
    /// message. Two turns, reaped together, each checked against its own.
    #[sqlx::test(migrations = "./migrations")]
    async fn reap_stale_binds_each_notice_to_its_own_driving_message(pool: PgPool) {
        let (user_a, session_a) = seed_session(&pool).await;
        let (user_b, session_b) = seed_session(&pool).await;
        let repo = ChatQueueRepo { pool: &pool };
        let (msg_a, q_a) = seed_queued(&pool, session_a, user_a, "a", 100).await;
        let (msg_b, q_b) = seed_queued(&pool, session_b, user_b, "b", 100).await;
        for q in [q_a, q_b] {
            sqlx::query(
                "UPDATE engine.chat_turn_queue SET status='claimed', attempts=3, \
                 claimed_at = now() - interval '1 hour' WHERE id = $1",
            )
            .bind(q)
            .execute(&pool)
            .await
            .unwrap();
        }

        let failed = repo
            .reap_stale(std::time::Duration::from_secs(300), 3, "出错了")
            .await
            .unwrap();
        assert_eq!(failed.len(), 2, "both exhausted claims go terminal");

        for (session, driving) in [(session_a, msg_a), (session_b, msg_b)] {
            let got: Option<Uuid> = sqlx::query_scalar(
                "SELECT user_message_id FROM engine.chat_messages \
                 WHERE session_id = $1 AND role = 'system_error'",
            )
            .bind(session)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                got,
                Some(driving),
                "the notice in session {session} must name that session's own \
                 driving message, not the other turn's"
            );
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn reap_stale_releases_young_attempts_and_fails_exhausted_ones(pool: PgPool) {
        let (user_id, session_id) = seed_session(&pool).await;
        let (user2, session2) = seed_session(&pool).await;
        let repo = ChatQueueRepo { pool: &pool };
        let (_m1, q1) = seed_queued(&pool, session_id, user_id, "young", 100).await;
        let (m2, q2) = seed_queued(&pool, session2, user2, "exhausted", 100).await;
        // Simulate stale claims: q1 on attempt 1, q2 on its final attempt.
        sqlx::query(
            "UPDATE engine.chat_turn_queue SET status='claimed', attempts=1, \
             claimed_at = now() - interval '1 hour' WHERE id = $1",
        )
        .bind(q1)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE engine.chat_turn_queue SET status='claimed', attempts=3, \
             claimed_at = now() - interval '1 hour' WHERE id = $1",
        )
        .bind(q2)
        .execute(&pool)
        .await
        .unwrap();

        let failed = repo
            .reap_stale(std::time::Duration::from_secs(300), 3, "出错了")
            .await
            .unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].queue_id, q2);
        assert_eq!(failed[0].user_message_id, m2);
        // The terminal flip and its signal row are one transaction.
        let notices: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'system_error'",
        )
        .bind(session2)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(notices, 1, "exhausted turn's session gets its signal row");
        let young_notices: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'system_error'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(young_notices, 0, "released turn's session gets none");
        let s1: String =
            sqlx::query_scalar("SELECT status FROM engine.chat_turn_queue WHERE id = $1")
                .bind(q1)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(s1, "pending", "young stale claim goes back to pending");
        let s2: String =
            sqlx::query_scalar("SELECT status FROM engine.chat_turn_queue WHERE id = $1")
                .bind(q2)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(s2, "failed", "exhausted stale claim is terminal");
        // A fresh claim must NOT be reaped.
        seed_queued(&pool, session_id, user_id, "fresh", 5).await;
        let t = repo.claim_next().await.unwrap().unwrap();
        let reaped = repo
            .reap_stale(std::time::Duration::from_secs(300), 3, "出错了")
            .await
            .unwrap();
        assert!(reaped.is_empty());
        let st: String =
            sqlx::query_scalar("SELECT status FROM engine.chat_turn_queue WHERE id = $1")
                .bind(t.queue_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(st, "claimed", "fresh claim untouched by the reaper");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn mark_failed_with_notice_writes_both_or_neither(pool: PgPool) {
        let (user_id, session_id) = seed_session(&pool).await;
        let repo = ChatQueueRepo { pool: &pool };
        seed_queued(&pool, session_id, user_id, "x", 10).await;
        let t = repo.claim_next().await.unwrap().unwrap();
        repo.mark_failed_with_notice(t.queue_id, "upstream 500", "出错了")
            .await
            .unwrap();
        let (status, last_error): (String, Option<String>) =
            sqlx::query_as("SELECT status, last_error FROM engine.chat_turn_queue WHERE id = $1")
                .bind(t.queue_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "failed");
        assert_eq!(last_error.as_deref(), Some("upstream 500"));
        // The notice is bound to the turn that failed (#295): a client routing
        // replies per driving message must not have to guess "latest user row".
        let (notice, driving): (String, Option<Uuid>) = sqlx::query_as(
            "SELECT content, user_message_id FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'system_error'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(notice, "出错了");
        assert_eq!(driving, Some(t.user_message_id));
        // The notice's session and driving message come off the same queue row,
        // so a notice can never be filed against a session its message does not
        // belong to.
        let notice_session: Uuid = sqlx::query_scalar(
            "SELECT m.session_id FROM engine.chat_messages m \
             WHERE m.id = (SELECT id FROM engine.chat_messages \
                           WHERE session_id = $1 AND role = 'system_error')",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(notice_session, session_id);
    }

    /// A queue id that matches nothing must write nothing. Before the flip
    /// started reading its own `RETURNING`, this updated no row and still filed
    /// a `system_error` notice.
    #[sqlx::test(migrations = "./migrations")]
    async fn mark_failed_with_notice_is_a_noop_on_an_unknown_queue_id(pool: PgPool) {
        seed_session(&pool).await;
        ChatQueueRepo { pool: &pool }
            .mark_failed_with_notice(Uuid::new_v4(), "upstream 500", "出错了")
            .await
            .unwrap();
        let notices: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages WHERE role = 'system_error'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(notices, 0, "no queue row flipped ⇒ no notice");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn enqueue_claimed_inserts_a_claimed_row_and_blocks_only_its_session(pool: PgPool) {
        let (user_a, session_a) = seed_session(&pool).await;
        let (user_b, session_b) = seed_session(&pool).await;
        let repo = ChatQueueRepo { pool: &pool };

        let ClaimedEnqueueOutcome::Claimed {
            user_message_id,
            queue_id,
        } = repo
            .enqueue_user_message_claimed(
                session_a,
                "hi",
                "01JW00000000000000000000CL",
                "user",
                None,
                user_a,
                &serde_json::json!({"tier": "warm"}),
            )
            .await
            .unwrap()
        else {
            panic!("expected Claimed");
        };
        let (status, attempts, claimed_at, params): (
            String,
            i32,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<serde_json::Value>,
        ) = sqlx::query_as(
            "SELECT status, attempts, claimed_at, params \
                 FROM engine.chat_turn_queue WHERE id = $1",
        )
        .bind(queue_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "claimed", "born claimed — never pending");
        assert_eq!(attempts, 1, "the handler's own drive is attempt #1");
        assert!(claimed_at.is_some());
        assert_eq!(params, Some(serde_json::json!({"tier": "warm"})));
        let mid_check: Uuid =
            sqlx::query_scalar("SELECT user_message_id FROM engine.chat_turn_queue WHERE id = $1")
                .bind(queue_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(mid_check, user_message_id);

        // One-way serialization (spec §4/§5): the claimed row blocks claim_next
        // for ITS session's pending rows, and no other session.
        seed_queued(
            &pool,
            session_a,
            user_a,
            "queued behind the stream turn",
            10,
        )
        .await;
        seed_queued(&pool, session_b, user_b, "other session", 5).await;
        let t = repo
            .claim_next()
            .await
            .unwrap()
            .expect("session B is claimable");
        assert_eq!(t.session_id, session_b);
        assert!(
            repo.claim_next().await.unwrap().is_none(),
            "session A blocked by the born-claimed row; B already in flight"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn enqueue_claimed_replays_an_answered_message(pool: PgPool) {
        let (user_id, session_id) = seed_session(&pool).await;
        let repo = ChatQueueRepo { pool: &pool };
        let mid: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, client_msg_id) \
             VALUES ($1, 'user', 'hello', '01JW00000000000000000000RP') RETURNING id",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, user_message_id) \
             VALUES ($1, 'assistant', 'hi there', $2)",
        )
        .bind(session_id)
        .bind(mid)
        .execute(&pool)
        .await
        .unwrap();

        let ClaimedEnqueueOutcome::Replay {
            user_message_id,
            ghost,
            assistant_chain,
        } = repo
            .enqueue_user_message_claimed(
                session_id,
                "hello",
                "01JW00000000000000000000RP",
                "user",
                None,
                user_id,
                &serde_json::json!({}),
            )
            .await
            .unwrap()
        else {
            panic!("expected Replay");
        };
        assert_eq!(user_message_id, mid);
        assert!(!ghost);
        assert_eq!(assistant_chain.len(), 1);
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_turn_queue WHERE user_message_id = $1",
        )
        .bind(mid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 0, "a replay creates no queue row");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn enqueue_claimed_reports_duplicate_in_progress(pool: PgPool) {
        let (user_id, session_id) = seed_session(&pool).await;
        let repo = ChatQueueRepo { pool: &pool };
        let mid: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, client_msg_id) \
             VALUES ($1, 'user', 'hello', '01JW00000000000000000000DP') RETURNING id",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let ClaimedEnqueueOutcome::DuplicateInProgress { user_message_id } = repo
            .enqueue_user_message_claimed(
                session_id,
                "hello",
                "01JW00000000000000000000DP",
                "user",
                None,
                user_id,
                &serde_json::json!({}),
            )
            .await
            .unwrap()
        else {
            panic!("expected DuplicateInProgress");
        };
        assert_eq!(user_message_id, mid);
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_turn_queue WHERE user_message_id = $1",
        )
        .bind(mid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 0);
    }
}
