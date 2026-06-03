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

/// OpenRouter call metadata captured for the audit columns on event tables
/// (`companion_insights_events`, `companion_affinity_events`). All optional —
/// a non-LLM event (e.g. a gift affinity event) carries the default (all None).
#[derive(Debug, Clone, Default)]
pub struct OpenRouterCallMeta {
    pub generation_id: Option<String>,
    pub model: Option<String>,
    pub usage: Option<serde_json::Value>,
}

#[cfg(test)]
mod migration_tests {
    use sqlx::PgPool;

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

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0023_drops_nft_ownership_stack(pool: PgPool) {
        // The three tables are gone.
        for tbl in ["wallet_links", "persona_ownership", "sync_cursors"] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
                 WHERE table_schema = 'engine' AND table_name = $1)",
            )
            .bind(tbl)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(!exists, "engine.{tbl} must be dropped by migration 0023");
        }
        // persona_genomes.asset_id is gone.
        let col_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_schema = 'engine' AND table_name = 'persona_genomes' \
               AND column_name = 'asset_id')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!col_exists, "persona_genomes.asset_id must be dropped");
        // persona_genomes itself survives (sanity).
        let pg_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_schema = 'engine' AND table_name = 'persona_genomes')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(pg_exists, "persona_genomes table must survive");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0024_strips_persona_genomes_to_chat_data(pool: PgPool) {
        for col in ["is_active", "avatar_url"] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
                 WHERE table_schema = 'engine' AND table_name = 'persona_genomes' \
                   AND column_name = $1)",
            )
            .bind(col)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(
                !exists,
                "persona_genomes.{col} must be dropped by migration 0024"
            );
        }
        // The chat-relevant columns survive.
        for col in ["name", "system_prompt", "tip_personality", "art_metadata"] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
                 WHERE table_schema = 'engine' AND table_name = 'persona_genomes' \
                   AND column_name = $1)",
            )
            .bind(col)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(exists, "persona_genomes.{col} must survive migration 0024");
        }
    }
}
