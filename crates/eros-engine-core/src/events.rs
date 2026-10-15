// SPDX-License-Identifier: AGPL-3.0-only
//! Life events: what may happen in the persona's day, and which one does
//! (spec 2026-10-15-life-events-v2-design.md §3–§5).

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::needs::{NeedLevel, Needs};

const DAY: i64 = 24;

/// Where a kind sits (§3.1). `Short` and `Mid` are foreground incidents;
/// `Body` and `Trip` are background states that last days.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventCategory {
    Short,
    Mid,
    Body,
    Trip,
}

impl EventCategory {
    /// Every category, in draw order.
    pub const ALL: [EventCategory; 4] = [
        EventCategory::Short,
        EventCategory::Mid,
        EventCategory::Body,
        EventCategory::Trip,
    ];

    /// A background lasts days, and incidents happen inside it.
    pub fn is_background(self) -> bool {
        matches!(self, EventCategory::Body | EventCategory::Trip)
    }
}

/// A category's weight in the first stage, and the hours before any of its
/// kinds can start again.
struct CategoryRule {
    weight: f64,
    cooldown_hours: i64,
}

fn category_rule(category: EventCategory) -> CategoryRule {
    let (weight, cooldown_hours) = match category {
        EventCategory::Short => (5.0, 0),
        EventCategory::Mid => (4.0, 0),
        EventCategory::Body => (0.5, 14 * DAY),
        EventCategory::Trip => (0.5, 7 * DAY),
    };
    CategoryRule {
        weight,
        cooldown_hours,
    }
}

/// What happens to the persona.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    PoliceEncounter,
    AnimalEncounter,
    HitSomeone,
    SuddenDesire,
    TempWork,
    ExMessage,
    WeatherChange,
    GotHit,
    Breakup,
    WorkingOut,
    Reading,
    Gaming,
    AwayFromPhone,
    Unwell,
    Irritable,
    Sick,
    Period,
    Hospitalized,
    PostSurgery,
    BusinessTrip,
    ShortTrip,
    LongTrip,
    HometownVisit,
}

impl EventKind {
    /// Every kind, in draw order.
    pub const ALL: [EventKind; 23] = [
        EventKind::PoliceEncounter,
        EventKind::AnimalEncounter,
        EventKind::HitSomeone,
        EventKind::SuddenDesire,
        EventKind::TempWork,
        EventKind::ExMessage,
        EventKind::WeatherChange,
        EventKind::GotHit,
        EventKind::Breakup,
        EventKind::WorkingOut,
        EventKind::Reading,
        EventKind::Gaming,
        EventKind::AwayFromPhone,
        EventKind::Unwell,
        EventKind::Irritable,
        EventKind::Sick,
        EventKind::Period,
        EventKind::Hospitalized,
        EventKind::PostSurgery,
        EventKind::BusinessTrip,
        EventKind::ShortTrip,
        EventKind::LongTrip,
        EventKind::HometownVisit,
    ];

    /// The stored and wire value.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::PoliceEncounter => "police_encounter",
            EventKind::AnimalEncounter => "animal_encounter",
            EventKind::HitSomeone => "hit_someone",
            EventKind::SuddenDesire => "sudden_desire",
            EventKind::TempWork => "temp_work",
            EventKind::ExMessage => "ex_message",
            EventKind::WeatherChange => "weather_change",
            EventKind::GotHit => "got_hit",
            EventKind::Breakup => "breakup",
            EventKind::WorkingOut => "working_out",
            EventKind::Reading => "reading",
            EventKind::Gaming => "gaming",
            EventKind::AwayFromPhone => "away_from_phone",
            EventKind::Unwell => "unwell",
            EventKind::Irritable => "irritable",
            EventKind::Sick => "sick",
            EventKind::Period => "period",
            EventKind::Hospitalized => "hospitalized",
            EventKind::PostSurgery => "post_surgery",
            EventKind::BusinessTrip => "business_trip",
            EventKind::ShortTrip => "short_trip",
            EventKind::LongTrip => "long_trip",
            EventKind::HometownVisit => "hometown_visit",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    pub fn category(self) -> EventCategory {
        use EventKind::*;
        match self {
            PoliceEncounter | AnimalEncounter | HitSomeone | SuddenDesire => EventCategory::Short,
            TempWork | ExMessage | WeatherChange | GotHit | Breakup | WorkingOut | Reading
            | Gaming | AwayFromPhone | Unwell | Irritable => EventCategory::Mid,
            Sick | Period | Hospitalized | PostSurgery => EventCategory::Body,
            BusinessTrip | ShortTrip | LongTrip | HometownVisit => EventCategory::Trip,
        }
    }

    pub fn is_background(self) -> bool {
        self.category().is_background()
    }
}

/// How long a kind lasts.
#[derive(Clone, Copy)]
enum Lasts {
    /// A fixed length in minutes.
    Minutes(i64),
    /// Whole hours, drawn uniformly in `lo..=hi`.
    Hours(i64, i64),
}

/// One kind's place in the second stage (§3.2): relative weight within its
/// category, hours before it can happen again, how long it lasts.
struct Rule {
    weight: f64,
    cooldown_hours: i64,
    lasts: Lasts,
}

fn rule(kind: EventKind) -> Rule {
    use EventKind::*;
    let (weight, cooldown_hours, lasts) = match kind {
        PoliceEncounter => (1.0, 48, Lasts::Minutes(30)),
        AnimalEncounter => (3.0, 12, Lasts::Hours(1, 1)),
        HitSomeone => (0.5, 14 * DAY, Lasts::Hours(1, 1)),
        SuddenDesire => (2.0, 24, Lasts::Hours(1, 1)),
        TempWork => (3.0, 24, Lasts::Hours(4, 4)),
        ExMessage => (1.0, 72, Lasts::Hours(3, 3)),
        WeatherChange => (4.0, 8, Lasts::Hours(4, 4)),
        GotHit => (0.5, 14 * DAY, Lasts::Hours(4, 4)),
        Breakup => (0.5, 30 * DAY, Lasts::Hours(4, 4)),
        WorkingOut => (2.0, 24, Lasts::Hours(1, 2)),
        Reading => (2.0, 12, Lasts::Hours(1, 3)),
        Gaming => (2.0, 12, Lasts::Hours(2, 4)),
        AwayFromPhone => (1.0, 48, Lasts::Hours(1, 3)),
        Unwell => (1.0, 24, Lasts::Hours(1, 4)),
        Irritable => (1.0, 24, Lasts::Hours(2, 4)),
        Sick => (3.0, 14 * DAY, Lasts::Hours(DAY, 3 * DAY)),
        Period => (3.0, 28 * DAY, Lasts::Hours(3 * DAY, 7 * DAY)),
        Hospitalized => (0.5, 60 * DAY, Lasts::Hours(2 * DAY, 5 * DAY)),
        PostSurgery => (0.5, 90 * DAY, Lasts::Hours(3 * DAY, 7 * DAY)),
        BusinessTrip => (3.0, 0, Lasts::Hours(DAY, DAY)),
        ShortTrip => (2.0, 0, Lasts::Hours(2 * DAY, 2 * DAY)),
        LongTrip => (1.0, 0, Lasts::Hours(3 * DAY, 7 * DAY)),
        HometownVisit => (1.0, 0, Lasts::Hours(3 * DAY, 3 * DAY)),
    };
    Rule {
        weight,
        cooldown_hours,
        lasts,
    }
}

/// The lowest intimacy rung at which she confides an ex or a breakup.
const CONFIDE_MIN_RUNG: u8 = 2;
/// The lowest intimacy rung at which she tells of sudden desire.
const DESIRE_MIN_RUNG: u8 = 3;
/// What a preferred `mid` kind, and a travel-loving persona's `trip`
/// category, weigh relative to the base (§4).
const PREFERENCE_FACTOR: f64 = 3.0;

/// How the persona's body fares (§4): it scales the `body` category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Health {
    Weak,
    #[default]
    Normal,
    Strong,
}

impl Health {
    /// `art_metadata.health`; `None` for any other value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "weak" => Some(Health::Weak),
            "normal" => Some(Health::Normal),
            "strong" => Some(Health::Strong),
            _ => None,
        }
    }
}

/// The persona's settings the draw reads (§4). `Default` is a persona with
/// none of them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PersonaTraits {
    /// `art_metadata.gender` is `"female"`: the gate on `period`.
    pub female: bool,
    pub health: Health,
    pub prefer_trip: bool,
    /// Weighs only `mid` kinds; any other kind listed has no effect.
    pub prefer_events: Vec<EventKind>,
}

fn passes_gate(kind: EventKind, rung: u8, persona: &PersonaTraits) -> bool {
    match kind {
        EventKind::ExMessage | EventKind::Breakup => rung >= CONFIDE_MIN_RUNG,
        EventKind::SuddenDesire => rung >= DESIRE_MIN_RUNG,
        EventKind::Period => persona.female,
        _ => true,
    }
}

/// How the persona's needs scale a kind within its category; 1 without a
/// rating.
fn needs_factor(kind: EventKind, needs: Option<&Needs>) -> f64 {
    use EventKind::*;
    let Some(n) = needs else {
        return 1.0;
    };
    match kind {
        TempWork | WorkingOut if n.stamina == NeedLevel::Low => 0.5,
        Unwell | Sick if n.stamina == NeedLevel::Low => 2.0,
        ExMessage if n.emotional == NeedLevel::High => 3.0,
        AnimalEncounter | Irritable if n.emotional == NeedLevel::High => 2.0,
        _ => 1.0,
    }
}

/// How the persona's settings scale a category in the first stage.
fn category_factor(category: EventCategory, persona: &PersonaTraits) -> f64 {
    match category {
        EventCategory::Body => match persona.health {
            Health::Weak => 2.0,
            Health::Normal => 1.0,
            Health::Strong => 0.5,
        },
        EventCategory::Trip if persona.prefer_trip => PREFERENCE_FACTOR,
        _ => 1.0,
    }
}

/// How `prefer_events` scales a kind in the second stage: `mid` kinds only.
fn preference_factor(kind: EventKind, persona: &PersonaTraits) -> f64 {
    if kind.category() == EventCategory::Mid && persona.prefer_events.contains(&kind) {
        PREFERENCE_FACTOR
    } else {
        1.0
    }
}

/// Whether a background rules a foreground kind out (§3.3): in hospital or
/// recovering from surgery, she is not out and about.
fn rules_out(background: EventKind, kind: EventKind) -> bool {
    use EventKind::*;
    matches!(background, Hospitalized | PostSurgery)
        && matches!(
            kind,
            TempWork | PoliceEncounter | AnimalEncounter | HitSomeone | GotHit | WorkingOut
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
    pub persona: &'a PersonaTraits,
    /// The instance's events, at least `lookback()` back.
    pub history: &'a [PastEvent],
    /// The kinds the caller allows.
    pub allowed: &'a [EventKind],
}

/// Three uniform draws in [0, 1): one per stage, one for the length.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rolls {
    pub category: f64,
    pub kind: f64,
    pub duration: f64,
}

/// The kind drawn and how long it lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Drawn {
    pub kind: EventKind,
    pub lasts: Duration,
}

/// The kind drawn, or `None` (§5).
pub fn draw(input: &DrawInput, rolls: Rolls) -> Option<Drawn> {
    let under_way = |e: &&PastEvent| e.ends_at > input.now;
    if input
        .history
        .iter()
        .filter(under_way)
        .any(|e| !e.kind.is_background())
    {
        return None;
    }
    let background = input
        .history
        .iter()
        .filter(under_way)
        .find(|e| e.kind.is_background())
        .map(|e| e.kind);
    let cooling =
        |started_at: DateTime<Utc>, hours: i64| started_at + Duration::hours(hours) > input.now;
    let eligible: Vec<EventKind> = EventKind::ALL
        .into_iter()
        .filter(|k| input.allowed.contains(k))
        .filter(|k| passes_gate(*k, input.intimacy_rung, input.persona))
        .filter(|k| {
            !input
                .history
                .iter()
                .any(|e| e.kind == *k && cooling(e.started_at, rule(*k).cooldown_hours))
        })
        .filter(|k| {
            let c = k.category();
            !input.history.iter().any(|e| {
                e.kind.category() == c && cooling(e.started_at, category_rule(c).cooldown_hours)
            })
        })
        .filter(|k| match background {
            Some(b) => !k.is_background() && !rules_out(b, *k),
            None => true,
        })
        .collect();
    let categories: Vec<(EventCategory, f64)> = EventCategory::ALL
        .into_iter()
        .filter(|c| eligible.iter().any(|k| k.category() == *c))
        .map(|c| {
            (
                c,
                category_rule(c).weight * category_factor(c, input.persona),
            )
        })
        .collect();
    let category = pick(&categories, rolls.category)?;
    let kinds: Vec<(EventKind, f64)> = eligible
        .into_iter()
        .filter(|k| k.category() == category)
        .map(|k| {
            let weight = rule(k).weight
                * needs_factor(k, input.needs.as_ref())
                * preference_factor(k, input.persona);
            (k, weight)
        })
        .collect();
    let kind = pick(&kinds, rolls.kind)?;
    Some(Drawn {
        kind,
        lasts: lasts(kind, rolls.duration),
    })
}

/// One entry by cumulative weight; a roll of exactly 1.0 (or float residue)
/// lands on the last entry.
fn pick<T: Copy>(weighted: &[(T, f64)], roll: f64) -> Option<T> {
    let total: f64 = weighted.iter().map(|(_, w)| w).sum();
    if total <= 0.0 {
        return None;
    }
    let mut target = roll.clamp(0.0, 1.0) * total;
    for (item, weight) in weighted {
        if target < *weight {
            return Some(*item);
        }
        target -= weight;
    }
    weighted.last().map(|(item, _)| *item)
}

fn lasts(kind: EventKind, roll: f64) -> Duration {
    match rule(kind).lasts {
        Lasts::Minutes(m) => Duration::minutes(m),
        Lasts::Hours(lo, hi) => {
            let span = (hi - lo + 1) as f64;
            let step = ((roll.clamp(0.0, 1.0) * span).floor() as i64).min(hi - lo);
            Duration::hours(lo + step)
        }
    }
}

fn max_hours(lasts: Lasts) -> i64 {
    match lasts {
        Lasts::Minutes(m) => (m + 59) / 60,
        Lasts::Hours(_, hi) => hi,
    }
}

/// How far back a draw, and every under-way read, looks: the longest of all
/// cooldowns and all maximum durations.
pub fn lookback() -> Duration {
    let kinds = EventKind::ALL.into_iter().map(|k| {
        let r = rule(k);
        r.cooldown_hours.max(max_hours(r.lasts))
    });
    let categories = EventCategory::ALL
        .into_iter()
        .map(|c| category_rule(c).cooldown_hours);
    Duration::hours(kinds.chain(categories).max().unwrap_or(0))
}

/// The kinds whose category is a background, in draw order.
pub fn background_kinds() -> Vec<EventKind> {
    EventKind::ALL
        .into_iter()
        .filter(|k| k.is_background())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::EventKind::*;
    use super::*;
    use chrono::TimeZone;

    const E: f64 = 1e-9;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap()
    }

    fn past(kind: EventKind, started_hours_ago: i64, lasts_hours: i64) -> PastEvent {
        let started_at = now() - Duration::hours(started_hours_ago);
        PastEvent {
            kind,
            started_at,
            ends_at: started_at + Duration::hours(lasts_hours),
        }
    }

    fn rolls(category: f64, kind: f64) -> Rolls {
        Rolls {
            category,
            kind,
            duration: 0.0,
        }
    }

    fn female() -> PersonaTraits {
        PersonaTraits {
            female: true,
            ..Default::default()
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

    fn pick_with(
        persona: &PersonaTraits,
        needs: Option<Needs>,
        rung: u8,
        history: &[PastEvent],
        allowed: &[EventKind],
        r: Rolls,
    ) -> Option<EventKind> {
        draw(
            &DrawInput {
                now: now(),
                needs,
                intimacy_rung: rung,
                persona,
                history,
                allowed,
            },
            r,
        )
        .map(|d| d.kind)
    }

    /// A female persona at rung 3 without needs: every gate open.
    fn pick(history: &[PastEvent], allowed: &[EventKind], r: Rolls) -> Option<EventKind> {
        pick_with(&female(), None, 3, history, allowed, r)
    }

    #[test]
    fn nothing_is_drawn_while_a_foreground_event_is_under_way() {
        let h = [past(WeatherChange, 1, 4)];
        assert_eq!(pick(&h, &EventKind::ALL, rolls(0.99, 0.5)), None);
        assert_eq!(pick(&h, &EventKind::ALL, rolls(0.0, 0.0)), None);
    }

    #[test]
    fn a_background_keeps_backgrounds_out_and_lets_incidents_in() {
        let h = [past(LongTrip, 24, 96)];
        for i in 0..=20 {
            let r = i as f64 / 20.0;
            let k = pick(&h, &EventKind::ALL, rolls(r, 0.5)).expect("an incident");
            assert!(!k.is_background(), "{k:?} at {r}");
        }
        assert_eq!(pick(&h, &[Sick, BusinessTrip], rolls(0.5, 0.5)), None);
    }

    #[test]
    fn hospital_and_surgery_rule_out_the_outdoor_incidents() {
        let outdoor = [
            TempWork,
            PoliceEncounter,
            AnimalEncounter,
            HitSomeone,
            GotHit,
            WorkingOut,
        ];
        for bg in [Hospitalized, PostSurgery] {
            let h = [past(bg, 1, 72)];
            assert_eq!(pick(&h, &outdoor, rolls(0.0, 0.0)), None, "{bg:?}");
            assert_eq!(pick(&h, &[Reading], rolls(0.0, 0.0)), Some(Reading));
        }
        for bg in [
            Sick,
            Period,
            BusinessTrip,
            ShortTrip,
            LongTrip,
            HometownVisit,
        ] {
            let h = [past(bg, 1, 48)];
            for k in outdoor {
                assert_eq!(pick(&h, &[k], rolls(0.0, 0.0)), Some(k), "{bg:?} {k:?}");
            }
        }
    }

    #[test]
    fn a_kind_is_eligible_again_exactly_one_cooldown_after_it_started() {
        let only = [WeatherChange];
        let cooling = [PastEvent {
            started_at: now() - Duration::hours(8) + Duration::seconds(1),
            ..past(WeatherChange, 8, 4)
        }];
        assert_eq!(pick(&cooling, &only, rolls(0.0, 0.0)), None);
        let ready = [past(WeatherChange, 8, 4)];
        assert_eq!(pick(&ready, &only, rolls(0.0, 0.0)), Some(WeatherChange));
    }

    #[test]
    fn a_trip_starts_at_most_once_a_week_across_its_kinds() {
        let week = Duration::hours(7 * 24);
        let trip = |started_at: DateTime<Utc>| PastEvent {
            kind: BusinessTrip,
            started_at,
            ends_at: started_at + Duration::hours(24),
        };
        let only = [ShortTrip];
        let cooling = [trip(now() - week + Duration::seconds(1))];
        assert_eq!(pick(&cooling, &only, rolls(0.0, 0.0)), None);
        let ready = [trip(now() - week)];
        assert_eq!(pick(&ready, &only, rolls(0.0, 0.0)), Some(ShortTrip));
    }

    #[test]
    fn a_body_state_starts_at_most_once_a_fortnight() {
        let fortnight = Duration::hours(14 * 24);
        let sick = |started_at: DateTime<Utc>| PastEvent {
            kind: Sick,
            started_at,
            ends_at: started_at + Duration::hours(24),
        };
        let only = [Hospitalized];
        let cooling = [sick(now() - fortnight + Duration::seconds(1))];
        assert_eq!(pick(&cooling, &only, rolls(0.0, 0.0)), None);
        let ready = [sick(now() - fortnight)];
        assert_eq!(pick(&ready, &only, rolls(0.0, 0.0)), Some(Hospitalized));
    }

    #[test]
    fn the_gates() {
        let none = PersonaTraits::default();
        for k in [ExMessage, Breakup] {
            assert_eq!(pick_with(&none, None, 1, &[], &[k], rolls(0.0, 0.0)), None);
            assert_eq!(
                pick_with(&none, None, 2, &[], &[k], rolls(0.0, 0.0)),
                Some(k)
            );
        }
        let desire = [SuddenDesire];
        assert_eq!(
            pick_with(&none, None, 2, &[], &desire, rolls(0.0, 0.0)),
            None
        );
        assert_eq!(
            pick_with(&none, None, 3, &[], &desire, rolls(0.0, 0.0)),
            Some(SuddenDesire)
        );
        let period = [Period];
        assert_eq!(
            pick_with(&none, None, 3, &[], &period, rolls(0.0, 0.0)),
            None
        );
        assert_eq!(
            pick_with(&female(), None, 3, &[], &period, rolls(0.0, 0.0)),
            Some(Period)
        );
    }

    #[test]
    fn a_category_without_an_eligible_kind_takes_no_part() {
        // A category roll of 0 would land on `short` if it took part.
        assert_eq!(pick(&[], &[Reading], rolls(0.0, 0.0)), Some(Reading));
        let none = PersonaTraits::default();
        assert_eq!(
            pick_with(
                &none,
                None,
                2,
                &[],
                &[SuddenDesire, Reading],
                rolls(0.0, 0.0)
            ),
            Some(Reading)
        );
    }

    #[test]
    fn health_scales_the_body_category() {
        let allowed = [Reading, Sick];
        let with = |health| PersonaTraits {
            health,
            ..Default::default()
        };
        // mid 4 : body 0.5 (4.5) — 0.85 → 3.825 < 4 → mid; 0.92 → 4.14 → body.
        assert_eq!(
            pick_with(
                &with(Health::Normal),
                None,
                3,
                &[],
                &allowed,
                rolls(0.85, 0.0)
            ),
            Some(Reading)
        );
        assert_eq!(
            pick_with(
                &with(Health::Normal),
                None,
                3,
                &[],
                &allowed,
                rolls(0.92, 0.0)
            ),
            Some(Sick)
        );
        // weak: body 1 (5) — 0.85 → 4.25 → body.
        assert_eq!(
            pick_with(
                &with(Health::Weak),
                None,
                3,
                &[],
                &allowed,
                rolls(0.85, 0.0)
            ),
            Some(Sick)
        );
        // strong: body 0.25 (4.25) — 0.92 → 3.91 → mid.
        assert_eq!(
            pick_with(
                &with(Health::Strong),
                None,
                3,
                &[],
                &allowed,
                rolls(0.92, 0.0)
            ),
            Some(Reading)
        );
    }

    #[test]
    fn prefer_trip_scales_the_trip_category() {
        let allowed = [Reading, BusinessTrip];
        // mid 4 : trip 0.5 — 0.85 → 3.825 → mid; trip ×3 = 1.5 (5.5) — 4.675 → trip.
        assert_eq!(
            pick_with(
                &PersonaTraits::default(),
                None,
                3,
                &[],
                &allowed,
                rolls(0.85, 0.0)
            ),
            Some(Reading)
        );
        let traveller = PersonaTraits {
            prefer_trip: true,
            ..Default::default()
        };
        assert_eq!(
            pick_with(&traveller, None, 3, &[], &allowed, rolls(0.85, 0.0)),
            Some(BusinessTrip)
        );
    }

    #[test]
    fn prefer_events_scales_mid_kinds_only() {
        let allowed = [Reading, Gaming];
        // reading 2 : gaming 2 — kind roll 0.4 → 1.6 → reading.
        assert_eq!(pick(&[], &allowed, rolls(0.0, 0.4)), Some(Reading));
        // gaming ×3: 2 : 6 — 0.4 → 3.2 ≥ 2 → gaming.
        let gamer = PersonaTraits {
            prefer_events: vec![Gaming],
            ..female()
        };
        assert_eq!(
            pick_with(&gamer, None, 3, &[], &allowed, rolls(0.0, 0.4)),
            Some(Gaming)
        );
        // A body kind in the list weighs nothing: sick 3 : period 3 — 0.6 → period.
        let listed_sick = PersonaTraits {
            prefer_events: vec![Sick],
            ..female()
        };
        assert_eq!(
            pick_with(&listed_sick, None, 3, &[], &[Sick, Period], rolls(0.0, 0.6)),
            Some(Period)
        );
    }

    #[test]
    fn needs_scale_kinds_within_their_category() {
        use NeedLevel::{High, Low, Mid};
        let cases: [(&[EventKind], Option<Needs>, f64, EventKind); 14] = [
            // temp_work 3 : weather 4 — 0.3 → 2.1 → temp_work.
            (&[TempWork, WeatherChange], None, 0.3, TempWork),
            // stamina low: 1.5 : 4 — 0.3 → 1.65 → weather.
            (
                &[TempWork, WeatherChange],
                needs(Low, Mid),
                0.3,
                WeatherChange,
            ),
            // ex 1 : weather 4 — 0.3 → 1.5 → weather.
            (&[ExMessage, WeatherChange], None, 0.3, WeatherChange),
            // emotional high: 3 : 4 — 0.3 → 2.1 → ex.
            (
                &[ExMessage, WeatherChange],
                needs(Mid, High),
                0.3,
                ExMessage,
            ),
            // police 1 : animal 3 — 0.2 → 0.8 → police.
            (
                &[PoliceEncounter, AnimalEncounter],
                None,
                0.2,
                PoliceEncounter,
            ),
            // emotional high: 1 : 6 — 0.2 → 1.4 → animal.
            (
                &[PoliceEncounter, AnimalEncounter],
                needs(Mid, High),
                0.2,
                AnimalEncounter,
            ),
            // working_out 2 : reading 2 — 0.4 → 1.6 → working_out.
            (&[WorkingOut, Reading], None, 0.4, WorkingOut),
            // stamina low: 1 : 2 — 0.4 → 1.2 → reading.
            (&[WorkingOut, Reading], needs(Low, Mid), 0.4, Reading),
            // unwell 1 : irritable 1 — 0.6 → 1.2 → irritable.
            (&[Unwell, Irritable], None, 0.6, Irritable),
            // stamina low: 2 : 1 — 0.6 → 1.8 → unwell.
            (&[Unwell, Irritable], needs(Low, Mid), 0.6, Unwell),
            // 1 : 1 — 0.4 → 0.8 → unwell.
            (&[Unwell, Irritable], None, 0.4, Unwell),
            // emotional high: 1 : 2 — 0.4 → 1.2 → irritable.
            (&[Unwell, Irritable], needs(Mid, High), 0.4, Irritable),
            // sick 3 : hospitalized 0.5 — 0.9 → 3.15 → hospitalized.
            (&[Sick, Hospitalized], None, 0.9, Hospitalized),
            // stamina low: 6 : 0.5 — 0.9 → 5.85 → sick.
            (&[Sick, Hospitalized], needs(Low, Mid), 0.9, Sick),
        ];
        for (allowed, n, roll, want) in cases {
            assert_eq!(
                pick_with(&female(), n, 3, &[], allowed, rolls(0.0, roll)),
                Some(want),
                "{allowed:?} {n:?} {roll}"
            );
        }
    }

    /// Every kind, rung 3, a female persona, no needs: categories short 5,
    /// mid 4, body 0.5, trip 0.5 (10); within short police 1, animal 3,
    /// hit 0.5, desire 2 (6.5).
    #[test]
    fn the_stages_pick_by_cumulative_weight() {
        let at = |c: f64, k: f64| pick(&[], &EventKind::ALL, rolls(c, k)).unwrap();
        assert_eq!(at(0.0, 0.0), PoliceEncounter);
        assert_eq!(at(0.5 - E, 0.0).category(), EventCategory::Short);
        assert_eq!(at(0.5 + E, 0.0), TempWork);
        assert_eq!(at(0.9 - E, 0.0).category(), EventCategory::Mid);
        assert_eq!(at(0.9 + E, 0.0), Sick);
        assert_eq!(at(0.95 - E, 0.0).category(), EventCategory::Body);
        assert_eq!(at(0.95 + E, 0.0), BusinessTrip);
        assert_eq!(at(1.0, 1.0), HometownVisit);
        let b = |w: f64| w / 6.5;
        assert_eq!(at(0.0, b(1.0) - E), PoliceEncounter);
        assert_eq!(at(0.0, b(1.0) + E), AnimalEncounter);
        assert_eq!(at(0.0, b(4.0) - E), AnimalEncounter);
        assert_eq!(at(0.0, b(4.0) + E), HitSomeone);
        assert_eq!(at(0.0, b(4.5) - E), HitSomeone);
        assert_eq!(at(0.0, b(4.5) + E), SuddenDesire);
        assert_eq!(at(0.0, 1.0), SuddenDesire);
    }

    #[test]
    fn fixed_and_ranged_durations() {
        let lasts = |kind: EventKind, duration: f64| {
            draw(
                &DrawInput {
                    now: now(),
                    needs: None,
                    intimacy_rung: 3,
                    persona: &female(),
                    history: &[],
                    allowed: &[kind],
                },
                Rolls {
                    category: 0.0,
                    kind: 0.0,
                    duration,
                },
            )
            .unwrap()
            .lasts
        };
        assert_eq!(lasts(PoliceEncounter, 0.0), Duration::minutes(30));
        assert_eq!(lasts(PoliceEncounter, 0.99), Duration::minutes(30));
        assert_eq!(lasts(TempWork, 0.7), Duration::hours(4));
        assert_eq!(lasts(WorkingOut, 0.49), Duration::hours(1));
        assert_eq!(lasts(WorkingOut, 0.5), Duration::hours(2));
        assert_eq!(lasts(BusinessTrip, 0.5), Duration::hours(24));
        assert_eq!(lasts(LongTrip, 0.0), Duration::hours(72));
        assert_eq!(lasts(LongTrip, 1.0 - E), Duration::hours(168));
        assert_eq!(lasts(LongTrip, 1.0), Duration::hours(168));
    }

    #[test]
    fn lookback_and_background_kinds() {
        assert_eq!(lookback(), Duration::hours(90 * 24));
        assert_eq!(
            background_kinds(),
            vec![
                Sick,
                Period,
                Hospitalized,
                PostSurgery,
                BusinessTrip,
                ShortTrip,
                LongTrip,
                HometownVisit
            ]
        );
    }

    #[test]
    fn every_category_holds_its_kinds() {
        let count = |c| EventKind::ALL.iter().filter(|k| k.category() == c).count();
        assert_eq!(
            EventCategory::ALL.map(count),
            [4, 11, 4, 4],
            "short, mid, body, trip"
        );
        assert!(!EventCategory::Short.is_background());
        assert!(!EventCategory::Mid.is_background());
        assert!(EventCategory::Body.is_background());
        assert!(EventCategory::Trip.is_background());
    }

    #[test]
    fn kind_wire_values_round_trip() {
        for k in EventKind::ALL {
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
            assert_eq!(EventKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(EventKind::parse("dragon"), None);
    }

    #[test]
    fn health_parses_its_three_values() {
        assert_eq!(Health::parse("weak"), Some(Health::Weak));
        assert_eq!(Health::parse("normal"), Some(Health::Normal));
        assert_eq!(Health::parse("strong"), Some(Health::Strong));
        assert_eq!(Health::parse("fragile"), None);
        assert_eq!(Health::default(), Health::Normal);
    }
}
