// SPDX-License-Identifier: AGPL-3.0-only
//! Postgres + pgvector persistence layer.

pub mod affinity;
pub mod chat;
pub mod insight;
pub mod memory;
pub mod persona;
pub mod pool;

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
}
