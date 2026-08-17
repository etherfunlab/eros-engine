// SPDX-License-Identifier: AGPL-3.0-only
//! Shared response DTOs used by more than one routing subtree.
//!
//! Currently holds:
//!   * `AffinitySnapshot` — point-in-time view of the affinity vector,
//!     returned by `GET /bff/v1/comp/affinity/{sid}`.

use serde::{Deserialize, Serialize};

use eros_engine_core::affinity::Affinity;

/// Point-in-time projection of a session's `Affinity`.
///
/// Serialise this only from an `Affinity` that has had `apply_time_decay()`
/// and `refresh_endpoints()` applied: `warmth` and `patience` are derived as
/// of 4.0 and their stored columns are a write-time cache, so a snapshot taken
/// straight off the row reads systematically warm.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct AffinitySnapshot {
    pub warmth: f64,
    pub trust: f64,
    pub intrigue: f64,
    pub intimacy: f64,
    pub patience: f64,
    pub tension: f64,
    pub ghost_streak: i32,
    pub total_ghosts: i32,
    pub updated_at: String,
    /// Friendship line score, 0..1 — the real stored composite, no display
    /// curve (affinity 3.0 dropped the `bar()` projection; nonlinearity now
    /// lives in the write-side tier decay, so the raw value already reads
    /// fast-early/slow-late).
    pub bond: f64,
    /// Romance line score, 0..1 — real stored composite, no display curve.
    pub chemistry: f64,
    /// Friendship tier, 1..=5. Returned alongside the key so a client needs
    /// neither the thresholds nor an ordered tier array.
    pub bond_tier: u8,
    /// Romance tier, 1..=5.
    pub chem_tier: u8,
    /// Friendship tier key (`acquaintance`/`friend`/`close_friend`/`confidant`/`soulmate`).
    pub bond_label: String,
    /// Romance tier key (`spark`/`flirtation`/`crush`/`lover`/`beloved`).
    pub chemistry_label: String,
}

impl From<Affinity> for AffinitySnapshot {
    fn from(a: Affinity) -> Self {
        Self {
            warmth: a.warmth,
            trust: a.trust,
            intrigue: a.intrigue,
            intimacy: a.intimacy,
            patience: a.patience,
            tension: a.tension,
            ghost_streak: a.ghost_streak,
            total_ghosts: a.total_ghosts,
            updated_at: a.updated_at.to_rfc3339(),
            bond: a.bond_score(),
            chemistry: a.chemistry_score(),
            bond_tier: a.bond_tier(),
            chem_tier: a.chem_tier(),
            bond_label: a.bond_label().as_key().to_string(),
            chemistry_label: a.chemistry_label().as_key().to_string(),
        }
    }
}

/// One turn's exact per-turn bond/chemistry line delta, computed at persist time
/// from the floored before/after scores and read from the stored event column.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BondChemistryDeltas {
    pub bond: f64,
    pub chemistry: f64,
}

/// One line's tier transition (serialised keys). Read-side mirror of
/// `eros_engine_core::affinity::LabelTransition`.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LabelTransitionDto {
    pub from: String,
    pub to: String,
}

/// Per-turn tier transition across the two lines, read from the stored
/// `companion_affinity_events.label_changes` JSONB.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TurnLabelChangesDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bond: Option<LabelTransitionDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chemistry: Option<LabelTransitionDto>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use eros_engine_core::affinity::Affinity;
    use uuid::Uuid;

    fn affinity(warmth: f64, trust: f64, intrigue: f64, intimacy: f64, tension: f64) -> Affinity {
        let now = chrono::Utc::now();
        Affinity {
            id: Uuid::nil(),
            session_id: Uuid::nil(),
            user_id: Uuid::nil(),
            instance_id: Uuid::nil(),
            warmth,
            trust,
            intrigue,
            intimacy,
            patience: 0.5,
            tension,
            warmth_grade: 2,
            patience_grade: 2,
            ghost_streak: 0,
            last_ghost_at: None,
            total_ghosts: 0,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn snapshot_exposes_raw_line_scores_and_labels() {
        // bond = (0.6+0.6)/2 = 0.6 → tier3 close_friend ; chemistry = 0 → spark
        let snap = AffinitySnapshot::from(affinity(0.0, 0.6, 0.6, 0.0, 0.0));
        assert_eq!(snap.bond_label, "close_friend");
        assert_eq!(snap.chemistry_label, "spark");
        // The real composite, not a projection.
        assert!((snap.bond - 0.6).abs() < 1e-9);
        assert!((snap.chemistry).abs() < 1e-9);
    }

    #[test]
    fn snapshot_tier_number_and_key_agree() {
        // Both are emitted so a client needs no threshold table; they must never
        // disagree — same `tier_index` behind each.
        let snap = AffinitySnapshot::from(affinity(0.0, 0.6, 0.6, 0.0, 0.0));
        assert_eq!(snap.bond_tier, 3);
        assert_eq!(snap.bond_label, "close_friend");
        assert_eq!(snap.chem_tier, 1);
        assert_eq!(snap.chemistry_label, "spark");
    }

    #[test]
    fn snapshot_of_fresh_row_is_bottom_tier() {
        let snap = AffinitySnapshot::from(affinity(0.1, 0.0, 0.0, 0.0, 0.0));
        assert_eq!(snap.bond_tier, 1);
        assert_eq!(snap.chem_tier, 1);
        assert_eq!(snap.bond_label, "acquaintance");
        assert_eq!(snap.chemistry_label, "spark");
    }
}
