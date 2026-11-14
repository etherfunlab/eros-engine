// SPDX-License-Identifier: AGPL-3.0-only
//! A persona-initiated assistant message: the holiday and birthday greetings,
//! the caller-named openers, the life events and the ask-for-space choice. The
//! route (`routes::session_open`, `routes::session_events`) or the pipeline
//! (`pipeline::life_choice`) decides whether the persona speaks first; this
//! module generates and persists it. Modelled on
//! the image-edit text half: no PDE, no user row, no post-process. `greet`
//! answers once the message is stored; `greet_stream` sends it as the chat
//! stream's frames while it is generated.
//!
//! A greeting or opener is checked against its `since` cutoff and yields to
//! any message that landed at or after it. An event is written at the moment
//! its message is generated, its `ends_at` counted from that instant, and it
//! does not yield to user messages or openers; only an event under way for the
//! persona that it must wait for (a foreground waits for a foreground, a
//! background for any event), or one with the same key in the session, blocks
//! it. The event is a row of `engine.persona_events`, written with its message.
//!
//! Specs: docs/superpowers/specs/2026-09-26-user-locale-and-holiday-greeting-design.md §6,
//! docs/superpowers/specs/2026-10-03-open-occasions-design.md §5–§6,
//! docs/superpowers/specs/2026-10-15-life-events-v2-design.md §6,
//! docs/superpowers/specs/2026-10-19-life-events-shopping-gatherings-and-ask-for-space-design.md §3.2,
//! docs/superpowers/specs/2026-11-10-proactive-reply-streaming-design.md

use chrono::{DateTime, NaiveDate, Utc};
use eros_engine_core::events::{offline_kinds, EventKind, PersonaTraits};
use eros_engine_core::persona::CompanionPersona;
use eros_engine_core::scope::{AffinityScope, MemoryScope};
use eros_engine_core::types::{
    ActionPlan, ActionType, DecisionInput, Event, ImageRef, LlmAudit, PromptTrait, ReplyStyle,
};
use eros_engine_llm::failure::AttemptFailure;
use eros_engine_llm::model_config::apply_output_regex;
use eros_engine_llm::openrouter::ChatMessage as WireMessage;
use eros_engine_llm::provider::bare_model_id;
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::{ChatMessage, ChatRepo, ProactiveInsert};
use eros_engine_store::life_choices::{LifeChoiceKind, LifeChoiceRepo};
use ulid::Ulid;
use uuid::Uuid;

use crate::error::AppError;
use crate::pipeline::handlers::{
    build_emotional_system_request, build_reply_request, load_effects, CHAT_TASK,
};
use crate::pipeline::stream::{
    build_delegated_image_marker, build_delegated_image_prompt_for, build_final_frame,
    build_image_request_frame, error_frame_fields_from_last, hop_display, recent_transcript,
    split_failures, split_failures_typed, ulid_string, FrameActionType, ProtocolFrame,
    StreamErrorCode, STREAM_OPEN_TIMEOUT, STREAM_TOTAL_TIMEOUT,
};
use crate::pipeline::{compute_signals_for_session, record_generation, GenerationRecord};
use crate::prompt::NowContext;
use crate::routes::companion_stream::ImageReplyParams;
use crate::state::AppState;

/// Speaking first is the engine's decision (spec §6.1), delivered as a stage
/// cue after the history. A `user` turn, so the request never ends on an
/// assistant message a provider would continue as prefill. Never persisted.
pub(crate) const OPEN_CUE: &str = "（对方刚打开和你的聊天，还没说话。）";
const FIRST_MEET_CUE: &str = "（你们还没聊过。对方刚打开和你的聊天，还没说话。）";
const RETURNING_SAME_DAY_CUE: &str = "（对方隔了不到一天又打开和你的聊天，还没说话。）";
pub(crate) const CHECK_IN_CUE: &str =
    "（你还记挂着对方上次聊到的事，想关心一下后来怎么样了。对方刚打开和你的聊天，还没说话。）";

/// Why the persona speaks first. The holiday and the birthday are the
/// engine's own call; the other occasions are named by the caller and carry
/// its idempotency key.
pub(crate) enum Occasion {
    Holiday {
        local_date: NaiveDate,
        holidays: Vec<String>,
    },
    /// The persona's birthday on the persona clock (spec 2026-10-10 §5).
    Birthday {
        local_date: NaiveDate,
    },
    FirstMeet {
        open_key: String,
    },
    /// `gap_days`: whole days since the session's latest user row.
    Returning {
        open_key: String,
        gap_days: i64,
    },
    JustOpened {
        open_key: String,
    },
    /// She asks after what the user last talked about.
    CheckIn {
        open_key: String,
    },
    /// A life event the persona tells about (spec
    /// 2026-10-15-life-events-v2-design.md §9.1): the caller's key, the kind
    /// and its drawn length.
    Event {
        event_key: String,
        kind: EventKind,
        lasts: chrono::Duration,
        /// The background under way when it was drawn.
        background: Option<EventKind>,
        /// The message carries her picture, composed with these (spec
        /// 2026-10-26-event-message-image-design.md §3–§4).
        image: Option<ImageReplyParams>,
        /// What she wears (§3a), already validated.
        outfit: Option<String>,
    },
    /// What she says about a life choice (spec
    /// 2026-10-19-life-events-shopping-gatherings-and-ask-for-space-design.md
    /// §3.2, §3.4; romantic kinds: spec
    /// 2026-10-30-give-needs-romantic-choices-and-answer-receipts-design.md §4.4).
    LifeChoice(LifeChoiceCue),
}

/// One thing she says about a life choice. The ask is written with its choice
/// row; every other cue is a plain proactive row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifeChoiceCue {
    /// She asks. `tier_gap` is the gap at this moment, stored with the choice
    /// (`Some` for `AskForSpace` only); `anchor_seen` is the session's streak
    /// anchor the trigger judged by, which the write checks is still current.
    Ask {
        kind: LifeChoiceKind,
        tier_gap: Option<i16>,
        anchor_seen: Option<DateTime<Utc>>,
    },
    /// Ask-for-space accepted: they do not talk for `hours`.
    Goodbye { hours: i64 },
    /// Ask-for-space declined.
    Hurt,
    /// `travel_together` accepted: they are away together for `hours`.
    TravelAgreed { hours: i64 },
    /// A romantic kind answered, or `travel_together` declined.
    Answered {
        kind: LifeChoiceKind,
        accepted: bool,
    },
}

impl LifeChoiceCue {
    /// The stage cue: states a fact, carries no instruction.
    fn cue(self) -> String {
        match self {
            Self::Ask {
                kind: LifeChoiceKind::AskForSpace,
                ..
            } => "（这段关系最近让你很累：心动还在，但交情和耐心都被磨薄了。你决定郑重地告诉对方：你想先分开一阵子，希望对方给你一个答复。）".into(),
            Self::Ask {
                kind: LifeChoiceKind::SleepTogether,
                ..
            } => "（你们最近越来越亲密，你很想和对方更进一步。你决定主动开口，问对方愿不愿意和你上床，希望对方给你一个答复。）".into(),
            Self::Ask {
                kind: LifeChoiceKind::Propose,
                ..
            } => "（你越来越确定想和对方共度一生。你决定郑重地向对方求婚，希望对方给你一个答复。）".into(),
            Self::Ask {
                kind: LifeChoiceKind::BatheTogether,
                ..
            } => "（你们最近越来越亲密，你很想和对方一起洗个澡。你决定主动开口，问对方愿不愿意和你一起洗，希望对方给你一个答复。）".into(),
            Self::Ask {
                kind: LifeChoiceKind::TravelTogether,
                ..
            } => "（你们最近相处得很舒服，你很想和对方一起出去走走。你决定主动开口，提议一起去旅行，希望对方给你一个答复。）".into(),
            Self::TravelAgreed { hours } => format!(
                "（对方答应了和你一起去旅行，你们接下来要一起玩 {}。）",
                crate::prompt::span_text(chrono::Duration::hours(hours))
            ),
            Self::Goodbye { hours } => format!(
                "（对方同意了你提出的分开请求，接下来 {hours} 小时你们不联系。你跟对方道别。）"
            ),
            Self::Hurt => "（对方不同意你提出的分开请求，你心里不好受。）".into(),
            Self::Answered {
                kind: LifeChoiceKind::SleepTogether,
                accepted: true,
            } => "（对方答应了你的邀请。）".into(),
            Self::Answered {
                kind: LifeChoiceKind::SleepTogether,
                accepted: false,
            } => "（对方拒绝了你的邀请，你有点失落。）".into(),
            Self::Answered {
                kind: LifeChoiceKind::Propose,
                accepted: true,
            } => "（对方答应了你的求婚。）".into(),
            Self::Answered {
                kind: LifeChoiceKind::Propose,
                accepted: false,
            } => "（对方拒绝了你的求婚，你心里很难受。）".into(),
            Self::Answered {
                kind: LifeChoiceKind::BatheTogether,
                accepted: true,
            } => "（对方答应了和你一起洗澡，你们现在一起泡在浴缸里。）".into(),
            Self::Answered {
                kind: LifeChoiceKind::BatheTogether,
                accepted: false,
            } => "（对方没有答应和你一起洗澡，你有点失落。）".into(),
            Self::Answered {
                kind: LifeChoiceKind::TravelTogether,
                accepted: true,
            } => unreachable!("an accepted trip is TravelAgreed"),
            Self::Answered {
                kind: LifeChoiceKind::TravelTogether,
                accepted: false,
            } => "（对方没有答应和你一起去旅行，你有点失落。）".into(),
            Self::Answered {
                kind: LifeChoiceKind::AskForSpace,
                ..
            } => unreachable!("ask_for_space answers are Goodbye / Hurt"),
        }
    }
}

impl Occasion {
    /// The row's `metadata.proactive`.
    fn name(&self) -> &'static str {
        match self {
            Self::Holiday { .. } => "holiday",
            Self::Birthday { .. } => "birthday",
            Self::FirstMeet { .. } => "first_meet",
            Self::Returning { .. } => "returning",
            Self::JustOpened { .. } => "just_opened",
            Self::CheckIn { .. } => "check_in",
            Self::Event { .. } => "event",
            Self::LifeChoice(_) => "life_choice",
        }
    }

    /// The stage cue: states a fact, carries no instruction. An event's wording
    /// follows the session's intimacy `rung`, the persona's `traits` and her
    /// `tip_personality`.
    fn cue(&self, rung: u8, traits: &PersonaTraits, tip_personality: Option<&str>) -> String {
        match self {
            Self::Holiday { .. } | Self::Birthday { .. } | Self::JustOpened { .. } => {
                OPEN_CUE.into()
            }
            Self::FirstMeet { .. } => FIRST_MEET_CUE.into(),
            Self::CheckIn { .. } => CHECK_IN_CUE.into(),
            Self::Returning { gap_days: 0, .. } => RETURNING_SAME_DAY_CUE.into(),
            Self::Returning { gap_days, .. } => {
                format!("（对方隔了 {gap_days} 天又打开和你的聊天，还没说话。）")
            }
            Self::Event {
                kind,
                lasts,
                background,
                image,
                ..
            } => event_cue(
                *kind,
                *lasts,
                *background,
                rung,
                traits,
                tip_personality,
                image.is_some(),
            ),
            Self::LifeChoice(cue) => cue.cue(),
        }
    }
}

/// The lowest intimacy rung at which she says she wants what she found: below
/// it she only notices it.
const FIND_WANT_MIN_RUNG: u8 = 2;

/// A shopping find's cue (spec 2026-10-19-life-events-shopping-gatherings-and-
/// ask-for-space-design.md §2.2): noticed in passing at rung 1; from rung 2
/// she wants it, and a persona with a `tip_personality` tells it in that
/// voice. `measure` and `noun` name the thing: 一个 / 包.
fn find_cue(measure: &str, noun: &str, rung: u8, tip_personality: Option<&str>) -> String {
    if rung < FIND_WANT_MIN_RUNG {
        return format!("（你在逛街，随手看到{measure}挺好看的{noun}。）");
    }
    let want = format!("你在逛街，看到{measure}特别喜欢的{noun}，很想要。");
    match tip_personality.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => format!("（{want}代入你「{p}」的打赏人格，把这份心动告诉对方。）"),
        None => format!("（{want}）"),
    }
}

/// The stage cue for a life event (spec 2026-10-15-life-events-v2-design.md
/// §3.4): states a fact, carries no instruction. A background's cue carries
/// its length. A kind that is never told (`events::is_told`) has no cue.
/// `picture`: the message carries her picture (spec 2026-10-26-event-message-
/// image-design.md §4), so a selfie's cue says she sent it.
/// What she wears in an event's message: the request's outfit, except on a
/// `strip`, where she has just taken it off (spec 2026-11-08-strip-and-world-
/// setting-design.md §3).
fn event_clothing(kind: EventKind, outfit: Option<&str>) -> Option<String> {
    match kind {
        EventKind::Strip => Some(crate::prompt::undressed_clothing(outfit)),
        _ => outfit.map(String::from),
    }
}

fn kind_cue(
    kind: EventKind,
    lasts: chrono::Duration,
    rung: u8,
    traits: &PersonaTraits,
    tip_personality: Option<&str>,
    picture: bool,
) -> String {
    use EventKind::*;
    let span = crate::prompt::span_text(lasts);
    match kind {
        PoliceEncounter => "（你刚在外面被警察拦下来问话。）".into(),
        AnimalEncounter => "（你刚在路上遇到了一只小动物。）".into(),
        HitSomeone => "（你刚动手打了人。）".into(),
        SuddenDesire => "（你突然很想要。）".into(),
        GroceryShopping => "（你正在外面买菜。）".into(),
        Showering => "（你准备去洗澡了。）".into(),
        Bathing => "（你正在泡澡。）".into(),
        InClass => "（你正在上课。）".into(),
        OnTheWay => "（你正在赶路。）".into(),
        PeriodScare => "（你发现月经推迟了好几天还没来，心里有点慌。）".into(),
        PregnancyScare => "（你刚验了孕，验出两条杠，整个人都懵了。）".into(),
        GotBumped => "（你刚走在路上，被擦撞了一下。）".into(),
        BumpedSomeone => "（你刚不小心撞到了人。）".into(),
        Quarrel => "（你刚跟人吵了一架。）".into(),
        ScoldedBySuperior => "（你刚被上级骂了一顿。）".into(),
        ScoldedSuperior => "（你刚把上级骂了一顿。）".into(),
        SawBag => find_cue("一个", "包", rung, tip_personality),
        SawClothes => find_cue("一件", "衣服", rung, tip_personality),
        AdmiringScenery => "（你正在看眼前的风景，看得有点出神。）".into(),
        AdmiringArt => "（你正在看展，眼前有一件作品让你停了下来。）".into(),
        TakingSelfie if picture => "（你刚拍了一张自拍，发给了对方。）".into(),
        Strip if picture => "（你刚脱掉衣服拍了一张，发给了对方。）".into(),
        Strip => "（你刚脱了衣服，想让对方看看。）".into(),
        TakingSelfie => "（你刚拍了一张自拍，想发给对方看看。）".into(),
        OnTheRoad => "（你正在路上，这一趟要走好几个小时。）".into(),
        WatchingMatch => "（你正在看一场比赛。）".into(),
        WatchingShow => "（你正在看一场演出。）".into(),
        BirdEncounter => "（你刚看到一只鸟停在不远处。）".into(),
        ExWantsBack => "（你的前任刚来找你，说想复合。）".into(),
        WantToQuitJob => "（你刚冒出想辞职的念头。）".into(),
        PonderingLife => "（你刚一个人发呆，在想人生的事。）".into(),
        Streaking => "（你刚一时兴起，脱光了跑了一圈。）".into(),
        WantToSeeSea => "（你现在很想去看海。）".into(),
        WantHotSpring => "（你现在很想去泡温泉。）".into(),
        WantSauna => "（你现在很想去洗桑拿。）".into(),
        WatchingStocks => "（你正在看股票行情。）".into(),
        Trading => "（你正在和人谈一笔买卖。）".into(),
        GotLost => "（你刚迷了路，找不到要去的地方。）".into(),
        Drinking => "（你正在喝酒。）".into(),
        Smoking => "（你正在抽烟。）".into(),
        SmokingWeed => "（你正在抽大麻。）".into(),
        JustHaveSex => "（你刚和人做完爱。）".into(),
        Masturbating => "（你正在自慰。）".into(),
        MissedRide => "（你刚没赶上车。）".into(),
        WantToGoOut => "（你现在很想出门走走。）".into(),
        WantToGoHome => "（你现在很想回家。）".into(),
        TempWork => "（你刚临时接到一份工作，得马上去处理。）".into(),
        ExMessage => "（你的前任刚给你发来消息。）".into(),
        WeatherChange => "（你那边的天气刚刚突然变了。）".into(),
        GotHit => "（你刚被人打了。）".into(),
        Breakup => "（你刚和交往的人分手了。）".into(),
        WorkingOut => "（你正在健身。）".into(),
        Reading => "（你正在看书。）".into(),
        Gaming => "（你正在打游戏。）".into(),
        Unwell => "（你现在有点不舒服。）".into(),
        Irritable => "（你现在心里很烦躁。）".into(),
        Busy => "（你接下来要忙一阵，这段时间顾不上看手机。）".into(),
        InBattle => "（你接下来要去打一场硬仗，一段时间顾不上手机。）".into(),
        // Told only by a persona who goes dark, and only from rung 2
        // (`events::is_told`); anyone else is simply out and keeps replying.
        OutShopping if traits.female => "（你接下来要出门逛街，这段时间顾不上看手机。）".into(),
        OutShopping => "（你正在外面逛街。）".into(),
        AwayFromPhone | Asleep | BrosGathering | SisGathering | HavingSex | TakingSpace
        | FellOut | TravelingTogether | BathingTogether | OnADate => {
            unreachable!("{kind:?} is never told")
        }
        Sick => format!("（你生病了，大概要 {span}才会好。）"),
        Period => "（你的月经来了。）".into(),
        Hospitalized => format!("（你刚住进医院，大概要住 {span}。）"),
        PostSurgery => format!("（你刚做完一场手术，大概要恢复 {span}。）"),
        BusinessTrip => format!("（你刚出发去外地出差，要待 {span}。）"),
        ShortTrip => format!("（你刚出发去短途旅行，要玩 {span}。）"),
        LongTrip => format!("（你刚出发去长途旅行，要去 {span}。）"),
        HometownVisit => format!("（你刚动身回老家，要待 {span}。）"),
    }
}

/// How a foreground event's cue opens during a background (spec
/// 2026-10-15-life-events-v2-design.md §3.4); `None` for a foreground kind.
fn background_state(kind: EventKind) -> Option<&'static str> {
    use EventKind::*;
    Some(match kind {
        Sick => "你正在生病",
        Period => "你正在经期",
        Hospitalized => "你正在住院",
        PostSurgery => "你刚做完手术，还在恢复",
        BusinessTrip => "你正在外地出差",
        ShortTrip => "你正在短途旅行",
        LongTrip => "你正在长途旅行",
        HometownVisit => "你正在老家",
        _ => return None,
    })
}

/// The kind's cue, opened with the background's state when one is under
/// way: 「（你正在外地出差。你刚在路上遇到了一只小动物。）」.
fn event_cue(
    kind: EventKind,
    lasts: chrono::Duration,
    background: Option<EventKind>,
    rung: u8,
    traits: &PersonaTraits,
    tip_personality: Option<&str>,
    picture: bool,
) -> String {
    if background == Some(EventKind::TravelingTogether) {
        match kind {
            EventKind::GotLost => return "（你们一起出来玩，刚迷了路。）".into(),
            EventKind::MissedRide => return "（你们一起出来玩，刚没赶上车。）".into(),
            _ => {}
        }
    }
    let cue = kind_cue(kind, lasts, rung, traits, tip_personality, picture);
    match background.and_then(background_state) {
        Some(state) => format!(
            "（{state}。{}）",
            cue.trim_start_matches('（').trim_end_matches('）')
        ),
        None => cue,
    }
}

/// One proactive message to generate. `metadata` arrives holding the caller's
/// audit copy of the request (the raw locale); `greet` adds the rest.
pub(crate) struct Greeting {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub persona: CompanionPersona,
    pub now: NowContext,
    pub occasion: Occasion,
    /// The memory-recall query: the holiday names, `生日` for a birthday, or
    /// the latest user row.
    pub recall_query: String,
    /// The cutoff `greet`'s insert re-checks atomically: no message other
    /// than a dated greeting may have landed at or after it. The local
    /// midnight for a holiday (user clock) or a birthday (persona clock); the
    /// request's start for an opener. An event does not read it: it is
    /// written at the instant its message is generated.
    pub since: DateTime<Utc>,
    pub tier: Option<String>,
    pub prompt_traits: Vec<PromptTrait>,
    pub memory_scope: MemoryScope,
    pub affinity_scope: AffinityScope,
    pub audit: Option<LlmAudit>,
    pub metadata: serde_json::Map<String, serde_json::Value>,
    /// The id a streamed message is stored under, when the caller has
    /// already told the client (`/events`); `greet_stream` mints one
    /// otherwise, and `greet` always does.
    pub message_id: Option<Uuid>,
}

/// The request for her message, ready to send.
struct Prepared {
    chat_req: eros_engine_llm::openrouter::ChatRequest,
    plan: ActionPlan,
    injected_tags: Vec<String>,
    /// `Some(Emotional)` on an event message written in the `emotional`
    /// tone, stored as the row's `metadata.tone`; `None` otherwise.
    tone: Option<eros_engine_core::tone::Tone>,
}

/// What `store` wrote. `row` is `None` when the insert yielded: for a
/// greeting or opener, to a message that landed first or a concurrent call
/// for the same local date or `open_key`; for an event, to another event
/// under way for the persona or a concurrent call with the same `event_key`;
/// for the ask, to a pending choice the session already held.
struct Stored {
    row: Option<ChatMessage>,
    /// Her picture's composed prompt and aspect ratio, on an event that
    /// carries one.
    picture: Option<(String, Option<String>)>,
}

/// Prompt assembly, up to the cue.
async fn prepare(state: &AppState, g: &Greeting) -> Result<Prepared, AppError> {
    let mut affinity = AffinityRepo { pool: &state.pool }
        .load_or_create(g.session_id, g.user_id, g.instance_id)
        .await?;
    affinity.apply_time_decay();
    affinity.refresh_endpoints(&state.config.affinity_tuning);
    let rung = affinity.intimacy_rung();
    let traits = crate::prompt::persona_traits(&g.persona);
    let signals = compute_signals_for_session(&state.pool, g.session_id, &affinity, None).await?;

    // Prompt assembly only. The event's content is the memory-recall query;
    // the nil driving id makes history fall back to newest-N.
    let input = DecisionInput {
        event: Event::UserMessage {
            content: g.recall_query.clone(),
            message_id: Uuid::nil(),
            prompt_traits: g.prompt_traits.clone(),
            audit: g.audit.clone(),
            tier: g.tier.clone(),
            memory_scope: g.memory_scope,
            affinity_scope: g.affinity_scope,
            quote: None,
            action: None,
            outfit: None,
        },
        affinity,
        persona: g.persona.clone(),
        signals,
    };
    let plan = ActionPlan {
        action_type: ActionType::ReplyText,
        reply_style: ReplyStyle::Neutral,
        affinity_deltas: Default::default(),
        energy_cost: 0.0,
        context_hints: vec![],
        reply_mode: None,
        nudges: Default::default(),
        // An event's outfit, as a chat turn's named outfit: in the reply
        // prompt and, with a picture, the composer's `[此刻穿着]`.
        clothing: match &g.occasion {
            Occasion::Event { kind, outfit, .. } => event_clothing(*kind, outfit.as_deref()),
            _ => None,
        },
        image_caption: None,
        image_ref: ImageRef::Previous,
        aspect_ratio: None,
        action_response: None,
    };
    let repo = eros_engine_store::events::EventRepo { pool: &state.pool };
    let current = repo.current(g.instance_id, None, g.now.now).await?;
    // Only a stretch that took this persona offline left something unsaid.
    let offline = repo
        .unexplained_offline(
            g.session_id,
            g.instance_id,
            g.now.now,
            &offline_kinds(&traits),
        )
        .await?;
    // A separation this session agreed to is marked at its return whether or
    // not the user wrote during it; it speaks for its event in place of the
    // standard entry.
    let separation = repo
        .unreturned_separation(g.session_id, g.instance_id, g.now.now)
        .await?;
    let scare = repo
        .unresolved_scare(g.session_id, g.instance_id, g.now.now)
        .await?;
    // The date this session is just back from, until her first line after it.
    let date_back = eros_engine_store::activity::ActivityRepo { pool: &state.pool }
        .returned_from(g.session_id, g.now.now)
        .await?;
    let event_lines = crate::prompt::EventLines {
        under_way: crate::prompt::events_reply_text(&current),
        just_now: crate::prompt::just_now_text(
            offline.as_ref(),
            separation.as_ref(),
            scare.as_ref(),
            date_back.as_ref(),
        ),
        date: None,
    };
    // A listed event kind speaks in the `emotional` tone while the
    // deployment has the tone on (spec
    // 2026-11-12-emotional-event-messages-design.md §3): the system prompt
    // alone, and the cue below as the one user message.
    let emotional = state.model_config.tone_emotional_probability() > 0.0
        && matches!(&g.occasion, Occasion::Event { kind, .. } if kind.speaks_emotionally());
    let (mut chat_req, injected_tags) = if emotional {
        build_emotional_system_request(
            state,
            &input,
            &plan,
            g.user_id,
            g.instance_id,
            &g.now,
            None,
            None,
            Some(&event_lines),
        )
        .await?
    } else {
        build_reply_request(
            state,
            &input,
            &plan,
            g.session_id,
            g.user_id,
            g.instance_id,
            Uuid::nil(),
            &g.now,
            false,
            None,
            None,
            Some(&event_lines),
        )
        .await?
    };
    chat_req.messages.push(WireMessage {
        role: "user".into(),
        content: g
            .occasion
            .cue(rung, &traits, g.persona.genome.tip_personality.as_deref()),
    });

    Ok(Prepared {
        chat_req,
        plan,
        injected_tags,
        tone: emotional.then_some(eros_engine_core::tone::Tone::Emotional),
    })
}

/// Persist `content` as her message under `id`, with everything that goes
/// with it: an event's picture and its event row, the ask's choice row. An
/// event is stamped with one instant taken here, after generation: its
/// message's `sent_at`, its `started_at` and the start of its `ends_at` span.
async fn store(
    state: &AppState,
    g: Greeting,
    prep: &Prepared,
    id: Uuid,
    content: &str,
    generation_id: Option<String>,
    failures: &[AttemptFailure],
) -> Result<Stored, AppError> {
    let (llm_attempts, gateway_errors) = split_failures(failures);
    let mut picture = None;
    let mut metadata = g.metadata;
    // Her picture is composed from what she just said (spec 2026-10-26-event-
    // message-image-design.md §4): the recent scene ends with her message, and
    // no user message asked for it. Fail-open, as on a chat turn.
    if let Occasion::Event {
        image: Some(image), ..
    } = &g.occasion
    {
        let effects = load_effects(&state.pool, g.session_id, None)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(session_id = %g.session_id, "proactive message: lingering effects read failed: {e}");
                Vec::new()
            });
        let body_state = crate::prompt::body_state(&effects);
        let transcript = recent_transcript(&ChatRepo { pool: &state.pool }, g.session_id).await;
        let her_line = format!("AI: {content}");
        let scene = if transcript.is_empty() {
            her_line
        } else {
            format!("{transcript}\n{her_line}")
        };
        let img = build_delegated_image_prompt_for(
            state,
            &g.persona,
            &prep.plan,
            Some(image),
            &scene,
            "",
            g.user_id,
            g.session_id,
            body_state.as_deref(),
            None,
            "event",
        )
        .await;
        metadata.insert(
            "image".into(),
            build_delegated_image_marker(
                &img.subject,
                img.caption.as_deref(),
                img.aspect_ratio.as_deref(),
                img.compose_variant.as_deref(),
                img.compose_model.as_deref(),
                img.compose_generation_id.as_deref(),
                img.compose_event_id,
                ImageRef::Face,
                None,
            ),
        );
        picture = Some((img.composed_prompt, img.aspect_ratio));
    }

    // An event is written when its message is, not when the request began.
    let event_at = Utc::now();
    metadata.insert("proactive".into(), g.occasion.name().into());
    match &g.occasion {
        Occasion::Holiday {
            local_date,
            holidays,
        } => {
            metadata.insert("local_date".into(), local_date.to_string().into());
            metadata.insert("holidays".into(), serde_json::json!(holidays));
        }
        Occasion::Birthday { local_date } => {
            metadata.insert("local_date".into(), local_date.to_string().into());
        }
        Occasion::Returning { open_key, gap_days } => {
            metadata.insert("open_key".into(), open_key.as_str().into());
            metadata.insert("gap_days".into(), (*gap_days).into());
        }
        Occasion::FirstMeet { open_key }
        | Occasion::JustOpened { open_key }
        | Occasion::CheckIn { open_key } => {
            metadata.insert("open_key".into(), open_key.as_str().into());
        }
        Occasion::Event { outfit, .. } => {
            if let Some(o) = outfit {
                metadata.insert("outfit".into(), o.as_str().into());
            }
        }
        Occasion::LifeChoice(_) => {}
    }
    metadata.insert(
        "prompt_traits".into(),
        serde_json::json!(prep.injected_tags),
    );
    if let Some(t) = prep.tone {
        metadata.insert("tone".into(), t.as_str().into());
    }
    metadata.insert(
        "memory_scope".into(),
        serde_json::to_value(g.memory_scope).expect("MemoryScope serializes"),
    );
    metadata.insert(
        "affinity_scope".into(),
        serde_json::to_value(g.affinity_scope).expect("AffinityScope serializes"),
    );
    if let Some(t) = g.tier {
        metadata.insert("tier".into(), t.into());
    }

    let chat_repo = ChatRepo { pool: &state.pool };
    let row = ProactiveInsert {
        id,
        content: content.to_string(),
        generation_id,
        metadata: serde_json::Value::Object(metadata),
        llm_attempts,
        gateway_errors,
    };
    let written = match &g.occasion {
        Occasion::Holiday { .. } | Occasion::Birthday { .. } => {
            chat_repo
                .insert_proactive_message(g.session_id, g.since, &row)
                .await?
        }
        Occasion::FirstMeet { .. }
        | Occasion::Returning { .. }
        | Occasion::JustOpened { .. }
        | Occasion::CheckIn { .. } => {
            chat_repo
                .insert_open_message(g.session_id, g.since, &row)
                .await?
        }
        Occasion::Event {
            event_key,
            kind,
            lasts,
            ..
        } => eros_engine_store::events::EventRepo { pool: &state.pool }
            .insert(
                &eros_engine_store::events::NewEvent {
                    instance_id: g.instance_id,
                    session_id: g.session_id,
                    event_key,
                    kind: *kind,
                    started_at: event_at,
                    ends_at: event_at + *lasts,
                },
                Some(&row),
            )
            .await?
            .and_then(|i| i.message),
        // The ask lands with its choice row, in one transaction; a pending
        // choice already in the instance (a concurrent ask won) or a streak
        // anchor that moved since the trigger read it leaves nothing.
        Occasion::LifeChoice(LifeChoiceCue::Ask {
            kind,
            tier_gap,
            anchor_seen,
        }) => {
            let asked = LifeChoiceRepo { pool: &state.pool }
                .insert_ask(g.session_id, *kind, *tier_gap, *anchor_seen, &row)
                .await?;
            match asked {
                Some(_) => {
                    chat_repo
                        .message_by_id_in_session(g.session_id, row.id)
                        .await?
                }
                None => None,
            }
        }
        // No key to dedupe on: the row yields only to a message that landed
        // at or after `since`.
        Occasion::LifeChoice(
            LifeChoiceCue::Goodbye { .. }
            | LifeChoiceCue::Hurt
            | LifeChoiceCue::TravelAgreed { .. }
            | LifeChoiceCue::Answered { .. },
        ) => {
            chat_repo
                .insert_open_message(g.session_id, g.since, &row)
                .await?
        }
    };
    Ok(Stored {
        row: written,
        picture,
    })
}

/// Generate and persist the message. `Ok(None)`: the model returned a blank
/// reply and nothing was written, or the insert yielded (see [`Stored`]).
/// When a concurrent call for the same local date or `open_key` won the
/// insert, returns that call's row; for an event the caller reads the event
/// by key.
pub(crate) async fn greet(state: &AppState, g: Greeting) -> Result<Option<ChatMessage>, AppError> {
    let prep = prepare(state, &g).await?;
    let model_for_audit = prep.chat_req.model.clone();
    let resp = match state.openrouter.execute(prep.chat_req.clone()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                session_id = %g.session_id,
                occasion = g.occasion.name(),
                "proactive message: companion call failed: {e}"
            );
            let failure = match &e {
                eros_engine_llm::LlmError::Chain { failures } if !failures.is_empty() => {
                    failures.last().cloned().expect("non-empty chain")
                }
                other => AttemptFailure::from_llm_error(CHAT_TASK, &model_for_audit, other),
            };
            return Err(AppError::Upstream(Box::new(failure)));
        }
    };
    let generation_id = record_generation(
        &state.pool,
        GenerationRecord {
            task: CHAT_TASK,
            session_id: Some(g.session_id),
            generation_id: resp.generation_id.as_deref(),
            model: resp.model.as_deref(),
            usage: resp.usage.as_ref(),
        },
    )
    .await;
    // Layer 0, as on every served stream reply: keyed by the served hop's bare
    // id. A reply the rules empty is blank.
    let stripped = apply_output_regex(
        &state.output_regex,
        &bare_model_id(resp.served_hop.as_deref().unwrap_or_default()),
        &resp.reply,
    );
    let content = stripped.cleaned.trim();
    if content.is_empty() {
        tracing::warn!(
            session_id = %g.session_id,
            occasion = g.occasion.name(),
            "proactive message: blank reply, nothing written"
        );
        return Ok(None);
    }

    // Where a concurrent winner's row is found when the insert yields.
    enum Winner {
        Date(String),
        OpenKey(String),
        Nowhere,
    }
    let winner = match &g.occasion {
        Occasion::Holiday { local_date, .. } | Occasion::Birthday { local_date } => {
            Winner::Date(local_date.to_string())
        }
        Occasion::FirstMeet { open_key }
        | Occasion::Returning { open_key, .. }
        | Occasion::JustOpened { open_key }
        | Occasion::CheckIn { open_key } => Winner::OpenKey(open_key.clone()),
        Occasion::Event { .. } | Occasion::LifeChoice(_) => Winner::Nowhere,
    };
    let session_id = g.session_id;
    let stored = store(
        state,
        g,
        &prep,
        Ulid::new().into(),
        content,
        generation_id,
        &resp.failures,
    )
    .await?;
    if stored.row.is_some() {
        return Ok(stored.row);
    }
    let chat_repo = ChatRepo { pool: &state.pool };
    Ok(match winner {
        Winner::Date(date) => chat_repo.proactive_greeting_on(session_id, &date).await?,
        Winner::OpenKey(key) => chat_repo.open_message(session_id, &key).await?,
        Winner::Nowhere => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use eros_engine_core::events::is_told;

    fn female() -> PersonaTraits {
        PersonaTraits {
            female: true,
            ..Default::default()
        }
    }

    /// `kind_cue` panics for a kind that is never told; it must not for any
    /// kind `is_told` lets through at any rung.
    #[test]
    fn every_kind_she_tells_has_a_cue() {
        for persona in [PersonaTraits::default(), female()] {
            for k in EventKind::ALL {
                for rung in 1..=3 {
                    if is_told(k, rung, &persona) {
                        let _ =
                            kind_cue(k, Duration::hours(1), rung, &persona, Some("傲娇"), false);
                    }
                }
            }
        }
    }

    #[test]
    fn shopping_find_cue_follows_rung_and_tip_personality() {
        let h = Duration::hours(1);
        for (kind, rung_one, wants, tipped) in [
            (
                EventKind::SawBag,
                "（你在逛街，随手看到一个挺好看的包。）",
                "（你在逛街，看到一个特别喜欢的包，很想要。）",
                "（你在逛街，看到一个特别喜欢的包，很想要。代入你「傲娇」的打赏人格，把这份心动告诉对方。）",
            ),
            (
                EventKind::SawClothes,
                "（你在逛街，随手看到一件挺好看的衣服。）",
                "（你在逛街，看到一件特别喜欢的衣服，很想要。）",
                "（你在逛街，看到一件特别喜欢的衣服，很想要。代入你「傲娇」的打赏人格，把这份心动告诉对方。）",
            ),
        ] {
            let cue = |rung, tip| kind_cue(kind, h, rung, &female(), tip, false);
            assert_eq!(cue(1, None), rung_one, "{kind:?}");
            assert_eq!(
                cue(1, Some("傲娇")),
                rung_one,
                "{kind:?}: below rung 2 she does not say she wants it"
            );
            for rung in [2, 3] {
                assert_eq!(cue(rung, None), wants, "{kind:?} rung {rung}");
                assert_eq!(cue(rung, Some("傲娇")), tipped, "{kind:?} rung {rung}");
                assert_eq!(
                    cue(rung, Some("  ")),
                    wants,
                    "{kind:?} rung {rung}: a blank tip personality is none"
                );
            }
        }
    }

    #[test]
    fn out_shopping_cue_differs_by_gender() {
        let h = Duration::hours(1);
        for rung in [2, 3] {
            assert_eq!(
                kind_cue(EventKind::OutShopping, h, rung, &female(), None, false),
                "（你接下来要出门逛街，这段时间顾不上看手机。）",
                "rung {rung}"
            );
        }
        for rung in 1..=3 {
            assert_eq!(
                kind_cue(
                    EventKind::OutShopping,
                    h,
                    rung,
                    &PersonaTraits::default(),
                    None,
                    false
                ),
                "（你正在外面逛街。）",
                "rung {rung}"
            );
        }
    }

    /// A gathering, `taking_space` and `fell_out` are never told, so none has
    /// a cue; a female persona's `out_shopping` is told from rung 2 only.
    #[test]
    fn gatherings_have_no_cue() {
        for persona in [PersonaTraits::default(), female()] {
            for rung in 1..=3 {
                for k in [
                    EventKind::BrosGathering,
                    EventKind::SisGathering,
                    EventKind::TakingSpace,
                    EventKind::FellOut,
                ] {
                    assert!(!is_told(k, rung, &persona), "{k:?} rung {rung}");
                }
            }
        }
        assert!(!is_told(EventKind::OutShopping, 1, &female()));
        assert!(is_told(EventKind::OutShopping, 2, &female()));
        assert!(is_told(
            EventKind::OutShopping,
            1,
            &PersonaTraits::default()
        ));
    }

    #[test]
    fn every_told_kind_has_its_cue() {
        let h = Duration::hours(1);
        for (kind, cue) in [
            (
                EventKind::PoliceEncounter,
                "（你刚在外面被警察拦下来问话。）",
            ),
            (
                EventKind::AnimalEncounter,
                "（你刚在路上遇到了一只小动物。）",
            ),
            (EventKind::HitSomeone, "（你刚动手打了人。）"),
            (EventKind::SuddenDesire, "（你突然很想要。）"),
            (EventKind::GroceryShopping, "（你正在外面买菜。）"),
            (EventKind::Showering, "（你准备去洗澡了。）"),
            (EventKind::Bathing, "（你正在泡澡。）"),
            (EventKind::InClass, "（你正在上课。）"),
            (EventKind::OnTheWay, "（你正在赶路。）"),
            (
                EventKind::TempWork,
                "（你刚临时接到一份工作，得马上去处理。）",
            ),
            (EventKind::ExMessage, "（你的前任刚给你发来消息。）"),
            (EventKind::WeatherChange, "（你那边的天气刚刚突然变了。）"),
            (EventKind::GotHit, "（你刚被人打了。）"),
            (EventKind::Breakup, "（你刚和交往的人分手了。）"),
            (EventKind::WorkingOut, "（你正在健身。）"),
            (EventKind::Reading, "（你正在看书。）"),
            (EventKind::Gaming, "（你正在打游戏。）"),
            (EventKind::Unwell, "（你现在有点不舒服。）"),
            (EventKind::Irritable, "（你现在心里很烦躁。）"),
            (EventKind::WantToGoOut, "（你现在很想出门走走。）"),
            (EventKind::WantToGoHome, "（你现在很想回家。）"),
            (EventKind::Period, "（你的月经来了。）"),
            (
                EventKind::PeriodScare,
                "（你发现月经推迟了好几天还没来，心里有点慌。）",
            ),
            (
                EventKind::PregnancyScare,
                "（你刚验了孕，验出两条杠，整个人都懵了。）",
            ),
            (EventKind::GotBumped, "（你刚走在路上，被擦撞了一下。）"),
            (EventKind::BumpedSomeone, "（你刚不小心撞到了人。）"),
            (EventKind::Quarrel, "（你刚跟人吵了一架。）"),
            (EventKind::ScoldedBySuperior, "（你刚被上级骂了一顿。）"),
            (EventKind::ScoldedSuperior, "（你刚把上级骂了一顿。）"),
            (
                EventKind::Busy,
                "（你接下来要忙一阵，这段时间顾不上看手机。）",
            ),
            (
                EventKind::InBattle,
                "（你接下来要去打一场硬仗，一段时间顾不上手机。）",
            ),
            (
                EventKind::AdmiringScenery,
                "（你正在看眼前的风景，看得有点出神。）",
            ),
            (
                EventKind::AdmiringArt,
                "（你正在看展，眼前有一件作品让你停了下来。）",
            ),
            (
                EventKind::TakingSelfie,
                "（你刚拍了一张自拍，想发给对方看看。）",
            ),
            (
                EventKind::OnTheRoad,
                "（你正在路上，这一趟要走好几个小时。）",
            ),
            (EventKind::WatchingMatch, "（你正在看一场比赛。）"),
            (EventKind::WatchingShow, "（你正在看一场演出。）"),
            (EventKind::BirdEncounter, "（你刚看到一只鸟停在不远处。）"),
            (EventKind::ExWantsBack, "（你的前任刚来找你，说想复合。）"),
            (EventKind::WantToQuitJob, "（你刚冒出想辞职的念头。）"),
            (
                EventKind::PonderingLife,
                "（你刚一个人发呆，在想人生的事。）",
            ),
            (EventKind::Streaking, "（你刚一时兴起，脱光了跑了一圈。）"),
            (EventKind::WantToSeeSea, "（你现在很想去看海。）"),
            (EventKind::WantHotSpring, "（你现在很想去泡温泉。）"),
            (EventKind::WantSauna, "（你现在很想去洗桑拿。）"),
            (EventKind::WatchingStocks, "（你正在看股票行情。）"),
            (EventKind::Trading, "（你正在和人谈一笔买卖。）"),
            (EventKind::GotLost, "（你刚迷了路，找不到要去的地方。）"),
            (EventKind::Drinking, "（你正在喝酒。）"),
            (EventKind::Smoking, "（你正在抽烟。）"),
            (EventKind::SmokingWeed, "（你正在抽大麻。）"),
            (EventKind::JustHaveSex, "（你刚和人做完爱。）"),
            (EventKind::Masturbating, "（你正在自慰。）"),
            (EventKind::MissedRide, "（你刚没赶上车。）"),
        ] {
            assert_eq!(
                kind_cue(kind, h, 1, &PersonaTraits::default(), None, false),
                cue,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn she_gets_lost_alone_or_with_the_user() {
        let h = Duration::hours(1);
        let cue = |bg| {
            event_cue(
                EventKind::GotLost,
                h,
                bg,
                1,
                &PersonaTraits::default(),
                None,
                false,
            )
        };
        assert_eq!(
            cue(Some(EventKind::ShortTrip)),
            "（你正在短途旅行。你刚迷了路，找不到要去的地方。）"
        );
        assert_eq!(
            cue(Some(EventKind::TravelingTogether)),
            "（你们一起出来玩，刚迷了路。）"
        );
    }

    #[test]
    fn she_misses_a_ride_alone_or_with_the_user() {
        let h = Duration::hours(1);
        let cue = |bg| {
            event_cue(
                EventKind::MissedRide,
                h,
                bg,
                1,
                &PersonaTraits::default(),
                None,
                false,
            )
        };
        assert_eq!(
            cue(Some(EventKind::ShortTrip)),
            "（你正在短途旅行。你刚没赶上车。）"
        );
        assert_eq!(
            cue(Some(EventKind::TravelingTogether)),
            "（你们一起出来玩，刚没赶上车。）"
        );
    }

    #[test]
    fn an_incident_during_a_background_opens_with_its_state() {
        let h = Duration::hours(1);
        let cue = |kind, bg| event_cue(kind, h, bg, 1, &PersonaTraits::default(), None, false);
        assert_eq!(
            cue(EventKind::AnimalEncounter, Some(EventKind::BusinessTrip)),
            "（你正在外地出差。你刚在路上遇到了一只小动物。）"
        );
        assert_eq!(
            cue(EventKind::WantToGoOut, Some(EventKind::Hospitalized)),
            "（你正在住院。你现在很想出门走走。）"
        );
        assert_eq!(
            cue(EventKind::WantToGoHome, Some(EventKind::LongTrip)),
            "（你正在长途旅行。你现在很想回家。）"
        );
        for (bg, state) in [
            (EventKind::Sick, "你正在生病"),
            (EventKind::Period, "你正在经期"),
            (EventKind::Hospitalized, "你正在住院"),
            (EventKind::PostSurgery, "你刚做完手术，还在恢复"),
            (EventKind::BusinessTrip, "你正在外地出差"),
            (EventKind::ShortTrip, "你正在短途旅行"),
            (EventKind::LongTrip, "你正在长途旅行"),
            (EventKind::HometownVisit, "你正在老家"),
        ] {
            assert_eq!(
                cue(EventKind::Reading, Some(bg)),
                format!("（{state}。你正在看书。）")
            );
        }
        assert_eq!(cue(EventKind::Reading, None), "（你正在看书。）");
    }

    /// Spec 2026-11-08-strip-and-world-setting-design.md §3.
    #[test]
    fn a_strip_event_reads_undressed() {
        assert_eq!(
            event_clothing(EventKind::Strip, None).as_deref(),
            Some("刚把衣服脱了")
        );
        assert_eq!(
            event_clothing(EventKind::Strip, Some("红色旗袍")).as_deref(),
            Some("刚把「红色旗袍」脱了")
        );
        assert_eq!(
            event_clothing(EventKind::Reading, Some("睡衣")).as_deref(),
            Some("睡衣")
        );
        assert_eq!(event_clothing(EventKind::Reading, None), None);
        let h = Duration::minutes(30);
        let d = PersonaTraits::default();
        assert_eq!(
            kind_cue(EventKind::Strip, h, 3, &d, None, true),
            "（你刚脱掉衣服拍了一张，发给了对方。）"
        );
        assert_eq!(
            kind_cue(EventKind::Strip, h, 3, &d, None, false),
            "（你刚脱了衣服，想让对方看看。）"
        );
    }

    #[test]
    fn a_selfie_with_its_picture_says_she_sent_it() {
        let h = Duration::hours(1);
        let d = PersonaTraits::default();
        assert_eq!(
            kind_cue(EventKind::TakingSelfie, h, 1, &d, None, true),
            "（你刚拍了一张自拍，发给了对方。）"
        );
        assert_eq!(
            kind_cue(EventKind::TakingSelfie, h, 1, &d, None, false),
            "（你刚拍了一张自拍，想发给对方看看。）"
        );
        // `picture` is read by the selfie only.
        for k in [
            EventKind::Reading,
            EventKind::AdmiringScenery,
            EventKind::SawBag,
        ] {
            assert_eq!(
                kind_cue(k, h, 2, &female(), Some("傲娇"), true),
                kind_cue(k, h, 2, &female(), Some("傲娇"), false),
                "{k:?}"
            );
        }
    }

    #[test]
    fn a_picture_cue_opens_with_the_background() {
        let cue = |bg| {
            event_cue(
                EventKind::TakingSelfie,
                Duration::minutes(30),
                Some(bg),
                1,
                &PersonaTraits::default(),
                None,
                true,
            )
        };
        assert_eq!(
            cue(EventKind::ShortTrip),
            "（你正在短途旅行。你刚拍了一张自拍，发给了对方。）"
        );
        assert_eq!(
            cue(EventKind::Hospitalized),
            "（你正在住院。你刚拍了一张自拍，发给了对方。）"
        );
    }

    #[test]
    fn a_false_alarm_during_a_trip_opens_with_the_trip() {
        assert_eq!(
            event_cue(
                EventKind::PeriodScare,
                Duration::hours(1),
                Some(EventKind::BusinessTrip),
                1,
                &PersonaTraits::default(),
                None,
                false
            ),
            "（你正在外地出差。你发现月经推迟了好几天还没来，心里有点慌。）"
        );
    }

    #[test]
    fn the_life_choice_cues_are_exact() {
        let cue = |c| Occasion::LifeChoice(c).cue(1, &PersonaTraits::default(), None);
        assert_eq!(
            cue(LifeChoiceCue::Ask {
                kind: LifeChoiceKind::AskForSpace,
                tier_gap: Some(3),
                anchor_seen: None
            }),
            "（这段关系最近让你很累：心动还在，但交情和耐心都被磨薄了。你决定郑重地告诉对方：你想先分开一阵子，希望对方给你一个答复。）"
        );
        assert_eq!(
            cue(LifeChoiceCue::Goodbye { hours: 18 }),
            "（对方同意了你提出的分开请求，接下来 18 小时你们不联系。你跟对方道别。）"
        );
        assert_eq!(
            cue(LifeChoiceCue::Hurt),
            "（对方不同意你提出的分开请求，你心里不好受。）"
        );
        for c in [
            LifeChoiceCue::Ask {
                kind: LifeChoiceKind::AskForSpace,
                tier_gap: Some(2),
                anchor_seen: None,
            },
            LifeChoiceCue::Goodbye { hours: 12 },
            LifeChoiceCue::Hurt,
            LifeChoiceCue::Ask {
                kind: LifeChoiceKind::Propose,
                tier_gap: None,
                anchor_seen: None,
            },
            LifeChoiceCue::Answered {
                kind: LifeChoiceKind::SleepTogether,
                accepted: true,
            },
        ] {
            assert_eq!(Occasion::LifeChoice(c).name(), "life_choice");
        }
    }

    #[test]
    fn the_romantic_life_choice_cues_are_exact() {
        let cue = |c| Occasion::LifeChoice(c).cue(1, &PersonaTraits::default(), None);
        let ask = |kind| LifeChoiceCue::Ask {
            kind,
            tier_gap: None,
            anchor_seen: None,
        };
        let answered = |kind, accepted| LifeChoiceCue::Answered { kind, accepted };
        assert_eq!(
            cue(ask(LifeChoiceKind::SleepTogether)),
            "（你们最近越来越亲密，你很想和对方更进一步。你决定主动开口，问对方愿不愿意和你上床，希望对方给你一个答复。）"
        );
        assert_eq!(
            cue(answered(LifeChoiceKind::SleepTogether, true)),
            "（对方答应了你的邀请。）"
        );
        assert_eq!(
            cue(answered(LifeChoiceKind::SleepTogether, false)),
            "（对方拒绝了你的邀请，你有点失落。）"
        );
        assert_eq!(
            cue(ask(LifeChoiceKind::Propose)),
            "（你越来越确定想和对方共度一生。你决定郑重地向对方求婚，希望对方给你一个答复。）"
        );
        assert_eq!(
            cue(ask(LifeChoiceKind::BatheTogether)),
            "（你们最近越来越亲密，你很想和对方一起洗个澡。你决定主动开口，问对方愿不愿意和你一起洗，希望对方给你一个答复。）"
        );
        assert_eq!(
            cue(answered(LifeChoiceKind::BatheTogether, true)),
            "（对方答应了和你一起洗澡，你们现在一起泡在浴缸里。）"
        );
        assert_eq!(
            cue(answered(LifeChoiceKind::BatheTogether, false)),
            "（对方没有答应和你一起洗澡，你有点失落。）"
        );
        assert_eq!(
            cue(answered(LifeChoiceKind::Propose, true)),
            "（对方答应了你的求婚。）"
        );
        assert_eq!(
            cue(answered(LifeChoiceKind::Propose, false)),
            "（对方拒绝了你的求婚，你心里很难受。）"
        );
    }

    #[test]
    fn a_background_cue_carries_its_length() {
        let cue = |k, h| {
            kind_cue(
                k,
                Duration::hours(h),
                1,
                &PersonaTraits::default(),
                None,
                false,
            )
        };
        assert_eq!(
            cue(EventKind::Sick, 30),
            "（你生病了，大概要 30 小时才会好。）"
        );
        assert_eq!(
            cue(EventKind::Hospitalized, 72),
            "（你刚住进医院，大概要住 3 天。）"
        );
        assert_eq!(
            cue(EventKind::PostSurgery, 120),
            "（你刚做完一场手术，大概要恢复 5 天。）"
        );
        assert_eq!(
            cue(EventKind::BusinessTrip, 24),
            "（你刚出发去外地出差，要待 24 小时。）"
        );
        assert_eq!(
            cue(EventKind::ShortTrip, 48),
            "（你刚出发去短途旅行，要玩 2 天。）"
        );
        assert_eq!(
            cue(EventKind::LongTrip, 168),
            "（你刚出发去长途旅行，要去 7 天。）"
        );
        assert_eq!(
            cue(EventKind::HometownVisit, 72),
            "（你刚动身回老家，要待 3 天。）"
        );
    }
}

/// A stored row's id as the frames carry it: the ULID string of the same
/// 128 bits.
pub(crate) fn frame_message_id(id: Uuid) -> String {
    ulid_string(Ulid::from(id))
}

/// The `done` of a stored proactive row.
fn done_frame(
    row: &ChatMessage,
    usage: Option<serde_json::Value>,
    generation_id: Option<String>,
) -> ProtocolFrame {
    ProtocolFrame::ProactiveDone {
        message_id: frame_message_id(row.id),
        truncated: false,
        usage,
        generation_id,
        sent_at: row.sent_at,
        occasion: crate::routes::session_open::GreetingDto::from(row.clone()).occasion,
    }
}

/// The frames for a proactive row that is already stored (a replayed key):
/// `meta`, its whole content as one `delta`, `done`, `image_request` when
/// given, `final`.
pub(crate) fn replay_frames(
    row: &ChatMessage,
    image_request: Option<ProtocolFrame>,
) -> Vec<ProtocolFrame> {
    let message_id = frame_message_id(row.id);
    let meta = row.metadata.as_ref();
    let mut frames = vec![
        ProtocolFrame::Meta {
            message_id: message_id.clone(),
            action_type: FrameActionType::Proactive,
            model: None,
            continues_from: None,
            speaker_session_id: None,
        },
        ProtocolFrame::Delta {
            message_id,
            content: row.content.clone(),
        },
        done_frame(row, None, row.generation_id.clone()),
    ];
    frames.extend(image_request);
    frames.push(build_final_frame(
        false,
        meta.and_then(|m| m.get("prompt_traits"))
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        meta.and_then(|m| m.get("tier"))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        0,
        0,
        Vec::new(),
        Vec::new(),
    ));
    frames
}

/// The `final` of a stream that generated nothing and replays nothing.
pub(crate) fn bare_final(tier: Option<String>) -> ProtocolFrame {
    build_final_frame(false, None, tier, 0, 0, Vec::new(), Vec::new())
}

fn error_frame(
    code: StreamErrorCode,
    retryable: bool,
    message: &str,
    upstream_status: Option<u16>,
    provider_code: Option<String>,
) -> ProtocolFrame {
    ProtocolFrame::Error {
        code,
        retryable,
        message: message.into(),
        user_message: match code {
            StreamErrorCode::Internal => "服务出现问题，请稍后再试",
            StreamErrorCode::Superseded => "已有更新的消息",
            _ => "AI 服务暂时不可用，稍后再试",
        }
        .into(),
        upstream_status,
        provider_code,
    }
}

/// [`greet`] as a stream of frames (spec
/// 2026-11-10-proactive-reply-streaming-design.md §4–§5): `meta` → `delta`…
/// → `done` → `image_request`? → `final`; `final` alone for a blank reply; a
/// terminal `error` when the chain fails or the insert yields. Same prompt,
/// same chain and same inserts as `greet`; the row is stored before `done`,
/// under the id `meta` announced. Nothing here outlives the stream: dropping
/// it drops the generation.
///
/// `fixed_retryable` pins every error's `retryable`, for a caller whose
/// request cannot be made again.
pub(crate) fn greet_stream(
    state: AppState,
    g: Greeting,
    fixed_retryable: Option<bool>,
) -> impl futures_util::Stream<Item = ProtocolFrame> + Send + 'static {
    async_stream::stream! {
        let retryable = |r: bool| fixed_retryable.unwrap_or(r);
        let prep = match prepare(&state, &g).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(session_id = %g.session_id, "proactive stream: prepare failed: {e}");
                yield error_frame(StreamErrorCode::Internal, retryable(false), "internal error", None, None);
                return;
            }
        };
        let id: Uuid = g.message_id.unwrap_or_else(|| Ulid::new().into());
        let message_id = frame_message_id(id);
        let display = state.model_config.display_override(CHAT_TASK);
        let chain = crate::byok::reply_chain(&prep.chat_req, None);
        let mut failures: Vec<AttemptFailure> = Vec::new();
        // The hop that served: its index, raw text, usage and generation id.
        let mut served = None;

        for (idx, hop) in chain.iter().enumerate() {
            let bare = bare_model_id(&hop.slug);
            let mut scrubber = eros_engine_llm::stream_scrub::StreamScrubber::new(
                &state.output_regex,
                &bare,
            );
            let mut acc = String::new();
            // Scrubbed text not yet sent: `meta` waits for its first
            // non-whitespace character.
            let mut held = String::new();
            let mut meta_sent = false;
            let mut usage = None;
            let mut generation_id = None;
            let mut failed = false;

            match tokio::time::timeout(
                STREAM_OPEN_TIMEOUT,
                state.openrouter.execute_stream_as(&prep.chat_req, &hop.slug),
            )
            .await
            {
                Ok(Ok(mut upstream)) => {
                    use futures_util::StreamExt as _;
                    let deadline = tokio::time::Instant::now() + STREAM_TOTAL_TIMEOUT;
                    loop {
                        let chunk = match tokio::time::timeout_at(deadline, upstream.next()).await {
                            Ok(Some(Ok(c))) => c,
                            Ok(Some(Err(e))) => {
                                tracing::warn!("proactive stream: upstream chunk err: {e}");
                                failures.push(AttemptFailure::from_llm_error(CHAT_TASK, &hop.audit, &e));
                                failed = true;
                                break;
                            }
                            Ok(None) => break,
                            Err(_) => {
                                failures.push(gateway_timeout(
                                    &hop.audit,
                                    eros_engine_llm::failure::GatewayKind::TotalTimeout,
                                ));
                                failed = true;
                                break;
                            }
                        };
                        if let Some(content) = chunk.content {
                            acc.push_str(&content);
                            held.push_str(&scrubber.push(&content));
                            if !held.trim().is_empty() {
                                if !meta_sent {
                                    meta_sent = true;
                                    yield ProtocolFrame::Meta {
                                        message_id: message_id.clone(),
                                        action_type: FrameActionType::Proactive,
                                        model: hop_display(hop, display.as_ref(), &bare),
                                        continues_from: None,
                                        speaker_session_id: None,
                                    };
                                }
                                yield ProtocolFrame::Delta {
                                    message_id: message_id.clone(),
                                    content: std::mem::take(&mut held),
                                };
                            } else if meta_sent && !held.is_empty() {
                                yield ProtocolFrame::Delta {
                                    message_id: message_id.clone(),
                                    content: std::mem::take(&mut held),
                                };
                            }
                        }
                        if chunk.usage.is_some() {
                            usage = chunk.usage;
                        }
                        if chunk.generation_id.is_some() {
                            generation_id = chunk.generation_id;
                        }
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!("proactive stream: upstream open err: {e}");
                    failures.push(AttemptFailure::from_llm_error(CHAT_TASK, &hop.audit, &e));
                    failed = true;
                }
                Err(_) => {
                    failures.push(gateway_timeout(
                        &hop.audit,
                        eros_engine_llm::failure::GatewayKind::OpenTimeout,
                    ));
                    failed = true;
                }
            }
            // Undecoded byte-level BPE (issue #84) is a failed hop, as in
            // `execute`.
            let garbled = !failed && eros_engine_llm::byte_bpe::looks_byte_garbled(&acc);
            if garbled {
                tracing::error!(model = %hop.audit, "proactive stream: byte-BPE garbled completion");
            }
            if failed || garbled {
                if !meta_sent {
                    continue;
                }
                // Her text has gone out: no other model may take over.
                let (code, r, upstream_status, provider_code) = if garbled {
                    (StreamErrorCode::UpstreamUnavailable, true, None, None)
                } else {
                    error_frame_fields_from_last(&failures)
                };
                yield error_frame(code, retryable(r), "generation failed mid-reply", upstream_status, provider_code);
                return;
            }

            held.push_str(&scrubber.finish());
            if !held.trim().is_empty() && !meta_sent {
                meta_sent = true;
                yield ProtocolFrame::Meta {
                    message_id: message_id.clone(),
                    action_type: FrameActionType::Proactive,
                    model: hop_display(hop, display.as_ref(), &bare),
                    continues_from: None,
                    speaker_session_id: None,
                };
            }
            if meta_sent && !held.is_empty() {
                yield ProtocolFrame::Delta {
                    message_id: message_id.clone(),
                    content: held,
                };
            }
            served = Some((idx, bare, hop.audit.clone(), acc, usage, generation_id));
            break;
        }

        let Some((idx, bare, model, acc, usage, generation_id)) = served else {
            tracing::warn!(
                session_id = %g.session_id,
                occasion = g.occasion.name(),
                "proactive stream: companion chain exhausted"
            );
            let (code, r, upstream_status, provider_code) = error_frame_fields_from_last(&failures);
            yield error_frame(code, retryable(r), "all models failed", upstream_status, provider_code);
            return;
        };
        let usage_full = usage.as_ref().and_then(|u| serde_json::to_value(u).ok());
        let generation_id = record_generation(
            &state.pool,
            GenerationRecord {
                task: CHAT_TASK,
                session_id: Some(g.session_id),
                generation_id: generation_id.as_deref(),
                model: Some(model.as_str()),
                usage: usage_full.as_ref(),
            },
        )
        .await;
        let (llm_attempts, gateway_errors) = split_failures_typed(&failures);
        let final_frame = build_final_frame(
            false,
            Some(prep.injected_tags.clone()),
            g.tier.clone(),
            idx as u32,
            0,
            llm_attempts,
            gateway_errors,
        );
        let stripped = apply_output_regex(&state.output_regex, &bare, &acc);
        let content = stripped.cleaned.trim();
        if content.is_empty() {
            tracing::warn!(
                session_id = %g.session_id,
                occasion = g.occasion.name(),
                "proactive stream: blank reply, nothing written"
            );
            yield final_frame;
            return;
        }

        let session_id = g.session_id;
        let stored = match store(&state, g, &prep, id, content, generation_id.clone(), &failures).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(%session_id, "proactive stream: store failed: {e}");
                yield error_frame(StreamErrorCode::Internal, retryable(false), "internal error", None, None);
                return;
            }
        };
        let Some(row) = stored.row else {
            yield error_frame(
                StreamErrorCode::Superseded,
                false,
                "a newer message landed first",
                None,
                None,
            );
            return;
        };
        let mut wire_usage = usage_full;
        crate::routes::companion::filter_usage_keys(
            &mut wire_usage,
            &state.config.openrouter_usage_hidden_keys,
        );
        yield done_frame(&row, wire_usage, generation_id);
        if let Some((composed_prompt, aspect_ratio)) = stored.picture {
            yield build_image_request_frame(
                message_id,
                &composed_prompt,
                ImageRef::Face,
                aspect_ratio.as_deref(),
            );
        }
        yield final_frame;
    }
}

fn gateway_timeout(model: &str, kind: eros_engine_llm::failure::GatewayKind) -> AttemptFailure {
    AttemptFailure::Gateway(eros_engine_llm::failure::GatewayError {
        task: CHAT_TASK.to_string(),
        model: Some(model.to_string()),
        kind,
        message: "stream timeout".into(),
    })
}

/// `greet_stream` against a streamed upstream (spec
/// 2026-11-10-proactive-reply-streaming-design.md §4–§5).
#[cfg(test)]
mod stream_tests {
    use super::*;
    use futures_util::StreamExt as _;
    use serde_json::{json, Value};
    use sqlx::PgPool;
    use std::sync::Arc;
    use wiremock::matchers::{body_partial_json, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::pipeline::stream::ProtocolFrame;
    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{seed_genome, seed_instance, seed_session};
    use crate::routes::companion_stream::resolve_text_turn;

    /// An upstream SSE body: one content chunk per entry, then `[DONE]`.
    fn sse(chunks: &[&str]) -> String {
        let mut body = String::new();
        for c in chunks {
            body.push_str(&format!(
                "data: {}\n\n",
                json!({"id": "gen-s", "model": "served", "choices": [{"delta": {"content": c}}]})
            ));
        }
        body.push_str("data: [DONE]\n\n");
        body
    }

    async fn mount(mock: &MockServer, model: &str, template: ResponseTemplate) {
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_partial_json(json!({"model": model})))
            .respond_with(template)
            .mount(mock)
            .await;
    }

    fn streamed(body: String) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
    }

    fn state_for(pool: PgPool, mock_uri: &str) -> AppState {
        let mut state = test_state(pool);
        state.model_config = Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"primary\"\nfallback = [\"backup\"]\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{mock_uri}/api/v1/chat/completions"),
            ),
        );
        state
    }

    /// A `just_opened` greeting for a fresh session, `since` an hour ago.
    async fn greeting(state: &AppState) -> Greeting {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&state.pool, "Aria").await;
        let instance_id = seed_instance(&state.pool, genome_id, user_id).await;
        let session_id = seed_session(&state.pool, user_id, instance_id).await;
        let (_s, persona, _i) = resolve_text_turn(state, session_id, user_id, "test")
            .await
            .unwrap();
        Greeting {
            session_id,
            user_id,
            instance_id,
            persona,
            now: NowContext::default(),
            occasion: Occasion::JustOpened {
                open_key: "k1".into(),
            },
            recall_query: String::new(),
            since: Utc::now() - chrono::Duration::hours(1),
            tier: None,
            prompt_traits: Vec::new(),
            memory_scope: Default::default(),
            affinity_scope: Default::default(),
            audit: None,
            metadata: serde_json::Map::new(),
            message_id: None,
        }
    }

    async fn run(state: &AppState, g: Greeting, fixed: Option<bool>) -> Vec<Value> {
        greet_stream(state.clone(), g, fixed)
            .map(|f: ProtocolFrame| serde_json::to_value(f).unwrap())
            .collect()
            .await
    }

    fn types(frames: &[Value]) -> Vec<&str> {
        frames.iter().map(|f| f["type"].as_str().unwrap()).collect()
    }

    fn deltas(frames: &[Value]) -> String {
        frames
            .iter()
            .filter(|f| f["type"] == "delta")
            .map(|f| f["content"].as_str().unwrap())
            .collect()
    }

    async fn proactive_rows(pool: &PgPool, session_id: Uuid) -> Vec<(Uuid, String)> {
        sqlx::query_as(
            "SELECT id, content FROM engine.chat_messages \
             WHERE session_id = $1 AND assistant_action_type = 'proactive'",
        )
        .bind(session_id)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn stream_happy_path_orders_frames_and_stores_under_meta_id(pool: PgPool) {
        let mock = MockServer::start().await;
        mount(&mock, "primary", streamed(sse(&["你来", "啦", "～"]))).await;
        let state = state_for(pool.clone(), &mock.uri());
        let g = greeting(&state).await;
        let session_id = g.session_id;

        let frames = run(&state, g, None).await;
        assert_eq!(
            types(&frames),
            ["meta", "delta", "delta", "delta", "done", "final"]
        );
        assert_eq!(frames[0]["action_type"], "proactive");
        let meta_id = frames[0]["message_id"].as_str().unwrap();
        let rows = proactive_rows(&pool, session_id).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].0,
            Uuid::from(Ulid::from_string(meta_id).unwrap()),
            "stored under the id meta announced"
        );
        assert_eq!(rows[0].1, "你来啦～");
        assert_eq!(deltas(&frames).trim(), rows[0].1);
        let done = &frames[4];
        assert_eq!(done["message_id"], meta_id);
        assert_eq!(done["occasion"], "just_opened");
        assert_eq!(done["truncated"], false);
        assert!(done["sent_at"].is_string());
        assert_eq!(frames[5]["retries_chat"], 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn stream_whitespace_first_chunk_delays_meta(pool: PgPool) {
        let mock = MockServer::start().await;
        mount(&mock, "primary", streamed(sse(&["\n", "hi"]))).await;
        let state = state_for(pool.clone(), &mock.uri());
        let g = greeting(&state).await;

        let frames = run(&state, g, None).await;
        assert_eq!(types(&frames), ["meta", "delta", "done", "final"]);
        assert_eq!(deltas(&frames), "\nhi");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn stream_blank_reply_is_final_alone(pool: PgPool) {
        let mock = MockServer::start().await;
        mount(&mock, "primary", streamed(sse(&[" ", "\n"]))).await;
        let state = state_for(pool.clone(), &mock.uri());
        let g = greeting(&state).await;
        let session_id = g.session_id;

        let frames = run(&state, g, None).await;
        assert_eq!(types(&frames), ["final"]);
        assert!(proactive_rows(&pool, session_id).await.is_empty());
        assert_eq!(
            mock.received_requests().await.unwrap().len(),
            1,
            "a blank reply does not try the next model"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn stream_falls_back_before_first_delta(pool: PgPool) {
        let mock = MockServer::start().await;
        mount(&mock, "primary", ResponseTemplate::new(500)).await;
        mount(&mock, "backup", streamed(sse(&["在呢"]))).await;
        let state = state_for(pool.clone(), &mock.uri());
        let g = greeting(&state).await;
        let session_id = g.session_id;

        let frames = run(&state, g, None).await;
        assert_eq!(types(&frames), ["meta", "delta", "done", "final"]);
        let last = frames.last().unwrap();
        assert_eq!(last["retries_chat"], 1);
        assert_eq!(last["llm_attempts"].as_array().unwrap().len(), 1);
        assert_eq!(proactive_rows(&pool, session_id).await.len(), 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn stream_error_after_delta_stores_nothing(pool: PgPool) {
        let mock = MockServer::start().await;
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices": [{"delta": {"content": "我刚"}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "error"}],
                   "error": {"code": 502, "message": "upstream died"}}),
        );
        mount(&mock, "primary", streamed(body)).await;
        mount(&mock, "backup", streamed(sse(&["不该用到"]))).await;
        let state = state_for(pool.clone(), &mock.uri());
        let g = greeting(&state).await;
        let session_id = g.session_id;

        let frames = run(&state, g, None).await;
        assert_eq!(types(&frames), ["meta", "delta", "error"]);
        assert!(proactive_rows(&pool, session_id).await.is_empty());
        assert_eq!(
            mock.received_requests().await.unwrap().len(),
            1,
            "no fallback once text has gone out"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn stream_chain_exhausted_is_error(pool: PgPool) {
        let mock = MockServer::start().await;
        mount(&mock, "primary", ResponseTemplate::new(500)).await;
        mount(&mock, "backup", ResponseTemplate::new(503)).await;
        let state = state_for(pool.clone(), &mock.uri());
        let g = greeting(&state).await;
        let session_id = g.session_id;

        let frames = run(&state, g, None).await;
        assert_eq!(types(&frames), ["error"]);
        assert_eq!(frames[0]["code"], "upstream_unavailable");
        assert_eq!(frames[0]["upstream_status"], 503);
        assert_eq!(frames[0]["retryable"], true);
        assert!(proactive_rows(&pool, session_id).await.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn stream_fixed_retryable_pins_error(pool: PgPool) {
        let mock = MockServer::start().await;
        mount(&mock, "primary", ResponseTemplate::new(500)).await;
        mount(&mock, "backup", ResponseTemplate::new(503)).await;
        let state = state_for(pool.clone(), &mock.uri());
        let g = greeting(&state).await;

        let frames = run(&state, g, Some(false)).await;
        assert_eq!(types(&frames), ["error"]);
        assert_eq!(frames[0]["retryable"], false);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn stream_superseded_when_insert_yields(pool: PgPool) {
        let mock = MockServer::start().await;
        mount(&mock, "primary", streamed(sse(&["你来啦"]))).await;
        let state = state_for(pool.clone(), &mock.uri());
        let g = greeting(&state).await;
        let session_id = g.session_id;
        // A user message landed after the request began.
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
             VALUES ($1, 'user', '在吗', $2)",
        )
        .bind(session_id)
        .bind(g.since + chrono::Duration::minutes(1))
        .execute(&pool)
        .await
        .unwrap();

        let frames = run(&state, g, None).await;
        assert_eq!(types(&frames), ["meta", "delta", "error"]);
        let err = frames.last().unwrap();
        assert_eq!(err["code"], "superseded");
        assert_eq!(err["retryable"], false);
        assert!(proactive_rows(&pool, session_id).await.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn stream_uses_the_callers_message_id(pool: PgPool) {
        let mock = MockServer::start().await;
        mount(&mock, "primary", streamed(sse(&["你来啦"]))).await;
        let state = state_for(pool.clone(), &mock.uri());
        let mut g = greeting(&state).await;
        let id: Uuid = Ulid::new().into();
        g.message_id = Some(id);
        let session_id = g.session_id;

        let frames = run(&state, g, None).await;
        assert_eq!(frames[0]["message_id"], frame_message_id(id));
        assert_eq!(proactive_rows(&pool, session_id).await[0].0, id);
    }

    /// A rule that can only be judged on the whole reply makes the scrubber
    /// hold everything until the stream ends: the text still goes out, after
    /// `meta` and before `done`.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn stream_sends_text_the_scrubber_held_to_the_end(pool: PgPool) {
        let mock = MockServer::start().await;
        mount(&mock, "primary", streamed(sse(&["想", "你了"]))).await;
        let mut state = state_for(pool.clone(), &mock.uri());
        let cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(
            r#"
            [tasks.chat_companion]
            model = "primary"

            [[tasks.chat_companion.output_regex]]
            models = ["primary"]
            pattern = '^\s*\[[^\]]*\]\s*$'
            "#,
        )
        .unwrap();
        state.output_regex = Arc::new(cfg.compile_output_regex().unwrap());
        state.model_config = Arc::new(cfg);
        let g = greeting(&state).await;
        let session_id = g.session_id;

        let frames = run(&state, g, None).await;
        let kinds = types(&frames);
        assert_eq!(kinds.first(), Some(&"meta"));
        assert_eq!(&kinds[kinds.len() - 2..], ["done", "final"]);
        assert_eq!(deltas(&frames), "想你了");
        assert_eq!(proactive_rows(&pool, session_id).await[0].1, "想你了");
    }

    #[test]
    fn replay_frames_carry_the_stored_row() {
        let id: Uuid = Ulid::new().into();
        let sent_at = Utc::now();
        let row: ChatMessage = serde_json::from_value(json!({
            "id": id,
            "session_id": Uuid::new_v4(),
            "role": "assistant",
            "content": "中秋快乐",
            "sent_at": sent_at,
            "metadata": {"proactive": "holiday", "tier": "plus"},
        }))
        .unwrap();
        let frames: Vec<Value> = replay_frames(&row, None)
            .into_iter()
            .map(|f| serde_json::to_value(f).unwrap())
            .collect();
        assert_eq!(types(&frames), ["meta", "delta", "done", "final"]);
        assert_eq!(frames[0]["message_id"], frame_message_id(id));
        assert_eq!(frames[0]["action_type"], "proactive");
        assert_eq!(frames[1]["content"], "中秋快乐");
        assert_eq!(frames[2]["occasion"], "holiday");
        assert_eq!(frames[3]["tier"], "plus");
    }
}
