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

/// One affinity event row joined to its session. `id` is the stable,
/// unique freshness/dedup key (created_at is not unique under same-now() ties).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct AffinityEventRow {
    pub id: Uuid,
    pub event_type: String,
    pub deltas: serde_json::Value,                   // pre-EMA
    pub effective_deltas: Option<serde_json::Value>, // post-EMA (NULL pre-0014)
    pub created_at: DateTime<Utc>,
}

pub struct AffinityRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> AffinityRepo<'a> {
    /// Load the affinity row for `session_id`, if one exists.
    pub async fn load(&self, session_id: Uuid) -> Result<Option<Affinity>, sqlx::Error> {
        let row = sqlx::query_as::<_, AffinityRow>(
            "SELECT * FROM engine.companion_affinity WHERE session_id = $1",
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
            "INSERT INTO engine.companion_affinity (session_id, user_id, instance_id) \
             VALUES ($1, $2, $3) RETURNING *",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(self.pool)
        .await?;
        Ok(row.into_domain())
    }

    /// Newest-first affinity events for a session, optionally filtered by
    /// event_type. Joins events → affinity by session_id (companion_affinity
    /// .session_id is UNIQUE, so at most one affinity row participates — no
    /// cross-session leakage). Uses idx_affinity_events_affinity_created.
    pub async fn list_events(
        &self,
        session_id: Uuid,
        limit: i64,
        offset: i64,
        event_type: Option<&str>,
    ) -> Result<Vec<AffinityEventRow>, sqlx::Error> {
        sqlx::query_as::<_, AffinityEventRow>(
            "SELECT e.id, e.event_type, e.deltas, e.effective_deltas, e.created_at \
             FROM engine.companion_affinity_events e \
             JOIN engine.companion_affinity a ON a.id = e.affinity_id \
             WHERE a.session_id = $1 \
               AND ($4::text IS NULL OR e.event_type = $4) \
             ORDER BY e.created_at DESC, e.id DESC \
             LIMIT $2 OFFSET $3",
        )
        .bind(session_id)
        .bind(limit)
        .bind(offset)
        .bind(event_type)
        .fetch_all(self.pool)
        .await
    }

    /// Most-recent user-turn event (message/gift/proactive/ghost) for a
    /// session, or None if none exists yet. Excludes time_decay (background
    /// drift). Ghost is included so the latest turn is never misreported as a
    /// stale prior turn — a ghost returns all-zero effective_deltas.
    pub async fn latest_turn_event(
        &self,
        session_id: Uuid,
    ) -> Result<Option<AffinityEventRow>, sqlx::Error> {
        sqlx::query_as::<_, AffinityEventRow>(
            "SELECT e.id, e.event_type, e.deltas, e.effective_deltas, e.created_at \
             FROM engine.companion_affinity_events e \
             JOIN engine.companion_affinity a ON a.id = e.affinity_id \
             WHERE a.session_id = $1 \
               AND e.event_type IN ('message', 'gift', 'proactive', 'ghost') \
             ORDER BY e.created_at DESC, e.id DESC \
             LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(self.pool)
        .await
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
        // Snapshot pre-EMA axis values to derive the post-EMA effective change.
        let before = AffinityDeltas {
            warmth: affinity.warmth,
            trust: affinity.trust,
            intrigue: affinity.intrigue,
            intimacy: affinity.intimacy,
            patience: affinity.patience,
            tension: affinity.tension,
        };

        affinity.apply_deltas(deltas, ema_inertia);
        let label = affinity.infer_label();
        affinity.relationship_label = label;

        // Post-EMA effective change = after − before (captures EMA + clamping).
        let effective = AffinityDeltas {
            warmth: affinity.warmth - before.warmth,
            trust: affinity.trust - before.trust,
            intrigue: affinity.intrigue - before.intrigue,
            intimacy: affinity.intimacy - before.intimacy,
            patience: affinity.patience - before.patience,
            tension: affinity.tension - before.tension,
        };

        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "UPDATE engine.companion_affinity \
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
        let effective_json = serde_json::to_value(&effective).unwrap_or_default();
        sqlx::query(
            "INSERT INTO engine.companion_affinity_events \
               (affinity_id, event_type, deltas, effective_deltas, context) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(affinity.id)
        .bind(event_type)
        .bind(deltas_json)
        .bind(effective_json)
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
            "UPDATE engine.companion_affinity \
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

        let zero = serde_json::to_value(AffinityDeltas::default()).unwrap_or_default();
        sqlx::query(
            "INSERT INTO engine.companion_affinity_events \
               (affinity_id, event_type, deltas, effective_deltas, context) \
             VALUES ($1, 'ghost', '{}'::jsonb, $2, '{}'::jsonb)",
        )
        .bind(affinity.id)
        .bind(zero)
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
            "SELECT COUNT(*) FROM engine.companion_affinity_events WHERE affinity_id = $1",
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

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_with_event_records_post_ema_effective(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();

        // Raw delta +0.4 warmth with EMA inertia 0.8 → effective = 0.2 * 0.4 = 0.08.
        let deltas = AffinityDeltas {
            warmth: 0.4,
            ..Default::default()
        };
        repo.persist_with_event(&mut a, &deltas, 0.8, "message", serde_json::json!({}))
            .await
            .unwrap();

        let row: (serde_json::Value, Option<serde_json::Value>) = sqlx::query_as(
            "SELECT deltas, effective_deltas FROM engine.companion_affinity_events \
             WHERE affinity_id = $1",
        )
        .bind(a.id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // Pre-EMA stored verbatim.
        assert!((row.0["warmth"].as_f64().unwrap() - 0.4).abs() < 1e-9);
        // Post-EMA effective = blend * raw = 0.2 * 0.4 = 0.08, NOT 0.4.
        let eff = row.1.expect("effective_deltas present");
        assert!(
            (eff["warmth"].as_f64().unwrap() - 0.08).abs() < 1e-9,
            "effective warmth should be EMA-smoothed 0.08, got {eff}"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_with_event_effective_reflects_clamping(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        // trust starts at 0.2; push it past the [0,1] ceiling with no smoothing.
        let deltas = AffinityDeltas {
            trust: 5.0,
            ..Default::default()
        };
        repo.persist_with_event(&mut a, &deltas, 0.0, "message", serde_json::json!({}))
            .await
            .unwrap();

        let eff: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT effective_deltas FROM engine.companion_affinity_events WHERE affinity_id = $1",
        )
        .bind(a.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        // Effective is the CLAMPED change (1.0 − 0.2 = 0.8), not the raw 5.0.
        assert!((eff.unwrap()["trust"].as_f64().unwrap() - 0.8).abs() < 1e-9);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn record_ghost_writes_zero_effective_deltas(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        repo.record_ghost(&mut a).await.unwrap();

        let eff: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT effective_deltas FROM engine.companion_affinity_events \
             WHERE affinity_id = $1 AND event_type = 'ghost'",
        )
        .bind(a.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let eff = eff.expect("ghost effective_deltas present (all-zero, not NULL)");
        for axis in [
            "warmth", "trust", "intrigue", "intimacy", "patience", "tension",
        ] {
            assert_eq!(eff[axis].as_f64().unwrap(), 0.0, "axis {axis} must be 0");
        }
    }

    async fn seed_event(
        pool: &PgPool,
        affinity_id: Uuid,
        event_type: &str,
        eff_warmth: f64,
        secs_ago: i64,
    ) {
        sqlx::query(
            "INSERT INTO engine.companion_affinity_events \
               (affinity_id, event_type, deltas, effective_deltas, created_at) \
             VALUES ($1, $2, '{}'::jsonb, $3, now() - make_interval(secs => $4))",
        )
        .bind(affinity_id)
        .bind(event_type)
        .bind(serde_json::json!({ "warmth": eff_warmth }))
        .bind(secs_ago as f64)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn list_events_newest_first_with_filter(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;
        let a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();

        seed_event(&pool, a.id, "message", 0.1, 30).await; // oldest
        seed_event(&pool, a.id, "gift", 0.2, 20).await;
        seed_event(&pool, a.id, "time_decay", -0.05, 10).await; // newest

        let all = repo.list_events(session_id, 50, 0, None).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].event_type, "time_decay"); // newest first
        assert_eq!(all[2].event_type, "message");

        let only_gift = repo
            .list_events(session_id, 50, 0, Some("gift"))
            .await
            .unwrap();
        assert_eq!(only_gift.len(), 1);
        assert_eq!(only_gift[0].event_type, "gift");

        let page = repo.list_events(session_id, 1, 1, None).await.unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].event_type, "gift"); // offset 1 of newest-first
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn latest_turn_event_includes_ghost_skips_time_decay(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;
        let a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();

        seed_event(&pool, a.id, "message", 0.3, 30).await;
        seed_event(&pool, a.id, "ghost", 0.0, 20).await; // newer user turn
        seed_event(&pool, a.id, "time_decay", -0.05, 10).await; // newest but background

        // latest user-turn event = the ghost (NOT the older message, NOT time_decay).
        let latest = repo
            .latest_turn_event(session_id)
            .await
            .unwrap()
            .expect("some");
        assert_eq!(latest.event_type, "ghost");
        assert_eq!(
            latest.effective_deltas.unwrap()["warmth"].as_f64().unwrap(),
            0.0
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn latest_turn_event_none_when_only_time_decay(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;
        let a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        seed_event(&pool, a.id, "time_decay", -0.05, 10).await;

        assert!(repo.latest_turn_event(session_id).await.unwrap().is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn latest_turn_event_none_for_session_without_events(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;
        repo.load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        assert!(repo.latest_turn_event(session_id).await.unwrap().is_none());
    }
}
