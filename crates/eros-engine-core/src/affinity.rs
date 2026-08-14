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
    /// Apply committed deltas directly, clamping each axis to its range.
    /// Damping lives in `grade_turn`'s tier decay, so a committed delta means
    /// exactly what it says.
    pub fn apply_deltas(&mut self, d: &AffinityDeltas) {
        self.warmth = clamp(self.warmth + d.warmth, -1.0, 1.0);
        self.trust = clamp(self.trust + d.trust, 0.0, 1.0);
        self.intrigue = clamp(self.intrigue + d.intrigue, 0.0, 1.0);
        self.intimacy = clamp(self.intimacy + d.intimacy, 0.0, 1.0);
        self.patience = clamp(self.patience + d.patience, 0.0, 1.0);
        self.tension = clamp(self.tension + d.tension, 0.0, 1.0);
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

// ─── Affinity write-side pipeline (scope → grades → raw → decay → penalty → gate) ───
//
// The judge reports per-axis *grades* (0..=4 magnitude + direction, folded to a
// signed integer at parse time); the engine owns every number. 3.1 first steers
// the grades by the request's resolved `AffinityScope` (see `ScopeMode`), then
// 3.0 runs unchanged: a grade converts to a raw delta, positive raw is damped by
// the line's tier, line-exclusive axes pay a cross-line penalty while the
// counterpart line is high, and the resulting real delta passes a threshold
// accumulator before it commits.
// Design specs: docs/superpowers/specs/2026-08-13-affinity-30-grade-pipeline-design.md
//               docs/superpowers/specs/2026-08-14-affinity-31-scope-steering-design.md
//               docs/superpowers/specs/2026-08-14-cross-penalty-by-grade-design.md

/// Tuning knobs for the 3.0 pipeline, env-driven server-side. Defaults
/// set the single-turn envelope: grade ±4 lands +0.20 / −0.30 per axis.
#[derive(Debug, Clone, PartialEq)]
pub struct AffinityTuning {
    /// Raw score per grade step (`AFFINITY_GRADE_UNIT`).
    pub grade_unit: f64,
    /// Extra multiplier on negative raw scores (`AFFINITY_NEG_FACTOR`) —
    /// keeps 2.0's "slow up, fast down" asymmetry.
    pub neg_factor: f64,
    /// Positive-delta damping per tier 1..=5 (`AFFINITY_TIER_DECAY`).
    pub tier_decay: [f64; 5],
    /// Cross-line penalty ceiling κ (`AFFINITY_CROSS_PENALTY`).
    pub cross_penalty: f64,
    /// Counterpart line score where the penalty starts (`AFFINITY_CROSS_PENALTY_START`).
    pub cross_penalty_start: f64,
    /// Commit threshold θ (`AFFINITY_DELTA_THRESHOLD`); 0 commits every turn.
    pub delta_threshold: f64,
    /// Multiplier on the judge's positive raw component for demo sessions
    /// (`AFFINITY_DEMO_BOOST`); rule nudges are unaffected.
    pub demo_boost: f64,
    /// Constant multiplier on positive raw for the bond-exclusive axes when the
    /// resolved scope names the bond half only (`AFFINITY_SCOPE_BOND_BOOST`).
    /// 1.0 disables the boost.
    pub scope_bond_boost: f64,
    /// Grade ladder applied to the chemistry-exclusive axes when the resolved
    /// scope carries any chemistry axis (`AFFINITY_SCOPE_CHEM_LADDER`), indexed
    /// by the positive grade 0..=4. The identity `[0,1,2,3,4]` disables it.
    pub scope_chem_ladder: [i8; 5],
}

impl Default for AffinityTuning {
    fn default() -> Self {
        Self {
            grade_unit: 0.05,
            neg_factor: 1.5,
            tier_decay: [1.0, 0.70, 0.45, 0.25, 0.10],
            cross_penalty: 0.05,
            cross_penalty_start: 0.35,
            delta_threshold: 0.0,
            demo_boost: 1.4,
            scope_bond_boost: 1.5,
            // g1 filtered, g2 halved, milestones untouched — see `ScopeMode`.
            scope_chem_ladder: [0, 0, 1, 3, 4],
        }
    }
}

/// How a resolved `AffinityScope` steers the write side (affinity 3.1).
///
/// The scope field is *borrowed* here for the bond/chemistry mental model its
/// two named values carry — not for the axis triads behind them. Those triads
/// (`scope.rs`, 1.0-era) partition all six axes for prompt injection and
/// `length_score`; the score composites (`bond_score`/`chemistry_score`, 2.0+)
/// overlap on warmth and drop patience. Two different algebras on purpose;
/// neither is a mis-grouping of the other.
///
/// Total over the six bools, so an axes array steers as predictably as a named
/// value: an empty scope is neutral, anything touching the chemistry half
/// suppresses, everything else boosts bond.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeMode {
    /// 3.0 behaviour verbatim.
    Neutral,
    /// Bond-exclusive axes earn `scope_bond_boost` on positive raw.
    BoostBond,
    /// Chemistry-exclusive positive grades drop through `scope_chem_ladder`.
    /// A grade laddered to 0 reads as "the judge did not touch this axis", so
    /// it charges no cross penalty either — the reason the ladder introduces no
    /// break-even case that 3.0 did not already have.
    SuppressChemistry,
}

impl ScopeMode {
    pub fn of(scope: &crate::scope::AffinityScope) -> Self {
        if scope.is_empty() {
            Self::Neutral
        } else if scope.any_chemistry_axis() {
            Self::SuppressChemistry
        } else {
            Self::BoostBond
        }
    }

    pub fn as_key(self) -> &'static str {
        match self {
            ScopeMode::Neutral => "neutral",
            ScopeMode::BoostBond => "boost_bond",
            ScopeMode::SuppressChemistry => "suppress_chemistry",
        }
    }
}

/// Signed judge grades, one per evaluated axis, −4..=4 (out-of-range input is
/// clamped). 0 = nothing happened, the overwhelmingly common verdict.
/// `patience` is absent by construction: it stays on its absolute channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AxisGrades {
    pub warmth: i8,
    pub trust: i8,
    pub intrigue: i8,
    pub intimacy: i8,
    pub tension: i8,
}

/// Per-axis balance the threshold gate is still holding back. Persisted as
/// JSONB on the affinity row; absent column reads as all-zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct PendingDeltas {
    #[serde(default)]
    pub warmth: f64,
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
        self.warmth == 0.0
            && self.trust == 0.0
            && self.intrigue == 0.0
            && self.intimacy == 0.0
            && self.tension == 0.0
    }
}

/// One turn through the 3.0 pipeline.
/// `raw` = grade conversion (boost applied) + rule deltas, pre-decay — what the
/// event row records as `deltas`. `committed` = what actually applies to the
/// axes this turn (zero while the gate holds). `pending` = the gate's new
/// balance. `patience` is a rule axis: its rule delta passes through both
/// `raw` and `committed` untouched (no decay, penalty or gating), so the
/// judge-failure fallback still lands 1:1.
#[derive(Debug, Clone)]
pub struct GradeTurnOutcome {
    pub raw: AffinityDeltas,
    pub committed: AffinityDeltas,
    pub pending: PendingDeltas,
    /// The grades actually converted, after scope steering. Equal to the
    /// judge's grades under `ScopeMode::Neutral`. Audited alongside the
    /// judge's originals so a committed 0 stays attributable: the judge said
    /// nothing, or the ladder filtered it.
    pub effective_grades: AxisGrades,
    pub scope_mode: ScopeMode,
    /// Cross-line penalty *assessed* this turn, per line-exclusive axis.
    ///
    /// Assessed, not applied. It is subtracted inside `ρ` before the threshold
    /// gate, so on a turn the gate buffers, nothing has reached the axis yet —
    /// the amount rides along in `pending` rather than being lost (the gate
    /// re-times commits, it never rescales them), and the caller's axis clamp
    /// can swallow part of a commit besides. Now that the penalty scales with
    /// the applied grade this is no longer derivable from the grades alone, so
    /// it is recorded rather than reconstructed. warmth is penalty-exempt and
    /// therefore absent.
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

impl Default for GradeTurnOutcome {
    fn default() -> Self {
        Self {
            raw: AffinityDeltas::default(),
            committed: AffinityDeltas::default(),
            pending: PendingDeltas::default(),
            effective_grades: AxisGrades::default(),
            scope_mode: ScopeMode::Neutral,
            cross_penalty_assessed: CrossPenaltyAssessed::default(),
        }
    }
}

/// Run one judge verdict through scope steering, conversion, tier decay,
/// cross-line penalty and the threshold gate. Pure: reads the *pre-turn*
/// affinity snapshot (all axes use the same tier lookup, so ordering inside a
/// turn cannot matter) and returns what to apply; the caller owns clamping and
/// persistence.
///
/// `scope` is the request's resolved injection scope, reused to steer the
/// multipliers — see [`ScopeMode`]. An empty scope is neutral, i.e. 3.0
/// verbatim.
pub fn grade_turn(
    a: &Affinity,
    grades: &AxisGrades,
    rule: &AffinityDeltas,
    pending: &PendingDeltas,
    boost: f64,
    scope: &crate::scope::AffinityScope,
    t: &AffinityTuning,
) -> GradeTurnOutcome {
    // Scope steering (3.1). warmth is exempt from both directions: it feeds
    // BOTH composites, so scaling it would leak the correction onto the other
    // line — the same reason the cross penalty exempts it.
    let mode = ScopeMode::of(scope);
    let bond_mult = match mode {
        ScopeMode::BoostBond => t.scope_bond_boost,
        _ => 1.0,
    };
    let ladder = |g: i8| -> i8 {
        match mode {
            ScopeMode::SuppressChemistry if g > 0 => t.scope_chem_ladder[g.min(4) as usize],
            _ => g,
        }
    };
    let eff = AxisGrades {
        warmth: grades.warmth,
        trust: grades.trust,
        intrigue: grades.intrigue,
        intimacy: ladder(grades.intimacy),
        tension: ladder(grades.tension),
    };

    // Pre-turn snapshot: every axis reads the same tiers and counterparts.
    let bond = a.bond_score();
    let chem = a.chemistry_score();
    let bond_tier = tier_index(bond) as usize;
    let chem_tier = tier_index(chem) as usize;

    let decay = |tier: usize| t.tier_decay[tier - 1];
    let penalty = |counterpart: f64| {
        let start = t.cross_penalty_start;
        let ramp = ((counterpart - start).max(0.0) / (1.0 - start)).powi(2);
        t.cross_penalty * ramp
    };

    // grade → raw: positive grades earn unit × boost × scope multiplier,
    // negative grades cost unit × neg_factor (never scaled by scope — the
    // correction raises the bar, it does not soften damage). Rule nudges join
    // pre-decay.
    let raw = |g: i8, rule_d: f64, mult: f64| {
        let g = f64::from(g.clamp(-4, 4));
        let judge = if g >= 0.0 {
            g * t.grade_unit * boost * mult
        } else {
            g * t.grade_unit * t.neg_factor
        };
        judge + rule_d
    };

    // ρ = D·max(r,0) + min(r,0) − P: positive part damped, negative part full
    // price, and the cross penalty assessed in PROPORTION to the grade actually
    // applied — P = κ·φ(y)·(|g|/4).
    //
    // It used to be binary: any non-zero grade paid the full κ·φ(y). That made
    // the tax independent of how much the axis moved, so at a high counterpart
    // score a small honest push was charged the same as a milestone. The
    // proportional form keeps the zero case exactly as it was (a grade of 0,
    // including one the 3.1 ladder filtered, charges nothing) and turns the old
    // on/off step into the endpoint of a continuous rule.
    //
    // What it buys, stated exactly — ignoring rule nudges, ρ factorises:
    //     g > 0:  ρ = g · (D_k·u·mult − κ·φ(y)/4)
    //     g < 0:  ρ = g · (u·λ⁻ + κ·φ(y)/4)          (no decay on the negative part)
    // Neither bracket contains g, so **the outcome cannot change sign between
    // grades at a fixed position** — the flat toll's failure mode, where a small
    // honest push lost ground while a bigger one gained it. The negative bracket
    // is always positive, so a negative verdict always lowers the axis.
    //
    // It does NOT make every positive verdict a gain: whether the positive
    // bracket is positive is a property of the POSITION, break-even at
    // φ(y*) = 4·D_k·u·mult/κ. Past it every grade nets negative, uniformly —
    // see `tier_five_against_a_high_counterpart_still_loses_at_every_grade`.
    //
    // Rule nudges sit outside this: they join `r` before decay but are not part
    // of `g`, so a large enough opposing nudge could in principle invert the
    // sign. None can today — the only rule deltas reaching a graded axis are
    // intrigue +0.02 and tension +0.03, both positive (the negative ones all
    // land on patience, which bypasses the pipeline).
    //
    // Magnitude, not sign: a negative grade already moves the axis away from the
    // double-high position the penalty exists to discourage, so it pays in
    // proportion too rather than at the old flat rate.
    let real = |r: f64, own_tier: usize, counterpart: Option<f64>, g: i8| {
        let p = match counterpart {
            Some(y) => penalty(y) * f64::from(g.saturating_abs().min(4)) / 4.0,
            None => 0.0,
        };
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

    // `g` here is the POST-ladder grade, so an axis the ladder zeroed reads as
    // untouched and pays no cross penalty.
    let axis =
        |g: i8, rule_d: f64, own_tier: usize, counterpart: Option<f64>, pend: f64, mult: f64| {
            let r = raw(g, rule_d, mult);
            let (rho, charged) = real(r, own_tier, counterpart, g);
            let (committed, pend) = gate(rho, pend);
            (r, committed, pend, charged)
        };

    let max_tier = bond_tier.max(chem_tier);
    let (w_raw, w_com, w_pend, _w_pen) =
        axis(eff.warmth, rule.warmth, max_tier, None, pending.warmth, 1.0);
    let (t_raw, t_com, t_pend, t_pen) = axis(
        eff.trust,
        rule.trust,
        bond_tier,
        Some(chem),
        pending.trust,
        bond_mult,
    );
    let (ig_raw, ig_com, ig_pend, ig_pen) = axis(
        eff.intrigue,
        rule.intrigue,
        bond_tier,
        Some(chem),
        pending.intrigue,
        bond_mult,
    );
    let (im_raw, im_com, im_pend, im_pen) = axis(
        eff.intimacy,
        rule.intimacy,
        chem_tier,
        Some(bond),
        pending.intimacy,
        1.0,
    );
    let (tn_raw, tn_com, tn_pend, tn_pen) = axis(
        eff.tension,
        rule.tension,
        chem_tier,
        Some(bond),
        pending.tension,
        1.0,
    );

    GradeTurnOutcome {
        raw: AffinityDeltas {
            warmth: w_raw,
            trust: t_raw,
            intrigue: ig_raw,
            intimacy: im_raw,
            tension: tn_raw,
            patience: rule.patience,
        },
        committed: AffinityDeltas {
            warmth: w_com,
            trust: t_com,
            intrigue: ig_com,
            intimacy: im_com,
            tension: tn_com,
            patience: rule.patience,
        },
        pending: PendingDeltas {
            warmth: w_pend,
            trust: t_pend,
            intrigue: ig_pend,
            intimacy: im_pend,
            tension: tn_pend,
        },
        effective_grades: eff,
        scope_mode: mode,
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
    fn warmth_can_go_negative_others_cannot() {
        let mut a = fresh();
        a.apply_deltas(&AffinityDeltas {
            warmth: -2.0, // -1.0 floor
            trust: 0.0,
            intrigue: 0.0,
            intimacy: 0.0,
            patience: 0.0,
            tension: 0.0,
        });
        assert_eq!(a.warmth, -1.0);
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
        a.intrigue = 0.6; // bond = 0.4 → tier 3
        assert_eq!(a.bond_label(), BondLabel::CloseFriend);
        a.warmth = 0.0;
        a.intimacy = 0.0;
        a.tension = 0.0;
        assert_eq!(a.chemistry_label(), ChemistryLabel::Spark); // chem 0
        a.intimacy = 1.0;
        a.tension = 1.0; // chem = 0.667 → tier 4
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
        after.trust = 0.9;
        after.intrigue = 0.9; // bond = 0.6 → tier 3 (close_friend)
        let d = diff_labels(&before, &after).unwrap();
        let bond = d.bond.unwrap();
        assert_eq!(bond.from, "acquaintance");
        assert_eq!(bond.to, "close_friend");
        assert!(d.chemistry.is_none());
    }

    // ─── affinity 3.0 pipeline ───

    fn zeroed() -> Affinity {
        let mut a = fresh();
        a.warmth = 0.0;
        a.trust = 0.0;
        a.intrigue = 0.0;
        a.intimacy = 0.0;
        a.tension = 0.0;
        a
    }

    /// 3.0-era helper: no scope steering (`none()` → `ScopeMode::Neutral`), so
    /// every pre-3.1 expectation below still asserts the unsteered pipeline.
    fn turn(
        a: &Affinity,
        grades: AxisGrades,
        rule: AffinityDeltas,
        pending: PendingDeltas,
        boost: f64,
        t: &AffinityTuning,
    ) -> GradeTurnOutcome {
        grade_turn(
            a,
            &grades,
            &rule,
            &pending,
            boost,
            &crate::scope::AffinityScope::none(),
            t,
        )
    }

    fn scoped(
        a: &Affinity,
        grades: AxisGrades,
        scope: crate::scope::AffinityScope,
    ) -> GradeTurnOutcome {
        grade_turn(
            a,
            &grades,
            &AffinityDeltas::default(),
            &PendingDeltas::default(),
            1.0,
            &scope,
            &AffinityTuning::default(),
        )
    }

    /// P3 envelope continuity: with defaults, grade +4 lands +0.20 and grade −4
    /// lands −0.30 — the 2.0 effective caps, bit for bit. patience never moves.
    #[test]
    fn grade_envelope_matches_2_0_effective_caps() {
        let a = zeroed();
        let o = turn(
            &a,
            AxisGrades {
                warmth: 4,
                trust: -4,
                ..Default::default()
            },
            AffinityDeltas::default(),
            PendingDeltas::default(),
            1.0,
            &AffinityTuning::default(),
        );
        assert!((o.raw.warmth - 0.2).abs() < 1e-9);
        assert!((o.committed.warmth - 0.2).abs() < 1e-9);
        assert!((o.committed.trust - (-0.3)).abs() < 1e-9);
        assert_eq!(o.raw.patience, 0.0);
        assert_eq!(o.committed.patience, 0.0);
    }

    /// Positive raw is damped by the OWN line's tier; a counterpart line inside
    /// the grace zone charges no penalty, past it the quadratic ramp starts.
    #[test]
    fn positive_decays_by_own_tier_and_pays_counterpart_ramp() {
        let mut a = zeroed();
        a.trust = 0.75;
        a.intrigue = 0.75; // bond = 0.5 → tier 3; chemistry = 0 → tier 1
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
            &AffinityTuning::default(),
        );
        // trust: own tier 3 → 0.45 × 0.10, counterpart chem 0 → no penalty
        assert!((o.committed.trust - 0.045).abs() < 1e-9);
        // intimacy: own tier 1 → ×1.0, counterpart bond 0.5 → κ·((0.15/0.65)²),
        // charged at 2/4 because the judge graded this axis a 2 (the penalty is
        // proportional to the applied grade, not a flat toll on any non-zero).
        let p = 0.05 * (0.15f64 / 0.65).powi(2) * 2.0 / 4.0;
        assert!((o.committed.intimacy - (0.10 - p)).abs() < 1e-9);
        assert!((o.cross_penalty_assessed.intimacy - p).abs() < 1e-9);
        assert_eq!(o.cross_penalty_assessed.trust, 0.0, "counterpart below y₀");
    }

    /// warmth feeds both lines, so its damping reads the FURTHER line's tier
    /// (same max rule as the intimacy rung) and it never pays the penalty.
    #[test]
    fn warmth_decays_by_max_tier_and_is_penalty_exempt() {
        let mut a = zeroed();
        a.warmth = 0.85;
        a.intimacy = 1.0;
        a.tension = 1.0; // chemistry = 0.95 → tier 5; bond ≈ 0.283 → tier 2
        let o = turn(
            &a,
            AxisGrades {
                warmth: 2,
                ..Default::default()
            },
            AffinityDeltas::default(),
            PendingDeltas::default(),
            1.0,
            &AffinityTuning::default(),
        );
        assert!((o.committed.warmth - 0.1 * 0.10).abs() < 1e-9);
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
        a.warmth = 1.0;
        a.intimacy = 1.0;
        a.tension = 1.0; // chemistry = 1.0; bond = 1/3 → tier 2
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
                &AffinityTuning::default(),
            )
        };
        // g1: 0.70 × 0.05 − 0.05 × 1 × ¼ = +0.0225 — honest effort now lands.
        assert!((at(1).committed.trust - 0.0225).abs() < 1e-9);
        // g2: exactly double. Proportional in, proportional out.
        assert!((at(2).committed.trust - 0.045).abs() < 1e-9);
        assert!((at(4).committed.trust - 0.09).abs() < 1e-9);
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
        let mut a = zeroed();
        a.warmth = 1.0;
        a.trust = 1.0;
        a.intrigue = 1.0; // bond = 1.0 — the counterpart
        a.intimacy = 1.0;
        a.tension = 1.0; // chemistry = 1.0 → own tier 5
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
                &AffinityTuning::default(),
            );
            // 0.10 × 0.05·g − 0.05 × 1 × g/4 = g × (0.005 − 0.0125) < 0
            assert!(
                (o.committed.intimacy - f64::from(g) * (0.005 - 0.0125)).abs() < 1e-9,
                "g{g}"
            );
            assert!(o.committed.intimacy < 0.0, "g{g}");
        }
    }

    /// Negative raw is never damped by tier, and the penalty still stacks on
    /// top — now at the grade's share rather than the full toll.
    #[test]
    fn negative_skips_decay_and_pays_extra() {
        let mut a = zeroed();
        a.warmth = 1.0;
        a.intimacy = 1.0;
        a.tension = 1.0; // chemistry = 1.0
        a.trust = 0.9;
        a.intrigue = 0.9; // bond ≈ 0.933 → tier 5 (own tier must not soften the loss)
        let o = turn(
            &a,
            AxisGrades {
                trust: -2,
                ..Default::default()
            },
            AffinityDeltas::default(),
            PendingDeltas::default(),
            1.0,
            &AffinityTuning::default(),
        );
        // −2 × 0.05 × 1.5 = −0.15, minus κ·φ(1.0) × 2/4 = 0.025 → −0.175.
        // (Was −0.20 under the flat toll. The penalty reads the grade's
        // MAGNITUDE, so a loss is taxed by how big it is, not a flat rate.)
        assert!((o.committed.trust - (-0.175)).abs() < 1e-9);
        assert!((o.cross_penalty_assessed.trust - 0.025).abs() < 1e-9);
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
        a.warmth = 1.0;
        a.intimacy = 1.0;
        a.tension = 1.0; // chem 1.0; bond 1/3 → tier 2
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
        let a = zeroed();
        let o = turn(
            &a,
            AxisGrades {
                warmth: 1,
                trust: -1,
                ..Default::default()
            },
            AffinityDeltas::default(),
            PendingDeltas::default(),
            1.4,
            &AffinityTuning::default(),
        );
        assert!((o.committed.warmth - 0.07).abs() < 1e-9);
        assert!((o.committed.trust - (-0.075)).abs() < 1e-9);
    }

    /// patience is a rule axis: its rule delta passes through the pipeline
    /// untouched — no decay, no penalty, no threshold gate — so the fallback
    /// path (judge eval failed, rule nudges only) still lands 1:1.
    #[test]
    fn rule_patience_passes_through_ungated() {
        let a = zeroed();
        let t = AffinityTuning {
            delta_threshold: 0.5, // must NOT buffer patience
            ..Default::default()
        };
        let o = turn(
            &a,
            AxisGrades::default(),
            AffinityDeltas {
                patience: -0.02,
                ..Default::default()
            },
            PendingDeltas::default(),
            1.0,
            &t,
        );
        assert!((o.committed.patience - (-0.02)).abs() < 1e-9);
        assert!((o.raw.patience - (-0.02)).abs() < 1e-9);
    }

    /// Out-of-range grades clamp instead of scaling: the judge cannot mint
    /// more than a ±4 verdict no matter what it emits.
    #[test]
    fn grades_clamp_to_plus_minus_four() {
        let a = zeroed();
        let o = turn(
            &a,
            AxisGrades {
                warmth: 9,
                trust: -9,
                ..Default::default()
            },
            AffinityDeltas::default(),
            PendingDeltas::default(),
            1.0,
            &AffinityTuning::default(),
        );
        assert!((o.committed.warmth - 0.2).abs() < 1e-9);
        assert!((o.committed.trust - (-0.3)).abs() < 1e-9);
    }

    // ─── affinity 3.1 scope steering ───

    /// The mode is a TOTAL function over the six bools, so an axes array steers
    /// as predictably as a named value. Anything touching the chemistry half
    /// suppresses; only a bond-half-exclusive scope boosts.
    #[test]
    fn scope_mode_is_total_over_the_six_bools() {
        use crate::scope::{AffinityAxis, AffinityScope};
        let of = |s: AffinityScope| ScopeMode::of(&s);
        assert_eq!(of(AffinityScope::none()), ScopeMode::Neutral);
        assert_eq!(of(AffinityScope::bond()), ScopeMode::BoostBond);
        assert_eq!(of(AffinityScope::chemistry()), ScopeMode::SuppressChemistry);
        assert_eq!(of(AffinityScope::full()), ScopeMode::SuppressChemistry);
        // A mixed array lands on the suppress side — the default disposition.
        assert_eq!(
            of(AffinityScope::from_axes(&[
                AffinityAxis::Warmth,
                AffinityAxis::Trust
            ])),
            ScopeMode::SuppressChemistry
        );
        // A lone bond-half axis still boosts.
        assert_eq!(
            of(AffinityScope::from_axes(&[AffinityAxis::Warmth])),
            ScopeMode::BoostBond
        );
        assert_eq!(ScopeMode::BoostBond.as_key(), "boost_bond");
    }

    /// `bond` scope: the bond-EXCLUSIVE axes take the constant. warmth feeds
    /// both composites and is exempt, or the boost would leak onto chemistry.
    #[test]
    fn bond_scope_boosts_exclusive_axes_only_and_spares_warmth() {
        let a = zeroed();
        let o = scoped(
            &a,
            AxisGrades {
                warmth: 2,
                trust: 2,
                intrigue: 2,
                ..Default::default()
            },
            crate::scope::AffinityScope::bond(),
        );
        assert_eq!(o.scope_mode, ScopeMode::BoostBond);
        // 2 × 0.05 × 1.5 = 0.15 on the exclusive axes …
        assert!((o.committed.trust - 0.15).abs() < 1e-9);
        assert!((o.committed.intrigue - 0.15).abs() < 1e-9);
        // … and a plain 2 × 0.05 on shared warmth.
        assert!((o.committed.warmth - 0.10).abs() < 1e-9);
    }

    /// The boost raises the bar, it does not soften damage: negative raw keeps
    /// paying `neg_factor` and nothing else. (grade −1 rather than −2 so a
    /// wrongly-applied 1.5 boost cannot alias the 1.5 neg_factor.)
    #[test]
    fn bond_scope_never_scales_losses() {
        let a = zeroed();
        let o = scoped(
            &a,
            AxisGrades {
                trust: -1,
                ..Default::default()
            },
            crate::scope::AffinityScope::bond(),
        );
        // −1 × 0.05 × 1.5 = −0.075; boosted would be −0.1125.
        assert!((o.committed.trust - (-0.075)).abs() < 1e-9);
    }

    /// The ladder: g1 filtered, g2 halved, milestones untouched. Chemistry
    /// axes only — trust/intrigue ride through at 3.0 rates under this mode.
    #[test]
    fn chemistry_scope_ladders_only_the_low_grades() {
        let a = zeroed();
        let grade = |g: i8| {
            scoped(
                &a,
                AxisGrades {
                    intimacy: g,
                    ..Default::default()
                },
                crate::scope::AffinityScope::chemistry(),
            )
            .committed
            .intimacy
        };
        assert_eq!(grade(1), 0.0, "g1 is filtered out entirely");
        assert!((grade(2) - 0.05).abs() < 1e-9, "g2 lands as a g1");
        assert!((grade(3) - 0.15).abs() < 1e-9, "g3 is untouched");
        assert!((grade(4) - 0.20).abs() < 1e-9, "g4 is untouched");
        // Losses are never laddered.
        assert!((grade(-2) - (-0.15)).abs() < 1e-9);
    }

    /// The property the whole design rests on: a grade the ladder zeroes reads
    /// as "the judge did not touch this axis", so it charges NO cross penalty.
    /// Without that, halving small pushes while leaving the penalty at full
    /// price would manufacture a friend-zone wall 3.0 never had.
    #[test]
    fn laddered_out_grade_pays_no_cross_penalty() {
        let mut a = zeroed();
        a.warmth = 1.0;
        a.trust = 1.0;
        a.intrigue = 1.0; // bond = 1.0 — the most expensive counterpart there is
        a.intimacy = 1.0;
        a.tension = 1.0; // chemistry = 1.0 → own tier 5, where a loss is possible
        let g = AxisGrades {
            intimacy: 1,
            ..Default::default()
        };
        // Unsteered: the push lands, the penalty takes more than it.
        let neutral = scoped(&a, g, crate::scope::AffinityScope::none());
        assert!(neutral.committed.intimacy < 0.0);
        assert!(neutral.cross_penalty_assessed.intimacy > 0.0);
        // Suppressed: nothing happens at all — not a smaller loss, zero. The
        // ladder maps g1 to 0, and a grade of 0 is simply where the proportional
        // penalty starts.
        let suppressed = scoped(&a, g, crate::scope::AffinityScope::chemistry());
        assert_eq!(suppressed.committed.intimacy, 0.0);
        assert_eq!(suppressed.cross_penalty_assessed.intimacy, 0.0);
    }

    /// `full` is the dominant production value and steers the suppress side —
    /// and because the mode is exclusive, bond gets no boost on those turns.
    #[test]
    fn full_scope_suppresses_chemistry_without_boosting_bond() {
        let a = zeroed();
        let o = scoped(
            &a,
            AxisGrades {
                trust: 2,
                intimacy: 2,
                tension: 1,
                ..Default::default()
            },
            crate::scope::AffinityScope::full(),
        );
        assert_eq!(o.scope_mode, ScopeMode::SuppressChemistry);
        assert!((o.committed.trust - 0.10).abs() < 1e-9, "no bond boost");
        assert!((o.committed.intimacy - 0.05).abs() < 1e-9, "g2 → g1");
        assert_eq!(o.committed.tension, 0.0, "g1 filtered");
    }

    /// The audit trail: a committed 0 must stay attributable. `effective_grades`
    /// records what was converted; the judge's originals are logged beside it.
    #[test]
    fn effective_grades_record_what_the_ladder_did() {
        let a = zeroed();
        let judged = AxisGrades {
            warmth: 1,
            trust: 1,
            intrigue: 0,
            intimacy: 1,
            tension: 2,
        };
        let o = scoped(&a, judged, crate::scope::AffinityScope::full());
        assert_eq!(
            o.effective_grades,
            AxisGrades {
                warmth: 1,
                trust: 1,
                intrigue: 0,
                intimacy: 0, // filtered
                tension: 1,  // halved
            }
        );
        // Neutral steering leaves the grades verbatim.
        let n = scoped(&a, judged, crate::scope::AffinityScope::none());
        assert_eq!(n.effective_grades, judged);
    }

    /// Identity settings turn 3.1 off without a code path of its own.
    #[test]
    fn identity_tuning_reproduces_30_under_any_scope() {
        let a = zeroed();
        let t = AffinityTuning {
            scope_bond_boost: 1.0,
            scope_chem_ladder: [0, 1, 2, 3, 4],
            ..Default::default()
        };
        let g = AxisGrades {
            trust: 2,
            intimacy: 1,
            ..Default::default()
        };
        for scope in [
            crate::scope::AffinityScope::bond(),
            crate::scope::AffinityScope::full(),
            crate::scope::AffinityScope::none(),
        ] {
            let o = grade_turn(
                &a,
                &g,
                &AffinityDeltas::default(),
                &PendingDeltas::default(),
                1.0,
                &scope,
                &t,
            );
            assert!((o.committed.trust - 0.10).abs() < 1e-9);
            assert!((o.committed.intimacy - 0.05).abs() < 1e-9);
        }
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
