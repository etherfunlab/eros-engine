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
    GroceryShopping,
    Showering,
    Bathing,
    InClass,
    OnTheWay,
    PeriodScare,
    PregnancyScare,
    GotBumped,
    BumpedSomeone,
    Quarrel,
    ScoldedBySuperior,
    ScoldedSuperior,
    SawBag,
    SawClothes,
    AdmiringScenery,
    AdmiringArt,
    TakingSelfie,
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
    Busy,
    InBattle,
    Asleep,
    OutShopping,
    BrosGathering,
    SisGathering,
    OnTheRoad,
    WatchingMatch,
    WatchingShow,
    HavingSex,
    WantToGoOut,
    WantToGoHome,
    Sick,
    Period,
    Hospitalized,
    PostSurgery,
    BusinessTrip,
    ShortTrip,
    LongTrip,
    HometownVisit,
    TakingSpace,
    FellOut,
}

impl EventKind {
    /// Every drawable kind, in draw order.
    pub const ALL: [EventKind; 52] = [
        EventKind::PoliceEncounter,
        EventKind::AnimalEncounter,
        EventKind::HitSomeone,
        EventKind::SuddenDesire,
        EventKind::GroceryShopping,
        EventKind::Showering,
        EventKind::Bathing,
        EventKind::InClass,
        EventKind::OnTheWay,
        EventKind::PeriodScare,
        EventKind::PregnancyScare,
        EventKind::GotBumped,
        EventKind::BumpedSomeone,
        EventKind::Quarrel,
        EventKind::ScoldedBySuperior,
        EventKind::ScoldedSuperior,
        EventKind::SawBag,
        EventKind::SawClothes,
        EventKind::AdmiringScenery,
        EventKind::AdmiringArt,
        EventKind::TakingSelfie,
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
        EventKind::Busy,
        EventKind::InBattle,
        EventKind::Asleep,
        EventKind::OutShopping,
        EventKind::BrosGathering,
        EventKind::SisGathering,
        EventKind::OnTheRoad,
        EventKind::WatchingMatch,
        EventKind::WatchingShow,
        EventKind::HavingSex,
        EventKind::WantToGoOut,
        EventKind::WantToGoHome,
        EventKind::Sick,
        EventKind::Period,
        EventKind::Hospitalized,
        EventKind::PostSurgery,
        EventKind::BusinessTrip,
        EventKind::ShortTrip,
        EventKind::LongTrip,
        EventKind::HometownVisit,
    ];

    /// Kinds the writers place directly, never drawn: parsed and stored, not in `ALL`.
    pub const SPECIAL: [EventKind; 2] = [EventKind::TakingSpace, EventKind::FellOut];

    /// The stored and wire value.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::PoliceEncounter => "police_encounter",
            EventKind::AnimalEncounter => "animal_encounter",
            EventKind::HitSomeone => "hit_someone",
            EventKind::SuddenDesire => "sudden_desire",
            EventKind::GroceryShopping => "grocery_shopping",
            EventKind::Showering => "showering",
            EventKind::Bathing => "bathing",
            EventKind::InClass => "in_class",
            EventKind::OnTheWay => "on_the_way",
            EventKind::PeriodScare => "period_scare",
            EventKind::PregnancyScare => "pregnancy_scare",
            EventKind::GotBumped => "got_bumped",
            EventKind::BumpedSomeone => "bumped_someone",
            EventKind::Quarrel => "quarrel",
            EventKind::ScoldedBySuperior => "scolded_by_superior",
            EventKind::ScoldedSuperior => "scolded_superior",
            EventKind::SawBag => "saw_bag",
            EventKind::SawClothes => "saw_clothes",
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
            EventKind::Busy => "busy",
            EventKind::InBattle => "in_battle",
            EventKind::Asleep => "asleep",
            EventKind::OutShopping => "out_shopping",
            EventKind::BrosGathering => "bros_gathering",
            EventKind::SisGathering => "sis_gathering",
            EventKind::OnTheRoad => "on_the_road",
            EventKind::WatchingMatch => "watching_match",
            EventKind::WatchingShow => "watching_show",
            EventKind::HavingSex => "having_sex",
            EventKind::WantToGoOut => "want_to_go_out",
            EventKind::WantToGoHome => "want_to_go_home",
            EventKind::AdmiringScenery => "admiring_scenery",
            EventKind::AdmiringArt => "admiring_art",
            EventKind::TakingSelfie => "taking_selfie",
            EventKind::Sick => "sick",
            EventKind::Period => "period",
            EventKind::Hospitalized => "hospitalized",
            EventKind::PostSurgery => "post_surgery",
            EventKind::BusinessTrip => "business_trip",
            EventKind::ShortTrip => "short_trip",
            EventKind::LongTrip => "long_trip",
            EventKind::HometownVisit => "hometown_visit",
            EventKind::TakingSpace => "taking_space",
            EventKind::FellOut => "fell_out",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .chain(Self::SPECIAL)
            .find(|k| k.as_str() == s)
    }

    pub fn category(self) -> EventCategory {
        use EventKind::*;
        match self {
            PoliceEncounter | AnimalEncounter | HitSomeone | SuddenDesire | GroceryShopping
            | Showering | Bathing | InClass | OnTheWay | PeriodScare | PregnancyScare
            | GotBumped | BumpedSomeone | Quarrel | ScoldedBySuperior | ScoldedSuperior
            | SawBag | SawClothes | AdmiringScenery | AdmiringArt | TakingSelfie => {
                EventCategory::Short
            }
            TempWork | ExMessage | WeatherChange | GotHit | Breakup | WorkingOut | Reading
            | Gaming | AwayFromPhone | Unwell | Irritable | Busy | InBattle | Asleep
            | OutShopping | BrosGathering | SisGathering | OnTheRoad | WatchingMatch
            | WatchingShow | HavingSex | WantToGoOut | WantToGoHome | TakingSpace | FellOut => {
                EventCategory::Mid
            }
            Sick | Period | Hospitalized | PostSurgery => EventCategory::Body,
            BusinessTrip | ShortTrip | LongTrip | HometownVisit => EventCategory::Trip,
        }
    }

    pub fn is_background(self) -> bool {
        self.category().is_background()
    }

    /// A kind whose message can carry her picture.
    pub fn is_photo(self) -> bool {
        self == EventKind::TakingSelfie
    }

    /// Offline kinds make the persona unreachable while they last
    /// (spec 2026-10-16-life-events-offline-and-scares-design.md §2). Whether
    /// `out_shopping` does depends on the persona (spec 2026-10-19-life-events-
    /// shopping-gatherings-and-ask-for-space-design.md §2.1): a female persona
    /// goes dark, any other keeps replying.
    pub fn is_offline(self, persona: &PersonaTraits) -> bool {
        match self {
            EventKind::OutShopping => persona.female,
            k => k.may_be_offline(),
        }
    }

    /// Whether the kind takes any persona offline; `is_offline` narrows it to
    /// one persona. For a reader with no persona in hand, such as a query over
    /// stored events.
    pub fn may_be_offline(self) -> bool {
        use EventKind::*;
        matches!(
            self,
            AwayFromPhone
                | Busy
                | InBattle
                | Asleep
                | OutShopping
                | BrosGathering
                | SisGathering
                | HavingSex
                | TakingSpace
        )
    }

    /// A scare that turns out to be nothing once it ends (spec
    /// 2026-10-16-life-events-offline-and-scares-design.md §5).
    pub fn is_false_alarm(self) -> bool {
        matches!(self, EventKind::PeriodScare | EventKind::PregnancyScare)
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
        GroceryShopping => (2.0, 24, Lasts::Hours(1, 1)),
        Showering => (2.0, 12, Lasts::Minutes(30)),
        Bathing => (1.0, 48, Lasts::Hours(1, 1)),
        InClass => (1.0, 24, Lasts::Hours(1, 1)),
        OnTheWay => (2.0, 12, Lasts::Minutes(30)),
        PeriodScare => (0.5, 28 * DAY, Lasts::Hours(1, 1)),
        PregnancyScare => (0.5, 60 * DAY, Lasts::Hours(1, 1)),
        GotBumped => (1.0, 72, Lasts::Minutes(30)),
        BumpedSomeone => (0.5, 7 * DAY, Lasts::Minutes(30)),
        Quarrel => (1.0, 48, Lasts::Hours(1, 1)),
        ScoldedBySuperior => (1.0, 72, Lasts::Hours(1, 1)),
        ScoldedSuperior => (0.5, 14 * DAY, Lasts::Hours(1, 1)),
        SawBag => (2.0, 48, Lasts::Hours(1, 1)),
        SawClothes => (2.0, 48, Lasts::Hours(1, 1)),
        AdmiringScenery => (2.0, 24, Lasts::Hours(1, 1)),
        AdmiringArt => (1.0, 48, Lasts::Hours(1, 1)),
        TakingSelfie => (2.0, 24, Lasts::Minutes(30)),
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
        Busy => (2.0, 12, Lasts::Hours(1, 3)),
        InBattle => (1.0, 48, Lasts::Hours(2, 4)),
        Asleep => (2.0, 24, Lasts::Hours(1, 4)),
        OutShopping => (2.0, 24, Lasts::Hours(2, 4)),
        BrosGathering => (1.0, 72, Lasts::Hours(2, 4)),
        SisGathering => (1.0, 72, Lasts::Hours(2, 4)),
        OnTheRoad => (2.0, 24, Lasts::Hours(2, 4)),
        WatchingMatch => (2.0, 24, Lasts::Hours(2, 3)),
        WatchingShow => (1.0, 72, Lasts::Hours(2, 3)),
        HavingSex => (1.0, 48, Lasts::Hours(1, 2)),
        WantToGoOut => (1.0, 24, Lasts::Hours(1, 3)),
        WantToGoHome => (1.0, 24, Lasts::Hours(1, 3)),
        Sick => (3.0, 14 * DAY, Lasts::Hours(DAY, 3 * DAY)),
        Period => (3.0, 28 * DAY, Lasts::Hours(3 * DAY, 7 * DAY)),
        Hospitalized => (0.5, 60 * DAY, Lasts::Hours(2 * DAY, 5 * DAY)),
        PostSurgery => (0.5, 90 * DAY, Lasts::Hours(3 * DAY, 7 * DAY)),
        BusinessTrip => (3.0, 0, Lasts::Hours(DAY, DAY)),
        ShortTrip => (2.0, 0, Lasts::Hours(2 * DAY, 2 * DAY)),
        LongTrip => (1.0, 0, Lasts::Hours(3 * DAY, 7 * DAY)),
        HometownVisit => (1.0, 0, Lasts::Hours(3 * DAY, 3 * DAY)),
        TakingSpace => (0.0, 0, Lasts::Hours(12, 24)),
        FellOut => (0.0, 0, Lasts::Hours(1, 1)),
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
/// What a kind weighs while an event that leads to it is live (spec
/// 2026-10-25-life-events-follow-ons-and-persona-keys-design.md §2.1), per
/// live prior.
const FOLLOW_FACTOR: f64 = 3.0;
/// How long after it ended a prior stays live.
const FOLLOW_WINDOW_HOURS: i64 = 48;
/// What a kind weighs while what leads to it in life has never happened
/// (spec §3, §4): rare, not impossible.
const UNLIKELY_FACTOR: f64 = 0.2;
/// Hours after any offline kind started before any offline kind can start
/// again (spec 2026-10-16-life-events-offline-and-scares-design.md §3.4): it
/// bounds how often she goes silent.
const OFFLINE_COOLDOWN_HOURS: i64 = 12;

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
    /// `art_metadata.gender` is `"female"`: the gate on `period` and the
    /// shopping finds, and what takes `out_shopping` offline.
    pub female: bool,
    pub health: Health,
    pub prefer_trip: bool,
    /// Weighs only `mid` kinds; any other kind listed has no effect.
    pub prefer_events: Vec<EventKind>,
    /// `art_metadata.is_virgin`. Her virginity as the draw sees it is this
    /// and no `having_sex` row ever (`DrawInput::ever_had_sex`); the first
    /// row ends it with no write.
    pub is_virgin: bool,
    /// `art_metadata.homesick`: `true`, her home is her base; `false`, she
    /// roams; `None`, no preference. Steers where a trip goes (§5).
    pub homesick: Option<bool>,
}

fn passes_gate(kind: EventKind, rung: u8, persona: &PersonaTraits) -> bool {
    match kind {
        EventKind::ExMessage | EventKind::Breakup => rung >= CONFIDE_MIN_RUNG,
        EventKind::SuddenDesire => rung >= DESIRE_MIN_RUNG,
        EventKind::Period | EventKind::SawBag | EventKind::SawClothes => persona.female,
        EventKind::PeriodScare => persona.female && rung >= CONFIDE_MIN_RUNG,
        EventKind::PregnancyScare => persona.female && rung >= DESIRE_MIN_RUNG,
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
        Unwell | Sick | Bathing | Asleep if n.stamina == NeedLevel::Low => 2.0,
        GroceryShopping if n.hunger == NeedLevel::High => 2.0,
        ExMessage if n.emotional == NeedLevel::High => 3.0,
        AnimalEncounter | Irritable | Quarrel if n.emotional == NeedLevel::High => 2.0,
        WantToGoOut if n.outing == NeedLevel::High => 3.0,
        WantToGoHome if n.outing == NeedLevel::Low => 3.0,
        _ => 1.0,
    }
}

/// How the urge to let loose scales a kind within its category (spec
/// 2026-10-28-outing-indulgence-needs-and-habits-design.md §5.2): it sends
/// her out and frays her temper. Its own factor, so it multiplies with
/// `needs_factor`. 1 without a rating.
fn indulgence_factor(kind: EventKind, needs: Option<&Needs>) -> f64 {
    match (kind, needs) {
        (EventKind::WantToGoOut | EventKind::Irritable, Some(n))
            if n.indulgence == NeedLevel::High =>
        {
            2.0
        }
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

/// Whether `prior` makes `kind` likelier while it is live (spec §2.2).
fn follows(prior: EventKind, kind: EventKind) -> bool {
    use EventKind::*;
    matches!(
        (prior, kind),
        (ExMessage | SuddenDesire, HavingSex)
            | (Sick, PeriodScare)
            | (HavingSex, PregnancyScare)
            | (BusinessTrip, OnTheRoad | OnTheWay | Busy | TempWork)
            | (
                ShortTrip | LongTrip | HometownVisit,
                OnTheRoad | AdmiringScenery | TakingSelfie
            )
            | (HometownVisit, BrosGathering | SisGathering)
            | (Hospitalized | PostSurgery, Reading | Gaming)
            | (Period, Irritable | Unwell)
            | (Breakup | Quarrel | ScoldedBySuperior | InBattle, Irritable)
            | (GotHit | GotBumped | WeatherChange, Unwell)
            | (WorkingOut, Showering | Bathing)
    )
}

/// `FOLLOW_FACTOR` once for every distinct live prior of `kind`: under way
/// at `now`, or ended within `FOLLOW_WINDOW_HOURS` before it.
fn follow_factor(kind: EventKind, history: &[PastEvent], now: DateTime<Utc>) -> f64 {
    let since = now - Duration::hours(FOLLOW_WINDOW_HOURS);
    let mut live: Vec<EventKind> = history
        .iter()
        .filter(|e| e.ends_at > since && follows(e.kind, kind))
        .map(|e| e.kind)
        .collect();
    live.sort_by_key(|k| k.as_str());
    live.dedup();
    FOLLOW_FACTOR.powi(live.len() as i32)
}

/// `UNLIKELY_FACTOR` for `having_sex` while she is a virgin, and for
/// `pregnancy_scare` while she has never had sex (spec §3, §4).
fn prior_factor(kind: EventKind, persona: &PersonaTraits, ever_had_sex: bool) -> f64 {
    match kind {
        EventKind::HavingSex if persona.is_virgin && !ever_had_sex => UNLIKELY_FACTOR,
        EventKind::PregnancyScare if !ever_had_sex => UNLIKELY_FACTOR,
        _ => 1.0,
    }
}

/// How `homesick` steers a trip within its category (spec §5): home three
/// times or a third as likely, and leisure trips a third as likely for a
/// persona whose home is her base. `business_trip` is work and untouched.
fn homesick_factor(kind: EventKind, persona: &PersonaTraits) -> f64 {
    use EventKind::*;
    match (kind, persona.homesick) {
        (HometownVisit, Some(true)) => PREFERENCE_FACTOR,
        (HometownVisit, Some(false)) | (ShortTrip | LongTrip, Some(true)) => {
            1.0 / PREFERENCE_FACTOR
        }
        _ => 1.0,
    }
}

/// Whether a background rules a foreground kind out (spec
/// 2026-10-15-life-events-v2-design.md §3.3; spec 2026-10-16-life-events-
/// offline-and-scares-design.md §3.2; spec 2026-10-17-life-events-run-ins-and-
/// kind-filters-design.md §2.2): in hospital or recovering from surgery,
/// she is not out and about; during her period, a late-period or pregnancy
/// scare cannot happen.
fn rules_out(background: EventKind, kind: EventKind) -> bool {
    use EventKind::*;
    match background {
        Hospitalized | PostSurgery => matches!(
            kind,
            TempWork
                | PoliceEncounter
                | AnimalEncounter
                | HitSomeone
                | GotHit
                | WorkingOut
                | GroceryShopping
                | InClass
                | OnTheWay
                | InBattle
                | GotBumped
                | BumpedSomeone
                | SawBag
                | SawClothes
                | OutShopping
                | BrosGathering
                | SisGathering
                | AdmiringScenery
                | AdmiringArt
                | WatchingShow
                | OnTheRoad
                | HavingSex
        ),
        Period => kind.is_false_alarm(),
        _ => false,
    }
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
    /// Whether the instance has a `having_sex` row, ever (not bounded by
    /// `lookback()`).
    pub ever_had_sex: bool,
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
        .filter(|k| {
            !k.is_offline(input.persona)
                || !input.history.iter().any(|e| {
                    e.kind.is_offline(input.persona)
                        && cooling(e.started_at, OFFLINE_COOLDOWN_HOURS)
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
                * indulgence_factor(k, input.needs.as_ref())
                * preference_factor(k, input.persona)
                * follow_factor(k, input.history, input.now)
                * prior_factor(k, input.persona, input.ever_had_sex)
                * homesick_factor(k, input.persona);
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

/// The kinds that take this persona offline: the drawn ones that
/// `is_offline`, and `taking_space`, which is written directly and never
/// drawn. What a query over stored events asks for.
pub fn offline_kinds(persona: &PersonaTraits) -> Vec<EventKind> {
    EventKind::ALL
        .into_iter()
        .chain(EventKind::SPECIAL)
        .filter(|k| k.is_offline(persona))
        .collect()
}

/// Whether a drawn kind is told in a message when it starts (spec
/// 2026-10-16-life-events-offline-and-scares-design.md §3.5): every kind that
/// is not offline; `busy` and `in_battle` from rung 2, so she says she will be
/// away only to someone she is close to, and so does a female persona's
/// `out_shopping` (spec 2026-10-19-life-events-shopping-gatherings-and-ask-for-
/// space-design.md §2.1); never `asleep`, `away_from_phone`, a gathering, or
/// the storage-only `taking_space` and `fell_out`.
pub fn is_told(kind: EventKind, rung: u8, persona: &PersonaTraits) -> bool {
    match kind {
        EventKind::TakingSpace | EventKind::FellOut => false,
        EventKind::Busy | EventKind::InBattle => rung >= CONFIDE_MIN_RUNG,
        EventKind::OutShopping if persona.female => rung >= CONFIDE_MIN_RUNG,
        k => !k.is_offline(persona),
    }
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
            outing: NeedLevel::Mid,
            indulgence: NeedLevel::Mid,
        })
    }

    fn hungry() -> Option<Needs> {
        Some(Needs {
            hunger: NeedLevel::High,
            social: NeedLevel::Mid,
            stamina: NeedLevel::Mid,
            emotional: NeedLevel::Mid,
            outing: NeedLevel::Mid,
            indulgence: NeedLevel::Mid,
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
                ever_had_sex: false,
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
            GroceryShopping,
            InClass,
            OnTheWay,
            GotBumped,
            BumpedSomeone,
        ];
        for bg in [Hospitalized, PostSurgery] {
            let h = [past(bg, 1, 72)];
            assert_eq!(pick(&h, &outdoor, rolls(0.0, 0.0)), None, "{bg:?}");
            assert_eq!(pick(&h, &[Reading], rolls(0.0, 0.0)), Some(Reading));
        }
        for bg in [Hospitalized, PostSurgery] {
            let h = [past(bg, 1, 72)];
            for k in [
                Showering,
                Bathing,
                Quarrel,
                ScoldedBySuperior,
                ScoldedSuperior,
            ] {
                assert_eq!(pick(&h, &[k], rolls(0.0, 0.0)), Some(k), "{bg:?} {k:?}");
            }
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
        let cases: [(&[EventKind], Option<Needs>, f64, EventKind); 18] = [
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
            // grocery 2 : showering 2 — 0.6 → 2.4 → showering.
            (&[GroceryShopping, Showering], None, 0.6, Showering),
            // hunger high: 4 : 2 — 0.6 → 3.6 → grocery.
            (
                &[GroceryShopping, Showering],
                hungry(),
                0.6,
                GroceryShopping,
            ),
            // bathing 1 : in_class 1 — 0.6 → 1.2 → in_class.
            (&[Bathing, InClass], None, 0.6, InClass),
            // stamina low: 2 : 1 — 0.6 → 1.8 → bathing.
            (&[Bathing, InClass], needs(Low, Mid), 0.6, Bathing),
        ];
        for (allowed, n, roll, want) in cases {
            assert_eq!(
                pick_with(&female(), n, 3, &[], allowed, rolls(0.0, roll)),
                Some(want),
                "{allowed:?} {n:?} {roll}"
            );
        }
    }

    /// Every kind in `earlier()`, rung 3, a female persona, no needs:
    /// categories short 5, mid 4, body 0.5, trip 0.5 (10); within short
    /// police 1, animal 3, hit 0.5, desire 2, grocery 2, showering 2,
    /// bathing 1, in_class 1, on_the_way 2 (14.5).
    #[test]
    fn the_stages_pick_by_cumulative_weight() {
        let allowed = earlier();
        let at = |c: f64, k: f64| pick(&[], &allowed, rolls(c, k)).unwrap();
        assert_eq!(at(0.0, 0.0), PoliceEncounter);
        assert_eq!(at(0.5 - E, 0.0).category(), EventCategory::Short);
        assert_eq!(at(0.5 + E, 0.0), TempWork);
        assert_eq!(at(0.9 - E, 0.0).category(), EventCategory::Mid);
        assert_eq!(at(0.9 + E, 0.0), Sick);
        assert_eq!(at(0.95 - E, 0.0).category(), EventCategory::Body);
        assert_eq!(at(0.95 + E, 0.0), BusinessTrip);
        assert_eq!(at(1.0, 1.0), HometownVisit);
        let b = |w: f64| w / 14.5;
        assert_eq!(at(0.0, b(1.0) - E), PoliceEncounter);
        assert_eq!(at(0.0, b(1.0) + E), AnimalEncounter);
        assert_eq!(at(0.0, b(4.0) - E), AnimalEncounter);
        assert_eq!(at(0.0, b(4.0) + E), HitSomeone);
        assert_eq!(at(0.0, b(4.5) - E), HitSomeone);
        assert_eq!(at(0.0, b(4.5) + E), SuddenDesire);
        assert_eq!(at(0.0, b(6.5) - E), SuddenDesire);
        assert_eq!(at(0.0, b(6.5) + E), GroceryShopping);
        assert_eq!(at(0.0, b(8.5) + E), Showering);
        assert_eq!(at(0.0, b(10.5) + E), Bathing);
        assert_eq!(at(0.0, b(11.5) + E), InClass);
        assert_eq!(at(0.0, b(12.5) + E), OnTheWay);
        assert_eq!(at(0.0, 1.0), OnTheWay);
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
                    ever_had_sex: false,
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
        assert_eq!(lasts(Showering, 0.9), Duration::minutes(30));
        assert_eq!(lasts(OnTheWay, 0.0), Duration::minutes(30));
        assert_eq!(lasts(GroceryShopping, 0.5), Duration::hours(1));
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
            [21, 23, 4, 4],
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

    /// The kinds added after the 28 v2 kinds; tests pinned to the weights of
    /// those 28 draw from `earlier()`.
    const ADDED: [EventKind; 24] = [
        PeriodScare,
        PregnancyScare,
        Busy,
        InBattle,
        Asleep,
        GotBumped,
        BumpedSomeone,
        Quarrel,
        ScoldedBySuperior,
        ScoldedSuperior,
        SawBag,
        SawClothes,
        OutShopping,
        BrosGathering,
        SisGathering,
        AdmiringScenery,
        AdmiringArt,
        TakingSelfie,
        OnTheRoad,
        WatchingMatch,
        WatchingShow,
        HavingSex,
        WantToGoOut,
        WantToGoHome,
    ];

    fn earlier() -> Vec<EventKind> {
        EventKind::ALL
            .into_iter()
            .filter(|k| !ADDED.contains(k))
            .collect()
    }

    #[test]
    fn the_false_alarm_gates() {
        let male = PersonaTraits::default();
        for (k, min_rung) in [(PeriodScare, 2), (PregnancyScare, 3)] {
            assert_eq!(
                pick_with(&female(), None, min_rung - 1, &[], &[k], rolls(0.0, 0.0)),
                None,
                "{k:?} below its rung"
            );
            assert_eq!(
                pick_with(&female(), None, min_rung, &[], &[k], rolls(0.0, 0.0)),
                Some(k),
                "{k:?} at its rung"
            );
            assert_eq!(
                pick_with(&male, None, 3, &[], &[k], rolls(0.0, 0.0)),
                None,
                "{k:?} needs a female persona"
            );
        }
    }

    #[test]
    fn a_period_rules_the_false_alarms_out() {
        let h = [past(Period, 24, 96)];
        for k in [PeriodScare, PregnancyScare] {
            assert_eq!(pick(&h, &[k], rolls(0.0, 0.0)), None, "{k:?}");
        }
        assert_eq!(pick(&h, &[Reading], rolls(0.0, 0.0)), Some(Reading));
        let trip = [past(BusinessTrip, 2, 24)];
        assert_eq!(
            pick(&trip, &[PeriodScare], rolls(0.0, 0.0)),
            Some(PeriodScare)
        );
    }

    #[test]
    fn hospital_and_surgery_rule_out_a_hard_battle() {
        for bg in [Hospitalized, PostSurgery] {
            assert_eq!(
                pick(&[past(bg, 1, 72)], &[InBattle], rolls(0.0, 0.0)),
                None,
                "{bg:?}"
            );
            assert_eq!(
                pick(&[past(bg, 1, 72)], &[Asleep], rolls(0.0, 0.0)),
                Some(Asleep)
            );
            assert_eq!(
                pick(&[past(bg, 1, 72)], &[Busy], rolls(0.0, 0.0)),
                Some(Busy)
            );
        }
        assert_eq!(
            pick(&[past(Sick, 1, 48)], &[InBattle], rolls(0.0, 0.0)),
            Some(InBattle)
        );
    }

    #[test]
    fn offline_kinds_share_a_twelve_hour_cooldown() {
        let asleep_at = |hours_ago: i64| PastEvent {
            kind: Asleep,
            started_at: now() - Duration::hours(hours_ago),
            ends_at: now() - Duration::hours(hours_ago) + Duration::hours(1),
        };
        let cooling = [PastEvent {
            started_at: now() - Duration::hours(12) + Duration::seconds(1),
            ends_at: now() - Duration::hours(11),
            kind: Asleep,
        }];
        for k in [Busy, InBattle, AwayFromPhone] {
            assert_eq!(pick(&cooling, &[k], rolls(0.0, 0.0)), None, "{k:?}");
            assert_eq!(
                pick(&[asleep_at(12)], &[k], rolls(0.0, 0.0)),
                Some(k),
                "{k:?}"
            );
        }
        assert_eq!(pick(&cooling, &[Reading], rolls(0.0, 0.0)), Some(Reading));
    }

    #[test]
    fn asleep_weighs_double_when_she_is_worn_out() {
        // Reading 2 then Asleep 2 within `mid`: 0.4 × 4 = 1.6 lands on Reading;
        // worn out, Asleep weighs 4: 0.4 × 6 = 2.4 lands on Asleep.
        let r = rolls(0.0, 0.4);
        let at = |n| pick_with(&female(), n, 3, &[], &[Reading, Asleep], r);
        assert_eq!(at(None), Some(Reading));
        assert_eq!(at(needs(NeedLevel::Low, NeedLevel::Mid)), Some(Asleep));
    }

    #[test]
    fn offline_false_alarm_and_told() {
        let none = PersonaTraits::default();
        for k in EventKind::ALL {
            // `out_shopping` is the one kind whose answer follows the persona.
            assert_eq!(
                k.is_offline(&none),
                matches!(
                    k,
                    AwayFromPhone
                        | Busy
                        | InBattle
                        | Asleep
                        | BrosGathering
                        | SisGathering
                        | HavingSex
                ),
                "{k:?}"
            );
            assert_eq!(
                k.is_false_alarm(),
                matches!(k, PeriodScare | PregnancyScare),
                "{k:?}"
            );
        }
        assert!(!is_told(Busy, 1, &none));
        assert!(is_told(Busy, 2, &none));
        assert!(!is_told(InBattle, 1, &none));
        assert!(is_told(InBattle, 3, &none));
        assert!(!is_told(Asleep, 3, &none));
        assert!(!is_told(AwayFromPhone, 3, &none));
        assert!(is_told(Reading, 1, &none));
        assert!(is_told(PeriodScare, 2, &none));
        for k in [
            GotBumped,
            BumpedSomeone,
            Quarrel,
            ScoldedBySuperior,
            ScoldedSuperior,
        ] {
            assert!(is_told(k, 0, &none), "{k:?}");
        }
    }

    #[test]
    fn the_added_kinds_last() {
        let lasts = |kind: EventKind, duration: f64| {
            draw(
                &DrawInput {
                    now: now(),
                    needs: None,
                    intimacy_rung: 3,
                    persona: &female(),
                    history: &[],
                    allowed: &[kind],
                    ever_had_sex: false,
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
        assert_eq!(lasts(PeriodScare, 0.5), Duration::hours(1));
        assert_eq!(lasts(PregnancyScare, 0.9), Duration::hours(1));
        assert_eq!(lasts(Busy, 0.0), Duration::hours(1));
        assert_eq!(lasts(Busy, 1.0), Duration::hours(3));
        assert_eq!(lasts(InBattle, 0.0), Duration::hours(2));
        assert_eq!(lasts(InBattle, 1.0), Duration::hours(4));
        assert_eq!(lasts(Asleep, 0.0), Duration::hours(1));
        assert_eq!(lasts(Asleep, 1.0), Duration::hours(4));
        for (k, want) in [
            (GotBumped, Duration::minutes(30)),
            (BumpedSomeone, Duration::minutes(30)),
            (Quarrel, Duration::hours(1)),
            (ScoldedBySuperior, Duration::hours(1)),
            (ScoldedSuperior, Duration::hours(1)),
        ] {
            assert_eq!(lasts(k, 0.0), want, "{k:?}");
            assert_eq!(lasts(k, 1.0), want, "{k:?}");
        }
    }

    #[test]
    fn the_run_ins_have_no_gate() {
        let none = PersonaTraits::default();
        for k in [
            GotBumped,
            BumpedSomeone,
            Quarrel,
            ScoldedBySuperior,
            ScoldedSuperior,
        ] {
            assert_eq!(
                pick_with(&none, None, 0, &[], &[k], rolls(0.0, 0.0)),
                Some(k),
                "{k:?}"
            );
        }
    }

    #[test]
    fn a_quarrel_weighs_double_when_she_needs_to_be_cared_for() {
        // police 1 : quarrel 1 — 0.4 × 2 = 0.8 lands on police; with
        // emotional high, quarrel weighs 2: 0.4 × 3 = 1.2 lands on quarrel.
        let r = rolls(0.0, 0.4);
        let at = |n| pick_with(&female(), n, 3, &[], &[PoliceEncounter, Quarrel], r);
        assert_eq!(at(None), Some(PoliceEncounter));
        assert_eq!(at(needs(NeedLevel::Mid, NeedLevel::High)), Some(Quarrel));
    }

    #[test]
    fn new_kinds_parse_and_count() {
        for (k, s) in [
            (SawBag, "saw_bag"),
            (SawClothes, "saw_clothes"),
            (OutShopping, "out_shopping"),
            (BrosGathering, "bros_gathering"),
            (SisGathering, "sis_gathering"),
        ] {
            assert_eq!(k.as_str(), s);
            assert_eq!(EventKind::parse(s), Some(k));
            assert!(EventKind::ALL.contains(&k), "{k:?}");
        }
        assert_eq!(EventKind::ALL.len(), 52);
    }

    #[test]
    fn special_kinds_parse_but_are_never_drawn() {
        assert_eq!(EventKind::parse("taking_space"), Some(TakingSpace));
        assert_eq!(EventKind::parse("fell_out"), Some(FellOut));
        assert!(!EventKind::ALL.contains(&TakingSpace));
        assert!(!EventKind::ALL.contains(&FellOut));
        assert_eq!(EventKind::ALL.len(), 52);
        for k in EventKind::SPECIAL {
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
            assert_eq!(EventKind::parse(k.as_str()), Some(k));
            for i in 0..=10 {
                let r = i as f64 / 10.0;
                assert_eq!(pick(&[], &EventKind::SPECIAL, rolls(r, r)), None, "{k:?}");
                assert_ne!(pick(&[], &EventKind::ALL, rolls(r, r)), Some(k), "{k:?}");
            }
        }
    }

    #[test]
    fn taking_space_is_offline_fell_out_is_not() {
        let personas = [
            PersonaTraits::default(),
            female(),
            PersonaTraits {
                prefer_trip: true,
                ..Default::default()
            },
        ];
        for p in &personas {
            assert!(TakingSpace.is_offline(p), "{p:?}");
            assert!(!FellOut.is_offline(p), "{p:?}");
            for rung in 0..=4 {
                assert!(!is_told(TakingSpace, rung, p), "{p:?} rung {rung}");
                assert!(!is_told(FellOut, rung, p), "{p:?} rung {rung}");
            }
        }
        assert!(TakingSpace.may_be_offline());
        assert!(!FellOut.may_be_offline());
    }

    #[test]
    fn a_separation_blocks_the_draw() {
        assert_eq!(TakingSpace.category(), EventCategory::Mid);
        assert!(!TakingSpace.is_background());
        let h = [past(TakingSpace, 1, 24)];
        assert_eq!(pick(&h, &EventKind::ALL, rolls(0.99, 0.5)), None);
        assert_eq!(pick(&h, &EventKind::ALL, rolls(0.0, 0.0)), None);
    }

    #[test]
    fn shopping_finds_need_a_female_persona() {
        let male = PersonaTraits::default();
        for rung in 1..=3 {
            for k in [SawBag, SawClothes] {
                assert_eq!(
                    pick_with(&male, None, rung, &[], &[k], rolls(0.0, 0.0)),
                    None,
                    "{k:?} at rung {rung}"
                );
                assert_eq!(
                    pick_with(&female(), None, rung, &[], &[k], rolls(0.0, 0.0)),
                    Some(k),
                    "{k:?} at rung {rung}"
                );
            }
            for k in [OutShopping, BrosGathering, SisGathering] {
                for persona in [&male, &female()] {
                    assert_eq!(
                        pick_with(persona, None, rung, &[], &[k], rolls(0.0, 0.0)),
                        Some(k),
                        "{k:?} at rung {rung}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_new_kinds_weigh_cool_and_last_as_specified() {
        // Short: police 1 : find 2 — 0.32 × 3 = 0.96 lands on police, 0.34 on the find.
        for k in [SawBag, SawClothes] {
            assert_eq!(
                pick(&[], &[PoliceEncounter, k], rolls(0.0, 0.32)),
                Some(PoliceEncounter)
            );
            assert_eq!(pick(&[], &[PoliceEncounter, k], rolls(0.0, 0.34)), Some(k));
        }
        // Mid: out_shopping 2 : gathering 1 — 0.66 × 3 = 1.98 lands on
        // out_shopping, 0.68 on the gathering.
        for k in [BrosGathering, SisGathering] {
            assert_eq!(
                pick(&[], &[OutShopping, k], rolls(0.0, 0.66)),
                Some(OutShopping)
            );
            assert_eq!(pick(&[], &[OutShopping, k], rolls(0.0, 0.68)), Some(k));
        }
        for (k, cooldown) in [
            (SawBag, 48),
            (SawClothes, 48),
            (OutShopping, 24),
            (BrosGathering, 72),
            (SisGathering, 72),
        ] {
            let cooling = [PastEvent {
                started_at: now() - Duration::hours(cooldown) + Duration::seconds(1),
                ..past(k, cooldown, 1)
            }];
            assert_eq!(pick(&cooling, &[k], rolls(0.0, 0.0)), None, "{k:?}");
            assert_eq!(
                pick(&[past(k, cooldown, 1)], &[k], rolls(0.0, 0.0)),
                Some(k),
                "{k:?}"
            );
        }
        let lasts = |kind: EventKind, duration: f64| {
            draw(
                &DrawInput {
                    now: now(),
                    needs: None,
                    intimacy_rung: 3,
                    persona: &female(),
                    history: &[],
                    allowed: &[kind],
                    ever_had_sex: false,
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
        for k in [SawBag, SawClothes] {
            assert_eq!(lasts(k, 0.0), Duration::hours(1), "{k:?}");
            assert_eq!(lasts(k, 1.0), Duration::hours(1), "{k:?}");
        }
        for k in [OutShopping, BrosGathering, SisGathering] {
            assert_eq!(lasts(k, 0.0), Duration::hours(2), "{k:?}");
            assert_eq!(lasts(k, 1.0), Duration::hours(4), "{k:?}");
        }
    }

    #[test]
    fn offline_follows_the_persona() {
        let male = PersonaTraits::default();
        for persona in [&male, &female()] {
            for k in [
                BrosGathering,
                SisGathering,
                AwayFromPhone,
                Busy,
                InBattle,
                Asleep,
            ] {
                assert!(k.is_offline(persona), "{k:?}");
            }
            for k in [SawBag, SawClothes, Reading] {
                assert!(!k.is_offline(persona), "{k:?}");
            }
        }
        assert!(OutShopping.is_offline(&female()));
        assert!(!OutShopping.is_offline(&male));
        for k in EventKind::ALL {
            assert_eq!(
                k.may_be_offline(),
                matches!(
                    k,
                    AwayFromPhone
                        | Busy
                        | InBattle
                        | Asleep
                        | OutShopping
                        | BrosGathering
                        | SisGathering
                        | HavingSex
                ),
                "{k:?}"
            );
        }
    }

    #[test]
    fn offline_kinds_list_what_takes_this_persona_offline() {
        let male_kinds = offline_kinds(&PersonaTraits::default());
        let female_kinds = offline_kinds(&female());
        for persona in [&male_kinds, &female_kinds] {
            for k in [
                AwayFromPhone,
                Busy,
                InBattle,
                Asleep,
                BrosGathering,
                SisGathering,
                TakingSpace,
            ] {
                assert!(persona.contains(&k), "{k:?}");
            }
            assert!(!persona.contains(&FellOut));
            assert!(!persona.contains(&Reading));
        }
        assert!(!male_kinds.contains(&OutShopping));
        assert!(female_kinds.contains(&OutShopping));
        for (traits, kinds) in [
            (PersonaTraits::default(), &male_kinds),
            (female(), &female_kinds),
        ] {
            for k in EventKind::ALL.into_iter().chain(EventKind::SPECIAL) {
                assert_eq!(kinds.contains(&k), k.is_offline(&traits), "{k:?}");
            }
        }
    }

    #[test]
    fn told_follows_offline_and_rung() {
        let male = PersonaTraits::default();
        assert!(!is_told(OutShopping, 1, &female()));
        assert!(is_told(OutShopping, 2, &female()));
        assert!(is_told(OutShopping, 3, &female()));
        for rung in 0..=3 {
            assert!(is_told(OutShopping, rung, &male), "rung {rung}");
            for persona in [&male, &female()] {
                assert!(!is_told(BrosGathering, rung, persona), "rung {rung}");
                assert!(!is_told(SisGathering, rung, persona), "rung {rung}");
                assert!(is_told(SawBag, rung, persona), "rung {rung}");
                assert!(is_told(SawClothes, rung, persona), "rung {rung}");
            }
        }
    }

    #[test]
    fn shopping_and_gatherings_stay_home_from_hospital() {
        let five = [SawBag, SawClothes, OutShopping, BrosGathering, SisGathering];
        for bg in [Hospitalized, PostSurgery] {
            let h = [past(bg, 1, 72)];
            for k in five {
                assert_eq!(pick(&h, &[k], rolls(0.0, 0.0)), None, "{bg:?} {k:?}");
            }
        }
        let h = [past(Sick, 1, 48)];
        for k in five {
            assert_eq!(pick(&h, &[k], rolls(0.0, 0.0)), Some(k), "Sick {k:?}");
        }
    }

    #[test]
    fn offline_cooldown_is_persona_aware() {
        let male = PersonaTraits::default();
        let shopping = |started_hours_ago: i64| [past(OutShopping, started_hours_ago, 2)];
        for k in [Busy, InBattle, Asleep, AwayFromPhone, BrosGathering] {
            // Her shopping went dark 5 h ago: another offline kind waits.
            assert_eq!(
                pick_with(&female(), None, 3, &shopping(5), &[k], rolls(0.0, 0.0)),
                None,
                "{k:?}"
            );
            // The same history under a persona who kept replying blocks nothing.
            assert_eq!(
                pick_with(&male, None, 3, &shopping(5), &[k], rolls(0.0, 0.0)),
                Some(k),
                "{k:?}"
            );
            // And 12 h after it started it is over for her too.
            assert_eq!(
                pick_with(&female(), None, 3, &shopping(12), &[k], rolls(0.0, 0.0)),
                Some(k),
                "{k:?}"
            );
        }
        // The other way round: an offline kind in the history holds back her
        // shopping only when shopping would take her offline.
        let asleep = [past(Asleep, 5, 2)];
        assert_eq!(
            pick_with(&female(), None, 3, &asleep, &[OutShopping], rolls(0.0, 0.0)),
            None
        );
        assert_eq!(
            pick_with(&male, None, 3, &asleep, &[OutShopping], rolls(0.0, 0.0)),
            Some(OutShopping)
        );
    }

    #[test]
    fn seven_more_kinds_parse_and_categorise() {
        for (k, s, c) in [
            (AdmiringScenery, "admiring_scenery", EventCategory::Short),
            (AdmiringArt, "admiring_art", EventCategory::Short),
            (TakingSelfie, "taking_selfie", EventCategory::Short),
            (OnTheRoad, "on_the_road", EventCategory::Mid),
            (WatchingMatch, "watching_match", EventCategory::Mid),
            (WatchingShow, "watching_show", EventCategory::Mid),
            (HavingSex, "having_sex", EventCategory::Mid),
        ] {
            assert_eq!(k.as_str(), s);
            assert_eq!(EventKind::parse(s), Some(k));
            assert_eq!(k.category(), c, "{k:?}");
            assert!(EventKind::ALL.contains(&k), "{k:?}");
        }
    }

    #[test]
    fn having_sex_is_offline_for_everyone_and_never_told() {
        let none = PersonaTraits::default();
        assert!(HavingSex.is_offline(&none));
        assert!(HavingSex.is_offline(&female()));
        assert!(!is_told(HavingSex, 3, &none));
        for k in [
            AdmiringScenery,
            AdmiringArt,
            TakingSelfie,
            OnTheRoad,
            WatchingMatch,
            WatchingShow,
        ] {
            assert!(!k.is_offline(&none), "{k:?}");
            assert!(!k.is_offline(&female()), "{k:?}");
            assert!(is_told(k, 1, &none), "{k:?}");
        }
    }

    #[test]
    fn hospital_keeps_the_sights_and_shows_in_but_allows_a_selfie() {
        let allowed = [
            AdmiringScenery,
            AdmiringArt,
            WatchingShow,
            OnTheRoad,
            HavingSex,
            TakingSelfie,
            WatchingMatch,
        ];
        for bg in [Hospitalized, PostSurgery] {
            let h = [past(bg, 24, 72)];
            for c in [0.0, 0.99] {
                for k in [0.0, 0.25, 0.5, 0.75, 0.99] {
                    let got = pick(&h, &allowed, rolls(c, k));
                    assert!(
                        matches!(got, Some(TakingSelfie) | Some(WatchingMatch)),
                        "{bg:?} {c} {k} {got:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn seven_more_rules() {
        for (k, weight, cooldown, lo, hi) in [
            (AdmiringScenery, 2.0, 24, 1, 1),
            (AdmiringArt, 1.0, 48, 1, 1),
            (OnTheRoad, 2.0, 24, 2, 4),
            (WatchingMatch, 2.0, 24, 2, 3),
            (WatchingShow, 1.0, 72, 2, 3),
            (HavingSex, 1.0, 48, 1, 2),
        ] {
            let r = rule(k);
            assert!((r.weight - weight).abs() < E, "{k:?}");
            assert_eq!(r.cooldown_hours, cooldown, "{k:?}");
            assert_eq!(lasts(k, 0.0), Duration::hours(lo), "{k:?}");
            assert_eq!(lasts(k, 0.999), Duration::hours(hi), "{k:?}");
        }
        let r = rule(TakingSelfie);
        assert!((r.weight - 2.0).abs() < E);
        assert_eq!(r.cooldown_hours, 24);
        assert_eq!(lasts(TakingSelfie, 0.5), Duration::minutes(30));
    }

    #[test]
    fn only_a_selfie_is_a_photo() {
        for k in EventKind::ALL {
            assert_eq!(k.is_photo(), k == EventKind::TakingSelfie, "{k:?}");
        }
    }

    #[test]
    fn a_live_prior_triples_its_follow_on() {
        // HavingSex 1 : Reading 2. With ex_message live, HavingSex weighs 3:
        // 0.5 × 5 = 2.5 lands on HavingSex; without, 0.5 × 3 = 1.5 on Reading.
        let none = PersonaTraits::default();
        let live = [past(ExMessage, 20, 3)];
        let r = rolls(0.0, 0.5);
        assert_eq!(
            pick_with(&none, None, 3, &live, &[HavingSex, Reading], r),
            Some(HavingSex)
        );
        assert_eq!(
            pick_with(&none, None, 3, &[], &[HavingSex, Reading], r),
            Some(Reading)
        );
    }

    #[test]
    fn a_prior_ended_49_hours_ago_is_not_live() {
        let none = PersonaTraits::default();
        let stale = [past(ExMessage, 52, 3)];
        assert_eq!(
            pick_with(
                &none,
                None,
                3,
                &stale,
                &[HavingSex, Reading],
                rolls(0.0, 0.5)
            ),
            Some(Reading)
        );
    }

    #[test]
    fn a_background_under_way_is_live() {
        let none = PersonaTraits::default();
        let trip = [past(BusinessTrip, 2, 24)];
        assert_eq!(
            pick_with(
                &none,
                None,
                3,
                &trip,
                &[OnTheRoad, Reading],
                rolls(0.0, 0.5)
            ),
            Some(OnTheRoad)
        );
    }

    #[test]
    fn two_live_priors_multiply() {
        // HavingSex 1 × 3 × 3 = 9 : Reading 2 — 0.8 × 11 = 8.8 lands on HavingSex.
        let none = PersonaTraits::default();
        let live = [past(ExMessage, 20, 3), past(SuddenDesire, 10, 1)];
        assert_eq!(
            pick_with(
                &none,
                None,
                3,
                &live,
                &[HavingSex, Reading],
                rolls(0.0, 0.8)
            ),
            Some(HavingSex)
        );
    }

    #[test]
    fn a_live_prior_never_lifts_a_gate() {
        let male = PersonaTraits::default();
        let sick = [past(Sick, 2, 72)];
        for k in [0.0, 0.5, 0.99] {
            assert_eq!(
                pick_with(
                    &male,
                    None,
                    3,
                    &sick,
                    &[PeriodScare, Reading],
                    rolls(0.0, k)
                ),
                Some(Reading)
            );
        }
    }

    #[test]
    fn follow_table_is_what_the_spec_says() {
        for (prior, kind) in [
            (ExMessage, HavingSex),
            (SuddenDesire, HavingSex),
            (Sick, PeriodScare),
            (HavingSex, PregnancyScare),
            (BusinessTrip, OnTheRoad),
            (BusinessTrip, OnTheWay),
            (BusinessTrip, Busy),
            (BusinessTrip, TempWork),
            (ShortTrip, OnTheRoad),
            (LongTrip, AdmiringScenery),
            (HometownVisit, TakingSelfie),
            (HometownVisit, BrosGathering),
            (HometownVisit, SisGathering),
            (Hospitalized, Reading),
            (PostSurgery, Gaming),
            (Period, Irritable),
            (Period, Unwell),
            (Breakup, Irritable),
            (Quarrel, Irritable),
            (ScoldedBySuperior, Irritable),
            (InBattle, Irritable),
            (GotHit, Unwell),
            (GotBumped, Unwell),
            (WeatherChange, Unwell),
            (WorkingOut, Showering),
            (WorkingOut, Bathing),
        ] {
            assert!(follows(prior, kind), "{prior:?} -> {kind:?}");
        }
        assert!(!follows(Reading, HavingSex));
        assert!(!follows(HavingSex, HavingSex));
    }

    /// A draw with no history for `persona`, with or without a `having_sex`
    /// row ever.
    fn pick_ever(
        persona: &PersonaTraits,
        ever_had_sex: bool,
        allowed: &[EventKind],
        r: Rolls,
    ) -> Option<EventKind> {
        draw(
            &DrawInput {
                now: now(),
                needs: None,
                intimacy_rung: 3,
                persona,
                history: &[],
                allowed,
                ever_had_sex,
            },
            r,
        )
        .map(|d| d.kind)
    }

    #[test]
    fn a_virgin_rarely_has_sex_until_she_has() {
        // Draw order is `ALL` order: Reading 2 then HavingSex 0.2 — 0.8 × 2.2
        // = 1.76 lands on Reading; once she has, Reading 2 then HavingSex 1 —
        // 0.8 × 3 = 2.4 lands on HavingSex.
        let virgin = PersonaTraits {
            is_virgin: true,
            ..Default::default()
        };
        let both = [HavingSex, Reading];
        let r = rolls(0.0, 0.8);
        assert_eq!(pick_ever(&virgin, false, &both, r), Some(Reading));
        assert_eq!(pick_ever(&virgin, true, &both, r), Some(HavingSex));
        assert_eq!(
            pick_ever(&PersonaTraits::default(), false, &both, r),
            Some(HavingSex)
        );
    }

    #[test]
    fn a_pregnancy_scare_is_unlikely_without_sex() {
        // AnimalEncounter 3 then PregnancyScare 0.5 × 0.2 = 0.1 — 0.9 × 3.1 =
        // 2.79 lands on AnimalEncounter; with a row, 3 then 0.5 — 0.9 × 3.5 =
        // 3.15 on PregnancyScare.
        let both = [PregnancyScare, AnimalEncounter];
        let r = rolls(0.0, 0.9);
        assert_eq!(pick_ever(&female(), false, &both, r), Some(AnimalEncounter));
        assert_eq!(pick_ever(&female(), true, &both, r), Some(PregnancyScare));
    }

    #[test]
    fn homesick_steers_her_leave() {
        let trips = [HometownVisit, ShortTrip, BusinessTrip];
        let with = |homesick| PersonaTraits {
            homesick,
            ..Default::default()
        };
        let at = |p: &PersonaTraits, k| pick_ever(p, false, &trips, rolls(0.0, k));
        // Draw order is `ALL` order: BusinessTrip, ShortTrip, HometownVisit.
        // true: 3, 2/3, 3 (6 2/3) — spans [0, .45), [.45, .55), [.55, 1].
        let home = with(Some(true));
        assert_eq!(at(&home, 0.44), Some(BusinessTrip));
        assert_eq!(at(&home, 0.5), Some(ShortTrip));
        assert_eq!(at(&home, 0.6), Some(HometownVisit));
        // false: 3, 2, 1/3 (5 1/3) — spans [0, .5625), [.5625, .9375), [.9375, 1].
        let roams = with(Some(false));
        assert_eq!(at(&roams, 0.5), Some(BusinessTrip));
        assert_eq!(at(&roams, 0.8), Some(ShortTrip));
        assert_eq!(at(&roams, 0.95), Some(HometownVisit));
        // absent: 3, 2, 1 (6) — spans [0, .5), [.5, .833), [.833, 1].
        let none = with(None);
        assert_eq!(at(&none, 0.4), Some(BusinessTrip));
        assert_eq!(at(&none, 0.6), Some(ShortTrip));
        assert_eq!(at(&none, 0.9), Some(HometownVisit));
    }

    fn rated(outing: NeedLevel, indulgence: NeedLevel, emotional: NeedLevel) -> Option<Needs> {
        Some(Needs {
            hunger: NeedLevel::Mid,
            social: NeedLevel::Mid,
            stamina: NeedLevel::Mid,
            emotional,
            outing,
            indulgence,
        })
    }

    #[test]
    fn going_out_and_going_home_are_told_mid_kinds() {
        for (k, s) in [
            (WantToGoOut, "want_to_go_out"),
            (WantToGoHome, "want_to_go_home"),
        ] {
            assert_eq!(k.as_str(), s);
            assert_eq!(EventKind::parse(s), Some(k));
            assert_eq!(k.category(), EventCategory::Mid);
            let r = rule(k);
            assert_eq!((r.weight, r.cooldown_hours), (1.0, 24), "{k:?}");
            assert_eq!(max_hours(r.lasts), 3, "{k:?}");
            assert!(!k.may_be_offline(), "{k:?}");
            assert!(is_told(k, 1, &PersonaTraits::default()), "{k:?}");
            for bg in [Hospitalized, PostSurgery, Period, LongTrip] {
                assert_eq!(
                    pick(&[past(bg, 1, 72)], &[k], rolls(0.0, 0.0)),
                    Some(k),
                    "{k:?} in {bg:?}"
                );
            }
        }
    }

    /// Draw order is `ALL` order: Reading 2, WantToGoOut 1, WantToGoHome 1.
    #[test]
    fn outing_steers_going_out_and_going_home() {
        use NeedLevel::{High, Low, Mid};
        let allowed = [Reading, WantToGoOut, WantToGoHome];
        let at = |n, k| {
            pick_with(
                &PersonaTraits::default(),
                n,
                3,
                &[],
                &allowed,
                rolls(0.0, k),
            )
        };
        // 2 : 1 : 1 — 0.6 → 2.4 → out; 0.8 → 3.2 → home.
        assert_eq!(at(rated(Mid, Mid, Mid), 0.6), Some(WantToGoOut));
        assert_eq!(at(rated(Mid, Mid, Mid), 0.8), Some(WantToGoHome));
        // outing high: 2 : 3 : 1 — 0.8 → 4.8 → out.
        assert_eq!(at(rated(High, Mid, Mid), 0.8), Some(WantToGoOut));
        // outing low: 2 : 1 : 3 — 0.6 → 3.6 → home.
        assert_eq!(at(rated(Low, Mid, Mid), 0.6), Some(WantToGoHome));
    }

    #[test]
    fn the_urge_to_let_loose_sends_her_out_and_frays_her_temper() {
        use NeedLevel::{High, Mid};
        let none = PersonaTraits::default();
        // Reading 2 : WantToGoOut 1 — 0.55 → 1.65 → reading; indulgence high
        // ×2: 2 : 2 — 0.55 → 2.2 → out.
        let out = [Reading, WantToGoOut];
        let at = |n, k| pick_with(&none, n, 3, &[], &out, rolls(0.0, k));
        assert_eq!(at(rated(Mid, Mid, Mid), 0.55), Some(Reading));
        assert_eq!(at(rated(Mid, High, Mid), 0.55), Some(WantToGoOut));
        // Outing too: 2 : 6 — 0.3 → 2.4 → out; outing alone 2 : 3 — 0.3 →
        // 1.5 → reading.
        assert_eq!(at(rated(High, High, Mid), 0.3), Some(WantToGoOut));
        assert_eq!(at(rated(High, Mid, Mid), 0.3), Some(Reading));
        // Irritable 1 : Busy 2 (ALL order) — 0.4 → 1.2 → busy; ×2: 2 : 2 —
        // 0.4 → 1.6 → irritable; with emotional high ×4: 4 : 2 — 0.6 → 3.6
        // → irritable; emotional alone 2 : 2 — 0.6 → 2.4 → busy.
        let temper = [Irritable, Busy];
        let at = |n, k| pick_with(&none, n, 3, &[], &temper, rolls(0.0, k));
        assert_eq!(at(rated(Mid, Mid, Mid), 0.4), Some(Busy));
        assert_eq!(at(rated(Mid, High, Mid), 0.4), Some(Irritable));
        assert_eq!(at(rated(Mid, High, High), 0.6), Some(Irritable));
        assert_eq!(at(rated(Mid, Mid, High), 0.6), Some(Busy));
    }
}
