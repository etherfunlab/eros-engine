// SPDX-License-Identifier: AGPL-3.0-only
//! The relationship scanner's readout (`engine.relationship_scan_events`): one
//! row per scan or arc attempt, counts and ids only.

use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, Default)]
pub struct ScanEventInsert {
    pub instance_id: Option<Uuid>,
    /// `scan`, `arc` or `backfill`.
    pub kind: &'static str,
    /// `ok`, `empty`, `parse_error`, `rejected` or `failed`.
    pub status: &'static str,
    /// The configured chain entry that served the call.
    pub model: Option<String>,
    /// `true` when a fallback hop, not the primary, served the call.
    pub served_fallback: bool,
    pub rows_covered: i32,
    pub memories_written: i32,
    pub episode_written: bool,
    pub journal_written: i32,
    pub generation_id: Option<String>,
}

pub struct ScanEventRepo<'a> {
    pub pool: &'a PgPool,
}

impl ScanEventRepo<'_> {
    /// Append one row. Fail-open: a lost readout row is logged, never raised,
    /// so it cannot undo the scan it describes.
    pub async fn record(&self, ev: ScanEventInsert) {
        let written = sqlx::query(
            "INSERT INTO engine.relationship_scan_events \
                 (instance_id, kind, status, model, served_fallback, rows_covered, \
                  memories_written, episode_written, journal_written, generation_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(ev.instance_id)
        .bind(ev.kind)
        .bind(ev.status)
        .bind(ev.model.as_deref())
        .bind(ev.served_fallback)
        .bind(ev.rows_covered)
        .bind(ev.memories_written)
        .bind(ev.episode_written)
        .bind(ev.journal_written)
        .bind(ev.generation_id.as_deref())
        .execute(self.pool)
        .await;
        if let Err(e) = written {
            tracing::warn!(
                instance_id = ?ev.instance_id,
                kind = ev.kind,
                status = ev.status,
                "relationship scan event persist failed: {e}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::seed_persona_instance;

    type Row = (
        Option<Uuid>,
        String,
        String,
        Option<String>,
        bool,
        i32,
        i32,
        bool,
        i32,
        Option<String>,
    );

    async fn rows(pool: &PgPool) -> Vec<Row> {
        sqlx::query_as(
            "SELECT instance_id, kind, status, model, served_fallback, rows_covered, \
                    memories_written, episode_written, journal_written, generation_id \
             FROM engine.relationship_scan_events ORDER BY id",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn scan_event_record_writes_every_column(pool: PgPool) {
        let repo = ScanEventRepo { pool: &pool };
        let instance = seed_persona_instance(&pool, Uuid::new_v4()).await;

        repo.record(ScanEventInsert {
            instance_id: Some(instance),
            kind: "scan",
            status: "ok",
            model: Some("scan/f".to_string()),
            served_fallback: true,
            rows_covered: 12,
            memories_written: 2,
            episode_written: true,
            journal_written: 1,
            generation_id: Some("gen-1".to_string()),
        })
        .await;
        repo.record(ScanEventInsert {
            instance_id: Some(instance),
            kind: "arc",
            status: "rejected",
            ..Default::default()
        })
        .await;

        assert_eq!(
            rows(&pool).await,
            vec![
                (
                    Some(instance),
                    "scan".to_string(),
                    "ok".to_string(),
                    Some("scan/f".to_string()),
                    true,
                    12,
                    2,
                    true,
                    1,
                    Some("gen-1".to_string()),
                ),
                (
                    Some(instance),
                    "arc".to_string(),
                    "rejected".to_string(),
                    None,
                    false,
                    0,
                    0,
                    false,
                    0,
                    None,
                ),
            ]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn scan_event_record_is_fail_open(pool: PgPool) {
        let repo = ScanEventRepo { pool: &pool };
        let instance = seed_persona_instance(&pool, Uuid::new_v4()).await;

        // A status outside the vocabulary and an instance that does not exist
        // are both refused by the table; neither reaches the caller.
        repo.record(ScanEventInsert {
            instance_id: Some(instance),
            kind: "scan",
            status: "exploded",
            ..Default::default()
        })
        .await;
        repo.record(ScanEventInsert {
            instance_id: Some(Uuid::new_v4()),
            kind: "scan",
            status: "ok",
            ..Default::default()
        })
        .await;

        assert!(rows(&pool).await.is_empty());
    }
}
