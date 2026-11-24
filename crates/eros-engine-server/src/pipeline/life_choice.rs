// SPDX-License-Identifier: AGPL-3.0-only
//! The life choices on the pipeline side: when a judged turn starts or ends a
//! cold war, or makes her ask to break up, to confess, to become partners,
//! to marry, to sleep together or fall asleep together, to bathe or to go
//! away together; her generated words; and the growth scale of a cold war.
//! The answer, its costs and the ignore path live in the resolution route and
//! the turn entry.
//!
//! Spec: docs/superpowers/specs/2026-11-23-affinity-50-design.md §6.3 (the
//! cold war and its growth scale), §7.1–§7.3 (her status asks and her sleep
//! ask), §7.4 (her cold-war message), §7.5 (`ask_for_space`, retired); the
//! romantic kinds:
//! docs/superpowers/specs/2026-10-30-give-needs-romantic-choices-and-answer-receipts-design.md
//! §4.3 (triggers), §4.4 (the ask).

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use eros_engine_core::affinity::{Affinity, AffinityLine};
use eros_engine_core::events::EventKind;
use eros_engine_core::persona::CompanionPersona;
use eros_engine_core::relationship::{
    cold_war_ends, cold_war_starts, leading_line, sleep_ask_is_sex, wants_break_up,
    RelationshipStatus, StatusAsk, COLD_WAR_WINDOW,
};
use eros_engine_core::scope::{romance_compatible, AffinityScope, MemoryScope};
use eros_engine_store::activity::ActivityRepo;
use eros_engine_store::affinity::{AffinityRepo, JudgedEndpoints};
use eros_engine_store::chat::ChatRepo;
use eros_engine_store::events::EventRepo;
use eros_engine_store::life_choices::{LifeChoiceKind, LifeChoiceRepo};

use crate::byoc::LoadedInstance;
use crate::error::AppError;
use crate::holiday::UserLocale;
use crate::pipeline::handlers::recall_query_text;
use crate::pipeline::proactive::{greet, Greeting, LifeChoiceCue, Occasion};
use crate::prompt::NowContext;
use crate::state::AppState;

/// Positive line growth while a cold war lasts.
const SLOWED_GROWTH: f64 = 0.5;
/// A judged turn counts toward a romantic choice when its warmth and its
/// patience are both above this; `confess` reads its warmth alone and
/// `partner_up` its patience alone. A rule threshold of the engine, not a
/// judge band (spec 2026-10-30 §4.3, spec 2026-11-23-affinity-50 §7.2).
const WARM: f64 = 0.7;
/// Her sleep ask, `travel_together`, `confess` and `partner_up`: the newest
/// judged turns that must all clear their threshold.
const WARM_STREAK: usize = 3;
/// `confess`: the chemistry tier it needs.
const CONFESS_MIN_CHEM_TIER: u8 = 3;
/// `partner_up`: the bond tier it needs.
const PARTNER_MIN_BOND_TIER: u8 = 3;
/// `confess` and `partner_up`: days she waits after the session's latest
/// status change, and after the kind's last answer of any sort.
const STATUS_COOLDOWN_DAYS: i64 = 14;
/// `propose`: days after an ignore before she asks again.
const PROPOSE_COOLDOWN_DAYS: i64 = 14;
/// `travel_together`: the bond tier it needs.
const TRAVEL_MIN_BOND_TIER: u8 = 3;
/// `travel_together`: days after an accept or an ignore before she asks again.
const TRAVEL_COOLDOWN_DAYS: i64 = 14;
/// `bathe_together`: how many of the newest judged turns it looks at.
const BATHE_WINDOW: i64 = 7;
/// `bathe_together`: the turns in the window whose warmth must reach
/// [`BATHE_WARMTH`].
const BATHE_HITS: usize = 3;
/// `bathe_together`: a rule threshold of the engine, not a judge band.
const BATHE_WARMTH: f64 = 0.5;
/// `bathe_together`: days after an accept or an ignore before she asks again.
const BATHE_COOLDOWN_DAYS: i64 = 7;
/// `propose`: how many of the newest judged turns it looks at (spec
/// 2026-10-30 §4.3).
const PROPOSE_WINDOW: i64 = 8;
/// `propose`: the warm turns the window must hold (spec 2026-10-30 §4.3).
const PROPOSE_HITS: usize = 5;

/// The scale on every positive line delta of a commit in `session_id`:
/// [`SLOWED_GROWTH`] while the session's cold war is on, 1.0 otherwise. A
/// read that fails commits at full speed. The flag is read as stored.
pub(crate) async fn growth_scale_for(state: &AppState, session_id: Uuid) -> f64 {
    match (AffinityRepo { pool: &state.pool }).load(session_id).await {
        Ok(Some(a)) if a.relationship.cold_war() => SLOWED_GROWTH,
        Ok(_) => 1.0,
        Err(e) => {
            tracing::warn!(%session_id, "cold war read failed: {e}");
            1.0
        }
    }
}

/// The ignore path (spec 2026-10-19 §3.4c): an ordinary text turn that
/// arrives while she waits for an answer is the answer. The choice becomes
/// `ignored`, whatever its kind, at no cost and with no event: the judge reads
/// the user's words like any other turn's (spec 2026-10-30 §4.5, spec
/// 2026-11-23-affinity-50 §7.2, §7.5). No tier changes: the judge rates the
/// turn itself.
///
/// The clock is the user row's `sent_at`: whether the message arrived while
/// the choice was pending is a fact about the message, so a turn that waited
/// in the queue past the 24 hours still answers it, and a re-drive reads the
/// same. A message sent before her ask landed answers nothing: the ask can be
/// written while the user's next message is already in flight.
///
/// Nothing happens (and nothing is written) when no choice is pending then —
/// a lapsed one converts nothing (§3.4d) — when a separation she agreed to is
/// under way at the message, or when the resolution route answered first. The
/// caller leaves out action turns: a give, a tip or a kiss does not answer
/// her. `Ok(true)` when the choice was converted.
pub(crate) async fn convert_ignored_ask(
    state: &AppState,
    session_id: Uuid,
    instance_id: Uuid,
    user_message_id: Uuid,
) -> Result<bool, AppError> {
    let Some(sent_at) = (ChatRepo { pool: &state.pool })
        .message_by_id_in_session(session_id, user_message_id)
        .await?
        .map(|m| m.sent_at)
    else {
        return Ok(false);
    };
    let choices = LifeChoiceRepo { pool: &state.pool };
    let Some(choice) = choices
        .pending(session_id, sent_at)
        .await?
        .filter(|c| c.asked_at <= sent_at)
    else {
        return Ok(false);
    };
    // A message during an agreed separation (one from before `ask_for_space`
    // was retired, running out) only ghosts, and answers nothing.
    if separated(&state.pool, instance_id, Some(user_message_id), sent_at).await? {
        return Ok(false);
    }
    // The resolution route may have answered between the read and here; the
    // loser writes nothing.
    if choices
        .resolve(choice.id, "ignored", sent_at)
        .await?
        .is_none()
    {
        return Ok(false);
    }
    tracing::info!(%session_id, choice_id = %choice.id, "her ask was answered in words");
    Ok(true)
}

/// Whether a separation she agreed to is under way for the instance: at
/// `at_message`'s `sent_at`, or at `now` without one.
async fn separated(
    pool: &PgPool,
    instance_id: Uuid,
    at_message: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    Ok(EventRepo { pool }
        .current(instance_id, at_message, now)
        .await?
        .foreground
        .is_some_and(|e| e.kind == EventKind::TakingSpace))
}

/// After a judged turn's affinity committed: start or end a cold war, then
/// put a choice to the user when the relationship calls for one: a break-up
/// in a cold war, a confession or a partnership, then the warm asks (spec
/// 2026-11-23-affinity-50 §7.2). That is §7.2's order although `propose`
/// runs inside [`try_romance`], after `confess` and `partner_up`: its
/// `asked_from` set is disjoint from theirs, so no status lets it and either
/// of them be asked. The affinity is loaded once for every check, and again
/// only when the cold war moved. Every failure is logged and swallowed; the
/// turn is already answered and its affinity written.
pub(crate) async fn maybe_ask(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    persona: &CompanionPersona,
) {
    let mut affinity = match load_decayed(state, session_id).await {
        Ok(Some(a)) => a,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(%session_id, "life-choice affinity read failed: {e}");
            return;
        }
    };
    match try_cold_war(state, session_id, user_id, instance_id, persona, &affinity).await {
        Ok(false) => {}
        Ok(true) => match load_decayed(state, session_id).await {
            Ok(Some(a)) => affinity = a,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(%session_id, "life-choice affinity read failed: {e}");
                return;
            }
        },
        Err(e) => tracing::warn!(%session_id, "cold war check failed: {e}"),
    }
    let a = &affinity;
    // An ask written by one check is pending, which closes the runway for
    // the next.
    if let Err(e) = try_break_up(state, session_id, user_id, instance_id, persona, a).await {
        tracing::warn!(%session_id, "break-up ask failed: {e}");
    }
    if let Err(e) = try_confess(state, session_id, user_id, instance_id, persona, a).await {
        tracing::warn!(%session_id, "confession failed: {e}");
    }
    if let Err(e) = try_partner_up(state, session_id, user_id, instance_id, persona, a).await {
        tracing::warn!(%session_id, "partnership ask failed: {e}");
    }
    if let Err(e) = try_romance(state, session_id, user_id, instance_id, persona, a).await {
        tracing::warn!(%session_id, "romantic choice failed: {e}");
    }
    if let Err(e) = try_bathe(state, session_id, user_id, instance_id, persona, a).await {
        tracing::warn!(%session_id, "bath choice failed: {e}");
    }
    if let Err(e) = try_travel(state, session_id, user_id, instance_id, persona, a).await {
        tracing::warn!(%session_id, "travel choice failed: {e}");
    }
}

/// The session's affinity as the checks read it: time decay applied and the
/// endpoints refreshed. `None` when the session has no row.
async fn load_decayed(state: &AppState, session_id: Uuid) -> Result<Option<Affinity>, AppError> {
    let Some(mut affinity) = (AffinityRepo { pool: &state.pool })
        .load(session_id)
        .await?
    else {
        return Ok(None);
    };
    affinity.apply_time_decay();
    affinity.refresh_endpoints(&state.config.affinity_tuning);
    Ok(Some(affinity))
}

/// Whether she is free to put a choice to the user at all (spec 2026-10-19
/// §3.1, spec 2026-10-30 §4.3): nothing pending on the instance, no
/// separation or date under way, and [`session_eligible`].
async fn runway_clear(
    state: &AppState,
    session_id: Uuid,
    instance_id: Uuid,
    now: DateTime<Utc>,
) -> Result<bool, AppError> {
    // She has one life: one pending ask at a time across the instance's
    // sessions, and none while she is apart from the user, or out with them
    // on a date (a main-chat turn sent before it opened can land during it).
    if (LifeChoiceRepo { pool: &state.pool })
        .pending_for_instance(instance_id, now)
        .await?
    {
        return Ok(false);
    }
    if separated(&state.pool, instance_id, None, now).await? {
        return Ok(false);
    }
    if (ActivityRepo { pool: &state.pool })
        .date_on(instance_id, None, now)
        .await?
    {
        return Ok(false);
    }
    session_eligible(state, session_id, instance_id).await
}

/// Whether the session takes part in her choices and the cold war at all: a
/// 1:1 text session of a genome instance (spec 2026-11-23-affinity-50 §6.5).
async fn session_eligible(
    state: &AppState,
    session_id: Uuid,
    instance_id: Uuid,
) -> Result<bool, AppError> {
    // 1:1 text only: the image-edit route also runs the affinity pass, and it
    // serves a voice session too.
    match (ChatRepo { pool: &state.pool })
        .get_session(session_id)
        .await?
    {
        Some(s) if s.channel == "text" => {}
        _ => return Ok(false),
    }

    // A genome instance only: the resolution route refuses BYOC, so an ask to
    // a BYOC session could never be answered, and a BYOC session stays outside
    // the status system.
    Ok(matches!(
        crate::byoc::load_instance(state, instance_id).await?,
        Some(LoadedInstance::Genome(_))
    ))
}

/// The cold war (spec 2026-11-23-affinity-50 §6.3, §7.4), read from the
/// judge's patience levels since the streak anchor. Off, it starts on the
/// newest three at the lowest level or five of the newest seven, and she says
/// so in one message; on, it ends on the newest three above the lowest, in
/// silence. The flag is committed before she speaks, so a failed or blank
/// message leaves it on. A pending choice or a date does not hold it back.
/// The levels decide first; [`session_eligible`] is read only when the flag
/// would move, and gates the write and the message. `Ok(true)` when the flag
/// was written.
async fn try_cold_war(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    persona: &CompanionPersona,
    affinity: &Affinity,
) -> Result<bool, AppError> {
    let now = Utc::now();
    let affinity_repo = AffinityRepo { pool: &state.pool };
    let since = (LifeChoiceRepo { pool: &state.pool })
        .streak_anchor(session_id)
        .await?;
    let levels: Vec<i16> = affinity_repo
        .last_judged_endpoints(session_id, since, COLD_WAR_WINDOW as i64)
        .await?
        .iter()
        .map(|j| j.patience_grade)
        .collect();

    let on = affinity.relationship.cold_war();
    let moves = if on {
        cold_war_ends(&levels)
    } else {
        cold_war_starts(&levels)
    };
    if !moves
        || !session_eligible(state, session_id, instance_id).await?
        || !affinity_repo.set_cold_war(session_id, !on, now).await?
    {
        return Ok(false);
    }
    if on {
        tracing::info!(%session_id, "the cold war ended");
        return Ok(true);
    }
    tracing::info!(%session_id, "a cold war started");
    match speak(
        state,
        session_id,
        user_id,
        instance_id,
        persona,
        now,
        Occasion::ColdWar,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => tracing::warn!(%session_id, "the cold war started with nothing said"),
        Err(e) => tracing::warn!(%session_id, "the cold war started, its message failed: {e}"),
    }
    Ok(true)
}

/// Her break-up ask (spec 2026-11-23-affinity-50 §7.2): a breakable status
/// in a cold war, and the newest three judged turns since the streak anchor
/// all at the judge's lowest patience level. The turn that starts a cold war
/// writes a `relationship` event, which moves the anchor, so it never asks
/// too: three more lowest turns must follow.
async fn try_break_up(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    persona: &CompanionPersona,
    affinity: &Affinity,
) -> Result<(), AppError> {
    let now = Utc::now();
    let relationship = affinity.relationship;
    if !relationship.cold_war() || !relationship.status.is_breakable() {
        return Ok(());
    }
    let since = (LifeChoiceRepo { pool: &state.pool })
        .streak_anchor(session_id)
        .await?;
    let levels: Vec<i16> = (AffinityRepo { pool: &state.pool })
        .last_judged_endpoints(session_id, since, COLD_WAR_WINDOW as i64)
        .await?
        .iter()
        .map(|j| j.patience_grade)
        .collect();
    if !wants_break_up(&levels) || !runway_clear(state, session_id, instance_id, now).await? {
        return Ok(());
    }

    let cue = LifeChoiceCue::Ask {
        kind: LifeChoiceKind::BreakUp,
        anchor_seen: since,
    };
    if speak(
        state,
        session_id,
        user_id,
        instance_id,
        persona,
        now,
        Occasion::LifeChoice(cue),
    )
    .await?
    {
        tracing::info!(%session_id, "she asked to break up");
    }
    Ok(())
}

/// Her confession (spec 2026-11-23-affinity-50 §7.2): from a status it can
/// be asked from, a compatible pair whose chemistry leads at tier
/// [`CONFESS_MIN_CHEM_TIER`] or more, the newest [`WARM_STREAK`] judged
/// turns all with warmth above [`WARM`], and [`status_ask_waits`] over.
async fn try_confess(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    persona: &CompanionPersona,
    affinity: &Affinity,
) -> Result<(), AppError> {
    let now = Utc::now();
    let relationship = affinity.relationship;
    if relationship.cold_war() || !StatusAsk::Confess.asked_from(relationship.status) {
        return Ok(());
    }
    if leading_line(affinity) != AffinityLine::Chemistry
        || affinity.chem_tier() < CONFESS_MIN_CHEM_TIER
    {
        return Ok(());
    }

    match (ChatRepo { pool: &state.pool })
        .latest_scopes(session_id)
        .await?
    {
        Some((_, scope)) if romance_compatible(scope) => {}
        _ => return Ok(()),
    }

    let since = (LifeChoiceRepo { pool: &state.pool })
        .streak_anchor(session_id)
        .await?;
    let judged = (AffinityRepo { pool: &state.pool })
        .last_judged_endpoints(session_id, since, WARM_STREAK as i64)
        .await?;
    if !newest_streak(&judged, |e| e.warmth > WARM)
        || status_ask_waits(state, session_id, LifeChoiceKind::Confess, now).await?
        || !runway_clear(state, session_id, instance_id, now).await?
    {
        return Ok(());
    }

    let cue = LifeChoiceCue::Ask {
        kind: LifeChoiceKind::Confess,
        anchor_seen: since,
    };
    if speak(
        state,
        session_id,
        user_id,
        instance_id,
        persona,
        now,
        Occasion::LifeChoice(cue),
    )
    .await?
    {
        tracing::info!(%session_id, "she confessed");
    }
    Ok(())
}

/// Her offer of a partnership (spec 2026-11-23-affinity-50 §7.2): from a
/// status it can be asked from, bond leading at tier
/// [`PARTNER_MIN_BOND_TIER`] or more, the newest [`WARM_STREAK`] judged turns
/// all with patience above [`WARM`], and [`status_ask_waits`] over. The
/// pair's scopes are not read.
async fn try_partner_up(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    persona: &CompanionPersona,
    affinity: &Affinity,
) -> Result<(), AppError> {
    let now = Utc::now();
    let relationship = affinity.relationship;
    if relationship.cold_war() || !StatusAsk::PartnerUp.asked_from(relationship.status) {
        return Ok(());
    }
    if leading_line(affinity) != AffinityLine::Bond || affinity.bond_tier() < PARTNER_MIN_BOND_TIER
    {
        return Ok(());
    }

    let since = (LifeChoiceRepo { pool: &state.pool })
        .streak_anchor(session_id)
        .await?;
    let judged = (AffinityRepo { pool: &state.pool })
        .last_judged_endpoints(session_id, since, WARM_STREAK as i64)
        .await?;
    if !newest_streak(&judged, |e| e.patience > WARM)
        || status_ask_waits(state, session_id, LifeChoiceKind::PartnerUp, now).await?
        || !runway_clear(state, session_id, instance_id, now).await?
    {
        return Ok(());
    }

    let cue = LifeChoiceCue::Ask {
        kind: LifeChoiceKind::PartnerUp,
        anchor_seen: since,
    };
    if speak(
        state,
        session_id,
        user_id,
        instance_id,
        persona,
        now,
        Occasion::LifeChoice(cue),
    )
    .await?
    {
        tracing::info!(%session_id, "she offered a partnership");
    }
    Ok(())
}

/// Whether `kind` (`confess` or `partner_up`) still waits at `now` (spec
/// 2026-11-23-affinity-50 §7.1): the session's status changed, or `kind`
/// was answered in any way, within the last [`STATUS_COOLDOWN_DAYS`].
async fn status_ask_waits(
    state: &AppState,
    session_id: Uuid,
    kind: LifeChoiceKind,
    now: DateTime<Utc>,
) -> Result<bool, AppError> {
    let cooled = now - Duration::days(STATUS_COOLDOWN_DAYS);
    let changed = (AffinityRepo { pool: &state.pool })
        .latest_status_change(session_id)
        .await?;
    Ok(changed.is_some_and(|at| at > cooled)
        || (LifeChoiceRepo { pool: &state.pool })
            .answered_since(session_id, kind, cooled)
            .await?)
}

/// The romantic trigger (spec 2026-10-30 §4.3, spec 2026-11-23-affinity-50
/// §7.2, §7.3), then her ask (§4.4) through [`speak`]: a proposal, or her
/// sleep ask in the form the status gives it. Only a pair the caller's
/// recorded scopes mark as romantic qualifies, and the ask is generated under
/// those scopes.
async fn try_romance(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    persona: &CompanionPersona,
    affinity: &Affinity,
) -> Result<(), AppError> {
    let now = Utc::now();
    // No warm ask while a cold war is on (spec 2026-11-23-affinity-50 §7.3).
    if affinity.relationship.cold_war() {
        return Ok(());
    }

    // The scopes the session's newest reply recorded; none reads as not
    // romantic.
    match (ChatRepo { pool: &state.pool })
        .latest_scopes(session_id)
        .await?
    {
        Some((_, scope)) if romance_compatible(scope) => {}
        _ => return Ok(()),
    }

    // The judged turns after the last episode of any kind, newest first.
    let choices = LifeChoiceRepo { pool: &state.pool };
    let since = choices.streak_anchor(session_id).await?;
    let judged = (AffinityRepo { pool: &state.pool })
        .last_judged_endpoints(session_id, since, PROPOSE_WINDOW)
        .await?;
    let Some(kind) = romantic_kind(
        affinity.chem_tier(),
        affinity.bond_tier(),
        affinity.relationship.status,
        &judged,
        choices
            .settled(session_id, LifeChoiceKind::SleepTogether)
            .await?,
        choices
            .settled(session_id, LifeChoiceKind::FallAsleepTogether)
            .await?,
        choices
            .settled_since(
                session_id,
                LifeChoiceKind::Propose,
                now - Duration::days(PROPOSE_COOLDOWN_DAYS),
            )
            .await?,
    ) else {
        return Ok(());
    };
    if !runway_clear(state, session_id, instance_id, now).await? {
        return Ok(());
    }

    let cue = LifeChoiceCue::Ask {
        kind,
        anchor_seen: since,
    };
    if speak(
        state,
        session_id,
        user_id,
        instance_id,
        persona,
        now,
        Occasion::LifeChoice(cue),
    )
    .await?
    {
        tracing::info!(%session_id, kind = kind.as_str(), "she made a romantic choice");
    }
    Ok(())
}

/// The kind the window qualifies for, `propose` first (spec 2026-10-30
/// §4.3, spec 2026-11-23-affinity-50 §7.1–§7.3). `endpoints` newest first:
/// the judged turns after the streak anchor, at most [`PROPOSE_WINDOW`]. A
/// turn is warm when its warmth and its patience are both above [`WARM`].
/// `propose` is asked of a couple only, not while `propose_cooling` (an
/// ignore within [`PROPOSE_COOLDOWN_DAYS`]), and needs both tiers at 4 or
/// more and [`PROPOSE_HITS`] warm turns in the window. Her sleep ask needs
/// chemistry tier 3 or more and the newest [`WARM_STREAK`] all warm; it is
/// `sleep_together` where [`sleep_ask_is_sex`] says so and
/// `fall_asleep_together` otherwise, and is not chosen once that kind is
/// settled.
fn romantic_kind(
    chem_tier: u8,
    bond_tier: u8,
    status: RelationshipStatus,
    endpoints: &[JudgedEndpoints],
    settled_sleep: bool,
    settled_fall_asleep: bool,
    propose_cooling: bool,
) -> Option<LifeChoiceKind> {
    if StatusAsk::Propose.asked_from(status)
        && !propose_cooling
        && chem_tier >= 4
        && bond_tier >= 4
        && endpoints.iter().filter(|e| is_warm(e)).count() >= PROPOSE_HITS
    {
        return Some(LifeChoiceKind::Propose);
    }
    if chem_tier < 3 || !newest_streak(endpoints, is_warm) {
        return None;
    }
    if sleep_ask_is_sex(status, chem_tier) {
        (!settled_sleep).then_some(LifeChoiceKind::SleepTogether)
    } else {
        (!settled_fall_asleep).then_some(LifeChoiceKind::FallAsleepTogether)
    }
}

fn is_warm(e: &JudgedEndpoints) -> bool {
    e.warmth > WARM && e.patience > WARM
}

/// `endpoints` newest first: the newest [`WARM_STREAK`] all pass `pred`.
fn newest_streak(endpoints: &[JudgedEndpoints], pred: impl Fn(&JudgedEndpoints) -> bool) -> bool {
    endpoints
        .get(..WARM_STREAK)
        .is_some_and(|newest| newest.iter().all(pred))
}

/// Whether the pair qualifies for a bath together: chemistry tier 2 or more
/// for a hygienic persona, 3 or more for one with no `hygienic`, never for
/// one who is not hygienic; and [`BATHE_HITS`] of `endpoints` (the newest
/// judged turns, at most [`BATHE_WINDOW`]) at [`BATHE_WARMTH`] or more.
fn bathe_qualifies(chem_tier: u8, hygienic: Option<bool>, endpoints: &[JudgedEndpoints]) -> bool {
    let floor = match hygienic {
        Some(true) => 2,
        None => 3,
        Some(false) => return false,
    };
    chem_tier >= floor
        && endpoints
            .iter()
            .filter(|e| e.warmth >= BATHE_WARMTH)
            .count()
            >= BATHE_HITS
}

/// She asks the user to take a bath with her: a romance-compatible pair that
/// [`bathe_qualifies`], not within [`BATHE_COOLDOWN_DAYS`] of the last
/// accept or ignore. Its own trigger, independent of the bed: it runs after
/// the romantic trigger, whose pending ask closes the runway.
async fn try_bathe(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    persona: &CompanionPersona,
    affinity: &Affinity,
) -> Result<(), AppError> {
    let now = Utc::now();
    if affinity.relationship.cold_war() {
        return Ok(());
    }

    match (ChatRepo { pool: &state.pool })
        .latest_scopes(session_id)
        .await?
    {
        Some((_, scope)) if romance_compatible(scope) => {}
        _ => return Ok(()),
    }

    let choices = LifeChoiceRepo { pool: &state.pool };
    let since = choices.streak_anchor(session_id).await?;
    let judged = (AffinityRepo { pool: &state.pool })
        .last_judged_endpoints(session_id, since, BATHE_WINDOW)
        .await?;
    let hygienic = crate::prompt::persona_traits(persona).hygienic;
    if !bathe_qualifies(affinity.chem_tier(), hygienic, &judged) {
        return Ok(());
    }
    let cooled = now - Duration::days(BATHE_COOLDOWN_DAYS);
    if choices
        .settled_since(session_id, LifeChoiceKind::BatheTogether, cooled)
        .await?
        || !runway_clear(state, session_id, instance_id, now).await?
    {
        return Ok(());
    }

    let cue = LifeChoiceCue::Ask {
        kind: LifeChoiceKind::BatheTogether,
        anchor_seen: since,
    };
    if speak(
        state,
        session_id,
        user_id,
        instance_id,
        persona,
        now,
        Occasion::LifeChoice(cue),
    )
    .await?
    {
        tracing::info!(%session_id, "she asked for a bath together");
    }
    Ok(())
}

fn travel_qualifies(bond_tier: u8, endpoints: &[JudgedEndpoints]) -> bool {
    bond_tier >= TRAVEL_MIN_BOND_TIER && newest_streak(endpoints, is_warm)
}

/// She suggests going away together: a close, warm stretch, whatever the
/// pair's scopes. Not while a background is under way, and not within
/// [`TRAVEL_COOLDOWN_DAYS`] of the last accept or ignore. It runs after the
/// romantic trigger, whose pending ask closes the runway.
async fn try_travel(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    persona: &CompanionPersona,
    affinity: &Affinity,
) -> Result<(), AppError> {
    let now = Utc::now();
    if affinity.relationship.cold_war() {
        return Ok(());
    }

    let choices = LifeChoiceRepo { pool: &state.pool };
    let since = choices.streak_anchor(session_id).await?;
    let judged = (AffinityRepo { pool: &state.pool })
        .last_judged_endpoints(session_id, since, WARM_STREAK as i64)
        .await?;
    if !travel_qualifies(affinity.bond_tier(), &judged) {
        return Ok(());
    }
    let cooled = now - Duration::days(TRAVEL_COOLDOWN_DAYS);
    if choices
        .settled_since(session_id, LifeChoiceKind::TravelTogether, cooled)
        .await?
    {
        return Ok(());
    }
    let background = (EventRepo { pool: &state.pool })
        .current(instance_id, None, now)
        .await?
        .background;
    if background.is_some() || !runway_clear(state, session_id, instance_id, now).await? {
        return Ok(());
    }

    let cue = LifeChoiceCue::Ask {
        kind: LifeChoiceKind::TravelTogether,
        anchor_seen: since,
    };
    if speak(
        state,
        session_id,
        user_id,
        instance_id,
        persona,
        now,
        Occasion::LifeChoice(cue),
    )
    .await?
    {
        tracing::info!(%session_id, "she suggested going away together");
    }
    Ok(())
}

/// Her words on `occasion` through the proactive path (spec 2026-10-30 §4.4,
/// spec 2026-11-23-affinity-50 §7.4): an ask is written with its choice row
/// beside it, under the scopes the pair's replies recorded (a pair with none
/// reads as romance-compatible); the cold-war message is written on its own,
/// under the engine's default scopes. `true` when it was written. A reply that is blank,
/// strips to nothing or fails writes nothing: an ask is made again on a later
/// qualifying turn; the cold-war message is not retried, its flag already
/// committed.
async fn speak(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    persona: &CompanionPersona,
    now: DateTime<Utc>,
    occasion: Occasion,
) -> Result<bool, AppError> {
    // The turn's own request fields are not carried here; the locale and the
    // tier are read back from the user row that recorded them.
    let latest = (ChatRepo { pool: &state.pool })
        .latest_user_message(session_id)
        .await?;
    let raw = |key: &str| {
        latest
            .as_ref()
            .and_then(|m| m.metadata.as_ref())
            .and_then(|m| m.get(key))
            .and_then(|v| v.as_str())
    };
    let (timezone, country, region) = (
        raw("user_timezone"),
        raw("user_country"),
        raw("user_region"),
    );
    let mut metadata = serde_json::Map::new();
    crate::holiday::record_raw(&mut metadata, timezone, country, region);
    let locale = UserLocale::resolve(timezone, country, region);
    let tier = raw("tier").map(String::from);
    let recall_query = latest.as_ref().map(recall_query_text).unwrap_or_default();
    // Her ask is written under the scopes the pair's replies recorded; a pair
    // with none recorded reads as romance-compatible. The cold-war message
    // keeps the engine's defaults.
    let (memory_scope, affinity_scope) = match occasion {
        Occasion::LifeChoice(_) => (ChatRepo { pool: &state.pool })
            .latest_scopes(session_id)
            .await?
            .unwrap_or((MemoryScope::default(), AffinityScope::full())),
        _ => Default::default(),
    };

    let spoken = greet(
        state,
        Greeting {
            session_id,
            user_id,
            instance_id,
            persona: persona.clone(),
            now: NowContext::for_user(&locale, now),
            occasion,
            recall_query,
            since: now,
            tier,
            prompt_traits: Vec::new(),
            memory_scope,
            affinity_scope,
            audit: None,
            metadata,
            message_id: None,
        },
    )
    .await?;
    Ok(spoken.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Duration, TimeZone, Utc};
    use eros_engine_core::events::EventKind;
    use eros_engine_core::scope::AffinityScope;
    use eros_engine_core::types::ActionType;
    use eros_engine_store::events::{EventRepo, NewEvent};
    use eros_engine_store::life_choices::{LifeChoiceKind, LifeChoiceRepo};
    use eros_engine_store::persona::PersonaRepo;
    use serde_json::json;
    use sqlx::PgPool;
    use std::sync::Arc;
    use wiremock::matchers::{body_string_contains, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::pipeline::post_process::run_affinity_turn;
    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{
        seed_byoc_instance_with_key, seed_genome, seed_instance, seed_session,
    };

    const ASK_TEXT: &str = "我想先分开一阵子，你能给我一个答复吗。";
    /// Her cold-war message, the cue that is the request's last `user` turn.
    const COLD_WAR_CUE: &str =
        "（你对对方的耐心被磨光了。你决定不再给对方好脸色，告诉对方你现在不想好好说话。）";
    const COLD_TEXT: &str = "我现在不想好好说话。";
    const SEEN: &str = "今天过得怎么样呀";

    /// The judge's verdict: nothing moved, patience at the bottom level.
    const LOW_PATIENCE: &str = r#"{"warmth":2,"trust":{"grade":0,"direction":"up"},"intimacy":{"grade":0,"direction":"up"},"intrigue":{"grade":0,"direction":"up"},"tension":{"grade":0,"direction":"up"},"patience":1,"reason":"累了"}"#;
    /// Nothing moved, patience at the top level.
    const PATIENT: &str = r#"{"warmth":2,"trust":{"grade":0,"direction":"up"},"intimacy":{"grade":0,"direction":"up"},"intrigue":{"grade":0,"direction":"up"},"tension":{"grade":0,"direction":"up"},"patience":3,"reason":"耐心还很足"}"#;
    /// A verdict that moves trust up a clear step, patience at the usual level.
    const TRUST_UP: &str = r#"{"warmth":2,"trust":{"grade":3,"direction":"up"},"intimacy":{"grade":0,"direction":"up"},"intrigue":{"grade":0,"direction":"up"},"tension":{"grade":0,"direction":"up"},"patience":2,"reason":"暖了"}"#;

    fn with_models(mut state: AppState, mock_uri: &str) -> AppState {
        state.model_config = Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"companion\"\n\
                 [tasks.affinity_evaluation]\nmodel = \"judge\"\n",
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

    /// A mock that judges every turn with `verdict` and, for the persona's
    /// own call ("you are a companion"), answers `companion`: `Some(text)` as
    /// her reply, `None` as a provider failure.
    async fn mock_llm(verdict: &str, companion: Option<&str>) -> MockServer {
        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("你就是对话里的这个角色"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "gen-judge",
                "model": "served/judge",
                "choices": [{"message": {"content": verdict}}],
            })))
            .mount(&mock)
            .await;
        let reply = match companion {
            Some(text) => ResponseTemplate::new(200).set_body_json(json!({
                "id": "gen-ask",
                "model": "served/companion",
                "choices": [{"message": {"content": text}}],
            })),
            None => ResponseTemplate::new(500).set_body_string("provider down"),
        };
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("you are a companion"))
            .respond_with(reply)
            .mount(&mock)
            .await;
        mock
    }

    /// Requests that reached her, as JSON bodies.
    async fn companion_calls(mock: &MockServer) -> Vec<serde_json::Value> {
        mock.received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| String::from_utf8_lossy(&r.body).contains("you are a companion"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    struct Scene {
        pool: PgPool,
        user_id: Uuid,
        instance_id: Uuid,
        session_id: Uuid,
        persona: CompanionPersona,
    }

    /// A text session whose chemistry is far above its bond: bond 0.0 (tier 1),
    /// chemistry 0.70 (tier 4), a gap of 3.
    async fn scene(pool: &PgPool) -> Scene {
        scene_with_lines(pool, 0.0, 0.7).await
    }

    async fn scene_with_lines(pool: &PgPool, bond: f64, chem: f64) -> Scene {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(pool, "Aria").await;
        let instance_id = seed_instance(pool, genome_id, user_id).await;
        let session_id = session_with_lines(pool, user_id, instance_id, bond, chem).await;
        let persona = PersonaRepo { pool }
            .load_companion(instance_id)
            .await
            .unwrap()
            .expect("the persona");
        Scene {
            pool: pool.clone(),
            user_id,
            instance_id,
            session_id,
            persona,
        }
    }

    async fn lopsided_session(pool: &PgPool, user_id: Uuid, instance_id: Uuid) -> Uuid {
        session_with_lines(pool, user_id, instance_id, 0.0, 0.7).await
    }

    /// A text session whose bond line sits at `bond` and chemistry line at `chem`.
    async fn session_with_lines(
        pool: &PgPool,
        user_id: Uuid,
        instance_id: Uuid,
        bond: f64,
        chem: f64,
    ) -> Uuid {
        let session_id = seed_session(pool, user_id, instance_id).await;
        eros_engine_store::affinity::AffinityRepo { pool }
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE engine.companion_affinity \
                SET trust = $2, intrigue = $2, intimacy = $3, tension = $3, updated_at = now() \
              WHERE session_id = $1",
        )
        .bind(session_id)
        .bind(bond)
        .bind(chem)
        .execute(pool)
        .await
        .unwrap();
        session_id
    }

    impl Scene {
        /// One 1:1 text turn in `session`, through the whole affinity pass.
        /// No ignore conversion runs: as far as a pending ask is concerned
        /// this is an action turn, which does not answer her.
        async fn turn_in(&self, state: &AppState, session_id: Uuid, text: &str) {
            let umid = self.user_row(session_id, text).await;
            self.affinity_pass(state, session_id, text, umid).await;
        }

        /// The user's row of a turn, as the stream persists it.
        async fn user_row(&self, session_id: Uuid, text: &str) -> Uuid {
            sqlx::query_scalar(
                "INSERT INTO engine.chat_messages (session_id, role, content) \
                 VALUES ($1, 'user', $2) RETURNING id",
            )
            .bind(session_id)
            .bind(text)
            .fetch_one(&self.pool)
            .await
            .unwrap()
        }

        /// An ordinary text turn in the scene's own session: the ignore
        /// conversion first, as the stream runs it, then the affinity pass.
        async fn converting_turn(&self, state: &AppState, text: &str) {
            let umid = self.user_row(self.session_id, text).await;
            convert_ignored_ask(state, self.session_id, self.instance_id, umid)
                .await
                .unwrap();
            self.affinity_pass(state, self.session_id, text, umid).await;
        }

        async fn affinity_pass(&self, state: &AppState, session_id: Uuid, text: &str, umid: Uuid) {
            run_affinity_turn(
                state,
                session_id,
                self.user_id,
                self.instance_id,
                &self.persona,
                ActionType::ReplyText,
                text,
                "嗯，我在听。",
                Default::default(),
                Default::default(),
                Some(umid),
                None,
                false,
                None,
                None,
                false,
            )
            .await;
        }

        async fn turn(&self, state: &AppState, text: &str) {
            self.turn_in(state, self.session_id, text).await;
        }

        /// (status, tier_gap, message_id) of every choice in the session.
        async fn choices(&self) -> Vec<(String, Option<i16>, Option<Uuid>)> {
            self.choices_in(self.session_id).await
        }

        async fn choices_in(&self, session_id: Uuid) -> Vec<(String, Option<i16>, Option<Uuid>)> {
            sqlx::query_as(
                "SELECT status, tier_gap, message_id FROM engine.life_choices \
                  WHERE session_id = $1 ORDER BY asked_at",
            )
            .bind(session_id)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// Every assistant row of the session written as a life choice.
        async fn ask_rows(&self) -> Vec<(Uuid, String, Option<String>)> {
            sqlx::query_as(
                "SELECT id, content, assistant_action_type FROM engine.chat_messages \
                  WHERE session_id = $1 AND role = 'assistant' \
                    AND metadata->>'proactive' = 'life_choice'",
            )
            .bind(self.session_id)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// A choice asked `ago` ago, in `status`.
        async fn seed_choice(&self, status: &str, ago: Duration) {
            sqlx::query(
                "INSERT INTO engine.life_choices (session_id, kind, status, tier_gap, asked_at, \
                                                  resolved_at) \
                 VALUES ($1, 'ask_for_space', $2, 3, now() - make_interval(secs => $3), \
                         CASE WHEN $2 = 'pending' THEN NULL \
                              ELSE now() - make_interval(secs => $3) + interval '1 minute' END)",
            )
            .bind(self.session_id)
            .bind(status)
            .bind(ago.num_seconds() as f64)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        /// The status and the cold-war flag, written directly: no
        /// `relationship` event, so the streak anchor stays where it was.
        async fn set_status(&self, status: &str, cold_war: bool) {
            sqlx::query(
                "UPDATE engine.companion_affinity \
                    SET relationship_status = $2, \
                        cold_war_since = CASE WHEN $3 THEN now() END \
                  WHERE session_id = $1",
            )
            .bind(self.session_id)
            .bind(status)
            .bind(cold_war)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        /// Whether the session's cold war is on.
        async fn cold_war(&self) -> bool {
            sqlx::query_scalar(
                "SELECT cold_war_since IS NOT NULL FROM engine.companion_affinity \
                  WHERE session_id = $1",
            )
            .bind(self.session_id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
        }

        /// The `cold_war` field of the session's `relationship` events,
        /// oldest first.
        async fn cold_war_events(&self) -> Vec<Option<String>> {
            sqlx::query_scalar(
                "SELECT e.context->>'cold_war' FROM engine.companion_affinity_events e \
                   JOIN engine.companion_affinity a ON a.id = e.affinity_id \
                  WHERE a.session_id = $1 AND e.event_type = 'relationship' \
                  ORDER BY e.created_at, e.id",
            )
            .bind(self.session_id)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// (content, assistant_action_type) of her cold-war messages.
        async fn cold_war_rows(&self) -> Vec<(String, Option<String>)> {
            sqlx::query_as(
                "SELECT content, assistant_action_type FROM engine.chat_messages \
                  WHERE session_id = $1 AND role = 'assistant' \
                    AND metadata->>'proactive' = 'cold_war'",
            )
            .bind(self.session_id)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }
    }

    fn proactive(content: &str) -> (String, Option<String>) {
        (content.into(), Some("proactive".into()))
    }

    // ---- The cold war (spec 2026-11-23-affinity-50 §6.3, §7.4) ----

    /// Three judged turns at the judge's lowest patience level start a cold
    /// war at any tiers: the flag, its `relationship` event, and her one
    /// message about it, with no choice row.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn three_lowest_patience_turns_start_a_cold_war_and_she_says_so(pool: PgPool) {
        // Equal tiers: no gap is needed.
        let s = scene_with_lines(&pool, 0.45, 0.45).await;
        let mock = mock_llm(LOW_PATIENCE, Some(COLD_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        s.turn(&state, SEEN).await;
        s.turn(&state, SEEN).await;
        assert!(!s.cold_war().await, "two judged turns are not three");
        assert!(s.cold_war_events().await.is_empty());
        assert!(companion_calls(&mock).await.is_empty(), "and cost nothing");

        s.turn(&state, SEEN).await;
        let (bond, chem) = s.tiers().await;
        assert_eq!(bond, chem, "equal tiers");
        assert!(s.cold_war().await);
        assert_eq!(s.cold_war_events().await, vec![Some("entered".into())]);
        assert!(s.choices().await.is_empty(), "no choice row");
        assert_eq!(s.cold_war_rows().await, vec![proactive(COLD_TEXT)]);
        assert_eq!(only_cue(&mock).await, json!(COLD_WAR_CUE));

        // The turns' own affinity events are untouched: three judged turns.
        let events: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.companion_affinity_events e \
               JOIN engine.companion_affinity a ON a.id = e.affinity_id \
              WHERE a.session_id = $1 AND e.event_type = 'message'",
        )
        .bind(s.session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(events, 3);

        // A fourth lowest turn: the cold war is already on, nothing more.
        s.turn(&state, SEEN).await;
        assert_eq!(s.cold_war_events().await.len(), 1);
        assert_eq!(companion_calls(&mock).await.len(), 1);
    }

    /// A failed or blank message leaves the cold war on, and she does not try
    /// again on the next turn.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_blank_cold_war_message_keeps_the_cold_war(pool: PgPool) {
        let blank = mock_llm(LOW_PATIENCE, Some("  ")).await;
        let failing = mock_llm(LOW_PATIENCE, None).await;
        for mock in [&blank, &failing] {
            let s = scene(&pool).await;
            let state = with_models(test_state(pool.clone()), &mock.uri());
            for _ in 0..4 {
                s.turn(&state, SEEN).await;
            }
            assert!(s.cold_war().await);
            assert_eq!(s.cold_war_events().await, vec![Some("entered".into())]);
            assert!(s.cold_war_rows().await.is_empty(), "no message");
            assert_eq!(companion_calls(mock).await.len(), 1, "one try");
        }
    }

    /// Three judged turns above the lowest level end the cold war, in
    /// silence.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn three_turns_above_the_lowest_end_the_cold_war(pool: PgPool) {
        let s = scene(&pool).await;
        s.set_status("none", true).await;
        let mock = mock_llm(PATIENT, Some(COLD_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        s.turn(&state, SEEN).await;
        s.turn(&state, SEEN).await;
        assert!(s.cold_war().await, "two turns are not three");

        s.turn(&state, SEEN).await;
        assert!(!s.cold_war().await);
        assert_eq!(s.cold_war_events().await, vec![Some("ended".into())]);
        assert!(s.cold_war_rows().await.is_empty(), "no message");
        assert!(companion_calls(&mock).await.is_empty());
    }

    /// A cold war is not a choice: a pending one, or a date under way, does
    /// not hold it back.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_pending_choice_or_a_date_does_not_hold_a_cold_war_back(pool: PgPool) {
        use crate::routes::companion::testutil::seed_date;

        let mock = mock_llm(LOW_PATIENCE, Some(COLD_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        let waiting = scene(&pool).await;
        waiting.seed_choice("pending", Duration::hours(1)).await;
        let dating = scene(&pool).await;
        seed_date(
            &pool,
            dating.session_id,
            dating.user_id,
            dating.instance_id,
            Utc::now(),
        )
        .await;

        for s in [&waiting, &dating] {
            for _ in 0..3 {
                s.turn(&state, SEEN).await;
            }
            assert!(s.cold_war().await);
            assert_eq!(s.cold_war_rows().await, vec![proactive(COLD_TEXT)]);
        }
        assert_eq!(
            waiting.choices().await[0].0,
            "pending",
            "the choice still waits"
        );
        assert_eq!(companion_calls(&mock).await.len(), 2);
    }

    /// No warm ask while a cold war is on (spec 2026-11-23-affinity-50
    /// §7.3). A warm turn is above the lowest patience level, so three of
    /// them end a cold war: the turns run while a sibling session's pending
    /// ask closes the runway, and each check is then called on its own.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn no_warm_ask_during_a_cold_war(pool: PgPool) {
        let mock = mock_llm(WARM, Some(SLEEP_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        for kind in [
            LifeChoiceKind::SleepTogether,
            LifeChoiceKind::BatheTogether,
            LifeChoiceKind::TravelTogether,
        ] {
            // A compatible warm couple at tiers 3/3: each of the three would ask.
            let s = scene_with_lines(&pool, 0.5, 0.5).await;
            s.record_scopes("full", AffinityScope::chemistry()).await;
            s.set_status("dating", false).await;
            let sibling = seed_session(&pool, s.user_id, s.instance_id).await;
            s.pending_in(sibling, Utc::now() - Duration::minutes(1))
                .await;
            for _ in 0..3 {
                s.turn(&state, SEEN).await;
            }
            assert!(s.kinds().await.is_empty(), "the runway was closed");
            sqlx::query(
                "UPDATE engine.life_choices SET status = 'declined', resolved_at = now() \
                  WHERE session_id = $1",
            )
            .bind(sibling)
            .execute(&pool)
            .await
            .unwrap();

            let check = |s: &Scene| {
                let (state, persona) = (&state, s.persona.clone());
                let (session_id, user_id, instance_id) = (s.session_id, s.user_id, s.instance_id);
                async move {
                    let a = &load_decayed(state, session_id).await.unwrap().expect("row");
                    match kind {
                        LifeChoiceKind::SleepTogether => {
                            try_romance(state, session_id, user_id, instance_id, &persona, a).await
                        }
                        LifeChoiceKind::BatheTogether => {
                            try_bathe(state, session_id, user_id, instance_id, &persona, a).await
                        }
                        _ => try_travel(state, session_id, user_id, instance_id, &persona, a).await,
                    }
                    .unwrap()
                }
            };
            s.set_status("dating", true).await;
            check(&s).await;
            assert!(s.kinds().await.is_empty(), "{kind:?} during a cold war");

            s.set_status("dating", false).await;
            check(&s).await;
            assert_eq!(
                s.kinds().await,
                vec![pending(kind.as_str())],
                "{kind:?} once it is over"
            );
        }
        assert_eq!(
            companion_calls(&mock).await.len(),
            3,
            "the three asks after it, none during"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_unjudged_turn_neither_counts_nor_breaks(pool: PgPool) {
        let s = scene(&pool).await;
        let mock = mock_llm(LOW_PATIENCE, Some(COLD_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        s.turn(&state, SEEN).await;
        s.turn(&state, "hi").await; // too short to judge
        s.turn(&state, SEEN).await;
        assert!(
            !s.cold_war().await,
            "the skipped turn is not the third lowest turn"
        );
        let skipped: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.companion_affinity_events e \
               JOIN engine.companion_affinity a ON a.id = e.affinity_id \
              WHERE a.session_id = $1 AND e.context->>'eval_skip_reason' = 'short_user_msg'",
        )
        .bind(s.session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(skipped, 1);

        s.turn(&state, SEEN).await;
        assert!(
            s.cold_war().await,
            "lowest, skipped, lowest, lowest starts it"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn turns_before_the_last_ask_do_not_count(pool: PgPool) {
        let s = scene(&pool).await;
        let mock = mock_llm(LOW_PATIENCE, Some(COLD_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        s.turn(&state, SEEN).await;
        s.turn(&state, SEEN).await;
        // An ask came and went after them: both turns are older than it.
        sqlx::query(
            "UPDATE engine.companion_affinity_events \
                SET created_at = created_at - interval '2 hours'",
        )
        .execute(&pool)
        .await
        .unwrap();
        s.seed_choice("declined", Duration::hours(1)).await;

        s.turn(&state, SEEN).await;
        assert!(!s.cold_war().await, "one fresh turn is not a streak");
        assert!(companion_calls(&mock).await.is_empty());

        s.turn(&state, SEEN).await;
        assert!(!s.cold_war().await, "two fresh turns are not either");
        s.turn(&state, SEEN).await;
        assert!(s.cold_war().await, "three fresh turns start it");
        assert_eq!(companion_calls(&mock).await.len(), 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn no_ask_outside_a_text_session(pool: PgPool) {
        let s = scene(&pool).await;
        sqlx::query("UPDATE engine.chat_sessions SET channel = 'voice' WHERE id = $1")
            .bind(s.session_id)
            .execute(&pool)
            .await
            .unwrap();
        let mock = mock_llm(LOW_PATIENCE, Some(ASK_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        for _ in 0..3 {
            s.turn(&state, SEEN).await;
        }
        assert!(s.choices().await.is_empty());
        assert!(!s.cold_war().await, "nor a cold war");
        assert!(companion_calls(&mock).await.is_empty());
    }

    /// A BYOC session stays outside the status system (spec
    /// 2026-11-23-affinity-50 §6.5): the turns that would start a cold war
    /// anywhere else start none, and she says nothing.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_byoc_session_never_enters_a_cold_war(pool: PgPool) {
        let mut s = scene(&pool).await;
        let (instance_id, session_id) = {
            let (instance_id, _) = seed_byoc_instance_with_key(&pool, s.user_id).await;
            let session_id = session_with_lines(&pool, s.user_id, instance_id, 0.0, 0.7).await;
            (instance_id, session_id)
        };
        // The in-memory card a BYOC turn runs on has no genome behind it.
        s.persona.instance_id = instance_id;
        s.persona.instance.id = instance_id;
        s.persona.instance.genome_id = None;
        s.persona.genome.id = None;
        s.instance_id = instance_id;
        s.session_id = session_id;

        let mock = mock_llm(LOW_PATIENCE, Some(COLD_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        for _ in 0..4 {
            s.turn(&state, SEEN).await;
        }
        assert!(!s.cold_war().await);
        assert!(s.cold_war_events().await.is_empty());
        assert!(s.cold_war_rows().await.is_empty());
        assert!(s.choices().await.is_empty());
        assert!(s.ask_rows().await.is_empty());
        assert!(companion_calls(&mock).await.is_empty());
    }

    /// A judged turn above the lowest level is in the newest three, so the
    /// streak rule waits for three lowest ones after it; with fewer than five
    /// lowest among the newest seven, the other rule does not start it either.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_turn_above_the_lowest_breaks_the_streak(pool: PgPool) {
        let s = scene(&pool).await;
        let low = mock_llm(LOW_PATIENCE, Some(COLD_TEXT)).await;
        let high = mock_llm(PATIENT, Some(COLD_TEXT)).await;
        let low_state = with_models(test_state(pool.clone()), &low.uri());
        let high_state = with_models(test_state(pool.clone()), &high.uri());

        s.turn(&low_state, SEEN).await;
        s.turn(&high_state, SEEN).await;
        s.turn(&low_state, SEEN).await;
        s.turn(&low_state, SEEN).await;
        assert!(!s.cold_war().await, "low, high, low, low");
        s.turn(&low_state, SEEN).await;
        assert!(s.cold_war().await, "high, low, low, low starts it");
    }

    /// Five of the newest seven at the lowest level start a cold war with no
    /// three in a row: low, low, high, low, low, high, low starts it on the
    /// seventh judged turn and not before.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn five_lowest_of_seven_start_a_cold_war(pool: PgPool) {
        let s = scene(&pool).await;
        let low = mock_llm(LOW_PATIENCE, Some(COLD_TEXT)).await;
        let high = mock_llm(PATIENT, Some(COLD_TEXT)).await;
        let low_state = with_models(test_state(pool.clone()), &low.uri());
        let high_state = with_models(test_state(pool.clone()), &high.uri());

        let turns = [true, true, false, true, true, false, true]; // lowest?
        for (i, lowest) in turns.into_iter().enumerate() {
            let state = if lowest { &low_state } else { &high_state };
            s.turn(state, SEEN).await;
            if i < 6 {
                assert!(!s.cold_war().await, "not after turn {}", i + 1);
            }
        }
        assert!(s.cold_war().await, "the seventh starts it");
        assert_eq!(s.cold_war_events().await, vec![Some("entered".into())]);
        assert_eq!(s.cold_war_rows().await, vec![proactive(COLD_TEXT)]);
    }

    /// A blank, empty or failed ask writes neither her words nor the choice
    /// row, so the next qualifying turn asks again.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_blank_ask_writes_nothing(pool: PgPool) {
        // A couple at tiers 3/3 asked to sleep together: not hygienic, so no
        // bath, and away on a trip of her own, so no trip together.
        let mut s = scene_with_lines(&pool, 0.5, 0.5).await;
        s.set_art(json!({"hygienic": false})).await;
        s.record_scopes("full", AffinityScope::chemistry()).await;
        s.set_status("dating", false).await;
        s.event_under_way(EventKind::ShortTrip).await;
        // A reply the model leaves blank, an empty one, and one the provider
        // fails.
        let blank = mock_llm(WARM, Some("  ")).await;
        let empty = mock_llm(WARM, Some("")).await;
        let failing = mock_llm(WARM, None).await;
        let spoken = mock_llm(WARM, Some(SLEEP_TEXT)).await;
        let blank_state = with_models(test_state(pool.clone()), &blank.uri());
        let empty_state = with_models(test_state(pool.clone()), &empty.uri());
        let failing_state = with_models(test_state(pool.clone()), &failing.uri());
        let spoken_state = with_models(test_state(pool.clone()), &spoken.uri());

        s.turn(&blank_state, SEEN).await;
        s.turn(&blank_state, SEEN).await;
        s.turn(&blank_state, SEEN).await;
        assert_eq!(companion_calls(&blank).await.len(), 1, "she was asked");
        assert!(s.choices().await.is_empty(), "blank: no choice row");
        assert!(s.ask_rows().await.is_empty(), "blank: no message");

        s.turn(&empty_state, SEEN).await;
        assert_eq!(companion_calls(&empty).await.len(), 1);
        assert!(s.choices().await.is_empty(), "empty: no choice row");
        assert!(s.ask_rows().await.is_empty(), "empty: no message");

        s.turn(&failing_state, SEEN).await;
        assert_eq!(companion_calls(&failing).await.len(), 1);
        assert!(s.choices().await.is_empty(), "failed: no choice row");
        assert!(s.ask_rows().await.is_empty(), "failed: no message");

        // Nothing was consumed: the next qualifying turn asks again.
        s.turn(&spoken_state, SEEN).await;
        assert_eq!(s.choices().await.len(), 1);
        assert_eq!(s.ask_rows().await.len(), 1);
    }

    /// An accepted choice whose separation ended at `ends`, in `session`.
    async fn accepted_separation(s: &Scene, session_id: Uuid, ends: DateTime<Utc>) {
        let repo = LifeChoiceRepo { pool: &s.pool };
        let message = eros_engine_store::chat::ProactiveInsert {
            id: Uuid::new_v4(),
            content: "我想先分开一阵子".into(),
            generation_id: None,
            metadata: json!({"proactive": "life_choice"}),
            llm_attempts: None,
            gateway_errors: None,
        };
        let choice = repo
            .insert_ask(
                session_id,
                LifeChoiceKind::AskForSpace,
                Some(3),
                None,
                &message,
            )
            .await
            .unwrap()
            .expect("written");
        repo.resolve(choice.id, "accepted", choice.asked_at)
            .await
            .unwrap()
            .expect("resolved");
        let event = EventRepo { pool: &s.pool }
            .insert_preempting(&NewEvent {
                instance_id: s.instance_id,
                session_id,
                event_key: &format!("apart-{session_id}"),
                kind: EventKind::TakingSpace,
                started_at: ends - Duration::hours(18),
                ends_at: ends,
            })
            .await
            .unwrap();
        repo.set_event(choice.id, event.id).await.unwrap();
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn growth_scale_is_half_in_a_cold_war_in_that_session_only(pool: PgPool) {
        let s = scene(&pool).await;
        let sibling = seed_session(&pool, s.user_id, s.instance_id).await;
        let state = test_state(pool.clone());
        let affinity = AffinityRepo { pool: &pool };
        let at = Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap();
        // A sibling session's own row, so its flag could be read.
        affinity
            .load_or_create(sibling, s.user_id, s.instance_id)
            .await
            .unwrap();

        let scale = |session| {
            let state = state.clone();
            async move { growth_scale_for(&state, session).await }
        };
        assert_eq!(scale(s.session_id).await, 1.0, "no cold war");

        affinity.set_cold_war(s.session_id, true, at).await.unwrap();
        assert_eq!(scale(s.session_id).await, 0.5, "the cold war is on");
        // The cost is that relationship's, not the instance's.
        assert_eq!(scale(sibling).await, 1.0);

        affinity
            .set_cold_war(s.session_id, false, at)
            .await
            .unwrap();
        assert_eq!(scale(s.session_id).await, 1.0, "it ended: full speed");
        // A session with no affinity row grows at full speed too.
        assert_eq!(scale(Uuid::new_v4()).await, 1.0);
    }

    /// The cold war is wired into the turn's commit: the same verdict lands
    /// half as much inside it, and only in the session that is in one.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn growth_in_a_cold_war_lands_halved_through_the_turn(pool: PgPool) {
        let s = scene(&pool).await;
        let mock = mock_llm(TRUST_UP, Some(ASK_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        let twin = lopsided_session(&pool, s.user_id, s.instance_id).await;
        (AffinityRepo { pool: &pool })
            .set_cold_war(s.session_id, true, Utc::now() - Duration::hours(1))
            .await
            .unwrap();

        s.turn_in(&state, s.session_id, SEEN).await;
        s.turn_in(&state, twin, SEEN).await;

        let trust = |session: Uuid| {
            let pool = pool.clone();
            async move {
                let (eff, ctx): (serde_json::Value, serde_json::Value) = sqlx::query_as(
                    "SELECT e.effective_deltas, e.context \
                       FROM engine.companion_affinity_events e \
                       JOIN engine.companion_affinity a ON a.id = e.affinity_id \
                      WHERE a.session_id = $1 AND e.event_type = 'message'",
                )
                .bind(session)
                .fetch_one(&pool)
                .await
                .unwrap();
                (eff["trust"].as_f64().unwrap(), ctx)
            }
        };
        let (slowed, slowed_ctx) = trust(s.session_id).await;
        let (full, full_ctx) = trust(twin).await;
        assert!(full > 0.0, "the verdict moves trust: {full}");
        assert!((slowed - full / 2.0).abs() < 1e-9, "{slowed} vs {full}");
        assert_eq!(slowed_ctx["growth_scale"], json!(0.5));
        assert!(full_ctx.get("growth_scale").is_none(), "{full_ctx}");
    }

    // ---- The ignore conversion (spec §3.4c, §3.4d) ----

    /// The turn's judge verdict: an ordinary reply.
    const TURN_VERDICT: &str = r#"{"action": "reply_text", "inner_state": ""}"#;
    /// The falling-out as the judge and the reply read it.
    const FELL_OUT_LABEL: &str = "为分开的事闹得不愉快";

    /// A whole turn's providers: the judge ("pde/judge") and her reply
    /// ("deepseek/x"). Returns the state wired to them and the mock.
    async fn turn_env(pool: &PgPool) -> (Arc<AppState>, MockServer) {
        let mock = MockServer::start().await;
        let judge = json!({
            "id": "gj", "model": "pde/judge",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": TURN_VERDICT}}],
        });
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("pde/judge"))
            .respond_with(crate::pipeline::reasoning_call::test_support::sse_completion(judge))
            .mount(&mock)
            .await;
        let reply = "data: {\"choices\":[{\"delta\":{\"content\":\"嗯\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"id\":\"g\",\"model\":\"deepseek/x\"}\n\ndata: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("deepseek/x"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(reply, "text/event-stream"),
            )
            .mount(&mock)
            .await;
        let mut state = test_state(pool.clone());
        state.model_config = Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"deepseek/x\"\n\
                 [tasks.pde_decision]\nmodel = \"pde/judge\"\nfilter_prompt = \"Decide.\"\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        (Arc::new(state), mock)
    }

    /// A request body the judge or the reply received, by the model it names.
    async fn request_to(mock: &MockServer, model: &str) -> Vec<String> {
        mock.received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .filter(|b| b.contains(model))
            .collect()
    }

    /// A time in the recent past, whole microseconds, so what Postgres stores
    /// is what the test compares.
    fn recent(minutes_ago: i64) -> DateTime<Utc> {
        use chrono::SubsecRound;
        (Utc::now() - Duration::minutes(minutes_ago)).trunc_subsecs(6)
    }

    impl Scene {
        /// A choice asked at `asked_at`, still pending; returns its id.
        async fn pending_at(&self, asked_at: DateTime<Utc>) -> Uuid {
            self.pending_in(self.session_id, asked_at).await
        }

        /// [`Self::pending_at`] in any session of the instance.
        async fn pending_in(&self, session_id: Uuid, asked_at: DateTime<Utc>) -> Uuid {
            sqlx::query_scalar(
                "INSERT INTO engine.life_choices (session_id, kind, tier_gap, asked_at) \
                 VALUES ($1, 'ask_for_space', 3, $2) RETURNING id",
            )
            .bind(session_id)
            .bind(asked_at)
            .fetch_one(&self.pool)
            .await
            .unwrap()
        }

        /// One 1:1 turn through the whole stream, the user's row sent at
        /// `sent_at` and carrying `action`. Returns the user row's id.
        async fn stream_turn(
            &self,
            state: &Arc<AppState>,
            sent_at: DateTime<Utc>,
            action: Option<eros_engine_core::types::UserAction>,
        ) -> Uuid {
            self.stream_turn_in(self.session_id, state, sent_at, action)
                .await
        }

        /// [`Self::stream_turn`] in any session of the instance.
        async fn stream_turn_in(
            &self,
            session_id: Uuid,
            state: &Arc<AppState>,
            sent_at: DateTime<Utc>,
            action: Option<eros_engine_core::types::UserAction>,
        ) -> Uuid {
            use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
            use futures_util::StreamExt;

            let metadata = action
                .as_ref()
                .map(|a| json!({ "action": serde_json::to_value(a).unwrap() }));
            let umid = match (ChatRepo { pool: &self.pool })
                .upsert_user_message_idempotent(
                    session_id,
                    if action.is_some() { "" } else { "在吗" },
                    &ulid::Ulid::new().to_string(),
                    "user",
                    metadata.as_ref(),
                )
                .await
                .unwrap()
            {
                UpsertUserOutcome::Inserted { message_id } => message_id,
                _ => unreachable!(),
            };
            sqlx::query("UPDATE engine.chat_messages SET sent_at = $2 WHERE id = $1")
                .bind(umid)
                .bind(sent_at)
                .execute(&self.pool)
                .await
                .unwrap();
            let _frames: Vec<_> = crate::pipeline::stream::run_stream(
                state.clone(),
                crate::pipeline::stream::PersistedUserMessage {
                    user_message_id: umid,
                    session_id,
                    user_id: self.user_id,
                    instance_id: self.instance_id,
                    content: if action.is_some() { "" } else { "在吗" }.into(),
                    prompt_traits: vec![],
                    audit: None,
                    tier: None,
                    memory_scope: Default::default(),
                    affinity_scope: Default::default(),
                    image_url: None,
                    image: None,
                    quote: Default::default(),
                    user_locale: Default::default(),
                    action,
                    outfit: None,
                },
                None,
            )
            .collect()
            .await;
            umid
        }

        /// (status, resolved_at) of the choice.
        async fn status_of(&self, choice: Uuid) -> (String, Option<DateTime<Utc>>) {
            sqlx::query_as("SELECT status, resolved_at FROM engine.life_choices WHERE id = $1")
                .bind(choice)
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

        /// (kind, event_key, session_id, started_at, ends_at, message_id) of
        /// the instance's events, oldest first.
        #[allow(clippy::type_complexity)]
        async fn instance_events(
            &self,
        ) -> Vec<(
            String,
            String,
            Option<Uuid>,
            DateTime<Utc>,
            DateTime<Utc>,
            Option<Uuid>,
        )> {
            sqlx::query_as(
                "SELECT kind, event_key, session_id, started_at, ends_at, message_id \
                   FROM engine.persona_events WHERE instance_id = $1 \
                  ORDER BY started_at, id",
            )
            .bind(self.instance_id)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// A running event of the instance, written in a sibling session (an
        /// event belongs to the persona, not the session it was drawn in).
        async fn seed_running(
            &self,
            kind: &str,
            key: &str,
            started_at: DateTime<Utc>,
            ends_at: DateTime<Utc>,
        ) {
            let elsewhere = seed_session(&self.pool, self.user_id, self.instance_id).await;
            sqlx::query(
                "INSERT INTO engine.persona_events \
                   (instance_id, session_id, event_key, kind, started_at, ends_at) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(self.instance_id)
            .bind(elsewhere)
            .bind(key)
            .bind(kind)
            .bind(started_at)
            .bind(ends_at)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        /// The four line axes, whose position is the tiers, and the number of
        /// `life_choice` affinity events of the session. The endpoints (warmth,
        /// patience) and the stored tier columns follow the lines on any turn
        /// and settle in the turn's detached post-process, so they are left out.
        async fn affinity_fingerprint(&self) -> (Vec<f64>, i64) {
            let axes: Vec<f64> = sqlx::query_scalar(
                "SELECT ARRAY[trust, intrigue, intimacy, tension] \
                   FROM engine.companion_affinity WHERE session_id = $1",
            )
            .bind(self.session_id)
            .fetch_one(&self.pool)
            .await
            .unwrap();
            let drops: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM engine.companion_affinity_events e \
                   JOIN engine.companion_affinity a ON a.id = e.affinity_id \
                  WHERE a.session_id = $1 AND e.event_type = 'life_choice'",
            )
            .bind(self.session_id)
            .fetch_one(&self.pool)
            .await
            .unwrap();
            (axes, drops)
        }
    }

    /// The first ordinary text message after she asked is the user's answer:
    /// the choice is `ignored` at that message, and that is all. A stored
    /// `ask_for_space` is converted like any other kind (spec
    /// 2026-11-23-affinity-50 §7.5): no falling-out, and the incident she is
    /// in keeps running.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_ignored_ask_writes_no_falling_out(pool: PgPool) {
        let s = scene(&pool).await;
        let (state, mock) = turn_env(&pool).await;
        let sent = recent(10);
        let choice = s.pending_at(sent - Duration::hours(2)).await;
        // She was reading.
        s.seed_running(
            "reading",
            "r",
            sent - Duration::minutes(20),
            sent + Duration::minutes(100),
        )
        .await;
        let events = s.instance_events().await;

        s.stream_turn(&state, sent, None).await;

        assert_eq!(
            s.status_of(choice).await,
            ("ignored".into(), Some(sent)),
            "resolved at the message that ignored her"
        );
        assert_eq!(
            s.instance_events().await,
            events,
            "no falling-out, and the incident keeps its end"
        );

        // The same turn's judge and reply hear of no falling-out.
        let judge = request_to(&mock, "pde/judge").await;
        assert_eq!(judge.len(), 1);
        assert!(!judge[0].contains(FELL_OUT_LABEL), "{}", judge[0]);
        assert!(judge[0].contains("在看书"), "still reading: {}", judge[0]);
        let reply = request_to(&mock, "deepseek/x").await;
        assert_eq!(reply.len(), 1);
        assert!(!reply[0].contains(FELL_OUT_LABEL), "{}", reply[0]);

        // No tier change, and no drop event: the judge rates the turn itself.
        // The scene's lines (bond 0.0, chemistry 0.7: tiers 1 and 4) stand.
        assert_eq!(
            s.affinity_fingerprint().await,
            (vec![0.0, 0.0, 0.7, 0.7], 0)
        );

        // The next message finds nothing pending: no new resolve.
        s.stream_turn(&state, sent + Duration::minutes(5), None)
            .await;
        assert_eq!(s.instance_events().await, events);
        assert_eq!(s.status_of(choice).await, ("ignored".into(), Some(sent)));
    }

    /// A typed reply to a romantic ask settles it as ignored at the message,
    /// and costs nothing: no falling-out. The judge rates the words like any
    /// other turn's (spec 2026-10-30 §4.5).
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_typed_answer_to_a_romantic_ask_ignores_it_without_a_falling_out(pool: PgPool) {
        let s = scene(&pool).await;
        let mock = mock_llm(PATIENT, None).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        s.seed_romantic(LifeChoiceKind::Propose, "pending", Duration::hours(1))
            .await;

        s.converting_turn(&state, "好啊").await;

        let sent_at: DateTime<Utc> = sqlx::query_scalar(
            "SELECT sent_at FROM engine.chat_messages WHERE session_id = $1 AND role = 'user'",
        )
        .bind(s.session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let (status, resolved_at): (String, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT status, resolved_at FROM engine.life_choices WHERE session_id = $1",
        )
        .bind(s.session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "ignored");
        assert_eq!(
            resolved_at,
            Some(sent_at),
            "resolved at the message that answered in words"
        );
        assert!(
            s.instance_events().await.is_empty(),
            "an ignored romantic ask starts no event"
        );
    }

    /// A give, a tip or a kiss while she waits is an attempt to mend, not an
    /// answer: the ask stays pending and the action is answered by its own
    /// rules. The next ordinary message is the answer.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_action_turn_leaves_the_ask_pending(pool: PgPool) {
        use eros_engine_core::types::UserAction;

        let s = scene(&pool).await;
        let (state, mock) = turn_env(&pool).await;
        let sent = recent(30);
        let choice = s.pending_at(sent - Duration::hours(2)).await;

        s.stream_turn(&state, sent, Some(UserAction::Kiss)).await;
        s.stream_turn(
            &state,
            sent + Duration::minutes(1),
            Some(UserAction::Tip { amount_usd: 1.0 }),
        )
        .await;
        assert_eq!(s.status_of(choice).await, ("pending".into(), None));
        assert!(s.instance_events().await.is_empty(), "no falling-out");
        assert!(
            request_to(&mock, "deepseek/x")
                .await
                .iter()
                .all(|r| !r.contains(FELL_OUT_LABEL)),
            "and her replies know of none"
        );

        let word = sent + Duration::minutes(2);
        s.stream_turn(&state, word, None).await;
        assert_eq!(s.status_of(choice).await, ("ignored".into(), Some(word)));
        assert!(
            s.instance_events().await.is_empty(),
            "an ignore writes no falling-out"
        );
    }

    /// A choice that lapsed (24 hours past its ask, the instant itself
    /// included) converts nothing: it stays `pending` and the turn goes on as
    /// any other. One microsecond earlier it was still waiting.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_expired_ask_converts_nothing(pool: PgPool) {
        let (state, mock) = turn_env(&pool).await;
        let micro = Duration::microseconds(1);
        let asked = recent(60 * 24 + 30);
        let expiry = asked + Duration::hours(24);

        for (sent, converts) in [
            (expiry + micro, false),
            (expiry, false),
            (expiry - micro, true),
        ] {
            let s = scene(&pool).await;
            let choice = s.pending_at(asked).await;
            s.stream_turn(&state, sent, None).await;
            let (status, resolved) = s.status_of(choice).await;
            if converts {
                assert_eq!((status.as_str(), resolved), ("ignored", Some(sent)));
            } else {
                assert_eq!((status.as_str(), resolved), ("pending", None), "at {sent}");
            }
            assert!(s.instance_events().await.is_empty(), "at {sent}");
        }
        // The lapsed turns were answered all the same, with nothing said of it.
        let replies = request_to(&mock, "deepseek/x").await;
        assert_eq!(replies.len(), 3);
        assert!(
            replies.iter().all(|r| !r.contains(FELL_OUT_LABEL)),
            "no turn hears of a falling-out"
        );
    }

    /// A message sent before her ask landed cannot be the answer to it: the
    /// ask can be written while the user's next message is already in flight.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_message_sent_before_the_ask_does_not_answer_it(pool: PgPool) {
        let s = scene(&pool).await;
        let (state, _mock) = turn_env(&pool).await;
        let asked = recent(20);
        let choice = s.pending_at(asked).await;

        s.stream_turn(&state, asked - Duration::seconds(1), None)
            .await;
        assert_eq!(s.status_of(choice).await, ("pending".into(), None));
        assert!(s.instance_events().await.is_empty());

        s.stream_turn(&state, asked + Duration::seconds(1), None)
            .await;
        assert_eq!(s.status_of(choice).await.0, "ignored");
    }

    // ---- One pending ask per instance; the conversion never runs apart ----

    /// She has one life: while any session of the instance waits for an answer
    /// the others do not ask, however warm it runs there. Answering it lets
    /// the same streak through.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_sibling_sessions_pending_ask_holds_the_trigger_back(pool: PgPool) {
        let mut b = scene_with_lines(&pool, 0.5, 0.5).await;
        b.set_art(json!({"hygienic": false})).await;
        b.record_scopes("full", AffinityScope::chemistry()).await;
        b.set_status("dating", false).await;
        b.event_under_way(EventKind::ShortTrip).await;
        let a = seed_session(&pool, b.user_id, b.instance_id).await;
        let mock = mock_llm(WARM, Some(SLEEP_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        b.pending_in(a, Utc::now() - Duration::hours(1)).await;

        // B meets every other condition of the bed: compatible scopes, three
        // warm judged turns, a text session of a genome instance.
        for _ in 0..3 {
            b.turn(&state, SEEN).await;
        }
        assert!(b.choices().await.is_empty(), "A's ask holds B back");
        assert!(
            companion_calls(&mock).await.is_empty(),
            "and no generation spent"
        );
        assert_eq!(b.choices_in(a).await.len(), 1, "A's ask is untouched");

        sqlx::query("UPDATE engine.life_choices SET status = 'ignored' WHERE session_id = $1")
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();
        b.turn(&state, SEEN).await;
        assert_eq!(
            b.kinds().await,
            vec![pending("sleep_together")],
            "answered, B asks"
        );
        assert_eq!(companion_calls(&mock).await.len(), 1);
    }

    /// A message during an agreed separation only ghosts. A choice a sibling
    /// session holds pending (the state before the instance held one ask at a
    /// time) is not converted by it: no falling-out, and the separation
    /// keeps its `ends_at`.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_message_during_a_separation_converts_no_pending_ask(pool: PgPool) {
        let a = scene(&pool).await;
        let b = lopsided_session(&pool, a.user_id, a.instance_id).await;
        let (state, _mock) = turn_env(&pool).await;
        let sent = recent(10);
        // A's ask was accepted: she is apart until ten hours after the message.
        accepted_separation(&a, a.session_id, sent + Duration::hours(10)).await;
        let choice = a.pending_in(b, sent - Duration::hours(2)).await;
        let before = a.instance_events().await;
        assert_eq!(before.len(), 1, "the separation, nothing else");

        let umid = a.stream_turn_in(b, &state, sent, None).await;

        let ghosted: bool =
            sqlx::query_scalar("SELECT ghost_decision FROM engine.chat_messages WHERE id = $1")
                .bind(umid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(ghosted, "a message during a separation ghosts");
        assert_eq!(
            a.status_of(choice).await,
            ("pending".into(), None),
            "and converts nothing"
        );
        assert_eq!(
            a.instance_events().await,
            before,
            "no falling-out, and the separation is not capped at the message"
        );
    }

    // ---- The streak starts over after the whole episode (spec §3.1) ----

    impl Scene {
        /// An accepted choice asked `asked_ago` ago and answered `resolved_ago`
        /// ago, whose separation ran from the answer until `ends_ago` ago.
        async fn seed_accepted_choice(
            &self,
            asked_ago: Duration,
            resolved_ago: Duration,
            ends_ago: Duration,
        ) {
            let now = Utc::now();
            let choice: Uuid = sqlx::query_scalar(
                "INSERT INTO engine.life_choices \
                   (session_id, kind, status, tier_gap, asked_at, resolved_at) \
                 VALUES ($1, 'ask_for_space', 'accepted', 3, $2, $3) RETURNING id",
            )
            .bind(self.session_id)
            .bind(now - asked_ago)
            .bind(now - resolved_ago)
            .fetch_one(&self.pool)
            .await
            .unwrap();
            let event = EventRepo { pool: &self.pool }
                .insert_preempting(&NewEvent {
                    instance_id: self.instance_id,
                    session_id: self.session_id,
                    event_key: "apart",
                    kind: EventKind::TakingSpace,
                    started_at: now - resolved_ago,
                    ends_at: now - ends_ago,
                })
                .await
                .unwrap();
            LifeChoiceRepo { pool: &self.pool }
                .set_event(choice, event.id)
                .await
                .unwrap();
        }

        /// Move every affinity event of the scene `ago` into the past.
        async fn age_events(&self, ago: Duration) {
            sqlx::query(
                "UPDATE engine.companion_affinity_events \
                    SET created_at = created_at - make_interval(secs => $1)",
            )
            .bind(ago.num_seconds() as f64)
            .execute(&self.pool)
            .await
            .unwrap();
        }
    }

    /// Judged turns while she waits (a give, a tip or a kiss does not answer
    /// her) are not the new streak: the ignore that follows resolves the
    /// choice at the converting message, and the converting turn is the first
    /// of three. Anchored at the ask it would fire on the converting turn
    /// itself.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_give_then_an_ignore_does_not_ask_again_on_the_converting_turn(pool: PgPool) {
        // A couple at tiers 3/3 the bed would be asked of: not hygienic, so no
        // bath, and away on a trip of her own, so no trip together.
        let mut s = scene_with_lines(&pool, 0.5, 0.5).await;
        s.set_art(json!({"hygienic": false})).await;
        s.record_scopes("full", AffinityScope::chemistry()).await;
        s.set_status("dating", false).await;
        s.event_under_way(EventKind::ShortTrip).await;
        let mock = mock_llm(WARM, Some(SLEEP_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        s.pending_at(Utc::now() - Duration::hours(2)).await;

        // Three warm judged turns, all after the ask, none of them an answer.
        for _ in 0..3 {
            s.turn(&state, SEEN).await;
        }
        assert_eq!(s.choices().await.len(), 1, "held back while she waits");

        // An ordinary message: the ask is ignored. Its own judge reading is
        // warm, and it is turn one of the new streak.
        s.converting_turn(&state, SEEN).await;
        let choices = s.choices().await;
        assert_eq!(choices.len(), 1, "{choices:?}");
        assert_eq!(choices[0].0, "ignored");
        assert!(
            companion_calls(&mock).await.is_empty(),
            "no ask on the turn that ignored her"
        );

        s.turn(&state, SEEN).await;
        assert_eq!(s.choices().await.len(), 1, "two turns after the episode");
        s.turn(&state, SEEN).await;
        assert_eq!(s.choices().await.len(), 2, "three turns after it ask again");
        assert_eq!(companion_calls(&mock).await.len(), 1);
    }

    /// Action turns inside a separation (one agreed before `ask_for_space`
    /// was retired) are judged like any other, but they are part of the
    /// episode: once she is back the streak counts only the turns after the
    /// separation ended.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn action_turns_during_a_separation_do_not_pre_load_the_return_streak(pool: PgPool) {
        let s = scene(&pool).await;
        let mock = mock_llm(LOW_PATIENCE, Some(COLD_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        // Two judged action turns, 20 hours ago. The separation they fell in
        // ran from 29 to 11 hours ago, and the ask was put 30 hours ago.
        s.turn(&state, SEEN).await;
        s.turn(&state, SEEN).await;
        assert!(!s.cold_war().await, "two turns are not three");
        s.age_events(Duration::hours(20)).await;
        s.seed_accepted_choice(
            Duration::hours(30),
            Duration::hours(29),
            Duration::hours(11),
        )
        .await;

        s.turn(&state, SEEN).await;
        assert!(!s.cold_war().await, "one turn since she came back");
        assert!(companion_calls(&mock).await.is_empty());
        s.turn(&state, SEEN).await;
        assert!(!s.cold_war().await, "two");
        s.turn(&state, SEEN).await;
        assert!(s.cold_war().await, "three since the return start it");
        assert_eq!(companion_calls(&mock).await.len(), 1);
    }

    // ---- The romantic choices (spec 2026-10-30 §4.3–§4.6) ----

    const SLEEP_CUE: &str = "（你们最近越来越亲密，你很想和对方更进一步。你决定主动开口，问对方愿不愿意和你上床，希望对方给你一个答复。）";
    const PROPOSE_CUE: &str =
        "（你越来越确定想和对方共度一生。你决定郑重地向对方求婚，希望对方给你一个答复。）";
    const SLEEP_TEXT: &str = "今晚留下来陪我，好不好？";
    const BATHE_CUE: &str = "（你们最近越来越亲密，你很想和对方一起洗个澡。你决定主动开口，问对方愿不愿意和你一起洗，希望对方给你一个答复。）";
    const BATHE_TEXT: &str = "要不要一起洗澡？";
    const PROPOSE_TEXT: &str = "你愿意和我结婚吗？";
    /// Nothing moved, warmth and patience both at the top level.
    const WARM: &str = r#"{"warmth":3,"trust":{"grade":0,"direction":"up"},"intimacy":{"grade":0,"direction":"up"},"intrigue":{"grade":0,"direction":"up"},"tension":{"grade":0,"direction":"up"},"patience":3,"reason":"很好"}"#;

    impl Scene {
        /// The pair's scopes as the caller sent them, recorded on an ordinary
        /// reply of hers.
        async fn record_scopes(&self, memory: &str, affinity: AffinityScope) {
            sqlx::query(
                "INSERT INTO engine.chat_messages (session_id, role, content, metadata) \
                 VALUES ($1, 'assistant', '嗯，我在听。', $2)",
            )
            .bind(self.session_id)
            .bind(json!({
                "memory_scope": memory,
                "affinity_scope": serde_json::to_value(affinity).unwrap(),
            }))
            .execute(&self.pool)
            .await
            .unwrap();
        }

        /// The persona's `art_metadata`, read by the next turn.
        async fn set_art(&mut self, art: serde_json::Value) {
            sqlx::query(
                "UPDATE engine.persona_genomes g SET art_metadata = $2 \
                   FROM engine.persona_instances i \
                  WHERE i.id = $1 AND g.id = i.genome_id",
            )
            .bind(self.instance_id)
            .bind(art)
            .execute(&self.pool)
            .await
            .unwrap();
            self.persona = PersonaRepo { pool: &self.pool }
                .load_companion(self.instance_id)
                .await
                .unwrap()
                .expect("the persona");
        }

        /// A `kind` choice asked `ago` ago, in `status`. A romantic kind
        /// carries no tier gap.
        async fn seed_romantic(&self, kind: LifeChoiceKind, status: &str, ago: Duration) {
            sqlx::query(
                "INSERT INTO engine.life_choices (session_id, kind, status, asked_at, \
                                                  resolved_at) \
                 VALUES ($1, $2, $3, now() - make_interval(secs => $4), \
                         CASE WHEN $3 = 'pending' THEN NULL \
                              ELSE now() - make_interval(secs => $4) + interval '1 minute' END)",
            )
            .bind(self.session_id)
            .bind(kind.as_str())
            .bind(status)
            .bind(ago.num_seconds() as f64)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        /// (kind, status, tier_gap) of every choice in the session, oldest first.
        async fn kinds(&self) -> Vec<(String, String, Option<i16>)> {
            sqlx::query_as(
                "SELECT kind, status, tier_gap FROM engine.life_choices \
                  WHERE session_id = $1 ORDER BY asked_at",
            )
            .bind(self.session_id)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// The stored (bond_tier, chem_tier).
        async fn tiers(&self) -> (i16, i16) {
            sqlx::query_as(
                "SELECT bond_tier, chem_tier FROM engine.companion_affinity WHERE session_id = $1",
            )
            .bind(self.session_id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
        }
    }

    fn pending(kind: &str) -> (String, String, Option<i16>) {
        (kind.into(), "pending".into(), None)
    }

    /// The cue of her one ask: the request's last `user` turn.
    async fn only_cue(mock: &MockServer) -> serde_json::Value {
        let calls = companion_calls(mock).await;
        assert_eq!(calls.len(), 1, "one ask, one generation");
        let last = calls[0]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(last["role"], json!("user"));
        last["content"].clone()
    }

    #[test]
    fn romantic_kind_table() {
        let hot = |n| {
            vec![
                JudgedEndpoints {
                    warmth: 0.8,
                    patience: 0.8,
                    patience_grade: 2,
                };
                n
            ]
        };
        let mut mixed = hot(8);
        mixed[1].patience = 0.5;
        mixed[4].warmth = 0.1;
        mixed[7].warmth = 0.1; // 5 hits of 8
        let dating = RelationshipStatus::Dating;
        assert_eq!(
            romantic_kind(3, 3, dating, &hot(3), false, false, false),
            Some(LifeChoiceKind::SleepTogether)
        );
        assert_eq!(
            romantic_kind(3, 3, dating, &hot(2), false, false, false),
            None,
            "two is not three"
        );
        assert_eq!(
            romantic_kind(2, 5, dating, &hot(3), false, false, false),
            None,
            "chem below 3"
        );
        assert_eq!(
            romantic_kind(4, 4, dating, &hot(8), false, false, false),
            Some(LifeChoiceKind::Propose),
            "propose wins the tie"
        );
        assert_eq!(
            romantic_kind(4, 4, dating, &hot(8), false, false, true),
            Some(LifeChoiceKind::SleepTogether),
            "propose settled"
        );
        assert_eq!(
            romantic_kind(4, 4, dating, &mixed, true, false, false),
            Some(LifeChoiceKind::Propose),
            "5 of 8"
        );
        {
            let mut four = mixed.clone();
            four[0].patience = 0.69;
            assert_eq!(
                romantic_kind(4, 4, dating, &four, true, false, false),
                None,
                "4 of 8"
            );
        }
        assert_eq!(
            romantic_kind(4, 3, dating, &hot(8), true, false, false),
            None,
            "bond below 4"
        );
        assert_eq!(
            romantic_kind(5, 5, dating, &hot(8), true, false, true),
            None,
            "both settled"
        );

        // Only a couple is proposed to; `none` never is.
        for status in RelationshipStatus::ALL {
            let proposes = matches!(
                status,
                RelationshipStatus::Dating
                    | RelationshipStatus::Fwb
                    | RelationshipStatus::Situationship
            );
            assert_eq!(
                romantic_kind(5, 5, status, &hot(8), true, true, false),
                proposes.then_some(LifeChoiceKind::Propose),
                "{status:?}"
            );
        }
        // A bare partnership asks to fall asleep at chemistry 3, for sex at 4.
        let partner = RelationshipStatus::Partner;
        assert_eq!(
            romantic_kind(3, 3, partner, &hot(3), false, false, false),
            Some(LifeChoiceKind::FallAsleepTogether)
        );
        assert_eq!(
            romantic_kind(4, 4, partner, &hot(8), false, false, false),
            Some(LifeChoiceKind::SleepTogether)
        );
        assert_eq!(
            romantic_kind(4, 4, RelationshipStatus::None, &hot(8), false, false, false),
            Some(LifeChoiceKind::FallAsleepTogether)
        );
        // Each sleep ask is settled on its own.
        assert_eq!(
            romantic_kind(3, 3, partner, &hot(3), false, true, false),
            None,
            "falling asleep settled"
        );
        assert_eq!(
            romantic_kind(3, 3, dating, &hot(3), false, true, false),
            Some(LifeChoiceKind::SleepTogether),
            "falling asleep settled leaves the bed open"
        );
        assert_eq!(
            romantic_kind(4, 4, partner, &hot(8), true, false, false),
            None,
            "the bed settled leaves no ask at chemistry 4"
        );
    }

    #[test]
    fn the_bath_rule() {
        let e = |warmth| JudgedEndpoints {
            warmth,
            patience: 0.1,
            patience_grade: 2,
        };
        // Newest first; three of five reach 0.5.
        let three = [e(0.5), e(0.2), e(0.6), e(0.1), e(0.9)];
        let two = [e(0.5), e(0.49), e(0.6), e(0.1)];
        assert!(bathe_qualifies(2, Some(true), &three), "hygienic at 2");
        assert!(!bathe_qualifies(1, Some(true), &three), "hygienic at 1");
        assert!(!bathe_qualifies(2, None, &three), "neutral at 2");
        assert!(bathe_qualifies(3, None, &three), "neutral at 3");
        assert!(
            !bathe_qualifies(5, Some(false), &three),
            "never if not hygienic"
        );
        assert!(!bathe_qualifies(5, Some(true), &two), "two warm turns");
    }

    /// A hygienic persona at chemistry tier 2: her third warm-enough judged
    /// turn asks for a bath, where the bed (tier 3) and a trip (warmth above
    /// 0.7) cannot.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_hygienic_persona_asks_for_a_bath_at_chemistry_2(pool: PgPool) {
        let mut s = scene_with_lines(&pool, 0.5, 0.25).await;
        s.set_art(json!({"hygienic": true})).await;
        s.record_scopes("full", AffinityScope::chemistry()).await;
        s.set_status("dating", false).await;
        let mock = mock_llm(WARM, Some(BATHE_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        s.turn(&state, SEEN).await;
        s.turn(&state, SEEN).await;
        assert!(s.kinds().await.is_empty(), "two turns are not three");

        s.turn(&state, SEEN).await;
        assert_eq!(s.tiers().await.1, 2, "chemistry tier 2");
        assert_eq!(s.kinds().await, vec![pending("bathe_together")]);
        assert_eq!(s.ask_rows().await[0].1, BATHE_TEXT);
        assert_eq!(only_cue(&mock).await, json!(BATHE_CUE));
    }

    /// The same streak at tier 2 asks nothing of a persona with no
    /// `hygienic`.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_neutral_persona_waits_for_chemistry_3(pool: PgPool) {
        let s = scene_with_lines(&pool, 0.5, 0.25).await;
        s.record_scopes("full", AffinityScope::chemistry()).await;
        s.set_status("dating", false).await;
        let mock = mock_llm(WARM, Some(BATHE_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        for _ in 0..4 {
            s.turn(&state, SEEN).await;
        }
        assert!(s.kinds().await.is_empty());
        assert!(companion_calls(&mock).await.is_empty());
    }

    /// An accepted or ignored bath holds the next ask back for 7 days.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_bath_cools_down_for_7_days(pool: PgPool) {
        let mock = mock_llm(WARM, Some(BATHE_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        for (days, asks) in [(6, false), (8, true)] {
            let mut s = scene_with_lines(&pool, 0.5, 0.25).await;
            s.set_art(json!({"hygienic": true})).await;
            s.record_scopes("full", AffinityScope::chemistry()).await;
            s.set_status("dating", false).await;
            s.seed_romantic(
                LifeChoiceKind::BatheTogether,
                "ignored",
                Duration::days(days),
            )
            .await;
            for _ in 0..3 {
                s.turn(&state, SEEN).await;
            }
            assert_eq!(
                s.kinds().await.contains(&pending("bathe_together")),
                asks,
                "{days} days"
            );
        }
    }

    /// Lines at 0.5 (tiers 3/3), where a level-3 verdict reads about 0.74 on
    /// both endpoints: on a compatible pair the third warm judged turn asks.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn three_warm_turns_on_a_compatible_pair_ask_to_sleep_together(pool: PgPool) {
        let s = scene_with_lines(&pool, 0.5, 0.5).await;
        s.record_scopes("full", AffinityScope::chemistry()).await;
        s.set_status("dating", false).await;
        let mock = mock_llm(WARM, Some(SLEEP_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        s.turn(&state, SEEN).await;
        s.turn(&state, SEEN).await;
        assert!(s.kinds().await.is_empty(), "two warm turns are not three");
        assert!(companion_calls(&mock).await.is_empty(), "and cost nothing");

        s.turn(&state, SEEN).await;
        assert_eq!(s.tiers().await, (3, 3));
        assert_eq!(s.kinds().await, vec![pending("sleep_together")]);

        // Her words: an ordinary assistant row, proactive, written as the
        // choice's message.
        let rows = s.ask_rows().await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(s.choices().await[0].2, Some(rows[0].0));
        assert_eq!(rows[0].1, SLEEP_TEXT);
        assert_eq!(rows[0].2.as_deref(), Some("proactive"));
        assert_eq!(only_cue(&mock).await, json!(SLEEP_CUE));
    }

    /// No recorded scopes, or scopes without a chemistry axis, read as an
    /// incompatible pair: the same warm streak asks nothing until the caller's
    /// scopes allow it.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn without_compatible_scopes_nothing_fires(pool: PgPool) {
        let s = scene_with_lines(&pool, 0.5, 0.5).await;
        s.set_status("dating", false).await;
        // A background keeps the trip ask, which needs no scopes, out of it.
        s.event_under_way(EventKind::ShortTrip).await;
        let mock = mock_llm(WARM, Some(SLEEP_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        for _ in 0..3 {
            s.turn(&state, SEEN).await;
        }
        assert!(s.kinds().await.is_empty(), "no scopes recorded");

        s.record_scopes("neutral_and_relationship", AffinityScope::bond())
            .await;
        s.turn(&state, SEEN).await;
        assert!(s.kinds().await.is_empty(), "bond axes only");
        assert!(companion_calls(&mock).await.is_empty());

        s.record_scopes("full", AffinityScope::chemistry()).await;
        s.turn(&state, SEEN).await;
        assert_eq!(s.kinds().await, vec![pending("sleep_together")]);
    }

    /// Lines at 0.7 (tiers 4/4): five warm judged turns among the newest
    /// eight propose, four do not. The cold turns (warmth level 2) keep any
    /// three in a row from asking to sleep together first.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn five_of_eight_warm_turns_at_tier_four_propose(pool: PgPool) {
        let warm = mock_llm(WARM, Some(PROPOSE_TEXT)).await;
        let cold = mock_llm(PATIENT, Some(PROPOSE_TEXT)).await;
        let warm_state = with_models(test_state(pool.clone()), &warm.uri());
        let cold_state = with_models(test_state(pool.clone()), &cold.uri());
        let state = |is_warm: bool| if is_warm { &warm_state } else { &cold_state };

        // Not hygienic: the bath, whose bar is lower, never asks.
        let mut s = scene_with_lines(&pool, 0.7, 0.7).await;
        s.set_art(json!({"hygienic": false})).await;
        s.record_scopes("full", AffinityScope::chemistry()).await;
        s.set_status("dating", false).await;
        for (i, is_warm) in [false, true, true, false, true, true, false, true]
            .into_iter()
            .enumerate()
        {
            assert!(s.kinds().await.is_empty(), "nothing before turn {}", i + 1);
            s.turn(state(is_warm), SEEN).await;
        }
        assert_eq!(s.tiers().await, (4, 4));
        assert_eq!(s.kinds().await, vec![pending("propose")]);
        assert_eq!(only_cue(&warm).await, json!(PROPOSE_CUE));
        assert!(companion_calls(&cold).await.is_empty());

        let mut four = scene_with_lines(&pool, 0.7, 0.7).await;
        four.set_art(json!({"hygienic": false})).await;
        four.record_scopes("full", AffinityScope::chemistry()).await;
        four.set_status("dating", false).await;
        for is_warm in [true, false, true, true, false, true, false, false] {
            four.turn(state(is_warm), SEEN).await;
        }
        assert!(four.kinds().await.is_empty(), "four of eight");
        assert_eq!(companion_calls(&warm).await.len(), 1, "the first ask only");
    }

    /// When both kinds qualify on one turn, she proposes. The warm turns run
    /// while the pair is not yet compatible, so the turn the scopes allow is
    /// the first where either could fire, and both do.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn propose_wins_when_both_qualify(pool: PgPool) {
        let s = scene_with_lines(&pool, 0.7, 0.7).await;
        s.set_status("dating", false).await;
        // A background keeps the trip ask, which needs no scopes, out of it.
        s.event_under_way(EventKind::ShortTrip).await;
        let mock = mock_llm(WARM, Some(PROPOSE_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        for _ in 0..7 {
            s.turn(&state, SEEN).await;
        }
        assert!(s.kinds().await.is_empty());
        s.record_scopes("full", AffinityScope::chemistry()).await;
        s.turn(&state, SEEN).await;
        assert_eq!(s.kinds().await, vec![pending("propose")]);
        assert_eq!(only_cue(&mock).await, json!(PROPOSE_CUE));
    }

    /// A kind once ignored (or accepted) has had its answer: however warm the
    /// turns, it is not asked again in the session.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_ignored_kind_is_settled(pool: PgPool) {
        let mock = mock_llm(WARM, Some(SLEEP_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        for status in ["ignored", "accepted"] {
            let s = scene_with_lines(&pool, 0.5, 0.5).await;
            s.record_scopes("full", AffinityScope::chemistry()).await;
            s.set_status("dating", false).await;
            // A background keeps the trip ask out of it.
            s.event_under_way(EventKind::ShortTrip).await;
            // A bath taken yesterday keeps the bath ask out of it.
            s.seed_romantic(LifeChoiceKind::BatheTogether, "accepted", Duration::days(1))
                .await;
            s.seed_romantic(LifeChoiceKind::SleepTogether, status, Duration::hours(1))
                .await;
            for _ in 0..4 {
                s.turn(&state, SEEN).await;
            }
            assert_eq!(s.kinds().await.len(), 2, "{status}");
        }
        assert!(companion_calls(&mock).await.is_empty());
    }

    /// One pending ask at a time: an `ask_for_space` waiting for its answer
    /// holds the romantic ask back; once answered, the same streak asks.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_pending_ask_for_space_blocks_a_romantic_ask(pool: PgPool) {
        let s = scene_with_lines(&pool, 0.5, 0.5).await;
        s.record_scopes("full", AffinityScope::chemistry()).await;
        s.set_status("dating", false).await;
        let mock = mock_llm(WARM, Some(SLEEP_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        s.seed_choice("pending", Duration::minutes(1)).await;

        for _ in 0..3 {
            s.turn(&state, SEEN).await;
        }
        assert_eq!(
            s.kinds().await,
            vec![("ask_for_space".into(), "pending".into(), Some(3))]
        );
        assert!(companion_calls(&mock).await.is_empty());

        sqlx::query(
            "UPDATE engine.life_choices \
                SET status = 'declined', resolved_at = asked_at + interval '1 second' \
              WHERE session_id = $1",
        )
        .bind(s.session_id)
        .execute(&pool)
        .await
        .unwrap();
        s.turn(&state, SEEN).await;
        assert_eq!(s.kinds().await[1], pending("sleep_together"));
    }

    /// A decline settles nothing: the kind re-arms once a fresh streak has
    /// run after the answer. Warm turns from before it belong to the episode.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_declined_kind_re_arms_after_a_fresh_streak(pool: PgPool) {
        let s = scene_with_lines(&pool, 0.5, 0.5).await;
        s.record_scopes("full", AffinityScope::chemistry()).await;
        s.set_status("dating", false).await;
        let mock = mock_llm(WARM, Some(SLEEP_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        s.turn(&state, SEEN).await;
        s.turn(&state, SEEN).await;
        s.age_events(Duration::hours(2)).await;
        s.seed_romantic(
            LifeChoiceKind::SleepTogether,
            "declined",
            Duration::hours(1),
        )
        .await;

        s.turn(&state, SEEN).await;
        s.turn(&state, SEEN).await;
        assert_eq!(s.kinds().await.len(), 1, "two fresh turns are not three");
        assert!(companion_calls(&mock).await.is_empty());
        s.turn(&state, SEEN).await;
        assert_eq!(
            s.kinds().await,
            vec![
                ("sleep_together".into(), "declined".into(), None),
                pending("sleep_together"),
            ]
        );
    }

    // ---- Traveling together ----

    const TRAVEL_TEXT: &str = "这周末我们一起出去玩吧？";

    impl Scene {
        /// An event of `kind` under way on the instance for the next day.
        async fn event_under_way(&self, kind: EventKind) {
            let now = Utc::now();
            EventRepo { pool: &self.pool }
                .insert_preempting(&NewEvent {
                    instance_id: self.instance_id,
                    session_id: self.session_id,
                    event_key: "under-way",
                    kind,
                    started_at: now - Duration::hours(1),
                    ends_at: now + Duration::hours(24),
                })
                .await
                .unwrap();
        }

        /// The scopes of a pair with no chemistry axis, so no romantic ask
        /// gets in first.
        async fn record_bond_only(&self) {
            self.record_scopes("neutral_and_relationship", AffinityScope::bond())
                .await;
        }
    }

    #[test]
    fn travel_qualifies_table() {
        let hot = |n| {
            vec![
                JudgedEndpoints {
                    warmth: 0.8,
                    patience: 0.8,
                    patience_grade: 2,
                };
                n
            ]
        };
        assert!(travel_qualifies(3, &hot(3)));
        assert!(travel_qualifies(5, &hot(3)));
        assert!(!travel_qualifies(2, &hot(3)), "bond below 3");
        assert!(!travel_qualifies(3, &hot(2)), "two is not three");
        let mut cool = hot(3);
        cool[2].patience = 0.7;
        assert!(!travel_qualifies(3, &cool), "0.7 is not above 0.7");
    }

    /// The trip needs no chemistry axis in scope: a pair the romantic kinds
    /// never reach is asked on its third warm turn.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn three_warm_turns_ask_a_bond_only_pair_to_travel(pool: PgPool) {
        let s = scene_with_lines(&pool, 0.5, 0.5).await;
        s.record_bond_only().await;
        s.set_status("partner", false).await;
        let mock = mock_llm(WARM, Some(TRAVEL_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        s.turn(&state, SEEN).await;
        s.turn(&state, SEEN).await;
        assert!(s.kinds().await.is_empty(), "two warm turns are not three");

        s.turn(&state, SEEN).await;
        assert_eq!(s.kinds().await, vec![pending("travel_together")]);
        let rows = s.ask_rows().await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].1, TRAVEL_TEXT);
    }

    /// Everything the one generation was sent, as one string.
    async fn only_prompt(mock: &MockServer) -> String {
        let calls = companion_calls(mock).await;
        assert_eq!(calls.len(), 1, "one generation");
        calls[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Her ask is written under the scopes the pair's replies recorded, so a
    /// compatible pair at (3, 3) reads its own composed name.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn her_ask_reads_the_pairs_recorded_scopes(pool: PgPool) {
        let s = scene_with_lines(&pool, 0.5, 0.5).await;
        s.record_scopes("full", AffinityScope::chemistry()).await;
        s.set_status("dating", false).await;
        let mock = mock_llm(WARM, Some(SLEEP_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        for _ in 0..3 {
            s.turn(&state, SEEN).await;
        }
        assert_eq!(s.kinds().await, vec![pending("sleep_together")]);
        let prompt = only_prompt(&mock).await;
        assert!(prompt.contains("论感情，你们现在是心上密友"), "{prompt}");
        // The recorded scope itself, not a wider one: it has no warmth axis,
        // so the warm pair's pet-name line stays out.
        assert!(!prompt.contains("可以用一些亲昵的称呼"), "{prompt}");
    }

    /// Recorded scopes without a chemistry axis keep the chemistry-1 name in
    /// her ask; a pair with no scopes recorded reads as compatible.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_trip_ask_follows_recorded_scopes_and_defaults_to_compatible(pool: PgPool) {
        let bond_only = scene_with_lines(&pool, 0.5, 0.5).await;
        bond_only.record_bond_only().await;
        bond_only.set_status("partner", false).await;
        let mock = mock_llm(WARM, Some(TRAVEL_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        for _ in 0..3 {
            bond_only.turn(&state, SEEN).await;
        }
        assert_eq!(bond_only.kinds().await, vec![pending("travel_together")]);
        let prompt = only_prompt(&mock).await;
        assert!(prompt.contains("论感情，你们现在是亲近好友"), "{prompt}");

        let unrecorded = scene_with_lines(&pool, 0.5, 0.5).await;
        unrecorded.set_status("partner", false).await;
        let mock = mock_llm(WARM, Some(TRAVEL_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        for _ in 0..3 {
            unrecorded.turn(&state, SEEN).await;
        }
        assert_eq!(unrecorded.kinds().await, vec![pending("travel_together")]);
        let prompt = only_prompt(&mock).await;
        assert!(prompt.contains("论感情，你们现在是心上密友"), "{prompt}");
        // Every axis is in scope, warmth included.
        assert!(prompt.contains("可以用一些亲昵的称呼"), "{prompt}");
    }

    /// The cold-war message keeps the engine's default scopes, whatever the
    /// pair recorded.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_cold_war_message_keeps_the_default_scopes(pool: PgPool) {
        let s = scene_with_lines(&pool, 0.45, 0.45).await;
        s.record_scopes("full", AffinityScope::chemistry()).await;
        let mock = mock_llm(LOW_PATIENCE, Some(COLD_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        for _ in 0..3 {
            s.turn(&state, SEEN).await;
        }
        assert!(s.cold_war().await);
        let prompt = only_prompt(&mock).await;
        assert!(prompt.contains("你是用户的亲近好友"), "{prompt}");
        assert!(!prompt.contains("心上密友"), "{prompt}");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_romantic_ask_goes_before_a_trip(pool: PgPool) {
        let s = scene_with_lines(&pool, 0.5, 0.5).await;
        s.record_scopes("full", AffinityScope::chemistry()).await;
        s.set_status("dating", false).await;
        let mock = mock_llm(WARM, Some(SLEEP_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        for _ in 0..3 {
            s.turn(&state, SEEN).await;
        }
        assert_eq!(s.kinds().await, vec![pending("sleep_together")]);
    }

    /// Accepted or ignored holds the next ask back fourteen days; a decline
    /// holds it only until a fresh streak.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_answered_trip_waits_fourteen_days(pool: PgPool) {
        let mock = mock_llm(WARM, Some(TRAVEL_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        for (status, days, asks) in [
            ("accepted", 13, false),
            ("ignored", 13, false),
            ("accepted", 15, true),
            ("declined", 1, true),
        ] {
            let s = scene_with_lines(&pool, 0.5, 0.5).await;
            s.record_bond_only().await;
            s.set_status("partner", false).await;
            s.seed_romantic(LifeChoiceKind::TravelTogether, status, Duration::days(days))
                .await;
            for _ in 0..3 {
                s.turn(&state, SEEN).await;
            }
            assert_eq!(
                s.kinds().await.contains(&pending("travel_together")),
                asks,
                "{status} {days} days ago"
            );
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn she_does_not_ask_to_travel_during_a_background(pool: PgPool) {
        let s = scene_with_lines(&pool, 0.5, 0.5).await;
        s.record_bond_only().await;
        s.set_status("partner", false).await;
        s.event_under_way(EventKind::ShortTrip).await;
        let mock = mock_llm(WARM, Some(TRAVEL_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        for _ in 0..3 {
            s.turn(&state, SEEN).await;
        }
        assert!(s.kinds().await.is_empty());
    }

    /// The trip keeps her romantic asks open.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_trip_together_leaves_a_romantic_ask_open(pool: PgPool) {
        let s = scene_with_lines(&pool, 0.5, 0.5).await;
        s.record_scopes("full", AffinityScope::chemistry()).await;
        s.set_status("dating", false).await;
        s.event_under_way(EventKind::TravelingTogether).await;
        let mock = mock_llm(WARM, Some(SLEEP_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        for _ in 0..3 {
            s.turn(&state, SEEN).await;
        }
        assert_eq!(s.kinds().await, vec![pending("sleep_together")]);
    }

    // ---- Her status asks (spec 2026-11-23-affinity-50 §7.1–§7.3) ----

    const CONFESS_CUE: &str = "（你发现自己喜欢上对方了。你决定郑重地向对方告白，问对方愿不愿意和你在一起，希望对方给你一个答复。）";
    const PARTNER_CUE: &str =
        "（你越来越觉得对方是值得托付的人。你决定郑重地提出和对方结成伙伴，希望对方给你一个答复。）";
    const BREAK_UP_CUE: &str =
        "（这段冷战让你心灰意冷。你决定郑重地向对方提出分手，希望对方给你一个答复。）";
    const CONFESS_TEXT: &str = "我好像喜欢上你了，你愿意和我在一起吗？";
    const PARTNER_TEXT: &str = "我们结成伙伴吧，好吗？";
    const BREAK_UP_TEXT: &str = "我们分手吧。";
    /// The newest judged turns of a proposal scene: five warm among eight,
    /// never three warm in a row.
    const FIVE_OF_EIGHT: [bool; 8] = [false, true, true, false, true, true, false, true];

    impl Scene {
        /// A status change `ago` ago: the `relationship` event it wrote, from
        /// `from` to `to`. The row's own status is [`Self::set_status`]'s.
        async fn seed_status_change(&self, from: &str, to: &str, ago: Duration) {
            sqlx::query(
                "INSERT INTO engine.companion_affinity_events \
                   (affinity_id, event_type, context, created_at) \
                 SELECT id, 'relationship', \
                        jsonb_build_object('from', $2::text, 'to', $3::text), \
                        now() - make_interval(secs => $4) \
                   FROM engine.companion_affinity WHERE session_id = $1",
            )
            .bind(self.session_id)
            .bind(from)
            .bind(to)
            .bind(ago.num_seconds() as f64)
            .execute(&self.pool)
            .await
            .unwrap();
        }
    }

    /// A compatible pair in `status` whose chemistry leads: bond 0.2 (tier
    /// 2), chemistry 0.5 (tier 3). A level-3 verdict reads warmth about 0.74
    /// and patience about 0.59, so no turn is warm enough for the bed. Not
    /// hygienic, so no bath.
    async fn chemistry_led(pool: &PgPool, status: &str) -> Scene {
        let mut s = scene_with_lines(pool, 0.2, 0.5).await;
        s.set_art(json!({"hygienic": false})).await;
        s.record_scopes("full", AffinityScope::chemistry()).await;
        s.set_status(status, false).await;
        s
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_compatible_chemistry_led_warm_pair_is_confessed_to(pool: PgPool) {
        let s = chemistry_led(&pool, "none").await;
        let mock = mock_llm(WARM, Some(CONFESS_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        s.turn(&state, SEEN).await;
        s.turn(&state, SEEN).await;
        assert!(s.kinds().await.is_empty(), "two warm turns are not three");

        s.turn(&state, SEEN).await;
        assert_eq!(s.tiers().await, (2, 3));
        assert_eq!(s.kinds().await, vec![pending("confess")]);
        assert_eq!(s.ask_rows().await[0].1, CONFESS_TEXT);
        assert_eq!(only_cue(&mock).await, json!(CONFESS_CUE));
    }

    /// The same lines on bond-only scopes: not compatible, so no confession;
    /// bond neither leads nor reaches tier 3, so no partnership or trip.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_incompatible_chemistry_led_pair_is_asked_nothing(pool: PgPool) {
        let s = scene_with_lines(&pool, 0.2, 0.5).await;
        s.record_bond_only().await;
        let mock = mock_llm(WARM, Some(CONFESS_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        for _ in 0..4 {
            s.turn(&state, SEEN).await;
        }
        assert_eq!(s.tiers().await, (2, 3));
        assert!(s.kinds().await.is_empty());
        assert!(companion_calls(&mock).await.is_empty());
    }

    /// Bond leads at tier 3 (0.5 over chemistry 0.2) and three judged turns
    /// have the top patience level, which reads about 0.74: she offers a
    /// partnership, whatever the scopes. Their warmth stays low, so no trip.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_bond_led_patient_pair_is_offered_a_partnership(pool: PgPool) {
        let s = scene_with_lines(&pool, 0.5, 0.2).await;
        s.record_bond_only().await;
        let mock = mock_llm(PATIENT, Some(PARTNER_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        s.turn(&state, SEEN).await;
        s.turn(&state, SEEN).await;
        assert!(
            s.kinds().await.is_empty(),
            "two patient turns are not three"
        );

        s.turn(&state, SEEN).await;
        assert_eq!(s.tiers().await, (3, 2));
        assert_eq!(s.kinds().await, vec![pending("partner_up")]);
        assert_eq!(only_cue(&mock).await, json!(PARTNER_CUE));
    }

    /// A bare partnership: her sleep ask is to fall asleep side by side at
    /// chemistry tier 3, and for sex at tier 4. Bond leads, so she does not
    /// confess.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_partner_is_asked_to_fall_asleep_at_chemistry_three_and_for_sex_at_four(
        pool: PgPool,
    ) {
        let mock = mock_llm(WARM, Some(SLEEP_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        for (bond, chem, tiers, kind) in [
            (0.55, 0.5, (3, 3), "fall_asleep_together"),
            (0.75, 0.7, (4, 4), "sleep_together"),
        ] {
            let s = scene_with_lines(&pool, bond, chem).await;
            s.record_scopes("full", AffinityScope::chemistry()).await;
            s.set_status("partner", false).await;
            for _ in 0..3 {
                s.turn(&state, SEEN).await;
            }
            assert_eq!(s.tiers().await, tiers);
            assert_eq!(s.kinds().await, vec![pending(kind)], "{tiers:?}");
        }
        assert_eq!(companion_calls(&mock).await.len(), 2);
    }

    /// Her confession waits fourteen days after its last answer of any sort,
    /// a decline included, and fourteen days after the status last changed.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_confession_waits_fourteen_days(pool: PgPool) {
        let mock = mock_llm(WARM, Some(CONFESS_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        for (days, asks) in [(1, false), (15, true)] {
            let s = chemistry_led(&pool, "none").await;
            s.seed_romantic(LifeChoiceKind::Confess, "declined", Duration::days(days))
                .await;
            for _ in 0..3 {
                s.turn(&state, SEEN).await;
            }
            let mut kinds = vec![("confess".into(), "declined".into(), None)];
            if asks {
                kinds.push(pending("confess"));
            }
            assert_eq!(s.kinds().await, kinds, "declined {days} days ago");
        }

        // They broke up a day ago.
        let s = chemistry_led(&pool, "ex").await;
        s.seed_status_change("dating", "ex", Duration::days(1))
            .await;
        for _ in 0..3 {
            s.turn(&state, SEEN).await;
        }
        assert!(s.kinds().await.is_empty(), "the status changed a day ago");
        assert_eq!(companion_calls(&mock).await.len(), 1, "fifteen days only");
    }

    /// A second lap: she was accepted once, they broke up, and both lie
    /// twenty days back. The once-only rule does not hold a confession; only
    /// its waits do, and they are over.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_second_lap_after_a_break_up(pool: PgPool) {
        let s = chemistry_led(&pool, "ex").await;
        s.seed_romantic(LifeChoiceKind::Confess, "accepted", Duration::days(20))
            .await;
        s.seed_status_change("dating", "ex", Duration::days(20))
            .await;
        let mock = mock_llm(WARM, Some(CONFESS_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        for _ in 0..3 {
            s.turn(&state, SEEN).await;
        }
        assert_eq!(
            s.kinds().await,
            vec![
                ("confess".into(), "accepted".into(), None),
                pending("confess"),
            ]
        );
    }

    /// In a cold war over a breakable status, three judged turns at the
    /// judge's lowest patience level make her ask to break up. Over `none`
    /// there is nothing to end.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn she_asks_to_break_up_after_three_more_lowest_turns(pool: PgPool) {
        let mock = mock_llm(LOW_PATIENCE, Some(BREAK_UP_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        for (status, asks) in [("dating", true), ("none", false)] {
            let s = scene(&pool).await;
            s.set_status(status, true).await;

            s.turn(&state, SEEN).await;
            s.turn(&state, SEEN).await;
            assert!(s.kinds().await.is_empty(), "{status}: two are not three");

            s.turn(&state, SEEN).await;
            let kinds = if asks {
                vec![pending("break_up")]
            } else {
                vec![]
            };
            assert_eq!(s.kinds().await, kinds, "{status}");
            assert!(s.cold_war().await, "{status}: the cold war goes on");
        }
        assert_eq!(only_cue(&mock).await, json!(BREAK_UP_CUE));
    }

    /// The turn that starts a cold war writes a `relationship` event, which
    /// moves the streak anchor: the turns that started it do not also make
    /// her ask to break up. Three more at the lowest level do.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_turn_that_starts_a_cold_war_does_not_ask_to_break_up(pool: PgPool) {
        let s = scene(&pool).await;
        s.set_status("dating", false).await;
        let mock = mock_llm(LOW_PATIENCE, Some(COLD_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        for _ in 0..3 {
            s.turn(&state, SEEN).await;
        }
        assert!(s.cold_war().await);
        assert_eq!(s.cold_war_rows().await, vec![proactive(COLD_TEXT)]);
        assert!(s.choices().await.is_empty(), "no choice row");

        s.turn(&state, SEEN).await;
        s.turn(&state, SEEN).await;
        assert!(s.choices().await.is_empty(), "two turns since it started");
        s.turn(&state, SEEN).await;
        assert_eq!(s.kinds().await, vec![pending("break_up")]);
        assert_eq!(companion_calls(&mock).await.len(), 2);
    }

    /// Only a couple is proposed to: the turns that propose at `dating`
    /// (`five_of_eight_warm_turns_at_tier_four_propose`) ask nothing of a
    /// partner, and once three warm turns run in a row she asks for the bed.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_proposal_needs_a_couple(pool: PgPool) {
        let warm = mock_llm(WARM, Some(SLEEP_TEXT)).await;
        let cold = mock_llm(PATIENT, Some(SLEEP_TEXT)).await;
        let warm_state = with_models(test_state(pool.clone()), &warm.uri());
        let cold_state = with_models(test_state(pool.clone()), &cold.uri());

        let mut s = scene_with_lines(&pool, 0.7, 0.7).await;
        s.set_art(json!({"hygienic": false})).await;
        s.record_scopes("full", AffinityScope::chemistry()).await;
        s.set_status("partner", false).await;
        for is_warm in FIVE_OF_EIGHT {
            s.turn(if is_warm { &warm_state } else { &cold_state }, SEEN)
                .await;
        }
        assert_eq!(s.tiers().await, (4, 4));
        assert!(s.kinds().await.is_empty(), "no proposal");

        s.turn(&warm_state, SEEN).await;
        s.turn(&warm_state, SEEN).await;
        assert_eq!(s.kinds().await, vec![pending("sleep_together")]);
    }

    /// An ignored proposal waits fourteen days; it is not settled for good.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_ignored_proposal_waits_fourteen_days(pool: PgPool) {
        let warm = mock_llm(WARM, Some(PROPOSE_TEXT)).await;
        let cold = mock_llm(PATIENT, Some(PROPOSE_TEXT)).await;
        let warm_state = with_models(test_state(pool.clone()), &warm.uri());
        let cold_state = with_models(test_state(pool.clone()), &cold.uri());

        for (days, asks) in [(1, false), (15, true)] {
            let mut s = scene_with_lines(&pool, 0.7, 0.7).await;
            s.set_art(json!({"hygienic": false})).await;
            s.record_scopes("full", AffinityScope::chemistry()).await;
            s.set_status("dating", false).await;
            s.seed_romantic(LifeChoiceKind::Propose, "ignored", Duration::days(days))
                .await;
            for is_warm in FIVE_OF_EIGHT {
                s.turn(if is_warm { &warm_state } else { &cold_state }, SEEN)
                    .await;
            }
            let mut kinds = vec![("propose".into(), "ignored".into(), None)];
            if asks {
                kinds.push(pending("propose"));
            }
            assert_eq!(s.kinds().await, kinds, "ignored {days} days ago");
        }
    }

    /// Neither status ask is made while a cold war is on (spec
    /// 2026-11-23-affinity-50 §7.3). As in `no_warm_ask_during_a_cold_war`,
    /// the turns run while a sibling session's pending ask closes the runway,
    /// and the check is then called on its own.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn no_confession_or_partnership_during_a_cold_war(pool: PgPool) {
        for (kind, bond, chem, verdict) in [
            (LifeChoiceKind::Confess, 0.2, 0.5, WARM),
            (LifeChoiceKind::PartnerUp, 0.5, 0.2, PATIENT),
        ] {
            let mock = mock_llm(verdict, Some(CONFESS_TEXT)).await;
            let state = with_models(test_state(pool.clone()), &mock.uri());
            let s = scene_with_lines(&pool, bond, chem).await;
            s.record_scopes("full", AffinityScope::chemistry()).await;
            let sibling = seed_session(&pool, s.user_id, s.instance_id).await;
            s.pending_in(sibling, Utc::now() - Duration::minutes(1))
                .await;
            for _ in 0..3 {
                s.turn(&state, SEEN).await;
            }
            assert!(s.kinds().await.is_empty(), "the runway was closed");
            sqlx::query(
                "UPDATE engine.life_choices SET status = 'declined', resolved_at = now() \
                  WHERE session_id = $1",
            )
            .bind(sibling)
            .execute(&pool)
            .await
            .unwrap();

            let check = || async {
                let (persona, session_id) = (&s.persona, s.session_id);
                let a = &load_decayed(&state, session_id)
                    .await
                    .unwrap()
                    .expect("row");
                match kind {
                    LifeChoiceKind::Confess => {
                        try_confess(&state, session_id, s.user_id, s.instance_id, persona, a).await
                    }
                    _ => {
                        try_partner_up(&state, session_id, s.user_id, s.instance_id, persona, a)
                            .await
                    }
                }
                .unwrap()
            };
            s.set_status("none", true).await;
            check().await;
            assert!(s.kinds().await.is_empty(), "{kind:?} during a cold war");

            s.set_status("none", false).await;
            check().await;
            assert_eq!(
                s.kinds().await,
                vec![pending(kind.as_str())],
                "{kind:?} once it is over"
            );
            assert_eq!(companion_calls(&mock).await.len(), 1, "{kind:?}");
        }
    }
}
