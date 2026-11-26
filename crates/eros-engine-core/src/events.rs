// SPDX-License-Identifier: AGPL-3.0-only
//! Life events: what may happen in the persona's day, and which one does
//! (spec 2026-10-15-life-events-v2-design.md §3–§5).

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::cycle::{self, CycleRows};
use crate::needs::{Habits, NeedLevel, Needs};

const DAY: i64 = 24;

/// Where a kind sits (§3.1). `Short` and `Mid` are foreground incidents;
/// `Body` and `Trip` are background states that last days; the slot a row
/// under way takes is `EventKind::slot`'s, which gives `period` and
/// `period_scare` one of their own.
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
    Showering,
    Bathing,
    OnTheWay,
    PeriodScare,
    PregnancyScare,
    GotBumped,
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
    /// She and the user are away together: written when the user accepts her
    /// `travel_together` ask.
    TravelingTogether,
    /// She undresses for the user and sends a picture (spec
    /// 2026-11-08-strip-and-world-setting-design.md §3).
    Strip,
    BirdEncounter,
    ExWantsBack,
    WantToQuitJob,
    PonderingLife,
    Streaking,
    WantToSeeSea,
    WantHotSpring,
    WantSauna,
    GotLost,
    Drinking,
    Smoking,
    SmokingWeed,
    JustHaveSex,
    Masturbating,
    /// She missed her ride: only on a trip.
    MissedRide,
    /// She is on a boat: only on a trip (spec 2026-11-19-vehicles-actions-
    /// species-design.md §2).
    OnABoat,
    /// She is on a cruise ship: only on a leisure trip.
    OnACruise,
    /// She is on a coach: only on a trip.
    OnABus,
    /// She is on a train: only on a trip.
    OnATrain,
    /// She is on a plane: only on a trip.
    OnAPlane,
    /// She is driving: only on a trip.
    Driving,
    /// She and the user take a bath together: written when the user accepts
    /// her `bathe_together` ask, or she accepts the user's `bath_together`.
    BathingTogether,
    /// She is out on a date with the user: written when the user's `date`
    /// invitation is accepted, and ended when the activity does.
    OnADate,
    /// She is in the bathroom for a few minutes: any world, not a machine.
    UsingToilet,
    /// She and the user shower together: written when she accepts the user's
    /// `shower_together`.
    ShoweringTogether,
    /// She and the user take a sauna together: written when she accepts the
    /// user's `sauna_together`.
    SaunaTogether,
    /// She and the user use the toilet together: written when she accepts the user's
    /// `toilet_together`.
    UsingToiletTogether,
    /// She and the user smoke together: written when she accepts the user's
    /// `smoke_together`.
    SmokingTogether,
    /// She and the user sleep side by side; she still answers: written when she accepts the user's
    /// `fall_asleep_together`.
    AsleepTogether,
    /// She and the user are in bed together: written when she accepts the user's
    /// `sex_together`.
    HavingSexTogether,
    /// She and the user take a walk together: written when she accepts the user's
    /// `walk_together`.
    WalkingTogether,
    /// She and the user smoke weed together: written when she accepts the user's
    /// `smoke_weed_together`.
    SmokingWeedTogether,
    /// She and the user jog together: written when she accepts the user's
    /// `jog_together`.
    JoggingTogether,
    /// She and the user do yoga together: written when she accepts the user's
    /// `yoga_together`.
    YogaTogether,
    /// She and the user watch the stars together: written when she accepts the user's
    /// `stargaze_together`.
    StargazingTogether,
    /// She and the user ride horses together: written when she accepts the user's
    /// `ride_horse_together`.
    HorseRidingTogether,
    /// She and the user streak together: written when she accepts the user's
    /// `streak_together`.
    StreakingTogether,
    /// She and the user drink together: written when she accepts the user's
    /// `drink_together`.
    DrinkingTogether,
    /// She and the user soak in a hot spring: written when she accepts the
    /// user's `hot_spring_together`.
    HotSpringTogether,
    /// She and the user fight side by side: written when she accepts the
    /// user's `fight_together`; only in a `fantasy` world.
    FightingTogether,
    /// She and the user work out together: written when she accepts the
    /// user's `workout_together` (spec 2026-11-24-focused-tone-four-together-
    /// actions-and-user-gender-design.md §6).
    WorkingOutTogether,
    /// She and the user swim together: written when she accepts the user's
    /// `swim_together`.
    SwimmingTogether,
    /// She and the user cycle together: written when she accepts the user's
    /// `cycle_together`; only in a `modern` world.
    CyclingTogether,
    /// She and the user hike together: written when she accepts the user's
    /// `hike_together`.
    HikingTogether,
    /// She and the user dance together: written when she accepts the user's
    /// `dance_together` (spec 2026-11-26-more-together-actions-bath-outfit-
    /// chemistry-drop-and-custom-together-design.md §4).
    DancingTogether,
    /// She and the user go adventuring together: written when she accepts
    /// the user's `adventure_together`.
    AdventuringTogether,
    /// She and the user play with fire together: written when she accepts
    /// the user's `play_with_fire_together`.
    PlayingWithFireTogether,
    /// She and the user sing together: written when she accepts the user's
    /// `sing_together`.
    SingingTogether,
    /// She and the user skinny-dip together: written when she accepts the
    /// user's `skinny_dip_together`.
    SkinnyDippingTogether,
    /// She and the user commit a crime together: written when she accepts
    /// the user's `commit_crime_together`; it names no crime.
    CommittingCrimeTogether,
    /// She and the user go fishing together: written when she accepts the
    /// user's `fish_together` (spec 2026-11-27-swim-outfit-ladder-and-fish-
    /// together-design.md §6.1).
    FishingTogether,
    /// She and the user ride a horse naked together: written when she accepts
    /// the user's `ride_horse_naked_together` (spec 2026-11-27 §6.2).
    NakedHorseRidingTogether,
    /// She and the user do the thing the user named: written when she
    /// accepts the user's `custom_together` (spec 2026-11-26 §8). The row's
    /// `message_id` is the user's message, which holds the text.
    CustomTogether,
}

/// Where a row under way sits: at most one event is live in each slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Foreground,
    Background,
    Cycle,
}

/// Every kind (`ALL`, then `SPECIAL`) that occupies `slot`.
pub fn kinds_in(slot: Slot) -> Vec<EventKind> {
    EventKind::ALL
        .into_iter()
        .chain(EventKind::SPECIAL)
        .filter(|k| k.slot() == slot)
        .collect()
}

impl EventKind {
    /// Every drawable kind, in draw order.
    pub const ALL: [EventKind; 70] = [
        EventKind::PoliceEncounter,
        EventKind::AnimalEncounter,
        EventKind::HitSomeone,
        EventKind::SuddenDesire,
        EventKind::Showering,
        EventKind::Bathing,
        EventKind::OnTheWay,
        EventKind::PeriodScare,
        EventKind::PregnancyScare,
        EventKind::GotBumped,
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
        EventKind::Strip,
        EventKind::BirdEncounter,
        EventKind::ExWantsBack,
        EventKind::WantToQuitJob,
        EventKind::PonderingLife,
        EventKind::Streaking,
        EventKind::WantToSeeSea,
        EventKind::WantHotSpring,
        EventKind::WantSauna,
        EventKind::GotLost,
        EventKind::Drinking,
        EventKind::Smoking,
        EventKind::SmokingWeed,
        EventKind::JustHaveSex,
        EventKind::Masturbating,
        EventKind::MissedRide,
        EventKind::OnABoat,
        EventKind::OnACruise,
        EventKind::OnABus,
        EventKind::OnATrain,
        EventKind::OnAPlane,
        EventKind::Driving,
        EventKind::UsingToilet,
    ];

    /// Kinds the writers place directly, never drawn: parsed and stored, not in `ALL`.
    pub const SPECIAL: [EventKind; 34] = [
        EventKind::TakingSpace,
        EventKind::FellOut,
        EventKind::TravelingTogether,
        EventKind::BathingTogether,
        EventKind::OnADate,
        EventKind::ShoweringTogether,
        EventKind::SaunaTogether,
        EventKind::UsingToiletTogether,
        EventKind::SmokingTogether,
        EventKind::AsleepTogether,
        EventKind::HavingSexTogether,
        EventKind::WalkingTogether,
        EventKind::SmokingWeedTogether,
        EventKind::JoggingTogether,
        EventKind::YogaTogether,
        EventKind::StargazingTogether,
        EventKind::HorseRidingTogether,
        EventKind::StreakingTogether,
        EventKind::DrinkingTogether,
        EventKind::HotSpringTogether,
        EventKind::FightingTogether,
        EventKind::WorkingOutTogether,
        EventKind::SwimmingTogether,
        EventKind::CyclingTogether,
        EventKind::HikingTogether,
        EventKind::DancingTogether,
        EventKind::AdventuringTogether,
        EventKind::PlayingWithFireTogether,
        EventKind::SingingTogether,
        EventKind::SkinnyDippingTogether,
        EventKind::CommittingCrimeTogether,
        EventKind::FishingTogether,
        EventKind::NakedHorseRidingTogether,
        EventKind::CustomTogether,
    ];

    /// The kinds a chemistry drop ends while they are under way: the ones the
    /// romance-gated actions place, whoever asked, so a row her accepted
    /// `bathe_together` or `sleep_together` ask placed ends too (spec
    /// 2026-11-26-more-together-actions-bath-outfit-chemistry-drop-and-custom-
    /// together-design.md §6; the naked ride, spec 2026-11-27 §6.2).
    pub const ENDS_ON_CHEMISTRY_DROP: [EventKind; 6] = [
        EventKind::ShoweringTogether,
        EventKind::BathingTogether,
        EventKind::HavingSexTogether,
        EventKind::StreakingTogether,
        EventKind::SkinnyDippingTogether,
        EventKind::NakedHorseRidingTogether,
    ];

    /// The kinds under which she holds back her `bathe_together` and
    /// `sleep_together` asks (spec 2026-11-26 §7).
    pub const HOLDS_BACK_INTIMATE_ASKS: [EventKind; 5] = [
        EventKind::BathingTogether,
        EventKind::ShoweringTogether,
        EventKind::HotSpringTogether,
        EventKind::SaunaTogether,
        EventKind::HavingSexTogether,
    ];

    /// The kinds that are sex: any row of one, ever, ends her virginity
    /// (spec 2026-11-11-stocks-lost-habit-events-and-just-have-sex-design.md
    /// §4, 2026-11-23-twelve-together-actions-and-propose-design.md §6).
    pub const SEX: [EventKind; 3] = [
        EventKind::HavingSex,
        EventKind::JustHaveSex,
        EventKind::HavingSexTogether,
    ];

    /// The stored and wire value.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::PoliceEncounter => "police_encounter",
            EventKind::AnimalEncounter => "animal_encounter",
            EventKind::HitSomeone => "hit_someone",
            EventKind::SuddenDesire => "sudden_desire",
            EventKind::Showering => "showering",
            EventKind::Bathing => "bathing",
            EventKind::OnTheWay => "on_the_way",
            EventKind::PeriodScare => "period_scare",
            EventKind::PregnancyScare => "pregnancy_scare",
            EventKind::GotBumped => "got_bumped",
            EventKind::Quarrel => "quarrel",
            EventKind::ScoldedBySuperior => "scolded_by_superior",
            EventKind::ScoldedSuperior => "scolded_superior",
            EventKind::SawBag => "saw_bag",
            EventKind::SawClothes => "saw_clothes",
            EventKind::TempWork => "temp_work",
            EventKind::ExMessage => "ex_message",
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
            EventKind::TravelingTogether => "traveling_together",
            EventKind::Strip => "strip",
            EventKind::BirdEncounter => "bird_encounter",
            EventKind::ExWantsBack => "ex_wants_back",
            EventKind::WantToQuitJob => "want_to_quit_job",
            EventKind::PonderingLife => "pondering_life",
            EventKind::Streaking => "streaking",
            EventKind::WantToSeeSea => "want_to_see_sea",
            EventKind::WantHotSpring => "want_hot_spring",
            EventKind::WantSauna => "want_sauna",
            EventKind::BathingTogether => "bathing_together",
            EventKind::OnADate => "on_a_date",
            EventKind::GotLost => "got_lost",
            EventKind::Drinking => "drinking",
            EventKind::Smoking => "smoking",
            EventKind::SmokingWeed => "smoking_weed",
            EventKind::JustHaveSex => "just_have_sex",
            EventKind::Masturbating => "masturbating",
            EventKind::MissedRide => "missed_ride",
            EventKind::OnABoat => "on_a_boat",
            EventKind::OnACruise => "on_a_cruise",
            EventKind::OnABus => "on_a_bus",
            EventKind::OnATrain => "on_a_train",
            EventKind::OnAPlane => "on_a_plane",
            EventKind::Driving => "driving",
            EventKind::UsingToilet => "using_toilet",
            EventKind::ShoweringTogether => "showering_together",
            EventKind::SaunaTogether => "sauna_together",
            EventKind::UsingToiletTogether => "using_toilet_together",
            EventKind::SmokingTogether => "smoking_together",
            EventKind::AsleepTogether => "asleep_together",
            EventKind::HavingSexTogether => "having_sex_together",
            EventKind::WalkingTogether => "walking_together",
            EventKind::SmokingWeedTogether => "smoking_weed_together",
            EventKind::JoggingTogether => "jogging_together",
            EventKind::YogaTogether => "yoga_together",
            EventKind::StargazingTogether => "stargazing_together",
            EventKind::HorseRidingTogether => "horse_riding_together",
            EventKind::StreakingTogether => "streaking_together",
            EventKind::DrinkingTogether => "drinking_together",
            EventKind::HotSpringTogether => "hot_spring_together",
            EventKind::FightingTogether => "fighting_together",
            EventKind::WorkingOutTogether => "working_out_together",
            EventKind::SwimmingTogether => "swimming_together",
            EventKind::CyclingTogether => "cycling_together",
            EventKind::HikingTogether => "hiking_together",
            EventKind::DancingTogether => "dancing_together",
            EventKind::AdventuringTogether => "adventuring_together",
            EventKind::PlayingWithFireTogether => "playing_with_fire_together",
            EventKind::SingingTogether => "singing_together",
            EventKind::SkinnyDippingTogether => "skinny_dipping_together",
            EventKind::CommittingCrimeTogether => "committing_crime_together",
            EventKind::FishingTogether => "fishing_together",
            EventKind::NakedHorseRidingTogether => "naked_horse_riding_together",
            EventKind::CustomTogether => "custom_together",
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
            PoliceEncounter
            | AnimalEncounter
            | HitSomeone
            | SuddenDesire
            | Showering
            | Bathing
            | OnTheWay
            | PeriodScare
            | PregnancyScare
            | GotBumped
            | Quarrel
            | ScoldedBySuperior
            | ScoldedSuperior
            | SawBag
            | SawClothes
            | AdmiringScenery
            | AdmiringArt
            | TakingSelfie
            | Strip
            | BirdEncounter
            | ExWantsBack
            | WantToQuitJob
            | PonderingLife
            | Streaking
            | WantToSeeSea
            | WantHotSpring
            | WantSauna
            | BathingTogether
            | OnADate
            | GotLost
            | Drinking
            | Smoking
            | SmokingWeed
            | JustHaveSex
            | Masturbating
            | MissedRide
            | UsingToilet
            | ShoweringTogether
            | SaunaTogether
            | UsingToiletTogether
            | SmokingTogether
            | HavingSexTogether
            | WalkingTogether
            | SmokingWeedTogether
            | JoggingTogether
            | YogaTogether
            | StargazingTogether
            | HorseRidingTogether
            | StreakingTogether
            | DrinkingTogether
            | HotSpringTogether
            | WorkingOutTogether
            | SwimmingTogether
            | CyclingTogether
            | DancingTogether
            | PlayingWithFireTogether
            | SingingTogether
            | SkinnyDippingTogether
            | CommittingCrimeTogether
            | NakedHorseRidingTogether
            | CustomTogether => EventCategory::Short,
            AsleepTogether | FightingTogether | HikingTogether | AdventuringTogether
            | FishingTogether | TempWork | ExMessage | GotHit | Breakup | WorkingOut | Reading
            | Gaming | AwayFromPhone | Unwell | Irritable | Busy | InBattle | Asleep
            | OutShopping | BrosGathering | SisGathering | OnTheRoad | WatchingShow | HavingSex
            | WantToGoOut | WantToGoHome | TakingSpace | FellOut | OnABoat | OnACruise | OnABus
            | OnATrain | OnAPlane | Driving => EventCategory::Mid,
            Sick | Period | Hospitalized | PostSurgery => EventCategory::Body,
            BusinessTrip | ShortTrip | LongTrip | HometownVisit | TravelingTogether => {
                EventCategory::Trip
            }
        }
    }

    pub fn is_background(self) -> bool {
        self.category().is_background()
    }

    /// The slot a row of this kind occupies while it is under way: the cycle
    /// for `period` and `period_scare`, the background for every other `body`
    /// or `trip` kind, the foreground for the rest.
    pub fn slot(self) -> Slot {
        match self {
            EventKind::Period | EventKind::PeriodScare => Slot::Cycle,
            k if k.is_background() => Slot::Background,
            _ => Slot::Foreground,
        }
    }

    /// Where the kind happens, as `outdoorsy` reads it (spec
    /// 2026-11-11-outdoorsy-missed-ride-and-accept-ends-incident-design.md
    /// §4): out in the open air, indoors, or either. Every kind that can
    /// take her offline is `Neutral`, so the key never moves how often she
    /// is out of reach.
    pub fn place(self) -> Place {
        use EventKind::*;
        match self {
            PoliceEncounter | AnimalEncounter | OnTheWay | GotBumped | AdmiringScenery
            | BirdEncounter | WantToSeeSea | WantHotSpring | GotLost | MissedRide | OnTheRoad
            | WantToGoOut | OnABoat | OnACruise | OnABus | OnATrain | OnAPlane | Driving => {
                Place::Outdoor
            }
            Showering | Bathing | ScoldedBySuperior | ScoldedSuperior | AdmiringArt | WantSauna
            | Reading | Gaming | WantToGoHome | UsingToilet => Place::Indoor,
            _ => Place::Neutral,
        }
    }

    /// Whether the message this kind sends when it starts is written in the
    /// `emotional` tone, when the deployment has the tone on (spec
    /// 2026-11-12-emotional-event-messages-design.md §2).
    pub fn speaks_emotionally(self) -> bool {
        use EventKind::*;
        matches!(
            self,
            Quarrel
                | ScoldedBySuperior
                | ScoldedSuperior
                | GotHit
                | HitSomeone
                | PoliceEncounter
                | ExWantsBack
                | Irritable
                | WantToQuitJob
                | Unwell
                | Sick
                | Period
                | SuddenDesire
                | Drinking
                | SmokingWeed
                | JustHaveSex
        )
    }

    /// A kind whose message can carry her picture.
    pub fn is_photo(self) -> bool {
        matches!(self, EventKind::TakingSelfie | EventKind::Strip)
    }

    /// Whether she does it with the user: the date, the trip together and
    /// every kind a together action places (spec
    /// 2026-11-24-focused-tone-four-together-actions-and-user-gender-design.md
    /// §7.2).
    pub fn is_shared(self) -> bool {
        use EventKind::*;
        matches!(
            self,
            TravelingTogether
                | OnADate
                | BathingTogether
                | ShoweringTogether
                | SaunaTogether
                | UsingToiletTogether
                | SmokingTogether
                | AsleepTogether
                | HavingSexTogether
                | WalkingTogether
                | SmokingWeedTogether
                | JoggingTogether
                | YogaTogether
                | StargazingTogether
                | HorseRidingTogether
                | StreakingTogether
                | DrinkingTogether
                | HotSpringTogether
                | FightingTogether
                | WorkingOutTogether
                | SwimmingTogether
                | CyclingTogether
                | HikingTogether
                | DancingTogether
                | AdventuringTogether
                | PlayingWithFireTogether
                | SingingTogether
                | SkinnyDippingTogether
                | CommittingCrimeTogether
                | FishingTogether
                | NakedHorseRidingTogether
                | CustomTogether
        )
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

    /// Whether the kind belongs in a persona's world (spec
    /// 2026-11-08-strip-and-world-setting-design.md §4): the kinds that need
    /// modern life fit only `modern`, among them the five modern-life ones
    /// (`saw_bag`, `saw_clothes`, `out_shopping`, `want_to_quit_job`,
    /// `admiring_art`; spec 2026-11-24-focused-tone-four-together-actions-
    /// and-user-gender-design.md §4) and `cycling_together` (§6);
    /// `on_the_road` fits only `fantasy`, where the vehicles do not exist
    /// (spec 2026-11-19-vehicles-actions-species-design.md §3), and so does
    /// `fighting_together` (spec 2026-11-23-hot-spring-fight-challenge-bet-
    /// design.md §5); every other kind fits both.
    pub fn fits(self, setting: Setting) -> bool {
        use EventKind::*;
        let modern_only = matches!(
            self,
            PoliceEncounter
                | Showering
                | TakingSelfie
                | Gaming
                | WatchingShow
                | BusinessTrip
                | Hospitalized
                | PostSurgery
                | TempWork
                | ScoldedSuperior
                | OnACruise
                | OnABus
                | OnATrain
                | OnAPlane
                | Driving
                | SawBag
                | SawClothes
                | OutShopping
                | WantToQuitJob
                | AdmiringArt
                | CyclingTogether
        );
        if modern_only {
            setting == Setting::Modern
        } else if matches!(self, OnTheRoad | FightingTogether) {
            setting == Setting::Fantasy
        } else {
            true
        }
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
        Showering => (2.0, 12, Lasts::Minutes(30)),
        Bathing => (1.0, 48, Lasts::Hours(1, 1)),
        OnTheWay => (2.0, 12, Lasts::Minutes(30)),
        // The `Lasts` of `period` and `period_scare` only bound `lookback()`:
        // `draw` sets their real length from the cycle (spec
        // 2026-11-24-period-cycle-design.md §5, §6). A scare starts at most
        // three days before its due moment and ends at most three after.
        PeriodScare => (0.5, 0, Lasts::Hours(DAY, 6 * DAY)),
        PregnancyScare => (0.5, 60 * DAY, Lasts::Hours(1, 1)),
        GotBumped => (1.0, 72, Lasts::Minutes(30)),
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
        WatchingShow => (1.0, 72, Lasts::Hours(2, 3)),
        HavingSex => (1.0, 48, Lasts::Hours(1, 2)),
        WantToGoOut => (1.0, 24, Lasts::Hours(1, 3)),
        WantToGoHome => (1.0, 24, Lasts::Hours(1, 3)),
        Sick => (3.0, 14 * DAY, Lasts::Hours(DAY, 3 * DAY)),
        Period => (3.0, 28 * DAY, Lasts::Hours(DAY, 5 * DAY)),
        Hospitalized => (0.5, 60 * DAY, Lasts::Hours(2 * DAY, 5 * DAY)),
        PostSurgery => (0.5, 90 * DAY, Lasts::Hours(3 * DAY, 7 * DAY)),
        BusinessTrip => (3.0, 0, Lasts::Hours(DAY, DAY)),
        ShortTrip => (2.0, 0, Lasts::Hours(2 * DAY, 2 * DAY)),
        LongTrip => (1.0, 0, Lasts::Hours(3 * DAY, 7 * DAY)),
        HometownVisit => (1.0, 0, Lasts::Hours(3 * DAY, 3 * DAY)),
        TakingSpace => (0.0, 0, Lasts::Hours(12, 24)),
        FellOut => (0.0, 0, Lasts::Hours(1, 1)),
        TravelingTogether => (0.0, 0, Lasts::Hours(2 * DAY, 3 * DAY)),
        Strip => (1.0, 48, Lasts::Minutes(30)),
        BirdEncounter => (2.0, 12, Lasts::Minutes(30)),
        ExWantsBack => (0.5, 30 * DAY, Lasts::Hours(1, 1)),
        WantToQuitJob => (1.0, 7 * DAY, Lasts::Hours(1, 1)),
        PonderingLife => (2.0, 48, Lasts::Hours(1, 1)),
        Streaking => (0.5, 30 * DAY, Lasts::Minutes(30)),
        WantToSeeSea => (2.0, 72, Lasts::Hours(1, 1)),
        WantHotSpring => (1.0, 72, Lasts::Hours(1, 1)),
        WantSauna => (1.0, 72, Lasts::Hours(1, 1)),
        BathingTogether => (0.0, 0, Lasts::Minutes(30)),
        // Never drawn: the activity sets its own end.
        OnADate => (0.0, 0, Lasts::Hours(1, 4)),
        GotLost => (3.0, 48, Lasts::Hours(1, 1)),
        Drinking => (2.0, 12, Lasts::Minutes(30)),
        Smoking => (2.0, 12, Lasts::Minutes(30)),
        SmokingWeed => (2.0, 12, Lasts::Minutes(30)),
        JustHaveSex => (1.0, 48, Lasts::Minutes(30)),
        Masturbating => (1.0, 24, Lasts::Minutes(30)),
        MissedRide => (3.0, 48, Lasts::Hours(1, 1)),
        OnABoat => (1.0, 48, Lasts::Hours(1, 2)),
        OnACruise => (1.0, 48, Lasts::Hours(2, 4)),
        OnABus => (1.0, 48, Lasts::Hours(2, 4)),
        OnATrain => (1.0, 48, Lasts::Hours(2, 4)),
        OnAPlane => (1.0, 48, Lasts::Hours(2, 4)),
        Driving => (1.0, 48, Lasts::Hours(1, 3)),
        UsingToilet => (1.0, 12, Lasts::Minutes(30)),
        ShoweringTogether => (0.0, 0, Lasts::Minutes(30)),
        SaunaTogether => (0.0, 0, Lasts::Hours(1, 1)),
        UsingToiletTogether => (0.0, 0, Lasts::Minutes(30)),
        SmokingTogether => (0.0, 0, Lasts::Minutes(30)),
        AsleepTogether => (0.0, 0, Lasts::Hours(1, 4)),
        HavingSexTogether => (0.0, 0, Lasts::Hours(1, 1)),
        WalkingTogether => (0.0, 0, Lasts::Hours(1, 1)),
        SmokingWeedTogether => (0.0, 0, Lasts::Minutes(30)),
        JoggingTogether => (0.0, 0, Lasts::Hours(1, 1)),
        YogaTogether => (0.0, 0, Lasts::Hours(1, 1)),
        StargazingTogether => (0.0, 0, Lasts::Hours(1, 1)),
        HorseRidingTogether => (0.0, 0, Lasts::Hours(1, 1)),
        StreakingTogether => (0.0, 0, Lasts::Minutes(30)),
        DrinkingTogether => (0.0, 0, Lasts::Hours(1, 1)),
        HotSpringTogether => (0.0, 0, Lasts::Hours(1, 1)),
        FightingTogether => (0.0, 0, Lasts::Hours(1, 2)),
        WorkingOutTogether => (0.0, 0, Lasts::Hours(1, 1)),
        SwimmingTogether => (0.0, 0, Lasts::Hours(1, 1)),
        CyclingTogether => (0.0, 0, Lasts::Hours(1, 1)),
        HikingTogether => (0.0, 0, Lasts::Hours(2, 3)),
        DancingTogether => (0.0, 0, Lasts::Hours(1, 1)),
        AdventuringTogether => (0.0, 0, Lasts::Hours(2, 3)),
        PlayingWithFireTogether => (0.0, 0, Lasts::Hours(1, 1)),
        SingingTogether => (0.0, 0, Lasts::Hours(1, 1)),
        SkinnyDippingTogether => (0.0, 0, Lasts::Hours(1, 1)),
        CommittingCrimeTogether => (0.0, 0, Lasts::Hours(1, 1)),
        FishingTogether => (0.0, 0, Lasts::Hours(2, 3)),
        NakedHorseRidingTogether => (0.0, 0, Lasts::Hours(1, 1)),
        CustomTogether => (0.0, 0, Lasts::Hours(1, 1)),
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

/// The world a persona lives in, `art_metadata.setting` (spec
/// 2026-11-08-strip-and-world-setting-design.md §4). Absent or unknown reads
/// as `Modern`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Setting {
    #[default]
    Modern,
    Fantasy,
}

impl Setting {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "modern" => Some(Setting::Modern),
            "fantasy" => Some(Setting::Fantasy),
            _ => None,
        }
    }
}

/// How much of her day is work, `art_metadata.employed` (spec
/// 2026-11-11-eight-short-events-and-hygienic-design.md §4). Absent or
/// unknown reads as `PartTime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Employment {
    Free,
    #[default]
    PartTime,
    FullTime,
}

impl Employment {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "free" => Some(Employment::Free),
            "part_time" => Some(Employment::PartTime),
            "full_time" => Some(Employment::FullTime),
            _ => None,
        }
    }
}

/// Where a foreground kind happens (spec 2026-11-11-outdoorsy-missed-ride-
/// and-accept-ends-incident-design.md §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Outdoor,
    Neutral,
    Indoor,
}

/// `art_metadata.outdoorsy`: whether she leans outdoors or stays in (spec
/// 2026-11-11-outdoorsy-missed-ride-and-accept-ends-incident-design.md §5).
/// Absent or unknown reads as `Neutral`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Outdoorsy {
    Outdoor,
    #[default]
    Neutral,
    Indoor,
}

impl Outdoorsy {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "outdoor" => Some(Outdoorsy::Outdoor),
            "neutral" => Some(Outdoorsy::Neutral),
            "indoor" => Some(Outdoorsy::Indoor),
            _ => None,
        }
    }
}

/// The persona's settings the draw reads (§4). `Default` is a persona with
/// none of them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PersonaTraits {
    /// `art_metadata.gender` is `"female"`: the gate on the shopping finds
    /// and `pregnancy_scare`, and what takes `out_shopping` offline.
    pub female: bool,
    pub health: Health,
    pub prefer_trip: bool,
    /// Weighs only `mid` kinds; any other kind listed has no effect.
    pub prefer_events: Vec<EventKind>,
    /// `art_metadata.is_virgin`. Her virginity as the draw sees it is this
    /// and no `having_sex` or `just_have_sex` row ever
    /// (`DrawInput::ever_had_sex`); the first such row ends it with no write.
    pub is_virgin: bool,
    /// `art_metadata.homesick`: `true`, her home is her base; `false`, she
    /// roams; `None`, no preference. Steers where a trip goes (§5).
    pub homesick: Option<bool>,
    /// `art_metadata.sex_addict`: `true` or `false` scales `sudden_desire`,
    /// `strip`, `streaking` and `just_have_sex`; `None` leaves them be. Only
    /// `true` draws `having_sex` and `masturbating`.
    pub sex_addict: Option<bool>,
    /// `art_metadata.setting`: the kinds that do not fit her world are never
    /// drawn.
    pub setting: Setting,
    /// `art_metadata.hygienic`: `true` or `false` scales the bathing kinds;
    /// `None` leaves them be.
    pub hygienic: Option<bool>,
    /// `art_metadata.employed`: scales the work kinds; `Free` never wants to
    /// quit a job.
    pub employed: Employment,
    /// What she is hooked on (`smoking`, `alcoholic`, `drug`, …): the gates
    /// on `drinking`, `smoking` and `smoking_weed`.
    pub habits: Habits,
    /// `art_metadata.outdoorsy`: scales the outdoor and indoor kinds.
    pub outdoorsy: Outdoorsy,
    /// `art_metadata.species` is `robot` or `android`: never draws
    /// `pregnancy_scare` or `using_toilet`, and never has a period (spec
    /// 2026-11-19-vehicles-actions-species-design.md §5; spec 2026-11-20-
    /// together-actions-state-in-picture-and-genome-routes-design.md §4).
    pub machine: bool,
    /// She has a period (spec 2026-11-24-period-cycle-design.md §4): a female
    /// human unless the card says `period: false`, a female of another
    /// species that is no machine only when it says `period: true`. The gate
    /// on `period` and `period_scare`, and what gives her a cycle.
    pub period: bool,
}

impl PersonaTraits {
    /// `Some(health)` for a persona with a period: what the cycle reads to
    /// size it. `None` for one with no cycle.
    pub fn cycle_health(&self) -> Option<Health> {
        self.period.then_some(self.health)
    }
}

fn passes_gate(
    kind: EventKind,
    rung: u8,
    persona: &PersonaTraits,
    romance_compatible: bool,
) -> bool {
    match kind {
        EventKind::Strip | EventKind::Streaking => rung >= DESIRE_MIN_RUNG && romance_compatible,
        EventKind::ExMessage | EventKind::Breakup | EventKind::ExWantsBack => {
            rung >= CONFIDE_MIN_RUNG
        }
        EventKind::SuddenDesire | EventKind::JustHaveSex => rung >= DESIRE_MIN_RUNG,
        EventKind::HavingSex => persona.sex_addict == Some(true),
        EventKind::Masturbating => persona.sex_addict == Some(true) && rung >= DESIRE_MIN_RUNG,
        EventKind::Drinking => persona.habits.alcoholic == Some(true),
        EventKind::Smoking => persona.habits.smoking == Some(true),
        EventKind::SmokingWeed => persona.habits.drug == Some(true),
        EventKind::WantToQuitJob => persona.employed != Employment::Free,
        EventKind::SawBag | EventKind::SawClothes => persona.female,
        EventKind::Period => persona.period,
        EventKind::PeriodScare => persona.period && rung >= CONFIDE_MIN_RUNG,
        EventKind::PregnancyScare => persona.female && !persona.machine && rung >= DESIRE_MIN_RUNG,
        EventKind::UsingToilet => !persona.machine,
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
        ExMessage if n.emotional == NeedLevel::High => 3.0,
        AnimalEncounter | Irritable | Quarrel if n.emotional == NeedLevel::High => 2.0,
        WantToGoOut if n.outing == NeedLevel::High => 3.0,
        WantToSeeSea if n.outing == NeedLevel::High => 2.0,
        WantToGoHome if n.outing == NeedLevel::Low => 3.0,
        _ => 1.0,
    }
}

/// How the craving for what she's hooked on scales a kind within its
/// category (spec 2026-10-28-outing-indulgence-needs-and-habits-design.md
/// §5.2): it sends her out and frays her temper. Its own factor, so it
/// multiplies with `needs_factor`. 1 without a rating.
fn indulgence_factor(kind: EventKind, needs: Option<&Needs>) -> f64 {
    match (kind, needs) {
        (EventKind::WantToGoOut | EventKind::Irritable, Some(n))
            if n.indulgence == NeedLevel::High =>
        {
            2.0
        }
        (EventKind::Drinking | EventKind::Smoking | EventKind::SmokingWeed, Some(n))
            if n.indulgence == NeedLevel::High =>
        {
            3.0
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
        (ExMessage | SuddenDesire | Strip, HavingSex | JustHaveSex)
            | (SuddenDesire, Strip | Masturbating)
            | (Sick, PeriodScare)
            | (HavingSex | JustHaveSex | HavingSexTogether, PregnancyScare)
            | (BusinessTrip, OnTheWay | Busy | TempWork)
            | (
                ShortTrip | LongTrip | HometownVisit | TravelingTogether,
                OnTheRoad | AdmiringScenery | TakingSelfie
            )
            | (HometownVisit, BrosGathering | SisGathering)
            | (Hospitalized | PostSurgery, Reading | Gaming)
            | (Period, Irritable | Unwell)
            | (Breakup | Quarrel | ScoldedBySuperior | InBattle, Irritable)
            | (GotHit | GotBumped, Unwell)
            | (WorkingOut, Showering | Bathing)
            | (ExMessage | Breakup, ExWantsBack)
            | (ScoldedBySuperior | TempWork, WantToQuitJob)
            | (Breakup | Quarrel | WantToQuitJob, PonderingLife)
            | (PonderingLife, WantToSeeSea)
    )
}

/// `FOLLOW_FACTOR` once for every distinct live prior of `kind`: under way
/// at `now`, or ended within `FOLLOW_WINDOW_HOURS` before it. `latest_period`
/// is the period the cycle derives, which has no row in `history` (spec
/// 2026-11-24-period-cycle-design.md §7): one more prior, and the same one as
/// a `period` row of `history` when both are there.
fn follow_factor(
    kind: EventKind,
    history: &[PastEvent],
    latest_period: Option<&PastEvent>,
    now: DateTime<Utc>,
) -> f64 {
    let since = now - Duration::hours(FOLLOW_WINDOW_HOURS);
    let mut live: Vec<EventKind> = history
        .iter()
        .chain(latest_period)
        .filter(|e| e.ends_at > since && follows(e.kind, kind))
        .map(|e| e.kind)
        .collect();
    live.sort_by_key(|k| k.as_str());
    live.dedup();
    FOLLOW_FACTOR.powi(live.len() as i32)
}

/// `UNLIKELY_FACTOR` for `having_sex` and `just_have_sex` while she is a virgin, and for
/// `pregnancy_scare` while she has never had sex (spec §3, §4).
fn prior_factor(kind: EventKind, persona: &PersonaTraits, ever_had_sex: bool) -> f64 {
    match kind {
        EventKind::HavingSex | EventKind::JustHaveSex if persona.is_virgin && !ever_had_sex => {
            UNLIKELY_FACTOR
        }
        EventKind::PregnancyScare if !ever_had_sex => UNLIKELY_FACTOR,
        _ => 1.0,
    }
}

/// `FOLLOW_FACTOR`, once, for `period_scare` when any kind of sex started
/// since `window` opened, the three days before her next period is due (spec
/// 2026-11-24-period-cycle-design.md §6); 1 for every other kind, and without
/// a window.
fn sex_before_period_factor(
    kind: EventKind,
    history: &[PastEvent],
    window: Option<DateTime<Utc>>,
) -> f64 {
    match window {
        Some(since) if kind == EventKind::PeriodScare && had_sex_since(history, since) => {
            FOLLOW_FACTOR
        }
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

/// How `sex_addict` scales `sudden_desire`, `strip`, `streaking` and
/// `just_have_sex` within their category:
/// three times or a third as likely. Their gates hold either way.
fn sex_addict_factor(kind: EventKind, persona: &PersonaTraits) -> f64 {
    match (kind, persona.sex_addict) {
        (
            EventKind::SuddenDesire
            | EventKind::Strip
            | EventKind::Streaking
            | EventKind::JustHaveSex,
            Some(true),
        ) => PREFERENCE_FACTOR,
        (
            EventKind::SuddenDesire
            | EventKind::Strip
            | EventKind::Streaking
            | EventKind::JustHaveSex,
            Some(false),
        ) => 1.0 / PREFERENCE_FACTOR,
        _ => 1.0,
    }
}

/// How `hygienic` scales the bathing kinds within their category: three
/// times or a third as likely.
fn hygienic_factor(kind: EventKind, persona: &PersonaTraits) -> f64 {
    use EventKind::*;
    match (kind, persona.hygienic) {
        (Showering | Bathing | WantHotSpring | WantSauna, Some(true)) => PREFERENCE_FACTOR,
        (Showering | Bathing | WantHotSpring | WantSauna, Some(false)) => 1.0 / PREFERENCE_FACTOR,
        _ => 1.0,
    }
}

/// How `employed` scales the work kinds within their category: a third as
/// likely for a persona with no job, three times for a full-time one.
fn employed_factor(kind: EventKind, persona: &PersonaTraits) -> f64 {
    use EventKind::*;
    match (kind, persona.employed) {
        (TempWork | ScoldedBySuperior | ScoldedSuperior | BusinessTrip, Employment::Free) => {
            1.0 / PREFERENCE_FACTOR
        }
        (TempWork | ScoldedBySuperior | ScoldedSuperior | BusinessTrip, Employment::FullTime) => {
            PREFERENCE_FACTOR
        }
        _ => 1.0,
    }
}

/// How `outdoorsy` scales a kind within its category by its place: three
/// times her side's kinds, a third the other side's; neutral kinds and a
/// neutral persona are left be.
fn outdoorsy_factor(kind: EventKind, persona: &PersonaTraits) -> f64 {
    match (persona.outdoorsy, kind.place()) {
        (Outdoorsy::Outdoor, Place::Outdoor) | (Outdoorsy::Indoor, Place::Indoor) => {
            PREFERENCE_FACTOR
        }
        (Outdoorsy::Outdoor, Place::Indoor) | (Outdoorsy::Indoor, Place::Outdoor) => {
            1.0 / PREFERENCE_FACTOR
        }
        _ => 1.0,
    }
}

/// The backgrounds a kind can happen only during, or `None` for a kind
/// that needs none (spec 2026-11-11-stocks-lost-habit-events-and-just-have-
/// sex-design.md §2.1): she gets lost only on a trip; she misses a ride on
/// any trip, a business trip included (spec 2026-11-11-outdoorsy-missed-
/// ride-and-accept-ends-incident-design.md §3), and is on her way or on the
/// road only then (spec 2026-11-16-prune-life-events-design.md §4.2). The
/// vehicles ride on any trip, except a cruise, which is never business (spec
/// 2026-11-19-vehicles-actions-species-design.md §2).
fn needs_background(kind: EventKind) -> Option<&'static [EventKind]> {
    use EventKind::*;
    match kind {
        GotLost | OnACruise => Some(&[ShortTrip, LongTrip, HometownVisit, TravelingTogether]),
        MissedRide | OnTheWay | OnTheRoad | OnABoat | OnABus | OnATrain | OnAPlane | Driving => {
            Some(&[
                BusinessTrip,
                ShortTrip,
                LongTrip,
                HometownVisit,
                TravelingTogether,
            ])
        }
        _ => None,
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
                | OnTheWay
                | InBattle
                | GotBumped
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
                | BirdEncounter
                | Streaking
                | GotLost
                | Drinking
                | Smoking
                | SmokingWeed
                | JustHaveSex
        ),
        Period => kind.is_false_alarm(),
        // She is with the user: nothing takes her out of reach.
        TravelingTogether => kind.may_be_offline() || kind == JustHaveSex,
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
    /// Whether the instance has a row of any kind in [`EventKind::SEX`], ever
    /// (not bounded by `lookback()`).
    pub ever_had_sex: bool,
    /// Whether the session's scopes are romance-compatible
    /// (`scope::romance_compatible`): the gate on `strip`.
    pub romance_compatible: bool,
    /// The newest `period` and `period_scare` rows started at or before
    /// `now`, however old (spec 2026-11-24-period-cycle-design.md §5);
    /// `CycleRows::default()` for a persona with no period.
    pub cycle: CycleRows,
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

/// Whether `history` holds a kind of [`EventKind::SEX`] that started at or
/// after `since`.
fn had_sex_since(history: &[PastEvent], since: DateTime<Utc>) -> bool {
    history
        .iter()
        .any(|e| EventKind::SEX.contains(&e.kind) && e.started_at >= since)
}

/// The kind drawn, or `None` (§5). Her cycle (spec
/// 2026-11-24-period-cycle-design.md §5–§7) enters as `input.cycle`: a
/// period under way keeps backgrounds out and the false alarms ruled out as
/// a `period` background does, `period` is drawn only without an anchor and
/// `period_scare` only in the days before her next period is due.
pub fn draw(input: &DrawInput, rolls: Rolls) -> Option<Drawn> {
    let under_way = |e: &&PastEvent| e.ends_at > input.now;
    if input
        .history
        .iter()
        .filter(under_way)
        .any(|e| e.kind.slot() == Slot::Foreground)
    {
        return None;
    }
    // A persona with no period has no cycle, whatever rows her instance holds.
    let cycle = if input.persona.period {
        input.cycle
    } else {
        CycleRows::default()
    };
    let health = input.persona.health;
    let cycle_state = cycle::state(&cycle, health, input.now);
    let on_period = cycle_state.is_some_and(|s| s.kind == EventKind::Period);
    let latest_period = cycle::latest_period(&cycle, health, input.now);
    // Her next due moment, and the window before it that a scare can start in.
    let due = cycle
        .period
        .map(|anchor| cycle::next_due(anchor.started_at, input.now));
    let window = due.map(|due| due - Duration::days(cycle::SCARE_WINDOW_DAYS));
    let scare_allowed = cycle_state.is_none() && window.is_some_and(|w| input.now >= w);
    let background = input
        .history
        .iter()
        .filter(under_way)
        .find(|e| e.kind.slot() == Slot::Background)
        .map(|e| e.kind);
    let cooling =
        |started_at: DateTime<Utc>, hours: i64| started_at + Duration::hours(hours) > input.now;
    let eligible: Vec<EventKind> = EventKind::ALL
        .into_iter()
        .filter(|k| input.allowed.contains(k))
        .filter(|k| {
            passes_gate(
                *k,
                input.intimacy_rung,
                input.persona,
                input.romance_compatible,
            )
        })
        .filter(|k| k.fits(input.persona.setting))
        .filter(|k| match k {
            EventKind::Period => cycle.period.is_none(),
            EventKind::PeriodScare => scare_allowed,
            _ => true,
        })
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
        .filter(|k| {
            let kept_out = (background.is_some() || on_period) && k.is_background();
            let ruled_out = background.is_some_and(|b| rules_out(b, *k))
                || (on_period && rules_out(EventKind::Period, *k));
            !kept_out && !ruled_out
        })
        .filter(|k| match needs_background(*k) {
            Some(during) => input
                .history
                .iter()
                .filter(under_way)
                .any(|e| during.contains(&e.kind)),
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
                * follow_factor(k, input.history, latest_period.as_ref(), input.now)
                * prior_factor(k, input.persona, input.ever_had_sex)
                * homesick_factor(k, input.persona)
                * sex_addict_factor(k, input.persona)
                * hygienic_factor(k, input.persona)
                * employed_factor(k, input.persona)
                * outdoorsy_factor(k, input.persona)
                * sex_before_period_factor(k, input.history, window);
            (k, weight)
        })
        .collect();
    let kind = pick(&kinds, rolls.kind)?;
    let lasts = match (kind, due) {
        (EventKind::Period, _) => Duration::days(cycle::period_days(health)),
        (EventKind::PeriodScare, Some(due)) => cycle::scare_ends(due, rolls.duration) - input.now,
        _ => lasts(kind, rolls.duration),
    };
    Some(Drawn { kind, lasts })
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

/// How long `kind` lasts for a uniform `roll` in [0, 1). The table only
/// bounds `period` and `period_scare` (for `lookback()`); `draw` sets their
/// length from the cycle.
pub fn lasts(kind: EventKind, roll: f64) -> Duration {
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
        .chain(EventKind::SPECIAL)
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
/// the placed kinds (`EventKind::SPECIAL`).
pub fn is_told(kind: EventKind, rung: u8, persona: &PersonaTraits) -> bool {
    match kind {
        k if EventKind::SPECIAL.contains(&k) => false,
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
            period: true,
            ..Default::default()
        }
    }

    fn needs(emotional: NeedLevel) -> Option<Needs> {
        Some(Needs {
            social: NeedLevel::Mid,
            emotional,
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
                romance_compatible: false,
                cycle: CycleRows::default(),
            },
            r,
        )
        .map(|d| d.kind)
    }

    /// `pick_with` for a female persona whose pair is romance-compatible.
    fn pick_romantic(rung: u8, allowed: &[EventKind], r: Rolls) -> Option<EventKind> {
        draw(
            &DrawInput {
                now: now(),
                needs: None,
                intimacy_rung: rung,
                persona: &female(),
                history: &[],
                allowed,
                ever_had_sex: false,
                romance_compatible: true,
                cycle: CycleRows::default(),
            },
            r,
        )
        .map(|d| d.kind)
    }

    /// A female persona at rung 3 without needs: every gate open.
    fn pick(history: &[PastEvent], allowed: &[EventKind], r: Rolls) -> Option<EventKind> {
        pick_with(&female(), None, 3, history, allowed, r)
    }

    /// A draw for `persona` with no needs, handed the cycle rows.
    fn draw_cycle(
        persona: &PersonaTraits,
        cycle: CycleRows,
        history: &[PastEvent],
        allowed: &[EventKind],
        rung: u8,
        r: Rolls,
    ) -> Option<Drawn> {
        draw(
            &DrawInput {
                now: now(),
                needs: None,
                intimacy_rung: rung,
                persona,
                history,
                allowed,
                ever_had_sex: false,
                romance_compatible: false,
                cycle,
            },
            r,
        )
    }

    /// The anchor row: a three-day `period` that started `hours_ago`.
    fn anchor(hours_ago: i64) -> PastEvent {
        past(Period, hours_ago, 72)
    }

    /// An anchor whose next due moment is `now() + 24h`: inside the scare
    /// window.
    fn due_tomorrow() -> PastEvent {
        anchor(27 * 24)
    }

    /// An anchor whose period 1 started an hour ago: a derived period is
    /// under way.
    fn on_period() -> PastEvent {
        anchor(28 * 24 + 1)
    }

    /// The cycle rows of an instance whose only row is `anchor`.
    fn cycle_of(anchor: PastEvent) -> CycleRows {
        CycleRows {
            period: Some(anchor),
            scare: None,
        }
    }

    #[test]
    fn nothing_is_drawn_while_a_foreground_event_is_under_way() {
        let h = [past(TempWork, 1, 4)];
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
            GotBumped,
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
        let only = [Reading];
        let cooling = [PastEvent {
            started_at: now() - Duration::hours(12) + Duration::seconds(1),
            ..past(Reading, 12, 2)
        }];
        assert_eq!(pick(&cooling, &only, rolls(0.0, 0.0)), None);
        let ready = [past(Reading, 12, 2)];
        assert_eq!(pick(&ready, &only, rolls(0.0, 0.0)), Some(Reading));
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
        use NeedLevel::High;
        let cases: [(&[EventKind], Option<Needs>, f64, EventKind); 12] = [
            // temp_work 3 : ex 1 — 0.6 → 2.4 → temp_work.
            (&[ExMessage, TempWork], None, 0.6, TempWork),
            // emotional high: 3 : 3 — 0.6 → 3.6 → ex.
            (&[ExMessage, TempWork], needs(High), 0.6, ExMessage),
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
                needs(High),
                0.2,
                AnimalEncounter,
            ),
            // unwell 1 : irritable 1 — 0.6 → 1.2 → irritable.
            (&[Unwell, Irritable], None, 0.6, Irritable),
            // 1 : 1 — 0.4 → 0.8 → unwell.
            (&[Unwell, Irritable], None, 0.4, Unwell),
            // emotional high: 1 : 2 — 0.4 → 1.2 → irritable.
            (&[Unwell, Irritable], needs(High), 0.4, Irritable),
            // temp_work 3 : reading 2 — 0.3 → 1.5 → temp_work.
            (&[TempWork, Reading], None, 0.3, TempWork),
            // working_out 2 : reading 2 — 0.4 → 1.6 → working_out.
            (&[WorkingOut, Reading], None, 0.4, WorkingOut),
            // sick 3 : hospitalized 0.5 — 0.9 → 3.15 → hospitalized.
            (&[Sick, Hospitalized], None, 0.9, Hospitalized),
            // showering 2 : selfie 2 — 0.6 → 2.4 → selfie.
            (&[Showering, TakingSelfie], None, 0.6, TakingSelfie),
            // police 1 : bathing 1 — 0.6 → 1.2 → bathing.
            (&[PoliceEncounter, Bathing], None, 0.6, Bathing),
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
    /// police 1, animal 3, hit 0.5, desire 2, showering 2, bathing 1 (9.5);
    /// on_the_way needs a trip.
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
        let b = |w: f64| w / 9.5;
        assert_eq!(at(0.0, b(1.0) - E), PoliceEncounter);
        assert_eq!(at(0.0, b(1.0) + E), AnimalEncounter);
        assert_eq!(at(0.0, b(4.0) - E), AnimalEncounter);
        assert_eq!(at(0.0, b(4.0) + E), HitSomeone);
        assert_eq!(at(0.0, b(4.5) - E), HitSomeone);
        assert_eq!(at(0.0, b(4.5) + E), SuddenDesire);
        assert_eq!(at(0.0, b(6.5) - E), SuddenDesire);
        assert_eq!(at(0.0, b(6.5) + E), Showering);
        assert_eq!(at(0.0, b(8.5) + E), Bathing);
        assert_eq!(at(0.0, 1.0), Bathing);
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
                    romance_compatible: false,
                    cycle: CycleRows::default(),
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
                HometownVisit,
                TravelingTogether
            ]
        );
    }

    #[test]
    fn every_category_holds_its_kinds() {
        let count = |c| EventKind::ALL.iter().filter(|k| k.category() == c).count();
        assert_eq!(
            EventCategory::ALL.map(count),
            [35, 27, 4, 4],
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

    /// The kinds added after the v2 kinds; tests pinned to the weights of
    /// those draw from `earlier()`.
    const ADDED: [EventKind; 44] = [
        UsingToilet,
        OnABoat,
        OnACruise,
        OnABus,
        OnATrain,
        OnAPlane,
        Driving,
        MissedRide,
        GotLost,
        Drinking,
        Smoking,
        SmokingWeed,
        JustHaveSex,
        Masturbating,
        BirdEncounter,
        ExWantsBack,
        WantToQuitJob,
        PonderingLife,
        Streaking,
        WantToSeeSea,
        WantHotSpring,
        WantSauna,
        PeriodScare,
        PregnancyScare,
        Busy,
        InBattle,
        Asleep,
        GotBumped,
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
            // A late-period scare is drawn only in the days before her
            // period is due.
            let cycle = if k == PeriodScare {
                cycle_of(due_tomorrow())
            } else {
                CycleRows::default()
            };
            let kind = |p: &PersonaTraits, rung| {
                draw_cycle(p, cycle, &[], &[k], rung, rolls(0.0, 0.0)).map(|d| d.kind)
            };
            assert_eq!(kind(&female(), min_rung - 1), None, "{k:?} below its rung");
            assert_eq!(kind(&female(), min_rung), Some(k), "{k:?} at its rung");
            assert_eq!(kind(&male, 3), None, "{k:?} needs a female persona");
        }
    }

    #[test]
    fn a_period_rules_the_false_alarms_out() {
        // The anchor row under way is her period: it is in the cycle rows
        // and in the history, as the server reads it.
        let period = past(Period, 24, 96);
        let h = [period];
        let kind = |cycle: CycleRows, history: &[PastEvent], k: EventKind| {
            draw_cycle(&female(), cycle, history, &[k], 3, rolls(0.0, 0.0)).map(|d| d.kind)
        };
        for k in [PeriodScare, PregnancyScare] {
            assert_eq!(kind(cycle_of(period), &h, k), None, "{k:?}");
        }
        assert_eq!(
            kind(cycle_of(period), &h, Reading),
            Some(Reading),
            "an incident is still drawn"
        );
        let trip = [past(BusinessTrip, 2, 24)];
        assert_eq!(
            kind(cycle_of(due_tomorrow()), &trip, PeriodScare),
            Some(PeriodScare)
        );
    }

    /// Spec 2026-11-24-period-cycle-design.md §5.
    #[test]
    fn period_is_drawn_once() {
        // No anchor: drawn, and it lasts by health.
        for (health, days) in [(Health::Strong, 1), (Health::Normal, 3), (Health::Weak, 5)] {
            let persona = PersonaTraits { health, ..female() };
            let d = draw_cycle(
                &persona,
                CycleRows::default(),
                &[],
                &[Period],
                3,
                rolls(0.0, 0.0),
            )
            .unwrap();
            assert_eq!(
                (d.kind, d.lasts),
                (Period, Duration::days(days)),
                "{health:?}"
            );
        }
        // With an anchor, however old: never again.
        let rows = cycle_of(anchor(400 * 24));
        assert_eq!(
            draw_cycle(&female(), rows, &[], &[Period], 3, rolls(0.0, 0.0)),
            None
        );
    }

    /// Spec 2026-11-24-period-cycle-design.md §6.
    #[test]
    fn a_scare_is_drawn_only_in_the_three_days_before_her_period() {
        let scare = |rows: CycleRows, persona: &PersonaTraits, rung: u8, duration: f64| {
            draw_cycle(
                persona,
                rows,
                &[],
                &[PeriodScare],
                rung,
                Rolls {
                    category: 0.0,
                    kind: 0.0,
                    duration,
                },
            )
        };
        let due = cycle_of(due_tomorrow());
        let d = scare(due, &female(), 2, 0.0).unwrap();
        assert_eq!((d.kind, d.lasts), (PeriodScare, Duration::hours(24 + 24)));
        let d = scare(due, &female(), 2, 0.99).unwrap();
        assert_eq!(d.lasts, Duration::hours(24 + 72));
        // Mid-cycle.
        assert_eq!(scare(cycle_of(anchor(10 * 24)), &female(), 2, 0.0), None);
        // No anchor.
        assert_eq!(scare(CycleRows::default(), &female(), 2, 0.0), None);
        // The rung gate.
        assert_eq!(scare(due, &female(), 1, 0.0), None);
        let no_trait = PersonaTraits {
            period: false,
            ..female()
        };
        assert_eq!(scare(due, &no_trait, 2, 0.0), None);
        // The window opens exactly three days before the due moment.
        assert!(scare(cycle_of(anchor(25 * 24)), &female(), 2, 0.0).is_some());
        assert_eq!(
            scare(cycle_of(anchor(25 * 24 - 1)), &female(), 2, 0.0),
            None
        );
    }

    #[test]
    fn sex_before_her_period_makes_a_scare_likelier() {
        // Short, in draw order: animal_encounter 3.0 then period_scare 0.5. A
        // kind roll of 0.7 lands on the animal (2.45 of 3.5) and, with the
        // scare at 1.5, on the scare (3.15 of 4.5).
        let rows = cycle_of(due_tomorrow());
        let both = [AnimalEncounter, PeriodScare];
        let kind = |h: &[PastEvent]| {
            draw_cycle(&female(), rows, h, &both, 2, rolls(0.0, 0.7)).map(|d| d.kind)
        };
        assert_eq!(kind(&[]), Some(AnimalEncounter));
        assert_eq!(kind(&[past(JustHaveSex, 24, 1)]), Some(PeriodScare));
        assert_eq!(kind(&[past(HavingSexTogether, 24, 1)]), Some(PeriodScare));
        // Before the window.
        assert_eq!(kind(&[past(JustHaveSex, 5 * 24, 1)]), Some(AnimalEncounter));
        // Once, however many: the scare weighs 1.5, not 4.5, so it takes
        // the roll from 3/4.5 on and not from 3/7.5.
        let twice = [past(JustHaveSex, 24, 1), past(HavingSexTogether, 30, 1)];
        let at = |roll: f64| {
            draw_cycle(&female(), rows, &twice, &both, 2, rolls(0.0, roll)).map(|d| d.kind)
        };
        assert_eq!(at(0.5), Some(AnimalEncounter));
        assert_eq!(at(0.68), Some(PeriodScare));
    }

    /// Spec 2026-11-24-period-cycle-design.md §7: a derived period does what
    /// a `period` row under way does.
    #[test]
    fn a_derived_period_acts_as_a_period_background() {
        // The anchor row under way, and period 1 derived from an anchor that
        // started 28 days ago; the history holds the anchor row as the server
        // reads it.
        let row = anchor(24);
        let derived = on_period();
        for (rows, history) in [(cycle_of(row), [row]), (cycle_of(derived), [derived])] {
            let kind = |k: EventKind| {
                draw_cycle(&female(), rows, &history, &[k], 3, rolls(0.0, 0.0)).map(|d| d.kind)
            };
            for k in [ShortTrip, Sick] {
                assert_eq!(kind(k), None, "{k:?}");
            }
            assert_eq!(kind(PregnancyScare), None);
            assert_eq!(kind(AnimalEncounter), Some(AnimalEncounter));
        }
        // No trait, no cycle.
        let no_trait = PersonaTraits {
            period: false,
            ..female()
        };
        for k in [ShortTrip, Sick, PregnancyScare] {
            assert_eq!(
                draw_cycle(&no_trait, cycle_of(derived), &[], &[k], 3, rolls(0.0, 0.0))
                    .map(|d| d.kind),
                Some(k),
                "{k:?}"
            );
        }
    }

    #[test]
    fn irritable_follows_a_period_for_two_days_after() {
        // Mid, in draw order: reading 2 then irritable 1.
        let kind = |rows: CycleRows, history: &[PastEvent], roll: f64| {
            draw_cycle(
                &female(),
                rows,
                history,
                &[Reading, Irritable],
                3,
                rolls(0.0, roll),
            )
            .map(|d| d.kind)
        };
        // Irritable weighs 3 with a live period: it takes the roll from 2/5.
        let weighs_three = |rows: CycleRows, history: &[PastEvent]| {
            kind(rows, history, 0.39) == Some(Reading)
                && kind(rows, history, 0.41) == Some(Irritable)
        };
        // Irritable weighs 1 otherwise: it takes the roll from 2/3.
        let weighs_one = |rows: CycleRows, history: &[PastEvent]| {
            kind(rows, history, 0.65) == Some(Reading)
                && kind(rows, history, 0.68) == Some(Irritable)
        };
        assert!(weighs_one(CycleRows::default(), &[]));
        // The anchor row under way, a derived period under way, and one
        // that ended 24 hours ago.
        assert!(weighs_three(cycle_of(anchor(24)), &[]));
        assert!(weighs_three(cycle_of(on_period()), &[]));
        assert!(weighs_three(cycle_of(anchor(32 * 24)), &[]));
        // One that ended 72 hours ago.
        assert!(weighs_one(cycle_of(anchor(34 * 24)), &[]));
        // The anchor row, held in the history and in the cycle rows, is one
        // prior and not two.
        assert!(weighs_three(cycle_of(anchor(24)), &[anchor(24)]));
        // The multiplier itself: the latest period is one more prior.
        let latest = cycle::latest_period(&cycle_of(on_period()), Health::Normal, now());
        assert_eq!(
            follow_factor(Irritable, &[], latest.as_ref(), now()),
            FOLLOW_FACTOR * follow_factor(Irritable, &[], None, now())
        );
    }

    #[test]
    fn a_derived_period_does_not_cool_the_body_category() {
        // Period 1 started five days ago; the anchor row is 33 days old.
        let rows = cycle_of(anchor(33 * 24));
        assert_eq!(
            draw_cycle(&female(), rows, &[], &[Sick], 3, rolls(0.0, 0.0)).map(|d| d.kind),
            Some(Sick)
        );
    }

    #[test]
    fn a_scare_under_way_stops_nothing() {
        let s = past(PeriodScare, 24, 96);
        let rows = CycleRows {
            period: Some(anchor(27 * 24 + 12)),
            scare: Some(s),
        };
        for k in [AnimalEncounter, ShortTrip] {
            assert_eq!(
                draw_cycle(&female(), rows, &[s], &[k], 3, rolls(0.0, 0.0)).map(|d| d.kind),
                Some(k),
                "{k:?}"
            );
        }
        // One per cycle.
        assert_eq!(
            draw_cycle(&female(), rows, &[s], &[PeriodScare], 3, rolls(0.0, 0.0)),
            None
        );
    }

    #[test]
    fn a_scare_waits_for_a_foreground_to_end() {
        let rows = cycle_of(due_tomorrow());
        assert_eq!(
            draw_cycle(
                &female(),
                rows,
                &[past(Reading, 1, 3)],
                &[PeriodScare],
                2,
                rolls(0.0, 0.0)
            ),
            None
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
        for k in [GotBumped, Quarrel, ScoldedBySuperior, ScoldedSuperior] {
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
                    romance_compatible: false,
                    cycle: cycle_of(due_tomorrow()),
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
        // Due in 24 hours; a duration of 0.5 ends it two days after that.
        assert_eq!(lasts(PeriodScare, 0.5), Duration::hours(24 + 48));
        assert_eq!(lasts(PregnancyScare, 0.9), Duration::hours(1));
        assert_eq!(lasts(Busy, 0.0), Duration::hours(1));
        assert_eq!(lasts(Busy, 1.0), Duration::hours(3));
        assert_eq!(lasts(InBattle, 0.0), Duration::hours(2));
        assert_eq!(lasts(InBattle, 1.0), Duration::hours(4));
        assert_eq!(lasts(Asleep, 0.0), Duration::hours(1));
        assert_eq!(lasts(Asleep, 1.0), Duration::hours(4));
        for (k, want) in [
            (GotBumped, Duration::minutes(30)),
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
        for k in [GotBumped, Quarrel, ScoldedBySuperior, ScoldedSuperior] {
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
        assert_eq!(at(needs(NeedLevel::High)), Some(Quarrel));
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
        assert_eq!(EventKind::ALL.len(), 70);
    }

    #[test]
    fn special_kinds_parse_but_are_never_drawn() {
        assert_eq!(EventKind::parse("taking_space"), Some(TakingSpace));
        assert_eq!(EventKind::parse("fell_out"), Some(FellOut));
        assert!(!EventKind::ALL.contains(&TakingSpace));
        assert!(!EventKind::ALL.contains(&FellOut));
        assert_eq!(EventKind::ALL.len(), 70);
        assert_eq!(EventKind::SPECIAL.len(), 34);
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
                    romance_compatible: false,
                    cycle: CycleRows::default(),
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
    fn sixteen_kinds_speak_emotionally() {
        let listed = [
            Quarrel,
            ScoldedBySuperior,
            ScoldedSuperior,
            GotHit,
            HitSomeone,
            PoliceEncounter,
            ExWantsBack,
            Irritable,
            WantToQuitJob,
            Unwell,
            Sick,
            Period,
            SuddenDesire,
            Drinking,
            SmokingWeed,
            JustHaveSex,
        ];
        assert_eq!(listed.len(), 16);
        for k in EventKind::ALL.into_iter().chain(EventKind::SPECIAL) {
            assert_eq!(k.speaks_emotionally(), listed.contains(&k), "{k:?}");
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
        ];
        for bg in [Hospitalized, PostSurgery] {
            let h = [past(bg, 24, 72)];
            for c in [0.0, 0.99] {
                for k in [0.0, 0.25, 0.5, 0.75, 0.99] {
                    let got = pick(&h, &allowed, rolls(c, k));
                    assert!(matches!(got, Some(TakingSelfie)), "{bg:?} {c} {k} {got:?}");
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
    fn only_a_selfie_and_a_strip_are_photos() {
        for k in EventKind::ALL {
            assert_eq!(
                k.is_photo(),
                matches!(k, EventKind::TakingSelfie | EventKind::Strip),
                "{k:?}"
            );
        }
    }

    #[test]
    fn a_live_prior_triples_its_follow_on() {
        // HavingSex 1 : Reading 2. With ex_message live, HavingSex weighs 3:
        // 0.5 × 5 = 2.5 lands on HavingSex; without, 0.5 × 3 = 1.5 on Reading.
        // `having_sex` is drawn for a sex addict only; the factor leaves it be.
        let none = addict(Some(true));
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
        let fantasy = PersonaTraits {
            setting: Setting::Fantasy,
            ..Default::default()
        };
        let trip = [past(ShortTrip, 2, 48)];
        assert_eq!(
            pick_with(
                &fantasy,
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
        // `having_sex` is drawn for a sex addict only; the factor leaves it be.
        let none = addict(Some(true));
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
            (BusinessTrip, OnTheWay),
            (BusinessTrip, Busy),
            (BusinessTrip, TempWork),
            (ShortTrip, OnTheRoad),
            (LongTrip, AdmiringScenery),
            (HometownVisit, TakingSelfie),
            (TravelingTogether, OnTheRoad),
            (TravelingTogether, AdmiringScenery),
            (TravelingTogether, TakingSelfie),
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
            (WorkingOut, Showering),
            (WorkingOut, Bathing),
        ] {
            assert!(follows(prior, kind), "{prior:?} -> {kind:?}");
        }
        assert!(!follows(Reading, HavingSex));
        assert!(!follows(HavingSex, HavingSex));
    }

    /// Spec 2026-11-23-twelve-together-actions-and-propose-design.md §6: bed
    /// with the user leads to a scare as her own sex does, and ends her
    /// virginity.
    #[test]
    fn sex_together_counts_as_sex() {
        assert!(follows(HavingSexTogether, PregnancyScare));
        assert!(!follows(HavingSexTogether, HavingSex));
        assert!(!follows(AsleepTogether, PregnancyScare));
        assert_eq!(EventKind::SEX, [HavingSex, JustHaveSex, HavingSexTogether]);
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
                romance_compatible: false,
                cycle: CycleRows::default(),
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
        // `having_sex` is drawn for a sex addict only; the factor leaves it be.
        let virgin = PersonaTraits {
            is_virgin: true,
            ..addict(Some(true))
        };
        let both = [HavingSex, Reading];
        let r = rolls(0.0, 0.8);
        assert_eq!(pick_ever(&virgin, false, &both, r), Some(Reading));
        assert_eq!(pick_ever(&virgin, true, &both, r), Some(HavingSex));
        assert_eq!(
            pick_ever(&addict(Some(true)), false, &both, r),
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

    #[test]
    fn sex_addict_scales_sudden_desire() {
        let both = [SuddenDesire, Showering];
        let with = |sex_addict| PersonaTraits {
            sex_addict,
            ..Default::default()
        };
        let at = |p: &PersonaTraits, k| pick_ever(p, false, &both, rolls(0.0, k));
        // Draw order is `ALL` order: SuddenDesire, Showering.
        // true: 6, 2 (8) — spans [0, .75), [.75, 1].
        let addict = with(Some(true));
        assert_eq!(at(&addict, 0.7), Some(SuddenDesire));
        assert_eq!(at(&addict, 0.8), Some(Showering));
        // false: 2/3, 2 (8/3) — spans [0, .25), [.25, 1].
        let not = with(Some(false));
        assert_eq!(at(&not, 0.2), Some(SuddenDesire));
        assert_eq!(at(&not, 0.3), Some(Showering));
        // absent: 2, 2 (4) — spans [0, .5), [.5, 1].
        let none = with(None);
        assert_eq!(at(&none, 0.45), Some(SuddenDesire));
        assert_eq!(at(&none, 0.55), Some(Showering));
        // The rung gate holds for an addict.
        assert_eq!(
            pick_with(&addict, None, 2, &[], &[SuddenDesire], rolls(0.0, 0.0)),
            None
        );
    }

    fn rated(outing: NeedLevel, indulgence: NeedLevel, emotional: NeedLevel) -> Option<Needs> {
        Some(Needs {
            social: NeedLevel::Mid,
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

    #[test]
    fn traveling_together_is_a_placed_background_she_does_not_tell() {
        let t = TravelingTogether;
        assert!(EventKind::SPECIAL.contains(&t) && !EventKind::ALL.contains(&t));
        assert_eq!(EventKind::parse("traveling_together"), Some(t));
        assert_eq!(t.category(), EventCategory::Trip);
        assert!(!t.may_be_offline());
        for rung in 1..=3 {
            assert!(!is_told(t, rung, &PersonaTraits::default()), "rung {rung}");
        }
    }

    #[test]
    fn bathing_together_is_a_placed_incident_she_does_not_tell() {
        let b = BathingTogether;
        assert!(EventKind::SPECIAL.contains(&b) && !EventKind::ALL.contains(&b));
        assert_eq!(EventKind::parse("bathing_together"), Some(b));
        assert_eq!(b.category(), EventCategory::Short);
        assert!(!b.may_be_offline());
        assert!(b.fits(Setting::Fantasy));
        assert_eq!(lasts(b, 0.5), Duration::minutes(30));
        for rung in 1..=5 {
            assert!(!is_told(b, rung, &PersonaTraits::default()), "rung {rung}");
        }
    }

    /// Spec 2026-11-20-together-actions-state-in-picture-and-genome-routes-
    /// design.md §5.
    #[test]
    fn the_together_kinds_are_placed_incidents_she_does_not_tell() {
        for (k, wire, length) in [
            (
                ShoweringTogether,
                "showering_together",
                Duration::minutes(30),
            ),
            (SaunaTogether, "sauna_together", Duration::hours(1)),
            // Spec 2026-11-23-twelve-together-actions-and-propose-design.md §4.
            (
                UsingToiletTogether,
                "using_toilet_together",
                Duration::minutes(30),
            ),
            (SmokingTogether, "smoking_together", Duration::minutes(30)),
            (HavingSexTogether, "having_sex_together", Duration::hours(1)),
            (WalkingTogether, "walking_together", Duration::hours(1)),
            (
                SmokingWeedTogether,
                "smoking_weed_together",
                Duration::minutes(30),
            ),
            (JoggingTogether, "jogging_together", Duration::hours(1)),
            (YogaTogether, "yoga_together", Duration::hours(1)),
            (
                StargazingTogether,
                "stargazing_together",
                Duration::hours(1),
            ),
            (
                HorseRidingTogether,
                "horse_riding_together",
                Duration::hours(1),
            ),
            (
                StreakingTogether,
                "streaking_together",
                Duration::minutes(30),
            ),
            (DrinkingTogether, "drinking_together", Duration::hours(1)),
        ] {
            assert!(
                EventKind::SPECIAL.contains(&k) && !EventKind::ALL.contains(&k),
                "{k:?}"
            );
            assert_eq!(k.as_str(), wire);
            assert_eq!(EventKind::parse(wire), Some(k));
            assert_eq!(serde_json::to_value(k).unwrap(), wire);
            assert_eq!(k.category(), EventCategory::Short);
            assert_eq!(k.place(), Place::Neutral);
            assert!(k.fits(Setting::Modern) && k.fits(Setting::Fantasy), "{k:?}");
            assert!(!k.may_be_offline(), "{k:?}");
            assert!(!k.is_offline(&female()), "{k:?}");
            let r = rule(k);
            assert_eq!((r.weight, r.cooldown_hours), (0.0, 0), "{k:?}");
            for roll in [0.0, 0.5, 0.99] {
                assert_eq!(lasts(k, roll), length, "{k:?}");
            }
            for rung in 0..=5 {
                assert!(!is_told(k, rung, &PersonaTraits::default()), "rung {rung}");
            }
        }
    }

    /// Spec 2026-11-23-hot-spring-fight-challenge-bet-design.md §5.
    #[test]
    fn the_hot_spring_and_the_fight_are_placed_kinds() {
        let h = HotSpringTogether;
        assert_eq!(h.as_str(), "hot_spring_together");
        assert_eq!(h.category(), EventCategory::Short);
        assert_eq!(lasts(h, 0.0), Duration::hours(1));
        assert_eq!(lasts(h, 0.99), Duration::hours(1));
        assert!(h.fits(Setting::Modern) && h.fits(Setting::Fantasy));
        let f = FightingTogether;
        assert_eq!(f.as_str(), "fighting_together");
        assert_eq!(f.category(), EventCategory::Mid);
        assert_eq!(lasts(f, 0.0), Duration::hours(1));
        assert_eq!(lasts(f, 0.999_999), Duration::hours(2));
        assert!(f.fits(Setting::Fantasy) && !f.fits(Setting::Modern));
        for k in [h, f] {
            assert!(
                EventKind::SPECIAL.contains(&k) && !EventKind::ALL.contains(&k),
                "{k:?}"
            );
            assert_eq!(EventKind::parse(k.as_str()), Some(k));
            let r = rule(k);
            assert_eq!((r.weight, r.cooldown_hours), (0.0, 0), "{k:?}");
            assert_eq!(k.place(), Place::Neutral, "{k:?}");
            assert!(!k.may_be_offline() && !k.is_offline(&female()), "{k:?}");
            for rung in 0..=5 {
                assert!(
                    !is_told(k, rung, &PersonaTraits::default()),
                    "{k:?} rung {rung}"
                );
            }
        }
    }

    /// Spec 2026-11-24-focused-tone-four-together-actions-and-user-gender-
    /// design.md §6.
    #[test]
    fn the_four_new_together_kinds_are_placed_kinds() {
        for (k, wire, category, short, long) in [
            (
                WorkingOutTogether,
                "working_out_together",
                EventCategory::Short,
                Duration::hours(1),
                Duration::hours(1),
            ),
            (
                SwimmingTogether,
                "swimming_together",
                EventCategory::Short,
                Duration::hours(1),
                Duration::hours(1),
            ),
            (
                CyclingTogether,
                "cycling_together",
                EventCategory::Short,
                Duration::hours(1),
                Duration::hours(1),
            ),
            (
                HikingTogether,
                "hiking_together",
                EventCategory::Mid,
                Duration::hours(2),
                Duration::hours(3),
            ),
        ] {
            assert!(
                EventKind::SPECIAL.contains(&k) && !EventKind::ALL.contains(&k),
                "{k:?}"
            );
            assert_eq!(k.as_str(), wire);
            assert_eq!(EventKind::parse(wire), Some(k));
            assert_eq!(serde_json::to_value(k).unwrap(), wire);
            assert_eq!(k.category(), category, "{k:?}");
            assert_eq!(lasts(k, 0.0), short, "{k:?}");
            assert_eq!(lasts(k, 0.999_999), long, "{k:?}");
            let r = rule(k);
            assert_eq!((r.weight, r.cooldown_hours), (0.0, 0), "{k:?}");
            assert_eq!(k.place(), Place::Neutral, "{k:?}");
            assert!(!k.may_be_offline() && !k.is_offline(&female()), "{k:?}");
            assert!(!k.speaks_emotionally(), "{k:?}");
            for rung in 0..=5 {
                assert!(
                    !is_told(k, rung, &PersonaTraits::default()),
                    "{k:?} rung {rung}"
                );
            }
            assert!(k.fits(Setting::Modern), "{k:?}");
            assert_eq!(k.fits(Setting::Fantasy), k != CyclingTogether, "{k:?}");
        }
    }

    /// Spec 2026-11-23-twelve-together-actions-and-propose-design.md §4: the
    /// one together kind that is `mid`, and she still answers during it.
    #[test]
    fn asleep_together_is_a_mid_kind_of_one_to_four_hours() {
        let a = AsleepTogether;
        assert!(EventKind::SPECIAL.contains(&a) && !EventKind::ALL.contains(&a));
        assert_eq!(a.as_str(), "asleep_together");
        assert_eq!(EventKind::parse("asleep_together"), Some(a));
        assert_eq!(a.category(), EventCategory::Mid);
        assert_eq!(a.place(), Place::Neutral);
        assert!(a.fits(Setting::Modern) && a.fits(Setting::Fantasy));
        assert!(!a.may_be_offline());
        assert!(!a.is_offline(&female()));
        let r = rule(a);
        assert_eq!((r.weight, r.cooldown_hours), (0.0, 0));
        assert_eq!(lasts(a, 0.0), Duration::hours(1));
        assert_eq!(lasts(a, 0.999_999), Duration::hours(4));
        assert_eq!(lasts(a, 1.0), Duration::hours(4));
        for rung in 0..=5 {
            assert!(!is_told(a, rung, &PersonaTraits::default()), "rung {rung}");
        }
    }

    #[test]
    fn on_a_date_is_a_placed_incident_she_does_not_tell() {
        let d = OnADate;
        assert!(EventKind::SPECIAL.contains(&d) && !EventKind::ALL.contains(&d));
        assert_eq!(EventKind::parse("on_a_date"), Some(OnADate));
        assert_eq!(d.as_str(), "on_a_date");
        assert_eq!(d.category(), EventCategory::Short);
        assert!(!d.may_be_offline());
        assert!(!d.is_offline(&PersonaTraits::default()));
        assert!(!d.is_offline(&female()));
        assert_eq!(d.place(), Place::Neutral);
        for rung in 1..=5 {
            assert!(!is_told(d, rung, &PersonaTraits::default()), "rung {rung}");
        }
    }

    #[test]
    fn a_trip_together_lasts_two_to_three_days() {
        assert_eq!(lasts(TravelingTogether, 0.0), Duration::hours(48));
        assert_eq!(lasts(TravelingTogether, 1.0), Duration::hours(72));
    }

    /// She is with the user, so nothing may take her out of reach.
    #[test]
    fn no_offline_kind_is_drawn_on_a_trip_together() {
        let persona = PersonaTraits::default();
        let at = |bg| {
            pick_with(
                &persona,
                None,
                3,
                &[past(bg, 1, 48)],
                &[Asleep],
                rolls(0.0, 0.0),
            )
        };
        assert_eq!(at(ShortTrip), Some(Asleep), "alone on a trip she can sleep");
        assert_eq!(at(TravelingTogether), None);
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

    #[test]
    fn setting_parses_two_words() {
        assert_eq!(Setting::parse("modern"), Some(Setting::Modern));
        assert_eq!(Setting::parse("fantasy"), Some(Setting::Fantasy));
        assert_eq!(Setting::parse("space"), None);
    }

    /// Spec 2026-11-26-more-together-actions-bath-outfit-chemistry-drop-and-
    /// custom-together-design.md §4.
    #[test]
    fn the_six_new_together_kinds_are_placed_kinds() {
        for (k, wire, category, short, long) in [
            (
                DancingTogether,
                "dancing_together",
                EventCategory::Short,
                Duration::hours(1),
                Duration::hours(1),
            ),
            (
                AdventuringTogether,
                "adventuring_together",
                EventCategory::Mid,
                Duration::hours(2),
                Duration::hours(3),
            ),
            (
                PlayingWithFireTogether,
                "playing_with_fire_together",
                EventCategory::Short,
                Duration::hours(1),
                Duration::hours(1),
            ),
            (
                SingingTogether,
                "singing_together",
                EventCategory::Short,
                Duration::hours(1),
                Duration::hours(1),
            ),
            (
                SkinnyDippingTogether,
                "skinny_dipping_together",
                EventCategory::Short,
                Duration::hours(1),
                Duration::hours(1),
            ),
            (
                CommittingCrimeTogether,
                "committing_crime_together",
                EventCategory::Short,
                Duration::hours(1),
                Duration::hours(1),
            ),
        ] {
            assert!(
                EventKind::SPECIAL.contains(&k) && !EventKind::ALL.contains(&k),
                "{k:?}"
            );
            assert_eq!(k.as_str(), wire);
            assert_eq!(EventKind::parse(wire), Some(k));
            assert_eq!(serde_json::to_value(k).unwrap(), wire);
            assert_eq!(k.category(), category, "{k:?}");
            assert_eq!(lasts(k, 0.0), short, "{k:?}");
            assert_eq!(lasts(k, 0.999_999), long, "{k:?}");
            let r = rule(k);
            assert_eq!((r.weight, r.cooldown_hours), (0.0, 0), "{k:?}");
            assert_eq!(k.place(), Place::Neutral, "{k:?}");
            assert!(k.is_shared(), "{k:?}");
            assert!(!k.may_be_offline() && !k.is_offline(&female()), "{k:?}");
            assert!(!k.speaks_emotionally(), "{k:?}");
            for rung in 0..=5 {
                assert!(
                    !is_told(k, rung, &PersonaTraits::default()),
                    "{k:?} rung {rung}"
                );
            }
            assert!(k.fits(Setting::Modern) && k.fits(Setting::Fantasy), "{k:?}");
        }
    }

    /// Spec 2026-11-27-swim-outfit-ladder-and-fish-together-design.md §6.
    #[test]
    fn fishing_and_naked_horse_riding_are_placed_kinds() {
        for (k, wire, category, short, long) in [
            (
                FishingTogether,
                "fishing_together",
                EventCategory::Mid,
                Duration::hours(2),
                Duration::hours(3),
            ),
            (
                NakedHorseRidingTogether,
                "naked_horse_riding_together",
                EventCategory::Short,
                Duration::hours(1),
                Duration::hours(1),
            ),
        ] {
            assert!(
                EventKind::SPECIAL.contains(&k) && !EventKind::ALL.contains(&k),
                "{k:?}"
            );
            assert_eq!(k.as_str(), wire);
            assert_eq!(EventKind::parse(wire), Some(k));
            assert_eq!(serde_json::to_value(k).unwrap(), wire);
            assert_eq!(k.category(), category, "{k:?}");
            assert_eq!(lasts(k, 0.0), short, "{k:?}");
            assert_eq!(lasts(k, 0.999_999), long, "{k:?}");
            let r = rule(k);
            assert_eq!((r.weight, r.cooldown_hours), (0.0, 0), "{k:?}");
            assert_eq!(k.place(), Place::Neutral, "{k:?}");
            assert!(k.is_shared(), "{k:?}");
            assert!(!k.may_be_offline() && !k.is_offline(&female()), "{k:?}");
            assert!(!k.speaks_emotionally(), "{k:?}");
            for rung in 0..=5 {
                assert!(
                    !is_told(k, rung, &PersonaTraits::default()),
                    "{k:?} rung {rung}"
                );
            }
            assert!(k.fits(Setting::Modern) && k.fits(Setting::Fantasy), "{k:?}");
        }
    }

    /// Spec 2026-11-26-more-together-actions-bath-outfit-chemistry-drop-and-
    /// custom-together-design.md §8.2.
    #[test]
    fn custom_together_is_a_placed_kind_in_neither_intimate_list() {
        let k = CustomTogether;
        assert!(
            EventKind::SPECIAL.contains(&k) && !EventKind::ALL.contains(&k),
            "{k:?}"
        );
        assert_eq!(k.as_str(), "custom_together");
        assert_eq!(EventKind::parse("custom_together"), Some(k));
        assert_eq!(serde_json::to_value(k).unwrap(), "custom_together");
        assert_eq!(k.category(), EventCategory::Short);
        assert_eq!(lasts(k, 0.0), Duration::hours(1));
        assert_eq!(lasts(k, 0.999_999), Duration::hours(1));
        let r = rule(k);
        assert_eq!((r.weight, r.cooldown_hours), (0.0, 0));
        assert_eq!(k.place(), Place::Neutral);
        assert!(k.is_shared());
        assert!(!k.may_be_offline() && !k.is_offline(&female()));
        assert!(!k.speaks_emotionally());
        for rung in 0..=5 {
            assert!(!is_told(k, rung, &PersonaTraits::default()), "rung {rung}");
        }
        assert!(k.fits(Setting::Modern) && k.fits(Setting::Fantasy));
        assert!(!EventKind::ENDS_ON_CHEMISTRY_DROP.contains(&k));
        assert!(!EventKind::HOLDS_BACK_INTIMATE_ASKS.contains(&k));
        assert!(!EventKind::SEX.contains(&k));
    }

    /// Spec 2026-11-26 §6, §7.
    #[test]
    fn the_two_intimate_lists() {
        assert_eq!(
            EventKind::ENDS_ON_CHEMISTRY_DROP,
            [
                ShoweringTogether,
                BathingTogether,
                HavingSexTogether,
                StreakingTogether,
                SkinnyDippingTogether,
                NakedHorseRidingTogether
            ]
        );
        assert_eq!(
            EventKind::HOLDS_BACK_INTIMATE_ASKS,
            [
                BathingTogether,
                ShoweringTogether,
                HotSpringTogether,
                SaunaTogether,
                HavingSexTogether
            ]
        );
    }

    /// Spec 2026-11-08-strip-and-world-setting-design.md §4; spec
    /// 2026-11-16-prune-life-events-design.md §4.3; spec
    /// 2026-11-24-focused-tone-four-together-actions-and-user-gender-design.md
    /// §4, §6.
    const MODERN_ONLY: [EventKind; 21] = [
        OnACruise,
        OnABus,
        OnATrain,
        OnAPlane,
        Driving,
        PoliceEncounter,
        Showering,
        TakingSelfie,
        Gaming,
        WatchingShow,
        BusinessTrip,
        Hospitalized,
        PostSurgery,
        TempWork,
        ScoldedSuperior,
        SawBag,
        SawClothes,
        OutShopping,
        WantToQuitJob,
        AdmiringArt,
        CyclingTogether,
    ];

    /// Spec 2026-11-19-vehicles-actions-species-design.md §3.
    const FANTASY_ONLY: [EventKind; 2] = [OnTheRoad, FightingTogether];

    #[test]
    fn the_shared_kinds_are_the_date_and_everything_done_together() {
        for k in EventKind::ALL.into_iter().chain(EventKind::SPECIAL) {
            let expected = k == OnADate || k.as_str().ends_with("_together");
            assert_eq!(k.is_shared(), expected, "{k:?}");
        }
        assert!(TravelingTogether.is_shared() && HikingTogether.is_shared());
        assert!(!TakingSelfie.is_shared() && !ShortTrip.is_shared());
    }

    #[test]
    fn modern_only_kinds_never_fit_fantasy() {
        for k in EventKind::ALL.into_iter().chain(EventKind::SPECIAL) {
            assert_eq!(k.fits(Setting::Modern), !FANTASY_ONLY.contains(&k), "{k:?}");
            assert_eq!(k.fits(Setting::Fantasy), !MODERN_ONLY.contains(&k), "{k:?}");
        }
    }

    #[test]
    fn a_fantasy_persona_never_draws_a_modern_only_kind() {
        let fantasy = PersonaTraits {
            setting: Setting::Fantasy,
            ..female()
        };
        assert_eq!(
            pick_with(&fantasy, None, 3, &[], &[TakingSelfie], rolls(0.5, 0.5)),
            None
        );
        assert_eq!(
            pick_with(&female(), None, 3, &[], &[TakingSelfie], rolls(0.5, 0.5)),
            Some(TakingSelfie)
        );
        let fan = PersonaTraits {
            prefer_events: vec![Gaming],
            ..fantasy
        };
        for r in [0.0, 0.5, 0.99] {
            assert_eq!(
                pick_with(&fan, None, 3, &[], &[Gaming, Reading], rolls(r, r)),
                Some(Reading),
                "{r}"
            );
        }
        let worker = PersonaTraits {
            prefer_events: vec![TempWork],
            ..fan
        };
        for r in [0.0, 0.5, 0.99] {
            assert_eq!(
                pick_with(&worker, None, 3, &[], &[TempWork, Reading], rolls(r, r)),
                Some(Reading),
                "{r}"
            );
        }
    }

    /// Spec 2026-11-24-focused-tone-four-together-actions-and-user-gender-design.md §4.
    #[test]
    fn five_modern_life_kinds_never_fit_fantasy() {
        let fantasy = PersonaTraits {
            setting: Setting::Fantasy,
            ..female()
        };
        for k in [SawBag, SawClothes, OutShopping, WantToQuitJob, AdmiringArt] {
            assert!(
                k.fits(Setting::Modern) && !k.fits(Setting::Fantasy),
                "{k:?}"
            );
            let fan = PersonaTraits {
                prefer_events: vec![k],
                ..fantasy.clone()
            };
            for r in [0.0, 0.5, 0.99] {
                assert_eq!(
                    pick_with(&fan, None, 3, &[], &[k, Reading], rolls(r, r)),
                    Some(Reading),
                    "{k:?} {r}"
                );
            }
        }
    }

    #[test]
    fn a_strip_needs_closeness_and_a_compatible_pair() {
        assert_eq!(pick_romantic(3, &[Strip], rolls(0.5, 0.5)), Some(Strip));
        assert_eq!(pick_romantic(2, &[Strip], rolls(0.5, 0.5)), None);
        assert_eq!(
            pick_with(&female(), None, 3, &[], &[Strip], rolls(0.5, 0.5)),
            None
        );
    }

    #[test]
    fn strip_is_a_short_photo_kind() {
        assert_eq!(Strip.category(), EventCategory::Short);
        assert!(Strip.is_photo());
        assert_eq!(EventKind::parse("strip"), Some(Strip));
        assert!(Strip.fits(Setting::Fantasy));
    }

    #[test]
    fn strip_follows_desire_and_leads_to_sex() {
        assert!(follows(SuddenDesire, Strip));
        assert!(follows(Strip, HavingSex));
    }

    #[test]
    fn sex_addict_scales_strip() {
        let with = |sex_addict| PersonaTraits {
            sex_addict,
            ..Default::default()
        };
        assert_eq!(sex_addict_factor(Strip, &with(Some(true))), 3.0);
        assert_eq!(sex_addict_factor(Strip, &with(Some(false))), 1.0 / 3.0);
        assert_eq!(sex_addict_factor(Strip, &with(None)), 1.0);
    }

    #[test]
    fn the_eight_short_kinds() {
        for (k, w, cd, want) in [
            (BirdEncounter, 2.0, 12, Duration::minutes(30)),
            (ExWantsBack, 0.5, 30 * DAY, Duration::hours(1)),
            (WantToQuitJob, 1.0, 7 * DAY, Duration::hours(1)),
            (PonderingLife, 2.0, 48, Duration::hours(1)),
            (Streaking, 0.5, 30 * DAY, Duration::minutes(30)),
            (WantToSeeSea, 2.0, 72, Duration::hours(1)),
            (WantHotSpring, 1.0, 72, Duration::hours(1)),
            (WantSauna, 1.0, 72, Duration::hours(1)),
        ] {
            assert_eq!(k.category(), EventCategory::Short, "{k:?}");
            assert_eq!(rule(k).weight, w, "{k:?}");
            assert_eq!(rule(k).cooldown_hours, cd, "{k:?}");
            assert_eq!(lasts(k, 0.5), want, "{k:?}");
            assert!(!k.may_be_offline() && !k.is_photo(), "{k:?}");
            assert_eq!(k.fits(Setting::Fantasy), k != WantToQuitJob, "{k:?}");
            assert!(is_told(k, 0, &PersonaTraits::default()), "{k:?}");
            assert!(EventKind::ALL.contains(&k), "{k:?}");
            assert_eq!(EventKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(BirdEncounter.as_str(), "bird_encounter");
        assert_eq!(ExWantsBack.as_str(), "ex_wants_back");
        assert_eq!(WantToQuitJob.as_str(), "want_to_quit_job");
        assert_eq!(PonderingLife.as_str(), "pondering_life");
        assert_eq!(Streaking.as_str(), "streaking");
        assert_eq!(WantToSeeSea.as_str(), "want_to_see_sea");
        assert_eq!(WantHotSpring.as_str(), "want_hot_spring");
        assert_eq!(WantSauna.as_str(), "want_sauna");
    }

    #[test]
    fn the_eight_gates() {
        let p = PersonaTraits::default();
        assert!(!passes_gate(ExWantsBack, 1, &p, false));
        assert!(passes_gate(ExWantsBack, 2, &p, false));
        assert!(!passes_gate(Streaking, 3, &p, false));
        assert!(!passes_gate(Streaking, 2, &p, true));
        assert!(passes_gate(Streaking, 3, &p, true));
        assert!(passes_gate(PonderingLife, 0, &p, false));
        // Not compatible at rung 3: never drawn.
        assert_eq!(pick_ever(&p, false, &[Streaking], rolls(0.0, 0.0)), None);
    }

    #[test]
    fn the_former_stamina_and_hunger_kinds_do_not_scale() {
        for level in [NeedLevel::Low, NeedLevel::High] {
            let n = Needs {
                social: level,
                emotional: level,
                outing: level,
                indulgence: level,
            };
            for k in [
                TempWork,
                WorkingOut,
                Unwell,
                Sick,
                Bathing,
                Asleep,
                WantHotSpring,
                WantSauna,
            ] {
                assert_eq!(needs_factor(k, Some(&n)), 1.0, "{k:?} {level:?}");
            }
        }
    }

    #[test]
    fn the_eight_factors_follows_and_rule_outs() {
        let mid = Needs {
            social: NeedLevel::Mid,
            emotional: NeedLevel::Mid,
            outing: NeedLevel::Mid,
            indulgence: NeedLevel::Mid,
        };
        let outing_high = Needs {
            outing: NeedLevel::High,
            ..mid
        };
        assert_eq!(needs_factor(WantToSeeSea, Some(&outing_high)), 2.0);
        assert_eq!(needs_factor(WantToSeeSea, Some(&mid)), 1.0);
        let addict = |sex_addict| PersonaTraits {
            sex_addict,
            ..Default::default()
        };
        assert_eq!(sex_addict_factor(Streaking, &addict(Some(true))), 3.0);
        assert_eq!(
            sex_addict_factor(Streaking, &addict(Some(false))),
            1.0 / 3.0
        );
        for (prior, kind) in [
            (ExMessage, ExWantsBack),
            (Breakup, ExWantsBack),
            (ScoldedBySuperior, WantToQuitJob),
            (TempWork, WantToQuitJob),
            (Breakup, PonderingLife),
            (Quarrel, PonderingLife),
            (WantToQuitJob, PonderingLife),
            (PonderingLife, WantToSeeSea),
        ] {
            assert!(follows(prior, kind), "{prior:?} -> {kind:?}");
        }
        for bg in [Hospitalized, PostSurgery] {
            assert!(rules_out(bg, BirdEncounter) && rules_out(bg, Streaking));
            for k in [
                ExWantsBack,
                WantToQuitJob,
                PonderingLife,
                WantToSeeSea,
                WantHotSpring,
                WantSauna,
            ] {
                assert!(!rules_out(bg, k), "{bg:?} {k:?}");
            }
        }
    }

    fn with_hygienic(hygienic: Option<bool>) -> PersonaTraits {
        PersonaTraits {
            hygienic,
            ..Default::default()
        }
    }

    #[test]
    fn every_kind_has_its_place() {
        let outdoor = [
            PoliceEncounter,
            AnimalEncounter,
            OnTheWay,
            GotBumped,
            AdmiringScenery,
            BirdEncounter,
            WantToSeeSea,
            WantHotSpring,
            GotLost,
            MissedRide,
            OnTheRoad,
            WantToGoOut,
            OnABoat,
            OnACruise,
            OnABus,
            OnATrain,
            OnAPlane,
            Driving,
        ];
        let indoor = [
            Showering,
            Bathing,
            ScoldedBySuperior,
            ScoldedSuperior,
            AdmiringArt,
            WantSauna,
            Reading,
            Gaming,
            WantToGoHome,
            UsingToilet,
        ];
        for k in EventKind::ALL.into_iter().chain(EventKind::SPECIAL) {
            let want = if outdoor.contains(&k) {
                Place::Outdoor
            } else if indoor.contains(&k) {
                Place::Indoor
            } else {
                Place::Neutral
            };
            assert_eq!(k.place(), want, "{k:?}");
        }
    }

    #[test]
    fn offline_kinds_are_neutral() {
        for k in EventKind::ALL.into_iter().filter(|k| k.may_be_offline()) {
            assert_eq!(k.place(), Place::Neutral, "{k:?}");
        }
    }

    #[test]
    fn the_outdoorsy_factor() {
        let with = |outdoorsy| PersonaTraits {
            outdoorsy,
            ..Default::default()
        };
        let (out, neutral, ind) = (
            with(Outdoorsy::Outdoor),
            with(Outdoorsy::Neutral),
            with(Outdoorsy::Indoor),
        );
        assert_eq!(outdoorsy_factor(AnimalEncounter, &out), 3.0);
        assert_eq!(outdoorsy_factor(Reading, &out), 1.0 / 3.0);
        assert_eq!(outdoorsy_factor(Quarrel, &out), 1.0);
        assert_eq!(outdoorsy_factor(AnimalEncounter, &ind), 1.0 / 3.0);
        assert_eq!(outdoorsy_factor(Reading, &ind), 3.0);
        assert_eq!(outdoorsy_factor(Quarrel, &ind), 1.0);
        for k in [AnimalEncounter, Reading, Quarrel] {
            assert_eq!(outdoorsy_factor(k, &neutral), 1.0, "{k:?}");
        }
        assert_eq!(Outdoorsy::default(), Outdoorsy::Neutral);
        assert_eq!(Outdoorsy::parse("outdoor"), Some(Outdoorsy::Outdoor));
        assert_eq!(Outdoorsy::parse("neutral"), Some(Outdoorsy::Neutral));
        assert_eq!(Outdoorsy::parse("indoor"), Some(Outdoorsy::Indoor));
        assert_eq!(Outdoorsy::parse("yes"), None);
    }

    /// An outdoor persona draws an outdoor kind she would not otherwise: the
    /// factor reaches the draw.
    #[test]
    fn an_outdoor_persona_leans_outdoors() {
        let only = [Reading, WantToGoOut];
        let roll = rolls(0.0, 0.45);
        let at = |p: &PersonaTraits| pick_with(p, None, 1, &[], &only, roll);
        assert_eq!(at(&PersonaTraits::default()), Some(Reading));
        let out = PersonaTraits {
            outdoorsy: Outdoorsy::Outdoor,
            ..Default::default()
        };
        assert_eq!(at(&out), Some(WantToGoOut));
    }

    fn with_employed(employed: Employment) -> PersonaTraits {
        PersonaTraits {
            employed,
            ..Default::default()
        }
    }

    #[test]
    fn hygienic_scales_the_bathing_kinds() {
        for k in [Showering, Bathing, WantHotSpring, WantSauna] {
            assert_eq!(hygienic_factor(k, &with_hygienic(Some(true))), 3.0);
            assert_eq!(hygienic_factor(k, &with_hygienic(Some(false))), 1.0 / 3.0);
            assert_eq!(hygienic_factor(k, &with_hygienic(None)), 1.0);
        }
        assert_eq!(
            hygienic_factor(PonderingLife, &with_hygienic(Some(true))),
            1.0
        );
        // Draw order is `ALL` order: Showering, PonderingLife, both 2.
        let both = [Showering, PonderingLife];
        let at = |p: &PersonaTraits, k| pick_ever(p, false, &both, rolls(0.0, k));
        // true: 6, 2 — Showering below .75.
        assert_eq!(at(&with_hygienic(Some(true)), 0.7), Some(Showering));
        assert_eq!(at(&with_hygienic(Some(true)), 0.8), Some(PonderingLife));
        // false: 2/3, 2 — Showering below .25.
        assert_eq!(at(&with_hygienic(Some(false)), 0.2), Some(Showering));
        assert_eq!(at(&with_hygienic(Some(false)), 0.3), Some(PonderingLife));
        // unset: 2, 2 — Showering below .5.
        assert_eq!(at(&with_hygienic(None), 0.45), Some(Showering));
        assert_eq!(at(&with_hygienic(None), 0.55), Some(PonderingLife));
    }

    #[test]
    fn employed_scales_the_work_kinds() {
        for k in [TempWork, ScoldedBySuperior, ScoldedSuperior, BusinessTrip] {
            assert_eq!(
                employed_factor(k, &with_employed(Employment::Free)),
                1.0 / 3.0
            );
            assert_eq!(
                employed_factor(k, &with_employed(Employment::PartTime)),
                1.0
            );
            assert_eq!(
                employed_factor(k, &with_employed(Employment::FullTime)),
                3.0
            );
        }
        assert_eq!(
            employed_factor(Reading, &with_employed(Employment::Free)),
            1.0
        );
        // Draw order is `ALL` order: Quarrel, ScoldedBySuperior; both weigh 1.
        let both = [Quarrel, ScoldedBySuperior];
        let at = |p: &PersonaTraits, k| pick_ever(p, false, &both, rolls(0.0, k));
        // full_time: 1, 3 — Quarrel below .25.
        let full = with_employed(Employment::FullTime);
        assert_eq!(at(&full, 0.2), Some(Quarrel));
        assert_eq!(at(&full, 0.3), Some(ScoldedBySuperior));
        // free: 1, 1/3 — Quarrel below .75.
        let free = with_employed(Employment::Free);
        assert_eq!(at(&free, 0.7), Some(Quarrel));
        assert_eq!(at(&free, 0.8), Some(ScoldedBySuperior));
    }

    #[test]
    fn a_free_persona_never_wants_to_quit() {
        let only = [WantToQuitJob];
        let at = |e| pick_ever(&with_employed(e), false, &only, rolls(0.0, 0.0));
        assert_eq!(at(Employment::Free), None);
        assert_eq!(at(Employment::PartTime), Some(WantToQuitJob));
        assert_eq!(at(Employment::FullTime), Some(WantToQuitJob));
    }

    #[test]
    fn employment_parses_its_three_values() {
        assert_eq!(Employment::parse("free"), Some(Employment::Free));
        assert_eq!(Employment::parse("part_time"), Some(Employment::PartTime));
        assert_eq!(Employment::parse("full_time"), Some(Employment::FullTime));
        assert_eq!(Employment::parse("retired"), None);
        assert_eq!(Employment::default(), Employment::PartTime);
    }

    fn addict(sex_addict: Option<bool>) -> PersonaTraits {
        PersonaTraits {
            sex_addict,
            ..Default::default()
        }
    }

    #[test]
    fn the_stock_lost_habit_and_sex_kinds() {
        for (k, wire, w, cd, want) in [
            (GotLost, "got_lost", 3.0, 48, Duration::hours(1)),
            (Drinking, "drinking", 2.0, 12, Duration::minutes(30)),
            (Smoking, "smoking", 2.0, 12, Duration::minutes(30)),
            (SmokingWeed, "smoking_weed", 2.0, 12, Duration::minutes(30)),
            (JustHaveSex, "just_have_sex", 1.0, 48, Duration::minutes(30)),
            (Masturbating, "masturbating", 1.0, 24, Duration::minutes(30)),
            (MissedRide, "missed_ride", 3.0, 48, Duration::hours(1)),
        ] {
            assert_eq!(k.as_str(), wire);
            assert_eq!(EventKind::parse(wire), Some(k));
            assert!(EventKind::ALL.contains(&k), "{k:?}");
            assert_eq!(k.category(), EventCategory::Short, "{k:?}");
            assert_eq!(rule(k).weight, w, "{k:?}");
            assert_eq!(rule(k).cooldown_hours, cd, "{k:?}");
            assert_eq!(lasts(k, 0.5), want, "{k:?}");
            assert!(!k.may_be_offline() && !k.is_photo(), "{k:?}");
            assert!(is_told(k, 3, &PersonaTraits::default()), "{k:?}");
            assert!(k.fits(Setting::Fantasy), "{k:?}");
        }
    }

    #[test]
    fn the_habit_kinds_need_their_habit() {
        let with = |habits| PersonaTraits {
            habits,
            ..Default::default()
        };
        let on = Some(true);
        for (k, habits) in [
            (
                Drinking,
                Habits {
                    alcoholic: on,
                    ..Default::default()
                },
            ),
            (
                Smoking,
                Habits {
                    smoking: on,
                    ..Default::default()
                },
            ),
            (
                SmokingWeed,
                Habits {
                    drug: on,
                    ..Default::default()
                },
            ),
        ] {
            assert!(passes_gate(k, 0, &with(habits), false), "{k:?}");
            assert!(!passes_gate(k, 5, &PersonaTraits::default(), true), "{k:?}");
            let off = Habits {
                alcoholic: Some(false),
                smoking: Some(false),
                drug: Some(false),
                ..Default::default()
            };
            assert!(!passes_gate(k, 5, &with(off), true), "{k:?}");
        }
        let high = Needs {
            social: NeedLevel::Mid,
            emotional: NeedLevel::Mid,
            outing: NeedLevel::Mid,
            indulgence: NeedLevel::High,
        };
        for k in [Drinking, Smoking, SmokingWeed] {
            assert_eq!(indulgence_factor(k, Some(&high)), 3.0, "{k:?}");
        }
    }

    #[test]
    fn having_sex_and_masturbating_only_for_a_sex_addict() {
        for p in [addict(None), addict(Some(false))] {
            assert!(!passes_gate(HavingSex, 5, &p, true));
            assert!(!passes_gate(Masturbating, 5, &p, true));
            assert_eq!(pick_ever(&p, true, &[HavingSex], rolls(0.0, 0.0)), None);
        }
        let yes = addict(Some(true));
        assert!(passes_gate(HavingSex, 0, &yes, false));
        assert!(passes_gate(Masturbating, 3, &yes, false));
        assert!(!passes_gate(Masturbating, 2, &yes, false));
    }

    #[test]
    fn just_have_sex_is_told_from_rung_3() {
        let p = PersonaTraits::default();
        assert!(!passes_gate(JustHaveSex, 2, &p, false));
        assert!(passes_gate(JustHaveSex, 3, &p, false));
        assert_eq!(sex_addict_factor(JustHaveSex, &addict(Some(true))), 3.0);
        assert_eq!(
            sex_addict_factor(JustHaveSex, &addict(Some(false))),
            1.0 / 3.0
        );
        let virgin = PersonaTraits {
            is_virgin: true,
            ..Default::default()
        };
        assert_eq!(prior_factor(JustHaveSex, &virgin, false), UNLIKELY_FACTOR);
        assert_eq!(prior_factor(JustHaveSex, &virgin, true), 1.0);
        for prior in [ExMessage, SuddenDesire, Strip] {
            assert!(follows(prior, JustHaveSex), "{prior:?}");
        }
        assert!(follows(JustHaveSex, PregnancyScare));
        assert!(follows(SuddenDesire, Masturbating));
    }

    #[test]
    fn the_rule_outs_of_the_new_kinds() {
        for bg in [Hospitalized, PostSurgery] {
            for k in [GotLost, Drinking, Smoking, SmokingWeed, JustHaveSex] {
                assert!(rules_out(bg, k), "{bg:?} {k:?}");
            }
            assert!(!rules_out(bg, Masturbating), "{bg:?}");
        }
        assert!(rules_out(TravelingTogether, JustHaveSex));
        assert!(!rules_out(TravelingTogether, GotLost));
    }

    /// Spec 2026-11-16-prune-life-events-design.md §4.1.
    const DELETED: [&str; 7] = [
        "grocery_shopping",
        "in_class",
        "watching_stocks",
        "trading",
        "bumped_someone",
        "weather_change",
        "watching_match",
    ];

    #[test]
    fn the_deleted_kinds_are_gone() {
        for name in DELETED {
            assert_eq!(EventKind::parse(name), None, "{name}");
        }
        assert_eq!(EventKind::ALL.len(), 70);
    }

    /// Spec 2026-11-16-prune-life-events-design.md §4.2.
    #[test]
    fn the_commute_kinds_come_up_only_on_a_trip() {
        let modern = PersonaTraits::default();
        let fantasy = PersonaTraits {
            setting: Setting::Fantasy,
            ..Default::default()
        };
        for (k, p) in [(OnTheWay, &modern), (OnTheRoad, &fantasy)] {
            let at = |history: &[PastEvent]| pick_with(p, None, 1, history, &[k], rolls(0.0, 0.0));
            assert_eq!(at(&[]), None, "{k:?} no background");
            for bg in [
                BusinessTrip,
                ShortTrip,
                LongTrip,
                HometownVisit,
                TravelingTogether,
            ] {
                assert_eq!(at(&[past(bg, 1, 48)]), Some(k), "{k:?} {bg:?}");
            }
            assert_eq!(at(&[past(Sick, 1, 48)]), None, "{k:?} not a trip");
            // Ended an hour ago: inside the follow window, still not drawn.
            assert_eq!(
                at(&[past(BusinessTrip, 25, 24)]),
                None,
                "{k:?} the trip is over"
            );
        }
    }

    #[test]
    fn she_gets_lost_only_on_a_trip() {
        let only = [GotLost];
        let p = PersonaTraits::default();
        let at = |history: &[PastEvent]| pick_with(&p, None, 1, history, &only, rolls(0.0, 0.0));
        assert_eq!(at(&[]), None, "no background");
        for bg in [ShortTrip, LongTrip, HometownVisit, TravelingTogether] {
            assert_eq!(at(&[past(bg, 1, 48)]), Some(GotLost), "{bg:?}");
        }
        for bg in [BusinessTrip, Sick] {
            assert_eq!(at(&[past(bg, 1, 48)]), None, "{bg:?}");
        }
        assert_eq!(at(&[past(ShortTrip, 72, 48)]), None, "the trip is over");
    }

    #[test]
    fn she_misses_a_ride_only_on_a_trip() {
        let only = [MissedRide];
        let p = PersonaTraits::default();
        let at = |history: &[PastEvent]| pick_with(&p, None, 1, history, &only, rolls(0.0, 0.0));
        assert_eq!(at(&[]), None, "no background");
        for bg in [
            BusinessTrip,
            ShortTrip,
            LongTrip,
            HometownVisit,
            TravelingTogether,
        ] {
            assert_eq!(at(&[past(bg, 1, 48)]), Some(MissedRide), "{bg:?}");
        }
        assert_eq!(at(&[past(Sick, 1, 48)]), None, "not a trip");
        assert_eq!(at(&[past(BusinessTrip, 72, 48)]), None, "the trip is over");
    }

    /// Spec 2026-11-19-vehicles-actions-species-design.md §2.
    const VEHICLES: [(EventKind, &str, i64, i64); 6] = [
        (OnABoat, "on_a_boat", 1, 2),
        (OnACruise, "on_a_cruise", 2, 4),
        (OnABus, "on_a_bus", 2, 4),
        (OnATrain, "on_a_train", 2, 4),
        (OnAPlane, "on_a_plane", 2, 4),
        (Driving, "driving", 1, 3),
    ];

    #[test]
    fn six_vehicle_kinds_parse_and_rule() {
        for (k, s, lo, hi) in VEHICLES {
            assert_eq!(k.as_str(), s);
            assert_eq!(EventKind::parse(s), Some(k));
            assert!(EventKind::ALL.contains(&k), "{k:?}");
            assert_eq!(k.category(), EventCategory::Mid, "{k:?}");
            assert_eq!(k.place(), Place::Outdoor, "{k:?}");
            let r = rule(k);
            assert!((r.weight - 1.0).abs() < E, "{k:?}");
            assert_eq!(r.cooldown_hours, 48, "{k:?}");
            assert_eq!(lasts(k, 0.0), Duration::hours(lo), "{k:?}");
            assert_eq!(lasts(k, 0.999), Duration::hours(hi), "{k:?}");
            assert!(
                !k.may_be_offline() && !k.is_photo() && !k.speaks_emotionally(),
                "{k:?}"
            );
            assert!(is_told(k, 1, &PersonaTraits::default()), "{k:?}");
            assert!(k.fits(Setting::Modern), "{k:?}");
            assert_eq!(k.fits(Setting::Fantasy), k == OnABoat, "{k:?}");
        }
    }

    #[test]
    fn the_vehicles_come_up_only_on_a_trip() {
        let p = PersonaTraits::default();
        for (k, ..) in VEHICLES {
            let at = |h: &[PastEvent]| pick_with(&p, None, 1, h, &[k], rolls(0.0, 0.0));
            assert_eq!(at(&[]), None, "{k:?} no background");
            for bg in [ShortTrip, LongTrip, HometownVisit, TravelingTogether] {
                assert_eq!(at(&[past(bg, 1, 48)]), Some(k), "{k:?} {bg:?}");
            }
            assert_eq!(
                at(&[past(BusinessTrip, 1, 48)]),
                (k != OnACruise).then_some(k),
                "{k:?}"
            );
            assert_eq!(at(&[past(Sick, 1, 48)]), None, "{k:?} not a trip");
            assert_eq!(
                at(&[past(ShortTrip, 72, 48)]),
                None,
                "{k:?} the trip is over"
            );
        }
        let fantasy = PersonaTraits {
            setting: Setting::Fantasy,
            ..Default::default()
        };
        for (k, ..) in VEHICLES {
            let got = pick_with(
                &fantasy,
                None,
                1,
                &[past(ShortTrip, 1, 48)],
                &[k],
                rolls(0.0, 0.0),
            );
            assert_eq!(got, (k == OnABoat).then_some(k), "{k:?}");
        }
    }

    /// Spec 2026-11-19-vehicles-actions-species-design.md §3.
    #[test]
    fn on_the_road_is_drawn_only_in_a_fantasy_world() {
        let trip = [past(ShortTrip, 1, 48)];
        let modern = PersonaTraits {
            prefer_events: vec![OnTheRoad],
            ..Default::default()
        };
        let fantasy = PersonaTraits {
            setting: Setting::Fantasy,
            ..Default::default()
        };
        assert_eq!(
            pick_with(&modern, None, 1, &trip, &[OnTheRoad], rolls(0.0, 0.0)),
            None
        );
        assert_eq!(
            pick_with(&fantasy, None, 1, &trip, &[OnTheRoad], rolls(0.0, 0.0)),
            Some(OnTheRoad)
        );
        assert_eq!(EventKind::parse("on_the_road"), Some(OnTheRoad));
        assert!(!follows(BusinessTrip, OnTheRoad));
        assert!(follows(ShortTrip, OnTheRoad));
    }

    /// Spec 2026-11-19-vehicles-actions-species-design.md §5.
    #[test]
    fn a_machine_never_draws_the_period_kinds() {
        let machine = PersonaTraits {
            machine: true,
            period: false,
            ..female()
        };
        // Even an anchor inside the scare window gives a machine nothing.
        let cycle = cycle_of(due_tomorrow());
        for k in [Period, PeriodScare, PregnancyScare] {
            let cycle = if k == Period {
                CycleRows::default()
            } else {
                cycle
            };
            let kind = |p: &PersonaTraits| {
                draw_cycle(p, cycle, &[], &[k], 3, rolls(0.0, 0.0)).map(|d| d.kind)
            };
            assert_eq!(kind(&machine), None, "{k:?}");
            assert_eq!(kind(&female()), Some(k), "{k:?}");
        }
        for k in [Sick, SawBag] {
            assert_eq!(
                pick_with(&machine, None, 3, &[], &[k], rolls(0.0, 0.0)),
                Some(k),
                "{k:?}"
            );
        }
    }

    /// Spec 2026-11-20-together-actions-state-in-picture-and-genome-routes-
    /// design.md §4.
    #[test]
    fn using_toilet_is_a_drawn_half_hour_indoor_short_kind() {
        assert_eq!(UsingToilet.as_str(), "using_toilet");
        assert_eq!(EventKind::parse("using_toilet"), Some(UsingToilet));
        assert_eq!(serde_json::to_value(UsingToilet).unwrap(), "using_toilet");
        assert!(EventKind::ALL.contains(&UsingToilet));
        assert!(!EventKind::SPECIAL.contains(&UsingToilet));
        assert_eq!(UsingToilet.category(), EventCategory::Short);
        assert_eq!(UsingToilet.place(), Place::Indoor);
        let r = rule(UsingToilet);
        assert!((r.weight - 1.0).abs() < E);
        assert_eq!(r.cooldown_hours, 12);
        assert_eq!(lasts(UsingToilet, 0.0), Duration::minutes(30));
        assert_eq!(lasts(UsingToilet, 0.99), Duration::minutes(30));
    }

    #[test]
    fn using_toilet_fits_both_worlds_and_is_told_not_offline() {
        assert!(UsingToilet.fits(Setting::Modern));
        assert!(UsingToilet.fits(Setting::Fantasy));
        let none = PersonaTraits::default();
        assert!(!UsingToilet.is_offline(&none));
        assert!(!UsingToilet.may_be_offline());
        assert!(!UsingToilet.is_photo());
        assert!(!UsingToilet.speaks_emotionally());
        assert!(!UsingToilet.is_false_alarm());
        assert!(!UsingToilet.is_background());
        assert!(!offline_kinds(&none).contains(&UsingToilet));
        for rung in 0..=3 {
            assert!(is_told(UsingToilet, rung, &none), "rung {rung}");
            assert!(is_told(UsingToilet, rung, &female()), "rung {rung}");
        }
        for persona in [PersonaTraits::default(), female()] {
            for rung in 0..=3 {
                assert_eq!(
                    pick_with(&persona, None, rung, &[], &[UsingToilet], rolls(0.0, 0.0)),
                    Some(UsingToilet),
                    "rung {rung}"
                );
            }
        }
        let fantasy = PersonaTraits {
            setting: Setting::Fantasy,
            ..Default::default()
        };
        assert_eq!(
            pick_with(&fantasy, None, 0, &[], &[UsingToilet], rolls(0.0, 0.0)),
            Some(UsingToilet)
        );
    }

    #[test]
    fn using_toilet_has_no_follow_on() {
        for k in EventKind::ALL.into_iter().chain(EventKind::SPECIAL) {
            assert!(!follows(k, UsingToilet), "{k:?}");
            assert!(!follows(UsingToilet, k), "{k:?}");
        }
    }

    /// Spec 2026-11-20-together-actions-state-in-picture-and-genome-routes-
    /// design.md §4.
    #[test]
    fn a_machine_never_draws_using_toilet() {
        for (species, traits) in [
            (
                "machine",
                PersonaTraits {
                    machine: true,
                    ..Default::default()
                },
            ),
            (
                "female machine",
                PersonaTraits {
                    machine: true,
                    ..female()
                },
            ),
        ] {
            assert_eq!(
                pick_with(&traits, None, 3, &[], &[UsingToilet], rolls(0.0, 0.0)),
                None,
                "{species}"
            );
        }
        for traits in [PersonaTraits::default(), female()] {
            assert_eq!(
                pick_with(&traits, None, 3, &[], &[UsingToilet], rolls(0.0, 0.0)),
                Some(UsingToilet)
            );
        }
        // A machine still draws any other short kind.
        let machine = PersonaTraits {
            machine: true,
            ..Default::default()
        };
        assert_eq!(
            pick_with(&machine, None, 3, &[], &[Bathing], rolls(0.0, 0.0)),
            Some(Bathing)
        );
    }

    #[test]
    fn the_period_trait_gates_period_and_its_scare() {
        let no_period = PersonaTraits {
            period: false,
            ..female()
        };
        // The trait alone decides, not gender: a persona the card gives a
        // period draws it.
        let card_period = PersonaTraits {
            period: true,
            ..Default::default()
        };
        for k in [Period, PeriodScare] {
            // The scare also needs an anchor whose due moment is near.
            let cycle = if k == Period {
                CycleRows::default()
            } else {
                cycle_of(due_tomorrow())
            };
            let kind = |p: &PersonaTraits| {
                draw_cycle(p, cycle, &[], &[k], 3, rolls(0.0, 0.0)).map(|d| d.kind)
            };
            assert_eq!(kind(&no_period), None, "{k:?}");
            assert_eq!(kind(&card_period), Some(k), "{k:?}");
        }
        // `pregnancy_scare` keeps reading gender.
        assert_eq!(
            pick_with(
                &card_period,
                None,
                3,
                &[],
                &[PregnancyScare],
                rolls(0.0, 0.0)
            ),
            None
        );
        assert_eq!(
            pick_with(&no_period, None, 3, &[], &[PregnancyScare], rolls(0.0, 0.0)),
            Some(PregnancyScare)
        );
    }

    #[test]
    fn cycle_health_is_the_health_of_a_persona_with_a_period() {
        assert_eq!(PersonaTraits::default().cycle_health(), None);
        for health in [Health::Weak, Health::Normal, Health::Strong] {
            let traits = PersonaTraits {
                health,
                period: true,
                ..Default::default()
            };
            assert_eq!(traits.cycle_health(), Some(health));
            let none = PersonaTraits {
                health,
                ..Default::default()
            };
            assert_eq!(none.cycle_health(), None);
        }
    }

    #[test]
    fn slots() {
        for k in [Period, PeriodScare] {
            assert_eq!(k.slot(), Slot::Cycle, "{k:?}");
        }
        for k in [Sick, LongTrip, TravelingTogether] {
            assert_eq!(k.slot(), Slot::Background, "{k:?}");
        }
        for k in [PregnancyScare, Asleep, OnADate] {
            assert_eq!(k.slot(), Slot::Foreground, "{k:?}");
        }
        // The three slots partition every kind, `ALL` and `SPECIAL` alike.
        let mut all: Vec<EventKind> = EventKind::ALL
            .into_iter()
            .chain(EventKind::SPECIAL)
            .collect();
        let mut parted: Vec<EventKind> = [Slot::Foreground, Slot::Background, Slot::Cycle]
            .into_iter()
            .flat_map(kinds_in)
            .collect();
        all.sort_by_key(|k| k.as_str());
        parted.sort_by_key(|k| k.as_str());
        assert_eq!(all, parted);
        // A slot other than the cycle still follows the category.
        for k in kinds_in(Slot::Background) {
            assert!(k.is_background(), "{k:?}");
        }
        assert!(Period.is_background());
    }
}
