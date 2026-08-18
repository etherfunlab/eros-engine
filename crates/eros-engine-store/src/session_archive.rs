// SPDX-License-Identifier: AGPL-3.0-only

//! Soft-deleting a relationship's sessions.
//!
//! A client asking to clear a conversation wants two things that the schema
//! used to fuse into one `DELETE`: the conversation should stop being visible
//! and resumable, and the relationship state should reset so a fresh start is
//! genuinely fresh. Only the second requires destroying rows.
//!
//! See docs/superpowers/specs/2026-08-18-session-soft-delete-design.md.

use sqlx::PgPool;
use uuid::Uuid;

pub struct SessionArchiveRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> SessionArchiveRepo<'a> {
    /// Archive every session this user holds with this persona instance, and
    /// delete the relationship state that would otherwise carry into the next
    /// conversation. Returns the number of sessions flipped by THIS call, so a
    /// repeat call reports 0 rather than re-reporting the first call's work.
    ///
    /// Every channel is archived, voice included: the unit is the
    /// relationship, not one client's view of it.
    ///
    /// Memories are deleted by `(user_id, instance_id)` rather than by session
    /// id because `MemoryRepo::search` retrieves on exactly that key — deleting
    /// by session would leave a row retrievable that the caller believed gone.
    /// The same predicate is what spares the profile layer: those rows carry
    /// `instance_id IS NULL`, and `instance_id = $2` never matches NULL. They
    /// are cross-persona facts about the user and ending one relationship must
    /// not erase them.
    ///
    /// `companion_affinity_events` goes with its parent through the existing
    /// ON DELETE CASCADE — accepted, not worked around. The audit tables an
    /// operator needs (`companion_decision_events`, `character_insights_events`,
    /// `chat_vision_events`, `chat_images_events`) hold `session_id` without a
    /// foreign key and are untouched; with the transcript now surviving too,
    /// those records still point at something readable.
    ///
    /// `persona_story_insights.digest` is cleared, not the row: the row's
    /// other columns (`occupation`, `city`, `life_rhythm`, ...) are the
    /// persona's own typed life base, live only in this one row, and deleting
    /// it would take them with it. `digest` is the one column that is instead
    /// conversation-derived and re-injected into every later reply, so it
    /// gets the same treatment as `character_insights` above without taking
    /// the rest of the row's life facts with it.
    ///
    /// The `character_insights` delete and the `persona_story_insights` update
    /// are both keyed on `instance_id` alone, unlike the two `user_id +
    /// instance_id` deletes above them. That's safe: `persona_instances.owner_uid`
    /// is never updated anywhere in this workspace (only ever set once, at
    /// INSERT), so the instance identifies its owner for the entire life of the
    /// row and there is no window in which `instance_id` alone could resolve to
    /// the wrong relationship.
    ///
    /// `persona_instances.status` is deliberately not written: the user still
    /// owns the persona, and ownership is the client's business.
    pub async fn archive_relationship(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
    ) -> Result<u64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        let archived = sqlx::query(
            "UPDATE engine.chat_sessions SET archived = true \
             WHERE user_id = $1 AND instance_id = $2 AND NOT archived",
        )
        .bind(user_id)
        .bind(instance_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        sqlx::query(
            "DELETE FROM engine.companion_memories \
             WHERE user_id = $1 AND instance_id = $2",
        )
        .bind(user_id)
        .bind(instance_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "DELETE FROM engine.companion_affinity \
             WHERE user_id = $1 AND instance_id = $2",
        )
        .bind(user_id)
        .bind(instance_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM engine.character_insights WHERE instance_id = $1")
            .bind(instance_id)
            .execute(&mut *tx)
            .await?;

        // UPDATE, not DELETE: this row also carries the persona's own typed
        // life base (occupation, city, ...), which lives only here — deleting
        // the row would take those with it. Only `digest` is this
        // relationship's record: it is conversation-derived and injected into
        // every subsequent reply, the same property that justifies deleting
        // character_insights above. Clearing it is enough on its own:
        // fetch_stories_context returns early on a blank digest before it
        // ever reads persona_story_memories, so the episode recall built up
        // during this relationship stops being injected too.
        sqlx::query("UPDATE engine.persona_story_insights SET digest = '' WHERE instance_id = $1")
            .bind(instance_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(archived)
    }
}

#[cfg(test)]
mod tests {
    use super::SessionArchiveRepo;
    use crate::chat::ChatRepo;
    use crate::testutil::seed_persona_instance;
    use sqlx::PgPool;
    use uuid::Uuid;

    /// Persist one session with one message, an affinity row, one
    /// relationship-layer memory and one profile-layer memory.
    async fn seed_relationship(pool: &PgPool, user_id: Uuid, instance_id: Uuid) -> Uuid {
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content) \
             VALUES ($1, 'user', 'hello')",
        )
        .bind(session_id)
        .execute(pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO engine.companion_affinity (session_id, user_id, instance_id) \
             VALUES ($1, $2, $3)",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(instance_id)
        .execute(pool)
        .await
        .unwrap();

        // Relationship layer (instance-scoped) and profile layer (instance_id
        // NULL, cross-persona). Both hang off this session by FK.
        let zeros = format!("[{}]", vec!["0"; 512].join(","));
        sqlx::query(
            "INSERT INTO engine.companion_memories \
                 (session_id, user_id, instance_id, content, embedding) \
             VALUES ($1, $2, $3, 'she likes jazz', $4::vector), \
                    ($1, $2, NULL,  'user is a vet', $4::vector)",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(instance_id)
        .bind(&zeros)
        .execute(pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO engine.character_insights (instance_id, location) \
             VALUES ($1, 'the office')",
        )
        .bind(instance_id)
        .execute(pool)
        .await
        .unwrap();

        session_id
    }

    async fn count(pool: &PgPool, sql: &str, id: Uuid) -> i64 {
        sqlx::query_scalar(sql)
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// The transcript is evidence: archiving flips a flag and leaves every
    /// chat_messages row where it is.
    #[sqlx::test(migrations = "./migrations")]
    async fn archives_sessions_and_keeps_the_transcript(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = seed_relationship(&pool, user_id, instance_id).await;

        let n = SessionArchiveRepo { pool: &pool }
            .archive_relationship(user_id, instance_id)
            .await
            .unwrap();
        assert_eq!(n, 1, "one session flipped");

        let archived: bool =
            sqlx::query_scalar("SELECT archived FROM engine.chat_sessions WHERE id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(archived);

        let messages = count(
            &pool,
            "SELECT count(*) FROM engine.chat_messages WHERE session_id = $1",
            session_id,
        )
        .await;
        assert_eq!(messages, 1, "transcript must survive the archive");
    }

    /// Relationship state is deleted so the next conversation starts cold —
    /// but the profile layer is a fact about the user, shared with every other
    /// companion, and must not be collateral.
    #[sqlx::test(migrations = "./migrations")]
    async fn deletes_relationship_state_but_spares_the_profile_layer(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        seed_relationship(&pool, user_id, instance_id).await;

        SessionArchiveRepo { pool: &pool }
            .archive_relationship(user_id, instance_id)
            .await
            .unwrap();

        let affinity = count(
            &pool,
            "SELECT count(*) FROM engine.companion_affinity WHERE instance_id = $1",
            instance_id,
        )
        .await;
        assert_eq!(affinity, 0, "affinity is deleted");

        let relationship_memories = count(
            &pool,
            "SELECT count(*) FROM engine.companion_memories WHERE instance_id = $1",
            instance_id,
        )
        .await;
        assert_eq!(
            relationship_memories, 0,
            "relationship-layer memories are deleted"
        );

        let profile_memories = count(
            &pool,
            "SELECT count(*) FROM engine.companion_memories \
             WHERE user_id = $1 AND instance_id IS NULL",
            user_id,
        )
        .await;
        assert_eq!(profile_memories, 1, "profile-layer memories must survive");

        let insights = count(
            &pool,
            "SELECT count(*) FROM engine.character_insights WHERE instance_id = $1",
            instance_id,
        )
        .await;
        assert_eq!(insights, 0, "character insights are deleted");
    }

    /// Every channel goes, and a second call is a no-op that still succeeds.
    #[sqlx::test(migrations = "./migrations")]
    async fn archives_every_channel_and_is_idempotent(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        seed_relationship(&pool, user_id, instance_id).await;
        sqlx::query(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, channel) \
             VALUES ($1, $2, 'voice')",
        )
        .bind(user_id)
        .bind(instance_id)
        .execute(&pool)
        .await
        .unwrap();

        let repo = SessionArchiveRepo { pool: &pool };
        assert_eq!(
            repo.archive_relationship(user_id, instance_id)
                .await
                .unwrap(),
            2,
            "text and voice sessions both archived"
        );
        assert_eq!(
            repo.archive_relationship(user_id, instance_id)
                .await
                .unwrap(),
            0,
            "second call finds nothing left to flip"
        );
    }

    /// Another user's relationship with the same instance is untouched — the
    /// predicate is (user_id, instance_id), not instance_id alone.
    #[sqlx::test(migrations = "./migrations")]
    async fn leaves_another_users_sessions_alone(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let other = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        seed_relationship(&pool, user_id, instance_id).await;
        let other_session: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(other)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        SessionArchiveRepo { pool: &pool }
            .archive_relationship(user_id, instance_id)
            .await
            .unwrap();

        let archived: bool =
            sqlx::query_scalar("SELECT archived FROM engine.chat_sessions WHERE id = $1")
                .bind(other_session)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!archived, "another user's session must stay visible");
    }

    /// The other half of the predicate: `(user_id, instance_id)`, not
    /// `user_id` alone. Without `AND instance_id = $2` on the
    /// `companion_memories` delete, archiving one relationship would erase
    /// every relationship-layer memory the user holds with every companion —
    /// and `leaves_another_users_sessions_alone` above would not catch it,
    /// since that test only varies `user_id`.
    #[sqlx::test(migrations = "./migrations")]
    async fn leaves_the_same_users_other_instance_alone(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_a = seed_persona_instance(&pool, user_id).await;
        let instance_b = seed_persona_instance(&pool, user_id).await;
        seed_relationship(&pool, user_id, instance_a).await;
        let session_b = seed_relationship(&pool, user_id, instance_b).await;

        SessionArchiveRepo { pool: &pool }
            .archive_relationship(user_id, instance_a)
            .await
            .unwrap();

        let archived: bool =
            sqlx::query_scalar("SELECT archived FROM engine.chat_sessions WHERE id = $1")
                .bind(session_b)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            !archived,
            "the other instance's session must stay unarchived"
        );

        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM engine.companion_memories WHERE instance_id = $1",
                instance_b,
            )
            .await,
            1,
            "the other instance's relationship-layer memory must survive"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM engine.companion_affinity WHERE instance_id = $1",
                instance_b,
            )
            .await,
            1,
            "the other instance's affinity row must survive"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM engine.character_insights WHERE instance_id = $1",
                instance_b,
            )
            .await,
            1,
            "the other instance's character insight row must survive"
        );
    }

    /// The API-facing choke point. Every entry-point guard resolves a session
    /// through get_session, so this one predicate is what turns history,
    /// affinity, chat stream and voice turn into 404 for an archived session.
    #[sqlx::test(migrations = "./migrations")]
    async fn get_session_hides_archived_sessions(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = seed_relationship(&pool, user_id, instance_id).await;
        let chat = ChatRepo { pool: &pool };

        assert!(chat.get_session(session_id).await.unwrap().is_some());

        SessionArchiveRepo { pool: &pool }
            .archive_relationship(user_id, instance_id)
            .await
            .unwrap();

        assert!(
            chat.get_session(session_id).await.unwrap().is_none(),
            "archived session must not resolve"
        );

        // Revival is an operator UPDATE, and it is the whole restore story.
        sqlx::query("UPDATE engine.chat_sessions SET archived = false WHERE id = $1")
            .bind(session_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            chat.get_session(session_id).await.unwrap().is_some(),
            "un-archiving must bring the session back"
        );
    }

    /// The critical one: without this predicate the next chat/start reanimates
    /// the archived session and the user gets their deleted conversation back
    /// with a blank relationship.
    #[sqlx::test(migrations = "./migrations")]
    async fn resume_never_returns_an_archived_session(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        seed_relationship(&pool, user_id, instance_id).await;
        let chat = ChatRepo { pool: &pool };

        SessionArchiveRepo { pool: &pool }
            .archive_relationship(user_id, instance_id)
            .await
            .unwrap();

        assert!(
            chat.resume_latest_session(user_id, instance_id, "text")
                .await
                .unwrap()
                .is_none(),
            "resume must fall through to creating a new session"
        );
        // `ChatSession` carries no `archived` field and none should be added —
        // nothing in Rust reads it — so assert against the column directly.
        let resumed = chat.create_or_resume(user_id, instance_id).await.unwrap();
        let is_archived: bool =
            sqlx::query_scalar("SELECT archived FROM engine.chat_sessions WHERE id = $1")
                .bind(resumed.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            !is_archived,
            "create_or_resume must produce a fresh, unarchived session"
        );
    }

    /// The story digest is instance-keyed and injected into every subsequent
    /// reply, so it survives the archive the same way `character_insights`
    /// used to — clearing it is the fifth statement. `persona_story_events`
    /// and `persona_story_memories` are the persona's own life, not this
    /// relationship's record, so they must survive; that pair is the
    /// assertion that catches someone later "tidying" the UPDATE into a
    /// DELETE.
    #[sqlx::test(migrations = "./migrations")]
    async fn clears_the_story_digest_but_keeps_the_persona_life(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        seed_relationship(&pool, user_id, instance_id).await;

        sqlx::query(
            "INSERT INTO engine.persona_story_insights \
                 (instance_id, owner_uid, occupation, digest) \
             VALUES ($1, $2, 'barista', 'opened a coffee shop this week')",
        )
        .bind(instance_id)
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();

        let event_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_story_events \
                 (owner_uid, instance_id, category, content, story_date) \
             VALUES ($1, $2, 'life', 'the espresso machine broke on opening day', CURRENT_DATE) \
             RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let zeros = format!("[{}]", vec!["0"; 512].join(","));
        sqlx::query(
            "INSERT INTO engine.persona_story_memories \
                 (owner_uid, instance_id, event_id, content, embedding, story_date) \
             VALUES ($1, $2, $3, 'the espresso machine broke on opening day', $4::vector, CURRENT_DATE)",
        )
        .bind(user_id)
        .bind(instance_id)
        .bind(event_id)
        .bind(&zeros)
        .execute(&pool)
        .await
        .unwrap();

        SessionArchiveRepo { pool: &pool }
            .archive_relationship(user_id, instance_id)
            .await
            .unwrap();

        let (digest, occupation): (String, Option<String>) = sqlx::query_as(
            "SELECT digest, occupation FROM engine.persona_story_insights \
             WHERE instance_id = $1",
        )
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(digest, "", "story digest must be cleared");
        assert_eq!(
            occupation.as_deref(),
            Some("barista"),
            "retained scalar insight columns must survive"
        );

        let events = count(
            &pool,
            "SELECT count(*) FROM engine.persona_story_events WHERE instance_id = $1",
            instance_id,
        )
        .await;
        assert_eq!(events, 1, "the persona's life-progression log must survive");

        let memories = count(
            &pool,
            "SELECT count(*) FROM engine.persona_story_memories WHERE instance_id = $1",
            instance_id,
        )
        .await;
        assert_eq!(memories, 1, "the story recall mirror must survive");
    }

    /// The session list is a read surface like any other.
    #[sqlx::test(migrations = "./migrations")]
    async fn list_sessions_hides_archived_sessions(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        seed_relationship(&pool, user_id, instance_id).await;
        let chat = ChatRepo { pool: &pool };

        assert_eq!(chat.list_sessions(user_id).await.unwrap().len(), 1);

        SessionArchiveRepo { pool: &pool }
            .archive_relationship(user_id, instance_id)
            .await
            .unwrap();

        assert!(
            chat.list_sessions(user_id).await.unwrap().is_empty(),
            "archived sessions must not appear in the list"
        );
    }
}
