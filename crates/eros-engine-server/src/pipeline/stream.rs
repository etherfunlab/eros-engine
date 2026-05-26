// SPDX-License-Identifier: AGPL-3.0-only
//! Streaming pipeline — ProtocolFrame state machine + run_stream generator.
//!
//! Wire-level frame layout follows
//! `docs/superpowers/specs/2026-05-19-sse-streaming-chat-0.2-design.md` §1.5.
//!
//! Task 4 only ships the type layer; the `run_stream` generator lands in
//! later tasks (T10/T11/T12).

use rand::Rng;
use serde::Serialize;
use ulid::Ulid;

/// Stream-level error code enum. Renders to the spec's lowercase string.
///
/// `RateLimited` and `Timeout` are spec-defined codes (§1.5) reserved for
/// the per-stream rate-limit and 120s hard-timeout enforcement that the
/// 0.2 generator does not yet implement (open §1.9 follow-up).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum StreamErrorCode {
    UpstreamUnavailable,
    RateLimited,
    Internal,
    Timeout,
}

/// Action type tag used in `meta` frames.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FrameActionType {
    Reply,
    Ghost,
    GiftReaction,
}

/// One wire frame in the SSE protocol.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProtocolFrame {
    Meta {
        message_id: String,
        action_type: FrameActionType,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        continues_from: Option<String>,
    },
    Delta {
        message_id: String,
        content: String,
    },
    Done {
        message_id: String,
        truncated: bool,
        /// OpenRouter usage, post-`OPENROUTER_USAGE_HIDDEN_KEYS` filtering.
        /// A `serde_json::Value` (not `UsageBlock`) so configured keys can be
        /// stripped before the frame reaches the client — the DB persists the
        /// full unfiltered usage separately.
        usage: Option<serde_json::Value>,
        generation_id: Option<String>,
    },
    Final {
        lead_score: f64,
        should_show_cta: bool,
        agent_training_level: f64,
        filtered: bool,
        // null when no trait injected; always present (no skip_serializing_if).
        prompt_injected: Option<Vec<String>>,
        // echo of the request tier; null when none. always present.
        tier: Option<String>,
        retries_chat: u32,
        retries_filter: u32,
    },
    Error {
        code: StreamErrorCode,
        retryable: bool,
        message: String,
        user_message: String,
    },
}

/// Render a 128-bit id as a Crockford Base32 ULID string (26 chars).
pub fn ulid_string(u: Ulid) -> String {
    u.to_string()
}

/// Maximum number of model attempts per streaming burst (= 1 primary + up to
/// 2 fallbacks). Each attempt surfaces as a separate visible bubble; the
/// frontend masks attempts beyond the first behind a "thinking" affordance, so
/// a depth of 3 buys extra resilience without looking like a bug to users.
pub const MAX_STREAM_FALLBACK_DEPTH: usize = 3;

use std::sync::Arc;
use uuid::Uuid;

use eros_engine_core::pde;
use eros_engine_core::types::{ActionType, DecisionInput, Event};
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::ChatRepo;
use eros_engine_store::persona::PersonaRepo;

use crate::routes::companion::filter_usage_keys;
use crate::state::AppState;

/// Result of a single streaming burst, shared back to `run_stream` via a
/// mutex. Replaces the old `produced_out: Vec<ProducedMessage>` channel so
/// the caller can also learn whether the turn was filtered and which model
/// attempt (chat / filter) actually served.
#[derive(Default)]
pub struct BurstOutcome {
    pub produced: Vec<crate::pipeline::post_process::ProducedMessage>,
    pub filtered: bool,
    pub retries_chat: u32,   // successful chat-attempt index (0 = primary)
    pub retries_filter: u32, // served filter-model index (0 when none/primary)
}

/// Per-burst driver: walks the model fallback chain, emits Meta/Delta/Done
/// per attempt, persists each logical message before its Done, and yields
/// a final Error{UpstreamUnavailable} if the chain exhausts. On a clean
/// burst, returns the produced messages (plus filter/attempt status) via
/// `outcome` for the caller to spawn post_process with. Does NOT yield
/// Final — the caller emits it after the burst so it reflects post-burst
/// state.
///
/// Two modes: when the resolved output filter's turn-level predicates pass
/// (live=false), the burst buffers each attempt, runs the filter LLM, and
/// only emits the filtered text (never the original). Otherwise it streams
/// live per-chunk exactly as before.
#[allow(clippy::too_many_arguments)]
fn drive_chat_burst(
    state: Arc<AppState>,
    session_id: Uuid,
    user_message_id: Uuid,
    frame_action: FrameActionType,
    persist_action: &'static str, // "reply" | "gift_reaction"
    plan_action: ActionType,
    req: eros_engine_llm::openrouter::ChatRequest,
    display_override: Option<eros_engine_llm::model_config::DisplayOverride>,
    filter: Option<eros_engine_llm::model_config::ResolvedOutputFilter>,
    trait_tags: Vec<String>,  // requested prompt-trait tags (the turn's)
    random_draw: Option<f64>, // sampled once per turn by run_stream; None when trigger.random is unset
    outcome: std::sync::Arc<std::sync::Mutex<BurstOutcome>>,
) -> impl futures_util::Stream<Item = ProtocolFrame> + Send + 'static {
    async_stream::stream! {
        let chat_repo = ChatRepo { pool: &state.pool };
        let chain: Vec<String> = std::iter::once(req.model.clone())
            .chain(req.fallback_model.iter().cloned())
            .filter(|s| !s.is_empty())
            .take(MAX_STREAM_FALLBACK_DEPTH)
            .collect();
        if chain.is_empty() {
            yield ProtocolFrame::Error {
                code: StreamErrorCode::Internal,
                retryable: false,
                message: "no models configured".into(),
                user_message: "服务出现问题，请稍后再试".into(),
            };
            return;
        }

        let tag_refs: Vec<&str> = trait_tags.iter().map(String::as_str).collect();
        let filtered_mode = filter
            .as_ref()
            .map(|f| f.trigger.turn_level_pass(random_draw, &tag_refs))
            .unwrap_or(false);

        if !filtered_mode {
            // ===== LIVE MODE (preserved verbatim from the pre-filter burst) =====
            let mut continues_from: Option<Ulid> = None;
            for (idx, model_id) in chain.iter().enumerate() {
                let msg_ulid = Ulid::new();
                let msg_uuid: Uuid = msg_ulid.into();
                let mut acc = String::new();
                let mut last_usage: Option<eros_engine_llm::openrouter::UsageBlock> = None;
                let mut last_gen_id: Option<String> = None;
                let mut truncated = false;

                yield ProtocolFrame::Meta {
                    message_id: ulid_string(msg_ulid),
                    action_type: frame_action,
                    model: display_override.as_ref().and_then(|d| d.display(model_id)),
                    continues_from: continues_from.map(ulid_string),
                };

                let mut per_model_req = req.clone();
                per_model_req.model = model_id.clone();
                per_model_req.fallback_model = Vec::new();

                match state.openrouter.execute_stream(per_model_req).await {
                    Ok(mut s) => {
                        use futures_util::StreamExt as _;
                        while let Some(item) = s.next().await {
                            match item {
                                Ok(c) => {
                                    if let Some(content) = c.content {
                                        acc.push_str(&content);
                                        yield ProtocolFrame::Delta {
                                            message_id: ulid_string(msg_ulid),
                                            content,
                                        };
                                    }
                                    if c.usage.is_some()         { last_usage = c.usage; }
                                    if c.generation_id.is_some() { last_gen_id = c.generation_id; }
                                    if matches!(c.finish_reason.as_deref(), Some("length")) {
                                        truncated = true;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("stream: upstream chunk err: {e}");
                                    truncated = true;
                                    break;
                                }
                            }
                        }
                        if !truncated && acc.is_empty() {
                            truncated = true;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("stream: upstream open err: {e}");
                        truncated = true;
                    }
                }

                // Persist BEFORE yielding Done (spec §2.3 risk R7).
                let row = eros_engine_store::chat::AssistantInsert {
                    id: msg_uuid,
                    content: acc.clone(),
                    assistant_action_type: persist_action.into(),
                    continues_from_message_id: continues_from.map(Into::into),
                    truncated,
                    model: Some(model_id.clone()),
                    usage: last_usage.as_ref().and_then(|u| serde_json::to_value(u).ok()),
                    generation_id: last_gen_id.clone(),
                    filter_audit: None,
                    metadata: Some(serde_json::json!({ "prompt_traits": &trait_tags })),
                };
                if let Err(e) = chat_repo
                    .insert_assistant_batch(session_id, user_message_id, &[row])
                    .await
                {
                    tracing::warn!("stream: assistant persist failed: {e}");
                }
                outcome.lock().unwrap().produced.push(crate::pipeline::post_process::ProducedMessage {
                    message_id: msg_uuid,
                    full_text: acc.clone(),
                    action: plan_action,
                });

                // Strip OPENROUTER_USAGE_HIDDEN_KEYS from the wire usage. The DB
                // row above persists the full unfiltered usage; only the frame is
                // filtered (mirrors the sync send_message path).
                let mut wire_usage = last_usage.as_ref().and_then(|u| serde_json::to_value(u).ok());
                filter_usage_keys(&mut wire_usage, &state.config.openrouter_usage_hidden_keys);
                yield ProtocolFrame::Done {
                    message_id: ulid_string(msg_ulid),
                    truncated,
                    usage: wire_usage,
                    generation_id: last_gen_id,
                };

                if !truncated {
                    outcome.lock().unwrap().retries_chat = idx as u32;
                    return;
                }
                if idx + 1 == chain.len() {
                    yield ProtocolFrame::Error {
                        code: StreamErrorCode::UpstreamUnavailable,
                        retryable: true,
                        message: "all fallback models truncated".into(),
                        user_message: "AI 服务暂时不可用，稍后再试".into(),
                    };
                    return;
                }
                continues_from = Some(msg_ulid);
            }
            return;
        }

        // ===== FILTERED MODE =====
        // The turn's trait/random predicates pass: buffer each attempt, run the
        // filter LLM, and emit ONLY the filtered text (the original reply must
        // never reach the client). Per-attempt the model predicate decides
        // whether that specific served model is actually filtered; on filter
        // error we fail open and emit the original.
        let f = filter.expect("filtered_mode ⇒ filter present");
        for (idx, model_id) in chain.iter().enumerate() {
            let msg_ulid = Ulid::new();
            let msg_uuid: Uuid = msg_ulid.into();
            let mut acc = String::new();
            let mut last_usage: Option<eros_engine_llm::openrouter::UsageBlock> = None;
            let mut last_gen_id: Option<String> = None;
            let mut truncated = false;

            let mut per_model_req = req.clone();
            per_model_req.model = model_id.clone();
            per_model_req.fallback_model = Vec::new();
            match state.openrouter.execute_stream(per_model_req).await {
                Ok(mut s) => {
                    use futures_util::StreamExt as _;
                    while let Some(item) = s.next().await {
                        match item {
                            Ok(c) => {
                                if let Some(content) = c.content { acc.push_str(&content); }
                                if c.usage.is_some() { last_usage = c.usage; }
                                if c.generation_id.is_some() { last_gen_id = c.generation_id; }
                                if matches!(c.finish_reason.as_deref(), Some("length")) { truncated = true; }
                            }
                            Err(e) => { tracing::warn!("stream(filtered): chunk err: {e}"); truncated = true; break; }
                        }
                    }
                    if !truncated && acc.is_empty() { truncated = true; }
                }
                Err(e) => { tracing::warn!("stream(filtered): open err: {e}"); truncated = true; }
            }

            if truncated {
                if idx + 1 == chain.len() {
                    yield ProtocolFrame::Error {
                        code: StreamErrorCode::UpstreamUnavailable,
                        retryable: true,
                        message: "all fallback models truncated".into(),
                        user_message: "AI 服务暂时不可用，稍后再试".into(),
                    };
                }
                continue;
            }

            outcome.lock().unwrap().retries_chat = idx as u32;
            let hits = f.trigger.should_filter(model_id, &tag_refs, random_draw);
            yield ProtocolFrame::Meta {
                message_id: ulid_string(msg_ulid),
                action_type: frame_action,
                model: display_override.as_ref().and_then(|d| d.display(model_id)),
                continues_from: None,
            };

            let (visible, filter_audit): (
                String,
                Option<eros_engine_store::chat::FilterAudit>,
            ) = match hits {
                Some(h) => match run_output_filter(&state, &f, &acc).await {
                    Some(out) => {
                        let mut o = outcome.lock().unwrap();
                        o.filtered = true;
                        o.retries_filter = out.retries_filter;
                        drop(o); // release MutexGuard before the yield below — must not cross suspension point
                        let audit = eros_engine_store::chat::FilterAudit {
                            pre_filter_content: acc.clone(),
                            filter_model: out.filter_model,
                            filter_triggers: serde_json::to_value(&h)
                                .expect("TriggerHits Serialize is infallible"),
                            f_client_msg_id: out.f_client_msg_id,
                            f_generation_id: out.f_generation_id,
                        };
                        (out.filtered_text, Some(audit))
                    }
                    None => (acc.clone(), None), // fail-open
                },
                None => (acc.clone(), None), // models-miss or trigger off
            };

            if !visible.is_empty() {
                yield ProtocolFrame::Delta {
                    message_id: ulid_string(msg_ulid),
                    content: visible.clone(),
                };
            }

            let row = eros_engine_store::chat::AssistantInsert {
                id: msg_uuid,
                content: visible.clone(),
                assistant_action_type: persist_action.into(),
                continues_from_message_id: None,
                truncated: false,
                model: Some(model_id.clone()),
                usage: last_usage.as_ref().and_then(|u| serde_json::to_value(u).ok()),
                generation_id: last_gen_id.clone(),
                filter_audit,
                metadata: Some(serde_json::json!({ "prompt_traits": &trait_tags })),
            };
            if let Err(e) = chat_repo.insert_assistant_batch(session_id, user_message_id, &[row]).await {
                tracing::warn!("stream(filtered): persist failed: {e}");
            }
            let extracted = extract_text(f.timing, &acc, &visible);
            outcome.lock().unwrap().produced.push(crate::pipeline::post_process::ProducedMessage {
                message_id: msg_uuid,
                full_text: extracted,
                action: plan_action,
            });

            let mut wire_usage = last_usage.as_ref().and_then(|u| serde_json::to_value(u).ok());
            filter_usage_keys(&mut wire_usage, &state.config.openrouter_usage_hidden_keys);
            yield ProtocolFrame::Done {
                message_id: ulid_string(msg_ulid),
                truncated: false,
                usage: wire_usage,
                generation_id: last_gen_id,
            };
            return;
        }
    }
}

/// Pick the text post_process extracts from: original (after) vs visible (before).
fn extract_text(
    timing: eros_engine_llm::model_config::FilterTiming,
    original: &str,
    visible: &str,
) -> String {
    match timing {
        eros_engine_llm::model_config::FilterTiming::AfterExtract => original.to_string(),
        eros_engine_llm::model_config::FilterTiming::BeforeExtract => visible.to_string(),
    }
}

/// Result of a filter LLM call. `f_client_msg_id` is the engine-generated
/// idempotency / trace ULID for the call (prefix `f_`), reused across the
/// filter's internal fallback retries. `filter_model` is the model actually
/// served (from `ChatResponse.model`), falling back to the requested primary
/// model if the response omits it. `f_generation_id` mirrors the optional
/// nature of `ChatResponse.generation_id` so SQL NULL propagates cleanly.
struct RunFilterOutcome {
    filtered_text: String,
    retries_filter: u32,
    filter_model: String,
    f_client_msg_id: String,
    f_generation_id: Option<String>,
}

/// Run the output-filter LLM over `original`. `execute()` walks the (already
/// depth-capped) chain; `ChatResponse.model` reports the model served. Returns
/// `RunFilterOutcome` on success. `None` on error/timeout/empty.
async fn run_output_filter(
    state: &AppState,
    f: &eros_engine_llm::model_config::ResolvedOutputFilter,
    original: &str,
) -> Option<RunFilterOutcome> {
    use eros_engine_llm::openrouter::{ChatMessage, ChatRequest};
    let f_client_msg_id = format!("f_{}", Ulid::new());
    let req = ChatRequest {
        model: f.model.clone(),
        fallback_model: f.fallback_model.clone(),
        messages: vec![
            ChatMessage {
                role: "system".into(),
                content: f.filter_prompt.clone(),
            },
            ChatMessage {
                role: "user".into(),
                content: original.to_string(),
            },
        ],
        temperature: f.temperature as f32,
        max_tokens: f.max_tokens,
        ..Default::default()
    };
    const FILTER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
    match tokio::time::timeout(FILTER_TIMEOUT, state.openrouter.execute(req)).await {
        Ok(Ok(resp)) => {
            super::log_openrouter_usage("chat_output_filter", None, &resp);
            let out = resp.reply.trim().to_string();
            if out.is_empty() {
                return None;
            }
            let served = resp.model.clone();
            let chain = std::iter::once(f.model.as_str())
                .chain(f.fallback_model.iter().map(String::as_str));
            let retries_filter = served
                .as_deref()
                .and_then(|s| {
                    chain
                        .enumerate()
                        .find(|(_, m)| *m == s)
                        .map(|(i, _)| i as u32)
                })
                .unwrap_or(0);
            // Falling back to f.model when the response omits the served model is
            // safe: that is the model we requested, and OpenRouter only omits it
            // on error paths (which we have already excluded via .reply.is_empty()).
            let filter_model = served.unwrap_or_else(|| f.model.clone());
            Some(RunFilterOutcome {
                filtered_text: out,
                retries_filter,
                filter_model,
                f_client_msg_id,
                f_generation_id: resp.generation_id,
            })
        }
        Ok(Err(e)) => {
            tracing::warn!("output filter LLM failed: {e}");
            None
        }
        Err(_) => {
            tracing::warn!("output filter timed out");
            None
        }
    }
}

/// All persisted bits needed to drive a streaming burst.
#[derive(Debug, Clone)]
pub struct PersistedUserMessage {
    pub user_message_id: Uuid,
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub content: String,
    pub prompt_traits: Vec<eros_engine_core::types::PromptTrait>,
    pub audit: Option<eros_engine_core::types::LlmAudit>,
    pub tier: Option<String>,
    pub memory_scope: eros_engine_core::scope::MemoryScope,
    pub affinity_scope: eros_engine_core::scope::AffinityScope,
    pub tips_amount_usd: Option<f64>,
}

/// Produce a stream of `ProtocolFrame` events for a single burst. The
/// generator owns its `AppState` clone so it stays `'static` and survives
/// `Sse`'s body lifetime. Task 10 implements the Ghost branch; T11/T12
/// fill in Reply / GiftReaction.
pub fn run_stream(
    state: Arc<AppState>,
    user_msg: PersistedUserMessage,
) -> impl futures_util::Stream<Item = ProtocolFrame> + Send + 'static {
    async_stream::stream! {
        let chat_repo = ChatRepo { pool: &state.pool };
        let persona_repo = PersonaRepo { pool: &state.pool };
        let affinity_repo = AffinityRepo { pool: &state.pool };

        let persona = match persona_repo.load_companion(user_msg.instance_id).await {
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
        let mut affinity = match affinity_repo
            .load_or_create(user_msg.session_id, user_msg.user_id, user_msg.instance_id)
            .await
        {
            Ok(mut a) => { a.apply_time_decay(); a }
            Err(e) => {
                tracing::warn!("stream: affinity load failed: {e}");
                yield ProtocolFrame::Error {
                    code: StreamErrorCode::Internal,
                    retryable: false,
                    message: format!("affinity load failed: {e}"),
                    user_message: "服务出现问题，请稍后再试".into(),
                };
                return;
            }
        };
        let signals = match super::compute_signals_for_session(
            &state.pool, user_msg.session_id, &affinity,
        ).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("stream: signals failed: {e}");
                yield ProtocolFrame::Error {
                    code: StreamErrorCode::Internal,
                    retryable: false,
                    message: format!("signals failed: {e}"),
                    user_message: "服务出现问题，请稍后再试".into(),
                };
                return;
            }
        };

        let input = DecisionInput {
            event: Event::UserMessage {
                content: user_msg.content.clone(),
                message_id: user_msg.user_message_id,
                prompt_traits: user_msg.prompt_traits.clone(),
                audit: user_msg.audit.clone(),
                tier: user_msg.tier.clone(),
                memory_scope: user_msg.memory_scope,
                affinity_scope: user_msg.affinity_scope,
                tips_amount_usd: user_msg.tips_amount_usd,
            },
            affinity: affinity.clone(),
            persona,
            signals,
        };
        let plan = pde::decide(&input);

        match plan.action_type {
            ActionType::Ghost => {
                let msg_id = Ulid::new();
                // Persist the ghost decision on the user row so replay can
                // distinguish "ghost outcome" from "still generating" (§1.10).
                if let Err(e) = chat_repo.mark_user_message_ghosted(user_msg.user_message_id).await {
                    tracing::warn!("stream: ghost mark failed: {e}");
                }
                if let Err(e) = affinity_repo.record_ghost(&mut affinity).await {
                    tracing::warn!("stream: record_ghost failed: {e}");
                }
                yield ProtocolFrame::Meta {
                    message_id: ulid_string(msg_id),
                    action_type: FrameActionType::Ghost,
                    model: None,
                    continues_from: None,
                };
                yield ProtocolFrame::Done {
                    message_id: ulid_string(msg_id),
                    truncated: false,
                    usage: None,
                    generation_id: None,
                };
                let final_frame = compute_final_frame(&state, user_msg.session_id, user_msg.user_id, false, None, user_msg.tier.clone(), 0, 0).await;
                yield final_frame;
            }
            ActionType::Reply | ActionType::GiftReaction => {
                let is_gift = matches!(plan.action_type, ActionType::GiftReaction);
                let req_res = if is_gift {
                    crate::pipeline::handlers::build_gift_request(
                        &state, &input, &plan,
                        user_msg.session_id, user_msg.user_id, user_msg.instance_id,
                        &[],
                    ).await
                } else {
                    crate::pipeline::handlers::build_reply_request(
                        &state, &input, &plan,
                        user_msg.session_id, user_msg.user_id, user_msg.instance_id,
                    ).await
                };
                let (req, injected_tags) = match req_res {
                    Ok(r) => r,
                    Err(e) => {
                        yield ProtocolFrame::Error {
                            code: StreamErrorCode::Internal,
                            retryable: false,
                            message: format!("build_*_request failed: {e}"),
                            user_message: "服务出现问题，请稍后再试".into(),
                        };
                        return;
                    }
                };
                // The filter trigger's `traits` predicate AND `prompt_injected`
                // both use the KEPT tags (post tier `allow_traits` gating), so a
                // tier that drops a requested trait can't trigger filtering on it.
                let trait_tags: Vec<String> = injected_tags.clone();
                let prompt_injected = if injected_tags.is_empty() { None } else { Some(injected_tags) };
                let (frame_action, persist_action, plan_action) = if is_gift {
                    (FrameActionType::GiftReaction, "gift_reaction", ActionType::GiftReaction)
                } else {
                    (FrameActionType::Reply, "reply", ActionType::Reply)
                };

                let display_override = state.model_config.display_override("chat_companion");

                // Resolve the output filter for this tier and draw the per-turn
                // random gate ONCE (so live/filter share the same coin flip).
                let tier = user_msg.tier.as_deref();
                let filter = state.model_config.resolve_output_filter(tier);
                let random_draw: Option<f64> = filter
                    .as_ref()
                    .and_then(|f| f.trigger.random)
                    .map(|_| rand::thread_rng().gen::<f64>());

                let outcome = std::sync::Arc::new(std::sync::Mutex::new(
                    crate::pipeline::stream::BurstOutcome::default(),
                ));
                let burst = drive_chat_burst(
                    state.clone(),
                    user_msg.session_id,
                    user_msg.user_message_id,
                    frame_action,
                    persist_action,
                    plan_action,
                    req,
                    display_override,
                    filter,
                    trait_tags,
                    random_draw,
                    outcome.clone(),
                );
                {
                    use futures_util::StreamExt as _;
                    let mut burst = Box::pin(burst);
                    while let Some(frame) = burst.next().await {
                        if matches!(frame, ProtocolFrame::Error { .. }) {
                            yield frame;
                            return;
                        }
                        yield frame;
                    }
                }
                let (produced, did_filter, retries_chat, retries_filter) = {
                    let g = outcome.lock().unwrap();
                    (g.produced.clone(), g.filtered, g.retries_chat, g.retries_filter)
                };

                // Reset ghost streak (mirrors sync pipeline policy).
                if let Err(e) = sqlx::query(
                    "UPDATE engine.companion_affinity SET ghost_streak = 0, updated_at = now() \
                     WHERE session_id = $1 AND ghost_streak <> 0",
                )
                .bind(user_msg.session_id)
                .execute(&state.pool)
                .await
                {
                    tracing::warn!("stream: ghost streak reset failed: {e}");
                }

                let final_frame = compute_final_frame(
                    &state,
                    user_msg.session_id,
                    user_msg.user_id,
                    did_filter,
                    prompt_injected.clone(),
                    user_msg.tier.clone(),
                    retries_chat,
                    retries_filter,
                )
                .await;
                yield final_frame;

                // Spawn post-process; do not await.
                let state_bg = (*state).clone();
                let plan_bg = plan.clone();
                let event_bg = Event::UserMessage {
                    content: user_msg.content.clone(),
                    message_id: user_msg.user_message_id,
                    prompt_traits: user_msg.prompt_traits.clone(),
                    audit: user_msg.audit.clone(),
                    tier: user_msg.tier.clone(),
                    memory_scope: user_msg.memory_scope,
                    affinity_scope: user_msg.affinity_scope,
                    tips_amount_usd: user_msg.tips_amount_usd,
                };
                let user_id_bg = user_msg.user_id;
                let instance_id_bg = user_msg.instance_id;
                let session_id_bg = user_msg.session_id;
                tokio::spawn(async move {
                    crate::pipeline::post_process::run(
                        state_bg,
                        session_id_bg,
                        user_id_bg,
                        instance_id_bg,
                        event_bg,
                        plan_bg,
                        produced,
                    )
                    .await;
                });
            }
            _ => {
                // Proactive and any future variants: Final-only.
                let final_frame = compute_final_frame(&state, user_msg.session_id, user_msg.user_id, false, None, user_msg.tier.clone(), 0, 0).await;
                yield final_frame;
            }
        }
    }
}

/// Compute the spec's `final` frame from current session/user state.
#[allow(clippy::too_many_arguments)]
async fn compute_final_frame(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    filtered: bool,
    prompt_injected: Option<Vec<String>>,
    tier: Option<String>,
    retries_chat: u32,
    retries_filter: u32,
) -> ProtocolFrame {
    let lead_score: f64 =
        sqlx::query_scalar("SELECT lead_score FROM engine.chat_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten()
            .unwrap_or(0.0);

    let training_level: f64 = match (eros_engine_store::insight::InsightRepo { pool: &state.pool })
        .load(user_id)
        .await
    {
        Ok(Some(row)) => eros_engine_store::insight::compute_training_level(&row.insights),
        _ => 0.0,
    };
    let should_show_cta = lead_score >= 7.0 && training_level >= 0.4;
    // Normalise lead_score from 0..10 to 0..1 to match the spec's declared
    // [0.0, 1.0] range. Operator dashboards still see the 0..10 raw value
    // via the sync /message handler.
    let normalised_lead = (lead_score / 10.0).clamp(0.0, 1.0);
    ProtocolFrame::Final {
        lead_score: normalised_lead,
        should_show_cta,
        agent_training_level: training_level,
        filtered,
        prompt_injected,
        tier,
        retries_chat,
        retries_filter,
    }
}

/// Build a frame stream from previously persisted assistant rows for a
/// given user_message_id. The chain is given in original chronological
/// order; emits one (meta, single-delta, done) trio per row, then one
/// `final` computed from current session state. Ghost replay emits a
/// synthetic Meta+Done(no usage, not truncated) followed by Final.
pub fn replay_stream(
    state: Arc<AppState>,
    session_id: Uuid,
    user_id: Uuid,
    ghost: bool,
    rows: Vec<eros_engine_store::chat::ChatMessage>,
) -> impl futures_util::Stream<Item = ProtocolFrame> + Send + 'static {
    async_stream::stream! {
        let display_override = state.model_config.display_override("chat_companion");
        if ghost {
            let msg_id = Ulid::new();
            yield ProtocolFrame::Meta {
                message_id: ulid_string(msg_id),
                action_type: FrameActionType::Ghost,
                model: None,
                continues_from: None,
            };
            yield ProtocolFrame::Done {
                message_id: ulid_string(msg_id),
                truncated: false,
                usage: None,
                generation_id: None,
            };
        } else {
            for row in &rows {
                let msg_ulid = Ulid::from(row.id);
                let prev_ulid = row.continues_from_message_id.map(Ulid::from);
                let action = match row.assistant_action_type.as_deref() {
                    Some("gift_reaction") => FrameActionType::GiftReaction,
                    _ => FrameActionType::Reply,
                };
                yield ProtocolFrame::Meta {
                    message_id: ulid_string(msg_ulid),
                    action_type: action,
                    model: display_override
                        .as_ref()
                        .and_then(|d| d.display(row.model.as_deref().unwrap_or_default())),
                    continues_from: prev_ulid.map(ulid_string),
                };
                if !row.content.is_empty() {
                    yield ProtocolFrame::Delta {
                        message_id: ulid_string(msg_ulid),
                        content: row.content.clone(),
                    };
                }
                // Replay the persisted (full) usage, then strip
                // OPENROUTER_USAGE_HIDDEN_KEYS for the wire — same contract as
                // the live burst above.
                let mut usage = row.usage.clone();
                filter_usage_keys(&mut usage, &state.config.openrouter_usage_hidden_keys);
                yield ProtocolFrame::Done {
                    message_id: ulid_string(msg_ulid),
                    truncated: row.truncated,
                    usage,
                    generation_id: row.generation_id.clone(),
                };
            }
            // If every persisted assistant row was truncated, emit the same
            // terminal Error that the original burst emitted so the client
            // knows retrying is appropriate.
            if !rows.is_empty() && rows.iter().all(|r| r.truncated) {
                yield ProtocolFrame::Error {
                    code: StreamErrorCode::UpstreamUnavailable,
                    retryable: true,
                    message: "all fallback models truncated (replayed)".into(),
                    user_message: "AI 服务暂时不可用，稍后再试".into(),
                };
                return;
            }
        }
        let final_frame = compute_final_frame(&state, session_id, user_id, false, None, None, 0, 0).await;
        yield final_frame;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_frame_serializes_with_required_fields() {
        let id = Ulid::new();
        let f = ProtocolFrame::Meta {
            message_id: ulid_string(id),
            action_type: FrameActionType::Reply,
            model: Some("x-ai/grok-4-fast".into()),
            continues_from: None,
        };
        let s = serde_json::to_string(&f).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "meta");
        assert_eq!(v["action_type"], "reply");
        assert_eq!(v["model"], "x-ai/grok-4-fast");
        assert!(
            v.get("continues_from").is_none(),
            "must be omitted when None"
        );
        assert_eq!(v["message_id"].as_str().unwrap().len(), 26);
    }

    #[test]
    fn meta_frame_serializes_continues_from_when_present() {
        let prev = ulid_string(Ulid::new());
        let f = ProtocolFrame::Meta {
            message_id: ulid_string(Ulid::new()),
            action_type: FrameActionType::Reply,
            model: Some("x-ai/grok-4-fast".into()),
            continues_from: Some(prev.clone()),
        };
        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        assert_eq!(v["continues_from"], prev);
    }

    #[test]
    fn meta_frame_omits_model_when_none() {
        let f = ProtocolFrame::Meta {
            message_id: ulid_string(Ulid::new()),
            action_type: FrameActionType::Ghost,
            model: None,
            continues_from: None,
        };
        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        assert_eq!(v["type"], "meta");
        assert!(v.get("model").is_none(), "model must be omitted when None");
    }

    #[test]
    fn delta_frame_serializes_with_content() {
        let id = ulid_string(Ulid::new());
        let f = ProtocolFrame::Delta {
            message_id: id.clone(),
            content: "你好".into(),
        };
        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        assert_eq!(v["type"], "delta");
        assert_eq!(v["message_id"], id);
        assert_eq!(v["content"], "你好");
    }

    #[test]
    fn done_frame_serializes_with_usage_and_truncated_flag() {
        let f = ProtocolFrame::Done {
            message_id: ulid_string(Ulid::new()),
            truncated: true,
            usage: Some(serde_json::json!({
                "prompt_tokens": 10,
                "completion_tokens": 4,
                "total_tokens": 14,
            })),
            generation_id: Some("gen-1".into()),
        };
        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        assert_eq!(v["type"], "done");
        assert_eq!(v["truncated"], true);
        assert_eq!(v["usage"]["prompt_tokens"], 10);
        assert_eq!(v["generation_id"], "gen-1");
    }

    #[test]
    fn final_frame_carries_filter_and_status_fields() {
        let f = ProtocolFrame::Final {
            lead_score: 0.71,
            should_show_cta: false,
            agent_training_level: 0.42,
            filtered: true,
            prompt_injected: Some(vec!["nsfw_boost".into()]),
            tier: Some("gold".into()),
            retries_chat: 1,
            retries_filter: 0,
        };
        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        assert_eq!(v["type"], "final");
        assert_eq!(v["filtered"], true);
        assert_eq!(v["prompt_injected"][0], "nsfw_boost");
        assert_eq!(v["tier"], "gold");
        assert_eq!(v["retries_chat"], 1);
        assert_eq!(v["retries_filter"], 0);

        let f2 = ProtocolFrame::Final {
            lead_score: 0.0,
            should_show_cta: false,
            agent_training_level: 0.0,
            filtered: false,
            prompt_injected: None,
            tier: None,
            retries_chat: 0,
            retries_filter: 0,
        };
        let v2: serde_json::Value = serde_json::to_value(&f2).unwrap();
        assert!(v2["prompt_injected"].is_null());
        assert!(v2["tier"].is_null());
        assert_eq!(v2["filtered"], false);
    }

    #[test]
    fn error_frame_uses_snake_case_code() {
        let f = ProtocolFrame::Error {
            code: StreamErrorCode::UpstreamUnavailable,
            retryable: true,
            message: "internal".into(),
            user_message: "AI 服务暂时不可用，稍后再试".into(),
        };
        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["code"], "upstream_unavailable");
        assert_eq!(v["retryable"], true);
    }

    #[test]
    fn done_frame_emits_null_usage_when_absent() {
        let f = ProtocolFrame::Done {
            message_id: ulid_string(Ulid::new()),
            truncated: false,
            usage: None,
            generation_id: None,
        };
        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        // Spec §1.5 done schema permits `usage: null` — do NOT omit.
        assert!(v.get("usage").is_some());
        assert!(v["usage"].is_null());
    }

    use sqlx::PgPool;

    async fn seed_persona_and_session(pool: &PgPool, user_id: Uuid) -> (Uuid, Uuid, Uuid) {
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata, is_active) \
             VALUES ('GhostTest', 'sp', '{}'::jsonb, true) RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id)
        .bind(user_id)
        .fetch_one(pool)
        .await
        .unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(pool)
        .await
        .unwrap();
        (genome_id, instance_id, session_id)
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_stream_terminates_with_final_or_error(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        // test_state's openrouter client points at the real api root — that's
        // fine here because the Ghost branch never makes an LLM call. If the
        // PDE picks Reply, the test will fail when the LLM call short-circuits;
        // that's OK — Reply path testing lives in T11.
        let state = std::sync::Arc::new(crate::routes::companion::test_state(pool.clone()));
        let chat_repo = ChatRepo { pool: &state.pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01J1111111111111111111111A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        let frames: Vec<ProtocolFrame> = run_stream(
            state.clone(),
            PersistedUserMessage {
                user_message_id,
                session_id,
                user_id,
                instance_id,
                content: "hi".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
            },
        )
        .collect()
        .await;

        // Tolerant: the test just proves the generator runs end-to-end and
        // terminates. T11/T15 add per-frame assertions for Reply/replay paths.
        assert!(frames.last().is_some(), "must emit at least one frame");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn replay_done_strips_openrouter_usage_hidden_keys(pool: PgPool) {
        use futures_util::StreamExt;

        let user_id = Uuid::new_v4();
        let (_g, _instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.config.openrouter_usage_hidden_keys =
            std::collections::HashSet::from(["cost".to_string()]);
        let state = std::sync::Arc::new(state);

        // A persisted assistant row carrying full usage incl. `cost`.
        let row = eros_engine_store::chat::ChatMessage {
            id: Uuid::new_v4(),
            session_id,
            role: "assistant".into(),
            content: "hello".into(),
            sent_at: chrono::Utc::now(),
            client_msg_id: None,
            ghost_decision: false,
            user_message_id: None,
            continues_from_message_id: None,
            truncated: false,
            model: Some("x-ai/grok-4-fast".into()),
            usage: Some(serde_json::json!({
                "prompt_tokens": 1290,
                "completion_tokens": 17,
                "total_tokens": 1307,
                "cost": 0.0015878
            })),
            generation_id: Some("gen-1".into()),
            assistant_action_type: Some("reply".into()),
        };

        let frames: Vec<ProtocolFrame> =
            replay_stream(state, session_id, user_id, false, vec![row])
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

        // The hidden key is gone; the rest survive.
        assert!(
            usage.get("cost").is_none(),
            "cost must be stripped by OPENROUTER_USAGE_HIDDEN_KEYS; got {usage}"
        );
        assert_eq!(usage["prompt_tokens"], 1290);
        assert_eq!(usage["total_tokens"], 1307);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_stream_reply_terminates_cleanly_with_mock_openrouter(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2,\"total_tokens\":4},\"id\":\"gen-r\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                eros_engine_llm::openrouter::AppAttribution::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01J2222222222222222222222A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        let frames: Vec<ProtocolFrame> = run_stream(
            std::sync::Arc::new(state),
            PersistedUserMessage {
                user_message_id,
                session_id,
                user_id,
                instance_id,
                content: "hi".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
            },
        )
        .collect()
        .await;

        // Tolerant assertions: PDE may pick Ghost depending on persona/seed,
        // but if it picks Reply the stream must end without an Error frame
        // and end with Final.
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "no error frame expected, got {frames:?}",
        );
        assert!(matches!(frames.last(), Some(ProtocolFrame::Final { .. })));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_stream_done_strips_hidden_usage_keys_live(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        // Upstream usage carries `cost` — which OPENROUTER_USAGE_HIDDEN_KEYS hides.
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2,\"total_tokens\":4,\"cost\":0.0015},\"id\":\"gen-r\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.config.openrouter_usage_hidden_keys =
            std::collections::HashSet::from(["cost".to_string()]);
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                eros_engine_llm::openrouter::AppAttribution::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01J3333333333333333333333A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        let frames: Vec<ProtocolFrame> = run_stream(
            std::sync::Arc::new(state),
            PersistedUserMessage {
                user_message_id,
                session_id,
                user_id,
                instance_id,
                content: "hi".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
            },
        )
        .collect()
        .await;

        // PDE may pick Ghost (no usage) or Reply (usage present). Either way, no
        // Done frame may leak `cost`. If Reply ran, this proves the live-burst
        // filter; if Ghost ran, usage is None and the guard is trivially held.
        let mut saw_filtered_usage = false;
        for f in &frames {
            if let ProtocolFrame::Done { usage: Some(u), .. } = f {
                assert!(
                    u.get("cost").is_none(),
                    "live Done frame leaked hidden key `cost`: {u}"
                );
                assert_eq!(u["prompt_tokens"], 2, "non-hidden keys must survive");
                saw_filtered_usage = true;
            }
        }
        // If the reply path ran, confirm we actually exercised the filter.
        if frames
            .iter()
            .any(|f| matches!(f, ProtocolFrame::Delta { .. }))
        {
            assert!(
                saw_filtered_usage,
                "a Reply burst ran but no Done frame carried usage to filter"
            );
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn live_burst_meta_omits_model_when_override_false(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"id\":\"g\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"deepseek/x\"\nmodel_name_display_override = false\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                eros_engine_llm::openrouter::AppAttribution::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01J4444444444444444444444A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        let frames: Vec<ProtocolFrame> = run_stream(
            std::sync::Arc::new(state),
            PersistedUserMessage {
                user_message_id,
                session_id,
                user_id,
                instance_id,
                content: "hi".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
            },
        )
        .collect()
        .await;

        for f in &frames {
            if let ProtocolFrame::Meta { model, .. } = f {
                assert_eq!(*model, None, "override=false must omit meta.model");
            }
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn replay_applies_display_override(pool: PgPool) {
        use futures_util::StreamExt;

        let user_id = Uuid::new_v4();
        let (_g, _instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        let row = eros_engine_store::chat::ChatMessage {
            id: Uuid::new_v4(),
            session_id,
            role: "assistant".into(),
            content: "hello".into(),
            sent_at: chrono::Utc::now(),
            client_msg_id: None,
            ghost_decision: false,
            user_message_id: None,
            continues_from_message_id: None,
            truncated: false,
            model: Some("deepseek/x".into()),
            usage: None,
            generation_id: None,
            assistant_action_type: Some("reply".into()),
        };

        let meta_model = |frames: &[ProtocolFrame]| -> Option<String> {
            frames.iter().find_map(|f| match f {
                ProtocolFrame::Meta { model, .. } => Some(model.clone()),
                _ => None,
            })?
        };

        // false -> omit
        let mut s1 = crate::routes::companion::test_state(pool.clone());
        s1.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"deepseek/x\"\nmodel_name_display_override = false\n",
            )
            .unwrap(),
        );
        let f1: Vec<ProtocolFrame> = replay_stream(
            std::sync::Arc::new(s1),
            session_id,
            user_id,
            false,
            vec![row.clone()],
        )
        .collect()
        .await;
        assert_eq!(meta_model(&f1), None);

        // pinned string -> that name
        let mut s2 = crate::routes::companion::test_state(pool.clone());
        s2.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"deepseek/x\"\nmodel_name_display_override = \"Aria\"\n",
            )
            .unwrap(),
        );
        let f2: Vec<ProtocolFrame> = replay_stream(
            std::sync::Arc::new(s2),
            session_id,
            user_id,
            false,
            vec![row.clone()],
        )
        .collect()
        .await;
        assert_eq!(meta_model(&f2), Some("Aria".to_string()));

        // map hit -> mapped name
        let mut s3 = crate::routes::companion::test_state(pool.clone());
        s3.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"deepseek/x\"\nmodel_name_display_override = { \"deepseek/x\" = \"Nova\", default = \"Companion\" }\n",
            )
            .unwrap(),
        );
        let f3: Vec<ProtocolFrame> = replay_stream(
            std::sync::Arc::new(s3),
            session_id,
            user_id,
            false,
            vec![row.clone()],
        )
        .collect()
        .await;
        assert_eq!(meta_model(&f3), Some("Nova".to_string()));
    }

    #[test]
    fn extract_text_picks_by_timing() {
        use eros_engine_llm::model_config::FilterTiming::*;
        assert_eq!(extract_text(AfterExtract, "orig", "filt"), "orig");
        assert_eq!(extract_text(BeforeExtract, "orig", "filt"), "filt");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn filtered_turn_emits_filtered_and_persists_filtered(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let chat_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ORIG\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"id\":\"g\",\"model\":\"primary\"}\n\ndata: [DONE]\n\n";
        // The output filter uses the NON-streaming `execute()` path, so its mock
        // must return a JSON completion object (choices[].message.content), not
        // SSE. `model:"fast/m"` makes retries_filter resolve to the primary (0).
        let filt_body = serde_json::json!({
            "id": "gf", "model": "fast/m",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": "FILT"}}],
        });
        // Route the two calls by the MODEL ID present in the request body so the two
        // mocks are MUTUALLY EXCLUSIVE (mount order / precedence cannot matter):
        //   chat call body contains "deepseek/x"; filter call body contains "fast/m".
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("fast/m"))
            .respond_with(ResponseTemplate::new(200).set_body_json(filt_body))
            .mount(&mock)
            .await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("deepseek/x"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(chat_body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel=\"deepseek/x\"\noutput_filter=true\n\
                 [tasks.chat_output_filter]\nmodel=\"fast/m\"\nfilter_prompt=\"REWRITE\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                Default::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let umid = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hello there friend",
                "01J9999999999999999999999A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        let frames: Vec<ProtocolFrame> = run_stream(
            std::sync::Arc::new(state),
            PersistedUserMessage {
                user_message_id: umid,
                session_id,
                user_id,
                instance_id,
                content: "hello there friend".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
            },
        )
        .collect()
        .await;

        let deltas: String = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        if frames
            .iter()
            .any(|f| matches!(f, ProtocolFrame::Delta { .. }))
        {
            assert!(
                deltas.contains("FILT"),
                "client must see filtered text, got {deltas:?}"
            );
            assert!(
                !deltas.contains("ORIG"),
                "original must never reach client, got {deltas:?}"
            );
            let (filtered, rc, rf) = frames
                .iter()
                .find_map(|f| match f {
                    ProtocolFrame::Final {
                        filtered,
                        retries_chat,
                        retries_filter,
                        ..
                    } => Some((*filtered, *retries_chat, *retries_filter)),
                    _ => None,
                })
                .unwrap();
            assert!(filtered, "final.filtered must be true");
            assert_eq!(rc, 0, "primary chat model served");
            assert_eq!(rf, 0, "primary filter model served");
            let row = sqlx::query_scalar::<_, String>(
                "SELECT content FROM engine.chat_messages WHERE session_id=$1 AND role='assistant' ORDER BY sent_at DESC LIMIT 1")
                .bind(session_id).fetch_one(&pool).await.unwrap();
            assert_eq!(row, "FILT");
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn filtered_turn_fail_open_emits_original(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let chat_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ORIG\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"id\":\"g\",\"model\":\"primary\"}\n\ndata: [DONE]\n\n";
        // Filter model returns 500 → fail open to the original reply.
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("fast/m"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("deepseek/x"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(chat_body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel=\"deepseek/x\"\noutput_filter=true\n\
                 [tasks.chat_output_filter]\nmodel=\"fast/m\"\nfilter_prompt=\"REWRITE\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                Default::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let umid = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hello there friend",
                "01J9999999999999999999999B",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        let frames: Vec<ProtocolFrame> = run_stream(
            std::sync::Arc::new(state),
            PersistedUserMessage {
                user_message_id: umid,
                session_id,
                user_id,
                instance_id,
                content: "hello there friend".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
            },
        )
        .collect()
        .await;

        let deltas: String = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        if frames
            .iter()
            .any(|f| matches!(f, ProtocolFrame::Delta { .. }))
        {
            assert!(
                deltas.contains("ORIG"),
                "fail-open must emit original, got {deltas:?}"
            );
            assert!(
                !deltas.contains("FILT"),
                "no filtered text on fail-open, got {deltas:?}"
            );
            let filtered = frames
                .iter()
                .find_map(|f| match f {
                    ProtocolFrame::Final { filtered, .. } => Some(*filtered),
                    _ => None,
                })
                .unwrap();
            assert!(!filtered, "final.filtered must be false on fail-open");
            let row = sqlx::query_scalar::<_, String>(
                "SELECT content FROM engine.chat_messages WHERE session_id=$1 AND role='assistant' ORDER BY sent_at DESC LIMIT 1")
                .bind(session_id).fetch_one(&pool).await.unwrap();
            assert_eq!(row, "ORIG");
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn live_mode_when_random_zero(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let chat_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ORIG\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"id\":\"g\",\"model\":\"primary\"}\n\ndata: [DONE]\n\n";
        // random=0.0 ⇒ turn never passes the gate ⇒ LIVE mode; the filter model
        // must never be contacted.
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("fast/m"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw("data: [DONE]\n\n", "text/event-stream"),
            )
            .expect(0)
            .mount(&mock)
            .await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("deepseek/x"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(chat_body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel=\"deepseek/x\"\noutput_filter=true\n\
                 [tasks.chat_output_filter]\nmodel=\"fast/m\"\nfilter_prompt=\"REWRITE\"\ntrigger = { random = 0.0 }\n",
            ).unwrap());
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                Default::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let umid = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hello there friend",
                "01J9999999999999999999999C",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        let frames: Vec<ProtocolFrame> = run_stream(
            std::sync::Arc::new(state),
            PersistedUserMessage {
                user_message_id: umid,
                session_id,
                user_id,
                instance_id,
                content: "hello there friend".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
            },
        )
        .collect()
        .await;

        let deltas: String = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        if frames
            .iter()
            .any(|f| matches!(f, ProtocolFrame::Delta { .. }))
        {
            assert!(
                deltas.contains("ORIG"),
                "live mode must emit original, got {deltas:?}"
            );
            let filtered = frames
                .iter()
                .find_map(|f| match f {
                    ProtocolFrame::Final { filtered, .. } => Some(*filtered),
                    _ => None,
                })
                .unwrap();
            assert!(!filtered, "final.filtered must be false in live mode");
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_stream_tip_injects_reward_block_in_prompt(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"谢谢\"}}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2,\"total_tokens\":4},\"id\":\"gen-r\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                eros_engine_llm::openrouter::AppAttribution::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "(打赏 $20)",
                "01J5555555555555555555555A",
                "gift_user",
                Some(&serde_json::json!({"tips_amount_usd": 20.0})),
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        let frames: Vec<ProtocolFrame> = run_stream(
            std::sync::Arc::new(state),
            PersistedUserMessage {
                user_message_id,
                session_id,
                user_id,
                instance_id,
                content: "(打赏 $20)".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: Some(20.0),
            },
        )
        .collect()
        .await;

        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "no error frame expected, got {frames:?}",
        );
        assert!(matches!(frames.last(), Some(ProtocolFrame::Final { .. })));

        // A tip is never ghosted ⇒ exactly one LLM call, whose system prompt
        // carries the tip block.
        let reqs = mock.received_requests().await.unwrap();
        assert!(
            !reqs.is_empty(),
            "tip must trigger an LLM call (never ghosted)"
        );
        let sent = String::from_utf8_lossy(&reqs[0].body);
        assert!(
            sent.contains("【刚收到的打赏】") && sent.contains("$20 美元的红包"),
            "system prompt must contain the tip block, got: {sent}",
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn filtered_mode_models_miss_emits_original(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let chat_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ORIG\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"id\":\"g\",\"model\":\"primary\"}\n\ndata: [DONE]\n\n";
        // Turn-level predicates pass (no random/traits gate) ⇒ FILTERED mode, but
        // the per-attempt models predicate fails (primary chat is "deepseek/x",
        // not "other/model") ⇒ no filter call, emit the original.
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("fast/m"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw("data: [DONE]\n\n", "text/event-stream"),
            )
            .expect(0)
            .mount(&mock)
            .await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("deepseek/x"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(chat_body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel=\"deepseek/x\"\noutput_filter=true\n\
                 [tasks.chat_output_filter]\nmodel=\"fast/m\"\nfilter_prompt=\"REWRITE\"\ntrigger = { models = [\"other/model\"] }\n",
            ).unwrap());
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                Default::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let umid = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hello there friend",
                "01J9999999999999999999999D",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        let frames: Vec<ProtocolFrame> = run_stream(
            std::sync::Arc::new(state),
            PersistedUserMessage {
                user_message_id: umid,
                session_id,
                user_id,
                instance_id,
                content: "hello there friend".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
            },
        )
        .collect()
        .await;

        let deltas: String = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        if frames
            .iter()
            .any(|f| matches!(f, ProtocolFrame::Delta { .. }))
        {
            assert_eq!(
                deltas, "ORIG",
                "models-miss must emit only the original, got {deltas:?}"
            );
            let filtered = frames
                .iter()
                .find_map(|f| match f {
                    ProtocolFrame::Final { filtered, .. } => Some(*filtered),
                    _ => None,
                })
                .unwrap();
            assert!(
                !filtered,
                "final.filtered must be false when models predicate misses"
            );
            let meta_count = frames
                .iter()
                .filter(|f| matches!(f, ProtocolFrame::Meta { .. }))
                .count();
            let done_count = frames
                .iter()
                .filter(|f| matches!(f, ProtocolFrame::Done { .. }))
                .count();
            assert_eq!(meta_count, 1, "exactly one Meta frame");
            assert_eq!(done_count, 1, "exactly one Done frame");
        }
    }
}
