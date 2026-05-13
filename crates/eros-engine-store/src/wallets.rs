// SPDX-License-Identifier: AGPL-3.0-only
//! Wallet ↔ user binding mirror, fed by /s2s/wallets/upsert and the
//! self-heal /since pull. Maintains the invariant that "one wallet is
//! bound to at most one user at a time" via the partial unique index
//! defined in migration 0009.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize, serde::Deserialize)]
pub struct WalletLink {
    pub user_id: Uuid,
    pub wallet_pubkey: String,
    pub linked: bool,
    pub source_updated_at: DateTime<Utc>,
}

pub struct WalletLinkRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> WalletLinkRepo<'a> {
    /// Idempotent UPSERT with stale-write protection: only applies if
    /// `incoming.source_updated_at > existing.source_updated_at`.
    /// Returns `Ok(true)` if the row was applied, `Ok(false)` if dropped as stale.
    pub async fn upsert(
        &self,
        user_id: Uuid,
        wallet_pubkey: &str,
        linked: bool,
        source_updated_at: DateTime<Utc>,
    ) -> sqlx::Result<bool> {
        let res = sqlx::query(
            "INSERT INTO engine.wallet_links
                (user_id, wallet_pubkey, linked, linked_at, source_updated_at, updated_at)
             VALUES ($1, $2, $3, $4, $4, now())
             ON CONFLICT (user_id, wallet_pubkey) DO UPDATE
               SET linked            = EXCLUDED.linked,
                   source_updated_at = EXCLUDED.source_updated_at,
                   updated_at        = now()
               WHERE EXCLUDED.source_updated_at > engine.wallet_links.source_updated_at",
        )
        .bind(user_id)
        .bind(wallet_pubkey)
        .bind(linked)
        .bind(source_updated_at)
        .execute(self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Cursor-paginated `since` read. `cursor_pk` for wallets is the string
    /// "{user_id}:{wallet_pubkey}". An empty cursor_pk pairs with cursor_ts
    /// to start from the very beginning of the table.
    pub async fn since(
        &self,
        cursor_ts: DateTime<Utc>,
        cursor_pk: &str,
        limit: i64,
    ) -> sqlx::Result<Vec<WalletLink>> {
        // Parse cursor_pk = "{user_id}:{wallet_pubkey}" into the typed pair.
        // Empty cursor_pk → start-of-table sentinel (Uuid::nil + empty pubkey).
        let (cur_user, cur_pubkey) = match cursor_pk.split_once(':') {
            Some((u, p)) => (Uuid::parse_str(u).unwrap_or(Uuid::nil()), p),
            None => (Uuid::nil(), ""),
        };
        sqlx::query_as::<_, WalletLink>(
            "SELECT user_id, wallet_pubkey, linked, source_updated_at
               FROM engine.wallet_links
              WHERE (source_updated_at, user_id, wallet_pubkey)
                  > ($1, $2, $3)
              ORDER BY source_updated_at ASC, user_id ASC, wallet_pubkey ASC
              LIMIT $4",
        )
        .bind(cursor_ts)
        .bind(cur_user)
        .bind(cur_pubkey)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_applies_then_drops_stale(pool: PgPool) {
        let repo = WalletLinkRepo { pool: &pool };
        let u = Uuid::new_v4();
        let w = "BvHvbHBeF2zXa1pT5eExMzTAydPGFTyhqMAbPyuMTfQt";

        assert!(repo.upsert(u, w, true, ts(100)).await.unwrap());
        // Newer event applies.
        assert!(repo.upsert(u, w, true, ts(200)).await.unwrap());
        // Older event is silently dropped (returns false, no row affected).
        assert!(!repo.upsert(u, w, false, ts(150)).await.unwrap());

        let row: (bool, DateTime<Utc>) = sqlx::query_as(
            "SELECT linked, source_updated_at FROM engine.wallet_links
              WHERE user_id = $1 AND wallet_pubkey = $2",
        )
        .bind(u)
        .bind(w)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            row.0,
            "linked must stay true since stale unlink was dropped"
        );
        assert_eq!(row.1, ts(200));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unlink_writes_tombstone_not_delete(pool: PgPool) {
        let repo = WalletLinkRepo { pool: &pool };
        let u = Uuid::new_v4();
        let w = "BvHvbHBeF2zXa1pT5eExMzTAydPGFTyhqMAbPyuMTfQt";

        repo.upsert(u, w, true, ts(100)).await.unwrap();
        repo.upsert(u, w, false, ts(200)).await.unwrap();

        let row: (bool,) = sqlx::query_as(
            "SELECT linked FROM engine.wallet_links
              WHERE user_id = $1 AND wallet_pubkey = $2",
        )
        .bind(u)
        .bind(w)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!row.0, "row remains as a tombstone");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn since_paginates_compound_cursor(pool: PgPool) {
        let repo = WalletLinkRepo { pool: &pool };
        // Two rows at the same source_updated_at — must not split across pages.
        let u1 = Uuid::new_v4();
        let u2 = Uuid::new_v4();
        let w1 = "11111111111111111111111111111111";
        let w2 = "11111111111111111111111111111112";
        repo.upsert(u1, w1, true, ts(100)).await.unwrap();
        repo.upsert(u2, w2, true, ts(100)).await.unwrap();

        let page1 = repo
            .since(DateTime::<Utc>::from_timestamp(0, 0).unwrap(), "", 1)
            .await
            .unwrap();
        assert_eq!(page1.len(), 1);
        let last = &page1[0];
        let cursor_pk = format!("{}:{}", last.user_id, last.wallet_pubkey);
        let page2 = repo
            .since(last.source_updated_at, &cursor_pk, 10)
            .await
            .unwrap();
        assert_eq!(page2.len(), 1, "second page must contain the second row");

        // Strong invariant: the two pages together cover both rows exactly once.
        let mut seen: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
        seen.insert(page1[0].user_id);
        seen.insert(page2[0].user_id);
        assert_eq!(seen.len(), 2, "no duplicate rows across pages");
        assert!(seen.contains(&u1) && seen.contains(&u2));
    }
}
