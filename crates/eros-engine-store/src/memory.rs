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

    /// Top-K profile-layer rows per category, for rows with `category` set
    /// (i.e. produced by the dreaming-lite classifier — not raw write_turn
    /// dumps). Single SQL using `ROW_NUMBER() OVER (PARTITION BY category
    /// ORDER BY embedding <=> $emb)` so the database picks the per-category
    /// nearest neighbours in one round-trip.
    ///
    /// Result is ordered by `(category ASC, rn ASC)`, so callers can group
    /// by streaming through the vector — no hash needed to preserve
    /// per-category proximity ordering.
    pub async fn search_profile_grouped(
        &self,
        user_id: Uuid,
        query_embedding: &[f32],
        k_per_category: i32,
    ) -> Result<Vec<MemoryRow>, sqlx::Error> {
        let vec_text = format_vector(query_embedding);
        sqlx::query_as::<_, MemoryRow>(
            "SELECT id, session_id, user_id, instance_id, content, category, created_at \
             FROM ( \
                 SELECT id, session_id, user_id, instance_id, content, category, \
                        embedding, created_at, \
                        ROW_NUMBER() OVER ( \
                            PARTITION BY category \
                            ORDER BY embedding <=> $2::vector \
                        ) AS rn \
                 FROM engine.companion_memories \
                 WHERE user_id = $1 \
                   AND instance_id IS NULL \
                   AND category IS NOT NULL \
             ) ranked \
             WHERE rn <= $3 \
             ORDER BY category, rn",
        )
        .bind(user_id)
        .bind(vec_text)
        .bind(k_per_category as i64)
        .fetch_all(self.pool)
        .await
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
    async fn search_profile_grouped_picks_top_k_per_category(pool: PgPool) {
        let repo = MemoryRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, None).await;

        // Three categories with varying counts and embedding distances.
        // Within each category, the lower seed = nearer to query seed=10.
        let inserts: &[(&str, usize)] = &[
            ("fact", 10),        // nearest
            ("fact", 50),        // mid
            ("fact", 100),       // far
            ("preference", 10),  // nearest
            ("preference", 200), // far
            ("event", 10),       // single
            ("emotion", 300),    // category exists but far
        ];
        for (cat, seed) in inserts {
            repo.upsert(
                MemoryLayer::Profile,
                session_id,
                user_id,
                None,
                &format!("{cat}-{seed}"),
                &unit_embedding(*seed),
                Some(cat),
            )
            .await
            .unwrap();
        }
        // An uncategorised row that should be excluded entirely.
        repo.upsert(
            MemoryLayer::Profile,
            session_id,
            user_id,
            None,
            "raw-untagged",
            &unit_embedding(10),
            None,
        )
        .await
        .unwrap();

        let rows = repo
            .search_profile_grouped(user_id, &unit_embedding(10), 2)
            .await
            .unwrap();

        // 2 fact + 2 preference + 1 event + 1 emotion = 6.
        assert_eq!(rows.len(), 6);
        for r in &rows {
            assert!(r.category.is_some());
            assert_ne!(r.content, "raw-untagged");
        }

        // Group by category, preserving SQL order.
        let mut by_cat: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for r in &rows {
            by_cat
                .entry(r.category.clone().unwrap())
                .or_default()
                .push(r.content.clone());
        }
        // Top-2 per category — fact/preference saturate, event/emotion truncate to 1.
        assert_eq!(by_cat.get("fact").map(Vec::len), Some(2));
        assert_eq!(by_cat.get("preference").map(Vec::len), Some(2));
        assert_eq!(by_cat.get("event").map(Vec::len), Some(1));
        assert_eq!(by_cat.get("emotion").map(Vec::len), Some(1));

        // Within fact: the seed=10 row (cosine distance 0) must come first.
        assert_eq!(by_cat["fact"][0], "fact-10");
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
