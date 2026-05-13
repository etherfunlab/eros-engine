// SPDX-License-Identifier: AGPL-3.0-only
//! Postgres + pgvector persistence layer.

pub mod affinity;
pub mod chat;
pub mod insight;
pub mod memory;
pub mod persona;
pub mod pool;
pub mod pubkey;
pub mod wallets;

pub use sqlx::PgPool;

#[cfg(test)]
mod migration_tests {
    use sqlx::PgPool;

    #[sqlx::test(migrations = "./migrations")]
    async fn wallet_links_schema_is_correct(pool: PgPool) {
        // Insert one row; assert the columns we documented exist.
        sqlx::query(
            "INSERT INTO engine.wallet_links
                (user_id, wallet_pubkey, linked, source_updated_at)
             VALUES ($1, $2, true, now())",
        )
        .bind(uuid::Uuid::new_v4())
        .bind("BvHvbHBeF2zXa1pT5eExMzTAydPGFTyhqMAbPyuMTfQt")
        .execute(&pool)
        .await
        .expect("insert into wallet_links");

        // The partial unique index allows another row with linked=false.
        let same_wallet = "BvHvbHBeF2zXa1pT5eExMzTAydPGFTyhqMAbPyuMTfQt";
        let res = sqlx::query(
            "INSERT INTO engine.wallet_links
                (user_id, wallet_pubkey, linked, source_updated_at)
             VALUES ($1, $2, false, now())",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(same_wallet)
        .execute(&pool)
        .await;
        // The PK is (user_id, wallet_pubkey) so different user_id is fine.
        // The active-only UNIQUE index is on wallet_pubkey WHERE linked=true,
        // so a tombstone for the same pubkey but different user_id is allowed.
        assert!(res.is_ok(), "tombstone insert must succeed: {res:?}");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persona_ownership_and_sync_cursors_schema(pool: PgPool) {
        // persona_ownership: PK = asset_id, must accept source_updated_at.
        let asset = "11111111111111111111111111111111";
        sqlx::query(
            "INSERT INTO engine.persona_ownership
                (asset_id, persona_id, owner_wallet, source_updated_at)
             VALUES ($1, 'persona-test', 'OwnerWallet1111111111111111111111', now())",
        )
        .bind(asset)
        .execute(&pool)
        .await
        .expect("insert into persona_ownership");

        // sync_cursors: PK = name, compound (cursor_ts, cursor_pk) writeable.
        sqlx::query(
            "INSERT INTO engine.sync_cursors (name, cursor_ts, cursor_pk)
             VALUES ('ownership', now(), '')",
        )
        .execute(&pool)
        .await
        .expect("insert into sync_cursors");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persona_genomes_gains_nullable_asset_id(pool: PgPool) {
        // The legacy seed-persona path inserts WITHOUT asset_id and must keep working.
        let legacy_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.persona_genomes
                (id, name, system_prompt, art_metadata, is_active)
             VALUES ($1, 'LegacyGenome', 'prompt', '{}'::jsonb, true)",
        )
        .bind(legacy_id)
        .execute(&pool)
        .await
        .expect("legacy insert without asset_id");

        // A new NFT-backed genome carries asset_id.
        let nft_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.persona_genomes
                (id, name, system_prompt, art_metadata, is_active, asset_id)
             VALUES ($1, 'NftGenome', 'prompt', '{}'::jsonb, true, $2)",
        )
        .bind(nft_id)
        .bind("11111111111111111111111111111112")
        .execute(&pool)
        .await
        .expect("nft insert with asset_id");

        // Partial unique: the same non-NULL asset_id cannot be claimed twice.
        let dup_id = uuid::Uuid::new_v4();
        let dup_res = sqlx::query(
            "INSERT INTO engine.persona_genomes
                (id, name, system_prompt, art_metadata, is_active, asset_id)
             VALUES ($1, 'Dup', 'prompt', '{}'::jsonb, true, $2)",
        )
        .bind(dup_id)
        .bind("11111111111111111111111111111112")
        .execute(&pool)
        .await;
        assert!(dup_res.is_err(), "duplicate asset_id must be rejected");
    }
}
