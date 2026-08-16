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
    pub warmth: f64,   //  0.0 ..= 1.0 (derived cache, 4.0 — see refresh_endpoints)
    pub trust: f64,    //  0.0 ..= 1.0
    pub intrigue: f64, //  0.0 ..= 1.0
    pub intimacy: f64, //  0.0 ..= 1.0
    pub patience: f64, //  0.0 ..= 1.0 (derived cache, 4.0)
    pub tension: f64,  //  0.0 ..= 1.0
    /// Judge's last absolute warmth level (1..=3). Authoritative; `warmth` is
    /// a materialized cache of `endpoint_value` over it.
    pub warmth_grade: i16,
    /// Judge's last absolute patience level (1..=3).
    pub patience_grade: i16,
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
    /// Apply committed deltas directly, clamping each axis to its range.
    /// Damping lives in `grade_turn`'s tier decay, so a committed delta means
    /// exactly what it says.
    pub fn apply_deltas(&mut self, d: &AffinityDeltas) {
        self.warmth = clamp(self.warmth + d.warmth, 0.0, 1.0);
        self.trust = clamp(self.trust + d.trust, 0.0, 1.0);
        self.intrigue = clamp(self.intrigue + d.intrigue, 0.0, 1.0);
        self.intimacy = clamp(self.intimacy + d.intimacy, 0.0, 1.0);
        self.patience = clamp(self.patience + d.patience, 0.0, 1.0);
        self.tension = clamp(self.tension + d.tension, 0.0, 1.0);
        self.updated_at = Utc::now();
    }

    /// Per-day drift on the line axes only (intrigue cools, tension softens).
    /// The old patience up-drift is retired: absence handling for the two
    /// endpoints lives in `refresh_endpoints`' multiplicative decay, which
    /// cools rather than heals — an old friend's resilience comes from
    /// B(bond), not from a drift patch.
    pub fn apply_time_decay(&mut self) {
        let days = (Utc::now() - self.updated_at).num_minutes() as f64 / (60.0 * 24.0);
        if days <= 0.0 {
            return;
        }
        self.intrigue = clamp(self.intrigue - 0.01 * days, 0.0, 1.0);
        self.tension = clamp(self.tension - 0.005 * days, 0.0, 1.0);
    }

    /// Recompute the two derived endpoints from the authoritative facts
    /// (judge levels + line scores + time since last update). Runs wherever
    /// `apply_time_decay` runs: the row-locked persist and the in-memory
    /// read paths. Line scores no longer contain warmth, so there is no
    /// circularity.
    pub fn refresh_endpoints(&mut self, t: &AffinityTuning) {
        let days = (Utc::now() - self.updated_at).num_minutes() as f64 / (60.0 * 24.0);
        let decay = endpoint_time_decay(days, t.time_decay_rate, t.time_decay_floor);
        self.warmth = endpoint_value(
            self.warmth_grade,
            self.chemistry_score(),
            decay,
            t.floor_ratio,
        );
        self.patience =
            endpoint_value(self.patience_grade, self.bond_score(), decay, t.floor_ratio);
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

// ─── Bond / Chemistry lines (read-layer folds of the 4 line axes) ───
//
// As of 4.0 the two lines share nothing: bond is friendship (trust + continued
// interest), chemistry is romance (closeness + charge). warmth and patience are
// no longer inputs to either line — they are OUTPUTS, derived from the judge's
// absolute levels amplified by the counterpart line (see endpoint derivation
// below).
//
// Mirrored by the `bond`/`chemistry` GENERATED columns in store migration 0048.
// Keep the formula in sync.

/// Tier upper bounds on a line's 0..1 score. Widening by design: easy early, a
/// grind near the top. Tier 1 = [0, T1), 2 = [T1, T2), 3 = [T2, T3),
/// 4 = [T3, T4), 5 = [T4, 1]. Tunable.
const TIER1_HI: f64 = 0.15;
const TIER2_HI: f64 = 0.35;
const TIER3_HI: f64 = 0.62;
const TIER4_HI: f64 = 0.9;

/// Floor of the top intimacy rung, on `max(bond_score, chemistry_score)`. Sits
/// *inside* tier 4 rather than on the tier-5 edge, deliberately loose: the rung
/// ladder exists to stop a stranger talking their way into a nude, not to make
/// intimacy expensive, and gating the top rung at the apex (`TIER4_HI`) is a
/// wall rather than a gate. The bottom rung still folds `TIER1_HI`, so only this
/// cut is independent — keep it in `(TIER3_HI, TIER4_HI)` so the rungs stay
/// coarser than the tier ladder they sit on. Tunable.
const INTIMACY_RUNG3_LO: f64 = 0.76;

const _: () = assert!(
    TIER3_HI < INTIMACY_RUNG3_LO && INTIMACY_RUNG3_LO < TIER4_HI,
    "the top intimacy rung must open inside tier 4, not at the apex"
);

/// Patience band cut-points: low = [0, LO), mid = [LO, HI), high = [HI, 1].
/// Separate from the tier ladder above on purpose — `patience` is rule-owned
/// and never folded into either composite, so it carries its own cuts. These
/// mirror the three bands the PDE judge prompt already prescribes; the engine
/// owns them so the judge classifies against a stated band instead of
/// comparing floats itself. Tunable.
const PATIENCE_LO: f64 = 0.35;
const PATIENCE_HI: f64 = 0.65;

// ─── Endpoint derivation (affinity 4.0) ─────────────────────────────
//
// warmth and patience are no longer accumulated state: the judge reports a
// coarse absolute level (1 cold / 2 baseline / 3 warm) and the engine folds it
// into a continuous value using the counterpart LINE score — chemistry warms
// warmth, bond funds patience. Amplification, not correlation.
// Design spec: docs/superpowers/specs/2026-08-16-affinity-40-design.md

/// Boost at a counterpart score of 1.0. With base(3) = 2/3, a full judge level
/// times a full counterpart line lands exactly at 1.0 — a structural
/// commitment, so a code constant rather than a knob (the pivot below is
/// `TIER2_HI` for the same reason: the boost turns positive the moment the
/// counterpart line enters tier 3).
pub const ENDPOINT_BOOST_MAX: f64 = 1.5;

/// The judge's absolute endpoint levels for one turn. `None` = the judge
/// omitted the field or the eval was skipped → hold the stored level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EndpointLevelReads {
    pub warmth: Option<i16>,
    pub patience: Option<i16>,
}

/// base(g) = (g−1)/3 ∈ {0, 1/3, 2/3}; out-of-range stored levels clamp.
fn endpoint_base(level: i16) -> f64 {
    f64::from(level.clamp(1, 3) - 1) / 3.0
}

/// B(x) = 1 + λ·(x − TIER2_HI), λ = (B_MAX − 1)/(1 − TIER2_HI) = 10/13.
/// Below the pivot the endpoint is damped under its base; above it, boosted.
pub fn endpoint_boost(counterpart: f64) -> f64 {
    let slope = (ENDPOINT_BOOST_MAX - 1.0) / (1.0 - TIER2_HI);
    1.0 + slope * (counterpart - TIER2_HI)
}

/// Multiplicative absence decay: 1 − rate·days, floored. Linear like the
/// line-axis drift; the floor keeps long absence from zeroing a relationship.
pub fn endpoint_time_decay(days: f64, rate: f64, floor: f64) -> f64 {
    (1.0 - rate * days.max(0.0)).max(floor)
}

/// One endpoint's real value. The φ·x floor only ever acts on level 1
/// (φ·x ≤ φ < 1/3·B(0) for φ ≤ 0.2): a cold verdict decays to a
/// relationship-scaled ember instead of an absolute zero, and it can never
/// overwrite a non-cold verdict. clamp01 is float insurance, not mechanism.
pub fn endpoint_value(level: i16, counterpart: f64, decay: f64, floor_ratio: f64) -> f64 {
    let boosted = endpoint_base(level) * endpoint_boost(counterpart);
    (boosted.max(floor_ratio * counterpart) * decay).clamp(0.0, 1.0)
}

/// 1..=5 tier index for a 0..1 line score.
fn tier_index(score: f64) -> u8 {
    if score < TIER1_HI {
        1
    } else if score < TIER2_HI {
        2
    } else if score < TIER3_HI {
        3
    } else if score < TIER4_HI {
        4
    } else {
        5
    }
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
    Soulmate,
}

impl BondLabel {
    pub fn as_key(self) -> &'static str {
        match self {
            BondLabel::Acquaintance => "acquaintance",
            BondLabel::Friend => "friend",
            BondLabel::CloseFriend => "close_friend",
            BondLabel::Confidant => "confidant",
            BondLabel::Soulmate => "soulmate",
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
    Beloved,
}

impl ChemistryLabel {
    pub fn as_key(self) -> &'static str {
        match self {
            ChemistryLabel::Spark => "spark",
            ChemistryLabel::Flirtation => "flirtation",
            ChemistryLabel::Crush => "crush",
            ChemistryLabel::Lover => "lover",
            ChemistryLabel::Beloved => "beloved",
        }
    }
}

/// Patience band for the PDE judge. Three bands, not five: the judge prompt
/// prescribes one interaction register per band (how curt the tone runs,
/// whether irritation shows), and the engine states which band applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatienceBand {
    Low,
    Mid,
    High,
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
    /// 0..1 friendship composite. Mirrors the `bond` generated column in
    /// migration 0048.
    pub fn bond_score(&self) -> f64 {
        clamp((self.trust + self.intrigue) / 2.0, 0.0, 1.0)
    }

    /// 0..1 romance composite. Mirrors the `chemistry` generated column in
    /// migration 0048.
    pub fn chemistry_score(&self) -> f64 {
        clamp((self.intimacy + self.tension) / 2.0, 0.0, 1.0)
    }

    /// Friendship-line tier label.
    pub fn bond_label(&self) -> BondLabel {
        match tier_index(self.bond_score()) {
            1 => BondLabel::Acquaintance,
            2 => BondLabel::Friend,
            3 => BondLabel::CloseFriend,
            4 => BondLabel::Confidant,
            _ => BondLabel::Soulmate,
        }
    }

    /// Romance-line tier label.
    pub fn chemistry_label(&self) -> ChemistryLabel {
        match tier_index(self.chemistry_score()) {
            1 => ChemistryLabel::Spark,
            2 => ChemistryLabel::Flirtation,
            3 => ChemistryLabel::Crush,
            4 => ChemistryLabel::Lover,
            _ => ChemistryLabel::Beloved,
        }
    }

    /// Coarse 1..=3 intimacy rung for the PDE image gate, taken over whichever
    /// line is further along. Rung 1 = both lines still tier 1; rung 3 = at or
    /// above `INTIMACY_RUNG3_LO`; rung 2 = everything between. `max` rather than
    /// a sum so a purely romantic track and a purely companionable one can each
    /// unlock on their own.
    ///
    /// The bottom cut folds `TIER1_HI` and so cannot drift away from the
    /// `Acquaintance` / `Spark` labels the rest of the system shows. The top cut
    /// is deliberately its own constant, set below the tier-5 apex — see
    /// `INTIMACY_RUNG3_LO`.
    pub fn intimacy_rung(&self) -> u8 {
        let s = self.bond_score().max(self.chemistry_score());
        if tier_index(s) == 1 {
            1
        } else if s < INTIMACY_RUNG3_LO {
            2
        } else {
            3
        }
    }

    /// Patience band. Reads the raw axis, not a composite — `patience` is
    /// rule-owned and stays outside both folds.
    pub fn patience_band(&self) -> PatienceBand {
        if self.patience < PATIENCE_LO {
            PatienceBand::Low
        } else if self.patience < PATIENCE_HI {
            PatienceBand::Mid
        } else {
            PatienceBand::High
        }
    }
}

// ─── Affinity write-side pipeline (grades → raw → decay → penalty → gate) ───
//
// The judge reports per-axis *grades* (0..=4 magnitude + direction, folded to a
// signed integer at parse time) for the FOUR line axes; the engine owns every
// number. A grade converts to a raw delta at its line's unit, positive raw is
// damped by the line's tier, every axis pays a cross-line penalty while the
// counterpart line is high, and the resulting real delta passes a threshold
// accumulator before it commits. The two endpoints (warmth/patience) never
// enter this pipeline — they are derived, see `refresh_endpoints`. 3.1's
// scope steering is retired: `AffinityScope` is read-side only again.
// Design specs: docs/superpowers/specs/2026-08-13-affinity-30-grade-pipeline-design.md
//               docs/superpowers/specs/2026-08-14-cross-penalty-by-grade-design.md
//               docs/superpowers/specs/2026-08-16-affinity-40-design.md

/// Tuning knobs for the 4.0 pipeline, env-driven server-side.
#[derive(Debug, Clone, PartialEq)]
pub struct AffinityTuning {
    /// Raw score per grade step on the bond axes (`AFFINITY_GRADE_UNIT_BOND`).
    /// The 2.96× spread between the two units is the judge's measured grading
    /// asymmetry (tension reaches grade ≥2 on ~half of turns, trust is graded
    /// 0 on ~80%), written down where it can be argued with.
    pub grade_unit_bond: f64,
    /// Raw score per grade step on the chemistry axes (`AFFINITY_GRADE_UNIT_CHEM`).
    pub grade_unit_chem: f64,
    /// Extra multiplier on negative raw scores (`AFFINITY_NEG_FACTOR`) —
    /// keeps 2.0's "slow up, fast down" asymmetry.
    pub neg_factor: f64,
    /// Positive-delta damping per tier 1..=5 (`AFFINITY_TIER_DECAY`).
    pub tier_decay: [f64; 5],
    /// Cross-line penalty ceiling as a multiple of the line's unit
    /// (`AFFINITY_CROSS_PENALTY_RATIO`): κ_line = ratio · u_line. Tying κ to
    /// the unit makes the double-high break-even independent of the unit —
    /// per-line units would otherwise silently move the wall.
    pub cross_penalty_ratio: f64,
    /// Counterpart line score where the penalty starts (`AFFINITY_CROSS_PENALTY_START`).
    pub cross_penalty_start: f64,
    /// Commit threshold θ (`AFFINITY_DELTA_THRESHOLD`); 0 commits every turn.
    pub delta_threshold: f64,
    /// Multiplier on the judge's positive raw component for demo sessions
    /// (`AFFINITY_DEMO_BOOST`); rule nudges are unaffected.
    pub demo_boost: f64,
    /// Endpoint floor ratio φ (`AFFINITY_FLOOR_RATIO`): a level-1 verdict
    /// reads φ·counterpart instead of 0. Must stay ≤ 0.24 so the floor can
    /// never touch a level-2 verdict (1/3·B(0) ≈ 0.2436).
    pub floor_ratio: f64,
    /// Endpoint absence decay per day (`AFFINITY_TIME_DECAY_RATE`).
    pub time_decay_rate: f64,
    /// Endpoint absence decay floor (`AFFINITY_TIME_DECAY_FLOOR`).
    pub time_decay_floor: f64,
}

impl Default for AffinityTuning {
    fn default() -> Self {
        Self {
            // Derived to reproduce the shipped 3.1 pace (tier 5 in ~99/98
            // turns) after the shared warmth term and the chemistry ladder are
            // both gone; re-derive on a full week of 4.0 data.
            grade_unit_bond: 0.0786,
            grade_unit_chem: 0.0266,
            neg_factor: 1.5,
            tier_decay: [1.0, 0.70, 0.45, 0.25, 0.10],
            // 5/6 = the 3.x κ/u ratio (0.05/0.06) made definitional.
            cross_penalty_ratio: 5.0 / 6.0,
            cross_penalty_start: 0.35,
            delta_threshold: 0.0,
            demo_boost: 1.4,
            floor_ratio: 0.2,
            time_decay_rate: 0.02,
            time_decay_floor: 0.5,
        }
    }
}

/// Signed judge grades, one per line axis, −4..=4 (out-of-range input is
/// clamped). 0 = nothing happened, the overwhelmingly common verdict.
/// `warmth` and `patience` are absent by construction: they are absolute
/// levels on the derived channel (`EndpointLevelReads`), not graded deltas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AxisGrades {
    pub trust: i8,
    pub intrigue: i8,
    pub intimacy: i8,
    pub tension: i8,
}

/// Per-axis balance the threshold gate is still holding back. Persisted as
/// JSONB on the affinity row; absent column reads as all-zero. A stale
/// `"warmth"` key from pre-4.0 rows is ignored by serde and drains naturally.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct PendingDeltas {
    #[serde(default)]
    pub trust: f64,
    #[serde(default)]
    pub intrigue: f64,
    #[serde(default)]
    pub intimacy: f64,
    #[serde(default)]
    pub tension: f64,
}

impl PendingDeltas {
    pub fn is_zero(&self) -> bool {
        self.trust == 0.0 && self.intrigue == 0.0 && self.intimacy == 0.0 && self.tension == 0.0
    }
}

/// One turn through the 4.0 pipeline.
/// `raw` = grade conversion + rule deltas, pre-decay — what the event row
/// records as `deltas`. `committed` = what actually applies to the axes this
/// turn (zero while the gate holds). `pending` = the gate's new balance.
/// The `warmth`/`patience` fields of `raw` and `committed` are always 0.0:
/// the endpoints left the graded pipeline (see `refresh_endpoints`).
#[derive(Debug, Clone, Default)]
pub struct GradeTurnOutcome {
    pub raw: AffinityDeltas,
    pub committed: AffinityDeltas,
    pub pending: PendingDeltas,
    /// Cross-line penalty *assessed* this turn, per axis.
    ///
    /// Assessed, not applied. It is subtracted inside `ρ` before the threshold
    /// gate, so on a turn the gate buffers, nothing has reached the axis yet —
    /// the amount rides along in `pending` rather than being lost (the gate
    /// re-times commits, it never rescales them), and the caller's axis clamp
    /// can swallow part of a commit besides. The penalty scales with the
    /// applied grade, so it is not derivable from the grades alone and is
    /// recorded rather than reconstructed.
    pub cross_penalty_assessed: CrossPenaltyAssessed,
}

/// Per-axis cross-line penalty assessed in one turn (always ≥ 0).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CrossPenaltyAssessed {
    pub trust: f64,
    pub intrigue: f64,
    pub intimacy: f64,
    pub tension: f64,
}

impl CrossPenaltyAssessed {
    pub fn is_zero(&self) -> bool {
        self.trust == 0.0 && self.intrigue == 0.0 && self.intimacy == 0.0 && self.tension == 0.0
    }
}

/// Run one judge verdict through conversion, tier decay, cross-line penalty
/// and the threshold gate. Pure: reads the *pre-turn* affinity snapshot (all
/// axes use the same tier lookup, so ordering inside a turn cannot matter)
/// and returns what to apply; the caller owns clamping and persistence.
pub fn grade_turn(
    a: &Affinity,
    grades: &AxisGrades,
    rule: &AffinityDeltas,
    pending: &PendingDeltas,
    boost: f64,
    t: &AffinityTuning,
) -> GradeTurnOutcome {
    // Pre-turn snapshot: every axis reads the same tiers and counterparts.
    let bond = a.bond_score();
    let chem = a.chemistry_score();
    let bond_tier = tier_index(bond) as usize;
    let chem_tier = tier_index(chem) as usize;

    let decay = |tier: usize| t.tier_decay[tier - 1];
    // κ_line = ratio · u_line, so the break-even below is unit-independent.
    let penalty = |counterpart: f64, kappa: f64| {
        let start = t.cross_penalty_start;
        let ramp = ((counterpart - start).max(0.0) / (1.0 - start)).powi(2);
        kappa * ramp
    };

    // grade → raw at the line's unit: positive grades earn unit × boost,
    // negative grades cost unit × neg_factor. Rule nudges join pre-decay.
    let raw = |g: i8, rule_d: f64, unit: f64| {
        let g = f64::from(g.clamp(-4, 4));
        let judge = if g >= 0.0 {
            g * unit * boost
        } else {
            g * unit * t.neg_factor
        };
        judge + rule_d
    };

    // ρ = D·max(r,0) + min(r,0) − P: positive part damped, negative part full
    // price, and the cross penalty assessed in PROPORTION to the grade actually
    // applied — P = κ·φ(y)·(|g|/4), κ = ratio·u.
    //
    // Ignoring rule nudges, ρ factorises:
    //     g > 0:  ρ = g·u · (D_k − ratio·φ(y)/4)
    //     g < 0:  ρ = g·u · (λ⁻ + ratio·φ(y)/4)      (no decay on the negative part)
    // Neither bracket contains g OR u, so the outcome cannot change sign
    // between grades at a fixed position, and the break-even position
    // φ(y*) = 4·D_k/ratio is the SAME for both lines regardless of their
    // units — see `break_even_position_is_unit_invariant`. The negative
    // bracket is always positive, so a negative verdict always lowers the
    // axis.
    //
    // It does NOT make every positive verdict a gain: past y* every grade nets
    // negative, uniformly — see
    // `tier_five_against_a_high_counterpart_still_loses_at_every_grade`.
    //
    // Rule nudges sit outside this: they join `r` before decay but are not
    // part of `g`, so a large enough opposing nudge could in principle invert
    // the sign. None can today — the only rule deltas reaching a graded axis
    // are intrigue +0.02 and tension +0.03, both positive.
    //
    // Magnitude, not sign: a negative grade already moves the axis away from
    // the double-high position the penalty exists to discourage, so it pays in
    // proportion too rather than at a flat rate.
    let real = |r: f64, own_tier: usize, counterpart: f64, kappa: f64, g: i8| {
        let p = penalty(counterpart, kappa) * f64::from(g.saturating_abs().min(4)) / 4.0;
        (decay(own_tier) * r.max(0.0) + r.min(0.0) - p, p)
    };

    // Threshold gate: signed accumulation, everything commits once |acc| ≥ θ.
    let gate = |rho: f64, pend: f64| {
        let acc = pend + rho;
        if acc.abs() >= t.delta_threshold {
            (acc, 0.0)
        } else {
            (0.0, acc)
        }
    };

    let axis = |g: i8, rule_d: f64, own_tier: usize, counterpart: f64, pend: f64, unit: f64| {
        let r = raw(g, rule_d, unit);
        let kappa = t.cross_penalty_ratio * unit;
        let (rho, charged) = real(r, own_tier, counterpart, kappa, g);
        let (committed, pend) = gate(rho, pend);
        (r, committed, pend, charged)
    };

    let (t_raw, t_com, t_pend, t_pen) = axis(
        grades.trust,
        rule.trust,
        bond_tier,
        chem,
        pending.trust,
        t.grade_unit_bond,
    );
    let (ig_raw, ig_com, ig_pend, ig_pen) = axis(
        grades.intrigue,
        rule.intrigue,
        bond_tier,
        chem,
        pending.intrigue,
        t.grade_unit_bond,
    );
    let (im_raw, im_com, im_pend, im_pen) = axis(
        grades.intimacy,
        rule.intimacy,
        chem_tier,
        bond,
        pending.intimacy,
        t.grade_unit_chem,
    );
    let (tn_raw, tn_com, tn_pend, tn_pen) = axis(
        grades.tension,
        rule.tension,
        chem_tier,
        bond,
        pending.tension,
        t.grade_unit_chem,
    );

    GradeTurnOutcome {
        raw: AffinityDeltas {
            warmth: 0.0, // endpoints are derived, never graded — see refresh_endpoints
            trust: t_raw,
            intrigue: ig_raw,
            intimacy: im_raw,
            tension: tn_raw,
            patience: 0.0,
        },
        committed: AffinityDeltas {
            warmth: 0.0,
            trust: t_com,
            intrigue: ig_com,
            intimacy: im_com,
            tension: tn_com,
            patience: 0.0,
        },
        pending: PendingDeltas {
            trust: t_pend,
            intrigue: ig_pend,
            intimacy: im_pend,
            tension: tn_pend,
        },
        cross_penalty_assessed: CrossPenaltyAssessed {
            trust: t_pen,
            intrigue: ig_pen,
            intimacy: im_pen,
            tension: tn_pen,
        },
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
            warmth_grade: 2,
            patience_grade: 2,
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
        a.apply_deltas(&AffinityDeltas {
            warmth: 5.0, // would push past 1.0
            trust: -2.0, // would push below 0.0
            intrigue: 0.1,
            intimacy: 0.0,
            patience: 0.0,
            tension: 0.0,
        });
        assert_eq!(a.warmth, 1.0, "warmth clamps to 1.0 (max)");
        assert_eq!(a.trust, 0.0, "trust clamps to 0.0 (min)");
        assert!((a.intrigue - 0.6).abs() < 1e-9);
    }

    #[test]
    fn warmth_floors_at_zero_like_every_axis() {
        // 4.0: warmth is 0..1 like everything else — no negative band.
        let mut a = fresh();
        a.apply_deltas(&AffinityDeltas {
            warmth: -2.0,
            trust: 0.0,
            intrigue: 0.0,
            intimacy: 0.0,
            patience: 0.0,
            tension: 0.0,
        });
        assert_eq!(a.warmth, 0.0);
    }

    #[test]
    fn apply_deltas_is_direct_no_smoothing() {
        // A committed delta from grade_turn applies verbatim: 0.3 + 0.15 = 0.45.
        let mut a = fresh(); // warmth 0.3
        a.apply_deltas(&AffinityDeltas {
            warmth: 0.15,
            ..Default::default()
        });
        assert!((a.warmth - 0.45).abs() < 1e-9);
    }

    #[test]
    fn time_decay_reduces_intrigue_softens_tension_leaves_the_rest() {
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
        // 10 days * -0.005/day = -0.05
        assert!((a.tension - 0.45).abs() < 1e-9);
        // unchanged — the endpoints' absence handling lives in refresh_endpoints
        assert_eq!(a.patience, 0.5);
        assert_eq!(a.warmth, 0.7);
        assert_eq!(a.trust, 0.6);
        assert_eq!(a.intimacy, 0.4);
    }

    #[test]
    fn time_decay_clamps_at_floors() {
        let mut a = fresh();
        a.intrigue = 0.05;
        a.tension = 0.02;
        a.updated_at = Utc::now() - chrono::Duration::days(100);

        a.apply_time_decay();

        assert_eq!(a.intrigue, 0.0);
        assert_eq!(a.tension, 0.0);
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
        assert_eq!(tier_index(0.899), 4);
        assert_eq!(tier_index(0.9), 5);
        assert_eq!(tier_index(1.0), 5);
    }

    /// Rung 1 stays welded to the bottom tier labels, so it cannot drift away
    /// from what the rest of the system displays. The top rung has a floor of
    /// its own, deliberately inside tier 4 — a tier-4 relationship already
    /// clears it, well short of the apex. (That the two ladders cannot cross is
    /// a compile-time assertion beside the constant. Exact cut values are not
    /// reachable through the `/3` composites in f64, hence values either side
    /// rather than on them.)
    #[test]
    fn intimacy_rung_cuts_against_the_tier_ladder() {
        let mut a = fresh();

        // Both lines at the bottom label → rung 1.
        a.warmth = 0.1;
        a.trust = 0.0;
        a.intrigue = 0.0;
        a.intimacy = 0.0;
        a.tension = 0.0;
        assert_eq!(a.bond_label(), BondLabel::Acquaintance);
        assert_eq!(a.chemistry_label(), ChemistryLabel::Spark);
        assert_eq!(a.intimacy_rung(), 1);

        // Clear of tier 1, short of the top floor → rung 2.
        a.warmth = 0.7;
        a.trust = 0.7;
        a.intrigue = 0.7;
        assert_eq!(a.intimacy_rung(), 2);

        // Past the floor while still tier 4 → rung 3 without reaching the apex.
        a.warmth = 0.8;
        a.trust = 0.8;
        a.intrigue = 0.8;
        assert_eq!(a.bond_label(), BondLabel::Confidant);
        assert_eq!(a.intimacy_rung(), 3);
    }

    /// `max` over the two lines: either track alone can unlock.
    #[test]
    fn intimacy_rung_takes_the_further_line() {
        let mut a = fresh();
        // chemistry = (warmth + intimacy + tension)/3 = 0.95, bond = 0.1
        a.warmth = 0.95;
        a.trust = 0.0;
        a.intrigue = 0.3;
        a.intimacy = 0.95;
        a.tension = 0.95;
        assert!(a.bond_score() < a.chemistry_score());
        assert_eq!(a.intimacy_rung(), 3);
        // Mirror image: bond ahead, chemistry flat.
        a.trust = 0.95;
        a.intrigue = 0.95;
        a.intimacy = 0.0;
        a.tension = 0.0;
        assert!(a.chemistry_score() < a.bond_score());
        assert_eq!(a.intimacy_rung(), 3);
    }

    /// A brand-new session (migration-0029 seed) is rung 1, not an absent value.
    #[test]
    fn intimacy_rung_of_a_seeded_session_is_one() {
        let mut a = fresh();
        a.warmth = 0.1;
        a.trust = 0.0;
        a.intrigue = 0.0;
        a.intimacy = 0.0;
        a.tension = 0.0;
        assert!(a.bond_score().max(a.chemistry_score()) < TIER1_HI);
        assert_eq!(a.intimacy_rung(), 1);
    }

    #[test]
    fn patience_band_boundaries() {
        let mut a = fresh();
        let mut at = |p: f64| {
            a.patience = p;
            a.patience_band()
        };
        assert_eq!(at(0.0), PatienceBand::Low);
        assert_eq!(at(0.349), PatienceBand::Low);
        assert_eq!(at(0.35), PatienceBand::Mid); // low → mid, inclusive
        assert_eq!(at(0.649), PatienceBand::Mid);
        assert_eq!(at(0.65), PatienceBand::High); // mid → high, inclusive
        assert_eq!(at(1.0), PatienceBand::High);
    }

    /// The band reads the raw axis: moving the composites must not move it.
    #[test]
    fn patience_band_is_independent_of_the_composites() {
        let mut a = fresh();
        a.patience = 0.2;
        a.warmth = 1.0;
        a.trust = 1.0;
        a.intrigue = 1.0;
        a.intimacy = 1.0;
        a.tension = 1.0;
        assert_eq!(a.intimacy_rung(), 3);
        assert_eq!(a.patience_band(), PatienceBand::Low);
    }

    #[test]
    fn labels_map_from_scores() {
        let mut a = fresh();
        a.warmth = 0.0;
        a.trust = 0.0;
        a.intrigue = 0.0;
        assert_eq!(a.bond_label(), BondLabel::Acquaintance); // bond 0
        a.trust = 0.6;
        a.intrigue = 0.6; // bond = 0.6 → tier 3
        assert_eq!(a.bond_label(), BondLabel::CloseFriend);
        a.warmth = 0.0;
        a.intimacy = 0.0;
        a.tension = 0.0;
        assert_eq!(a.chemistry_label(), ChemistryLabel::Spark); // chem 0
        a.intimacy = 0.8;
        a.tension = 0.6; // chem = 0.7 → tier 4
        assert_eq!(a.chemistry_label(), ChemistryLabel::Lover);
        // tier 5 apex
        a.warmth = 1.0;
        a.trust = 1.0;
        a.intrigue = 1.0; // bond = 1.0 → tier 5
        assert_eq!(a.bond_label(), BondLabel::Soulmate);
        a.intimacy = 1.0;
        a.tension = 1.0; // chem = 1.0 → tier 5
        assert_eq!(a.chemistry_label(), ChemistryLabel::Beloved);
        assert_eq!(BondLabel::Soulmate.as_key(), "soulmate");
        assert_eq!(ChemistryLabel::Beloved.as_key(), "beloved");
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
        after.trust = 0.6;
        after.intrigue = 0.6; // bond = 0.6 → tier 3 (close_friend)
        let d = diff_labels(&before, &after).unwrap();
        let bond = d.bond.unwrap();
        assert_eq!(bond.from, "acquaintance");
        assert_eq!(bond.to, "close_friend");
        assert!(d.chemistry.is_none());
    }

    // ─── grade pipeline (4.0) ───

    fn zeroed() -> Affinity {
        let mut a = fresh();
        a.warmth = 0.0;
        a.trust = 0.0;
        a.intrigue = 0.0;
        a.intimacy = 0.0;
        a.tension = 0.0;
        a
    }

    fn turn(
        a: &Affinity,
        grades: AxisGrades,
        rule: AffinityDeltas,
        pending: PendingDeltas,
        boost: f64,
        t: &AffinityTuning,
    ) -> GradeTurnOutcome {
        grade_turn(a, &grades, &rule, &pending, boost, t)
    }

    /// Positive raw is damped by the OWN line's tier; a counterpart line inside
    /// the grace zone charges no penalty, past it the quadratic ramp starts.
    #[test]
    fn positive_decays_by_own_tier_and_pays_counterpart_ramp() {
        let t = AffinityTuning::default();
        let mut a = zeroed();
        a.trust = 0.75;
        a.intrigue = 0.75; // bond = 0.75 → tier 4; chemistry = 0 → tier 1
        let o = turn(
            &a,
            AxisGrades {
                trust: 2,
                intimacy: 2,
                ..Default::default()
            },
            AffinityDeltas::default(),
            PendingDeltas::default(),
            1.0,
            &t,
        );
        // trust: own tier 4 → ×0.25 at the bond unit, counterpart chem 0 → no penalty
        assert!((o.committed.trust - 2.0 * t.grade_unit_bond * 0.25).abs() < 1e-9);
        // intimacy: own tier 1 → ×1.0 at the chem unit, counterpart bond 0.75 →
        // κ_chem·((0.40/0.65)²), charged at 2/4 because the judge graded this
        // axis a 2 (proportional to the applied grade, not a flat toll).
        let p = t.cross_penalty_ratio * t.grade_unit_chem * (0.40f64 / 0.65).powi(2) * 2.0 / 4.0;
        assert!((o.committed.intimacy - (2.0 * t.grade_unit_chem - p)).abs() < 1e-9);
        assert!((o.cross_penalty_assessed.intimacy - p).abs() < 1e-9);
        assert_eq!(o.cross_penalty_assessed.trust, 0.0, "counterpart below y₀");
    }

    /// **Changed with proportional charging.** Under the old flat toll this
    /// case asserted a g1 push netting −0.015 against a maxed counterpart while
    /// a g2 netted +0.02 — the sign flipped between grades, which is what made
    /// "the judge said up and the meter went down" possible.
    ///
    /// Charging in proportion makes `ρ = g · (D_k·u − κ·φ(y)/4)`: the bracket no
    /// longer depends on the grade, so **the outcome's sign always matches the
    /// verdict's** at this position. The grade sets the size, not the direction.
    #[test]
    fn penalty_scales_with_the_grade_so_the_outcome_cannot_flip_between_grades() {
        let mut a = zeroed();
        a.intimacy = 1.0;
        a.tension = 1.0; // chemistry = 1.0 — the counterpart; bond = 0 → own tier 1
        let t = AffinityTuning::default();
        let at = |g: i8| {
            turn(
                &a,
                AxisGrades {
                    trust: g,
                    ..Default::default()
                },
                AffinityDeltas::default(),
                PendingDeltas::default(),
                1.0,
                &t,
            )
        };
        // ρ = g·u_bond·(D₁ − ratio·φ(1)/4) = g·u_bond·(1 − 5/24): the bracket
        // has no g in it, so honest effort lands and g2 is exactly double g1.
        let unit_net = t.grade_unit_bond * (1.0 - t.cross_penalty_ratio / 4.0);
        assert!((at(1).committed.trust - unit_net).abs() < 1e-9);
        assert!((at(2).committed.trust - 2.0 * unit_net).abs() < 1e-9);
        assert!((at(4).committed.trust - 4.0 * unit_net).abs() < 1e-9);
        for g in 1..=4 {
            assert!(
                at(g).committed.trust > 0.0,
                "a positive verdict must not lower the score here (g{g})"
            );
        }
    }

    /// The double-high lock survives where it is meant to. At own tier 5 the
    /// bracket `D₅·u − κ·φ(y)/4` is genuinely negative once the counterpart
    /// passes ≈0.761, so every grade nets negative — uniformly, not just the
    /// cheap ones. "You cannot be both" still holds at the apex; it just stopped
    /// firing on ordinary mid-relationship turns.
    #[test]
    fn tier_five_against_a_high_counterpart_still_loses_at_every_grade() {
        let t = AffinityTuning::default();
        let mut a = zeroed();
        a.trust = 1.0;
        a.intrigue = 1.0; // bond = 1.0 — the counterpart
        a.intimacy = 1.0;
        a.tension = 1.0; // chemistry = 1.0 → own tier 5
                         // ρ = g·u_chem·(D₅ − ratio·φ(1)/4) = g·u_chem·(0.10 − 5/24) < 0.
        let unit_net = t.grade_unit_chem * (0.10 - t.cross_penalty_ratio / 4.0);
        assert!(unit_net < 0.0);
        for g in 1..=4 {
            let o = turn(
                &a,
                AxisGrades {
                    intimacy: g,
                    ..Default::default()
                },
                AffinityDeltas::default(),
                PendingDeltas::default(),
                1.0,
                &t,
            );
            assert!(
                (o.committed.intimacy - f64::from(g) * unit_net).abs() < 1e-9,
                "g{g}"
            );
            assert!(o.committed.intimacy < 0.0, "g{g}");
        }
    }

    /// Negative raw is never damped by tier, and the penalty still stacks on
    /// top — now at the grade's share rather than the full toll.
    #[test]
    fn negative_skips_decay_and_pays_extra() {
        let t = AffinityTuning::default();
        let mut a = zeroed();
        a.intimacy = 1.0;
        a.tension = 1.0; // chemistry = 1.0
        a.trust = 0.9;
        a.intrigue = 0.9; // bond = 0.9 → tier 5 (own tier must not soften the loss)
        let o = turn(
            &a,
            AxisGrades {
                trust: -2,
                ..Default::default()
            },
            AffinityDeltas::default(),
            PendingDeltas::default(),
            1.0,
            &t,
        );
        // −2·u_bond·1.5, minus κ_bond·φ(1.0)·2/4 on top — the penalty reads the
        // grade's MAGNITUDE, so a loss is taxed by how big it is.
        let p = t.cross_penalty_ratio * t.grade_unit_bond * 2.0 / 4.0;
        let expect = -2.0 * t.grade_unit_bond * t.neg_factor - p;
        assert!((o.committed.trust - expect).abs() < 1e-9);
        assert!((o.cross_penalty_assessed.trust - p).abs() < 1e-9);
    }

    /// P6: the pipeline charges events, not rent — an all-zero verdict moves
    /// nothing no matter how high the lines sit, and pending survives intact.
    #[test]
    fn zero_verdict_charges_nothing() {
        let mut a = zeroed();
        a.warmth = 1.0;
        a.trust = 1.0;
        a.intrigue = 1.0;
        a.intimacy = 1.0;
        a.tension = 1.0;
        let pending = PendingDeltas {
            trust: 0.02,
            ..Default::default()
        };
        let t = AffinityTuning {
            delta_threshold: 0.5,
            ..Default::default()
        };
        let o = turn(
            &a,
            AxisGrades::default(),
            AffinityDeltas::default(),
            pending,
            1.0,
            &t,
        );
        assert_eq!(o.committed.trust, 0.0);
        assert_eq!(o.committed.warmth, 0.0);
        assert!((o.pending.trust - 0.02).abs() < 1e-9);
    }

    /// Rule nudges ride the same decay but never trigger the penalty (only a
    /// judge-touched axis pays it).
    #[test]
    fn rule_only_delta_decays_but_pays_no_penalty() {
        let mut a = zeroed();
        a.trust = 0.2;
        a.intrigue = 0.3; // bond = 0.25 → own tier 2 (decay 0.70)
        a.intimacy = 1.0;
        a.tension = 1.0; // chem 1.0 — the counterpart, maximally expensive
        let o = turn(
            &a,
            AxisGrades::default(),
            AffinityDeltas {
                intrigue: 0.02,
                ..Default::default()
            },
            PendingDeltas::default(),
            1.0,
            &AffinityTuning::default(),
        );
        assert!((o.committed.intrigue - 0.7 * 0.02).abs() < 1e-9);
    }

    /// The decision-doc threshold example, verbatim: θ=0.5, real scores
    /// 0.1 / 0.2 / 0.3 per turn → committed deltas 0 / 0 / 0.6.
    #[test]
    fn threshold_accumulates_until_it_clears() {
        let a = zeroed(); // trust stays 0 (nothing commits), so tier stays 1 and D=1
        let t = AffinityTuning {
            delta_threshold: 0.5,
            ..Default::default()
        };
        let mut pending = PendingDeltas::default();
        let mut committed = Vec::new();
        for r in [0.1, 0.2, 0.3] {
            let o = turn(
                &a,
                AxisGrades::default(),
                AffinityDeltas {
                    trust: r,
                    ..Default::default()
                },
                pending,
                1.0,
                &t,
            );
            committed.push(o.committed.trust);
            pending = o.pending;
        }
        assert_eq!(committed[0], 0.0);
        assert_eq!(committed[1], 0.0);
        assert!((committed[2] - 0.6).abs() < 1e-9);
        assert!(pending.is_zero());
    }

    /// Signed accumulation: opposite-sign real scores cancel inside the gate,
    /// and a cancelled balance neither commits nor lingers.
    #[test]
    fn threshold_gate_cancels_opposite_signs() {
        let a = zeroed();
        let t = AffinityTuning {
            delta_threshold: 0.5,
            ..Default::default()
        };
        let o = turn(
            &a,
            AxisGrades::default(),
            AffinityDeltas {
                trust: -0.1,
                ..Default::default()
            },
            PendingDeltas {
                trust: 0.1,
                ..Default::default()
            },
            1.0,
            &t,
        );
        assert_eq!(o.committed.trust, 0.0);
        assert_eq!(o.pending.trust, 0.0, "+0.1 pending − 0.1 real cancels out");
    }

    /// Demo boost multiplies positive raw only; losses stay full price.
    #[test]
    fn demo_boost_is_positive_only() {
        let t = AffinityTuning::default();
        let a = zeroed();
        let o = turn(
            &a,
            AxisGrades {
                trust: 1,
                intimacy: -1,
                ..Default::default()
            },
            AffinityDeltas::default(),
            PendingDeltas::default(),
            1.4,
            &t,
        );
        assert!((o.committed.trust - 1.4 * t.grade_unit_bond).abs() < 1e-9);
        assert!((o.committed.intimacy - (-t.grade_unit_chem * t.neg_factor)).abs() < 1e-9);
    }

    /// The endpoints left the pipeline: a rule patience delta (there are none
    /// in production any more) is discarded, and raw/committed report 0.0 on
    /// both endpoint fields — deriving them is `refresh_endpoints`' job.
    #[test]
    fn endpoint_fields_are_inert_in_the_pipeline() {
        let a = zeroed();
        let o = turn(
            &a,
            AxisGrades::default(),
            AffinityDeltas {
                warmth: 0.5,
                patience: -0.02,
                ..Default::default()
            },
            PendingDeltas::default(),
            1.0,
            &AffinityTuning::default(),
        );
        assert_eq!(o.raw.warmth, 0.0);
        assert_eq!(o.committed.warmth, 0.0);
        assert_eq!(o.raw.patience, 0.0);
        assert_eq!(o.committed.patience, 0.0);
    }

    /// Out-of-range grades clamp instead of scaling: the judge cannot mint
    /// more than a ±4 verdict no matter what it emits.
    #[test]
    fn grades_clamp_to_plus_minus_four() {
        let t = AffinityTuning::default();
        let a = zeroed();
        let o = turn(
            &a,
            AxisGrades {
                trust: 9,
                intimacy: -9,
                ..Default::default()
            },
            AffinityDeltas::default(),
            PendingDeltas::default(),
            1.0,
            &t,
        );
        assert!((o.committed.trust - 4.0 * t.grade_unit_bond).abs() < 1e-9);
        assert!((o.committed.intimacy - (-4.0 * t.grade_unit_chem * t.neg_factor)).abs() < 1e-9);
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
        after.trust = 0.6;
        after.intrigue = 0.6; // bond = 0.6 → tier 3 (close_friend)
        after.intimacy = 0.5;
        after.tension = 0.5; // chem = 0.5 → tier 3 (crush)
        let d = diff_labels(&before, &after).unwrap();
        assert_eq!(d.bond.unwrap().to, "close_friend");
        assert_eq!(d.chemistry.unwrap().to, "crush");
    }

    // ─── Endpoint derivation (4.0) ──────────────────────────────────

    #[test]
    fn endpoint_boost_anchors() {
        // B(PIVOT)=1 exactly; B(1)=B_MAX; B(0)=1−0.35·10/13.
        assert!((endpoint_boost(0.35) - 1.0).abs() < 1e-12);
        assert!((endpoint_boost(1.0) - 1.5).abs() < 1e-12);
        assert!((endpoint_boost(0.0) - (1.0 - 0.35 * 10.0 / 13.0)).abs() < 1e-12);
    }

    #[test]
    fn endpoint_value_exact_ceiling_and_ranges() {
        // Level 3 × counterpart 1 × decay 1 = exactly 1.0 (no clamp doing work).
        assert!((endpoint_value(3, 1.0, 1.0, 0.2) - 1.0).abs() < 1e-9);
        // Level ranges at decay=1: L2 ∈ [0.2436, 0.5], L3 ∈ [0.4872, 1.0].
        assert!(
            (endpoint_value(2, 0.0, 1.0, 0.2) - (1.0 / 3.0) * (1.0 - 0.35 * 10.0 / 13.0)).abs()
                < 1e-9
        );
        assert!((endpoint_value(2, 1.0, 1.0, 0.2) - 0.5).abs() < 1e-9);
        assert!(
            (endpoint_value(3, 0.0, 1.0, 0.2) - (2.0 / 3.0) * (1.0 - 0.35 * 10.0 / 13.0)).abs()
                < 1e-9
        );
    }

    #[test]
    fn endpoint_floor_only_acts_on_level_one() {
        // Level 1: value = φ·x (base 0, floor carries it).
        assert!((endpoint_value(1, 0.9, 1.0, 0.2) - 0.18).abs() < 1e-9);
        assert!((endpoint_value(1, 0.0, 1.0, 0.2) - 0.0).abs() < 1e-9);
        // Level 2 at ANY counterpart beats the floor: φ·x ≤ 0.2 < 0.2436 ≤ base·B.
        for x in [0.0, 0.35, 0.7, 1.0] {
            let with_floor = endpoint_value(2, x, 1.0, 0.2);
            let without = endpoint_value(2, x, 1.0, 0.0);
            assert!(
                (with_floor - without).abs() < 1e-12,
                "floor must not touch level 2 at x={x}"
            );
        }
    }

    #[test]
    fn endpoint_level_out_of_range_clamps() {
        // Defensive: a stored 0 or 7 behaves as the nearest valid level.
        assert_eq!(
            endpoint_value(0, 0.5, 1.0, 0.2),
            endpoint_value(1, 0.5, 1.0, 0.2)
        );
        assert_eq!(
            endpoint_value(7, 0.5, 1.0, 0.2),
            endpoint_value(3, 0.5, 1.0, 0.2)
        );
    }

    #[test]
    fn composites_are_two_axis_means() {
        let mut a = fresh();
        a.warmth = 1.0; // must NOT leak into either line any more
        a.trust = 0.4;
        a.intrigue = 0.6;
        a.intimacy = 0.3;
        a.tension = 0.2;
        assert!((a.bond_score() - 0.5).abs() < 1e-9);
        assert!((a.chemistry_score() - 0.25).abs() < 1e-9);
    }

    #[test]
    fn refresh_endpoints_tsundere_quadrant() {
        // Low bond × high chem ⇒ low patience × high warmth, from one verdict pair.
        let t = AffinityTuning::default();
        let mut a = fresh();
        a.trust = 0.1;
        a.intrigue = 0.1; // bond = 0.1
        a.intimacy = 0.9;
        a.tension = 0.9; // chem = 0.9
        a.warmth_grade = 3;
        a.patience_grade = 3;
        a.updated_at = Utc::now(); // decay ≈ 1
        a.refresh_endpoints(&t);
        // warmth = 2/3 · B(0.9) = 2/3 · 1.4231 ≈ 0.949
        assert!((a.warmth - (2.0 / 3.0) * (1.0 + (10.0 / 13.0) * 0.55)).abs() < 1e-6);
        // patience = 2/3 · B(0.1) = 2/3 · 0.8077 ≈ 0.538
        assert!((a.patience - (2.0 / 3.0) * (1.0 - (10.0 / 13.0) * 0.25)).abs() < 1e-6);
        assert!(a.warmth > a.patience, "tsundere: warm but impatient");
    }

    #[test]
    fn time_decay_no_longer_drifts_patience_up() {
        let mut a = fresh();
        a.patience = 0.4;
        a.updated_at = Utc::now() - chrono::Duration::days(10);
        a.apply_time_decay();
        assert!(
            (a.patience - 0.4).abs() < 1e-12,
            "patience drift retired; endpoint decay owns absence now"
        );
    }

    #[test]
    fn per_line_units_and_ratio_kappa() {
        // A +2 trust grade at tier 1, no counterpart pressure, no gate:
        // committed = 2 · u_bond · decay(tier1)=1.0. Same grade on intimacy → 2·u_chem.
        let t = AffinityTuning::default();
        let mut a = fresh();
        a.warmth = 0.0;
        a.trust = 0.0;
        a.intrigue = 0.0;
        a.intimacy = 0.0;
        a.tension = 0.0;
        let g = AxisGrades {
            trust: 2,
            intrigue: 0,
            intimacy: 2,
            tension: 0,
        };
        let out = grade_turn(
            &a,
            &g,
            &AffinityDeltas::default(),
            &PendingDeltas::default(),
            1.0,
            &t,
        );
        assert!((out.committed.trust - 2.0 * t.grade_unit_bond).abs() < 1e-9);
        assert!((out.committed.intimacy - 2.0 * t.grade_unit_chem).abs() < 1e-9);
        assert_eq!(out.committed.warmth, 0.0);
        assert_eq!(out.committed.patience, 0.0);
    }

    #[test]
    fn break_even_position_is_unit_invariant() {
        // κ = ratio·u ⇒ φ(y*) = 4·D_k/ratio — the double-high wall cannot move
        // with the unit. Verify by scanning for the sign flip of a +1 intimacy
        // grade at chem tier 5, under two very different chem units.
        let mut t1 = AffinityTuning::default();
        let mut t2 = AffinityTuning::default();
        t1.grade_unit_chem = 0.0266;
        t2.grade_unit_chem = 0.10;
        let y_star = |t: &AffinityTuning| {
            let mut a = fresh();
            a.warmth = 0.0;
            a.intimacy = 1.0;
            a.tension = 0.9; // own line (chem) tier 5
            (0..=1000).map(|i| f64::from(i) / 1000.0).find(|&y| {
                let mut b = a.clone();
                b.trust = y;
                b.intrigue = y; // counterpart bond = y
                let g = AxisGrades {
                    trust: 0,
                    intrigue: 0,
                    intimacy: 1,
                    tension: 0,
                };
                let out = grade_turn(
                    &b,
                    &g,
                    &AffinityDeltas::default(),
                    &PendingDeltas::default(),
                    1.0,
                    t,
                );
                out.committed.intimacy < 0.0
            })
        };
        let y1 = y_star(&t1);
        assert!(y1.is_some(), "a break-even must exist at tier 5");
        assert_eq!(
            y1,
            y_star(&t2),
            "κ tied to unit ⇒ wall does not move with the unit"
        );
    }

    #[test]
    fn endpoint_time_decay_linear_with_floor() {
        assert!((endpoint_time_decay(0.0, 0.02, 0.5) - 1.0).abs() < 1e-12);
        assert!((endpoint_time_decay(7.0, 0.02, 0.5) - 0.86).abs() < 1e-12);
        assert!((endpoint_time_decay(25.0, 0.02, 0.5) - 0.5).abs() < 1e-12);
        assert!((endpoint_time_decay(60.0, 0.02, 0.5) - 0.5).abs() < 1e-12);
        assert!((endpoint_time_decay(-3.0, 0.02, 0.5) - 1.0).abs() < 1e-12); // clock skew → no decay
    }
}
