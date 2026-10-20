// SPDX-License-Identifier: AGPL-3.0-only
//! Choices the persona puts to the user, one row each in
//! `engine.life_choices` (spec 2026-10-19-life-events-shopping-gatherings-and-
//! ask-for-space-design.md §3.6).

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::chat::ProactiveInsert;

/// How long a choice waits for an answer; the only place it is written down.
/// A choice never stores its expiry: it is `asked_at` plus this.
const PENDING_HOURS: i32 = 24;

/// The kind of the one choice there is.
const ASK_FOR_SPACE: &str = "ask_for_space";

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
}

pub struct LifeChoiceRepo<'a> {
    pub pool: &'a PgPool,
}

impl LifeChoiceRepo<'_> {
    /// Write the message that asks and the choice it asks, in one
    /// transaction. `None` (nothing written, the message with it) while the
    /// session holds a pending choice that has not lapsed. `asked_at` is the
    /// message's `sent_at`.
    pub async fn insert_ask(
        &self,
        session_id: Uuid,
        tier_gap: i16,
        message: &ProactiveInsert,
    ) -> Result<Option<LifeChoiceRecord>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // Touching the session first takes its row lock: two asks for one
        // session queue here, and the second one's check sees the first's commit.
        sqlx::query("UPDATE engine.chat_sessions SET last_active_at = now() WHERE id = $1")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        let held: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM engine.life_choices \
                             WHERE session_id = $1 AND status = 'pending' \
                               AND asked_at > now() - make_interval(hours => $2))",
        )
        .bind(session_id)
        .bind(PENDING_HOURS)
        .fetch_one(&mut *tx)
        .await?;
        if held {
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
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM engine.life_choices c \
                              JOIN engine.chat_sessions s ON s.id = c.session_id \
                             WHERE s.instance_id = $1 AND c.status = 'pending' \
                               AND c.asked_at > $2 - make_interval(hours => $3))",
        )
        .bind(instance_id)
        .bind(now)
        .bind(PENDING_HOURS)
        .fetch_one(self.pool)
        .await
    }

    /// Answer the choice: `status` and `now` as its resolution. `None` when
    /// it is no longer pending or has lapsed at `now` — the answer that lost
    /// the race, or came too late. The first one wins.
    pub async fn resolve(
        &self,
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
        .fetch_optional(self.pool)
        .await
    }

    /// When the session was last asked, answered or not.
    pub async fn latest_asked_at(
        &self,
        session_id: Uuid,
    ) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
        sqlx::query_scalar("SELECT max(asked_at) FROM engine.life_choices WHERE session_id = $1")
            .bind(session_id)
            .fetch_one(self.pool)
            .await
    }

    /// Record the separation an accept wrote.
    pub async fn set_event(&self, choice_id: Uuid, event_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE engine.life_choices SET event_id = $2 WHERE id = $1")
            .bind(choice_id)
            .bind(event_id)
            .execute(self.pool)
            .await?;
        Ok(())
    }
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

    #[sqlx::test(migrations = "./migrations")]
    async fn insert_ask_writes_message_and_choice_atomically(pool: PgPool) {
        let (_, _, session) = instance_and_session(&pool).await;
        let m = ask_message("我想先分开一阵子");
        let choice = LifeChoiceRepo { pool: &pool }
            .insert_ask(session, 3, &m)
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
            .insert_ask(session, 2, &ask_message("第一次"))
            .await
            .unwrap()
            .expect("written");
        let second = ask_message("第二次");
        assert!(repo
            .insert_ask(session, 4, &second)
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
        assert!(repo.insert_ask(session, 4, &third).await.unwrap().is_some());
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
        assert!(repo
            .insert_ask(session, 2, &ask_message("再问"))
            .await
            .unwrap()
            .is_some());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn resolve_wins_once_and_respects_expiry(pool: PgPool) {
        let (_, _, session) = instance_and_session(&pool).await;
        let repo = LifeChoiceRepo { pool: &pool };
        let choice = repo
            .insert_ask(session, 3, &ask_message("想先分开"))
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
            .insert_ask(session, 3, &ask_message("再想想"))
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
            .insert_ask(session, 2, &ask_message("想先分开"))
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
        // The latest ask is the one just made, answered or not.
        assert_eq!(
            repo.latest_asked_at(session).await.unwrap(),
            Some(choice.asked_at)
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
            .insert_ask(asked_in, 3, &ask_message("想先分开"))
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

    #[sqlx::test(migrations = "./migrations")]
    async fn set_event_records_the_separation_and_latest_asked_at_reads_the_newest(pool: PgPool) {
        use crate::events::{EventRepo, NewEvent};
        use eros_engine_core::events::EventKind;

        let (_, instance, session) = instance_and_session(&pool).await;
        let repo = LifeChoiceRepo { pool: &pool };
        assert_eq!(repo.latest_asked_at(session).await.unwrap(), None);
        let choice = repo
            .insert_ask(session, 3, &ask_message("想先分开"))
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
        assert_eq!(
            repo.latest_asked_at(session).await.unwrap(),
            Some(choice.asked_at)
        );

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
}
