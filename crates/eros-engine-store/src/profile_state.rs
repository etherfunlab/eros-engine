// SPDX-License-Identifier: AGPL-3.0-only
//! Rebuild bookkeeping for the derived profile (`engine.human_profile_state`):
//! one row per user counting ledger facts that arrived since the last rebuild.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// Pending facts at which a rebuild is due without waiting for idleness.
pub const REBUILD_K: i32 = 20;
/// Consecutive failed rebuilds after which the pending count is dropped.
pub const MAX_REBUILD_FAILURES: i32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProfileState {
    pub user_id: Uuid,
    pub pending_facts: i32,
    pub last_fact_at: Option<DateTime<Utc>>,
    pub last_rebuilt_at: Option<DateTime<Utc>>,
    pub rebuild_failures: i32,
    pub claimed_at: Option<DateTime<Utc>>,
}

const COLUMNS: &str =
    "user_id, pending_facts, last_fact_at, last_rebuilt_at, rebuild_failures, claimed_at";

pub struct ProfileStateRepo<'a> {
    pub pool: &'a PgPool,
}

impl ProfileStateRepo<'_> {
    /// Add `n` newly ledgered facts to the user's counter, creating the row on
    /// first use, and return the new `pending_facts`. Restarts the idle window.
    pub async fn bump(&self, user_id: Uuid, n: i32) -> Result<i32, sqlx::Error> {
        sqlx::query_scalar(
            "INSERT INTO engine.human_profile_state (user_id, pending_facts, last_fact_at) \
             VALUES ($1, $2, now()) \
             ON CONFLICT (user_id) DO UPDATE SET \
                pending_facts = human_profile_state.pending_facts + EXCLUDED.pending_facts, \
                last_fact_at  = now() \
             RETURNING pending_facts",
        )
        .bind(user_id)
        .bind(n)
        .fetch_one(self.pool)
        .await
    }

    /// Take the rebuild for one user. `true` only for the caller that sets
    /// `claimed_at`: the user must have pending facts and no live claim, where a
    /// claim older than `stale` belongs to a worker presumed dead.
    pub async fn claim(&self, user_id: Uuid, stale: Duration) -> Result<bool, sqlx::Error> {
        let claimed: Option<Uuid> = sqlx::query_scalar(
            "UPDATE engine.human_profile_state SET claimed_at = now() \
             WHERE user_id = $1 AND pending_facts > 0 \
               AND (claimed_at IS NULL \
                    OR claimed_at < now() - make_interval(secs => $2::float8)) \
             RETURNING user_id",
        )
        .bind(user_id)
        .bind(stale.as_secs() as i64)
        .fetch_optional(self.pool)
        .await?;
        Ok(claimed.is_some())
    }

    /// Claim up to `limit` users whose pending facts have sat for `idle` with
    /// no new arrivals, longest-waiting first. Claims follow the same rules as
    /// `claim`; rows another sweeper has locked are skipped, not waited on.
    pub async fn claim_due(
        &self,
        idle: Duration,
        stale: Duration,
        limit: i64,
    ) -> Result<Vec<Uuid>, sqlx::Error> {
        sqlx::query_scalar(
            "UPDATE engine.human_profile_state SET claimed_at = now() \
             WHERE user_id IN ( \
                 SELECT user_id FROM engine.human_profile_state \
                 WHERE pending_facts > 0 \
                   AND last_fact_at < now() - make_interval(secs => $1::float8) \
                   AND (claimed_at IS NULL \
                        OR claimed_at < now() - make_interval(secs => $2::float8)) \
                 ORDER BY last_fact_at \
                 LIMIT $3 \
                 FOR UPDATE SKIP LOCKED) \
             RETURNING user_id",
        )
        .bind(idle.as_secs() as i64)
        .bind(stale.as_secs() as i64)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }

    /// Restart the staleness clock of a claim the caller already holds. `claim`
    /// and `claim_due` stamp every user of a batch at once, while the batch is
    /// rebuilt one user after another, so a later user's claim would age past
    /// the stale window before its turn and be taken over. `false` when the row
    /// is gone (the user was erased) or the claim was released: there is
    /// nothing left to rebuild.
    pub async fn refresh_claim(&self, user_id: Uuid) -> Result<bool, sqlx::Error> {
        let refreshed: Option<Uuid> = sqlx::query_scalar(
            "UPDATE engine.human_profile_state SET claimed_at = now() \
             WHERE user_id = $1 AND claimed_at IS NOT NULL \
             RETURNING user_id",
        )
        .bind(user_id)
        .fetch_optional(self.pool)
        .await?;
        Ok(refreshed.is_some())
    }

    /// A rebuild succeeded: the counter restarts and the claim is released.
    pub async fn finish_ok(&self, user_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.human_profile_state SET \
                pending_facts = 0, last_rebuilt_at = now(), \
                rebuild_failures = 0, claimed_at = NULL \
             WHERE user_id = $1",
        )
        .bind(user_id)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// A rebuild failed: release the claim and restart the idle window so the
    /// sweeper retries after `idle`, not at once. The failure that reaches
    /// `MAX_REBUILD_FAILURES` drops the pending count and the failure streak
    /// instead; the next fact starts a fresh count.
    pub async fn finish_failed(&self, user_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.human_profile_state SET \
                pending_facts = CASE WHEN rebuild_failures + 1 >= $2 \
                                     THEN 0 ELSE pending_facts END, \
                rebuild_failures = CASE WHEN rebuild_failures + 1 >= $2 \
                                        THEN 0 ELSE rebuild_failures + 1 END, \
                last_fact_at = now(), claimed_at = NULL \
             WHERE user_id = $1",
        )
        .bind(user_id)
        .bind(MAX_REBUILD_FAILURES)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn load(&self, user_id: Uuid) -> Result<Option<ProfileState>, sqlx::Error> {
        sqlx::query_as::<_, ProfileState>(&format!(
            "SELECT {COLUMNS} FROM engine.human_profile_state WHERE user_id = $1"
        ))
        .bind(user_id)
        .fetch_optional(self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEN_MIN: Duration = Duration::from_secs(600);
    const ONE_HOUR: Duration = Duration::from_secs(3600);

    async fn shift(pool: &PgPool, user_id: Uuid, column: &str, ago: &str) {
        sqlx::query(&format!(
            "UPDATE engine.human_profile_state SET {column} = now() - interval '{ago}' \
             WHERE user_id = $1"
        ))
        .bind(user_id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn bump_creates_then_accumulates(pool: PgPool) {
        let repo = ProfileStateRepo { pool: &pool };
        let user = Uuid::new_v4();
        assert!(repo.load(user).await.unwrap().is_none());

        assert_eq!(repo.bump(user, 3).await.unwrap(), 3);
        assert_eq!(repo.bump(user, 18).await.unwrap(), 21);

        let s = repo.load(user).await.unwrap().expect("row exists");
        assert_eq!(s.pending_facts, 21);
        assert!(s.last_fact_at.is_some());
        assert!(s.last_rebuilt_at.is_none());
        assert_eq!((s.rebuild_failures, s.claimed_at), (0, None));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_is_exclusive_until_finished_or_stale(pool: PgPool) {
        let repo = ProfileStateRepo { pool: &pool };
        let user = Uuid::new_v4();
        assert!(
            !repo.claim(user, TEN_MIN).await.unwrap(),
            "no row, nothing to claim"
        );

        repo.bump(user, 5).await.unwrap();
        assert!(repo.claim(user, TEN_MIN).await.unwrap());
        assert!(
            !repo.claim(user, TEN_MIN).await.unwrap(),
            "a live claim blocks the second worker"
        );

        repo.finish_ok(user).await.unwrap();
        let s = repo.load(user).await.unwrap().unwrap();
        assert_eq!((s.pending_facts, s.claimed_at), (0, None));
        assert!(s.last_rebuilt_at.is_some());
        assert!(
            !repo.claim(user, TEN_MIN).await.unwrap(),
            "nothing pending, nothing to claim"
        );

        repo.bump(user, 5).await.unwrap();
        assert!(repo.claim(user, TEN_MIN).await.unwrap());
        shift(&pool, user, "claimed_at", "9 minutes").await;
        assert!(
            !repo.claim(user, TEN_MIN).await.unwrap(),
            "a claim younger than the stale window still holds"
        );
        shift(&pool, user, "claimed_at", "11 minutes").await;
        assert!(
            repo.claim(user, TEN_MIN).await.unwrap(),
            "a stale claim is taken over"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn refresh_claim_restarts_a_held_claim_only(pool: PgPool) {
        let repo = ProfileStateRepo { pool: &pool };
        let user = Uuid::new_v4();
        assert!(
            !repo.refresh_claim(user).await.unwrap(),
            "no row, nothing to refresh"
        );

        repo.bump(user, 5).await.unwrap();
        assert!(
            !repo.refresh_claim(user).await.unwrap(),
            "an unclaimed row is not refreshed into a claim"
        );
        assert!(repo.load(user).await.unwrap().unwrap().claimed_at.is_none());

        assert!(repo.claim(user, TEN_MIN).await.unwrap());
        shift(&pool, user, "claimed_at", "9 minutes").await;
        let aged = repo.load(user).await.unwrap().unwrap().claimed_at.unwrap();
        assert!(repo.refresh_claim(user).await.unwrap());
        let fresh = repo.load(user).await.unwrap().unwrap().claimed_at.unwrap();
        assert!(fresh > aged, "the claim's clock restarts");
        shift(&pool, user, "claimed_at", "11 minutes").await;
        assert!(repo.refresh_claim(user).await.unwrap());
        assert!(
            !repo.claim(user, TEN_MIN).await.unwrap(),
            "a refreshed claim is not stale, so a second worker cannot take it"
        );

        repo.finish_ok(user).await.unwrap();
        assert!(
            !repo.refresh_claim(user).await.unwrap(),
            "a released claim is not refreshed"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_due_picks_idle_only(pool: PgPool) {
        let repo = ProfileStateRepo { pool: &pool };
        let (idle_user, busy_user) = (Uuid::new_v4(), Uuid::new_v4());
        repo.bump(idle_user, 4).await.unwrap();
        repo.bump(busy_user, 4).await.unwrap();
        shift(&pool, idle_user, "last_fact_at", "2 hours").await;
        shift(&pool, busy_user, "last_fact_at", "10 minutes").await;

        let picked = repo.claim_due(ONE_HOUR, TEN_MIN, 10).await.unwrap();
        assert_eq!(picked, vec![idle_user]);
        assert!(repo
            .load(idle_user)
            .await
            .unwrap()
            .unwrap()
            .claimed_at
            .is_some());
        assert!(repo
            .load(busy_user)
            .await
            .unwrap()
            .unwrap()
            .claimed_at
            .is_none());

        assert!(
            repo.claim_due(ONE_HOUR, TEN_MIN, 10)
                .await
                .unwrap()
                .is_empty(),
            "a claimed user is not handed out twice"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn finish_failed_backs_off_then_strikes_out(pool: PgPool) {
        let repo = ProfileStateRepo { pool: &pool };
        let user = Uuid::new_v4();
        repo.bump(user, REBUILD_K).await.unwrap();

        for failures in 1..MAX_REBUILD_FAILURES {
            shift(&pool, user, "last_fact_at", "2 hours").await;
            assert!(repo.claim(user, TEN_MIN).await.unwrap());
            repo.finish_failed(user).await.unwrap();

            let s = repo.load(user).await.unwrap().unwrap();
            assert_eq!(s.pending_facts, REBUILD_K, "the count survives a failure");
            assert_eq!(s.rebuild_failures, failures);
            assert!(s.claimed_at.is_none());
            let fresh: bool = sqlx::query_scalar(
                "SELECT last_fact_at > now() - interval '1 minute' \
                 FROM engine.human_profile_state WHERE user_id = $1",
            )
            .bind(user)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(fresh, "a failure restarts the idle window");
            assert!(
                repo.claim_due(ONE_HOUR, TEN_MIN, 10)
                    .await
                    .unwrap()
                    .is_empty(),
                "so the sweeper leaves the user alone for now"
            );
        }

        assert!(repo.claim(user, TEN_MIN).await.unwrap());
        repo.finish_failed(user).await.unwrap();
        let s = repo.load(user).await.unwrap().unwrap();
        assert_eq!((s.pending_facts, s.rebuild_failures), (0, 0));
        assert!(s.claimed_at.is_none());
    }
}
