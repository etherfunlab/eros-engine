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

/// A claim's ownership token: the `claimed_at` the claim wrote. Every write a
/// rebuild makes after its model call is guarded on it, so a worker whose
/// claim was released (a deleted fact) or taken over (a stale claim) writes and
/// settles nothing.
pub type ClaimToken = DateTime<Utc>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProfileState {
    pub user_id: Uuid,
    pub pending_facts: i32,
    pub last_fact_at: Option<DateTime<Utc>>,
    pub last_rebuilt_at: Option<DateTime<Utc>>,
    pub rebuild_failures: i32,
    pub claimed_at: Option<DateTime<Utc>>,
    /// A fact was deleted since the last rebuild: the sweeper takes the user
    /// on its next tick, without waiting for the idle window.
    pub rebuild_due: bool,
}

const COLUMNS: &str = "user_id, pending_facts, last_fact_at, last_rebuilt_at, rebuild_failures, \
                       claimed_at, rebuild_due";

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

    /// Take the rebuild for one user. `Some(token)` only for the caller that
    /// sets `claimed_at`: the user must have pending facts and no live claim,
    /// where a claim older than `stale` belongs to a worker presumed dead.
    pub async fn claim(
        &self,
        user_id: Uuid,
        stale: Duration,
    ) -> Result<Option<ClaimToken>, sqlx::Error> {
        sqlx::query_scalar(
            "UPDATE engine.human_profile_state SET claimed_at = clock_timestamp() \
             WHERE user_id = $1 AND pending_facts > 0 \
               AND (claimed_at IS NULL \
                    OR claimed_at < now() - make_interval(secs => $2::float8)) \
             RETURNING claimed_at",
        )
        .bind(user_id)
        .bind(stale.as_secs() as i64)
        .fetch_optional(self.pool)
        .await
    }

    /// Claim up to `limit` users that are due: marked `rebuild_due` by a
    /// deleted fact (until a rebuild of it fails; it then backs off like any
    /// failure), or with pending facts that have sat for `idle` with no new
    /// arrivals. Marked users first, then the longest-waiting. Claims
    /// follow the same rules as `claim`; rows another sweeper has locked are
    /// skipped, not waited on. Each user comes with its claim's token.
    pub async fn claim_due(
        &self,
        idle: Duration,
        stale: Duration,
        limit: i64,
    ) -> Result<Vec<(Uuid, ClaimToken)>, sqlx::Error> {
        sqlx::query_as(
            "UPDATE engine.human_profile_state SET claimed_at = clock_timestamp() \
             WHERE user_id IN ( \
                 SELECT user_id FROM engine.human_profile_state \
                 WHERE pending_facts > 0 \
                   AND ((rebuild_due AND rebuild_failures = 0) \
                        OR last_fact_at < now() - make_interval(secs => $1::float8)) \
                   AND (claimed_at IS NULL \
                        OR claimed_at < now() - make_interval(secs => $2::float8)) \
                 ORDER BY rebuild_due DESC, last_fact_at \
                 LIMIT $3 \
                 FOR UPDATE SKIP LOCKED) \
             RETURNING user_id, claimed_at",
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
    /// the stale window before its turn and be taken over. Returns the new
    /// token; `None` when the row is gone (the user was erased) or `token` is
    /// no longer the claim (released or taken over): there is nothing left
    /// for this worker to rebuild.
    pub async fn refresh_claim(
        &self,
        user_id: Uuid,
        token: ClaimToken,
    ) -> Result<Option<ClaimToken>, sqlx::Error> {
        sqlx::query_scalar(
            "UPDATE engine.human_profile_state SET claimed_at = clock_timestamp() \
             WHERE user_id = $1 AND claimed_at = $2 \
             RETURNING claimed_at",
        )
        .bind(user_id)
        .bind(token)
        .fetch_optional(self.pool)
        .await
    }

    /// A rebuild succeeded: the counter restarts, the due mark clears and the
    /// claim is released. A no-op unless `token` is still the claim.
    pub async fn finish_ok(&self, user_id: Uuid, token: ClaimToken) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.human_profile_state SET \
                pending_facts = 0, last_rebuilt_at = now(), \
                rebuild_failures = 0, rebuild_due = false, claimed_at = NULL \
             WHERE user_id = $1 AND claimed_at = $2",
        )
        .bind(user_id)
        .bind(token)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// A rebuild failed: release the claim and restart the idle window so the
    /// sweeper retries after `idle`, not at once. The failure that reaches
    /// `MAX_REBUILD_FAILURES` drops the pending count and the failure streak
    /// and the due mark instead; the next fact starts a fresh count. A no-op
    /// unless `token` is still the claim.
    pub async fn finish_failed(&self, user_id: Uuid, token: ClaimToken) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.human_profile_state SET \
                pending_facts = CASE WHEN rebuild_failures + 1 >= $2 \
                                     THEN 0 ELSE pending_facts END, \
                rebuild_due = rebuild_due AND rebuild_failures + 1 < $2, \
                rebuild_failures = CASE WHEN rebuild_failures + 1 >= $2 \
                                        THEN 0 ELSE rebuild_failures + 1 END, \
                last_fact_at = now(), claimed_at = NULL \
             WHERE user_id = $1 AND claimed_at = $3",
        )
        .bind(user_id)
        .bind(MAX_REBUILD_FAILURES)
        .bind(token)
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
            repo.claim(user, TEN_MIN).await.unwrap().is_none(),
            "no row, nothing to claim"
        );

        repo.bump(user, 5).await.unwrap();
        let token = repo.claim(user, TEN_MIN).await.unwrap().expect("claimed");
        assert!(
            repo.claim(user, TEN_MIN).await.unwrap().is_none(),
            "a live claim blocks the second worker"
        );

        repo.finish_ok(user, token).await.unwrap();
        let s = repo.load(user).await.unwrap().unwrap();
        assert_eq!((s.pending_facts, s.claimed_at), (0, None));
        assert!(s.last_rebuilt_at.is_some());
        assert!(
            repo.claim(user, TEN_MIN).await.unwrap().is_none(),
            "nothing pending, nothing to claim"
        );

        repo.bump(user, 5).await.unwrap();
        assert!(repo.claim(user, TEN_MIN).await.unwrap().is_some());
        shift(&pool, user, "claimed_at", "9 minutes").await;
        assert!(
            repo.claim(user, TEN_MIN).await.unwrap().is_none(),
            "a claim younger than the stale window still holds"
        );
        shift(&pool, user, "claimed_at", "11 minutes").await;
        assert!(
            repo.claim(user, TEN_MIN).await.unwrap().is_some(),
            "a stale claim is taken over"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn refresh_claim_restarts_a_held_claim_only(pool: PgPool) {
        let repo = ProfileStateRepo { pool: &pool };
        let user = Uuid::new_v4();
        let any = Utc::now();
        assert!(
            repo.refresh_claim(user, any).await.unwrap().is_none(),
            "no row, nothing to refresh"
        );

        repo.bump(user, 5).await.unwrap();
        assert!(
            repo.refresh_claim(user, any).await.unwrap().is_none(),
            "an unclaimed row is not refreshed into a claim"
        );
        assert!(repo.load(user).await.unwrap().unwrap().claimed_at.is_none());

        assert!(repo.claim(user, TEN_MIN).await.unwrap().is_some());
        shift(&pool, user, "claimed_at", "9 minutes").await;
        let aged = repo.load(user).await.unwrap().unwrap().claimed_at.unwrap();
        let fresh = repo.refresh_claim(user, aged).await.unwrap().expect("held");
        assert!(fresh > aged, "the claim's clock restarts");
        assert_eq!(
            repo.load(user).await.unwrap().unwrap().claimed_at,
            Some(fresh)
        );
        shift(&pool, user, "claimed_at", "11 minutes").await;
        let aged = repo.load(user).await.unwrap().unwrap().claimed_at.unwrap();
        let fresh = repo.refresh_claim(user, aged).await.unwrap().expect("held");
        assert!(
            repo.claim(user, TEN_MIN).await.unwrap().is_none(),
            "a refreshed claim is not stale, so a second worker cannot take it"
        );

        repo.finish_ok(user, fresh).await.unwrap();
        assert!(
            repo.refresh_claim(user, fresh).await.unwrap().is_none(),
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

        let picked: Vec<Uuid> = repo
            .claim_due(ONE_HOUR, TEN_MIN, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|(u, _)| u)
            .collect();
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
            let token = repo.claim(user, TEN_MIN).await.unwrap().expect("claimed");
            repo.finish_failed(user, token).await.unwrap();

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

        let token = repo.claim(user, TEN_MIN).await.unwrap().expect("claimed");
        repo.finish_failed(user, token).await.unwrap();
        let s = repo.load(user).await.unwrap().unwrap();
        assert_eq!((s.pending_facts, s.rebuild_failures), (0, 0));
        assert!(s.claimed_at.is_none());
    }

    /// A claim is an ownership token (spec 2026-11-10-row-deletion §5.2): a
    /// worker whose claim was released and taken again by another settles
    /// nothing.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_stale_token_settles_and_refreshes_nothing(pool: PgPool) {
        let repo = ProfileStateRepo { pool: &pool };
        let user = Uuid::new_v4();
        repo.bump(user, 3).await.unwrap();
        let first = repo.claim(user, TEN_MIN).await.unwrap().expect("claimed");
        sqlx::query("UPDATE engine.human_profile_state SET claimed_at = NULL WHERE user_id = $1")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        let second = repo
            .claim(user, TEN_MIN)
            .await
            .unwrap()
            .expect("re-claimed");
        assert_ne!(first, second);

        repo.finish_ok(user, first).await.unwrap();
        repo.finish_failed(user, first).await.unwrap();
        assert_eq!(repo.refresh_claim(user, first).await.unwrap(), None);
        let s = repo.load(user).await.unwrap().unwrap();
        assert_eq!(s.pending_facts, 3, "the stale worker zeroed nothing");
        assert_eq!(s.rebuild_failures, 0);
        assert_eq!(s.claimed_at, Some(second), "the newer claim stands");

        let refreshed = repo
            .refresh_claim(user, second)
            .await
            .unwrap()
            .expect("held");
        assert!(refreshed >= second);
        repo.finish_ok(user, second).await.unwrap();
        assert_eq!(
            repo.load(user).await.unwrap().unwrap().claimed_at,
            Some(refreshed),
            "the token changed with the refresh"
        );
        repo.finish_ok(user, refreshed).await.unwrap();
        let s = repo.load(user).await.unwrap().unwrap();
        assert_eq!((s.pending_facts, s.claimed_at), (0, None));
    }

    /// A due user is taken on the next sweep, before the idle window and ahead
    /// of merely idle users; a settled rebuild clears the mark.
    #[sqlx::test(migrations = "./migrations")]
    async fn claim_due_takes_due_users_first(pool: PgPool) {
        let repo = ProfileStateRepo { pool: &pool };
        let (idle_user, due_user) = (Uuid::new_v4(), Uuid::new_v4());
        repo.bump(idle_user, 1).await.unwrap();
        repo.bump(due_user, 1).await.unwrap();
        shift(&pool, idle_user, "last_fact_at", "3 hours").await;
        sqlx::query("UPDATE engine.human_profile_state SET rebuild_due = true WHERE user_id = $1")
            .bind(due_user)
            .execute(&pool)
            .await
            .unwrap();

        let picked = repo.claim_due(ONE_HOUR, TEN_MIN, 1).await.unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].0, due_user, "due first, though it is not idle");
        assert_eq!(
            repo.load(due_user).await.unwrap().unwrap().claimed_at,
            Some(picked[0].1),
            "the token is the claimed_at written"
        );
        repo.finish_ok(due_user, picked[0].1).await.unwrap();
        assert!(!repo.load(due_user).await.unwrap().unwrap().rebuild_due);

        // Struck out: the mark goes with the count.
        sqlx::query(
            "UPDATE engine.human_profile_state SET rebuild_due = true, pending_facts = 1, \
                    rebuild_failures = $2 - 1 WHERE user_id = $1",
        )
        .bind(due_user)
        .bind(MAX_REBUILD_FAILURES)
        .execute(&pool)
        .await
        .unwrap();
        let token = repo.claim(due_user, TEN_MIN).await.unwrap().unwrap();
        repo.finish_failed(due_user, token).await.unwrap();
        let s = repo.load(due_user).await.unwrap().unwrap();
        assert_eq!((s.pending_facts, s.rebuild_due), (0, false));
    }

    /// A due rebuild that failed waits out the idle window like any other
    /// failure, instead of being retried on every tick.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_failed_due_rebuild_waits_out_the_idle_window(pool: PgPool) {
        let repo = ProfileStateRepo { pool: &pool };
        let user = Uuid::new_v4();
        repo.bump(user, 1).await.unwrap();
        sqlx::query("UPDATE engine.human_profile_state SET rebuild_due = true WHERE user_id = $1")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        let token = repo.claim(user, TEN_MIN).await.unwrap().expect("claimed");
        repo.finish_failed(user, token).await.unwrap();
        assert!(
            repo.load(user).await.unwrap().unwrap().rebuild_due,
            "still due"
        );
        assert!(
            repo.claim_due(ONE_HOUR, TEN_MIN, 10)
                .await
                .unwrap()
                .is_empty(),
            "not on the next tick"
        );
        shift(&pool, user, "last_fact_at", "2 hours").await;
        assert_eq!(
            repo.claim_due(ONE_HOUR, TEN_MIN, 10).await.unwrap().len(),
            1
        );
    }
}
