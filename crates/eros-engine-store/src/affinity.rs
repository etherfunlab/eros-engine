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
    pub deltas: serde_json::Value,                        // pre-EMA
    pub effective_deltas: Option<serde_json::Value>,      // post-EMA (NULL pre-0014)
    pub label_changes: Option<serde_json::Value>,         // per-turn tier transition (NULL = none)
    pub effective_line_deltas: Option<serde_json::Value>, // exact {bond,chemistry} per-turn delta (NULL pre-migration)
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
            "SELECT e.id, e.event_type, e.deltas, e.effective_deltas, e.label_changes, e.effective_line_deltas, e.created_at \
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
            "SELECT e.id, e.event_type, e.deltas, e.effective_deltas, e.label_changes, e.effective_line_deltas, e.created_at \
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

    /// Most-recent `affinity_reason` strings (newest-first) from `message` /
    /// `gift` events for this session, strictly before the current turn's user
    /// row (`before_message_id`), skipping rows whose `context` has no non-empty
    /// `affinity_reason`. The `e.created_at < (sent_at of before_message_id)`
    /// cutoff is resolved via subquery — same race-safety as
    /// `ChatRepo::recent_assistant_contents`: under concurrent same-session
    /// streams, a later turn's affinity event (written after this message
    /// arrived) cannot leak into this turn's prompt as a "future" reason.
    /// Session-scoped via the same affinity join as `list_events`
    /// (companion_affinity.session_id is UNIQUE — no cross-session leakage).
    /// A nonexistent `before_message_id` → NULL cutoff → empty Vec. Used to
    /// inject the `[emotional_context]` block; the caller reverses to
    /// oldest→newest for the prompt.
    pub async fn recent_emotional_reasons(
        &self,
        session_id: Uuid,
        before_message_id: Uuid,
        limit: i64,
    ) -> Result<Vec<String>, sqlx::Error> {
        let rows: Vec<(Option<String>,)> = sqlx::query_as(
            "SELECT e.context->>'affinity_reason' \
             FROM engine.companion_affinity_events e \
             JOIN engine.companion_affinity a ON a.id = e.affinity_id \
             WHERE a.session_id = $1 \
               AND e.created_at < (SELECT sent_at FROM engine.chat_messages WHERE id = $2) \
               AND e.event_type IN ('message', 'gift') \
               AND e.context->>'affinity_reason' IS NOT NULL \
               AND length(trim(e.context->>'affinity_reason')) > 0 \
             ORDER BY e.created_at DESC, e.id DESC \
             LIMIT $3",
        )
        .bind(session_id)
        .bind(before_message_id)
        .bind(limit)
        .fetch_all(self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(|(r,)| r).collect())
    }

    /// Apply EMA-smoothed deltas in core, persist updated row, log event —
    /// all in a single transaction. The current vector is re-read **inside**
    /// the tx under `SELECT ... FOR UPDATE`, so overlapping same-session
    /// writes serialize instead of clobbering each other (the lost-update
    /// bug the six-axis activation makes real — design spec §6.2). Time
    /// decay is computed from the locked row, not a pre-read snapshot.
    /// Mutates `affinity` in place to reflect the persisted state.
    pub async fn persist_with_event(
        &self,
        affinity: &mut Affinity,
        deltas: &AffinityDeltas,
        ema_inertia: f64,
        event_type: &str,
        context: serde_json::Value,
        meta: Option<&crate::OpenRouterCallMeta>,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        // Lock the row and read the freshest committed values inside the tx.
        let locked = sqlx::query_as::<_, AffinityRow>(
            "SELECT * FROM engine.companion_affinity WHERE id = $1 FOR UPDATE",
        )
        .bind(affinity.id)
        .fetch_one(&mut *tx)
        .await?;
        let mut current = locked.into_domain();

        // Decay from the locked row, then snapshot the pre-delta baseline so
        // effective_deltas captures only the delta application, not decay.
        current.apply_time_decay();
        let before_affinity = current.clone();
        let before = AffinityDeltas {
            warmth: current.warmth,
            trust: current.trust,
            intrigue: current.intrigue,
            intimacy: current.intimacy,
            patience: current.patience,
            tension: current.tension,
        };

        current.apply_deltas(deltas, ema_inertia);
        let label = current.legacy_relationship_label();
        current.relationship_label = Some(label);

        let label_changes = eros_engine_core::affinity::diff_labels(&before_affinity, &current)
            .and_then(|c| serde_json::to_value(&c).ok());

        // Exact per-turn line delta from the floored before/after scores
        // (the absolute bond/chemistry formulas use max(warmth,0), so a raw axis
        // fold would misreport when warmth < 0). Always written for delta events.
        let effective_line_deltas = serde_json::json!({
            "bond": current.bond_score() - before_affinity.bond_score(),
            "chemistry": current.chemistry_score() - before_affinity.chemistry_score(),
        });

        // Post-EMA effective change = after − before (captures EMA + clamping).
        let effective = AffinityDeltas {
            warmth: current.warmth - before.warmth,
            trust: current.trust - before.trust,
            intrigue: current.intrigue - before.intrigue,
            intimacy: current.intimacy - before.intimacy,
            patience: current.patience - before.patience,
            tension: current.tension - before.tension,
        };

        sqlx::query(
            "UPDATE engine.companion_affinity \
             SET warmth = $2, trust = $3, intrigue = $4, intimacy = $5, \
                 patience = $6, tension = $7, \
                 relationship_label = $8, updated_at = now() \
             WHERE id = $1",
        )
        .bind(current.id)
        .bind(current.warmth)
        .bind(current.trust)
        .bind(current.intrigue)
        .bind(current.intimacy)
        .bind(current.patience)
        .bind(current.tension)
        .bind(label_to_str(label))
        .execute(&mut *tx)
        .await?;

        let deltas_json = serde_json::to_value(deltas).unwrap_or_default();
        let effective_json = serde_json::to_value(&effective).unwrap_or_default();
        sqlx::query(
            "INSERT INTO engine.companion_affinity_events \
               (affinity_id, event_type, deltas, effective_deltas, label_changes, effective_line_deltas, context, \
                model, usage, generation_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(current.id)
        .bind(event_type)
        .bind(deltas_json)
        .bind(effective_json)
        .bind(label_changes)
        .bind(effective_line_deltas)
        .bind(context)
        .bind(meta.and_then(|m| m.model.clone()))
        .bind(meta.and_then(|m| m.usage.clone()))
        .bind(meta.and_then(|m| m.generation_id.clone()))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        // Reflect the persisted state back to the caller.
        *affinity = current;
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
        let zero_line = serde_json::json!({ "bond": 0.0, "chemistry": 0.0 });
        sqlx::query(
            "INSERT INTO engine.companion_affinity_events \
               (affinity_id, event_type, deltas, effective_deltas, effective_line_deltas, context) \
             VALUES ($1, 'ghost', '{}'::jsonb, $2, $3, '{}'::jsonb)",
        )
        .bind(affinity.id)
        .bind(zero)
        .bind(zero_line)
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
        // Lowered seed (stranger start) from migration 0029.
        assert!((a1.warmth - 0.1).abs() < 1e-9);
        assert!((a1.intrigue).abs() < 1e-9);
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
            None,
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
        repo.persist_with_event(&mut a, &deltas, 0.8, "message", serde_json::json!({}), None)
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
        // trust starts at 0.0 (lowered seed from migration 0029); push it past
        // the [0,1] ceiling with no smoothing.
        let deltas = AffinityDeltas {
            trust: 5.0,
            ..Default::default()
        };
        repo.persist_with_event(&mut a, &deltas, 0.0, "message", serde_json::json!({}), None)
            .await
            .unwrap();

        let eff: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT effective_deltas FROM engine.companion_affinity_events WHERE affinity_id = $1",
        )
        .bind(a.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        // Effective is the CLAMPED change (1.0 − 0.0 = 1.0), not the raw 5.0.
        assert!((eff.unwrap()["trust"].as_f64().unwrap() - 1.0).abs() < 1e-9);
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

    #[sqlx::test(migrations = "./migrations")]
    async fn concurrent_persist_with_event_does_not_lose_updates(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;
        let base = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        let start_warmth = base.warmth; // default 0.3

        // Two overlapping writers, each +0.1 warmth at full gain (ema 0.0).
        // Without row locking the second UPDATE clobbers the first and the
        // result is 0.4 (one lost increment); with FOR UPDATE it is 0.5.
        let p1 = pool.clone();
        let p2 = pool.clone();
        let h1 = tokio::spawn(async move {
            let repo = AffinityRepo { pool: &p1 };
            let mut a = repo.load(session_id).await.unwrap().unwrap();
            repo.persist_with_event(
                &mut a,
                &AffinityDeltas {
                    warmth: 0.1,
                    ..Default::default()
                },
                0.0,
                "message",
                serde_json::json!({}),
                None,
            )
            .await
            .unwrap();
        });
        let h2 = tokio::spawn(async move {
            let repo = AffinityRepo { pool: &p2 };
            let mut a = repo.load(session_id).await.unwrap().unwrap();
            repo.persist_with_event(
                &mut a,
                &AffinityDeltas {
                    warmth: 0.1,
                    ..Default::default()
                },
                0.0,
                "message",
                serde_json::json!({}),
                None,
            )
            .await
            .unwrap();
        });
        h1.await.unwrap();
        h2.await.unwrap();

        let reloaded = repo.load(session_id).await.unwrap().unwrap();
        assert!(
            (reloaded.warmth - (start_warmth + 0.2)).abs() < 1e-9,
            "both increments must land: expected {}, got {}",
            start_warmth + 0.2,
            reloaded.warmth
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM engine.companion_affinity_events WHERE affinity_id = $1",
        )
        .bind(base.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 2, "each turn writes exactly one event row");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_with_event_flips_label_when_axes_cross_thresholds(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        // Defaults warmth 0.3 / intimacy 0.0 / tension 0.1 → not Romantic.
        // Cross the Romantic thresholds (warmth>=0.7, intimacy>=0.4,
        // tension>=0.3) at full gain — confirms the now-live axes drive labels.
        let deltas = AffinityDeltas {
            warmth: 0.5,   // 0.3 -> 0.8
            intimacy: 0.5, // 0.0 -> 0.5
            tension: 0.3,  // 0.1 -> 0.4
            ..Default::default()
        };
        repo.persist_with_event(&mut a, &deltas, 0.0, "message", serde_json::json!({}), None)
            .await
            .unwrap();
        let reloaded = repo.load(session_id).await.unwrap().unwrap();
        assert_eq!(
            reloaded.relationship_label,
            Some(RelationshipLabel::Romantic)
        );
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

    async fn seed_event_ctx(
        pool: &PgPool,
        affinity_id: Uuid,
        event_type: &str,
        context: serde_json::Value,
        secs_ago: i64,
    ) {
        sqlx::query(
            "INSERT INTO engine.companion_affinity_events \
               (affinity_id, event_type, deltas, effective_deltas, context, created_at) \
             VALUES ($1, $2, '{}'::jsonb, '{}'::jsonb, $3, now() - make_interval(secs => $4))",
        )
        .bind(affinity_id)
        .bind(event_type)
        .bind(context)
        .bind(secs_ago as f64)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Insert a chat row (sent_at = now()) to serve as the turn boundary for
    /// the `recent_emotional_reasons` cutoff, and return its id.
    async fn insert_cutoff_msg(pool: &PgPool, session_id: Uuid) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.chat_messages (session_id, role, content) \
             VALUES ($1, 'user', 'cutoff') RETURNING id",
        )
        .bind(session_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recent_emotional_reasons_newest_first_skips_empty_and_scoped(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;
        let a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();

        // oldest → newest. One message with a reason, one gift with a reason,
        // one message with an EMPTY context (no reason → skipped), and a
        // time_decay row (wrong type → skipped).
        seed_event_ctx(
            &pool,
            a.id,
            "message",
            serde_json::json!({ "affinity_reason": "他主动分享了心事" }),
            40,
        )
        .await;
        seed_event_ctx(&pool, a.id, "message", serde_json::json!({}), 30).await; // no reason → skip
        seed_event_ctx(
            &pool,
            a.id,
            "gift",
            serde_json::json!({ "affinity_reason": "送了礼物很开心" }),
            20,
        )
        .await;
        seed_event_ctx(
            &pool,
            a.id,
            "time_decay",
            serde_json::json!({ "affinity_reason": "应被忽略" }),
            10,
        )
        .await; // wrong type → skip

        // Another session must not leak in.
        let other_session = make_session(&pool, Uuid::new_v4(), Uuid::new_v4()).await;
        let b = repo
            .load_or_create(other_session, Uuid::new_v4(), Uuid::new_v4())
            .await
            .unwrap();
        seed_event_ctx(
            &pool,
            b.id,
            "message",
            serde_json::json!({ "affinity_reason": "别的会话" }),
            5,
        )
        .await;

        // Cutoff = a chat row sent now(); all seeded events above are in the
        // past, so they are all before it.
        let before = insert_cutoff_msg(&pool, session_id).await;
        let got = repo
            .recent_emotional_reasons(session_id, before, 5)
            .await
            .unwrap();
        assert_eq!(
            got,
            vec!["送了礼物很开心".to_string(), "他主动分享了心事".to_string()],
            "newest-first, empty + wrong-type + other-session rows excluded"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recent_emotional_reasons_excludes_events_after_cutoff(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;
        let a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();

        // A prior turn (before the cutoff) and a concurrent later turn whose
        // affinity event landed AFTER this message (negative secs_ago → a
        // future created_at). The cutoff must exclude the future one so a
        // racing concurrent stream cannot leak its reason into this prompt.
        seed_event_ctx(
            &pool,
            a.id,
            "message",
            serde_json::json!({ "affinity_reason": "上一轮" }),
            30,
        )
        .await;
        seed_event_ctx(
            &pool,
            a.id,
            "message",
            serde_json::json!({ "affinity_reason": "未来并发轮" }),
            -60,
        )
        .await;

        let before = insert_cutoff_msg(&pool, session_id).await;
        let got = repo
            .recent_emotional_reasons(session_id, before, 5)
            .await
            .unwrap();
        assert_eq!(
            got,
            vec!["上一轮".to_string()],
            "an affinity event created after the cutoff message must be excluded"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn generated_bond_chemistry_match_core(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        let deltas = AffinityDeltas {
            warmth: 0.5,
            trust: 0.4,
            intrigue: 0.6,
            intimacy: 0.3,
            patience: 0.0,
            tension: 0.2,
        };
        repo.persist_with_event(&mut a, &deltas, 0.0, "message", serde_json::json!({}), None)
            .await
            .unwrap();
        let (bond, chemistry): (f64, f64) =
            sqlx::query_as("SELECT bond, chemistry FROM engine.companion_affinity WHERE id = $1")
                .bind(a.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!((bond - a.bond_score()).abs() < 1e-9);
        assert!((chemistry - a.chemistry_score()).abs() < 1e-9);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_writes_new_legacy_label(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        // Push chemistry high → romantic.
        let deltas = AffinityDeltas {
            intimacy: 0.9,
            tension: 0.9,
            ..Default::default()
        };
        repo.persist_with_event(&mut a, &deltas, 0.0, "message", serde_json::json!({}), None)
            .await
            .unwrap();
        let reloaded = repo.load(session_id).await.unwrap().unwrap();
        assert_eq!(
            reloaded.relationship_label,
            Some(RelationshipLabel::Romantic)
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn label_changes_recorded_on_tier_crossing_and_null_otherwise(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        // Big positive turn crosses bond tier 1 → 4.
        let deltas = AffinityDeltas {
            trust: 0.9,
            intrigue: 0.9,
            ..Default::default()
        };
        repo.persist_with_event(&mut a, &deltas, 0.0, "message", serde_json::json!({}), None)
            .await
            .unwrap();
        // Flat turn crosses nothing.
        repo.persist_with_event(
            &mut a,
            &AffinityDeltas::default(),
            0.0,
            "message",
            serde_json::json!({}),
            None,
        )
        .await
        .unwrap();
        let rows: Vec<Option<serde_json::Value>> = sqlx::query_scalar(
            "SELECT label_changes FROM engine.companion_affinity_events \
             WHERE affinity_id = $1 ORDER BY created_at ASC, id ASC",
        )
        .bind(a.id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        let first = rows[0].as_ref().expect("first turn records a label change");
        assert!(
            first.get("bond").is_some(),
            "bond transition recorded: {first}"
        );
        assert!(rows[1].is_none(), "flat turn records NULL label_changes");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn effective_line_deltas_are_floored_exact(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        // Seed warmth negative so a raw axis fold would differ from the floored truth.
        // Fresh seed warmth=0.1; push it to -0.3 this turn (ema 0.0 = full apply).
        let deltas = AffinityDeltas {
            warmth: -0.4,
            ..Default::default()
        };
        repo.persist_with_event(&mut a, &deltas, 0.0, "message", serde_json::json!({}), None)
            .await
            .unwrap();
        // before: warmth 0.1, bond=(0.1+0+0)/3=0.0333 ; after: warmth -0.3 → floored 0 → bond 0.
        // exact line delta = 0 - 0.0333 = -0.0333 (a raw fold would give -0.4/3 = -0.133).
        let line: serde_json::Value = sqlx::query_scalar(
            "SELECT effective_line_deltas FROM engine.companion_affinity_events \
             WHERE affinity_id = $1 ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(a.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let bond = line["bond"].as_f64().unwrap();
        assert!(
            (bond - (-0.033333)).abs() < 1e-4,
            "floored exact bond delta, got {bond}"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_with_event_stores_openrouter_audit_trio(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();

        let deltas = AffinityDeltas {
            warmth: 0.1,
            trust: 0.0,
            intrigue: 0.0,
            intimacy: 0.0,
            patience: 0.0,
            tension: 0.0,
        };
        let meta = crate::OpenRouterCallMeta {
            generation_id: Some("gen-aff".into()),
            model: Some("aff/m".into()),
            usage: Some(serde_json::json!({"total_tokens": 9})),
        };
        repo.persist_with_event(
            &mut a,
            &deltas,
            0.0,
            "message",
            serde_json::json!({}),
            Some(&meta),
        )
        .await
        .unwrap();

        let row: (Option<String>, Option<String>, Option<serde_json::Value>) = sqlx::query_as(
            "SELECT generation_id, model, usage FROM engine.companion_affinity_events \
             WHERE affinity_id = $1",
        )
        .bind(a.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0.as_deref(), Some("gen-aff"));
        assert_eq!(row.1.as_deref(), Some("aff/m"));
        assert_eq!(row.2, Some(serde_json::json!({"total_tokens": 9})));
    }
}
