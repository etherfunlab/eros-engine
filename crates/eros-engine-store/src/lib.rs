// SPDX-License-Identifier: AGPL-3.0-only
//! Postgres + pgvector persistence layer.

pub mod affinity;
pub mod chat;
pub mod error_handling;
pub mod human_insight;
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

    #[sqlx::test(migrations = "./migrations")]
    async fn human_insights_has_rls_enabled(pool: PgPool) {
        let enabled: bool = sqlx::query_scalar(
            "SELECT relrowsecurity FROM pg_class \
             WHERE oid = 'engine.human_insights'::regclass",
        )
        .fetch_one(&pool)
        .await
        .expect("query relrowsecurity for human_insights");
        assert!(enabled, "RLS must be enabled on engine.human_insights");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn sqlx_migrations_has_rls_enabled(pool: PgPool) {
        let enabled: bool = sqlx::query_scalar(
            "SELECT relrowsecurity FROM pg_class \
             WHERE oid = 'public._sqlx_migrations'::regclass",
        )
        .fetch_one(&pool)
        .await
        .expect("query relrowsecurity for _sqlx_migrations");
        assert!(enabled, "RLS must be enabled on public._sqlx_migrations");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn chat_messages_has_no_extracted_facts_column(pool: PgPool) {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_schema = 'engine' AND table_name = 'chat_messages' \
               AND column_name = 'extracted_facts')",
        )
        .fetch_one(&pool)
        .await
        .expect("query for extracted_facts column");
        assert!(
            !exists,
            "extracted_facts column must be dropped (migration 0017)"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0020_seeds_ten_fallback_phrases(pool: PgPool) {
        let payload: serde_json::Value = sqlx::query_scalar(
            "SELECT payload FROM engine.error_handling_config \
             WHERE kind = 'chat_stream_failure_fallback_phrases'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let arr = payload.as_array().expect("payload is an array");
        assert_eq!(arr.len(), 10, "seed must carry exactly 10 phrases");
        for item in arr {
            assert!(item.is_string(), "each phrase must be a string: {item}");
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn pick_chat_stream_fallback_phrase_returns_seeded_phrase(pool: PgPool) {
        use crate::error_handling::ErrorHandlingRepo;
        let repo = ErrorHandlingRepo { pool: &pool };
        let phrase = repo.pick_chat_stream_fallback_phrase().await.unwrap();
        let phrase = phrase.expect("seeded phrase should be available");
        let seeded = [
            "huh?",
            "hm?",
            "...",
            "oh?",
            "mhm",
            "ok",
            "👀",
            "😅",
            "say again?",
            "wait what?",
        ];
        assert!(
            seeded.contains(&phrase.as_str()),
            "picked {phrase:?} not in seed"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn pick_chat_stream_fallback_phrase_returns_none_when_kind_missing(pool: PgPool) {
        use crate::error_handling::ErrorHandlingRepo;
        let repo = ErrorHandlingRepo { pool: &pool };
        // Clear the seeded row to simulate a fresh DB without the kind.
        sqlx::query(
            "DELETE FROM engine.error_handling_config \
             WHERE kind = 'chat_stream_failure_fallback_phrases'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let phrase = repo.pick_chat_stream_fallback_phrase().await.unwrap();
        assert!(phrase.is_none(), "expected None when config row absent");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn companion_insights_snapshot_schema(pool: PgPool) {
        // Table exists with the five expected columns at expected types.
        let cols: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT column_name, data_type, is_nullable \
             FROM information_schema.columns \
             WHERE table_schema = 'engine' \
               AND table_name   = 'companion_insights_snapshot' \
             ORDER BY ordinal_position",
        )
        .fetch_all(&pool)
        .await
        .expect("query columns for companion_insights_snapshot");
        let names: Vec<&str> = cols.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["id", "user_id", "insights", "training_level", "captured_at"],
            "column order/identity must match the migration"
        );
        // captured_at must be NOT NULL — sweeper sets it explicitly.
        let captured_at_null = cols
            .iter()
            .find(|(n, _, _)| n == "captured_at")
            .map(|(_, _, nullable)| nullable.as_str())
            .unwrap();
        assert_eq!(captured_at_null, "NO", "captured_at must be NOT NULL");

        // Index on (user_id, captured_at DESC) exists.
        let idx_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS ( \
                SELECT 1 FROM pg_indexes \
                 WHERE schemaname = 'engine' \
                   AND tablename  = 'companion_insights_snapshot' \
                   AND indexname  = 'idx_companion_insights_snapshot_user_time')",
        )
        .fetch_one(&pool)
        .await
        .expect("query pg_indexes");
        assert!(
            idx_exists,
            "idx_companion_insights_snapshot_user_time must be created by 0021"
        );

        // RLS enabled (no policy → server-side only access).
        let rls_enabled: bool = sqlx::query_scalar(
            "SELECT relrowsecurity FROM pg_class \
              WHERE oid = 'engine.companion_insights_snapshot'::regclass",
        )
        .fetch_one(&pool)
        .await
        .expect("query relrowsecurity");
        assert!(
            rls_enabled,
            "RLS must be enabled on companion_insights_snapshot"
        );
    }
}
