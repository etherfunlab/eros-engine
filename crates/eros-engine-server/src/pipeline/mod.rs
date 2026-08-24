// SPDX-License-Identifier: AGPL-3.0-only
//! Pipeline module — shared helpers (`compute_signals_for_session`,
//! `record_generation`) plus the chat / post-process / dreaming
//! submodules. The streaming chat entry point is `stream::run_stream`.

pub mod chat_queue;
pub mod dreaming;
pub mod handlers;
pub mod post_process;
pub mod snapshot;
pub mod story;
pub mod stream;
pub mod voice;
pub mod world;
pub mod world_town;

use uuid::Uuid;

use eros_engine_core::affinity::Affinity;
use eros_engine_core::types::ConversationSignals;

use crate::error::AppError;

/// How long **any** fail-open audit write gets before it is abandoned —
/// `llm_generations` here, `chat_images_events` and `chat_vision_events` in
/// `stream.rs`. One constant for all three because they are one operation: a
/// single-row INSERT into the same Postgres, whose failure costs an audit row
/// and never a turn. Two bounds for the same operation is two facts where
/// there is one.
///
/// It covers the pool acquisition as well as the statement, and it is
/// deliberately far below both the pool's 5s `acquire_timeout` and any
/// LLM-sized budget (`FILTER_TIMEOUT` and its siblings). This is one local
/// round trip, not a network call to a provider, so an LLM-sized bound would
/// hide exactly the stall it exists to catch. With no bound at all, `sqlx` has
/// no client-side statement timeout, so an insert that HANGS — as opposed to
/// one that is refused — stalls the turn indefinitely.
///
/// **The number is chosen against the per-TURN cost, not the per-write one.**
/// A chat turn makes up to four of these serially on its critical path
/// (`chat_input_filter`, `pde_decision`, `chat_companion`,
/// `chat_output_filter`), plus the image / vision events on the paths that
/// have them — where what waits behind the write is the image marker, the
/// endpoint's JSON body, the pre-stream 502, or the terminal SSE frame. A
/// bound that reads as generous per write multiplies: at 2s the
/// worst case was 8s of a turn spent waiting on audit rows, all of it outside
/// the LLM timeout budget, and what the user sees is a reply that hangs. A
/// single-row insert's p99 is milliseconds, so 500ms is still a hundredfold
/// margin while capping the turn near 2s.
///
/// What this does NOT do: dropping the future at the timeout does not
/// guarantee Postgres receives a `CancelRequest`, so a genuinely stuck
/// statement can keep running server-side and its connection is churned rather
/// than reused. That residual wants a server-side `statement_timeout`, which no
/// query in this codebase sets and which is a pool-wide decision — a pgvector
/// recall is legitimately slower than an audit insert, so one bound cannot
/// serve both. Bounding the caller is still strictly better than the
/// alternative: without this, the turn stalls AND the connection is stuck.
const AUDIT_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// One LLM generation, as the call site knows it. Five loose fields rather
/// than a `&ChatResponse`: four streaming arms never own one, and two of them
/// used to synthesise a fake with an empty `reply` purely to satisfy this
/// function's parameter.
pub(super) struct GenerationRecord<'a> {
    pub task: &'a str,
    pub session_id: Option<Uuid>,
    pub generation_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub usage: Option<&'a serde_json::Value>,
}

/// Record one billable generation to `engine.llm_generations` and emit the
/// structured `openrouter: call completed` log line. Called from EVERY
/// codepath that owns an LLM response — including the world, story and
/// dreaming sweepers, for which this table is the only database record there
/// is.
///
/// **The return value is the contract.** Callers must store what comes back in
/// their own `generation_id` column, never `resp.generation_id`:
///
/// - `Some(id)` — the parent row is committed and may be referenced.
/// - `None` — upstream gave no id, or the write failed. Store `NULL`.
///
/// Fail-open by design: an audit row is recoverable from the tracing line and
/// from the provider's log, a user's reply is not. Nothing here can fail a
/// turn, and — see `AUDIT_WRITE_TIMEOUT` — nothing here can stall one either.
///
/// Token / cost fields in the log line are best-effort parses off the opaque
/// `usage` JSON — missing fields silently drop out. The row stores `usage`
/// whole and unfiltered; `OPENROUTER_USAGE_HIDDEN_KEYS` applies only on the
/// way out to clients.
pub(super) async fn record_generation(
    pool: &sqlx::PgPool,
    rec: GenerationRecord<'_>,
) -> Option<String> {
    let usage_ref = rec.usage;
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
        task = rec.task,
        session = ?rec.session_id,
        generation_id = ?rec.generation_id,
        model = ?rec.model,
        prompt_tokens = ?prompt_tokens,
        completion_tokens = ?completion_tokens,
        total_tokens = ?total_tokens,
        cost = ?cost,
        "openrouter: call completed"
    );

    let generation_id = rec.generation_id?;
    let repo = eros_engine_store::generation::LlmGenerationRepo { pool };
    let write = repo.record(eros_engine_store::generation::LlmGenerationInsert {
        generation_id,
        session_id: rec.session_id,
        task: rec.task,
        model: rec.model,
        usage: rec.usage,
    });
    match tokio::time::timeout(AUDIT_WRITE_TIMEOUT, write).await {
        Ok(Ok(())) => Some(generation_id.to_string()),
        Ok(Err(e)) => {
            tracing::warn!(
                task = rec.task,
                generation_id = generation_id,
                error = %e,
                "llm_generations: audit write failed; child row will store NULL"
            );
            None
        }
        // Distinct message from the error arm: this one means the database (or
        // the pool) is slow rather than refusing, which is an operational
        // signal, not a schema one.
        Err(_) => {
            tracing::warn!(
                task = rec.task,
                generation_id = generation_id,
                timeout_ms = AUDIT_WRITE_TIMEOUT.as_millis(),
                "llm_generations: audit write timed out; child row will store NULL"
            );
            None
        }
    }
}

/// Walk forward from the first `{` and return the substring up to its
/// balanced `}`, ignoring braces inside string literals. Shared by the
/// background LLM-output parsers (dreaming memory extraction, post-process
/// affinity eval, world director) — they wrap model JSON that is sometimes
/// fenced or prose-prefixed.
pub(super) fn find_json_block(raw: &str) -> Option<&str> {
    let bytes = raw.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&raw[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Parse an LLM reply expected to contain a JSON object of type `T`: try the
/// whole text first, then fall back to the balanced `{…}` block (via
/// `find_json_block`) for replies that fence or prose-wrap their JSON.
pub(super) fn parse_llm_json<T: serde::de::DeserializeOwned>(raw: &str) -> Option<T> {
    serde_json::from_str::<T>(raw)
        .ok()
        .or_else(|| find_json_block(raw).and_then(|b| serde_json::from_str::<T>(b).ok()))
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
             WHERE session_id = $1 AND role IN ('user', 'gift_user') \
               AND channel IS NULL",
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
            warmth_grade: 2,
            patience_grade: 2,
            ghost_streak: 0,
            last_ghost_at: None,
            total_ghosts: 0,
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

    /// `channel`-marked rows (e.g. `product_qa`) are out-of-character asides,
    /// not companion conversation — they must not count toward the signal.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn signals_exclude_channel_marked_rows(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (_genome_id, instance_id, session_id) = seed_session(&pool, user_id).await;

        for (role, channel) in [("user", None::<&str>), ("user", Some("product_qa"))] {
            sqlx::query(
                "INSERT INTO engine.chat_messages (session_id, role, content, channel) \
                 VALUES ($1, $2, 'x', $3)",
            )
            .bind(session_id)
            .bind(role)
            .bind(channel)
            .execute(&pool)
            .await
            .unwrap();
        }

        let aff = fixture_affinity(session_id, user_id, instance_id);
        let signals = compute_signals_for_session(&pool, session_id, &aff)
            .await
            .unwrap();

        assert_eq!(signals.message_count, 1); // the product_qa question doesn't count
    }

    #[test]
    fn find_json_block_balanced_with_string_braces() {
        let raw = r#"prefix {"a": "b}c", "d": 1} trailing"#;
        let block = super::find_json_block(raw).unwrap();
        let v: serde_json::Value = serde_json::from_str(block).unwrap();
        assert_eq!(v["a"], "b}c");
        assert_eq!(v["d"], 1);
    }

    #[test]
    fn find_json_block_returns_none_when_no_object() {
        assert!(super::find_json_block("no json here").is_none());
    }

    #[test]
    fn find_json_block_balanced_with_nested_object() {
        let raw = r#"prefix {"a":"b}c","d":{"e":1}} trailing"#;
        let block = super::find_json_block(raw).unwrap();
        let v: serde_json::Value = serde_json::from_str(block).unwrap();
        assert_eq!(v["a"], "b}c");
        assert_eq!(v["d"]["e"], 1);
    }

    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct ParseProbe {
        verdict: String,
        score: i32,
    }

    #[test]
    fn parse_llm_json_accepts_pure_json() {
        let raw = r#"{"verdict":"pass","score":3}"#;
        let parsed: ParseProbe = super::parse_llm_json(raw).unwrap();
        assert_eq!(
            parsed,
            ParseProbe {
                verdict: "pass".into(),
                score: 3
            }
        );
    }

    #[test]
    fn parse_llm_json_falls_back_to_embedded_block() {
        let raw = "Sure! Here is the result:\n```json\n{\"verdict\":\"fail\",\"score\":-2}\n```\nHope that helps.";
        let parsed: ParseProbe = super::parse_llm_json(raw).unwrap();
        assert_eq!(
            parsed,
            ParseProbe {
                verdict: "fail".into(),
                score: -2
            }
        );
    }

    #[test]
    fn parse_llm_json_none_when_no_json() {
        assert!(super::parse_llm_json::<ParseProbe>("no json here").is_none());
    }

    #[test]
    fn parse_llm_json_none_when_shape_mismatch() {
        // Valid JSON block, wrong shape: the fallback must also fail typed.
        assert!(super::parse_llm_json::<ParseProbe>(r#"prose {"other": 1} prose"#).is_none());
    }
}

#[cfg(test)]
mod generation_record_tests {
    use super::*;
    use sqlx::PgPool;
    use uuid::Uuid;

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn records_the_row_and_returns_the_id(pool: PgPool) {
        let usage = serde_json::json!({ "cost": 0.002 });
        let out = record_generation(
            &pool,
            GenerationRecord {
                task: "pde_decision",
                session_id: None,
                generation_id: Some("gen-1"),
                model: Some("m"),
                usage: Some(&usage),
            },
        )
        .await;
        assert_eq!(out.as_deref(), Some("gen-1"));

        let task: String = sqlx::query_scalar(
            "SELECT task FROM engine.llm_generations WHERE generation_id = 'gen-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(task, "pde_decision");
    }

    /// No id from upstream ⇒ no row, no query, and the caller stores NULL.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn returns_none_and_writes_nothing_when_upstream_gave_no_id(pool: PgPool) {
        let out = record_generation(
            &pool,
            GenerationRecord {
                task: "chat_vision",
                session_id: None,
                generation_id: None,
                model: Some("m"),
                usage: None,
            },
        )
        .await;
        assert!(out.is_none());

        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM engine.llm_generations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    /// Fail-open: a parent write that cannot land degrades to `None` so the
    /// child row stores NULL and the turn survives. Forced here by pointing at
    /// a session id that does not exist, which the table's own FK rejects.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn degrades_to_none_when_the_write_fails(pool: PgPool) {
        let out = record_generation(
            &pool,
            GenerationRecord {
                task: "chat_companion",
                session_id: Some(Uuid::new_v4()), // no such session
                generation_id: Some("gen-doomed"),
                model: None,
                usage: None,
            },
        )
        .await;
        assert!(
            out.is_none(),
            "a failed parent write must degrade, not panic"
        );

        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM engine.llm_generations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }
}
