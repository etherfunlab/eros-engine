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
    pub meta: crate::OpenRouterCallMeta,
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
                model, usage, generation_id) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
        )
        .bind(ev.run_id)
        .bind(ev.user_id)
        .bind(ev.session_id)
        .bind(ev.message_id)
        .bind(ev.stage)
        .bind(ev.status)
        .bind(ev.payload)
        .bind(ev.meta.model)
        .bind(ev.meta.usage)
        .bind(ev.meta.generation_id)
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
        let session_id = Uuid::new_v4();
        let message_id = Uuid::new_v4();

        repo.record(InsightEventInsert {
            run_id,
            user_id,
            session_id: Some(session_id),
            message_id: Some(message_id),
            stage: "facts",
            status: "ok",
            payload: Some(serde_json::json!({"facts": ["用户在深圳工作"], "details": []})),
            meta: crate::OpenRouterCallMeta {
                generation_id: Some("gen-facts".into()),
                model: Some("ins/m".into()),
                usage: Some(serde_json::json!({"total_tokens": 7})),
            },
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
            meta: crate::OpenRouterCallMeta::default(),
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
            "SELECT stage, status, generation_id, payload, usage \
             FROM engine.companion_insights_events \
             WHERE run_id = $1 ORDER BY stage",
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
        assert_eq!(rows[0].4, Some(serde_json::json!({"total_tokens": 7})));
        assert_eq!(rows[1].0, "structured");
        assert_eq!(rows[1].1, "parse_error");
        assert_eq!(rows[1].2, None); // default meta ⇒ NULL generation_id
        assert_eq!(rows[1].3, None); // parse_error ⇒ NULL payload
        assert_eq!(rows[1].4, None); // default meta ⇒ NULL usage
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0025_creates_events_table_and_affinity_audit_cols(pool: PgPool) {
        // companion_insights_events exists with every column.
        sqlx::query(
            "INSERT INTO engine.companion_insights_events \
               (run_id, user_id, session_id, message_id, stage, status, payload, model, usage, generation_id) \
             VALUES ($1,$2,$3,$4,'facts','ok','[]'::jsonb,'m','{}'::jsonb,'g')",
        )
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
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
