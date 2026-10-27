// SPDX-License-Identifier: AGPL-3.0-only
//! Per-request injection scope flags (issue #40). These gate prompt
//! *injection* only — post-process writes (insight extraction, memory writes,
//! six-axis affinity eval) are unaffected.

use crate::affinity::{Affinity, AffinityLine};
use serde::{Deserialize, Serialize};

/// How much of the user-global structured profile ("基础画像") to inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsightMode {
    Off,
    /// Drop the intimate fields: love_values / relationship_history /
    /// interests / emotional_needs / family / finance_status / kinks /
    /// mental_health.
    Neutral,
    Full,
}

/// Caller-supplied memory injection scope. Default narrows today's behavior
/// (the #40 mitigation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Full,
    #[default]
    NeutralAndRelationship,
    RelationshipOnly,
    NeutralOnly,
    InsightsOnly,
    None,
}

impl MemoryScope {
    /// Resolve to `(insight mode, inject global memory X, inject relationship memory Y)`.
    pub fn resolve(self) -> (InsightMode, bool, bool) {
        match self {
            MemoryScope::Full => (InsightMode::Full, true, true),
            MemoryScope::NeutralAndRelationship => (InsightMode::Neutral, true, true),
            MemoryScope::RelationshipOnly => (InsightMode::Off, false, true),
            MemoryScope::NeutralOnly => (InsightMode::Neutral, false, false),
            MemoryScope::InsightsOnly => (InsightMode::Full, false, false),
            MemoryScope::None => (InsightMode::Off, false, false),
        }
    }
}

/// One of the eight intimate 基础画像 fields — the ones only
/// `InsightMode::Full` renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntimateField {
    LoveValues,
    RelationshipHistory,
    Kinks,
    Interests,
    EmotionalNeeds,
    Family,
    FinanceStatus,
    MentalHealth,
}

impl IntimateField {
    pub const ALL: [IntimateField; 8] = [
        IntimateField::LoveValues,
        IntimateField::RelationshipHistory,
        IntimateField::Kinks,
        IntimateField::Interests,
        IntimateField::EmotionalNeeds,
        IntimateField::Family,
        IntimateField::FinanceStatus,
        IntimateField::MentalHealth,
    ];

    /// The line this field reads and the lowest tier on it that lets the
    /// field into the prompt (spec 2026-10-27-affinity-gated-profile-recall
    /// §2.4). Each field reads its own line only, so friendship never
    /// unlocks romance-side knowledge.
    pub fn threshold(self) -> (AffinityLine, u8) {
        use AffinityLine::{Bond, Chemistry};
        match self {
            IntimateField::LoveValues => (Chemistry, 2),
            IntimateField::RelationshipHistory => (Chemistry, 3),
            IntimateField::Kinks => (Chemistry, 4),
            IntimateField::Interests => (Bond, 2),
            IntimateField::EmotionalNeeds => (Bond, 3),
            IntimateField::Family => (Bond, 3),
            IntimateField::FinanceStatus => (Bond, 4),
            IntimateField::MentalHealth => (Bond, 4),
        }
    }

    /// The field a profile-layer memory's `metadata.domain` label mirrors
    /// (spec §2.6). The label is prompt-owned, so it is matched trimmed and
    /// case-folded; anything else is unrouted (`None`). `kink` and
    /// `mental_health` are the fact ledger's names for the same split.
    pub fn from_memory_domain(domain: &str) -> Option<IntimateField> {
        Some(match domain.trim().to_ascii_lowercase().as_str() {
            "sexuality" | "kink" => IntimateField::Kinks,
            "romance" => IntimateField::LoveValues,
            "family" => IntimateField::Family,
            "finance" => IntimateField::FinanceStatus,
            "health" | "mental_health" => IntimateField::MentalHealth,
            _ => return None,
        })
    }
}

/// How much of the user's profile the relationship's affinity lets into the
/// prompt (spec 2026-10-27-affinity-gated-profile-recall). Read side only,
/// and always inside the caller's `MemoryScope`: the gate narrows, never
/// widens. Built from the two line tiers of the (decayed) affinity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecallGate {
    pub bond_tier: u8,
    pub chem_tier: u8,
}

impl RecallGate {
    /// A relationship with no affinity yet: tier 1 on both lines.
    pub const STRANGER: RecallGate = RecallGate {
        bond_tier: 1,
        chem_tier: 1,
    };

    pub fn from_affinity(a: &Affinity) -> RecallGate {
        RecallGate {
            bond_tier: a.bond_tier(),
            chem_tier: a.chem_tier(),
        }
    }

    pub fn allows(self, field: IntimateField) -> bool {
        let (line, min) = field.threshold();
        let tier = match line {
            AffinityLine::Bond => self.bond_tier,
            AffinityLine::Chemistry => self.chem_tier,
        };
        tier >= min
    }

    /// Whether a profile-layer memory labelled `domain` may be recalled.
    /// Unrouted (or absent) domains pass; the K ramp still applies.
    pub fn allows_memory_domain(self, domain: Option<&str>) -> bool {
        domain
            .and_then(IntimateField::from_memory_domain)
            .is_none_or(|f| self.allows(f))
    }

    /// The profile memory layer is recalled at all (higher tier ≥ 2).
    pub fn recalls_profile_layer(self) -> bool {
        self.higher_tier() >= 2
    }

    /// Profile-layer K from the site's constant `k`: none at tier 1, half
    /// (rounded up) at tier 2, all of it from tier 3 (spec §2.5).
    pub fn profile_k(self, k: i32) -> i32 {
        match self.higher_tier() {
            0 | 1 => 0,
            2 => (k + 1) / 2,
            _ => k,
        }
    }

    /// Relationship-layer K: shrinks at low tiers, never exceeds `k`
    /// (spec §2.7).
    pub fn relationship_k(self, k: i32) -> i32 {
        k.min(i32::from(self.higher_tier()))
    }

    /// The memory layers carry no field to route by line, so they read the
    /// higher of the two tiers.
    fn higher_tier(self) -> u8 {
        self.bond_tier.max(self.chem_tier)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AffinityAxis {
    Warmth,
    Trust,
    Intrigue,
    Intimacy,
    Patience,
    Tension,
}

/// Resolved set of affinity axes to inject. Default = `bond`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AffinityScope {
    pub warmth: bool,
    pub trust: bool,
    pub intrigue: bool,
    pub intimacy: bool,
    pub patience: bool,
    pub tension: bool,
}

impl Default for AffinityScope {
    fn default() -> Self {
        Self::bond()
    }
}

impl AffinityScope {
    pub fn none() -> Self {
        Self {
            warmth: false,
            trust: false,
            intrigue: false,
            intimacy: false,
            patience: false,
            tension: false,
        }
    }
    pub fn full() -> Self {
        Self {
            warmth: true,
            trust: true,
            intrigue: true,
            intimacy: true,
            patience: true,
            tension: true,
        }
    }
    /// 朋友感: warmth + intimacy + tension.
    pub fn bond() -> Self {
        Self {
            warmth: true,
            intimacy: true,
            tension: true,
            trust: false,
            intrigue: false,
            patience: false,
        }
    }
    /// 暧昧感: trust + intrigue + patience.
    pub fn chemistry() -> Self {
        Self {
            trust: true,
            intrigue: true,
            patience: true,
            warmth: false,
            intimacy: false,
            tension: false,
        }
    }
    pub fn from_axes(axes: &[AffinityAxis]) -> Self {
        let mut s = Self::none();
        for a in axes {
            match a {
                AffinityAxis::Warmth => s.warmth = true,
                AffinityAxis::Trust => s.trust = true,
                AffinityAxis::Intrigue => s.intrigue = true,
                AffinityAxis::Intimacy => s.intimacy = true,
                AffinityAxis::Patience => s.patience = true,
                AffinityAxis::Tension => s.tension = true,
            }
        }
        s
    }
    /// Any axis of the `bond()` half (warmth / intimacy / tension) active.
    /// The voice relationship line injects at half granularity — these two
    /// helpers are its flattening rule.
    pub fn any_bond_axis(&self) -> bool {
        self.warmth || self.intimacy || self.tension
    }
    /// Any axis of the `chemistry()` half (trust / intrigue / patience) active.
    pub fn any_chemistry_axis(&self) -> bool {
        self.trust || self.intrigue || self.patience
    }
    pub fn contains(self, axis: AffinityAxis) -> bool {
        match axis {
            AffinityAxis::Warmth => self.warmth,
            AffinityAxis::Trust => self.trust,
            AffinityAxis::Intrigue => self.intrigue,
            AffinityAxis::Intimacy => self.intimacy,
            AffinityAxis::Patience => self.patience,
            AffinityAxis::Tension => self.tension,
        }
    }
    pub fn is_empty(self) -> bool {
        !(self.warmth
            || self.trust
            || self.intrigue
            || self.intimacy
            || self.patience
            || self.tension)
    }

    /// Number of axes that are active (0..=6). Used for observability tracing.
    pub fn active_count(self) -> usize {
        [
            self.warmth,
            self.trust,
            self.intrigue,
            self.intimacy,
            self.patience,
            self.tension,
        ]
        .into_iter()
        .filter(|b| *b)
        .count()
    }

    /// Composite length score per the #40 spec. `None` when no axis is in scope
    /// (caller falls back to the strictest tier, matching `affinity = None`).
    pub fn length_score(self, a: &Affinity) -> Option<f64> {
        // warmth is 0..1 as of 4.0; the old (w+1)/2 shift would inflate this
        // half by 0.25.
        let warm01 = clamp01(a.warmth);
        let bond = clamp01((warm01 + a.intimacy + a.tension) / 3.0);
        let chemistry = clamp01((a.trust + a.intrigue + a.patience) / 3.0);
        let bond_active = self.warmth || self.intimacy || self.tension;
        let chem_active = self.trust || self.intrigue || self.patience;
        match (bond_active, chem_active) {
            (true, true) => Some((bond + chemistry) / 2.0),
            (true, false) => Some(bond),
            (false, true) => Some(chemistry),
            (false, false) => None,
        }
    }
}

fn clamp01(x: f64) -> f64 {
    x.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn affinity(
        warmth: f64,
        trust: f64,
        intrigue: f64,
        intimacy: f64,
        patience: f64,
        tension: f64,
    ) -> Affinity {
        let now = Utc::now();
        Affinity {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            instance_id: Uuid::new_v4(),
            warmth,
            trust,
            intrigue,
            intimacy,
            patience,
            tension,
            warmth_grade: 2,
            patience_grade: 2,
            ghost_streak: 0,
            last_ghost_at: None,
            total_ghosts: 0,
            feeling_clause: None,
            feeling_clause_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn gate(bond_tier: u8, chem_tier: u8) -> RecallGate {
        RecallGate {
            bond_tier,
            chem_tier,
        }
    }

    #[test]
    fn intimate_field_thresholds_match_the_spec_table() {
        use AffinityLine::*;
        use IntimateField::*;
        let want = [
            (LoveValues, (Chemistry, 2)),
            (RelationshipHistory, (Chemistry, 3)),
            (Kinks, (Chemistry, 4)),
            (Interests, (Bond, 2)),
            (EmotionalNeeds, (Bond, 3)),
            (Family, (Bond, 3)),
            (FinanceStatus, (Bond, 4)),
            (MentalHealth, (Bond, 4)),
        ];
        for (f, t) in want {
            assert_eq!(f.threshold(), t, "{f:?}");
        }
        assert_eq!(IntimateField::ALL.len(), 8);
    }

    #[test]
    fn a_field_unlocks_at_its_tier_on_its_own_line_only() {
        for f in IntimateField::ALL {
            let (line, min) = f.threshold();
            // The other line is maxed: it must not unlock the field.
            let on = |t: u8| match line {
                AffinityLine::Bond => gate(t, 5),
                AffinityLine::Chemistry => gate(5, t),
            };
            assert!(!on(min - 1).allows(f), "{f:?} below its tier");
            assert!(on(min).allows(f), "{f:?} at its tier");
        }
        assert!(!gate(5, 3).allows(IntimateField::Kinks));
        assert!(!gate(3, 5).allows(IntimateField::FinanceStatus));
    }

    #[test]
    fn memory_domains_mirror_their_fields() {
        use IntimateField::*;
        for (d, f) in [
            ("sexuality", Kinks),
            ("kink", Kinks),
            ("romance", LoveValues),
            ("family", Family),
            ("finance", FinanceStatus),
            ("health", MentalHealth),
            ("mental_health", MentalHealth),
            ("Sexuality", Kinks),
            (" sexuality ", Kinks),
        ] {
            assert_eq!(IntimateField::from_memory_domain(d), Some(f), "{d:?}");
        }
        for d in ["career", "communication", "", "sexual"] {
            assert_eq!(IntimateField::from_memory_domain(d), None, "{d:?}");
        }
    }

    #[test]
    fn memory_domain_gate() {
        assert!(RecallGate::STRANGER.allows_memory_domain(None));
        assert!(RecallGate::STRANGER.allows_memory_domain(Some("career")));
        assert!(!gate(1, 3).allows_memory_domain(Some("sexuality")));
        assert!(gate(1, 4).allows_memory_domain(Some("sexuality")));
        assert!(gate(1, 2).allows_memory_domain(Some("romance")));
        assert!(!gate(5, 1).allows_memory_domain(Some("romance")));
    }

    #[test]
    fn profile_k_ramps_on_the_higher_tier() {
        assert!(!RecallGate::STRANGER.recalls_profile_layer());
        assert!(gate(1, 2).recalls_profile_layer());
        for k in [1, 2, 4] {
            assert_eq!(RecallGate::STRANGER.profile_k(k), 0);
        }
        assert_eq!(gate(2, 1).profile_k(2), 1);
        assert_eq!(gate(1, 2).profile_k(4), 2);
        assert_eq!(gate(2, 1).profile_k(1), 1); // ⌈1/2⌉
        assert_eq!(gate(1, 3).profile_k(4), 4);
        assert_eq!(gate(5, 5).profile_k(2), 2);
    }

    #[test]
    fn relationship_k_is_capped_by_the_higher_tier() {
        let got: Vec<i32> = [gate(1, 1), gate(2, 1), gate(1, 3), gate(5, 5)]
            .into_iter()
            .map(|g| g.relationship_k(3))
            .collect();
        assert_eq!(got, vec![1, 2, 3, 3]);
        assert_eq!(gate(1, 1).relationship_k(2), 1);
        assert_eq!(gate(5, 1).relationship_k(2), 2);
    }

    #[test]
    fn gate_reads_the_two_line_tiers() {
        assert_eq!(
            RecallGate::from_affinity(&Affinity::unrecorded()),
            RecallGate::STRANGER
        );
        // bond = (trust + intrigue)/2 = 0.7 → tier 4;
        // chemistry = (intimacy + tension)/2 = 0.4 → tier 3.
        let a = affinity(0.5, 0.7, 0.7, 0.4, 0.5, 0.4);
        assert_eq!(RecallGate::from_affinity(&a), gate(4, 3));
    }

    #[test]
    fn memory_scope_resolution_table() {
        use InsightMode::*;
        assert_eq!(MemoryScope::Full.resolve(), (Full, true, true));
        assert_eq!(
            MemoryScope::NeutralAndRelationship.resolve(),
            (Neutral, true, true)
        );
        assert_eq!(MemoryScope::RelationshipOnly.resolve(), (Off, false, true));
        assert_eq!(MemoryScope::NeutralOnly.resolve(), (Neutral, false, false));
        assert_eq!(MemoryScope::InsightsOnly.resolve(), (Full, false, false));
        assert_eq!(MemoryScope::None.resolve(), (Off, false, false));
    }

    #[test]
    fn memory_scope_default_is_neutral_and_relationship() {
        assert_eq!(MemoryScope::default(), MemoryScope::NeutralAndRelationship);
    }

    #[test]
    fn memory_scope_serde_snake_case() {
        let s: MemoryScope = serde_json::from_str("\"relationship_only\"").unwrap();
        assert_eq!(s, MemoryScope::RelationshipOnly);
        // multi-word default variant round-trips
        let n: MemoryScope = serde_json::from_str("\"neutral_and_relationship\"").unwrap();
        assert_eq!(n, MemoryScope::NeutralAndRelationship);
        assert_eq!(
            serde_json::to_string(&MemoryScope::NeutralAndRelationship).unwrap(),
            "\"neutral_and_relationship\""
        );
        assert!(serde_json::from_str::<MemoryScope>("\"bogus\"").is_err());
    }

    #[test]
    fn affinity_scope_contains_matches_fields() {
        let s = AffinityScope::bond();
        assert!(s.contains(AffinityAxis::Warmth));
        assert!(s.contains(AffinityAxis::Intimacy));
        assert!(s.contains(AffinityAxis::Tension));
        assert!(!s.contains(AffinityAxis::Trust));
        assert!(!s.contains(AffinityAxis::Intrigue));
        assert!(!s.contains(AffinityAxis::Patience));
    }

    #[test]
    fn affinity_scope_default_is_bond() {
        let d = AffinityScope::default();
        assert_eq!(d, AffinityScope::bond());
        assert!(d.warmth && d.intimacy && d.tension);
        assert!(!d.trust && !d.intrigue && !d.patience);
    }

    #[test]
    fn affinity_scope_chemistry_and_full() {
        let c = AffinityScope::chemistry();
        assert!(c.trust && c.intrigue && c.patience);
        assert!(!c.warmth && !c.intimacy && !c.tension);
        let f = AffinityScope::full();
        assert!(!f.is_empty());
        assert!(f.warmth && f.trust && f.intrigue && f.intimacy && f.patience && f.tension);
    }

    #[test]
    fn affinity_scope_from_axes_and_empty() {
        let s = AffinityScope::from_axes(&[AffinityAxis::Warmth, AffinityAxis::Trust]);
        assert!(s.warmth && s.trust);
        assert!(!s.intrigue && !s.intimacy && !s.patience && !s.tension);
        assert!(AffinityScope::from_axes(&[]).is_empty());
        assert!(AffinityScope::none().is_empty());
    }

    #[test]
    fn affinity_scope_active_count() {
        assert_eq!(AffinityScope::none().active_count(), 0);
        assert_eq!(AffinityScope::bond().active_count(), 3);
        assert_eq!(AffinityScope::full().active_count(), 6);
        let one = AffinityScope::from_axes(&[AffinityAxis::Warmth]);
        assert_eq!(one.active_count(), 1);
    }

    #[test]
    fn affinity_axis_serde_snake_case() {
        let a: AffinityAxis = serde_json::from_str("\"warmth\"").unwrap();
        assert_eq!(a, AffinityAxis::Warmth);
        assert!(serde_json::from_str::<AffinityAxis>("\"warm\"").is_err());
    }

    #[test]
    fn length_score_named_cases() {
        // warmth=0.5 (0..1 as of 4.0) → warm01=0.5; intimacy=0.5; tension=0.5 → bond=0.5
        // trust=0.9; intrigue=0.9; patience=0.9 → chemistry=0.9
        let a = affinity(0.5, 0.9, 0.9, 0.5, 0.9, 0.5);
        let bond = AffinityScope::bond().length_score(&a).unwrap();
        let chem = AffinityScope::chemistry().length_score(&a).unwrap();
        let full = AffinityScope::full().length_score(&a).unwrap();
        assert!((bond - 0.5).abs() < 1e-9);
        assert!((chem - 0.9).abs() < 1e-9);
        assert!((full - 0.7).abs() < 1e-9); // (0.5 + 0.9) / 2
        assert_eq!(AffinityScope::none().length_score(&a), None);
    }

    #[test]
    fn length_score_array_activates_both_triads() {
        let a = affinity(0.5, 0.9, 0.9, 0.5, 0.9, 0.5);
        // warmth ∈ bond, trust ∈ chemistry → both active → avg
        let s = AffinityScope::from_axes(&[AffinityAxis::Warmth, AffinityAxis::Trust]);
        assert!((s.length_score(&a).unwrap() - 0.7).abs() < 1e-9);
    }

    #[test]
    fn affinity_scope_half_detection() {
        assert!(AffinityScope::bond().any_bond_axis());
        assert!(!AffinityScope::bond().any_chemistry_axis());
        assert!(AffinityScope::chemistry().any_chemistry_axis());
        assert!(!AffinityScope::chemistry().any_bond_axis());
        assert!(AffinityScope::full().any_bond_axis());
        assert!(AffinityScope::full().any_chemistry_axis());
        assert!(!AffinityScope::none().any_bond_axis());
        assert!(!AffinityScope::none().any_chemistry_axis());
        // A single axis activates exactly its half.
        let w = AffinityScope::from_axes(&[AffinityAxis::Warmth]);
        assert!(w.any_bond_axis() && !w.any_chemistry_axis());
        let t = AffinityScope::from_axes(&[AffinityAxis::Trust]);
        assert!(t.any_chemistry_axis() && !t.any_bond_axis());
    }
}
