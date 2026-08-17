// SPDX-License-Identifier: AGPL-3.0-only
//! Affinity row persistence + graded-turn event recording.
//!
//! Domain math (the 3.0 grade pipeline, label inference) lives in
//! `eros_engine_core::affinity`. This module strictly handles I/O.

use chrono::{DateTime, Utc};
use eros_engine_core::affinity::{
    grade_turn, Affinity, AffinityDeltas, AffinityTuning, AxisGrades, EndpointLevelReads,
    PendingDeltas,
};
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
    /// Judge's last absolute endpoint levels (1..=3, migration 0048).
    pub warmth_grade: i16,
    pub patience_grade: i16,
    pub ghost_streak: i32,
    pub last_ghost_at: Option<DateTime<Utc>>,
    pub total_ghosts: i32,
    // `bond_tier` / `chem_tier` (migration 0049) are deliberately absent: this
    // struct is populated by `SELECT *`, and sqlx's derived FromRow ignores
    // columns it has no field for. Engine code always holds the score and
    // derives the tier through `Affinity::bond_tier`; reading back its own
    // projection would be a round-trip for nothing and would expose it to rows
    // written by an older engine. Those columns exist for SQL consumers that
    // cannot call Rust.
    /// Threshold-gate balance (migration 0043). NULL = all-zero.
    pub pending_deltas: Option<serde_json::Value>,
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
            warmth_grade: self.warmth_grade,
            patience_grade: self.patience_grade,
            ghost_streak: self.ghost_streak,
            last_ghost_at: self.last_ghost_at,
            total_ghosts: self.total_ghosts,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

pub fn to_domain(row: AffinityRow) -> Affinity {
    row.into_domain()
}

/// Point-in-time snapshot written to `companion_affinity_events.state_before` /
/// `state_after` (migration 0049). Carries the derived values (`bond`,
/// `chemistry`, both tiers) alongside the axes on purpose: an audit row is
/// immutable and must record what the tier WAS under the thresholds in force at
/// the time, which a later re-derivation cannot reproduce once they move.
///
/// `updated_at` and the ghost counters are in here for the same reason. A ghost
/// moves no axis, so without them its two snapshots would be identical and
/// would read as "nothing happened" — while the operation has in fact reset the
/// decay clock, which silently forgives however much absence had accrued. An
/// audit row that asserts no change across a real change is worse than one that
/// says nothing, so the fields that did move are recorded.
fn state_snapshot(a: &Affinity) -> serde_json::Value {
    serde_json::json!({
        "warmth": a.warmth,
        "trust": a.trust,
        "intrigue": a.intrigue,
        "intimacy": a.intimacy,
        "patience": a.patience,
        "tension": a.tension,
        "bond": a.bond_score(),
        "chemistry": a.chemistry_score(),
        "bond_tier": a.bond_tier(),
        "chem_tier": a.chem_tier(),
        "warmth_grade": a.warmth_grade,
        "patience_grade": a.patience_grade,
        "ghost_streak": a.ghost_streak,
        "total_ghosts": a.total_ghosts,
        "updated_at": a.updated_at,
    })
}

/// One affinity event row joined to its session. `id` is the stable,
/// unique freshness/dedup key (created_at is not unique under same-now() ties).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct AffinityEventRow {
    pub id: Uuid,
    pub event_type: String,
    pub deltas: serde_json::Value,                   // raw, pre-decay
    pub effective_deltas: Option<serde_json::Value>, // committed (NULL pre-0014)
    pub label_changes: Option<serde_json::Value>,    // per-turn tier transition (NULL = none)
    pub effective_line_deltas: Option<serde_json::Value>, // exact {bond,chemistry} per-turn delta (NULL pre-migration)
    /// Post-decay/pre-delta state (migration 0049). NULL on pre-4.1 rows.
    pub state_before: Option<serde_json::Value>,
    /// Post-turn state (migration 0049). NULL on pre-4.1 rows.
    pub state_after: Option<serde_json::Value>,
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

    /// Load the freshest affinity row for a user × persona-instance pair,
    /// across all sessions. `companion_affinity` is session-keyed
    /// (`session_id UNIQUE`) and rows are created only by the text
    /// pipeline, so a voice-channel session never has one of its own — the
    /// voice path resolves affinity by pair instead, taking whichever
    /// session's row was updated most recently (latest state wins). Never
    /// creates a row. `idx_affinity_user` covers the `user_id` filter;
    /// per-user row counts are small enough that this scan is cheap.
    pub async fn load_latest_for_pair(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
    ) -> Result<Option<Affinity>, sqlx::Error> {
        let row = sqlx::query_as::<_, AffinityRow>(
            "SELECT * FROM engine.companion_affinity \
             WHERE user_id = $1 AND instance_id = $2 \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(instance_id)
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
            "SELECT e.id, e.event_type, e.deltas, e.effective_deltas, e.label_changes, e.effective_line_deltas, e.state_before, e.state_after, e.created_at \
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
            "SELECT e.id, e.event_type, e.deltas, e.effective_deltas, e.label_changes, e.effective_line_deltas, e.state_before, e.state_after, e.created_at \
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

    /// Run one judge verdict through the 4.0 pipeline in core, persist the
    /// updated row, log the event — all in a single transaction. The current
    /// vector AND the threshold-gate balance are re-read **inside** the tx
    /// under `SELECT ... FOR UPDATE`, so overlapping same-session writes
    /// serialize instead of clobbering each other (the lost-update bug the
    /// six-axis activation makes real — design spec §6.2), and the tier lookup
    /// the decay/penalty terms depend on always reads committed state. Time
    /// decay is computed from the locked row, not a pre-read snapshot.
    /// Mutates `affinity` in place to reflect the persisted state.
    ///
    /// Event row semantics: `deltas` = the turn's raw scores (grade conversion
    /// plus rule nudges, pre-decay); `effective_deltas` = the applied change,
    /// i.e. after − before, zero on a gated turn (for the two derived
    /// endpoints this IS the per-turn delta output, measured against the
    /// post-decay snapshot); `context` additionally carries the judge's
    /// `grades` verbatim, the endpoint levels/boosts/decay in force, the
    /// per-line units, and the gate's `pending_after` balance.
    ///
    /// `levels`: the judge's absolute endpoint reads. A `Some` overwrites the
    /// stored level; a `None` (omitted field / skipped eval) holds it. The
    /// endpoint values themselves are recomputed either way.
    #[allow(clippy::too_many_arguments)] // each arg is a distinct persist concern
    pub async fn persist_with_event(
        &self,
        affinity: &mut Affinity,
        grades: &AxisGrades,
        rule_deltas: &AffinityDeltas,
        boost: f64,
        tuning: &AffinityTuning,
        event_type: &str,
        context: serde_json::Value,
        meta: Option<&crate::OpenRouterCallMeta>,
        levels: EndpointLevelReads,
        llm_attempts: Option<serde_json::Value>,
        gateway_errors: Option<serde_json::Value>,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        // Lock the row and read the freshest committed values inside the tx.
        let locked = sqlx::query_as::<_, AffinityRow>(
            "SELECT * FROM engine.companion_affinity WHERE id = $1 FOR UPDATE",
        )
        .bind(affinity.id)
        .fetch_one(&mut *tx)
        .await?;
        let pending: PendingDeltas = locked
            .pending_deltas
            .as_ref()
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let mut current = locked.into_domain();

        // Decay from the locked row, then snapshot the pre-delta baseline so
        // effective_deltas captures only the delta application, not decay —
        // for the endpoints that means refreshing them (old levels, decayed)
        // BEFORE the snapshot, so their effective delta attributes this turn's
        // judgment and coupling change, never the absence gap.
        let days = (chrono::Utc::now() - current.updated_at).num_minutes() as f64 / (60.0 * 24.0);
        let decay_factor = eros_engine_core::affinity::endpoint_time_decay(
            days,
            tuning.time_decay_rate,
            tuning.time_decay_floor,
        );
        current.apply_time_decay();
        current.refresh_endpoints(tuning);
        let before_affinity = current.clone();
        let before = AffinityDeltas {
            warmth: current.warmth,
            trust: current.trust,
            intrigue: current.intrigue,
            intimacy: current.intimacy,
            patience: current.patience,
            tension: current.tension,
        };

        let outcome = grade_turn(&current, grades, rule_deltas, &pending, boost, tuning);
        current.apply_deltas(&outcome.committed);

        // Judge-owned absolute levels: a `Some` overwrites the stored level, a
        // `None` (omitted / skipped eval) holds it. The endpoints themselves
        // are then recomputed from the post-turn lines; apply_deltas just
        // bumped updated_at, so this refresh runs at decay ≈ 1.
        if let Some(l) = levels.warmth {
            current.warmth_grade = l.clamp(1, 3);
        }
        if let Some(l) = levels.patience {
            current.patience_grade = l.clamp(1, 3);
        }
        current.refresh_endpoints(tuning);

        let label_changes = eros_engine_core::affinity::diff_labels(&before_affinity, &current)
            .and_then(|c| serde_json::to_value(&c).ok());

        // Exact per-turn line delta from the floored before/after scores
        // (the absolute bond/chemistry formulas use max(warmth,0), so a raw axis
        // fold would misreport when warmth < 0). Always written for delta events.
        let effective_line_deltas = serde_json::json!({
            "bond": current.bond_score() - before_affinity.bond_score(),
            "chemistry": current.chemistry_score() - before_affinity.chemistry_score(),
        });

        // Effective change = after − before (captures gating + clamping).
        let effective = AffinityDeltas {
            warmth: current.warmth - before.warmth,
            trust: current.trust - before.trust,
            intrigue: current.intrigue - before.intrigue,
            intimacy: current.intimacy - before.intimacy,
            patience: current.patience - before.patience,
            tension: current.tension - before.tension,
        };

        // The tier columns are the engine's own result projected for SQL
        // consumers — written here, never read back (see `AffinityRow`).
        let (returned_updated_at,): (chrono::DateTime<Utc>,) = sqlx::query_as(
            "UPDATE engine.companion_affinity \
             SET warmth = $2, trust = $3, intrigue = $4, intimacy = $5, \
                 patience = $6, tension = $7, \
                 warmth_grade = $8, patience_grade = $9, \
                 bond_tier = $10, chem_tier = $11, \
                 pending_deltas = $12, updated_at = now() \
             WHERE id = $1 \
             RETURNING updated_at",
        )
        .bind(current.id)
        .bind(current.warmth)
        .bind(current.trust)
        .bind(current.intrigue)
        .bind(current.intimacy)
        .bind(current.patience)
        .bind(current.tension)
        .bind(current.warmth_grade)
        .bind(current.patience_grade)
        .bind(i16::from(current.bond_tier()))
        .bind(i16::from(current.chem_tier()))
        .bind(serde_json::to_value(outcome.pending).ok())
        .fetch_one(&mut *tx)
        .await?;
        // The row's own `now()`, not `apply_deltas`' Rust-side one: the
        // snapshot's `updated_at` is the decay baseline a replay measures the
        // next gap from, so it has to be the value the table actually holds.
        current.updated_at = returned_updated_at;

        // The audit trail keeps the judge's verdict verbatim and the gate's
        // new balance next to whatever context the caller supplied.
        // The engine owns these keys, so they are written unconditionally — a
        // caller that supplied a non-object context (this is a published crate;
        // the caller is not necessarily us) keeps its value under `caller`
        // rather than silently costing the turn its audit trail.
        let mut context = match context {
            v @ serde_json::Value::Object(_) => v,
            serde_json::Value::Null => serde_json::json!({}),
            other => serde_json::json!({ "caller": other }),
        };
        if let Some(obj) = context.as_object_mut() {
            if let Ok(g) = serde_json::to_value(grades) {
                obj.insert("grades".into(), g);
            }
            // The 3.1 ladder is retired: no key may claim a steered grade.
            // Its absence from new events is the retirement's cleanest proof.
            obj.remove("effective_grades");
            if let Ok(p) = serde_json::to_value(outcome.pending) {
                obj.insert("pending_after".into(), p);
            }
            // 4.0 endpoint audit: judge levels (only when actually read this
            // turn), the boosts and decay in force, and the per-line units —
            // the derivation is reproducible from these plus the axis values.
            if let Some(l) = levels.warmth {
                obj.insert("warmth_grade".into(), serde_json::json!(l));
            }
            if let Some(l) = levels.patience {
                obj.insert("patience_grade".into(), serde_json::json!(l));
            }
            obj.insert(
                "boost_warmth".into(),
                serde_json::json!(eros_engine_core::affinity::endpoint_boost(
                    current.chemistry_score()
                )),
            );
            obj.insert(
                "boost_patience".into(),
                serde_json::json!(eros_engine_core::affinity::endpoint_boost(
                    current.bond_score()
                )),
            );
            obj.insert("decay_factor".into(), serde_json::json!(decay_factor));
            obj.insert(
                "units".into(),
                serde_json::json!({
                    "bond": tuning.grade_unit_bond,
                    "chem": tuning.grade_unit_chem
                }),
            );
            // The cross-line penalty now scales with the applied grade, so how
            // much a turn was taxed is no longer recoverable from the grades and
            // the stored scores alone. ASSESSED, not applied: on a gated turn it
            // rides in `pending_after` rather than reaching the axis. Written
            // only when non-zero, so it stays absent on the common quiet turn.
            if !outcome.cross_penalty_assessed.is_zero() {
                if let Ok(p) = serde_json::to_value(outcome.cross_penalty_assessed) {
                    obj.insert("cross_penalty_assessed".into(), p);
                }
            } else {
                obj.remove("cross_penalty_assessed");
            }
        }

        let deltas_json = serde_json::to_value(&outcome.raw).unwrap_or_default();
        let effective_json = serde_json::to_value(&effective).unwrap_or_default();
        // `before_affinity` is post-decay/pre-delta, so within a row
        // `state_after − state_before == effective_deltas`, and the gap to the
        // PREVIOUS row's `state_after` is the absence effect applied above —
        // which used to appear nowhere in the log (migration 0049).
        sqlx::query(
            "INSERT INTO engine.companion_affinity_events \
               (affinity_id, event_type, deltas, effective_deltas, label_changes, effective_line_deltas, context, \
                state_before, state_after, model, usage, generation_id, \
                llm_attempts, gateway_errors) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(current.id)
        .bind(event_type)
        .bind(deltas_json)
        .bind(effective_json)
        .bind(label_changes)
        .bind(effective_line_deltas)
        .bind(context)
        .bind(state_snapshot(&before_affinity))
        .bind(state_snapshot(&current))
        .bind(meta.and_then(|m| m.model.clone()))
        .bind(meta.and_then(|m| m.usage.clone()))
        .bind(meta.and_then(|m| m.generation_id.clone()))
        .bind(llm_attempts)
        .bind(gateway_errors)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        // Reflect the persisted state back to the caller.
        *affinity = current;
        Ok(())
    }

    /// Increment ghost counters and stamp `last_ghost_at`. No deltas applied.
    ///
    /// Row-locked like [`Self::persist_with_event`], and for the same two
    /// reasons. First, the counters are read-modify-write: incrementing from
    /// the caller's possibly-stale in-memory value loses one of two concurrent
    /// ghosts, so the increment happens in SQL against the locked row. Second,
    /// callers hand us an `Affinity` they have already decayed and refreshed
    /// IN MEMORY (the stream path does exactly that at turn start) without
    /// writing those axes back — snapshotting that copy would put values in the
    /// audit row that never existed in the table, and the next turn, reading
    /// the row under lock, would show the relationship jumping back up with no
    /// event to explain it. Both snapshots come off the row.
    pub async fn record_ghost(&self, affinity: &mut Affinity) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        let before = sqlx::query_as::<_, AffinityRow>(
            "SELECT * FROM engine.companion_affinity WHERE id = $1 FOR UPDATE",
        )
        .bind(affinity.id)
        .fetch_one(&mut *tx)
        .await?
        .into_domain();

        let after = sqlx::query_as::<_, AffinityRow>(
            "UPDATE engine.companion_affinity \
             SET ghost_streak = ghost_streak + 1, total_ghosts = total_ghosts + 1, \
                 last_ghost_at = now(), updated_at = now() \
             WHERE id = $1 \
             RETURNING *",
        )
        .bind(affinity.id)
        .fetch_one(&mut *tx)
        .await?
        .into_domain();

        let zero = serde_json::to_value(AffinityDeltas::default()).unwrap_or_default();
        let zero_line = serde_json::json!({ "bond": 0.0, "chemistry": 0.0 });
        // No axis moves, so the two snapshots agree on every axis — they differ
        // on the counters and on `updated_at`, which is the point: the reset
        // decay clock is the one thing a ghost actually changes about future
        // state, and it must not be invisible in the log.
        sqlx::query(
            "INSERT INTO engine.companion_affinity_events \
               (affinity_id, event_type, deltas, effective_deltas, effective_line_deltas, context, \
                state_before, state_after) \
             VALUES ($1, 'ghost', '{}'::jsonb, $2, $3, '{}'::jsonb, $4, $5)",
        )
        .bind(affinity.id)
        .bind(zero)
        .bind(zero_line)
        .bind(state_snapshot(&before))
        .bind(state_snapshot(&after))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        // Reflect the persisted state back to the caller, same contract as
        // `persist_with_event`. Leaving the caller holding decayed axes and a
        // stale `updated_at` while the row holds neither is how a later
        // `persist_with_event` on the same struct would double-count decay.
        *affinity = after;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::seed_persona_instance;

    /// Persist a rule-deltas-only "message" turn with default tuning: at a
    /// fresh seed every line sits in tier 1 (decay ×1.0) and an ungraded axis
    /// pays no penalty, so the rule values land 1:1 — which keeps the numeric
    /// expectations below identical to the direct-apply era.
    async fn persist_rule(repo: &AffinityRepo<'_>, a: &mut Affinity, rule: AffinityDeltas) {
        repo.persist_with_event(
            a,
            &AxisGrades::default(),
            &rule,
            1.0,
            &AffinityTuning::default(),
            "message",
            serde_json::json!({}),
            None,
            EndpointLevelReads::default(),
            None,
            None,
        )
        .await
        .unwrap()
    }

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
        let instance_id = seed_persona_instance(&pool, user_id).await;
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
        // Stranger start (migration 0048): level-2 defaults over empty lines.
        let expect = (1.0 / 3.0) * (1.0 - 0.35 * 10.0 / 13.0);
        assert!((a1.warmth - expect).abs() < 1e-9);
        assert!((a1.patience - expect).abs() < 1e-9);
        assert!((a1.intrigue).abs() < 1e-9);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn load_latest_for_pair_returns_freshest_row(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;

        // Two sessions for the same user × instance pair (e.g. a text
        // session and a later voice session) each get their own affinity
        // row — companion_affinity.session_id is UNIQUE.
        let older_session = make_session(&pool, user_id, instance_id).await;
        let newer_session = make_session(&pool, user_id, instance_id).await;
        let older = repo
            .load_or_create(older_session, user_id, instance_id)
            .await
            .unwrap();
        let newer = repo
            .load_or_create(newer_session, user_id, instance_id)
            .await
            .unwrap();

        // Force a deterministic updated_at ordering instead of relying on
        // real-time gaps between the two load_or_create calls above.
        sqlx::query("UPDATE engine.companion_affinity SET updated_at = now() - interval '1 hour' WHERE id = $1")
            .bind(older.id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE engine.companion_affinity SET updated_at = now() WHERE id = $1")
            .bind(newer.id)
            .execute(&pool)
            .await
            .unwrap();

        let latest = repo
            .load_latest_for_pair(user_id, instance_id)
            .await
            .unwrap()
            .expect("a row exists for this pair");
        assert_eq!(
            latest.id, newer.id,
            "the most recently updated session's row wins, not the oldest"
        );

        // A different pair (different user) must not leak in.
        let other_user = Uuid::new_v4();
        let other_instance = seed_persona_instance(&pool, other_user).await;
        let other_session = make_session(&pool, other_user, other_instance).await;
        repo.load_or_create(other_session, other_user, other_instance)
            .await
            .unwrap();
        let scoped = repo
            .load_latest_for_pair(user_id, instance_id)
            .await
            .unwrap()
            .expect("still the original pair's row");
        assert_eq!(scoped.id, newer.id);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn load_latest_for_pair_none_when_pair_has_none(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        // A brand-new user × instance pair with zero sessions and zero
        // affinity rows — the state of every voice-channel session before
        // any text session for the same pair has ever run.
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        assert!(repo
            .load_latest_for_pair(user_id, instance_id)
            .await
            .unwrap()
            .is_none());
    }

    /// With the penalty scaling by grade, "what did this turn pay" is no longer
    /// derivable from the grades and the stored scores, so the event row carries
    /// it. Two grades at the same position must differ by exactly their ratio.
    #[sqlx::test(migrations = "./migrations")]
    async fn event_context_records_the_cross_penalty_assessed(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;

        // Push bond high so the chemistry-exclusive axes face a real tax.
        async fn charged(
            pool: &PgPool,
            user_id: Uuid,
            instance_id: Uuid,
            g: i8,
        ) -> serde_json::Value {
            let repo = AffinityRepo { pool };
            let session = make_session(pool, user_id, instance_id).await;
            let mut a = repo
                .load_or_create(session, user_id, instance_id)
                .await
                .unwrap();
            a.warmth = 1.0;
            a.trust = 1.0;
            a.intrigue = 1.0; // bond = 1.0
            sqlx::query(
                "UPDATE engine.companion_affinity SET warmth=1, trust=1, intrigue=1 WHERE id=$1",
            )
            .bind(a.id)
            .execute(pool)
            .await
            .unwrap();
            repo.persist_with_event(
                &mut a,
                &AxisGrades {
                    intimacy: g,
                    ..Default::default()
                },
                &AffinityDeltas::default(),
                1.0,
                &AffinityTuning::default(),
                "message",
                serde_json::json!({}),
                None,
                EndpointLevelReads::default(),
                None,
                None,
            )
            .await
            .unwrap();
            sqlx::query_scalar::<_, serde_json::Value>(
                "SELECT context FROM engine.companion_affinity_events WHERE affinity_id = $1 \
                 ORDER BY created_at DESC, id DESC LIMIT 1",
            )
            .bind(a.id)
            .fetch_one(pool)
            .await
            .unwrap()
        }
        // (Each call opens its own session, so `affinity_id` already selects a
        // single event — the ORDER BY is belt-and-braces against a future edit
        // that reuses one.)

        let g1 = charged(&pool, user_id, instance_id, 1).await;
        let g4 = charged(&pool, user_id, instance_id, 4).await;
        let p1 = g1["cross_penalty_assessed"]["intimacy"].as_f64().unwrap();
        let p4 = g4["cross_penalty_assessed"]["intimacy"].as_f64().unwrap();
        assert!(p1 > 0.0, "a maxed counterpart must charge something");
        assert!(
            (p4 - p1 * 4.0).abs() < 1e-9,
            "g4 pays exactly 4× g1: {p1} vs {p4}"
        );
        // Penalty-exempt warmth never appears with a charge.
        assert_eq!(g4["cross_penalty_assessed"].get("warmth"), None);
    }

    /// The 3.1 ladder is retired: no event may ever carry `effective_grades`
    /// again — its absence from new rows is the retirement's cleanest proof.
    #[sqlx::test(migrations = "./migrations")]
    async fn event_context_never_carries_effective_grades(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();

        repo.persist_with_event(
            &mut a,
            &AxisGrades {
                intimacy: 1,
                ..Default::default()
            },
            &AffinityDeltas::default(),
            1.0,
            &AffinityTuning::default(),
            "message",
            serde_json::json!({}),
            None,
            EndpointLevelReads::default(),
            None,
            None,
        )
        .await
        .unwrap();

        let ctx: serde_json::Value = sqlx::query_scalar(
            "SELECT context FROM engine.companion_affinity_events WHERE affinity_id = $1",
        )
        .bind(a.id)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert!(ctx.get("effective_grades").is_none());
        assert!((a.intimacy - AffinityTuning::default().grade_unit_chem).abs() < 1e-9);
    }

    /// This is a published crate, so the caller is not necessarily us. A
    /// non-object context must not cost the turn its audit trail, and a
    /// caller-supplied `effective_grades` must never survive to make an
    /// unsteered turn look corrected.
    #[sqlx::test(migrations = "./migrations")]
    async fn engine_owned_audit_keys_survive_hostile_caller_context(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;

        async fn write(
            pool: &PgPool,
            user_id: Uuid,
            instance_id: Uuid,
            ctx: serde_json::Value,
        ) -> serde_json::Value {
            let repo = AffinityRepo { pool };
            let session = make_session(pool, user_id, instance_id).await;
            let mut a = repo
                .load_or_create(session, user_id, instance_id)
                .await
                .unwrap();
            repo.persist_with_event(
                &mut a,
                &AxisGrades {
                    trust: 1,
                    ..Default::default()
                },
                &AffinityDeltas::default(),
                1.0,
                &AffinityTuning::default(),
                "message",
                ctx,
                None,
                EndpointLevelReads::default(),
                None,
                None,
            )
            .await
            .unwrap();
            sqlx::query_scalar::<_, serde_json::Value>(
                "SELECT context FROM engine.companion_affinity_events WHERE affinity_id = $1",
            )
            .bind(a.id)
            .fetch_one(pool)
            .await
            .unwrap()
        }

        // A null context still gets the engine's keys.
        let ctx = write(&pool, user_id, instance_id, serde_json::Value::Null).await;
        assert!(ctx["grades"].is_object());

        // A non-object context is preserved rather than dropped, and does not
        // block the audit keys.
        let ctx = write(
            &pool,
            user_id,
            instance_id,
            serde_json::json!("a bare string"),
        )
        .await;
        assert!(ctx["grades"].is_object());
        assert_eq!(ctx["caller"], "a bare string");

        // A spoofed key is cleared: this turn was NOT steered.
        let ctx = write(
            &pool,
            user_id,
            instance_id,
            serde_json::json!({ "effective_grades": { "tension": 9 } }),
        )
        .await;
        assert!(ctx["grades"].is_object());
        assert!(
            ctx.get("effective_grades").is_none(),
            "a caller must not be able to claim the retired ladder fired"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_with_event_updates_vector_and_logs(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;

        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        let before_trust = a.trust;

        // Graded verdict: trust +2 / intrigue +2 at a fresh seed (tier 1, no
        // penalty) → +2·u_bond each.
        repo.persist_with_event(
            &mut a,
            &AxisGrades {
                trust: 2,
                intrigue: 2,
                ..Default::default()
            },
            &AffinityDeltas::default(),
            1.0,
            &AffinityTuning::default(),
            "message",
            serde_json::json!({ "source": "test" }),
            None,
            EndpointLevelReads::default(),
            None,
            None,
        )
        .await
        .unwrap();

        // In-memory mutated.
        assert!(a.trust > before_trust);

        // DB reflects updated row.
        let reloaded = repo.load(session_id).await.unwrap().unwrap();
        assert!((reloaded.trust - a.trust).abs() < 1e-9);
        assert!((reloaded.patience - a.patience).abs() < 1e-9);

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
        let instance_id = seed_persona_instance(&pool, user_id).await;
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
    async fn persist_with_event_records_raw_and_effective(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();

        // Grade +4 trust at a fresh seed: raw = 4 × u_bond, and with tier-1
        // decay ×1.0 and no gate it applies verbatim.
        repo.persist_with_event(
            &mut a,
            &AxisGrades {
                trust: 4,
                ..Default::default()
            },
            &AffinityDeltas::default(),
            1.0,
            &AffinityTuning::default(),
            "message",
            serde_json::json!({}),
            None,
            EndpointLevelReads::default(),
            None,
            None,
        )
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

        // deltas column = the turn's raw score (4 × u_bond).
        let expect = 4.0 * AffinityTuning::default().grade_unit_bond;
        assert!((row.0["trust"].as_f64().unwrap() - expect).abs() < 1e-9);
        // effective = applied change: same value, i.e. committed 1:1.
        let eff = row.1.expect("effective_deltas present");
        assert!(
            (eff["trust"].as_f64().unwrap() - expect).abs() < 1e-9,
            "effective trust should be the direct raw, got {eff}"
        );
    }

    /// The threshold gate: real scores below θ buffer in `pending_deltas`
    /// (axes untouched, effective zero), and the next turn that lifts the
    /// balance past θ commits the whole accumulated amount.
    #[sqlx::test(migrations = "./migrations")]
    async fn threshold_gate_buffers_then_flushes(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        let tuning = AffinityTuning {
            delta_threshold: 0.5,
            ..Default::default()
        };

        repo.persist_with_event(
            &mut a,
            &AxisGrades::default(),
            &AffinityDeltas {
                trust: 0.1,
                ..Default::default()
            },
            1.0,
            &tuning,
            "message",
            serde_json::json!({}),
            None,
            EndpointLevelReads::default(),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(a.trust, 0.0, "gated turn moves nothing");
        let pending: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT pending_deltas FROM engine.companion_affinity WHERE id = $1",
        )
        .bind(a.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            (pending.unwrap()["trust"].as_f64().unwrap() - 0.1).abs() < 1e-9,
            "the balance persists on the row"
        );

        repo.persist_with_event(
            &mut a,
            &AxisGrades::default(),
            &AffinityDeltas {
                trust: 0.5,
                ..Default::default()
            },
            1.0,
            &tuning,
            "message",
            serde_json::json!({}),
            None,
            EndpointLevelReads::default(),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            (a.trust - 0.6).abs() < 1e-9,
            "0.1 + 0.5 clears θ=0.5 and commits in full, got {}",
            a.trust
        );
        let pending: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT pending_deltas FROM engine.companion_affinity WHERE id = $1",
        )
        .bind(a.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            pending.unwrap()["trust"].as_f64().unwrap(),
            0.0,
            "flush resets the balance"
        );
    }

    /// Two CONCURRENT sub-threshold turns: the pending balance is re-read
    /// under the same `FOR UPDATE` lock as the axes, so both buffered amounts
    /// must survive — a lost pending update would silently eat earned score.
    #[sqlx::test(migrations = "./migrations")]
    async fn concurrent_subthreshold_turns_accumulate_pending(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let base = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();

        let mut handles = Vec::new();
        for _ in 0..2 {
            let p = pool.clone();
            handles.push(tokio::spawn(async move {
                let repo = AffinityRepo { pool: &p };
                let mut a = repo.load(session_id).await.unwrap().unwrap();
                repo.persist_with_event(
                    &mut a,
                    &AxisGrades::default(),
                    &AffinityDeltas {
                        trust: 0.1,
                        ..Default::default()
                    },
                    1.0,
                    &AffinityTuning {
                        delta_threshold: 0.5,
                        ..Default::default()
                    },
                    "message",
                    serde_json::json!({}),
                    None,
                    EndpointLevelReads::default(),
                    None,
                    None,
                )
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let reloaded = repo.load(session_id).await.unwrap().unwrap();
        assert_eq!(reloaded.trust, 0.0, "both turns gated — axis untouched");
        let pending: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT pending_deltas FROM engine.companion_affinity WHERE id = $1",
        )
        .bind(base.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            (pending.unwrap()["trust"].as_f64().unwrap() - 0.2).abs() < 1e-9,
            "both buffered amounts must land — no lost pending update"
        );
    }

    /// A flush that overshoots the axis ceiling: the committed amount is the
    /// full accumulator, the axis clamps, `effective_deltas` reports the
    /// APPLIED change (after − before), and the balance still resets.
    #[sqlx::test(migrations = "./migrations")]
    async fn gate_flush_clamps_and_reports_applied_change(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        let tuning = AffinityTuning {
            delta_threshold: 1.5,
            ..Default::default()
        };
        for _ in 0..2 {
            repo.persist_with_event(
                &mut a,
                &AxisGrades::default(),
                &AffinityDeltas {
                    trust: 0.9,
                    ..Default::default()
                },
                1.0,
                &tuning,
                "message",
                serde_json::json!({}),
                None,
                EndpointLevelReads::default(),
                None,
                None,
            )
            .await
            .unwrap();
        }
        // Turn 1 buffers 0.9; turn 2 accumulates 1.8 ≥ 1.5 → commits 1.8,
        // trust clamps 0.0 → 1.0.
        assert_eq!(a.trust, 1.0, "clamped at the ceiling");
        let (pending, eff): (Option<serde_json::Value>, Option<serde_json::Value>) =
            sqlx::query_as(
                "SELECT a.pending_deltas, e.effective_deltas \
                 FROM engine.companion_affinity a \
                 JOIN engine.companion_affinity_events e ON e.affinity_id = a.id \
                 WHERE a.id = $1 ORDER BY e.created_at DESC, e.id DESC LIMIT 1",
            )
            .bind(a.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            pending.unwrap()["trust"].as_f64().unwrap(),
            0.0,
            "flush resets the balance even when the axis clamped"
        );
        assert!(
            (eff.unwrap()["trust"].as_f64().unwrap() - 1.0).abs() < 1e-9,
            "effective_deltas reports the applied 1.0, not the flushed 1.8"
        );
    }

    /// The event context carries the judge's verdict and the gate balance next
    /// to whatever the caller supplied.
    #[sqlx::test(migrations = "./migrations")]
    async fn event_context_carries_grades_and_pending(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        repo.persist_with_event(
            &mut a,
            &AxisGrades {
                intimacy: 2,
                trust: -1,
                ..Default::default()
            },
            &AffinityDeltas::default(),
            1.0,
            &AffinityTuning::default(),
            "message",
            serde_json::json!({ "affinity_reason": "他坦白了" }),
            None,
            EndpointLevelReads::default(),
            None,
            None,
        )
        .await
        .unwrap();

        let ctx: serde_json::Value = sqlx::query_scalar(
            "SELECT context FROM engine.companion_affinity_events WHERE affinity_id = $1",
        )
        .bind(a.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ctx["affinity_reason"].as_str(), Some("他坦白了"));
        assert_eq!(ctx["grades"]["intimacy"].as_i64(), Some(2));
        assert_eq!(ctx["grades"]["trust"].as_i64(), Some(-1));
        assert!(ctx["pending_after"].is_object());
        // 4.0 audit keys are always present…
        assert!(ctx["boost_warmth"].is_f64());
        assert!(ctx["boost_patience"].is_f64());
        assert!(ctx["decay_factor"].is_f64());
        assert!(ctx["units"]["bond"].is_f64());
        // …the level keys only when the judge actually read them, and the
        // ladder's key never again.
        assert!(ctx.get("warmth_grade").is_none());
        assert!(ctx.get("effective_grades").is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_with_event_effective_reflects_clamping(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        // trust starts at 0.0 (lowered seed from migration 0029); push it past
        // the [0,1] ceiling via an oversized rule delta.
        persist_rule(
            &repo,
            &mut a,
            AffinityDeltas {
                trust: 5.0,
                ..Default::default()
            },
        )
        .await;

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
        let instance_id = seed_persona_instance(&pool, user_id).await;
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
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let base = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        let start_trust = base.trust; // seed 0.0

        // Two overlapping writers, each a trust +2 verdict (+2·u_bond at
        // tier 1). Without row locking the second UPDATE clobbers the first
        // and one increment is lost; with FOR UPDATE both land.
        let p1 = pool.clone();
        let p2 = pool.clone();
        let h1 = tokio::spawn(async move {
            let repo = AffinityRepo { pool: &p1 };
            let mut a = repo.load(session_id).await.unwrap().unwrap();
            repo.persist_with_event(
                &mut a,
                &AxisGrades {
                    trust: 2,
                    ..Default::default()
                },
                &AffinityDeltas::default(),
                1.0,
                &AffinityTuning::default(),
                "message",
                serde_json::json!({}),
                None,
                EndpointLevelReads::default(),
                None,
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
                &AxisGrades {
                    trust: 2,
                    ..Default::default()
                },
                &AffinityDeltas::default(),
                1.0,
                &AffinityTuning::default(),
                "message",
                serde_json::json!({}),
                None,
                EndpointLevelReads::default(),
                None,
                None,
            )
            .await
            .unwrap();
        });
        h1.await.unwrap();
        h2.await.unwrap();

        let reloaded = repo.load(session_id).await.unwrap().unwrap();
        let expect = start_trust + 2.0 * (2.0 * AffinityTuning::default().grade_unit_bond);
        assert!(
            (reloaded.trust - expect).abs() < 1e-9,
            "both increments must land: expected {}, got {}",
            expect,
            reloaded.trust
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
    async fn persist_with_event_moves_tier_when_axes_cross_thresholds(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        // Push chemistry ahead of bond and past tier 3 via rule deltas (fresh
        // seed → tier 1, so they land 1:1) — confirms the live axes drive labels.
        persist_rule(
            &repo,
            &mut a,
            AffinityDeltas {
                warmth: 0.5,   // 0.1 -> 0.6
                intimacy: 0.5, // 0.0 -> 0.5
                tension: 0.3,  // 0.0 -> 0.3
                ..Default::default()
            },
        )
        .await;
        let reloaded = repo.load(session_id).await.unwrap().unwrap();
        // chemistry = (0.5 + 0.3) / 2 = 0.4 → tier 3.
        assert_eq!(reloaded.chem_tier(), 3);
        assert_eq!(stored_tiers(&pool, session_id).await, (1, 3));
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
        let instance_id = seed_persona_instance(&pool, user_id).await;
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
        let instance_id = seed_persona_instance(&pool, user_id).await;
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
        let instance_id = seed_persona_instance(&pool, user_id).await;
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
        let instance_id = seed_persona_instance(&pool, user_id).await;
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
        let instance_id = seed_persona_instance(&pool, user_id).await;
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
        let other_user = Uuid::new_v4();
        let other_instance = seed_persona_instance(&pool, other_user).await;
        let other_session = make_session(&pool, other_user, other_instance).await;
        let b = repo
            .load_or_create(other_session, other_user, other_instance)
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
        let instance_id = seed_persona_instance(&pool, user_id).await;
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

    /// Read back the tier columns the engine wrote (they are write-only from
    /// the engine's side, so nothing else surfaces them).
    async fn stored_tiers(pool: &PgPool, session_id: Uuid) -> (i16, i16) {
        sqlx::query_as::<_, (i16, i16)>(
            "SELECT bond_tier, chem_tier FROM engine.companion_affinity WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// The persisted tier is taken from the POST-delta state, at every boundary
    /// and on both sides of it, and survives the i16 round-trip to the column.
    ///
    /// It does NOT prove the ladder is single-sourced — both sides of the assert
    /// run the same `tier_index` at the same commit, so moving a threshold moves
    /// them together. That property is structural (one implementation exists),
    /// not test-enforced. The genuine two-implementation risk is the migration's
    /// inlined backfill `CASE`, which
    /// `migration_backfill_case_matches_tier_index` covers.
    ///
    /// The two lines are given DIFFERENT scores so a bond/chem mix-up in the
    /// UPDATE's bind order cannot pass.
    #[sqlx::test(migrations = "./migrations")]
    async fn stored_tier_matches_core_at_every_boundary(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        for score in [
            0.0, 0.149999, 0.15, 0.349999, 0.35, 0.619999, 0.62, 0.899999, 0.90, 1.0,
        ] {
            let user_id = Uuid::new_v4();
            let instance_id = seed_persona_instance(&pool, user_id).await;
            let session_id = make_session(&pool, user_id, instance_id).await;
            let mut a = repo
                .load_or_create(session_id, user_id, instance_id)
                .await
                .unwrap();
            // bond = (trust + intrigue) / 2, so both axes at `score` put the
            // bond line exactly on `score`. Chemistry is deliberately parked one
            // tier away (0.0 unless bond is already at the bottom) so the two
            // columns are distinguishable.
            let chem = if score < 0.15 { 0.95 } else { 0.0 };
            persist_rule(
                &repo,
                &mut a,
                AffinityDeltas {
                    trust: score,
                    intrigue: score,
                    intimacy: chem,
                    tension: chem,
                    ..Default::default()
                },
            )
            .await;

            let reloaded = repo.load(session_id).await.unwrap().unwrap();
            let (bond_tier, chem_tier) = stored_tiers(&pool, session_id).await;
            assert_eq!(
                bond_tier,
                i16::from(reloaded.bond_tier()),
                "bond_tier column disagrees with tier_index at score {score}"
            );
            assert_eq!(
                chem_tier,
                i16::from(reloaded.chem_tier()),
                "chem_tier column disagrees with tier_index at score {score}"
            );
        }
    }

    /// The one place the tier thresholds genuinely exist twice: `tier_index` in
    /// Rust, and the inlined `CASE` in migration 0049's backfill. Run the
    /// migration's own expression against the boundary set and demand it agree.
    /// Moving a threshold in Rust without touching the backfill fails here.
    #[sqlx::test(migrations = "./migrations")]
    async fn migration_backfill_case_matches_tier_index(pool: PgPool) {
        let backfill = "SELECT CASE WHEN $1 < 0.15 THEN 1 WHEN $1 < 0.35 THEN 2 \
                               WHEN $1 < 0.62 THEN 3 WHEN $1 < 0.90 THEN 4 \
                               ELSE 5 END";
        for score in [
            0.0, 0.149999, 0.15, 0.349999, 0.35, 0.619999, 0.62, 0.899999, 0.90, 1.0,
        ] {
            let from_sql: i32 = sqlx::query_scalar(backfill)
                .bind(score)
                .fetch_one(&pool)
                .await
                .unwrap();
            let from_rust = i32::from(eros_engine_core::affinity::tier_index(score));
            assert_eq!(
                from_sql, from_rust,
                "migration backfill disagrees with tier_index at {score}"
            );
        }
    }

    /// The replay chain across a real absence gap — the case that makes
    /// `state_before` worth storing at all.
    ///
    /// Also the regression guard for ghost snapshots: `record_ghost` must
    /// record the ROW, never the caller's in-memory copy. Stream callers decay
    /// and refresh their `Affinity` in memory at turn start without writing it
    /// back, so snapshotting that copy would write an audit row that contradicts
    /// the table and make the relationship appear to rebound on the next turn.
    #[sqlx::test(migrations = "./migrations")]
    async fn absence_gap_and_ghost_snapshots_stay_on_the_row(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        persist_rule(
            &repo,
            &mut a,
            AffinityDeltas {
                trust: 0.5,
                intrigue: 0.5,
                ..Default::default()
            },
        )
        .await;

        // Backdate the row, then hand `record_ghost` a copy decayed in memory
        // exactly the way the stream path does it.
        sqlx::query(
            "UPDATE engine.companion_affinity \
             SET updated_at = now() - interval '30 days' WHERE id = $1",
        )
        .bind(a.id)
        .execute(&pool)
        .await
        .unwrap();

        let mut in_memory = repo.load(session_id).await.unwrap().unwrap();
        in_memory.apply_time_decay();
        in_memory.refresh_endpoints(&AffinityTuning::default());
        let decayed_intrigue = in_memory.intrigue;
        let row_intrigue: f64 =
            sqlx::query_scalar("SELECT intrigue FROM engine.companion_affinity WHERE id = $1")
                .bind(a.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            decayed_intrigue < row_intrigue,
            "precondition: 30 days must cool the in-memory copy below the row"
        );

        repo.record_ghost(&mut in_memory).await.unwrap();

        let (before, after): (serde_json::Value, serde_json::Value) = sqlx::query_as(
            "SELECT state_before, state_after FROM engine.companion_affinity_events \
             WHERE affinity_id = $1 AND event_type = 'ghost'",
        )
        .bind(a.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let axis = |v: &serde_json::Value, k: &str| v.get(k).unwrap().as_f64().unwrap();
        for k in [
            "warmth", "trust", "intrigue", "intimacy", "patience", "tension",
        ] {
            assert_eq!(
                axis(&before, k),
                axis(&after, k),
                "a ghost moves no axis ({k})"
            );
        }
        // …but it DOES reset the decay clock, and that must not be invisible:
        // two identical snapshots would assert "nothing happened" across the
        // one thing a ghost actually changes about future state.
        assert_ne!(
            before.get("updated_at"),
            after.get("updated_at"),
            "the reset decay clock must be visible in the log"
        );
        assert_eq!(
            before.get("ghost_streak").unwrap().as_i64().unwrap() + 1,
            after.get("ghost_streak").unwrap().as_i64().unwrap()
        );
        let snap_intrigue = axis(&after, "intrigue");
        assert!(
            (snap_intrigue - row_intrigue).abs() < 1e-9,
            "ghost snapshot must record the row ({row_intrigue}), \
             not the caller's decayed copy ({decayed_intrigue}); got {snap_intrigue}"
        );
    }

    /// A row that has never been through `persist_with_event` still reads as
    /// tier 1 — the column DEFAULT is derived from an all-zero fresh row, not
    /// picked.
    #[sqlx::test(migrations = "./migrations")]
    async fn fresh_row_defaults_to_bottom_tier(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        assert_eq!(stored_tiers(&pool, session_id).await, (1, 1));
        assert_eq!(a.bond_tier(), 1);
        assert_eq!(a.chem_tier(), 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn generated_bond_chemistry_match_core(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        persist_rule(
            &repo,
            &mut a,
            AffinityDeltas {
                warmth: 0.5,
                trust: 0.4,
                intrigue: 0.6,
                intimacy: 0.3,
                patience: 0.0,
                tension: 0.2,
            },
        )
        .await;
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
    async fn persist_writes_tier_columns(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        // Push chemistry high → romantic.
        persist_rule(
            &repo,
            &mut a,
            AffinityDeltas {
                intimacy: 0.9,
                tension: 0.9,
                ..Default::default()
            },
        )
        .await;
        let reloaded = repo.load(session_id).await.unwrap().unwrap();
        // chemistry = (0.9 + 0.9) / 2 = 0.9 → tier 5.
        assert_eq!(reloaded.chem_tier(), 5);
        // The columns are a projection of exactly that, never re-derived in SQL.
        assert_eq!(stored_tiers(&pool, session_id).await, (1, 5));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0048_levels_and_composites(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        assert_eq!(a.warmth_grade, 2);
        assert_eq!(a.patience_grade, 2);
        // Fresh row: lines 0 ⇒ endpoints at the level-2 damped base — the DB
        // default is the exact derivation expression, so parity is tight.
        let expect = (1.0 / 3.0) * (1.0 - 0.35 * 10.0 / 13.0);
        assert!((a.warmth - expect).abs() < 1e-9);
        assert!((a.patience - expect).abs() < 1e-9);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_levels_and_derives_endpoints(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        let t = AffinityTuning::default();
        let grades = AxisGrades {
            intimacy: 4,
            tension: 4,
            ..Default::default()
        };
        let levels = EndpointLevelReads {
            warmth: Some(3),
            patience: Some(1),
        };
        repo.persist_with_event(
            &mut a,
            &grades,
            &AffinityDeltas::default(),
            1.0,
            &t,
            "message",
            serde_json::json!({}),
            None,
            levels,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(a.warmth_grade, 3);
        assert_eq!(a.patience_grade, 1);
        // warmth = 2/3 · B(chem) at decay ≈ 1 (fresh turn re-anchors the clock).
        let chem = a.chemistry_score();
        assert!(chem > 0.0);
        assert!(
            (a.warmth - (2.0 / 3.0) * eros_engine_core::affinity::endpoint_boost(chem)).abs()
                < 1e-6
        );
        // patience level 1 with bond 0 ⇒ floor φ·0 = 0.
        assert!(a.patience.abs() < 1e-9);
        // DB row mirrors the derivation and the levels.
        let reloaded = repo.load(session_id).await.unwrap().unwrap();
        assert_eq!(reloaded.warmth_grade, 3);
        assert!((reloaded.warmth - a.warmth).abs() < 1e-9);
        // Event context carries the endpoint audit keys.
        let ctx: serde_json::Value = sqlx::query_scalar(
            "SELECT context FROM engine.companion_affinity_events WHERE affinity_id = $1 \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(a.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ctx["warmth_grade"], serde_json::json!(3));
        assert_eq!(ctx["patience_grade"], serde_json::json!(1));
        assert!(ctx["units"]["chem"].is_f64());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_none_levels_hold_and_rule_patience_is_discarded(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();

        // Skipped-eval shape: no levels, a (hypothetical) rule patience nudge.
        // The stored level holds at its default 2 and the nudge is discarded —
        // patience stays exactly the level-2 derivation over bond 0.
        persist_rule(
            &repo,
            &mut a,
            AffinityDeltas {
                patience: 0.4,
                ..Default::default()
            },
        )
        .await;

        let reloaded = repo.load(session_id).await.unwrap().unwrap();
        assert_eq!(reloaded.warmth_grade, 2);
        assert_eq!(reloaded.patience_grade, 2);
        let expect = (1.0 / 3.0) * eros_engine_core::affinity::endpoint_boost(0.0);
        assert!(
            (reloaded.patience - expect).abs() < 1e-6,
            "rule patience must be discarded; got {}",
            reloaded.patience
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn label_changes_recorded_on_tier_crossing_and_null_otherwise(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        // Big positive turn crosses bond tier 1 → 4.
        persist_rule(
            &repo,
            &mut a,
            AffinityDeltas {
                trust: 0.9,
                intrigue: 0.9,
                ..Default::default()
            },
        )
        .await;
        // Flat turn crosses nothing.
        persist_rule(&repo, &mut a, AffinityDeltas::default()).await;
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
    async fn effective_line_deltas_are_exact(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        // A trust rule delta at a fresh seed lands 1:1 (tier 1, no penalty):
        // before bond = 0, after bond = (0.9 + 0)/2 = 0.45 — the event must
        // carry the exact before/after line delta, not a folded axis delta.
        persist_rule(
            &repo,
            &mut a,
            AffinityDeltas {
                trust: 0.9,
                ..Default::default()
            },
        )
        .await;
        let line: serde_json::Value = sqlx::query_scalar(
            "SELECT effective_line_deltas FROM engine.companion_affinity_events \
             WHERE affinity_id = $1 ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(a.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let bond = line["bond"].as_f64().unwrap();
        assert!((bond - 0.45).abs() < 1e-9, "exact bond delta, got {bond}");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_with_event_stores_openrouter_audit_trio(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();

        let meta = crate::OpenRouterCallMeta {
            generation_id: Some("gen-aff".into()),
            model: Some("aff/m".into()),
            usage: Some(serde_json::json!({"total_tokens": 9})),
        };
        repo.persist_with_event(
            &mut a,
            &AxisGrades {
                trust: 2,
                ..Default::default()
            },
            &AffinityDeltas::default(),
            1.0,
            &AffinityTuning::default(),
            "message",
            serde_json::json!({}),
            Some(&meta),
            EndpointLevelReads::default(),
            None,
            None,
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

    #[sqlx::test(migrations = "./migrations")]
    async fn event_state_snapshots_close_the_replay_chain(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();

        for trust in [0.4_f64, 0.3_f64] {
            persist_rule(
                &repo,
                &mut a,
                AffinityDeltas {
                    trust,
                    ..Default::default()
                },
            )
            .await;
        }

        let rows: Vec<(serde_json::Value, serde_json::Value, serde_json::Value)> = sqlx::query_as(
            "SELECT state_before, state_after, effective_deltas \
             FROM engine.companion_affinity_events \
             WHERE affinity_id = $1 ORDER BY created_at ASC, id ASC",
        )
        .bind(a.id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);

        let axis = |v: &serde_json::Value, k: &str| v.get(k).unwrap().as_f64().unwrap();

        for (before, after, effective) in &rows {
            // Within a row the snapshots bracket exactly the delta application.
            for k in [
                "warmth", "trust", "intrigue", "intimacy", "patience", "tension",
            ] {
                assert!(
                    (axis(after, k) - axis(before, k) - axis(effective, k)).abs() < 1e-9,
                    "state_after - state_before must equal effective_deltas on {k}"
                );
            }
            // The snapshot records the derived values as they stood, so a
            // replay never has to re-derive them under later thresholds.
            assert!(
                (axis(after, "bond") - (axis(after, "trust") + axis(after, "intrigue")) / 2.0)
                    .abs()
                    < 1e-9
            );
            assert!(after.get("bond_tier").is_some());
            assert!(after.get("chem_tier").is_some());
            assert!(after.get("warmth_grade").is_some());
        }

        // Consecutive turns with no elapsed gap chain exactly: the second
        // turn's baseline IS the first turn's result. Where a gap exists this
        // difference is the absence effect, which previously appeared nowhere.
        for k in ["trust", "intrigue", "intimacy", "tension"] {
            assert!(
                (axis(&rows[0].1, k) - axis(&rows[1].0, k)).abs() < 1e-9,
                "previous state_after must be the next state_before on {k}"
            );
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn ghost_event_writes_equal_state_snapshots(pool: PgPool) {
        let repo = AffinityRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, instance_id).await;
        let mut a = repo
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        repo.record_ghost(&mut a).await.unwrap();

        let (before, after): (Option<serde_json::Value>, Option<serde_json::Value>) =
            sqlx::query_as(
                "SELECT state_before, state_after FROM engine.companion_affinity_events \
                 WHERE affinity_id = $1 AND event_type = 'ghost'",
            )
            .bind(a.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        // No axis moves, but both are written — a NULL here would put a hole
        // back in the chain.
        let (before, after) = (before.expect("state_before"), after.expect("state_after"));
        for k in [
            "warmth", "trust", "intrigue", "intimacy", "patience", "tension",
        ] {
            assert_eq!(before.get(k), after.get(k), "a ghost moves no axis ({k})");
        }
        assert_ne!(before.get("total_ghosts"), after.get("total_ghosts"));
    }
}
