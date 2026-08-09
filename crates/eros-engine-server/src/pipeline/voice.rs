// SPDX-License-Identifier: AGPL-3.0-only
//! Voice channel — thin per-turn generator and prompt.
//!
//! Spec: docs/superpowers/specs/2026-07-07-voice-call-parts-design.md

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use ulid::Ulid;
use uuid::Uuid;

use eros_engine_core::affinity::{Affinity, BondLabel, ChemistryLabel};
use eros_engine_core::persona::PersonaGenome;
use eros_engine_core::scope::{InsightMode, MemoryScope, RelationshipScope};
use eros_engine_llm::model_config::ResolvedVoice;
use eros_engine_llm::openrouter::{ChatMessage as WireMessage, ChatRequest, UsageBlock};
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::{ChatMessageSlim, ChatRepo};
use eros_engine_store::human_insight::HumanInsightRepo;
use eros_engine_store::persona::PersonaRepo;

use crate::pipeline::handlers::human_insights_to_bullets;
use crate::pipeline::stream::{
    ulid_string, ProtocolFrame, StreamErrorCode, STREAM_OPEN_TIMEOUT, STREAM_TOTAL_TIMEOUT,
};
use crate::state::AppState;

/// `chat_sessions.metadata` key holding a voice session's frozen bootstrap
/// snapshot (write-once — see `ChatRepo::set_voice_bootstrap`).
const VOICE_BOOTSTRAP_KEY: &str = "voice_bootstrap";

/// Assemble the thin voice system prompt: persona + voice directive +
/// the (already rendered) bootstrap block + one optional relationship line
/// (bond/chemistry-derived, gated by `relationship_scope`). Deliberately
/// excludes memories, traits, scopes, and every heavy block the text path's
/// `build_prompt` composes.
///
/// Order matters: persona → directive → bootstrap → relationship line. The
/// bootstrap block is frozen for the whole call, so everything up to (and
/// including) it is byte-stable across the call's turns — provider-side
/// prefix caching keeps working.
pub fn build_voice_prompt(
    genome: &PersonaGenome,
    directive: &str,
    bootstrap: Option<&str>,
    affinity: Option<&Affinity>,
    relationship_scope: RelationshipScope,
) -> String {
    let mut s = String::with_capacity(genome.system_prompt.len() + directive.len() + 384);
    s.push_str(&genome.system_prompt);
    s.push_str("\n\n");
    s.push_str(directive);
    if let Some(block) = bootstrap {
        s.push_str("\n\n");
        s.push_str(block);
    }
    if let Some(line) = affinity.and_then(|a| relationship_line(a, relationship_scope)) {
        s.push_str("\n\n");
        s.push_str(&line);
    }
    s
}

/// A voice call's frozen bootstrap context, stored under
/// `chat_sessions.metadata.voice_bootstrap` on the session's first turn and
/// re-injected verbatim on every later turn (OpenRouter is stateless — there
/// is no such thing as "injected once at session start").
///
/// Everything here is stored **rendered**: the bullets and the transcript are
/// the exact strings that go into the prompt, never re-derived from live rows.
/// That is what makes the prefix byte-stable for the whole call even as the
/// underlying `human_insights` row changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceBootstrap {
    /// 基础画像 bullets, rendered at first turn under that turn's `InsightMode`.
    pub insights: Vec<String>,
    /// Previous voice call's tail, rendered as a plain transcript.
    #[serde(default)]
    pub prev_call: Option<String>,
    /// The sibling voice session the transcript came from (audit only).
    #[serde(default)]
    pub prev_session_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// What this turn should do about the bootstrap snapshot, decided purely from
/// the session `metadata` the route already loaded — no query.
#[derive(Debug)]
enum BootstrapPlan {
    /// Marker present and well-formed: inject it, touch nothing.
    Frozen(VoiceBootstrap),
    /// Marker present but unreadable: inject nothing and NEVER rewrite — the
    /// snapshot is write-once, and clobbering it would be worse than degrading.
    Malformed,
    /// Marker absent: assemble a snapshot this turn (the only path that reads).
    Assemble,
}

fn plan_bootstrap(session_metadata: &serde_json::Value) -> BootstrapPlan {
    match session_metadata.get(VOICE_BOOTSTRAP_KEY) {
        None => BootstrapPlan::Assemble,
        Some(v) => match serde_json::from_value::<VoiceBootstrap>(v.clone()) {
            Ok(b) => BootstrapPlan::Frozen(b),
            Err(_) => BootstrapPlan::Malformed,
        },
    }
}

/// Render the bootstrap block: `[关于他]` bullets then the `[上次通话]`
/// transcript. Each sub-block is omitted when its part is empty; `None` when
/// both are — the caller then emits no block at all (no stray blank lines).
fn render_bootstrap(b: &VoiceBootstrap) -> Option<String> {
    let bullets: Vec<&str> = b
        .insights
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let prev = b
        .prev_call
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if bullets.is_empty() && prev.is_none() {
        return None;
    }
    let mut s = String::new();
    if !bullets.is_empty() {
        s.push_str("[关于他]");
        for b in bullets {
            s.push_str("\n- ");
            s.push_str(b);
        }
    }
    if let Some(t) = prev {
        if !s.is_empty() {
            s.push_str("\n\n");
        }
        s.push_str("[上次通话]\n");
        s.push_str(t);
    }
    Some(s)
}

/// Render a sibling voice session's tail as a plain transcript, one line per
/// message. Empty-content rows are skipped (a caption-less image turn would
/// otherwise render as a bare speaker prefix) and so is any role outside the
/// user/assistant pair; `gift_user` speaks as the user.
fn render_prev_call(rows: &[ChatMessageSlim]) -> Option<String> {
    let mut lines: Vec<String> = Vec::with_capacity(rows.len());
    for m in rows {
        let content = m.content.trim();
        if content.is_empty() {
            continue;
        }
        let speaker = match m.role.as_str() {
            "user" | "gift_user" => "用户",
            "assistant" => "她",
            _ => continue,
        };
        lines.push(format!("{speaker}：{content}"));
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// 基础画像 bullets for the snapshot. `Off` short-circuits without a query —
/// an empty part, and still a SUCCESS (the marker may be written).
async fn load_bootstrap_insights(
    pool: &PgPool,
    user_id: Uuid,
    mode: InsightMode,
) -> Result<Vec<String>, sqlx::Error> {
    if matches!(mode, InsightMode::Off) {
        return Ok(Vec::new());
    }
    // Deliberately NOT `load_human_insight_bullets`: it swallows Err into an
    // empty vec, erasing the failed-vs-empty distinction this snapshot needs
    // (an empty read freezes the marker; a failed read must not).
    match (HumanInsightRepo { pool }).load(user_id).await {
        Ok(Some(row)) => Ok(human_insights_to_bullets(&row, mode)),
        Ok(None) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// The previous call's tail: the latest sibling voice session for this
/// user × instance (never this session, never a text session) and its last
/// `VOICE_HISTORY_WINDOW` messages. No sibling ⇒ an empty part, still a
/// success (first-ever call).
async fn load_bootstrap_prev_call(
    pool: &PgPool,
    user_id: Uuid,
    instance_id: Uuid,
    session_id: Uuid,
) -> Result<(Option<String>, Option<Uuid>), sqlx::Error> {
    let repo = ChatRepo { pool };
    let Some(sibling) = repo
        .latest_sibling_voice_session(user_id, instance_id, session_id)
        .await?
    else {
        return Ok((None, None));
    };
    // Same 8-message unit as the in-session window (spec §3). `history_slim`
    // is the narrow, channel-blind projection — the sibling is already
    // channel-scoped by the lookup.
    let rows = repo
        .history_slim(sibling.id, VOICE_HISTORY_WINDOW, 0)
        .await?;
    Ok((render_prev_call(&rows), Some(sibling.id)))
}

/// Assemble a first-turn snapshot. The two parts degrade independently; the
/// returned flag is `true` only when BOTH succeeded — a partial snapshot is
/// injected in memory for this turn but must not be frozen, so the next turn
/// retries.
async fn assemble_bootstrap(
    pool: &PgPool,
    user_id: Uuid,
    instance_id: Uuid,
    session_id: Uuid,
    insight_mode: InsightMode,
) -> (VoiceBootstrap, bool) {
    let (insights_res, prev_res) = tokio::join!(
        load_bootstrap_insights(pool, user_id, insight_mode),
        load_bootstrap_prev_call(pool, user_id, instance_id, session_id),
    );
    let mut complete = true;
    let insights = match insights_res {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "voice: bootstrap insights read failed");
            complete = false;
            Vec::new()
        }
    };
    let (prev_call, prev_session_id) = match prev_res {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "voice: bootstrap prev-call read failed");
            complete = false;
            (None, None)
        }
    };
    (
        VoiceBootstrap {
            insights,
            prev_call,
            prev_session_id,
            created_at: Utc::now(),
        },
        complete,
    )
}

/// One relationship-tone line, derived at read time from the affinity row's
/// bond/chemistry tiers — never from the cached `relationship_label`, which
/// the voice path (no per-turn affinity eval) would leave stale forever.
/// `scope` gates which halves are injected.
fn relationship_line(affinity: &Affinity, scope: RelationshipScope) -> Option<String> {
    let base = || {
        match affinity.bond_label() {
        BondLabel::Acquaintance => {
            "You two are still getting to know each other; keep it light and natural."
        }
        BondLabel::Friend => "You two are friends; be warm, easy, and natural.",
        BondLabel::CloseFriend => "You two are close friends; be warm, familiar, and comfortable.",
        BondLabel::Confidant => {
            "You two trust each other deeply; speak openly, at ease, and with quiet closeness."
        }
        BondLabel::Soulmate => {
            "You two know each other inside out; total comfort, familiarity, and unspoken understanding."
        }
    }
    };
    let clause = || {
        match affinity.chemistry_label() {
        ChemistryLabel::Spark | ChemistryLabel::Flirtation => {
            "A faint, unspoken spark exists between you. Keep it subtle — light teasing is allowed, but do not lean into romance or seduction yet."
        }
        ChemistryLabel::Crush => {
            "There's a clear and growing attraction between you. Let soft flirtation and quiet allure color your words. Be teasing, a little magnetic, but still restrained."
        }
        ChemistryLabel::Lover => {
            "You share a romantic and physical bond. Be affectionate, intimate, and gently alluring. Your voice and manner should feel warm, close, and quietly seductive."
        }
        ChemistryLabel::Beloved => {
            "You two are deeply in love and highly attuned to each other. Be openly affectionate, sensual, and alluring. Speak with natural intimacy, quiet desire, and magnetic ease — as if the other person is already yours."
        }
    }
    };
    match scope {
        RelationshipScope::None => None,
        RelationshipScope::Bond => Some(base().to_string()),
        RelationshipScope::Chemistry => Some(clause().to_string()),
        RelationshipScope::Both => Some(format!("{} {}", base(), clause())),
    }
}

/// Inputs for one voice turn. The user utterance is already persisted (by the
/// route) as the latest history row, so the generator reads it from history —
/// it is not passed again here.
pub struct VoiceTurn {
    pub session_id: Uuid,
    pub instance_id: Uuid,
    pub user_id: Uuid,
    pub user_message_id: Uuid,
    pub relationship_scope: RelationshipScope,
    /// This turn's memory scope. On the FIRST turn its resolved `InsightMode`
    /// picks the bootstrap snapshot's insight tier — frozen for the whole call
    /// from then on.
    pub memory_scope: MemoryScope,
    /// `chat_sessions.metadata` as the route already loaded it. Carries the
    /// `voice_bootstrap` marker, so the common path (marker present) costs no
    /// extra query at all.
    pub session_metadata: serde_json::Value,
}

/// Recent turns fed to the model on a voice turn — 8 messages (4 exchanges).
/// Shorter than the text path to keep latency/tokens down; the bootstrap
/// snapshot and per-turn recall carry the longer memory. Doubles as the tail
/// length quoted from the previous call into the bootstrap snapshot.
pub const VOICE_HISTORY_WINDOW: i64 = 8;

/// Drive one voice turn: load persona + (optional) affinity + recent history
/// (+ the bootstrap snapshot on a session's first turn), assemble the thin
/// prompt, stream a single-model completion (walking the outage fallback chain
/// ourselves, since `execute_stream` is single-model), emit `delta`* then
/// `done`, and persist the assistant turn. `error` only when no candidate
/// produced anything — never for a memory problem, which always degrades to a
/// warn.
pub fn run_voice_turn(
    state: Arc<AppState>,
    turn: VoiceTurn,
    resolved: ResolvedVoice,
) -> impl futures_util::Stream<Item = ProtocolFrame> + Send + 'static {
    async_stream::stream! {
        let chat_repo = ChatRepo { pool: &state.pool };
        let persona_repo = PersonaRepo { pool: &state.pool };
        let affinity_repo = AffinityRepo { pool: &state.pool };

        // Decided from the session row the route already loaded: after the
        // first turn this is `Frozen` and the whole bootstrap costs ZERO
        // queries.
        let plan = plan_bootstrap(&turn.session_metadata);
        if matches!(plan, BootstrapPlan::Malformed) {
            tracing::warn!(
                session_id = %turn.session_id,
                "voice: unreadable voice_bootstrap metadata — injecting nothing, never rewriting",
            );
        }
        let assemble_bootstrap_now = matches!(plan, BootstrapPlan::Assemble);
        // The FIRST turn's scope decides the snapshot's insight tier; later
        // turns can't change it (the snapshot is frozen).
        let insight_mode = turn.memory_scope.resolve().0;

        // All independent reads in one round trip's worth of wall clock. The
        // bootstrap future is inert unless this is the first turn.
        let (persona_res, affinity_res, history_res, assembled) = tokio::join!(
            persona_repo.load_companion(turn.instance_id),
            // Resolve affinity by user × persona-instance, not by session:
            // `companion_affinity` is populated only by the text pipeline, so a
            // voice-channel session's own `session_id` never has a row — this
            // read-only lookup takes the freshest row across every session for
            // the same pair (e.g. a prior text session) instead. Never creates a
            // row on the voice path.
            affinity_repo.load_latest_for_pair(turn.user_id, turn.instance_id),
            // Chronological history, includes the just-persisted user turn.
            chat_repo.history(turn.session_id, VOICE_HISTORY_WINDOW, 0),
            async {
                if assemble_bootstrap_now {
                    Some(assemble_bootstrap(
                        &state.pool,
                        turn.user_id,
                        turn.instance_id,
                        turn.session_id,
                        insight_mode,
                    ).await)
                } else {
                    None
                }
            },
        );

        let persona = match persona_res {
            Ok(Some(p)) => p,
            _ => {
                yield ProtocolFrame::Error {
                    code: StreamErrorCode::Internal,
                    retryable: false,
                    message: "persona instance not found".into(),
                    user_message: "服务出现问题，请稍后再试".into(),
                };
                return;
            }
        };

        let affinity = affinity_res.unwrap_or(None);

        let history = match history_res {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "voice: history read failed");
                yield ProtocolFrame::Error {
                    code: StreamErrorCode::Internal,
                    retryable: true,
                    message: "history read failed".into(),
                    user_message: "服务出现问题，请稍后再试".into(),
                };
                return;
            }
        };

        // Freeze the snapshot on the first turn, then inject it (this turn from
        // the in-memory copy, later turns from `metadata`). Every failure here
        // is a warn: a memory problem never costs the caller a reply.
        let bootstrap = match plan {
            BootstrapPlan::Frozen(b) => Some(b),
            BootstrapPlan::Malformed => None,
            BootstrapPlan::Assemble => match assembled {
                Some((snapshot, complete)) => {
                    if complete {
                        match serde_json::to_value(&snapshot) {
                            // rows_affected 0 = the key was already there (a
                            // concurrent first turn won the race): normal, and
                            // this turn just uses its own identical copy.
                            Ok(v) => if let Err(e) = chat_repo.set_voice_bootstrap(turn.session_id, &v).await {
                                tracing::warn!(error = %e, "voice: bootstrap snapshot write failed; using in-memory copy");
                            },
                            Err(e) => tracing::warn!(error = %e, "voice: bootstrap snapshot serialize failed"),
                        }
                    } else {
                        tracing::warn!(
                            session_id = %turn.session_id,
                            "voice: bootstrap assembly incomplete — injecting what loaded, marker left unwritten (next turn retries)",
                        );
                    }
                    Some(snapshot)
                }
                None => None,
            },
        };
        let bootstrap_block = bootstrap.as_ref().and_then(render_bootstrap);

        let system_prompt = build_voice_prompt(
            &persona.genome,
            &resolved.directive,
            bootstrap_block.as_deref(),
            affinity.as_ref(),
            turn.relationship_scope,
        );

        let mut messages = Vec::with_capacity(history.len() + 1);
        messages.push(WireMessage { role: "system".into(), content: system_prompt });
        for m in history {
            if m.content.is_empty() {
                continue; // defensive: never emit an empty-content wire message
                          // (e.g. a caption-less image turn from a mixed session) —
                          // some providers reject empty messages.
            }
            let role = match m.role.as_str() {
                "assistant" => "assistant",
                "user" | "gift_user" => "user",
                _ => continue,
            };
            messages.push(WireMessage { role: role.into(), content: m.content });
        }

        // Candidate chain: primary + outage fallbacks (single-model each).
        let mut candidates = Vec::with_capacity(1 + resolved.fallback_model.len());
        candidates.push(resolved.model.clone());
        candidates.extend(resolved.fallback_model.iter().cloned());

        let mid = Ulid::new();
        let message_id = ulid_string(mid);
        let assistant_uuid: Uuid = mid.into();

        let mut acc = String::new();
        let mut last_usage: Option<UsageBlock> = None;
        let mut last_gen_id: Option<String> = None;
        let mut served_model: Option<String> = None;
        let mut truncated = false;

        // Chain-shared request: `execute_stream_as` borrows it and sends each
        // candidate's model id, so the prompt is never cloned per attempt
        // (issue #188 — parity with the text pipeline's loops). `model` here
        // is a placeholder `execute_stream_as` ignores.
        let req = ChatRequest {
            model: String::new(),
            messages,
            temperature: resolved.temperature as f32,
            max_tokens: resolved.max_tokens,
            reasoning: resolved.reasoning.clone(),
            task: Some("chat_voice".into()),
            ..Default::default()
        };

        'candidates: for model_id in candidates {
            // Per-attempt metadata: reset so an abandoned candidate's usage / gen_id /
            // model / truncated never leaks onto a later fallback's reply.
            last_usage = None;
            last_gen_id = None;
            served_model = None;
            truncated = false;
            // Bound the open: a provider that accepts the socket but never
            // sends response headers must not hold the turn (issue #188 —
            // the same caps as the text pipeline).
            let stream = match tokio::time::timeout(
                STREAM_OPEN_TIMEOUT,
                state.openrouter.execute_stream_as(&req, &model_id),
            ).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    tracing::warn!(model = %model_id, error = %e, "voice: open stream failed");
                    if acc.is_empty() { continue 'candidates; }
                    truncated = true;
                    break 'candidates;
                }
                Err(_) => {
                    tracing::warn!(
                        model = %model_id,
                        "voice: open timeout ({}s)",
                        STREAM_OPEN_TIMEOUT.as_secs()
                    );
                    if acc.is_empty() { continue 'candidates; }
                    truncated = true;
                    break 'candidates;
                }
            };
            futures_util::pin_mut!(stream);
            // Hard per-attempt cap: byte-level idle liveness is bounded in the
            // llm client; this caps a stream that keeps trickling forever.
            let deadline = tokio::time::Instant::now() + STREAM_TOTAL_TIMEOUT;
            loop {
                let item = match tokio::time::timeout_at(
                    deadline,
                    futures_util::StreamExt::next(&mut stream),
                )
                .await
                {
                    Ok(item) => item,
                    Err(_) => {
                        tracing::warn!(
                            model = %model_id,
                            "voice: total timeout ({}s)",
                            STREAM_TOTAL_TIMEOUT.as_secs()
                        );
                        if acc.is_empty() { continue 'candidates; }
                        truncated = true;
                        break 'candidates;
                    }
                };
                match item {
                    Some(Ok(chunk)) => {
                        if chunk.usage.is_some() { last_usage = chunk.usage.clone(); }
                        if chunk.generation_id.is_some() { last_gen_id = chunk.generation_id.clone(); }
                        if chunk.model.is_some() { served_model = chunk.model.clone(); }
                        if let Some(text) = chunk.content {
                            acc.push_str(&text);
                            yield ProtocolFrame::Delta { message_id: message_id.clone(), content: text };
                        }
                        // "content_filter" = mid-generation safety cut: the
                        // text is incomplete, so it carries the same
                        // truncation signal as "length" (issue #188 — parity
                        // with the text pipeline's gates).
                        if matches!(
                            chunk.finish_reason.as_deref(),
                            Some("length") | Some("content_filter")
                        ) {
                            truncated = true;
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(model = %model_id, error = %e, "voice: mid-stream error");
                        if acc.is_empty() { continue 'candidates; }
                        truncated = true;
                        break 'candidates;
                    }
                    None => {
                        // Clean stream end. If this candidate streamed nothing, treat it as an
                        // empty/failed candidate and try the next one (fallbacks apply to empty
                        // completions too); otherwise we have a reply — stop.
                        if acc.is_empty() {
                            continue 'candidates;
                        }
                        break 'candidates;
                    }
                }
            }
        }

        // Any empty accumulation — no successful open, or a stream that sent
        // only metadata (id/model) and then errored or ended without content
        // — is an upstream failure. Never emit an empty `done`.
        if acc.is_empty() {
            yield ProtocolFrame::Error {
                code: StreamErrorCode::UpstreamUnavailable,
                retryable: true,
                message: "voice generation failed on all candidates".into(),
                user_message: "对方暂时说不出话，请稍后再试".into(),
            };
            return;
        }

        // Persist the assistant turn only when it carries text. The DB always
        // gets the FULL unfiltered usage; the wire `Done` frame below gets a
        // separate, hidden-keys-filtered copy (mirrors the text/replay paths).
        let usage_full = last_usage.as_ref().and_then(|u| serde_json::to_value(u).ok());
        let scope_metadata = serde_json::json!({ "relationship_scope": turn.relationship_scope });
        if !acc.is_empty() {
            if let Err(e) = chat_repo
                .insert_voice_assistant_message(
                    turn.session_id,
                    turn.user_message_id,
                    assistant_uuid,
                    &acc,
                    served_model.as_deref(),
                    usage_full.as_ref(),
                    last_gen_id.as_deref(),
                    truncated,
                    Some(&scope_metadata),
                )
                .await
            {
                tracing::warn!(error = %e, "voice: assistant persist failed");
            }
        }

        let mut usage_wire = usage_full;
        crate::routes::companion::filter_usage_keys(
            &mut usage_wire,
            &state.config.openrouter_usage_hidden_keys,
        );

        yield ProtocolFrame::Done {
            message_id,
            truncated,
            usage: usage_wire,
            generation_id: last_gen_id,
            ghost_fallback: false,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn genome() -> PersonaGenome {
        PersonaGenome {
            id: Uuid::new_v4(),
            name: "Mia".into(),
            system_prompt: "You are Mia.".into(),
            tip_personality: None,
            art_metadata: serde_json::json!({}),
        }
    }

    /// Affinity row landing on chosen tiers via raw axis values; cached
    /// relationship_label deliberately None — the line must not read it.
    fn affinity_at(
        warmth: f64,
        trust: f64,
        intrigue: f64,
        intimacy: f64,
        tension: f64,
    ) -> Affinity {
        let now = Utc::now();
        Affinity {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            instance_id: Uuid::new_v4(),
            warmth,
            trust,
            intrigue,
            intimacy,
            patience: 0.5,
            tension,
            ghost_streak: 0,
            last_ghost_at: None,
            total_ghosts: 0,
            relationship_label: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// A frozen snapshot with both parts populated.
    fn snapshot(insights: &[&str], prev_call: Option<&str>) -> VoiceBootstrap {
        VoiceBootstrap {
            insights: insights.iter().map(|s| (*s).to_string()).collect(),
            prev_call: prev_call.map(str::to_string),
            prev_session_id: None,
            created_at: Utc::now(),
        }
    }

    fn slim(role: &str, content: &str) -> ChatMessageSlim {
        ChatMessageSlim {
            id: Uuid::new_v4(),
            role: role.into(),
            content: content.into(),
            sent_at: Utc::now(),
            client_msg_id: None,
            tips_amount_usd: None,
            channel: None,
        }
    }

    #[test]
    fn includes_persona_and_directive_without_affinity() {
        let p = build_voice_prompt(
            &genome(),
            "DIRECTIVE",
            None,
            None,
            RelationshipScope::default(),
        );
        assert_eq!(p, "You are Mia.\n\nDIRECTIVE");
    }

    // ─── bootstrap block rendering ──────────────────────────────────────

    /// The whole block, byte-pinned: label, bullet prefix, sub-block order,
    /// and the `\n\n` joining discipline the prompt already uses.
    #[test]
    fn bootstrap_block_rendering_is_pinned() {
        let s = snapshot(
            &["城市：上海", "职业：设计师"],
            Some("用户：你好\n她：嗨呀"),
        );
        let p = build_voice_prompt(
            &genome(),
            "DIRECTIVE",
            render_bootstrap(&s).as_deref(),
            None,
            RelationshipScope::None,
        );
        assert_eq!(
            p,
            "You are Mia.\n\nDIRECTIVE\n\n\
             [关于他]\n- 城市：上海\n- 职业：设计师\n\n\
             [上次通话]\n用户：你好\n她：嗨呀"
        );
    }

    /// Order: persona → directive → bootstrap → relationship line.
    #[test]
    fn bootstrap_block_sits_between_directive_and_relationship_line() {
        let a = affinity_at(0.0, 0.0, 0.0, 0.0, 0.0);
        let s = snapshot(&["城市：上海"], Some("用户：你好"));
        let p = build_voice_prompt(
            &genome(),
            "DIRECTIVE",
            render_bootstrap(&s).as_deref(),
            Some(&a),
            RelationshipScope::default(),
        );
        let directive = p.find("DIRECTIVE").expect("directive present");
        let about = p.find("[关于他]").expect("insights sub-block present");
        let prev = p.find("[上次通话]").expect("prev-call sub-block present");
        let line = p
            .find("still getting to know each other")
            .expect("relationship line present");
        assert!(
            directive < about && about < prev && prev < line,
            "expected persona → directive → bootstrap → relationship line; got {p}"
        );
    }

    #[test]
    fn bootstrap_empty_insights_omits_the_about_sub_block() {
        let s = snapshot(&[], Some("用户：你好"));
        let p = build_voice_prompt(
            &genome(),
            "DIRECTIVE",
            render_bootstrap(&s).as_deref(),
            None,
            RelationshipScope::None,
        );
        assert_eq!(p, "You are Mia.\n\nDIRECTIVE\n\n[上次通话]\n用户：你好");
        assert!(!p.contains("[关于他]"));
    }

    #[test]
    fn bootstrap_missing_prev_call_omits_the_prev_call_sub_block() {
        let s = snapshot(&["城市：上海"], None);
        let p = build_voice_prompt(
            &genome(),
            "DIRECTIVE",
            render_bootstrap(&s).as_deref(),
            None,
            RelationshipScope::None,
        );
        assert_eq!(p, "You are Mia.\n\nDIRECTIVE\n\n[关于他]\n- 城市：上海");
        assert!(!p.contains("[上次通话]"));
    }

    /// Both parts empty ⇒ no block at all, and no stray blank lines: the
    /// prompt must be byte-identical to the no-bootstrap prompt.
    #[test]
    fn bootstrap_both_parts_empty_emits_no_block() {
        let empty = snapshot(&[], None);
        assert!(render_bootstrap(&empty).is_none());
        let p = build_voice_prompt(
            &genome(),
            "DIRECTIVE",
            render_bootstrap(&empty).as_deref(),
            None,
            RelationshipScope::None,
        );
        assert_eq!(p, "You are Mia.\n\nDIRECTIVE");
        // A whitespace-only prev_call is empty too.
        let blank = snapshot(&["  "], Some("   \n "));
        assert!(render_bootstrap(&blank).is_none());
    }

    // ─── prev-call transcript rendering ─────────────────────────────────

    #[test]
    fn prev_call_transcript_maps_roles_and_skips_noise() {
        let rows = vec![
            slim("user", "你好"),
            slim("assistant", "嗨呀"),
            slim("assistant", "   "), // empty-content row: skipped
            slim("gift_user", "（打赏 $5）"),
            slim("system", "ignored"), // unknown role: skipped
            slim("user", "晚安"),
        ];
        assert_eq!(
            render_prev_call(&rows).unwrap(),
            "用户：你好\n她：嗨呀\n用户：（打赏 $5）\n用户：晚安"
        );
    }

    #[test]
    fn prev_call_transcript_none_when_nothing_renders() {
        assert!(render_prev_call(&[]).is_none());
        assert!(render_prev_call(&[slim("assistant", ""), slim("tool", "x")]).is_none());
    }

    // ─── bootstrap marker plan ──────────────────────────────────────────

    #[test]
    fn plan_is_assemble_when_marker_absent() {
        assert!(matches!(
            plan_bootstrap(&serde_json::json!({})),
            BootstrapPlan::Assemble
        ));
        assert!(matches!(
            plan_bootstrap(&serde_json::json!({ "is_demo": true })),
            BootstrapPlan::Assemble
        ));
    }

    #[test]
    fn plan_is_frozen_when_marker_well_formed() {
        let s = snapshot(&["城市：上海"], Some("用户：你好"));
        let meta = serde_json::json!({ "voice_bootstrap": serde_json::to_value(&s).unwrap() });
        match plan_bootstrap(&meta) {
            BootstrapPlan::Frozen(b) => {
                assert_eq!(b.insights, vec!["城市：上海".to_string()]);
                assert_eq!(b.prev_call.as_deref(), Some("用户：你好"));
            }
            other => panic!("expected Frozen, got {other:?}"),
        }
    }

    /// Malformed marker ⇒ inject nothing AND never rewrite (a distinct plan
    /// from `Assemble`, which would write).
    #[test]
    fn plan_is_malformed_on_garbage_marker() {
        for v in [
            serde_json::json!({ "voice_bootstrap": "not-an-object" }),
            serde_json::json!({ "voice_bootstrap": null }),
            serde_json::json!({ "voice_bootstrap": { "insights": "not-a-list" } }),
            serde_json::json!({ "voice_bootstrap": { "insights": [] } }), // no created_at
        ] {
            assert!(
                matches!(plan_bootstrap(&v), BootstrapPlan::Malformed),
                "expected Malformed for {v}"
            );
        }
    }

    #[test]
    fn fresh_affinity_gets_acquaintance_and_spark_line() {
        // bond 0 ⇒ tier 1 Acquaintance; chemistry 0 ⇒ tier 1 Spark.
        let a = affinity_at(0.0, 0.0, 0.0, 0.0, 0.0);
        let p = build_voice_prompt(
            &genome(),
            "DIRECTIVE",
            None,
            Some(&a),
            RelationshipScope::default(),
        );
        assert!(p.contains("still getting to know each other"));
        assert!(p.contains("faint, unspoken spark"));
    }

    #[test]
    fn high_bond_low_chemistry_keeps_romance_restrained() {
        // bond (0.9+0.9+0.9)/3 = 0.9 ⇒ tier 5 Soulmate;
        // chemistry (0.9+0+0)/3 = 0.3 ⇒ tier 2 Flirtation.
        let a = affinity_at(0.9, 0.9, 0.9, 0.0, 0.0);
        let p = build_voice_prompt(
            &genome(),
            "DIRECTIVE",
            None,
            Some(&a),
            RelationshipScope::default(),
        );
        assert!(p.contains("know each other inside out"));
        assert!(p.contains("do not lean into romance or seduction yet"));
        assert!(!p.contains("growing attraction"));
        assert!(!p.contains("quietly seductive"));
        assert!(!p.contains("deeply in love"));
    }

    #[test]
    fn high_chemistry_appends_affectionate_clause() {
        // bond (0.9+0.2+0.2)/3 ≈ 0.433 ⇒ tier 3 CloseFriend;
        // chemistry (0.9+0.9+0.9)/3 = 0.9 ⇒ tier 5 Beloved.
        let a = affinity_at(0.9, 0.2, 0.2, 0.9, 0.9);
        let p = build_voice_prompt(
            &genome(),
            "DIRECTIVE",
            None,
            Some(&a),
            RelationshipScope::default(),
        );
        assert!(p.contains("close friends"));
        assert!(p.contains("deeply in love"));
    }

    #[test]
    fn chemistry_boundary_switches_clause_at_crush() {
        // (0+0.52+0.50)/3 ≈ 0.34 ⇒ tier 2: still the subtle clause.
        let low = affinity_at(0.0, 0.0, 0.0, 0.52, 0.50);
        let p_low = build_voice_prompt(
            &genome(),
            "D",
            None,
            Some(&low),
            RelationshipScope::default(),
        );
        assert!(p_low.contains("faint, unspoken spark"));
        assert!(!p_low.contains("growing attraction"));
        // (0+0.54+0.54)/3 = 0.36 ⇒ tier 3: switches to the Crush clause.
        let high = affinity_at(0.0, 0.0, 0.0, 0.54, 0.54);
        let p_high = build_voice_prompt(
            &genome(),
            "D",
            None,
            Some(&high),
            RelationshipScope::default(),
        );
        assert!(p_high.contains("growing attraction"));
        assert!(!p_high.contains("faint, unspoken spark"));
    }

    #[test]
    fn scope_none_suppresses_line() {
        let a = affinity_at(0.9, 0.2, 0.2, 0.9, 0.9);
        let p = build_voice_prompt(
            &genome(),
            "DIRECTIVE",
            None,
            Some(&a),
            RelationshipScope::None,
        );
        assert_eq!(p, "You are Mia.\n\nDIRECTIVE");
    }

    #[test]
    fn scope_bond_emits_base_only() {
        let a = affinity_at(0.9, 0.2, 0.2, 0.9, 0.9); // CloseFriend / Beloved
        let p = build_voice_prompt(
            &genome(),
            "DIRECTIVE",
            None,
            Some(&a),
            RelationshipScope::Bond,
        );
        assert!(p.contains("close friends"));
        assert!(!p.contains("deeply in love"));
    }

    #[test]
    fn scope_chemistry_emits_clause_only() {
        let a = affinity_at(0.9, 0.2, 0.2, 0.9, 0.9);
        let p = build_voice_prompt(
            &genome(),
            "DIRECTIVE",
            None,
            Some(&a),
            RelationshipScope::Chemistry,
        );
        assert!(!p.contains("close friends"));
        assert!(p.contains("deeply in love"));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_streams_delta_then_done_and_persists(pool: sqlx::PgPool) {
        use eros_engine_llm::model_config::ModelConfig;
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3},\"id\":\"gen-v\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // Seed persona + instance + session.
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('V', 'You are V.', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // Persist the user turn as the route would.
        let repo = ChatRepo { pool: &pool };
        let umid = match repo
            .insert_voice_user_message(session_id, "hello", "01J9000000000000000000VOICE")
            .await
            .unwrap()
        {
            eros_engine_store::chat::VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        // State with a chat_voice task + mock OpenRouter.
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = Arc::new(
            ModelConfig::from_toml_str(
                "[tasks.chat_voice]\nmodel = \"primary\"\nmax_tokens = 100\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let resolved = state.model_config.resolve_voice().unwrap();
        let frames: Vec<ProtocolFrame> = run_voice_turn(
            Arc::new(state),
            VoiceTurn {
                session_id,
                instance_id,
                user_id,
                user_message_id: umid,
                relationship_scope: RelationshipScope::default(),
                memory_scope: MemoryScope::default(),
                session_metadata: serde_json::json!({}),
            },
            resolved,
        )
        .collect()
        .await;

        // delta(s) carry the text; terminal frame is Done; no Error.
        let text: String = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hi there");
        assert!(matches!(frames.last(), Some(ProtocolFrame::Done { .. })));
        assert!(!frames
            .iter()
            .any(|f| matches!(f, ProtocolFrame::Error { .. })));

        // Assistant row persisted on the voice channel.
        let (content, channel): (String, Option<String>) = sqlx::query_as(
            "SELECT content, channel FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(content, "hi there");
        assert_eq!(channel.as_deref(), Some("voice"));

        // Resolved scope audited on the assistant row.
        let scope_meta: Option<String> = sqlx::query_scalar(
            "SELECT metadata->>'relationship_scope' FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(scope_meta.as_deref(), Some("both"));
    }

    /// Task 0 (affinity dead-row fix): `companion_affinity` is session-keyed
    /// and populated only by the text pipeline, so a voice-channel session's
    /// own `session_id` never has a row. The turn here runs on a voice
    /// session that has none of its own, while an earlier TEXT session for
    /// the same user × instance pair does — proving the relationship line
    /// resolves via the pair, not the (affinity-less) voice session_id.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_renders_relationship_line_from_text_session_affinity(
        pool: sqlx::PgPool,
    ) {
        use eros_engine_llm::model_config::ModelConfig;
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}],\"id\":\"gen-v\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // Seed persona + instance.
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('V', 'You are V.', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();

        // A prior TEXT session for this pair, with its own affinity row —
        // fresh-seed defaults land on Acquaintance/Spark (bond ≈ chemistry ≈
        // 0.033, same tiers `fresh_affinity_gets_acquaintance_and_spark_line`
        // asserts on for those exact phrases).
        let text_session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, channel) \
             VALUES ($1, $2, 'text') RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        AffinityRepo { pool: &pool }
            .load_or_create(text_session_id, user_id, instance_id)
            .await
            .unwrap();

        // The voice session this turn actually runs on — no affinity row of
        // its own.
        let voice_session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, channel) \
             VALUES ($1, $2, 'voice') RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // Persist the user turn as the route would.
        let repo = ChatRepo { pool: &pool };
        let umid = match repo
            .insert_voice_user_message(voice_session_id, "hello", "01J9000000000000000000VOIC7")
            .await
            .unwrap()
        {
            eros_engine_store::chat::VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        // State with a chat_voice task + mock OpenRouter.
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = Arc::new(
            ModelConfig::from_toml_str(
                "[tasks.chat_voice]\nmodel = \"primary\"\nmax_tokens = 100\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let resolved = state.model_config.resolve_voice().unwrap();
        let frames: Vec<ProtocolFrame> = run_voice_turn(
            Arc::new(state),
            VoiceTurn {
                session_id: voice_session_id,
                instance_id,
                user_id,
                user_message_id: umid,
                relationship_scope: RelationshipScope::default(),
                memory_scope: MemoryScope::default(),
                session_metadata: serde_json::json!({}),
            },
            resolved,
        )
        .collect()
        .await;

        assert!(matches!(frames.last(), Some(ProtocolFrame::Done { .. })));
        assert!(!frames
            .iter()
            .any(|f| matches!(f, ProtocolFrame::Error { .. })));

        // The outbound system prompt must carry the relationship line
        // derived from the TEXT session's affinity row — proof the voice
        // path resolved it by pair, not by its own (nonexistent) row.
        let received = mock
            .received_requests()
            .await
            .expect("recording enabled by default");
        assert_eq!(received.len(), 1);
        let req_body = String::from_utf8_lossy(&received[0].body);
        assert!(
            req_body.contains("still getting to know each other"),
            "expected the bond relationship line in the outbound request; body={req_body}"
        );
        assert!(
            req_body.contains("faint, unspoken spark"),
            "expected the chemistry relationship line in the outbound request; body={req_body}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_empty_completion_is_error(pool: sqlx::PgPool) {
        use eros_engine_llm::model_config::ModelConfig;
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        // Metadata-only frame (no content delta) then a clean [DONE] — the
        // stream ends without ever producing text.
        let body = "\
data: {\"choices\":[{\"delta\":{}}],\"id\":\"gen-e\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // Seed persona + instance + session.
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('V', 'You are V.', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // Persist the user turn as the route would.
        let repo = ChatRepo { pool: &pool };
        let umid = match repo
            .insert_voice_user_message(session_id, "hello", "01J9000000000000000000VOIC3")
            .await
            .unwrap()
        {
            eros_engine_store::chat::VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        // State with a chat_voice task + mock OpenRouter.
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = Arc::new(
            ModelConfig::from_toml_str(
                "[tasks.chat_voice]\nmodel = \"primary\"\nmax_tokens = 100\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let resolved = state.model_config.resolve_voice().unwrap();
        let frames: Vec<ProtocolFrame> = run_voice_turn(
            Arc::new(state),
            VoiceTurn {
                session_id,
                instance_id,
                user_id,
                user_message_id: umid,
                relationship_scope: RelationshipScope::default(),
                memory_scope: MemoryScope::default(),
                session_metadata: serde_json::json!({}),
            },
            resolved,
        )
        .collect()
        .await;

        // An empty completion must yield an Error frame, never a Done.
        assert!(
            frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "expected an Error frame, got {frames:?}"
        );
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Done { .. })),
            "must not emit Done on an empty completion; got {frames:?}"
        );

        // No assistant row persisted.
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            n, 0,
            "no assistant row should be persisted on empty completion"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_falls_back_to_content_on_empty_primary(pool: sqlx::PgPool) {
        use eros_engine_llm::model_config::ModelConfig;
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // First request (PRIMARY) — metadata-only + clean [DONE]: an empty
        // completion. Limited to one match so the SECOND request (the
        // fallback candidate) falls through to the content mock below.
        let empty_body = "\
data: {\"choices\":[{\"delta\":{}}],\"id\":\"gen-empty\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(empty_body, "text/event-stream"),
            )
            .up_to_n_times(1)
            .mount(&mock)
            .await;

        // Second request onward (the fallback candidate) — normal content.
        let content_body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3},\"id\":\"gen-backup\",\"model\":\"backup\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(content_body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // Seed persona + instance + session.
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('V', 'You are V.', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // Persist the user turn as the route would.
        let repo = ChatRepo { pool: &pool };
        let umid = match repo
            .insert_voice_user_message(session_id, "hello", "01J9000000000000000000VOIC4")
            .await
            .unwrap()
        {
            eros_engine_store::chat::VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        // State with a chat_voice task configured with a fallback model, +
        // mock OpenRouter.
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = Arc::new(
            ModelConfig::from_toml_str(
                "[tasks.chat_voice]\nmodel = \"primary\"\nfallback = [\"backup\"]\nmax_tokens = 100\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let resolved = state.model_config.resolve_voice().unwrap();
        assert_eq!(resolved.fallback_model, vec!["backup".to_string()]);
        let frames: Vec<ProtocolFrame> = run_voice_turn(
            Arc::new(state),
            VoiceTurn {
                session_id,
                instance_id,
                user_id,
                user_message_id: umid,
                relationship_scope: RelationshipScope::default(),
                memory_scope: MemoryScope::default(),
                session_metadata: serde_json::json!({}),
            },
            resolved,
        )
        .collect()
        .await;

        // The empty PRIMARY must not surface as an error — the fallback
        // candidate's content wins: a Delta carrying "recovered", a terminal
        // Done, and no Error frame.
        let text: String = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "recovered");
        assert!(matches!(frames.last(), Some(ProtocolFrame::Done { .. })));
        assert!(!frames
            .iter()
            .any(|f| matches!(f, ProtocolFrame::Error { .. })));

        // Assistant row persisted with the fallback's content.
        let (content, channel): (String, Option<String>) = sqlx::query_as(
            "SELECT content, channel FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(content, "recovered");
        assert_eq!(channel.as_deref(), Some("voice"));

        // Sanity: both candidates were actually hit (one empty, one content).
        let received = mock
            .received_requests()
            .await
            .expect("recording enabled by default");
        assert_eq!(received.len(), 2, "expected primary + fallback requests");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_skips_empty_content_history_rows(pool: sqlx::PgPool) {
        use eros_engine_llm::model_config::ModelConfig;
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3},\"id\":\"gen-v\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // Seed persona + instance + session.
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('V', 'You are V.', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // A stray empty-content assistant row (e.g. a caption-less image turn
        // from a mixed session) landing BEFORE the voice user turn. It must be
        // skipped in the wire-message mapping, never sent upstream.
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content) \
             VALUES ($1, 'assistant', '')",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        // Persist the user turn as the route would.
        let repo = ChatRepo { pool: &pool };
        let umid = match repo
            .insert_voice_user_message(session_id, "hello", "01J9000000000000000000VOIC2")
            .await
            .unwrap()
        {
            eros_engine_store::chat::VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        // State with a chat_voice task + mock OpenRouter.
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = Arc::new(
            ModelConfig::from_toml_str(
                "[tasks.chat_voice]\nmodel = \"primary\"\nmax_tokens = 100\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let resolved = state.model_config.resolve_voice().unwrap();
        let frames: Vec<ProtocolFrame> = run_voice_turn(
            Arc::new(state),
            VoiceTurn {
                session_id,
                instance_id,
                user_id,
                user_message_id: umid,
                relationship_scope: RelationshipScope::default(),
                memory_scope: MemoryScope::default(),
                session_metadata: serde_json::json!({}),
            },
            resolved,
        )
        .collect()
        .await;

        // Stream still completes cleanly: delta(s) carry the text, terminal
        // frame is Done, no Error — the empty history row must not break the
        // turn.
        let text: String = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hi there");
        assert!(matches!(frames.last(), Some(ProtocolFrame::Done { .. })));
        assert!(!frames
            .iter()
            .any(|f| matches!(f, ProtocolFrame::Error { .. })));

        // The outgoing request body must NOT contain the empty-content row —
        // proof the skip guard actually dropped it from the wire mapping.
        let received = mock
            .received_requests()
            .await
            .expect("recording enabled by default");
        assert!(
            !received.is_empty(),
            "expected at least one upstream request"
        );
        for req in &received {
            let req_body = String::from_utf8_lossy(&req.body);
            assert!(
                !req_body.contains("\"content\":\"\""),
                "request body must not contain an empty-content message; body={req_body}",
            );
        }
    }

    /// Codex P2 (r5): the `Done` frame's usage must have deployment-hidden keys
    /// (e.g. `cost`) stripped, while the persisted assistant row keeps the FULL
    /// unfiltered usage — mirrors the text/replay paths' `filter_usage_keys` use.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_done_filters_hidden_usage_keys(pool: sqlx::PgPool) {
        use eros_engine_llm::model_config::ModelConfig;
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3,\"cost\":0.01},\"id\":\"gen-v\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // Seed persona + instance + session.
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('V', 'You are V.', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // Persist the user turn as the route would.
        let repo = ChatRepo { pool: &pool };
        let umid = match repo
            .insert_voice_user_message(session_id, "hello", "01J9000000000000000000VOIC5")
            .await
            .unwrap()
        {
            eros_engine_store::chat::VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        // State with a chat_voice task + mock OpenRouter, and `cost` configured
        // as a deployment-hidden usage key.
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = Arc::new(
            ModelConfig::from_toml_str(
                "[tasks.chat_voice]\nmodel = \"primary\"\nmax_tokens = 100\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        state.config.openrouter_usage_hidden_keys =
            std::collections::HashSet::from(["cost".to_string()]);

        let resolved = state.model_config.resolve_voice().unwrap();
        let frames: Vec<ProtocolFrame> = run_voice_turn(
            Arc::new(state),
            VoiceTurn {
                session_id,
                instance_id,
                user_id,
                user_message_id: umid,
                relationship_scope: RelationshipScope::default(),
                memory_scope: MemoryScope::default(),
                session_metadata: serde_json::json!({}),
            },
            resolved,
        )
        .collect()
        .await;

        let usage = frames
            .iter()
            .find_map(|f| match f {
                ProtocolFrame::Done { usage, .. } => Some(usage.clone()),
                _ => None,
            })
            .expect("a Done frame")
            .expect("usage present");
        assert!(
            usage.get("cost").is_none(),
            "cost must be stripped from the wire Done frame; got {usage}"
        );
        assert_eq!(usage["prompt_tokens"], 1);
        assert_eq!(usage["total_tokens"], 3);

        // The persisted row keeps the FULL unfiltered usage, incl. `cost`.
        let persisted_usage: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT usage FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let persisted_usage = persisted_usage.expect("usage persisted");
        assert_eq!(
            persisted_usage["cost"], 0.01,
            "DB row must keep the FULL unfiltered usage; got {persisted_usage}"
        );
    }

    /// Issue #188 item 1: `content_filter` is a mid-generation safety cut —
    /// the text is incomplete, so the voice path must mark the turn truncated
    /// exactly like `length` (parity with the text pipeline's handling).
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_content_filter_finish_marks_truncated(pool: sqlx::PgPool) {
        use eros_engine_llm::model_config::ModelConfig;
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"cut short\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"content_filter\"}],\"id\":\"gen-cf\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // Seed persona + instance + session.
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('V', 'You are V.', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let repo = ChatRepo { pool: &pool };
        let umid = match repo
            .insert_voice_user_message(session_id, "hello", "01J9000000000000000000VOIC8")
            .await
            .unwrap()
        {
            eros_engine_store::chat::VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = Arc::new(
            ModelConfig::from_toml_str(
                "[tasks.chat_voice]\nmodel = \"primary\"\nmax_tokens = 100\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let resolved = state.model_config.resolve_voice().unwrap();
        let frames: Vec<ProtocolFrame> = run_voice_turn(
            Arc::new(state),
            VoiceTurn {
                session_id,
                instance_id,
                user_id,
                user_message_id: umid,
                relationship_scope: RelationshipScope::default(),
                memory_scope: MemoryScope::default(),
                session_metadata: serde_json::json!({}),
            },
            resolved,
        )
        .collect()
        .await;

        let truncated = frames
            .iter()
            .find_map(|f| match f {
                ProtocolFrame::Done { truncated, .. } => Some(*truncated),
                _ => None,
            })
            .expect("a Done frame");
        assert!(
            truncated,
            "content_filter finish must mark the voice turn truncated; frames={frames:?}"
        );
    }

    /// Codex P2 (r5): when a primary candidate emits terminal metadata (usage /
    /// generation_id / a `length` finish) but NO content and the loop falls
    /// through to a fallback, that abandoned metadata must never leak onto the
    /// fallback's successful reply.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_fallback_does_not_inherit_primary_metadata(pool: sqlx::PgPool) {
        use eros_engine_llm::model_config::ModelConfig;
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // First request (PRIMARY) — metadata-only, a `length` finish, usage +
        // generation_id, but NO content: an empty completion that also happens
        // to carry terminal metadata. Limited to one match so the SECOND
        // request (the fallback candidate) falls through to the content mock.
        let primary_body = "\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":0,\"total_tokens\":9},\"id\":\"gen-primary\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(primary_body, "text/event-stream"),
            )
            .up_to_n_times(1)
            .mount(&mock)
            .await;

        // Second request onward (the fallback candidate) — plain content, no
        // usage/id/model of its own.
        let fallback_body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(fallback_body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // Seed persona + instance + session.
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('V', 'You are V.', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // Persist the user turn as the route would.
        let repo = ChatRepo { pool: &pool };
        let umid = match repo
            .insert_voice_user_message(session_id, "hello", "01J9000000000000000000VOIC6")
            .await
            .unwrap()
        {
            eros_engine_store::chat::VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        // State with a chat_voice task configured with a fallback model, +
        // mock OpenRouter.
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = Arc::new(
            ModelConfig::from_toml_str(
                "[tasks.chat_voice]\nmodel = \"primary\"\nfallback = [\"backup\"]\nmax_tokens = 100\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let resolved = state.model_config.resolve_voice().unwrap();
        assert_eq!(resolved.fallback_model, vec!["backup".to_string()]);
        let frames: Vec<ProtocolFrame> = run_voice_turn(
            Arc::new(state),
            VoiceTurn {
                session_id,
                instance_id,
                user_id,
                user_message_id: umid,
                relationship_scope: RelationshipScope::default(),
                memory_scope: MemoryScope::default(),
                session_metadata: serde_json::json!({}),
            },
            resolved,
        )
        .collect()
        .await;

        let text: String = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "recovered");

        let done = frames
            .iter()
            .find_map(|f| match f {
                ProtocolFrame::Done {
                    truncated,
                    generation_id,
                    ..
                } => Some((*truncated, generation_id.clone())),
                _ => None,
            })
            .expect("a Done frame");
        assert!(
            !done.0,
            "truncated must not inherit the abandoned primary's `length` finish"
        );
        assert_ne!(
            done.1,
            Some("gen-primary".to_string()),
            "generation_id must not inherit the abandoned primary's id"
        );
        assert_eq!(
            done.1, None,
            "the successful fallback carried no generation_id of its own"
        );

        // The persisted row carries the fallback's content — proof the earlier
        // primary's abandoned metadata never reached persistence either.
        let content: String = sqlx::query_scalar(
            "SELECT content FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(content, "recovered");
    }

    // ─── bootstrap snapshot, end to end ─────────────────────────────────
    //
    // Shared scaffolding for the bootstrap e2e tests: one always-succeeding
    // mock, and a `run_bootstrap_turn` that mirrors the route (load the
    // session row → persist the user turn → drive the pipeline with that
    // row's metadata), so "the marker written by turn 1 is what turn 2 sees"
    // is exercised the way production reaches it.

    async fn bootstrap_mock() -> wiremock::MockServer {
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"id\":\"gen-b\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;
        mock
    }

    async fn seed_instance(pool: &sqlx::PgPool, user_id: Uuid) -> Uuid {
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('V', 'You are V.', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id)
        .bind(user_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn seed_session(pool: &sqlx::PgPool, user_id: Uuid, instance_id: Uuid, ch: &str) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, channel) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .bind(ch)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// A message `mins_ago` minutes old, so ordering is deterministic.
    async fn seed_message(
        pool: &sqlx::PgPool,
        session_id: Uuid,
        role: &str,
        content: &str,
        mins_ago: i32,
    ) {
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, channel, sent_at) \
             VALUES ($1, $2, $3, 'voice', now() - make_interval(mins => $4))",
        )
        .bind(session_id)
        .bind(role)
        .bind(content)
        .bind(mins_ago)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_insights(pool: &sqlx::PgPool, user_id: Uuid, insights: serde_json::Value) {
        eros_engine_store::human_insight::HumanInsightRepo { pool }
            .project_from_insights(user_id, &insights)
            .await
            .unwrap();
    }

    /// `metadata->'voice_bootstrap'`; `None` when the marker is absent.
    async fn read_marker(pool: &sqlx::PgPool, session_id: Uuid) -> Option<serde_json::Value> {
        sqlx::query_scalar(
            "SELECT metadata->'voice_bootstrap' FROM engine.chat_sessions WHERE id = $1",
        )
        .bind(session_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn last_request_body(mock: &wiremock::MockServer) -> serde_json::Value {
        let received = mock
            .received_requests()
            .await
            .expect("recording enabled by default");
        let last = received.last().expect("at least one upstream request");
        serde_json::from_slice(&last.body).expect("JSON request body")
    }

    fn system_prompt_of(body: &serde_json::Value) -> String {
        let m = &body["messages"][0];
        assert_eq!(
            m["role"], "system",
            "first wire message must be the system prompt"
        );
        m["content"].as_str().expect("system content").to_string()
    }

    /// One turn, exactly as the route drives it.
    async fn run_bootstrap_turn(
        pool: &sqlx::PgPool,
        mock: &wiremock::MockServer,
        session_id: Uuid,
        instance_id: Uuid,
        user_id: Uuid,
        client_msg_id: &str,
        memory_scope: MemoryScope,
    ) -> Vec<ProtocolFrame> {
        use eros_engine_llm::model_config::ModelConfig;
        use futures_util::StreamExt;

        let repo = ChatRepo { pool };
        // The route loads the full session row (metadata included) BEFORE the
        // user insert and hands `metadata` to the pipeline — mirror that.
        let session = repo
            .get_session(session_id)
            .await
            .unwrap()
            .expect("session exists");
        let umid = match repo
            .insert_voice_user_message(session_id, "hello", client_msg_id)
            .await
            .unwrap()
        {
            eros_engine_store::chat::VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = Arc::new(
            ModelConfig::from_toml_str(
                "[tasks.chat_voice]\nmodel = \"primary\"\nmax_tokens = 100\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        let resolved = state.model_config.resolve_voice().unwrap();
        run_voice_turn(
            Arc::new(state),
            VoiceTurn {
                session_id,
                instance_id,
                user_id,
                user_message_id: umid,
                relationship_scope: RelationshipScope::None,
                memory_scope,
                session_metadata: session.metadata,
            },
            resolved,
        )
        .collect()
        .await
    }

    fn assert_no_error_frame(frames: &[ProtocolFrame]) {
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "a memory path must never emit an error frame; got {frames:?}"
        );
    }

    /// First turn freezes the snapshot into `metadata.voice_bootstrap` and
    /// injects it: insights at the default (Neutral) tier + the previous
    /// call's tail.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_first_turn_freezes_and_injects_bootstrap(pool: sqlx::PgPool) {
        let mock = bootstrap_mock().await;
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;

        seed_insights(
            &pool,
            user_id,
            serde_json::json!({ "city": "上海", "occupation": "设计师", "love_values": "慢热" }),
        )
        .await;

        // A previous call (sibling voice session) with a two-message tail.
        let sibling = seed_session(&pool, user_id, instance_id, "voice").await;
        seed_message(&pool, sibling, "user", "你好", 20).await;
        seed_message(&pool, sibling, "assistant", "嗨呀", 19).await;

        let session_id = seed_session(&pool, user_id, instance_id, "voice").await;
        let frames = run_bootstrap_turn(
            &pool,
            &mock,
            session_id,
            instance_id,
            user_id,
            "01J9BOOT0000000000000001",
            MemoryScope::default(),
        )
        .await;
        assert_no_error_frame(&frames);

        // Frozen in the session row, RENDERED.
        let marker = read_marker(&pool, session_id)
            .await
            .expect("marker written");
        assert_eq!(
            marker["insights"],
            serde_json::json!(["城市：上海", "职业：设计师"]),
            "default scope ⇒ Neutral tier (no 感情观); got {marker}"
        );
        assert_eq!(
            marker["prev_call"],
            serde_json::json!("用户：你好\n她：嗨呀")
        );
        assert_eq!(marker["prev_session_id"], serde_json::json!(sibling));
        assert!(
            marker["created_at"].is_string(),
            "created_at recorded: {marker}"
        );

        // …and injected into this very turn's system prompt.
        let sys = system_prompt_of(&last_request_body(&mock).await);
        assert!(
            sys.contains("[关于他]\n- 城市：上海\n- 职业：设计师"),
            "insights sub-block missing: {sys}"
        );
        assert!(
            sys.contains("[上次通话]\n用户：你好\n她：嗨呀"),
            "prev-call sub-block missing: {sys}"
        );
        assert!(
            !sys.contains("感情观"),
            "Neutral tier must not leak intimate fields: {sys}"
        );
    }

    /// The snapshot is frozen: a later turn re-injects the stored strings and
    /// never re-reads (or rewrites) live insight data.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_second_turn_reuses_frozen_snapshot(pool: sqlx::PgPool) {
        let mock = bootstrap_mock().await;
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;
        seed_insights(&pool, user_id, serde_json::json!({ "city": "上海" })).await;
        let session_id = seed_session(&pool, user_id, instance_id, "voice").await;

        let frames = run_bootstrap_turn(
            &pool,
            &mock,
            session_id,
            instance_id,
            user_id,
            "01J9BOOT0000000000000001",
            MemoryScope::default(),
        )
        .await;
        assert_no_error_frame(&frames);
        let first = read_marker(&pool, session_id)
            .await
            .expect("marker written");

        // Live data moves on mid-call — and must NOT reach the prompt.
        seed_insights(&pool, user_id, serde_json::json!({ "city": "北京" })).await;

        let frames = run_bootstrap_turn(
            &pool,
            &mock,
            session_id,
            instance_id,
            user_id,
            "01J9BOOT0000000000000002",
            MemoryScope::Full,
        )
        .await;
        assert_no_error_frame(&frames);

        let second = read_marker(&pool, session_id)
            .await
            .expect("marker still there");
        assert_eq!(second, first, "the snapshot must never be rewritten");

        let sys = system_prompt_of(&last_request_body(&mock).await);
        assert!(sys.contains("城市：上海"), "frozen bullet missing: {sys}");
        assert!(
            !sys.contains("北京"),
            "live insight data must not reach a later turn: {sys}"
        );
    }

    /// No previous call ⇒ insights-only snapshot, no `[上次通话]` label.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_bootstrap_without_sibling_is_insights_only(pool: sqlx::PgPool) {
        let mock = bootstrap_mock().await;
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;
        seed_insights(&pool, user_id, serde_json::json!({ "city": "上海" })).await;
        let session_id = seed_session(&pool, user_id, instance_id, "voice").await;

        let frames = run_bootstrap_turn(
            &pool,
            &mock,
            session_id,
            instance_id,
            user_id,
            "01J9BOOT0000000000000001",
            MemoryScope::default(),
        )
        .await;
        assert_no_error_frame(&frames);

        let marker = read_marker(&pool, session_id)
            .await
            .expect("marker written");
        assert_eq!(marker["insights"], serde_json::json!(["城市：上海"]));
        assert_eq!(marker["prev_call"], serde_json::Value::Null);
        assert_eq!(marker["prev_session_id"], serde_json::Value::Null);

        let sys = system_prompt_of(&last_request_body(&mock).await);
        assert!(sys.contains("[关于他]"));
        assert!(!sys.contains("[上次通话]"), "no sibling ⇒ no tail: {sys}");
    }

    /// Sibling selection: never this session, never a text session.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_bootstrap_sibling_skips_current_and_text_sessions(pool: sqlx::PgPool) {
        let mock = bootstrap_mock().await;
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;

        // The real previous call — oldest of the three, so "most recently
        // active" alone would NOT pick it.
        let voice_sibling = seed_session(&pool, user_id, instance_id, "voice").await;
        seed_message(&pool, voice_sibling, "user", "语音上次内容", 30).await;

        // A text session for the same pair, more recently active.
        let text_session = seed_session(&pool, user_id, instance_id, "text").await;
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
             VALUES ($1, 'user', '文字会话内容', now() - interval '5 minutes')",
        )
        .bind(text_session)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE engine.chat_sessions SET last_active_at = now() WHERE id = $1")
            .bind(text_session)
            .execute(&pool)
            .await
            .unwrap();

        // The session being bootstrapped, with prior content of its own.
        let session_id = seed_session(&pool, user_id, instance_id, "voice").await;
        seed_message(&pool, session_id, "user", "当前会话内容", 1).await;

        let frames = run_bootstrap_turn(
            &pool,
            &mock,
            session_id,
            instance_id,
            user_id,
            "01J9BOOT0000000000000001",
            MemoryScope::default(),
        )
        .await;
        assert_no_error_frame(&frames);

        let marker = read_marker(&pool, session_id)
            .await
            .expect("marker written");
        assert_eq!(marker["prev_session_id"], serde_json::json!(voice_sibling));
        let prev = marker["prev_call"].as_str().expect("a transcript");
        assert_eq!(prev, "用户：语音上次内容");
        assert!(
            !prev.contains("文字会话内容"),
            "text session leaked: {prev}"
        );
        assert!(!prev.contains("当前会话内容"), "self quoted: {prev}");
    }

    /// `memory_scope: "none"` on the FIRST turn ⇒ no insight part, frozen for
    /// the whole call (a later turn asking for `full` still gets none).
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_memory_scope_none_freezes_snapshot_without_insights(
        pool: sqlx::PgPool,
    ) {
        let mock = bootstrap_mock().await;
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;
        seed_insights(&pool, user_id, serde_json::json!({ "city": "上海" })).await;
        let sibling = seed_session(&pool, user_id, instance_id, "voice").await;
        seed_message(&pool, sibling, "user", "你好", 20).await;
        let session_id = seed_session(&pool, user_id, instance_id, "voice").await;

        let frames = run_bootstrap_turn(
            &pool,
            &mock,
            session_id,
            instance_id,
            user_id,
            "01J9BOOT0000000000000001",
            MemoryScope::None,
        )
        .await;
        assert_no_error_frame(&frames);

        let marker = read_marker(&pool, session_id)
            .await
            .expect("marker written");
        assert_eq!(
            marker["insights"],
            serde_json::json!([]),
            "InsightMode::Off ⇒ empty part, still a written marker; got {marker}"
        );
        assert_eq!(marker["prev_call"], serde_json::json!("用户：你好"));
        let sys = system_prompt_of(&last_request_body(&mock).await);
        assert!(
            !sys.contains("[关于他]"),
            "scope none injected insights: {sys}"
        );
        assert!(
            sys.contains("[上次通话]"),
            "prev call still injected: {sys}"
        );

        // Turn 2 asks for Full — the frozen snapshot still wins.
        let frames = run_bootstrap_turn(
            &pool,
            &mock,
            session_id,
            instance_id,
            user_id,
            "01J9BOOT0000000000000002",
            MemoryScope::Full,
        )
        .await;
        assert_no_error_frame(&frames);
        let sys = system_prompt_of(&last_request_body(&mock).await);
        assert!(
            !sys.contains("城市：上海"),
            "the first turn's scope decides the whole call: {sys}"
        );
    }

    /// The in-session window is 8 messages — asserted on the wire request.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_history_window_is_eight(pool: sqlx::PgPool) {
        let mock = bootstrap_mock().await;
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id, "voice").await;

        // 10 prior messages, oldest first; the turn's own user row makes 11.
        for i in 0..10i32 {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            seed_message(&pool, session_id, role, &format!("m{i}"), 20 - i).await;
        }

        let frames = run_bootstrap_turn(
            &pool,
            &mock,
            session_id,
            instance_id,
            user_id,
            "01J9BOOT0000000000000001",
            MemoryScope::default(),
        )
        .await;
        assert_no_error_frame(&frames);

        let body = last_request_body(&mock).await;
        let msgs = body["messages"].as_array().expect("messages array");
        assert_eq!(
            msgs.len(),
            9,
            "system + VOICE_HISTORY_WINDOW(8) messages; got {}",
            serde_json::to_string(&body["messages"]).unwrap()
        );
        assert_eq!(msgs[0]["role"], "system");
        let wire: Vec<&str> = msgs[1..]
            .iter()
            .map(|m| m["content"].as_str().unwrap())
            .collect();
        assert_eq!(
            wire,
            vec!["m3", "m4", "m5", "m6", "m7", "m8", "m9", "hello"]
        );
    }

    /// A marker that is present but unreadable: inject nothing, degrade with a
    /// warn, and NEVER overwrite it.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_malformed_bootstrap_marker_is_never_rewritten(pool: sqlx::PgPool) {
        let mock = bootstrap_mock().await;
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;
        seed_insights(&pool, user_id, serde_json::json!({ "city": "上海" })).await;
        let sibling = seed_session(&pool, user_id, instance_id, "voice").await;
        seed_message(&pool, sibling, "user", "你好", 20).await;
        let session_id = seed_session(&pool, user_id, instance_id, "voice").await;
        sqlx::query(
            "UPDATE engine.chat_sessions \
             SET metadata = '{\"voice_bootstrap\": \"garbage\"}'::jsonb WHERE id = $1",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        let frames = run_bootstrap_turn(
            &pool,
            &mock,
            session_id,
            instance_id,
            user_id,
            "01J9BOOT0000000000000001",
            MemoryScope::default(),
        )
        .await;
        assert_no_error_frame(&frames);

        assert_eq!(
            read_marker(&pool, session_id).await,
            Some(serde_json::json!("garbage")),
            "a write-once snapshot must never be clobbered, not even when unreadable"
        );
        let sys = system_prompt_of(&last_request_body(&mock).await);
        assert!(!sys.contains("[关于他]"), "{sys}");
        assert!(!sys.contains("[上次通话]"), "{sys}");
    }

    /// A failed part leaves the marker unwritten so the NEXT turn retries.
    /// The insights read is broken by renaming its table out from under the
    /// query (each `sqlx::test` gets its own database), then restored.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_failed_bootstrap_part_retries_next_turn(pool: sqlx::PgPool) {
        let mock = bootstrap_mock().await;
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;
        seed_insights(&pool, user_id, serde_json::json!({ "city": "上海" })).await;
        let sibling = seed_session(&pool, user_id, instance_id, "voice").await;
        seed_message(&pool, sibling, "user", "你好", 20).await;
        let session_id = seed_session(&pool, user_id, instance_id, "voice").await;

        sqlx::query("ALTER TABLE engine.human_insights RENAME TO human_insights_broken")
            .execute(&pool)
            .await
            .unwrap();
        let frames = run_bootstrap_turn(
            &pool,
            &mock,
            session_id,
            instance_id,
            user_id,
            "01J9BOOT0000000000000001",
            MemoryScope::default(),
        )
        .await;
        assert_no_error_frame(&frames);

        assert_eq!(
            read_marker(&pool, session_id).await,
            None,
            "a partial assembly must not freeze the snapshot"
        );
        // The part that DID load is still injected this turn.
        let sys = system_prompt_of(&last_request_body(&mock).await);
        assert!(sys.contains("[上次通话]\n用户：你好"), "{sys}");
        assert!(!sys.contains("[关于他]"), "{sys}");

        sqlx::query("ALTER TABLE engine.human_insights_broken RENAME TO human_insights")
            .execute(&pool)
            .await
            .unwrap();
        let frames = run_bootstrap_turn(
            &pool,
            &mock,
            session_id,
            instance_id,
            user_id,
            "01J9BOOT0000000000000002",
            MemoryScope::default(),
        )
        .await;
        assert_no_error_frame(&frames);

        let marker = read_marker(&pool, session_id)
            .await
            .expect("the retry wrote the marker");
        assert_eq!(marker["insights"], serde_json::json!(["城市：上海"]));
        let sys = system_prompt_of(&last_request_body(&mock).await);
        assert!(sys.contains("[关于他]\n- 城市：上海"), "{sys}");
    }
}
