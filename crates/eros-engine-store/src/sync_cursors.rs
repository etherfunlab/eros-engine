// SPDX-License-Identifier: AGPL-3.0-only
//! Compound (cursor_ts, cursor_pk) persistence for the self-heal loop.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

#[derive(Debug, Clone)]
pub struct Cursor {
    pub cursor_ts: DateTime<Utc>,
    pub cursor_pk: String,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            cursor_ts: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            cursor_pk: String::new(),
        }
    }
}

pub struct SyncCursorRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> SyncCursorRepo<'a> {
    /// Read the cursor for `name`, returning the epoch+empty default if no
    /// row exists yet.
    pub async fn get(&self, name: &str) -> sqlx::Result<Cursor> {
        let row: Option<(DateTime<Utc>, String)> =
            sqlx::query_as("SELECT cursor_ts, cursor_pk FROM engine.sync_cursors WHERE name = $1")
                .bind(name)
                .fetch_optional(self.pool)
                .await?;
        Ok(row
            .map(|(ts, pk)| Cursor {
                cursor_ts: ts,
                cursor_pk: pk,
            })
            .unwrap_or_default())
    }

    /// Idempotent UPSERT — overwrites the previous cursor with the new one.
    pub async fn set(&self, name: &str, cursor: &Cursor) -> sqlx::Result<()> {
        sqlx::query(
            "INSERT INTO engine.sync_cursors (name, cursor_ts, cursor_pk, updated_at)
             VALUES ($1, $2, $3, now())
             ON CONFLICT (name) DO UPDATE
               SET cursor_ts  = EXCLUDED.cursor_ts,
                   cursor_pk  = EXCLUDED.cursor_pk,
                   updated_at = now()",
        )
        .bind(name)
        .bind(cursor.cursor_ts)
        .bind(&cursor.cursor_pk)
        .execute(self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "./migrations")]
    async fn missing_cursor_returns_epoch_default(pool: PgPool) {
        let repo = SyncCursorRepo { pool: &pool };
        let c = repo.get("ownership").await.unwrap();
        assert_eq!(c.cursor_ts, DateTime::<Utc>::from_timestamp(0, 0).unwrap());
        assert_eq!(c.cursor_pk, "");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn set_then_get_roundtrips(pool: PgPool) {
        let repo = SyncCursorRepo { pool: &pool };
        let want = Cursor {
            cursor_ts: DateTime::<Utc>::from_timestamp(1700000000, 0).unwrap(),
            cursor_pk: "11111111111111111111111111111111".into(),
        };
        repo.set("ownership", &want).await.unwrap();
        let got = repo.get("ownership").await.unwrap();
        assert_eq!(got.cursor_ts, want.cursor_ts);
        assert_eq!(got.cursor_pk, want.cursor_pk);
    }
}
