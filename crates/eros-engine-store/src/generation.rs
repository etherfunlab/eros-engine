// SPDX-License-Identifier: AGPL-3.0-only
//! Writer for `engine.llm_generations` — one row per billable LLM generation.

use sqlx::PgPool;
use uuid::Uuid;

/// One `engine.llm_generations` row. `generation_id` is the provider's opaque
/// handle and is not optional — a call that produced no id produced no row.
/// `session_id` is `None` for the tasks that have no conversation behind them
/// (world/story sweepers, the standalone compose endpoint).
pub struct LlmGenerationInsert<'a> {
    pub generation_id: &'a str,
    pub session_id: Option<Uuid>,
    pub task: &'a str,
    pub model: Option<&'a str>,
    pub usage: Option<&'a serde_json::Value>,
}

pub struct LlmGenerationRepo<'a> {
    pub pool: &'a PgPool,
}

impl LlmGenerationRepo<'_> {
    /// Record one generation. Idempotent by primary key: a streamed reply that
    /// persists as several `chat_messages` rows calls this once per row, and
    /// the first write wins. `created_at` is the column default rather than a
    /// parameter — it is the moment the engine recorded the generation, and
    /// taking it from callers would invite each one to pass a different clock.
    pub async fn record(&self, g: LlmGenerationInsert<'_>) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO engine.llm_generations \
               (generation_id, session_id, task, model, usage) \
             VALUES ($1,$2,$3,$4,$5) \
             ON CONFLICT (generation_id) DO NOTHING",
        )
        .bind(g.generation_id)
        .bind(g.session_id)
        .bind(g.task)
        .bind(g.model)
        .bind(g.usage)
        .execute(self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::seed_chat_session;
    use sqlx::PgPool;
    use uuid::Uuid;

    fn usage() -> serde_json::Value {
        serde_json::json!({ "prompt_tokens": 11, "completion_tokens": 7, "cost": 0.0004 })
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn record_round_trips_every_column(pool: PgPool) {
        let session = seed_chat_session(&pool, Uuid::new_v4()).await;
        let repo = LlmGenerationRepo { pool: &pool };
        let u = usage();
        repo.record(LlmGenerationInsert {
            generation_id: "gen-abc",
            session_id: Some(session),
            task: "chat_companion",
            model: Some("vendor/model"),
            usage: Some(&u),
        })
        .await
        .expect("insert");

        let (sid, task, model, stored): (
            Option<Uuid>,
            String,
            Option<String>,
            Option<serde_json::Value>,
        ) = sqlx::query_as(
            "SELECT session_id, task, model, usage FROM engine.llm_generations \
                 WHERE generation_id = $1",
        )
        .bind("gen-abc")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(sid, Some(session));
        assert_eq!(task, "chat_companion");
        assert_eq!(model.as_deref(), Some("vendor/model"));
        assert_eq!(stored.unwrap()["cost"], 0.0004);
    }

    /// One streamed reply can persist as several `chat_messages` rows
    /// (`continues_from_message_id`), so the child writer calls this once per
    /// row for a single generation. The second call must be a no-op, not an
    /// error and not an overwrite.
    #[sqlx::test(migrations = "./migrations")]
    async fn record_is_idempotent_and_first_write_wins(pool: PgPool) {
        let repo = LlmGenerationRepo { pool: &pool };
        repo.record(LlmGenerationInsert {
            generation_id: "gen-dup",
            session_id: None,
            task: "chat_companion",
            model: Some("first"),
            usage: None,
        })
        .await
        .expect("first insert");
        repo.record(LlmGenerationInsert {
            generation_id: "gen-dup",
            session_id: None,
            task: "chat_companion",
            model: Some("second"),
            usage: None,
        })
        .await
        .expect("second insert must not error");

        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.llm_generations WHERE generation_id = $1",
        )
        .bind("gen-dup")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 1);
        let model: Option<String> =
            sqlx::query_scalar("SELECT model FROM engine.llm_generations WHERE generation_id = $1")
                .bind("gen-dup")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(model.as_deref(), Some("first"), "first write wins");
    }

    /// The world/story sweepers and the standalone compose endpoint have no
    /// session at all.
    #[sqlx::test(migrations = "./migrations")]
    async fn record_accepts_a_null_session(pool: PgPool) {
        let repo = LlmGenerationRepo { pool: &pool };
        repo.record(LlmGenerationInsert {
            generation_id: "gen-sessionless",
            session_id: None,
            task: "world_director",
            model: None,
            usage: None,
        })
        .await
        .expect("insert");

        let sid: Option<Uuid> = sqlx::query_scalar(
            "SELECT session_id FROM engine.llm_generations WHERE generation_id = $1",
        )
        .bind("gen-sessionless")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(sid.is_none());
    }

    /// The cost record must outlive the conversation: deleting the session
    /// blanks the pointer instead of taking the row with it.
    #[sqlx::test(migrations = "./migrations")]
    async fn deleting_the_session_blanks_the_pointer_and_keeps_the_row(pool: PgPool) {
        let session = seed_chat_session(&pool, Uuid::new_v4()).await;
        let repo = LlmGenerationRepo { pool: &pool };
        repo.record(LlmGenerationInsert {
            generation_id: "gen-outlives",
            session_id: Some(session),
            task: "affinity_evaluation",
            model: None,
            usage: None,
        })
        .await
        .expect("insert");

        sqlx::query("DELETE FROM engine.chat_sessions WHERE id = $1")
            .bind(session)
            .execute(&pool)
            .await
            .unwrap();

        let sid: Option<Option<Uuid>> = sqlx::query_scalar(
            "SELECT session_id FROM engine.llm_generations WHERE generation_id = $1",
        )
        .bind("gen-outlives")
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert_eq!(sid, Some(None), "row survives, pointer blanked");
    }

    /// `task` carries no CHECK on purpose: a deployer adding a `[tasks.*]`
    /// section must never turn a live turn into an insert failure.
    #[sqlx::test(migrations = "./migrations")]
    async fn record_accepts_a_task_name_the_engine_does_not_ship(pool: PgPool) {
        let repo = LlmGenerationRepo { pool: &pool };
        repo.record(LlmGenerationInsert {
            generation_id: "gen-unknown-task",
            session_id: None,
            task: "some_deployer_task",
            model: None,
            usage: None,
        })
        .await
        .expect("an unrecognised task name must insert");
    }
}
