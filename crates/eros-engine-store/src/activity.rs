// SPDX-License-Identifier: AGPL-3.0-only
//! Activities: a date the user invites the persona to, one row each in
//! `engine.activities`, run in a sub-session of its own (spec
//! 2026-11-12-activity-sub-session-design.md §9). The event row is the only
//! record of when the date starts and ends; the row here derives its status
//! from that span and from `settled_at`.

use chrono::{DateTime, Duration, Utc};
use eros_engine_core::events::EventKind;
use eros_engine_core::types::DateDuration;
use sqlx::PgPool;
use uuid::Uuid;

use crate::chat::ChatMessage;
use crate::events::{lock_instance, EventRepo, NewEvent};
use crate::session_archive::lock_live_session;

/// An activity's columns, in `ActivityRecord` order; `started_at` and
/// `ends_at` come from the event.
const COLUMNS: &str = "a.id, a.session_id, a.sub_session_id, a.message_id, a.event_id, \
                       a.duration, a.place, a.content, e.started_at, e.ends_at, \
                       a.settle_claimed_at, a.settled_at, a.created_at";

/// `LEFT JOIN`: an activity whose event row is gone still reads, as ended.
const FROM: &str = "engine.activities a LEFT JOIN engine.persona_events e ON e.id = a.event_id";

fn select(tail: &str) -> String {
    format!("SELECT {COLUMNS} FROM {FROM} {tail}")
}

/// One date. `session_id` is the parent, `sub_session_id` where it is played.
/// `message_id` is the invitation and `event_id` the `on_a_date` event, either
/// `None` once its row is gone; `started_at` and `ends_at` are the event's,
/// `None` with it.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct ActivityRecord {
    pub id: Uuid,
    pub session_id: Uuid,
    pub sub_session_id: Uuid,
    pub message_id: Option<Uuid>,
    pub event_id: Option<Uuid>,
    pub duration: String,
    pub place: String,
    pub content: String,
    pub started_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub settle_claimed_at: Option<DateTime<Utc>>,
    pub settled_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Where a date stands; derived, never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityStatus {
    Ongoing,
    Ended,
    Settled,
}

impl ActivityStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ongoing => "ongoing",
            Self::Ended => "ended",
            Self::Settled => "settled",
        }
    }
}

impl ActivityRecord {
    /// Settled once `settled_at` is set; otherwise ongoing while the event has
    /// not ended, else ended, a missing event included.
    pub fn status_at(&self, now: DateTime<Utc>) -> ActivityStatus {
        if self.settled_at.is_some() {
            ActivityStatus::Settled
        } else if self.ends_at.is_some_and(|ends| ends > now) {
            ActivityStatus::Ongoing
        } else {
            ActivityStatus::Ended
        }
    }

    /// The column's CHECK admits `short` and `mid` only.
    pub fn duration(&self) -> DateDuration {
        DateDuration::parse(&self.duration).unwrap_or(DateDuration::Short)
    }
}

/// A date to open. `user_id` and `instance_id` are the parent session's; they
/// go to the sub-session and the event, not to the activity row.
pub struct NewActivity<'a> {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Uuid,
    /// The invitation, a user row of the parent.
    pub message_id: Uuid,
    pub duration: DateDuration,
    pub place: &'a str,
    pub content: &'a str,
}

/// What a settlement claim found.
#[derive(Debug, Clone, PartialEq)]
pub enum SettleClaim {
    /// The caller now holds the claim and settles.
    Claimed(ActivityRecord),
    /// Another caller holds a live claim.
    Held,
    /// Already settled; the stored row.
    Settled(ActivityRecord),
}

pub struct ActivityRepo<'a> {
    pub pool: &'a PgPool,
}

/// [`ActivityRepo::date_on`] on any executor, so `accept` reads it under the
/// instance's lock.
async fn date_on<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    instance_id: Uuid,
    invitation: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS ( \
             SELECT 1 FROM engine.persona_events e \
              WHERE e.instance_id = $1 AND e.kind = $2 AND e.ends_at > $3 \
                AND NOT EXISTS (SELECT 1 FROM engine.activities a \
                                 WHERE a.event_id = e.id AND a.message_id = $4))",
    )
    .bind(instance_id)
    .bind(EventKind::OnADate.as_str())
    .bind(now)
    .bind(invitation)
    .fetch_one(executor)
    .await
}

impl ActivityRepo<'_> {
    /// Open the date `a` for an accepted invitation, in one transaction: store
    /// her answer on the invitation row (`metadata.action.response = accept`),
    /// then, under the instance's lock, write the sub-session
    /// (`channel = 'activity'`), the `on_a_date` event and the activity row.
    ///
    /// The event starts at her reply's `sent_at` (`now` without a reply) and
    /// runs for the duration; it ends any incident under way and leaves a
    /// background running. An invitation that already has its date gets that
    /// date back, so a re-driven turn opens one. `None` while a date of the
    /// instance has not ended at `now`, one that starts after `now` included
    /// (its start is a stored `sent_at`, which can be later than the caller's
    /// clock), or once the parent is archived: nothing is written but her
    /// answer.
    ///
    /// The parent's row is locked first, as settlement locks it before it
    /// writes, so an archive waits for the date to open (and then ends it) or
    /// is seen here.
    pub async fn accept(
        &self,
        a: &NewActivity<'_>,
        now: DateTime<Utc>,
    ) -> Result<Option<ActivityRecord>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let live = lock_live_session(&mut tx, a.session_id).await?;
        sqlx::query(
            "UPDATE engine.chat_messages \
                SET metadata = jsonb_set(metadata, '{action,response}', to_jsonb('accept'::text)) \
              WHERE id = $1 AND metadata ? 'action'",
        )
        .bind(a.message_id)
        .execute(&mut *tx)
        .await?;
        if !live {
            tx.commit().await?;
            return Ok(None);
        }

        // The lookup and the insert are separate statements: a second driver
        // of the same turn must wait here and then find the first's row.
        lock_instance(&mut tx, a.instance_id).await?;
        let existing: Option<ActivityRecord> = sqlx::query_as(&select("WHERE a.message_id = $1"))
            .bind(a.message_id)
            .fetch_optional(&mut *tx)
            .await?;
        if existing.is_some() {
            tx.commit().await?;
            return Ok(existing);
        }
        if date_on(&mut *tx, a.instance_id, None, now).await? {
            tx.commit().await?;
            return Ok(None);
        }

        let replied: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT max(sent_at) FROM engine.chat_messages \
              WHERE user_message_id = $1 AND role = 'assistant'",
        )
        .bind(a.message_id)
        .fetch_one(&mut *tx)
        .await?;
        let started_at = replied.unwrap_or(now);

        let sub_session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, channel, metadata) \
             VALUES ($1, $2, 'activity', '{}'::jsonb) RETURNING id",
        )
        .bind(a.user_id)
        .bind(a.instance_id)
        .fetch_one(&mut *tx)
        .await?;
        let event_key = format!("activity:{}", a.message_id);
        let event = EventRepo::insert_preempting_in_tx(
            &mut tx,
            &NewEvent {
                instance_id: a.instance_id,
                session_id: a.session_id,
                event_key: &event_key,
                kind: EventKind::OnADate,
                started_at,
                ends_at: started_at + Duration::hours(a.duration.hours()),
            },
        )
        .await?;
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.activities \
               (session_id, sub_session_id, message_id, event_id, duration, place, content) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
        )
        .bind(a.session_id)
        .bind(sub_session_id)
        .bind(a.message_id)
        .bind(event.id)
        .bind(a.duration.as_str())
        .bind(a.place)
        .bind(a.content)
        .fetch_one(&mut *tx)
        .await?;
        let record = sqlx::query_as(&select("WHERE a.id = $1"))
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some(record))
    }

    /// Whether a date of the instance has not ended at `now`, one that starts
    /// after `now` included: [`Self::accept`]'s guard. The date `invitation`
    /// opened, if any, does not count, as `accept` hands that one back.
    pub async fn date_on(
        &self,
        instance_id: Uuid,
        invitation: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> Result<bool, sqlx::Error> {
        date_on(self.pool, instance_id, invitation, now).await
    }

    pub async fn get(&self, id: Uuid) -> Result<Option<ActivityRecord>, sqlx::Error> {
        sqlx::query_as(&select("WHERE a.id = $1"))
            .bind(id)
            .fetch_optional(self.pool)
            .await
    }

    /// The date played in `sub_session_id`.
    pub async fn by_sub_session(
        &self,
        sub_session_id: Uuid,
    ) -> Result<Option<ActivityRecord>, sqlx::Error> {
        sqlx::query_as(&select("WHERE a.sub_session_id = $1"))
            .bind(sub_session_id)
            .fetch_optional(self.pool)
            .await
    }

    /// The instance's newest date not yet settled, through its parent session;
    /// an archived parent holds none.
    pub async fn unsettled_for_instance(
        &self,
        instance_id: Uuid,
    ) -> Result<Option<ActivityRecord>, sqlx::Error> {
        sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM {FROM} \
               JOIN engine.chat_sessions s ON s.id = a.session_id \
              WHERE s.instance_id = $1 AND NOT s.archived AND a.settled_at IS NULL \
              ORDER BY a.created_at DESC, a.id DESC LIMIT 1"
        ))
        .bind(instance_id)
        .fetch_optional(self.pool)
        .await
    }

    /// The dates whose invitation is one of `ids`, oldest first.
    pub async fn by_message_ids(&self, ids: &[Uuid]) -> Result<Vec<ActivityRecord>, sqlx::Error> {
        sqlx::query_as(&select(
            "WHERE a.message_id = ANY($1) ORDER BY a.created_at, a.id",
        ))
        .bind(ids)
        .fetch_all(self.pool)
        .await
    }

    /// The date the persona is back from in `session_id` (the parent) at `at`:
    /// the newest one that ended at or before `at`, until her first line with
    /// text after it (a row stamped after `at` is not yet said). Spec §8.
    pub async fn returned_from(
        &self,
        session_id: Uuid,
        at: DateTime<Utc>,
    ) -> Result<Option<ActivityRecord>, sqlx::Error> {
        sqlx::query_as(&select(
            "WHERE a.session_id = $1 AND e.ends_at <= $2 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM engine.chat_messages m \
                    WHERE m.session_id = a.session_id AND m.role = 'assistant' \
                      AND btrim(m.content) <> '' \
                      AND m.sent_at > e.ends_at AND m.sent_at <= $2) \
             ORDER BY e.ends_at DESC LIMIT 1",
        ))
        .bind(session_id)
        .bind(at)
        .fetch_optional(self.pool)
        .await
    }

    /// Move the end of a date still on to `now`. `false` when it has ended
    /// already, has not started, or has no event.
    pub async fn end_early(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool, sqlx::Error> {
        let moved = sqlx::query(
            "UPDATE engine.persona_events e SET ends_at = $2 \
               FROM engine.activities a \
              WHERE a.id = $1 AND e.id = a.event_id \
                AND e.started_at < $2 AND e.ends_at > $2",
        )
        .bind(id)
        .bind(now)
        .execute(self.pool)
        .await?;
        Ok(moved.rows_affected() > 0)
    }

    /// Take the settlement of `id` for the caller: a claim older than `stale`
    /// counts as abandoned and is taken over. One statement, so two callers
    /// never both claim; a refused caller is told whether the date is settled
    /// or another caller is on it. A missing activity is `RowNotFound`.
    pub async fn claim_settlement(
        &self,
        id: Uuid,
        now: DateTime<Utc>,
        stale: Duration,
    ) -> Result<SettleClaim, sqlx::Error> {
        let claimed: Option<ActivityRecord> = sqlx::query_as(&format!(
            "WITH a AS ( \
                 UPDATE engine.activities SET settle_claimed_at = $2 \
                  WHERE id = $1 AND settled_at IS NULL \
                    AND (settle_claimed_at IS NULL OR settle_claimed_at < $3) \
                 RETURNING *) \
             SELECT {COLUMNS} FROM a LEFT JOIN engine.persona_events e ON e.id = a.event_id"
        ))
        .bind(id)
        .bind(now)
        .bind(now - stale)
        .fetch_optional(self.pool)
        .await?;
        if let Some(a) = claimed {
            return Ok(SettleClaim::Claimed(a));
        }
        let held: ActivityRecord = sqlx::query_as(&select("WHERE a.id = $1"))
            .bind(id)
            .fetch_one(self.pool)
            .await?;
        Ok(if held.settled_at.is_some() {
            SettleClaim::Settled(held)
        } else {
            SettleClaim::Held
        })
    }

    /// Give back the claim a caller took at `claimed_at`, so the next caller
    /// settles at once. A claim taken over since, or a settled date, is left
    /// as it is.
    pub async fn release_claim(
        &self,
        id: Uuid,
        claimed_at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.activities SET settle_claimed_at = NULL \
              WHERE id = $1 AND settle_claimed_at = $2 AND settled_at IS NULL",
        )
        .bind(id)
        .bind(claimed_at)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// The newest `limit` rows of the sub-session, oldest first, and how many
    /// earlier rows were left out.
    pub async fn transcript(
        &self,
        sub_session_id: Uuid,
        limit: i64,
    ) -> Result<(Vec<ChatMessage>, i64), sqlx::Error> {
        let mut rows = sqlx::query_as::<_, ChatMessage>(
            "SELECT * FROM engine.chat_messages WHERE session_id = $1 \
             ORDER BY sent_at DESC, id DESC LIMIT $2",
        )
        .bind(sub_session_id)
        .bind(limit)
        .fetch_all(self.pool)
        .await?;
        rows.reverse();
        let Some(oldest) = rows.first() else {
            return Ok((rows, 0));
        };
        let left_out = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
              WHERE session_id = $1 AND (sent_at, id) < ($2, $3)",
        )
        .bind(sub_session_id)
        .bind(oldest.sent_at)
        .bind(oldest.id)
        .fetch_one(self.pool)
        .await?;
        Ok((rows, left_out))
    }

    /// The turns of the sub-session worth judging: user rows with text that
    /// have at least one reply with text.
    pub async fn substantive_turns(&self, sub_session_id: Uuid) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages u \
              WHERE u.session_id = $1 AND u.role = 'user' AND btrim(u.content) <> '' \
                AND EXISTS (SELECT 1 FROM engine.chat_messages r \
                             WHERE r.user_message_id = u.id AND r.role = 'assistant' \
                               AND btrim(r.content) <> '')",
        )
        .bind(sub_session_id)
        .fetch_one(self.pool)
        .await
    }

    /// Stamp the date settled inside the caller's transaction, so the stamp
    /// commits with what settlement writes. A repeat leaves the first stamp.
    pub async fn mark_settled_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.activities SET settled_at = $2 WHERE id = $1 AND settled_at IS NULL",
        )
        .bind(id)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ChatRepo;
    use crate::events::{EventRepo, NewEvent};
    use crate::testutil::seed_persona_instance;
    use chrono::Duration;
    use eros_engine_core::events::EventKind;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// When her accepting reply was stored.
    const REPLY_AT: &str = "2026-11-12T10:00:00Z";
    /// The clock of the turn that accepts.
    const NOW: &str = "2026-11-12T10:00:05Z";

    /// A parent text session with the invitation and her accepting reply.
    struct Scene {
        owner: Uuid,
        instance: Uuid,
        session: Uuid,
        invite: Uuid,
    }

    #[allow(clippy::too_many_arguments)]
    async fn put(
        pool: &PgPool,
        session: Uuid,
        role: &str,
        content: &str,
        at: &str,
        answers: Option<Uuid>,
        metadata: Option<serde_json::Value>,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_messages \
               (session_id, role, content, sent_at, user_message_id, metadata) \
             VALUES ($1, $2, $3, $4::timestamptz, $5, $6) RETURNING id",
        )
        .bind(session)
        .bind(role)
        .bind(content)
        .bind(at)
        .bind(answers)
        .bind(metadata)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn scene(pool: &PgPool) -> Scene {
        let owner = Uuid::new_v4();
        let instance = seed_persona_instance(pool, owner).await;
        let session = ChatRepo { pool }
            .create_session(owner, instance)
            .await
            .unwrap()
            .id;
        let invite = put(
            pool,
            session,
            "user",
            "周末去海边走走吧",
            "2026-11-12T09:59:50Z",
            None,
            Some(serde_json::json!({
                "action": {"type": "date", "place": "海边", "content": "散步", "duration": "mid"}
            })),
        )
        .await;
        put(
            pool,
            session,
            "assistant",
            "好呀",
            REPLY_AT,
            Some(invite),
            None,
        )
        .await;
        Scene {
            owner,
            instance,
            session,
            invite,
        }
    }

    fn invitation(s: &Scene, duration: DateDuration) -> NewActivity<'static> {
        NewActivity {
            session_id: s.session,
            user_id: s.owner,
            instance_id: s.instance,
            message_id: s.invite,
            duration,
            place: "海边",
            content: "散步",
        }
    }

    async fn count(pool: &PgPool, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
    }

    async fn open(pool: &PgPool, s: &Scene, duration: DateDuration) -> ActivityRecord {
        ActivityRepo { pool }
            .accept(&invitation(s, duration), t(NOW))
            .await
            .unwrap()
            .expect("a date opened")
    }

    fn event(
        s: &Scene,
        key: &'static str,
        kind: EventKind,
        from: &str,
        to: &str,
    ) -> NewEvent<'static> {
        NewEvent {
            instance_id: s.instance,
            session_id: s.session,
            event_key: key,
            kind,
            started_at: t(from),
            ends_at: t(to),
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn accept_opens_a_sub_session_an_event_and_the_row(pool: PgPool) {
        let s = scene(&pool).await;
        let a = open(&pool, &s, DateDuration::Mid).await;

        let (channel, user_id, instance_id): (String, Uuid, Option<Uuid>) = sqlx::query_as(
            "SELECT channel, user_id, instance_id FROM engine.chat_sessions WHERE id = $1",
        )
        .bind(a.sub_session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(channel, "activity");
        assert_eq!(user_id, s.owner);
        assert_eq!(instance_id, Some(s.instance));
        assert_eq!(a.session_id, s.session);
        assert_eq!(a.message_id, Some(s.invite));
        assert_eq!((a.place.as_str(), a.content.as_str()), ("海边", "散步"));
        assert_eq!(a.duration(), DateDuration::Mid);

        let (kind, session_id, started, ends): (
            String,
            Option<Uuid>,
            DateTime<Utc>,
            DateTime<Utc>,
        ) = sqlx::query_as(
            "SELECT kind, session_id, started_at, ends_at FROM engine.persona_events \
                  WHERE id = $1",
        )
        .bind(a.event_id.expect("the event is linked"))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kind, "on_a_date");
        assert_eq!(session_id, Some(s.session), "told in the parent");
        assert_eq!(started, t(REPLY_AT), "from her reply");
        assert_eq!(ends, started + Duration::hours(4));
        assert_eq!(a.started_at, Some(started));
        assert_eq!(a.ends_at, Some(ends));

        let response: String = sqlx::query_scalar(
            "SELECT metadata -> 'action' ->> 'response' FROM engine.chat_messages WHERE id = $1",
        )
        .bind(s.invite)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(response, "accept");

        assert_eq!(
            a.status_at(started + Duration::hours(1)),
            ActivityStatus::Ongoing
        );
        assert_eq!(
            a.status_at(started + Duration::hours(4)),
            ActivityStatus::Ended
        );
        assert_eq!(a.settled_at, None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_short_date_lasts_an_hour(pool: PgPool) {
        let s = scene(&pool).await;
        let a = open(&pool, &s, DateDuration::Short).await;
        assert_eq!(a.duration(), DateDuration::Short);
        assert_eq!(a.ends_at, Some(t(REPLY_AT) + Duration::hours(1)));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn without_a_reply_row_the_date_starts_at_the_clock(pool: PgPool) {
        let s = scene(&pool).await;
        sqlx::query("DELETE FROM engine.chat_messages WHERE user_message_id = $1")
            .bind(s.invite)
            .execute(&pool)
            .await
            .unwrap();
        let a = open(&pool, &s, DateDuration::Short).await;
        assert_eq!(a.started_at, Some(t(NOW)));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn accept_is_idempotent_on_the_invitation(pool: PgPool) {
        let s = scene(&pool).await;
        let first = open(&pool, &s, DateDuration::Mid).await;
        let again = ActivityRepo { pool: &pool }
            .accept(
                &invitation(&s, DateDuration::Mid),
                t("2026-11-12T10:20:00Z"),
            )
            .await
            .unwrap()
            .expect("the date it already opened");
        assert_eq!(again, first);
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM engine.chat_sessions WHERE channel = 'activity'"
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM engine.persona_events WHERE kind = 'on_a_date'"
            )
            .await,
            1
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM engine.activities").await,
            1
        );
    }

    /// Two drivers of one turn at once open one date: the second waits for the
    /// instance's lock and then finds the first's row.
    #[sqlx::test(migrations = "./migrations")]
    async fn concurrent_accepts_of_one_invitation_open_one_date(pool: PgPool) {
        let s = scene(&pool).await;
        let repo = ActivityRepo { pool: &pool };
        let n = invitation(&s, DateDuration::Mid);
        let (x, y) = tokio::join!(repo.accept(&n, t(NOW)), repo.accept(&n, t(NOW)));
        let (x, y) = (x.unwrap().expect("opened"), y.unwrap().expect("opened"));
        assert_eq!(x.id, y.id);
        assert_eq!(x.sub_session_id, y.sub_session_id);
        assert_eq!(
            count(&pool, "SELECT count(*) FROM engine.activities").await,
            1
        );
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM engine.chat_sessions WHERE channel = 'activity'"
            )
            .await,
            1
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn accept_refuses_while_a_date_of_the_instance_is_on(pool: PgPool) {
        let s = scene(&pool).await;
        let elsewhere = ChatRepo { pool: &pool }
            .create_session(s.owner, s.instance)
            .await
            .unwrap()
            .id;
        EventRepo { pool: &pool }
            .insert_preempting(&NewEvent {
                instance_id: s.instance,
                session_id: elsewhere,
                event_key: "activity:another",
                kind: EventKind::OnADate,
                started_at: t("2026-11-12T09:30:00Z"),
                ends_at: t("2026-11-12T13:30:00Z"),
            })
            .await
            .unwrap();

        let refused = ActivityRepo { pool: &pool }
            .accept(&invitation(&s, DateDuration::Mid), t(NOW))
            .await
            .unwrap();
        assert!(refused.is_none(), "one date at a time");
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM engine.chat_sessions WHERE channel = 'activity'"
            )
            .await,
            0,
            "no sub-session written"
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM engine.activities").await,
            0
        );
        let response: String = sqlx::query_scalar(
            "SELECT metadata -> 'action' ->> 'response' FROM engine.chat_messages WHERE id = $1",
        )
        .bind(s.invite)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(response, "accept", "her reply stands");
    }

    /// A date's start is her reply's stored `sent_at`, which can be later than
    /// the caller's clock: a date yet to start still holds the instance.
    #[sqlx::test(migrations = "./migrations")]
    async fn accept_refuses_while_a_date_of_the_instance_is_about_to_start(pool: PgPool) {
        let s = scene(&pool).await;
        let elsewhere = ChatRepo { pool: &pool }
            .create_session(s.owner, s.instance)
            .await
            .unwrap()
            .id;
        EventRepo { pool: &pool }
            .insert_preempting(&NewEvent {
                instance_id: s.instance,
                session_id: elsewhere,
                event_key: "activity:another",
                kind: EventKind::OnADate,
                started_at: t(NOW) + Duration::seconds(1),
                ends_at: t(NOW) + Duration::hours(1),
            })
            .await
            .unwrap();

        let refused = ActivityRepo { pool: &pool }
            .accept(&invitation(&s, DateDuration::Short), t(NOW))
            .await
            .unwrap();
        assert!(refused.is_none(), "a date about to start is on");
        assert_eq!(
            count(&pool, "SELECT count(*) FROM engine.activities").await,
            0
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_date_that_has_ended_does_not_hold_the_next(pool: PgPool) {
        let s = scene(&pool).await;
        EventRepo { pool: &pool }
            .insert_preempting(&event(
                &s,
                "activity:old",
                EventKind::OnADate,
                "2026-11-12T08:00:00Z",
                "2026-11-12T09:00:00Z",
            ))
            .await
            .unwrap();
        assert!(ActivityRepo { pool: &pool }
            .accept(&invitation(&s, DateDuration::Short), t(NOW))
            .await
            .unwrap()
            .is_some());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn accept_ends_an_incident_but_keeps_a_background(pool: PgPool) {
        let s = scene(&pool).await;
        let events = EventRepo { pool: &pool };
        let trip = events
            .insert_preempting(&event(
                &s,
                "trip",
                EventKind::ShortTrip,
                "2026-11-11T10:00:00Z",
                "2026-11-13T10:00:00Z",
            ))
            .await
            .unwrap();
        let reading = events
            .insert_preempting(&event(
                &s,
                "reading",
                EventKind::Reading,
                "2026-11-12T09:30:00Z",
                "2026-11-12T12:30:00Z",
            ))
            .await
            .unwrap();

        open(&pool, &s, DateDuration::Mid).await;

        let ends = |id: Uuid| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, DateTime<Utc>>(
                    "SELECT ends_at FROM engine.persona_events WHERE id = $1",
                )
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        assert_eq!(
            ends(reading.id).await,
            t(REPLY_AT),
            "capped where the date starts"
        );
        assert_eq!(ends(trip.id).await, trip.ends_at, "a background carries on");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn readers_find_an_accepted_date(pool: PgPool) {
        let s = scene(&pool).await;
        let a = open(&pool, &s, DateDuration::Mid).await;
        let repo = ActivityRepo { pool: &pool };
        assert_eq!(repo.get(a.id).await.unwrap(), Some(a.clone()));
        assert_eq!(repo.get(Uuid::new_v4()).await.unwrap(), None);
        assert_eq!(
            repo.by_sub_session(a.sub_session_id).await.unwrap(),
            Some(a.clone())
        );
        assert_eq!(repo.by_sub_session(s.session).await.unwrap(), None);
        assert_eq!(
            repo.by_message_ids(&[Uuid::new_v4(), s.invite])
                .await
                .unwrap(),
            vec![a.clone()]
        );
        assert!(repo.by_message_ids(&[]).await.unwrap().is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unsettled_for_instance_skips_settled_and_archived(pool: PgPool) {
        let s = scene(&pool).await;
        let a = open(&pool, &s, DateDuration::Mid).await;
        let repo = ActivityRepo { pool: &pool };
        assert_eq!(
            repo.unsettled_for_instance(s.instance).await.unwrap(),
            Some(a.clone())
        );
        assert_eq!(
            repo.unsettled_for_instance(Uuid::new_v4()).await.unwrap(),
            None,
            "another instance has none"
        );

        sqlx::query("UPDATE engine.chat_sessions SET archived = true WHERE id = $1")
            .bind(s.session)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(repo.unsettled_for_instance(s.instance).await.unwrap(), None);
        sqlx::query("UPDATE engine.chat_sessions SET archived = false WHERE id = $1")
            .bind(s.session)
            .execute(&pool)
            .await
            .unwrap();

        let mut tx = pool.begin().await.unwrap();
        ActivityRepo::mark_settled_in_tx(&mut tx, a.id, t("2026-11-12T14:05:00Z"))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(repo.unsettled_for_instance(s.instance).await.unwrap(), None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_settlement_claims_once_then_holds_then_reports_settled(pool: PgPool) {
        let s = scene(&pool).await;
        let a = open(&pool, &s, DateDuration::Mid).await;
        let repo = ActivityRepo { pool: &pool };
        let stale = Duration::minutes(10);
        let t0 = t("2026-11-12T14:05:00Z");

        let SettleClaim::Claimed(claimed) = repo.claim_settlement(a.id, t0, stale).await.unwrap()
        else {
            panic!("the first caller claims");
        };
        assert_eq!(claimed.settle_claimed_at, Some(t0));
        assert_eq!(
            claimed.ends_at, a.ends_at,
            "the claim carries the event's span"
        );

        assert!(
            matches!(
                repo.claim_settlement(a.id, t0 + Duration::minutes(1), stale)
                    .await
                    .unwrap(),
                SettleClaim::Held
            ),
            "a live claim holds"
        );
        let t1 = t0 + Duration::minutes(11);
        let SettleClaim::Claimed(taken_over) =
            repo.claim_settlement(a.id, t1, stale).await.unwrap()
        else {
            panic!("a stale claim is taken over");
        };
        assert_eq!(taken_over.settle_claimed_at, Some(t1));

        let mut tx = pool.begin().await.unwrap();
        ActivityRepo::mark_settled_in_tx(&mut tx, a.id, t1 + Duration::minutes(1))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let SettleClaim::Settled(done) = repo
            .claim_settlement(a.id, t1 + Duration::hours(1), stale)
            .await
            .unwrap()
        else {
            panic!("a settled activity reports itself");
        };
        assert_eq!(done.settled_at, Some(t1 + Duration::minutes(1)));
        assert_eq!(
            done.status_at(t1 + Duration::hours(1)),
            ActivityStatus::Settled
        );
    }

    /// An invitation accepted after its relationship was archived opens no
    /// date; her answer is still stored on the invitation.
    #[sqlx::test(migrations = "./migrations")]
    async fn accept_on_an_archived_parent_opens_nothing(pool: PgPool) {
        let s = scene(&pool).await;
        sqlx::query("UPDATE engine.chat_sessions SET archived = true WHERE id = $1")
            .bind(s.session)
            .execute(&pool)
            .await
            .unwrap();

        let opened = ActivityRepo { pool: &pool }
            .accept(&invitation(&s, DateDuration::Short), t(NOW))
            .await
            .unwrap();
        assert_eq!(opened, None);
        assert_eq!(
            count(&pool, "SELECT count(*) FROM engine.activities").await,
            0
        );
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM engine.chat_sessions WHERE channel = 'activity'"
            )
            .await,
            0
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM engine.persona_events").await,
            0
        );
        let response: Option<String> = sqlx::query_scalar(
            "SELECT metadata->'action'->>'response' FROM engine.chat_messages WHERE id = $1",
        )
        .bind(s.invite)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(response.as_deref(), Some("accept"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn date_on_until_the_end_leaving_out_the_invitations_own(pool: PgPool) {
        let s = scene(&pool).await;
        let repo = ActivityRepo { pool: &pool };
        assert!(!repo.date_on(s.instance, None, t(NOW)).await.unwrap());

        let a = open(&pool, &s, DateDuration::Mid).await;
        assert!(repo.date_on(s.instance, None, t(NOW)).await.unwrap());
        assert!(
            repo.date_on(s.instance, Some(Uuid::new_v4()), t(NOW))
                .await
                .unwrap(),
            "another invitation sees it"
        );
        assert!(
            !repo
                .date_on(s.instance, Some(s.invite), t(NOW))
                .await
                .unwrap(),
            "the invitation that opened it does not"
        );
        let ends = a.ends_at.expect("the event's end");
        assert!(!repo.date_on(s.instance, None, ends).await.unwrap());
        assert!(!repo.date_on(Uuid::new_v4(), None, t(NOW)).await.unwrap());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn release_claim_gives_back_only_the_callers_own_claim(pool: PgPool) {
        let s = scene(&pool).await;
        let a = open(&pool, &s, DateDuration::Mid).await;
        let repo = ActivityRepo { pool: &pool };
        let stale = Duration::minutes(10);
        let t0 = t("2026-11-12T14:05:00Z");

        assert!(matches!(
            repo.claim_settlement(a.id, t0, stale).await.unwrap(),
            SettleClaim::Claimed(_)
        ));
        repo.release_claim(a.id, t0 - Duration::minutes(11))
            .await
            .unwrap();
        assert_eq!(
            repo.get(a.id).await.unwrap().unwrap().settle_claimed_at,
            Some(t0),
            "a claim taken over stays"
        );

        repo.release_claim(a.id, t0).await.unwrap();
        assert_eq!(
            repo.get(a.id).await.unwrap().unwrap().settle_claimed_at,
            None
        );
        assert!(
            matches!(
                repo.claim_settlement(a.id, t0 + Duration::minutes(1), stale)
                    .await
                    .unwrap(),
                SettleClaim::Claimed(_)
            ),
            "the next caller claims at once"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn end_early_moves_the_end_to_now(pool: PgPool) {
        let s = scene(&pool).await;
        let a = open(&pool, &s, DateDuration::Mid).await;
        let repo = ActivityRepo { pool: &pool };
        let now = t("2026-11-12T10:30:00Z");

        assert!(repo.end_early(a.id, now).await.unwrap());
        let after = repo.get(a.id).await.unwrap().unwrap();
        assert_eq!(after.ends_at, Some(now));
        assert_eq!(after.status_at(now), ActivityStatus::Ended);

        assert!(
            !repo
                .end_early(a.id, now + Duration::minutes(5))
                .await
                .unwrap(),
            "already ended"
        );
        assert_eq!(repo.get(a.id).await.unwrap().unwrap().ends_at, Some(now));
        assert!(!repo.end_early(Uuid::new_v4(), now).await.unwrap());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn returned_from_until_her_first_line(pool: PgPool) {
        let s = scene(&pool).await;
        let a = open(&pool, &s, DateDuration::Mid).await;
        let repo = ActivityRepo { pool: &pool };
        let end = t("2026-11-12T10:30:00Z");
        assert!(repo.end_early(a.id, end).await.unwrap());

        assert_eq!(
            repo.returned_from(s.session, end - Duration::minutes(1))
                .await
                .unwrap(),
            None,
            "not over yet"
        );
        let back = repo
            .returned_from(s.session, end + Duration::minutes(1))
            .await
            .unwrap()
            .expect("just back");
        assert_eq!(back.id, a.id);
        assert_eq!(
            repo.returned_from(s.owner, end + Duration::minutes(1))
                .await
                .unwrap(),
            None,
            "only the parent session"
        );
        assert_eq!(
            repo.returned_from(a.sub_session_id, end + Duration::minutes(1))
                .await
                .unwrap(),
            None
        );

        put(
            &pool,
            s.session,
            "assistant",
            "",
            "2026-11-12T10:31:30Z",
            None,
            None,
        )
        .await;
        assert!(
            repo.returned_from(s.session, end + Duration::minutes(2))
                .await
                .unwrap()
                .is_some(),
            "an empty row is not a line"
        );

        put(
            &pool,
            s.session,
            "assistant",
            "回来啦",
            "2026-11-12T10:32:00Z",
            None,
            None,
        )
        .await;
        assert_eq!(
            repo.returned_from(s.session, end + Duration::minutes(3))
                .await
                .unwrap(),
            None,
            "her first line ends it"
        );
        assert!(
            repo.returned_from(s.session, end + Duration::minutes(1))
                .await
                .unwrap()
                .is_some(),
            "as of a moment before that line it still stands"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn transcript_returns_the_newest_rows_and_the_count_left_out(pool: PgPool) {
        let s = scene(&pool).await;
        let a = open(&pool, &s, DateDuration::Mid).await;
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
             SELECT $1, CASE WHEN n % 2 = 0 THEN 'user' ELSE 'assistant' END, 'm' || n, \
                    '2026-11-12T10:01:00Z'::timestamptz + make_interval(secs => n) \
               FROM generate_series(1, 130) n",
        )
        .bind(a.sub_session_id)
        .execute(&pool)
        .await
        .unwrap();
        let repo = ActivityRepo { pool: &pool };

        let (rows, left_out) = repo.transcript(a.sub_session_id, 120).await.unwrap();
        assert_eq!(rows.len(), 120);
        assert_eq!(left_out, 10);
        assert_eq!(rows[0].content, "m11", "oldest of the newest 120");
        assert_eq!(rows[119].content, "m130");
        assert!(rows.windows(2).all(|w| w[0].sent_at < w[1].sent_at));

        let (all, none_left) = repo.transcript(a.sub_session_id, 200).await.unwrap();
        assert_eq!((all.len(), none_left), (130, 0));
        let (empty, zero) = repo.transcript(s.session, 0).await.unwrap();
        assert_eq!((empty.len(), zero), (0, 0));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn substantive_turns_counts_answered_user_rows(pool: PgPool) {
        let s = scene(&pool).await;
        let a = open(&pool, &s, DateDuration::Mid).await;
        let sub = a.sub_session_id;
        let at = |n: u32| format!("2026-11-12T10:{n:02}:00Z");

        let hi = put(&pool, sub, "user", "你好", &at(1), None, None).await;
        put(&pool, sub, "assistant", "你好呀", &at(2), Some(hi), None).await;

        // Unanswered.
        put(&pool, sub, "user", "还在吗", &at(3), None, None).await;

        // Empty user content, answered.
        let blank = put(&pool, sub, "user", "", &at(4), None, None).await;
        put(&pool, sub, "assistant", "嗯？", &at(5), Some(blank), None).await;

        // Answered by an empty reply only.
        let silent = put(&pool, sub, "user", "喂", &at(6), None, None).await;
        put(&pool, sub, "assistant", "", &at(7), Some(silent), None).await;

        // Two replies, one of them empty: still one turn.
        let two = put(&pool, sub, "user", "走吧", &at(8), None, None).await;
        put(&pool, sub, "assistant", "", &at(9), Some(two), None).await;
        put(&pool, sub, "assistant", "走", &at(10), Some(two), None).await;

        let repo = ActivityRepo { pool: &pool };
        assert_eq!(repo.substantive_turns(sub).await.unwrap(), 2);
        assert_eq!(
            repo.substantive_turns(s.session).await.unwrap(),
            1,
            "counted per session: the parent's invitation"
        );
    }
}
