// SPDX-License-Identifier: AGPL-3.0-only
//! Shared response DTOs used by more than one routing subtree.
//!
//! Currently holds:
//!   * `AffinitySnapshot` — point-in-time view of the 6-axis affinity
//!     vector, used by both `/comp/affinity/{sid}` (debug) and
//!     `/bff/v1/comp/chat/start` (Plan C).

use serde::{Deserialize, Serialize};

use eros_engine_core::affinity::{bar, Affinity, RelationshipLabel};
use crate::routes::companion::AffinityDeltasDto;

/// Point-in-time projection of a session's `Affinity`. Same field set
/// as the historical `AffinityDebugResponse`; renamed because it now
/// flows through non-debug surfaces too.
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
    pub relationship_label: Option<String>,
    pub updated_at: String,
    /// Friendship bar fill, 0..1 (curve-applied; render as %).
    pub bond: f64,
    /// Romance bar fill, 0..1 (curve-applied; render as %).
    pub chemistry: f64,
    /// Friendship tier key (`acquaintance`/`friend`/`close_friend`/`confidant`).
    pub bond_label: String,
    /// Romance tier key (`spark`/`flirtation`/`crush`/`lover`).
    pub chemistry_label: String,
}

fn label_to_str(l: RelationshipLabel) -> String {
    match l {
        RelationshipLabel::Stranger => "stranger",
        RelationshipLabel::Romantic => "romantic",
        RelationshipLabel::Friend => "friend",
        RelationshipLabel::Frenemy => "frenemy",
        RelationshipLabel::SlowBurn => "slow_burn",
    }
    .to_string()
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
            relationship_label: a.relationship_label.map(label_to_str),
            updated_at: a.updated_at.to_rfc3339(),
            bond: bar(a.bond_score()),
            chemistry: bar(a.chemistry_score()),
            bond_label: a.bond_label().as_key().to_string(),
            chemistry_label: a.chemistry_label().as_key().to_string(),
        }
    }
}

/// One turn's post-EMA delta folded into the two lines (raw-composite units).
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct BondChemistryDeltas {
    pub bond: f64,
    pub chemistry: f64,
}

impl BondChemistryDeltas {
    /// Linear fold of a per-axis (post-EMA) delta into the two lines. Exact while
    /// warmth stays ≥ 0 across the turn (warmth is floored at 0 in the absolute
    /// composite); a good per-turn pulse otherwise.
    pub fn from_axis_deltas(d: &AffinityDeltasDto) -> Self {
        Self {
            bond: (d.warmth + d.trust + d.intrigue) / 3.0,
            chemistry: (d.warmth + d.intimacy + d.tension) / 3.0,
        }
    }
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
            ghost_streak: 0,
            last_ghost_at: None,
            total_ghosts: 0,
            relationship_label: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn snapshot_exposes_bond_chemistry_bars_and_labels() {
        // bond = (0+0.9+0.9)/3 = 0.6 → tier3 close_friend ; chemistry = 0 → spark
        let snap = AffinitySnapshot::from(affinity(0.0, 0.9, 0.9, 0.0, 0.0));
        assert_eq!(snap.bond_label, "close_friend");
        assert_eq!(snap.chemistry_label, "spark");
        // bar(0.6): tier3 band 0.50 + (0.6-0.35)/0.27*0.25
        assert!((snap.bond - (0.50 + (0.6 - 0.35) / 0.27 * 0.25)).abs() < 1e-9);
        assert!((snap.chemistry).abs() < 1e-9);
    }

    #[test]
    fn bond_chemistry_deltas_fold_axes() {
        let d = crate::routes::companion::AffinityDeltasDto {
            warmth: 0.3,
            trust: 0.3,
            intrigue: 0.0,
            intimacy: 0.6,
            patience: 0.0,
            tension: 0.0,
        };
        let f = BondChemistryDeltas::from_axis_deltas(&d);
        assert!((f.bond - 0.2).abs() < 1e-9); // (0.3+0.3+0)/3
        assert!((f.chemistry - 0.3).abs() < 1e-9); // (0.3+0.6+0)/3
    }
}
