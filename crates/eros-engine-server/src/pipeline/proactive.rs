// SPDX-License-Identifier: AGPL-3.0-only
//! A persona-initiated assistant message: the holiday and birthday greetings,
//! the caller-named openers and the life events. The route
//! (`routes::session_open`, `routes::session_events`) decides whether the
//! persona speaks first; this module generates and persists it. Modelled on
//! the image-edit text half: no PDE, no streaming, no user row, no
//! post-process.
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
//! docs/superpowers/specs/2026-10-15-life-events-v2-design.md §6

use chrono::{DateTime, NaiveDate, Utc};
use eros_engine_core::events::EventKind;
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
use ulid::Ulid;
use uuid::Uuid;

use crate::error::AppError;
use crate::pipeline::handlers::{build_reply_request, CHAT_TASK};
use crate::pipeline::stream::split_failures;
use crate::pipeline::{compute_signals_for_session, record_generation, GenerationRecord};
use crate::prompt::NowContext;
use crate::state::AppState;

/// Speaking first is the engine's decision (spec §6.1), delivered as a stage
/// cue after the history. A `user` turn, so the request never ends on an
/// assistant message a provider would continue as prefill. Never persisted.
pub(crate) const OPEN_CUE: &str = "（对方刚打开和你的聊天，还没说话。）";
const FIRST_MEET_CUE: &str = "（你们还没聊过。对方刚打开和你的聊天，还没说话。）";
const RETURNING_SAME_DAY_CUE: &str = "（对方隔了不到一天又打开和你的聊天，还没说话。）";

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
    /// A life event the persona tells about (spec
    /// 2026-10-15-life-events-v2-design.md §9.1): the caller's key, the kind
    /// and its drawn length.
    Event {
        event_key: String,
        kind: EventKind,
        lasts: chrono::Duration,
        /// The background under way when it was drawn.
        background: Option<EventKind>,
    },
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
            Self::Event { .. } => "event",
        }
    }

    /// The stage cue: states a fact, carries no instruction.
    fn cue(&self) -> String {
        match self {
            Self::Holiday { .. } | Self::Birthday { .. } | Self::JustOpened { .. } => {
                OPEN_CUE.into()
            }
            Self::FirstMeet { .. } => FIRST_MEET_CUE.into(),
            Self::Returning { gap_days: 0, .. } => RETURNING_SAME_DAY_CUE.into(),
            Self::Returning { gap_days, .. } => {
                format!("（对方隔了 {gap_days} 天又打开和你的聊天，还没说话。）")
            }
            Self::Event {
                kind,
                lasts,
                background,
                ..
            } => event_cue(*kind, *lasts, *background),
        }
    }
}

/// The stage cue for a life event (spec 2026-10-15-life-events-v2-design.md
/// §3.4): states a fact, carries no instruction. A background's cue carries
/// its length. A kind that is never told (`events::is_told`) has no cue.
fn kind_cue(kind: EventKind, lasts: chrono::Duration) -> String {
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
        AwayFromPhone | Asleep => unreachable!("{kind:?} is never told"),
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
fn event_cue(kind: EventKind, lasts: chrono::Duration, background: Option<EventKind>) -> String {
    let cue = kind_cue(kind, lasts);
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
pub(crate) struct Greeting<'a> {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub persona: &'a CompanionPersona,
    pub now: &'a NowContext,
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
}

/// Generate and persist the message. `Ok(None)`: the model returned a blank
/// reply and nothing was written; or, for a greeting or opener, the insert
/// yielded to a message that landed first; or, for an event, another event
/// under way for the persona blocked the insert, or a concurrent call with
/// the same `event_key` won it (the caller reads the event by key). When a
/// concurrent call for the same local date or `open_key` won the insert,
/// returns that call's row. An event is stamped with one instant taken after
/// generation: its message's `sent_at`, its `started_at` and the start of its
/// `ends_at` span.
pub(crate) async fn greet(
    state: &AppState,
    g: Greeting<'_>,
) -> Result<Option<ChatMessage>, AppError> {
    let mut affinity = AffinityRepo { pool: &state.pool }
        .load_or_create(g.session_id, g.user_id, g.instance_id)
        .await?;
    affinity.apply_time_decay();
    affinity.refresh_endpoints(&state.config.affinity_tuning);
    let signals = compute_signals_for_session(&state.pool, g.session_id, &affinity).await?;

    // Prompt assembly only. The event's content is the memory-recall query;
    // the nil driving id makes history fall back to newest-N.
    let input = DecisionInput {
        event: Event::UserMessage {
            content: g.recall_query,
            message_id: Uuid::nil(),
            prompt_traits: g.prompt_traits,
            audit: g.audit,
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
        clothing: None,
        image_caption: None,
        image_ref: ImageRef::Previous,
        aspect_ratio: None,
        action_response: None,
    };
    let repo = eros_engine_store::events::EventRepo { pool: &state.pool };
    let current = repo.current(g.instance_id, None, g.now.now).await?;
    let offline = repo
        .unexplained_offline(g.session_id, g.instance_id, g.now.now)
        .await?;
    let scare = repo
        .unresolved_scare(g.session_id, g.instance_id, g.now.now)
        .await?;
    let event_lines = crate::prompt::EventLines {
        under_way: crate::prompt::events_reply_text(&current),
        just_now: crate::prompt::just_now_text(offline.as_ref(), scare.as_ref()),
    };
    let (mut chat_req, injected_tags) = build_reply_request(
        state,
        &input,
        &plan,
        g.session_id,
        g.user_id,
        g.instance_id,
        Uuid::nil(),
        g.now,
        false,
        None,
        None,
        Some(&event_lines),
    )
    .await?;
    chat_req.messages.push(WireMessage {
        role: "user".into(),
        content: g.occasion.cue(),
    });

    let model_for_audit = chat_req.model.clone();
    let resp = match state.openrouter.execute(chat_req).await {
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
    let (llm_attempts, gateway_errors) = split_failures(&resp.failures);

    // An event is written when its message is, not when the request began.
    let event_at = Utc::now();
    let mut metadata = g.metadata;
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
        Occasion::FirstMeet { open_key } | Occasion::JustOpened { open_key } => {
            metadata.insert("open_key".into(), open_key.as_str().into());
        }
        Occasion::Event { .. } => {}
    }
    metadata.insert("prompt_traits".into(), serde_json::json!(injected_tags));
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
        id: Ulid::new().into(),
        content: content.to_string(),
        generation_id,
        metadata: serde_json::Value::Object(metadata),
        llm_attempts,
        gateway_errors,
    };
    match &g.occasion {
        Occasion::Holiday { local_date, .. } | Occasion::Birthday { local_date } => match chat_repo
            .insert_proactive_message(g.session_id, g.since, &row)
            .await?
        {
            Some(written) => Ok(Some(written)),
            None => Ok(chat_repo
                .proactive_greeting_on(g.session_id, &local_date.to_string())
                .await?),
        },
        Occasion::FirstMeet { open_key }
        | Occasion::Returning { open_key, .. }
        | Occasion::JustOpened { open_key } => {
            match chat_repo
                .insert_open_message(g.session_id, g.since, &row)
                .await?
            {
                Some(written) => Ok(Some(written)),
                None => Ok(chat_repo.open_message(g.session_id, open_key).await?),
            }
        }
        Occasion::Event {
            event_key,
            kind,
            lasts,
            ..
        } => Ok(eros_engine_store::events::EventRepo { pool: &state.pool }
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
            .and_then(|i| i.message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    /// `kind_cue` panics for a kind that is never told; it must not for any
    /// kind `is_told` lets through at any rung.
    #[test]
    fn every_kind_she_tells_has_a_cue() {
        use eros_engine_core::events::{is_told, EventKind};
        for k in EventKind::ALL {
            for rung in 1..=3 {
                if is_told(k, rung) {
                    let _ = super::kind_cue(k, chrono::Duration::hours(1));
                }
            }
        }
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
            (EventKind::Period, "（你的月经来了。）"),
            (
                EventKind::PeriodScare,
                "（你发现月经推迟了好几天还没来，心里有点慌。）",
            ),
            (
                EventKind::PregnancyScare,
                "（你刚验了孕，验出两条杠，整个人都懵了。）",
            ),
            (
                EventKind::Busy,
                "（你接下来要忙一阵，这段时间顾不上看手机。）",
            ),
            (
                EventKind::InBattle,
                "（你接下来要去打一场硬仗，一段时间顾不上手机。）",
            ),
        ] {
            assert_eq!(kind_cue(kind, h), cue, "{kind:?}");
        }
    }

    #[test]
    fn an_incident_during_a_background_opens_with_its_state() {
        let h = Duration::hours(1);
        assert_eq!(
            event_cue(EventKind::AnimalEncounter, h, Some(EventKind::BusinessTrip)),
            "（你正在外地出差。你刚在路上遇到了一只小动物。）"
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
                event_cue(EventKind::Reading, h, Some(bg)),
                format!("（{state}。你正在看书。）")
            );
        }
        assert_eq!(event_cue(EventKind::Reading, h, None), "（你正在看书。）");
    }

    #[test]
    fn a_false_alarm_during_a_trip_opens_with_the_trip() {
        assert_eq!(
            event_cue(
                EventKind::PeriodScare,
                Duration::hours(1),
                Some(EventKind::BusinessTrip)
            ),
            "（你正在外地出差。你发现月经推迟了好几天还没来，心里有点慌。）"
        );
    }

    #[test]
    fn a_background_cue_carries_its_length() {
        let cue = |k, h| kind_cue(k, Duration::hours(h));
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
