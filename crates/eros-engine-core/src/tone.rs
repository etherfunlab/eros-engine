// SPDX-License-Identifier: AGPL-3.0-only
//! The tone of a chat turn, drawn by the engine and never by the judge
//! (spec 2026-11-12-chat-tone-emotional-design.md §3; the `focused` tone is
//! spec 2026-11-24-focused-tone-four-together-actions-and-user-gender-design.md §5).

use crate::events::{Employment, EventKind};
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
    /// The reply of someone at work: the default request and system prompt
    /// keep the history, the memories and the relationship, drop `[feelings]`
    /// and `[emotional_context]`, hold `[reply_length]` at the lowest tier
    /// and put one work line in `[this_turn]` (spec §5.4).
    Focused,
}

impl Tone {
    /// The value stored as an assistant row's `metadata.tone`.
    pub fn as_str(self) -> &'static str {
        match self {
            Tone::Default => "default",
            Tone::Emotional => "emotional",
            Tone::Focused => "focused",
        }
    }
}

/// A persona whose `moody` key is `true` draws `emotional` this many times
/// as often; `false`, this many times less.
const MOODY_FACTOR: f64 = 3.0;
/// Applied once when she is craving or wants to be cared about, whichever
/// of the two holds.
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
    let in_need =
        needs.is_some_and(|n| n.emotional == NeedLevel::High || n.indulgence == NeedLevel::High);
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

/// The chance of a `focused` turn for a full-time persona who is off work with
/// no incident under way (spec §5.2, §5.3). The `tones.focused` key switches it
/// on and does not scale it.
pub const CAREER_MINDED_P: f64 = 0.125;

/// Whether she is at work (spec §5.1): a `temp_work` incident is under way, or
/// the background is a `business_trip` and no incident is.
pub fn at_work(background: Option<EventKind>, foreground: Option<EventKind>) -> bool {
    foreground == Some(EventKind::TempWork)
        || (background == Some(EventKind::BusinessTrip) && foreground.is_none())
}

/// The chance that a turn is `focused` (spec §5.3), or `None` when no roll is
/// made. `key` is `tones.focused`; at `0` or below the tone is off. At work the
/// chance is `key`; off work it is `CAREER_MINDED_P` for a `full_time` persona
/// with no incident under way; any other turn makes no roll.
pub fn focused_probability(
    key: f64,
    background: Option<EventKind>,
    foreground: Option<EventKind>,
    employed: Employment,
) -> Option<f64> {
    if key <= 0.0 {
        None
    } else if at_work(background, foreground) {
        Some(key)
    } else if employed == Employment::FullTime && foreground.is_none() {
        Some(CAREER_MINDED_P)
    } else {
        None
    }
}

/// The tone for a `focused` probability `p` and a `roll` in `[0, 1)`.
pub fn draw_focused(p: f64, roll: f64) -> Tone {
    if roll < p {
        Tone::Focused
    } else {
        Tone::Default
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventKind::{
        BusinessTrip, Busy, Drinking, JoggingTogether, OnADate, Reading, ScoldedBySuperior,
        ScoldedSuperior, ShortTrip, TempWork, WantToQuitJob,
    };
    use NeedLevel::{High, Low, Mid};

    fn needs(emotional: NeedLevel, indulgence: NeedLevel) -> Needs {
        Needs {
            social: Mid,
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
        let calm = needs(Mid, Mid);
        let craving = needs(Mid, High);
        assert!(close(
            emotional_probability(0.05, None, Some(&calm), false),
            0.05
        ));
        assert!(close(
            emotional_probability(0.05, None, Some(&calm), true),
            0.10
        ));
        assert!(close(
            emotional_probability(0.05, None, Some(&craving), true),
            0.30
        ));
        assert!(close(
            emotional_probability(0.05, Some(true), Some(&craving), true),
            0.90
        ));
        assert!(close(
            emotional_probability(0.05, Some(false), Some(&craving), true),
            0.10
        ));
    }

    #[test]
    fn each_of_the_two_needs_counts_and_they_do_not_stack() {
        for n in [needs(High, Mid), needs(Mid, High), needs(High, High)] {
            assert!(
                close(emotional_probability(0.1, None, Some(&n), false), 0.3),
                "{n:?}"
            );
        }
    }

    #[test]
    fn the_other_levels_and_needs_do_not_count() {
        let mut n = needs(Low, Low);
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
        let craving = needs(Mid, High);
        assert_eq!(
            emotional_probability(1.0, Some(true), Some(&craving), true),
            1.0
        );
        assert_eq!(
            emotional_probability(0.0, Some(true), Some(&craving), true),
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

    #[test]
    fn she_is_at_work_on_a_temp_job_or_an_idle_business_trip() {
        assert!(at_work(None, Some(TempWork)));
        assert!(at_work(Some(ShortTrip), Some(TempWork)));
        assert!(at_work(Some(BusinessTrip), None));
        assert!(!at_work(Some(BusinessTrip), Some(Drinking)));
        assert!(!at_work(None, None));
        for k in [ScoldedBySuperior, ScoldedSuperior, WantToQuitJob, Busy] {
            assert!(!at_work(None, Some(k)), "{k:?}");
        }
        assert!(!at_work(Some(ShortTrip), None));
    }

    #[test]
    fn the_focused_probability_table() {
        use Employment::{Free, FullTime, PartTime};
        // at work: the key, for any employment
        for e in [Free, PartTime, FullTime] {
            assert_eq!(focused_probability(1.0, None, Some(TempWork), e), Some(1.0));
            assert_eq!(
                focused_probability(0.4, Some(BusinessTrip), None, e),
                Some(0.4)
            );
        }
        // off work: 1/8 for full_time with no incident, any background, any key above 0
        for key in [1e-12, 0.4, 1.0] {
            assert_eq!(focused_probability(key, None, None, FullTime), Some(0.125));
            assert_eq!(
                focused_probability(key, Some(ShortTrip), None, FullTime),
                Some(0.125)
            );
        }
        // off work, an incident under way: no roll, a date and a together kind included
        for fg in [OnADate, Drinking, JoggingTogether, Reading] {
            assert_eq!(
                focused_probability(1.0, None, Some(fg), FullTime),
                None,
                "{fg:?}"
            );
        }
        for e in [Free, PartTime] {
            assert_eq!(focused_probability(1.0, None, None, e), None);
        }
        // the key is the switch
        for key in [0.0, -0.1] {
            assert_eq!(
                focused_probability(key, None, Some(TempWork), FullTime),
                None
            );
            assert_eq!(focused_probability(key, None, None, FullTime), None);
        }
    }

    #[test]
    fn a_focused_draw_and_its_stored_name() {
        assert_eq!(draw_focused(0.125, 0.1249), Tone::Focused);
        assert_eq!(draw_focused(0.125, 0.125), Tone::Default);
        assert_eq!(draw_focused(1.0, 0.999_999), Tone::Focused);
        assert_eq!(Tone::Focused.as_str(), "focused");
    }
}
