// SPDX-License-Identifier: AGPL-3.0-only
//! companion_insights_events audit storage (per-LLM-call rows for the insight
//! extractor).

use sqlx::PgPool;
use uuid::Uuid;

/// One `companion_insights_events` row to insert. `payload` is the
/// `{facts, details}` object (`stage='facts'`) or the structured insight delta
/// (`stage='structured'`), or `None` on a parse error.
pub struct InsightEventInsert<'a> {
    pub run_id: Uuid,
    pub user_id: Uuid,
    pub session_id: Option<Uuid>,
    pub message_id: Option<Uuid>,
    pub stage: &'a str,
    pub status: &'a str,
    pub payload: Option<serde_json::Value>,
    /// `record_generation`'s return value — never `resp.generation_id`. The
    /// model and usage for this call live in `engine.llm_generations`, reached
    /// by joining on this column (spec §7.4). `None` when no call was made, or
    /// when the parent write failed and the trail degraded to NULL.
    pub generation_id: Option<String>,
}

pub struct InsightEventRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> InsightEventRepo<'a> {
    /// Append one audit row. Append-only; no FK on user_id (a row may precede
    /// the user's first human_insights row on an empty-facts run).
    pub async fn record(&self, ev: InsightEventInsert<'_>) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO engine.companion_insights_events \
               (run_id, user_id, session_id, message_id, stage, status, payload, \
                generation_id) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
        )
        .bind(ev.run_id)
        .bind(ev.user_id)
        .bind(ev.session_id)
        .bind(ev.message_id)
        .bind(ev.stage)
        .bind(ev.status)
        .bind(ev.payload)
        .bind(ev.generation_id)
        .execute(self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "./migrations")]
    async fn insight_event_repo_records_rows_with_run_id_and_trio(pool: PgPool) {
        let repo = InsightEventRepo { pool: &pool };
        let run_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        // Both are FK-referenced since 0058; a fabricated id no longer inserts.
        let session_id = crate::testutil::seed_chat_session(&pool, user_id).await;
        let message_id = crate::testutil::seed_chat_message(&pool, session_id).await;
        // ...and generation_id since 0060.
        crate::testutil::seed_generation(&pool, "gen-facts").await;

        repo.record(InsightEventInsert {
            run_id,
            user_id,
            session_id: Some(session_id),
            message_id: Some(message_id),
            stage: "facts",
            status: "ok",
            payload: Some(serde_json::json!({"facts": ["用户在深圳工作"], "details": []})),
            generation_id: Some("gen-facts".into()),
        })
        .await
        .unwrap();

        repo.record(InsightEventInsert {
            run_id,
            user_id,
            session_id: Some(session_id),
            message_id: Some(message_id),
            stage: "structured",
            status: "parse_error",
            payload: None,
            generation_id: None,
        })
        .await
        .unwrap();

        // Two rows, same run_id. Also round-trip the JSONB payload + usage so a
        // bind/column swap of the trio would be caught here, not just at the DB.
        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            String,
            String,
            Option<String>,
            Option<serde_json::Value>,
            Option<serde_json::Value>,
        )> = sqlx::query_as(
            "SELECT e.stage, e.status, e.generation_id, e.payload, g.usage \
             FROM engine.companion_insights_events e \
             LEFT JOIN engine.llm_generations g ON g.generation_id = e.generation_id \
             WHERE e.run_id = $1 ORDER BY e.stage",
        )
        .bind(run_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "facts");
        assert_eq!(rows[0].1, "ok");
        assert_eq!(rows[0].2.as_deref(), Some("gen-facts"));
        assert_eq!(
            rows[0].3,
            Some(serde_json::json!({"facts": ["用户在深圳工作"], "details": []}))
        );
        // Since B1 usage comes from the parent row, which `seed_generation`
        // writes without one. The join reaching it is what this pins.
        assert_eq!(rows[0].4, None);
        assert_eq!(rows[1].0, "structured");
        assert_eq!(rows[1].1, "parse_error");
        assert_eq!(rows[1].2, None); // default meta ⇒ NULL generation_id
        assert_eq!(rows[1].3, None); // parse_error ⇒ NULL payload
        assert_eq!(rows[1].4, None); // default meta ⇒ NULL usage
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0025_creates_events_table_and_affinity_audit_cols(pool: PgPool) {
        // companion_insights_events exists with every column. session_id and
        // message_id are FK-referenced since 0058, so they have to be real.
        let user_id = Uuid::new_v4();
        let session_id = crate::testutil::seed_chat_session(&pool, user_id).await;
        let message_id = crate::testutil::seed_chat_message(&pool, session_id).await;
        // ...and generation_id since 0060.
        crate::testutil::seed_generation(&pool, "g").await;
        sqlx::query(
            "INSERT INTO engine.companion_insights_events \
               (run_id, user_id, session_id, message_id, stage, status, payload, model, usage, generation_id) \
             VALUES ($1,$2,$3,$4,'facts','ok','[]'::jsonb,'m','{}'::jsonb,'g')",
        )
        .bind(Uuid::new_v4())
        .bind(user_id)
        .bind(session_id)
        .bind(message_id)
        .execute(&pool)
        .await
        .expect("insert into companion_insights_events");

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM engine.companion_insights_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1);

        // companion_affinity_events now has the audit trio (select compiles ⇒ columns exist).
        let _ =
            sqlx::query("SELECT model, usage, generation_id FROM engine.companion_affinity_events")
                .fetch_all(&pool)
                .await
                .expect("affinity audit columns exist");
    }
}
