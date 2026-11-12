// SPDX-License-Identifier: AGPL-3.0-only
//! The tone of a chat turn, drawn by the engine and never by the judge
//! (spec 2026-11-12-chat-tone-emotional-design.md §3).

use crate::needs::{NeedLevel, Needs};

/// How a turn's reply is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tone {
    /// The considered reply: history, profile, memories and the affinity
    /// conclusions are all in front of the model.
    #[default]
    Default,
    /// The least-hint reply: persona, the user's line and little else.
    Emotional,
}

impl Tone {
    /// The value stored as an assistant row's `metadata.tone`.
    pub fn as_str(self) -> &'static str {
        match self {
            Tone::Default => "default",
            Tone::Emotional => "emotional",
        }
    }
}

/// A persona whose `moody` key is `true` draws `emotional` this many times
/// as often; `false`, this many times less.
const MOODY_FACTOR: f64 = 3.0;
/// Applied once when she is hungry, tired, craving or wants to be cared
/// about, however many of the four hold.
const NEEDS_FACTOR: f64 = 3.0;
/// Applied when the pair is romantically compatible.
const COMPATIBLE_FACTOR: f64 = 2.0;

/// The chance that a turn is `emotional`: `base` times the three factors,
/// capped at 1. No needs rating counts as no need holding.
pub fn emotional_probability(
    base: f64,
    moody: Option<bool>,
    needs: Option<&Needs>,
    compatible: bool,
) -> f64 {
    let moody = match moody {
        Some(true) => MOODY_FACTOR,
        Some(false) => 1.0 / MOODY_FACTOR,
        None => 1.0,
    };
    let in_need = needs.is_some_and(|n| {
        n.emotional == NeedLevel::High
            || n.stamina == NeedLevel::Low
            || n.hunger == NeedLevel::High
            || n.indulgence == NeedLevel::High
    });
    let needs = if in_need { NEEDS_FACTOR } else { 1.0 };
    let compatible = if compatible { COMPATIBLE_FACTOR } else { 1.0 };
    (base * moody * needs * compatible).min(1.0)
}

/// The tone for a probability `p` and a `roll` in `[0, 1)`.
pub fn draw(p: f64, roll: f64) -> Tone {
    if roll < p {
        Tone::Emotional
    } else {
        Tone::Default
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use NeedLevel::{High, Low, Mid};

    fn needs(
        emotional: NeedLevel,
        stamina: NeedLevel,
        hunger: NeedLevel,
        indulgence: NeedLevel,
    ) -> Needs {
        Needs {
            hunger,
            social: Mid,
            stamina,
            emotional,
            outing: Mid,
            indulgence,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn the_spec_table_at_base_005() {
        let calm = needs(Mid, Mid, Mid, Mid);
        let hungry = needs(Mid, Mid, High, Mid);
        assert!(close(
            emotional_probability(0.05, None, Some(&calm), false),
            0.05
        ));
        assert!(close(
            emotional_probability(0.05, None, Some(&calm), true),
            0.10
        ));
        assert!(close(
            emotional_probability(0.05, None, Some(&hungry), true),
            0.30
        ));
        assert!(close(
            emotional_probability(0.05, Some(true), Some(&hungry), true),
            0.90
        ));
        assert!(close(
            emotional_probability(0.05, Some(false), Some(&hungry), true),
            0.10
        ));
    }

    #[test]
    fn each_of_the_four_needs_counts_and_they_do_not_stack() {
        for n in [
            needs(High, Mid, Mid, Mid),
            needs(Mid, Low, Mid, Mid),
            needs(Mid, Mid, High, Mid),
            needs(Mid, Mid, Mid, High),
            needs(High, Low, High, High),
        ] {
            assert!(
                close(emotional_probability(0.1, None, Some(&n), false), 0.3),
                "{n:?}"
            );
        }
    }

    #[test]
    fn the_other_levels_and_needs_do_not_count() {
        let mut n = needs(Low, High, Low, Low);
        n.social = High;
        n.outing = Low;
        assert!(close(
            emotional_probability(0.1, None, Some(&n), false),
            0.1
        ));
        assert!(
            close(emotional_probability(0.1, None, None, false), 0.1),
            "no rating is ×1"
        );
    }

    #[test]
    fn capped_at_one_and_zero_stays_zero() {
        let hungry = needs(Mid, Mid, High, Mid);
        assert_eq!(
            emotional_probability(1.0, Some(true), Some(&hungry), true),
            1.0
        );
        assert_eq!(
            emotional_probability(0.0, Some(true), Some(&hungry), true),
            0.0
        );
    }

    #[test]
    fn draw_is_emotional_below_p() {
        assert_eq!(draw(0.3, 0.29), Tone::Emotional);
        assert_eq!(draw(0.3, 0.3), Tone::Default);
        assert_eq!(draw(1.0, 0.999_999), Tone::Emotional);
        assert_eq!(draw(0.0, 0.0), Tone::Default);
        assert_eq!(Tone::Emotional.as_str(), "emotional");
        assert_eq!(Tone::Default.as_str(), "default");
    }
}
