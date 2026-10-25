// SPDX-License-Identifier: AGPL-3.0-only
//! Life events, one row each in `engine.persona_events` (spec
//! 2026-10-15-life-events-v2-design.md §6).

use chrono::{DateTime, Utc};
use eros_engine_core::events::{self, EventKind, PastEvent};
use sqlx::PgPool;
use uuid::Uuid;

use crate::chat::{ChatMessage, ProactiveInsert};
use crate::life_choices::pending_for_instance_on;

/// One event. `session_id` is where it was drawn and told, `message_id` the
/// message that told it; either is `None` once its row is gone, and
/// `message_id` is `None` for an event that is never told.
#[derive(Debug, Clone, PartialEq)]
pub struct EventRecord {
    pub id: Uuid,
    pub kind: EventKind,
    pub session_id: Option<Uuid>,
    pub started_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub message_id: Option<Uuid>,
}

impl EventRecord {
    pub fn past(&self) -> PastEvent {
        PastEvent {
            kind: self.kind,
            started_at: self.started_at,
            ends_at: self.ends_at,
        }
    }
}

/// The events under way at a moment, and that moment.
#[derive(Debug, Clone, PartialEq)]
pub struct CurrentEvents {
    pub foreground: Option<EventRecord>,
    pub background: Option<EventRecord>,
    pub at: DateTime<Utc>,
}

/// An event to write.
pub struct NewEvent<'a> {
    pub instance_id: Uuid,
    pub session_id: Uuid,
    pub event_key: &'a str,
    pub kind: EventKind,
    pub started_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}

/// What `insert` wrote.
#[derive(Debug, Clone)]
pub struct Inserted {
    pub event: EventRecord,
    pub message: Option<ChatMessage>,
}

/// An event row's columns: id, kind, session_id, started_at, ends_at,
/// message_id.
type EventColumns = (
    Uuid,
    String,
    Option<Uuid>,
    DateTime<Utc>,
    DateTime<Utc>,
    Option<Uuid>,
);

/// The row as an event; `None` for a kind this build does not know.
fn record(
    (id, kind, session_id, started_at, ends_at, message_id): EventColumns,
) -> Option<EventRecord> {
    Some(EventRecord {
        id,
        kind: EventKind::parse(&kind)?,
        session_id,
        started_at,
        ends_at,
        message_id,
    })
}

fn lookback_hours() -> i32 {
    events::lookback().num_hours() as i32
}

/// Serialize the writers of one instance's incidents and asks: a
/// transaction-scoped advisory lock keyed on the instance, released with the
/// transaction. Every writer takes it as its first statement; under READ
/// COMMITTED each later statement then sees all that the previous holder
/// committed, so a guard or a recheck reads the real state, not a snapshot
/// that misses a transaction still in flight.
pub(crate) async fn lock_instance(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))")
        .bind(instance_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub struct EventRepo<'a> {
    pub pool: &'a PgPool,
}

impl EventRepo<'_> {
    /// Write an event and, when it has one, the message that tells it, in
    /// one transaction (spec §6.4). Nothing is written — `None` — while the
    /// instance has a foreground event under way at `started_at`, or, for a
    /// background, any event; nor while any session of the instance waits for
    /// an answer to an ask; nor on a repeated `event_key` in the session.
    /// The guards are read under the instance's lock, so a writer in flight
    /// (an accept, a conversion, an ask) is waited for, not missed. The
    /// message's `sent_at` is the event's `started_at`.
    pub async fn insert(
        &self,
        e: &NewEvent<'_>,
        message: Option<&ProactiveInsert>,
    ) -> Result<Option<Inserted>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        lock_instance(&mut tx, e.instance_id).await?;
        if pending_for_instance_on(&mut *tx, e.instance_id, Utc::now()).await? {
            // Dropped uncommitted.
            return Ok(None);
        }
        let written = match message {
            Some(m) => Some(
                sqlx::query_as::<_, ChatMessage>(
                    "INSERT INTO engine.chat_messages \
                       (id, session_id, role, content, generation_id, assistant_action_type, \
                        metadata, llm_attempts, gateway_errors, sent_at) \
                     VALUES ($1, $2, 'assistant', $3, $4, 'proactive', $5, $6, $7, $8) \
                     RETURNING *",
                )
                .bind(m.id)
                .bind(e.session_id)
                .bind(&m.content)
                .bind(&m.generation_id)
                .bind(&m.metadata)
                .bind(&m.llm_attempts)
                .bind(&m.gateway_errors)
                .bind(e.started_at)
                .fetch_one(&mut *tx)
                .await?,
            ),
            None => None,
        };
        let background: Vec<String> = events::background_kinds()
            .into_iter()
            .map(|k| k.as_str().to_string())
            .collect();
        let row: Option<EventColumns> = sqlx::query_as(
            "INSERT INTO engine.persona_events \
               (instance_id, session_id, event_key, kind, started_at, ends_at, message_id) \
             SELECT $1, $2, $3, $4, $5, $6, $7 \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM engine.persona_events p \
                  WHERE p.instance_id = $1 \
                    AND p.started_at > $5 - make_interval(hours => $8) \
                    AND p.ends_at > $5 \
                    AND ($9 OR NOT (p.kind = ANY($10))) \
             ) \
             ON CONFLICT (session_id, event_key) DO NOTHING \
             RETURNING id, kind, session_id, started_at, ends_at, message_id",
        )
        .bind(e.instance_id)
        .bind(e.session_id)
        .bind(e.event_key)
        .bind(e.kind.as_str())
        .bind(e.started_at)
        .bind(e.ends_at)
        .bind(written.as_ref().map(|m| m.id))
        .bind(lookback_hours())
        .bind(e.kind.is_background())
        .bind(&background)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(event) = row.and_then(record) else {
            // Dropped uncommitted: the message goes with it.
            return Ok(None);
        };
        if written.is_some() {
            sqlx::query("UPDATE engine.chat_sessions SET last_active_at = now() WHERE id = $1")
                .bind(e.session_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(Some(Inserted {
            event,
            message: written,
        }))
    }

    /// Write an event that displaces what she is doing: every foreground
    /// under way at `started_at` ends there, a background keeps running, and
    /// the event lands whether or not one was. No message, and no draw guard:
    /// the caller has decided. One transaction; a repeated `event_key` in the
    /// session is an error and undoes the cap. See [`Self::insert_preempting_in_tx`].
    pub async fn insert_preempting(&self, e: &NewEvent<'_>) -> Result<EventRecord, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let event = Self::insert_preempting_in_tx(&mut tx, e).await?;
        tx.commit().await?;
        Ok(event)
    }

    /// [`Self::insert_preempting`] inside the caller's transaction, so the
    /// event commits or rolls back together with whatever else the caller
    /// writes; nothing here commits. An error leaves the transaction unusable:
    /// drop it, and the cap goes with it.
    pub async fn insert_preempting_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        e: &NewEvent<'_>,
    ) -> Result<EventRecord, sqlx::Error> {
        // The cap and the insert are two statements: no other writer of the
        // instance may land between them. The lock is held to the caller's
        // commit.
        lock_instance(tx, e.instance_id).await?;
        let background: Vec<String> = events::background_kinds()
            .into_iter()
            .map(|k| k.as_str().to_string())
            .collect();
        // `started_at < $2`: a row starting at the moment itself is not capped
        // (its `ends_at` would equal its `started_at`, which the table forbids).
        sqlx::query(
            "UPDATE engine.persona_events SET ends_at = $2 \
              WHERE instance_id = $1 AND started_at < $2 AND ends_at > $2 \
                AND NOT (kind = ANY($3))",
        )
        .bind(e.instance_id)
        .bind(e.started_at)
        .bind(&background)
        .execute(&mut **tx)
        .await?;
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_events \
               (instance_id, session_id, event_key, kind, started_at, ends_at) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             RETURNING id",
        )
        .bind(e.instance_id)
        .bind(e.session_id)
        .bind(e.event_key)
        .bind(e.kind.as_str())
        .bind(e.started_at)
        .bind(e.ends_at)
        .fetch_one(&mut **tx)
        .await?;
        Ok(EventRecord {
            id,
            kind: e.kind,
            session_id: Some(e.session_id),
            started_at: e.started_at,
            ends_at: e.ends_at,
            message_id: None,
        })
    }

    /// The event written in this session under `event_key`.
    pub async fn by_key(
        &self,
        session_id: Uuid,
        event_key: &str,
    ) -> Result<Option<EventRecord>, sqlx::Error> {
        let row: Option<EventColumns> = sqlx::query_as(
            "SELECT id, kind, session_id, started_at, ends_at, message_id \
               FROM engine.persona_events WHERE session_id = $1 AND event_key = $2",
        )
        .bind(session_id)
        .bind(event_key)
        .fetch_optional(self.pool)
        .await?;
        Ok(row.and_then(record))
    }

    /// The instance's events started at or after `since`, across its
    /// sessions, newest first.
    pub async fn recent(
        &self,
        instance_id: Uuid,
        since: DateTime<Utc>,
    ) -> Result<Vec<EventRecord>, sqlx::Error> {
        let rows: Vec<EventColumns> = sqlx::query_as(
            "SELECT id, kind, session_id, started_at, ends_at, message_id \
               FROM engine.persona_events \
              WHERE instance_id = $1 AND started_at >= $2 \
              ORDER BY started_at DESC, id DESC",
        )
        .bind(instance_id)
        .bind(since)
        .fetch_all(self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(record).collect())
    }

    /// Whether the instance has ever had an event of `kind`, however long
    /// ago: not bounded by `lookback()`.
    pub async fn ever(&self, instance_id: Uuid, kind: EventKind) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM engine.persona_events \
                             WHERE instance_id = $1 AND kind = $2)",
        )
        .bind(instance_id)
        .bind(kind.as_str())
        .fetch_one(self.pool)
        .await
    }

    /// The foreground and the background under way at `at_message`'s
    /// `sent_at`, or at `now` without one: started at or before that moment,
    /// ending after it.
    pub async fn current(
        &self,
        instance_id: Uuid,
        at_message: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> Result<CurrentEvents, sqlx::Error> {
        type Row = (
            DateTime<Utc>,
            Option<Uuid>,
            Option<String>,
            Option<Uuid>,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
            Option<Uuid>,
        );
        // The LEFT JOIN keeps the moment's row when nothing is under way.
        let rows: Vec<Row> = sqlx::query_as(
            "WITH t AS (SELECT COALESCE( \
                     (SELECT a.sent_at FROM engine.chat_messages a WHERE a.id = $2), $3) AS at) \
             SELECT t.at, p.id, p.kind, p.session_id, p.started_at, p.ends_at, p.message_id \
               FROM t LEFT JOIN engine.persona_events p \
                 ON p.instance_id = $1 AND p.started_at <= t.at AND p.ends_at > t.at \
                AND p.started_at > t.at - make_interval(hours => $4) \
              ORDER BY p.started_at DESC, p.id DESC",
        )
        .bind(instance_id)
        .bind(at_message)
        .bind(now)
        .bind(lookback_hours())
        .fetch_all(self.pool)
        .await?;
        let mut out = CurrentEvents {
            foreground: None,
            background: None,
            at: rows.first().map(|r| r.0).unwrap_or(now),
        };
        for (_, id, kind, session_id, started_at, ends_at, message_id) in rows {
            let (Some(id), Some(kind), Some(started_at), Some(ends_at)) =
                (id, kind, started_at, ends_at)
            else {
                continue;
            };
            let Some(e) = record((id, kind, session_id, started_at, ends_at, message_id)) else {
                continue;
            };
            let slot = if e.kind.is_background() {
                &mut out.background
            } else {
                &mut out.foreground
            };
            if slot.is_none() {
                *slot = Some(e);
            }
        }
        Ok(out)
    }

    /// For each of `instance_ids`, the background under way at `at`
    /// (`started_at <= at < ends_at`, within `lookback()`), newest start
    /// first. Instances with none are absent. One query (spec 2026-10-23-
    /// life-events-world-wiring-design.md §4).
    pub async fn backgrounds_at(
        &self,
        instance_ids: &[Uuid],
        at: DateTime<Utc>,
    ) -> Result<std::collections::HashMap<Uuid, EventRecord>, sqlx::Error> {
        let background: Vec<String> = events::background_kinds()
            .into_iter()
            .map(|k| k.as_str().to_string())
            .collect();
        // instance_id, then the event's columns.
        type Row = (
            Uuid,
            Uuid,
            String,
            Option<Uuid>,
            DateTime<Utc>,
            DateTime<Utc>,
            Option<Uuid>,
        );
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT DISTINCT ON (instance_id) \
                        instance_id, id, kind, session_id, started_at, ends_at, message_id \
                   FROM engine.persona_events \
                  WHERE instance_id = ANY($1) AND kind = ANY($2) \
                    AND started_at <= $3 AND ends_at > $3 \
                    AND started_at > $3 - make_interval(hours => $4) \
                  ORDER BY instance_id, started_at DESC, id DESC",
        )
        .bind(instance_ids)
        .bind(&background)
        .bind(at)
        .bind(lookback_hours())
        .fetch_all(self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(
                |(instance_id, id, kind, session_id, started_at, ends_at, message_id)| {
                    record((id, kind, session_id, started_at, ends_at, message_id))
                        .map(|e| (instance_id, e))
                },
            )
            .collect())
    }

    /// The instance's newest event among `offline` that ended by `at`, when
    /// this session holds a user row ghosted inside it and no assistant row
    /// with text written after it ended (spec 2026-10-16-life-events-offline-
    /// and-scares-design.md §4.2). `offline` is what took this persona offline
    /// (`events::offline_kinds`): a stretch of any other kind is not in the
    /// query, so it cannot hide an older one that was.
    pub async fn unexplained_offline(
        &self,
        session_id: Uuid,
        instance_id: Uuid,
        at: DateTime<Utc>,
        offline: &[EventKind],
    ) -> Result<Option<EventRecord>, sqlx::Error> {
        let offline: Vec<&str> = offline.iter().map(|k| k.as_str()).collect();
        let row: Option<EventColumns> = sqlx::query_as(
            "WITH off AS ( \
                 SELECT id, kind, session_id, started_at, ends_at, message_id \
                   FROM engine.persona_events \
                  WHERE instance_id = $2 AND kind = ANY($4) \
                    AND ends_at <= $3 AND started_at > $3 - make_interval(hours => $5) \
                  ORDER BY started_at DESC, id DESC \
                  LIMIT 1 \
             ) \
             SELECT off.id, off.kind, off.session_id, off.started_at, off.ends_at, off.message_id \
               FROM off \
              WHERE EXISTS (SELECT 1 FROM engine.chat_messages m \
                             WHERE m.session_id = $1 AND m.role = 'user' AND m.ghost_decision \
                               AND m.sent_at >= off.started_at AND m.sent_at < off.ends_at) \
                AND NOT EXISTS (SELECT 1 FROM engine.chat_messages m \
                                 WHERE m.session_id = $1 AND m.role = 'assistant' \
                                   AND m.content <> '' \
                                   AND m.sent_at > off.ends_at AND m.sent_at <= $3)",
        )
        .bind(session_id)
        .bind(instance_id)
        .bind(at)
        .bind(&offline)
        .bind(lookback_hours())
        .fetch_optional(self.pool)
        .await?;
        Ok(row.and_then(record))
    }

    /// The separation this session agreed to (spec 2026-10-19-life-events-
    /// shopping-gatherings-and-ask-for-space-design.md §3.5, "The return is
    /// acknowledged either way"): the newest `taking_space` row an accepted
    /// choice of this session points at (`life_choices.event_id`) that ended
    /// by `at`, while the session holds no assistant row with text written
    /// after it ended. The flag says whether a user row ghosted inside it
    /// exists: `true` when they wrote into the silence, `false` when they
    /// honoured it. The standard rule of [`Self::unexplained_offline`] needs
    /// that row; this one does not, so the return is marked either way.
    pub async fn unreturned_separation(
        &self,
        session_id: Uuid,
        instance_id: Uuid,
        at: DateTime<Utc>,
    ) -> Result<Option<(EventRecord, bool)>, sqlx::Error> {
        type Row = (
            Uuid,
            String,
            Option<Uuid>,
            DateTime<Utc>,
            DateTime<Utc>,
            Option<Uuid>,
            bool,
        );
        let row: Option<Row> = sqlx::query_as(
            "WITH sep AS ( \
                 SELECT e.id, e.kind, e.session_id, e.started_at, e.ends_at, e.message_id \
                   FROM engine.life_choices c \
                   JOIN engine.persona_events e ON e.id = c.event_id \
                  WHERE c.session_id = $1 AND c.status = 'accepted' \
                    AND e.instance_id = $2 AND e.kind = $4 \
                    AND e.ends_at <= $3 AND e.started_at > $3 - make_interval(hours => $5) \
                  ORDER BY e.started_at DESC, e.id DESC \
                  LIMIT 1 \
             ) \
             SELECT sep.id, sep.kind, sep.session_id, sep.started_at, sep.ends_at, \
                    sep.message_id, \
                    EXISTS (SELECT 1 FROM engine.chat_messages m \
                             WHERE m.session_id = $1 AND m.role = 'user' AND m.ghost_decision \
                               AND m.sent_at >= sep.started_at AND m.sent_at < sep.ends_at) \
               FROM sep \
              WHERE NOT EXISTS (SELECT 1 FROM engine.chat_messages m \
                                 WHERE m.session_id = $1 AND m.role = 'assistant' \
                                   AND m.content <> '' \
                                   AND m.sent_at > sep.ends_at AND m.sent_at <= $3)",
        )
        .bind(session_id)
        .bind(instance_id)
        .bind(at)
        .bind(EventKind::TakingSpace.as_str())
        .bind(lookback_hours())
        .fetch_optional(self.pool)
        .await?;
        Ok(row.and_then(
            |(id, kind, session_id, started_at, ends_at, message_id, wrote)| {
                record((id, kind, session_id, started_at, ends_at, message_id)).map(|e| (e, wrote))
            },
        ))
    }

    /// The newest false alarm told in this session that ended by `at`, while
    /// the session holds no assistant row with text written after it ended
    /// (spec 2026-10-16-life-events-offline-and-scares-design.md §5.2).
    pub async fn unresolved_scare(
        &self,
        session_id: Uuid,
        instance_id: Uuid,
        at: DateTime<Utc>,
    ) -> Result<Option<EventRecord>, sqlx::Error> {
        let scares: Vec<&str> = EventKind::ALL
            .into_iter()
            .filter(|k| k.is_false_alarm())
            .map(EventKind::as_str)
            .collect();
        let row: Option<EventColumns> = sqlx::query_as(
            "WITH scare AS ( \
                 SELECT id, kind, session_id, started_at, ends_at, message_id \
                   FROM engine.persona_events \
                  WHERE instance_id = $2 AND session_id = $1 AND kind = ANY($4) \
                    AND ends_at <= $3 AND started_at > $3 - make_interval(hours => $5) \
                  ORDER BY started_at DESC, id DESC \
                  LIMIT 1 \
             ) \
             SELECT scare.id, scare.kind, scare.session_id, scare.started_at, scare.ends_at, \
                    scare.message_id \
               FROM scare \
              WHERE NOT EXISTS (SELECT 1 FROM engine.chat_messages m \
                                 WHERE m.session_id = $1 AND m.role = 'assistant' \
                                   AND m.content <> '' \
                                   AND m.sent_at > scare.ends_at AND m.sent_at <= $3)",
        )
        .bind(session_id)
        .bind(instance_id)
        .bind(at)
        .bind(&scares)
        .bind(lookback_hours())
        .fetch_optional(self.pool)
        .await?;
        Ok(row.and_then(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ChatRepo;
    use crate::testutil::seed_persona_instance;
    use eros_engine_core::events::EventKind::*;
    use eros_engine_core::events::PersonaTraits;
    use serde_json::json;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// The kinds that take a non-female persona offline.
    fn kinds() -> Vec<EventKind> {
        events::offline_kinds(&PersonaTraits::default())
    }

    fn female_kinds() -> Vec<EventKind> {
        events::offline_kinds(&PersonaTraits {
            female: true,
            ..PersonaTraits::default()
        })
    }

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

    async fn another_session(pool: &PgPool, owner: Uuid, instance: Uuid) -> Uuid {
        ChatRepo { pool }
            .create_session(owner, instance)
            .await
            .unwrap()
            .id
    }

    fn ev<'a>(
        instance_id: Uuid,
        session_id: Uuid,
        key: &'a str,
        kind: EventKind,
        from: &str,
        to: &str,
    ) -> NewEvent<'a> {
        NewEvent {
            instance_id,
            session_id,
            event_key: key,
            kind,
            started_at: t(from),
            ends_at: t(to),
        }
    }

    fn message(content: &str) -> ProactiveInsert {
        ProactiveInsert {
            id: Uuid::new_v4(),
            content: content.into(),
            generation_id: None,
            metadata: json!({"proactive": "event"}),
            llm_attempts: None,
            gateway_errors: None,
        }
    }

    /// A row with text when it is the persona's, empty when it is the user's.
    async fn row(pool: &PgPool, session_id: Uuid, role: &str, at: &str, ghosted: bool) -> Uuid {
        let content = if role == "assistant" { "…" } else { "" };
        row_with(pool, session_id, role, content, at, ghosted).await
    }

    async fn row_with(
        pool: &PgPool,
        session_id: Uuid,
        role: &str,
        content: &str,
        at: &str,
        ghosted: bool,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, ghost_decision, sent_at) \
             VALUES ($1, $2, $3, $4, $5::timestamptz) RETURNING id",
        )
        .bind(session_id)
        .bind(role)
        .bind(content)
        .bind(ghosted)
        .bind(at)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn insert_writes_the_event_and_its_message(pool: PgPool) {
        let (_, instance, session) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        let m = message("刚被叫去加班");
        let got = repo
            .insert(
                &ev(
                    instance,
                    session,
                    "k1",
                    TempWork,
                    "2026-10-01T10:00:00Z",
                    "2026-10-01T14:00:00Z",
                ),
                Some(&m),
            )
            .await
            .unwrap()
            .expect("written");
        assert_eq!(got.event.kind, TempWork);
        assert_eq!(got.event.session_id, Some(session));
        assert_eq!(got.event.message_id, Some(m.id));
        let msg = got.message.expect("the message");
        assert_eq!((msg.id, msg.sent_at), (m.id, t("2026-10-01T10:00:00Z")));
        assert_eq!(
            repo.by_key(session, "k1").await.unwrap(),
            Some(got.event.clone())
        );
        assert_eq!(repo.by_key(session, "other").await.unwrap(), None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_event_without_a_message_writes_none(pool: PgPool) {
        let (_, instance, session) = instance_and_session(&pool).await;
        let got = EventRepo { pool: &pool }
            .insert(
                &ev(
                    instance,
                    session,
                    "a1",
                    AwayFromPhone,
                    "2026-10-01T10:00:00Z",
                    "2026-10-01T12:00:00Z",
                ),
                None,
            )
            .await
            .unwrap()
            .expect("written");
        assert!(got.message.is_none());
        assert_eq!(got.event.message_id, None);
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.chat_messages WHERE session_id = $1")
                .bind(session)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_foreground_waits_for_a_foreground_not_a_background(pool: PgPool) {
        let (_, i, s) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        let wrote = |e: Option<Inserted>| e.is_some();
        assert!(wrote(
            repo.insert(
                &ev(
                    i,
                    s,
                    "trip",
                    BusinessTrip,
                    "2026-10-01T10:00:00Z",
                    "2026-10-02T10:00:00Z"
                ),
                None
            )
            .await
            .unwrap()
        ));
        assert!(
            wrote(
                repo.insert(
                    &ev(
                        i,
                        s,
                        "work",
                        TempWork,
                        "2026-10-01T11:00:00Z",
                        "2026-10-01T15:00:00Z"
                    ),
                    None
                )
                .await
                .unwrap()
            ),
            "an incident lands during a trip"
        );
        assert!(
            !wrote(
                repo.insert(
                    &ev(
                        i,
                        s,
                        "rain",
                        WeatherChange,
                        "2026-10-01T12:00:00Z",
                        "2026-10-01T16:00:00Z"
                    ),
                    None
                )
                .await
                .unwrap()
            ),
            "a second incident waits for the first"
        );
        assert!(wrote(
            repo.insert(
                &ev(
                    i,
                    s,
                    "rain2",
                    WeatherChange,
                    "2026-10-01T15:00:00Z",
                    "2026-10-01T19:00:00Z"
                ),
                None
            )
            .await
            .unwrap()
        ));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_background_waits_for_any_event_under_way(pool: PgPool) {
        let (_, i, s) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        let wrote = |e: Option<Inserted>| e.is_some();
        assert!(wrote(
            repo.insert(
                &ev(
                    i,
                    s,
                    "work",
                    TempWork,
                    "2026-10-01T10:00:00Z",
                    "2026-10-01T14:00:00Z"
                ),
                None
            )
            .await
            .unwrap()
        ));
        assert!(!wrote(
            repo.insert(
                &ev(
                    i,
                    s,
                    "trip",
                    BusinessTrip,
                    "2026-10-01T11:00:00Z",
                    "2026-10-02T11:00:00Z"
                ),
                None
            )
            .await
            .unwrap()
        ));
        assert!(
            wrote(
                repo.insert(
                    &ev(
                        i,
                        s,
                        "trip2",
                        BusinessTrip,
                        "2026-10-01T14:00:00Z",
                        "2026-10-02T14:00:00Z"
                    ),
                    None
                )
                .await
                .unwrap()
            ),
            "ends_at is exclusive"
        );
        assert!(!wrote(
            repo.insert(
                &ev(
                    i,
                    s,
                    "sick",
                    Sick,
                    "2026-10-01T15:00:00Z",
                    "2026-10-02T15:00:00Z"
                ),
                None
            )
            .await
            .unwrap()
        ));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_event_key_is_unique_per_session(pool: PgPool) {
        let (owner, i, s1) = instance_and_session(&pool).await;
        let s2 = another_session(&pool, owner, i).await;
        let repo = EventRepo { pool: &pool };
        assert!(repo
            .insert(
                &ev(
                    i,
                    s1,
                    "same",
                    Reading,
                    "2026-10-01T10:00:00Z",
                    "2026-10-01T11:00:00Z"
                ),
                None
            )
            .await
            .unwrap()
            .is_some());
        assert!(repo
            .insert(
                &ev(
                    i,
                    s1,
                    "same",
                    Gaming,
                    "2026-10-01T12:00:00Z",
                    "2026-10-01T13:00:00Z"
                ),
                None
            )
            .await
            .unwrap()
            .is_none());
        assert!(repo
            .insert(
                &ev(
                    i,
                    s2,
                    "same",
                    Gaming,
                    "2026-10-01T12:00:00Z",
                    "2026-10-01T13:00:00Z"
                ),
                None
            )
            .await
            .unwrap()
            .is_some());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_lost_insert_leaves_no_message(pool: PgPool) {
        let (_, i, s) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        repo.insert(
            &ev(
                i,
                s,
                "work",
                TempWork,
                "2026-10-01T10:00:00Z",
                "2026-10-01T14:00:00Z",
            ),
            None,
        )
        .await
        .unwrap()
        .expect("written");
        let m = message("又下雨了");
        assert!(repo
            .insert(
                &ev(
                    i,
                    s,
                    "rain",
                    WeatherChange,
                    "2026-10-01T11:00:00Z",
                    "2026-10-01T15:00:00Z"
                ),
                Some(&m)
            )
            .await
            .unwrap()
            .is_none());
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM engine.chat_messages WHERE id = $1")
            .bind(m.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn current_reads_the_foreground_and_the_background_at_the_anchor(pool: PgPool) {
        let (_, i, s) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        let trip = repo
            .insert(
                &ev(
                    i,
                    s,
                    "trip",
                    BusinessTrip,
                    "2026-10-01T09:00:00Z",
                    "2026-10-02T09:00:00Z",
                ),
                None,
            )
            .await
            .unwrap()
            .unwrap()
            .event;
        let work = repo
            .insert(
                &ev(
                    i,
                    s,
                    "work",
                    TempWork,
                    "2026-10-01T10:00:00Z",
                    "2026-10-01T14:00:00Z",
                ),
                None,
            )
            .await
            .unwrap()
            .unwrap()
            .event;
        let before = row(&pool, s, "user", "2026-10-01T09:59:00Z", false).await;
        let during = row(&pool, s, "user", "2026-10-01T11:00:00Z", false).await;
        let after = row(&pool, s, "user", "2026-10-01T14:00:00Z", false).await;
        let at = |anchor| repo.current(i, Some(anchor), Utc::now());

        let c = at(before).await.unwrap();
        assert_eq!(
            (c.foreground, c.background.clone(), c.at),
            (None, Some(trip.clone()), t("2026-10-01T09:59:00Z"))
        );
        let c = at(during).await.unwrap();
        assert_eq!(
            (c.foreground, c.background),
            (Some(work.clone()), Some(trip.clone()))
        );
        let c = at(after).await.unwrap();
        assert_eq!(
            (c.foreground, c.background),
            (None, Some(trip.clone())),
            "ends_at is exclusive"
        );
        // Without an anchor, `now` is the moment.
        let c = repo
            .current(i, None, t("2026-10-01T12:00:00Z"))
            .await
            .unwrap();
        assert_eq!(
            (c.foreground, c.background, c.at),
            (Some(work), Some(trip), t("2026-10-01T12:00:00Z"))
        );
        let c = repo
            .current(i, None, t("2026-10-02T09:00:00Z"))
            .await
            .unwrap();
        assert_eq!((c.foreground, c.background), (None, None));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recent_reads_the_instances_events_since_and_skips_unknown_kinds(pool: PgPool) {
        let (owner, i, s1) = instance_and_session(&pool).await;
        let s2 = another_session(&pool, owner, i).await;
        let (_, other_i, other_s) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        let e1 = repo
            .insert(
                &ev(
                    i,
                    s1,
                    "e1",
                    Reading,
                    "2026-10-01T08:00:00Z",
                    "2026-10-01T09:00:00Z",
                ),
                None,
            )
            .await
            .unwrap()
            .unwrap()
            .event;
        let e2 = repo
            .insert(
                &ev(
                    i,
                    s2,
                    "e2",
                    Gaming,
                    "2026-10-01T10:00:00Z",
                    "2026-10-01T11:00:00Z",
                ),
                None,
            )
            .await
            .unwrap()
            .unwrap()
            .event;
        repo.insert(
            &ev(
                other_i,
                other_s,
                "e3",
                Reading,
                "2026-10-01T10:00:00Z",
                "2026-10-01T11:00:00Z",
            ),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.persona_events (instance_id, session_id, event_key, kind, started_at, ends_at) \
             VALUES ($1, $2, 'd', 'dragon', '2026-10-01T12:00:00Z', '2026-10-01T13:00:00Z')",
        )
        .bind(i)
        .bind(s1)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            repo.recent(i, t("2026-10-01T09:30:00Z")).await.unwrap(),
            vec![e2.clone()]
        );
        assert_eq!(
            repo.recent(i, t("2026-10-01T00:00:00Z")).await.unwrap(),
            vec![e2, e1]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn deleting_a_message_or_a_session_keeps_the_event(pool: PgPool) {
        let (_, i, s) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        let m = message("出发啦");
        repo.insert(
            &ev(
                i,
                s,
                "trip",
                ShortTrip,
                "2026-10-01T10:00:00Z",
                "2026-10-03T10:00:00Z",
            ),
            Some(&m),
        )
        .await
        .unwrap()
        .unwrap();
        sqlx::query("DELETE FROM engine.chat_messages WHERE id = $1")
            .bind(m.id)
            .execute(&pool)
            .await
            .unwrap();
        let e = repo.by_key(s, "trip").await.unwrap().expect("still there");
        assert_eq!(e.message_id, None);
        sqlx::query("DELETE FROM engine.chat_sessions WHERE id = $1")
            .bind(s)
            .execute(&pool)
            .await
            .unwrap();
        let left = repo.recent(i, t("2026-10-01T00:00:00Z")).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].session_id, None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unexplained_away_wants_a_ghosted_row_and_no_reply_since(pool: PgPool) {
        let (_, i, s) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        let away = repo
            .insert(
                &ev(
                    i,
                    s,
                    "away",
                    AwayFromPhone,
                    "2026-10-01T10:00:00Z",
                    "2026-10-01T12:00:00Z",
                ),
                None,
            )
            .await
            .unwrap()
            .unwrap()
            .event;
        // A user row before the window, not ghosted inside it: nothing to explain.
        row(&pool, s, "user", "2026-10-01T09:00:00Z", true).await;
        assert_eq!(
            repo.unexplained_offline(s, i, t("2026-10-01T13:00:00Z"), &kinds())
                .await
                .unwrap(),
            None
        );
        // Ghosted inside the window.
        row(&pool, s, "user", "2026-10-01T11:00:00Z", true).await;
        assert_eq!(
            repo.unexplained_offline(s, i, t("2026-10-01T13:00:00Z"), &kinds())
                .await
                .unwrap(),
            Some(away.clone())
        );
        // Not before the stretch ended.
        assert_eq!(
            repo.unexplained_offline(s, i, t("2026-10-01T11:30:00Z"), &kinds())
                .await
                .unwrap(),
            None
        );
        // A reply after the end explains it — from the reply's moment on.
        row(&pool, s, "assistant", "2026-10-01T12:30:00Z", false).await;
        assert_eq!(
            repo.unexplained_offline(s, i, t("2026-10-01T12:15:00Z"), &kinds())
                .await
                .unwrap(),
            Some(away)
        );
        assert_eq!(
            repo.unexplained_offline(s, i, t("2026-10-01T13:00:00Z"), &kinds())
                .await
                .unwrap(),
            None
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unexplained_away_is_per_session(pool: PgPool) {
        let (owner, i, s1) = instance_and_session(&pool).await;
        let s2 = another_session(&pool, owner, i).await;
        let repo = EventRepo { pool: &pool };
        repo.insert(
            &ev(
                i,
                s1,
                "away",
                AwayFromPhone,
                "2026-10-01T10:00:00Z",
                "2026-10-01T12:00:00Z",
            ),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        row(&pool, s2, "user", "2026-10-01T11:00:00Z", true).await;
        assert!(repo
            .unexplained_offline(s2, i, t("2026-10-01T13:00:00Z"), &kinds())
            .await
            .unwrap()
            .is_some());
        assert!(repo
            .unexplained_offline(s1, i, t("2026-10-01T13:00:00Z"), &kinds())
            .await
            .unwrap()
            .is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_reply_inside_the_window_does_not_end_the_unexplained_state(pool: PgPool) {
        let (_, i, s) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        let away = repo
            .insert(
                &ev(
                    i,
                    s,
                    "away",
                    AwayFromPhone,
                    "2026-10-01T10:00:00Z",
                    "2026-10-01T12:00:00Z",
                ),
                None,
            )
            .await
            .unwrap()
            .unwrap()
            .event;
        row(&pool, s, "user", "2026-10-01T10:30:00Z", true).await;
        // An action turn answered mid-stretch.
        row(&pool, s, "assistant", "2026-10-01T11:00:00Z", false).await;
        assert_eq!(
            repo.unexplained_offline(s, i, t("2026-10-01T13:00:00Z"), &kinds())
                .await
                .unwrap(),
            Some(away)
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn every_offline_kind_is_explained_afterwards(pool: PgPool) {
        for kind in [
            AwayFromPhone,
            Busy,
            InBattle,
            Asleep,
            BrosGathering,
            SisGathering,
            TakingSpace,
        ] {
            let (_, i, s) = instance_and_session(&pool).await;
            let repo = EventRepo { pool: &pool };
            repo.insert(
                &ev(
                    i,
                    s,
                    "off",
                    kind,
                    "2026-10-01T10:00:00Z",
                    "2026-10-01T12:00:00Z",
                ),
                None,
            )
            .await
            .unwrap()
            .unwrap();
            row(&pool, s, "user", "2026-10-01T10:30:00Z", true).await;
            let got = repo
                .unexplained_offline(s, i, t("2026-10-01T13:00:00Z"), &kinds())
                .await
                .unwrap();
            assert_eq!(got.map(|e| e.kind), Some(kind), "{kind:?}");
        }
        let (_, i, s) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        repo.insert(
            &ev(
                i,
                s,
                "r",
                Reading,
                "2026-10-01T10:00:00Z",
                "2026-10-01T12:00:00Z",
            ),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        row(&pool, s, "user", "2026-10-01T10:30:00Z", true).await;
        assert_eq!(
            repo.unexplained_offline(s, i, t("2026-10-01T13:00:00Z"), &kinds())
                .await
                .unwrap(),
            None,
            "reading is not offline"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_taking_space_row_is_found_by_unexplained_offline(pool: PgPool) {
        let (_, i, s) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        let space = repo
            .insert_preempting(&ev(
                i,
                s,
                "space",
                TakingSpace,
                "2026-10-01T10:00:00Z",
                "2026-10-01T16:00:00Z",
            ))
            .await
            .unwrap();
        // The user honoured the break: nothing ghosted inside it, nothing to explain
        // under the standard rule.
        assert_eq!(
            repo.unexplained_offline(s, i, t("2026-10-01T17:00:00Z"), &kinds())
                .await
                .unwrap(),
            None
        );
        // A message ghosted inside the window, no reply since.
        row(&pool, s, "user", "2026-10-01T11:00:00Z", true).await;
        assert_eq!(
            repo.unexplained_offline(s, i, t("2026-10-01T17:00:00Z"), &kinds())
                .await
                .unwrap(),
            Some(space)
        );
        // A reply after the end explains it.
        row(&pool, s, "assistant", "2026-10-01T16:30:00Z", false).await;
        assert_eq!(
            repo.unexplained_offline(s, i, t("2026-10-01T17:00:00Z"), &kinds())
                .await
                .unwrap(),
            None
        );
    }

    /// An accepted choice of `session_id` whose separation is `event_id`.
    async fn accepted_choice(pool: &PgPool, session_id: Uuid, event_id: Uuid) {
        choice_with(pool, session_id, event_id, "accepted").await;
    }

    async fn choice_with(pool: &PgPool, session_id: Uuid, event_id: Uuid, status: &str) {
        sqlx::query(
            "INSERT INTO engine.life_choices \
               (session_id, kind, status, tier_gap, asked_at, resolved_at, event_id) \
             VALUES ($1, 'ask_for_space', $3, 3, '2026-10-01T09:00:00Z', \
                     '2026-10-01T09:00:00Z', $2)",
        )
        .bind(session_id)
        .bind(event_id)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
    }

    /// A separation of the persona written in `session_id` between `from` and `to`.
    async fn separation(
        repo: &EventRepo<'_>,
        instance: Uuid,
        session_id: Uuid,
        key: &str,
        from: &str,
        to: &str,
    ) -> EventRecord {
        repo.insert_preempting(&ev(instance, session_id, key, TakingSpace, from, to))
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unreturned_separation_stands_from_its_end_until_the_first_reply_with_text(
        pool: PgPool,
    ) {
        let (_, i, s) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        let space = separation(
            &repo,
            i,
            s,
            "space",
            "2026-10-01T10:00:00Z",
            "2026-10-01T16:00:00Z",
        )
        .await;
        accepted_choice(&pool, s, space.id).await;
        let read = |at: &str| {
            let repo = EventRepo { pool: &pool };
            let at = t(at);
            async move { repo.unreturned_separation(s, i, at).await.unwrap() }
        };

        // Under way: nothing to mark yet.
        assert_eq!(read("2026-10-01T15:59:59.999999Z").await, None);
        // Back, and nobody wrote: still the one to mark.
        assert_eq!(
            read("2026-10-01T16:00:00Z").await,
            Some((space.clone(), false))
        );
        // A reply inside the window, one exactly at its end, and one with no
        // text (an image alone) do not mark it.
        row(&pool, s, "assistant", "2026-10-01T12:00:00Z", false).await;
        row(&pool, s, "assistant", "2026-10-01T16:00:00Z", false).await;
        row_with(&pool, s, "assistant", "", "2026-10-01T16:30:00Z", false).await;
        assert_eq!(
            read("2026-10-01T17:00:00Z").await,
            Some((space.clone(), false))
        );
        // The first reply with text after it ended closes it, from its moment on.
        row(&pool, s, "assistant", "2026-10-01T18:00:00Z", false).await;
        assert_eq!(
            read("2026-10-01T17:59:59.999999Z").await,
            Some((space.clone(), false))
        );
        assert_eq!(read("2026-10-01T18:00:00Z").await, None);
        assert_eq!(read("2026-10-02T09:00:00Z").await, None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unreturned_separation_flags_only_a_ghosted_row_inside_the_window(pool: PgPool) {
        let (owner, i, s) = instance_and_session(&pool).await;
        let sibling = another_session(&pool, owner, i).await;
        let repo = EventRepo { pool: &pool };
        let space = separation(
            &repo,
            i,
            s,
            "space",
            "2026-10-01T10:00:00Z",
            "2026-10-01T16:00:00Z",
        )
        .await;
        accepted_choice(&pool, s, space.id).await;
        let at = t("2026-10-01T17:00:00Z");
        let flag = || async {
            repo.unreturned_separation(s, i, at)
                .await
                .unwrap()
                .unwrap()
                .1
        };

        // A message that was answered, one before it began, one at its end,
        // one after it, and another session's ghosted row: none is the user
        // writing into the silence.
        row(&pool, s, "user", "2026-10-01T11:00:00Z", false).await;
        row(&pool, s, "user", "2026-10-01T09:59:59.999999Z", true).await;
        row(&pool, s, "user", "2026-10-01T16:00:00Z", true).await;
        row(&pool, s, "user", "2026-10-01T16:30:00Z", true).await;
        row(&pool, sibling, "user", "2026-10-01T11:00:00Z", true).await;
        assert!(!flag().await);
        // A ghosted row in the window is.
        row(&pool, s, "user", "2026-10-01T10:00:00Z", true).await;
        assert!(flag().await);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unreturned_separation_is_reached_only_through_this_sessions_accepted_choice(
        pool: PgPool,
    ) {
        let (owner, i, s) = instance_and_session(&pool).await;
        let sibling = another_session(&pool, owner, i).await;
        let repo = EventRepo { pool: &pool };
        let at = t("2026-10-01T17:00:00Z");
        let space = separation(
            &repo,
            i,
            s,
            "space",
            "2026-10-01T10:00:00Z",
            "2026-10-01T16:00:00Z",
        )
        .await;
        // The separation is the instance's, but only the choice's session
        // owes the return.
        accepted_choice(&pool, s, space.id).await;
        assert!(repo
            .unreturned_separation(s, i, at)
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            repo.unreturned_separation(sibling, i, at).await.unwrap(),
            None
        );

        // A separation no choice points at, and one a choice that did not
        // end in an accept points at, are not reached.
        let (owner, i, s) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        let loose = separation(
            &repo,
            i,
            s,
            "loose",
            "2026-10-01T10:00:00Z",
            "2026-10-01T16:00:00Z",
        )
        .await;
        assert_eq!(repo.unreturned_separation(s, i, at).await.unwrap(), None);
        choice_with(&pool, s, loose.id, "declined").await;
        assert_eq!(repo.unreturned_separation(s, i, at).await.unwrap(), None);

        // Only a separation: another kind a choice points at is not one.
        let s2 = another_session(&pool, owner, i).await;
        let away = repo
            .insert(
                &ev(
                    i,
                    s2,
                    "away",
                    AwayFromPhone,
                    "2026-10-01T16:30:00Z",
                    "2026-10-01T16:45:00Z",
                ),
                None,
            )
            .await
            .unwrap()
            .unwrap()
            .event;
        accepted_choice(&pool, s2, away.id).await;
        assert_eq!(repo.unreturned_separation(s2, i, at).await.unwrap(), None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unreturned_separation_reads_the_newest_inside_the_lookback(pool: PgPool) {
        let (_, i, s) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        let older = separation(
            &repo,
            i,
            s,
            "older",
            "2026-10-01T10:00:00Z",
            "2026-10-01T16:00:00Z",
        )
        .await;
        accepted_choice(&pool, s, older.id).await;
        // 90 days of lookback, counted from the start.
        assert_eq!(
            repo.unreturned_separation(s, i, t("2026-12-30T09:59:59Z"))
                .await
                .unwrap(),
            Some((older.clone(), false))
        );
        assert_eq!(
            repo.unreturned_separation(s, i, t("2026-12-30T10:00:00Z"))
                .await
                .unwrap(),
            None
        );

        let newer = separation(
            &repo,
            i,
            s,
            "newer",
            "2026-10-05T10:00:00Z",
            "2026-10-05T16:00:00Z",
        )
        .await;
        accepted_choice(&pool, s, newer.id).await;
        assert_eq!(
            repo.unreturned_separation(s, i, t("2026-10-06T00:00:00Z"))
                .await
                .unwrap(),
            Some((newer, false))
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_non_offline_kind_for_this_persona_is_invisible_to_the_query(pool: PgPool) {
        let (_, i, s) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        let asleep = repo
            .insert(
                &ev(
                    i,
                    s,
                    "asleep",
                    Asleep,
                    "2026-10-01T02:00:00Z",
                    "2026-10-01T08:00:00Z",
                ),
                None,
            )
            .await
            .unwrap()
            .unwrap()
            .event;
        let shopping = repo
            .insert(
                &ev(
                    i,
                    s,
                    "shopping",
                    OutShopping,
                    "2026-10-01T14:00:00Z",
                    "2026-10-01T16:00:00Z",
                ),
                None,
            )
            .await
            .unwrap()
            .unwrap()
            .event;
        row(&pool, s, "user", "2026-10-01T05:00:00Z", true).await;
        row(&pool, s, "user", "2026-10-01T15:00:00Z", true).await;
        let at = t("2026-10-01T17:00:00Z");
        // For a persona who stayed reachable while shopping, the newest row is not
        // in the query at all, so the older genuine stretch is found.
        assert_eq!(
            repo.unexplained_offline(s, i, at, &kinds()).await.unwrap(),
            Some(asleep)
        );
        // For a female persona the shopping trip is the newest offline stretch.
        assert_eq!(
            repo.unexplained_offline(s, i, at, &female_kinds())
                .await
                .unwrap(),
            Some(shopping)
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn insert_preempting_caps_the_running_incident(pool: PgPool) {
        let (_, i, s) = instance_and_session(&pool).await;
        let (_, other_i, other_s) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        let wrote = |e: Option<Inserted>| e.expect("written").event;
        let trip = wrote(
            repo.insert(
                &ev(
                    i,
                    s,
                    "trip",
                    BusinessTrip,
                    "2026-10-01T09:00:00Z",
                    "2026-10-02T09:00:00Z",
                ),
                None,
            )
            .await
            .unwrap(),
        );
        let earlier = wrote(
            repo.insert(
                &ev(
                    i,
                    s,
                    "gaming",
                    Gaming,
                    "2026-10-01T09:30:00Z",
                    "2026-10-01T09:50:00Z",
                ),
                None,
            )
            .await
            .unwrap(),
        );
        wrote(
            repo.insert(
                &ev(
                    i,
                    s,
                    "reading",
                    Reading,
                    "2026-10-01T10:00:00Z",
                    "2026-10-01T14:00:00Z",
                ),
                None,
            )
            .await
            .unwrap(),
        );
        let elsewhere = wrote(
            repo.insert(
                &ev(
                    other_i,
                    other_s,
                    "reading",
                    Reading,
                    "2026-10-01T10:00:00Z",
                    "2026-10-01T14:00:00Z",
                ),
                None,
            )
            .await
            .unwrap(),
        );

        let space = repo
            .insert_preempting(&ev(
                i,
                s,
                "space",
                TakingSpace,
                "2026-10-01T11:00:00Z",
                "2026-10-01T17:00:00Z",
            ))
            .await
            .unwrap();
        assert_eq!(
            (
                space.kind,
                space.session_id,
                space.started_at,
                space.ends_at,
                space.message_id
            ),
            (
                TakingSpace,
                Some(s),
                t("2026-10-01T11:00:00Z"),
                t("2026-10-01T17:00:00Z"),
                None
            )
        );
        let ends = |k: &str| {
            let k = k.to_string();
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, DateTime<Utc>>(
                    "SELECT ends_at FROM engine.persona_events WHERE session_id = $1 AND event_key = $2",
                )
                .bind(s)
                .bind(k)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        assert_eq!(
            ends("reading").await,
            t("2026-10-01T11:00:00Z"),
            "the running incident is capped at the new one's start"
        );
        assert_eq!(
            ends("trip").await,
            trip.ends_at,
            "a background keeps running"
        );
        assert_eq!(
            ends("gaming").await,
            earlier.ends_at,
            "a finished one is left"
        );
        let other: DateTime<Utc> =
            sqlx::query_scalar("SELECT ends_at FROM engine.persona_events WHERE id = $1")
                .bind(elsewhere.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(other, elsewhere.ends_at, "another instance is untouched");

        let c = repo
            .current(i, None, t("2026-10-01T12:00:00Z"))
            .await
            .unwrap();
        assert_eq!(
            (c.foreground.map(|e| e.kind), c.background.map(|e| e.id)),
            (Some(TakingSpace), Some(trip.id)),
            "one incident at a time, the trip underneath"
        );
    }

    /// (events of the instance, rows of the message id).
    async fn counts(pool: &PgPool, instance: Uuid, message_id: Uuid) -> (i64, i64) {
        let events =
            sqlx::query_scalar("SELECT count(*) FROM engine.persona_events WHERE instance_id = $1")
                .bind(instance)
                .fetch_one(pool)
                .await
                .unwrap();
        let messages =
            sqlx::query_scalar("SELECT count(*) FROM engine.chat_messages WHERE id = $1")
                .bind(message_id)
                .fetch_one(pool)
                .await
                .unwrap();
        (events, messages)
    }

    /// While she waits for an answer to an ask, in any session of the
    /// instance, a draw writes nothing: not the event, not its message. The
    /// route's own check only sees an ask that landed before it ran; this is
    /// the one that closes the gap.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_draw_is_refused_while_an_ask_is_pending_in_a_sibling_session(pool: PgPool) {
        use crate::life_choices::LifeChoiceRepo;

        let (owner, i, s) = instance_and_session(&pool).await;
        let sibling = another_session(&pool, owner, i).await;
        let (_, other_i, other_s) = instance_and_session(&pool).await;
        let asks = LifeChoiceRepo { pool: &pool };
        let ask = asks
            .insert_ask(sibling, 3, None, &message("想先分开"))
            .await
            .unwrap()
            .expect("asked");
        let repo = EventRepo { pool: &pool };
        let now = Utc::now();
        let draw = |instance, session, key| NewEvent {
            instance_id: instance,
            session_id: session,
            event_key: key,
            kind: TempWork,
            started_at: now,
            ends_at: now + chrono::Duration::hours(4),
        };

        let told = message("刚被叫去加班");
        assert!(repo
            .insert(&draw(i, s, "work"), Some(&told))
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            counts(&pool, i, told.id).await,
            (0, 0),
            "no event and no message"
        );
        assert!(
            repo.insert(&draw(other_i, other_s, "work"), None)
                .await
                .unwrap()
                .is_some(),
            "another instance's ask has no say"
        );

        // Answered, or lapsed: the draw goes through again.
        asks.resolve(ask.id, "declined", now)
            .await
            .unwrap()
            .expect("resolved");
        assert!(repo
            .insert(&draw(i, s, "work"), Some(&told))
            .await
            .unwrap()
            .is_some());
        assert_eq!(counts(&pool, i, told.id).await, (1, 1));
        sqlx::query("DELETE FROM engine.persona_events WHERE instance_id = $1")
            .bind(i)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE engine.life_choices SET status = 'pending', resolved_at = NULL, \
                    asked_at = $2 WHERE id = $1",
        )
        .bind(ask.id)
        .bind(now - chrono::Duration::hours(25))
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            repo.insert(&draw(i, s, "work2"), None)
                .await
                .unwrap()
                .is_some(),
            "a lapsed ask does not hold a draw"
        );
    }

    /// An accept in flight (its cap run, its separation written, nothing
    /// committed) is invisible to a draw's guard snapshot. The draw waits for
    /// the instance's lock instead, then reads the committed separation and
    /// lands nothing: one foreground at a time.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_draw_waits_for_an_accept_in_flight_and_then_yields_to_it(pool: PgPool) {
        let (_, i, s) = instance_and_session(&pool).await;
        let mut accepting = pool.begin().await.unwrap();
        let space = EventRepo::insert_preempting_in_tx(
            &mut accepting,
            &ev(
                i,
                s,
                "space",
                TakingSpace,
                "2026-10-01T10:00:00Z",
                "2026-10-01T22:00:00Z",
            ),
        )
        .await
        .unwrap();

        let told = message("刚被叫去加班");
        let drawing = tokio::spawn({
            let (pool, told) = (pool.clone(), told.clone());
            async move {
                EventRepo { pool: &pool }
                    .insert(
                        &ev(
                            i,
                            s,
                            "work",
                            TempWork,
                            "2026-10-01T11:00:00Z",
                            "2026-10-01T15:00:00Z",
                        ),
                        Some(&told),
                    )
                    .await
                    .unwrap()
                    .map(|w| w.event.id)
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !drawing.is_finished(),
            "the draw waits for the accept's lock"
        );
        accepting.commit().await.unwrap();

        assert!(
            drawing.await.unwrap().is_none(),
            "and then sees the separation under way"
        );
        assert_eq!(
            counts(&pool, i, told.id).await,
            (1, 0),
            "only the separation; the draw's message went with it"
        );
        let c = EventRepo { pool: &pool }
            .current(i, None, t("2026-10-01T11:00:00Z"))
            .await
            .unwrap();
        assert_eq!(c.foreground.map(|e| e.id), Some(space.id));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_scare_is_resolved_where_it_was_told(pool: PgPool) {
        let (owner, i, s) = instance_and_session(&pool).await;
        let elsewhere = another_session(&pool, owner, i).await;
        let repo = EventRepo { pool: &pool };
        let scare = repo
            .insert(
                &ev(
                    i,
                    s,
                    "scare",
                    PeriodScare,
                    "2026-10-01T10:00:00Z",
                    "2026-10-01T11:00:00Z",
                ),
                Some(&message("月经推迟了好几天")),
            )
            .await
            .unwrap()
            .unwrap()
            .event;
        // Not yet over.
        assert_eq!(
            repo.unresolved_scare(s, i, t("2026-10-01T10:30:00Z"))
                .await
                .unwrap(),
            None
        );
        // Over, in the session that told it; never in another.
        assert_eq!(
            repo.unresolved_scare(s, i, t("2026-10-01T12:00:00Z"))
                .await
                .unwrap(),
            Some(scare.clone())
        );
        assert_eq!(
            repo.unresolved_scare(elsewhere, i, t("2026-10-01T12:00:00Z"))
                .await
                .unwrap(),
            None
        );
        // A reply inside the scare does not resolve it.
        row(&pool, s, "assistant", "2026-10-01T10:40:00Z", false).await;
        assert_eq!(
            repo.unresolved_scare(s, i, t("2026-10-01T12:00:00Z"))
                .await
                .unwrap(),
            Some(scare)
        );
        // The first one after it does.
        row(&pool, s, "assistant", "2026-10-01T11:30:00Z", false).await;
        assert_eq!(
            repo.unresolved_scare(s, i, t("2026-10-01T12:00:00Z"))
                .await
                .unwrap(),
            None
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_image_only_reply_does_not_explain_or_resolve(pool: PgPool) {
        let (_, i, s) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        let away = repo
            .insert(
                &ev(
                    i,
                    s,
                    "away",
                    AwayFromPhone,
                    "2026-10-01T10:00:00Z",
                    "2026-10-01T12:00:00Z",
                ),
                None,
            )
            .await
            .unwrap()
            .unwrap()
            .event;
        let scare = repo
            .insert(
                &ev(
                    i,
                    s,
                    "scare",
                    PeriodScare,
                    "2026-10-01T12:00:00Z",
                    "2026-10-01T13:00:00Z",
                ),
                Some(&message("月经推迟了好几天")),
            )
            .await
            .unwrap()
            .unwrap()
            .event;
        row(&pool, s, "user", "2026-10-01T11:00:00Z", true).await;
        let at = t("2026-10-01T15:00:00Z");
        // An image-only reply after both ends carries no `刚才` line.
        row_with(&pool, s, "assistant", "", "2026-10-01T14:00:00Z", false).await;
        assert_eq!(
            repo.unexplained_offline(s, i, at, &kinds()).await.unwrap(),
            Some(away)
        );
        assert_eq!(repo.unresolved_scare(s, i, at).await.unwrap(), Some(scare));
        // One with text does.
        row_with(&pool, s, "assistant", "刚醒", "2026-10-01T14:30:00Z", false).await;
        assert_eq!(
            repo.unexplained_offline(s, i, at, &kinds()).await.unwrap(),
            None
        );
        assert_eq!(repo.unresolved_scare(s, i, at).await.unwrap(), None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_newer_scare_elsewhere_does_not_hide_this_one(pool: PgPool) {
        let (owner, i, s) = instance_and_session(&pool).await;
        let elsewhere = another_session(&pool, owner, i).await;
        let repo = EventRepo { pool: &pool };
        let here = repo
            .insert(
                &ev(
                    i,
                    s,
                    "here",
                    PeriodScare,
                    "2026-10-01T10:00:00Z",
                    "2026-10-01T11:00:00Z",
                ),
                Some(&message("月经推迟了好几天")),
            )
            .await
            .unwrap()
            .unwrap()
            .event;
        repo.insert(
            &ev(
                i,
                elsewhere,
                "there",
                PregnancyScare,
                "2026-10-01T12:00:00Z",
                "2026-10-01T13:00:00Z",
            ),
            Some(&message("验出两条杠")),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            repo.unresolved_scare(s, i, t("2026-10-01T14:00:00Z"))
                .await
                .unwrap(),
            Some(here)
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn only_false_alarms_are_resolved(pool: PgPool) {
        let (_, i, s) = instance_and_session(&pool).await;
        let repo = EventRepo { pool: &pool };
        repo.insert(
            &ev(
                i,
                s,
                "w",
                WeatherChange,
                "2026-10-01T10:00:00Z",
                "2026-10-01T11:00:00Z",
            ),
            Some(&message("下雨了")),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            repo.unresolved_scare(s, i, t("2026-10-01T12:00:00Z"))
                .await
                .unwrap(),
            None
        );
    }

    #[sqlx::test(migrations = false)]
    async fn the_backfill_copies_v1_events_and_leaves_them_in_place(pool: PgPool) {
        use sqlx::migrate::Migrate;
        let migrator = sqlx::migrate!("./migrations");
        let mut conn = pool.acquire().await.unwrap();
        conn.ensure_migrations_table().await.unwrap();
        for m in migrator.iter().filter(|m| m.version < 72) {
            conn.apply(m).await.unwrap();
        }
        drop(conn);
        let (_, _, session) = instance_and_session(&pool).await;
        let insert_v1 = |meta: serde_json::Value| {
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO engine.chat_messages \
                   (session_id, role, content, assistant_action_type, metadata, sent_at) \
                 VALUES ($1, 'assistant', '下雨了', 'proactive', $2, '2026-10-01T10:00:00Z') \
                 RETURNING id",
            )
            .bind(session)
            .bind(meta)
            .fetch_one(&pool)
        };
        let id = insert_v1(json!({"proactive": "event", "event_key": "k1",
            "event": {"kind": "weather_change", "ends_at": "2026-10-01T14:00:00Z"}}))
        .await
        .unwrap();
        insert_v1(json!({"proactive": "event", "event_key": "k2", "event": {"kind": "temp_work"}}))
            .await
            .unwrap();

        let mut conn = pool.acquire().await.unwrap();
        for m in migrator.iter().filter(|m| m.version == 72) {
            conn.apply(m).await.unwrap();
        }
        drop(conn);

        let repo = EventRepo { pool: &pool };
        let e = repo.by_key(session, "k1").await.unwrap().expect("copied");
        assert_eq!(
            (e.kind, e.started_at, e.ends_at, e.message_id),
            (
                WeatherChange,
                t("2026-10-01T10:00:00Z"),
                t("2026-10-01T14:00:00Z"),
                Some(id)
            )
        );
        assert!(
            repo.by_key(session, "k2").await.unwrap().is_none(),
            "no ends_at, not copied"
        );
        let meta: serde_json::Value =
            sqlx::query_scalar("SELECT metadata FROM engine.chat_messages WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(meta["event"]["kind"], json!("weather_change"));
        assert_eq!(meta["event_key"], json!("k1"));
        let index: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_indexes \
              WHERE schemaname = 'engine' AND indexname = 'chat_messages_event_key_uidx')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(index);
    }

    #[sqlx::test(migrations = false)]
    async fn the_v1_event_metadata_and_its_index_are_dropped(pool: PgPool) {
        use sqlx::migrate::Migrate;
        let migrator = sqlx::migrate!("./migrations");
        let mut conn = pool.acquire().await.unwrap();
        conn.ensure_migrations_table().await.unwrap();
        for m in migrator.iter().filter(|m| m.version < 72) {
            conn.apply(m).await.unwrap();
        }
        drop(conn);
        let (_, _, session) = instance_and_session(&pool).await;
        let id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.chat_messages \
               (session_id, role, content, assistant_action_type, metadata, sent_at) \
             VALUES ($1, 'assistant', '下雨了', 'proactive', $2, '2026-10-01T10:00:00Z') \
             RETURNING id",
        )
        .bind(session)
        .bind(
            json!({"proactive": "event", "locale": "zh", "event_key": "k1",
            "event": {"kind": "weather_change", "ends_at": "2026-10-01T14:00:00Z"}}),
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let mut conn = pool.acquire().await.unwrap();
        for m in migrator.iter().filter(|m| m.version >= 72) {
            conn.apply(m).await.unwrap();
        }
        drop(conn);

        let meta: serde_json::Value =
            sqlx::query_scalar("SELECT metadata FROM engine.chat_messages WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(meta, json!({"proactive": "event", "locale": "zh"}));
        let e = EventRepo { pool: &pool }
            .by_key(session, "k1")
            .await
            .unwrap()
            .expect("the event row stays");
        assert_eq!((e.kind, e.message_id), (WeatherChange, Some(id)));
        let index: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_indexes \
              WHERE schemaname = 'engine' AND indexname = 'chat_messages_event_key_uidx')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!index);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn ever_sees_rows_older_than_lookback(pool: PgPool) {
        let repo = EventRepo { pool: &pool };
        let (_, a, sa) = instance_and_session(&pool).await;
        let (_, b, _) = instance_and_session(&pool).await;
        let long_ago = Utc::now() - chrono::Duration::days(200);
        repo.insert_preempting(&NewEvent {
            instance_id: a,
            session_id: sa,
            event_key: "k1",
            kind: HavingSex,
            started_at: long_ago,
            ends_at: long_ago + chrono::Duration::hours(1),
        })
        .await
        .unwrap();
        assert!(repo.ever(a, HavingSex).await.unwrap());
        assert!(!repo.ever(a, Reading).await.unwrap());
        assert!(!repo.ever(b, HavingSex).await.unwrap());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn backgrounds_at_maps_each_instance(pool: PgPool) {
        let repo = EventRepo { pool: &pool };
        let (_, a, sa) = instance_and_session(&pool).await;
        let (_, b, sb) = instance_and_session(&pool).await;
        let (_, c, sc) = instance_and_session(&pool).await;
        let (_, d, sd) = instance_and_session(&pool).await;
        let now = t("2026-10-23T08:00:00Z");
        for e in [
            ev(
                a,
                sa,
                "k1",
                Hospitalized,
                "2026-10-22T08:00:00Z",
                "2026-10-25T08:00:00Z",
            ),
            ev(
                a,
                sa,
                "k2",
                Reading,
                "2026-10-23T07:50:00Z",
                "2026-10-23T09:00:00Z",
            ),
            ev(
                b,
                sb,
                "k3",
                BusinessTrip,
                "2026-10-21T08:00:00Z",
                "2026-10-22T08:00:00Z",
            ),
            ev(
                c,
                sc,
                "k4",
                Sick,
                "2026-10-22T08:00:00Z",
                "2026-10-24T08:00:00Z",
            ),
            ev(
                d,
                sd,
                "k5",
                Busy,
                "2026-10-23T07:00:00Z",
                "2026-10-23T10:00:00Z",
            ),
        ] {
            repo.insert_preempting(&e).await.unwrap();
        }
        let got = repo.backgrounds_at(&[a, b, d], now).await.unwrap();
        assert_eq!(
            got.len(),
            1,
            "only a has a background under way among a, b, d"
        );
        assert_eq!(got[&a].kind, Hospitalized);
    }
}
