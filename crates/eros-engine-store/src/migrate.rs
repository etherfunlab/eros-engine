// SPDX-License-Identifier: AGPL-3.0-only
//! The migrate entry: collapse legacy `_sqlx_migrations` history onto the
//! baseline, then run the embedded sqlx migrator.
//!
//! `0000_baseline.sql` holds the v2.3.1 schema (the folded 0000–0077). A
//! database that lived through those files carries rows 0–77 in
//! `_sqlx_migrations` and must never run the baseline; [`migrate`] rewrites
//! that history instead — rows 0–77 become one version-0 row with the
//! baseline's checksum — and sqlx then treats the baseline as applied. A
//! fresh database skips the collapse and applies the baseline like any
//! migration. sqlx's own validation (checksums, applied-but-missing) stays
//! on; `set_ignore_missing` is deliberately not used.

use sqlx::PgPool;

/// The embedded migrator: the baseline plus every incremental migration.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Highest folded version: history rows `0..=FLOOR` collapse onto the
/// baseline. 0077 is v2.3.1's last migration; a database that never reached
/// it predates v2.3.1, and its pending migrations below the floor no longer
/// exist as files.
const FLOOR: i64 = 77;

/// [`migrate`]'s failures: the collapse's own refusal, or anything sqlx
/// raises.
#[derive(Debug, thiserror::Error)]
pub enum MigrateFailure {
    #[error(
        "this database predates v2.3.1 (its migration history stops before \
         0077); upgrade to v2.3.1 and run its migrations first"
    )]
    PredatesBaseline,
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
}

/// Apply migrations: collapse, then `Migrator::run`. The supported entry for
/// the engine binary's `migrate` subcommand and for embedders alike.
pub async fn migrate(pool: &PgPool) -> Result<(), MigrateFailure> {
    collapse(pool).await?;
    MIGRATOR.run(pool).await?;
    Ok(())
}

/// Rewrite pre-squash history onto the baseline row. No-op on a fresh
/// database and on an already-collapsed one; refuses a pre-v2.3.1 history.
/// One transaction under an advisory lock, so concurrent runs serialize and
/// a crash leaves the old rows for the next attempt.
async fn collapse(pool: &PgPool) -> Result<(), MigrateFailure> {
    let baseline = MIGRATOR
        .iter()
        .find(|m| m.version == 0)
        .expect("the embedded migrations carry the version-0 baseline");

    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(72310000)")
        .execute(&mut *tx)
        .await?;

    let table_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *tx)
            .await?;
    if !table_exists {
        return Ok(()); // fresh database: the migrator applies the baseline
    }

    let (floor_applied, legacy_rows): (bool, i64) = sqlx::query_as::<_, (Option<bool>, i64)>(
        "SELECT bool_or(version = $1), count(*) FROM _sqlx_migrations WHERE version <= $1",
    )
    .bind(FLOOR)
    .fetch_one(&mut *tx)
    .await
    .map(|(f, c)| (f.unwrap_or(false), c))?;

    if legacy_rows == 0 {
        // Born on the baseline era (or an empty bookkeeping table): nothing
        // to fold; the migrator takes it from here.
        return Ok(());
    }
    if legacy_rows == 1 {
        let row0: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 0")
                .fetch_optional(&mut *tx)
                .await?;
        if row0.as_deref() == Some(baseline.checksum.as_ref()) {
            return Ok(()); // already collapsed
        }
    }
    if !floor_applied {
        return Err(MigrateFailure::PredatesBaseline);
    }

    let folded = sqlx::query("DELETE FROM _sqlx_migrations WHERE version <= $1")
        .bind(FLOOR)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    sqlx::query(
        "INSERT INTO _sqlx_migrations \
             (version, description, installed_on, success, checksum, execution_time) \
         VALUES (0, 'baseline', now(), true, $1, 0)",
    )
    .bind(baseline.checksum.as_ref())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::info!(folded, "migration history folded onto the baseline");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{migrate, MigrateFailure, MIGRATOR};
    use sqlx::PgPool;

    const BASELINE_SQL: &str = include_str!("../migrations/0000_baseline.sql");

    /// `_sqlx_migrations` as sqlx 0.8 creates it; the stub stands in for the
    /// table sqlx would have made before running v2.3.1's migrations.
    async fn stub_bookkeeping(pool: &PgPool) {
        sqlx::raw_sql(
            "CREATE TABLE public._sqlx_migrations (\
                 version BIGINT PRIMARY KEY, description TEXT NOT NULL, \
                 installed_on TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 success BOOLEAN NOT NULL, checksum BYTEA NOT NULL, \
                 execution_time BIGINT NOT NULL)",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    /// Insert dummy history rows `0..=upto` (row 0 standing in for the old
    /// `0000_schema.sql`, whose checksum differs from the baseline's).
    async fn fake_history(pool: &PgPool, upto: i64) {
        sqlx::query(
            "INSERT INTO _sqlx_migrations \
                 (version, description, success, checksum, execution_time) \
             SELECT v, 'legacy', true, '\\xdead'::bytea, 0 \
               FROM generate_series(0, $1) v",
        )
        .bind(upto)
        .execute(pool)
        .await
        .unwrap();
    }

    /// A database that lived through v2.3.1: its schema (the baseline body is
    /// exactly that) plus bookkeeping rows 0..=77.
    async fn fabricate_v2_3_1(pool: &PgPool) {
        stub_bookkeeping(pool).await;
        sqlx::raw_sql(BASELINE_SQL).execute(pool).await.unwrap();
        fake_history(pool, 77).await;
    }

    async fn bookkeeping(pool: &PgPool) -> Vec<(i64, Vec<u8>)> {
        sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// The original chain never pinned pgvector's schema; the baseline must
    /// not either, or a fresh install where the extension already lives
    /// outside `public` (a managed-Postgres toggle) fails on the first
    /// vector column.
    #[test]
    fn baseline_carries_no_schema_pinned_vector() {
        assert!(
            !BASELINE_SQL.contains("WITH SCHEMA public"),
            "extension schema pinned"
        );
        assert!(
            !BASELINE_SQL.contains("public.vector"),
            "vector type/opclass schema-pinned"
        );
    }

    #[sqlx::test(migrations = false)]
    async fn upgrade_from_v2_3_1_collapses_and_applies_increments(pool: PgPool) {
        fabricate_v2_3_1(&pool).await;
        migrate(&pool).await.unwrap();

        let rows = bookkeeping(&pool).await;
        let versions: Vec<i64> = rows.iter().map(|r| r.0).collect();
        assert_eq!(versions, [0, 78, 79, 80, 81], "collapsed + increments");
        let baseline = MIGRATOR.iter().find(|m| m.version == 0).unwrap();
        assert_eq!(
            rows[0].1,
            baseline.checksum.as_ref(),
            "row 0 carries the baseline checksum"
        );

        // 0081's rename reached the fabricated database.
        let renamed: bool =
            sqlx::query_scalar("SELECT to_regclass('engine.human_insights_events') IS NOT NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(renamed, "increments applied on top of the v2.3.1 schema");
        let phrases: i32 = sqlx::query_scalar(
            "SELECT jsonb_array_length(payload) FROM engine.error_handling_config \
              WHERE kind = 'chat_stream_failure_fallback_phrases'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(phrases, 10, "the carried seed survives the collapse");
    }

    #[sqlx::test(migrations = false)]
    async fn fresh_database_applies_baseline_and_increments(pool: PgPool) {
        migrate(&pool).await.unwrap();
        let versions: Vec<i64> = bookkeeping(&pool).await.iter().map(|r| r.0).collect();
        assert_eq!(versions, [0, 78, 79, 80, 81]);
        let phrases: i32 = sqlx::query_scalar(
            "SELECT jsonb_array_length(payload) FROM engine.error_handling_config \
              WHERE kind = 'chat_stream_failure_fallback_phrases'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(phrases, 10, "fresh installs get the seed from the baseline");
    }

    #[sqlx::test(migrations = false)]
    async fn pre_v2_3_1_history_is_refused_untouched(pool: PgPool) {
        stub_bookkeeping(&pool).await;
        fake_history(&pool, 60).await;
        let err = migrate(&pool).await.unwrap_err();
        assert!(
            matches!(err, MigrateFailure::PredatesBaseline),
            "got: {err}"
        );
        assert!(err.to_string().contains("v2.3.1"), "{err}");
        assert_eq!(bookkeeping(&pool).await.len(), 61, "history untouched");
    }

    #[sqlx::test(migrations = false)]
    async fn migrate_is_idempotent_after_the_collapse(pool: PgPool) {
        fabricate_v2_3_1(&pool).await;
        migrate(&pool).await.unwrap();
        let first = bookkeeping(&pool).await;
        migrate(&pool).await.unwrap();
        assert_eq!(
            first,
            bookkeeping(&pool).await,
            "second run changes nothing"
        );
    }

    /// Raw sqlx against an existing database is unsupported; the baseline's
    /// own guard aborts before touching anything.
    #[sqlx::test(migrations = "./migrations")]
    async fn baseline_guard_aborts_on_an_existing_database(pool: PgPool) {
        let err = sqlx::raw_sql(BASELINE_SQL)
            .execute(&pool)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("eros-engine migrate"),
            "the guard names the supported entry: {err}"
        );
    }
}
