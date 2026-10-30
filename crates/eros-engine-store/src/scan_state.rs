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
    /// The cursor the row was created with; it never moves. Rows up to it are
    /// the backfill's, rows after it live scanning's.
    pub live_from_at: DateTime<Utc>,
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

const COLUMNS: &str =
    "instance_id, scanned_through_at, live_from_at, pending_turns, last_turn_at, \
                       last_scanned_at, scan_failures, claimed_at, user_timezone, \
                       episodes_since_arc, arc_failures, backfilled_through_at";

/// The rows a scan reads (live text and voice sessions, voice only while `$1`,
/// user and assistant rows) that the backfill has not covered: after
/// `backfilled_through_at`, up to `live_from_at`, where live scanning took
/// over. A relationship without a state row has its whole history here.
const BACKFILL_ROWS: &str = "FROM engine.chat_messages m \
     JOIN engine.chat_sessions s ON s.id = m.session_id \
     LEFT JOIN engine.relationship_scan_state st ON st.instance_id = s.instance_id \
     WHERE s.instance_id IS NOT NULL AND NOT s.archived \
       AND s.channel IN ('text', 'voice') AND ($1 OR s.channel = 'text') \
       AND m.role IN ('user', 'assistant') \
       AND (m.channel IS NULL OR m.channel = 'voice') \
       AND m.sent_at > COALESCE(st.backfilled_through_at, '-infinity') \
       AND m.sent_at <= COALESCE(st.live_from_at, 'infinity')";

/// One row of a backfill window: what the backfill needs to cut segments and
/// decide whether a segment's profile memories are still unextracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
pub struct BackfillRow {
    pub sent_at: DateTime<Utc>,
    pub assistant: bool,
    /// The retired memory sweeper never stamped the row's session, or stamped
    /// it before this row was said.
    pub unextracted: bool,
}

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
                 (instance_id, scanned_through_at, live_from_at, pending_turns, last_turn_at, \
                  user_timezone) \
             SELECT $1, c.at, c.at, 1, now(), $3::text \
             FROM (SELECT COALESCE( \
                       (SELECT max(m.sent_at) FROM engine.chat_messages m \
                        JOIN engine.chat_sessions s ON s.id = m.session_id \
                        WHERE s.instance_id = $1 AND m.sent_at < $2::timestamptz), \
                       $2::timestamptz - interval '1 microsecond') AS at) c \
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
    /// `claimed_at`: the relationship must have at least `min_pending` pending
    /// turns, read on the row itself, and no live claim, where a claim older
    /// than `stale` belongs to a worker presumed dead. The `SCAN_K` path passes
    /// `SCAN_K`, so a count read before another scan settled cannot claim; the
    /// idle sweeper uses `claim_due`.
    pub async fn claim(
        &self,
        instance_id: Uuid,
        stale: Duration,
        min_pending: i32,
    ) -> Result<bool, sqlx::Error> {
        let claimed: Option<Uuid> = sqlx::query_scalar(
            "UPDATE engine.relationship_scan_state SET claimed_at = now() \
             WHERE instance_id = $1 AND pending_turns >= $3 \
               AND (claimed_at IS NULL \
                    OR claimed_at < now() - make_interval(secs => $2::float8)) \
             RETURNING instance_id",
        )
        .bind(instance_id)
        .bind(stale.as_secs() as i64)
        .bind(min_pending)
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

    /// A scan succeeded: the cursor moves to `cursor_to` (never back), the
    /// `covered_user_rows` it read leave the pending count, and the claim is
    /// released. `false` when the row is gone or the claim was already
    /// released, in which case nothing changed.
    pub async fn finish_scan_ok(
        &self,
        instance_id: Uuid,
        cursor_to: DateTime<Utc>,
        covered_user_rows: i32,
    ) -> Result<bool, sqlx::Error> {
        let finished: Option<Uuid> = sqlx::query_scalar(
            "UPDATE engine.relationship_scan_state SET \
                scanned_through_at = GREATEST(scanned_through_at, $2), \
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
    /// reset; the caller logs it. Only a released claim protects the row: one
    /// whose claim is already released is left alone, but a claim another
    /// worker took over once this one went stale is not told apart from the
    /// caller's own and is cleared. The cursor never moves back either way.
    pub async fn finish_scan_failed(
        &self,
        instance_id: Uuid,
        cursor_to: DateTime<Utc>,
        covered_user_rows: i32,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.relationship_scan_state SET \
                scanned_through_at = CASE WHEN scan_failures + 1 >= $4 \
                                          THEN GREATEST(scanned_through_at, $2) \
                                          ELSE scanned_through_at END, \
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
             WHERE instance_id = $1 AND claimed_at IS NOT NULL",
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

    /// An arc rewrite is starting: the episode count restarts so the scans that
    /// land during it cannot start a second rewrite over the same episodes.
    /// The failure streak stays; it ends only with a folded or struck-out arc.
    pub async fn reserve_fold(&self, instance_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.relationship_scan_state SET episodes_since_arc = 0 \
             WHERE instance_id = $1",
        )
        .bind(instance_id)
        .execute(self.pool)
        .await?;
        Ok(())
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

    /// An arc rewrite failed; returns the failure count this call registered:
    /// 1, 2, or `MAX_ARC_FAILURES` for the failure that strikes out, which also
    /// drops the episode count and resets the streak so the next episode starts
    /// a fresh count. `0` only when the row is gone.
    pub async fn arc_failed(&self, instance_id: Uuid) -> Result<i32, sqlx::Error> {
        let n: Option<i32> = sqlx::query_scalar(
            "UPDATE engine.relationship_scan_state SET \
                episodes_since_arc = CASE WHEN arc_failures + 1 >= $2 \
                                          THEN 0 ELSE episodes_since_arc END, \
                arc_failures = CASE WHEN arc_failures + 1 >= $2 \
                                    THEN 0 ELSE arc_failures + 1 END \
             WHERE instance_id = $1 \
             RETURNING CASE WHEN arc_failures = 0 THEN $2 ELSE arc_failures END",
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

    /// Take the relationship for one backfill segment: unlike `claim`, it
    /// needs no pending turns, only no live claim (one older than `stale` is
    /// taken over). `false` when the row is gone or a scan holds it.
    pub async fn claim_for_backfill(
        &self,
        instance_id: Uuid,
        stale: Duration,
    ) -> Result<bool, sqlx::Error> {
        let claimed: Option<Uuid> = sqlx::query_scalar(
            "UPDATE engine.relationship_scan_state SET claimed_at = now() \
             WHERE instance_id = $1 \
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

    /// Release a claim without settling anything: a backfill segment's, or a
    /// live scan's that found nothing past the cursor.
    pub async fn release_claim(&self, instance_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.relationship_scan_state SET claimed_at = NULL WHERE instance_id = $1",
        )
        .bind(instance_id)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Open the scan state of a relationship that has history but has not
    /// chatted since the scanner shipped: the cursor sits at its newest row,
    /// so everything it said so far is the backfill's and nothing is pending.
    /// `true` when this call created the row; `false` when one exists or the
    /// relationship has no rows.
    pub async fn ensure_for_backfill(&self, instance_id: Uuid) -> Result<bool, sqlx::Error> {
        let created: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO engine.relationship_scan_state \
                 (instance_id, scanned_through_at, live_from_at) \
             SELECT $1, max(m.sent_at), max(m.sent_at) FROM engine.chat_messages m \
             JOIN engine.chat_sessions s ON s.id = m.session_id \
             WHERE s.instance_id = $1 AND NOT s.archived \
               AND s.channel IN ('text', 'voice') \
               AND m.role IN ('user', 'assistant') \
               AND (m.channel IS NULL OR m.channel = 'voice') \
             HAVING max(m.sent_at) IS NOT NULL \
             ON CONFLICT (instance_id) DO NOTHING \
             RETURNING instance_id",
        )
        .bind(instance_id)
        .fetch_optional(self.pool)
        .await?;
        Ok(created.is_some())
    }

    /// Relationships with at least `min_rows` rows left for the backfill, most
    /// first, at most `limit` (all when `None`), only `only` when given. A
    /// relationship without a state row counts its whole history.
    pub async fn instances_with_history(
        &self,
        min_rows: i64,
        limit: Option<i64>,
        only: Option<Uuid>,
        include_voice: bool,
    ) -> Result<Vec<(Uuid, i64)>, sqlx::Error> {
        sqlx::query_as(&format!(
            "SELECT s.instance_id, count(*) {BACKFILL_ROWS} \
               AND ($2::uuid IS NULL OR s.instance_id = $2) \
             GROUP BY s.instance_id \
             HAVING count(*) >= $3 \
             ORDER BY count(*) DESC, s.instance_id \
             LIMIT $4"
        ))
        .bind(include_voice)
        .bind(only)
        .bind(min_rows)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }

    /// The rows the backfill has left for one relationship, oldest first, in
    /// the order a scan reads them.
    pub async fn backfill_window(
        &self,
        instance_id: Uuid,
        include_voice: bool,
    ) -> Result<Vec<BackfillRow>, sqlx::Error> {
        sqlx::query_as(&format!(
            "SELECT m.sent_at, m.role = 'assistant' AS assistant, \
                    (s.classified_at IS NULL OR s.classified_at < m.sent_at) AS unextracted \
             {BACKFILL_ROWS} AND s.instance_id = $2 \
             ORDER BY m.sent_at, m.id"
        ))
        .bind(include_voice)
        .bind(instance_id)
        .fetch_all(self.pool)
        .await
    }

    /// Record how far the backfill has read. Only the claim holder settles:
    /// `false` when the row is gone or its claim was released, in which case
    /// nothing changed.
    pub async fn set_backfilled_through(
        &self,
        instance_id: Uuid,
        at: DateTime<Utc>,
    ) -> Result<bool, sqlx::Error> {
        let set: Option<Uuid> = sqlx::query_scalar(
            "UPDATE engine.relationship_scan_state SET backfilled_through_at = $2 \
             WHERE instance_id = $1 AND claimed_at IS NOT NULL \
             RETURNING instance_id",
        )
        .bind(instance_id)
        .bind(at)
        .fetch_optional(self.pool)
        .await?;
        Ok(set.is_some())
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
        assert_eq!(
            s.live_from_at, t0,
            "live scanning starts at the first cursor"
        );
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
        assert_eq!(s.live_from_at, t0);
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
            !repo.claim(instance, TEN_MIN, 1).await.unwrap(),
            "no row, nothing to claim"
        );

        repo.bump(instance, at(1_000), None).await.unwrap();
        assert!(
            repo.claim(instance, TEN_MIN, 1).await.unwrap(),
            "one pending turn is enough at a floor of one"
        );
        assert!(
            !repo.claim(instance, TEN_MIN, 1).await.unwrap(),
            "a live claim blocks the second worker"
        );

        shift(&pool, instance, "claimed_at", "9 minutes").await;
        assert!(!repo.claim(instance, TEN_MIN, 1).await.unwrap());
        shift(&pool, instance, "claimed_at", "11 minutes").await;
        assert!(
            repo.claim(instance, TEN_MIN, 1).await.unwrap(),
            "a stale claim is taken over"
        );

        assert!(repo.finish_scan_ok(instance, at(1_000), 1).await.unwrap());
        assert!(
            !repo.claim(instance, TEN_MIN, 1).await.unwrap(),
            "nothing pending, nothing to claim"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn k_path_claim_needs_scan_k_pending_turns(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        let (instance, _) = seed_relationship(&pool).await;
        repo.bump(instance, at(1_000), None).await.unwrap();
        assert!(
            !repo.claim(instance, TEN_MIN, SCAN_K).await.unwrap(),
            "one pending turn is not a K-path scan"
        );
        assert!(repo
            .load(instance)
            .await
            .unwrap()
            .unwrap()
            .claimed_at
            .is_none());

        bump_n(&repo, instance, SCAN_K - 2).await;
        assert!(!repo.claim(instance, TEN_MIN, SCAN_K).await.unwrap());
        repo.bump(instance, at(1_000), None).await.unwrap();
        assert!(
            repo.claim(instance, TEN_MIN, SCAN_K).await.unwrap(),
            "the K-th pending turn makes it claimable"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn cursor_never_moves_back(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        let (instance, _) = seed_relationship(&pool).await;
        bump_n(&repo, instance, 5).await;
        assert!(repo.claim(instance, TEN_MIN, 1).await.unwrap());
        assert!(repo.finish_scan_ok(instance, at(5_000), 1).await.unwrap());

        assert!(repo.claim(instance, TEN_MIN, 1).await.unwrap());
        assert!(repo.finish_scan_ok(instance, at(3_000), 1).await.unwrap());
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(
            (s.scanned_through_at, s.pending_turns),
            (at(5_000), 3),
            "an older bound settles the count but keeps the cursor"
        );

        sqlx::query(
            "UPDATE engine.relationship_scan_state SET scan_failures = $2 WHERE instance_id = $1",
        )
        .bind(instance)
        .bind(MAX_SCAN_FAILURES - 1)
        .execute(&pool)
        .await
        .unwrap();
        assert!(repo.claim(instance, TEN_MIN, 1).await.unwrap());
        repo.finish_scan_failed(instance, at(3_000), 1)
            .await
            .unwrap();
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(
            (s.scanned_through_at, s.pending_turns, s.scan_failures),
            (at(5_000), 2, 0),
            "a strike-out on an older bound keeps the cursor too"
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
        assert!(repo.claim(instance, TEN_MIN, 1).await.unwrap());
        shift(&pool, instance, "claimed_at", "11 minutes").await;
        assert!(repo.refresh_claim(instance).await.unwrap());
        assert!(
            !repo.claim(instance, TEN_MIN, 1).await.unwrap(),
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
        assert!(repo.claim(instance, TEN_MIN, 1).await.unwrap());
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

        assert!(repo.claim(instance, TEN_MIN, 1).await.unwrap());
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
            assert!(repo.claim(instance, TEN_MIN, 1).await.unwrap());
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

        assert!(repo.claim(instance, TEN_MIN, 1).await.unwrap());
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
            MAX_ARC_FAILURES,
            "the strike-out reports the count it registered, then resets"
        );
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!((s.episodes_since_arc, s.arc_failures), (0, 0));

        assert_eq!(
            repo.arc_failed(Uuid::new_v4()).await.unwrap(),
            0,
            "a gone row registers nothing"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn reserve_fold_restarts_the_arc_count_and_keeps_the_streak(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        let (instance, _) = seed_relationship(&pool).await;
        repo.bump(instance, at(1_000), None).await.unwrap();
        for _ in 0..4 {
            repo.bump_episodes(instance).await.unwrap();
        }
        repo.arc_failed(instance).await.unwrap();
        repo.arc_failed(instance).await.unwrap();

        repo.reserve_fold(instance).await.unwrap();
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!((s.episodes_since_arc, s.arc_failures), (0, 2));
        assert_eq!(
            repo.arc_failed(instance).await.unwrap(),
            MAX_ARC_FAILURES,
            "a reserved fold still strikes out on the third failure"
        );

        repo.reserve_fold(Uuid::new_v4()).await.unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn finish_scan_failed_leaves_an_unclaimed_row_alone(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        let (instance, _) = seed_relationship(&pool).await;
        bump_n(&repo, instance, 5).await;
        sqlx::query(
            "UPDATE engine.relationship_scan_state SET scan_failures = $2 WHERE instance_id = $1",
        )
        .bind(instance)
        .bind(MAX_SCAN_FAILURES - 1)
        .execute(&pool)
        .await
        .unwrap();
        shift(&pool, instance, "last_turn_at", "2 hours").await;
        let before = repo.load(instance).await.unwrap().unwrap();
        assert!(before.claimed_at.is_none());

        // A late failure from a worker whose claim was already released (or
        // taken over and finished) must not strike out or move the cursor.
        repo.finish_scan_failed(instance, at(5_000), 3)
            .await
            .unwrap();
        let after = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(after.scanned_through_at, before.scanned_through_at);
        assert_eq!(after.pending_turns, before.pending_turns);
        assert_eq!(after.scan_failures, MAX_SCAN_FAILURES - 1);
        assert_eq!(after.last_turn_at, before.last_turn_at);
        assert_eq!(after.last_scanned_at, None);

        repo.finish_scan_failed(Uuid::new_v4(), at(5_000), 3)
            .await
            .unwrap();
    }

    async fn insert_journal(
        pool: &PgPool,
        instance_id: Uuid,
        dedup_key: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO engine.relationship_journal \
                 (instance_id, occurred_on, kind, realm, summary, source, dedup_key, recurrence) \
             VALUES ($1, current_date, 'seed', 'real', 'seed', 'engine', $2, 'none')",
        )
        .bind(instance_id)
        .bind(dedup_key)
        .execute(pool)
        .await
        .map(|_| ())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn journal_dedup_key_is_unique_per_instance_unless_null(pool: PgPool) {
        let (instance, _) = seed_relationship(&pool).await;
        let (other, _) = seed_relationship(&pool).await;

        insert_journal(&pool, instance, Some("k")).await.unwrap();
        let err = insert_journal(&pool, instance, Some("k"))
            .await
            .expect_err("same (instance_id, dedup_key) is a duplicate");
        assert_eq!(
            err.as_database_error().and_then(|e| e.constraint()),
            Some("uq_relationship_journal_dedup")
        );
        insert_journal(&pool, other, Some("k"))
            .await
            .expect("the key is scoped to its instance");

        insert_journal(&pool, instance, None).await.unwrap();
        insert_journal(&pool, instance, None)
            .await
            .expect("a NULL dedup_key marks a repeatable kind");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn set_backfilled_through_records_the_mark(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        let (instance, _) = seed_relationship(&pool).await;
        repo.bump(instance, at(1_000), None).await.unwrap();

        assert!(
            !repo
                .set_backfilled_through(instance, at(500))
                .await
                .unwrap(),
            "only the claim holder settles"
        );
        assert_eq!(
            repo.load(instance)
                .await
                .unwrap()
                .unwrap()
                .backfilled_through_at,
            None
        );
        assert!(repo.claim_for_backfill(instance, TEN_MIN).await.unwrap());
        assert!(repo
            .set_backfilled_through(instance, at(500))
            .await
            .unwrap());
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(s.backfilled_through_at, Some(at(500)));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_for_backfill_ignores_pending_and_respects_live_claim(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        let (instance, _) = seed_relationship(&pool).await;
        assert!(
            !repo.claim_for_backfill(instance, TEN_MIN).await.unwrap(),
            "no row, nothing to claim"
        );

        repo.bump(instance, at(1_000), None).await.unwrap();
        assert!(repo.claim(instance, TEN_MIN, 1).await.unwrap());
        assert!(repo.finish_scan_ok(instance, at(1_000), 1).await.unwrap());
        assert!(!repo.claim(instance, TEN_MIN, 1).await.unwrap());
        assert!(
            repo.claim_for_backfill(instance, TEN_MIN).await.unwrap(),
            "the backfill claims with nothing pending"
        );
        assert!(!repo.claim_for_backfill(instance, TEN_MIN).await.unwrap());
        repo.bump(instance, at(2_000), None).await.unwrap();
        assert!(
            !repo.claim(instance, TEN_MIN, 1).await.unwrap(),
            "a live scan waits for the backfill's segment"
        );

        repo.release_claim(instance).await.unwrap();
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!((s.claimed_at, s.pending_turns), (None, 1));
        assert!(repo.claim(instance, TEN_MIN, 1).await.unwrap());
        assert!(
            !repo.claim_for_backfill(instance, TEN_MIN).await.unwrap(),
            "the backfill waits for a live claim"
        );
        shift(&pool, instance, "claimed_at", "11 minutes").await;
        assert!(
            repo.claim_for_backfill(instance, TEN_MIN).await.unwrap(),
            "a stale claim is taken over"
        );

        repo.release_claim(Uuid::new_v4()).await.unwrap();
    }

    /// A session of `channel` on the relationship, archived or not.
    async fn seed_channel_session(
        pool: &PgPool,
        instance: Uuid,
        channel: &str,
        archived: bool,
    ) -> Uuid {
        let owner: Uuid =
            sqlx::query_scalar("SELECT owner_uid FROM engine.persona_instances WHERE id = $1")
                .bind(instance)
                .fetch_one(pool)
                .await
                .unwrap();
        let session = seed_session(pool, owner, instance, archived).await;
        sqlx::query("UPDATE engine.chat_sessions SET channel = $2 WHERE id = $1")
            .bind(session)
            .bind(channel)
            .execute(pool)
            .await
            .unwrap();
        session
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn ensure_for_backfill_places_cursor_at_newest_live_row(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        let (instance, session) = seed_relationship(&pool).await;
        let voice = seed_channel_session(&pool, instance, "voice", false).await;
        let archived = seed_channel_session(&pool, instance, "text", true).await;
        seed_row(&pool, session, "user", at(1_000)).await;
        seed_row(&pool, voice, "assistant", at(2_000)).await;
        seed_row(&pool, archived, "user", at(3_000)).await;
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, channel, sent_at) \
             VALUES ($1, 'assistant', 'qa', 'product_qa', $2)",
        )
        .bind(session)
        .bind(at(4_000))
        .execute(&pool)
        .await
        .unwrap();

        assert!(repo.ensure_for_backfill(instance).await.unwrap());
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(
            s.scanned_through_at,
            at(2_000),
            "archived and product Q&A rows are not history"
        );
        assert_eq!(s.live_from_at, at(2_000));
        assert_eq!((s.pending_turns, s.last_turn_at), (0, None));
        assert_eq!(s.backfilled_through_at, None);

        seed_row(&pool, session, "user", at(5_000)).await;
        assert!(!repo.ensure_for_backfill(instance).await.unwrap());
        assert_eq!(
            repo.load(instance)
                .await
                .unwrap()
                .unwrap()
                .scanned_through_at,
            at(2_000),
            "an existing row keeps its cursor"
        );

        let (silent, _) = seed_relationship(&pool).await;
        assert!(
            !repo.ensure_for_backfill(silent).await.unwrap(),
            "no rows, no state"
        );
        assert!(repo.load(silent).await.unwrap().is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn instances_with_history_counts_the_unscanned_window(pool: PgPool) {
        let repo = ScanStateRepo { pool: &pool };
        // Five rows, no state yet: the whole history is the window.
        let (fresh, fresh_session) = seed_relationship(&pool).await;
        for t in 1..=5 {
            seed_row(&pool, fresh_session, "user", at(t * 1_000)).await;
        }
        // Six rows; live scanning started at the fifth, the backfill has read
        // the first: rows two to four remain, one of them in a voice session.
        let (started, started_session) = seed_relationship(&pool).await;
        let voice = seed_channel_session(&pool, started, "voice", false).await;
        for t in 1..=6 {
            let session = if t == 3 { voice } else { started_session };
            let role = if t % 2 == 0 { "assistant" } else { "user" };
            seed_row(&pool, session, role, at(t * 1_000)).await;
        }
        repo.bump(started, at(5_000), None).await.unwrap();
        assert!(repo.claim_for_backfill(started, TEN_MIN).await.unwrap());
        assert!(repo
            .set_backfilled_through(started, at(1_000))
            .await
            .unwrap());
        // A live scan has since read past the sixth row: that progress is not
        // the backfill's window.
        assert!(repo.finish_scan_ok(started, at(6_000), 1).await.unwrap());
        // One row: under the floor.
        let (short, short_session) = seed_relationship(&pool).await;
        seed_row(&pool, short_session, "user", at(1_000)).await;

        assert_eq!(
            repo.instances_with_history(2, None, None, true)
                .await
                .unwrap(),
            vec![(fresh, 5), (started, 3)]
        );
        assert_eq!(
            repo.instances_with_history(2, None, None, false)
                .await
                .unwrap(),
            vec![(fresh, 5), (started, 2)],
            "voice rows count only while voice is read"
        );
        assert_eq!(
            repo.instances_with_history(2, Some(1), None, true)
                .await
                .unwrap(),
            vec![(fresh, 5)]
        );
        assert_eq!(
            repo.instances_with_history(1, None, Some(short), true)
                .await
                .unwrap(),
            vec![(short, 1)]
        );

        // The session was stamped by the retired sweeper between rows two and
        // four; the voice session never was.
        sqlx::query("UPDATE engine.chat_sessions SET classified_at = $2 WHERE id = $1")
            .bind(started_session)
            .bind(at(3_000))
            .execute(&pool)
            .await
            .unwrap();
        let window = repo.backfill_window(started, true).await.unwrap();
        assert_eq!(
            window
                .iter()
                .map(|r| (r.sent_at, r.assistant, r.unextracted))
                .collect::<Vec<_>>(),
            [
                (at(2_000), true, false),
                (at(3_000), false, true),
                (at(4_000), true, true),
            ]
        );
        assert_eq!(
            repo.backfill_window(fresh, true).await.unwrap().len(),
            5,
            "no state row: every row"
        );
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
