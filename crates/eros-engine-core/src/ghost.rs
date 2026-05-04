// SPDX-License-Identifier: AGPL-3.0-only
//! Ghost decision: should the agent stay silent on this turn?
//!
//! Score formula and protection rules are deterministic — no LLM call.

use crate::affinity::Affinity;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GhostSignals {
    pub message_count: i64,
    pub hours_since_last_ghost: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GhostDecision {
    Ghost,
    Reply,
}

/// Pure score: (1-intrigue)*0.4 + (1-patience)*0.4 + tension*0.2
pub fn score(a: &Affinity) -> f64 {
    (1.0 - a.intrigue) * 0.4 + (1.0 - a.patience) * 0.4 + a.tension * 0.2
}

/// Decide whether to ghost, with 4 protection layers:
///   1. message_count < 10        → never ghost (relationship still nascent)
///   2. ghost_streak >= 2         → don't ghost twice in a row
///   3. last ghost < 1h ago       → cooldown
///   4. recent ghost (any time)   → threshold rises to 0.85; otherwise 0.65
pub fn decide(a: &Affinity, s: GhostSignals) -> GhostDecision {
    if s.message_count < 10 {
        return GhostDecision::Reply;
    }
    if a.ghost_streak >= 2 {
        return GhostDecision::Reply;
    }
    if matches!(s.hours_since_last_ghost, Some(h) if h < 1.0) {
        return GhostDecision::Reply;
    }
    let threshold = if s.hours_since_last_ghost.is_some() {
        0.85
    } else {
        0.65
    };
    if score(a) > threshold {
        GhostDecision::Ghost
    } else {
        GhostDecision::Reply
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::affinity::Affinity;
    use chrono::Utc;
    use uuid::Uuid;

    fn aff(intrigue: f64, patience: f64, tension: f64, ghost_streak: i32) -> Affinity {
        let now = Utc::now();
        Affinity {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            instance_id: Uuid::new_v4(),
            warmth: 0.3,
            trust: 0.2,
            intrigue,
            intimacy: 0.0,
            patience,
            tension,
            ghost_streak,
            last_ghost_at: None,
            total_ghosts: 0,
            relationship_label: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn never_ghost_when_message_count_below_10() {
        let a = aff(0.0, 0.0, 1.0, 0); // would normally ghost
        let s = GhostSignals {
            message_count: 5,
            hours_since_last_ghost: None,
        };
        assert_eq!(decide(&a, s), GhostDecision::Reply);
    }

    #[test]
    fn never_ghost_two_in_a_row() {
        let a = aff(0.0, 0.0, 1.0, 2);
        let s = GhostSignals {
            message_count: 50,
            hours_since_last_ghost: Some(0.5),
        };
        assert_eq!(decide(&a, s), GhostDecision::Reply);
    }

    #[test]
    fn cooldown_blocks_ghost_within_one_hour() {
        let a = aff(0.0, 0.0, 1.0, 1);
        let s = GhostSignals {
            message_count: 50,
            hours_since_last_ghost: Some(0.5),
        };
        assert_eq!(decide(&a, s), GhostDecision::Reply);
    }

    #[test]
    fn ghost_when_score_above_threshold_post_protection() {
        // ghost_score = (1-0.1)*0.4 + (1-0.1)*0.4 + 0.5*0.2 = 0.36 + 0.36 + 0.1 = 0.82
        // base threshold 0.65 → ghost
        let a = aff(0.1, 0.1, 0.5, 0);
        let s = GhostSignals {
            message_count: 50,
            hours_since_last_ghost: None,
        };
        assert_eq!(decide(&a, s), GhostDecision::Ghost);
    }

    #[test]
    fn raised_threshold_after_recent_ghost_blocks_mid_score() {
        // ghost_score = (1-0.5)*0.4 + (1-0.5)*0.4 + 0.0*0.2 = 0.4
        // base 0.65 → would NOT ghost; post-ghost 0.85 → would NOT ghost
        let a = aff(0.5, 0.5, 0.0, 1);
        let s = GhostSignals {
            message_count: 50,
            hours_since_last_ghost: Some(2.0),
        };
        assert_eq!(decide(&a, s), GhostDecision::Reply);
    }

    #[test]
    fn high_score_blocked_by_post_ghost_higher_threshold() {
        // ghost_score = (1-0.05)*0.4 + (1-0.05)*0.4 + 0.0*0.2 = 0.76
        // base 0.65 → would ghost; post-ghost 0.85 → would NOT ghost (0.76 < 0.85)
        let a = aff(0.05, 0.05, 0.0, 1);
        let s = GhostSignals {
            message_count: 50,
            hours_since_last_ghost: Some(2.0),
        };
        assert_eq!(decide(&a, s), GhostDecision::Reply);
    }

    #[test]
    fn ghost_score_formula() {
        let a = aff(0.4, 0.6, 0.5, 0);
        let expected = (1.0 - 0.4) * 0.4 + (1.0 - 0.6) * 0.4 + 0.5 * 0.2;
        assert!((score(&a) - expected).abs() < 1e-9);
    }
}
