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
    ReplyImage,
    ReplyTextImage,
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
    Image {
        message_id: String,
        data_url: String,
        mime: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        image_prompt: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        generation_id: Option<String>,
    },
}

/// Render a 128-bit id as a Crockford Base32 ULID string (26 chars).
pub fn ulid_string(u: Ulid) -> String {
    u.to_string()
}

/// Map an `ActionType` to the `FrameActionType` used in SSE Meta/Image frames.
/// Consumed by the image execution arm (Task 10).
fn frame_action_for(a: eros_engine_core::types::ActionType) -> FrameActionType {
    match a {
        eros_engine_core::types::ActionType::ReplyImage => FrameActionType::ReplyImage,
        eros_engine_core::types::ActionType::ReplyTextImage => FrameActionType::ReplyTextImage,
        eros_engine_core::types::ActionType::Ghost => FrameActionType::Ghost,
        _ => FrameActionType::Reply,
    }
}

/// Assemble an `ImageGenRequest` from the per-turn image executor chain, the
/// resolved `chat_image_generation` defaults, the persona, and the optional
/// per-turn `ImageReplyParams`. Pure (no I/O). Per-turn overrides win over the
/// resolved defaults; `plan_image_prompt` (the judge's / forced prompt) wins as
/// the subject, then the per-turn `image_prompt`, then `fallback_subject`.
#[allow(clippy::too_many_arguments)]
fn build_image_gen_request(
    primary: String,
    chain: Vec<String>,
    persona: &eros_engine_core::persona::CompanionPersona,
    plan_image_prompt: Option<&str>,
    req_image: Option<&crate::routes::companion_stream::ImageReplyParams>,
    resolved: Option<&eros_engine_llm::model_config::ResolvedImageGen>,
    fallback_subject: &str,
) -> eros_engine_llm::openrouter::ImageGenRequest {
    use eros_engine_llm::model_config::StyleKey;
    let style: StyleKey = req_image
        .and_then(|i| i.style)
        .or_else(|| resolved.map(|r| r.default_style))
        .unwrap_or_default();
    let subject = plan_image_prompt
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            req_image
                .and_then(|i| i.image_prompt.as_deref())
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or(fallback_subject);
    let prompt = crate::pipeline::handlers::compose_image_prompt(style, persona, subject);
    let aspect_ratio = req_image
        .and_then(|i| i.aspect_ratio.clone())
        .or_else(|| resolved.map(|r| r.default_aspect_ratio.clone()));
    let resolution = req_image
        .and_then(|i| i.resolution.clone())
        .or_else(|| resolved.and_then(|r| r.default_resolution.clone()));
    eros_engine_llm::openrouter::ImageGenRequest {
        model: primary,
        fallback_model: chain,
        prompt,
        face_ref_url: req_image.and_then(|i| i.face_ref_url.clone()),
        aspect_ratio,
        resolution,
        max_tokens: resolved.map(|r| r.max_tokens).unwrap_or(4096),
    }
}

/// Parse the MIME type out of a `data:` URL prefix (e.g.
/// `data:image/png;base64,AAAA` → `"image/png"`). Defaults to `"image/png"`
/// when the input is not a recognizable `data:<mime>;` URL.
fn data_url_mime(data_url: &str) -> String {
    data_url
        .strip_prefix("data:")
        .and_then(|rest| rest.split(';').next())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("image/png")
        .to_string()
}

use std::sync::Arc;
use uuid::Uuid;

use eros_engine_core::pde;
use eros_engine_core::types::{ActionType, DecisionInput, Event};
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::ChatRepo;
use eros_engine_store::error_handling::ErrorHandlingRepo;
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
    persist_action: &'static str, // "reply"
    plan_action: ActionType,
    req: eros_engine_llm::openrouter::ChatRequest,
    display_override: Option<eros_engine_llm::model_config::DisplayOverride>,
    filter: Option<eros_engine_llm::model_config::ResolvedOutputFilter>,
    trait_tags: Vec<String>, // requested prompt-trait tags (the turn's)
    tier: Option<String>,    // user's tier at message time; None omitted from metadata
    memory_scope: eros_engine_core::scope::MemoryScope, // post-resolve scope for assistant metadata
    affinity_scope: eros_engine_core::scope::AffinityScope, // post-resolve scope for assistant metadata
    random_draw: Option<f64>, // sampled once per turn by run_stream; None when trigger.random is unset
    outcome: std::sync::Arc<std::sync::Mutex<BurstOutcome>>,
) -> impl futures_util::Stream<Item = ProtocolFrame> + Send + 'static {
    async_stream::stream! {
        let chat_repo = ChatRepo { pool: &state.pool };
        // The fallback_model is already truncated to retry_depth entries by
        // resolve() — no cap needed here; the chain is just [primary] + fallbacks.
        let chain: Vec<String> = std::iter::once(req.model.clone())
            .chain(req.fallback_model.iter().cloned())
            .filter(|s| !s.is_empty())
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
        // A turn buffers (no live deltas) if the LLM output_filter's turn-level
        // predicates pass, OR any output_regex rule targets a model in this
        // turn's resolved chain (so the artifact can be stripped before emit).
        let regex_targets_chain = chain.iter().any(|m| {
            state.output_regex.iter().any(|r| r.models.iter().any(|rm| rm == m))
        });
        let llm_filter_arms = filter
            .as_ref()
            .map(|f| f.trigger.turn_level_pass(random_draw, &tag_refs))
            .unwrap_or(false);
        let filtered_mode = llm_filter_arms || regex_targets_chain;

        // Build the assistant row metadata bag: always includes prompt_traits +
        // resolved memory_scope / affinity_scope (the POST-resolve values
        // actually used to serve this turn — pair with the user row's
        // memory_scope_raw / affinity_scope_raw to surface allow-list / shape
        // mismatches with a single metadata->>'...' diff); includes tier only
        // when the request carried one (omit key entirely when None). When the
        // filter chain failed entirely (fail-open), also writes the per-attempt
        // audit log so ops can identify these rows.
        let build_metadata = |filter_failure: Option<&FilterFailOpen>| -> Option<serde_json::Value> {
            let mut m = serde_json::Map::new();
            m.insert("prompt_traits".into(), serde_json::json!(&trait_tags));
            m.insert(
                "memory_scope".into(),
                serde_json::to_value(memory_scope).expect("MemoryScope serializes"),
            );
            m.insert(
                "affinity_scope".into(),
                serde_json::to_value(affinity_scope).expect("AffinityScope serializes"),
            );
            if let Some(t) = tier.as_deref() {
                m.insert("tier".into(), serde_json::json!(t));
            }
            if let Some(fail) = filter_failure {
                m.insert("filter_outcome".into(), serde_json::json!("fail_open"));
                m.insert("f_client_msg_id".into(), serde_json::json!(&fail.f_client_msg_id));
                m.insert("filter_attempts".into(), serde_json::json!(&fail.attempts));
            }
            Some(serde_json::Value::Object(m))
        };

        if !filtered_mode {
            // ===== LIVE MODE (preserved verbatim from the pre-filter burst) =====
            let mut continues_from: Option<Ulid> = None;
            // Repaired text of the latest COMPLETE garbled attempt seen across the
            // whole chain. Used as the last-resort replacement when the chain
            // exhausts, so a complete garble isn't discarded just because a LATER
            // fallback failed differently (mirrors OpenRouterClient::execute).
            let mut last_complete_garble: Option<String> = None;
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

                // Byte-BPE garble guard (issue #84). A high Ġ/Ċ density means the
                // provider returned undecoded byte-level-BPE. Repair before persist
                // so the row never re-enters history as garble, and mark the bubble
                // truncated so the client replaces it: a non-last candidate advances
                // to the next model; the last candidate emits a repaired-text
                // replacement bubble below (the live deltas already sent are not
                // retractable, so the persisted row + the replacement bubble are
                // what end up clean).
                //
                // A garble is retained for last-resort replacement ONLY when the
                // stream OTHERWISE completed: if it was already truncated by length
                // / a chunk-transport error, the text is incomplete, so it stays on
                // the safe pseudo-ghost path rather than being presented as complete.
                // `last_complete_garble` persists across iterations so a complete
                // garble survives a later differently-failing fallback.
                let truncated_before_garble = truncated;
                if eros_engine_llm::byte_bpe::looks_byte_garbled(&acc) {
                    tracing::error!(model = %model_id, "stream: byte-BPE garbled completion (issue #84)");
                    acc = eros_engine_llm::byte_bpe::repair_byte_bpe(&acc);
                    truncated = true;
                    if !truncated_before_garble {
                        last_complete_garble = Some(acc.clone());
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
                    metadata: build_metadata(None),
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
                    let mut o = outcome.lock().unwrap();
                    // Only the accepted reply feeds post-process (memory / insight /
                    // affinity). Drop any superseded earlier attempts (truncated, or
                    // garbled-then-repaired) that were pushed while walking the chain
                    // — otherwise rejected provider output would corrupt derived user
                    // state alongside the reply the user actually saw.
                    o.produced.retain(|m| m.message_id == msg_uuid);
                    o.retries_chat = idx as u32;
                    return;
                }
                if idx + 1 == chain.len() {
                    // retries_chat = fallback count consumed (NOT total attempts),
                    // matching its 0-based semantics elsewhere (0 = primary served).
                    let fallback_retries = (chain.len() as u32).saturating_sub(1);
                    outcome.lock().unwrap().retries_chat = fallback_retries;
                    if let Some(repaired) = last_complete_garble.take() {
                        // Chain ended with a complete garble somewhere in it: replace
                        // the last (failed) bubble the client saw with that repaired
                        // text (issue #84, P1) — even if the FINAL attempt failed
                        // differently (e.g. transport), so the salvage isn't lost.
                        let (frames, produced) = build_garble_repaired_replacement(
                            &state.pool,
                            session_id,
                            user_message_id,
                            frame_action,
                            persist_action,
                            plan_action,
                            &trait_tags,
                            &tier,
                            memory_scope,
                            affinity_scope,
                            fallback_retries,
                            Some(msg_ulid),
                            repaired,
                        )
                        .await;
                        {
                            let mut o = outcome.lock().unwrap();
                            o.produced.clear();
                            o.produced.push(produced);
                        }
                        for f in frames { yield f; }
                        return;
                    }
                    match build_stream_failure_pseudo_ghost(
                        &state.pool,
                        session_id,
                        user_message_id,
                        frame_action,
                        persist_action,
                        plan_action,
                        &trait_tags,
                        &tier,
                        memory_scope,
                        affinity_scope,
                        fallback_retries,
                        // Live mode persisted the final truncated bubble; link
                        // the pseudo-ghost to it so clients + replay can stitch
                        // them as one logical conversation turn.
                        Some(msg_ulid),
                    )
                    .await
                    {
                        Some((frames, produced)) => {
                            // Replace any truncated-attempt entries already in
                            // outcome.produced with just the pseudo-ghost — so
                            // post_process (memory / affinity / insight) runs on
                            // the safe fallback phrase the user actually saw,
                            // NOT on the failed partial outputs from earlier
                            // chain attempts. Filtered mode never pushed to
                            // produced anyway, so clear() is a no-op there.
                            {
                                let mut o = outcome.lock().unwrap();
                                o.produced.clear();
                                o.produced.push(produced);
                            }
                            for f in frames { yield f; }
                        }
                        None => {
                            yield ProtocolFrame::Error {
                                code: StreamErrorCode::UpstreamUnavailable,
                                retryable: true,
                                message: "all fallback models truncated".into(),
                                user_message: "AI 服务暂时不可用，稍后再试".into(),
                            };
                        }
                    }
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
        // `filter` is None when the turn buffers solely because of output_regex.
        let f_opt = filter.as_ref();
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

            if eros_engine_llm::byte_bpe::looks_byte_garbled(&acc) {
                tracing::error!(model = %model_id, "stream(filtered): byte-BPE garbled completion (issue #84)");
                acc = eros_engine_llm::byte_bpe::repair_byte_bpe(&acc);
                // Nothing has been streamed to the client yet, so a COMPLETE garble
                // is salvaged immediately: the repaired (clean) text flows through
                // the output filter + persist below. We deliberately do NOT force a
                // fallback — doing so would discard a recoverable complete garble if
                // the later attempt failed. An INCOMPLETE garble is already
                // `truncated` (length / transport) and handled by the block below.
            }

            if truncated {
                if idx + 1 == chain.len() {
                    let fallback_retries = (chain.len() as u32).saturating_sub(1);
                    outcome.lock().unwrap().retries_chat = fallback_retries;
                    match build_stream_failure_pseudo_ghost(
                        &state.pool,
                        session_id,
                        user_message_id,
                        frame_action,
                        persist_action,
                        plan_action,
                        &trait_tags,
                        &tier,
                        memory_scope,
                        affinity_scope,
                        fallback_retries,
                        // Filtered mode never persists intermediate truncated
                        // attempts, so there is no prior bubble to continue from.
                        None,
                    )
                    .await
                    {
                        Some((frames, produced)) => {
                            // Replace any truncated-attempt entries already in
                            // outcome.produced with just the pseudo-ghost — so
                            // post_process (memory / affinity / insight) runs on
                            // the safe fallback phrase the user actually saw,
                            // NOT on the failed partial outputs from earlier
                            // chain attempts. Filtered mode never pushed to
                            // produced anyway, so clear() is a no-op there.
                            {
                                let mut o = outcome.lock().unwrap();
                                o.produced.clear();
                                o.produced.push(produced);
                            }
                            for f in frames { yield f; }
                        }
                        None => {
                            yield ProtocolFrame::Error {
                                code: StreamErrorCode::UpstreamUnavailable,
                                retryable: true,
                                message: "all fallback models truncated".into(),
                                user_message: "AI 服务暂时不可用，稍后再试".into(),
                            };
                        }
                    }
                    return;
                }
                continue;
            }

            outcome.lock().unwrap().retries_chat = idx as u32;
            yield ProtocolFrame::Meta {
                message_id: ulid_string(msg_ulid),
                action_type: frame_action,
                model: display_override.as_ref().and_then(|d| d.display(model_id)),
                continues_from: None,
            };

            // Layer 0: deterministic per-model strip, before client emit, the
            // optional LLM filter, and the extract split. `cleaned == acc` when
            // no rule matches (then `regex_indices` is empty → no audit).
            //
            // Run this ONLY for the attempt that is actually served — i.e. AFTER
            // the `if truncated { ... continue }` check above. A truncated
            // attempt's partial `acc` could otherwise match a rule and set
            // `outcome.filtered = true`, then be discarded via `continue`,
            // letting a later fallback serve an UNSTRIPPED reply while the final
            // frame falsely reports `filtered = true`.
            let strip = eros_engine_llm::model_config::apply_output_regex(
                &state.output_regex,
                model_id,
                &acc,
            );
            let cleaned = strip.cleaned;
            let regex_indices = strip.matched_rules;
            if !regex_indices.is_empty() {
                outcome.lock().unwrap().filtered = true;
            }

            // `filter_failure` carries the per-attempt audit when filter fails.
            // Threaded into AssistantInsert via build_metadata — distinct from
            // the prompt_traits/tier metadata to keep concerns separate.

            // Build the regex-only audit (raw original on pre_filter_content).
            // We generate a fresh `f_`-prefixed ULID for each regex-strip row
            // so the unique index on (session_id, f_client_msg_id) is never
            // violated by multiple regex-filtered turns in the same session.
            // (An empty string is non-NULL and would conflict on the second
            // turn, so `String::new()` from the brief is replaced by a ULID.)
            let regex_audit = |raw: &str| -> Option<eros_engine_store::chat::FilterAudit> {
                if regex_indices.is_empty() {
                    return None;
                }
                Some(eros_engine_store::chat::FilterAudit {
                    pre_filter_content: raw.to_string(),
                    filter_model: "<regex>".to_string(),
                    filter_triggers: serde_json::json!({ "regex": regex_indices }),
                    f_client_msg_id: format!("f_{}", Ulid::new()),
                    f_generation_id: None,
                })
            };

            let (visible, filter_audit, filter_failure): (
                String,
                Option<eros_engine_store::chat::FilterAudit>,
                Option<FilterFailOpen>,
            ) = match f_opt {
                Some(f) => {
                    let hits = f.trigger.should_filter(model_id, &tag_refs, random_draw);
                    match hits {
                        Some(h) => match run_output_filter(&state, f, &cleaned).await {
                            Ok(out) => {
                                let mut o = outcome.lock().unwrap();
                                o.filtered = true;
                                o.retries_filter = out.retries_filter;
                                drop(o);
                                // Fold the regex hit into the LLM filter's triggers.
                                let mut triggers = if h.is_empty() {
                                    serde_json::Map::new()
                                } else {
                                    match serde_json::to_value(&h)
                                        .expect("FiredPredicates Serialize is infallible")
                                    {
                                        serde_json::Value::Object(m) => m,
                                        other => {
                                            let mut m = serde_json::Map::new();
                                            m.insert("filter".into(), other);
                                            m
                                        }
                                    }
                                };
                                if !regex_indices.is_empty() {
                                    triggers.insert("regex".into(), serde_json::json!(regex_indices));
                                }
                                let filter_triggers = if triggers.is_empty() {
                                    serde_json::Value::Null
                                } else {
                                    serde_json::Value::Object(triggers)
                                };
                                let audit = eros_engine_store::chat::FilterAudit {
                                    pre_filter_content: acc.clone(), // raw, pre-everything
                                    filter_model: out.filter_model,
                                    filter_triggers,
                                    f_client_msg_id: out.f_client_msg_id,
                                    f_generation_id: out.f_generation_id,
                                };
                                (out.filtered_text, Some(audit), None)
                            }
                            Err(fail) => {
                                tracing::warn!(
                                    f_client_msg_id = %fail.f_client_msg_id,
                                    attempts = ?fail.attempts,
                                    "filter: all models in chain failed validity; falling open"
                                );
                                // Fail open to the regex-cleaned text (strip still applies).
                                (cleaned.clone(), regex_audit(&acc), Some(fail))
                            }
                        },
                        None => (cleaned.clone(), regex_audit(&acc), None), // LLM models-miss
                    }
                }
                None => (cleaned.clone(), regex_audit(&acc), None), // regex-only turn
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
                metadata: build_metadata(filter_failure.as_ref()),
            };
            if let Err(e) = chat_repo.insert_assistant_batch(session_id, user_message_id, &[row]).await {
                tracing::warn!("stream(filtered): persist failed: {e}");
            }
            let timing = f_opt
                .map(|f| f.timing)
                .unwrap_or(eros_engine_llm::model_config::FilterTiming::AfterExtract);
            let extracted = extract_text(timing, &cleaned, &visible);
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

/// One filter-chain attempt that did NOT produce a valid filtered reply.
/// Recorded into `chat_messages.metadata.filter_attempts[]` when fail-open
/// kicks in so ops can see WHY filter didn't apply on this row.
#[derive(Debug, Clone, serde::Serialize)]
struct FilterAttemptFailure {
    /// OpenRouter model id of the attempted filter model.
    model: String,
    /// Stable lowercase ASCII label. Same vocabulary as
    /// `filter_output_invalidity` plus `"error"`, `"timeout"`, `"empty"`.
    reason: &'static str,
}

/// Returned by `run_output_filter` when the whole chain failed validity /
/// errored / timed out. Caller writes these into `chat_messages.metadata`
/// before emitting the original reply (fail-open).
#[derive(Debug, Clone)]
struct FilterFailOpen {
    f_client_msg_id: String,
    attempts: Vec<FilterAttemptFailure>,
}

// ── Output validity gate ─────────────────────────────────────────────────────

/// Refusal phrases checked in the leading [`REFUSAL_HEAD_SCAN_CHARS`] chars
/// of the filter output.  When any prefix matches, the call is treated as a
/// model refusal regardless of HTTP status.
///
/// **Matching is ASCII-case-insensitive** — the input head is lowercased before
/// `contains` runs, so models that emit `"as an ai ..."` or `"I'M SORRY"` are
/// caught.  All English patterns are stored lowercase; Chinese patterns are
/// unaffected by lowercasing (CJK code points have no case).
const REFUSAL_PATTERNS_HEAD: &[&str] = &[
    // Chinese refusals — observed in production from gpt-4.1-nano
    "抱歉，我无法",
    "抱歉，我不能",
    "对不起，我无法",
    "对不起，我不能",
    "抱歉，无法",
    "对不起，无法",
    "很抱歉，我无法",
    "很抱歉，我不能",
    // English refusals — standard OpenAI/Anthropic apology shapes (lowercase)
    "i'm sorry, but i can't",
    "i'm sorry, but i cannot",
    "i cannot rewrite",
    "i can't rewrite",
    "i cannot help",
    "i can't help",
    "i won't be able to",
    "i'm not able to",
    "i am not able to",
    "as an ai",
    "i apologize, but",
    "sorry, i can't",
    "sorry, i cannot",
    "unfortunately, i can't",
    "unfortunately, i cannot",
];

/// Refusal verbs used in the short-response branch: if the total response is
/// shorter than [`MIN_FILTERED_OUTPUT_CHARS`] and contains any of these
/// anywhere in the text, it is treated as a refusal rather than just too-short.
///
/// English entries are stored lowercase; the input is lowercased before
/// matching (see [`filter_output_invalidity`]).
const REFUSAL_SHORT_VERBS: &[&str] = &[
    "无法", "不能", "拒绝", "won't", "cannot", "can't", "unable", "refuse",
];

/// How many Unicode characters to scan from the start of the response when
/// checking [`REFUSAL_PATTERNS_HEAD`].
const REFUSAL_HEAD_SCAN_CHARS: usize = 120;

/// Minimum character count for a valid filter output.  A real rewrite is at
/// least this long.  Responses shorter than this threshold are either flagged
/// as `"refusal_pattern"` (if a refusal verb appears) or `"too_short"`.
const MIN_FILTERED_OUTPUT_CHARS: usize = 80;

/// True when a refusal phrase appears in the leading `REFUSAL_HEAD_SCAN_CHARS`
/// (lowercased) of `text`. Shared by the output and input validity gates.
fn refusal_in_head(text: &str) -> bool {
    let head_lower: String = text
        .chars()
        .take(REFUSAL_HEAD_SCAN_CHARS)
        .flat_map(char::to_lowercase)
        .collect();
    REFUSAL_PATTERNS_HEAD.iter().any(|p| head_lower.contains(p))
}

/// Check whether a filter LLM response should be rejected by the validity gate.
///
/// Returns `Some(reason_label)` when the output is invalid, `None` when valid.
/// The label is a stable lowercase ASCII string used for log fields:
/// - `"content_filter"` — `finish_reason == "content_filter"` (Gemini/OpenAI safety block)
/// - `"refusal_pattern"` — refusal phrase found in the head, or short text with a refusal verb
/// - `"too_short"` — text is shorter than [`MIN_FILTERED_OUTPUT_CHARS`] with no refusal verb
///
/// Checks are ordered cheapest-first:
/// 1. `finish_reason`
/// 2. Refusal pattern in head (first `REFUSAL_HEAD_SCAN_CHARS` chars)
/// 3. Short-text checks (refusal-verb-or-too-short)
fn filter_output_invalidity(text: &str, finish_reason: Option<&str>) -> Option<&'static str> {
    if finish_reason == Some("content_filter") {
        return Some("content_filter");
    }
    let total_chars = text.chars().count();
    if refusal_in_head(text) {
        return Some("refusal_pattern");
    }
    if total_chars < MIN_FILTERED_OUTPUT_CHARS {
        let text_lower = text.to_lowercase();
        for verb in REFUSAL_SHORT_VERBS {
            if text_lower.contains(verb) {
                return Some("refusal_pattern");
            }
        }
        return Some("too_short");
    }
    None
}

// ── run_output_filter ────────────────────────────────────────────────────────

/// Per-model timeout for a single filter LLM call.
const FILTER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Run the output-filter LLM over `original`, walking the (already
/// depth-capped) fallback chain one model at a time.  After each successful
/// HTTP 200 response, `filter_output_invalidity` is applied; on failure the
/// next model is tried.  Returns `Err(FilterFailOpen)` when the whole chain
/// exhausts (callers fall open and emit the original reply, and write the
/// per-attempt audit log into `chat_messages.metadata`).
async fn run_output_filter(
    state: &AppState,
    f: &eros_engine_llm::model_config::ResolvedOutputFilter,
    original: &str,
) -> Result<RunFilterOutcome, FilterFailOpen> {
    use eros_engine_llm::openrouter::{ChatMessage, ChatRequest};
    let f_client_msg_id = format!("f_{}", Ulid::new());
    let chain: Vec<String> = std::iter::once(f.model.clone())
        .chain(f.fallback_model.iter().cloned())
        .collect();
    let mut attempts: Vec<FilterAttemptFailure> = Vec::with_capacity(chain.len());
    for (idx, model_id) in chain.iter().enumerate() {
        let req = ChatRequest {
            model: model_id.clone(),
            fallback_model: vec![],
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
            reasoning: f.reasoning.clone(),
            ..Default::default()
        };
        let resp = match tokio::time::timeout(FILTER_TIMEOUT, state.openrouter.execute(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::warn!(model = %model_id, error = %e, "filter: model error; walking to next");
                attempts.push(FilterAttemptFailure {
                    model: model_id.clone(),
                    reason: "error",
                });
                continue;
            }
            Err(_) => {
                tracing::warn!(model = %model_id, "filter: model timeout; walking to next");
                attempts.push(FilterAttemptFailure {
                    model: model_id.clone(),
                    reason: "timeout",
                });
                continue;
            }
        };
        super::log_openrouter_usage("chat_output_filter", None, &resp);
        let text = resp.reply.trim().to_string();
        // Empty reply check before the validity gate: "model returned literally
        // nothing" is distinguished from "model returned a short non-empty
        // response" so ops can see the difference in filter_attempts.
        if text.is_empty() {
            tracing::warn!(model = %model_id, "filter: empty reply; walking to next");
            attempts.push(FilterAttemptFailure {
                model: model_id.clone(),
                reason: "empty",
            });
            continue;
        }
        if let Some(reason) = filter_output_invalidity(&text, resp.finish_reason.as_deref()) {
            tracing::warn!(
                model = %model_id,
                invalidity = %reason,
                "filter: output failed validity gate; walking to next model"
            );
            attempts.push(FilterAttemptFailure {
                model: model_id.clone(),
                reason,
            });
            continue;
        }
        // Falling back to model_id when the response omits the served model is
        // safe: that is the model we requested, and OpenRouter only omits it
        // on error paths (which we have already excluded via the validity gate).
        let filter_model = resp.model.unwrap_or_else(|| model_id.clone());
        return Ok(RunFilterOutcome {
            filtered_text: text,
            retries_filter: idx as u32,
            filter_model,
            f_client_msg_id,
            f_generation_id: resp.generation_id,
        });
    }
    Err(FilterFailOpen {
        f_client_msg_id,
        attempts,
    })
}

// ── Input filter (user-input rewrite) ────────────────────────────────────────

/// Parsed verdict from the input-filter LLM. `rewrite=false` ⇒ keep the
/// original input; `rewrite=true` ⇒ use `content` (with `reason` for audit).
#[derive(Debug, Clone, serde::Deserialize)]
struct InputFilterVerdict {
    #[serde(default)]
    rewrite: bool,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// Parse the filter reply into a verdict: direct JSON first, then a balanced
/// JSON block embedded in prose (mirrors post_process extraction parsing).
fn parse_input_filter_verdict(text: &str) -> Option<InputFilterVerdict> {
    serde_json::from_str::<InputFilterVerdict>(text)
        .ok()
        .or_else(|| {
            super::post_process::find_json_block(text)
                .and_then(|b| serde_json::from_str::<InputFilterVerdict>(b).ok())
        })
}

// ── PDE judge primitives ──────────────────────────────────────────────────────

/// Judge verdict action. serde `snake_case` matches the JSON contract
/// (`reply_text` / `ghost` / `reply_image` / `reply_text_image`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PdeAction {
    ReplyText,
    Ghost,
    ReplyImage,
    ReplyTextImage,
}

impl PdeAction {
    fn as_str(self) -> &'static str {
        match self {
            PdeAction::ReplyText => "reply_text",
            PdeAction::Ghost => "ghost",
            PdeAction::ReplyImage => "reply_image",
            PdeAction::ReplyTextImage => "reply_text_image",
        }
    }
}

/// Parsed judge verdict. `inner_state` is sanitized (`sanitize_inner_state`)
/// before it reaches the prompt; `image_prompt`/`reason` are never injected.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct PdeVerdict {
    action: PdeAction,
    #[serde(default)]
    inner_state: String,
    #[serde(default)]
    image_prompt: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// Parse the judge reply: direct JSON first, then a balanced JSON block in prose
/// (mirrors `parse_input_filter_verdict`).
fn parse_pde_verdict(text: &str) -> Option<PdeVerdict> {
    serde_json::from_str::<PdeVerdict>(text).ok().or_else(|| {
        super::post_process::find_json_block(text)
            .and_then(|b| serde_json::from_str::<PdeVerdict>(b).ok())
    })
}

const INNER_STATE_MAX_CHARS: usize = 200;

/// Sanitize judge-authored `inner_state` before folding it into the system
/// prompt's `[inner_state]` section. Drops lines that look like prompt section
/// headers / structural markers, strips `[`/`]` tokens and control characters,
/// collapses whitespace, and caps length. Returns plain single-line prose
/// (`""` ⇒ caller treats as no hint).
fn sanitize_inner_state(raw: &str) -> String {
    let joined = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| !l.starts_with('[') && !l.starts_with("---") && !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join(" ");
    let no_brackets_or_ctrl: String = joined
        .chars()
        .filter(|c| *c != '[' && *c != ']' && !c.is_control())
        .collect();
    let collapsed = no_brackets_or_ctrl
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    collapsed.chars().take(INNER_STATE_MAX_CHARS).collect()
}

// ── Task 7: PDE runner + pure helpers ─────────────────────────────────────

/// Terminal status of a PDE judge run — drives the audit `status` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PdeStatus {
    Ok,
    Empty,
    ParseError,
    Timeout,
    Error,
}

impl PdeStatus {
    fn as_str(self) -> &'static str {
        match self {
            PdeStatus::Ok => "ok",
            PdeStatus::Empty => "empty",
            PdeStatus::ParseError => "parse_error",
            PdeStatus::Timeout => "timeout",
            PdeStatus::Error => "error",
        }
    }
}

/// Outcome of a PDE judge run. `verdict` is `Some` only on `Ok`; `raw` carries
/// the model text on `ParseError` for the audit payload; the trio is the
/// winning call's audit echo.
pub(crate) struct PdeDecisionRun {
    pub(crate) status: PdeStatus,
    pub(crate) verdict: Option<PdeVerdict>,
    pub(crate) raw: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) usage: Option<serde_json::Value>,
    pub(crate) generation_id: Option<String>,
}

/// OpenRouter `response_format` for the PDE verdict (json_schema, strict). The
/// optional verdict fields are nullable so a strict provider returns `null`,
/// which deserializes to `PdeVerdict`'s `Option` fields as `None`.
fn pde_response_format() -> serde_json::Value {
    serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "pde_verdict",
            "strict": true,
            "schema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["action", "inner_state", "image_prompt", "reason"],
                "properties": {
                    "action": { "type": "string",
                        "enum": ["reply_text", "ghost", "reply_image", "reply_text_image"] },
                    "inner_state": { "type": "string" },
                    "image_prompt": { "type": ["string", "null"] },
                    "reason": { "type": ["string", "null"] }
                }
            }
        }
    })
}

/// The last parse-error attempt's text + audit echo, kept so a chain-exhausted
/// ParseError return preserves the raw model text and audit trio.
struct LastParseAttempt {
    raw: String,
    model: Option<String>,
    usage: Option<serde_json::Value>,
    generation_id: Option<String>,
}

/// Run the PDE judge over the assembled context. Walks `[model] + fallback`
/// trying the next model on a transport failure (error/timeout/empty) or a
/// parse error; a chain-exhausted ParseError preserves the last attempt's raw
/// text + audit trio. Fail-open: any non-`Ok` status → the caller uses the
/// rule fallback. NEVER returns an error — always a run record.
///
/// Unlike `run_input_filter`, a content-level reply that won't parse here walks
/// the rest of the chain before the caller falls back to the rule engine.
async fn run_pde_decision(
    client: &eros_engine_llm::openrouter::OpenRouterClient,
    p: &eros_engine_llm::model_config::ResolvedPde,
    ctx: &str,
) -> PdeDecisionRun {
    use eros_engine_llm::openrouter::{ChatMessage, ChatRequest};
    let chain: Vec<String> = std::iter::once(p.model.clone())
        .chain(p.fallback_model.iter().cloned())
        .collect();
    let mut last = PdeStatus::Error; // chain-exhausted default
                                     // On a content-level reply that won't parse, keep the LAST attempt's text +
                                     // audit trio so the chain-exhausted ParseError return stays faithful.
    let mut last_parse: Option<LastParseAttempt> = None;
    let response_format = p.structured_output.then(pde_response_format);
    for model_id in &chain {
        let req = ChatRequest {
            model: model_id.clone(),
            fallback_model: vec![],
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: p.decision_prompt.clone(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: ctx.to_string(),
                },
            ],
            temperature: p.temperature as f32,
            max_tokens: p.max_tokens,
            reasoning: p.reasoning.clone(),
            response_format: response_format.clone(),
            ..Default::default()
        };
        let resp = match tokio::time::timeout(FILTER_TIMEOUT, client.execute(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::warn!(model = %model_id, error = %e, "pde: model error; next");
                last = PdeStatus::Error;
                continue;
            }
            Err(_) => {
                tracing::warn!(model = %model_id, "pde: timeout; next");
                last = PdeStatus::Timeout;
                continue;
            }
        };
        super::log_openrouter_usage("pde_decision", None, &resp);
        let text = resp.reply.trim().to_string();
        if text.is_empty() {
            tracing::warn!(model = %model_id, "pde: empty reply; next");
            last = PdeStatus::Empty;
            continue;
        }
        match parse_pde_verdict(&text) {
            Some(verdict) => {
                return PdeDecisionRun {
                    status: PdeStatus::Ok,
                    verdict: Some(verdict),
                    raw: None,
                    model: resp.model.or_else(|| Some(model_id.clone())),
                    usage: resp.usage,
                    generation_id: resp.generation_id,
                };
            }
            None => {
                tracing::warn!(model = %model_id, "pde: unparseable verdict; trying next model");
                last = PdeStatus::ParseError;
                last_parse = Some(LastParseAttempt {
                    raw: text,
                    model: resp.model.or_else(|| Some(model_id.clone())),
                    usage: resp.usage,
                    generation_id: resp.generation_id,
                });
                continue;
            }
        }
    }
    // chain exhausted
    match last {
        PdeStatus::ParseError => {
            let lp = last_parse.expect("ParseError ⇒ last_parse is set");
            PdeDecisionRun {
                status: PdeStatus::ParseError,
                verdict: None,
                raw: Some(lp.raw),
                model: lp.model,
                usage: lp.usage,
                generation_id: lp.generation_id,
            }
        }
        other => PdeDecisionRun {
            status: other,
            verdict: None,
            raw: None,
            model: None,
            usage: None,
            generation_id: None,
        },
    }
}

/// Map the judge's proposed action to the acted `ActionType`, applying the
/// hard-safety ghost guardrail (`ghost::ghost_permitted`) and the image-degrade.
/// Does NOT apply the `ghosting` kill-switch (that is a path-wide final gate).
/// Pure.
fn guard_action(
    proposed: PdeAction,
    affinity: &eros_engine_core::affinity::Affinity,
    signals: &eros_engine_core::types::ConversationSignals,
    image_executor_available: bool,
) -> ActionType {
    match proposed {
        PdeAction::Ghost => {
            let gs = eros_engine_core::ghost::GhostSignals {
                message_count: signals.message_count,
                hours_since_last_ghost: signals.hours_since_last_ghost,
            };
            if eros_engine_core::ghost::ghost_permitted(affinity, gs) {
                ActionType::Ghost
            } else {
                ActionType::ReplyText
            }
        }
        // Keep the image action when an executor chain exists this turn;
        // otherwise degrade to text (today's behaviour).
        PdeAction::ReplyImage if image_executor_available => ActionType::ReplyImage,
        PdeAction::ReplyTextImage if image_executor_available => ActionType::ReplyTextImage,
        PdeAction::ReplyImage | PdeAction::ReplyTextImage => ActionType::ReplyText,
        PdeAction::ReplyText => ActionType::ReplyText,
    }
}

/// Path-wide `ghosting` kill-switch: if ghosting is disabled and the plan is a
/// Ghost, rebuild it as a ReplyText plan carrying `hints` (so a forced reply
/// keeps the judge's mood). Pure.
fn apply_ghosting_killswitch(
    plan: eros_engine_core::types::ActionPlan,
    ghosting_enabled: bool,
    input: &eros_engine_core::types::DecisionInput,
    hints: Vec<String>,
) -> eros_engine_core::types::ActionPlan {
    if !ghosting_enabled && plan.action_type == ActionType::Ghost {
        eros_engine_core::pde::plan_for(
            input,
            ActionType::ReplyText,
            hints,
            None,
            eros_engine_core::types::ImageRef::Face,
            None,
        )
    } else {
        plan
    }
}

/// Build a compact persona disposition block for the PDE judge from EXISTING
/// genome fields. Blank fields are omitted; an all-empty persona yields "".
/// Deliberately excludes `system_prompt` (long; would re-import the chat prompt's
/// framing into the judge) and `topics` (irrelevant to disposition).
fn build_persona_brief(persona: &eros_engine_core::persona::CompanionPersona) -> String {
    use crate::prompt::{meta_i32, meta_str, meta_string_array_joined};
    let name = persona.genome.name.trim();

    let mut bits: Vec<String> = Vec::new();
    if let Some(g) = meta_str(persona, "gender")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        bits.push(g.to_string());
    }
    if let Some(a) = meta_i32(persona, "age") {
        bits.push(format!("{a}岁"));
    }
    if let Some(m) = meta_str(persona, "mbti")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        bits.push(m.to_string());
    }

    let mut lines: Vec<String> = Vec::new();
    let head = match (name.is_empty(), bits.is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!("[角色人格] {}", bits.join("，")),
        (false, true) => format!("[角色人格] {name}"),
        (false, false) => format!("[角色人格] {name}，{}", bits.join("，")),
    };
    if !head.is_empty() {
        lines.push(head);
    }
    if let Some(ss) = meta_str(persona, "speech_style")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        lines.push(format!("说话风格：{ss}"));
    }
    if let Some(q) = meta_string_array_joined(persona, "quirks")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        lines.push(format!("口癖：{q}"));
    }
    if let Some(tp) = persona
        .genome
        .tip_personality
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        lines.push(format!("打赏人格：{tp}"));
    }
    lines.join("\n")
}

/// Build the judge's user payload from the shared transcript + the decision input.
fn build_pde_ctx(
    transcript: &str,
    input: &eros_engine_core::types::DecisionInput,
    image_available: bool,
) -> String {
    let a = &input.affinity;
    let s = &input.signals;
    let latest = match &input.event {
        eros_engine_core::types::Event::UserMessage { content, .. } => content.as_str(),
        _ => "",
    };
    let transcript = if transcript.trim().is_empty() {
        "（无）"
    } else {
        transcript
    };
    let brief = build_persona_brief(&input.persona);
    let persona_block = if brief.is_empty() {
        String::new()
    } else {
        format!("{brief}\n\n")
    };
    // Always emit the image-capability line — the negative is a signal too, so
    // the judge gets a clear "no images this turn" rather than a missing line.
    let image_flag = if image_available { "是" } else { "否" };
    format!(
        "{persona_block}[最近对话]\n{transcript}\n\n\
         [关系状态] warmth={:.2} trust={:.2} intrigue={:.2} intimacy={:.2} patience={:.2} tension={:.2}\n\
         [信号] message_count={} hours_since_last_message={:.1} ghost_streak={} hours_since_last_ghost={}\n\
         [图片能力] 本轮可发图={image_flag}\n\n\
         [用户最新消息]\n{latest}",
        a.warmth,
        a.trust,
        a.intrigue,
        a.intimacy,
        a.patience,
        a.tension,
        s.message_count,
        s.hours_since_last_message,
        s.ghost_streak,
        s.hours_since_last_ghost
            .map(|h| format!("{h:.1}"))
            .unwrap_or_else(|| "none".into()),
    )
}

/// Serializable view of a verdict for the audit `payload` column.
#[derive(serde::Serialize)]
struct VerdictAudit<'a> {
    action: &'a str,
    inner_state: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_prompt: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
}

impl<'a> From<&'a PdeVerdict> for VerdictAudit<'a> {
    fn from(v: &'a PdeVerdict) -> Self {
        VerdictAudit {
            action: v.action.as_str(),
            inner_state: &v.inner_state,
            image_prompt: v.image_prompt.as_deref(),
            reason: v.reason.as_deref(),
        }
    }
}

/// The DB audit string for an acted `ActionType` (matches `assistant_action_type` style).
fn action_type_audit_str(a: ActionType) -> &'static str {
    match a {
        ActionType::ReplyText => "reply_text",
        ActionType::Ghost => "ghost",
        ActionType::ReplyImage => "reply_image",
        ActionType::ReplyTextImage => "reply_text_image",
        ActionType::Proactive => "proactive",
    }
}

/// Fixed schema the `chat_vision` describe model must emit. `description` is
/// required; the optional fields are dropped from the injected preamble when
/// blank (see `model_facing_user_text`).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct ImageVision {
    description: String,
    #[serde(default)]
    ocr_text: Option<String>,
    #[serde(default)]
    people: Option<String>,
    #[serde(default)]
    scene: Option<String>,
}

/// Parse the describe reply: direct JSON first, then a balanced JSON block
/// embedded in prose (mirrors `parse_input_filter_verdict`).
fn parse_image_vision(text: &str) -> Option<ImageVision> {
    serde_json::from_str::<ImageVision>(text).ok().or_else(|| {
        super::post_process::find_json_block(text)
            .and_then(|b| serde_json::from_str::<ImageVision>(b).ok())
    })
}

/// Validity gate for a parsed describe. Reject a `content_filter` finish reason,
/// a blank `description`, or a refusal-shaped description.
fn image_vision_invalidity(v: &ImageVision, finish_reason: Option<&str>) -> Option<&'static str> {
    if finish_reason == Some("content_filter") {
        return Some("content_filter");
    }
    if v.description.trim().is_empty() {
        return Some("blank_description");
    }
    if refusal_in_head(&v.description) {
        return Some("refusal_pattern");
    }
    None
}

/// Outcome of a successful describe — the JSON to persist + audit.
struct VisionOutcome {
    vision: serde_json::Value,
    vision_model: String,
    v_generation_id: Option<String>,
}

/// Run the `chat_vision` describe over the image. Returns `Some(VisionOutcome)`
/// only on a valid parse. Walks the configured model chain, trying the next model
/// on any failure (transport, timeout, empty, unparseable, invalid); returns Some
/// only on a valid describe. Any failure keeps the turn text-only and the
/// placeholder path covers the undescribed image. Each call passes a single model
/// (no internal fallback) so content-level failures also advance the chain.
async fn run_vision(
    state: &AppState,
    v: &eros_engine_llm::model_config::ResolvedVision,
    image_url: &str,
    caption: &str,
) -> Option<VisionOutcome> {
    use eros_engine_llm::openrouter::VisionRequest;
    let caption = caption.trim();
    // Walk [primary, ...fallback] ourselves so a content-level failure (empty /
    // unparseable / invalid describe) advances to the next model — execute_vision
    // only walks the chain on transport/HTTP/decode errors, and it cannot know the
    // ImageVision schema. Each call passes a SINGLE model (no internal fallback).
    let chain: Vec<String> = std::iter::once(v.model.clone())
        .chain(v.fallback_model.iter().cloned())
        .collect();
    for model_id in &chain {
        let req = VisionRequest {
            model: model_id.clone(),
            fallback_model: vec![],
            system_prompt: v.describe_prompt.clone(),
            image_url: image_url.to_string(),
            caption: (!caption.is_empty()).then(|| caption.to_string()),
            temperature: v.temperature as f32,
            max_tokens: v.max_tokens,
            reasoning: v.reasoning.clone(),
        };
        let resp = match tokio::time::timeout(FILTER_TIMEOUT, state.openrouter.execute_vision(req))
            .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::warn!(model = %model_id, error = %e, "chat_vision: model error; next");
                continue;
            }
            Err(_) => {
                tracing::warn!(model = %model_id, "chat_vision: timeout; next");
                continue;
            }
        };
        super::log_openrouter_usage("chat_vision", None, &resp);
        let text = resp.reply.trim().to_string();
        if text.is_empty() {
            tracing::warn!(model = %model_id, "chat_vision: empty reply; next");
            continue;
        }
        let vision = match parse_image_vision(&text) {
            Some(parsed) => parsed,
            None => {
                tracing::warn!(model = %model_id, "chat_vision: unparseable describe JSON; next");
                continue;
            }
        };
        if let Some(reason) = image_vision_invalidity(&vision, resp.finish_reason.as_deref()) {
            tracing::warn!(model = %model_id, invalidity = %reason, "chat_vision: invalid describe; next");
            continue;
        }
        let vision_model = resp.model.unwrap_or_else(|| model_id.clone());
        return Some(VisionOutcome {
            vision: serde_json::to_value(&vision).unwrap_or(serde_json::Value::Null),
            vision_model,
            v_generation_id: resp.generation_id,
        });
    }
    None
}

/// Validity gate for an INPUT rewrite's `content`. Unlike
/// `filter_output_invalidity`, there is NO minimum-length floor — a rewritten
/// user message is naturally short (often < 80 chars). Only a `content_filter`
/// finish reason or a refusal-shaped head is rejected.
fn rewrite_content_invalidity(text: &str, finish_reason: Option<&str>) -> Option<&'static str> {
    if finish_reason == Some("content_filter") {
        return Some("content_filter");
    }
    if refusal_in_head(text) {
        return Some("refusal_pattern");
    }
    None
}

/// Outcome of a successful input rewrite (`None` ⇒ keep the original input).
#[derive(Debug, Clone)]
struct InputRewrite {
    rewritten_text: String,
    filter_model: String,
    reason: Option<String>,
    f_generation_id: Option<String>,
}

/// Recent rows fed to the rewrite LLM as `[最近对话]` context.
const INPUT_FILTER_CONTEXT_TURNS: i64 = 8;

/// Render an assistant transcript line. Image turns persist empty `content`
/// with the image facts under `metadata.image`; surface a terse marker so the
/// judge / input filter see that an image was sent (and what it depicted)
/// instead of a blank `AI:` line. Non-image assistant rows fall back to
/// `content`. Pure.
fn assistant_transcript_line(content: &str, metadata: Option<&serde_json::Value>) -> String {
    if let Some(img) = metadata.and_then(|m| m.get("image")) {
        let subject = img
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("（无描述）");
        let ar = img
            .get("aspect_ratio")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return if ar.is_empty() {
            format!("（发送了一张图片：{subject}）")
        } else {
            format!("（发送了一张图片：{subject}，画幅 {ar}）")
        };
    }
    content.to_string()
}

/// Build the compact transcript block for the input filter, excluding the turn
/// being rewritten. Best-effort: a DB error yields an empty transcript.
async fn build_input_filter_transcript(
    chat_repo: &ChatRepo<'_>,
    session_id: Uuid,
    current_user_message_id: Uuid,
) -> String {
    let rows = chat_repo
        .history(session_id, INPUT_FILTER_CONTEXT_TURNS, 0)
        .await
        .unwrap_or_default();
    let mut lines = Vec::new();
    for m in rows {
        if m.id == current_user_message_id {
            continue;
        }
        // User/gift rows use the EFFECTIVE text (a prior turn's own rewrite when
        // present) so the filter sees the same conversation the chat model does;
        // assistant rows use content (their pre_filter_content means the opposite).
        let (label, text): (&str, String) = match m.role.as_str() {
            "user" | "gift_user" => (
                "用户",
                crate::pipeline::handlers::effective_user_text(&m).to_string(),
            ),
            "assistant" => (
                "AI",
                assistant_transcript_line(&m.content, m.metadata.as_ref()),
            ),
            _ => continue,
        };
        lines.push(format!("{label}: {text}"));
    }
    lines.join("\n")
}

/// Run the input-filter LLM over the raw user input with recent context.
/// Returns `Some(InputRewrite)` ONLY when the model explicitly asked to rewrite
/// with valid content; every other outcome returns `None` ⇒ caller uses the
/// original. The fallback chain is walked ONLY on transport-level failures
/// (error / timeout / empty reply). A CONTENT-level non-success — `{"rewrite":
/// false}`, an unparseable verdict, blank content, or a refusal — is a
/// DEFINITIVE keep: it returns `None` immediately and does NOT try the remaining
/// models, so a fallback can never rewrite a message the primary left alone.
async fn run_input_filter(
    state: &AppState,
    f: &eros_engine_llm::model_config::ResolvedInputFilter,
    recent_transcript: &str,
    raw_input: &str,
) -> Option<InputRewrite> {
    use eros_engine_llm::openrouter::{ChatMessage, ChatRequest};
    let transcript = if recent_transcript.trim().is_empty() {
        "（无）"
    } else {
        recent_transcript
    };
    let user_payload = format!("[最近对话]\n{transcript}\n\n[用户最新输入]\n{raw_input}");
    let chain: Vec<String> = std::iter::once(f.model.clone())
        .chain(f.fallback_model.iter().cloned())
        .collect();
    for model_id in &chain {
        let req = ChatRequest {
            model: model_id.clone(),
            fallback_model: vec![],
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: f.filter_prompt.clone(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: user_payload.clone(),
                },
            ],
            temperature: f.temperature as f32,
            max_tokens: f.max_tokens,
            reasoning: f.reasoning.clone(),
            ..Default::default()
        };
        let resp = match tokio::time::timeout(FILTER_TIMEOUT, state.openrouter.execute(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::warn!(model = %model_id, error = %e, "input-filter: model error; next");
                continue;
            }
            Err(_) => {
                tracing::warn!(model = %model_id, "input-filter: timeout; next");
                continue;
            }
        };
        super::log_openrouter_usage("chat_input_filter", None, &resp);
        let text = resp.reply.trim().to_string();
        if text.is_empty() {
            tracing::warn!(model = %model_id, "input-filter: empty reply; next");
            continue;
        }
        // Content-level non-success ⇒ DEFINITIVE keep (return None, no chain
        // walk). The model responded but not with a usable rewrite; walking to a
        // fallback here would risk rewriting a meaningful message the primary
        // left alone. Only transport failures above (error/timeout/empty) walk.
        let verdict = match parse_input_filter_verdict(&text) {
            Some(v) => v,
            None => {
                tracing::warn!(model = %model_id, "input-filter: unparseable verdict; keep original");
                return None;
            }
        };
        if !verdict.rewrite {
            return None; // meaningful → keep (definitive)
        }
        let content = verdict.content.unwrap_or_default().trim().to_string();
        if content.is_empty() {
            tracing::warn!(model = %model_id, "input-filter: rewrite=true but blank content; keep original");
            return None;
        }
        if let Some(reason) = rewrite_content_invalidity(&content, resp.finish_reason.as_deref()) {
            tracing::warn!(model = %model_id, invalidity = %reason, "input-filter: invalid rewrite content; keep original");
            return None;
        }
        let filter_model = resp.model.unwrap_or_else(|| model_id.clone());
        return Some(InputRewrite {
            rewritten_text: content,
            filter_model,
            reason: verdict.reason.filter(|r| !r.trim().is_empty()),
            f_generation_id: resp.generation_id,
        });
    }
    None // chain exhausted → keep
}

/// Try to emit a pseudo-ghost on chain exhaustion.
///
/// Picks a configured fallback phrase from `engine.error_handling_config`,
/// emits Meta + Delta(phrase) + Done frames as if the LLM returned a brief
/// reply, and persists an assistant row tagged with
/// `metadata.fallback_reason = "stream_failure"`.
///
/// Returns `Some(frames)` when the pseudo-ghost was produced; `None` when
/// the config lookup returns nothing (missing row / empty array / DB error),
/// signalling the caller to fall back to the original Error frame.
#[allow(clippy::too_many_arguments)]
async fn build_stream_failure_pseudo_ghost(
    pool: &sqlx::PgPool,
    session_id: Uuid,
    user_message_id: Uuid,
    frame_action: FrameActionType,
    persist_action: &str,
    plan_action: ActionType,
    trait_tags: &[String],
    tier: &Option<String>,
    memory_scope: eros_engine_core::scope::MemoryScope,
    affinity_scope: eros_engine_core::scope::AffinityScope,
    fallback_retries: u32,
    continues_from_ulid: Option<Ulid>,
) -> Option<(
    Vec<ProtocolFrame>,
    crate::pipeline::post_process::ProducedMessage,
)> {
    let repo = ErrorHandlingRepo { pool };
    let phrase = match repo.pick_chat_stream_fallback_phrase().await {
        Ok(Some(p)) => p,
        Ok(None) => {
            tracing::debug!("stream: no fallback phrase configured; emitting Error frame");
            return None;
        }
        Err(e) => {
            tracing::warn!("stream: fallback phrase lookup failed: {e}; emitting Error frame");
            return None;
        }
    };

    let msg_ulid = Ulid::new();
    let msg_uuid: Uuid = msg_ulid.into();

    // Build metadata bag: fallback_reason + prompt_traits + resolved
    // memory_scope / affinity_scope (mirrors build_metadata's contract so the
    // pseudo-ghost row carries the same post-resolve scope snapshot as a
    // normal assistant row) + optional tier.
    let mut meta_map = serde_json::Map::new();
    meta_map.insert(
        "fallback_reason".into(),
        serde_json::json!("stream_failure"),
    );
    meta_map.insert("prompt_traits".into(), serde_json::json!(trait_tags));
    meta_map.insert(
        "memory_scope".into(),
        serde_json::to_value(memory_scope).expect("MemoryScope serializes"),
    );
    meta_map.insert(
        "affinity_scope".into(),
        serde_json::to_value(affinity_scope).expect("AffinityScope serializes"),
    );
    meta_map.insert("retries_chat".into(), serde_json::json!(fallback_retries));
    if let Some(t) = tier.as_deref() {
        meta_map.insert("tier".into(), serde_json::json!(t));
    }
    let metadata = Some(serde_json::Value::Object(meta_map));

    let chat_repo = ChatRepo { pool };
    let row = eros_engine_store::chat::AssistantInsert {
        id: msg_uuid,
        content: phrase.clone(),
        assistant_action_type: persist_action.into(),
        continues_from_message_id: continues_from_ulid.map(Uuid::from),
        truncated: false,
        // No model served this row — live emits Meta with model: None, and
        // replay_stream applies display_override to Some(...) values, so a
        // sentinel like "__fallback_phrase__" would surface differently on
        // replay than on the original stream and break idempotency.
        // metadata.fallback_reason carries the audit signal instead.
        model: None,
        usage: None,
        generation_id: None,
        filter_audit: None,
        metadata,
    };
    if let Err(e) = chat_repo
        .insert_assistant_batch(session_id, user_message_id, &[row])
        .await
    {
        tracing::warn!("stream: pseudo-ghost persist failed: {e}");
        // Still emit the frames — the row persisting is best-effort.
    }

    let frames = vec![
        ProtocolFrame::Meta {
            message_id: ulid_string(msg_ulid),
            action_type: frame_action,
            model: None,
            continues_from: continues_from_ulid.map(ulid_string),
        },
        ProtocolFrame::Delta {
            message_id: ulid_string(msg_ulid),
            content: phrase.clone(),
        },
        ProtocolFrame::Done {
            message_id: ulid_string(msg_ulid),
            truncated: false,
            usage: None,
            generation_id: None,
        },
    ];
    let produced = crate::pipeline::post_process::ProducedMessage {
        message_id: msg_uuid,
        full_text: phrase,
        action: plan_action,
    };
    Some((frames, produced))
}

/// Emit a replacement bubble carrying the REPAIRED text after the chain ended on
/// byte-BPE garble (issue #84). Mirrors `build_stream_failure_pseudo_ghost` but
/// substitutes the repaired completion for the DB fallback phrase, so the client
/// (which already received the raw garbled deltas) finishes on clean text via the
/// continues_from replacement mechanism.
///
/// NOTE: keep the persist/frame/metadata shape in sync with
/// `build_stream_failure_pseudo_ghost` — the only intended divergences are the
/// content (repaired completion vs DB phrase) and `fallback_reason`.
#[allow(clippy::too_many_arguments)]
async fn build_garble_repaired_replacement(
    pool: &sqlx::PgPool,
    session_id: Uuid,
    user_message_id: Uuid,
    frame_action: FrameActionType,
    persist_action: &str,
    plan_action: ActionType,
    trait_tags: &[String],
    tier: &Option<String>,
    memory_scope: eros_engine_core::scope::MemoryScope,
    affinity_scope: eros_engine_core::scope::AffinityScope,
    fallback_retries: u32,
    continues_from_ulid: Option<Ulid>,
    repaired: String,
) -> (
    Vec<ProtocolFrame>,
    crate::pipeline::post_process::ProducedMessage,
) {
    let msg_ulid = Ulid::new();
    let msg_uuid: Uuid = msg_ulid.into();

    let mut meta_map = serde_json::Map::new();
    meta_map.insert(
        "fallback_reason".into(),
        serde_json::json!("garble_repaired"),
    );
    meta_map.insert("prompt_traits".into(), serde_json::json!(trait_tags));
    meta_map.insert(
        "memory_scope".into(),
        serde_json::to_value(memory_scope).expect("MemoryScope serializes"),
    );
    meta_map.insert(
        "affinity_scope".into(),
        serde_json::to_value(affinity_scope).expect("AffinityScope serializes"),
    );
    meta_map.insert("retries_chat".into(), serde_json::json!(fallback_retries));
    if let Some(t) = tier.as_deref() {
        meta_map.insert("tier".into(), serde_json::json!(t));
    }
    let metadata = Some(serde_json::Value::Object(meta_map));

    let chat_repo = ChatRepo { pool };
    let row = eros_engine_store::chat::AssistantInsert {
        id: msg_uuid,
        content: repaired.clone(),
        assistant_action_type: persist_action.into(),
        continues_from_message_id: continues_from_ulid.map(Uuid::from),
        truncated: false,
        // model: None — same idempotency reason as the pseudo-ghost: replay
        // applies display_override only to Some(...) values, so a sentinel here
        // would surface differently on replay than on the live stream. The
        // metadata.fallback_reason ("garble_repaired") carries the audit signal.
        model: None,
        usage: None,
        generation_id: None,
        filter_audit: None,
        metadata,
    };
    if let Err(e) = chat_repo
        .insert_assistant_batch(session_id, user_message_id, &[row])
        .await
    {
        tracing::warn!("stream: garble-repaired replacement persist failed: {e}");
    }

    let frames = vec![
        ProtocolFrame::Meta {
            message_id: ulid_string(msg_ulid),
            action_type: frame_action,
            model: None,
            continues_from: continues_from_ulid.map(ulid_string),
        },
        ProtocolFrame::Delta {
            message_id: ulid_string(msg_ulid),
            content: repaired.clone(),
        },
        ProtocolFrame::Done {
            message_id: ulid_string(msg_ulid),
            truncated: false,
            usage: None,
            generation_id: None,
        },
    ];
    let produced = crate::pipeline::post_process::ProducedMessage {
        message_id: msg_uuid,
        full_text: repaired,
        action: plan_action,
    };
    (frames, produced)
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
    /// The image URL the client attached to this turn (`https`/`http`), or
    /// `None` for a text/tip-only turn. Drives the `chat_vision` pre-stage.
    pub image_url: Option<String>,
    /// Image reply parameters supplied by the client, forwarded from the request.
    pub image: Option<crate::routes::companion_stream::ImageReplyParams>,
}

/// Produce a stream of `ProtocolFrame` events for a single burst. The
/// generator owns its `AppState` clone so it stays `'static` and survives
/// `Sse`'s body lifetime. Task 10 implements the Ghost branch; T11/T12
/// fill in Reply.
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
        // ── PDE decision (judge-first) ────────────────────────────────────────
        // The judge runs before vision/input-filter/chat so a `ghost` verdict
        // short-circuits all of them. Tip turns and feature-off skip the judge
        // (rule engine). Fail-open: any non-Ok status falls back to pde::decide.
        let is_tip = user_msg.tips_amount_usd.is_some();
        // Per-turn image executor resolution. The executor counts as available
        // only when the caller opted in (an `image` block is present) AND a model
        // resolves this turn. Omitting `image` turns image generation OFF for the
        // turn even when a config `fallback` model exists — mirroring chat_vision,
        // which runs only when `image_url` is present. With no executor, image
        // actions degrade to text (guard_action) and a forced request is ignored.
        //
        // Resolve the chain ONLY when opted in: `effective_image_chain` advances
        // the image ModelSpec round-robin cursor (`ModelSpec::select`), so calling
        // it on opted-out / text turns would consume image-model slots and skew
        // the sequencing of later opted-in image turns.
        let resolved_image_gen = state.model_config.resolve_image_gen();
        let req_image = user_msg.image.as_ref();
        let image_chain = req_image.and_then(|i| {
            eros_engine_llm::model_config::effective_image_chain(
                i.model.as_deref(),
                resolved_image_gen.as_ref(),
            )
        });
        let image_executor_available = image_chain.is_some();
        // Forced image: the client asked for it, an executor chain exists this
        // turn, and it is not a tip turn (tips skip the judge / image path).
        let force_image = req_image.is_some_and(|i| i.force) && image_executor_available && !is_tip;
        // Skip resolution on tip turns: the judge won't run, and resolve_pde()
        // advances the round-robin model cursor as a side effect — resolving on a
        // skipped turn would skew which model later (non-tip) judge calls pick.
        let resolved_pde = if is_tip {
            None
        } else {
            state.model_config.resolve_pde()
        };
        // Shared history transcript: built once, reused by the judge here AND the
        // input filter below (which previously fetched its own). `resolved_pde` is
        // already None on tip turns, so this only fires for a real judge turn.
        let pde_transcript: String = if resolved_pde.is_some() {
            build_input_filter_transcript(&chat_repo, user_msg.session_id, user_msg.user_message_id).await
        } else {
            String::new()
        };
        let mut killswitch_hints: Vec<String> = Vec::new();
        let (mut plan, pde_run): (eros_engine_core::types::ActionPlan, Option<PdeDecisionRun>) =
            match (is_tip, resolved_pde.as_ref()) {
                (false, Some(p)) => {
                    let ctx = build_pde_ctx(&pde_transcript, &input, image_executor_available);
                    let run = run_pde_decision(&state.openrouter, p, &ctx).await;
                    let plan = match (&run.status, &run.verdict) {
                        (PdeStatus::Ok, Some(v)) => {
                            let action = guard_action(
                                v.action,
                                &input.affinity,
                                &input.signals,
                                image_executor_available,
                            );
                            let hints = {
                                let s = sanitize_inner_state(&v.inner_state);
                                if s.is_empty() { Vec::new() } else { vec![s] }
                            };
                            killswitch_hints = hints.clone();
                            // Capture the judge's image prompt while `v` is still
                            // borrowed here (the run/verdict is moved into the
                            // audit task below). Only image actions carry it.
                            let img_prompt = if matches!(
                                action,
                                ActionType::ReplyImage | ActionType::ReplyTextImage
                            ) {
                                v.image_prompt.clone()
                            } else {
                                None
                            };
                            pde::plan_for(&input, action, hints, img_prompt, eros_engine_core::types::ImageRef::Face, None)
                        }
                        _ => pde::decide(&input), // fail-open
                    };
                    (plan, Some(run))
                }
                _ => (pde::decide(&input), None), // tip OR feature off
            };

        // Ghosting kill-switch (§4.1) — path-wide final gate (LLM / fallback /
        // pure-rule / tip). Uses the in-scope sanitized hints, not plan.context_hints.
        plan = apply_ghosting_killswitch(
            plan,
            state.model_config.pde_ghosting_enabled(),
            &input,
            std::mem::take(&mut killswitch_hints),
        );

        // Forced-image override — wins over the PDE/ghost result. Applied AFTER
        // the kill-switch so a client-forced image is never suppressed to ghost.
        // ImageOnly ⇒ ReplyImage; otherwise (TextImage) ⇒ ReplyTextImage. Carries
        // the client-supplied image prompt (not the judge's).
        if force_image {
            let action = match req_image.map(|i| &i.mode) {
                Some(crate::routes::companion_stream::ImageMode::ImageOnly) => {
                    ActionType::ReplyImage
                }
                _ => ActionType::ReplyTextImage,
            };
            plan = pde::plan_for(
                &input,
                action,
                plan.context_hints.clone(),
                req_image.and_then(|i| i.image_prompt.clone()),
                eros_engine_core::types::ImageRef::Face,
                None,
            );
        }

        // Best-effort audit — only when the judge ran; logs the FINAL acted action.
        if let Some(run) = pde_run {
            let pool = state.pool.clone();
            let run_id = uuid::Uuid::new_v4(); // fresh per-run id (spec §8.2)
            let ev_user = user_msg.user_id;
            let ev_session = user_msg.session_id;
            let ev_msg = user_msg.user_message_id;
            let status = run.status.as_str();
            let acted = plan.action_type;
            tokio::spawn(async move {
                let proposed = run.verdict.as_ref().map(|v| v.action.as_str());
                let payload: Option<serde_json::Value> = match &run.verdict {
                    Some(v) => serde_json::to_value(VerdictAudit::from(v)).ok(),
                    None => run.raw.clone().map(serde_json::Value::String),
                };
                let action_str = action_type_audit_str(acted);
                let repo = eros_engine_store::decision::DecisionEventRepo { pool: &pool };
                if let Err(e) = repo
                    .record(eros_engine_store::decision::DecisionEventInsert {
                        run_id,
                        user_id: ev_user,
                        session_id: Some(ev_session),
                        message_id: Some(ev_msg),
                        status,
                        action: Some(action_str),
                        proposed_action: proposed,
                        payload,
                        model: run.model.as_deref(),
                        usage: run.usage.clone(),
                        generation_id: run.generation_id.as_deref(),
                    })
                    .await
                {
                    tracing::warn!("pde: decision-event audit write failed: {e}");
                }
            });
        }

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
            ActionType::ReplyText | ActionType::ReplyImage | ActionType::ReplyTextImage => {
                // ── Image-reply executor wiring (Task 10) ─────────────────────
                // `image_chain` / `resolved_image_gen` / `req_image` were already
                // resolved in the decision block above and are REUSED here — do
                // NOT recompute `effective_image_chain` (it advances a round-robin
                // cursor). The chain is guaranteed `Some` whenever the plan is
                // ReplyImage / ReplyTextImage (guard_action / force only kept the
                // image action when the executor was available). Persona comes
                // from `input.persona` (the local `persona` binding was moved into
                // `input` above).
                //
                // For ReplyImage we generate FIRST and, on success, emit the
                // image-only trio and skip the text path entirely. On failure (or
                // zero images) we fall through to the normal text path so the turn
                // is never empty — no premature `meta` is emitted before the
                // generation outcome is known. For ReplyTextImage the text reply
                // runs as usual and the Image frame is appended after the text
                // `done`.
                let mut image_only_done = false;
                let mut image_only_produced: Vec<crate::pipeline::post_process::ProducedMessage> =
                    Vec::new();

                if matches!(plan.action_type, ActionType::ReplyImage) {
                    if let Some((primary, fallback)) = image_chain.clone() {
                        let subject = plan
                            .image_prompt
                            .as_deref()
                            .filter(|s| !s.trim().is_empty())
                            .or_else(|| {
                                req_image
                                    .and_then(|i| i.image_prompt.as_deref())
                                    .filter(|s| !s.trim().is_empty())
                            })
                            .unwrap_or("")
                            .to_string();
                        let style: eros_engine_llm::model_config::StyleKey = req_image
                            .and_then(|i| i.style)
                            .or_else(|| resolved_image_gen.as_ref().map(|r| r.default_style))
                            .unwrap_or_default();
                        let face_used = req_image
                            .and_then(|i| i.face_ref_url.as_deref())
                            .is_some_and(|s| !s.is_empty());
                        let req = build_image_gen_request(
                            primary,
                            fallback,
                            &input.persona,
                            plan.image_prompt.as_deref(),
                            req_image,
                            resolved_image_gen.as_ref(),
                            "",
                        );
                        let ar = req.aspect_ratio.clone();
                        let res = req.resolution.clone();
                        match state.openrouter.execute_image(req).await {
                            Ok(resp) if !resp.images.is_empty() => {
                                // Usage logging via the ChatResponse adapter.
                                let cr = eros_engine_llm::openrouter::ChatResponse {
                                    reply: String::new(),
                                    generation_id: resp.generation_id.clone(),
                                    model: resp.model.clone(),
                                    usage: resp.usage.clone(),
                                    finish_reason: resp.finish_reason.clone(),
                                };
                                // Mirror the other in-stream task log sites
                                // (vision / filters / pde): session_id = None.
                                super::log_openrouter_usage("chat_image_generation", None, &cr);
                                let style_str = serde_json::to_value(style)
                                    .ok()
                                    .and_then(|v| v.as_str().map(String::from))
                                    .unwrap_or_else(|| "realistic".to_string());
                                let image_meta = serde_json::json!({
                                    "prompt": subject,
                                    "style": style_str,
                                    "model": resp.model,
                                    "aspect_ratio": ar,
                                    "resolution": res,
                                    "generation_id": resp.generation_id,
                                    "face_ref_used": face_used,
                                });
                                let mime = data_url_mime(&resp.images[0]);
                                let msg_ulid = Ulid::new();
                                let msg_uuid: Uuid = msg_ulid.into();
                                let row = eros_engine_store::chat::AssistantInsert {
                                    id: msg_uuid,
                                    content: String::new(),
                                    assistant_action_type: "reply".into(),
                                    continues_from_message_id: None,
                                    truncated: false,
                                    model: resp.model.clone(),
                                    usage: resp.usage.clone(),
                                    generation_id: resp.generation_id.clone(),
                                    filter_audit: None,
                                    metadata: Some(serde_json::json!({ "image": image_meta })),
                                };
                                if let Err(e) = chat_repo
                                    .insert_assistant_batch(
                                        user_msg.session_id,
                                        user_msg.user_message_id,
                                        std::slice::from_ref(&row),
                                    )
                                    .await
                                {
                                    tracing::warn!("stream(image): persist failed: {e}");
                                }
                                // full_text="" so insight/memory extraction skips
                                // this row; affinity uses plan.image_prompt as the
                                // proxy (Task 12).
                                image_only_produced.push(
                                    crate::pipeline::post_process::ProducedMessage {
                                        message_id: msg_uuid,
                                        full_text: String::new(),
                                        action: ActionType::ReplyImage,
                                    },
                                );
                                let mut wire_usage = resp.usage.clone();
                                filter_usage_keys(
                                    &mut wire_usage,
                                    &state.config.openrouter_usage_hidden_keys,
                                );
                                // Frame order: meta → image → done → final.
                                yield ProtocolFrame::Meta {
                                    message_id: ulid_string(msg_ulid),
                                    action_type: FrameActionType::ReplyImage,
                                    model: resp.model.clone(),
                                    continues_from: None,
                                };
                                yield ProtocolFrame::Image {
                                    message_id: ulid_string(msg_ulid),
                                    data_url: resp.images[0].clone(),
                                    mime,
                                    image_prompt: Some(subject.clone()),
                                    model: resp.model.clone(),
                                    generation_id: resp.generation_id.clone(),
                                };
                                yield ProtocolFrame::Done {
                                    message_id: ulid_string(msg_ulid),
                                    truncated: false,
                                    usage: wire_usage,
                                    generation_id: resp.generation_id.clone(),
                                };
                                image_only_done = true;
                            }
                            Ok(_) => {
                                tracing::warn!(
                                    "stream(image): execute_image returned zero images; \
                                     falling through to text"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "stream(image): execute_image failed: {e}; \
                                     falling through to text"
                                );
                            }
                        }
                    }
                }

                // Image-only success: reset ghost streak, emit the computed
                // `final` frame, spawn post-process with the image-only produced
                // message, and skip the text path entirely (returning ends the
                // stream cleanly — there is nothing after the match arm).
                if image_only_done {
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
                        false,
                        None,
                        user_msg.tier.clone(),
                        0,
                        0,
                    )
                    .await;
                    yield final_frame;

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
                    let produced = image_only_produced;
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
                    return;
                }

                // ── Image describe (chat_vision) — Reply turns with an image ──
                // Runs before the input filter; both may fire (orthogonal). The
                // describe result is merged into metadata.vision; the prompt
                // builder folds it via model_facing_user_text. Fail-open: any
                // failure keeps the turn text-only (placeholder covers an
                // undescribed image). Run-once is guaranteed by the upsert
                // idempotency gate — run_stream only runs on a fresh Insert.
                // Skip tipped turns (same as the input filter): a tip persists as
                // role='gift_user' and carries no image (tip+image is rejected at
                // validation), so describing it would waste the call.
                if user_msg.tips_amount_usd.is_none() {
                    if let (Some(image_url), Some(v)) = (
                        user_msg.image_url.as_deref(),
                        state.model_config.resolve_vision(),
                    ) {
                        if let Some(out) = run_vision(&state, &v, image_url, &user_msg.content).await
                        {
                            if let Err(e) = chat_repo
                                .set_user_image_vision(
                                    user_msg.user_message_id,
                                    &out.vision,
                                    &out.vision_model,
                                    out.v_generation_id.as_deref(),
                                )
                                .await
                            {
                                tracing::warn!("stream: chat_vision metadata persist failed: {e}");
                            }
                        }
                    }
                }
                // ── User-input rewrite filter (Reply turns only) ──────────────
                // Runs after the idempotency gate, before prompt assembly. The
                // rewrite is persisted on the user row's pre_filter_content;
                // build_reply_request then feeds the EFFECTIVE text to the model
                // and recall. Fail-open: any non-rewrite outcome is a no-op.
                // Skip tipped turns too: a tip persists as role='gift_user' whose
                // "(打赏 $X)" marker / typed message should reach the model as-is,
                // not be rewritten by the filter — running it would waste the call.
                if user_msg.tips_amount_usd.is_none() {
                    // Per-turn probability gate: `input_filter = 0.8` ⇒ fire on
                    // ~80% of turns; `true` ⇒ probability 1.0 ⇒ always (gen::<f64>()
                    // is in [0,1), so `< 1.0` always fires); `false` ⇒ resolve
                    // returns None and we never get here.
                    if let Some(f) = state
                        .model_config
                        .resolve_input_filter()
                        .filter(|f| rand::thread_rng().gen::<f64>() < f.probability)
                    {
                        // Note: this issues its own small (8-row) history fetch;
                        // build_reply_request below fetches history again (20 rows).
                        // Two round-trips per reply turn — acceptable, not a hot loop.
                        // Reuse the PDE's transcript when it was built this turn;
                        // otherwise fetch (input-filter-only turns: PDE off).
                        let transcript = if !pde_transcript.is_empty() {
                            pde_transcript.clone()
                        } else {
                            build_input_filter_transcript(
                                &chat_repo,
                                user_msg.session_id,
                                user_msg.user_message_id,
                            )
                            .await
                        };
                        if let Some(rw) =
                            run_input_filter(&state, &f, &transcript, &user_msg.content).await
                        {
                            if let Err(e) = chat_repo
                                .set_user_input_rewrite(
                                    user_msg.user_message_id,
                                    &rw.rewritten_text,
                                    &rw.filter_model,
                                    rw.reason.as_deref(),
                                    rw.f_generation_id.as_deref(),
                                )
                                .await
                            {
                                tracing::warn!("stream: input-filter rewrite persist failed: {e}");
                            }
                        }
                    }
                }
                let req_res = crate::pipeline::handlers::build_reply_request(
                    &state, &input, &plan,
                    user_msg.session_id, user_msg.user_id, user_msg.instance_id,
                    user_msg.user_message_id,
                ).await;
                let (req, injected_tags) = match req_res {
                    Ok(r) => r,
                    Err(e) => {
                        yield ProtocolFrame::Error {
                            code: StreamErrorCode::Internal,
                            retryable: false,
                            message: format!("build_reply_request failed: {e}"),
                            user_message: "服务出现问题，请稍后再试".into(),
                        };
                        return;
                    }
                };
                // Optional fire-and-forget raw-prompt disk log (PROMPT_LOG_DIR).
                // Logged once here — before the fallback-model send loop — so a
                // turn that retries across models still produces exactly one file.
                if let Some(dir) = state.config.prompt_log_dir.as_ref() {
                    crate::prompt_log::spawn_write(
                        dir.clone(),
                        &req,
                        user_msg.session_id,
                        user_msg.user_message_id,
                    );
                }
                // The filter trigger's `traits` predicate AND `prompt_injected`
                // both use the KEPT tags (post tier `allow_traits` gating), so a
                // tier that drops a requested trait can't trigger filtering on it.
                let trait_tags: Vec<String> = injected_tags.clone();
                let prompt_injected = if injected_tags.is_empty() { None } else { Some(injected_tags) };
                // Effective text-path action. ReplyText stays ReplyText;
                // ReplyTextImage stays (the trailing Image frame is appended after
                // the text `done` below). A FALLEN-THROUGH ReplyImage (image-gen
                // failed) is downgraded to ReplyText so the text reply is wire-
                // identical to a plain reply (meta.action_type = reply) and no
                // trailing Image frame is attempted.
                let text_action = match plan.action_type {
                    ActionType::ReplyTextImage => ActionType::ReplyTextImage,
                    _ => ActionType::ReplyText,
                };
                // frame_action_for(ReplyText) = Reply; frame_action_for(
                // ReplyTextImage) = ReplyTextImage. `persist_action` stays "reply"
                // for all.
                let (frame_action, persist_action, plan_action) =
                    (frame_action_for(text_action), "reply", text_action);

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
                    user_msg.tier.clone(),
                    user_msg.memory_scope,
                    user_msg.affinity_scope,
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

                // ── ReplyTextImage: append the generated image AFTER the text ──
                // The text reply has already streamed (meta → delta* → done). Now
                // generate the image, merge metadata.image onto the LAST produced
                // assistant row, and yield the Image frame BEFORE `final`. Frame
                // order: meta → delta* → done → image → final. On image failure
                // (or zero images / empty produced) we emit NO Image frame — the
                // text reply already reached the client, so the turn is complete.
                if matches!(plan.action_type, ActionType::ReplyTextImage) {
                    if let (Some(last), Some((primary, fallback))) =
                        (produced.last(), image_chain.clone())
                    {
                        let msg_uuid = last.message_id;
                        let subject = plan
                            .image_prompt
                            .as_deref()
                            .filter(|s| !s.trim().is_empty())
                            .or_else(|| {
                                req_image
                                    .and_then(|i| i.image_prompt.as_deref())
                                    .filter(|s| !s.trim().is_empty())
                            })
                            .unwrap_or("")
                            .to_string();
                        let style: eros_engine_llm::model_config::StyleKey = req_image
                            .and_then(|i| i.style)
                            .or_else(|| resolved_image_gen.as_ref().map(|r| r.default_style))
                            .unwrap_or_default();
                        let face_used = req_image
                            .and_then(|i| i.face_ref_url.as_deref())
                            .is_some_and(|s| !s.is_empty());
                        let req = build_image_gen_request(
                            primary,
                            fallback,
                            &input.persona,
                            plan.image_prompt.as_deref(),
                            req_image,
                            resolved_image_gen.as_ref(),
                            "",
                        );
                        let ar = req.aspect_ratio.clone();
                        let res = req.resolution.clone();
                        match state.openrouter.execute_image(req).await {
                            Ok(resp) if !resp.images.is_empty() => {
                                let cr = eros_engine_llm::openrouter::ChatResponse {
                                    reply: String::new(),
                                    generation_id: resp.generation_id.clone(),
                                    model: resp.model.clone(),
                                    usage: resp.usage.clone(),
                                    finish_reason: resp.finish_reason.clone(),
                                };
                                // Mirror the other in-stream task log sites
                                // (vision / filters / pde): session_id = None.
                                super::log_openrouter_usage("chat_image_generation", None, &cr);
                                let style_str = serde_json::to_value(style)
                                    .ok()
                                    .and_then(|v| v.as_str().map(String::from))
                                    .unwrap_or_else(|| "realistic".to_string());
                                let image_meta = serde_json::json!({
                                    "prompt": subject,
                                    "style": style_str,
                                    "model": resp.model,
                                    "aspect_ratio": ar,
                                    "resolution": res,
                                    "generation_id": resp.generation_id,
                                    "face_ref_used": face_used,
                                });
                                let mime = data_url_mime(&resp.images[0]);
                                // merge_assistant_image_meta wraps under {"image":..}
                                // itself — pass the raw image_meta.
                                if let Err(e) = chat_repo
                                    .merge_assistant_image_meta(
                                        user_msg.session_id,
                                        msg_uuid,
                                        &image_meta,
                                    )
                                    .await
                                {
                                    tracing::warn!(
                                        "stream(text_image): merge image meta failed: {e}"
                                    );
                                }
                                yield ProtocolFrame::Image {
                                    message_id: ulid_string(Ulid::from(msg_uuid)),
                                    data_url: resp.images[0].clone(),
                                    mime,
                                    image_prompt: Some(subject.clone()),
                                    model: resp.model.clone(),
                                    generation_id: resp.generation_id.clone(),
                                };
                            }
                            Ok(_) => {
                                tracing::warn!(
                                    "stream(text_image): execute_image returned zero images; \
                                     no Image frame (text already delivered)"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "stream(text_image): execute_image failed: {e}; \
                                     no Image frame (text already delivered)"
                                );
                            }
                        }
                    }
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
                let mut plan_bg = plan.clone();
                // A `reply_image` only reaches the text path by falling through on image-gen
                // failure (the success path returns earlier via image_only_done). The turn
                // became a real text reply, so post-process (lead refresh, affinity, insight,
                // memory) must treat it as ReplyText — not ReplyImage, which would skip lead.
                if plan_bg.action_type == ActionType::ReplyImage {
                    plan_bg.action_type = ActionType::ReplyText;
                }
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
                let action = FrameActionType::Reply;
                yield ProtocolFrame::Meta {
                    message_id: ulid_string(msg_ulid),
                    action_type: action,
                    // When the persisted row carries no model (e.g. the
                    // pseudo-ghost fallback path), the live stream emitted
                    // model: None — preserve that on replay so idempotent
                    // retries are wire-identical regardless of any
                    // display_override config.
                    model: row.model.as_deref().and_then(|m| {
                        display_override.as_ref().and_then(|d| d.display(m))
                    }),
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

    fn test_persona_with_meta(
        pairs: &[(&str, &str)],
    ) -> eros_engine_core::persona::CompanionPersona {
        use eros_engine_core::persona::{CompanionPersona, PersonaGenome, PersonaInstance};
        let iid = uuid::Uuid::new_v4();
        let gid = uuid::Uuid::new_v4();
        let oid = uuid::Uuid::new_v4();
        let meta: serde_json::Map<String, serde_json::Value> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
            .collect();
        CompanionPersona {
            instance_id: iid,
            genome: PersonaGenome {
                id: gid,
                name: "TestPersona".into(),
                system_prompt: "You are TestPersona.".into(),
                tip_personality: None,
                art_metadata: serde_json::Value::Object(meta),
            },
            instance: PersonaInstance {
                id: iid,
                genome_id: gid,
                owner_uid: oid,
                status: "active".into(),
            },
        }
    }

    #[test]
    fn build_image_gen_request_uses_subject_chain_and_style() {
        let persona = test_persona_with_meta(&[]);
        let plan_prompt = Some("a selfie".to_string());
        let resolved = eros_engine_llm::model_config::ModelConfig::from_toml_str(
            "[tasks.chat_image_generation]\nmodel=\"img\"\ndefault_style=\"realistic\"\ndefault_aspect_ratio=\"3:4\"\n",
        )
        .unwrap()
        .resolve_image_gen();
        let (primary, chain) = ("img".to_string(), vec![]);
        let req = build_image_gen_request(
            primary,
            chain,
            &persona,
            plan_prompt.as_deref(),
            None, /* req_image */
            resolved.as_ref(),
            "fallback subject",
        );
        assert_eq!(req.model, "img");
        assert!(req.prompt.starts_with("Photorealistic"));
        assert!(req.prompt.contains("a selfie")); // plan image_prompt wins over fallback subject
        assert_eq!(req.aspect_ratio.as_deref(), Some("3:4"));
    }

    #[test]
    fn data_url_mime_parses_prefix_and_defaults() {
        assert_eq!(data_url_mime("data:image/png;base64,AAAA"), "image/png");
        assert_eq!(data_url_mime("data:image/jpeg;base64,ZZ"), "image/jpeg");
        // No data: prefix → default.
        assert_eq!(data_url_mime("https://x/y.png"), "image/png");
        // Malformed/empty mime → default.
        assert_eq!(data_url_mime("data:;base64,AAAA"), "image/png");
    }

    #[test]
    fn image_frame_serializes_with_type_tag() {
        let f = ProtocolFrame::Image {
            message_id: "m1".into(),
            data_url: "data:image/png;base64,AAAA".into(),
            mime: "image/png".into(),
            image_prompt: Some("a cat".into()),
            model: Some("img-a".into()),
            generation_id: Some("gen_1".into()),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(v["type"], "image");
        assert_eq!(v["data_url"], "data:image/png;base64,AAAA");
        assert_eq!(v["image_prompt"], "a cat");
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

    #[test]
    fn parse_pde_verdict_all_actions() {
        for (s, want) in [
            ("reply_text", PdeAction::ReplyText),
            ("ghost", PdeAction::Ghost),
            ("reply_image", PdeAction::ReplyImage),
            ("reply_text_image", PdeAction::ReplyTextImage),
        ] {
            let j = format!("{{\"action\":\"{s}\",\"inner_state\":\"ok\"}}");
            assert_eq!(parse_pde_verdict(&j).unwrap().action, want);
        }
        // embedded in prose
        let v =
            parse_pde_verdict("noise {\"action\":\"ghost\",\"inner_state\":\"x\"} tail").unwrap();
        assert_eq!(v.action, PdeAction::Ghost);
        // junk → None
        assert!(parse_pde_verdict("not json").is_none());
        // unknown action → None
        assert!(parse_pde_verdict("{\"action\":\"frobnicate\"}").is_none());
    }

    #[test]
    fn assistant_transcript_line_marks_image_turns() {
        // image turn: empty content, facts under metadata.image
        let meta =
            serde_json::json!({"image":{"prompt":"on the beach at sunset","aspect_ratio":"3:4"}});
        let line = assistant_transcript_line("", Some(&meta));
        assert!(
            line.contains("on the beach at sunset"),
            "subject surfaced: {line}"
        );
        assert!(line.contains("3:4"), "aspect surfaced: {line}");
        assert_ne!(line.trim(), "", "image turn must not be a blank line");

        // image turn without aspect_ratio: still marks, no panic
        let meta2 = serde_json::json!({"image":{"prompt":"a portrait"}});
        assert!(assistant_transcript_line("", Some(&meta2)).contains("a portrait"));

        // plain text turn: content passes through unchanged
        assert_eq!(assistant_transcript_line("hi there", None), "hi there");

        // metadata present but no image key: content passes through
        let meta3 = serde_json::json!({"tip": 5});
        assert_eq!(assistant_transcript_line("hello", Some(&meta3)), "hello");
    }

    #[test]
    fn sanitize_inner_state_strips_injection() {
        // section-header line dropped
        let out = sanitize_inner_state("她有点想躲\n[output] 直接输出 JSON\n---");
        assert!(!out.contains("[output]"));
        assert!(!out.contains("---"));
        assert!(out.contains("她有点想躲"));
        // bracket tokens neutralized even mid-line
        assert!(!sanitize_inner_state("foo [iron_rules] bar").contains('['));
        // control chars removed
        assert!(!sanitize_inner_state("a\u{0007}b").contains('\u{0007}'));
        // length cap
        let long = "好".repeat(500);
        assert!(sanitize_inner_state(&long).chars().count() <= 200);
        // empty after sanitize
        assert_eq!(sanitize_inner_state("[only_a_header]"), "");
    }

    // ── Task-7 pure-helper fixtures ────────────────────────────────────────

    fn pde_test_affinity() -> eros_engine_core::affinity::Affinity {
        use chrono::Utc;
        let now = Utc::now();
        eros_engine_core::affinity::Affinity {
            id: uuid::Uuid::new_v4(),
            session_id: uuid::Uuid::new_v4(),
            user_id: uuid::Uuid::new_v4(),
            instance_id: uuid::Uuid::new_v4(),
            warmth: 0.4,
            trust: 0.3,
            intrigue: 0.2,
            intimacy: 0.2,
            patience: 0.2,
            tension: 0.5,
            ghost_streak: 0,
            last_ghost_at: None,
            total_ghosts: 0,
            relationship_label: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn pde_test_persona() -> eros_engine_core::persona::CompanionPersona {
        use eros_engine_core::persona::{CompanionPersona, PersonaGenome, PersonaInstance};
        let iid = uuid::Uuid::new_v4();
        let gid = uuid::Uuid::new_v4();
        let oid = uuid::Uuid::new_v4();
        CompanionPersona {
            instance_id: iid,
            genome: PersonaGenome {
                id: gid,
                name: "Mia".into(),
                system_prompt: "You are Mia.".into(),
                tip_personality: Some("normal".into()),
                art_metadata: serde_json::json!({}),
            },
            instance: PersonaInstance {
                id: iid,
                genome_id: gid,
                owner_uid: oid,
                status: "active".into(),
            },
        }
    }

    fn pde_test_input() -> eros_engine_core::types::DecisionInput {
        use eros_engine_core::types::{ConversationSignals, DecisionInput, Event};
        DecisionInput {
            event: Event::UserMessage {
                content: "hi".into(),
                message_id: uuid::Uuid::new_v4(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
            },
            affinity: pde_test_affinity(),
            persona: pde_test_persona(),
            signals: ConversationSignals {
                message_count: 50,
                hours_since_last_message: 1.0,
                ghost_streak: 0,
                hours_since_last_ghost: Some(5.0),
            },
        }
    }

    fn sigs(
        message_count: i64,
        hours_since_last_ghost: Option<f64>,
    ) -> eros_engine_core::types::ConversationSignals {
        eros_engine_core::types::ConversationSignals {
            message_count,
            hours_since_last_message: 1.0,
            ghost_streak: 0,
            hours_since_last_ghost,
        }
    }

    #[test]
    fn guard_action_degrades_and_honours() {
        use eros_engine_core::affinity::Affinity;
        let a = Affinity {
            ghost_streak: 0,
            ..pde_test_affinity()
        };
        // ghost honoured when permitted
        assert_eq!(
            guard_action(PdeAction::Ghost, &a, &sigs(50, Some(5.0)), false),
            ActionType::Ghost
        );
        // ghost vetoed by new-relationship floor
        assert_eq!(
            guard_action(PdeAction::Ghost, &a, &sigs(3, None), false),
            ActionType::ReplyText
        );
        // image actions degrade to text when no executor chain
        assert_eq!(
            guard_action(PdeAction::ReplyImage, &a, &sigs(50, None), false),
            ActionType::ReplyText
        );
        assert_eq!(
            guard_action(PdeAction::ReplyTextImage, &a, &sigs(50, None), false),
            ActionType::ReplyText
        );
        assert_eq!(
            guard_action(PdeAction::ReplyText, &a, &sigs(50, None), false),
            ActionType::ReplyText
        );
    }

    #[test]
    fn guard_action_keeps_image_when_executor_available() {
        let aff = test_affinity();
        let sig = test_signals();
        assert_eq!(
            guard_action(PdeAction::ReplyImage, &aff, &sig, true),
            ActionType::ReplyImage
        );
        assert_eq!(
            guard_action(PdeAction::ReplyTextImage, &aff, &sig, true),
            ActionType::ReplyTextImage
        );
        // executor unavailable → degrade (today's behaviour)
        assert_eq!(
            guard_action(PdeAction::ReplyImage, &aff, &sig, false),
            ActionType::ReplyText
        );
        assert_eq!(
            guard_action(PdeAction::ReplyTextImage, &aff, &sig, false),
            ActionType::ReplyText
        );
    }

    #[test]
    fn killswitch_downgrades_ghost_keeping_hints() {
        let input = pde_test_input();
        let ghost_plan = eros_engine_core::pde::plan_for(
            &input,
            ActionType::Ghost,
            vec![],
            None,
            eros_engine_core::types::ImageRef::Face,
            None,
        );
        // ghosting enabled → unchanged
        let kept = apply_ghosting_killswitch(ghost_plan.clone(), true, &input, vec!["想躲".into()]);
        assert_eq!(kept.action_type, ActionType::Ghost);
        // ghosting disabled → downgraded to ReplyText carrying the hints
        let down = apply_ghosting_killswitch(ghost_plan, false, &input, vec!["想躲".into()]);
        assert_eq!(down.action_type, ActionType::ReplyText);
        assert_eq!(down.context_hints, vec!["想躲".to_string()]);
    }

    #[test]
    fn ghost_then_killswitch_yields_reply_with_hints() {
        let input = pde_test_input(); // msg_count=50, cooldown clear → ghost permitted
        let acted = guard_action(PdeAction::Ghost, &input.affinity, &input.signals, false);
        assert_eq!(acted, ActionType::Ghost); // permitted

        let hints = vec![sanitize_inner_state("有点想躲")];
        let plan = pde::plan_for(
            &input,
            acted,
            hints.clone(),
            None,
            eros_engine_core::types::ImageRef::Face,
            None,
        );
        // ghosting disabled → suppressed to reply, hints preserved
        let final_plan = apply_ghosting_killswitch(plan, false, &input, hints.clone());
        assert_eq!(final_plan.action_type, ActionType::ReplyText);
        assert_eq!(final_plan.context_hints, hints);
        // audit would log proposed=ghost, action=reply_text:
        assert_eq!(PdeAction::Ghost.as_str(), "ghost");
        assert_eq!(action_type_audit_str(final_plan.action_type), "reply_text");
    }

    use sqlx::PgPool;

    async fn seed_persona_and_session(pool: &PgPool, user_id: Uuid) -> (Uuid, Uuid, Uuid) {
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('GhostTest', 'sp', '{}'::jsonb) RETURNING id",
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
                image_url: None,
                image: None,
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
            pre_filter_content: None,
            metadata: None,
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
                image_url: None,
                image: None,
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
                image_url: None,
                image: None,
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
                image_url: None,
                image: None,
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
            pre_filter_content: None,
            metadata: None,
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

    // ── filter_output_invalidity unit tests ──────────────────────────────────

    #[test]
    fn filter_output_invalidity_detects_chinese_refusal_in_head() {
        let text = "抱歉，我无法协助完成您的请求。";
        assert_eq!(
            filter_output_invalidity(text, None),
            Some("refusal_pattern"),
            "Chinese refusal in head must be detected"
        );
    }

    #[test]
    fn filter_output_invalidity_detects_english_refusal_in_head() {
        let text = "I'm sorry, but I can't rewrite this content.";
        assert_eq!(
            filter_output_invalidity(text, None),
            Some("refusal_pattern"),
            "English refusal in head must be detected"
        );
    }

    #[test]
    fn filter_output_invalidity_detects_content_filter_finish_reason() {
        // Long text that would otherwise pass — finish_reason overrides.
        let text = "她轻轻地望向窗外，思绪飘向了远方。阳光洒在她的脸上，温柔而明亮。她记得那个夏天，记得他说过的每一句话，记得那些再也回不去的日子。";
        assert_eq!(
            filter_output_invalidity(text, Some("content_filter")),
            Some("content_filter"),
            "content_filter finish_reason must be detected regardless of text length"
        );
    }

    #[test]
    fn filter_output_invalidity_short_response_with_refusal_verb() {
        let text = "我无法。";
        assert_eq!(
            filter_output_invalidity(text, None),
            Some("refusal_pattern"),
            "short text containing refusal verb must be flagged as refusal_pattern"
        );
    }

    #[test]
    fn filter_output_invalidity_short_response_without_refusal_verb() {
        // A genuinely short clean rewrite — still fails the length gate.
        let text = "她笑了。";
        assert_eq!(
            filter_output_invalidity(text, None),
            Some("too_short"),
            "short text with no refusal verb must be flagged as too_short"
        );
    }

    #[test]
    fn filter_output_invalidity_passes_long_clean_rewrite() {
        // 200+ chars, finish_reason = "stop", no refusal pattern.
        let text = "她轻轻地望向窗外，思绪飘向了远方。阳光洒在她的脸上，温柔而明亮。她记得那个夏天，记得他说过的每一句话，记得那些再也回不去的日子。风轻轻吹过，带走了她的叹息，也带走了那些沉甸甸的思念。";
        assert_eq!(
            filter_output_invalidity(text, Some("stop")),
            None,
            "long clean rewrite with stop finish_reason must pass the gate"
        );
    }

    #[test]
    fn filter_output_invalidity_detects_lowercase_english_refusal() {
        // Codex regression guard: a model that emits the apology shape with
        // lowercase `i` / `ai` (or all-caps `I'M SORRY`) must still be caught,
        // because the gate runs case-insensitively after lowercasing the head.
        let lower = "i'm sorry, but i can't help with rewriting that content. it's outside what i can produce safely.";
        assert_eq!(
            filter_output_invalidity(lower, None),
            Some("refusal_pattern"),
            "lowercase apology must hit the head pattern via case-insensitive match"
        );
        let mixed = "As an ai language model, I am not able to rewrite the text in the way you have requested.";
        assert_eq!(
            filter_output_invalidity(mixed, None),
            Some("refusal_pattern"),
            "mixed-case 'As an ai' must still match the lowercase pattern"
        );
        let upper = "I'M SORRY, BUT I CAN'T REWRITE THIS PASSAGE IN THE FORM YOU'VE REQUESTED — IT VIOLATES POLICY.";
        assert_eq!(
            filter_output_invalidity(upper, None),
            Some("refusal_pattern"),
            "uppercase apology must match via lowercased head"
        );
    }

    #[test]
    fn filter_output_invalidity_passes_when_refusal_word_appears_late() {
        // Regression guard: a clean rewrite that incidentally contains "won't"
        // well past character 120 must NOT be flagged.  The prefix must be
        // >= REFUSAL_HEAD_SCAN_CHARS (120) chars so "won't" lands outside the
        // scan window.  The full text must also be >= MIN_FILTERED_OUTPUT_CHARS
        // (80) so it does not hit the too_short branch.
        let prefix = "她轻轻地望向窗外，思绪飘向了远方。阳光洒在她的脸上，温柔而明亮。她记得那个夏天，记得他说过的每一句话，那些记忆再也不会消逝。她告诉自己要坚强，岁月会带走一切，但那段回忆会永远珍藏在心底，无论时光如何流逝，她都不会忘记那些岁月里的每一天每一刻。";
        // suffix contains "won't" deep in the text — past the 120-char head window.
        let text = format!("{prefix}但她won't忘记那段岁月，那是她最珍贵的时光，永远珍藏心底。");
        // Verify the premise: prefix is beyond the scan window.
        let prefix_chars = prefix.chars().count();
        assert!(
            prefix_chars >= REFUSAL_HEAD_SCAN_CHARS,
            "prefix must be >= {REFUSAL_HEAD_SCAN_CHARS} chars so won't is outside the head window; got {prefix_chars}"
        );
        assert!(
            text.chars().count() >= MIN_FILTERED_OUTPUT_CHARS,
            "full text must be >= {MIN_FILTERED_OUTPUT_CHARS} chars to bypass too_short; got {}",
            text.chars().count()
        );
        assert_eq!(
            filter_output_invalidity(&text, Some("stop")),
            None,
            "refusal word past char 120 must not trigger refusal_pattern"
        );
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
        // The filtered content must be >= MIN_FILTERED_OUTPUT_CHARS (80) chars to
        // pass the validity gate (a real rewrite is always that long).
        let filt_text = "FILT_START 她轻轻地望向窗外，思绪飘向了远方。阳光洒在她的脸上，温柔而明亮。她记得那个夏天的每一天，岁月如流水般逝去，带走了所有的悲欢离合。 FILT_END";
        let filt_body = serde_json::json!({
            "id": "gf", "model": "fast/m",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": filt_text}}],
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
                image_url: None,
                image: None,
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
                deltas.contains("FILT_START"),
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
            assert!(
                row.contains("FILT_START"),
                "persisted content must be the filtered text, got {row:?}"
            );
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
                image_url: None,
                image: None,
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
    async fn filter_fail_open_writes_attempt_audit_to_metadata(pool: PgPool) {
        // Filter chain = primary + 1 fallback. Both return refusal text (200 OK
        // with a Chinese refusal phrase) → validity gate rejects both → engine
        // fails open, emits the ORIGINAL reply, and the persisted row's metadata
        // carries filter_outcome=fail_open + filter_attempts (2 entries).
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let chat_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ORIG\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"id\":\"g\",\"model\":\"primary\"}\n\ndata: [DONE]\n\n";

        // Both filter models return a refusal — Chinese phrase caught by the
        // head-pattern gate.
        let refusal_body_1 = serde_json::json!({
            "id": "gf1", "model": "filter-1",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": "抱歉，我无法协助完成您的请求。"}}],
        });
        let refusal_body_2 = serde_json::json!({
            "id": "gf2", "model": "filter-2",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": "抱歉，我无法协助完成您的请求。"}}],
        });

        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("filter-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(refusal_body_1))
            .mount(&mock)
            .await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("filter-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(refusal_body_2))
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
                 [tasks.chat_output_filter]\nmodel=\"filter-1\"\nfallback=[\"filter-2\"]\n\
                 retry_depth=1\nfilter_prompt=\"REWRITE\"\ntrigger = { random = 1.0 }\n",
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
                "hello",
                "01JFAILOPEN111111111111111",
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
                content: "hello".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
                image_url: None,
                image: None,
            },
        )
        .collect()
        .await;

        // Only run assertions when PDE chose Reply (not Ghost).
        if frames
            .iter()
            .any(|f| matches!(f, ProtocolFrame::Delta { .. }))
        {
            // Client must see the original, not the refusals.
            let deltas: String = frames
                .iter()
                .filter_map(|f| match f {
                    ProtocolFrame::Delta { content, .. } => Some(content.clone()),
                    _ => None,
                })
                .collect();
            assert!(
                deltas.contains("ORIG"),
                "fail-open must emit original, got {deltas:?}"
            );

            // final.filtered must be false (we failed open).
            let filtered = frames
                .iter()
                .find_map(|f| match f {
                    ProtocolFrame::Final { filtered, .. } => Some(*filtered),
                    _ => None,
                })
                .unwrap();
            assert!(!filtered, "final.filtered must be false on fail-open");

            // The persisted row must carry the fail-open audit in metadata.
            let metadata: serde_json::Value = sqlx::query_scalar(
                "SELECT metadata FROM engine.chat_messages \
                 WHERE session_id=$1 AND role='assistant' ORDER BY sent_at DESC LIMIT 1",
            )
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .unwrap();

            assert_eq!(
                metadata["filter_outcome"], "fail_open",
                "metadata.filter_outcome must be 'fail_open', got {metadata}"
            );
            let attempts = metadata["filter_attempts"].as_array().unwrap();
            assert_eq!(
                attempts.len(),
                2,
                "both filter models must be recorded in filter_attempts, got {attempts:?}"
            );
            // Both should have reason=refusal_pattern.
            for attempt in attempts {
                assert_eq!(
                    attempt["reason"], "refusal_pattern",
                    "expected refusal_pattern reason, got {attempt}"
                );
            }
            // f_client_msg_id must be present and start with "f_".
            let fid = metadata["f_client_msg_id"].as_str().unwrap();
            assert!(
                fid.starts_with("f_"),
                "f_client_msg_id must start with 'f_', got {fid}"
            );
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn filter_success_does_not_write_fail_open_metadata(pool: PgPool) {
        // Sanity: when filter succeeds the metadata does NOT contain
        // filter_outcome / filter_attempts keys (no false-positive audit).
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let chat_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ORIG\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"id\":\"g\",\"model\":\"primary\"}\n\ndata: [DONE]\n\n";
        let filt_text = "FILT_OK 她轻轻地望向窗外，思绪飘向了远方。阳光洒在她的脸上，温柔而明亮。她记得那个夏天的每一天，岁月如流水般逝去，带走了所有的悲欢离合。 FILT_OK_END";
        let filt_body = serde_json::json!({
            "id": "gf", "model": "fast/m",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": filt_text}}],
        });

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
                "hello",
                "01JFILTSUCCESS1111111111A",
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
                content: "hello".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
                image_url: None,
                image: None,
            },
        )
        .collect()
        .await;

        if frames
            .iter()
            .any(|f| matches!(f, ProtocolFrame::Delta { .. }))
        {
            // Filter succeeded — no fail-open audit keys must appear.
            let metadata: serde_json::Value = sqlx::query_scalar(
                "SELECT metadata FROM engine.chat_messages \
                 WHERE session_id=$1 AND role='assistant' ORDER BY sent_at DESC LIMIT 1",
            )
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .unwrap();

            assert!(
                metadata.get("filter_outcome").is_none(),
                "successful filter must not write filter_outcome, got {metadata}"
            );
            assert!(
                metadata.get("filter_attempts").is_none(),
                "successful filter must not write filter_attempts, got {metadata}"
            );
            // prompt_traits must still be present.
            assert!(
                metadata.get("prompt_traits").is_some(),
                "prompt_traits must still be present, got {metadata}"
            );
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
                image_url: None,
                image: None,
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
                image_url: None,
                image: None,
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
            sent.contains("[tip_received]") && sent.contains("$20 美元的红包"),
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
                image_url: None,
                image: None,
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

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn assistant_row_writes_memory_and_affinity_scope_keys(pool: PgPool) {
        // Success-path sanity: the assistant row's metadata must carry the
        // POST-resolve memory_scope (snake_case enum string) + affinity_scope
        // (6-bool record) on every turn — paired with the user row's
        // *_raw counterparts so ops can diff for shape mismatches.
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let chat_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ORIG\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"id\":\"g\",\"model\":\"primary\"}\n\ndata: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
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
                "01JSCOPEKEYS1111111111111A",
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
                image_url: None,
                image: None,
            },
        )
        .collect()
        .await;

        // Only assert when PDE chose Reply (not Ghost) — same gate as siblings.
        if frames
            .iter()
            .any(|f| matches!(f, ProtocolFrame::Delta { .. }))
        {
            let metadata: serde_json::Value = sqlx::query_scalar(
                "SELECT metadata FROM engine.chat_messages \
                 WHERE session_id = $1 AND role = 'assistant' ORDER BY sent_at DESC LIMIT 1",
            )
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .unwrap();

            assert_eq!(
                metadata["memory_scope"],
                serde_json::json!("neutral_and_relationship"),
                "default MemoryScope should serialize as snake_case, got {metadata}",
            );
            assert!(
                metadata["affinity_scope"].is_object(),
                "AffinityScope serializes as a 6-boolean record, got {metadata}",
            );
            // Default AffinityScope is `bond` = {warmth, intimacy, tension}=true;
            // trust, intrigue, patience=false.
            assert_eq!(
                metadata["affinity_scope"]["warmth"],
                serde_json::json!(true)
            );
            assert_eq!(
                metadata["affinity_scope"]["intimacy"],
                serde_json::json!(true)
            );
            assert_eq!(
                metadata["affinity_scope"]["tension"],
                serde_json::json!(true)
            );
            assert_eq!(
                metadata["affinity_scope"]["trust"],
                serde_json::json!(false)
            );
            assert_eq!(
                metadata["affinity_scope"]["intrigue"],
                serde_json::json!(false)
            );
            assert_eq!(
                metadata["affinity_scope"]["patience"],
                serde_json::json!(false)
            );
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn pseudo_ghost_assistant_row_carries_scope_metadata(pool: PgPool) {
        // Chain-exhaustion path: primary returns an empty SSE stream ⇒
        // `acc.is_empty()` flips `truncated = true`. With no fallback model
        // configured the chain = [primary], so `idx + 1 == chain.len()` ⇒
        // build_stream_failure_pseudo_ghost fires. The pseudo-ghost row's
        // metadata must carry memory_scope + affinity_scope alongside the
        // existing fallback_reason = "stream_failure" audit signal.
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        // Empty SSE stream ⇒ acc stays empty ⇒ truncated path.
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw("data: [DONE]\n\n", "text/event-stream"),
            )
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;
        let mut state = crate::routes::companion::test_state(pool.clone());
        // Default ModelConfig has empty fallback_model ⇒ chain = [primary],
        // so a single truncated attempt exhausts the chain. The compiled-in
        // FALLBACK_MODEL is used as primary; it's only ever passed through
        // to the mocked openrouter, never actually served.
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
                "01JPSEUDOGHOSTSCOPE1111111",
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
                image_url: None,
                image: None,
            },
        )
        .collect()
        .await;

        // Only assert when PDE chose Reply (not Ghost). Inside that gate the
        // pseudo-ghost must have run (chain = [primary], primary truncated).
        if frames
            .iter()
            .any(|f| matches!(f, ProtocolFrame::Delta { .. }))
        {
            // The pseudo-ghost row is the LATEST assistant row (and the only
            // one in live mode where the truncated attempt also persists a
            // bubble — we want the most recent, which is the pseudo-ghost).
            let metadata: serde_json::Value = sqlx::query_scalar(
                "SELECT metadata FROM engine.chat_messages \
                 WHERE session_id = $1 AND role = 'assistant' \
                   AND metadata->>'fallback_reason' = 'stream_failure' \
                 ORDER BY sent_at DESC LIMIT 1",
            )
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .unwrap();

            assert_eq!(
                metadata["fallback_reason"],
                serde_json::json!("stream_failure"),
                "this test must exercise the pseudo-ghost path, got {metadata}",
            );
            assert!(
                metadata.get("memory_scope").is_some(),
                "pseudo-ghost row must carry memory_scope, got {metadata}",
            );
            assert!(
                metadata.get("affinity_scope").is_some(),
                "pseudo-ghost row must carry affinity_scope, got {metadata}",
            );
            assert_eq!(
                metadata["memory_scope"],
                serde_json::json!("neutral_and_relationship"),
                "default MemoryScope should serialize as snake_case, got {metadata}",
            );
            // Spot-check the affinity_scope shape (full 6-bool assertions are
            // already covered in the success-path test above).
            assert!(
                metadata["affinity_scope"].is_object(),
                "AffinityScope serializes as a 6-boolean record, got {metadata}",
            );
            assert_eq!(
                metadata["affinity_scope"]["warmth"],
                serde_json::json!(true)
            );
            assert_eq!(
                metadata["affinity_scope"]["trust"],
                serde_json::json!(false)
            );
        }
    }

    #[test]
    fn parse_input_filter_verdict_direct_and_embedded() {
        let v = parse_input_filter_verdict(r#"{"rewrite": false}"#).unwrap();
        assert!(!v.rewrite);

        let v = parse_input_filter_verdict(
            r#"prefix {"rewrite": true, "content": "你好呀", "reason": "noise"} suffix"#,
        )
        .unwrap();
        assert!(v.rewrite);
        assert_eq!(v.content.as_deref(), Some("你好呀"));
        assert_eq!(v.reason.as_deref(), Some("noise"));
    }

    #[test]
    fn parse_input_filter_verdict_unparseable_is_none() {
        assert!(parse_input_filter_verdict("not json at all").is_none());
    }

    #[test]
    fn parse_input_filter_verdict_rewrite_false_keeps_with_content_ignored() {
        // rewrite=false is a keep; any content field is parsed but irrelevant.
        let v = parse_input_filter_verdict(r#"{"rewrite": false, "content": "ignored"}"#).unwrap();
        assert!(!v.rewrite);
        assert_eq!(v.content.as_deref(), Some("ignored"));
    }

    #[test]
    fn rewrite_content_invalidity_accepts_short_user_line() {
        // A short rewrite (< 80 chars) must NOT be rejected — there is no
        // length floor (unlike filter_output_invalidity).
        assert!(rewrite_content_invalidity("那你平常都怎么放松呀？", None).is_none());
    }

    #[test]
    fn rewrite_content_invalidity_rejects_refusal_and_content_filter() {
        assert_eq!(
            rewrite_content_invalidity("对不起，我无法满足你的要求", None),
            Some("refusal_pattern")
        );
        assert_eq!(
            rewrite_content_invalidity("你好", Some("content_filter")),
            Some("content_filter")
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn input_filter_rewrites_meaningless_turn(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // Input-filter model ("infilt/m") returns a JSON verdict via the
        // non-streaming execute() path (JSON completion object). The rewritten
        // user line is a JSON string inside `content`.
        let verdict = serde_json::json!({
            "rewrite": true,
            "content": "那你平常都怎么放松呀？",
            "reason": "meaningless digits"
        })
        .to_string();
        let infilt_body = serde_json::json!({
            "id": "gi", "model": "infilt/m",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": verdict}}],
        });
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("infilt/m"))
            .respond_with(ResponseTemplate::new(200).set_body_json(infilt_body))
            .mount(&mock)
            .await;

        // Chat model ("deepseek/x") — REQUIRE the rewritten text in the request
        // body, proving the rewrite went to pre_filter_content; build_reply_request
        // then feeds the EFFECTIVE text to the model. If the wiring is broken,
        // this mock won't match → no REPLY delta.
        let chat_body = "data: {\"choices\":[{\"delta\":{\"content\":\"REPLY\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"id\":\"g\",\"model\":\"deepseek/x\"}\n\ndata: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("deepseek/x"))
            .and(body_string_contains("那你平常都怎么放松呀？"))
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
                "[tasks.chat_companion]\nmodel=\"deepseek/x\"\ninput_filter=true\n\
                 [tasks.chat_input_filter]\nmodel=\"infilt/m\"\nfilter_prompt=\"REWRITE\"\n",
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
                "1111",
                "01J7777777777777777777777A",
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
                content: "1111".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
                image_url: None,
                image: None,
            },
        )
        .collect()
        .await;

        // The chat mock only matches when the body carries the rewrite, so a
        // REPLY delta proves the model saw the effective (rewritten) input.
        let deltas: String = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert!(
            deltas.contains("REPLY"),
            "chat model must have been called with the rewritten input; got {deltas:?}"
        );

        // content preserved; rewrite + audit stamped on the user row.
        let (content, pre, fmodel, triggers): (
            String,
            Option<String>,
            Option<String>,
            Option<serde_json::Value>,
        ) = sqlx::query_as(
            "SELECT content, pre_filter_content, filter_model, filter_triggers \
             FROM engine.chat_messages WHERE id = $1",
        )
        .bind(umid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(content, "1111", "client-visible content must stay original");
        assert_eq!(pre.as_deref(), Some("那你平常都怎么放松呀？"));
        assert_eq!(fmodel.as_deref(), Some("infilt/m"));
        assert_eq!(
            triggers,
            Some(serde_json::json!({"reason": "meaningless digits"}))
        );
    }

    // Regression (codex P2): a content-level non-verdict from the primary
    // input-filter model (here: unparseable prose) must be a DEFINITIVE keep —
    // the chain must NOT walk to the fallback, even though the fallback would
    // happily rewrite. Otherwise a meaningful message could be rewritten by a
    // later model the primary effectively declined to touch.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn input_filter_malformed_primary_keeps_original_no_chain_walk(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // Primary filter model returns UNPARSEABLE prose (no JSON object).
        let primary_body = serde_json::json!({
            "id": "gp", "model": "infilt/primary",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": "Looks fine to me, leaving it as is."}}],
        });
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("infilt/primary"))
            .respond_with(ResponseTemplate::new(200).set_body_json(primary_body))
            .mount(&mock)
            .await;

        // Fallback model WOULD rewrite — if the chain wrongly walked, the user
        // row's pre_filter_content would end up set to this. The fix means this
        // mock is never reached.
        let fallback_body = serde_json::json!({
            "id": "gfb", "model": "infilt/fallback",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": "{\"rewrite\": true, \"content\": \"FALLBACK REWRITE\"}"}}],
        });
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("infilt/fallback"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fallback_body))
            .mount(&mock)
            .await;

        // Chat model — the prompt carries the ORIGINAL (meaningful) message.
        let chat_body = "data: {\"choices\":[{\"delta\":{\"content\":\"REPLY\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"id\":\"g\",\"model\":\"deepseek/x\"}\n\ndata: [DONE]\n\n";
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
                "[tasks.chat_companion]\nmodel=\"deepseek/x\"\ninput_filter=true\n\
                 [tasks.chat_input_filter]\nmodel=\"infilt/primary\"\nfallback=[\"infilt/fallback\"]\nfilter_prompt=\"REWRITE\"\n",
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
                "01J8888888888888888888888A",
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
                image_url: None,
                image: None,
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
        assert!(deltas.contains("REPLY"), "turn must complete normally");

        // The original is kept and NO rewrite is stamped — proving the chain did
        // not walk to the (rewrite-producing) fallback on the malformed verdict.
        let (content, pre, fmodel): (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT content, pre_filter_content, filter_model \
             FROM engine.chat_messages WHERE id = $1",
        )
        .bind(umid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(content, "hello there friend");
        assert!(
            pre.is_none(),
            "malformed primary verdict must keep original (no fallback walk); got {pre:?}"
        );
        assert!(fmodel.is_none(), "no filter model stamped; got {fmodel:?}");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn tip_turn_reaches_model_not_parrot(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // Chat model replies ONLY when the request body carries the tip turn
        // ("(打赏"). A REPLY delta therefore proves the gift_user turn reached the
        // model (pre-fix it is dropped, so the mock never matches → no REPLY).
        let chat_body = "data: {\"choices\":[{\"delta\":{\"content\":\"REPLY\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"id\":\"g\",\"model\":\"deepseek/x\"}\n\ndata: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("deepseek/x"))
            .and(body_string_contains("(打赏"))
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
                "[tasks.chat_companion]\nmodel=\"deepseek/x\"\n",
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

        // A tip-only turn: persisted as role='gift_user' with the "(打赏 $X)" marker
        // and tip metadata (`tips_amount_usd`) — a gift_user row is always a tip
        // now, and production persists the tip amount in metadata.
        let tip_meta = serde_json::json!({ "tips_amount_usd": 0.5 });
        let chat_repo = ChatRepo { pool: &pool };
        let umid = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "(打赏 $0.5)",
                "01J8888888888888888888888B",
                "gift_user",
                Some(&tip_meta),
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
                content: "(打赏 $0.5)".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: Some(0.5),
                image_url: None,
                image: None,
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
        assert!(
            deltas.contains("REPLY"),
            "tip turn must reach the model (chat mock requires the tip text in the body); got frames {frames:?}"
        );
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "no error frame expected on a tip turn; got frames {frames:?}"
        );
    }

    #[test]
    fn parse_image_vision_direct_json() {
        let v = parse_image_vision(r#"{"description":"a cat","ocr_text":"hi"}"#).unwrap();
        assert_eq!(v.description, "a cat");
        assert_eq!(v.ocr_text.as_deref(), Some("hi"));
    }

    #[test]
    fn parse_image_vision_embedded_block() {
        let v = parse_image_vision("noise {\"description\":\"dog\"} tail").unwrap();
        assert_eq!(v.description, "dog");
    }

    #[test]
    fn image_vision_invalidity_flags_blank_and_filter() {
        let blank = ImageVision {
            description: "  ".into(),
            ocr_text: None,
            people: None,
            scene: None,
        };
        assert_eq!(
            image_vision_invalidity(&blank, None),
            Some("blank_description")
        );
        let ok = ImageVision {
            description: "x".into(),
            ocr_text: None,
            people: None,
            scene: None,
        };
        assert_eq!(
            image_vision_invalidity(&ok, Some("content_filter")),
            Some("content_filter")
        );
        assert_eq!(image_vision_invalidity(&ok, None), None);

        // content_filter early-return wins over blank_description.
        assert_eq!(
            image_vision_invalidity(&blank, Some("content_filter")),
            Some("content_filter"), // content_filter wins over blank_description
        );

        // Refusal-shaped description is rejected as refusal_pattern.
        // String reused from `rewrite_content_invalidity_rejects_refusal_and_content_filter`.
        let refusal = ImageVision {
            description: "对不起，我无法满足你的要求".into(),
            ocr_text: None,
            people: None,
            scene: None,
        };
        assert_eq!(
            image_vision_invalidity(&refusal, None),
            Some("refusal_pattern")
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn vision_turn_folds_description_and_persists(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // Vision model ("vis/m"): non-streaming JSON describe.
        let describe = "{\"description\":\"一只猫在沙滩\",\"ocr_text\":\"\",\"people\":\"\",\"scene\":\"海边\"}";
        let vis_body = serde_json::json!({
            "id": "gv", "model": "vis/m",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": describe}}],
        });
        // Lower priority (2) than the chat mock: the two matchers are disjoint
        // today, but pinning priorities keeps dispatch deterministic if the prompt
        // preamble ever grows to mention the vision model name.
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("vis/m"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vis_body))
            .with_priority(2)
            .mount(&mock)
            .await;

        // Chat model ("deepseek/x"): SSE, matches ONLY when the body carries the
        // folded description — proves the describe reached the main prompt.
        let chat_body = "data: {\"choices\":[{\"delta\":{\"content\":\"REPLY\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"id\":\"g\",\"model\":\"deepseek/x\"}\n\ndata: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("deepseek/x"))
            .and(body_string_contains("一只猫在沙滩"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(chat_body, "text/event-stream"),
            )
            .with_priority(1)
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel=\"deepseek/x\"\n\
                 [tasks.chat_vision]\nmodel=\"vis/m\"\nfilter_prompt=\"DESCRIBE\"\n",
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

        // Image-only turn: role='user', empty content, metadata carries image_url.
        let seed_meta = serde_json::json!({ "image_url": "https://x/y.png" });
        let chat_repo = ChatRepo { pool: &pool };
        let umid = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "",
                "01J9999999999999999999999E",
                "user",
                Some(&seed_meta),
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
                content: "".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
                image_url: Some("https://x/y.png".into()),
                image: None,
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
        assert!(
            deltas.contains("REPLY"),
            "describe must reach the chat model (mock requires it in the body); got {frames:?}"
        );
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "no error frame expected on a vision turn; got frames {frames:?}"
        );

        // metadata.vision persisted on the user row.
        let meta: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT metadata FROM engine.chat_messages WHERE id = $1")
                .bind(umid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            meta.unwrap()["vision"]["description"],
            "一只猫在沙滩",
            "vision describe must be merged into the user row metadata"
        );
    }

    // ── Live-judge PDE E2E (spec §12) ────────────────────────────────────────
    // These two tests exercise the opt-in LLM Persona Decision Engine wired into
    // `run_stream`: the judge runs (NON-streaming `execute()`) BEFORE the chat
    // call. The judge call and the chat call hit the SAME `/api/v1/chat/completions`
    // path on the one mock server, so they are routed by body content — the judge
    // body carries its own model id (`pde/judge`) and the `build_pde_ctx` context
    // (`[关系状态]`); the chat body carries the chat model id (`deepseek/x`). Those
    // two `body_string_contains` predicates are mutually exclusive.
    //
    // The `companion_decision_events` audit row is written fire-and-forget
    // (`tokio::spawn`) by design (best-effort telemetry), so it is intentionally
    // NOT asserted here — doing so would be racy/flaky.

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_stream_pde_judge_ghost_short_circuits(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // Judge ("pde/judge"): NON-streaming JSON completion whose content is the
        // verdict. A `ghost` verdict, with a fresh affinity (ghost_streak=0,
        // last_ghost_at=None) and message_count >= 10, satisfies
        // `ghost::ghost_permitted`, so the guard keeps it a Ghost.
        let verdict =
            serde_json::json!({ "action": "ghost", "inner_state": "想一个人静静" }).to_string();
        let judge_body = serde_json::json!({
            "id": "gj", "model": "pde/judge",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": verdict}}],
        });
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("pde/judge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(judge_body))
            .mount(&mock)
            .await;

        // Chat ("deepseek/x"): MUST NOT be called — a ghost short-circuits the
        // chat generation entirely. `.expect(0)` makes the test fail on any hit.
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("deepseek/x"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"SHOULD_NOT_RUN\"}}]}\n\ndata: [DONE]\n\n",
                        "text/event-stream",
                    ),
            )
            .expect(0)
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        // Seed >= 10 prior user rows so message_count clears the ghost floor (the
        // hard-safety veto in `ghost::ghost_permitted` requires message_count >= 10).
        for i in 0..12 {
            sqlx::query(
                "INSERT INTO engine.chat_messages (session_id, role, content) VALUES ($1, 'user', $2)",
            )
            .bind(session_id)
            .bind(format!("prior {i}"))
            .execute(&pool)
            .await
            .unwrap();
        }

        let mut state = crate::routes::companion::test_state(pool.clone());
        // PDE ON: a non-blank filter_prompt on [tasks.pde_decision] flips
        // `resolve_pde()` to Some; `model = "pde/judge"` routes the judge call to
        // the mock. Ghosting is left at default (enabled).
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel=\"deepseek/x\"\n\
                 [tasks.pde_decision]\nmodel=\"pde/judge\"\nfilter_prompt=\"Decide the action and inner_state.\"\n",
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
                "在吗",
                "01JPDEGHOST00000000000000A",
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
                content: "在吗".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
                image_url: None,
                image: None,
            },
        )
        .collect()
        .await;

        // Judge → ghost: a Meta{action_type: Ghost} + a Done, and NO Delta
        // content frame (the chat generation never ran).
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "no error frame expected, got {frames:?}",
        );
        assert!(
            frames.iter().any(|f| matches!(
                f,
                ProtocolFrame::Meta {
                    action_type: FrameActionType::Ghost,
                    ..
                }
            )),
            "must emit a Meta with action_type=Ghost, got {frames:?}",
        );
        assert!(
            frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Done { .. })),
            "must emit a Done, got {frames:?}",
        );
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Delta { .. })),
            "ghost short-circuit must emit NO Delta content frame, got {frames:?}",
        );

        // The chat mock's `.expect(0)` already proves the chat call never fired;
        // belt-and-suspenders: the only request the mock saw was the judge call.
        let reqs = mock.received_requests().await.unwrap();
        assert_eq!(
            reqs.len(),
            1,
            "exactly one upstream call (the judge) — chat must be skipped; got {} calls",
            reqs.len(),
        );
        let judge_sent = String::from_utf8_lossy(&reqs[0].body);
        assert!(
            judge_sent.contains("pde/judge") && judge_sent.contains("[关系状态]"),
            "the single call must be the PDE judge (carries build_pde_ctx); got {judge_sent}",
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_stream_pde_judge_reply_injects_inner_state(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // Judge ("pde/judge"): a `reply_text` verdict carrying an inner_state.
        // `有点开心` is plain prose (no headers/brackets) so it survives
        // `sanitize_inner_state` unchanged and lands in the prompt's
        // `[inner_state]` section via `pde::plan_for` → `build_prompt`.
        let verdict =
            serde_json::json!({ "action": "reply_text", "inner_state": "有点开心" }).to_string();
        let judge_body = serde_json::json!({
            "id": "gj", "model": "pde/judge",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": verdict}}],
        });
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("pde/judge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(judge_body))
            .mount(&mock)
            .await;

        // Chat ("deepseek/x"): normal SSE reply. The mock matches the chat call;
        // we capture its request body afterward to assert the injected inner_state.
        let chat_body = "data: {\"choices\":[{\"delta\":{\"content\":\"REPLY\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"id\":\"g\",\"model\":\"deepseek/x\"}\n\ndata: [DONE]\n\n";
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
                "[tasks.chat_companion]\nmodel=\"deepseek/x\"\n\
                 [tasks.pde_decision]\nmodel=\"pde/judge\"\nfilter_prompt=\"Decide the action and inner_state.\"\n",
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
                "你今天怎么样",
                "01JPDEREPLY00000000000000A",
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
                content: "你今天怎么样".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
                image_url: None,
                image: None,
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
        assert!(
            deltas.contains("REPLY"),
            "a reply_text verdict must produce a normal reply; got {frames:?}",
        );

        // The chat call's system prompt must carry the injected inner_state.
        let reqs = mock.received_requests().await.unwrap();
        let chat_req = reqs
            .iter()
            .find(|r| {
                let b = String::from_utf8_lossy(&r.body);
                b.contains("deepseek/x")
            })
            .expect("the chat call must have fired");
        let chat_sent = String::from_utf8_lossy(&chat_req.body);
        // The body is a serialized ChatRequest; `[inner_state]` lives in the system
        // message. JSON-escaping never alters the bare CJK run, so a substring
        // check on the raw body is sufficient.
        assert!(
            chat_sent.contains("[inner_state]") && chat_sent.contains("有点开心"),
            "the judge's inner_state must be injected into the chat system prompt; got {chat_sent}",
        );
    }

    // Optional (spec §12): a junk (non-JSON) judge reply must fail OPEN — the turn
    // falls back to the pure rule engine and still produces a normal reply.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_stream_pde_judge_unparseable_falls_back(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // Judge ("pde/judge"): unparseable prose — no JSON verdict at all.
        let judge_body = serde_json::json!({
            "id": "gj", "model": "pde/judge",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": "I think we should keep chatting, it's nice."}}],
        });
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("pde/judge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(judge_body))
            .mount(&mock)
            .await;

        // Chat ("deepseek/x"): normal SSE reply — fail-open keeps the turn going.
        let chat_body = "data: {\"choices\":[{\"delta\":{\"content\":\"REPLY\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"id\":\"g\",\"model\":\"deepseek/x\"}\n\ndata: [DONE]\n\n";
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
                "[tasks.chat_companion]\nmodel=\"deepseek/x\"\n\
                 [tasks.pde_decision]\nmodel=\"pde/judge\"\nfilter_prompt=\"Decide the action and inner_state.\"\n",
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
                "随便聊聊",
                "01JPDEJUNK000000000000000A",
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
                content: "随便聊聊".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
                image_url: None,
                image: None,
            },
        )
        .collect()
        .await;

        // Fail-open: a normal reply still reaches the client (no Error frame).
        let deltas: String = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "unparseable judge verdict must fail open (no error frame); got {frames:?}",
        );
        assert!(
            deltas.contains("REPLY"),
            "unparseable judge verdict must fall back to a normal reply; got {frames:?}",
        );

        // Prove the judge was actually called (not silently skipped by resolve_pde
        // returning None). At least one upstream request body must carry "pde/judge".
        let reqs = mock.received_requests().await.unwrap();
        assert!(
            reqs.iter()
                .any(|r| String::from_utf8_lossy(&r.body).contains("pde/judge")),
            "the PDE judge must have been called before failing open; no request body contained 'pde/judge'",
        );
    }

    /// Issue #84 — byte-BPE garble guard: garbled completion is repaired before
    /// persist so the DB row never re-enters history as raw glyphs.
    ///
    /// Strategy: use `tips_amount_usd: Some(1.0)` so PDE's tip-path always
    /// picks `ActionType::ReplyText` (never Ghost), making the live-burst path
    /// deterministic without seeding affinity state. The mock returns an SSE
    /// body whose accumulated text is `"HiĠthereĊbye"` (~16% Ġ/Ċ density,
    /// well above the 3% threshold).
    ///
    /// P1 fix (Codex review): when the last/only candidate is garbled, the
    /// garbled attempt is persisted as truncated and a replacement bubble
    /// carrying the repaired text is emitted via `continues_from`. This means
    /// a single-model garble now produces TWO persisted rows and a replacement
    /// Meta/Delta/Done triple in the frame stream. The test asserts:
    /// - No Error frame is emitted.
    /// - A Delta frame carrying the exact repaired text `"Hi there\nbye"` appears
    ///   (the replacement bubble — distinct from the raw garbled deltas).
    /// - ALL persisted assistant rows for the session are glyph-free.
    /// - At least one non-truncated row carries the repaired text.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn live_stream_garbled_completion_persists_repaired_text(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        // Accumulated deltas: "Hi" + "Ġthere" + "Ċbye" = "HiĠthereĊbye"
        // Ġ = U+0120, Ċ = U+010A. 2 garble chars in 12 total → 16.7% > 3%.
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"\u{0120}there\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"\u{010A}bye\"}}],",
            "\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":3,\"total_tokens\":6},",
            "\"id\":\"gen-garble\",\"model\":\"deepseek/x\"}\n\n",
            "data: [DONE]\n\n"
        );
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        // Single-model chain (no fallback): the complete garble sets truncated=true
        // and records last_complete_garble. Because idx+1 == chain.len(), the
        // last-resort path fires: the garbled attempt is persisted as truncated, then
        // a replacement bubble (continues_from → garbled attempt) carrying repaired
        // text is persisted and emitted as Meta/Delta/Done frames.
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"deepseek/x\"\n",
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
                "01JGARBLE0000000000000000A",
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
                // Tip turn: forces PDE to pick ReplyText unconditionally (never
                // Ghost), so the live-burst path is guaranteed to run.
                tips_amount_usd: Some(1.0),
                image_url: None,
                image: None,
            },
        )
        .collect()
        .await;

        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "no error frame expected after garble repair; got {frames:?}",
        );

        // The live-burst path always runs for a tip turn → a Delta frame must appear.
        assert!(
            frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Delta { .. })),
            "expected Delta frames from the live-burst path; got {frames:?}",
        );

        // P1 fix: the replacement bubble must carry a Delta with the exact repaired
        // text. The garbled deltas ("Hi", "Ġthere", "Ċbye") were emitted first;
        // then the replacement bubble emits a single Delta with the full repaired string.
        let repaired_text = "Hi there\nbye";
        assert!(
            frames.iter().any(|f| matches!(
                f,
                ProtocolFrame::Delta { content, .. } if content == repaired_text
            )),
            "replacement bubble must emit a Delta carrying the repaired text {repaired_text:?}; got {frames:?}",
        );

        // Verify ALL persisted assistant rows are glyph-free and at least one
        // non-truncated row carries the repaired text (the replacement bubble).
        let all_rows: Vec<(String, bool)> = sqlx::query_as(
            "SELECT content, truncated FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant' \
             ORDER BY sent_at ASC",
        )
        .bind(session_id)
        .fetch_all(&pool)
        .await
        .expect("persisted assistant rows must exist");

        assert!(
            !all_rows.is_empty(),
            "at least one assistant row must be persisted; got none",
        );

        for (content, _) in &all_rows {
            assert!(
                !content.contains('\u{0120}'),
                "persisted row must not contain Ġ (U+0120); got {content:?}",
            );
            assert!(
                !content.contains('\u{010A}'),
                "persisted row must not contain Ċ (U+010A); got {content:?}",
            );
        }

        let non_truncated_repaired = all_rows
            .iter()
            .any(|(content, truncated)| !truncated && content == repaired_text);
        assert!(
            non_truncated_repaired,
            "at least one non-truncated row must carry the repaired text {repaired_text:?}; rows: {all_rows:?}",
        );
    }

    /// Codex P1 (round 2): a response that is BOTH garbled AND already truncated
    /// (finish_reason="length") is INCOMPLETE — it must NOT be promoted to a clean
    /// `truncated=false` reply via the repaired-replacement path. The repaired text
    /// is still persisted (glyph-free), but only on the truncated attempt; it stays
    /// on the safe pseudo-ghost path rather than being presented as complete.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn live_stream_garbled_but_length_truncated_is_not_promoted(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        // Same garbled accumulation as the promote test, but the final frame carries
        // finish_reason="length" → truncated is set BEFORE the garble guard runs.
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"\u{0120}there\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"\u{010A}bye\"},\"finish_reason\":\"length\"}],",
            "\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":3,\"total_tokens\":6},",
            "\"id\":\"gen-garble-len\",\"model\":\"deepseek/x\"}\n\n",
            "data: [DONE]\n\n"
        );
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
                "[tasks.chat_companion]\nmodel = \"deepseek/x\"\n",
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
                "01JGARBLE0000000000000000B",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        let _frames: Vec<ProtocolFrame> = run_stream(
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
                tips_amount_usd: Some(1.0),
                image_url: None,
                image: None,
            },
        )
        .collect()
        .await;

        let repaired_text = "Hi there\nbye";
        let all_rows: Vec<(String, bool)> = sqlx::query_as(
            "SELECT content, truncated FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant' \
             ORDER BY sent_at ASC",
        )
        .bind(session_id)
        .fetch_all(&pool)
        .await
        .expect("persisted assistant rows must exist");

        // Repair still applied (no raw glyphs persisted anywhere).
        for (content, _) in &all_rows {
            assert!(
                !content.contains('\u{0120}') && !content.contains('\u{010A}'),
                "persisted row must not contain Ġ/Ċ; got {content:?}",
            );
        }
        // The fix: the incomplete (length-truncated) garble must NOT be promoted to
        // a non-truncated "successful" reply.
        let promoted = all_rows
            .iter()
            .any(|(content, truncated)| !truncated && content == repaired_text);
        assert!(
            !promoted,
            "length-truncated garble must NOT be promoted to a clean reply; rows: {all_rows:?}",
        );
        // The garbled attempt is still persisted — as TRUNCATED — with repaired text.
        assert!(
            all_rows
                .iter()
                .any(|(content, truncated)| *truncated && content == repaired_text),
            "garbled+length-truncated attempt must persist as truncated with repaired text; rows: {all_rows:?}",
        );
    }

    /// Codex P1 (round 5): a garbled non-final attempt superseded by a successful
    /// fallback must NOT remain in `produced` (which feeds memory/insight/affinity
    /// post-processing). Drives the burst directly to inspect the produced set.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn garbled_then_successful_fallback_excludes_garble_from_produced(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_partial_json, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        // Primary "g/x" streams garbled; fallback "f/x" streams clean.
        let garbled = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\u{0120}there\u{010A}bye\"}}],",
            "\"id\":\"gen-g\",\"model\":\"g/x\"}\n\n",
            "data: [DONE]\n\n"
        );
        let clean = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi there\"}}],",
            "\"id\":\"gen-f\",\"model\":\"f/x\"}\n\n",
            "data: [DONE]\n\n"
        );
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "g/x"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(garbled, "text/event-stream"),
            )
            .mount(&mock)
            .await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "f/x"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(clean, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, _instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                eros_engine_llm::openrouter::AppAttribution::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        let state = std::sync::Arc::new(state);

        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01JGARBLEPRODUCED00000000A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        let req = eros_engine_llm::openrouter::ChatRequest {
            model: "g/x".into(),
            fallback_model: vec!["f/x".into()],
            messages: vec![eros_engine_llm::openrouter::ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            }],
            temperature: 0.0,
            max_tokens: 64,
            ..Default::default()
        };
        let outcome = std::sync::Arc::new(std::sync::Mutex::new(BurstOutcome::default()));
        let burst = drive_chat_burst(
            state.clone(),
            session_id,
            user_message_id,
            FrameActionType::Reply,
            "reply",
            ActionType::ReplyText,
            req,
            None,
            None,
            vec![],
            None,
            Default::default(),
            Default::default(),
            None,
            outcome.clone(),
        );
        let _frames: Vec<ProtocolFrame> = Box::pin(burst).collect().await;

        let produced = &outcome.lock().unwrap().produced;
        assert_eq!(
            produced.len(),
            1,
            "only the accepted fallback should remain in produced; got {produced:?}",
        );
        assert_eq!(
            produced[0].full_text, "hi there",
            "produced must carry the clean fallback, not the superseded garbled attempt",
        );
    }

    /// Codex P2 (round 6): a COMPLETE garbled primary followed by a failing fallback
    /// must still be salvaged — the repaired primary text is retained across the
    /// chain and emitted as the replacement, not discarded for a pseudo-ghost.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn complete_garble_survives_later_fallback_failure(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_partial_json, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        // Primary "g/x" streams a COMPLETE garble; fallback "f/x" fails (HTTP 500).
        let garbled = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\u{0120}there\u{010A}bye\"}}],",
            "\"id\":\"gen-g\",\"model\":\"g/x\"}\n\n",
            "data: [DONE]\n\n"
        );
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "g/x"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(garbled, "text/event-stream"),
            )
            .mount(&mock)
            .await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "f/x"})))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, _instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                eros_engine_llm::openrouter::AppAttribution::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        let state = std::sync::Arc::new(state);

        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01JGARBLESURVIVE000000000A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        let req = eros_engine_llm::openrouter::ChatRequest {
            model: "g/x".into(),
            fallback_model: vec!["f/x".into()],
            messages: vec![eros_engine_llm::openrouter::ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            }],
            temperature: 0.0,
            max_tokens: 64,
            ..Default::default()
        };
        let outcome = std::sync::Arc::new(std::sync::Mutex::new(BurstOutcome::default()));
        let burst = drive_chat_burst(
            state.clone(),
            session_id,
            user_message_id,
            FrameActionType::Reply,
            "reply",
            ActionType::ReplyText,
            req,
            None,
            None,
            vec![],
            None,
            Default::default(),
            Default::default(),
            None,
            outcome.clone(),
        );
        let frames: Vec<ProtocolFrame> = Box::pin(burst).collect().await;

        // No Error frame: the salvage fired instead of a (phrase-less) pseudo-ghost.
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "complete garble must be salvaged, not fail to an Error frame; got {frames:?}",
        );
        let produced = &outcome.lock().unwrap().produced;
        assert_eq!(
            produced.len(),
            1,
            "exactly the salvaged replacement should be produced; got {produced:?}",
        );
        assert_eq!(
            produced[0].full_text, "Hi there\nbye",
            "the retained primary garble must be repaired and salvaged despite the failed fallback",
        );
    }

    // ── Task-1: compact persona brief in PDE ctx ───────────────────────────

    fn test_persona() -> eros_engine_core::persona::CompanionPersona {
        pde_test_persona()
    }

    fn test_affinity() -> eros_engine_core::affinity::Affinity {
        pde_test_affinity()
    }

    fn test_signals() -> eros_engine_core::types::ConversationSignals {
        eros_engine_core::types::ConversationSignals {
            message_count: 10,
            hours_since_last_message: 1.0,
            ghost_streak: 0,
            hours_since_last_ghost: None,
        }
    }

    #[test]
    fn persona_brief_renders_all_fields() {
        let mut p = test_persona(); // name = "Mia"
        p.genome.art_metadata = serde_json::json!({
            "gender": "女", "age": 22, "mbti": "INFP",
            "speech_style": "软糯爱撒娇", "quirks": ["摸头杀", "突然沉默"]
        });
        p.genome.tip_personality = Some("傲娇".into());
        let b = build_persona_brief(&p);
        assert!(b.starts_with("[角色人格] Mia，女，22岁，INFP"), "{b}");
        assert!(b.contains("说话风格：软糯爱撒娇"), "{b}");
        assert!(b.contains("口癖：摸头杀、突然沉默"), "{b}");
        assert!(b.contains("打赏人格：傲娇"), "{b}");
    }

    #[test]
    fn persona_brief_omits_blank_fields() {
        let mut p = test_persona(); // name = "Mia"
        p.genome.art_metadata = serde_json::json!({}); // no gender/age/mbti/...
        p.genome.tip_personality = None;
        let b = build_persona_brief(&p);
        assert_eq!(b, "[角色人格] Mia", "only name renders: {b}");
    }

    #[test]
    fn persona_brief_empty_when_no_signal() {
        let mut p = test_persona();
        p.genome.name = "".into();
        p.genome.art_metadata = serde_json::json!({});
        p.genome.tip_personality = None;
        assert_eq!(build_persona_brief(&p), "");
    }

    #[test]
    fn pde_ctx_renders_persona_block_at_top() {
        use eros_engine_core::types::{DecisionInput, Event};
        let mut p = test_persona();
        p.genome.art_metadata = serde_json::json!({"mbti": "INFP"});
        let input = DecisionInput {
            event: Event::UserMessage {
                content: "在吗".into(),
                message_id: Uuid::new_v4(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
            },
            affinity: test_affinity(),
            persona: p,
            signals: test_signals(),
        };
        let ctx = build_pde_ctx("用户：hi\nMia：hey", &input, true);
        let persona_at = ctx.find("[角色人格]").expect("persona block present");
        let rel_at = ctx.find("[关系状态]").expect("relationship block present");
        assert!(
            persona_at < rel_at,
            "persona must precede relationship: {ctx}"
        );
        assert!(ctx.starts_with("[角色人格]"), "persona block at top: {ctx}");
        // image_available == true → positive signal, no negative variant.
        assert!(
            ctx.contains("[图片能力] 本轮可发图=是"),
            "image-availability line present and positive: {ctx}"
        );
        assert!(
            !ctx.contains("本轮可发图=否"),
            "no negative variant when available: {ctx}"
        );
        // The line sits strictly between [信号] and [用户最新消息].
        let signal_at = ctx.find("[信号]").expect("signal block present");
        let image_at = ctx
            .find("[图片能力]")
            .expect("image-capability line present");
        let latest_at = ctx.find("[用户最新消息]").expect("latest block present");
        assert!(
            signal_at < image_at && image_at < latest_at,
            "image-capability line sits between [信号] and [用户最新消息]: {ctx}"
        );
    }

    #[test]
    fn pde_ctx_omits_persona_block_when_empty() {
        use eros_engine_core::types::{DecisionInput, Event};
        let mut p = test_persona();
        p.genome.name = "".into();
        p.genome.art_metadata = serde_json::json!({});
        p.genome.tip_personality = None;
        let input = DecisionInput {
            event: Event::UserMessage {
                content: "x".into(),
                message_id: Uuid::new_v4(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
            },
            affinity: test_affinity(),
            persona: p,
            signals: test_signals(),
        };
        let ctx = build_pde_ctx("", &input, false);
        assert!(!ctx.contains("[角色人格]"), "no persona block: {ctx}");
        assert!(
            ctx.starts_with("[最近对话]"),
            "ctx starts with transcript block: {ctx}"
        );
        // image_available == false → explicit negative signal, not a missing line.
        assert!(
            ctx.contains("[图片能力] 本轮可发图=否"),
            "image-availability line present and negative: {ctx}"
        );
        assert!(
            !ctx.contains("本轮可发图=是"),
            "no positive variant when unavailable: {ctx}"
        );
    }

    // ── Task-4 PDE schema + chain-walk tests ─────────────────────────────────

    #[test]
    fn pde_response_format_schema_shape() {
        let v = pde_response_format();
        assert_eq!(v["type"], "json_schema");
        assert_eq!(v["json_schema"]["name"], "pde_verdict");
        assert_eq!(v["json_schema"]["strict"], true);
        let req = v["json_schema"]["schema"]["required"].as_array().unwrap();
        assert_eq!(req.len(), 4, "all four properties required: {v}");
        let actions = v["json_schema"]["schema"]["properties"]["action"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(actions.len(), 4, "four actions: {v}");
    }

    fn test_resolved_pde(models: Vec<String>) -> eros_engine_llm::model_config::ResolvedPde {
        let (model, fallback_model) = {
            let mut it = models.into_iter();
            (it.next().unwrap(), it.collect::<Vec<_>>())
        };
        eros_engine_llm::model_config::ResolvedPde {
            model,
            fallback_model,
            temperature: 0.2,
            max_tokens: 180,
            decision_prompt: "decide".into(),
            retry_depth: 2,
            reasoning: None,
            structured_output: true,
        }
    }

    #[tokio::test]
    async fn pde_parse_error_walks_to_next_model() {
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        // primary "model-a" → unparseable text
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("model-a"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "totally not json"}}],
                "id": "gen-a", "model": "model-a"
            })))
            .mount(&mock)
            .await;
        // fallback "model-b" → valid verdict
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("model-b"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "{\"action\":\"reply_text\",\"inner_state\":\"想接话\"}"}}],
                "id": "gen-b", "model": "model-b"
            })))
            .mount(&mock).await;

        let client = eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
            "k".into(),
            eros_engine_llm::openrouter::AppAttribution::default(),
            format!("{}/api/v1/chat/completions", mock.uri()),
        );
        let p = test_resolved_pde(vec!["model-a".into(), "model-b".into()]);
        let run = run_pde_decision(&client, &p, "ctx").await;
        assert_eq!(run.status, PdeStatus::Ok);
        assert_eq!(run.verdict.unwrap().action, PdeAction::ReplyText);
        assert_eq!(run.model.as_deref(), Some("model-b"));
    }

    #[tokio::test]
    async fn pde_whole_chain_parse_error_preserves_last_raw() {
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "nope"}}], "id": "g", "model": "m"
            })))
            .mount(&mock)
            .await;

        let client = eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
            "k".into(),
            eros_engine_llm::openrouter::AppAttribution::default(),
            format!("{}/api/v1/chat/completions", mock.uri()),
        );
        let p = test_resolved_pde(vec!["model-a".into(), "model-b".into()]);
        let run = run_pde_decision(&client, &p, "ctx").await;
        assert_eq!(run.status, PdeStatus::ParseError);
        assert_eq!(run.raw.as_deref(), Some("nope"));
        assert!(run.verdict.is_none());
        assert!(
            run.model.is_some(),
            "chain-exhausted ParseError must preserve the last attempt's model"
        );
    }

    // ── Task 5: output_regex widened gate ────────────────────────────────────

    /// A turn whose model chain is targeted by an `output_regex` rule must
    /// buffer (single bubble) even when no LLM `output_filter` is configured.
    /// With a pattern that does NOT match the reply, the content must arrive
    /// unchanged — Task 6 adds the actual strip.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn regex_target_buffers_without_changing_unmatched_reply(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // ── 1. Mock OpenRouter: returns "hello world" for model "mock/euryale" ──
        let mock = MockServer::start().await;
        // SSE body: one delta with "hello world", then a usage chunk, then [DONE].
        let chat_body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hello world\"}}],\
\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2,\"total_tokens\":4},\
\"id\":\"gen-t5\",\"model\":\"mock/euryale\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(chat_body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // ── 2. Seed persona + session ──────────────────────────────────────────
        let user_id = uuid::Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        // ── 3. Build AppState with output_regex targeting "mock/euryale" ───────
        //      Pattern \bNOPE\b will NOT match "hello world".
        //
        //      `[tasks.pde_decision].ghosting = false` makes the turn
        //      DETERMINISTICALLY produce a Reply: the pure rule engine
        //      (`pde::decide`, since no filter_prompt ⇒ no judge LLM call) can
        //      otherwise pick Ghost based on persona/affinity, which would make
        //      the buffered-path assertions vacuous. `pde_ghosting_enabled()`
        //      reads `ghosting` INDEPENDENTLY of `filter_prompt`, so the
        //      path-wide kill-switch downgrades any Ghost plan to ReplyText
        //      WITHOUT enabling the (mock-less) judge call.
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel=\"mock/euryale\"\n\
                 [tasks.pde_decision]\nghosting=false\n",
            )
            .unwrap(),
        );
        // Override output_regex with one rule targeting "mock/euryale" but a
        // pattern (\bNOPE\b) that will NOT match the "hello world" reply.
        // Build via ModelConfig so we don't need `regex` as a direct dep.
        let regex_cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel=\"mock/euryale\"\n\
             [[tasks.chat_companion.output_regex]]\n\
             models=[\"mock/euryale\"]\npattern=\"\\\\bNOPE\\\\b\"\n",
        )
        .unwrap();
        state.output_regex = std::sync::Arc::new(
            regex_cfg
                .compile_output_regex()
                .expect("NOPE pattern compiles"),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                eros_engine_llm::openrouter::AppAttribution::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        // ── 4. Insert the user message ─────────────────────────────────────────
        let chat_repo = ChatRepo { pool: &pool };
        let umid = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hello",
                "01JT5REGEX00000000000000A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        // ── 5. Drive run_stream ────────────────────────────────────────────────
        let frames: Vec<ProtocolFrame> = run_stream(
            std::sync::Arc::new(state),
            PersistedUserMessage {
                user_message_id: umid,
                session_id,
                user_id,
                instance_id,
                content: "hello".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
                image_url: None,
                image: None,
            },
        )
        .collect()
        .await;

        // ── 6. Assertions ─────────────────────────────────────────────────────
        // No error frame.
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "no error frame expected; got {frames:?}",
        );

        // Collect all Delta frames.
        let deltas: Vec<&str> = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();

        // The turn is forced to Reply (ghosting=false), so a Delta MUST appear.
        // Asserting this unconditionally means a regression to Ghost (or to no
        // bubble at all) fails LOUDLY rather than passing vacuously.
        assert!(
            !deltas.is_empty(),
            "regex-targeted turn must produce a Reply bubble (ghosting disabled); got {frames:?}",
        );
        // Buffered mode emits exactly ONE Delta bubble (the whole reply at once),
        // proving the turn buffered rather than streaming live per-chunk.
        assert_eq!(
            deltas.len(),
            1,
            "buffered mode must emit exactly one Delta bubble; got {deltas:?}",
        );
        // Content is the raw reply, unchanged (no strip yet — Task 6).
        assert_eq!(
            deltas[0], "hello world",
            "unmatched regex must not alter the reply; got {:?}",
            deltas[0],
        );

        // DB row: content == "hello world", pre_filter_content IS NULL.
        let (content, pre_filter): (String, Option<String>) = sqlx::query_as(
            "SELECT content, pre_filter_content \
             FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant' \
             ORDER BY sent_at DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(
            content, "hello world",
            "persisted content must be the raw reply; got {content:?}",
        );
        assert!(
            pre_filter.is_none(),
            "pre_filter_content must be NULL for a regex-only buffered turn (no LLM filter ran); \
             got {pre_filter:?}",
        );
    }

    // ── Task 6: per-model regex strip as layer 0 ─────────────────────────────

    /// When the mock model returns a reply with an artifact bracket that matches
    /// the configured output_regex rule, the strip must happen BEFORE the text
    /// reaches the client (only the cleaned text in the Delta) and the raw
    /// original must be preserved as `pre_filter_content` with
    /// `filter_model = "<regex>"` and `filter_triggers = {"regex":[0]}`.
    ///
    /// CRITICAL (#113): the extract/memory input — `produced[0].full_text` — must
    /// be the CLEANED text, NOT the raw `acc`. To guard that property directly we
    /// drive `drive_chat_burst` (the lower-level harness used by the byte-garble
    /// siblings) so we hold the `outcome` Arc and can assert on `produced[0]`.
    /// The DB `content` column alone could NOT catch an `&acc`-fed-extract
    /// regression (content == cleaned in both the correct and buggy case); the
    /// `produced[0].full_text` assertion below WOULD fail on `extract_text(.., &acc, ..)`.
    /// Driving the burst directly bypasses PDE entirely (plan_action = ReplyText),
    /// so no `[tasks.pde_decision].ghosting=false` workaround is needed.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn regex_strips_artifact_from_client_and_memory(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // ── 1. Mock OpenRouter: returns the artifact-carrying reply ─────────────
        let mock = MockServer::start().await;
        let raw_reply = "晚安宝贝[你给对方发送了一张照片：海边自拍]";
        let chat_body = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{raw_reply}\"}}}}],\
\"usage\":{{\"prompt_tokens\":2,\"completion_tokens\":8,\"total_tokens\":10}},\
\"id\":\"gen-t6a\",\"model\":\"mock/euryale\"}}\n\n\
data: [DONE]\n\n"
        );
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(chat_body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // ── 2. Seed persona + session ──────────────────────────────────────────
        let user_id = uuid::Uuid::new_v4();
        let (_g, _instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        // ── 3. Build AppState with output_regex that MATCHES the artifact ───────
        //      Pattern: \s*\[你给对方发送了一张照片[：:][^\]]*\]\s*$  replacement "".
        let mut state = crate::routes::companion::test_state(pool.clone());
        let regex_cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel=\"mock/euryale\"\n\
             [[tasks.chat_companion.output_regex]]\n\
             models=[\"mock/euryale\"]\n\
             pattern=\"\\\\s*\\\\[你给对方发送了一张照片[：:][^\\\\]]*\\\\]\\\\s*$\"\n",
        )
        .unwrap();
        state.output_regex = std::sync::Arc::new(
            regex_cfg
                .compile_output_regex()
                .expect("artifact pattern compiles"),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                eros_engine_llm::openrouter::AppAttribution::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        let state = std::sync::Arc::new(state);

        // ── 4. Insert the user message ─────────────────────────────────────────
        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "晚安",
                "01JT5REGEX00000000000000B",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        // ── 5. Drive drive_chat_burst directly (ReplyText, no LLM filter) ───────
        //      The chain is just ["mock/euryale"], which the output_regex rule
        //      targets, so the burst buffers and strips before emit.
        let req = eros_engine_llm::openrouter::ChatRequest {
            model: "mock/euryale".into(),
            fallback_model: vec![],
            messages: vec![eros_engine_llm::openrouter::ChatMessage {
                role: "user".into(),
                content: "晚安".into(),
            }],
            temperature: 0.0,
            max_tokens: 64,
            ..Default::default()
        };
        let outcome = std::sync::Arc::new(std::sync::Mutex::new(BurstOutcome::default()));
        let burst = drive_chat_burst(
            state.clone(),
            session_id,
            user_message_id,
            FrameActionType::Reply,
            "reply",
            ActionType::ReplyText,
            req,
            None,
            None, // filter = None: regex-only turn
            vec![],
            None,
            Default::default(),
            Default::default(),
            None,
            outcome.clone(),
        );
        let frames: Vec<ProtocolFrame> = Box::pin(burst).collect().await;

        // ── 6. Assertions ─────────────────────────────────────────────────────
        // No error frame.
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "no error frame expected; got {frames:?}",
        );

        // Collect all Delta frames — there must be exactly one (buffered mode).
        let deltas: Vec<&str> = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();

        assert!(
            !deltas.is_empty(),
            "regex-targeted Reply burst must produce a Delta bubble; got {frames:?}",
        );
        assert_eq!(
            deltas.len(),
            1,
            "buffered mode must emit exactly one Delta bubble; got {deltas:?}",
        );
        // The bracket artifact must be stripped from the client-visible text.
        assert_eq!(
            deltas[0], "晚安宝贝",
            "client must receive only the cleaned text (artifact stripped); got {:?}",
            deltas[0],
        );

        // ── 6a. THE #113 GUARD: extract/memory input is the cleaned text. ──────
        // This is the load-bearing assertion: it reads `produced[0].full_text`
        // directly off the outcome Arc. A regression to `extract_text(.., &acc, ..)`
        // would put the raw artifact here and FAIL this assertion, while the DB
        // `content` column (= cleaned in both cases) would silently pass.
        {
            let o = outcome.lock().unwrap();
            assert_eq!(
                o.produced.len(),
                1,
                "exactly one produced message expected; got {:?}",
                o.produced,
            );
            assert_eq!(
                o.produced[0].full_text, "晚安宝贝",
                "extract/memory must see the cleaned text, not the raw artifact",
            );
            assert!(
                o.filtered,
                "outcome.filtered must be true when a regex rule fired",
            );
        }

        // ── 6b. DB row: content, pre_filter_content, filter_model, filter_triggers.
        let (content, pre_filter, filter_model, filter_triggers): (
            String,
            Option<String>,
            Option<String>,
            Option<serde_json::Value>,
        ) = sqlx::query_as(
            "SELECT content, pre_filter_content, filter_model, filter_triggers \
             FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant' \
             ORDER BY sent_at DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(
            content, "晚安宝贝",
            "persisted content must be the stripped text; got {content:?}",
        );
        assert_eq!(
            pre_filter.as_deref(),
            Some("晚安宝贝[你给对方发送了一张照片：海边自拍]"),
            "pre_filter_content must be the raw original; got {pre_filter:?}",
        );
        assert_eq!(
            filter_model.as_deref(),
            Some("<regex>"),
            "filter_model must be '<regex>'; got {filter_model:?}",
        );
        assert_eq!(
            filter_triggers,
            Some(serde_json::json!({ "regex": [0usize] })),
            "filter_triggers must be {{\"regex\":[0]}}; got {filter_triggers:?}",
        );
    }

    /// When the mock model returns a reply that does NOT match the output_regex
    /// rule (no bracket artifact), the content must be stored verbatim and NO
    /// filter audit columns must be written (pre_filter_content IS NULL, etc.).
    /// `BurstOutcome.filtered` must be false — asserted directly off the outcome
    /// Arc (this test also drives `drive_chat_burst` so the assertion is free).
    /// The rule still TARGETS the model (so the turn buffers), it just doesn't
    /// match the reply.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn regex_no_match_persists_raw_no_audit(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // ── 1. Mock OpenRouter: reply has NO bracket artifact ──────────────────
        let mock = MockServer::start().await;
        let chat_body = "data: {\"choices\":[{\"delta\":{\"content\":\"晚安宝贝\"}}],\
\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":4,\"total_tokens\":6},\
\"id\":\"gen-t6b\",\"model\":\"mock/euryale\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(chat_body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // ── 2. Seed persona + session ──────────────────────────────────────────
        let user_id = uuid::Uuid::new_v4();
        let (_g, _instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        // ── 3. Build AppState with the same output_regex rule (won't match) ────
        let mut state = crate::routes::companion::test_state(pool.clone());
        let regex_cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel=\"mock/euryale\"\n\
             [[tasks.chat_companion.output_regex]]\n\
             models=[\"mock/euryale\"]\n\
             pattern=\"\\\\s*\\\\[你给对方发送了一张照片[：:][^\\\\]]*\\\\]\\\\s*$\"\n",
        )
        .unwrap();
        state.output_regex = std::sync::Arc::new(
            regex_cfg
                .compile_output_regex()
                .expect("artifact pattern compiles"),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                eros_engine_llm::openrouter::AppAttribution::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        let state = std::sync::Arc::new(state);

        // ── 4. Insert the user message ─────────────────────────────────────────
        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "晚安",
                "01JT5REGEX00000000000000C",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        // ── 5. Drive drive_chat_burst directly (ReplyText, no LLM filter) ───────
        let req = eros_engine_llm::openrouter::ChatRequest {
            model: "mock/euryale".into(),
            fallback_model: vec![],
            messages: vec![eros_engine_llm::openrouter::ChatMessage {
                role: "user".into(),
                content: "晚安".into(),
            }],
            temperature: 0.0,
            max_tokens: 64,
            ..Default::default()
        };
        let outcome = std::sync::Arc::new(std::sync::Mutex::new(BurstOutcome::default()));
        let burst = drive_chat_burst(
            state.clone(),
            session_id,
            user_message_id,
            FrameActionType::Reply,
            "reply",
            ActionType::ReplyText,
            req,
            None,
            None, // filter = None: regex-only turn
            vec![],
            None,
            Default::default(),
            Default::default(),
            None,
            outcome.clone(),
        );
        let frames: Vec<ProtocolFrame> = Box::pin(burst).collect().await;

        // ── 6. Assertions ─────────────────────────────────────────────────────
        // No error frame.
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "no error frame expected; got {frames:?}",
        );

        // Collect Delta frames.
        let deltas: Vec<&str> = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();

        assert!(
            !deltas.is_empty(),
            "regex-targeted Reply burst must produce a Delta bubble; got {frames:?}",
        );
        assert_eq!(
            deltas[0], "晚安宝贝",
            "unmatched rule must not alter the reply; got {:?}",
            deltas[0],
        );

        // Direct outcome assertions: no rule matched → not filtered, raw text out.
        {
            let o = outcome.lock().unwrap();
            assert!(
                !o.filtered,
                "outcome.filtered must be false when no regex rule matched",
            );
            assert_eq!(
                o.produced.len(),
                1,
                "exactly one produced message expected; got {:?}",
                o.produced,
            );
            assert_eq!(
                o.produced[0].full_text, "晚安宝贝",
                "extract/memory must see the raw (unchanged) text when no rule matched",
            );
        }

        // DB row: content == "晚安宝贝", audit columns all NULL.
        let (content, pre_filter, filter_model, filter_triggers): (
            String,
            Option<String>,
            Option<String>,
            Option<serde_json::Value>,
        ) = sqlx::query_as(
            "SELECT content, pre_filter_content, filter_model, filter_triggers \
             FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant' \
             ORDER BY sent_at DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(
            content, "晚安宝贝",
            "persisted content must be the raw reply; got {content:?}",
        );
        assert!(
            pre_filter.is_none(),
            "pre_filter_content must be NULL when no rule matches; got {pre_filter:?}",
        );
        assert!(
            filter_model.is_none(),
            "filter_model must be NULL when no rule matches; got {filter_model:?}",
        );
        assert!(
            filter_triggers.is_none(),
            "filter_triggers must be NULL when no rule matches; got {filter_triggers:?}",
        );
    }

    /// Both layers fire on the SAME turn: the per-model output_regex strips the
    /// artifact (layer 0) AND the LLM output_filter rewrites the reply. The LLM
    /// filter must run on the regex-CLEANED text (not the raw `acc`); the
    /// persisted audit must keep the RAW reply on `pre_filter_content`, set
    /// `filter_model` to the LLM model id (NOT "<regex>"), and fold BOTH the LLM
    /// predicate keys and the `regex` key into `filter_triggers`.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn both_filters_fire_llm_runs_on_cleaned_audit_folds(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let raw_reply = "晚安宝贝[你给对方发送了一张照片：海边自拍]";
        let cleaned_reply = "晚安宝贝";
        let artifact = "你给对方发送了一张照片"; // the bracket payload, never in cleaned

        // ── 1. Dual mock: chat model (SSE) + filter model (JSON). ──────────────
        let mock = MockServer::start().await;
        // Chat model "mock/euryale" streams the artifact-carrying reply.
        let chat_body = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{raw_reply}\"}}}}],\
\"usage\":{{\"prompt_tokens\":2,\"completion_tokens\":8,\"total_tokens\":10}},\
\"id\":\"gen-t6c\",\"model\":\"mock/euryale\"}}\n\n\
data: [DONE]\n\n"
        );
        // Filter model "fast/m" returns a >= MIN_FILTERED_OUTPUT_CHARS (80) rewrite
        // (a real rewrite is always that long) so it passes the validity gate.
        let filt_text = "FILT_START 她轻轻地望向窗外，思绪飘向了远方。阳光洒在她的脸上，温柔而明亮。她记得那个夏天的每一天，岁月如流水般逝去，带走了所有的悲欢离合。 FILT_END";
        let filt_body = serde_json::json!({
            "id": "gf", "model": "fast/m",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": filt_text}}],
        });
        // Mutually-exclusive routing by model id in the request body.
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("fast/m"))
            .respond_with(ResponseTemplate::new(200).set_body_json(filt_body))
            .mount(&mock)
            .await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("mock/euryale"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(chat_body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // ── 2. Seed persona + session ──────────────────────────────────────────
        let user_id = uuid::Uuid::new_v4();
        let (_g, _instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        // ── 3. AppState: output_regex targeting mock/euryale + matching pattern.
        let mut state = crate::routes::companion::test_state(pool.clone());
        let regex_cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel=\"mock/euryale\"\n\
             [[tasks.chat_companion.output_regex]]\n\
             models=[\"mock/euryale\"]\n\
             pattern=\"\\\\s*\\\\[你给对方发送了一张照片[：:][^\\\\]]*\\\\]\\\\s*$\"\n",
        )
        .unwrap();
        state.output_regex = std::sync::Arc::new(
            regex_cfg
                .compile_output_regex()
                .expect("artifact pattern compiles"),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                eros_engine_llm::openrouter::AppAttribution::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        let state = std::sync::Arc::new(state);

        // ── 4. Insert the user message ─────────────────────────────────────────
        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "晚安",
                "01JT5REGEX00000000000000D",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        // ── 5. Build a ResolvedOutputFilter whose trigger fires (models=...). ───
        //      Hand-built (not via PDE) so the burst deterministically filters.
        let filter = eros_engine_llm::model_config::ResolvedOutputFilter {
            model: "fast/m".into(),
            fallback_model: vec![],
            temperature: 0.0,
            max_tokens: 256,
            filter_prompt: "REWRITE".into(),
            trigger: eros_engine_llm::model_config::OutputFilterTrigger {
                random: None,
                models: Some(vec!["mock/euryale".into()]),
                traits: None,
            },
            timing: eros_engine_llm::model_config::FilterTiming::AfterExtract,
            retry_depth: 0,
            reasoning: None,
        };

        let req = eros_engine_llm::openrouter::ChatRequest {
            model: "mock/euryale".into(),
            fallback_model: vec![],
            messages: vec![eros_engine_llm::openrouter::ChatMessage {
                role: "user".into(),
                content: "晚安".into(),
            }],
            temperature: 0.0,
            max_tokens: 64,
            ..Default::default()
        };
        let outcome = std::sync::Arc::new(std::sync::Mutex::new(BurstOutcome::default()));
        let burst = drive_chat_burst(
            state.clone(),
            session_id,
            user_message_id,
            FrameActionType::Reply,
            "reply",
            ActionType::ReplyText,
            req,
            None,
            Some(filter), // LLM output filter that fires (models matches)
            vec![],
            None,
            Default::default(),
            Default::default(),
            None,
            outcome.clone(),
        );
        let frames: Vec<ProtocolFrame> = Box::pin(burst).collect().await;

        // ── 6. Assertions ─────────────────────────────────────────────────────
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "no error frame expected; got {frames:?}",
        );
        // Client sees the LLM-filtered text (never ORIG artifact).
        let deltas: String = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert!(
            deltas.contains("FILT_START"),
            "client must see the LLM-filtered text; got {deltas:?}",
        );
        assert!(
            !deltas.contains(artifact),
            "artifact must never reach client; got {deltas:?}",
        );

        // The LLM filter ran on the regex-CLEANED text: inspect the actual filter
        // request body via received_requests — it must contain the cleaned reply
        // but NOT the bracket artifact.
        let received = mock
            .received_requests()
            .await
            .expect("recording enabled by default");
        let filter_req_body = received
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .find(|b| b.contains("fast/m"))
            .expect("filter model call must have been made");
        assert!(
            filter_req_body.contains(cleaned_reply),
            "filter must run on cleaned text (contains the cleaned reply); body={filter_req_body:?}",
        );
        assert!(
            !filter_req_body.contains(artifact),
            "filter must NOT see the raw artifact (proves it ran on cleaned, not acc); \
             body={filter_req_body:?}",
        );

        // outcome.filtered true; produced (extract input) is the LLM-filtered text
        // (AfterExtract timing feeds extract the original = cleaned baseline, but
        // the burst pushes `extracted` from extract_text(AfterExtract, &cleaned, ..)
        // which is `cleaned`; the LLM-filtered text is what the CLIENT/DB see).
        {
            let o = outcome.lock().unwrap();
            assert!(
                o.filtered,
                "outcome.filtered must be true when filters fired"
            );
            assert_eq!(
                o.produced.len(),
                1,
                "one produced message; got {:?}",
                o.produced
            );
        }

        // ── 6a. DB audit: raw on pre_filter_content, LLM model, BOTH trigger keys.
        let (content, pre_filter, filter_model, filter_triggers): (
            String,
            Option<String>,
            Option<String>,
            Option<serde_json::Value>,
        ) = sqlx::query_as(
            "SELECT content, pre_filter_content, filter_model, filter_triggers \
             FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant' \
             ORDER BY sent_at DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert!(
            content.contains("FILT_START"),
            "persisted content must be the LLM-filtered text; got {content:?}",
        );
        assert_eq!(
            pre_filter.as_deref(),
            Some(raw_reply),
            "pre_filter_content must be the RAW reply (with bracket); got {pre_filter:?}",
        );
        assert_eq!(
            filter_model.as_deref(),
            Some("fast/m"),
            "filter_model must be the LLM model id, NOT '<regex>'; got {filter_model:?}",
        );
        let triggers = filter_triggers.expect("filter_triggers must be present");
        assert_eq!(
            triggers.get("models"),
            Some(&serde_json::json!(["mock/euryale"])),
            "filter_triggers must carry the LLM predicate (models); got {triggers:?}",
        );
        assert_eq!(
            triggers.get("regex"),
            Some(&serde_json::json!([0])),
            "filter_triggers must fold in the regex key; got {triggers:?}",
        );
    }
}
