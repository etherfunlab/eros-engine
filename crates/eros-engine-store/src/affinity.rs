// SPDX-License-Identifier: AGPL-3.0-only
//! Affinity row persistence + EMA-smoothed event recording.
//!
//! Domain math (EMA blending, label inference) lives in
//! `eros_engine_core::affinity`. This module strictly handles I/O.

use chrono::{DateTime, Utc};
use eros_engine_core::affinity::{Affinity, AffinityDeltas, RelationshipLabel};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// Direct row mapping for `companion_affinity`. Converts to/from the
/// domain `Affinity` via [`AffinityRow::into_domain`].
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AffinityRow {
    pub id: Uuid,
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub warmth: f64,
    pub trust: f64,
    pub intrigue: f64,
    pub intimacy: f64,
    pub patience: f64,
    pub tension: f64,
    pub ghost_streak: i32,
    pub last_ghost_at: Option<DateTime<Utc>>,
    pub total_ghosts: i32,
    pub relationship_label: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl AffinityRow {
    pub fn into_domain(self) -> Affinity {
        Affinity {
            id: self.id,
            session_id: self.session_id,
            user_id: self.user_id,
            instance_id: self.instance_id,
            warmth: self.warmth,
            trust: self.trust,
            intrigue: self.intrigue,
            intimacy: self.intimacy,
            patience: self.patience,
            tension: self.tension,
            ghost_streak: self.ghost_streak,
            last_ghost_at: self.last_ghost_at,
            total_ghosts: self.total_ghosts,
            relationship_label: self.relationship_label.as_deref().and_then(label_from_str),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

pub fn to_domain(row: AffinityRow) -> Affinity {
    row.into_domain()
}

fn label_from_str(s: &str) -> Option<RelationshipLabel> {
    match s {
        "stranger" => Some(RelationshipLabel::Stranger),
        "romantic" => Some(RelationshipLabel::Romantic),
        "friend" => Some(RelationshipLabel::Friend),
        "frenemy" => Some(RelationshipLabel::Frenemy),
        "slow_burn" => Some(RelationshipLabel::SlowBurn),
        _ => None,
    }
}

fn label_to_str(label: RelationshipLabel) -> &'static str {
    match label {
        RelationshipLabel::Stranger => "stranger",
        RelationshipLabel::Romantic => "romantic",
        RelationshipLabel::Friend => "friend",
        RelationshipLabel::Frenemy => "frenemy",
        RelationshipLabel::SlowBurn => "slow_burn",
    }
}

pub struct AffinityRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> AffinityRepo<'a> {
    /// Load the affinity row for `session_id`, if one exists.
    pub async fn load(&self, session_id: Uuid) -> Result<Option<Affinity>, sqlx::Error> {
        let row = sqlx::query_as::<_, AffinityRow>(
            "SELECT * FROM companion_affinity WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_optional(self.pool)
        .await?;
        Ok(row.map(AffinityRow::into_domain))
    }

    /// Load existing or insert a fresh row with default values.
    pub async fn load_or_create(
        &self,
        session_id: Uuid,
        user_id: Uuid,
        instance_id: Uuid,
    ) -> Result<Affinity, sqlx::Error> {
        if let Some(existing) = self.load(session_id).await? {
            return Ok(existing);
        }

        let row = sqlx::query_as::<_, AffinityRow>(
            "INSERT INTO companion_affinity (session_id, user_id, instance_id) \
             VALUES ($1, $2, $3) RETURNING *",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(self.pool)
        .await?;
        Ok(row.into_domain())
    }

    /// Apply EMA-smoothed deltas in core, persist updated row, log event
    /// — all in a single transaction. Mutates `affinity` in place.
    pub async fn persist_with_event(
        &self,
        affinity: &mut Affinity,
        deltas: &AffinityDeltas,
        ema_inertia: f64,
        event_type: &str,
        context: serde_json::Value,
    ) -> Result<(), sqlx::Error> {
        affinity.apply_deltas(deltas, ema_inertia);
        let label = affinity.infer_label();
        affinity.relationship_label = label;

        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "UPDATE companion_affinity \
             SET warmth = $2, trust = $3, intrigue = $4, intimacy = $5, \
                 patience = $6, tension = $7, \
                 relationship_label = $8, updated_at = now() \
             WHERE id = $1",
        )
        .bind(affinity.id)
        .bind(affinity.warmth)
        .bind(affinity.trust)
        .bind(affinity.intrigue)
        .bind(affinity.intimacy)
        .bind(affinity.patience)
        .bind(affinity.tension)
        .bind(label.map(label_to_str))
        .execute(&mut *tx)
        .await?;

        let deltas_json = serde_json::to_value(deltas).unwrap_or_default();
        sqlx::query(
            "INSERT INTO companion_affinity_events (affinity_id, event_type, deltas, context) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(affinity.id)
        .bind(event_type)
        .bind(deltas_json)
        .bind(context)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Increment ghost counters and stamp `last_ghost_at`. No deltas applied.
    pub async fn record_ghost(&self, affinity: &mut Affinity) -> Result<(), sqlx::Error> {
        affinity.ghost_streak += 1;
        affinity.total_ghosts += 1;
        affinity.last_ghost_at = Some(Utc::now());

        sqlx::query(
            "UPDATE companion_affinity \
             SET ghost_streak = $2, total_ghosts = $3, \
                 last_ghost_at = $4, updated_at = now() \
             WHERE id = $1",
        )
        .bind(affinity.id)
        .bind(affinity.ghost_streak)
        .bind(affinity.total_ghosts)
        .bind(affinity.last_ghost_at)
        .execute(self.pool)
        .await?;

        sqlx::query(
            "INSERT INTO companion_affinity_events (affinity_id, event_type, deltas, context) \
             VALUES ($1, 'ghost', '{}'::jsonb, '{}'::jsonb)",
        )
        .bind(affinity.id)
        .execute(self.pool)
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_session(pool: &PgPool, user_id: Uuid, instance_id: Uuid) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO chat_sessions (user_id, instance_id) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn load_or_create_idempotent(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;

        let a1 = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        let a2 = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        assert_eq!(a1.id, a2.id);
        // Defaults from migration
        assert!((a1.warmth - 0.3).abs() < 1e-9);
        assert!((a1.intrigue - 0.5).abs() < 1e-9);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_with_event_updates_vector_and_logs(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;

        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        let before_warmth = a.warmth;

        let deltas = AffinityDeltas {
            warmth: 0.4,
            trust: 0.2,
            intrigue: 0.0,
            intimacy: 0.0,
            patience: 0.0,
            tension: 0.0,
        };
        repo.persist_with_event(
            &mut a,
            &deltas,
            0.0, // no smoothing → full apply
            "message",
            serde_json::json!({ "source": "test" }),
        )
        .await
        .unwrap();

        // In-memory mutated.
        assert!(a.warmth > before_warmth);

        // DB reflects updated row.
        let reloaded = repo.load(session_id).await.unwrap().unwrap();
        assert!((reloaded.warmth - a.warmth).abs() < 1e-9);

        // One event row was logged.
        let event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM companion_affinity_events WHERE affinity_id = $1",
        )
        .bind(a.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(event_count, 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn record_ghost_increments_counters(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;

        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        assert_eq!(a.ghost_streak, 0);
        assert_eq!(a.total_ghosts, 0);

        repo.record_ghost(&mut a).await.unwrap();
        repo.record_ghost(&mut a).await.unwrap();

        let reloaded = repo.load(session_id).await.unwrap().unwrap();
        assert_eq!(reloaded.ghost_streak, 2);
        assert_eq!(reloaded.total_ghosts, 2);
        assert!(reloaded.last_ghost_at.is_some());
    }
}
