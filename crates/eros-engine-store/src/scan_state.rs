// SPDX-License-Identifier: AGPL-3.0-only
//! Scan bookkeeping for the relationship scanner (`engine.relationship_scan_state`):
//! one row per persona instance holding the message cursor, the count of user
//! turns since it, the scan claim, and the episode/arc counters.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// Pending user turns at which a scan is due without waiting for idleness.
pub const SCAN_K: i32 = 20;
/// Pending user turns an idle relationship needs before the sweeper scans it.
pub const FLUSH_MIN_TURNS: i32 = 3;
/// Consecutive failed scans after which the failing span is skipped.
pub const MAX_SCAN_FAILURES: i32 = 3;
/// Consecutive failed arc rewrites after which the episode count is dropped.
pub const MAX_ARC_FAILURES: i32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ScanState {
    pub instance_id: Uuid,
    /// Exclusive cursor on `chat_messages.sent_at`.
    pub scanned_through_at: DateTime<Utc>,
    /// User rows after the cursor.
    pub pending_turns: i32,
    pub last_turn_at: Option<DateTime<Utc>>,
    pub last_scanned_at: Option<DateTime<Utc>>,
    pub scan_failures: i32,
    pub claimed_at: Option<DateTime<Utc>>,
    /// Last valid IANA timezone seen on a turn.
    pub user_timezone: Option<String>,
    pub episodes_since_arc: i32,
    pub arc_failures: i32,
    /// `None` until the backfill starts.
    pub backfilled_through_at: Option<DateTime<Utc>>,
}

const COLUMNS: &str = "instance_id, scanned_through_at, pending_turns, last_turn_at, \
                       last_scanned_at, scan_failures, claimed_at, user_timezone, \
                       episodes_since_arc, arc_failures, backfilled_through_at";

/// What `bump` reports: the new pending count, and whether this turn created
/// the relationship's row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bumped {
    pub pending_turns: i32,
    pub created: bool,
}

pub struct ScanStateRepo<'a> {
    pub pool: &'a PgPool,
}

impl ScanStateRepo<'_> {
    /// Count one user turn for the relationship, creating its row on first use.
    /// A new row's cursor sits at the newest message of this instance older
    /// than `user_row_sent_at`, or one microsecond before that row when there
    /// is none; earlier history is the backfill's job. Restarts the idle
    /// window and, when given, replaces the stored timezone.
    pub async fn bump(
        &self,
        instance_id: Uuid,
        user_row_sent_at: DateTime<Utc>,
        user_timezone: Option<&str>,
    ) -> Result<Bumped, sqlx::Error> {
        let (pending_turns, created): (i32, bool) = sqlx::query_as(
            "INSERT INTO engine.relationship_scan_state \
                 (instance_id, scanned_through_at, pending_turns, last_turn_at, user_timezone) \
             VALUES ($1, \
                 COALESCE((SELECT max(m.sent_at) FROM engine.chat_messages m \
                           JOIN engine.chat_sessions s ON s.id = m.session_id \
                           WHERE s.instance_id = $1 AND m.sent_at < $2::timestamptz), \
                          $2::timestamptz - interval '1 microsecond'), \
                 1, now(), $3::text) \
             ON CONFLICT (instance_id) DO UPDATE SET \
                pending_turns = relationship_scan_state.pending_turns + 1, \
                last_turn_at  = now(), \
                user_timezone = COALESCE(EXCLUDED.user_timezone, \
                                         relationship_scan_state.user_timezone) \
             RETURNING pending_turns, (xmax = 0) AS created",
        )
        .bind(instance_id)
        .bind(user_row_sent_at)
        .bind(user_timezone)
        .fetch_one(self.pool)
        .await?;
        Ok(Bumped {
            pending_turns,
            created,
        })
    }

    /// Take the scan for one relationship. `true` only for the caller that sets
    /// `claimed_at`: the relationship must have pending turns and no live
    /// claim, where a claim older than `stale` belongs to a worker presumed
    /// dead. This is the `SCAN_K` path; the idle sweeper uses `claim_due`.
    pub async fn claim(&self, instance_id: Uuid, stale: Duration) -> Result<bool, sqlx::Error> {
        let claimed: Option<Uuid> = sqlx::query_scalar(
            "UPDATE engine.relationship_scan_state SET claimed_at = now() \
             WHERE instance_id = $1 AND pending_turns > 0 \
               AND (claimed_at IS NULL \
                    OR claimed_at < now() - make_interval(secs => $2::float8)) \
             RETURNING instance_id",
        )
        .bind(instance_id)
        .bind(stale.as_secs() as i64)
        .fetch_optional(self.pool)
        .await?;
        Ok(claimed.is_some())
    }

    /// Claim up to `limit` relationships that have sat idle for `idle` with at
    /// least `FLUSH_MIN_TURNS` pending turns, longest-waiting first. Claims
    /// follow the same rules as `claim`; rows another sweeper has locked are
    /// skipped, not waited on.
    pub async fn claim_due(
        &self,
        idle: Duration,
        stale: Duration,
        limit: i64,
    ) -> Result<Vec<Uuid>, sqlx::Error> {
        sqlx::query_scalar(
            "UPDATE engine.relationship_scan_state SET claimed_at = now() \
             WHERE instance_id IN ( \
                 SELECT instance_id FROM engine.relationship_scan_state \
                 WHERE pending_turns >= $4 \
                   AND last_turn_at < now() - make_interval(secs => $1::float8) \
                   AND (claimed_at IS NULL \
                        OR claimed_at < now() - make_interval(secs => $2::float8)) \
                 ORDER BY last_turn_at \
                 LIMIT $3 \
                 FOR UPDATE SKIP LOCKED) \
             RETURNING instance_id",
        )
        .bind(idle.as_secs() as i64)
        .bind(stale.as_secs() as i64)
        .bind(limit)
        .bind(FLUSH_MIN_TURNS)
        .fetch_all(self.pool)
        .await
    }

    /// Restart the staleness clock of a claim the caller already holds. `claim`
    /// and `claim_due` stamp every relationship of a batch at once, while the
    /// batch is scanned one after another, so a later claim would age past the
    /// stale window before its turn and be taken over. `false` when the row is
    /// gone (the relationship was erased) or the claim was released: there is
    /// nothing left to scan.
    pub async fn refresh_claim(&self, instance_id: Uuid) -> Result<bool, sqlx::Error> {
        let refreshed: Option<Uuid> = sqlx::query_scalar(
            "UPDATE engine.relationship_scan_state SET claimed_at = now() \
             WHERE instance_id = $1 AND claimed_at IS NOT NULL \
             RETURNING instance_id",
        )
        .bind(instance_id)
        .fetch_optional(self.pool)
        .await?;
        Ok(refreshed.is_some())
    }

    /// A scan succeeded: the cursor moves to `cursor_to`, the `covered_user_rows`
    /// it read leave the pending count, and the claim is released. `false` when
    /// the row is gone or the claim was already released, in which case nothing
    /// changed.
    pub async fn finish_scan_ok(
        &self,
        instance_id: Uuid,
        cursor_to: DateTime<Utc>,
        covered_user_rows: i32,
    ) -> Result<bool, sqlx::Error> {
        let finished: Option<Uuid> = sqlx::query_scalar(
            "UPDATE engine.relationship_scan_state SET \
                scanned_through_at = $2, \
                pending_turns = GREATEST(pending_turns - $3, 0), \
                last_scanned_at = now(), \
                scan_failures = 0, claimed_at = NULL \
             WHERE instance_id = $1 AND claimed_at IS NOT NULL \
             RETURNING instance_id",
        )
        .bind(instance_id)
        .bind(cursor_to)
        .bind(covered_user_rows)
        .fetch_optional(self.pool)
        .await?;
        Ok(finished.is_some())
    }

    /// A scan failed: release the claim and restart the idle window so the
    /// sweeper retries after `idle`, not at once; cursor and pending count stay.
    /// The failure that reaches `MAX_SCAN_FAILURES` gives up on the span
    /// instead and applies what `finish_scan_ok` would, with the failure streak
    /// reset; the caller logs it.
    pub async fn finish_scan_failed(
        &self,
        instance_id: Uuid,
        cursor_to: DateTime<Utc>,
        covered_user_rows: i32,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.relationship_scan_state SET \
                scanned_through_at = CASE WHEN scan_failures + 1 >= $4 \
                                          THEN $2 ELSE scanned_through_at END, \
                pending_turns = CASE WHEN scan_failures + 1 >= $4 \
                                     THEN GREATEST(pending_turns - $3, 0) \
                                     ELSE pending_turns END, \
                last_scanned_at = CASE WHEN scan_failures + 1 >= $4 \
                                       THEN now() ELSE last_scanned_at END, \
                last_turn_at = CASE WHEN scan_failures + 1 >= $4 \
                                    THEN last_turn_at ELSE now() END, \
                scan_failures = CASE WHEN scan_failures + 1 >= $4 \
                                     THEN 0 ELSE scan_failures + 1 END, \
                claimed_at = NULL \
             WHERE instance_id = $1",
        )
        .bind(instance_id)
        .bind(cursor_to)
        .bind(covered_user_rows)
        .bind(MAX_SCAN_FAILURES)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// One more episode awaits folding into the arc; returns the new
    /// `episodes_since_arc`. `0` when the row is gone (the relationship was
    /// erased): there is nothing to fold.
    pub async fn bump_episodes(&self, instance_id: Uuid) -> Result<i32, sqlx::Error> {
        let n: Option<i32> = sqlx::query_scalar(
            "UPDATE engine.relationship_scan_state SET \
                episodes_since_arc = episodes_since_arc + 1 \
             WHERE instance_id = $1 \
             RETURNING episodes_since_arc",
        )
        .bind(instance_id)
        .fetch_optional(self.pool)
        .await?;
        Ok(n.unwrap_or(0))
    }

    /// The arc absorbed the pending episodes: both arc counters restart.
    pub async fn fold_episodes(&self, instance_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.relationship_scan_state SET \
                episodes_since_arc = 0, arc_failures = 0 \
             WHERE instance_id = $1",
        )
        .bind(instance_id)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// An arc rewrite failed; returns the stored `arc_failures`. The failure
    /// that reaches `MAX_ARC_FAILURES` drops the episode count and the failure
    /// streak instead, so it returns `0` (and so does a gone row); the next
    /// episode starts a fresh count.
    pub async fn arc_failed(&self, instance_id: Uuid) -> Result<i32, sqlx::Error> {
        let n: Option<i32> = sqlx::query_scalar(
            "UPDATE engine.relationship_scan_state SET \
                episodes_since_arc = CASE WHEN arc_failures + 1 >= $2 \
                                          THEN 0 ELSE episodes_since_arc END, \
                arc_failures = CASE WHEN arc_failures + 1 >= $2 \
                                    THEN 0 ELSE arc_failures + 1 END \
             WHERE instance_id = $1 \
             RETURNING arc_failures",
        )
        .bind(instance_id)
        .bind(MAX_ARC_FAILURES)
        .fetch_optional(self.pool)
        .await?;
        Ok(n.unwrap_or(0))
    }

    pub async fn load(&self, instance_id: Uuid) -> Result<Option<ScanState>, sqlx::Error> {
        sqlx::query_as::<_, ScanState>(&format!(
            "SELECT {COLUMNS} FROM engine.relationship_scan_state WHERE instance_id = $1"
        ))
        .bind(instance_id)
        .fetch_optional(self.pool)
        .await
    }

    /// Record how far the backfill has read.
    pub async fn set_backfilled_through(
        &self,
        instance_id: Uuid,
        at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.relationship_scan_state SET backfilled_through_at = $2 \
             WHERE instance_id = $1",
        )
        .bind(instance_id)
        .bind(at)
        .execute(self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{seed_persona_instance, seed_session};
    use chrono::Duration as Span;

    const TEN_MIN: Duration = Duration::from_secs(600);
    const ONE_HOUR: Duration = Duration::from_secs(3600);

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    async fn shift(pool: &PgPool, instance_id: Uuid, column: &str, ago: &str) {
        sqlx::query(&format!(
            "UPDATE engine.relationship_scan_state SET {column} = now() - interval '{ago}' \
             WHERE instance_id = $1"
        ))
        .bind(instance_id)
        .execute(pool)
        .await
        .unwrap();
    }

    /// One persona instance for a fresh owner, with one live text session.
    async fn seed_relationship(pool: &PgPool) -> (Uuid, Uuid) {
        let owner = Uuid::new_v4();
        let instance = seed_persona_instance(pool, owner).await;
        let session = seed_session(pool, owner, instance, false).await;
        (instance, session)
    }

    async fn seed_row(pool: &PgPool, session_id: Uuid, role: &str, sent_at: DateTime<Utc>) {
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
             VALUES ($1, $2, 'seed', $3)",
        )
        .bind(session_id)
        .bind(role)
        .bind(sent_at)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Bump `n` times at `at(1_000)`, the cursor wherever the first bump put it.
    async fn bump_n(repo: &ScanStateRepo<'_>, instance_id: Uuid, n: i32) {
        for _ in 0..n {
            repo.bump(instance_id, at(1_000), None).await.unwrap();
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn bump_creates_with_cursor_before_triggering_row(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        let (instance, session) = seed_relationship(&pool).await;
        let (t0, t1) = (at(1_000), at(2_000));
        seed_row(&pool, session, "assistant", t0).await;
        seed_row(&pool, session, "user", t1).await;
        assert!(repo.load(instance).await.unwrap().is_none());

        let first = repo.bump(instance, t1, None).await.unwrap();
        assert_eq!(
            first,
            Bumped {
                pending_turns: 1,
                created: true
            }
        );
        let s = repo.load(instance).await.unwrap().expect("row exists");
        assert_eq!(s.scanned_through_at, t0);
        assert_eq!(s.pending_turns, 1);
        assert!(s.last_turn_at.is_some());
        assert_eq!((s.scan_failures, s.claimed_at), (0, None));
        assert_eq!(s.last_scanned_at, None);
        assert_eq!(s.backfilled_through_at, None);

        let second = repo.bump(instance, at(3_000), None).await.unwrap();
        assert_eq!(
            second,
            Bumped {
                pending_turns: 2,
                created: false
            }
        );
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(
            s.scanned_through_at, t0,
            "only the first bump places the cursor"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn bump_without_older_rows_places_cursor_one_microsecond_back(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        let (instance, session) = seed_relationship(&pool).await;
        let (_, other_session) = seed_relationship(&pool).await;
        let t1 = at(2_000);
        // Another relationship's older row and this one's newer row are not
        // "older rows of this instance".
        seed_row(&pool, other_session, "user", at(1_000)).await;
        seed_row(&pool, session, "user", at(3_000)).await;

        repo.bump(instance, t1, None).await.unwrap();
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(s.scanned_through_at, t1 - Span::microseconds(1));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn bump_keeps_last_timezone_when_none_given(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        let (instance, _) = seed_relationship(&pool).await;

        repo.bump(instance, at(1_000), Some("Asia/Tokyo"))
            .await
            .unwrap();
        repo.bump(instance, at(2_000), None).await.unwrap();
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(s.user_timezone.as_deref(), Some("Asia/Tokyo"));

        repo.bump(instance, at(3_000), Some("Europe/Paris"))
            .await
            .unwrap();
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(s.user_timezone.as_deref(), Some("Europe/Paris"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_is_exclusive_until_finished_or_stale(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        let (instance, _) = seed_relationship(&pool).await;
        assert!(
            !repo.claim(instance, TEN_MIN).await.unwrap(),
            "no row, nothing to claim"
        );

        repo.bump(instance, at(1_000), None).await.unwrap();
        assert!(
            repo.claim(instance, TEN_MIN).await.unwrap(),
            "the K path claims on one pending turn"
        );
        assert!(
            !repo.claim(instance, TEN_MIN).await.unwrap(),
            "a live claim blocks the second worker"
        );

        shift(&pool, instance, "claimed_at", "9 minutes").await;
        assert!(!repo.claim(instance, TEN_MIN).await.unwrap());
        shift(&pool, instance, "claimed_at", "11 minutes").await;
        assert!(
            repo.claim(instance, TEN_MIN).await.unwrap(),
            "a stale claim is taken over"
        );

        assert!(repo.finish_scan_ok(instance, at(1_000), 1).await.unwrap());
        assert!(
            !repo.claim(instance, TEN_MIN).await.unwrap(),
            "nothing pending, nothing to claim"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn refresh_claim_restarts_a_held_claim_only(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        let (instance, _) = seed_relationship(&pool).await;
        assert!(!repo.refresh_claim(instance).await.unwrap(), "no row");

        repo.bump(instance, at(1_000), None).await.unwrap();
        assert!(
            !repo.refresh_claim(instance).await.unwrap(),
            "an unclaimed row is not refreshed into a claim"
        );
        assert!(repo.claim(instance, TEN_MIN).await.unwrap());
        shift(&pool, instance, "claimed_at", "11 minutes").await;
        assert!(repo.refresh_claim(instance).await.unwrap());
        assert!(
            !repo.claim(instance, TEN_MIN).await.unwrap(),
            "a refreshed claim is not stale"
        );

        repo.finish_scan_ok(instance, at(1_000), 1).await.unwrap();
        assert!(
            !repo.refresh_claim(instance).await.unwrap(),
            "a released claim is not refreshed"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_due_requires_three_pending_and_idle(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        let (few, _) = seed_relationship(&pool).await;
        let (idle, _) = seed_relationship(&pool).await;
        let (busy, _) = seed_relationship(&pool).await;
        bump_n(&repo, few, FLUSH_MIN_TURNS - 1).await;
        bump_n(&repo, idle, FLUSH_MIN_TURNS).await;
        bump_n(&repo, busy, FLUSH_MIN_TURNS).await;
        shift(&pool, few, "last_turn_at", "2 hours").await;
        shift(&pool, idle, "last_turn_at", "2 hours").await;
        shift(&pool, busy, "last_turn_at", "10 minutes").await;

        let picked = repo.claim_due(ONE_HOUR, TEN_MIN, 10).await.unwrap();
        assert_eq!(picked, vec![idle], "pending 2 and not-idle-enough are left");
        assert!(repo.load(idle).await.unwrap().unwrap().claimed_at.is_some());
        assert!(repo.load(few).await.unwrap().unwrap().claimed_at.is_none());
        assert!(repo.load(busy).await.unwrap().unwrap().claimed_at.is_none());

        assert!(
            repo.claim_due(ONE_HOUR, TEN_MIN, 10)
                .await
                .unwrap()
                .is_empty(),
            "a claimed relationship is not handed out twice"
        );

        repo.bump(few, at(1_000), None).await.unwrap();
        shift(&pool, few, "last_turn_at", "2 hours").await;
        assert_eq!(
            repo.claim_due(ONE_HOUR, TEN_MIN, 10).await.unwrap(),
            vec![few],
            "the third pending turn makes it due"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn finish_scan_ok_advances_cursor_and_decrements(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        let (instance, _) = seed_relationship(&pool).await;
        bump_n(&repo, instance, 25).await;
        assert!(repo.claim(instance, TEN_MIN).await.unwrap());
        sqlx::query(
            "UPDATE engine.relationship_scan_state SET scan_failures = 2 WHERE instance_id = $1",
        )
        .bind(instance)
        .execute(&pool)
        .await
        .unwrap();

        let cursor = at(5_000);
        assert!(repo.finish_scan_ok(instance, cursor, 20).await.unwrap());
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(s.pending_turns, 5);
        assert_eq!(s.scanned_through_at, cursor);
        assert!(s.claimed_at.is_none());
        assert!(s.last_scanned_at.is_some());
        assert_eq!(s.scan_failures, 0);

        assert!(
            !repo.finish_scan_ok(instance, at(9_000), 5).await.unwrap(),
            "an unclaimed row is not finished"
        );
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!((s.pending_turns, s.scanned_through_at), (5, cursor));

        assert!(
            !repo
                .finish_scan_ok(Uuid::new_v4(), cursor, 1)
                .await
                .unwrap(),
            "a gone row is not finished"
        );

        assert!(repo.claim(instance, TEN_MIN).await.unwrap());
        assert!(repo.finish_scan_ok(instance, at(9_000), 99).await.unwrap());
        assert_eq!(
            repo.load(instance).await.unwrap().unwrap().pending_turns,
            0,
            "the count never goes negative"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn finish_scan_failed_backs_off_then_advances_on_third(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        let (instance, _) = seed_relationship(&pool).await;
        bump_n(&repo, instance, 5).await;
        let before = repo
            .load(instance)
            .await
            .unwrap()
            .unwrap()
            .scanned_through_at;
        let cursor = at(5_000);

        for failures in 1..MAX_SCAN_FAILURES {
            shift(&pool, instance, "last_turn_at", "2 hours").await;
            assert!(repo.claim(instance, TEN_MIN).await.unwrap());
            repo.finish_scan_failed(instance, cursor, 3).await.unwrap();

            let s = repo.load(instance).await.unwrap().unwrap();
            assert_eq!(s.pending_turns, 5, "the count survives a failure");
            assert_eq!(s.scanned_through_at, before, "so does the cursor");
            assert_eq!(s.scan_failures, failures);
            assert!(s.claimed_at.is_none());
            assert_eq!(s.last_scanned_at, None);
            let fresh: bool = sqlx::query_scalar(
                "SELECT last_turn_at > now() - interval '1 minute' \
                 FROM engine.relationship_scan_state WHERE instance_id = $1",
            )
            .bind(instance)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(fresh, "a failure restarts the idle window");
            assert!(
                repo.claim_due(ONE_HOUR, TEN_MIN, 10)
                    .await
                    .unwrap()
                    .is_empty(),
                "so the sweeper leaves it alone for now"
            );
        }

        assert!(repo.claim(instance, TEN_MIN).await.unwrap());
        repo.finish_scan_failed(instance, cursor, 3).await.unwrap();
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(
            s.scanned_through_at, cursor,
            "the third failure skips the span"
        );
        assert_eq!(s.pending_turns, 2);
        assert_eq!(s.scan_failures, 0);
        assert!(s.claimed_at.is_none());
        assert!(s.last_scanned_at.is_some());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn arc_counters_fold_and_strike_out(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        let (instance, _) = seed_relationship(&pool).await;
        repo.bump(instance, at(1_000), None).await.unwrap();

        for expected in 1..=4 {
            assert_eq!(repo.bump_episodes(instance).await.unwrap(), expected);
        }
        assert_eq!(repo.arc_failed(instance).await.unwrap(), 1);
        assert_eq!(repo.arc_failed(instance).await.unwrap(), 2);
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!((s.episodes_since_arc, s.arc_failures), (4, 2));

        repo.fold_episodes(instance).await.unwrap();
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!((s.episodes_since_arc, s.arc_failures), (0, 0));

        repo.bump_episodes(instance).await.unwrap();
        repo.arc_failed(instance).await.unwrap();
        repo.arc_failed(instance).await.unwrap();
        assert_eq!(
            repo.arc_failed(instance).await.unwrap(),
            0,
            "the third failure zeroes the streak it reports"
        );
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!((s.episodes_since_arc, s.arc_failures), (0, 0));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn set_backfilled_through_records_the_mark(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        let (instance, _) = seed_relationship(&pool).await;
        repo.bump(instance, at(1_000), None).await.unwrap();

        repo.set_backfilled_through(instance, at(500))
            .await
            .unwrap();
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(s.backfilled_through_at, Some(at(500)));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0082_creates_five_locked_tables(pool: PgPool) {
        let locked: Vec<String> = sqlx::query_scalar(
            "SELECT c.relname FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'engine' AND c.relname LIKE 'relationship\\_%' \
               AND c.relkind = 'r' AND c.relrowsecurity \
             ORDER BY c.relname",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            locked,
            [
                "relationship_arcs",
                "relationship_episodes",
                "relationship_journal",
                "relationship_scan_events",
                "relationship_scan_state",
            ]
        );
    }
}
