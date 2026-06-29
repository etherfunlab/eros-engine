// SPDX-License-Identifier: AGPL-3.0-only
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Affinity {
    pub id: Uuid,
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub warmth: f64,   // -1.0 ..= 1.0
    pub trust: f64,    //  0.0 ..= 1.0
    pub intrigue: f64, //  0.0 ..= 1.0
    pub intimacy: f64, //  0.0 ..= 1.0
    pub patience: f64, //  0.0 ..= 1.0
    pub tension: f64,  //  0.0 ..= 1.0
    pub ghost_streak: i32,
    pub last_ghost_at: Option<DateTime<Utc>>,
    pub total_ghosts: i32,
    pub relationship_label: Option<RelationshipLabel>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipLabel {
    Stranger,
    Romantic,
    Friend,
    Frenemy,
    SlowBurn,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AffinityDeltas {
    pub warmth: f64,
    pub trust: f64,
    pub intrigue: f64,
    pub intimacy: f64,
    pub patience: f64,
    pub tension: f64,
}

impl Affinity {
    /// Apply LLM-evaluated deltas with EMA smoothing.
    /// `ema_inertia ∈ [0, 1]` — 0 means full update; v1 default is 0.5 (gain 0.5).
    pub fn apply_deltas(&mut self, d: &AffinityDeltas, ema_inertia: f64) {
        let blend = 1.0 - ema_inertia;
        self.warmth = clamp(self.warmth + blend * d.warmth, -1.0, 1.0);
        self.trust = clamp(self.trust + blend * d.trust, 0.0, 1.0);
        self.intrigue = clamp(self.intrigue + blend * d.intrigue, 0.0, 1.0);
        self.intimacy = clamp(self.intimacy + blend * d.intimacy, 0.0, 1.0);
        self.patience = clamp(self.patience + blend * d.patience, 0.0, 1.0);
        self.tension = clamp(self.tension + blend * d.tension, 0.0, 1.0);
        self.updated_at = Utc::now();
    }

    pub fn apply_time_decay(&mut self) {
        let days = (Utc::now() - self.updated_at).num_minutes() as f64 / (60.0 * 24.0);
        if days <= 0.0 {
            return;
        }
        self.intrigue = clamp(self.intrigue - 0.01 * days, 0.0, 1.0);
        self.patience = clamp(self.patience + 0.005 * days, 0.0, 1.0);
        self.tension = clamp(self.tension - 0.005 * days, 0.0, 1.0);
    }

    /// Legacy 5-name relationship label (back-compat), derived purely from the
    /// two line scores — replaces the old multi-axis `infer_label` heuristic.
    /// New consumers should read `bond_label`/`chemistry_label`. `frenemy` is
    /// retired from emission (kept in the enum for parse compat).
    pub fn legacy_relationship_label(&self) -> RelationshipLabel {
        let bond = self.bond_score();
        let chem = self.chemistry_score();
        if tier_index(bond) == 1 && tier_index(chem) == 1 {
            return RelationshipLabel::Stranger;
        }
        if chem > bond {
            if tier_index(chem) >= 3 {
                RelationshipLabel::Romantic
            } else {
                RelationshipLabel::SlowBurn
            }
        } else {
            RelationshipLabel::Friend
        }
    }
}

fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

// ─── Bond / Chemistry lines (read-layer folds of the 6 axes) ────────
//
// Two composites folded from the unchanged 6-axis base. `warmth` is shared into
// both and FLOORED at 0 (a neutral/cold session contributes nothing, so a fresh
// session sits near 0). `patience` is rule-owned and excluded.
//
// Mirrored by the `bond`/`chemistry` GENERATED columns in store migration 0029
// (warmth floored via GREATEST(warmth,0)). Keep the formula in sync.

/// Tier upper bounds on a line's 0..1 score. Widening by design: easy early, a
/// grind near the top. Tier 1 = [0, T1), 2 = [T1, T2), 3 = [T2, T3), 4 = [T3, 1].
/// Tunable.
const TIER1_HI: f64 = 0.15;
const TIER2_HI: f64 = 0.35;
const TIER3_HI: f64 = 0.62;

/// 1..=4 tier index for a 0..1 line score.
fn tier_index(score: f64) -> u8 {
    if score < TIER1_HI {
        1
    } else if score < TIER2_HI {
        2
    } else if score < TIER3_HI {
        3
    } else {
        4
    }
}

/// Map a 0..1 line score to a 0..1 bar fill: each tier fills an even 25% band,
/// linear within. Higher tiers span more raw score, so the bar fills fast early
/// and crawls near the top. Tunable alongside the thresholds.
pub fn bar(score: f64) -> f64 {
    let (lo, hi, band_lo) = match tier_index(score) {
        1 => (0.0, TIER1_HI, 0.0),
        2 => (TIER1_HI, TIER2_HI, 0.25),
        3 => (TIER2_HI, TIER3_HI, 0.50),
        _ => (TIER3_HI, 1.0, 0.75),
    };
    let within = ((score - lo) / (hi - lo)).clamp(0.0, 1.0);
    (band_lo + within * 0.25).clamp(0.0, 1.0)
}

/// Friendship-line tier (pure function of `bond_score`). Serialised snake_case
/// key is the frontend's lookup; Chinese display lives in the frontend.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BondLabel {
    Acquaintance,
    Friend,
    CloseFriend,
    Confidant,
}

impl BondLabel {
    pub fn as_key(self) -> &'static str {
        match self {
            BondLabel::Acquaintance => "acquaintance",
            BondLabel::Friend => "friend",
            BondLabel::CloseFriend => "close_friend",
            BondLabel::Confidant => "confidant",
        }
    }
}

/// Romance-line tier (pure function of `chemistry_score`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChemistryLabel {
    Spark,
    Flirtation,
    Crush,
    Lover,
}

impl ChemistryLabel {
    pub fn as_key(self) -> &'static str {
        match self {
            ChemistryLabel::Spark => "spark",
            ChemistryLabel::Flirtation => "flirtation",
            ChemistryLabel::Crush => "crush",
            ChemistryLabel::Lover => "lover",
        }
    }
}

/// One line's tier transition this turn, as serialised keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelTransition {
    pub from: String,
    pub to: String,
}

/// Per-turn tier transition across the two lines. Serde skips `None` fields, so
/// a JSON object only carries the line(s) that actually moved.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnLabelChanges {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bond: Option<LabelTransition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chemistry: Option<LabelTransition>,
}

impl TurnLabelChanges {
    pub fn is_empty(&self) -> bool {
        self.bond.is_none() && self.chemistry.is_none()
    }
}

/// Tier transition over a delta-only span (before = post-decay/pre-delta,
/// after = post-delta). `None` when neither line crossed a tier.
pub fn diff_labels(before: &Affinity, after: &Affinity) -> Option<TurnLabelChanges> {
    let bond = (before.bond_label() != after.bond_label()).then(|| LabelTransition {
        from: before.bond_label().as_key().to_string(),
        to: after.bond_label().as_key().to_string(),
    });
    let chemistry =
        (before.chemistry_label() != after.chemistry_label()).then(|| LabelTransition {
            from: before.chemistry_label().as_key().to_string(),
            to: after.chemistry_label().as_key().to_string(),
        });
    let changes = TurnLabelChanges { bond, chemistry };
    (!changes.is_empty()).then_some(changes)
}

impl Affinity {
    /// 0..1 friendship composite. warmth floored at 0; mirrors the `bond`
    /// generated column in migration 0029.
    pub fn bond_score(&self) -> f64 {
        let warm_pos = self.warmth.max(0.0);
        clamp((warm_pos + self.trust + self.intrigue) / 3.0, 0.0, 1.0)
    }

    /// 0..1 romance composite. warmth floored at 0; mirrors the `chemistry`
    /// generated column in migration 0029.
    pub fn chemistry_score(&self) -> f64 {
        let warm_pos = self.warmth.max(0.0);
        clamp((warm_pos + self.intimacy + self.tension) / 3.0, 0.0, 1.0)
    }

    /// Friendship-line tier label.
    pub fn bond_label(&self) -> BondLabel {
        match tier_index(self.bond_score()) {
            1 => BondLabel::Acquaintance,
            2 => BondLabel::Friend,
            3 => BondLabel::CloseFriend,
            _ => BondLabel::Confidant,
        }
    }

    /// Romance-line tier label.
    pub fn chemistry_label(&self) -> ChemistryLabel {
        match tier_index(self.chemistry_score()) {
            1 => ChemistryLabel::Spark,
            2 => ChemistryLabel::Flirtation,
            3 => ChemistryLabel::Crush,
            _ => ChemistryLabel::Lover,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Affinity {
        let now = Utc::now();
        Affinity {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            instance_id: Uuid::new_v4(),
            warmth: 0.3,
            trust: 0.2,
            intrigue: 0.5,
            intimacy: 0.0,
            patience: 0.5,
            tension: 0.1,
            ghost_streak: 0,
            last_ghost_at: None,
            total_ghosts: 0,
            relationship_label: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn apply_deltas_clamps_to_valid_ranges() {
        let mut a = fresh();
        a.apply_deltas(
            &AffinityDeltas {
                warmth: 5.0, // would push past 1.0
                trust: -2.0, // would push below 0.0
                intrigue: 0.1,
                intimacy: 0.0,
                patience: 0.0,
                tension: 0.0,
            },
            /*ema_inertia*/ 0.0,
        ); // no smoothing → direct apply
        assert_eq!(a.warmth, 1.0, "warmth clamps to 1.0 (max)");
        assert_eq!(a.trust, 0.0, "trust clamps to 0.0 (min)");
        assert!((a.intrigue - 0.6).abs() < 1e-9);
    }

    #[test]
    fn warmth_can_go_negative_others_cannot() {
        let mut a = fresh();
        a.apply_deltas(
            &AffinityDeltas {
                warmth: -2.0, // -1.0 floor
                trust: 0.0,
                intrigue: 0.0,
                intimacy: 0.0,
                patience: 0.0,
                tension: 0.0,
            },
            0.0,
        );
        assert_eq!(a.warmth, -1.0);
    }

    #[test]
    fn ema_smoothing_applies_inertia() {
        // EMA with inertia=0.8: blended = (1.0 - 0.8) * delta = 0.2 * delta
        let mut a = fresh();
        let before = a.warmth;
        a.apply_deltas(
            &AffinityDeltas {
                warmth: 0.5,
                trust: 0.0,
                intrigue: 0.0,
                intimacy: 0.0,
                patience: 0.0,
                tension: 0.0,
            },
            0.8,
        );
        assert!((a.warmth - (before + 0.5 * 0.2)).abs() < 1e-9);
    }

    #[test]
    fn apply_deltas_combined_then_gains_and_clamps() {
        // A pre-summed (rule + llm) delta on a hot axis at v1 pacing
        // (ema_inertia 0.5 → gain 0.5): 0.3 + 0.5 * 0.15 = 0.375.
        let mut a = fresh(); // warmth 0.3
        a.apply_deltas(
            &AffinityDeltas {
                warmth: 0.15,
                ..Default::default()
            },
            0.5,
        );
        assert!((a.warmth - 0.375).abs() < 1e-9);
    }

    #[test]
    fn time_decay_reduces_intrigue_recovers_patience_softens_tension() {
        let mut a = fresh();
        a.intrigue = 0.5;
        a.patience = 0.5;
        a.tension = 0.5;
        a.warmth = 0.7;
        a.trust = 0.6;
        a.intimacy = 0.4;
        a.updated_at = Utc::now() - chrono::Duration::days(10);

        a.apply_time_decay();

        // 10 days * -0.01/day = -0.1
        assert!((a.intrigue - 0.4).abs() < 1e-9);
        // 10 days * +0.005/day = +0.05
        assert!((a.patience - 0.55).abs() < 1e-9);
        // 10 days * -0.005/day = -0.05
        assert!((a.tension - 0.45).abs() < 1e-9);
        // unchanged
        assert_eq!(a.warmth, 0.7);
        assert_eq!(a.trust, 0.6);
        assert_eq!(a.intimacy, 0.4);
    }

    #[test]
    fn time_decay_clamps_at_floors_and_ceilings() {
        let mut a = fresh();
        a.intrigue = 0.05;
        a.patience = 0.95;
        a.tension = 0.02;
        a.updated_at = Utc::now() - chrono::Duration::days(100);

        a.apply_time_decay();

        assert_eq!(a.intrigue, 0.0);
        assert_eq!(a.patience, 1.0);
        assert_eq!(a.tension, 0.0);
    }

    #[test]
    fn bond_chemistry_scores_fold_axes_with_warmth_floored() {
        let mut a = fresh();
        a.warmth = 0.2;
        a.trust = 0.4;
        a.intrigue = 0.6;
        a.intimacy = 0.1;
        a.tension = 0.3;
        // bond = (0.2 + 0.4 + 0.6)/3 = 0.4
        assert!((a.bond_score() - 0.4).abs() < 1e-9);
        // chemistry = (0.2 + 0.1 + 0.3)/3 = 0.2
        assert!((a.chemistry_score() - 0.2).abs() < 1e-9);
        // negative warmth floors to 0 in the composite
        a.warmth = -1.0;
        a.trust = 0.0;
        a.intrigue = 0.0;
        assert!((a.bond_score()).abs() < 1e-9);
    }

    #[test]
    fn tier_index_boundaries() {
        assert_eq!(tier_index(0.0), 1);
        assert_eq!(tier_index(0.149), 1);
        assert_eq!(tier_index(0.15), 2);
        assert_eq!(tier_index(0.349), 2);
        assert_eq!(tier_index(0.35), 3);
        assert_eq!(tier_index(0.619), 3);
        assert_eq!(tier_index(0.62), 4);
        assert_eq!(tier_index(1.0), 4);
    }

    #[test]
    fn bar_maps_tiers_to_even_bands() {
        assert!((bar(0.0)).abs() < 1e-9);
        assert!((bar(0.15) - 0.25).abs() < 1e-9);
        assert!((bar(0.35) - 0.50).abs() < 1e-9);
        assert!((bar(0.62) - 0.75).abs() < 1e-9);
        assert!((bar(1.0) - 1.0).abs() < 1e-9);
        // midpoint of tier 1 [0,0.15) → 0.075 → half of the 0..0.25 band
        assert!((bar(0.075) - 0.125).abs() < 1e-9);
    }

    #[test]
    fn labels_map_from_scores() {
        let mut a = fresh();
        a.warmth = 0.0;
        a.trust = 0.0;
        a.intrigue = 0.0;
        assert_eq!(a.bond_label(), BondLabel::Acquaintance); // bond 0
        a.trust = 0.6;
        a.intrigue = 0.6; // bond = 0.4 → tier 3
        assert_eq!(a.bond_label(), BondLabel::CloseFriend);
        a.warmth = 0.0;
        a.intimacy = 0.0;
        a.tension = 0.0;
        assert_eq!(a.chemistry_label(), ChemistryLabel::Spark); // chem 0
        a.intimacy = 1.0;
        a.tension = 1.0; // chem = 0.667 → tier 4
        assert_eq!(a.chemistry_label(), ChemistryLabel::Lover);
    }

    #[test]
    fn legacy_label_stranger_when_both_tier1() {
        let mut a = fresh();
        a.warmth = 0.0;
        a.trust = 0.0;
        a.intrigue = 0.0;
        a.intimacy = 0.0;
        a.tension = 0.0;
        assert_eq!(a.legacy_relationship_label(), RelationshipLabel::Stranger);
    }

    #[test]
    fn legacy_label_friend_when_bond_leads() {
        let mut a = fresh();
        // bond = (0.3+0.6+0.6)/3 = 0.5 ; chem = (0.3+0+0)/3 = 0.1
        a.warmth = 0.3;
        a.trust = 0.6;
        a.intrigue = 0.6;
        a.intimacy = 0.0;
        a.tension = 0.0;
        assert_eq!(a.legacy_relationship_label(), RelationshipLabel::Friend);
    }

    #[test]
    fn legacy_label_romantic_when_chemistry_high() {
        let mut a = fresh();
        // chem = (0.3+0.9+0.9)/3 = 0.7 (tier4) ; bond = 0.1
        a.warmth = 0.3;
        a.intimacy = 0.9;
        a.tension = 0.9;
        a.trust = 0.0;
        a.intrigue = 0.0;
        assert_eq!(a.legacy_relationship_label(), RelationshipLabel::Romantic);
    }

    #[test]
    fn legacy_label_slow_burn_when_chemistry_leads_but_mid() {
        let mut a = fresh();
        // chem = (0.3+0.3+0.2)/3 ≈ 0.267 (tier2) ; bond = 0.1 (tier1)
        a.warmth = 0.3;
        a.intimacy = 0.3;
        a.tension = 0.2;
        a.trust = 0.0;
        a.intrigue = 0.0;
        assert_eq!(a.legacy_relationship_label(), RelationshipLabel::SlowBurn);
    }

    #[test]
    fn diff_labels_none_when_no_tier_change() {
        let a = fresh();
        let b = a.clone();
        assert!(diff_labels(&a, &b).is_none());
    }

    #[test]
    fn diff_labels_reports_single_line_change() {
        let mut before = fresh();
        before.warmth = 0.0;
        before.trust = 0.0;
        before.intrigue = 0.0;
        before.intimacy = 0.0;
        before.tension = 0.0; // bond + chem both tier 1
        let mut after = before.clone();
        after.trust = 0.9;
        after.intrigue = 0.9; // bond = 0.6 → tier 3 (close_friend)
        let d = diff_labels(&before, &after).unwrap();
        let bond = d.bond.unwrap();
        assert_eq!(bond.from, "acquaintance");
        assert_eq!(bond.to, "close_friend");
        assert!(d.chemistry.is_none());
    }

    #[test]
    fn diff_labels_reports_both_lines() {
        let mut before = fresh();
        before.warmth = 0.0;
        before.trust = 0.0;
        before.intrigue = 0.0;
        before.intimacy = 0.0;
        before.tension = 0.0;
        let mut after = before.clone();
        after.trust = 0.9;
        after.intrigue = 0.9; // bond → close_friend
        after.intimacy = 0.9;
        after.tension = 0.9; // chem = 0.6 → tier 3 (crush)
        let d = diff_labels(&before, &after).unwrap();
        assert_eq!(d.bond.unwrap().to, "close_friend");
        assert_eq!(d.chemistry.unwrap().to, "crush");
    }
}
