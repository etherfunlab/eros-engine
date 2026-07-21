// SPDX-License-Identifier: AGPL-3.0-only
//! World Memories persistence: enrollment reads, director scheduling state,
//! and script-fragment storage/recall.
//!
//! Ownership split (spec §1): `world_enrollments` is downstream-written and
//! engine-read; `world_states` / `world_memories` are engine-private.
//! Scheduling uses the dreaming-lite claim pattern — a single
//! `UPDATE ... WHERE ... IN (SELECT ... FOR UPDATE SKIP LOCKED)` statement so
//! concurrent engine instances claim disjoint owners.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

pub struct WorldRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> WorldRepo<'a> {
    /// Backfill a `world_states` row for every enrollment that lacks one.
    /// Empty seed/digests (`{}`) marks a never-run world; the director prompt
    /// takes an "initialize world" branch for it. Returns rows inserted.
    pub async fn ensure_states_for_enrollments(&self) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(
            "INSERT INTO engine.world_states (owner_uid, seed, digests) \
             SELECT owner_uid, '{}'::jsonb, '{}'::jsonb FROM engine.world_enrollments \
             ON CONFLICT (owner_uid) DO NOTHING",
        )
        .execute(self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Atomically claim up to `batch` due owners (enrolled AND past their
    /// interval AND not freshly claimed). Same statement shape as the
    /// dreaming picker: concurrent sweepers see disjoint sets.
    pub async fn claim_due(
        &self,
        interval: Duration,
        stale: Duration,
        batch: i64,
    ) -> Result<Vec<Uuid>, sqlx::Error> {
        let now = Utc::now();
        let due_cutoff: DateTime<Utc> =
            now - chrono::Duration::from_std(interval).unwrap_or_default();
        let stale_cutoff: DateTime<Utc> =
            now - chrono::Duration::from_std(stale).unwrap_or_default();
        sqlx::query_scalar(
            "UPDATE engine.world_states SET claimed_at = now() \
             WHERE owner_uid IN ( \
                 SELECT ws.owner_uid FROM engine.world_states ws \
                 JOIN engine.world_enrollments we USING (owner_uid) \
                 WHERE (ws.last_run_at IS NULL OR ws.last_run_at < $1) \
                   AND (ws.claimed_at IS NULL OR ws.claimed_at < $2) \
                 ORDER BY ws.last_run_at ASC NULLS FIRST \
                 LIMIT $3 \
                 FOR UPDATE SKIP LOCKED \
             ) \
             RETURNING owner_uid",
        )
        .bind(due_cutoff)
        .bind(stale_cutoff)
        .bind(batch)
        .fetch_all(self.pool)
        .await
    }

    /// Reset the claim after a failed round so the owner retries at the next
    /// due scan instead of waiting out the stale window.
    pub async fn release_claim(&self, owner_uid: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE engine.world_states SET claimed_at = NULL WHERE owner_uid = $1")
            .bind(owner_uid)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Stamp a round that produced nothing to persist (e.g. empty roster):
    /// advances last_run_at and clears the claim without touching the seed.
    pub async fn mark_ran(&self, owner_uid: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.world_states \
             SET last_run_at = now(), claimed_at = NULL, updated_at = now() \
             WHERE owner_uid = $1",
        )
        .bind(owner_uid)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Current seed for the director prompt. `None` when no state row exists.
    pub async fn load_seed(
        &self,
        owner_uid: Uuid,
    ) -> Result<Option<serde_json::Value>, sqlx::Error> {
        sqlx::query_scalar("SELECT seed FROM engine.world_states WHERE owner_uid = $1")
            .bind(owner_uid)
            .fetch_optional(self.pool)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn enroll(pool: &PgPool, owner: Uuid) {
        sqlx::query("INSERT INTO engine.world_enrollments (owner_uid) VALUES ($1)")
            .bind(owner)
            .execute(pool)
            .await
            .unwrap();
    }

    const DAY: Duration = Duration::from_secs(24 * 3600);
    const STALE: Duration = Duration::from_secs(1800);

    #[sqlx::test(migrations = "./migrations")]
    async fn ensure_states_backfills_only_missing(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        enroll(&pool, a).await;
        enroll(&pool, b).await;

        assert_eq!(repo.ensure_states_for_enrollments().await.unwrap(), 2);
        // Idempotent: second run inserts nothing.
        assert_eq!(repo.ensure_states_for_enrollments().await.unwrap(), 0);

        let seed = repo.load_seed(a).await.unwrap().unwrap();
        assert_eq!(seed, serde_json::json!({}), "fresh world has empty seed");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_due_claims_never_run_enrolled_owner_once(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();

        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        assert_eq!(claimed, vec![owner], "never-run world is due");

        // Immediately re-claiming yields nothing (claimed_at is fresh).
        let again = repo.claim_due(DAY, STALE, 5).await.unwrap();
        assert!(again.is_empty(), "fresh claim must block re-claim");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_due_skips_unenrolled_and_recently_run(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        // State row WITHOUT enrollment (unenrolled leftover) must never claim.
        let orphan = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.world_states (owner_uid, seed, digests) \
             VALUES ($1, '{}'::jsonb, '{}'::jsonb)",
        )
        .bind(orphan)
        .execute(&pool)
        .await
        .unwrap();
        // Enrolled but ran 1h ago with a 24h interval → not due.
        let recent = Uuid::new_v4();
        enroll(&pool, recent).await;
        sqlx::query(
            "INSERT INTO engine.world_states (owner_uid, seed, digests, last_run_at) \
             VALUES ($1, '{}'::jsonb, '{}'::jsonb, now() - interval '1 hour')",
        )
        .bind(recent)
        .execute(&pool)
        .await
        .unwrap();

        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        assert!(claimed.is_empty(), "orphan + not-due must both be skipped");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_due_reclaims_stale_claims(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        // Claimed 2h ago (stale window 30 min) and never finished.
        sqlx::query(
            "INSERT INTO engine.world_states (owner_uid, seed, digests, claimed_at) \
             VALUES ($1, '{}'::jsonb, '{}'::jsonb, now() - interval '2 hours')",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();

        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        assert_eq!(claimed, vec![owner], "stale claim must be recovered");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn release_claim_makes_owner_due_again(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();

        assert_eq!(repo.claim_due(DAY, STALE, 5).await.unwrap(), vec![owner]);
        repo.release_claim(owner).await.unwrap();
        assert_eq!(
            repo.claim_due(DAY, STALE, 5).await.unwrap(),
            vec![owner],
            "released claim must be immediately re-claimable"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn mark_ran_advances_last_run_and_clears_claim(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();
        repo.claim_due(DAY, STALE, 5).await.unwrap();

        repo.mark_ran(owner).await.unwrap();
        let (last_run, claimed): (Option<DateTime<Utc>>, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT last_run_at, claimed_at FROM engine.world_states WHERE owner_uid = $1",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(last_run.is_some());
        assert!(claimed.is_none());
        // And it's no longer due under a 24h interval.
        assert!(repo.claim_due(DAY, STALE, 5).await.unwrap().is_empty());
    }
}
