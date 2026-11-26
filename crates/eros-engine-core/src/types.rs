// SPDX-License-Identifier: AGPL-3.0-only
//! Public types for the companion engine.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::affinity::{Affinity, AffinityDeltas, AffinityLine};
use crate::events::{EventKind, TogetherGroup};
use crate::persona::CompanionPersona;
use crate::relationship::StatusAsk;
use crate::scope::{AffinityScope, MemoryScope};

/// A caller-supplied system-prompt fragment. The engine treats `text` as
/// opaque — it is inserted verbatim under the `[additional_guidance]` section of
/// the persona system prompt. `tag` is for logging/observability only and
/// is constrained to `[a-z0-9_]{1,32}` by the HTTP layer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptTrait {
    pub tag: String,
    pub text: String,
}

/// Caller-supplied OpenRouter passthrough for per-request audit /
/// analytics. Engine never inspects these fields; they ride straight to
/// `openrouter.ai/api/v1/chat/completions` as wire-level `user`,
/// `session_id`, `metadata`. The HTTP layer applies size/shape caps;
/// content remains opaque to the engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct LlmAudit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Events that drive the engine pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)] // UserMessage is intentionally large; boxing would add indirection
pub enum Event {
    UserMessage {
        content: String,
        message_id: Uuid,
        /// Optional caller-supplied prompt traits. Empty for clients that
        /// don't send the field — preserves the legacy system-prompt output
        /// byte-for-byte.
        #[serde(default)]
        prompt_traits: Vec<PromptTrait>,
        /// Optional caller-supplied OpenRouter audit passthrough.
        /// `None` for clients that don't send the field. Engine carries it
        /// opaquely from request → handler → ChatRequest; content is never
        /// inspected.
        #[serde(default)]
        audit: Option<LlmAudit>,
        /// Optional caller-supplied tier (downstream-defined, e.g. "free" /
        /// "gold"). `None` for clients that don't send it. Routed opaquely to
        /// `model_config.resolve` to pick the per-tier model + allow_traits.
        #[serde(default)]
        tier: Option<String>,
        /// Optional caller-supplied memory injection scope (#40). Defaults to
        /// `neutral_and_relationship` when absent.
        #[serde(default)]
        memory_scope: MemoryScope,
        /// Optional caller-supplied affinity-axis injection scope (#40).
        /// Defaults to `bond` when absent.
        #[serde(default)]
        affinity_scope: AffinityScope,
        /// The message this turn quotes, resolved from the caller's
        /// `reply_to_message_id`. `None` on ordinary turns and when the id did
        /// not resolve. Never affects which history rows are injected — a
        /// quote points at one line, it does not rewind the conversation.
        #[serde(default)]
        quote: Option<QuotedMessage>,
        /// What the user did this turn besides talking. `None` on ordinary
        /// turns.
        #[serde(default)]
        action: Option<UserAction>,
        /// The outfit the caller named for this turn (spec
        /// 2026-10-12-persona-items-and-gift-design.md §6). `None` ⇒ the judge
        /// picks.
        #[serde(default)]
        outfit: Option<String>,
    },
    ProactiveTrigger,
    AppOpen,
}

/// A message the caller quoted via `reply_to_message_id`, resolved to its text.
/// Rendered as the prompt's `[quote]` block so the model knows which line the
/// user is pointing at — the history window is untouched either way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotedMessage {
    pub message_id: Uuid,
    /// `chat_messages.role` of the quoted row — decides whether the prompt
    /// attributes the line to the user or to the persona.
    pub role: String,
    pub content: String,
    /// When the quoted line was sent. Renders as a relative age ("3 天前") so
    /// the model can tell a callback to an old message from a same-breath one.
    pub sent_at: DateTime<Utc>,
    /// The user line the quoted message answered, when the quote should carry
    /// the exchange that produced it — a quoted picture on an image-edit full
    /// turn arrives with the message that asked for it. Rendered as a `用户：`
    /// line above the quoted one; `None` leaves the block a single line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
}

/// What a user's turn does to the persona besides talking (spec
/// 2026-10-05-chat-user-actions-design.md). Stored verbatim as the user row's
/// `metadata.action`, so the serde shape is a persisted contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserAction {
    Give {
        item: GivenItem,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        quantity: u8,
    },
    Kiss,
    Hug,
    Touch,
    Lick,
    FrenchKiss,
    HugFromBehind,
    Tease,
    Pat,
    Wink,
    Blink,
    /// The user asks her to dance with them.
    AskForDance,
    /// The user asks her to dance for them; accepted, the turn shows her
    /// picture (spec 2026-11-19-vehicles-actions-species-design.md §4).
    AskDancing,
    /// The user takes her picture; accepted, the turn shows it (spec
    /// 2026-11-08-five-actions-two-items-gluttony-card-1-1-design.md §2.3).
    Photo,
    /// The three social actions: the response says whether she falls for it
    /// (spec 2026-11-08 §2.2).
    Bluff,
    Threaten,
    Trick,
    /// The user undresses her; only a romance-compatible pair can have it
    /// accepted (spec 2026-11-08-strip-and-world-setting-design.md §2).
    Strip,
    /// The user asks to shower, take a bath or take a sauna with her (spec
    /// 2026-11-20-together-actions-state-in-picture-and-genome-routes-
    /// design.md §5). Accepted, she is in the event `together_kind` names,
    /// and the turn shows her picture when pictures are available. A shower
    /// and a bath are nude actions (spec 2026-11-27-together-action-groups-
    /// design.md §5.1); the judge decides them for any pair.
    ShowerTogether,
    BathTogether,
    SaunaTogether,
    /// Twelve more things the user asks to do with her (spec
    /// 2026-11-23-twelve-together-actions-and-propose-design.md §4): each
    /// behaves as `sauna_together` does, placing the event `together_kind`
    /// names. `fall_asleep_together` is the literal sleep; `sex_together` is
    /// her `sleep_together` ask the other way round (§6). The three habit
    /// actions count as a give of one (§5).
    ToiletTogether,
    SmokeTogether,
    FallAsleepTogether,
    SexTogether,
    WalkTogether,
    SmokeWeedTogether,
    JogTogether,
    YogaTogether,
    StargazeTogether,
    RideHorseTogether,
    StreakTogether,
    DrinkTogether,
    /// The user asks her to marry them (spec 2026-11-23 §7): the judge
    /// decides, a refusal drops nothing, and no event is placed. Put only
    /// from `dating`, `fwb` or `situationship`; accepted, the pair is
    /// `engaged` (spec 2026-11-23-affinity-50-design.md §8). On a BYOC
    /// instance it goes to the judge from any status and moves nothing
    /// (§6.5).
    Propose,
    /// The user breaks up with her (spec 2026-11-23-affinity-50-design.md
    /// §8): put only from a breakable status. Accepted, the pair is `ex`;
    /// refused, a cold war starts. A refusal drops nothing, and no event is
    /// placed.
    BreakUp,
    /// The user asks her into a hot spring with them; a together action, as
    /// `sauna_together` (spec 2026-11-23-hot-spring-fight-challenge-bet-
    /// design.md §4).
    HotSpringTogether,
    /// The user asks her to fight beside them; a together action that only a
    /// `fantasy` persona can have (spec 2026-11-23 §4).
    FightTogether,
    /// The user asks her to work out, swim, cycle or hike with them (spec
    /// 2026-11-24-focused-tone-four-together-actions-and-user-gender-
    /// design.md §6): each behaves as `jog_together` does. `cycle_together`
    /// is refused, and costs nothing, to a `fantasy` persona. A refused
    /// `cycle_together` or `hike_together` drops bond, as a refused
    /// `jog_together` or `ride_horse_together` does (spec 2026-11-27-swim-
    /// outfit-ladder-and-fish-together-design.md §7).
    WorkoutTogether,
    SwimTogether,
    CycleTogether,
    HikeTogether,
    /// The user asks her to dance, go adventuring, play with fire, sing,
    /// skinny-dip or commit a crime with them (spec 2026-11-26-more-together-
    /// actions-bath-outfit-chemistry-drop-and-custom-together-design.md §4).
    /// Adventure, fire and crime are thrill actions and a refusal drops bond;
    /// `skinny_dip_together` is a nude action, answered in the `emotional`
    /// tone; a refused dance, song or skinny dip drops chemistry (spec
    /// 2026-11-27-together-action-groups-design.md §5).
    DanceTogether,
    AdventureTogether,
    PlayWithFireTogether,
    SingTogether,
    SkinnyDipTogether,
    CommitCrimeTogether,
    /// The user asks her to go fishing with them (spec 2026-11-27-swim-
    /// outfit-ladder-and-fish-together-design.md §6.1): behaves as
    /// `hike_together` does, two to three hours.
    FishTogether,
    /// The user asks her to ride a horse naked with them (spec 2026-11-27-
    /// swim-outfit-ladder-and-fish-together-design.md §6.2): a nude action,
    /// the counterpart of `ride_horse_together`, so a refusal drops bond and
    /// chemistry (spec 2026-11-27-together-action-groups-design.md §5.3).
    RideHorseNakedTogether,
    /// The user asks her to do yoga, hike or take a hot spring naked with
    /// them (spec 2026-11-27-together-action-groups-design.md §6): nude
    /// counterparts of `yoga_together`, `hike_together` and
    /// `hot_spring_together`.
    YogaNakedTogether,
    HikeNakedTogether,
    HotSpringNakedTogether,
    /// The user asks her to do something with them, in their own words (spec
    /// 2026-11-26 §8): behaves as `jog_together` does, placing
    /// `custom_together`. A general action: not emotional, and a refusal
    /// drops chemistry as `custom`'s does. The row it places points at the
    /// user's message, which is where the text lives.
    CustomTogether {
        text: String,
    },
    /// The user challenges her, or proposes a bet: social actions whose
    /// answer says whether she takes it up; no winner is decided, and a
    /// refusal drops nothing (spec 2026-11-23 §4).
    Challenge,
    Bet,
    Custom {
        text: String,
    },
    /// Money handed over (spec 2026-10-11-tip-as-action-design.md). The amount
    /// is the caller's; the engine records it and never verifies it.
    Tip {
        amount_usd: f64,
    },
    /// Something the persona keeps (spec
    /// 2026-10-12-persona-items-and-gift-design.md). Accepted, it joins her
    /// items or her wardrobe by `kind`.
    Gift {
        kind: ItemKind,
        name: String,
    },
    /// The user invites her out; accepted, the date runs as an activity (spec
    /// 2026-11-12-activity-sub-session-design.md).
    Date {
        duration: DateDuration,
        place: String,
        content: String,
    },
}

/// How long a date the user proposes lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DateDuration {
    Short,
    Mid,
}

impl DateDuration {
    pub fn hours(self) -> i64 {
        match self {
            DateDuration::Short => 1,
            DateDuration::Mid => 4,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            DateDuration::Short => "short",
            DateDuration::Mid => "mid",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "short" => Some(DateDuration::Short),
            "mid" => Some(DateDuration::Mid),
            _ => None,
        }
    }
}

/// The together actions she may turn down only for not caring for the
/// outdoors: a refusal drops bond (spec 2026-11-27-swim-outfit-ladder-and-
/// fish-together-design.md §7).
const OUTDOOR_ACTIONS: [UserAction; 5] = [
    UserAction::JogTogether,
    UserAction::HikeTogether,
    UserAction::FishTogether,
    UserAction::CycleTogether,
    UserAction::RideHorseTogether,
];

impl UserAction {
    /// Whether her reply to this action is written in the `emotional` tone,
    /// accepted or refused, when the deployment has the tone on (spec
    /// 2026-11-12-emotional-action-replies-design.md §2): every nude together
    /// action (spec 2026-11-27-together-action-groups-design.md §5.1) and
    /// the gestures and status asks below.
    pub fn speaks_emotionally(&self) -> bool {
        self.together_group() == Some(TogetherGroup::Nude)
            || matches!(
                self,
                UserAction::FrenchKiss
                    | UserAction::Tease
                    | UserAction::HugFromBehind
                    | UserAction::Touch
                    | UserAction::Strip
                    | UserAction::Propose
                    | UserAction::BreakUp
                    | UserAction::Threaten
                    | UserAction::Tip { .. }
            )
    }

    /// The group of the kind this together action places (spec 2026-11-27
    /// §4.1); `None` for every other action.
    pub fn together_group(&self) -> Option<TogetherGroup> {
        self.together_kind().and_then(EventKind::together_group)
    }

    /// The status ask this action puts to her (spec 2026-11-23-affinity-50-
    /// design.md §8): `propose` and `break_up`. `None` for every other
    /// action.
    pub fn status_ask(&self) -> Option<StatusAsk> {
        match self {
            UserAction::Propose => Some(StatusAsk::Propose),
            UserAction::BreakUp => Some(StatusAsk::BreakUp),
            _ => None,
        }
    }

    /// The event an accepted together action puts her in: `None` for every
    /// other action.
    pub fn together_kind(&self) -> Option<EventKind> {
        match self {
            UserAction::ShowerTogether => Some(EventKind::ShoweringTogether),
            UserAction::BathTogether => Some(EventKind::BathingTogether),
            UserAction::SaunaTogether => Some(EventKind::SaunaTogether),
            UserAction::HotSpringTogether => Some(EventKind::HotSpringTogether),
            UserAction::FightTogether => Some(EventKind::FightingTogether),
            UserAction::ToiletTogether => Some(EventKind::UsingToiletTogether),
            UserAction::SmokeTogether => Some(EventKind::SmokingTogether),
            UserAction::FallAsleepTogether => Some(EventKind::AsleepTogether),
            UserAction::SexTogether => Some(EventKind::HavingSexTogether),
            UserAction::WalkTogether => Some(EventKind::WalkingTogether),
            UserAction::SmokeWeedTogether => Some(EventKind::SmokingWeedTogether),
            UserAction::JogTogether => Some(EventKind::JoggingTogether),
            UserAction::YogaTogether => Some(EventKind::YogaTogether),
            UserAction::StargazeTogether => Some(EventKind::StargazingTogether),
            UserAction::RideHorseTogether => Some(EventKind::HorseRidingTogether),
            UserAction::StreakTogether => Some(EventKind::StreakingTogether),
            UserAction::DrinkTogether => Some(EventKind::DrinkingTogether),
            UserAction::WorkoutTogether => Some(EventKind::WorkingOutTogether),
            UserAction::SwimTogether => Some(EventKind::SwimmingTogether),
            UserAction::CycleTogether => Some(EventKind::CyclingTogether),
            UserAction::HikeTogether => Some(EventKind::HikingTogether),
            UserAction::DanceTogether => Some(EventKind::DancingTogether),
            UserAction::AdventureTogether => Some(EventKind::AdventuringTogether),
            UserAction::PlayWithFireTogether => Some(EventKind::PlayingWithFireTogether),
            UserAction::SingTogether => Some(EventKind::SingingTogether),
            UserAction::SkinnyDipTogether => Some(EventKind::SkinnyDippingTogether),
            UserAction::CommitCrimeTogether => Some(EventKind::CommittingCrimeTogether),
            UserAction::FishTogether => Some(EventKind::FishingTogether),
            UserAction::RideHorseNakedTogether => Some(EventKind::NakedHorseRidingTogether),
            UserAction::YogaNakedTogether => Some(EventKind::NakedYogaTogether),
            UserAction::HikeNakedTogether => Some(EventKind::NakedHikingTogether),
            UserAction::HotSpringNakedTogether => Some(EventKind::NakedHotSpringTogether),
            UserAction::CustomTogether { .. } => Some(EventKind::CustomTogether),
            _ => None,
        }
    }

    /// The user's own words of a `custom_together`: what the event it places
    /// is about. `None` for every other action.
    pub fn together_text(&self) -> Option<&str> {
        match self {
            UserAction::CustomTogether { text } => Some(text),
            _ => None,
        }
    }

    /// What this action has her take, and how many: a `give` its item and
    /// quantity, a habit action one cigarette, herb or drink (spec
    /// 2026-11-23-twelve-together-actions-and-propose-design.md §5). The habit
    /// roll, the need move and the lingering effect all read it. `None` for
    /// every other action.
    pub fn consumed(&self) -> Option<(GivenItem, u8)> {
        match self {
            UserAction::Give { item, quantity, .. } => Some((*item, *quantity)),
            UserAction::SmokeTogether => Some((GivenItem::Cigarette, 1)),
            UserAction::SmokeWeedTogether => Some((GivenItem::Herb, 1)),
            UserAction::DrinkTogether => Some((GivenItem::Alcohol, 1)),
            _ => None,
        }
    }

    /// The lines a refusal of this action drops, each one tier (spec
    /// 2026-11-27-together-action-groups-design.md §5.3). Friendship
    /// (`bond`): handing something over, giving something to keep, a thrill
    /// action, or a jog, a hike, a fishing trip, a bike ride or a horse ride,
    /// which she may turn down only for not caring for the outdoors. Both
    /// lines: the nude counterpart of one of those outdoor five (a streak, a
    /// naked hike, a naked ride). Romance (`chemistry`): every other physical
    /// gesture. None: a tip (spec 2026-10-11 §6), seeing through a bluff, a
    /// threat or a trick (spec 2026-11-08 §2.2), turning down a date, a
    /// proposal, a break-up, a challenge or a bet.
    pub fn refusal_lines(&self) -> &'static [AffinityLine] {
        use AffinityLine::{Bond, Chemistry};
        match self {
            UserAction::Tip { .. }
            | UserAction::Bluff
            | UserAction::Threaten
            | UserAction::Trick
            | UserAction::Propose
            | UserAction::BreakUp
            | UserAction::Challenge
            | UserAction::Bet
            | UserAction::Date { .. } => &[],
            UserAction::Give { .. } | UserAction::Gift { .. } => &[Bond],
            a if OUTDOOR_ACTIONS.contains(a) => &[Bond],
            a => match a.together_kind() {
                Some(k) if k.together_group() == Some(TogetherGroup::Thrill) => &[Bond],
                Some(k)
                    if k.clothed_counterpart().is_some_and(|c| {
                        OUTDOOR_ACTIONS.iter().any(|o| o.together_kind() == Some(c))
                    }) =>
                {
                    &[Bond, Chemistry]
                }
                _ => &[Chemistry],
            },
        }
    }
}

/// What a `give` action hands over. Consumables linger on the persona
/// (`effects::curve`); valuables do not (spec
/// 2026-10-13-give-categories-design.md §2). The category is fixed; the
/// optional `name` on the action is free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GivenItem {
    Cigarette,
    Alcohol,
    Medicine,
    Food,
    SoftDrink,
    Tea,
    Coffee,
    Herb,
    /// A proper dish or meal; `food` is something to eat.
    Cuisine,
    Watch,
    Gem,
    Gold,
    Camera,
    Cash,
    Antique,
    Artwork,
    Handmade,
    CreditCard,
    Flower,
    Jewelry,
    Car,
    Motorcycle,
    Boat,
    Horse,
}

/// Which of the persona's lists an item belongs to (spec
/// 2026-10-12-persona-items-and-gift-design.md): `item` for what she owns,
/// `clothing` for her wardrobe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Item,
    Clothing,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Item => "item",
            ItemKind::Clothing => "clothing",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "item" => Some(ItemKind::Item),
            "clothing" => Some(ItemKind::Clothing),
            _ => None,
        }
    }
}

/// Whether the persona accepts a user action. Decided by the PDE judge before
/// the reply; accept whenever the judge did not decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionResponse {
    Accept,
    Refuse,
}

impl ActionResponse {
    pub fn as_str(self) -> &'static str {
        match self {
            ActionResponse::Accept => "accept",
            ActionResponse::Refuse => "refuse",
        }
    }
}

/// Which reference image an image turn should build on. `Face` = the static
/// avatar / face reference; `Previous` = the previously generated image
/// (iteration). Defaults to `Face`. Serialized (snake_case) into the delegated
/// `image_request` SSE frame; deserialized from the PDE verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageRef {
    #[default]
    Face,
    Previous,
}

/// Action decision produced by the PDE. NOT serialized to any DB/SSE wire path
/// (the persisted action string and `FrameActionType` are separate) — so the
/// rename is internal-only and the serde derive is intentionally absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionType {
    ReplyText,
    Ghost,
    ReplyImage,     // reserved — degrades to ReplyText until the image executor ships
    ReplyTextImage, // reserved — degrades to ReplyText until the image executor ships
    /// Out-of-character product answer (PDE action `product_qa`): a dedicated
    /// executor replies from operator-authored product docs; the turn is
    /// invisible to the companion brain. Never produced by the rule engine.
    ProductQa,
    Proactive, // KEPT — built by pde::decide for ProactiveTrigger/AppOpen; matched in post_process
}

impl ActionType {
    /// True for actions that produce a text reply downstream. Image variants are
    /// included so they route through the reply path once the executor ships;
    /// today the PDE guardrails degrade them to `ReplyText` first.
    pub fn is_text_reply(self) -> bool {
        matches!(
            self,
            ActionType::ReplyText | ActionType::ReplyImage | ActionType::ReplyTextImage
        )
    }

    /// True for actions that promise a photo this turn. An empty text
    /// completion on these is the expected shape of an image-only reply, not a
    /// silent turn — the stream's ghost-fallback tagging must not fire for
    /// them (the trailing `image_request` is the turn's real payload).
    pub fn promises_image(self) -> bool {
        matches!(self, ActionType::ReplyImage | ActionType::ReplyTextImage)
    }

    /// True for actions whose turn carries a text reply the persona speaks.
    /// Narrower than `is_text_reply`: a bare `ReplyImage` routes through the
    /// reply path but bears no text, so mode/dice/tone-like inputs drop.
    pub fn bears_text(self) -> bool {
        matches!(self, ActionType::ReplyText | ActionType::ReplyTextImage)
    }
}

/// Tone directive for the reply generation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReplyStyle {
    Warm,
    Neutral,
    Cold,
    Tsundere,
    Excited,
}

/// How this turn's reply relates to the user's message — the judge's call
/// (spec 2026-09-13 §3.1). `None` on the wire means the judge is unsure and
/// the engine's dice take over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyMode {
    Listen,
    Follow,
    Ask,
    Judge,
}

impl ReplyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ReplyMode::Listen => "listen",
            ReplyMode::Follow => "follow",
            ReplyMode::Ask => "ask",
            ReplyMode::Judge => "judge",
        }
    }
}

/// Engine-rolled per-turn nudges, rendered as the prompt's `[this_turn]`
/// block (all-false ⇒ block omitted). Rolled at plan finalisation in the
/// stream — never inside `build_prompt` — and serialised into the decision
/// audit payload (spec 2026-09-13 §3.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TurnNudges {
    pub affirm: bool,
    pub share_slice: bool,
    pub open_question: bool,
}

/// Output of the PDE.
#[derive(Debug, Clone)]
pub struct ActionPlan {
    pub action_type: ActionType,
    pub reply_style: ReplyStyle,
    pub affinity_deltas: AffinityDeltas,
    pub energy_cost: f64,
    pub context_hints: Vec<String>,
    /// Judge-decided reply mode for this turn. `Some` only on LLM-judge,
    /// text-bearing turns (reply_text / reply_text_image); `None` everywhere
    /// else — rule PDE, fail-open, tips, ghost, reply_image — and `None`
    /// means the dice own the turn's cadence.
    pub reply_mode: Option<ReplyMode>,
    /// The dice as rolled at plan finalisation. All-false when a definite
    /// reply_mode suppressed the roll, when the action bears no text, or on
    /// paths that never roll (proactive, edit turns).
    pub nudges: TurnNudges,
    /// What the persona has on this turn: the caller's named outfit when the
    /// event carries one, else the judge's (free text, sanitized by the
    /// server). Carried on every reply plan, a bare image turn included,
    /// since the composer dresses the picture with it; `None` on ghost,
    /// product_qa and proactive plans.
    pub clothing: Option<String>,
    /// What the picture showed, for post-process affinity on image turns.
    /// Written by the stream AFTER the composer resolves — always `None` at
    /// decision time, since the composer has not run yet. `None` also on the
    /// `raw` variant and on a failed compose, where the affinity evaluator
    /// falls back to a generic photo marker.
    pub image_caption: Option<String>,
    /// Which reference image an image turn builds on (avatar vs previous gen).
    /// `Face` for non-image actions.
    pub image_ref: ImageRef,
    /// Aspect ratio chosen by the PDE for an image turn; `None` ⇒ caller falls
    /// back (request → config default). Always `None` for non-image actions.
    pub aspect_ratio: Option<String>,
    /// The response in effect for this turn's user action: `Some` exactly when
    /// the event carries an action and the plan replies; `None` otherwise.
    pub action_response: Option<ActionResponse>,
    /// A refusal ruled before the judge that drops no line: one her period's
    /// roll decided (spec 2026-11-24-period-cycle-design.md §8), which
    /// cannot be recomputed from the event after the turn.
    pub refusal_free: bool,
}

/// Conversation signals computed from chat history.
#[derive(Debug, Clone, Default)]
pub struct ConversationSignals {
    pub message_count: i64,
    pub hours_since_last_message: f64,
    pub ghost_streak: i32,
    pub hours_since_last_ghost: Option<f64>,
}

/// Response from the chat engine — text reply plus opaque OpenRouter
/// wire echoes (`generation_id`, `model`, `usage`). The three echo
/// fields are `None` when no LLM call was made (handler returned None)
/// or when upstream omitted them.
#[derive(Debug, Clone, Default)]
pub struct ChatResponse {
    pub reply: String,
    pub generation_id: Option<String>,
    pub model: Option<String>,
    pub usage: Option<serde_json::Value>,
}

/// Input bundle consumed by the PDE.
#[derive(Clone)]
pub struct DecisionInput {
    pub event: Event,
    pub affinity: Affinity,
    pub persona: CompanionPersona,
    pub signals: ConversationSignals,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nine_actions_speak_emotionally() {
        let every = [
            UserAction::Give {
                item: GivenItem::Food,
                name: None,
                quantity: 1,
            },
            UserAction::Kiss,
            UserAction::Hug,
            UserAction::Touch,
            UserAction::Lick,
            UserAction::FrenchKiss,
            UserAction::HugFromBehind,
            UserAction::Tease,
            UserAction::Pat,
            UserAction::Wink,
            UserAction::Blink,
            UserAction::AskForDance,
            UserAction::AskDancing,
            UserAction::Photo,
            UserAction::Bluff,
            UserAction::Threaten,
            UserAction::Trick,
            UserAction::Strip,
            UserAction::ShowerTogether,
            UserAction::BathTogether,
            UserAction::SaunaTogether,
            UserAction::Custom { text: "x".into() },
            UserAction::Tip { amount_usd: 5.0 },
            UserAction::Gift {
                kind: ItemKind::Item,
                name: "x".into(),
            },
        ];
        let listed: Vec<&UserAction> = every.iter().filter(|a| a.speaks_emotionally()).collect();
        assert_eq!(
            listed,
            [
                &UserAction::Touch,
                &UserAction::FrenchKiss,
                &UserAction::HugFromBehind,
                &UserAction::Tease,
                &UserAction::Threaten,
                &UserAction::Strip,
                &UserAction::ShowerTogether,
                &UserAction::BathTogether,
                &UserAction::Tip { amount_usd: 5.0 },
            ]
        );
    }

    /// Spec 2026-11-20-together-actions-state-in-picture-and-genome-routes-
    /// design.md §5.
    #[test]
    fn the_together_actions_name_their_events() {
        for (a, wire, kind) in [
            (
                UserAction::ShowerTogether,
                "shower_together",
                EventKind::ShoweringTogether,
            ),
            (
                UserAction::BathTogether,
                "bath_together",
                EventKind::BathingTogether,
            ),
            (
                UserAction::SaunaTogether,
                "sauna_together",
                EventKind::SaunaTogether,
            ),
            // Spec 2026-11-23-twelve-together-actions-and-propose-design.md §4.
            (
                UserAction::ToiletTogether,
                "toilet_together",
                EventKind::UsingToiletTogether,
            ),
            (
                UserAction::SmokeTogether,
                "smoke_together",
                EventKind::SmokingTogether,
            ),
            (
                UserAction::FallAsleepTogether,
                "fall_asleep_together",
                EventKind::AsleepTogether,
            ),
            (
                UserAction::SexTogether,
                "sex_together",
                EventKind::HavingSexTogether,
            ),
            (
                UserAction::WalkTogether,
                "walk_together",
                EventKind::WalkingTogether,
            ),
            (
                UserAction::SmokeWeedTogether,
                "smoke_weed_together",
                EventKind::SmokingWeedTogether,
            ),
            (
                UserAction::JogTogether,
                "jog_together",
                EventKind::JoggingTogether,
            ),
            (
                UserAction::YogaTogether,
                "yoga_together",
                EventKind::YogaTogether,
            ),
            (
                UserAction::StargazeTogether,
                "stargaze_together",
                EventKind::StargazingTogether,
            ),
            (
                UserAction::RideHorseTogether,
                "ride_horse_together",
                EventKind::HorseRidingTogether,
            ),
            (
                UserAction::StreakTogether,
                "streak_together",
                EventKind::StreakingTogether,
            ),
            (
                UserAction::DrinkTogether,
                "drink_together",
                EventKind::DrinkingTogether,
            ),
        ] {
            let v = serde_json::json!({ "type": wire });
            assert_eq!(serde_json::to_value(&a).unwrap(), v);
            assert_eq!(serde_json::from_value::<UserAction>(v).unwrap(), a);
            assert_eq!(a.together_kind(), Some(kind), "{a:?}");
            // Spec 2026-11-27-swim-outfit-ladder-and-fish-together-design.md §7.
            let lines: &[AffinityLine] = match a {
                UserAction::JogTogether | UserAction::RideHorseTogether => &[AffinityLine::Bond],
                // Spec 2026-11-27-together-action-groups-design.md §5.3.
                UserAction::StreakTogether => &[AffinityLine::Bond, AffinityLine::Chemistry],
                _ => &[AffinityLine::Chemistry],
            };
            assert_eq!(a.refusal_lines(), lines, "{a:?}");
        }
        for a in [
            UserAction::Kiss,
            UserAction::Strip,
            UserAction::AskDancing,
            UserAction::Photo,
            UserAction::Custom { text: "x".into() },
            UserAction::Date {
                duration: DateDuration::Short,
                place: "海边".into(),
                content: "看海".into(),
            },
        ] {
            assert_eq!(a.together_kind(), None, "{a:?}");
        }
    }

    /// Spec 2026-11-23-twelve-together-actions-and-propose-design.md §4.2,
    /// §7: of the thirteen, bed, streaking and a proposal are charged.
    #[test]
    fn sex_streaking_and_a_proposal_speak_emotionally() {
        for a in [
            UserAction::SexTogether,
            UserAction::StreakTogether,
            UserAction::Propose,
        ] {
            assert!(a.speaks_emotionally(), "{a:?}");
        }
        for a in [
            UserAction::ToiletTogether,
            UserAction::SmokeTogether,
            UserAction::FallAsleepTogether,
            UserAction::WalkTogether,
            UserAction::SmokeWeedTogether,
            UserAction::JogTogether,
            UserAction::YogaTogether,
            UserAction::StargazeTogether,
            UserAction::RideHorseTogether,
            UserAction::DrinkTogether,
        ] {
            assert!(!a.speaks_emotionally(), "{a:?}");
        }
    }

    /// Spec 2026-11-23-twelve-together-actions-and-propose-design.md §5.
    #[test]
    fn a_habit_action_has_her_take_one_of_the_item() {
        use GivenItem::*;
        assert_eq!(UserAction::SmokeTogether.consumed(), Some((Cigarette, 1)));
        assert_eq!(UserAction::SmokeWeedTogether.consumed(), Some((Herb, 1)));
        assert_eq!(UserAction::DrinkTogether.consumed(), Some((Alcohol, 1)));
        let give = UserAction::Give {
            item: Coffee,
            name: None,
            quantity: 3,
        };
        assert_eq!(give.consumed(), Some((Coffee, 3)));
        for a in [
            UserAction::WalkTogether,
            UserAction::SexTogether,
            UserAction::Kiss,
            UserAction::Propose,
        ] {
            assert_eq!(a.consumed(), None, "{a:?}");
        }
    }

    /// Spec 2026-11-23-hot-spring-fight-challenge-bet-design.md §4.
    #[test]
    fn hot_spring_and_fight_together_name_their_events() {
        for (a, wire, kind) in [
            (
                UserAction::HotSpringTogether,
                "hot_spring_together",
                EventKind::HotSpringTogether,
            ),
            (
                UserAction::FightTogether,
                "fight_together",
                EventKind::FightingTogether,
            ),
        ] {
            let v = serde_json::json!({ "type": wire });
            assert_eq!(serde_json::to_value(&a).unwrap(), v);
            assert_eq!(serde_json::from_value::<UserAction>(v).unwrap(), a);
            assert_eq!(a.together_kind(), Some(kind), "{a:?}");
            // A fight is a thrill action (spec 2026-11-27-together-action-groups-design.md §5.3).
            let line = if a == UserAction::FightTogether {
                AffinityLine::Bond
            } else {
                AffinityLine::Chemistry
            };
            assert_eq!(a.refusal_lines(), [line], "{a:?}");
            assert!(!a.speaks_emotionally(), "{a:?}");
        }
    }

    /// Spec 2026-11-24-focused-tone-four-together-actions-and-user-gender-
    /// design.md §6.
    #[test]
    fn workout_swim_cycle_and_hike_together_name_their_events() {
        for (a, wire, kind) in [
            (
                UserAction::WorkoutTogether,
                "workout_together",
                EventKind::WorkingOutTogether,
            ),
            (
                UserAction::SwimTogether,
                "swim_together",
                EventKind::SwimmingTogether,
            ),
            (
                UserAction::CycleTogether,
                "cycle_together",
                EventKind::CyclingTogether,
            ),
            (
                UserAction::HikeTogether,
                "hike_together",
                EventKind::HikingTogether,
            ),
        ] {
            let v = serde_json::json!({ "type": wire });
            assert_eq!(serde_json::to_value(&a).unwrap(), v);
            assert_eq!(serde_json::from_value::<UserAction>(v).unwrap(), a);
            assert_eq!(a.together_kind(), Some(kind), "{a:?}");
            // Spec 2026-11-27-swim-outfit-ladder-and-fish-together-design.md §7.
            let line = if matches!(a, UserAction::CycleTogether | UserAction::HikeTogether) {
                AffinityLine::Bond
            } else {
                AffinityLine::Chemistry
            };
            assert_eq!(a.refusal_lines(), [line], "{a:?}");
            assert!(!a.speaks_emotionally(), "{a:?}");
            assert_eq!(a.consumed(), None, "{a:?}");
            assert_eq!(a.status_ask(), None, "{a:?}");
        }
    }

    /// Spec 2026-11-26-more-together-actions-bath-outfit-chemistry-drop-and-
    /// custom-together-design.md §4.
    #[test]
    fn six_more_together_actions_name_their_events() {
        use EventKind::*;
        for (a, wire, kind) in [
            (UserAction::DanceTogether, "dance_together", DancingTogether),
            (
                UserAction::AdventureTogether,
                "adventure_together",
                AdventuringTogether,
            ),
            (
                UserAction::PlayWithFireTogether,
                "play_with_fire_together",
                PlayingWithFireTogether,
            ),
            (UserAction::SingTogether, "sing_together", SingingTogether),
            (
                UserAction::SkinnyDipTogether,
                "skinny_dip_together",
                SkinnyDippingTogether,
            ),
            (
                UserAction::CommitCrimeTogether,
                "commit_crime_together",
                CommittingCrimeTogether,
            ),
        ] {
            let v = serde_json::json!({ "type": wire });
            assert_eq!(serde_json::to_value(&a).unwrap(), v);
            assert_eq!(serde_json::from_value::<UserAction>(v).unwrap(), a);
            assert_eq!(a.together_kind(), Some(kind), "{a:?}");
            assert_eq!(a.consumed(), None, "{a:?}");
            assert_eq!(a.status_ask(), None, "{a:?}");
        }
    }

    /// Spec 2026-11-26-more-together-actions-bath-outfit-chemistry-drop-and-
    /// custom-together-design.md §8.1: the one together action with a payload.
    #[test]
    fn custom_together_carries_its_text() {
        let a = UserAction::CustomTogether {
            text: "去海边放烟花".into(),
        };
        let v = serde_json::json!({ "type": "custom_together", "text": "去海边放烟花" });
        assert_eq!(serde_json::to_value(&a).unwrap(), v);
        assert_eq!(serde_json::from_value::<UserAction>(v).unwrap(), a);
        assert_eq!(a.together_kind(), Some(EventKind::CustomTogether));
        assert_eq!(a.together_text(), Some("去海边放烟花"));
        assert_eq!(a.together_group(), Some(TogetherGroup::General));
        assert!(!a.speaks_emotionally());
        assert_eq!(a.refusal_lines(), [AffinityLine::Chemistry]);
        assert_eq!(a.consumed(), None);
        assert_eq!(a.status_ask(), None);
        // No other action carries text for a together event.
        for other in all_actions() {
            if !matches!(other, UserAction::CustomTogether { .. }) {
                assert_eq!(other.together_text(), None, "{other:?}");
            }
        }
    }

    #[test]
    fn only_skinny_dip_of_the_six_speaks_emotionally_and_only_crime_drops_bond() {
        assert!(UserAction::SkinnyDipTogether.speaks_emotionally());
        for a in [
            UserAction::DanceTogether,
            UserAction::AdventureTogether,
            UserAction::PlayWithFireTogether,
            UserAction::SingTogether,
            UserAction::CommitCrimeTogether,
        ] {
            assert!(!a.speaks_emotionally(), "{a:?}");
        }
        assert_eq!(
            UserAction::CommitCrimeTogether.refusal_lines(),
            [AffinityLine::Bond]
        );
        assert_eq!(
            UserAction::SkinnyDipTogether.refusal_lines(),
            [AffinityLine::Chemistry]
        );
        assert_eq!(
            UserAction::SingTogether.refusal_lines(),
            [AffinityLine::Chemistry]
        );
    }

    /// Spec 2026-11-27-swim-outfit-ladder-and-fish-together-design.md §7: she
    /// may simply not care for the outdoors.
    #[test]
    fn outdoor_together_refusals_drop_bond() {
        for a in [
            UserAction::JogTogether,
            UserAction::HikeTogether,
            UserAction::FishTogether,
            UserAction::CycleTogether,
            UserAction::RideHorseTogether,
        ] {
            assert_eq!(a.refusal_lines(), [AffinityLine::Bond], "{a:?}");
        }
        for a in [
            UserAction::WalkTogether,
            UserAction::SwimTogether,
            UserAction::WorkoutTogether,
            UserAction::YogaTogether,
            UserAction::StargazeTogether,
            UserAction::DanceTogether,
            UserAction::SingTogether,
        ] {
            assert_eq!(a.refusal_lines(), [AffinityLine::Chemistry], "{a:?}");
        }
    }

    /// Spec 2026-11-27-together-action-groups-design.md §5.3: thrill drops
    /// bond; a nude counterpart of a bond-dropping outdoor action drops both.
    #[test]
    fn refusal_lines_by_group_and_counterpart() {
        use AffinityLine::{Bond, Chemistry};
        const OUTDOOR: [UserAction; 5] = [
            UserAction::JogTogether,
            UserAction::HikeTogether,
            UserAction::FishTogether,
            UserAction::CycleTogether,
            UserAction::RideHorseTogether,
        ];
        const BOTH: [UserAction; 3] = [
            UserAction::StreakTogether,
            UserAction::HikeNakedTogether,
            UserAction::RideHorseNakedTogether,
        ];
        for a in all_actions()
            .into_iter()
            .filter(|a| a.together_kind().is_some())
        {
            let want: &[AffinityLine] = if BOTH.contains(&a) {
                &[Bond, Chemistry]
            } else if OUTDOOR.contains(&a) || THRILL_ACTIONS.contains(&a) {
                &[Bond]
            } else {
                &[Chemistry]
            };
            assert_eq!(a.refusal_lines(), want, "{a:?}");
        }
        for a in [
            UserAction::SkinnyDipTogether,
            UserAction::YogaNakedTogether,
            UserAction::HotSpringNakedTogether,
            UserAction::ShowerTogether,
            UserAction::BathTogether,
            UserAction::SexTogether,
        ] {
            assert_eq!(a.refusal_lines(), [Chemistry], "{a:?}");
        }
    }

    /// Spec 2026-11-27-swim-outfit-ladder-and-fish-together-design.md §6.1.
    #[test]
    fn fish_together_names_its_event() {
        let a = UserAction::FishTogether;
        let v = serde_json::json!({ "type": "fish_together" });
        assert_eq!(serde_json::to_value(&a).unwrap(), v);
        assert_eq!(serde_json::from_value::<UserAction>(v).unwrap(), a);
        assert_eq!(a.together_kind(), Some(EventKind::FishingTogether));
        assert_eq!(a.consumed(), None);
        assert_eq!(a.status_ask(), None);
        assert_eq!(a.together_text(), None);
        assert!(!a.speaks_emotionally());
        assert_eq!(a.together_group(), Some(TogetherGroup::General));
    }

    /// Spec 2026-11-27-swim-outfit-ladder-and-fish-together-design.md §6.2.
    #[test]
    fn ride_horse_naked_together_is_nude_and_emotional() {
        let a = UserAction::RideHorseNakedTogether;
        let v = serde_json::json!({ "type": "ride_horse_naked_together" });
        assert_eq!(serde_json::to_value(&a).unwrap(), v);
        assert_eq!(serde_json::from_value::<UserAction>(v).unwrap(), a);
        assert_eq!(a.together_kind(), Some(EventKind::NakedHorseRidingTogether));
        assert_eq!(a.together_group(), Some(TogetherGroup::Nude));
        assert!(a.speaks_emotionally());
        assert_eq!(
            a.refusal_lines(),
            [AffinityLine::Bond, AffinityLine::Chemistry]
        );
        assert_eq!(a.consumed(), None);
        assert_eq!(a.status_ask(), None);
        assert_eq!(a.together_text(), None);
    }

    /// Spec 2026-11-27-together-action-groups-design.md §6.
    #[test]
    fn three_nude_actions_name_their_events() {
        use EventKind::*;
        for (a, wire, kind) in [
            (
                UserAction::YogaNakedTogether,
                "yoga_naked_together",
                NakedYogaTogether,
            ),
            (
                UserAction::HikeNakedTogether,
                "hike_naked_together",
                NakedHikingTogether,
            ),
            (
                UserAction::HotSpringNakedTogether,
                "hot_spring_naked_together",
                NakedHotSpringTogether,
            ),
        ] {
            let v = serde_json::json!({ "type": wire });
            assert_eq!(serde_json::to_value(&a).unwrap(), v);
            assert_eq!(serde_json::from_value::<UserAction>(v).unwrap(), a);
            assert_eq!(a.together_kind(), Some(kind), "{a:?}");
            assert_eq!(a.consumed(), None, "{a:?}");
            assert_eq!(a.status_ask(), None, "{a:?}");
            assert_eq!(a.together_text(), None, "{a:?}");
        }
    }

    /// One of every action: each unit variant and one of each payload variant.
    fn all_actions() -> Vec<UserAction> {
        use UserAction::*;
        vec![
            Give {
                item: GivenItem::Food,
                name: None,
                quantity: 1,
            },
            Kiss,
            Hug,
            Touch,
            Lick,
            FrenchKiss,
            HugFromBehind,
            Tease,
            Pat,
            Wink,
            Blink,
            AskForDance,
            AskDancing,
            Photo,
            Bluff,
            Threaten,
            Trick,
            Strip,
            ShowerTogether,
            BathTogether,
            SaunaTogether,
            ToiletTogether,
            SmokeTogether,
            FallAsleepTogether,
            SexTogether,
            WalkTogether,
            SmokeWeedTogether,
            JogTogether,
            YogaTogether,
            StargazeTogether,
            RideHorseTogether,
            StreakTogether,
            DrinkTogether,
            Propose,
            BreakUp,
            HotSpringTogether,
            FightTogether,
            WorkoutTogether,
            SwimTogether,
            CycleTogether,
            HikeTogether,
            DanceTogether,
            AdventureTogether,
            PlayWithFireTogether,
            SingTogether,
            SkinnyDipTogether,
            CommitCrimeTogether,
            FishTogether,
            RideHorseNakedTogether,
            YogaNakedTogether,
            HikeNakedTogether,
            HotSpringNakedTogether,
            CustomTogether { text: "x".into() },
            Challenge,
            Bet,
            Custom { text: "x".into() },
            Tip { amount_usd: 5.0 },
            Gift {
                kind: ItemKind::Item,
                name: "x".into(),
            },
            Date {
                duration: DateDuration::Short,
                place: "海边".into(),
                content: "看海".into(),
            },
        ]
    }

    /// The kinds a chemistry drop ends are exactly the kinds the nude
    /// actions place (spec 2026-11-27-together-action-groups-design.md §5.1).
    #[test]
    fn nude_actions_place_the_kinds_a_chemistry_drop_ends() {
        let sorted = |mut kinds: Vec<EventKind>| {
            kinds.sort_by_key(|k| k.as_str());
            kinds
        };
        let nude: Vec<EventKind> = all_actions()
            .iter()
            .filter(|a| a.together_group() == Some(TogetherGroup::Nude))
            .filter_map(|a| a.together_kind())
            .collect();
        assert_eq!(
            sorted(nude),
            sorted(EventKind::ENDS_ON_CHEMISTRY_DROP.to_vec())
        );
    }

    const NUDE_ACTIONS: [UserAction; 9] = [
        UserAction::ShowerTogether,
        UserAction::BathTogether,
        UserAction::SexTogether,
        UserAction::StreakTogether,
        UserAction::SkinnyDipTogether,
        UserAction::RideHorseNakedTogether,
        UserAction::YogaNakedTogether,
        UserAction::HikeNakedTogether,
        UserAction::HotSpringNakedTogether,
    ];

    const THRILL_ACTIONS: [UserAction; 4] = [
        UserAction::AdventureTogether,
        UserAction::CommitCrimeTogether,
        UserAction::PlayWithFireTogether,
        UserAction::FightTogether,
    ];

    /// Spec 2026-11-27-together-action-groups-design.md §4.1.
    #[test]
    fn every_together_action_has_its_group() {
        for a in all_actions() {
            let want = if NUDE_ACTIONS.contains(&a) {
                Some(TogetherGroup::Nude)
            } else if THRILL_ACTIONS.contains(&a) {
                Some(TogetherGroup::Thrill)
            } else if a.together_kind().is_some() {
                Some(TogetherGroup::General)
            } else {
                None
            };
            assert_eq!(a.together_group(), want, "{a:?}");
        }
        for a in [
            UserAction::FallAsleepTogether,
            UserAction::SwimTogether,
            UserAction::HotSpringTogether,
        ] {
            assert_eq!(a.together_group(), Some(TogetherGroup::General), "{a:?}");
        }
    }

    /// Spec 2026-11-27-together-action-groups-design.md §5.1: every nude
    /// action speaks emotionally; no general or thrill one does.
    #[test]
    fn nude_actions_speak_emotionally() {
        for a in all_actions()
            .into_iter()
            .filter(|a| a.together_kind().is_some())
        {
            assert_eq!(
                a.speaks_emotionally(),
                a.together_group() == Some(TogetherGroup::Nude),
                "{a:?}"
            );
        }
    }

    /// Spec 2026-11-23-hot-spring-fight-challenge-bet-design.md §4: social
    /// actions that place nothing and cost nothing when turned down.
    #[test]
    fn a_challenge_and_a_bet_are_social_actions() {
        for (a, wire) in [
            (UserAction::Challenge, "challenge"),
            (UserAction::Bet, "bet"),
        ] {
            let v = serde_json::json!({ "type": wire });
            assert_eq!(serde_json::to_value(&a).unwrap(), v);
            assert_eq!(serde_json::from_value::<UserAction>(v).unwrap(), a);
            assert_eq!(a.together_kind(), None, "{a:?}");
            assert_eq!(a.refusal_lines(), [] as [AffinityLine; 0], "{a:?}");
            assert!(!a.speaks_emotionally(), "{a:?}");
        }
    }

    /// Spec 2026-11-23 §7: turning a proposal down costs nothing.
    #[test]
    fn a_refused_proposal_drops_no_line() {
        assert_eq!(UserAction::Propose.refusal_lines(), [] as [AffinityLine; 0]);
    }

    /// Spec 2026-11-23-twelve-together-actions-and-propose-design.md §7.
    #[test]
    fn propose_is_a_plain_action() {
        let v = serde_json::json!({ "type": "propose" });
        assert_eq!(serde_json::to_value(UserAction::Propose).unwrap(), v);
        assert_eq!(
            serde_json::from_value::<UserAction>(v).unwrap(),
            UserAction::Propose
        );
        assert_eq!(UserAction::Propose.together_kind(), None);
    }

    /// Spec 2026-11-23-affinity-50-design.md §8: a break-up is answered in
    /// the `emotional` tone, places no event and drops nothing when refused.
    #[test]
    fn break_up_is_a_plain_charged_action() {
        let v = serde_json::json!({ "type": "break_up" });
        assert_eq!(serde_json::to_value(UserAction::BreakUp).unwrap(), v);
        assert_eq!(
            serde_json::from_value::<UserAction>(v).unwrap(),
            UserAction::BreakUp
        );
        assert!(UserAction::BreakUp.speaks_emotionally());
        assert_eq!(UserAction::BreakUp.refusal_lines(), [] as [AffinityLine; 0]);
        assert_eq!(UserAction::BreakUp.together_kind(), None);
        assert_eq!(UserAction::BreakUp.consumed(), None);
    }

    /// Spec 2026-11-23-affinity-50-design.md §8: the two user actions whose
    /// answer can move the status.
    #[test]
    fn propose_and_break_up_are_the_status_asks() {
        assert_eq!(UserAction::Propose.status_ask(), Some(StatusAsk::Propose));
        assert_eq!(UserAction::BreakUp.status_ask(), Some(StatusAsk::BreakUp));
        for a in [
            UserAction::Kiss,
            UserAction::SexTogether,
            UserAction::FallAsleepTogether,
            UserAction::Challenge,
            UserAction::Custom { text: "x".into() },
            UserAction::Date {
                duration: DateDuration::Short,
                place: "海边".into(),
                content: "看海".into(),
            },
        ] {
            assert_eq!(a.status_ask(), None, "{a:?}");
        }
    }

    #[test]
    fn date_action_round_trips_and_drops_no_line() {
        let a: UserAction = serde_json::from_value(serde_json::json!({
            "type": "date", "duration": "mid", "place": "海边", "content": "看海"
        }))
        .unwrap();
        assert_eq!(
            a,
            UserAction::Date {
                duration: DateDuration::Mid,
                place: "海边".into(),
                content: "看海".into()
            }
        );
        assert_eq!(serde_json::to_value(&a).unwrap()["duration"], "mid");
        assert_eq!(a.refusal_lines(), [] as [AffinityLine; 0]);
        assert!(!a.speaks_emotionally());
        assert_eq!(DateDuration::Short.hours(), 1);
        assert_eq!(DateDuration::Mid.hours(), 4);
        for d in [DateDuration::Short, DateDuration::Mid] {
            assert_eq!(DateDuration::parse(d.as_str()), Some(d));
        }
        assert_eq!(DateDuration::parse("long"), None);
    }

    #[test]
    fn event_user_message_defaults_prompt_traits_to_empty_vec() {
        let raw = r#"{"UserMessage":{"content":"hi","message_id":"00000000-0000-0000-0000-000000000001"}}"#;
        let ev: Event = serde_json::from_str(raw).expect("legacy body deserialises");
        match ev {
            Event::UserMessage { prompt_traits, .. } => {
                assert!(prompt_traits.is_empty(), "missing field must default to []");
            }
            _ => panic!("expected UserMessage"),
        }
    }

    #[test]
    fn prompt_trait_round_trips_through_serde() {
        let t = PromptTrait {
            tag: "nsfw_boost".into(),
            text: "be more daring".into(),
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: PromptTrait = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn llm_audit_serde_roundtrip_full() {
        let mut metadata = serde_json::Map::new();
        metadata.insert("plan".into(), serde_json::Value::String("pro".into()));
        let a = LlmAudit {
            user: Some("u_abc".into()),
            session_id: Some("conv_xyz".into()),
            metadata: Some(metadata),
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: LlmAudit = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn llm_audit_serde_roundtrip_empty_yields_all_none() {
        let a: LlmAudit = serde_json::from_str("{}").unwrap();
        assert!(a.user.is_none());
        assert!(a.session_id.is_none());
        assert!(a.metadata.is_none());
    }

    #[test]
    fn event_user_message_defaults_audit_to_none() {
        let raw = r#"{"UserMessage":{"content":"hi","message_id":"00000000-0000-0000-0000-000000000001"}}"#;
        let ev: Event = serde_json::from_str(raw).expect("legacy body deserialises");
        match ev {
            Event::UserMessage { audit, .. } => {
                assert!(audit.is_none(), "missing audit field must default to None");
            }
            _ => panic!("expected UserMessage"),
        }
    }

    #[test]
    fn image_ref_serializes_snake_case() {
        use crate::types::ImageRef;
        assert_eq!(serde_json::to_value(ImageRef::Face).unwrap(), "face");
        assert_eq!(
            serde_json::to_value(ImageRef::Previous).unwrap(),
            "previous"
        );
    }

    #[test]
    fn event_user_message_defaults_tier_to_none() {
        let raw = r#"{"UserMessage":{"content":"hi","message_id":"00000000-0000-0000-0000-000000000001"}}"#;
        let ev: Event = serde_json::from_str(raw).expect("legacy body deserialises");
        match ev {
            Event::UserMessage { tier, .. } => {
                assert!(tier.is_none(), "missing tier must default to None")
            }
            _ => panic!("expected UserMessage"),
        }
    }

    #[test]
    fn user_message_defaults_scopes_when_absent() {
        let raw = r#"{"UserMessage":{"content":"hi","message_id":"00000000-0000-0000-0000-000000000001"}}"#;
        let ev: Event = serde_json::from_str(raw).unwrap();
        match ev {
            Event::UserMessage {
                memory_scope,
                affinity_scope,
                ..
            } => {
                assert_eq!(
                    memory_scope,
                    crate::scope::MemoryScope::NeutralAndRelationship
                );
                assert_eq!(affinity_scope, crate::scope::AffinityScope::bond());
            }
            _ => panic!("expected UserMessage"),
        }
    }

    #[test]
    fn chat_response_defaults_audit_fields_to_none() {
        let r = ChatResponse {
            reply: "hi".into(),
            ..Default::default()
        };
        assert!(r.usage.is_none());
        assert!(r.generation_id.is_none());
        assert!(r.model.is_none());
    }

    #[test]
    fn is_text_reply_truth_table() {
        assert!(ActionType::ReplyText.is_text_reply());
        assert!(ActionType::ReplyImage.is_text_reply());
        assert!(ActionType::ReplyTextImage.is_text_reply());
        assert!(!ActionType::Ghost.is_text_reply());
        assert!(!ActionType::Proactive.is_text_reply());
    }

    #[test]
    fn quote_defaults_to_none_and_event_omits_it() {
        // An event JSON without `quote` still parses, defaulting to None.
        let raw = r#"{"UserMessage":{"content":"hi","message_id":"00000000-0000-0000-0000-000000000001"}}"#;
        let ev: Event = serde_json::from_str(raw).unwrap();
        match ev {
            Event::UserMessage { quote, .. } => assert_eq!(quote, None),
            _ => panic!("expected UserMessage"),
        }

        // A resolved quote round-trips through serde — the queue path
        // serializes the whole event.
        let q = QuotedMessage {
            message_id: uuid::Uuid::nil(),
            role: "assistant".into(),
            content: "那我们礼拜六去看展".into(),
            sent_at: chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap(),
            trigger: None,
        };
        let s = serde_json::to_string(&q).unwrap();
        // `trigger: None` serializes without the key, so this also proves a
        // pre-`trigger` payload (the queue may hold old events) still parses.
        assert!(!s.contains("trigger"), "got {s}");
        assert_eq!(serde_json::from_str::<QuotedMessage>(&s).unwrap(), q);
    }

    #[test]
    fn user_action_serializes_flat_with_type_tag() {
        let give = UserAction::Give {
            item: GivenItem::Alcohol,
            name: Some("威士忌".into()),
            quantity: 2,
        };
        assert_eq!(
            serde_json::to_value(&give).unwrap(),
            serde_json::json!({"type": "give", "item": "alcohol", "name": "威士忌", "quantity": 2})
        );
        let nameless = UserAction::Give {
            item: GivenItem::Cigarette,
            name: None,
            quantity: 1,
        };
        assert_eq!(
            serde_json::to_value(&nameless).unwrap(),
            serde_json::json!({"type": "give", "item": "cigarette", "quantity": 1})
        );
        assert_eq!(
            serde_json::to_value(&UserAction::Kiss).unwrap(),
            serde_json::json!({"type": "kiss"})
        );
        assert_eq!(
            serde_json::to_value(&UserAction::Custom {
                text: "捏了捏你的脸".into()
            })
            .unwrap(),
            serde_json::json!({"type": "custom", "text": "捏了捏你的脸"})
        );
        let back: UserAction = serde_json::from_value(serde_json::json!({"type": "lick"})).unwrap();
        assert_eq!(back, UserAction::Lick);
        for (a, wire) in [
            (UserAction::FrenchKiss, "french_kiss"),
            (UserAction::HugFromBehind, "hug_from_behind"),
            (UserAction::Tease, "tease"),
            (UserAction::Pat, "pat"),
            (UserAction::Wink, "wink"),
            (UserAction::Blink, "blink"),
            (UserAction::AskForDance, "ask_for_dance"),
            (UserAction::AskDancing, "ask_dancing"),
            (UserAction::Photo, "photo"),
            (UserAction::Bluff, "bluff"),
            (UserAction::Threaten, "threaten"),
            (UserAction::Trick, "trick"),
            (UserAction::Strip, "strip"),
        ] {
            let v = serde_json::json!({ "type": wire });
            assert_eq!(serde_json::to_value(&a).unwrap(), v);
            assert_eq!(serde_json::from_value::<UserAction>(v).unwrap(), a);
        }
    }

    #[test]
    fn user_action_line_maps_give_to_bond_and_the_rest_to_chemistry() {
        use crate::affinity::AffinityLine;
        let give = UserAction::Give {
            item: GivenItem::Medicine,
            name: None,
            quantity: 1,
        };
        assert_eq!(give.refusal_lines(), [AffinityLine::Bond]);
        for a in [
            UserAction::Kiss,
            UserAction::Hug,
            UserAction::Touch,
            UserAction::Lick,
            UserAction::FrenchKiss,
            UserAction::HugFromBehind,
            UserAction::Tease,
            UserAction::Pat,
            UserAction::Wink,
            UserAction::Blink,
            UserAction::AskForDance,
            UserAction::AskDancing,
            UserAction::Photo,
            UserAction::Strip,
            UserAction::ShowerTogether,
            UserAction::BathTogether,
            UserAction::SaunaTogether,
            UserAction::Custom { text: "x".into() },
        ] {
            assert_eq!(a.refusal_lines(), [AffinityLine::Chemistry], "{a:?}");
        }
    }

    #[test]
    fn gift_action_serde_shape() {
        let gift = UserAction::Gift {
            kind: ItemKind::Clothing,
            name: "红色旗袍".into(),
        };
        assert_eq!(
            serde_json::to_value(&gift).unwrap(),
            serde_json::json!({"type": "gift", "kind": "clothing", "name": "红色旗袍"})
        );
        let back: UserAction = serde_json::from_value(
            serde_json::json!({"type": "gift", "kind": "item", "name": "摩托车"}),
        )
        .unwrap();
        assert_eq!(
            back,
            UserAction::Gift {
                kind: ItemKind::Item,
                name: "摩托车".into()
            }
        );
    }

    #[test]
    fn a_refused_gift_drops_bond_like_give() {
        use crate::affinity::AffinityLine;
        let gift = UserAction::Gift {
            kind: ItemKind::Item,
            name: "摩托车".into(),
        };
        assert_eq!(gift.refusal_lines(), [AffinityLine::Bond]);
    }

    #[test]
    fn item_kind_round_trips_its_wire_value() {
        for k in [ItemKind::Item, ItemKind::Clothing] {
            assert_eq!(ItemKind::parse(k.as_str()), Some(k));
            assert_eq!(
                serde_json::to_value(k).unwrap(),
                serde_json::json!(k.as_str())
            );
        }
        assert_eq!(ItemKind::parse("shoes"), None);
    }

    #[test]
    fn given_item_keeps_its_wire_values() {
        assert_eq!(
            serde_json::to_value(GivenItem::Cigarette).unwrap(),
            "cigarette"
        );
        assert_eq!(serde_json::to_value(GivenItem::Alcohol).unwrap(), "alcohol");
        assert_eq!(
            serde_json::to_value(GivenItem::Medicine).unwrap(),
            "medicine"
        );
    }

    #[test]
    fn new_given_items_wire_values() {
        for (item, wire) in [
            (GivenItem::Food, "food"),
            (GivenItem::SoftDrink, "soft_drink"),
            (GivenItem::Tea, "tea"),
            (GivenItem::Coffee, "coffee"),
            (GivenItem::Herb, "herb"),
            (GivenItem::Watch, "watch"),
            (GivenItem::Gem, "gem"),
            (GivenItem::Gold, "gold"),
            (GivenItem::Camera, "camera"),
            (GivenItem::Cash, "cash"),
            (GivenItem::Antique, "antique"),
            (GivenItem::Artwork, "artwork"),
            (GivenItem::Handmade, "handmade"),
            (GivenItem::CreditCard, "credit_card"),
            (GivenItem::Flower, "flower"),
            (GivenItem::Cuisine, "cuisine"),
            (GivenItem::Jewelry, "jewelry"),
            (GivenItem::Car, "car"),
            (GivenItem::Motorcycle, "motorcycle"),
            (GivenItem::Boat, "boat"),
            (GivenItem::Horse, "horse"),
        ] {
            assert_eq!(serde_json::to_value(item).unwrap(), wire);
            assert_eq!(
                serde_json::from_value::<GivenItem>(serde_json::json!(wire)).unwrap(),
                item
            );
        }
        let give = UserAction::Give {
            item: GivenItem::CreditCard,
            name: None,
            quantity: 1,
        };
        assert_eq!(
            serde_json::to_value(&give).unwrap(),
            serde_json::json!({"type": "give", "item": "credit_card", "quantity": 1})
        );
    }

    #[test]
    fn tip_action_serde_shape() {
        let tip = UserAction::Tip { amount_usd: 20.0 };
        assert_eq!(
            serde_json::to_value(&tip).unwrap(),
            serde_json::json!({"type": "tip", "amount_usd": 20.0})
        );
        let back: UserAction =
            serde_json::from_value(serde_json::json!({"type": "tip", "amount_usd": 9.99})).unwrap();
        assert_eq!(back, UserAction::Tip { amount_usd: 9.99 });
    }

    #[test]
    fn a_refused_social_action_drops_no_line() {
        for a in [UserAction::Bluff, UserAction::Threaten, UserAction::Trick] {
            assert_eq!(a.refusal_lines(), [] as [AffinityLine; 0], "{a:?}");
        }
    }

    #[test]
    fn a_refused_tip_drops_no_line() {
        assert_eq!(
            UserAction::Tip { amount_usd: 20.0 }.refusal_lines(),
            [] as [AffinityLine; 0]
        );
    }

    #[test]
    fn action_response_wire_strings() {
        assert_eq!(ActionResponse::Accept.as_str(), "accept");
        assert_eq!(ActionResponse::Refuse.as_str(), "refuse");
        assert_eq!(
            serde_json::to_value(ActionResponse::Refuse).unwrap(),
            "refuse"
        );
        assert_eq!(
            serde_json::from_str::<ActionResponse>("\"accept\"").unwrap(),
            ActionResponse::Accept
        );
    }

    #[test]
    fn event_user_message_defaults_action_to_none() {
        let raw = r#"{"UserMessage":{"content":"hi","message_id":"00000000-0000-0000-0000-000000000001"}}"#;
        let ev: Event = serde_json::from_str(raw).expect("legacy body deserialises");
        match ev {
            Event::UserMessage { action, .. } => assert!(action.is_none()),
            _ => panic!("expected UserMessage"),
        }
    }
}
