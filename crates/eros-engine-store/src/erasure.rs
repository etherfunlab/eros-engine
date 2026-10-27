// SPDX-License-Identifier: AGPL-3.0-only
//! User-level erasure tombstones (`engine.user_erasures`): one row per erased
//! user, the deletion stream downstream mirrors read.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ErasureRow {
    pub user_id: Uuid,
    pub erased_at: DateTime<Utc>,
}

pub struct ErasureRepo<'a> {
    pub pool: &'a PgPool,
}

impl ErasureRepo<'_> {
    /// Tombstones strictly after the `(since, after_user)` keyset tuple, in
    /// `(erased_at, user_id)` order, at most `limit`. The tuple comparison keeps
    /// rows sharing one timestamp from being skipped or repeated across a page
    /// boundary. Uses `idx_user_erasures_erased_at`.
    pub async fn list_changes(
        &self,
        since: DateTime<Utc>,
        after_user: Uuid,
        limit: i64,
    ) -> Result<Vec<ErasureRow>, sqlx::Error> {
        sqlx::query_as::<_, ErasureRow>(
            "SELECT user_id, erased_at FROM engine.user_erasures \
             WHERE (erased_at, user_id) > ($1, $2) \
             ORDER BY erased_at, user_id \
             LIMIT $3",
        )
        .bind(since)
        .bind(after_user)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[sqlx::test(migrations = "./migrations")]
    async fn list_changes_keyset_pages_without_skip_or_repeat(pool: PgPool) {
        let repo = ErasureRepo { pool: &pool };
        let t0 = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap();
        // Three tombstones tie on t0, one is later.
        let mut expected: Vec<(DateTime<Utc>, Uuid)> = Vec::new();
        for at in [t0, t0, t0, t1] {
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO engine.user_erasures (user_id, erased_at) VALUES ($1, $2)")
                .bind(id)
                .bind(at)
                .execute(&pool)
                .await
                .unwrap();
            expected.push((at, id));
        }
        expected.sort();

        let mut walked = Vec::new();
        let mut cursor = (DateTime::<Utc>::UNIX_EPOCH, Uuid::nil());
        loop {
            let page = repo.list_changes(cursor.0, cursor.1, 2).await.unwrap();
            walked.extend(page.iter().map(|r| (r.erased_at, r.user_id)));
            if page.len() < 2 {
                break;
            }
            let last = page.last().unwrap();
            cursor = (last.erased_at, last.user_id);
        }
        assert_eq!(
            walked, expected,
            "every row once, in (erased_at, user_id) order"
        );
    }
}
