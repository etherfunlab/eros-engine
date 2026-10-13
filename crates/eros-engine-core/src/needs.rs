// SPDX-License-Identifier: AGPL-3.0-only
//! The persona's basic needs: rated by the PDE judge, carried forward in time
//! by the engine (spec 2026-10-13-persona-needs-design.md §3).

use serde::{Deserialize, Serialize};

/// One need's level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NeedLevel {
    Low,
    Mid,
    High,
}

/// The four needs at one moment. Stored as a user row's `metadata.needs` and
/// in a voice call's snapshot, so the serde shape is a persisted contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Needs {
    pub hunger: NeedLevel,
    pub social: NeedLevel,
    pub stamina: NeedLevel,
    pub emotional: NeedLevel,
}

/// Where each need settles with time, and the hours one step takes (§3.2).
const HUNGER_DRIFT: (NeedLevel, f64) = (NeedLevel::High, 4.0);
const SOCIAL_DRIFT: (NeedLevel, f64) = (NeedLevel::High, 12.0);
const STAMINA_DRIFT: (NeedLevel, f64) = (NeedLevel::Mid, 8.0);
const EMOTIONAL_DRIFT: (NeedLevel, f64) = (NeedLevel::Mid, 12.0);

impl NeedLevel {
    fn rank(self) -> i8 {
        match self {
            NeedLevel::Low => 0,
            NeedLevel::Mid => 1,
            NeedLevel::High => 2,
        }
    }

    fn from_rank(rank: i8) -> Self {
        match rank {
            ..=0 => NeedLevel::Low,
            1 => NeedLevel::Mid,
            _ => NeedLevel::High,
        }
    }
}

/// One level toward `rest` for every full `period` hours, stopping there.
fn drift(level: NeedLevel, (rest, period): (NeedLevel, f64), hours: f64) -> NeedLevel {
    if hours <= 0.0 {
        return level;
    }
    // `as` saturates, so a huge or NaN span cannot overflow.
    let steps = (hours / period).floor() as i8;
    let (from, to) = (level.rank(), rest.rank());
    let moved = if from < to {
        from + steps.min(to - from)
    } else {
        from - steps.min(from - to)
    };
    NeedLevel::from_rank(moved)
}

/// Levels rated `hours` ago, carried forward to now (§3.2).
pub fn carried(rated: Needs, hours: f64) -> Needs {
    Needs {
        hunger: drift(rated.hunger, HUNGER_DRIFT, hours),
        social: drift(rated.social, SOCIAL_DRIFT, hours),
        stamina: drift(rated.stamina, STAMINA_DRIFT, hours),
        emotional: drift(rated.emotional, EMOTIONAL_DRIFT, hours),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use NeedLevel::{High, Low, Mid};

    fn all(level: NeedLevel) -> Needs {
        Needs {
            hunger: level,
            social: level,
            stamina: level,
            emotional: level,
        }
    }

    #[test]
    fn no_time_or_negative_time_changes_nothing() {
        for level in [Low, Mid, High] {
            assert_eq!(carried(all(level), 0.0), all(level));
            assert_eq!(carried(all(level), -5.0), all(level));
        }
    }

    /// Each need: (field, start level, hours, expected level).
    #[test]
    fn each_need_drifts_toward_its_resting_level() {
        type Pick = fn(&Needs) -> NeedLevel;
        let hunger: Pick = |n| n.hunger;
        let social: Pick = |n| n.social;
        let stamina: Pick = |n| n.stamina;
        let emotional: Pick = |n| n.emotional;
        let cases: [(&str, Pick, NeedLevel, f64, NeedLevel); 16] = [
            ("hunger", hunger, Low, 3.9, Low),
            ("hunger", hunger, Low, 4.0, Mid),
            ("hunger", hunger, Low, 8.0, High),
            ("hunger", hunger, Low, 1000.0, High),
            ("hunger", hunger, High, 1000.0, High),
            ("social", social, Low, 11.9, Low),
            ("social", social, Low, 12.0, Mid),
            ("social", social, Low, 24.0, High),
            ("stamina", stamina, Low, 7.9, Low),
            ("stamina", stamina, Low, 8.0, Mid),
            ("stamina", stamina, Low, 80.0, Mid),
            ("stamina", stamina, High, 8.0, Mid),
            ("stamina", stamina, High, 80.0, Mid),
            ("emotional", emotional, High, 11.9, High),
            ("emotional", emotional, High, 12.0, Mid),
            ("emotional", emotional, Low, 1000.0, Mid),
        ];
        for (name, pick, from, hours, want) in cases {
            assert_eq!(
                pick(&carried(all(from), hours)),
                want,
                "{name} {from:?} +{hours}h"
            );
        }
    }

    #[test]
    fn needs_serde_shape() {
        let n = Needs {
            hunger: High,
            social: Low,
            stamina: Mid,
            emotional: Mid,
        };
        let v = serde_json::to_value(n).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"hunger": "high", "social": "low", "stamina": "mid", "emotional": "mid"})
        );
        assert_eq!(serde_json::from_value::<Needs>(v).unwrap(), n);
        assert!(serde_json::from_value::<Needs>(
            serde_json::json!({"hunger": "high", "social": "low", "stamina": "mid"})
        )
        .is_err());
        assert!(serde_json::from_value::<Needs>(
            serde_json::json!({"hunger": "medium", "social": "low", "stamina": "mid", "emotional": "mid"})
        )
        .is_err());
    }
}
