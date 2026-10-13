// SPDX-License-Identifier: AGPL-3.0-only
//! Life events: what may happen in the persona's day, and which one does
//! (spec 2026-10-13-life-events-design.md §3–§4).

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::needs::{NeedLevel, Needs};

/// What happens to the persona.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    TempWork,
    ExMessage,
    WeatherChange,
    PoliceEncounter,
    AnimalEncounter,
}

impl EventKind {
    /// Every kind, in draw order.
    pub const ALL: [EventKind; 5] = [
        EventKind::TempWork,
        EventKind::ExMessage,
        EventKind::WeatherChange,
        EventKind::PoliceEncounter,
        EventKind::AnimalEncounter,
    ];

    /// The stored and wire value.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::TempWork => "temp_work",
            EventKind::ExMessage => "ex_message",
            EventKind::WeatherChange => "weather_change",
            EventKind::PoliceEncounter => "police_encounter",
            EventKind::AnimalEncounter => "animal_encounter",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// One kind's place in the draw (§3): relative weight, hours before it can
/// happen again, hours it lasts.
struct Rule {
    weight: f64,
    cooldown_hours: i64,
    lasts_hours: i64,
}

fn rule(kind: EventKind) -> Rule {
    match kind {
        EventKind::TempWork => Rule {
            weight: 3.0,
            cooldown_hours: 24,
            lasts_hours: 4,
        },
        EventKind::ExMessage => Rule {
            weight: 1.0,
            cooldown_hours: 72,
            lasts_hours: 3,
        },
        EventKind::WeatherChange => Rule {
            weight: 4.0,
            cooldown_hours: 8,
            lasts_hours: 4,
        },
        EventKind::PoliceEncounter => Rule {
            weight: 1.0,
            cooldown_hours: 48,
            lasts_hours: 1,
        },
        EventKind::AnimalEncounter => Rule {
            weight: 3.0,
            cooldown_hours: 12,
            lasts_hours: 1,
        },
    }
}

/// The lowest intimacy rung at which `ex_message` can be drawn: a persona
/// does not confide this to someone she barely knows.
const EX_MESSAGE_MIN_RUNG: u8 = 2;

/// How the persona's needs scale a kind's weight; 1 without a rating.
fn factor(kind: EventKind, needs: Option<&Needs>) -> f64 {
    let Some(n) = needs else {
        return 1.0;
    };
    match kind {
        EventKind::TempWork if n.stamina == NeedLevel::Low => 0.5,
        EventKind::ExMessage if n.emotional == NeedLevel::High => 3.0,
        EventKind::AnimalEncounter if n.emotional == NeedLevel::High => 2.0,
        _ => 1.0,
    }
}

/// How long a kind lasts.
pub fn duration(kind: EventKind) -> Duration {
    Duration::hours(rule(kind).lasts_hours)
}

/// The longest cooldown of any kind: how far back a draw reads history.
pub fn longest_cooldown() -> Duration {
    Duration::hours(
        EventKind::ALL
            .into_iter()
            .map(|k| rule(k).cooldown_hours)
            .max()
            .unwrap_or(0),
    )
}

/// An event that already happened (or is under way).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PastEvent {
    pub kind: EventKind,
    pub started_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}

/// What a draw reads.
pub struct DrawInput<'a> {
    pub now: DateTime<Utc>,
    /// The persona's needs carried to now; `None` without a rating.
    pub needs: Option<Needs>,
    pub intimacy_rung: u8,
    /// The instance's events, at least one `longest_cooldown` back.
    pub history: &'a [PastEvent],
    /// The kinds the caller allows.
    pub allowed: &'a [EventKind],
}

/// The kind drawn, or `None` (§4). `roll` is uniform in [0, 1).
pub fn draw(input: &DrawInput, roll: f64) -> Option<EventKind> {
    if input.history.iter().any(|e| e.ends_at > input.now) {
        return None;
    }
    let weighted: Vec<(EventKind, f64)> = EventKind::ALL
        .into_iter()
        .filter(|k| input.allowed.contains(k))
        .filter(|k| *k != EventKind::ExMessage || input.intimacy_rung >= EX_MESSAGE_MIN_RUNG)
        .filter(|k| {
            let cooldown = Duration::hours(rule(*k).cooldown_hours);
            !input
                .history
                .iter()
                .any(|e| e.kind == *k && e.started_at + cooldown > input.now)
        })
        .map(|k| (k, rule(k).weight * factor(k, input.needs.as_ref())))
        .collect();
    let total: f64 = weighted.iter().map(|(_, w)| w).sum();
    if total <= 0.0 {
        return None;
    }
    let mut target = roll.clamp(0.0, 1.0) * total;
    for (kind, weight) in &weighted {
        if target < *weight {
            return Some(*kind);
        }
        target -= weight;
    }
    // A roll of exactly 1.0 (or float residue) lands on the last kind.
    weighted.last().map(|(k, _)| *k)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap()
    }

    fn past(kind: EventKind, started_hours_ago: i64) -> PastEvent {
        let started_at = now() - Duration::hours(started_hours_ago);
        PastEvent {
            kind,
            started_at,
            ends_at: started_at + duration(kind),
        }
    }

    fn input<'a>(
        needs: Option<Needs>,
        rung: u8,
        history: &'a [PastEvent],
        allowed: &'a [EventKind],
    ) -> DrawInput<'a> {
        DrawInput {
            now: now(),
            needs,
            intimacy_rung: rung,
            history,
            allowed,
        }
    }

    fn needs(stamina: NeedLevel, emotional: NeedLevel) -> Option<Needs> {
        Some(Needs {
            hunger: NeedLevel::Mid,
            social: NeedLevel::Mid,
            stamina,
            emotional,
        })
    }

    #[test]
    fn nothing_is_drawn_while_an_event_is_under_way() {
        let h = [past(EventKind::PoliceEncounter, 0)];
        assert_eq!(draw(&input(None, 3, &h, &EventKind::ALL), 0.0), None);
    }

    #[test]
    fn a_kind_is_eligible_again_exactly_one_cooldown_after_it_started() {
        let only = [EventKind::WeatherChange];
        let cooling = [PastEvent {
            started_at: now() - Duration::hours(8) + Duration::seconds(1),
            ..past(EventKind::WeatherChange, 8)
        }];
        assert_eq!(draw(&input(None, 3, &cooling, &only), 0.0), None);
        let ready = [past(EventKind::WeatherChange, 8)];
        assert_eq!(
            draw(&input(None, 3, &ready, &only), 0.0),
            Some(EventKind::WeatherChange)
        );
    }

    #[test]
    fn the_allowlist_bounds_the_draw() {
        assert_eq!(draw(&input(None, 3, &[], &[]), 0.5), None);
        for roll in [0.0, 0.5, 0.99] {
            assert_eq!(
                draw(&input(None, 3, &[], &[EventKind::PoliceEncounter]), roll),
                Some(EventKind::PoliceEncounter)
            );
        }
    }

    #[test]
    fn ex_message_needs_rung_two() {
        let only = [EventKind::ExMessage];
        assert_eq!(draw(&input(None, 1, &[], &only), 0.0), None);
        assert_eq!(
            draw(&input(None, 2, &[], &only), 0.0),
            Some(EventKind::ExMessage)
        );
    }

    #[test]
    fn needs_scale_the_weights() {
        use NeedLevel::{High, Low, Mid};
        // temp_work 3 : weather 4 — roll 0.3 → 2.1 < 3 → temp_work;
        // with low stamina 1.5 : 4 — 0.3 → 1.65 ≥ 1.5 → weather.
        let tw = [EventKind::TempWork, EventKind::WeatherChange];
        assert_eq!(
            draw(&input(None, 3, &[], &tw), 0.3),
            Some(EventKind::TempWork)
        );
        assert_eq!(
            draw(&input(needs(Low, Mid), 3, &[], &tw), 0.3),
            Some(EventKind::WeatherChange)
        );
        // ex 1 : weather 4 — 0.3 → 1.5 ≥ 1 → weather; emotional high 3 : 4 —
        // 0.3 → 2.1 < 3 → ex.
        let ex = [EventKind::ExMessage, EventKind::WeatherChange];
        assert_eq!(
            draw(&input(None, 3, &[], &ex), 0.3),
            Some(EventKind::WeatherChange)
        );
        assert_eq!(
            draw(&input(needs(Mid, High), 3, &[], &ex), 0.3),
            Some(EventKind::ExMessage)
        );
        // animal 3 : weather 4 — 0.5 → 3.5 ≥ 3 → weather; emotional high 6 : 4 —
        // 0.5 → 5 < 6 → animal.
        let an = [EventKind::AnimalEncounter, EventKind::WeatherChange];
        assert_eq!(
            draw(&input(None, 3, &[], &an), 0.5),
            Some(EventKind::WeatherChange)
        );
        assert_eq!(
            draw(&input(needs(Mid, High), 3, &[], &an), 0.5),
            Some(EventKind::AnimalEncounter)
        );
    }

    /// All five, rung 3, no needs: weights 3, 1, 4, 1, 3 (total 12).
    #[test]
    fn the_roll_picks_by_cumulative_weight() {
        let pick = |roll| draw(&input(None, 3, &[], &EventKind::ALL), roll);
        assert_eq!(pick(0.0), Some(EventKind::TempWork));
        assert_eq!(pick(0.2499), Some(EventKind::TempWork));
        assert_eq!(pick(0.25), Some(EventKind::ExMessage));
        assert_eq!(pick(4.0 / 12.0), Some(EventKind::WeatherChange));
        assert_eq!(pick(8.0 / 12.0), Some(EventKind::PoliceEncounter));
        assert_eq!(pick(9.0 / 12.0), Some(EventKind::AnimalEncounter));
        assert_eq!(pick(0.9999), Some(EventKind::AnimalEncounter));
        assert_eq!(pick(1.0), Some(EventKind::AnimalEncounter));
    }

    #[test]
    fn durations_and_windows() {
        let hours = |k| duration(k).num_hours();
        assert_eq!(hours(EventKind::TempWork), 4);
        assert_eq!(hours(EventKind::ExMessage), 3);
        assert_eq!(hours(EventKind::WeatherChange), 4);
        assert_eq!(hours(EventKind::PoliceEncounter), 1);
        assert_eq!(hours(EventKind::AnimalEncounter), 1);
        assert_eq!(longest_cooldown().num_hours(), 72);
    }

    #[test]
    fn kind_wire_values_round_trip() {
        for k in EventKind::ALL {
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
            assert_eq!(EventKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(EventKind::parse("dragon"), None);
    }
}
