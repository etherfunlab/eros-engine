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
    /// `ema_inertia ∈ [0, 1]` — 0 means full update, 0.8 is standard.
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

    pub fn infer_label(&self) -> Option<RelationshipLabel> {
        // Priority: romantic > friend > frenemy > slow_burn > stranger
        if self.warmth >= 0.7 && self.tension >= 0.3 && self.intimacy >= 0.4 {
            return Some(RelationshipLabel::Romantic);
        }
        if self.warmth >= 0.7 && self.trust >= 0.6 && self.tension < 0.2 {
            return Some(RelationshipLabel::Friend);
        }
        if self.warmth < 0.4 && self.tension >= 0.6 && self.intrigue >= 0.5 {
            return Some(RelationshipLabel::Frenemy);
        }
        if self.intrigue >= 0.6 && self.tension >= 0.4 && self.intimacy < 0.4 {
            return Some(RelationshipLabel::SlowBurn);
        }
        Some(RelationshipLabel::Stranger)
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
    fn infer_label_romantic_when_warm_intimate_and_tense() {
        let mut a = fresh();
        a.warmth = 0.8;
        a.tension = 0.4;
        a.intimacy = 0.5;
        assert_eq!(a.infer_label(), Some(RelationshipLabel::Romantic));
    }

    #[test]
    fn infer_label_friend_when_warm_trusted_low_tension() {
        let mut a = fresh();
        a.warmth = 0.75;
        a.trust = 0.7;
        a.tension = 0.1;
        assert_eq!(a.infer_label(), Some(RelationshipLabel::Friend));
    }

    #[test]
    fn infer_label_frenemy_when_cold_tense_intrigued() {
        let mut a = fresh();
        a.warmth = 0.3;
        a.tension = 0.7;
        a.intrigue = 0.6;
        assert_eq!(a.infer_label(), Some(RelationshipLabel::Frenemy));
    }

    #[test]
    fn infer_label_slow_burn_when_intrigued_tense_not_yet_intimate() {
        let mut a = fresh();
        a.intrigue = 0.7;
        a.tension = 0.5;
        a.intimacy = 0.2;
        assert_eq!(a.infer_label(), Some(RelationshipLabel::SlowBurn));
    }

    #[test]
    fn infer_label_stranger_when_no_thresholds_met() {
        let a = fresh();
        assert_eq!(a.infer_label(), Some(RelationshipLabel::Stranger));
    }
}
