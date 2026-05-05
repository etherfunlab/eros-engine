// SPDX-License-Identifier: AGPL-3.0-only
//! Chat session + message persistence.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChatSession {
    pub id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Option<Uuid>,
    pub lead_score: f64,
    pub is_converted: bool,
    pub last_active_at: DateTime<Utc>,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChatMessage {
    pub id: Uuid,
    pub session_id: Uuid,
    pub role: String,
    pub content: String,
    pub extracted_facts: Option<serde_json::Value>,
    pub sent_at: DateTime<Utc>,
}

pub struct ChatRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> ChatRepo<'a> {
    /// Create a new chat session for `user_id` × `instance_id`.
    pub async fn create_session(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
    ) -> Result<ChatSession, sqlx::Error> {
        sqlx::query_as::<_, ChatSession>(
            "INSERT INTO chat_sessions (user_id, instance_id) \
             VALUES ($1, $2) \
             RETURNING *",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(self.pool)
        .await
    }

    /// Look up a session by id.
    pub async fn get_session(&self, session_id: Uuid) -> Result<Option<ChatSession>, sqlx::Error> {
        sqlx::query_as::<_, ChatSession>("SELECT * FROM chat_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(self.pool)
            .await
    }

    /// Resume the most recent session for a user×instance pair, or create a new one.
    pub async fn create_or_resume(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
    ) -> Result<ChatSession, sqlx::Error> {
        if let Some(existing) = sqlx::query_as::<_, ChatSession>(
            "SELECT * FROM chat_sessions \
             WHERE user_id = $1 AND instance_id = $2 \
             ORDER BY last_active_at DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_optional(self.pool)
        .await?
        {
            sqlx::query("UPDATE chat_sessions SET last_active_at = now() WHERE id = $1")
                .bind(existing.id)
                .execute(self.pool)
                .await?;
            return Ok(existing);
        }
        self.create_session(user_id, instance_id).await
    }

    /// Append a message to a session and bump `last_active_at`.
    pub async fn append_message(
        &self,
        session_id: Uuid,
        role: &str,
        content: &str,
    ) -> Result<Uuid, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO chat_messages (session_id, role, content) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(session_id)
        .bind(role)
        .bind(content)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("UPDATE chat_sessions SET last_active_at = now() WHERE id = $1")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Fetch chat history in chronological (ascending) order.
    pub async fn history(
        &self,
        session_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ChatMessage>, sqlx::Error> {
        // We pull DESC + LIMIT/OFFSET (most recent N messages, paged
        // backwards), then reverse to ASC for the caller. This matches the
        // gateway's `get_history` semantics.
        let mut rows = sqlx::query_as::<_, ChatMessage>(
            "SELECT * FROM chat_messages \
             WHERE session_id = $1 \
             ORDER BY sent_at DESC \
             LIMIT $2 OFFSET $3",
        )
        .bind(session_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(self.pool)
        .await?;
        rows.reverse();
        Ok(rows)
    }

    /// All sessions belonging to a user, most-recently-active first.
    pub async fn list_sessions(&self, user_id: Uuid) -> Result<Vec<ChatSession>, sqlx::Error> {
        sqlx::query_as::<_, ChatSession>(
            "SELECT * FROM chat_sessions \
             WHERE user_id = $1 \
             ORDER BY last_active_at DESC",
        )
        .bind(user_id)
        .fetch_all(self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "./migrations")]
    async fn create_then_retrieve_session(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let s = repo.create_session(user_id, instance_id).await.unwrap();
        let loaded = repo.get_session(s.id).await.unwrap().unwrap();
        assert_eq!(loaded.user_id, user_id);
        assert_eq!(loaded.instance_id, Some(instance_id));
        assert_eq!(loaded.lead_score, 0.0);
        assert!(!loaded.is_converted);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn append_message_and_history_roundtrip(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let s = repo.create_session(user_id, instance_id).await.unwrap();

        repo.append_message(s.id, "user", "hello").await.unwrap();
        repo.append_message(s.id, "assistant", "hi there")
            .await
            .unwrap();
        repo.append_message(s.id, "user", "how are you?")
            .await
            .unwrap();

        let history = repo.history(s.id, 50, 0).await.unwrap();
        assert_eq!(history.len(), 3);
        // Chronological: first appended first.
        assert_eq!(history[0].role, "user");
        assert_eq!(history[0].content, "hello");
        assert_eq!(history[1].role, "assistant");
        assert_eq!(history[2].content, "how are you?");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn list_sessions_for_user(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let other_user = Uuid::new_v4();

        let i1 = Uuid::new_v4();
        let i2 = Uuid::new_v4();
        let i3 = Uuid::new_v4();

        repo.create_session(user_id, i1).await.unwrap();
        repo.create_session(user_id, i2).await.unwrap();
        repo.create_session(other_user, i3).await.unwrap();

        let sessions = repo.list_sessions(user_id).await.unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().all(|s| s.user_id == user_id));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn create_or_resume_returns_existing(pool: PgPool) {
        let repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let first = repo.create_session(user_id, instance_id).await.unwrap();
        let resumed = repo.create_or_resume(user_id, instance_id).await.unwrap();
        assert_eq!(first.id, resumed.id);
    }
}
