// SPDX-License-Identifier: AGPL-3.0-only
//! Choices the persona puts to the user, one row each in
//! `engine.life_choices` (spec 2026-10-19-life-events-shopping-gatherings-and-
//! ask-for-space-design.md §3.6).

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::chat::ProactiveInsert;
use crate::events::lock_instance;

/// How long a choice waits for an answer; the only place it is written down.
/// A choice never stores its expiry: it is `asked_at` plus this.
const PENDING_HOURS: i32 = 24;

/// How long after an accepted separation ends her growth stays slowed
/// (spec 2026-10-19 §3.5); the only place it is written down. The window is
/// derived: `ends_at` of the separation the accepted choice wrote, plus this.
const SLOWDOWN_HOURS: i32 = 72;

/// The kind of the one choice there is.
pub(crate) const ASK_FOR_SPACE: &str = "ask_for_space";

/// A choice's columns, in `LifeChoiceRecord` order.
const COLUMNS: &str = "id, session_id, kind, status, tier_gap, asked_at, resolved_at, \
                       message_id, event_id";

/// One choice. `message_id` is the message that asked it, `event_id` the
/// separation an accept wrote; either is `None` once its row is gone, and
/// `event_id` on every path but accept.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct LifeChoiceRecord {
    pub id: Uuid,
    pub session_id: Uuid,
    pub kind: String,
    /// `pending`, `accepted`, `declined` or `ignored`.
    pub status: String,
    /// The tier gap at ask time.
    pub tier_gap: i16,
    pub asked_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub message_id: Option<Uuid>,
    pub event_id: Option<Uuid>,
}

impl LifeChoiceRecord {
    /// When the choice lapses: derived, never stored.
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.asked_at + Duration::hours(i64::from(PENDING_HOURS))
    }

    /// Unanswered and not yet lapsed at `now`. A lapsed choice keeps status
    /// `pending` in the table and reads as expired everywhere.
    pub fn is_pending(&self, now: DateTime<Utc>) -> bool {
        self.status == "pending" && now < self.expires_at()
    }

    /// How the choice stands at `now` (spec 2026-10-30 §3): the stored
    /// status, except that a lapsed `pending` reads `expired`.
    pub fn status_at(&self, now: DateTime<Utc>) -> &str {
        if self.status == "pending" && now >= self.expires_at() {
            "expired"
        } else {
            &self.status
        }
    }
}

pub struct LifeChoiceRepo<'a> {
    pub pool: &'a PgPool,
}

impl LifeChoiceRepo<'_> {
    /// Write the message that asks and the choice it asks, in one
    /// transaction. `None` (nothing written, the message with it) while any
    /// session of the instance holds a pending choice that has not lapsed, or
    /// when the session's streak anchor is no longer `anchor_seen`, the one
    /// the trigger judged by before generation ran: another ask landed or was
    /// answered meanwhile, and the streak it counted is stale. Both are read
    /// under the instance's lock, so asks and event writes of one instance
    /// queue here. `asked_at` is the message's `sent_at`.
    pub async fn insert_ask(
        &self,
        session_id: Uuid,
        tier_gap: i16,
        anchor_seen: Option<DateTime<Utc>>,
        message: &ProactiveInsert,
    ) -> Result<Option<LifeChoiceRecord>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // The lock's key is the session's instance, a column that never
        // changes, so reading it ahead of the lock is safe.
        let instance_id: Uuid = sqlx::query_scalar(
            "SELECT instance_id FROM engine.chat_sessions \
              WHERE id = $1 AND instance_id IS NOT NULL",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;
        lock_instance(&mut tx, instance_id).await?;
        sqlx::query("UPDATE engine.chat_sessions SET last_active_at = now() WHERE id = $1")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        if pending_for_instance_on(&mut *tx, instance_id, Utc::now()).await?
            || streak_anchor_on(&mut *tx, session_id).await? != anchor_seen
        {
            // Dropped uncommitted.
            return Ok(None);
        }
        let sent_at: DateTime<Utc> = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages \
               (id, session_id, role, content, generation_id, assistant_action_type, \
                metadata, llm_attempts, gateway_errors) \
             VALUES ($1, $2, 'assistant', $3, $4, 'proactive', $5, $6, $7) \
             RETURNING sent_at",
        )
        .bind(message.id)
        .bind(session_id)
        .bind(&message.content)
        .bind(&message.generation_id)
        .bind(&message.metadata)
        .bind(&message.llm_attempts)
        .bind(&message.gateway_errors)
        .fetch_one(&mut *tx)
        .await?;
        let choice = sqlx::query_as::<_, LifeChoiceRecord>(&format!(
            "INSERT INTO engine.life_choices \
               (session_id, kind, tier_gap, asked_at, message_id) \
             VALUES ($1, $2, $3, $4, $5) \
             RETURNING {COLUMNS}"
        ))
        .bind(session_id)
        .bind(ASK_FOR_SPACE)
        .bind(tier_gap)
        .bind(sent_at)
        .bind(message.id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(choice))
    }

    /// The choices asked by any of `message_ids`, in no particular order
    /// (spec 2026-10-30 §3): one lookup per history page.
    pub async fn by_message_ids(
        &self,
        message_ids: &[Uuid],
    ) -> Result<Vec<LifeChoiceRecord>, sqlx::Error> {
        if message_ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM engine.life_choices WHERE message_id = ANY($1)"
        ))
        .bind(message_ids)
        .fetch_all(self.pool)
        .await
    }

    /// One choice by id, in whatever state it is in.
    pub async fn get(&self, choice_id: Uuid) -> Result<Option<LifeChoiceRecord>, sqlx::Error> {
        sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM engine.life_choices WHERE id = $1"
        ))
        .bind(choice_id)
        .fetch_optional(self.pool)
        .await
    }

    /// The session's unanswered choice that has not lapsed at `now`.
    pub async fn pending(
        &self,
        session_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<LifeChoiceRecord>, sqlx::Error> {
        sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM engine.life_choices \
              WHERE session_id = $1 AND status = 'pending' \
                AND asked_at > $2 - make_interval(hours => $3) \
              ORDER BY asked_at DESC LIMIT 1"
        ))
        .bind(session_id)
        .bind(now)
        .bind(PENDING_HOURS)
        .fetch_optional(self.pool)
        .await
    }

    /// Whether any session of the instance holds an unanswered choice that
    /// has not lapsed at `now`.
    pub async fn pending_for_instance(
        &self,
        instance_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<bool, sqlx::Error> {
        pending_for_instance_on(self.pool, instance_id, now).await
    }

    /// Answer the choice: `status` and `now` as its resolution. `None` when
    /// it is no longer pending or has lapsed at `now` — the answer that lost
    /// the race, or came too late. The first one wins. See
    /// [`Self::resolve_in_tx`].
    pub async fn resolve(
        &self,
        choice_id: Uuid,
        status: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<LifeChoiceRecord>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let resolved = Self::resolve_in_tx(&mut tx, choice_id, status, now).await?;
        tx.commit().await?;
        Ok(resolved)
    }

    /// [`Self::resolve`] inside the caller's transaction, so the resolution
    /// commits or rolls back together with what it is paired with (the
    /// falling-out, the separation, the tier drop); nothing here commits. A
    /// caller that gets `None` has lost and drops the transaction: nothing of
    /// the rest of its work may land. A concurrent resolve of the same row
    /// waits on its lock here and then finds the choice no longer pending.
    pub async fn resolve_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        choice_id: Uuid,
        status: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<LifeChoiceRecord>, sqlx::Error> {
        sqlx::query_as(&format!(
            "UPDATE engine.life_choices SET status = $2, resolved_at = $3 \
              WHERE id = $1 AND status = 'pending' \
                AND asked_at > $3 - make_interval(hours => $4) \
              RETURNING {COLUMNS}"
        ))
        .bind(choice_id)
        .bind(status)
        .bind(now)
        .bind(PENDING_HOURS)
        .fetch_optional(&mut **tx)
        .await
    }

    /// Where the session's next streak of judged turns starts: the latest of
    /// its newest choice's `asked_at`, its `resolved_at` and, for an accepted
    /// one, the end of the separation it wrote (`persona_events.ends_at`,
    /// through `event_id`). Turns up to that instant belong to the episode,
    /// not to the streak. `None` when the session was never asked; a choice
    /// whose separation row is gone anchors at its answer.
    pub async fn streak_anchor(
        &self,
        session_id: Uuid,
    ) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
        streak_anchor_on(self.pool, session_id).await
    }

    /// Whether `now` falls in the slowdown window of the session's newest
    /// accepted choice: from the end of its separation (`persona_events.
    /// ends_at`, through `event_id`) for 72 hours. Older accepted choices and
    /// every other session of the instance have no say, and a choice whose
    /// separation row is gone carries no window.
    pub async fn growth_slowed(
        &self,
        session_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS ( \
               SELECT 1 \
                 FROM (SELECT event_id FROM engine.life_choices \
                        WHERE session_id = $1 AND status = 'accepted' \
                        ORDER BY asked_at DESC LIMIT 1) c \
                 JOIN engine.persona_events e ON e.id = c.event_id \
                WHERE e.ends_at <= $2 \
                  AND $2 < e.ends_at + make_interval(hours => $3))",
        )
        .bind(session_id)
        .bind(now)
        .bind(SLOWDOWN_HOURS)
        .fetch_one(self.pool)
        .await
    }

    /// Record the separation an accept wrote. See [`Self::set_event_in_tx`].
    pub async fn set_event(&self, choice_id: Uuid, event_id: Uuid) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        Self::set_event_in_tx(&mut tx, choice_id, event_id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// [`Self::set_event`] inside the caller's transaction, so the link
    /// commits or rolls back together with the separation it names and the
    /// resolution it belongs to; nothing here commits.
    pub async fn set_event_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        choice_id: Uuid,
        event_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE engine.life_choices SET event_id = $2 WHERE id = $1")
            .bind(choice_id)
            .bind(event_id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
}

/// [`LifeChoiceRepo::pending_for_instance`] on any executor, so the write
/// that must refuse while she waits asks inside its own transaction.
pub(crate) async fn pending_for_instance_on<'e, E: sqlx::PgExecutor<'e>>(
    ex: E,
    instance_id: Uuid,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM engine.life_choices c \
                          JOIN engine.chat_sessions s ON s.id = c.session_id \
                         WHERE s.instance_id = $1 AND c.status = 'pending' \
                           AND c.asked_at > $2 - make_interval(hours => $3))",
    )
    .bind(instance_id)
    .bind(now)
    .bind(PENDING_HOURS)
    .fetch_one(ex)
    .await
}

/// [`LifeChoiceRepo::streak_anchor`] on any executor.
async fn streak_anchor_on<'e, E: sqlx::PgExecutor<'e>>(
    ex: E,
    session_id: Uuid,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT GREATEST(c.asked_at, c.resolved_at, e.ends_at) \
           FROM engine.life_choices c \
           LEFT JOIN engine.persona_events e ON e.id = c.event_id \
          WHERE c.session_id = $1 \
          ORDER BY c.asked_at DESC LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(ex)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ChatRepo;
    use crate::testutil::seed_persona_instance;
    use serde_json::json;

    /// An instance and one text session of it; returns (owner, instance, session).
    async fn instance_and_session(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
        let owner = Uuid::new_v4();
        let instance = seed_persona_instance(pool, owner).await;
        let s = ChatRepo { pool }
            .create_session(owner, instance)
            .await
            .unwrap();
        (owner, instance, s.id)
    }

    fn ask_message(content: &str) -> ProactiveInsert {
        ProactiveInsert {
            id: Uuid::new_v4(),
            content: content.into(),
            generation_id: None,
            metadata: json!({"proactive": "life_choice"}),
            llm_attempts: None,
            gateway_errors: None,
        }
    }

    async fn message_count(pool: &PgPool, id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM engine.chat_messages WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn record(status: &str, asked_ago: Duration) -> LifeChoiceRecord {
        let asked_at = Utc::now() - asked_ago;
        LifeChoiceRecord {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            kind: "ask_for_space".into(),
            status: status.into(),
            tier_gap: 3,
            asked_at,
            resolved_at: (status != "pending").then(|| asked_at + Duration::minutes(1)),
            message_id: None,
            event_id: None,
        }
    }

    #[test]
    fn status_at_reads_a_lapsed_pending_as_expired() {
        let now = Utc::now();
        assert_eq!(
            record("pending", Duration::hours(25)).status_at(now),
            "expired"
        );
        assert_eq!(
            record("pending", Duration::hours(1)).status_at(now),
            "pending"
        );
        assert_eq!(
            record("accepted", Duration::hours(25)).status_at(now),
            "accepted"
        );
        assert_eq!(
            record("declined", Duration::hours(1)).status_at(now),
            "declined"
        );
        assert_eq!(
            record("ignored", Duration::hours(25)).status_at(now),
            "ignored"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn by_message_ids_returns_only_the_asked_rows(pool: PgPool) {
        let (owner, instance, s1) = instance_and_session(&pool).await;
        let s2 = ChatRepo { pool: &pool }
            .create_session(owner, instance)
            .await
            .unwrap()
            .id;
        let repo = LifeChoiceRepo { pool: &pool };
        let m1 = ask_message("one");
        let c1 = repo.insert_ask(s1, 3, None, &m1).await.unwrap().unwrap();
        repo.resolve(c1.id, "declined", Utc::now()).await.unwrap();
        let m2 = ask_message("two");
        repo.insert_ask(s2, 3, None, &m2).await.unwrap().unwrap();
        sqlx::query(
            "INSERT INTO engine.life_choices (session_id, kind, status, tier_gap, asked_at) \
             VALUES ($1, 'ask_for_space', 'ignored', 3, now())",
        )
        .bind(s1)
        .execute(&pool)
        .await
        .unwrap();

        let got = repo.by_message_ids(&[m1.id, Uuid::new_v4()]).await.unwrap();
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].id, c1.id);
        assert_eq!(got[0].message_id, Some(m1.id));
        assert!(repo.by_message_ids(&[]).await.unwrap().is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn insert_ask_writes_message_and_choice_atomically(pool: PgPool) {
        let (_, _, session) = instance_and_session(&pool).await;
        let m = ask_message("我想先分开一阵子");
        let choice = LifeChoiceRepo { pool: &pool }
            .insert_ask(session, 3, None, &m)
            .await
            .unwrap()
            .expect("written");
        assert_eq!(choice.session_id, session);
        assert_eq!(choice.kind, "ask_for_space");
        assert_eq!(choice.status, "pending");
        assert_eq!(choice.tier_gap, 3);
        assert_eq!(choice.resolved_at, None);
        assert_eq!(choice.message_id, Some(m.id));
        assert_eq!(choice.event_id, None);

        let (role, action, content, proactive, sent_at): (
            String,
            Option<String>,
            String,
            Option<String>,
            DateTime<Utc>,
        ) = sqlx::query_as(
            "SELECT role, assistant_action_type, content, metadata->>'proactive', sent_at \
               FROM engine.chat_messages WHERE id = $1",
        )
        .bind(m.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (role.as_str(), action.as_deref(), content.as_str()),
            ("assistant", Some("proactive"), "我想先分开一阵子")
        );
        assert_eq!(proactive.as_deref(), Some("life_choice"));
        assert_eq!(choice.asked_at, sent_at, "asked when the message was sent");
        assert_eq!(
            choice.expires_at(),
            sent_at + Duration::hours(24),
            "expiry is derived, not stored"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_second_pending_ask_is_refused(pool: PgPool) {
        let (_, _, session) = instance_and_session(&pool).await;
        let repo = LifeChoiceRepo { pool: &pool };
        let first = repo
            .insert_ask(session, 2, None, &ask_message("第一次"))
            .await
            .unwrap()
            .expect("written");
        let second = ask_message("第二次");
        let anchor = repo.streak_anchor(session).await.unwrap();
        assert!(repo
            .insert_ask(session, 4, anchor, &second)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            message_count(&pool, second.id).await,
            0,
            "the refused ask leaves no message"
        );
        assert_eq!(
            repo.pending(session, first.asked_at).await.unwrap(),
            Some(first.clone()),
            "the first is untouched"
        );

        // Once the first has lapsed (still 'pending' in the table), a new ask is allowed.
        sqlx::query(
            "UPDATE engine.life_choices SET asked_at = asked_at - interval '25 hours' \
              WHERE id = $1",
        )
        .bind(first.id)
        .execute(&pool)
        .await
        .unwrap();
        let third = ask_message("过了一天再问");
        let anchor = repo.streak_anchor(session).await.unwrap();
        assert!(repo
            .insert_ask(session, 4, anchor, &third)
            .await
            .unwrap()
            .is_some());
        // Not so once it was answered, either: a resolved choice does not block.
        let latest = repo
            .pending(session, Utc::now())
            .await
            .unwrap()
            .expect("the new ask");
        repo.resolve(latest.id, "declined", latest.asked_at + Duration::hours(1))
            .await
            .unwrap()
            .expect("resolved");
        let anchor = repo.streak_anchor(session).await.unwrap();
        assert!(repo
            .insert_ask(session, 2, anchor, &ask_message("再问"))
            .await
            .unwrap()
            .is_some());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn resolve_wins_once_and_respects_expiry(pool: PgPool) {
        let (_, _, session) = instance_and_session(&pool).await;
        let repo = LifeChoiceRepo { pool: &pool };
        let choice = repo
            .insert_ask(session, 3, None, &ask_message("想先分开"))
            .await
            .unwrap()
            .expect("written");
        let at = choice.asked_at + Duration::hours(2);
        let won = repo
            .resolve(choice.id, "accepted", at)
            .await
            .unwrap()
            .expect("first resolve wins");
        assert_eq!(
            (won.status.as_str(), won.resolved_at),
            ("accepted", Some(at))
        );
        assert!(
            repo.resolve(choice.id, "declined", at)
                .await
                .unwrap()
                .is_none(),
            "the loser of the race gets nothing"
        );
        let status: String =
            sqlx::query_scalar("SELECT status FROM engine.life_choices WHERE id = $1")
                .bind(choice.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "accepted");

        // A choice that lapsed cannot be resolved, whatever its status says.
        sqlx::query("DELETE FROM engine.life_choices WHERE id = $1")
            .bind(choice.id)
            .execute(&pool)
            .await
            .unwrap();
        let lapsing = repo
            .insert_ask(session, 3, None, &ask_message("再想想"))
            .await
            .unwrap()
            .expect("written");
        let expiry = lapsing.expires_at();
        let micro = Duration::microseconds(1);
        assert!(repo
            .resolve(lapsing.id, "accepted", expiry + micro)
            .await
            .unwrap()
            .is_none());
        assert!(
            repo.resolve(lapsing.id, "accepted", expiry)
                .await
                .unwrap()
                .is_none(),
            "expires_at itself is already too late"
        );
        assert!(repo
            .resolve(lapsing.id, "ignored", expiry - micro)
            .await
            .unwrap()
            .is_some());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn pending_hides_an_expired_choice(pool: PgPool) {
        let (_, _, session) = instance_and_session(&pool).await;
        let repo = LifeChoiceRepo { pool: &pool };
        let choice = repo
            .insert_ask(session, 2, None, &ask_message("想先分开"))
            .await
            .unwrap()
            .expect("written");
        let expiry = choice.expires_at();
        let micro = Duration::microseconds(1);
        assert_eq!(
            repo.pending(session, choice.asked_at).await.unwrap(),
            Some(choice.clone())
        );
        assert!(choice.is_pending(expiry - micro));
        assert!(!choice.is_pending(expiry));
        assert_eq!(
            repo.pending(session, expiry - micro).await.unwrap(),
            Some(choice.clone())
        );
        assert_eq!(repo.pending(session, expiry).await.unwrap(), None);
        assert_eq!(repo.pending(session, expiry + micro).await.unwrap(), None);

        // Answered: no longer pending, though inside the window.
        let answered = repo
            .resolve(choice.id, "declined", choice.asked_at + Duration::hours(1))
            .await
            .unwrap()
            .expect("resolved");
        assert!(!answered.is_pending(choice.asked_at + Duration::hours(2)));
        assert_eq!(
            repo.pending(session, choice.asked_at + Duration::hours(2))
                .await
                .unwrap(),
            None
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn pending_for_instance_sees_a_sibling_session(pool: PgPool) {
        let (owner, instance, asked_in) = instance_and_session(&pool).await;
        let sibling = ChatRepo { pool: &pool }
            .create_session(owner, instance)
            .await
            .unwrap()
            .id;
        let (_, other_instance, _) = instance_and_session(&pool).await;
        let repo = LifeChoiceRepo { pool: &pool };
        let choice = repo
            .insert_ask(asked_in, 3, None, &ask_message("想先分开"))
            .await
            .unwrap()
            .expect("written");
        let inside = choice.asked_at + Duration::hours(1);
        assert!(repo.pending_for_instance(instance, inside).await.unwrap());
        assert_eq!(repo.pending(sibling, inside).await.unwrap(), None);
        assert!(!repo
            .pending_for_instance(other_instance, inside)
            .await
            .unwrap());
        assert!(!repo
            .pending_for_instance(instance, choice.expires_at())
            .await
            .unwrap());
        repo.resolve(choice.id, "ignored", inside)
            .await
            .unwrap()
            .expect("resolved");
        assert!(!repo.pending_for_instance(instance, inside).await.unwrap());
    }

    /// She has one life: two asks racing from sibling sessions (two detached
    /// post-process tasks, each past its own pre-generation check) land one.
    #[sqlx::test(migrations = "./migrations")]
    async fn concurrent_asks_from_sibling_sessions_land_one(pool: PgPool) {
        let (owner, instance, first) = instance_and_session(&pool).await;
        let second = ChatRepo { pool: &pool }
            .create_session(owner, instance)
            .await
            .unwrap()
            .id;
        let (m1, m2) = (ask_message("想先分开"), ask_message("我想静一静"));
        let ask = |session, message: ProactiveInsert| {
            let pool = pool.clone();
            async move {
                LifeChoiceRepo { pool: &pool }
                    .insert_ask(session, 3, None, &message)
                    .await
                    .unwrap()
            }
        };
        let (a, b) = tokio::join!(ask(first, m1.clone()), ask(second, m2.clone()));
        assert_eq!(
            [a.is_some(), b.is_some()].iter().filter(|w| **w).count(),
            1,
            "exactly one ask lands"
        );
        let (winner, loser) = if a.is_some() { (&m1, &m2) } else { (&m2, &m1) };
        assert_eq!(message_count(&pool, winner.id).await, 1);
        assert_eq!(
            message_count(&pool, loser.id).await,
            0,
            "the loser leaves no message"
        );
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM engine.life_choices")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }

    /// An ask in flight (written, not committed) is invisible to another
    /// ask's check. The second one waits for the instance's lock instead,
    /// then reads the committed ask and lands nothing.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_ask_waits_for_an_ask_in_flight_and_then_yields_to_it(pool: PgPool) {
        let (owner, instance, first) = instance_and_session(&pool).await;
        let second = ChatRepo { pool: &pool }
            .create_session(owner, instance)
            .await
            .unwrap()
            .id;
        let landing_message = ask_message("想先分开");
        let mut landing = pool.begin().await.unwrap();
        lock_instance(&mut landing, instance).await.unwrap();
        sqlx::query(
            "INSERT INTO engine.chat_messages (id, session_id, role, content, assistant_action_type) \
             VALUES ($1, $2, 'assistant', $3, 'proactive')",
        )
        .bind(landing_message.id)
        .bind(first)
        .bind(&landing_message.content)
        .execute(&mut *landing)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.life_choices (session_id, kind, tier_gap, asked_at, message_id) \
             VALUES ($1, 'ask_for_space', 3, now(), $2)",
        )
        .bind(first)
        .bind(landing_message.id)
        .execute(&mut *landing)
        .await
        .unwrap();

        let waiting_message = ask_message("我想静一静");
        let waiting = tokio::spawn({
            let (pool, message) = (pool.clone(), waiting_message.clone());
            async move {
                LifeChoiceRepo { pool: &pool }
                    .insert_ask(second, 3, None, &message)
                    .await
                    .unwrap()
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!waiting.is_finished(), "the second ask waits for the lock");
        landing.commit().await.unwrap();

        assert!(
            waiting.await.unwrap().is_none(),
            "and then sees the first one pending"
        );
        assert_eq!(message_count(&pool, landing_message.id).await, 1);
        assert_eq!(message_count(&pool, waiting_message.id).await, 0);
    }

    /// The streak was judged on an anchor the trigger read before generation
    /// ran. An ask that landed and was answered meanwhile moved it: the stale
    /// write is refused, the current one lands.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_ask_on_a_stale_streak_anchor_is_refused(pool: PgPool) {
        let (_, _, session) = instance_and_session(&pool).await;
        let repo = LifeChoiceRepo { pool: &pool };
        let seen = repo.streak_anchor(session).await.unwrap();
        assert_eq!(seen, None, "never asked when the trigger read it");

        // Meanwhile another ask landed and was answered.
        let earlier = repo
            .insert_ask(session, 3, seen, &ask_message("想先分开"))
            .await
            .unwrap()
            .expect("written");
        repo.resolve(
            earlier.id,
            "declined",
            earlier.asked_at + Duration::hours(1),
        )
        .await
        .unwrap()
        .expect("resolved");
        let moved = repo.streak_anchor(session).await.unwrap();
        assert_ne!(moved, seen);

        let stale = ask_message("又想分开");
        assert!(repo
            .insert_ask(session, 3, seen, &stale)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            message_count(&pool, stale.id).await,
            0,
            "the stale ask leaves no message"
        );
        assert!(repo
            .insert_ask(session, 3, moved, &ask_message("又想分开"))
            .await
            .unwrap()
            .is_some());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn set_event_records_the_separation_the_choice_wrote(pool: PgPool) {
        use crate::events::{EventRepo, NewEvent};
        use eros_engine_core::events::EventKind;

        let (_, instance, session) = instance_and_session(&pool).await;
        let repo = LifeChoiceRepo { pool: &pool };
        let choice = repo
            .insert_ask(session, 3, None, &ask_message("想先分开"))
            .await
            .unwrap()
            .expect("written");
        let started = choice.asked_at + Duration::hours(1);
        let event = EventRepo { pool: &pool }
            .insert_preempting(&NewEvent {
                instance_id: instance,
                session_id: session,
                event_key: "taking_space",
                kind: EventKind::TakingSpace,
                started_at: started,
                ends_at: started + Duration::hours(18),
            })
            .await
            .unwrap();
        repo.set_event(choice.id, event.id).await.unwrap();
        let event_id: Option<Uuid> =
            sqlx::query_scalar("SELECT event_id FROM engine.life_choices WHERE id = $1")
                .bind(choice.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(event_id, Some(event.id));

        // Deleting the event or the message keeps the choice: the thread outlives both.
        sqlx::query("DELETE FROM engine.persona_events WHERE id = $1")
            .bind(event.id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM engine.chat_messages WHERE id = $1")
            .bind(choice.message_id)
            .execute(&pool)
            .await
            .unwrap();
        let (message_id, event_id): (Option<Uuid>, Option<Uuid>) =
            sqlx::query_as("SELECT message_id, event_id FROM engine.life_choices WHERE id = $1")
                .bind(choice.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((message_id, event_id), (None, None));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn streak_anchor_is_the_latest_of_the_ask_its_answer_and_the_separation_end(
        pool: PgPool,
    ) {
        use crate::events::{EventRepo, NewEvent};
        use eros_engine_core::events::EventKind;

        let (owner, instance, session) = instance_and_session(&pool).await;
        let sibling = ChatRepo { pool: &pool }
            .create_session(owner, instance)
            .await
            .unwrap()
            .id;
        let repo = LifeChoiceRepo { pool: &pool };
        assert_eq!(
            repo.streak_anchor(session).await.unwrap(),
            None,
            "never asked"
        );

        // Asked and waiting: the ask itself.
        let choice = repo
            .insert_ask(session, 3, None, &ask_message("想先分开"))
            .await
            .unwrap()
            .expect("written");
        assert_eq!(
            repo.streak_anchor(session).await.unwrap(),
            Some(choice.asked_at)
        );

        // Answered: the answer, not the ask.
        let answered_at = choice.asked_at + Duration::hours(1);
        repo.resolve(choice.id, "accepted", answered_at)
            .await
            .unwrap()
            .expect("resolved");
        assert_eq!(
            repo.streak_anchor(session).await.unwrap(),
            Some(answered_at)
        );

        // Accepted with a separation: where the separation ends.
        let ends_at = answered_at + Duration::hours(18);
        let event = EventRepo { pool: &pool }
            .insert_preempting(&NewEvent {
                instance_id: instance,
                session_id: session,
                event_key: "taking_space",
                kind: EventKind::TakingSpace,
                started_at: answered_at,
                ends_at,
            })
            .await
            .unwrap();
        repo.set_event(choice.id, event.id).await.unwrap();
        assert_eq!(repo.streak_anchor(session).await.unwrap(), Some(ends_at));

        // The separation row gone: back to the answer.
        sqlx::query("DELETE FROM engine.persona_events WHERE id = $1")
            .bind(event.id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            repo.streak_anchor(session).await.unwrap(),
            Some(answered_at)
        );

        // The newest choice speaks, and a sibling session's episode has no say.
        let anchor = repo.streak_anchor(session).await.unwrap();
        let later = repo
            .insert_ask(session, 2, anchor, &ask_message("又来了"))
            .await
            .unwrap()
            .expect("written");
        assert_eq!(
            repo.streak_anchor(session).await.unwrap(),
            Some(later.asked_at)
        );
        assert_eq!(repo.streak_anchor(sibling).await.unwrap(), None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn get_reads_a_choice_by_id_whatever_its_state(pool: PgPool) {
        let (_, _, session) = instance_and_session(&pool).await;
        let repo = LifeChoiceRepo { pool: &pool };
        assert_eq!(repo.get(Uuid::new_v4()).await.unwrap(), None);
        let choice = repo
            .insert_ask(session, 3, None, &ask_message("想先分开"))
            .await
            .unwrap()
            .expect("written");
        assert_eq!(repo.get(choice.id).await.unwrap(), Some(choice.clone()));

        let answered = repo
            .resolve(choice.id, "declined", choice.asked_at + Duration::hours(1))
            .await
            .unwrap()
            .expect("resolved");
        assert_eq!(
            repo.get(choice.id).await.unwrap(),
            Some(answered),
            "an answered choice reads too"
        );
    }

    /// The accept's shape: resolve, the separation, and the link between them
    /// in the caller's one transaction.
    #[sqlx::test(migrations = "./migrations")]
    async fn set_event_in_tx_commits_or_rolls_back_with_the_transaction(pool: PgPool) {
        use crate::events::{EventRepo, NewEvent};
        use eros_engine_core::events::EventKind;

        let (_, instance, session) = instance_and_session(&pool).await;
        let repo = LifeChoiceRepo { pool: &pool };
        let choice = repo
            .insert_ask(session, 3, None, &ask_message("想先分开"))
            .await
            .unwrap()
            .expect("written");
        let at = choice.asked_at + Duration::hours(1);
        let choice_id = choice.id;
        let linked = |commit: bool| {
            let pool = pool.clone();
            async move {
                let mut tx = pool.begin().await.unwrap();
                let event = EventRepo::insert_preempting_in_tx(
                    &mut tx,
                    &NewEvent {
                        instance_id: instance,
                        session_id: session,
                        event_key: "taking_space",
                        kind: EventKind::TakingSpace,
                        started_at: at,
                        ends_at: at + Duration::hours(18),
                    },
                )
                .await
                .unwrap();
                LifeChoiceRepo::set_event_in_tx(&mut tx, choice_id, event.id)
                    .await
                    .unwrap();
                if commit {
                    tx.commit().await.unwrap();
                }
                event.id
            }
        };
        let event_id = |pool: PgPool| async move {
            sqlx::query_scalar::<_, Option<Uuid>>(
                "SELECT event_id FROM engine.life_choices WHERE id = $1",
            )
            .bind(choice_id)
            .fetch_one(&pool)
            .await
            .unwrap()
        };

        linked(false).await;
        assert_eq!(event_id(pool.clone()).await, None, "dropped: no link");
        let written = linked(true).await;
        assert_eq!(event_id(pool.clone()).await, Some(written));
    }

    /// The ignore conversion's shape: resolve first, the falling-out second,
    /// in the caller's one transaction. `None` when the resolve lost; the
    /// transaction is dropped uncommitted then, as on any error.
    async fn convert_in_tx(
        pool: &PgPool,
        choice: &LifeChoiceRecord,
        instance: Uuid,
        started_at: DateTime<Utc>,
        commit: bool,
    ) -> Result<Option<crate::events::EventRecord>, sqlx::Error> {
        use crate::events::{EventRepo, NewEvent};
        use eros_engine_core::events::EventKind;

        let mut tx = pool.begin().await?;
        let Some(resolved) =
            LifeChoiceRepo::resolve_in_tx(&mut tx, choice.id, "ignored", started_at).await?
        else {
            return Ok(None);
        };
        let key = format!("life_choice:{}:fell_out", resolved.id);
        let event = EventRepo::insert_preempting_in_tx(
            &mut tx,
            &NewEvent {
                instance_id: instance,
                session_id: resolved.session_id,
                event_key: &key,
                kind: EventKind::FellOut,
                started_at,
                ends_at: started_at + Duration::hours(1),
            },
        )
        .await?;
        if commit {
            tx.commit().await?;
        }
        Ok(Some(event))
    }

    async fn status_of(pool: &PgPool, choice: Uuid) -> (String, Option<DateTime<Utc>>) {
        sqlx::query_as("SELECT status, resolved_at FROM engine.life_choices WHERE id = $1")
            .bind(choice)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn event_kinds(pool: &PgPool, instance: Uuid) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT kind FROM engine.persona_events WHERE instance_id = $1 ORDER BY started_at",
        )
        .bind(instance)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_resolution_and_the_event_commit_in_one_transaction(pool: PgPool) {
        use crate::events::{EventRepo, NewEvent};
        use eros_engine_core::events::EventKind;

        let (_, instance, session) = instance_and_session(&pool).await;
        let repo = LifeChoiceRepo { pool: &pool };
        let choice = repo
            .insert_ask(session, 3, None, &ask_message("想先分开"))
            .await
            .unwrap()
            .expect("written");
        // An incident under way when the conversion lands.
        let at = choice.asked_at + Duration::hours(2);
        let running = EventRepo { pool: &pool }
            .insert_preempting(&NewEvent {
                instance_id: instance,
                session_id: session,
                event_key: "running",
                kind: EventKind::Quarrel,
                started_at: at - Duration::minutes(20),
                ends_at: at + Duration::minutes(40),
            })
            .await
            .unwrap();

        let event = convert_in_tx(&pool, &choice, instance, at, true)
            .await
            .unwrap()
            .expect("the resolve won");
        assert_eq!(
            (
                event.kind,
                event.started_at,
                event.ends_at,
                event.message_id
            ),
            (EventKind::FellOut, at, at + Duration::hours(1), None)
        );
        assert_eq!(
            status_of(&pool, choice.id).await,
            ("ignored".into(), Some(at))
        );
        let capped: DateTime<Utc> =
            sqlx::query_scalar("SELECT ends_at FROM engine.persona_events WHERE id = $1")
                .bind(running.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(capped, at, "the incident ends where the falling-out starts");
        assert_eq!(event_kinds(&pool, instance).await, ["quarrel", "fell_out"]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_conversion_that_does_not_commit_leaves_no_trace(pool: PgPool) {
        use crate::events::{EventRepo, NewEvent};
        use eros_engine_core::events::EventKind;

        let (_, instance, session) = instance_and_session(&pool).await;
        let repo = LifeChoiceRepo { pool: &pool };
        let choice = repo
            .insert_ask(session, 3, None, &ask_message("想先分开"))
            .await
            .unwrap()
            .expect("written");
        let at = choice.asked_at + Duration::hours(2);
        let ends = at + Duration::minutes(40);
        let running = EventRepo { pool: &pool }
            .insert_preempting(&NewEvent {
                instance_id: instance,
                session_id: session,
                event_key: "running",
                kind: EventKind::Quarrel,
                started_at: at - Duration::minutes(20),
                ends_at: ends,
            })
            .await
            .unwrap();

        // Both statements ran and the transaction was dropped: all or nothing.
        let written = convert_in_tx(&pool, &choice, instance, at, false)
            .await
            .unwrap();
        assert!(written.is_some(), "the statements themselves succeeded");
        assert_eq!(status_of(&pool, choice.id).await, ("pending".into(), None));
        assert_eq!(event_kinds(&pool, instance).await, ["quarrel"]);
        let still: DateTime<Utc> =
            sqlx::query_scalar("SELECT ends_at FROM engine.persona_events WHERE id = $1")
                .bind(running.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(still, ends, "the cap is undone with it");
        assert!(repo.pending(session, at).await.unwrap().is_some());

        // The same transaction run to the end after a failure part-way: a
        // repeated key makes the event insert error, and the resolve goes with it.
        convert_in_tx(&pool, &choice, instance, at, true)
            .await
            .unwrap()
            .expect("written");
        sqlx::query("UPDATE engine.life_choices SET status = 'pending', resolved_at = NULL")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            convert_in_tx(&pool, &choice, instance, at + Duration::minutes(1), true)
                .await
                .is_err(),
            "the key is taken"
        );
        assert_eq!(
            status_of(&pool, choice.id).await,
            ("pending".into(), None),
            "the failed event insert undid the resolve"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_resolve_that_loses_in_a_transaction_returns_none(pool: PgPool) {
        let (_, instance, session) = instance_and_session(&pool).await;
        let repo = LifeChoiceRepo { pool: &pool };
        let choice = repo
            .insert_ask(session, 3, None, &ask_message("想先分开"))
            .await
            .unwrap()
            .expect("written");
        let at = choice.asked_at + Duration::hours(2);

        // The endpoint answered first.
        repo.resolve(choice.id, "declined", at - Duration::minutes(1))
            .await
            .unwrap()
            .expect("resolved");
        assert!(convert_in_tx(&pool, &choice, instance, at, true)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            status_of(&pool, choice.id).await,
            ("declined".into(), Some(at - Duration::minutes(1)))
        );
        assert!(event_kinds(&pool, instance).await.is_empty());

        // A choice that lapsed cannot be resolved either; it stays pending.
        let lapsing = {
            sqlx::query("DELETE FROM engine.life_choices WHERE id = $1")
                .bind(choice.id)
                .execute(&pool)
                .await
                .unwrap();
            repo.insert_ask(session, 3, None, &ask_message("再想想"))
                .await
                .unwrap()
                .expect("written")
        };
        assert!(
            convert_in_tx(&pool, &lapsing, instance, lapsing.expires_at(), true)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(status_of(&pool, lapsing.id).await, ("pending".into(), None));
        assert!(event_kinds(&pool, instance).await.is_empty());
    }

    /// Two transactions racing for one choice: the second waits on the first's
    /// row lock and, once it commits, finds the choice answered.
    #[sqlx::test(migrations = "./migrations")]
    async fn concurrent_resolves_in_transactions_first_writer_wins(pool: PgPool) {
        let (_, _, session) = instance_and_session(&pool).await;
        let choice = LifeChoiceRepo { pool: &pool }
            .insert_ask(session, 3, None, &ask_message("想先分开"))
            .await
            .unwrap()
            .expect("written");
        let at = choice.asked_at + Duration::hours(2);

        let mut first = pool.begin().await.unwrap();
        LifeChoiceRepo::resolve_in_tx(&mut first, choice.id, "declined", at)
            .await
            .unwrap()
            .expect("the first wins");
        let second = tokio::spawn({
            let pool = pool.clone();
            async move {
                let mut tx = pool.begin().await.unwrap();
                let lost = LifeChoiceRepo::resolve_in_tx(&mut tx, choice.id, "ignored", at)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                lost
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!second.is_finished(), "the second waits for the first");
        first.commit().await.unwrap();
        assert!(
            second.await.unwrap().is_none(),
            "and then finds it answered"
        );
        assert_eq!(
            status_of(&pool, choice.id).await,
            ("declined".into(), Some(at))
        );
    }

    /// An accepted choice for `session` whose separation runs `[starts, ends)`.
    async fn accepted_with_separation(
        pool: &PgPool,
        instance: Uuid,
        session: Uuid,
        asked_ago: Duration,
        starts: DateTime<Utc>,
        ends: DateTime<Utc>,
        key: &str,
    ) -> LifeChoiceRecord {
        use crate::events::{EventRepo, NewEvent};
        use eros_engine_core::events::EventKind;

        let repo = LifeChoiceRepo { pool };
        let anchor = repo.streak_anchor(session).await.unwrap();
        let choice = repo
            .insert_ask(session, 3, anchor, &ask_message("想先分开"))
            .await
            .unwrap()
            .expect("written");
        repo.resolve(choice.id, "accepted", choice.asked_at)
            .await
            .unwrap()
            .expect("resolved");
        sqlx::query(
            "UPDATE engine.life_choices SET asked_at = asked_at - make_interval(secs => $2) \
              WHERE id = $1",
        )
        .bind(choice.id)
        .bind(asked_ago.num_seconds() as f64)
        .execute(pool)
        .await
        .unwrap();
        let event = EventRepo { pool }
            .insert_preempting(&NewEvent {
                instance_id: instance,
                session_id: session,
                event_key: key,
                kind: EventKind::TakingSpace,
                started_at: starts,
                ends_at: ends,
            })
            .await
            .unwrap();
        repo.set_event(choice.id, event.id).await.unwrap();
        choice
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn growth_slowed_is_the_72_hours_after_the_newest_accepted_separation(pool: PgPool) {
        use chrono::TimeZone;

        let (owner, instance, session) = instance_and_session(&pool).await;
        let sibling = ChatRepo { pool: &pool }
            .create_session(owner, instance)
            .await
            .unwrap()
            .id;
        let repo = LifeChoiceRepo { pool: &pool };
        let micro = Duration::microseconds(1);
        let starts = Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap();
        let ends = starts + Duration::hours(18);
        let window_end = ends + Duration::hours(72);

        // Nothing asked, nothing slowed.
        assert!(!repo.growth_slowed(session, ends).await.unwrap());

        let choice = accepted_with_separation(
            &pool,
            instance,
            session,
            Duration::hours(30),
            starts,
            ends,
            "k1",
        )
        .await;
        // The separation itself is not the window; it opens when it ends.
        assert!(!repo.growth_slowed(session, starts).await.unwrap());
        assert!(!repo.growth_slowed(session, ends - micro).await.unwrap());
        assert!(repo.growth_slowed(session, ends).await.unwrap());
        assert!(repo
            .growth_slowed(session, window_end - micro)
            .await
            .unwrap());
        assert!(!repo.growth_slowed(session, window_end).await.unwrap());
        // The choice's session only: a sibling of the same instance pays nothing.
        assert!(!repo.growth_slowed(sibling, ends).await.unwrap());

        // A choice that was not accepted opens no window, even with an event.
        sqlx::query("UPDATE engine.life_choices SET status = 'declined' WHERE id = $1")
            .bind(choice.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(!repo.growth_slowed(session, ends).await.unwrap());
        sqlx::query("UPDATE engine.life_choices SET status = 'accepted' WHERE id = $1")
            .bind(choice.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(repo.growth_slowed(session, ends).await.unwrap());

        // The newest accepted choice owns the window, not an older one.
        let later_starts = window_end + Duration::hours(10);
        let later_ends = later_starts + Duration::hours(12);
        accepted_with_separation(
            &pool,
            instance,
            session,
            Duration::hours(1),
            later_starts,
            later_ends,
            "k2",
        )
        .await;
        assert!(
            !repo.growth_slowed(session, ends).await.unwrap(),
            "the older window no longer applies"
        );
        assert!(repo.growth_slowed(session, later_ends).await.unwrap());

        // Once the separation row is gone the choice carries no window.
        sqlx::query("DELETE FROM engine.persona_events WHERE kind = 'taking_space'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(!repo.growth_slowed(session, later_ends).await.unwrap());
    }
}
