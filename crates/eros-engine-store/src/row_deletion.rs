// SPDX-License-Identifier: AGPL-3.0-only
//! Row tombstones (`engine.row_deletions`): one per profile memory or ledger
//! fact its owner deleted, ids only. Mirrors read them through
//! `GET /svc/v1/rows/deletions` (spec
//! 2026-11-10-row-deletion-and-deletion-stream-design.md §4).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// The kind of row a tombstone stands for.
pub const KIND_MEMORY: &str = "memory";
pub const KIND_FACT: &str = "fact";

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, PartialEq)]
pub struct RowDeletion {
    pub kind: String,
    pub id: Uuid,
    pub deleted_at: DateTime<Utc>,
}

pub struct RowDeletionRepo<'a> {
    pub pool: &'a PgPool,
}

impl RowDeletionRepo<'_> {
    /// Record a deleted row inside the transaction that deletes it.
    pub async fn insert_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        kind: &str,
        id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO engine.row_deletions (kind, id) VALUES ($1, $2)")
            .bind(kind)
            .bind(id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// Tombstones strictly after the `(since, after_id)` keyset tuple, in
    /// `(deleted_at, id)` order, at most `limit`. Uses
    /// `idx_row_deletions_export`.
    pub async fn list_changes(
        &self,
        since: DateTime<Utc>,
        after_id: Uuid,
        limit: i64,
    ) -> Result<Vec<RowDeletion>, sqlx::Error> {
        sqlx::query_as::<_, RowDeletion>(
            "SELECT kind, id, deleted_at FROM engine.row_deletions \
             WHERE (deleted_at, id) > ($1, $2) \
             ORDER BY deleted_at, id \
             LIMIT $3",
        )
        .bind(since)
        .bind(after_id)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    async fn tombstone(pool: &PgPool, kind: &str, id: Uuid, at: DateTime<Utc>) {
        sqlx::query("INSERT INTO engine.row_deletions (kind, id, deleted_at) VALUES ($1, $2, $3)")
            .bind(kind)
            .bind(id)
            .bind(at)
            .execute(pool)
            .await
            .unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn list_changes_pages_by_deleted_at_then_id(pool: PgPool) {
        let t1 = Utc.with_ymd_and_hms(2026, 11, 10, 8, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2026, 11, 10, 9, 0, 0).unwrap();
        let mut ids = [Uuid::new_v4(), Uuid::new_v4()];
        ids.sort();
        tombstone(&pool, KIND_FACT, ids[1], t1).await;
        tombstone(&pool, KIND_MEMORY, ids[0], t1).await;
        let late = Uuid::new_v4();
        tombstone(&pool, KIND_FACT, late, t2).await;

        let repo = RowDeletionRepo { pool: &pool };
        let page = repo
            .list_changes(DateTime::UNIX_EPOCH, Uuid::nil(), 2)
            .await
            .unwrap();
        assert_eq!(
            page.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![ids[0], ids[1]],
            "ties on deleted_at are ordered by id"
        );
        assert_eq!(page[0].kind, KIND_MEMORY);
        let last = page.last().unwrap();
        let rest = repo
            .list_changes(last.deleted_at, last.id, 2)
            .await
            .unwrap();
        assert_eq!(rest.iter().map(|r| r.id).collect::<Vec<_>>(), vec![late]);
        assert!(repo.list_changes(t2, late, 2).await.unwrap().is_empty());
    }
}
