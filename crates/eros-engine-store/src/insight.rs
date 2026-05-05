// SPDX-License-Identifier: AGPL-3.0-only
//! Companion insight storage + JSONB merge + training-level computation.
//!
//! `training_level` is a weighted score across known schema fields.
//! Weights ported verbatim from the gateway implementation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CompanionInsightsRow {
    pub user_id: Uuid,
    pub insights: serde_json::Value,
    pub training_level: f64,
    pub updated_at: DateTime<Utc>,
}

/// Per-field weights summing to 1.0. Matches the gateway's WEIGHTS table.
const WEIGHTS: &[(&str, f64)] = &[
    ("city", 0.05),
    ("occupation", 0.05),
    ("interests", 0.10),
    ("mbti_guess", 0.15),
    ("love_values", 0.15),
    ("emotional_needs", 0.15),
    ("life_rhythm", 0.10),
    ("personality_traits", 0.15),
    ("matching_preferences", 0.10),
];

/// Compute a [0.0, 1.0] training level from the JSONB insights blob.
pub fn compute_training_level(insights: &serde_json::Value) -> f64 {
    let Some(obj) = insights.as_object() else {
        return 0.0;
    };
    let mut score = 0.0;
    for &(field, weight) in WEIGHTS {
        if let Some(val) = obj.get(field) {
            if is_populated(val) {
                score += weight;
            }
        }
    }
    ((score * 1000.0).round() / 1000.0).min(1.0)
}

fn is_populated(val: &serde_json::Value) -> bool {
    match val {
        serde_json::Value::Null => false,
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(arr) => !arr.is_empty(),
        serde_json::Value::Object(obj) => !obj.is_empty(),
        _ => true,
    }
}

fn merge_objects(mut base: serde_json::Value, patch: &serde_json::Value) -> serde_json::Value {
    if let (Some(base_obj), Some(patch_obj)) = (base.as_object_mut(), patch.as_object()) {
        for (k, v) in patch_obj {
            base_obj.insert(k.clone(), v.clone());
        }
    }
    base
}

pub struct InsightRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> InsightRepo<'a> {
    pub async fn load(&self, user_id: Uuid) -> Result<Option<CompanionInsightsRow>, sqlx::Error> {
        sqlx::query_as::<_, CompanionInsightsRow>(
            "SELECT * FROM engine.companion_insights WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_optional(self.pool)
        .await
    }

    /// Merge `new_facts` into the user's stored JSONB, recompute
    /// `training_level`, upsert the row, and return the new state.
    pub async fn merge(
        &self,
        user_id: Uuid,
        new_facts: serde_json::Value,
    ) -> Result<CompanionInsightsRow, sqlx::Error> {
        let existing = self.load(user_id).await?;

        let merged = match existing {
            Some(prev) => merge_objects(prev.insights, &new_facts),
            None => new_facts,
        };
        let level = compute_training_level(&merged);

        let row = sqlx::query_as::<_, CompanionInsightsRow>(
            "INSERT INTO engine.companion_insights (user_id, insights, training_level) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (user_id) DO UPDATE SET \
                 insights       = EXCLUDED.insights, \
                 training_level = EXCLUDED.training_level, \
                 updated_at     = now() \
             RETURNING *",
        )
        .bind(user_id)
        .bind(merged)
        .bind(level)
        .fetch_one(self.pool)
        .await?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn training_level_empty_is_zero() {
        let v = serde_json::json!({});
        assert!(compute_training_level(&v).abs() < 1e-6);
    }

    #[test]
    fn training_level_partial() {
        let v = serde_json::json!({
            "city": "Shanghai",
            "interests": ["coffee"],
        });
        // 0.05 + 0.10
        assert!((compute_training_level(&v) - 0.15).abs() < 1e-3);
    }

    #[test]
    fn training_level_full_caps_at_one() {
        let v = serde_json::json!({
            "city": "Shanghai",
            "occupation": "engineer",
            "interests": ["coffee"],
            "mbti_guess": "INFP",
            "love_values": "slow burn",
            "emotional_needs": "validation",
            "life_rhythm": "night owl",
            "personality_traits": ["curious"],
            "matching_preferences": { "preferred_gender": "any" },
        });
        let l = compute_training_level(&v);
        assert!((l - 1.0).abs() < 1e-3);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn merge_creates_then_accumulates(pool: PgPool) {
        let repo = InsightRepo { pool: &pool };
        let user_id = Uuid::new_v4();

        // First merge → row created.
        let first = repo
            .merge(user_id, serde_json::json!({ "city": "Shanghai" }))
            .await
            .unwrap();
        assert_eq!(first.user_id, user_id);
        assert_eq!(first.insights["city"], "Shanghai");
        assert!((first.training_level - 0.05).abs() < 1e-3);

        // Second merge → adds field, level rises.
        let second = repo
            .merge(
                user_id,
                serde_json::json!({ "occupation": "engineer", "interests": ["coffee"] }),
            )
            .await
            .unwrap();
        assert_eq!(second.insights["city"], "Shanghai");
        assert_eq!(second.insights["occupation"], "engineer");
        assert!(second.training_level > first.training_level);
        assert!((second.training_level - 0.20).abs() < 1e-3);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn merge_overwrites_same_key(pool: PgPool) {
        let repo = InsightRepo { pool: &pool };
        let user_id = Uuid::new_v4();

        repo.merge(user_id, serde_json::json!({ "city": "Shanghai" }))
            .await
            .unwrap();
        let updated = repo
            .merge(user_id, serde_json::json!({ "city": "Beijing" }))
            .await
            .unwrap();
        assert_eq!(updated.insights["city"], "Beijing");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn load_returns_none_for_unknown_user(pool: PgPool) {
        let repo = InsightRepo { pool: &pool };
        let result = repo.load(Uuid::new_v4()).await.unwrap();
        assert!(result.is_none());
    }
}
