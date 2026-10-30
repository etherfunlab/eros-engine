// SPDX-License-Identifier: AGPL-3.0-only
//! The persona's basic needs: rated by the PDE judge, carried forward in time
//! by the engine (spec 2026-10-13-persona-needs-design.md §3).

use serde::{Deserialize, Serialize};

use crate::types::GivenItem;

/// One need's level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NeedLevel {
    Low,
    #[default]
    Mid,
    High,
}

/// The needs at one moment. Stored as a user row's `metadata.needs` and in a
/// voice call's snapshot, so the serde shape is a persisted contract. A value
/// stored before `outing` and `indulgence` existed reads them as `mid` (spec
/// 2026-10-28-outing-indulgence-needs-and-habits-design.md §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Needs {
    pub hunger: NeedLevel,
    pub social: NeedLevel,
    pub stamina: NeedLevel,
    pub emotional: NeedLevel,
    /// `high` wants to go out, `low` wants to stay in.
    #[serde(default)]
    pub outing: NeedLevel,
    /// `high` craves what she's hooked on: a smoke, a drink, weed, a coffee.
    #[serde(default)]
    pub indulgence: NeedLevel,
}

/// Whether the persona smokes, drinks, uses weed, drinks coffee:
/// `art_metadata.smoking`, `alcoholic`, `drug`, `coffee`, each `None` when
/// unset (spec 2026-10-28 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Habits {
    pub smoking: Option<bool>,
    pub alcoholic: Option<bool>,
    pub drug: Option<bool>,
    pub coffee: Option<bool>,
}

impl Habits {
    /// How many of the four are `true`.
    pub fn count_true(self) -> usize {
        [self.smoking, self.alcoholic, self.drug, self.coffee]
            .into_iter()
            .filter(|h| *h == Some(true))
            .count()
    }

    /// The key for what `item` is: tobacco, alcohol, weed or coffee. `None`
    /// for any other item, or when the key is unset.
    pub fn uses(self, item: GivenItem) -> Option<bool> {
        match item {
            GivenItem::Cigarette => self.smoking,
            GivenItem::Alcohol => self.alcoholic,
            GivenItem::Herb => self.drug,
            GivenItem::Coffee => self.coffee,
            _ => None,
        }
    }
}

/// Where each need settles with time, and the hours one step takes (§3.2).
const HUNGER_DRIFT: (NeedLevel, f64) = (NeedLevel::High, 4.0);
const SOCIAL_DRIFT: (NeedLevel, f64) = (NeedLevel::High, 12.0);
const STAMINA_DRIFT: (NeedLevel, f64) = (NeedLevel::Mid, 8.0);
const EMOTIONAL_DRIFT: (NeedLevel, f64) = (NeedLevel::Mid, 12.0);
const OUTING_DRIFT: (NeedLevel, f64) = (NeedLevel::Mid, 8.0);

/// Where `indulgence` settles and how fast, by how many habits are `true`
/// (spec 2026-10-28 §2.2): the more she uses, the sooner the urge returns.
/// Three or four share the fastest pace.
fn indulgence_drift(habits: Habits) -> (NeedLevel, f64) {
    match habits.count_true() {
        0 => (NeedLevel::Mid, 8.0),
        1 => (NeedLevel::High, 12.0),
        2 => (NeedLevel::High, 8.0),
        _ => (NeedLevel::High, 4.0),
    }
}

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
pub fn carried(rated: Needs, hours: f64, habits: Habits) -> Needs {
    Needs {
        hunger: drift(rated.hunger, HUNGER_DRIFT, hours),
        social: drift(rated.social, SOCIAL_DRIFT, hours),
        stamina: drift(rated.stamina, STAMINA_DRIFT, hours),
        emotional: drift(rated.emotional, EMOTIONAL_DRIFT, hours),
        outing: drift(rated.outing, OUTING_DRIFT, hours),
        indulgence: drift(rated.indulgence, indulgence_drift(habits), hours),
    }
}

/// Hours after the rating at which one need, rated `hours` ago, takes its
/// next step; `None` once it is at rest.
fn next_step(level: NeedLevel, (rest, period): (NeedLevel, f64), hours: f64) -> Option<f64> {
    if drift(level, (rest, period), hours) == rest {
        return None;
    }
    let done = (hours.max(0.0) / period).floor();
    Some((done + 1.0) * period)
}

/// Hours after the rating at which levels rated `hours` ago next change under
/// `carried`, or `None` when every need is at rest.
pub fn next_change(rated: Needs, hours: f64, habits: Habits) -> Option<f64> {
    [
        next_step(rated.hunger, HUNGER_DRIFT, hours),
        next_step(rated.social, SOCIAL_DRIFT, hours),
        next_step(rated.stamina, STAMINA_DRIFT, hours),
        next_step(rated.emotional, EMOTIONAL_DRIFT, hours),
        next_step(rated.outing, OUTING_DRIFT, hours),
        next_step(rated.indulgence, indulgence_drift(habits), hours),
    ]
    .into_iter()
    .flatten()
    .min_by(f64::total_cmp)
}

/// What an accepted give of `item` does to (hunger, social, indulgence), in
/// levels (spec 2026-10-30 §2.1): food fills her; tobacco, drink and weed
/// loosen her up; medicine and coffee do the opposite. `None` for an item
/// that moves nothing.
fn give_move(item: GivenItem) -> Option<(i8, i8, i8)> {
    use GivenItem::*;
    Some(match item {
        Food => (-1, 0, 0),
        Cigarette | Alcohol | Herb => (-1, 1, 1),
        Medicine | Coffee => (-1, -1, -1),
        _ => return None,
    })
}

/// `base` moved `by` levels, stopping at the ends; `rest` when the item does
/// not move this need at all.
fn step(base: NeedLevel, rest: NeedLevel, by: i8) -> NeedLevel {
    if by == 0 {
        rest
    } else {
        NeedLevel::from_rank(base.rank() + by)
    }
}

/// The levels to store after an accepted give of `item` (spec 2026-10-30
/// §2.2): the needs the item moves are `baseline` (the levels the judge was
/// shown) moved one level, whatever the judge rated them; the rest are
/// `judged`, else `baseline`. `None` when there is nothing to write: no
/// baseline and no rating, or an item that moves nothing and no rating.
pub fn after_give(
    baseline: Option<Needs>,
    judged: Option<Needs>,
    item: GivenItem,
) -> Option<Needs> {
    let (Some(base), Some((h, s, i))) = (baseline, give_move(item)) else {
        return judged;
    };
    let rest = judged.unwrap_or(base);
    Some(Needs {
        hunger: step(base.hunger, rest.hunger, h),
        social: step(base.social, rest.social, s),
        indulgence: step(base.indulgence, rest.indulgence, i),
        ..rest
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::GivenItem;
    use NeedLevel::{High, Low, Mid};

    fn all(level: NeedLevel) -> Needs {
        Needs {
            hunger: level,
            social: level,
            stamina: level,
            emotional: level,
            outing: Mid,
            indulgence: Mid,
        }
    }

    fn habits(true_keys: usize) -> Habits {
        let set = |i: usize| Some(i < true_keys);
        Habits {
            smoking: set(0),
            alcoholic: set(1),
            drug: set(2),
            coffee: set(3),
        }
    }

    #[test]
    fn no_time_or_negative_time_changes_nothing() {
        for level in [Low, Mid, High] {
            assert_eq!(carried(all(level), 0.0, Habits::default()), all(level));
            assert_eq!(carried(all(level), -5.0, Habits::default()), all(level));
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
                pick(&carried(all(from), hours, Habits::default())),
                want,
                "{name} {from:?} +{hours}h"
            );
        }
    }

    #[test]
    fn next_change_is_none_when_every_need_is_at_rest() {
        let rest = Needs {
            hunger: High,
            social: High,
            stamina: Mid,
            emotional: Mid,
            outing: Mid,
            indulgence: Mid,
        };
        for hours in [0.0, 5.0, 1000.0] {
            assert_eq!(
                next_change(rest, hours, Habits::default()),
                None,
                "+{hours}h"
            );
        }
    }

    /// (rated, hours since the rating, expected hours of the next change).
    #[test]
    fn next_change_is_the_earliest_pending_step() {
        let at_rest = Needs {
            hunger: High,
            social: High,
            stamina: Mid,
            emotional: Mid,
            outing: Mid,
            indulgence: Mid,
        };
        let hunger_low = Needs {
            hunger: Low,
            ..at_rest
        };
        let stamina_high = Needs {
            stamina: High,
            ..at_rest
        };
        let hunger_and_social_low = Needs {
            hunger: Low,
            social: Low,
            ..at_rest
        };
        let cases = [
            (hunger_low, 0.0, Some(4.0)),
            (hunger_low, -2.0, Some(4.0)),
            (hunger_low, 5.0, Some(8.0)),
            (hunger_low, 8.0, None),
            (stamina_high, 3.0, Some(8.0)),
            (stamina_high, 8.0, None),
            (hunger_and_social_low, 1.0, Some(4.0)),
            (hunger_and_social_low, 9.0, Some(12.0)),
            (hunger_and_social_low, 13.0, Some(24.0)),
        ];
        for (rated, hours, want) in cases {
            assert_eq!(
                next_change(rated, hours, Habits::default()),
                want,
                "{rated:?} +{hours}h"
            );
        }
    }

    /// At the reported hour, `carried` returns different levels; just before
    /// it, the same ones.
    #[test]
    fn next_change_agrees_with_carried() {
        let rated = Needs {
            hunger: Low,
            social: Mid,
            stamina: High,
            emotional: Low,
            outing: High,
            indulgence: Low,
        };
        let mut hours = 0.0;
        while let Some(at) = next_change(rated, hours, Habits::default()) {
            assert_eq!(
                carried(rated, at - 0.001, Habits::default()),
                carried(rated, hours, Habits::default())
            );
            assert_ne!(
                carried(rated, at, Habits::default()),
                carried(rated, hours, Habits::default())
            );
            hours = at;
        }
        assert_eq!(
            carried(rated, hours, Habits::default()),
            carried(rated, 1000.0, Habits::default())
        );
    }

    #[test]
    fn needs_serde_shape() {
        let n = Needs {
            hunger: High,
            social: Low,
            stamina: Mid,
            emotional: Mid,
            outing: High,
            indulgence: Low,
        };
        let v = serde_json::to_value(n).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"hunger": "high", "social": "low", "stamina": "mid", "emotional": "mid",
                "outing": "high", "indulgence": "low"})
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

    fn only(outing: NeedLevel, indulgence: NeedLevel) -> Needs {
        Needs {
            outing,
            indulgence,
            ..all(Mid)
        }
    }

    #[test]
    fn outing_drifts_to_mid_every_8h() {
        for n in 0..=3 {
            for (from, hours, want) in [
                (Low, 7.9, Low),
                (Low, 8.0, Mid),
                (High, 8.0, Mid),
                (High, 1000.0, Mid),
            ] {
                assert_eq!(
                    carried(only(from, Mid), hours, habits(n)).outing,
                    want,
                    "{n} true, {from:?} +{hours}h"
                );
            }
        }
    }

    /// (habits, start level, hours, expected level).
    #[test]
    fn indulgence_drift_follows_the_habit_count() {
        let cases = [
            (Habits::default(), High, 8.0, Mid),
            (Habits::default(), Low, 1000.0, Mid),
            (habits(0), High, 8.0, Mid),
            (habits(0), Low, 1000.0, Mid),
            (habits(1), Low, 11.9, Low),
            (habits(1), Low, 12.0, Mid),
            (habits(1), Low, 24.0, High),
            (habits(2), Low, 8.0, Mid),
            (habits(2), Low, 16.0, High),
            (habits(3), Low, 4.0, Mid),
            (habits(3), Mid, 4.0, High),
            (habits(3), High, 1000.0, High),
            (habits(4), Low, 4.0, Mid),
            (habits(4), Mid, 4.0, High),
        ];
        for (h, from, hours, want) in cases {
            assert_eq!(
                carried(only(Mid, from), hours, h).indulgence,
                want,
                "{h:?} {from:?} +{hours}h"
            );
        }
    }

    #[test]
    fn next_change_follows_the_habit_count() {
        let rested = Needs {
            hunger: High,
            social: High,
            ..only(Mid, Low)
        };
        assert_eq!(next_change(rested, 0.0, habits(0)), Some(8.0));
        assert_eq!(next_change(rested, 0.0, habits(1)), Some(12.0));
        assert_eq!(next_change(rested, 5.0, habits(3)), Some(8.0));
        assert_eq!(next_change(rested, 8.0, habits(3)), None);
    }

    #[test]
    fn a_rating_stored_before_outing_and_indulgence_reads_them_as_mid() {
        let old = serde_json::json!({"hunger": "high", "social": "low", "stamina": "mid", "emotional": "high"});
        assert_eq!(
            serde_json::from_value::<Needs>(old).unwrap(),
            Needs {
                hunger: High,
                social: Low,
                stamina: Mid,
                emotional: High,
                outing: Mid,
                indulgence: Mid,
            }
        );
        assert!(serde_json::from_value::<Needs>(serde_json::json!({
            "hunger": "high", "social": "low", "stamina": "mid", "outing": "low", "indulgence": "low"
        }))
        .is_err());
    }

    /// (item, expected hunger / social / indulgence after the move), the judge
    /// abstaining.
    #[test]
    fn each_item_moves_its_needs_one_level_from_the_baseline() {
        use GivenItem::*;
        let base = all(Mid);
        let cases = [
            (Food, (Low, Mid, Mid)),
            (Cigarette, (Low, High, High)),
            (Alcohol, (Low, High, High)),
            (Herb, (Low, High, High)),
            (Medicine, (Low, Low, Low)),
            (Coffee, (Low, Low, Low)),
        ];
        for (item, (h, s, i)) in cases {
            let got = after_give(Some(base), None, item).unwrap();
            assert_eq!(
                (got.hunger, got.social, got.indulgence),
                (h, s, i),
                "{item:?}"
            );
            assert_eq!(
                (got.stamina, got.emotional, got.outing),
                (Mid, Mid, Mid),
                "{item:?} leaves the rest"
            );
        }
    }

    #[test]
    fn moves_clamp_at_the_ends() {
        let floor = Needs {
            hunger: Low,
            social: Low,
            indulgence: Low,
            ..all(Mid)
        };
        let got = after_give(Some(floor), None, GivenItem::Medicine).unwrap();
        assert_eq!((got.hunger, got.social, got.indulgence), (Low, Low, Low));
        let ceiling = Needs {
            social: High,
            indulgence: High,
            ..all(Mid)
        };
        let got = after_give(Some(ceiling), None, GivenItem::Alcohol).unwrap();
        assert_eq!((got.social, got.indulgence), (High, High));
    }

    #[test]
    fn the_judge_is_overridden_on_moved_needs_and_kept_on_the_rest() {
        let judged = Needs {
            hunger: Low,
            social: High,
            stamina: High,
            emotional: Low,
            outing: High,
            indulgence: High,
        };
        let got = after_give(Some(all(Mid)), Some(judged), GivenItem::Food).unwrap();
        assert_eq!(
            got.hunger, Low,
            "baseline mid −1, not the judge's low again"
        );
        assert_eq!(got.social, High, "the judge's");
        assert_eq!(got.indulgence, High, "the judge's");
        assert_eq!((got.stamina, got.emotional, got.outing), (High, Low, High));
    }

    #[test]
    fn a_valuable_passes_the_judge_through() {
        let judged = all(High);
        assert_eq!(
            after_give(Some(all(Mid)), Some(judged), GivenItem::Watch),
            Some(judged)
        );
        assert_eq!(
            after_give(Some(all(Mid)), None, GivenItem::Watch),
            None,
            "nothing to write"
        );
    }

    #[test]
    fn without_a_baseline_the_judge_is_written_unshifted_or_nothing() {
        let judged = all(High);
        assert_eq!(
            after_give(None, Some(judged), GivenItem::Food),
            Some(judged)
        );
        assert_eq!(after_give(None, None, GivenItem::Food), None);
    }

    #[test]
    fn habits_count_and_uses() {
        assert_eq!(Habits::default().count_true(), 0);
        assert_eq!(habits(0).count_true(), 0);
        assert_eq!(habits(2).count_true(), 2);
        assert_eq!(habits(4).count_true(), 4);
        let h = Habits {
            smoking: Some(true),
            alcoholic: Some(false),
            drug: None,
            coffee: Some(false),
        };
        assert_eq!(h.uses(GivenItem::Cigarette), Some(true));
        assert_eq!(h.uses(GivenItem::Alcohol), Some(false));
        assert_eq!(h.uses(GivenItem::Herb), None);
        assert_eq!(h.uses(GivenItem::Coffee), Some(false));
        assert_eq!(h.uses(GivenItem::Tea), None);
    }
}
