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
    /// operator needs (`companion_decision_events`, `llm_attempt_audit`,
    /// `chat_vision_events`, `chat_images_events`) hold `session_id` without a
    /// foreign key and are untouched; with the transcript now surviving too,
    /// those records finally point at something readable.
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
