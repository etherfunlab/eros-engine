// SPDX-License-Identifier: AGPL-3.0-only
//! Per-request injection scope flags (issue #40). These gate prompt
//! *injection* only — post-process writes (insight extraction, memory writes,
//! six-axis affinity eval) are unaffected.

use crate::affinity::Affinity;
use serde::{Deserialize, Serialize};

/// How much of the user-global structured profile ("基础画像") to inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsightMode {
    Off,
    /// Drop the intimate fields: love_values / emotional_needs / interests.
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
        .iter()
        .filter(|b| **b)
        .count()
    }

    /// Composite length score per the #40 spec. `None` when no axis is in scope
    /// (caller falls back to the strictest tier, matching `affinity = None`).
    pub fn length_score(self, a: &Affinity) -> Option<f64> {
        let warm01 = clamp01((a.warmth + 1.0) / 2.0);
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
            ghost_streak: 0,
            last_ghost_at: None,
            total_ghosts: 0,
            relationship_label: None,
            created_at: now,
            updated_at: now,
        }
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
        // warmth=0 → warm01=0.5; intimacy=0.5; tension=0.5 → bond=0.5
        // trust=0.9; intrigue=0.9; patience=0.9 → chemistry=0.9
        let a = affinity(0.0, 0.9, 0.9, 0.5, 0.9, 0.5);
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
        let a = affinity(0.0, 0.9, 0.9, 0.5, 0.9, 0.5);
        // warmth ∈ bond, trust ∈ chemistry → both active → avg
        let s = AffinityScope::from_axes(&[AffinityAxis::Warmth, AffinityAxis::Trust]);
        assert!((s.length_score(&a).unwrap() - 0.7).abs() < 1e-9);
    }
}
