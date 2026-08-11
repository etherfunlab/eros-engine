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
    pub is_converted: bool,
    pub last_active_at: DateTime<Utc>,
    pub metadata: serde_json::Value,
    /// Conversation channel ('text' or 'voice'); start/resume is channel-scoped.
    pub channel: String,
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
    /// Conversation-flavor marker (migrations 0030/0034): NULL = normal text
    /// turn; 'voice' = voice channel; 'product_qa' = out-of-character product
    /// answer (excluded from companion context).
    #[serde(default)]
    pub channel: Option<String>,
    /// Input-filter rewrite of a `role='user'` row: the model-facing effective
    /// text when the user's original was rewritten (NULL otherwise). On
    /// assistant rows this column holds the OPPOSITE direction (the
    /// pre-OUTPUT-filter original) — see chat_input_filter design.
    #[serde(default)]
    pub pre_filter_content: Option<String>,
    /// Freeform per-row metadata (JSONB). Carries tip amount + raw scopes and,
    /// for image turns, `image_url` plus the `chat_vision` describe result under
    /// `vision`. Nullable in the table ⇒ `Option`. Exposed so the pipeline can
    /// read `metadata.vision` when rendering model-facing user text.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
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
    /// Conversation-flavor marker: `"product_qa"` = out-of-character product
    /// answer (excluded from companion context). NULL for normal turns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

pub struct ChatRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> ChatRepo<'a> {
    /// Create a new (text-channel) chat session for `user_id` × `instance_id`.
    pub async fn create_session(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
    ) -> Result<ChatSession, sqlx::Error> {
        self.create_session_with_metadata(user_id, instance_id, serde_json::json!({}), "text")
            .await
    }

    /// Create a session and seed `metadata` as the JSONB column. Used by
    /// callers that need session-scoped flags (e.g. `is_demo`) the
    /// pipeline reads later. `channel` scopes the session ('text'/'voice');
    /// resume is channel-scoped so the two never cross (see
    /// `resume_latest_session`).
    pub async fn create_session_with_metadata(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
        metadata: serde_json::Value,
        channel: &str,
    ) -> Result<ChatSession, sqlx::Error> {
        sqlx::query_as::<_, ChatSession>(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, metadata, channel) \
             VALUES ($1, $2, $3, $4) \
             RETURNING *",
        )
        .bind(user_id)
        .bind(instance_id)
        .bind(metadata)
        .bind(channel)
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
    ///
    /// WARNING: channel-unaware — resumes the latest session on ANY channel.
    /// Test-only convenience; production paths use `resume_latest_session`
    /// with an explicit channel.
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
    /// Channel-scoped: only sessions on `channel` are candidates.
    pub async fn resume_latest_session(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
        channel: &str,
    ) -> Result<Option<ChatSession>, sqlx::Error> {
        sqlx::query_as::<_, ChatSession>(
            "UPDATE engine.chat_sessions SET last_active_at = now() \
             WHERE id = ( \
                 SELECT id FROM engine.chat_sessions \
                 WHERE user_id = $1 AND instance_id = $2 AND channel = $3 \
                 ORDER BY last_active_at DESC \
                 LIMIT 1 \
             ) \
             RETURNING *",
        )
        .bind(user_id)
        .bind(instance_id)
        .bind(channel)
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

    /// `sent_at` of message `id` within `session_id`; `None` if the id is not in
    /// this session (or does not exist). Resolves a caller-supplied
    /// `reply_to_message_id` into a history anchor.
    pub async fn message_sent_at_in_session(
        &self,
        session_id: Uuid,
        id: Uuid,
    ) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT sent_at FROM engine.chat_messages WHERE id = $1 AND session_id = $2",
        )
        .bind(id)
        .bind(session_id)
        .fetch_optional(self.pool)
        .await
    }

    /// History for a quote-anchored turn, chronological.
    ///
    /// `anchor = Some(ts)` (rewind): returns **at most `limit` rows total**,
    /// chronological. The current message (`current_msg_id`, whose
    /// `sent_at > ts`) is always included and **counts toward `limit`**; it
    /// sorts first under `DESC` so `LIMIT` only trims the oldest
    /// below-anchor rows. Messages with `sent_at` strictly between `ts` and
    /// the current message's `sent_at` are excluded. This matches the
    /// `history()` window contract — both use `limit` rows total including
    /// the newest (current) message.
    ///
    /// `anchor = None` (drop history): only the `current_msg_id` row.
    ///
    /// Uses the existing `(session_id, sent_at DESC)` index — no migration.
    pub async fn history_anchored(
        &self,
        session_id: Uuid,
        current_msg_id: Uuid,
        anchor: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<ChatMessage>, sqlx::Error> {
        let mut rows = match anchor {
            Some(ts) => {
                sqlx::query_as::<_, ChatMessage>(
                    "SELECT * FROM engine.chat_messages \
                     WHERE session_id = $1 AND (sent_at <= $2 OR id = $3) \
                     ORDER BY sent_at DESC \
                     LIMIT $4",
                )
                .bind(session_id)
                .bind(ts)
                .bind(current_msg_id)
                .bind(limit)
                .fetch_all(self.pool)
                .await?
            }
            None => {
                sqlx::query_as::<_, ChatMessage>(
                    "SELECT * FROM engine.chat_messages \
                     WHERE session_id = $1 AND id = $2",
                )
                .bind(session_id)
                .bind(current_msg_id)
                .fetch_all(self.pool)
                .await?
            }
        };
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
                    (metadata->>'tips_amount_usd')::float8 AS tips_amount_usd, \
                    channel \
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

    /// Returns up to `limit` (user_or_gift_user_content, assistant_content)
    /// pairs from this session whose rows have `truncated = false`, ordered
    /// chronologically. Pairs are formed by walking the most-recent rows
    /// chronologically: a `user` or `gift_user` row immediately followed by an
    /// `assistant` row produces one pair; orphan rows are dropped.
    /// Channel-marked rows ('voice'/'product_qa') are out of companion context and excluded.
    ///
    /// Used by the chat pipeline to inject the `[recent_conversation]` short-
    /// term memory block into the system prompt. Uses the existing
    /// `idx_chat_messages_session(session_id, sent_at DESC)` index — no
    /// migration needed.
    pub async fn recent_turn_pairs(
        &self,
        session_id: Uuid,
        cutoff: DateTime<Utc>,
        limit: u8,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        // Pull enough rows for `limit` complete pairs. 2× is the minimum; we
        // fetch a small extra to absorb any role-interleaving slop without a
        // round trip. limit <= 10 in practice (a single turn's short-term
        // memory budget), so this stays cheap.
        let fetch_n: i64 = (limit as i64) * 2 + 2;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT role, content \
             FROM engine.chat_messages \
             WHERE session_id = $1 \
               AND sent_at < $2 \
               AND truncated = FALSE \
               AND channel IS NULL \
               AND role IN ('user', 'gift_user', 'assistant') \
             ORDER BY sent_at DESC \
             LIMIT $3",
        )
        .bind(session_id)
        .bind(cutoff)
        .bind(fetch_n)
        .fetch_all(self.pool)
        .await?;

        // Reverse to chronological order, then pair (user|gift_user) → assistant.
        let mut chrono = rows;
        chrono.reverse();

        let mut pairs: Vec<(String, String)> = Vec::new();
        let mut i = 0;
        while i + 1 < chrono.len() {
            let (role_a, content_a) = &chrono[i];
            let (role_b, content_b) = &chrono[i + 1];
            if (role_a == "user" || role_a == "gift_user") && role_b == "assistant" {
                pairs.push((content_a.clone(), content_b.clone()));
                i += 2;
            } else {
                i += 1;
            }
        }

        // Keep only the last `limit` pairs.
        let want = limit as usize;
        if pairs.len() > want {
            let drop = pairs.len() - want;
            pairs.drain(..drop);
        }
        Ok(pairs)
    }

    /// Same as `recent_turn_pairs` but cuts off at the sent_at of a specific
    /// message (typically the current turn's user row), not a caller-supplied
    /// timestamp. Looking up the cutoff via subquery avoids a race with
    /// concurrent streams that could insert a row between wall-clock-now and
    /// the read of recent rows.
    /// Channel-marked rows ('voice'/'product_qa') are out of companion context and excluded.
    ///
    /// Equivalent to `recent_turn_pairs(session_id, msg.sent_at, limit)` but
    /// in a single round-trip. If `message_id` doesn't exist in the session
    /// the subquery returns NULL and `sent_at < NULL` matches nothing →
    /// returns an empty Vec.
    pub async fn recent_turn_pairs_before_message(
        &self,
        session_id: Uuid,
        message_id: Uuid,
        limit: u8,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        let fetch_n: i64 = (limit as i64) * 2 + 2;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT role, content \
             FROM engine.chat_messages \
             WHERE session_id = $1 \
               AND sent_at < (SELECT sent_at FROM engine.chat_messages WHERE id = $2) \
               AND truncated = FALSE \
               AND channel IS NULL \
               AND role IN ('user', 'gift_user', 'assistant') \
             ORDER BY sent_at DESC \
             LIMIT $3",
        )
        .bind(session_id)
        .bind(message_id)
        .bind(fetch_n)
        .fetch_all(self.pool)
        .await?;

        // Same pair-walking algorithm as recent_turn_pairs (reverse → walk
        // user|gift_user → assistant pairs → keep last `limit`).
        let mut chrono = rows;
        chrono.reverse();
        let mut pairs: Vec<(String, String)> = Vec::new();
        let mut i = 0;
        while i + 1 < chrono.len() {
            let (role_a, content_a) = &chrono[i];
            let (role_b, content_b) = &chrono[i + 1];
            if (role_a == "user" || role_a == "gift_user") && role_b == "assistant" {
                pairs.push((content_a.clone(), content_b.clone()));
                i += 2;
            } else {
                i += 1;
            }
        }
        let want = limit as usize;
        if pairs.len() > want {
            let drop = pairs.len() - want;
            pairs.drain(..drop);
        }
        Ok(pairs)
    }

    /// Up to `limit` recent (question, answer) pairs on the product-QA channel,
    /// strictly before `message_id`'s sent_at, chronological. Mirrors
    /// `recent_turn_pairs_before_message` but scoped to `channel='product_qa'`
    /// rows (both sides of a product-QA exchange carry the marker). Feeds the
    /// judge's `[最近产品咨询]` block and the executor's follow-up context.
    pub async fn recent_product_qa_pairs(
        &self,
        session_id: Uuid,
        message_id: Uuid,
        limit: u8,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        let fetch_n: i64 = (limit as i64) * 2 + 2;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT role, content \
             FROM engine.chat_messages \
             WHERE session_id = $1 \
               AND sent_at < (SELECT sent_at FROM engine.chat_messages WHERE id = $2) \
               AND truncated = FALSE \
               AND channel = 'product_qa' \
               AND role IN ('user', 'assistant') \
             ORDER BY sent_at DESC \
             LIMIT $3",
        )
        .bind(session_id)
        .bind(message_id)
        .bind(fetch_n)
        .fetch_all(self.pool)
        .await?;

        let mut chrono = rows;
        chrono.reverse();
        let mut pairs: Vec<(String, String)> = Vec::new();
        let mut i = 0;
        while i + 1 < chrono.len() {
            let (role_a, content_a) = &chrono[i];
            let (role_b, content_b) = &chrono[i + 1];
            if role_a == "user" && role_b == "assistant" {
                pairs.push((content_a.clone(), content_b.clone()));
                i += 2;
            } else {
                i += 1;
            }
        }
        let want = limit as usize;
        if pairs.len() > want {
            let drop = pairs.len() - want;
            pairs.drain(..drop);
        }
        Ok(pairs)
    }

    /// Up to `limit` recent assistant `content` rows for the session, with
    /// `truncated = FALSE`, strictly before the current turn's user row
    /// (`before_message_id`). Returned newest-first. The cutoff is resolved
    /// via subquery on the message id — same race-safety as
    /// `recent_turn_pairs_before_message` (a concurrent stream can't leak a
    /// "future" row into this turn). Channel-marked rows ('voice'/'product_qa') are out of companion context and excluded.
    /// Used by the chat pipeline to mine over-used reply openings for the `[avoid_repetition]` block.
    pub async fn recent_assistant_contents(
        &self,
        session_id: Uuid,
        before_message_id: Uuid,
        limit: i64,
    ) -> Result<Vec<String>, sqlx::Error> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT content \
             FROM engine.chat_messages \
             WHERE session_id = $1 \
               AND sent_at < (SELECT sent_at FROM engine.chat_messages WHERE id = $2) \
               AND truncated = FALSE \
               AND channel IS NULL \
               AND role = 'assistant' \
             ORDER BY sent_at DESC \
             LIMIT $3",
        )
        .bind(session_id)
        .bind(before_message_id)
        .bind(limit)
        .fetch_all(self.pool)
        .await?;
        Ok(rows.into_iter().map(|(c,)| c).collect())
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
    pub assistant_action_type: String, // "reply"
    pub continues_from_message_id: Option<Uuid>,
    pub truncated: bool,
    pub model: Option<String>,
    pub usage: Option<serde_json::Value>,
    pub generation_id: Option<String>,
    pub filter_audit: Option<FilterAudit>,
    /// Open marker bag for assistant rows. Today carries `{"prompt_traits": [...]}`
    /// — the kept (post-tier-gated) prompt-trait tags injected this turn,
    /// mirroring the final frame's `prompt_injected`. Always written when
    /// trait info is known (potentially `{"prompt_traits": []}`). NULL on
    /// legacy rows from before this column existed.
    pub metadata: Option<serde_json::Value>,
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

/// Outcome of `insert_voice_user_message`. The route uses this to distinguish
/// a fresh user turn (proceed to generation) from a replayed `client_msg_id`
/// (return 409, never regenerate a second assistant reply).
#[derive(Debug, Clone, Copy)]
pub enum VoiceUserInsert {
    Inserted(Uuid),
    Duplicate(Uuid),
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

    /// Stamp the current turn's user row as a product-QA question
    /// (`channel = 'product_qa'`). Idempotent — re-marking is a no-op. Only an
    /// unmarked (`channel IS NULL`) text row is eligible; voice rows can never
    /// reach the PDE, so the guard is belt-and-suspenders.
    pub async fn mark_user_message_product_qa(
        &self,
        user_message_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.chat_messages SET channel = 'product_qa' \
             WHERE id = $1 AND role = 'user' AND channel IS NULL",
        )
        .bind(user_message_id)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Stamp an input-filter rewrite onto an existing `role='user'` row. The
    /// original text in `content` is left untouched (the client always sees the
    /// original); the rewrite lands in `pre_filter_content` (model-facing).
    /// `reason`, when non-blank, is stored as `filter_triggers = {"reason": …}`.
    /// `f_client_msg_id` stays NULL — the user row is updated in place rather
    /// than inserting a separate filter row.
    pub async fn set_user_input_rewrite(
        &self,
        user_message_id: Uuid,
        pre_filter_content: &str,
        filter_model: &str,
        reason: Option<&str>,
        f_generation_id: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let filter_triggers: Option<serde_json::Value> = reason
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|r| serde_json::json!({ "reason": r }));
        sqlx::query(
            "UPDATE engine.chat_messages \
             SET pre_filter_content = $2, filter_model = $3, \
                 filter_triggers = $4, f_generation_id = $5 \
             WHERE id = $1 AND role = 'user'",
        )
        .bind(user_message_id)
        .bind(pre_filter_content)
        .bind(filter_model)
        .bind(filter_triggers)
        .bind(f_generation_id)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Merge a `chat_vision` describe result into a `role='user'` row's
    /// metadata. Top-level JSONB merge; `COALESCE` handles a NULL metadata
    /// column. Leaves `content`, `pre_filter_content`, and existing keys (e.g.
    /// `image_url`) intact.
    pub async fn set_user_image_vision(
        &self,
        user_message_id: Uuid,
        vision: &serde_json::Value,
        vision_model: &str,
        generation_id: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let patch = serde_json::json!({
            "vision": vision,
            "vision_model": vision_model,
            "vision_generation_id": generation_id,
        });
        sqlx::query(
            "UPDATE engine.chat_messages \
             SET metadata = COALESCE(metadata, '{}'::jsonb) || $2 \
             WHERE id = $1 AND role = 'user'",
        )
        .bind(user_message_id)
        .bind(patch)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Merge the generated-image metadata onto an assistant row at gen time.
    /// Top-level JSONB merge; leaves content + existing keys intact. Scoped by
    /// `session_id` as well as `id` for defense-in-depth — callers always pass a
    /// same-session id, so behavior is unchanged, but the predicate prevents a
    /// foreign-session row from ever being touched.
    pub async fn merge_assistant_image_meta(
        &self,
        session_id: Uuid,
        message_id: Uuid,
        image: &serde_json::Value,
    ) -> Result<(), sqlx::Error> {
        let patch = serde_json::json!({ "image": image });
        sqlx::query(
            "UPDATE engine.chat_messages \
             SET metadata = COALESCE(metadata, '{}'::jsonb) || $3 \
             WHERE id = $1 AND session_id = $2 AND role = 'assistant'",
        )
        .bind(message_id)
        .bind(session_id)
        .bind(patch)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Shallow-merge `{ key: value }` into an assistant row's `metadata`
    /// (`metadata = COALESCE(metadata,'{}') || {key:value}`). Generic sibling of
    /// `merge_assistant_image_meta` (which hardcodes the `"image"` key). Scoped
    /// by `session_id` and `role = 'assistant'` (IDOR guard). No-op if no row
    /// matches.
    pub async fn merge_assistant_metadata_key(
        &self,
        session_id: Uuid,
        message_id: Uuid,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<(), sqlx::Error> {
        let patch = serde_json::json!({ key: value });
        sqlx::query(
            "UPDATE engine.chat_messages \
             SET metadata = COALESCE(metadata, '{}'::jsonb) || $3 \
             WHERE id = $1 AND session_id = $2 AND role = 'assistant'",
        )
        .bind(message_id)
        .bind(session_id)
        .bind(patch)
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
                        // JSON null and SQL NULL are equivalent "no predicate data"
                        // for this audit column; normalize JSON null to SQL NULL so
                        // `filter_triggers IS NULL` is a clean "no predicates fired"
                        // probe. (Binding Value::Null would otherwise write JSONB 'null'.)
                        (!a.filter_triggers.is_null()).then_some(&a.filter_triggers),
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
                    f_client_msg_id, f_generation_id, metadata) \
                 VALUES ($1, $2, 'assistant', $3, $4, $5, $6, $7, $8, $9, $10, \
                         $11, $12, $13, $14, $15, $16)",
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
            .bind(&row.metadata)
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

    /// Idempotently persist a user turn on the voice channel. Mirrors the
    /// `(session_id, client_msg_id)` uniqueness of the text path but writes
    /// `channel = 'voice'` and skips all replay/ghost machinery. Returns
    /// which case occurred — `Duplicate` when `client_msg_id` was already
    /// seen (existing row's id), `Inserted` (fresh row's id) otherwise — so
    /// callers can refuse to regenerate a reply for a duplicate. Bumps
    /// `last_active_at` only on the `Inserted` branch — a `Duplicate` replay
    /// must not bump, or it would reorder sibling-session selection for a
    /// later feature.
    pub async fn insert_voice_user_message(
        &self,
        session_id: Uuid,
        content: &str,
        client_msg_id: &str,
    ) -> Result<VoiceUserInsert, sqlx::Error> {
        // Atomic: on a (session_id, client_msg_id) conflict — the partial unique
        // index chat_messages_client_msg_id_uidx (WHERE client_msg_id IS NOT NULL,
        // migration 0012) — DO NOTHING and fall through to reading the existing id,
        // so concurrent duplicates are classified reliably (the loser gets
        // Duplicate, never a unique-violation 500).
        let mut tx = self.pool.begin().await?;
        let inserted: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, client_msg_id, channel) \
             VALUES ($1, 'user', $2, $3, 'voice') \
             ON CONFLICT (session_id, client_msg_id) WHERE client_msg_id IS NOT NULL DO NOTHING \
             RETURNING id",
        )
        .bind(session_id)
        .bind(content)
        .bind(client_msg_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(id) = inserted {
            // Bump only on a fresh insert. A `Duplicate` replay must NOT bump —
            // it would reorder sibling-session selection for a later feature.
            sqlx::query("UPDATE engine.chat_sessions SET last_active_at = now() WHERE id = $1")
                .bind(session_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(VoiceUserInsert::Inserted(id));
        }
        // Conflict → a row with this (session_id, client_msg_id) already exists.
        // The unique index is role-agnostic, so the conflicting row may be a
        // `user` OR a `gift_user` (tip) row — do NOT filter by role, or a tip
        // collision would miss and surface a 500 instead of the 409 duplicate.
        let existing: Uuid = sqlx::query_scalar(
            "SELECT id FROM engine.chat_messages \
             WHERE session_id = $1 AND client_msg_id = $2 LIMIT 1",
        )
        .bind(session_id)
        .bind(client_msg_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(VoiceUserInsert::Duplicate(existing))
    }

    /// Persist an assistant turn on the voice channel: `role='assistant'`,
    /// `assistant_action_type='reply'`, `channel='voice'`. No filter audit, no
    /// continues_from — voice replies are single, whole messages. Bumps
    /// `last_active_at` in the same transaction as the insert.
    ///
    /// Idempotent on `user_message_id` (migration 0041): a second writer for the
    /// same turn hits `ON CONFLICT`, whose behaviour depends on whether the
    /// EXISTING row already carries the interrupt marker
    /// (`metadata ? 'voice_interrupt'`):
    /// - **marked** (an interrupt inserted or touched this row first): `content`
    ///   and `truncated` are left as they are — the interrupt's report is
    ///   authoritative — while `model`/`usage`/`generation_id` still refill from
    ///   the conflicting write, so a still-live generator's billing audit is
    ///   not lost. This is what lets the interrupt endpoint and a still-live
    ///   generator race safely — content belongs to the interrupt, billing
    ///   audit to the generator.
    /// - **unmarked** (both writers are ordinary generators — e.g. an orphaned
    ///   generator from a dead connection racing a client's retry of the same
    ///   turn): last-writer-wins on every column this clause SETs — `content`
    ///   and `truncated` included, alongside the audit trio. This keeps
    ///   `content` and `generation_id` consistent with each other — the
    ///   previous unconditional "refill audit columns only" rule could leave
    ///   one generation's `content` paired with a DIFFERENT generation's
    ///   `generation_id`, which OpenRouter reconciliation would silently
    ///   mis-attribute.
    ///
    /// `metadata` is in NEITHER branch's SET list, so it is never refreshed on
    /// conflict in either case: the first writer's metadata stands. That is
    /// deliberate — it is what preserves an interrupt's `voice_interrupt`
    /// marker (and the generator's `relationship_scope`) against a later
    /// conflicting write, and the marker is the very key this CASE reads.
    ///
    /// `usage` additionally falls back to the existing row's value when the
    /// conflicting write's own `usage` is NULL (e.g. upstream never sent a
    /// usage payload), so a race never downgrades known usage to unknown.
    ///
    /// The insert is additionally **skipped entirely** when the user row is
    /// already marked `voice_interrupt` and no assistant reply exists yet —
    /// that combination means the client reported nothing was heard, and the
    /// "absent + empty" contract cell requires no assistant row at all. Without
    /// this guard a still-live generator could win a race against an
    /// empty-text interrupt and leave a full, untruncated reply with no
    /// interrupt marker: indistinguishable from a normal completion. The first
    /// statement below takes `FOR UPDATE` on the user row — the SAME row
    /// `upsert_voice_interrupt` locks first — so the two writers cannot
    /// interleave their check and their write; whichever commits first is the
    /// one the other observes. This lock is load-bearing, not incidental.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_voice_assistant_message(
        &self,
        session_id: Uuid,
        user_message_id: Uuid,
        id: Uuid,
        content: &str,
        model: Option<&str>,
        usage: Option<&serde_json::Value>,
        generation_id: Option<&str>,
        truncated: bool,
        metadata: Option<&serde_json::Value>,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // Same row `upsert_voice_interrupt` locks first (via its user-row
        // UPDATE) — serializes the two writers so the marker check below can't
        // race a concurrent empty-text interrupt.
        sqlx::query("SELECT 1 FROM engine.chat_messages WHERE id = $1 FOR UPDATE")
            .bind(user_message_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO engine.chat_messages AS m \
             (id, session_id, role, content, user_message_id, truncated, model, usage, \
              generation_id, assistant_action_type, channel, metadata) \
             SELECT $1, $2, 'assistant', $3, $4, $5, $6, $7, $8, 'reply', 'voice', $9 \
             WHERE NOT ( \
                 COALESCE((SELECT u.metadata->>'voice_interrupt' \
                             FROM engine.chat_messages u WHERE u.id = $4), 'false')::bool \
                 AND NOT EXISTS ( \
                     SELECT 1 FROM engine.chat_messages a \
                      WHERE a.user_message_id = $4 AND a.role = 'assistant' AND a.channel = 'voice' \
                 ) \
             ) \
             ON CONFLICT (user_message_id) WHERE role = 'assistant' AND channel = 'voice' \
             DO UPDATE SET content   = CASE WHEN m.metadata ? 'voice_interrupt' \
                                             THEN m.content ELSE EXCLUDED.content END, \
                           truncated = CASE WHEN m.metadata ? 'voice_interrupt' \
                                             THEN m.truncated ELSE EXCLUDED.truncated END, \
                           model = EXCLUDED.model, \
                           usage = COALESCE(EXCLUDED.usage, m.usage), \
                           generation_id = EXCLUDED.generation_id",
        )
        .bind(id)
        .bind(session_id)
        .bind(content)
        .bind(user_message_id)
        .bind(truncated)
        .bind(model)
        .bind(usage)
        .bind(generation_id)
        .bind(metadata)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE engine.chat_sessions SET last_active_at = now() WHERE id = $1")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Persist an out-of-character product answer: `role='assistant'`,
    /// `assistant_action_type='reply'`, `channel='product_qa'`. No filter
    /// audit, no continues_from — product answers are single, whole messages.
    /// The channel marker keeps the row out of companion context while replay
    /// and client history serve it normally.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_product_qa_assistant_message(
        &self,
        session_id: Uuid,
        user_message_id: Uuid,
        id: Uuid,
        content: &str,
        model: Option<&str>,
        usage: Option<&serde_json::Value>,
        generation_id: Option<&str>,
        truncated: bool,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO engine.chat_messages \
             (id, session_id, role, content, user_message_id, truncated, model, usage, \
              generation_id, assistant_action_type, channel) \
             VALUES ($1, $2, 'assistant', $3, $4, $5, $6, $7, $8, 'reply', 'product_qa')",
        )
        .bind(id)
        .bind(session_id)
        .bind(content)
        .bind(user_message_id)
        .bind(truncated)
        .bind(model)
        .bind(usage)
        .bind(generation_id)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// The user's most-recently-active OTHER voice session for this persona
    /// instance — used to quote its tail into a new voice session's bootstrap
    /// snapshot on that session's first turn. Same `(user_id, instance_id)`
    /// pair, `channel = 'voice'`, and excludes `exclude_session_id` (the
    /// session being bootstrapped) so a session never becomes its own
    /// sibling. Read-only — unlike `resume_latest_session`, does NOT bump
    /// `last_active_at`; this is a side lookup for a different session, not a
    /// resume. Rides `idx_chat_sessions_user_instance_channel` (migration
    /// 0031).
    pub async fn latest_sibling_voice_session(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
        exclude_session_id: Uuid,
    ) -> Result<Option<ChatSession>, sqlx::Error> {
        sqlx::query_as::<_, ChatSession>(
            "SELECT * FROM engine.chat_sessions \
             WHERE user_id = $1 AND instance_id = $2 AND channel = 'voice' AND id <> $3 \
             ORDER BY last_active_at DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(instance_id)
        .bind(exclude_session_id)
        .fetch_optional(self.pool)
        .await
    }

    /// Write-once snapshot of a voice session's bootstrap context into
    /// `metadata.voice_bootstrap`. Guarded by `NOT (metadata ? 'voice_bootstrap')`
    /// so a retried pipeline step, or two concurrent first-turn requests
    /// racing each other, can never clobber the original snapshot — the
    /// loser's write is a no-op, not a silent overwrite. No `COALESCE` needed:
    /// `chat_sessions.metadata` is `NOT NULL DEFAULT '{}'` (migration 0001).
    /// Returns `rows_affected`: 1 = this call wrote the snapshot; 0 = the key
    /// was already present (race lost, or already written on a prior turn) —
    /// callers treat 0 as a normal no-op, not an error.
    pub async fn set_voice_bootstrap(
        &self,
        session_id: Uuid,
        snapshot: &serde_json::Value,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(
            "UPDATE engine.chat_sessions \
             SET metadata = metadata || jsonb_build_object('voice_bootstrap', $2::jsonb) \
             WHERE id = $1 AND NOT (metadata ? 'voice_bootstrap')",
        )
        .bind(session_id)
        .bind(snapshot)
        .execute(self.pool)
        .await?;
        Ok(res.rows_affected())
    }
}

/// What the repair gate needs to know about one voice turn, in a single query.
#[derive(Debug, Clone)]
pub struct VoiceTurnRepair {
    /// An assistant reply already exists — retrying would double-bill.
    pub has_reply: bool,
    /// The turn was deliberately interrupted — there is nothing to repair.
    pub interrupted: bool,
    /// The PERSISTED utterance. The repair path regenerates from this, never
    /// from the retry body, so the turn's vector recall cannot drift between
    /// attempts.
    pub content: String,
}

/// Outcome of resolving a `client_msg_id` for the interrupt endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LatestTurnLookup {
    /// No user row with this `client_msg_id` in this session.
    NotFound,
    /// Found, but a newer user turn exists — you can only barge in on what is
    /// currently being spoken.
    Stale,
    Latest(Uuid),
}

impl<'a> ChatRepo<'a> {
    /// Read the two facts the `turn/stream` duplicate gate needs, plus the
    /// persisted utterance, in one round trip. Scoped to `role = 'user' AND
    /// channel = 'voice'` — `insert_voice_user_message`'s conflict lookup
    /// that supplies `user_message_id` is deliberately role-agnostic (the
    /// colliding row can be a `gift_user` tip row, or even a text-channel
    /// `user` row reached via `message/stream` against the same session id,
    /// since that route has no channel guard). Returns `None` when the row
    /// is not an actual voice user turn, so the caller can fall back to the
    /// ordinary unconditional 409 instead of treating a tip or text row as a
    /// repairable voice turn — regenerating against it would embed the
    /// wrong text as the recall query and write a `channel='voice'`
    /// assistant row pointing at a non-voice user row.
    pub async fn voice_turn_repair_state(
        &self,
        user_message_id: Uuid,
    ) -> Result<Option<VoiceTurnRepair>, sqlx::Error> {
        let row: Option<(String, bool, bool)> = sqlx::query_as(
            "SELECT u.content, \
                    COALESCE(u.metadata->>'voice_interrupt', 'false')::bool, \
                    EXISTS ( \
                        SELECT 1 FROM engine.chat_messages a \
                         WHERE a.user_message_id = u.id \
                           AND a.role = 'assistant' AND a.channel = 'voice' \
                    ) \
               FROM engine.chat_messages u \
              WHERE u.id = $1 AND u.role = 'user' AND u.channel = 'voice'",
        )
        .bind(user_message_id)
        .fetch_optional(self.pool)
        .await?;
        Ok(
            row.map(|(content, interrupted, has_reply)| VoiceTurnRepair {
                has_reply,
                interrupted,
                content,
            }),
        )
    }

    /// Resolve a `client_msg_id` to its **voice** user row and report whether it
    /// is the session's most recent voice user turn. Separating `NotFound` from
    /// `Stale` lets the route return 404 vs. 409 without a second round trip.
    ///
    /// Both the target row and the "latest" subquery filter `channel = 'voice'`,
    /// and that is load-bearing on both sides. A voice session can contain
    /// non-voice user rows — `routes/companion_stream.rs` has no session-channel
    /// gate, so a text turn can land in one — and without the filter on `m` such
    /// a row is interruptible, letting a caller mark it and hang a
    /// `channel='voice'` assistant reply off a text turn. Without the filter on
    /// `l`, a newer text row would make a genuine voice turn look `Stale` and
    /// block a legitimate barge-in. The comparison belongs entirely within the
    /// voice channel.
    pub async fn resolve_latest_voice_user_turn(
        &self,
        session_id: Uuid,
        client_msg_id: &str,
    ) -> Result<LatestTurnLookup, sqlx::Error> {
        let row: Option<(Uuid, bool)> = sqlx::query_as(
            "SELECT m.id, \
                    m.id = ( \
                        SELECT l.id FROM engine.chat_messages l \
                         WHERE l.session_id = $1 AND l.role = 'user' \
                           AND l.channel = 'voice' \
                         ORDER BY l.sent_at DESC, l.id DESC LIMIT 1 \
                    ) \
               FROM engine.chat_messages m \
              WHERE m.session_id = $1 AND m.client_msg_id = $2 AND m.role = 'user' \
                AND m.channel = 'voice'",
        )
        .bind(session_id)
        .bind(client_msg_id)
        .fetch_optional(self.pool)
        .await?;
        Ok(match row {
            None => LatestTurnLookup::NotFound,
            Some((_, false)) => LatestTurnLookup::Stale,
            Some((id, true)) => LatestTurnLookup::Latest(id),
        })
    }

    /// Record a deliberate barge-in: mark the user row, and make the assistant
    /// reply say what TTS actually played.
    ///
    /// `spoken_text` empty ⇒ NO assistant content is ever written (neither
    /// inserted nor overwritten); the user-row marker alone records the
    /// interrupt. That keeps the "never persist empty assistant content"
    /// invariant `run_voice_turn` holds.
    ///
    /// Two different mechanisms protect the two branches against a still-live
    /// generator, because a plain "check then write" is unsafe in general:
    /// - **non-empty `spoken_text`**: the insert carries its own `ON CONFLICT`
    ///   (rather than branching on a prior `SELECT`), so a concurrent
    ///   `insert_voice_assistant_message` cannot slip between the check and
    ///   the write — whichever arrives second just refills its own half
    ///   (content here, audit columns there).
    /// - **empty `spoken_text`**: there is no row to `ON CONFLICT` against
    ///   when none exists yet, so the race is closed differently — this
    ///   function's first statement (the marker `UPDATE ... WHERE id =
    ///   user_message_id`) takes a row lock on the user row, and
    ///   `insert_voice_assistant_message` takes the SAME lock as ITS first
    ///   statement before checking the marker. The two transactions therefore
    ///   serialize on that lock: whichever commits first is what the other
    ///   observes, so the generator's marker check can never read a stale
    ///   "not yet interrupted" state.
    pub async fn upsert_voice_interrupt(
        &self,
        session_id: Uuid,
        user_message_id: Uuid,
        candidate_assistant_id: Uuid,
        spoken_text: &str,
    ) -> Result<Option<Uuid>, sqlx::Error> {
        const MARKER: &str = r#"{"voice_interrupt": true}"#;
        let mut tx = self.pool.begin().await?;

        // This UPDATE is also the lock acquisition: it takes a row lock on
        // the user row that `insert_voice_assistant_message` takes too
        // (there, via an explicit `FOR UPDATE`) before consulting this same
        // marker. That serializes the two writers on the empty-text branch,
        // where there is no row to `ON CONFLICT` against yet. Load-bearing,
        // not incidental — do not reorder this below the branch that follows.
        sqlx::query(
            "UPDATE engine.chat_messages \
                SET metadata = COALESCE(metadata, '{}'::jsonb) || $2::jsonb \
              WHERE id = $1",
        )
        .bind(user_message_id)
        .bind(MARKER)
        .execute(&mut *tx)
        .await?;

        let assistant_id: Option<Uuid> = if spoken_text.is_empty() {
            sqlx::query_scalar(
                "UPDATE engine.chat_messages \
                    SET metadata = COALESCE(metadata, '{}'::jsonb) || $2::jsonb \
                  WHERE user_message_id = $1 AND role = 'assistant' AND channel = 'voice' \
                  RETURNING id",
            )
            .bind(user_message_id)
            .bind(MARKER)
            .fetch_optional(&mut *tx)
            .await?
        } else {
            Some(
                sqlx::query_scalar(
                    "INSERT INTO engine.chat_messages AS m \
                     (id, session_id, role, content, user_message_id, truncated, \
                      assistant_action_type, channel, metadata) \
                     VALUES ($1, $2, 'assistant', $3, $4, true, 'reply', 'voice', $5::jsonb) \
                     ON CONFLICT (user_message_id) WHERE role = 'assistant' AND channel = 'voice' \
                     DO UPDATE SET content = EXCLUDED.content, \
                                   truncated = true, \
                                   metadata = COALESCE(m.metadata, '{}'::jsonb) || EXCLUDED.metadata \
                     RETURNING m.id",
                )
                .bind(candidate_assistant_id)
                .bind(session_id)
                .bind(spoken_text)
                .bind(user_message_id)
                .bind(MARKER)
                .fetch_one(&mut *tx)
                .await?,
            )
        };

        sqlx::query("UPDATE engine.chat_sessions SET last_active_at = now() WHERE id = $1")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(assistant_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::seed_persona_instance;

    /// A session on a freshly seeded persona instance, for the many tests that
    /// just need "some session" and never look at the ids. Since 0040 the FK on
    /// `chat_sessions.instance_id` rejects a fabricated `Uuid::new_v4()`.
    async fn throwaway_session(pool: &PgPool) -> ChatSession {
        let owner = Uuid::new_v4();
        let instance_id = seed_persona_instance(pool, owner).await;
        ChatRepo { pool }
            .create_session(owner, instance_id)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn create_then_retrieve_session(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let s = repo.create_session(user_id, instance_id).await.unwrap();
        let loaded = repo.get_session(s.id).await.unwrap().unwrap();
        assert_eq!(loaded.user_id, user_id);
        assert_eq!(loaded.instance_id, Some(instance_id));
        assert!(!loaded.is_converted);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn append_message_and_history_roundtrip(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
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
        let instance_id = seed_persona_instance(&pool, user_id).await;
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
        let instance_id = seed_persona_instance(&pool, user_id).await;
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
    async fn history_slim_exposes_channel(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let s = repo.create_session(user_id, instance_id).await.unwrap();

        repo.append_message(s.id, "user", "normal turn")
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, channel) \
             VALUES ($1, 'assistant', 'answer', 'product_qa')",
        )
        .bind(s.id)
        .execute(&pool)
        .await
        .unwrap();

        let slim = repo.history_slim(s.id, 50, 0).await.unwrap();
        assert_eq!(slim.len(), 2);
        assert_eq!(slim[0].channel, None, "normal turn has no channel marker");
        assert_eq!(
            slim[1].channel.as_deref(),
            Some("product_qa"),
            "product_qa projection must surface the channel column"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn list_sessions_for_user(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let other_user = Uuid::new_v4();

        let i1 = seed_persona_instance(&pool, user_id).await;
        let i2 = seed_persona_instance(&pool, user_id).await;
        let i3 = seed_persona_instance(&pool, other_user).await;

        repo.create_session(user_id, i1).await.unwrap();
        repo.create_session_with_metadata(user_id, i2, serde_json::json!({}), "voice")
            .await
            .unwrap();
        repo.create_session(other_user, i3).await.unwrap();

        let sessions = repo.list_sessions(user_id).await.unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().all(|s| s.user_id == user_id));
        // channel is exposed on the projection and distinguishes the two
        // sessions (the text default vs. the explicit voice session above).
        let channels: std::collections::HashSet<&str> =
            sessions.iter().map(|s| s.channel.as_str()).collect();
        assert_eq!(
            channels,
            ["text", "voice"]
                .into_iter()
                .collect::<std::collections::HashSet<_>>()
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn create_or_resume_returns_existing(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let first = repo.create_session(user_id, instance_id).await.unwrap();
        let resumed = repo.create_or_resume(user_id, instance_id).await.unwrap();
        assert_eq!(first.id, resumed.id);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_user_message_idempotent_first_insert(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let s = repo.create_session(user_id, instance_id).await.unwrap();

        let outcome = repo
            .upsert_user_message_idempotent(
                s.id,
                "hello",
                "01J0000000000000000000000A",
                "user",
                None,
            )
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
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let s = repo.create_session(user_id, instance_id).await.unwrap();

        let first = match repo
            .upsert_user_message_idempotent(
                s.id,
                "hello",
                "01J0000000000000000000000A",
                "user",
                None,
            )
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
                metadata: None,
            }],
        )
        .await
        .unwrap();

        let outcome = repo
            .upsert_user_message_idempotent(
                s.id,
                "hello",
                "01J0000000000000000000000A",
                "user",
                None,
            )
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
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let s = repo.create_session(user_id, instance_id).await.unwrap();

        let first = match repo
            .upsert_user_message_idempotent(
                s.id,
                "hello",
                "01J0000000000000000000000A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            o => panic!("expected Inserted, got {o:?}"),
        };

        match repo
            .upsert_user_message_idempotent(
                s.id,
                "hello",
                "01J0000000000000000000000A",
                "user",
                None,
            )
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
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let s = repo.create_session(user_id, instance_id).await.unwrap();

        let first = match repo
            .upsert_user_message_idempotent(
                s.id,
                "hello",
                "01J0000000000000000000000A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            o => panic!("expected Inserted, got {o:?}"),
        };
        repo.mark_user_message_ghosted(first).await.unwrap();

        match repo
            .upsert_user_message_idempotent(
                s.id,
                "hello",
                "01J0000000000000000000000A",
                "user",
                None,
            )
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
        let instance_id = seed_persona_instance(&pool, user_id).await;

        // no session yet → None
        assert!(repo
            .resume_latest_session(user_id, instance_id, "text")
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
            .resume_latest_session(user_id, instance_id, "text")
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
        let s = throwaway_session(&pool).await;
        let outcome = repo
            .upsert_user_message_idempotent(s.id, "hi", "01J0000000000000000000000A", "user", None)
            .await
            .unwrap();
        match outcome {
            UpsertUserOutcome::Inserted { .. } => {}
            other => panic!("expected Inserted, got {other:?}"),
        }
        let (role, metadata): (String, Option<serde_json::Value>) =
            sqlx::query_as("SELECT role, metadata FROM engine.chat_messages WHERE session_id = $1")
                .bind(s.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(role, "user");
        assert!(
            metadata.is_none(),
            "metadata should be NULL on plain user rows"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_user_message_writes_gift_user_role_and_tip_metadata(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        let meta = serde_json::json!({ "tips_amount_usd": 20.0, "tier": "gold" });
        repo.upsert_user_message_idempotent(
            s.id,
            "(打赏 $20)",
            "01J0000000000000000000000B",
            "gift_user",
            Some(&meta),
        )
        .await
        .unwrap();
        let (role, metadata): (String, serde_json::Value) =
            sqlx::query_as("SELECT role, metadata FROM engine.chat_messages WHERE session_id = $1")
                .bind(s.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(role, "gift_user");
        assert_eq!(metadata["tips_amount_usd"].as_f64(), Some(20.0));
        assert_eq!(metadata["tier"], serde_json::json!("gold"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_user_message_replay_finds_gift_user_row(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
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
            other => panic!("expected DuplicateInProgress, got {other:?}"),
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    #[allow(clippy::type_complexity)] // sqlx tuple query — type alias would add noise
    async fn assistant_batch_round_trips_filter_audit(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        let user_msg_id = match repo
            .upsert_user_message_idempotent(s.id, "hi", "01J0000000000000000000001A", "user", None)
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("{other:?}"),
        };

        let triggers = serde_json::json!({
            "random": 0.3,
            "models": ["deepseek/deepseek-v4-flash"],
            "traits": { "any": ["nsfw_boost"], "when": "absent" }
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
            metadata: None,
        };
        repo.insert_assistant_batch(s.id, user_msg_id, &[row])
            .await
            .unwrap();

        let (content, pre_filter, filter_model, filter_triggers, f_client, f_gen): (
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
        let s = throwaway_session(&pool).await;
        let user_msg_id = match repo
            .upsert_user_message_idempotent(s.id, "hi", "01J0000000000000000000002A", "user", None)
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
            metadata: None,
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
            sqlx::query(&q)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("expected column {col} on engine.chat_messages: {e}"));
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
        let s = throwaway_session(&pool).await;
        let user_msg_id = match repo
            .upsert_user_message_idempotent(s.id, "hi", "01J0000000000000000000003A", "user", None)
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
            metadata: None,
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
        assert!(
            f_gen.is_none(),
            "f_generation_id should be NULL when None inside Some(FilterAudit)"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn assistant_batch_filter_triggers_json_null_persists_as_sql_null(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        let user_msg_id = match repo
            .upsert_user_message_idempotent(s.id, "hi", "01J0000000000000000000004A", "user", None)
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("{other:?}"),
        };
        // Empty-trigger case: filter ran (filter_model set) but no predicates
        // fired, so filter_triggers is JSON null → must land as SQL NULL.
        let row = AssistantInsert {
            id: Uuid::new_v4(),
            content: "filtered empty-trigger".into(),
            assistant_action_type: "reply".into(),
            continues_from_message_id: None,
            truncated: false,
            model: Some("anthropic/claude-sonnet-4.6".into()),
            usage: None,
            generation_id: Some("gen_chat_xyz".into()),
            filter_audit: Some(FilterAudit {
                pre_filter_content: "raw".into(),
                filter_model: "anthropic/claude-haiku-4.5".into(),
                filter_triggers: serde_json::Value::Null,
                f_client_msg_id: "f_01J0000000000000000000004Z".into(),
                f_generation_id: None,
            }),
            metadata: None,
        };
        repo.insert_assistant_batch(s.id, user_msg_id, &[row])
            .await
            .unwrap();
        // filter_triggers IS NULL (SQL NULL), but filter_model IS NOT NULL —
        // the "filter ran with no predicates" signal.
        let (triggers_is_null, model_is_set): (bool, bool) = sqlx::query_as(
            "SELECT filter_triggers IS NULL, filter_model IS NOT NULL \
             FROM engine.chat_messages \
             WHERE role='assistant' AND session_id=$1",
        )
        .bind(s.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(triggers_is_null, "JSON null must persist as SQL NULL");
        assert!(
            model_is_set,
            "filter_model retained as the filter-ran signal"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn assistant_batch_persists_metadata_with_prompt_traits(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        let user_msg_id = match repo
            .upsert_user_message_idempotent(s.id, "hi", "01J0000000000000000000004A", "user", None)
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("{other:?}"),
        };
        let metadata = serde_json::json!({ "prompt_traits": ["nsfw_boost", "tsundere"] });
        let row = AssistantInsert {
            id: Uuid::new_v4(),
            content: "hi".into(),
            assistant_action_type: "reply".into(),
            continues_from_message_id: None,
            truncated: false,
            model: Some("anthropic/claude-sonnet-4.6".into()),
            usage: None,
            generation_id: Some("gen_chat_xyz".into()),
            filter_audit: None,
            metadata: Some(metadata.clone()),
        };
        repo.insert_assistant_batch(s.id, user_msg_id, &[row])
            .await
            .unwrap();
        let m: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT metadata FROM engine.chat_messages \
             WHERE role='assistant' AND session_id=$1",
        )
        .bind(s.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(m, Some(metadata));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn assistant_batch_metadata_default_null(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        let user_msg_id = match repo
            .upsert_user_message_idempotent(s.id, "hi", "01J0000000000000000000005A", "user", None)
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("{other:?}"),
        };
        let row = AssistantInsert {
            id: Uuid::new_v4(),
            content: "hi".into(),
            assistant_action_type: "reply".into(),
            continues_from_message_id: None,
            truncated: false,
            model: Some("anthropic/claude-sonnet-4.6".into()),
            usage: None,
            generation_id: Some("gen_chat_xyz".into()),
            filter_audit: None,
            metadata: None,
        };
        repo.insert_assistant_batch(s.id, user_msg_id, &[row])
            .await
            .unwrap();
        let m: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT metadata FROM engine.chat_messages \
             WHERE role='assistant' AND session_id=$1",
        )
        .bind(s.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(m.is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn assistant_batch_persists_metadata_with_tier(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        let user_msg_id = match repo
            .upsert_user_message_idempotent(s.id, "hi", "01J0000000000000000000006A", "user", None)
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("{other:?}"),
        };
        let metadata = serde_json::json!({
            "prompt_traits": ["nsfw_boost"],
            "tier": "gold"
        });
        let row = AssistantInsert {
            id: Uuid::new_v4(),
            content: "hi".into(),
            assistant_action_type: "reply".into(),
            continues_from_message_id: None,
            truncated: false,
            model: Some("anthropic/claude-sonnet-4.6".into()),
            usage: None,
            generation_id: Some("gen_chat_xyz".into()),
            filter_audit: None,
            metadata: Some(metadata.clone()),
        };
        repo.insert_assistant_batch(s.id, user_msg_id, &[row])
            .await
            .unwrap();
        let m: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT metadata FROM engine.chat_messages \
             WHERE role='assistant' AND session_id=$1",
        )
        .bind(s.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(m, Some(metadata));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recent_turn_pairs_empty_session_returns_empty(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        let cutoff = Utc::now() + chrono::Duration::seconds(60);
        let pairs = repo.recent_turn_pairs(s.id, cutoff, 3).await.unwrap();
        assert!(pairs.is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recent_turn_pairs_single_pair_returned(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        let user_msg_id = match repo
            .upsert_user_message_idempotent(s.id, "hi", "01J0000000000000000010001A", "user", None)
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("expected Inserted, got {other:?}"),
        };
        let row = AssistantInsert {
            id: Uuid::new_v4(),
            content: "hi back".into(),
            assistant_action_type: "reply".into(),
            continues_from_message_id: None,
            truncated: false,
            model: Some("test-model".into()),
            usage: None,
            generation_id: None,
            filter_audit: None,
            metadata: None,
        };
        repo.insert_assistant_batch(s.id, user_msg_id, &[row])
            .await
            .unwrap();

        let cutoff = Utc::now() + chrono::Duration::seconds(60);
        let pairs = repo.recent_turn_pairs(s.id, cutoff, 3).await.unwrap();
        assert_eq!(pairs, vec![("hi".to_string(), "hi back".to_string())]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recent_turn_pairs_skips_truncated_assistant(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;

        let u1 = match repo
            .upsert_user_message_idempotent(s.id, "u1", "01J0000000000000000020001A", "user", None)
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("expected Inserted, got {other:?}"),
        };
        repo.insert_assistant_batch(
            s.id,
            u1,
            &[AssistantInsert {
                id: Uuid::new_v4(),
                content: "a1".into(),
                assistant_action_type: "reply".into(),
                continues_from_message_id: None,
                truncated: true,
                model: Some("m".into()),
                usage: None,
                generation_id: None,
                filter_audit: None,
                metadata: None,
            }],
        )
        .await
        .unwrap();

        let u2 = match repo
            .upsert_user_message_idempotent(s.id, "u2", "01J0000000000000000020003A", "user", None)
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("expected Inserted, got {other:?}"),
        };
        repo.insert_assistant_batch(
            s.id,
            u2,
            &[AssistantInsert {
                id: Uuid::new_v4(),
                content: "a2".into(),
                assistant_action_type: "reply".into(),
                continues_from_message_id: None,
                truncated: false,
                model: Some("m".into()),
                usage: None,
                generation_id: None,
                filter_audit: None,
                metadata: None,
            }],
        )
        .await
        .unwrap();

        let cutoff = Utc::now() + chrono::Duration::seconds(60);
        let pairs = repo.recent_turn_pairs(s.id, cutoff, 3).await.unwrap();
        // Truncated assistant excluded by WHERE; u1 then has no following assistant
        // and is dropped as an orphan.
        assert_eq!(pairs, vec![("u2".to_string(), "a2".to_string())]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recent_turn_pairs_returns_latest_three_when_more_exist(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        // Insert 5 complete pairs.
        for n in 0..5u8 {
            // ULIDs must be unique and monotonic. Use n in the suffix:
            let u_ulid = format!("01J000000000000000003{n}001A");
            let user_id = match repo
                .upsert_user_message_idempotent(s.id, &format!("u{n}"), &u_ulid, "user", None)
                .await
                .unwrap()
            {
                UpsertUserOutcome::Inserted { message_id } => message_id,
                other => panic!("expected Inserted, got {other:?}"),
            };
            repo.insert_assistant_batch(
                s.id,
                user_id,
                &[AssistantInsert {
                    id: Uuid::new_v4(),
                    content: format!("a{n}"),
                    assistant_action_type: "reply".into(),
                    continues_from_message_id: None,
                    truncated: false,
                    model: Some("m".into()),
                    usage: None,
                    generation_id: None,
                    filter_audit: None,
                    metadata: None,
                }],
            )
            .await
            .unwrap();
        }

        let cutoff = Utc::now() + chrono::Duration::seconds(60);
        let pairs = repo.recent_turn_pairs(s.id, cutoff, 3).await.unwrap();
        assert_eq!(
            pairs,
            vec![
                ("u2".to_string(), "a2".to_string()),
                ("u3".to_string(), "a3".to_string()),
                ("u4".to_string(), "a4".to_string()),
            ]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recent_turn_pairs_cutoff_excludes_current_turn(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        let u1 = match repo
            .upsert_user_message_idempotent(s.id, "u1", "01J0000000000000000040001A", "user", None)
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("expected Inserted, got {other:?}"),
        };
        repo.insert_assistant_batch(
            s.id,
            u1,
            &[AssistantInsert {
                id: Uuid::new_v4(),
                content: "a1".into(),
                assistant_action_type: "reply".into(),
                continues_from_message_id: None,
                truncated: false,
                model: Some("m".into()),
                usage: None,
                generation_id: None,
                filter_audit: None,
                metadata: None,
            }],
        )
        .await
        .unwrap();

        // Second user row inserted AFTER the cutoff timestamp we'll use.
        let cutoff = Utc::now();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let _ = repo
            .upsert_user_message_idempotent(
                s.id,
                "current",
                "01J0000000000000000040003A",
                "user",
                None,
            )
            .await
            .unwrap();

        let pairs = repo.recent_turn_pairs(s.id, cutoff, 3).await.unwrap();
        assert_eq!(pairs, vec![("u1".to_string(), "a1".to_string())]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recent_turn_pairs_drops_orphan_user_at_end(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        let u1 = match repo
            .upsert_user_message_idempotent(s.id, "u1", "01J0000000000000000050001A", "user", None)
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("expected Inserted, got {other:?}"),
        };
        repo.insert_assistant_batch(
            s.id,
            u1,
            &[AssistantInsert {
                id: Uuid::new_v4(),
                content: "a1".into(),
                assistant_action_type: "reply".into(),
                continues_from_message_id: None,
                truncated: false,
                model: Some("m".into()),
                usage: None,
                generation_id: None,
                filter_audit: None,
                metadata: None,
            }],
        )
        .await
        .unwrap();
        let _ = repo
            .upsert_user_message_idempotent(s.id, "u2", "01J0000000000000000050003A", "user", None)
            .await
            .unwrap();

        let cutoff = Utc::now() + chrono::Duration::seconds(60);
        let pairs = repo.recent_turn_pairs(s.id, cutoff, 3).await.unwrap();
        assert_eq!(pairs, vec![("u1".to_string(), "a1".to_string())]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recent_turn_pairs_includes_gift_user_pair(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        let meta = serde_json::json!({"tips_amount_usd": 20.0});
        let u1 = match repo
            .upsert_user_message_idempotent(
                s.id,
                "(打赏 $20)",
                "01J0000000000000000060001A",
                "gift_user",
                Some(&meta),
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("expected Inserted, got {other:?}"),
        };
        repo.insert_assistant_batch(
            s.id,
            u1,
            &[AssistantInsert {
                id: Uuid::new_v4(),
                content: "thanks!".into(),
                assistant_action_type: "reply".into(),
                continues_from_message_id: None,
                truncated: false,
                model: Some("m".into()),
                usage: None,
                generation_id: None,
                filter_audit: None,
                metadata: None,
            }],
        )
        .await
        .unwrap();

        let cutoff = Utc::now() + chrono::Duration::seconds(60);
        let pairs = repo.recent_turn_pairs(s.id, cutoff, 3).await.unwrap();
        assert_eq!(
            pairs,
            vec![("(打赏 $20)".to_string(), "thanks!".to_string())]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn set_user_input_rewrite_stamps_audit_keeps_content(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let session = throwaway_session(&pool).await;
        let umid = match repo
            .upsert_user_message_idempotent(
                session.id,
                "1111",
                "01J0000000000000000000000A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        repo.set_user_input_rewrite(
            umid,
            "那你平常都怎么放松呀？",
            "fast/in",
            Some("meaningless digits"),
            Some("gen-x"),
        )
        .await
        .unwrap();

        let (content, pre, model, triggers, fgen): (
            String,
            Option<String>,
            Option<String>,
            Option<serde_json::Value>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT content, pre_filter_content, filter_model, filter_triggers, f_generation_id \
             FROM engine.chat_messages WHERE id = $1",
        )
        .bind(umid)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(content, "1111", "original content must be preserved");
        assert_eq!(pre.as_deref(), Some("那你平常都怎么放松呀？"));
        assert_eq!(model.as_deref(), Some("fast/in"));
        assert_eq!(
            triggers,
            Some(serde_json::json!({"reason": "meaningless digits"}))
        );
        assert_eq!(fgen.as_deref(), Some("gen-x"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn set_user_input_rewrite_blank_reason_leaves_triggers_null(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let session = throwaway_session(&pool).await;
        let umid = match repo
            .upsert_user_message_idempotent(
                session.id,
                "????",
                "01J0000000000000000000000B",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };
        repo.set_user_input_rewrite(umid, "你今天过得怎么样？", "fast/in", Some("  "), None)
            .await
            .unwrap();
        let triggers: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT filter_triggers FROM engine.chat_messages WHERE id = $1")
                .bind(umid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            triggers.is_none(),
            "blank reason → SQL NULL filter_triggers"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recent_turn_pairs_before_message_uses_msg_sent_at_not_now(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;

        // Insert 1 prior complete pair.
        let u1 = match repo
            .upsert_user_message_idempotent(s.id, "u1", "01J0000000000000000090001A", "user", None)
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("expected Inserted, got {other:?}"),
        };
        repo.insert_assistant_batch(
            s.id,
            u1,
            &[AssistantInsert {
                id: Uuid::new_v4(),
                content: "a1".into(),
                assistant_action_type: "reply".into(),
                truncated: false,
                continues_from_message_id: None,
                model: Some("m".into()),
                usage: None,
                generation_id: None,
                filter_audit: None,
                metadata: None,
            }],
        )
        .await
        .unwrap();

        // Insert the "current" user row that the chat handler will pass.
        let current = match repo
            .upsert_user_message_idempotent(
                s.id,
                "current",
                "01J0000000000000000090002A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        // Sleep, then insert a LATER user row — simulating a concurrent stream
        // that completed between this turn's user-insert and the recent-turn fetch.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let _ = repo
            .upsert_user_message_idempotent(
                s.id,
                "later",
                "01J0000000000000000090003A",
                "user",
                None,
            )
            .await
            .unwrap();

        // The cutoff must come from `current`'s sent_at — NOT the wall clock,
        // because wall-clock-now happens AFTER the `later` insert and would
        // include it. With the proper cutoff, `later` is excluded.
        let pairs = repo
            .recent_turn_pairs_before_message(s.id, current, 3)
            .await
            .unwrap();
        assert_eq!(pairs, vec![("u1".to_string(), "a1".to_string())]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn history_row_exposes_metadata_column(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let s = repo.create_session(user_id, instance_id).await.unwrap();

        let meta = serde_json::json!({ "image_url": "https://x/y.png" });
        repo.upsert_user_message_idempotent(
            s.id,
            "hi",
            "01J0000000000000000000000A",
            "user",
            Some(&meta),
        )
        .await
        .unwrap();

        let rows = repo.history(s.id, 10, 0).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].metadata.as_ref().unwrap()["image_url"],
            "https://x/y.png"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn set_user_image_vision_merges_and_preserves(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let s = repo.create_session(user_id, instance_id).await.unwrap();

        let seed = serde_json::json!({ "image_url": "https://x/y.png" });
        let outcome = repo
            .upsert_user_message_idempotent(
                s.id,
                "",
                "01J000000000000000000VISION",
                "user",
                Some(&seed),
            )
            .await
            .unwrap();
        let uid = match outcome {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        let vision = serde_json::json!({
            "description": "a cat", "ocr_text": "", "people": "", "scene": "indoors"
        });
        repo.set_user_image_vision(uid, &vision, "vision-model", Some("gen-1"))
            .await
            .unwrap();

        let rows = repo.history(s.id, 10, 0).await.unwrap();
        let meta = rows[0].metadata.as_ref().unwrap();
        assert_eq!(meta["image_url"], "https://x/y.png"); // preserved
        assert_eq!(meta["vision"]["description"], "a cat");
        assert_eq!(meta["vision_model"], "vision-model");
        assert_eq!(meta["vision_generation_id"], "gen-1");
        assert_eq!(rows[0].content, ""); // untouched
    }

    /// 0027 cleanup: legacy non-tip gift_user rows (no tips_amount_usd metadata)
    /// are deleted; tips (gift_user with tips_amount_usd) and user rows survive.
    /// Mirrors the 0018 backfill-test pattern: #[sqlx::test] runs 0027 on the
    /// empty DB (a no-op), then we seed rows and re-run the embedded DELETE
    /// (valid because DELETE is idempotent).
    const CLEANUP_SQL_0027: &str =
        include_str!("../migrations/0027_drop_legacy_gift_user_rows.sql");

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0027_drops_legacy_non_tip_gift_user_rows(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, metadata) VALUES \
                 ($1, 'user', 'hi', NULL), \
                 ($1, 'gift_user', '(打赏 $20)', '{\"tips_amount_usd\": 20.0}'::jsonb), \
                 ($1, 'gift_user', 'rose', NULL), \
                 ($1, 'gift_user', 'rose', '{\"label\": \"rose\"}'::jsonb)",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        // Run the embedded cleanup migration.
        sqlx::query(CLEANUP_SQL_0027).execute(&pool).await.unwrap();

        // Only the tip gift_user row survives.
        let gift_user_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'gift_user'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(gift_user_count, 1, "only the tip gift_user row survives");

        // The survivor is the tip (carries tips_amount_usd).
        let surviving_tip: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'gift_user' \
               AND metadata ? 'tips_amount_usd'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(surviving_tip, 1, "the surviving gift_user row is the tip");

        // user rows are untouched.
        let user_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'user'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(user_count, 1, "user rows are untouched");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn assistant_image_meta_merge(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        // minimal session + user row + assistant row
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let s = repo.create_session(user_id, instance_id).await.unwrap();
        let u = match repo
            .upsert_user_message_idempotent(
                s.id,
                "hi",
                "client-msg-id-abcdefghij012345",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => panic!(),
        };
        let aid = Uuid::new_v4();
        repo.insert_assistant_batch(
            s.id,
            u,
            &[AssistantInsert {
                id: aid,
                content: String::new(),
                assistant_action_type: "reply".into(),
                continues_from_message_id: None,
                truncated: false,
                model: None,
                usage: None,
                generation_id: None,
                filter_audit: None,
                metadata: None,
            }],
        )
        .await
        .unwrap();

        let image = serde_json::json!({"prompt":"a cat","style":"realistic","model":"img-a"});
        repo.merge_assistant_image_meta(s.id, aid, &image)
            .await
            .unwrap();

        let meta: serde_json::Value =
            sqlx::query_scalar("SELECT metadata FROM engine.chat_messages WHERE id = $1")
                .bind(aid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(meta["image"]["prompt"], "a cat");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recent_assistant_contents_excludes_current_and_truncated(pool: PgPool) {
        // `AssistantInsert` / `ChatRepo` / `UpsertUserOutcome` are already in
        // scope via the test module's `use super::*;`.
        let chat_repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session = chat_repo
            .create_session(user_id, instance_id)
            .await
            .unwrap();

        // Two prior complete (user, assistant) pairs; the 2nd assistant row is
        // a TRUNCATED partial that must be excluded.
        for n in 0..2u8 {
            let ulid = format!("01J000000000000000008{n}001A");
            let uid = match chat_repo
                .upsert_user_message_idempotent(session.id, &format!("u{n}"), &ulid, "user", None)
                .await
                .unwrap()
            {
                UpsertUserOutcome::Inserted { message_id } => message_id,
                other => panic!("expected Inserted, got {other:?}"),
            };
            chat_repo
                .insert_assistant_batch(
                    session.id,
                    uid,
                    &[AssistantInsert {
                        id: Uuid::new_v4(),
                        content: format!("assistant-{n}"),
                        assistant_action_type: "reply".into(),
                        truncated: n == 1, // second one is a truncated partial
                        continues_from_message_id: None,
                        model: Some("test-model".into()),
                        usage: None,
                        generation_id: None,
                        filter_audit: None,
                        metadata: None,
                    }],
                )
                .await
                .unwrap();
        }

        // The "current" user row — its sent_at is the cutoff.
        let current = match chat_repo
            .upsert_user_message_idempotent(
                session.id,
                "u_current",
                "01J0000000000000000080900A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        let got = chat_repo
            .recent_assistant_contents(session.id, current, 6)
            .await
            .unwrap();
        // Only the non-truncated assistant row, newest-first.
        assert_eq!(got, vec!["assistant-0".to_string()]);
    }

    async fn insert_at(
        pool: &PgPool,
        session_id: Uuid,
        role: &str,
        content: &str,
        sent_at: DateTime<Utc>,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
             VALUES ($1, $2, $3, $4) RETURNING id",
        )
        .bind(session_id)
        .bind(role)
        .bind(content)
        .bind(sent_at)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn message_sent_at_in_session_scopes_by_session(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        let other = throwaway_session(&pool).await;
        let base = chrono::Utc::now();
        let m = insert_at(&pool, s.id, "user", "hi", base).await;
        let n = insert_at(&pool, other.id, "user", "yo", base).await;

        assert!(repo
            .message_sent_at_in_session(s.id, m)
            .await
            .unwrap()
            .is_some());
        // id exists but in a different session → None.
        assert!(repo
            .message_sent_at_in_session(s.id, n)
            .await
            .unwrap()
            .is_none());
        // nonexistent id → None.
        assert!(repo
            .message_sent_at_in_session(s.id, Uuid::new_v4())
            .await
            .unwrap()
            .is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn history_anchored_rewind_drops_between_and_keeps_m_and_u(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        let base = chrono::Utc::now();
        // A B M C D E U with strictly increasing sent_at.
        let labels = ["A", "B", "M", "C", "D", "E", "U"];
        let mut ids = Vec::new();
        for (i, label) in labels.iter().enumerate() {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            ids.push(
                insert_at(
                    &pool,
                    s.id,
                    role,
                    label,
                    base + chrono::Duration::seconds(i as i64),
                )
                .await,
            );
        }
        let m = ids[2];
        let u = ids[6];
        let m_ts = repo
            .message_sent_at_in_session(s.id, m)
            .await
            .unwrap()
            .unwrap();

        let rows = repo
            .history_anchored(s.id, u, Some(m_ts), 20)
            .await
            .unwrap();
        let got: Vec<&str> = rows.iter().map(|r| r.content.as_str()).collect();
        assert_eq!(
            got,
            vec!["A", "B", "M", "U"],
            "C/D/E dropped; M and U kept, chronological"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn history_anchored_drop_history_returns_only_current(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        let base = chrono::Utc::now();
        insert_at(&pool, s.id, "user", "old1", base).await;
        insert_at(
            &pool,
            s.id,
            "assistant",
            "old2",
            base + chrono::Duration::seconds(1),
        )
        .await;
        let u = insert_at(
            &pool,
            s.id,
            "user",
            "current",
            base + chrono::Duration::seconds(2),
        )
        .await;

        let rows = repo.history_anchored(s.id, u, None, 20).await.unwrap();
        let got: Vec<&str> = rows.iter().map(|r| r.content.as_str()).collect();
        assert_eq!(
            got,
            vec!["current"],
            "DropHistory → only the current message"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn history_anchored_rewind_counts_current_toward_limit(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        let base = chrono::Utc::now();
        // A B M C D E U with strictly increasing sent_at.
        let labels = ["A", "B", "M", "C", "D", "E", "U"];
        let mut ids = Vec::new();
        for (i, label) in labels.iter().enumerate() {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            ids.push(
                insert_at(
                    &pool,
                    s.id,
                    role,
                    label,
                    base + chrono::Duration::seconds(i as i64),
                )
                .await,
            );
        }
        let m = ids[2];
        let u = ids[6];
        let m_ts = repo
            .message_sent_at_in_session(s.id, m)
            .await
            .unwrap()
            .unwrap();

        // limit=3: qualifying rows are {A,B,M (sent_at<=ts)} + {U}; DESC = U,M,B,A; LIMIT 3 = U,M,B;
        // reversed = [B, M, U]. The current message U always survives (highest sent_at sorts first
        // under DESC); the oldest below-anchor row (A) is trimmed. U counts toward the limit.
        let rows = repo.history_anchored(s.id, u, Some(m_ts), 3).await.unwrap();
        let got: Vec<&str> = rows.iter().map(|r| r.content.as_str()).collect();
        assert_eq!(
            got,
            vec!["B", "M", "U"],
            "limit caps total incl. U; oldest (A) trimmed; U survives"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn merge_assistant_metadata_key_writes_under_named_key(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        let user_msg_id = match repo
            .upsert_user_message_idempotent(s.id, "hi", "01J0000000000000000000002A", "user", None)
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("{other:?}"),
        };
        let msg_id = Uuid::new_v4();
        repo.insert_assistant_batch(
            s.id,
            user_msg_id,
            &[AssistantInsert {
                id: msg_id,
                content: "fell through to text".into(),
                assistant_action_type: "reply".into(),
                continues_from_message_id: None,
                truncated: false,
                model: Some("m".into()),
                usage: None,
                generation_id: None,
                filter_audit: None,
                metadata: None,
            }],
        )
        .await
        .unwrap();

        let value = serde_json::json!({ "prompt": "a selfie", "attempts": [] });
        repo.merge_assistant_metadata_key(s.id, msg_id, "image_failed", &value)
            .await
            .unwrap();

        let meta: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT metadata FROM engine.chat_messages WHERE id = $1")
                .bind(msg_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let meta = meta.unwrap();
        assert_eq!(meta["image_failed"]["prompt"], "a selfie");

        // Wrong session must not write (IDOR guard).
        repo.merge_assistant_metadata_key(Uuid::new_v4(), msg_id, "x", &serde_json::json!(1))
            .await
            .unwrap();
        let meta2: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT metadata FROM engine.chat_messages WHERE id = $1")
                .bind(msg_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let meta2 = meta2.unwrap();
        assert!(
            meta2.get("x").is_none(),
            "cross-session write must be a no-op"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn channel_column_accepts_voice_and_null_rejects_other(pool: PgPool) {
        // Minimal session to satisfy FKs.
        let session_id = throwaway_session(&pool).await.id;

        // NULL channel (default) — ok.
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content) VALUES ($1, 'user', 'a')",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        // 'voice' — ok.
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, channel) VALUES ($1, 'user', 'b', 'voice')",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        // 'bogus' — rejected by CHECK.
        let err = sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, channel) VALUES ($1, 'user', 'c', 'bogus')",
        )
        .bind(session_id)
        .execute(&pool)
        .await;
        assert!(
            err.is_err(),
            "channel = 'bogus' must violate the CHECK constraint"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn voice_inserts_are_marked_and_idempotent(pool: PgPool) {
        let session_id = throwaway_session(&pool).await.id;
        let repo = ChatRepo { pool: &pool };

        // User insert: idempotent on client_msg_id.
        let cmid = "01J9000000000000000000VOICE";
        let u1 = match repo
            .insert_voice_user_message(session_id, "hello", cmid)
            .await
            .unwrap()
        {
            VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };
        match repo
            .insert_voice_user_message(session_id, "hello again", cmid)
            .await
            .unwrap()
        {
            VoiceUserInsert::Duplicate(id) => {
                assert_eq!(id, u1, "same client_msg_id must return the same row id");
            }
            other => panic!("expected Duplicate, got {other:?}"),
        }

        let (role, channel): (String, Option<String>) =
            sqlx::query_as("SELECT role, channel FROM engine.chat_messages WHERE id = $1")
                .bind(u1)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(role, "user");
        assert_eq!(channel.as_deref(), Some("voice"));

        // Assistant insert.
        let aid: Uuid = ulid::Ulid::new().into();
        repo.insert_voice_assistant_message(
            session_id,
            u1,
            aid,
            "hi there",
            Some("primary"),
            None,
            Some("gen-1"),
            false,
            Some(&serde_json::json!({"relationship_scope": "both"})),
        )
        .await
        .unwrap();
        let (role, aat, channel, scope): (String, Option<String>, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT role, assistant_action_type, channel, metadata->>'relationship_scope' \
                 FROM engine.chat_messages WHERE id = $1",
            )
            .bind(aid)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(role, "assistant");
        assert_eq!(aat.as_deref(), Some("reply"));
        assert_eq!(channel.as_deref(), Some("voice"));
        assert_eq!(scope.as_deref(), Some("both"));

        // history() returns both, chronologically, and content survives.
        let h = repo.history(session_id, 10, 0).await.unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].role, "user");
        assert_eq!(h[1].role, "assistant");
        assert_eq!(h[1].content, "hi there");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn voice_insert_duplicate_against_gift_user_row_is_not_500(pool: PgPool) {
        // A voice request reusing a client_msg_id that already exists on a tip
        // (`gift_user`) row must be classified as Duplicate (→ 409), not error
        // out with RowNotFound (→ 500). The unique index is role-agnostic, so
        // the conflict-recovery SELECT must not filter by role.
        let session_id = throwaway_session(&pool).await.id;
        let cmid = "01J9000000000000000000GIFT1";
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, client_msg_id) \
             VALUES ($1, 'gift_user', '(tip)', $2)",
        )
        .bind(session_id)
        .bind(cmid)
        .execute(&pool)
        .await
        .unwrap();

        let repo = ChatRepo { pool: &pool };
        // Must NOT panic (no RowNotFound → no 500); must be Duplicate.
        let out = repo
            .insert_voice_user_message(session_id, "hi", cmid)
            .await
            .unwrap();
        assert!(
            matches!(out, VoiceUserInsert::Duplicate(_)),
            "voice insert colliding with a gift_user row must be Duplicate, got {out:?}"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn voice_user_insert_bumps_last_active_at(pool: PgPool) {
        let session_id = throwaway_session(&pool).await.id;
        let repo = ChatRepo { pool: &pool };

        // Backdate so the bump is unambiguous regardless of clock resolution.
        sqlx::query(
            "UPDATE engine.chat_sessions SET last_active_at = now() - interval '1 hour' \
             WHERE id = $1",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();
        let before: DateTime<Utc> =
            sqlx::query_scalar("SELECT last_active_at FROM engine.chat_sessions WHERE id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();

        let out = repo
            .insert_voice_user_message(session_id, "hi", "01J9000000000000000000BUMP1")
            .await
            .unwrap();
        assert!(
            matches!(out, VoiceUserInsert::Inserted(_)),
            "expected Inserted, got {out:?}"
        );

        let after: DateTime<Utc> =
            sqlx::query_scalar("SELECT last_active_at FROM engine.chat_sessions WHERE id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            after > before,
            "a fresh voice user insert must bump last_active_at"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn voice_user_insert_duplicate_does_not_bump_last_active_at(pool: PgPool) {
        let session_id = throwaway_session(&pool).await.id;
        let repo = ChatRepo { pool: &pool };
        let cmid = "01J9000000000000000000BUMP2";

        let first = match repo
            .insert_voice_user_message(session_id, "hi", cmid)
            .await
            .unwrap()
        {
            VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        // Backdate so a stray bump on the duplicate branch would be visible.
        sqlx::query(
            "UPDATE engine.chat_sessions SET last_active_at = now() - interval '1 hour' \
             WHERE id = $1",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();
        let before: DateTime<Utc> =
            sqlx::query_scalar("SELECT last_active_at FROM engine.chat_sessions WHERE id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();

        let out = repo
            .insert_voice_user_message(session_id, "hi again", cmid)
            .await
            .unwrap();
        match out {
            VoiceUserInsert::Duplicate(id) => assert_eq!(id, first),
            other => panic!("expected Duplicate, got {other:?}"),
        }

        let after: DateTime<Utc> =
            sqlx::query_scalar("SELECT last_active_at FROM engine.chat_sessions WHERE id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            after, before,
            "a duplicate voice user insert must NOT bump last_active_at (would reorder \
             sibling-session selection for a replayed client_msg_id)"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn voice_assistant_insert_bumps_last_active_at(pool: PgPool) {
        let session_id = throwaway_session(&pool).await.id;
        let repo = ChatRepo { pool: &pool };

        let user_message_id = match repo
            .insert_voice_user_message(session_id, "hi", "01J9000000000000000000BUMP3")
            .await
            .unwrap()
        {
            VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        // Backdate so the assistant-insert bump is unambiguous.
        sqlx::query(
            "UPDATE engine.chat_sessions SET last_active_at = now() - interval '1 hour' \
             WHERE id = $1",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();
        let before: DateTime<Utc> =
            sqlx::query_scalar("SELECT last_active_at FROM engine.chat_sessions WHERE id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();

        let aid: Uuid = ulid::Ulid::new().into();
        repo.insert_voice_assistant_message(
            session_id,
            user_message_id,
            aid,
            "hi there",
            Some("primary"),
            None,
            Some("gen-1"),
            false,
            None,
        )
        .await
        .unwrap();

        let after: DateTime<Utc> =
            sqlx::query_scalar("SELECT last_active_at FROM engine.chat_sessions WHERE id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            after > before,
            "a voice assistant insert must bump last_active_at"
        );
    }

    /// Seeds a fresh voice session plus its one user turn via the real store
    /// methods (`throwaway_session` + `insert_voice_user_message`) rather than
    /// duplicating their genome/instance/session SQL inline.
    async fn seed_voice_turn(pool: &PgPool, content: &str) -> (Uuid, Uuid) {
        let session_id = throwaway_session(pool).await.id;
        let repo = ChatRepo { pool };
        let user_mid = match repo
            .insert_voice_user_message(session_id, content, &format!("cmid-{}", Uuid::new_v4()))
            .await
            .unwrap()
        {
            VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };
        (session_id, user_mid)
    }

    /// Generator-vs-generator race, no interrupt marker involved: e.g. an
    /// orphaned generator from a dead connection is still alive (TCP
    /// retransmission keeps it going for tens of seconds) when the client's
    /// retry lands its OWN generator for the same turn. Migration 0041 keeps
    /// this to one row, but the surviving row must be entirely the SAME
    /// call's data — `content`, `truncated`, AND the audit columns —  never a
    /// mix of one generation's `content` with a DIFFERENT generation's
    /// `generation_id` (which would corrupt OpenRouter reconciliation).
    #[sqlx::test(migrations = "./migrations")]
    #[allow(clippy::type_complexity)] // sqlx tuple query — type alias would add noise
    async fn voice_assistant_insert_is_idempotent_on_user_message(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let (session_id, user_mid) = seed_voice_turn(&pool, "hello").await;

        // Generator A writes first — no interrupt marker on the user row.
        repo.insert_voice_assistant_message(
            session_id,
            user_mid,
            Uuid::new_v4(),
            "first",
            Some("m/0"),
            None,
            Some("gen-0"),
            true,
            None,
        )
        .await
        .unwrap();

        // Generator B conflicts on the same row with DIFFERENT content AND a
        // DIFFERENT generation_id. Must NOT create a second row; must be
        // last-writer-wins on every column (content included), so the
        // surviving row never pairs A's content with B's generation_id.
        let usage = serde_json::json!({"total_tokens": 7});
        repo.insert_voice_assistant_message(
            session_id,
            user_mid,
            Uuid::new_v4(),
            "second",
            Some("m/1"),
            Some(&usage),
            Some("gen-1"),
            false,
            None,
        )
        .await
        .unwrap();

        let rows: Vec<(
            String,
            Option<String>,
            Option<String>,
            bool,
            Option<serde_json::Value>,
        )> = sqlx::query_as(
            "SELECT content, model, generation_id, truncated, usage FROM engine.chat_messages \
             WHERE user_message_id = $1 AND role = 'assistant' AND channel = 'voice'",
        )
        .bind(user_mid)
        .fetch_all(&pool)
        .await
        .unwrap();

        assert_eq!(
            rows.len(),
            1,
            "exactly one assistant reply per voice user turn"
        );
        assert_eq!(
            rows[0].0, "second",
            "no interrupt marker ⇒ last-writer-wins on content, not first-writer-wins"
        );
        assert_eq!(
            rows[0].1.as_deref(),
            Some("m/1"),
            "audit columns come from the SAME (surviving) writer as content"
        );
        assert_eq!(
            rows[0].2.as_deref(),
            Some("gen-1"),
            "generation_id must match the surviving content's call, never a stale earlier generation"
        );
        assert!(
            !rows[0].3,
            "truncated must come from the same writer as content"
        );
        assert_eq!(
            rows[0].4,
            Some(serde_json::json!({"total_tokens": 7})),
            "usage must come from the same writer as content"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn repair_state_reports_reply_marker_and_content(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let (session_id, user_mid) = seed_voice_turn(&pool, "hello there").await;

        // Fresh turn: nothing yet.
        let st = repo
            .voice_turn_repair_state(user_mid)
            .await
            .unwrap()
            .expect("seed_voice_turn produces an actual voice user turn");
        assert!(!st.has_reply);
        assert!(!st.interrupted);
        assert_eq!(
            st.content, "hello there",
            "the persisted utterance comes back"
        );

        // A reply flips has_reply.
        repo.insert_voice_assistant_message(
            session_id,
            user_mid,
            Uuid::new_v4(),
            "hi",
            None,
            None,
            None,
            false,
            None,
        )
        .await
        .unwrap();
        let st = repo
            .voice_turn_repair_state(user_mid)
            .await
            .unwrap()
            .expect("still an actual voice user turn");
        assert!(st.has_reply);
        assert!(!st.interrupted);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn repair_state_reports_interrupt_marker(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let (_session_id, user_mid) = seed_voice_turn(&pool, "hello").await;
        sqlx::query(
            "UPDATE engine.chat_messages \
             SET metadata = COALESCE(metadata,'{}'::jsonb) || '{\"voice_interrupt\": true}'::jsonb \
             WHERE id = $1",
        )
        .bind(user_mid)
        .execute(&pool)
        .await
        .unwrap();

        let st = repo
            .voice_turn_repair_state(user_mid)
            .await
            .unwrap()
            .expect("still an actual voice user turn");
        assert!(
            st.interrupted,
            "the marker must be visible to the repair gate"
        );
        assert!(!st.has_reply);
    }

    /// `insert_voice_user_message`'s conflict lookup that supplies
    /// `user_message_id` is deliberately role-agnostic (the colliding row can
    /// be a `gift_user` tip row, or a text-channel `user` row reached via
    /// `message/stream` against the same session id, since that route has no
    /// channel guard). The repair gate must independently refuse to treat
    /// either as a repairable voice turn.
    #[sqlx::test(migrations = "./migrations")]
    async fn repair_state_is_none_for_non_voice_user_rows(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let session_id = throwaway_session(&pool).await.id;

        let gift_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, client_msg_id, metadata) \
             VALUES ($1, 'gift_user', '(打赏 $20)', 'cid-gift', '{\"tips_amount_usd\": 20.0}'::jsonb) \
             RETURNING id",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            repo.voice_turn_repair_state(gift_id)
                .await
                .unwrap()
                .is_none(),
            "a gift_user row must never be reported as a repairable voice turn"
        );

        let text_user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, client_msg_id) \
             VALUES ($1, 'user', 'hi', 'cid-text') RETURNING id",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            repo.voice_turn_repair_state(text_user_id)
                .await
                .unwrap()
                .is_none(),
            "a channel-less (text) user row must never be reported as a repairable voice turn"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn latest_turn_lookup_classifies_missing_stale_and_latest(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let (session_id, _first) = seed_voice_turn(&pool, "one").await;

        // Give the first turn a client_msg_id, then add a newer one.
        sqlx::query(
            "UPDATE engine.chat_messages SET client_msg_id = 'CID-ONE' WHERE session_id = $1",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        assert!(matches!(
            repo.resolve_latest_voice_user_turn(session_id, "NOPE")
                .await
                .unwrap(),
            LatestTurnLookup::NotFound
        ));
        assert!(matches!(
            repo.resolve_latest_voice_user_turn(session_id, "CID-ONE")
                .await
                .unwrap(),
            LatestTurnLookup::Latest(_)
        ));

        let second: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, channel, client_msg_id) \
             VALUES ($1,'user','two','voice','CID-TWO') RETURNING id",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert!(
            matches!(
                repo.resolve_latest_voice_user_turn(session_id, "CID-ONE")
                    .await
                    .unwrap(),
                LatestTurnLookup::Stale
            ),
            "an older turn must not be interruptible"
        );
        assert_eq!(
            repo.resolve_latest_voice_user_turn(session_id, "CID-TWO")
                .await
                .unwrap(),
            LatestTurnLookup::Latest(second)
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn latest_turn_lookup_ignores_non_voice_rows_on_both_sides(pool: PgPool) {
        // A voice session can hold non-voice user rows: `routes/companion_stream.rs`
        // has no session-channel gate, so a text turn can land in one. Both sides
        // of the lookup must stay inside the voice channel.
        let repo = ChatRepo { pool: &pool };
        let (session_id, _voice) = seed_voice_turn(&pool, "spoken").await;
        sqlx::query(
            "UPDATE engine.chat_messages SET client_msg_id = 'CID-VOICE' WHERE session_id = $1",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        // A NEWER text-channel user row lands in the same session.
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, client_msg_id) \
             VALUES ($1,'user','typed','CID-TEXT')",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        // The text row must not be interruptible — otherwise a caller could mark
        // it and hang a channel='voice' assistant reply off a text turn.
        assert_eq!(
            repo.resolve_latest_voice_user_turn(session_id, "CID-TEXT")
                .await
                .unwrap(),
            LatestTurnLookup::NotFound,
            "a text-channel row must never resolve as an interruptible voice turn"
        );

        // And it must not shadow the genuine voice turn into Stale, which would
        // block a legitimate barge-in.
        assert!(
            matches!(
                repo.resolve_latest_voice_user_turn(session_id, "CID-VOICE")
                    .await
                    .unwrap(),
                LatestTurnLookup::Latest(_)
            ),
            "a newer text row must not make the latest voice turn look stale"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn channel_check_accepts_product_qa_and_rejects_unknown(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let session_id = throwaway_session(&pool).await.id;

        // product_qa accepted
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, channel) \
             VALUES ($1, 'assistant', 'answer', 'product_qa')",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("product_qa channel accepted");

        // unknown channel rejected by the named CHECK
        let err = sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, channel) \
             VALUES ($1, 'assistant', 'x', 'bogus')",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .expect_err("bogus channel rejected");
        assert!(err.to_string().contains("chat_messages_channel_check"));

        // ChatMessage projection surfaces the column
        let rows = repo.history(session_id, 10, 0).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].channel.as_deref(), Some("product_qa"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn sessions_are_channel_scoped(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;

        let text = repo.create_session(user_id, instance_id).await.unwrap();

        // No voice session yet → nothing to resume on the voice channel.
        assert!(repo
            .resume_latest_session(user_id, instance_id, "voice")
            .await
            .unwrap()
            .is_none());

        let voice = repo
            .create_session_with_metadata(user_id, instance_id, serde_json::json!({}), "voice")
            .await
            .unwrap();

        // Each channel resumes ITS OWN latest session, even though the other
        // channel's session is more recent.
        let resumed_text = repo
            .resume_latest_session(user_id, instance_id, "text")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resumed_text.id, text.id);

        let resumed_voice = repo
            .resume_latest_session(user_id, instance_id, "voice")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resumed_voice.id, voice.id);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn mark_user_message_product_qa_stamps_channel_idempotently(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let session_id = throwaway_session(&pool).await.id;
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content) \
             VALUES ($1, 'user', '介绍下这个产品') RETURNING id",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        repo.mark_user_message_product_qa(user_id).await.unwrap();
        repo.mark_user_message_product_qa(user_id).await.unwrap(); // re-mark = no-op

        let ch: Option<String> =
            sqlx::query_scalar("SELECT channel FROM engine.chat_messages WHERE id = $1")
                .bind(user_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(ch.as_deref(), Some("product_qa"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn insert_product_qa_assistant_message_round_trips(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let session_id = throwaway_session(&pool).await.id;
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content) \
             VALUES ($1, 'user', 'q') RETURNING id",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let aid = Uuid::new_v4();
        let usage = serde_json::json!({"total_tokens": 42});
        repo.insert_product_qa_assistant_message(
            session_id,
            user_id,
            aid,
            "产品介绍……",
            Some("anthropic/claude-haiku-4.5"),
            Some(&usage),
            Some("gen_pq_1"),
            false,
        )
        .await
        .unwrap();

        let row: ChatMessage = sqlx::query_as("SELECT * FROM engine.chat_messages WHERE id = $1")
            .bind(aid)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.role, "assistant");
        assert_eq!(row.channel.as_deref(), Some("product_qa"));
        assert_eq!(row.assistant_action_type.as_deref(), Some("reply"));
        assert_eq!(row.user_message_id, Some(user_id));
        assert_eq!(row.generation_id.as_deref(), Some("gen_pq_1"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recent_product_qa_pairs_returns_only_marked_pairs_before_cutoff(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let session_id = throwaway_session(&pool).await.id;
        // normal pair (must NOT appear)
        for (role, content) in [("user", "嗨"), ("assistant", "嗨呀")] {
            sqlx::query(
                "INSERT INTO engine.chat_messages (session_id, role, content) VALUES ($1,$2,$3)",
            )
            .bind(session_id)
            .bind(role)
            .bind(content)
            .execute(&pool)
            .await
            .unwrap();
        }
        // product_qa pair (must appear)
        for (role, content) in [("user", "这产品是什么"), ("assistant", "这是……")] {
            sqlx::query(
                "INSERT INTO engine.chat_messages (session_id, role, content, channel) \
                 VALUES ($1,$2,$3,'product_qa')",
            )
            .bind(session_id)
            .bind(role)
            .bind(content)
            .execute(&pool)
            .await
            .unwrap();
        }
        // current-turn user row = cutoff
        let cur: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content) \
             VALUES ($1, 'user', '那多少钱') RETURNING id",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let pairs = repo
            .recent_product_qa_pairs(session_id, cur, 3)
            .await
            .unwrap();
        assert_eq!(
            pairs,
            vec![("这产品是什么".to_string(), "这是……".to_string())]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn context_readers_exclude_channel_marked_rows(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let session_id = throwaway_session(&pool).await.id;
        // one normal pair, one product_qa pair, then cutoff row
        for (role, content, channel) in [
            ("user", "嗨", None::<&str>),
            ("assistant", "嗨呀", None),
            ("user", "这产品是什么", Some("product_qa")),
            ("assistant", "这是官方说明……", Some("product_qa")),
        ] {
            sqlx::query(
                "INSERT INTO engine.chat_messages (session_id, role, content, channel) \
                 VALUES ($1,$2,$3,$4)",
            )
            .bind(session_id)
            .bind(role)
            .bind(content)
            .bind(channel)
            .execute(&pool)
            .await
            .unwrap();
        }
        let cur: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content) \
             VALUES ($1, 'user', '在吗') RETURNING id",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let pairs = repo
            .recent_turn_pairs_before_message(session_id, cur, 5)
            .await
            .unwrap();
        assert_eq!(pairs, vec![("嗨".to_string(), "嗨呀".to_string())]);

        let pairs2 = repo
            .recent_turn_pairs(
                session_id,
                chrono::Utc::now() + chrono::Duration::seconds(60),
                5,
            )
            .await
            .unwrap();
        assert_eq!(pairs2, vec![("嗨".to_string(), "嗨呀".to_string())]);

        let openings = repo
            .recent_assistant_contents(session_id, cur, 6)
            .await
            .unwrap();
        assert!(openings.iter().all(|c| c != "这是官方说明……"));
        assert!(openings.contains(&"嗨呀".to_string()));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn latest_sibling_voice_session_excludes_current_and_text_channel(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;

        // The only session that should ever come back: an older voice
        // session for the same user × instance.
        let sibling = repo
            .create_session_with_metadata(user_id, instance_id, serde_json::json!({}), "voice")
            .await
            .unwrap();
        sqlx::query(
            "UPDATE engine.chat_sessions SET last_active_at = now() - interval '2 hours' \
             WHERE id = $1",
        )
        .bind(sibling.id)
        .execute(&pool)
        .await
        .unwrap();

        // Decoy: a text-channel session, more recently active than `sibling`.
        // If the channel filter were missing, this would be picked instead.
        let text_decoy = repo
            .create_session_with_metadata(user_id, instance_id, serde_json::json!({}), "text")
            .await
            .unwrap();
        sqlx::query(
            "UPDATE engine.chat_sessions SET last_active_at = now() - interval '1 hour' \
             WHERE id = $1",
        )
        .bind(text_decoy.id)
        .execute(&pool)
        .await
        .unwrap();

        // Current: the voice session we exclude by id. Left at its default
        // `now()` last_active_at — the most recent of all three — so a
        // missing `id <> $3` filter would wrongly return it instead of
        // `sibling`.
        let current = repo
            .create_session_with_metadata(user_id, instance_id, serde_json::json!({}), "voice")
            .await
            .unwrap();

        let found = repo
            .latest_sibling_voice_session(user_id, instance_id, current.id)
            .await
            .unwrap();
        assert_eq!(
            found.map(|s| s.id),
            Some(sibling.id),
            "must exclude the current session and any text-channel session"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn latest_sibling_voice_session_orders_by_last_active_at(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let current = repo
            .create_session_with_metadata(user_id, instance_id, serde_json::json!({}), "voice")
            .await
            .unwrap();

        let older = repo
            .create_session_with_metadata(user_id, instance_id, serde_json::json!({}), "voice")
            .await
            .unwrap();
        sqlx::query(
            "UPDATE engine.chat_sessions SET last_active_at = now() - interval '2 hours' \
             WHERE id = $1",
        )
        .bind(older.id)
        .execute(&pool)
        .await
        .unwrap();

        let newer = repo
            .create_session_with_metadata(user_id, instance_id, serde_json::json!({}), "voice")
            .await
            .unwrap();
        sqlx::query(
            "UPDATE engine.chat_sessions SET last_active_at = now() - interval '1 minute' \
             WHERE id = $1",
        )
        .bind(newer.id)
        .execute(&pool)
        .await
        .unwrap();

        let found = repo
            .latest_sibling_voice_session(user_id, instance_id, current.id)
            .await
            .unwrap()
            .expect("a sibling voice session exists");
        assert_eq!(
            found.id, newer.id,
            "must return the most-recently-active sibling"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn latest_sibling_voice_session_none_when_no_sibling(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let current = repo
            .create_session_with_metadata(user_id, instance_id, serde_json::json!({}), "voice")
            .await
            .unwrap();

        let found = repo
            .latest_sibling_voice_session(user_id, instance_id, current.id)
            .await
            .unwrap();
        assert!(
            found.is_none(),
            "no other voice session exists for this user × instance"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn set_voice_bootstrap_writes_once_then_noops(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let s = throwaway_session(&pool).await;
        let snapshot = serde_json::json!({
            "tail": "上次聊到这里",
            "prior_session_id": Uuid::new_v4().to_string(),
        });

        let rows = repo.set_voice_bootstrap(s.id, &snapshot).await.unwrap();
        assert_eq!(rows, 1, "first write must succeed");

        let loaded = repo.get_session(s.id).await.unwrap().unwrap();
        assert_eq!(loaded.metadata["voice_bootstrap"], snapshot);

        // Second call with a DIFFERENT snapshot must no-op — the key is
        // already present.
        let other_snapshot = serde_json::json!({"tail": "should never be written"});
        let rows2 = repo
            .set_voice_bootstrap(s.id, &other_snapshot)
            .await
            .unwrap();
        assert_eq!(
            rows2, 0,
            "second write must be a no-op (key already present)"
        );

        let loaded2 = repo.get_session(s.id).await.unwrap().unwrap();
        assert_eq!(
            loaded2.metadata["voice_bootstrap"], snapshot,
            "original snapshot must be preserved, not overwritten"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn set_voice_bootstrap_preserves_other_metadata_keys(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let s = repo
            .create_session_with_metadata(
                user_id,
                instance_id,
                serde_json::json!({"is_demo": true}),
                "voice",
            )
            .await
            .unwrap();

        let snapshot = serde_json::json!({"tail": "接上次"});
        let rows = repo.set_voice_bootstrap(s.id, &snapshot).await.unwrap();
        assert_eq!(rows, 1);

        let loaded = repo.get_session(s.id).await.unwrap().unwrap();
        assert_eq!(
            loaded.metadata["is_demo"],
            serde_json::json!(true),
            "pre-existing metadata keys must survive the merge"
        );
        assert_eq!(loaded.metadata["voice_bootstrap"], snapshot);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn interrupt_inserts_spoken_text_and_marks_user_row(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let (session_id, user_mid) = seed_voice_turn(&pool, "hello").await;

        let aid = repo
            .upsert_voice_interrupt(session_id, user_mid, Uuid::new_v4(), "你今天过得")
            .await
            .unwrap()
            .expect("a row is written when text was played");

        let (content, truncated): (String, bool) =
            sqlx::query_as("SELECT content, truncated FROM engine.chat_messages WHERE id = $1")
                .bind(aid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(content, "你今天过得");
        assert!(truncated);

        let marked: bool = sqlx::query_scalar(
            "SELECT COALESCE(metadata->>'voice_interrupt','false')::bool \
               FROM engine.chat_messages WHERE id = $1",
        )
        .bind(user_mid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(marked, "the user row carries the marker");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn interrupt_with_empty_text_marks_but_writes_no_row(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let (session_id, user_mid) = seed_voice_turn(&pool, "hello").await;

        let out = repo
            .upsert_voice_interrupt(session_id, user_mid, Uuid::new_v4(), "")
            .await
            .unwrap();
        assert!(
            out.is_none(),
            "empty spoken_text must never write assistant content"
        );

        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
              WHERE user_message_id = $1 AND role = 'assistant'",
        )
        .bind(user_mid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 0);

        let marked: bool = sqlx::query_scalar(
            "SELECT COALESCE(metadata->>'voice_interrupt','false')::bool \
               FROM engine.chat_messages WHERE id = $1",
        )
        .bind(user_mid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(marked, "still distinguishable from a disconnect");
    }

    /// The race the empty-text branch cannot close with `ON CONFLICT` alone:
    /// the client hears nothing and interrupts BEFORE the generator commits.
    /// `insert_voice_assistant_message` must see the marker (via the shared
    /// user-row lock) and decline to insert — the "absent + empty" contract
    /// cell requires no assistant row at all, not a full untruncated reply
    /// masquerading as a normal completion.
    #[sqlx::test(migrations = "./migrations")]
    async fn empty_interrupt_then_generator_leaves_no_assistant_row(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let (session_id, user_mid) = seed_voice_turn(&pool, "hello").await;

        let out = repo
            .upsert_voice_interrupt(session_id, user_mid, Uuid::new_v4(), "")
            .await
            .unwrap();
        assert!(out.is_none());

        // The generator was still alive and only now reaches its own insert.
        repo.insert_voice_assistant_message(
            session_id,
            user_mid,
            Uuid::new_v4(),
            "generated much more",
            Some("m/1"),
            None,
            Some("gen-late"),
            false,
            None,
        )
        .await
        .unwrap();

        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
              WHERE user_message_id = $1 AND role = 'assistant'",
        )
        .bind(user_mid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            n, 0,
            "the generator must decline to insert once the turn is marked interrupted with nothing played"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn interrupt_after_completion_overwrites_content_keeps_audit(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let (session_id, user_mid) = seed_voice_turn(&pool, "hello").await;
        let usage = serde_json::json!({"total_tokens": 42});
        let scope = serde_json::json!({"relationship_scope": "both"});
        repo.insert_voice_assistant_message(
            session_id,
            user_mid,
            Uuid::new_v4(),
            "the whole sentence",
            Some("m/1"),
            Some(&usage),
            Some("gen-9"),
            false,
            Some(&scope),
        )
        .await
        .unwrap();

        repo.upsert_voice_interrupt(session_id, user_mid, Uuid::new_v4(), "the whole")
            .await
            .unwrap()
            .expect("returns the existing row's id");

        let (content, model, gen, truncated, meta): (
            String,
            Option<String>,
            Option<String>,
            bool,
            serde_json::Value,
        ) = sqlx::query_as(
            "SELECT content, model, generation_id, truncated, metadata \
               FROM engine.chat_messages \
              WHERE user_message_id = $1 AND role = 'assistant'",
        )
        .bind(user_mid)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(content, "the whole", "content is what was actually heard");
        assert_eq!(model.as_deref(), Some("m/1"), "billing audit survives");
        assert_eq!(gen.as_deref(), Some("gen-9"));
        assert!(truncated);
        assert_eq!(
            meta["relationship_scope"], "both",
            "generator metadata preserved"
        );
        assert_eq!(
            meta["voice_interrupt"], true,
            "marker merged in, not replacing"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn interrupt_with_empty_text_leaves_completed_reply_alone(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let (session_id, user_mid) = seed_voice_turn(&pool, "hello").await;
        repo.insert_voice_assistant_message(
            session_id,
            user_mid,
            Uuid::new_v4(),
            "already said this",
            None,
            None,
            None,
            false,
            None,
        )
        .await
        .unwrap();

        repo.upsert_voice_interrupt(session_id, user_mid, Uuid::new_v4(), "")
            .await
            .unwrap();

        let content: String = sqlx::query_scalar(
            "SELECT content FROM engine.chat_messages \
              WHERE user_message_id = $1 AND role = 'assistant'",
        )
        .bind(user_mid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            content, "already said this",
            "degenerate case is non-destructive"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn double_write_race_is_order_independent(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };

        // Order A: interrupt first, generator second.
        let (s1, u1) = seed_voice_turn(&pool, "hello").await;
        repo.upsert_voice_interrupt(s1, u1, Uuid::new_v4(), "heard this much")
            .await
            .unwrap();
        repo.insert_voice_assistant_message(
            s1,
            u1,
            Uuid::new_v4(),
            "generated much more",
            Some("m/1"),
            None,
            Some("gen-a"),
            false,
            None,
        )
        .await
        .unwrap();

        // Order B: generator first, interrupt second.
        let (s2, u2) = seed_voice_turn(&pool, "hello").await;
        repo.insert_voice_assistant_message(
            s2,
            u2,
            Uuid::new_v4(),
            "generated much more",
            Some("m/1"),
            None,
            Some("gen-b"),
            false,
            None,
        )
        .await
        .unwrap();
        repo.upsert_voice_interrupt(s2, u2, Uuid::new_v4(), "heard this much")
            .await
            .unwrap();

        for (uid, gen) in [(u1, "gen-a"), (u2, "gen-b")] {
            let rows: Vec<(String, Option<String>)> = sqlx::query_as(
                "SELECT content, generation_id FROM engine.chat_messages \
                  WHERE user_message_id = $1 AND role = 'assistant'",
            )
            .bind(uid)
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(rows.len(), 1, "one row regardless of arrival order");
            assert_eq!(
                rows[0].0, "heard this much",
                "content is always the interrupt's"
            );
            assert_eq!(
                rows[0].1.as_deref(),
                Some(gen),
                "audit is always the generator's"
            );
        }
    }
}
