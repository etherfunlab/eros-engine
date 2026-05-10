// SPDX-License-Identifier: AGPL-3.0-only
//! pgvector-backed companion memory layer.
//!
//! Layer selection:
//!   - `MemoryLayer::Profile`      → cross-persona facts about the user
//!     (companion_memories.instance_id IS NULL)
//!   - `MemoryLayer::Relationship` → user × persona conversational memory
//!     (companion_memories.instance_id = persona instance id)

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// Format an `&[f32]` as the pgvector textual representation, e.g.
/// `"[0.1,0.2,0.3]"`. Bound as `String` and cast with `$N::vector` in SQL.
fn format_vector(values: &[f32]) -> String {
    let mut out = String::with_capacity(values.len() * 8 + 2);
    out.push('[');
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&v.to_string());
    }
    out.push(']');
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryLayer {
    Profile,
    Relationship,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MemoryRow {
    pub id: Uuid,
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Option<Uuid>,
    pub content: String,
    /// Optional classifier tag (e.g. `"fact"`, `"preference"`, `"event"`).
    /// `None` for rows written by the raw-turn writer; populated once the
    /// classifier extraction step lands.
    pub category: Option<String>,
    pub created_at: DateTime<Utc>,
}

pub struct MemoryRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> MemoryRepo<'a> {
    /// Insert a memory row. For `Profile`, `instance_id` is forced to `None`.
    /// For `Relationship`, the caller MUST pass `Some(instance_id)`.
    /// `category` is an optional classifier tag — pass `None` from
    /// the raw-turn writer; the future extraction step will populate it.
    #[allow(clippy::too_many_arguments)] // each arg is a distinct concern
    pub async fn upsert(
        &self,
        layer: MemoryLayer,
        session_id: Uuid,
        user_id: Uuid,
        instance_id: Option<Uuid>,
        content: &str,
        embedding: &[f32],
        category: Option<&str>,
    ) -> Result<Uuid, sqlx::Error> {
        let resolved_instance = match layer {
            MemoryLayer::Profile => None,
            MemoryLayer::Relationship => instance_id,
        };
        let vec_text = format_vector(embedding);

        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.companion_memories \
                 (session_id, user_id, instance_id, content, embedding, category) \
             VALUES ($1, $2, $3, $4, $5::vector, $6) RETURNING id",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(resolved_instance)
        .bind(content)
        .bind(vec_text)
        .bind(category)
        .fetch_one(self.pool)
        .await?;
        Ok(id)
    }

    /// Cosine-distance nearest-neighbour search.
    ///
    /// `instance_id = None` → profile layer (cross-persona).
    /// `instance_id = Some(_)` → relationship layer for that user×persona.
    pub async fn search(
        &self,
        user_id: Uuid,
        instance_id: Option<Uuid>,
        query_embedding: &[f32],
        k: i32,
    ) -> Result<Vec<MemoryRow>, sqlx::Error> {
        let vec_text = format_vector(query_embedding);

        match instance_id {
            Some(pid) => {
                sqlx::query_as::<_, MemoryRow>(
                    "SELECT id, session_id, user_id, instance_id, content, category, created_at \
                     FROM engine.companion_memories \
                     WHERE user_id = $1 AND instance_id = $2 \
                     ORDER BY embedding <=> $3::vector \
                     LIMIT $4",
                )
                .bind(user_id)
                .bind(pid)
                .bind(vec_text)
                .bind(k as i64)
                .fetch_all(self.pool)
                .await
            }
            None => {
                sqlx::query_as::<_, MemoryRow>(
                    "SELECT id, session_id, user_id, instance_id, content, category, created_at \
                     FROM engine.companion_memories \
                     WHERE user_id = $1 AND instance_id IS NULL \
                     ORDER BY embedding <=> $2::vector \
                     LIMIT $3",
                )
                .bind(user_id)
                .bind(vec_text)
                .bind(k as i64)
                .fetch_all(self.pool)
                .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_embedding(seed: usize) -> Vec<f32> {
        // Generate a deterministic 512-dim vector with a single hot index.
        let mut v = vec![0.0_f32; 512];
        v[seed % 512] = 1.0;
        v
    }

    async fn make_session(pool: &PgPool, user_id: Uuid, instance_id: Option<Uuid>) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_then_retrieve(pool: PgPool) {
        let repo = MemoryRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, Some(instance_id)).await;

        let emb = unit_embedding(7);
        let id = repo
            .upsert(
                MemoryLayer::Relationship,
                session_id,
                user_id,
                Some(instance_id),
                "user lives in shanghai",
                &emb,
                None,
            )
            .await
            .unwrap();

        // Search with the same embedding → that row should come back first.
        let hits = repo
            .search(user_id, Some(instance_id), &emb, 5)
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].id, id);
        assert_eq!(hits[0].content, "user lives in shanghai");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn cosine_search_picks_nearest_neighbour(pool: PgPool) {
        let repo = MemoryRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, Some(instance_id)).await;

        let target_id = repo
            .upsert(
                MemoryLayer::Relationship,
                session_id,
                user_id,
                Some(instance_id),
                "target",
                &unit_embedding(42),
                None,
            )
            .await
            .unwrap();
        repo.upsert(
            MemoryLayer::Relationship,
            session_id,
            user_id,
            Some(instance_id),
            "decoy a",
            &unit_embedding(100),
            None,
        )
        .await
        .unwrap();
        repo.upsert(
            MemoryLayer::Relationship,
            session_id,
            user_id,
            Some(instance_id),
            "decoy b",
            &unit_embedding(200),
            None,
        )
        .await
        .unwrap();

        let hits = repo
            .search(user_id, Some(instance_id), &unit_embedding(42), 1)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, target_id);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn category_roundtrips_through_search(pool: PgPool) {
        let repo = MemoryRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, Some(instance_id)).await;

        repo.upsert(
            MemoryLayer::Relationship,
            session_id,
            user_id,
            Some(instance_id),
            "tagged",
            &unit_embedding(33),
            Some("preference"),
        )
        .await
        .unwrap();
        repo.upsert(
            MemoryLayer::Relationship,
            session_id,
            user_id,
            Some(instance_id),
            "untagged",
            &unit_embedding(34),
            None,
        )
        .await
        .unwrap();

        let mut hits = repo
            .search(user_id, Some(instance_id), &unit_embedding(33), 10)
            .await
            .unwrap();
        hits.sort_by_key(|r| r.content.clone());
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].content, "tagged");
        assert_eq!(hits[0].category.as_deref(), Some("preference"));
        assert_eq!(hits[1].content, "untagged");
        assert_eq!(hits[1].category, None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn profile_layer_isolates_from_relationship(pool: PgPool) {
        let repo = MemoryRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, Some(instance_id)).await;

        // Profile-layer write has instance_id forced to NULL.
        repo.upsert(
            MemoryLayer::Profile,
            session_id,
            user_id,
            Some(instance_id), // ignored
            "profile fact",
            &unit_embedding(11),
            None,
        )
        .await
        .unwrap();
        // Relationship-layer write keeps the instance_id.
        repo.upsert(
            MemoryLayer::Relationship,
            session_id,
            user_id,
            Some(instance_id),
            "relationship fact",
            &unit_embedding(11),
            None,
        )
        .await
        .unwrap();

        let profile_hits = repo
            .search(user_id, None, &unit_embedding(11), 10)
            .await
            .unwrap();
        assert_eq!(profile_hits.len(), 1);
        assert_eq!(profile_hits[0].content, "profile fact");
        assert_eq!(profile_hits[0].instance_id, None);

        let rel_hits = repo
            .search(user_id, Some(instance_id), &unit_embedding(11), 10)
            .await
            .unwrap();
        assert_eq!(rel_hits.len(), 1);
        assert_eq!(rel_hits[0].content, "relationship fact");
    }
}
