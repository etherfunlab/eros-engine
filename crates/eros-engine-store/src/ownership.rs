// SPDX-License-Identifier: AGPL-3.0-only
//! Persona-ownership mirror, fed by /s2s/ownership/upsert and the
//! self-heal /since pull. Also exposes the gate-decision `owns()` join
//! that the chat-start and per-message handlers call.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize, serde::Deserialize)]
pub struct Ownership {
    pub asset_id: String,
    pub persona_id: String,
    pub owner_wallet: String,
    pub source_updated_at: DateTime<Utc>,
}

pub struct OwnershipRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> OwnershipRepo<'a> {
    /// Idempotent UPSERT with stale-write protection.
    /// Returns `Ok(true)` if applied, `Ok(false)` if dropped as stale.
    pub async fn upsert(
        &self,
        asset_id: &str,
        persona_id: &str,
        owner_wallet: &str,
        source_updated_at: DateTime<Utc>,
    ) -> sqlx::Result<bool> {
        let res = sqlx::query(
            "INSERT INTO engine.persona_ownership
                (asset_id, persona_id, owner_wallet, source_updated_at, updated_at)
             VALUES ($1, $2, $3, $4, now())
             ON CONFLICT (asset_id) DO UPDATE
               SET persona_id        = EXCLUDED.persona_id,
                   owner_wallet      = EXCLUDED.owner_wallet,
                   source_updated_at = EXCLUDED.source_updated_at,
                   updated_at        = now()
               WHERE EXCLUDED.source_updated_at > engine.persona_ownership.source_updated_at",
        )
        .bind(asset_id)
        .bind(persona_id)
        .bind(owner_wallet)
        .bind(source_updated_at)
        .execute(self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Cursor-paginated `since` read. cursor_pk is `asset_id`.
    pub async fn since(
        &self,
        cursor_ts: DateTime<Utc>,
        cursor_pk: &str,
        limit: i64,
    ) -> sqlx::Result<Vec<Ownership>> {
        sqlx::query_as::<_, Ownership>(
            "SELECT asset_id, persona_id, owner_wallet, source_updated_at
               FROM engine.persona_ownership
              WHERE (source_updated_at, asset_id) > ($1, $2)
              ORDER BY source_updated_at ASC, asset_id ASC
              LIMIT $3",
        )
        .bind(cursor_ts)
        .bind(cursor_pk)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }

    /// Gate decision. Returns true iff `user_id` has at least one *active*
    /// wallet link to the wallet currently recorded as owning `asset_id`.
    /// `linked = true` filter excludes tombstones.
    pub async fn owns(&self, user_id: Uuid, asset_id: &str) -> sqlx::Result<bool> {
        let owns: bool = sqlx::query_scalar(
            "SELECT EXISTS (
               SELECT 1
                 FROM engine.persona_ownership po
                 JOIN engine.wallet_links wl
                   ON wl.wallet_pubkey = po.owner_wallet
                WHERE po.asset_id = $1
                  AND wl.user_id  = $2
                  AND wl.linked   = true
             )",
        )
        .bind(asset_id)
        .bind(user_id)
        .fetch_one(self.pool)
        .await?;
        Ok(owns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn ownership_upsert_drops_stale(pool: PgPool) {
        let repo = OwnershipRepo { pool: &pool };
        let asset = "11111111111111111111111111111111";
        let wallet_old = "OwnerOld1111111111111111111111111";
        let wallet_new = "OwnerNew2222222222222222222222222";

        assert!(repo
            .upsert(asset, "p-1", wallet_old, ts(100))
            .await
            .unwrap());
        assert!(repo
            .upsert(asset, "p-1", wallet_new, ts(200))
            .await
            .unwrap());
        // Older event must NOT revert.
        assert!(!repo
            .upsert(asset, "p-1", wallet_old, ts(150))
            .await
            .unwrap());

        let row: (String,) =
            sqlx::query_as("SELECT owner_wallet FROM engine.persona_ownership WHERE asset_id = $1")
                .bind(asset)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, wallet_new);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn owns_passes_for_linked_owner(pool: PgPool) {
        use crate::wallets::WalletLinkRepo;
        let own = OwnershipRepo { pool: &pool };
        let wl = WalletLinkRepo { pool: &pool };

        let user = Uuid::new_v4();
        let wallet = "BvHvbHBeF2zXa1pT5eExMzTAydPGFTyhqMAbPyuMTfQt";
        let asset = "11111111111111111111111111111111";

        wl.upsert(user, wallet, true, ts(100)).await.unwrap();
        own.upsert(asset, "p-1", wallet, ts(100)).await.unwrap();

        assert!(own.owns(user, asset).await.unwrap());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn owns_rejects_unlinked_owner(pool: PgPool) {
        use crate::wallets::WalletLinkRepo;
        let own = OwnershipRepo { pool: &pool };
        let wl = WalletLinkRepo { pool: &pool };

        let user = Uuid::new_v4();
        let wallet = "BvHvbHBeF2zXa1pT5eExMzTAydPGFTyhqMAbPyuMTfQt";
        let asset = "11111111111111111111111111111111";

        wl.upsert(user, wallet, true, ts(100)).await.unwrap();
        own.upsert(asset, "p-1", wallet, ts(100)).await.unwrap();
        // Unlink the wallet.
        wl.upsert(user, wallet, false, ts(200)).await.unwrap();

        assert!(
            !own.owns(user, asset).await.unwrap(),
            "tombstone must block gate"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn owns_rejects_when_someone_else_owns(pool: PgPool) {
        use crate::wallets::WalletLinkRepo;
        let own = OwnershipRepo { pool: &pool };
        let wl = WalletLinkRepo { pool: &pool };

        let user = Uuid::new_v4();
        let my_wallet = "MyWallet111111111111111111111111";
        let their_wallet = "TheirWallet22222222222222222222";
        let asset = "11111111111111111111111111111111";

        wl.upsert(user, my_wallet, true, ts(100)).await.unwrap();
        own.upsert(asset, "p-1", their_wallet, ts(100))
            .await
            .unwrap();

        assert!(!own.owns(user, asset).await.unwrap());
    }
}
