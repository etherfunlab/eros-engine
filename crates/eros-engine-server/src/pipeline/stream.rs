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

/// Action type tag carried by the `meta` frame's `action_type` field.
///
/// Serializes snake_case: `reply` | `ghost` | `reply_image` | `reply_text_image` | `product_qa`.
///
/// Asymmetry worth calling out: this is the *wire* action, coarser than the
/// internal PDE [`ActionType`]. A plain-text
/// turn (`ActionType::ReplyText`, audited as `reply_text`) is reported here as
/// **`reply`** — there is no `reply_text` on the wire. The text+image variant, by
/// contrast, keeps its full name **`reply_text_image`**. So `reply_text_image`
/// appears but `reply_text` never does. See [`frame_action_for`] for the mapping.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FrameActionType {
    Reply,
    Ghost,
    ReplyImage,
    ReplyTextImage,
    ProductQa,
}

/// One wire frame in the SSE protocol.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProtocolFrame {
    Meta {
        message_id: String,
        /// Coarse wire action: `reply` | `ghost` | `reply_image` |
        /// `reply_text_image`. A plain-text `reply_text` turn is reported as
        /// `reply` (there is no `reply_text` on the wire); only the text+image
        /// variant keeps its full `reply_text_image` name. See [`FrameActionType`].
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
        /// True when this served reply_text resolved empty and is surfaced as a
        /// ghost. The cause lives in the persisted row's metadata.fallback_reason.
        #[serde(default, skip_serializing_if = "is_false")]
        ghost_fallback: bool,
    },
    Final {
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
    /// Delegated image turn: the engine composed the prompt and hands drawing to
    /// the consumer. The engine never draws — this frame is the engine's only
    /// image output.
    ImageRequest {
        message_id: String,
        /// base64(STANDARD, unwrapped) of the UTF-8 final wire prompt — exactly
        /// what the provider would have received. Opaque in transport; the
        /// consumer decodes it at the last hop and uses it verbatim.
        composed_prompt: String,
        image_ref: eros_engine_core::types::ImageRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        aspect_ratio: Option<String>,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Render a 128-bit id as a Crockford Base32 ULID string (26 chars).
pub fn ulid_string(u: Ulid) -> String {
    u.to_string()
}

/// Map the internal PDE `ActionType` to the coarser `FrameActionType` sent on the
/// wire in SSE Meta/Image frames. Consumed by the image execution arm (Task 10).
///
/// Note the asymmetry: `ReplyText` collapses to `Reply` (wire `reply`), so a plain
/// text turn is never reported as `reply_text`; only `ReplyTextImage` keeps its
/// full name (`reply_text_image`). See [`FrameActionType`].
fn frame_action_for(a: eros_engine_core::types::ActionType) -> FrameActionType {
    match a {
        eros_engine_core::types::ActionType::ReplyImage => FrameActionType::ReplyImage,
        eros_engine_core::types::ActionType::ReplyTextImage => FrameActionType::ReplyTextImage,
        eros_engine_core::types::ActionType::Ghost => FrameActionType::Ghost,
        eros_engine_core::types::ActionType::ProductQa => FrameActionType::ProductQa,
        _ => FrameActionType::Reply,
    }
}

/// Build the delegated `image_request` frame. `composed_prompt` is the final
/// wire prompt (style preset + persona appearance + enriched subject) — exactly
/// what the provider would receive. base64(STANDARD, unwrapped) of its UTF-8
/// bytes keeps the explicit/CJK text out of SSE transport. Pure.
fn build_image_request_frame(
    message_id: String,
    composed_prompt: &str,
    image_ref: eros_engine_core::types::ImageRef,
    aspect_ratio: Option<&str>,
) -> ProtocolFrame {
    use base64::Engine as _;
    ProtocolFrame::ImageRequest {
        message_id,
        composed_prompt: base64::engine::general_purpose::STANDARD
            .encode(composed_prompt.as_bytes()),
        image_ref,
        aspect_ratio: aspect_ratio.map(str::to_string),
    }
}

/// `metadata.image` marker for a delegated image turn. Always stores the
/// composer's subject (under `prompt`, the key `assistant_transcript_line`
/// reads) and the aspect ratio; stores `caption` when the composer produced
/// one; when the composer LLM call SUCCEEDED it also stores the audit trio
/// `compose_variant` / `compose_model` / `compose_generation_id` (spec
/// 2026-08-02; absence = no successful compose this turn — raw skip,
/// fail-open, or task not configured). Deliberately NOT stored: the composed
/// wire prompt (the consumer's job), url, or success/failure of the draw.
/// Pure.
fn build_delegated_image_marker(
    subject: &str,
    caption: Option<&str>,
    aspect_ratio: Option<&str>,
    compose_variant: Option<&str>,
    compose_model: Option<&str>,
    compose_generation_id: Option<&str>,
) -> serde_json::Value {
    let mut m = serde_json::json!({ "prompt": subject });
    if let Some(c) = caption.filter(|s| !s.trim().is_empty()) {
        m["caption"] = serde_json::Value::String(c.to_string());
    }
    if let Some(ar) = aspect_ratio.filter(|s| !s.is_empty()) {
        m["aspect_ratio"] = serde_json::Value::String(ar.to_string());
    }
    if let Some(v) = compose_variant.filter(|s| !s.is_empty()) {
        m["compose_variant"] = serde_json::Value::String(v.to_string());
    }
    if let Some(v) = compose_model.filter(|s| !s.is_empty()) {
        m["compose_model"] = serde_json::Value::String(v.to_string());
    }
    if let Some(v) = compose_generation_id.filter(|s| !s.is_empty()) {
        m["compose_generation_id"] = serde_json::Value::String(v.to_string());
    }
    m
}

/// The three ordered frames of an image-only turn: `meta → done → image_request`.
/// No image bytes; meta carries no model (the consumer selects it). Pure.
fn delegated_image_only_frames(
    message_id: String,
    composed_prompt: &str,
    image_ref: eros_engine_core::types::ImageRef,
    aspect_ratio: Option<&str>,
) -> Vec<ProtocolFrame> {
    vec![
        ProtocolFrame::Meta {
            message_id: message_id.clone(),
            action_type: FrameActionType::ReplyImage,
            model: None,
            continues_from: None,
        },
        ProtocolFrame::Done {
            message_id: message_id.clone(),
            truncated: false,
            usage: None,
            generation_id: None,
            ghost_fallback: false,
        },
        build_image_request_frame(message_id, composed_prompt, image_ref, aspect_ratio),
    ]
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
    /// True when this burst ended as an empty-reply ghost fallback. The caller
    /// skips affinity side-effects (the ghost_streak reset) when set.
    pub ghost_fallback: bool,
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
        // A turn buffers (no live deltas) ONLY when the LLM output_filter's
        // turn-level predicates pass — an LLM rewrite is inherently un-streamable
        // (the filtered text must be produced before any of it is safe to send).
        // `output_regex` no longer forces buffering: the live burst streams
        // through the rules incrementally with a bounded holdback
        // (`StreamScrubber`), so a regex-only chain (the common production case)
        // now gets true streaming TTFT instead of full-generation buffering.
        let llm_filter_arms = filter
            .as_ref()
            .map(|f| f.trigger.turn_level_pass(random_draw, &tag_refs))
            .unwrap_or(false);
        let filtered_mode = llm_filter_arms;

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
            // ===== LIVE MODE =====
            // Streams deltas as they arrive, scrubbing output_regex artifacts
            // incrementally via StreamScrubber (bounded holdback). `acc` keeps
            // the RAW text; the wire carries the scrubbed text; the served
            // attempt's persist re-runs the whole-text apply_output_regex so the
            // DB row + regex audit are byte-identical to the old buffered path.
            let mut continues_from: Option<Ulid> = None;
            // Repaired text of the latest COMPLETE garbled attempt seen across the
            // whole chain. Used as the last-resort replacement when the chain
            // exhausts, so a complete garble isn't discarded just because a LATER
            // fallback failed differently (mirrors OpenRouterClient::execute).
            let mut last_complete_garble: Option<String> = None;
            for (idx, model_id) in chain.iter().enumerate() {
                // Model-keyed config tables (display / output_regex / trigger)
                // are written with bare ids; model_id here is the full config
                // slug (may carry @provider — which audit KEEPS, spec §6).
                let bare_model_id = eros_engine_llm::provider::bare_model_id(model_id);
                let msg_ulid = Ulid::new();
                let msg_uuid: Uuid = msg_ulid.into();
                let mut acc = String::new();
                let mut last_usage: Option<eros_engine_llm::openrouter::UsageBlock> = None;
                let mut last_gen_id: Option<String> = None;
                let mut truncated = false;
                let mut empty_completion = false;
                // Scrubs output_regex artifacts out of the wire deltas as they
                // stream; `acc` still accumulates the raw text for the persist
                // apply below. Empty rule set ⇒ pure passthrough.
                let mut scrubber = eros_engine_llm::stream_scrub::StreamScrubber::new(
                    &state.output_regex,
                    &bare_model_id,
                );

                yield ProtocolFrame::Meta {
                    message_id: ulid_string(msg_ulid),
                    action_type: frame_action,
                    model: display_override.as_ref().and_then(|d| d.display(&bare_model_id)),
                    continues_from: continues_from.map(ulid_string),
                };

                // Per-attempt latency observability (spec §4.2). ttft = call →
                // first content delta; outcome is the terminal disposition.
                let attempt_started = std::time::Instant::now();
                let mut ttft_ms: Option<u64> = None;
                let mut attempt_outcome: &'static str = "served";

                // Borrow the shared request; only the served model differs per
                // attempt, so no per-fallback clone of the (large) prompt.
                match tokio::time::timeout(
                    STREAM_OPEN_TIMEOUT,
                    state.openrouter.execute_stream_as(&req, model_id),
                )
                .await
                {
                    Ok(Ok(mut s)) => {
                        use futures_util::StreamExt as _;
                        let deadline = tokio::time::Instant::now() + STREAM_TOTAL_TIMEOUT;
                        loop {
                            let item = match tokio::time::timeout_at(deadline, s.next()).await {
                                Ok(Some(item)) => item,
                                Ok(None) => break,
                                Err(_) => {
                                    tracing::warn!(
                                        "stream: total timeout ({}s), advancing chain",
                                        STREAM_TOTAL_TIMEOUT.as_secs()
                                    );
                                    truncated = true;
                                    attempt_outcome = "total_timeout";
                                    break;
                                }
                            };
                            match item {
                                Ok(c) => {
                                    // `execute_stream` filters empty deltas to None
                                    // (openrouter.rs `.filter(|s| !s.is_empty())`),
                                    // so a present `content` is always a real token.
                                    if let Some(content) = c.content {
                                        acc.push_str(&content);
                                        // Scrub artifacts before they hit the wire;
                                        // ttft counts the first *client-visible*
                                        // token (a leading artifact is held, so the
                                        // first emit can lag the first raw token).
                                        let emit = scrubber.push(&content);
                                        if !emit.is_empty() {
                                            ttft_ms.get_or_insert_with(|| {
                                                attempt_started.elapsed().as_millis() as u64
                                            });
                                            yield ProtocolFrame::Delta {
                                                message_id: ulid_string(msg_ulid),
                                                content: emit,
                                            };
                                        }
                                    }
                                    if c.usage.is_some()         { last_usage = c.usage; }
                                    if c.generation_id.is_some() { last_gen_id = c.generation_id; }
                                    // "content_filter" = mid-generation safety cut
                                    // (Gemini/OpenAI): the text is incomplete, so it
                                    // rides the same truncation → chain-advance path
                                    // as "length" (parity with the sync path's
                                    // filter_output_invalidity gate).
                                    match c.finish_reason.as_deref() {
                                        Some("length") => { truncated = true; attempt_outcome = "length"; }
                                        Some("content_filter") => {
                                            truncated = true;
                                            attempt_outcome = "content_filter";
                                        }
                                        _ => {}
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("stream: upstream chunk err: {e}");
                                    truncated = true;
                                    attempt_outcome = chunk_err_outcome(&e);
                                    break;
                                }
                            }
                        }
                        if !truncated && acc.is_empty() {
                            empty_completion = true;
                            attempt_outcome = "empty";
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!("stream: upstream open err: {e}");
                        truncated = true;
                        attempt_outcome = "open_error";
                    }
                    Err(_) => {
                        tracing::warn!(
                            "stream: open timeout ({}s), advancing chain",
                            STREAM_OPEN_TIMEOUT.as_secs()
                        );
                        truncated = true;
                        attempt_outcome = "open_timeout";
                    }
                }

                // Flush any text the scrubber held at end-of-stream (a completed
                // prefix under the head window, or a trailing unterminated span).
                // Empty scrubber ⇒ "". On a truncated attempt this partial is
                // sent then superseded, same as the raw deltas already were.
                let scrub_tail = scrubber.finish();
                if !scrub_tail.is_empty() {
                    // For a short reply fully held until now (head transform
                    // holds ≤64 chars; Opaque buffers all), this tail is the
                    // FIRST client-visible content — record ttft here too.
                    ttft_ms.get_or_insert_with(|| attempt_started.elapsed().as_millis() as u64);
                    yield ProtocolFrame::Delta {
                        message_id: ulid_string(msg_ulid),
                        content: scrub_tail,
                    };
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
                    attempt_outcome = "garbled";
                    if !truncated_before_garble {
                        last_complete_garble = Some(acc.clone());
                    }
                }

                tracing::info!(
                    target: "stream_metrics",
                    model = %model_id,
                    attempt = idx,
                    ttft_ms,
                    total_ms = attempt_started.elapsed().as_millis() as u64,
                    outcome = attempt_outcome,
                    "chat stream attempt"
                );

                // Disposition, computed once up front (spec §5.3): a non-last
                // empty completion advances to the next model below; only the
                // LAST chain attempt returning empty is a ghost fallback,
                // distinct from length/transport truncation (pseudo-ghost).
                let is_ghost_fallback = empty_completion && idx + 1 == chain.len();
                // A non-last empty completion is a superseded attempt, not a
                // successful turn: mark it `truncated` so the persisted row and
                // its Done frame carry the "replace me" signal (as before this
                // feature) and the client / replay never see a spurious empty
                // reply bubble. Only the LAST empty attempt is the ghost.
                if empty_completion && !is_ghost_fallback {
                    truncated = true;
                }

                // Layer 0: apply output_regex to the RAW acc, but ONLY for the
                // attempt actually served (`!truncated`). A truncated/superseded
                // partial must not match a rule and mislabel the turn (mirrors the
                // filtered burst's same guard). The wire already emitted this same
                // scrubbed text incrementally; here it governs the persisted row +
                // regex audit + the strip-to-empty ghost, byte-identical to the
                // old buffered path.
                let (persist_content, filter_audit, regex_ghost) = if truncated {
                    (acc.clone(), None, false)
                } else {
                    let strip = eros_engine_llm::model_config::apply_output_regex(
                        &state.output_regex,
                        &bare_model_id,
                        &acc,
                    );
                    let audit = if strip.matched_rules.is_empty() {
                        None
                    } else {
                        outcome.lock().unwrap().filtered = true;
                        Some(eros_engine_store::chat::FilterAudit {
                            pre_filter_content: acc.clone(),
                            filter_model: "<regex>".to_string(),
                            filter_triggers: serde_json::json!({ "regex": strip.matched_rules }),
                            f_client_msg_id: format!("f_{}", Ulid::new()),
                            f_generation_id: None,
                        })
                    };
                    // Artifact-only reply: the strip emptied a non-empty completion.
                    // Terminal ghost (does NOT advance the chain), matching the
                    // filtered burst's regex-strip-to-empty semantics.
                    let ghost = !strip.matched_rules.is_empty() && strip.cleaned.is_empty();
                    (strip.cleaned, audit, ghost)
                };
                let effective_ghost = is_ghost_fallback || regex_ghost;

                let usage_full = last_usage.as_ref().and_then(|u| serde_json::to_value(u).ok());
                // Persist BEFORE yielding Done (spec §2.3 risk R7).
                let row = eros_engine_store::chat::AssistantInsert {
                    id: msg_uuid,
                    content: persist_content.clone(),
                    assistant_action_type: persist_action.into(),
                    continues_from_message_id: continues_from.map(Into::into),
                    truncated,
                    model: Some(model_id.clone()),
                    usage: usage_full.clone(),
                    generation_id: last_gen_id.clone(),
                    filter_audit,
                    metadata: if is_ghost_fallback {
                        ghost_fallback_metadata(build_metadata(None), "empty_completion")
                    } else if regex_ghost {
                        ghost_fallback_metadata(build_metadata(None), "regex_strip")
                    } else {
                        build_metadata(None)
                    },
                };
                if let Err(e) = chat_repo
                    .insert_assistant_batch(session_id, user_message_id, &[row])
                    .await
                {
                    tracing::warn!("stream: assistant persist failed: {e}");
                }
                outcome.lock().unwrap().produced.push(crate::pipeline::post_process::ProducedMessage {
                    message_id: msg_uuid,
                    full_text: persist_content.clone(),
                    action: plan_action,
                });

                // Strip OPENROUTER_USAGE_HIDDEN_KEYS from the wire usage. The DB
                // row above persists the full unfiltered usage; only the frame is
                // filtered (mirrors the sync send_message path).
                let mut wire_usage = usage_full;
                filter_usage_keys(&mut wire_usage, &state.config.openrouter_usage_hidden_keys);
                yield ProtocolFrame::Done {
                    message_id: ulid_string(msg_ulid),
                    truncated,
                    usage: wire_usage,
                    generation_id: last_gen_id,
                    ghost_fallback: effective_ghost,
                };

                if is_ghost_fallback {
                    let mut o = outcome.lock().unwrap();
                    o.ghost_fallback = true;
                    // retries_chat = fallback count consumed (0-based, matches
                    // the sibling chain-exhausted-truncated branch below) so
                    // the Final frame doesn't under-report fallback attempts
                    // when only the LAST chain model returns empty.
                    o.retries_chat = (chain.len() as u32).saturating_sub(1);
                    // Drop any earlier (superseded) truncated attempts pushed
                    // in prior loop iterations — mirrors the accept path just
                    // below so post_process (memory/insight/affinity) only
                    // ever sees this ghost's empty full_text, never a partial
                    // truncated attempt from earlier in the chain.
                    o.produced.retain(|m| m.message_id == msg_uuid);
                    return;
                }
                // (A non-last empty completion is now marked `truncated` above,
                // so it falls through to the existing chain-advance path below —
                // no separate branch needed.)

                if !truncated {
                    let mut o = outcome.lock().unwrap();
                    // Only the accepted reply feeds post-process (memory / insight /
                    // affinity). Drop any superseded earlier attempts (truncated, or
                    // garbled-then-repaired) that were pushed while walking the chain
                    // — otherwise rejected provider output would corrupt derived user
                    // state alongside the reply the user actually saw.
                    o.produced.retain(|m| m.message_id == msg_uuid);
                    o.retries_chat = idx as u32;
                    // An artifact-only reply (regex stripped to empty) is a served
                    // ghost: keep affinity's ghost accounting consistent with the
                    // Done frame's ghost_fallback and the filtered burst.
                    if regex_ghost {
                        o.ghost_fallback = true;
                    }
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
            // Model-keyed config tables (display / output_regex / trigger)
            // are written with bare ids; model_id here is the full config
            // slug (may carry @provider — which audit KEEPS, spec §6).
            let bare_model_id = eros_engine_llm::provider::bare_model_id(model_id);
            let msg_ulid = Ulid::new();
            let msg_uuid: Uuid = msg_ulid.into();
            let mut acc = String::new();
            let mut last_usage: Option<eros_engine_llm::openrouter::UsageBlock> = None;
            let mut last_gen_id: Option<String> = None;
            let mut truncated = false;
            let mut empty_completion = false;

            // Per-attempt observability (spec §4.2). In filtered mode the client
            // sees nothing until the whole reply is rewritten, so ttft_ms here is
            // time-to-first-UPSTREAM-token (still useful to compare model speed),
            // not time-to-client.
            let attempt_started = std::time::Instant::now();
            let mut ttft_ms: Option<u64> = None;
            let mut attempt_outcome: &'static str = "served";
            // Borrow the shared request; no per-fallback prompt clone.
            match tokio::time::timeout(
                STREAM_OPEN_TIMEOUT,
                state.openrouter.execute_stream_as(&req, model_id),
            )
            .await
            {
                Ok(Ok(mut s)) => {
                    use futures_util::StreamExt as _;
                    let deadline = tokio::time::Instant::now() + STREAM_TOTAL_TIMEOUT;
                    loop {
                        let item = match tokio::time::timeout_at(deadline, s.next()).await {
                            Ok(Some(item)) => item,
                            Ok(None) => break,
                            Err(_) => {
                                tracing::warn!(
                                    "stream(filtered): total timeout ({}s), advancing chain",
                                    STREAM_TOTAL_TIMEOUT.as_secs()
                                );
                                truncated = true;
                                attempt_outcome = "total_timeout";
                                break;
                            }
                        };
                        match item {
                            Ok(c) => {
                                // Empty deltas are already filtered to None by
                                // execute_stream, so a present `content` is a real
                                // upstream token — ttft is not tripped by role/
                                // terminal empty frames.
                                if let Some(content) = c.content {
                                    ttft_ms.get_or_insert_with(|| {
                                        attempt_started.elapsed().as_millis() as u64
                                    });
                                    acc.push_str(&content);
                                }
                                if c.usage.is_some() { last_usage = c.usage; }
                                if c.generation_id.is_some() { last_gen_id = c.generation_id; }
                                match c.finish_reason.as_deref() {
                                    Some("length") => { truncated = true; attempt_outcome = "length"; }
                                    Some("content_filter") => { truncated = true; attempt_outcome = "content_filter"; }
                                    _ => {}
                                }
                            }
                            Err(e) => {
                                tracing::warn!("stream(filtered): chunk err: {e}");
                                truncated = true;
                                attempt_outcome = chunk_err_outcome(&e);
                                break;
                            }
                        }
                    }
                    if !truncated && acc.is_empty() { empty_completion = true; attempt_outcome = "empty"; }
                }
                Ok(Err(e)) => {
                    tracing::warn!("stream(filtered): open err: {e}");
                    truncated = true;
                    attempt_outcome = "open_error";
                }
                Err(_) => {
                    tracing::warn!(
                        "stream(filtered): open timeout ({}s), advancing chain",
                        STREAM_OPEN_TIMEOUT.as_secs()
                    );
                    truncated = true;
                    attempt_outcome = "open_timeout";
                }
            }

            if eros_engine_llm::byte_bpe::looks_byte_garbled(&acc) {
                tracing::error!(model = %model_id, "stream(filtered): byte-BPE garbled completion (issue #84)");
                acc = eros_engine_llm::byte_bpe::repair_byte_bpe(&acc);
                attempt_outcome = "garbled";
                // Nothing has been streamed to the client yet, so a COMPLETE garble
                // is salvaged immediately: the repaired (clean) text flows through
                // the output filter + persist below. We deliberately do NOT force a
                // fallback — doing so would discard a recoverable complete garble if
                // the later attempt failed. An INCOMPLETE garble is already
                // `truncated` (length / transport) and handled by the block below.
            }

            tracing::info!(
                target: "stream_metrics",
                model = %model_id,
                attempt = idx,
                ttft_ms,
                total_ms = attempt_started.elapsed().as_millis() as u64,
                outcome = attempt_outcome,
                filtered = true,
                "chat stream attempt"
            );

            if empty_completion {
                if idx + 1 == chain.len() {
                    // Last attempt served a 200 OK with an empty body: ghost
                    // fallback (affinity-neutral), NOT the pseudo-ghost/Error
                    // path below — that's reserved for length/transport
                    // truncation. Mirrors the regex-strip-to-empty case (a)
                    // above (`ghost_fallback_metadata`), tagged distinctly as
                    // "empty_completion" so ops can tell the two apart.
                    let msg_ulid = Ulid::new();
                    let msg_uuid: Uuid = msg_ulid.into();
                    let usage_full =
                        last_usage.as_ref().and_then(|u| serde_json::to_value(u).ok());
                    let row = eros_engine_store::chat::AssistantInsert {
                        id: msg_uuid,
                        content: String::new(),
                        assistant_action_type: persist_action.into(),
                        continues_from_message_id: None,
                        truncated: false,
                        model: Some(model_id.clone()),
                        usage: usage_full.clone(),
                        generation_id: last_gen_id.clone(),
                        filter_audit: None,
                        metadata: ghost_fallback_metadata(build_metadata(None), "empty_completion"),
                    };
                    if let Err(e) = chat_repo
                        .insert_assistant_batch(session_id, user_message_id, &[row])
                        .await
                    {
                        tracing::warn!("stream(filtered): ghost-fallback persist failed: {e}");
                    }
                    {
                        // Mirror the sibling truncated branch: report the
                        // fallback attempts consumed so the Final frame's
                        // retries_chat isn't under-reported when only the LAST
                        // chain model returns an empty completion.
                        let mut o = outcome.lock().unwrap();
                        o.ghost_fallback = true;
                        o.retries_chat = (chain.len() as u32).saturating_sub(1);
                        // Keep an (empty) produced row so a ReplyTextImage turn's
                        // trailing image_request still fires — the caller gates it
                        // on `produced.last()`. The live and regex-strip ghost
                        // paths both retain their row; without this, filtered-mode
                        // text+image turns would silently drop the image half. The
                        // empty full_text keeps the turn memory/insight/eval-neutral
                        // downstream (persist_affinity's rule delta is unchanged).
                        o.produced
                            .push(crate::pipeline::post_process::ProducedMessage {
                                message_id: msg_uuid,
                                full_text: String::new(),
                                action: plan_action,
                            });
                    }
                    yield ProtocolFrame::Meta {
                        message_id: ulid_string(msg_ulid),
                        action_type: frame_action,
                        model: display_override.as_ref().and_then(|d| d.display(&bare_model_id)),
                        continues_from: None,
                    };
                    // Forward the served usage (a provider can emit a usage block
                    // on an otherwise-empty completion) — same wire contract as the
                    // other served Done frames; the DB row above already persisted
                    // the full unfiltered usage.
                    let mut wire_usage = usage_full;
                    filter_usage_keys(&mut wire_usage, &state.config.openrouter_usage_hidden_keys);
                    yield ProtocolFrame::Done {
                        message_id: ulid_string(msg_ulid),
                        truncated: false,
                        usage: wire_usage,
                        generation_id: last_gen_id,
                        ghost_fallback: true,
                    };
                    return;
                }
                continue; // non-last empty completion: try the next model
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
                model: display_override.as_ref().and_then(|d| d.display(&bare_model_id)),
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
                &bare_model_id,
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
            ) = if !regex_indices.is_empty() && cleaned.is_empty() {
                // The regex strip emptied the WHOLE reply (artifact-only): this
                // is terminal. Do NOT hand "" to the LLM output_filter — a
                // rewrite model can return non-empty text and resurrect a bubble,
                // defeating the no-content-bubble guarantee. Emit nothing; the
                // regex audit (raw on pre_filter_content) still records the strip.
                (String::new(), regex_audit(&acc), None)
            } else {
                match f_opt {
                    Some(f) => {
                    let hits = f.trigger.should_filter(&bare_model_id, &tag_refs, random_draw);
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
                }
            };
            // Empty visible text (regex-strip-to-empty, case a) means nothing
            // was served: the assistant row is a ghost fallback, not a normal
            // reply — tag it in metadata and keep affinity's ghost_streak
            // untouched (see BurstOutcome.ghost_fallback gating). Tagging it
            // `"regex_strip"` is always correct here: `visible` can only be
            // empty via the regex-strip-to-empty branch above, since the LLM
            // output filter fails open to the (non-empty) `cleaned` text and
            // never emptifies an otherwise non-empty reply.
            let is_ghost = visible.is_empty();

            if !visible.is_empty() {
                yield ProtocolFrame::Delta {
                    message_id: ulid_string(msg_ulid),
                    content: visible.clone(),
                };
            }

            let usage_full = last_usage.as_ref().and_then(|u| serde_json::to_value(u).ok());
            let row = eros_engine_store::chat::AssistantInsert {
                id: msg_uuid,
                content: visible.clone(),
                assistant_action_type: persist_action.into(),
                continues_from_message_id: None,
                truncated: false,
                model: Some(model_id.clone()),
                usage: usage_full.clone(),
                generation_id: last_gen_id.clone(),
                filter_audit,
                metadata: if is_ghost {
                    ghost_fallback_metadata(build_metadata(filter_failure.as_ref()), "regex_strip")
                } else {
                    build_metadata(filter_failure.as_ref())
                },
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

            let mut wire_usage = usage_full;
            filter_usage_keys(&mut wire_usage, &state.config.openrouter_usage_hidden_keys);
            if is_ghost {
                outcome.lock().unwrap().ghost_fallback = true;
            }
            yield ProtocolFrame::Done {
                message_id: ulid_string(msg_ulid),
                truncated: false,
                usage: wire_usage,
                generation_id: last_gen_id,
                ghost_fallback: is_ghost,
            };
            return;
        }
    }
}

/// Assistant-row metadata for an empty-reply ghost fallback: the base metadata
/// bag (may be None) plus a `fallback_reason` tag.
fn ghost_fallback_metadata(
    base: Option<serde_json::Value>,
    reason: &str,
) -> Option<serde_json::Value> {
    let mut obj = base
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    obj.insert("fallback_reason".into(), serde_json::json!(reason));
    Some(serde_json::Value::Object(obj))
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
pub(crate) const FILTER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Max wait for a chat stream to OPEN (connect + queue + response headers).
/// A provider that accepts the socket but never sends headers must not hold
/// the turn — timeout ⇒ attempt fails ⇒ chain advances. `pub(crate)`: the
/// voice path applies the same caps (issue #188).
pub(crate) const STREAM_OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// Hard per-attempt cap on one model's whole generation (spec §1.5's 120s).
/// Byte-level idle liveness is bounded upstream in the llm client; this caps
/// a stream that keeps trickling forever.
pub(crate) const STREAM_TOTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Spec §4.2 outcome label for an `Err` item mid-consume. Provider error
/// frames (`LlmError::Provider`) and the byte-level idle watchdog are their
/// own failure modes — folding them into `"chunk_error"` made dashboards
/// undercount idle timeouts and blend provider failures into transport drops
/// (issue #188). Everything else (transport reset, parse) stays
/// `"chunk_error"`.
fn chunk_err_outcome(e: &eros_engine_llm::LlmError) -> &'static str {
    match e {
        eros_engine_llm::LlmError::Provider(_) => "error_frame",
        eros_engine_llm::LlmError::Stream(msg)
            if msg.contains(eros_engine_llm::openrouter::STREAM_IDLE_TIMEOUT_MSG) =>
        {
            "idle_timeout"
        }
        _ => "chunk_error",
    }
}

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
            sampling: f.sampling,
            reasoning: f.reasoning.clone(),
            task: Some("chat_output_filter".into()),
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
    super::parse_llm_json(text)
}

// ── PDE judge primitives ──────────────────────────────────────────────────────

/// Judge verdict action. serde `snake_case` matches the JSON contract
/// (`reply_text` / `ghost` / `reply_image` / `reply_text_image` / `product_qa`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PdeAction {
    ReplyText,
    Ghost,
    ReplyImage,
    ReplyTextImage,
    ProductQa,
}

impl PdeAction {
    fn as_str(self) -> &'static str {
        match self {
            PdeAction::ReplyText => "reply_text",
            PdeAction::Ghost => "ghost",
            PdeAction::ReplyImage => "reply_image",
            PdeAction::ReplyTextImage => "reply_text_image",
            PdeAction::ProductQa => "product_qa",
        }
    }
}

/// Parsed judge verdict. `inner_state` is sanitized (`sanitize_inner_state`)
/// before it reaches the prompt; `reason` is never injected. The judge writes
/// no image prompt — composition belongs to `chat_image_prompt_compose` (#212).
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct PdeVerdict {
    action: PdeAction,
    #[serde(default)]
    inner_state: String,
    /// Prescriptive delivery for this turn's reply (free text; sanitized like
    /// inner_state before injection). `None` on old prompts / null verdicts.
    #[serde(default)]
    tone: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    image_ref: eros_engine_core::types::ImageRef,
    #[serde(default)]
    aspect_ratio: Option<String>,
}

/// Parse the judge reply: direct JSON first, then a balanced JSON block in prose
/// (mirrors `parse_input_filter_verdict`).
fn parse_pde_verdict(text: &str) -> Option<PdeVerdict> {
    super::parse_llm_json(text)
}

const INNER_STATE_MAX_CHARS: usize = 200;

/// Sanitize judge-authored prose (`inner_state` / `tone`) before folding it into
/// the system prompt's `[inner_state]` / `[reply_tone]` sections. Drops lines
/// that look like prompt section
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
                "required": ["action", "inner_state", "tone", "reason", "image_ref", "aspect_ratio"],
                "properties": {
                    "action": { "type": "string",
                        "enum": ["reply_text", "ghost", "reply_image", "reply_text_image", "product_qa"] },
                    "inner_state": { "type": "string" },
                    "tone": { "type": ["string", "null"] },
                    "reason": { "type": ["string", "null"] },
                    "image_ref": { "type": "string", "enum": ["face", "previous"] },
                    "aspect_ratio": { "type": ["string", "null"],
                        "enum": ["1:1", "3:4", "4:3", "9:16", "16:9", null] }
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
            sampling: p.sampling,
            reasoning: p.reasoning.clone(),
            response_format: response_format.clone(),
            task: Some("pde_decision".into()),
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

/// Whether an image action is possible this turn.
///
/// Two independent facts must both hold: the request carries an `image` block
/// (the consumer signalling "I handle images this turn" — the engine holds no
/// image configuration and never draws), and `[tasks.chat_image_prompt_compose]`
/// is configured. The composer became mandatory when the judge stopped writing
/// seeds (#212): with no seed and no composer, nothing can produce an image
/// prompt at all.
///
/// The check is the task section's PRESENCE, deliberately not a
/// `resolve_image_prompt_compose(..)` call: that resolver reaches
/// `self.resolve(COMPOSE_TASK, None)`, which advances the round-robin model
/// cursor as a side effect, so calling it merely to answer a capability
/// question would skew which model later image turns actually pick.
fn image_capability_available(
    executor_available: bool,
    model_config: &eros_engine_llm::model_config::ModelConfig,
) -> bool {
    executor_available && model_config.has_task("chat_image_prompt_compose")
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
    product_qa_available: bool,
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
        PdeAction::ProductQa if product_qa_available => ActionType::ProductQa,
        // Hallucinated / stale-prompt proposal with the task unconfigured (or
        // the PDE-off deployment's schema echo): degrade like the image actions.
        PdeAction::ProductQa => ActionType::ReplyText,
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

/// Render recent product-QA pairs for the judge's `[最近产品咨询]` block and
/// the executor's follow-up context. Plain 用户/回答 lines, chronological.
fn render_product_qa_pairs(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(q, a)| format!("用户: {q}\n回答: {a}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the judge's user payload from the shared transcript + the decision input.
fn build_pde_ctx(
    t: &JudgeTranscript,
    input: &eros_engine_core::types::DecisionInput,
    image_available: bool,
    product_qa_recent: Option<&str>,
) -> String {
    let a = &input.affinity;
    let s = &input.signals;
    let latest = match &input.event {
        eros_engine_core::types::Event::UserMessage { content, .. } => content.as_str(),
        _ => "",
    };
    let transcript = if t.transcript.trim().is_empty() {
        "（无）"
    } else {
        t.transcript.as_str()
    };
    let brief = build_persona_brief(&input.persona);
    let persona_block = if brief.is_empty() {
        String::new()
    } else {
        format!("{brief}\n\n")
    };
    // Precomputed relationship buckets. The judge acts on stated facts but is
    // unreliable at deriving them: told to fold the six axes itself it mis-ranks
    // deep relationships as shallow, so both buckets arrive as engine-owned
    // lines. Unconditional, like 图片能力 — the low end of either scale is
    // itself a signal, so a missing line must never read as "rung 1".
    // Buckets ONLY, no numbers: raw scores would re-open the float comparison
    // the buckets exist to remove. The numeric state lives in the decision
    // row's `inputs` snapshot instead (issue #254).
    let rung = a.intimacy_rung();
    let patience_band = match a.patience_band() {
        eros_engine_core::affinity::PatienceBand::High => "高",
        eros_engine_core::affinity::PatienceBand::Mid => "中",
        eros_engine_core::affinity::PatienceBand::Low => "低",
    };
    // Always emit the image-capability line — the negative is a signal too, so
    // the judge gets a clear "no images this turn" rather than a missing line.
    let image_flag = if image_available { "是" } else { "否" };
    // Engine-counted facts: the judge cannot reliably count image markers in
    // the transcript, so state both numbers explicitly and tell it to trust
    // this line over the markers.
    let img_count = t.images_in_window;
    let last_img = if t.last_assistant_is_image {
        "是"
    } else {
        "否"
    };
    // Product-QA lines render ONLY when the task is enabled this deployment —
    // old judge prompts see zero drift and pay zero tokens (unlike 图片能力,
    // whose negative is itself a signal). `Some("")` = enabled, no history yet.
    let product_qa_section = match product_qa_recent {
        None => String::new(),
        Some("") => "[产品咨询] 本轮可答产品问题=是\n".to_string(),
        Some(recent) => {
            format!("[产品咨询] 本轮可答产品问题=是\n[最近产品咨询]\n{recent}\n")
        }
    };
    format!(
        "{persona_block}[最近对话]\n{transcript}\n\n\
         [亲密度] 当前档位=第 {rung} 档\n\
         [耐心] 当前档位={patience_band}\n\
         [信号] message_count={} hours_since_last_message={:.1} ghost_streak={} hours_since_last_ghost={}\n\
         [图片能力] 本轮可发图={image_flag}\n\
         [近期图片] 最近{INPUT_FILTER_CONTEXT_TURNS}条消息内已发图={img_count} 张；上一条 AI 消息是图片={last_img}（以本行计数为准，对话记录里的图片标记仅供参考）\n\
         {product_qa_section}\n\
         [用户最新消息]\n{latest}",
        s.message_count,
        s.hours_since_last_message,
        s.ghost_streak,
        s.hours_since_last_ghost
            .map(|h| format!("{h:.1}"))
            .unwrap_or_else(|| "none".into()),
    )
}

/// Engine-computed relationship state as shown to the judge this run (issue
/// #254): the two discrete labels the prompt carries, plus the line scores and
/// raw axes they were cut from — the prompt itself no longer carries any
/// number, so this snapshot is the only place the gate's inputs survive.
/// Written to `companion_decision_events.inputs`; fail-open at the call site.
fn pde_inputs_snapshot(a: &eros_engine_core::affinity::Affinity) -> serde_json::Value {
    let band = match a.patience_band() {
        eros_engine_core::affinity::PatienceBand::High => "high",
        eros_engine_core::affinity::PatienceBand::Mid => "mid",
        eros_engine_core::affinity::PatienceBand::Low => "low",
    };
    serde_json::json!({
        "v": 1,
        "intimacy_rung": a.intimacy_rung(),
        "patience_band": band,
        "bond": a.bond_score(),
        "chemistry": a.chemistry_score(),
        "axes": {
            "warmth": a.warmth,
            "trust": a.trust,
            "intrigue": a.intrigue,
            "intimacy": a.intimacy,
            "patience": a.patience,
            "tension": a.tension,
        },
    })
}

/// Serializable view of a verdict for the audit `payload` column.
#[derive(serde::Serialize)]
struct VerdictAudit<'a> {
    action: &'a str,
    inner_state: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tone: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    image_ref: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    aspect_ratio: Option<&'a str>,
}

impl<'a> From<&'a PdeVerdict> for VerdictAudit<'a> {
    fn from(v: &'a PdeVerdict) -> Self {
        VerdictAudit {
            action: v.action.as_str(),
            inner_state: &v.inner_state,
            tone: v.tone.as_deref(),
            reason: v.reason.as_deref(),
            image_ref: match v.image_ref {
                eros_engine_core::types::ImageRef::Face => "face",
                eros_engine_core::types::ImageRef::Previous => "previous",
            },
            aspect_ratio: v.aspect_ratio.as_deref(),
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
        ActionType::ProductQa => "product_qa",
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
    super::parse_llm_json(text)
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
            sampling: v.sampling,
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

/// Rows fed to the rewrite LLM as `[最近对话]` context, and the window the
/// judge's `[近期图片]` counts cover.
///
/// NOTE the name is a misnomer kept for compatibility: `ChatRepo::history`
/// applies `LIMIT` to **rows**, so this is the last 8 *messages* (roughly 4
/// exchanges), not 8 turns. Anything rendered for a model must say messages.
/// Renaming it and changing the window are both explicit non-goals of the
/// image-context design — the issue's bench inherited exactly this window, so
/// its measured numbers only transfer while the window holds.
const INPUT_FILTER_CONTEXT_TURNS: i64 = 8;

/// Render an assistant transcript line. Image turns persist empty `content`
/// with the image facts under `metadata.image`; surface a terse marker so the
/// judge / input filter see that an image was sent (and roughly what it showed)
/// instead of a blank `AI:` line. Non-image assistant rows fall back to
/// `content`. Pure.
///
/// The description comes from `metadata.image.caption` — a short line written
/// by the composer for exactly this purpose. It is deliberately NOT
/// `metadata.image.prompt`: that is the image-generation subject, which leads
/// with style-preset and appearance boilerplate and is long enough that echoing
/// it dominated the judge's context. When no caption exists (rows written before
/// captions shipped, a composer reply that carried none, a failed compose) the
/// marker stays bare rather than falling back to the prompt — the anti-spam
/// brake rides on `[近期图片]`'s counts, which do not depend on this text.
fn assistant_transcript_line(content: &str, metadata: Option<&serde_json::Value>) -> String {
    if let Some(img) = metadata.and_then(|m| m.get("image")) {
        let caption = img
            .get("caption")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let ar = img
            .get("aspect_ratio")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return match (caption, ar.is_empty()) {
            (None, _) => "（发送了一张图片）".to_string(),
            (Some(c), true) => format!("（发送了一张图片：{c}）"),
            (Some(c), false) => format!("（发送了一张图片：{c}，画幅 {ar}）"),
        };
    }
    content.to_string()
}

/// The judge/input-filter context: the rendered transcript plus the two image
/// facts the engine can compute exactly and the judge cannot count reliably.
#[derive(Debug, Default, Clone)]
struct JudgeTranscript {
    transcript: String,
    /// Assistant rows in the window carrying `metadata.image`.
    images_in_window: usize,
    /// The newest assistant row in the window is an image turn.
    last_assistant_is_image: bool,
}

/// Row-by-row accumulator behind `JudgeTranscript`, split out so the counting
/// is unit-testable without a database.
#[derive(Debug, Default)]
struct JudgeTranscriptAcc {
    lines: Vec<String>,
    images_in_window: usize,
    last_assistant_is_image: bool,
}

impl JudgeTranscriptAcc {
    /// Fold one already-filtered row (caller has skipped the current turn and
    /// channel-marked rows). Rows arrive oldest→newest, so the assistant flag
    /// is simply overwritten and ends up reflecting the newest assistant row.
    fn push(&mut self, role: &str, content: &str, metadata: Option<&serde_json::Value>) {
        let (label, text): (&str, String) = match role {
            "user" | "gift_user" => ("用户", content.to_string()),
            "assistant" => {
                let is_image = metadata.and_then(|m| m.get("image")).is_some();
                if is_image {
                    self.images_in_window += 1;
                }
                self.last_assistant_is_image = is_image;
                ("AI", assistant_transcript_line(content, metadata))
            }
            _ => return,
        };
        self.lines.push(format!("{label}: {text}"));
    }

    fn finish(self) -> JudgeTranscript {
        JudgeTranscript {
            transcript: self.lines.join("\n"),
            images_in_window: self.images_in_window,
            last_assistant_is_image: self.last_assistant_is_image,
        }
    }
}

/// Build the compact transcript block for the input filter and the PDE judge,
/// excluding the turn being rewritten, and count the window's image turns
/// while the rows are in hand (no second round trip). Best-effort: a DB error
/// yields an empty transcript and zero counts — which reads to the judge as
/// "no recent images", the same signal an empty transcript gives today.
async fn build_input_filter_transcript(
    chat_repo: &ChatRepo<'_>,
    session_id: Uuid,
    current_user_message_id: Uuid,
) -> JudgeTranscript {
    // +1: the turn being processed is already persisted, so it always occupies
    // the newest fetched slot and is then excluded below. Fetching exactly the
    // window size would leave 7 prior messages while `[近期图片]` tells the
    // judge it counted 8 — an every-turn off-by-one on the anti-spam facts.
    let rows = chat_repo
        .history(session_id, INPUT_FILTER_CONTEXT_TURNS + 1, 0)
        .await
        .unwrap_or_default();
    let mut acc = JudgeTranscriptAcc::default();
    for m in rows {
        if m.id == current_user_message_id {
            continue;
        }
        // Channel-marked rows (voice / product_qa) are out of companion
        // context — the judge and input filter never see them.
        if m.channel.is_some() {
            continue;
        }
        // User/gift rows use the EFFECTIVE text (a prior turn's own rewrite when
        // present) so the filter sees the same conversation the chat model does;
        // assistant rows use content (their pre_filter_content means the opposite).
        let text = match m.role.as_str() {
            "user" | "gift_user" => crate::pipeline::handlers::effective_user_text(&m).to_string(),
            _ => m.content.clone(),
        };
        acc.push(&m.role, &text, m.metadata.as_ref());
    }
    acc.finish()
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
            sampling: f.sampling,
            reasoning: f.reasoning.clone(),
            task: Some("chat_input_filter".into()),
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

/// Assemble the composer's user message from the appearance, recent scene,
/// latest user message, style, and aspect ratio. Pure (kept separate so it is
/// testable without a network call).
pub(crate) fn compose_user_payload(
    appearance: &str,
    recent_scene: &str,
    latest_user_msg: &str,
    style: &str,
    aspect_ratio: &str,
) -> String {
    format!(
        "[人物外观]\n{appearance}\n\n[最近场景]\n{recent_scene}\n\n[对方最新消息]\n{latest_user_msg}\n\n[风格]\n{style}\n\n[画幅]\n{aspect_ratio}"
    )
}

/// The composer's JSON contract.
#[derive(serde::Deserialize)]
struct ComposeReply {
    prompt: String,
    #[serde(default)]
    caption: Option<String>,
}

/// Parse a composer reply into `(prompt, caption)`.
///
/// Direct JSON first, then a balanced JSON block in prose (same ladder as
/// `parse_pde_verdict`). **A reply that is neither becomes the prompt with no
/// caption** — deliberate, and load-bearing for migration: a deployment that
/// ships this version without rewriting its EXPAND-era composer `filter_prompt`
/// still gets working images (degraded, since that prompt aims at a seed that
/// is now usually empty), just bare markers until the prompt is updated.
///
/// A blank caption is normalised to `None`; callers must not persist `""`.
pub(crate) fn parse_compose_reply(raw: &str) -> (String, Option<String>) {
    let parsed = super::parse_llm_json::<ComposeReply>(raw);
    match parsed {
        Some(r) => {
            let caption = r
                .caption
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty());
            (r.prompt.trim().to_string(), caption)
        }
        None => (raw.trim().to_string(), None),
    }
}

/// A SUCCESSFUL composer call's result: the picture subject, the short caption
/// for history-facing renders, plus the audit values persisted to
/// `metadata.image` (spec 2026-08-02). Mirrors `VisionOutcome`.
pub(crate) struct ComposeOutcome {
    pub(crate) prompt: String,
    /// One short line describing what the picture shows, for the chat history
    /// and the judge transcript. `None` when the model gave none — including
    /// when the reply wasn't JSON at all (the migration fallback in
    /// `parse_compose_reply`, where the whole reply becomes `prompt` instead).
    pub(crate) caption: Option<String>,
    /// Model that actually answered: `resp.model`, falling back to the
    /// attempted model id (same idiom as the vision audit).
    pub(crate) model: String,
    pub(crate) generation_id: Option<String>,
    /// `ResolvedImagePromptCompose::variant_key`, carried so the call site
    /// doesn't need the resolved config in scope.
    pub(crate) variant: Option<String>,
}

/// Generate the image prompt (and its caption) via the optional composer LLM.
/// Walks `[model] + fallback` on transport failure (error/timeout/empty);
/// returns the parsed prompt/caption plus the audit trio on first success, or
/// `None` (caller falls back to an empty subject — the portrait path). Never
/// blocks or fails the image turn. Mirrors `run_input_filter`.
///
/// Shared with `routes/persona.rs`: the standalone compose endpoint's
/// non-stream mode maps a `None` here to a 502 instead of the chat path's
/// fail-open (spec 2026-08-03 §3.6 — no portrait fallback there).
pub(crate) async fn run_image_prompt_compose(
    state: &AppState,
    c: &eros_engine_llm::model_config::ResolvedImagePromptCompose,
    persona: &eros_engine_core::persona::CompanionPersona,
    recent_scene: &str,
    latest_user_msg: &str,
    aspect_ratio: Option<&str>,
    style: &str,
) -> Option<ComposeOutcome> {
    use eros_engine_llm::openrouter::{ChatMessage, ChatRequest};
    let appearance = crate::prompt::meta_str(persona, "appearance")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("（无）");
    let scene = if recent_scene.trim().is_empty() {
        "（无）"
    } else {
        recent_scene
    };
    let latest = if latest_user_msg.trim().is_empty() {
        "（无）"
    } else {
        latest_user_msg
    };
    let ar = aspect_ratio.unwrap_or("（未指定）");
    let user_payload = compose_user_payload(appearance, scene, latest, style, ar);
    let chain: Vec<String> = std::iter::once(c.model.clone())
        .chain(c.fallback_model.iter().cloned())
        .collect();
    for model_id in &chain {
        let req = ChatRequest {
            model: model_id.clone(),
            fallback_model: vec![],
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: c.compose_prompt.clone(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: user_payload.clone(),
                },
            ],
            temperature: c.temperature as f32,
            max_tokens: c.max_tokens,
            sampling: c.sampling,
            reasoning: c.reasoning.clone(),
            task: Some("chat_image_prompt_compose".into()),
            ..Default::default()
        };
        let resp = match tokio::time::timeout(FILTER_TIMEOUT, state.openrouter.execute(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::warn!(model = %model_id, error = %e, "image-compose: model error; next");
                continue;
            }
            Err(_) => {
                tracing::warn!(model = %model_id, "image-compose: timeout; next");
                continue;
            }
        };
        super::log_openrouter_usage("chat_image_prompt_compose", None, &resp);
        let text = resp.reply.trim().to_string();
        if text.is_empty() {
            tracing::warn!(model = %model_id, "image-compose: empty reply; next");
            continue;
        }
        let (prompt, caption) = parse_compose_reply(&text);
        if prompt.is_empty() {
            tracing::warn!(model = %model_id, "image-compose: empty prompt after parse; next");
            continue;
        }
        return Some(ComposeOutcome {
            prompt,
            caption,
            model: resp.model.unwrap_or_else(|| model_id.clone()),
            generation_id: resp.generation_id,
            variant: c.variant_key.clone(),
        });
    }
    None
}

/// The two per-turn image inputs, resolved from plan → request. There is no
/// subject field: the composer decides what the picture shows from turn
/// context alone.
struct ImageTurnInputs {
    style: eros_engine_llm::model_config::StyleKey,
    aspect_ratio: Option<String>,
}

/// Pure: resolve the style and aspect ratio for a delegated image turn.
/// Precedence per field:
/// - style:  `req_image.style` → type default (`Realistic`)
/// - aspect: `plan.aspect_ratio` → `req_image.aspect_ratio` → `None`
///
/// There is no subject input here: the judge no longer writes a seed (#212
/// Task 4) and the client can no longer supply one either (#212 Task 5) — the
/// composer decides what the picture shows from turn context alone.
///
/// Blank strings count as absent at the plan and request levels. There are no
/// config-level defaults: the engine carries no image configuration, so style
/// and aspect are per-turn inputs only.
fn resolve_image_turn_inputs(
    plan: &eros_engine_core::types::ActionPlan,
    req_image: Option<&crate::routes::companion_stream::ImageReplyParams>,
) -> ImageTurnInputs {
    let style = req_image.and_then(|i| i.style).unwrap_or_default();
    let aspect_ratio = plan
        .aspect_ratio
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            req_image
                .and_then(|i| i.aspect_ratio.as_deref())
                .filter(|s| !s.trim().is_empty())
        })
        .map(str::to_string);
    ImageTurnInputs {
        style,
        aspect_ratio,
    }
}

/// Everything the two delegated-image call sites need. `style` is deliberately
/// absent: it is consumed internally by `compose_image_prompt` and neither
/// caller uses it afterwards.
struct DelegatedImagePrompt {
    /// What described the picture — feeds `build_delegated_image_marker`'s
    /// `prompt` key. The composer's decided subject on the compose path
    /// (spec 2026-08-02: this is deliberately the SHORT subject, not the
    /// composed wire string — that lives only in `composed_prompt`, below, and
    /// is never persisted); empty when the composer is skipped (not
    /// configured, or `raw`) or the compose call fails — there is no seed left
    /// to fall back to, so an empty subject is the portrait fallback (#212).
    subject: String,
    /// Short description of the picture, persisted as `metadata.image.caption`
    /// and read by every history-facing render. `None` on a failed compose or
    /// when the task is not configured.
    caption: Option<String>,
    /// Effective aspect ratio — feeds the marker and the wire frame.
    aspect_ratio: Option<String>,
    /// Final wire prompt — feeds the wire frame.
    composed_prompt: String,
    /// Audit trio — `Some` only when the composer call succeeded; feeds the
    /// marker's `compose_*` keys (spec 2026-08-02).
    compose_variant: Option<String>,
    compose_model: Option<String>,
    compose_generation_id: Option<String>,
}

/// Guards a speculatively-spawned `tokio::task::JoinHandle` so it is aborted
/// if it's ever dropped without being joined. Dropping a `JoinHandle` on its
/// own does NOT cancel the task — it keeps running to completion in the
/// background, discarding its result. `reply_text_image` spawns the image
/// composer early (concurrently with the chat call) so its latency hides
/// underneath, but the turn that spawned it can end several ways before
/// reaching the join point (an error frame, a ghost-fallback turn with no
/// produced row to attach an image to, the whole stream being dropped by a
/// disconnected client, ...). None of those turns will ever emit an image
/// frame, so an in-flight compose call at that point has no consumer left —
/// letting it run to completion would just waste an LLM round trip. Wrapping
/// the handle in this guard means every current AND future early exit aborts
/// the task automatically, instead of requiring each one to remember to.
struct AbortOnDrop<T>(Option<tokio::task::JoinHandle<T>>);

impl<T> AbortOnDrop<T> {
    /// Await the guarded task WITHOUT releasing the guard. `JoinHandle` is
    /// `Unpin`, so a `&mut` await works — and because the handle never leaves
    /// the guard, a caller dropped mid-await (client disconnect at exactly the
    /// join point) still aborts the in-flight task. Taking the handle out
    /// first would open that window: between `take()` and the await's
    /// completion, a drop would detach the raw `JoinHandle` and the task would
    /// run on with no consumer. After a completed await the handle stays put;
    /// aborting a finished task is a no-op, so the drop-time abort is
    /// harmless.
    async fn join(&mut self) -> Option<Result<T, tokio::task::JoinError>> {
        match self.0.as_mut() {
            Some(h) => Some(h.await),
            None => None,
        }
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(h) = &self.0 {
            h.abort();
        }
    }
}

/// Resolve the per-turn image inputs, generate the subject via the composer
/// LLM, and wrap the result into the final wire prompt.
///
/// The composer is skipped entirely when it is not configured. It is fail-open
/// throughout: a model error, timeout, or empty reply degrades to an empty
/// subject — there is nothing left to fall back to — and never blocks the
/// image turn. `compose_image_prompt` turns that empty subject into a plain
/// persona-appearance portrait prompt (#212).
async fn build_delegated_image_prompt(
    state: &AppState,
    persona: &eros_engine_core::persona::CompanionPersona,
    plan: &eros_engine_core::types::ActionPlan,
    req_image: Option<&crate::routes::companion_stream::ImageReplyParams>,
    pde_transcript: &str,
    latest_user_msg: &str,
) -> DelegatedImagePrompt {
    let inputs = resolve_image_turn_inputs(plan, req_image);
    let style_str = serde_json::to_value(inputs.style)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "realistic".to_string());
    let variant = req_image.and_then(|i| i.prompt_variant.as_deref());
    let resolved_compose = state.model_config.resolve_image_prompt_compose(variant);
    let composer_configured = resolved_compose.is_some();
    let compose = match resolved_compose {
        Some(c) => {
            run_image_prompt_compose(
                state,
                &c,
                persona,
                pde_transcript,
                latest_user_msg,
                inputs.aspect_ratio.as_deref(),
                &style_str,
            )
            .await
        }
        None => None,
    };
    let (final_subject, caption, compose_variant, compose_model, compose_generation_id) =
        match compose {
            Some(o) => (
                o.prompt,
                o.caption,
                o.variant,
                Some(o.model),
                o.generation_id,
            ),
            None => {
                // Loud on purpose: this is the ONE path where the capability
                // gate (`本轮可发图=否` when the composer isn't configured) is
                // bypassed — a forced image turn (`image.force = true`) always
                // reaches here regardless of gate state. A deployment that
                // upgraded past #212 without configuring
                // `[tasks.chat_image_prompt_compose]` would otherwise silently
                // get a generic persona-portrait prompt on every forced image,
                // with zero log lines anywhere else on this path. Still a warn,
                // not an error: the portrait fallback is the sanctioned
                // fail-open degradation, not a bug.
                tracing::warn!(
                    composer_configured,
                    "image-compose: no subject produced this turn; falling back to a generic \
                     persona-portrait prompt ({})",
                    if composer_configured {
                        "composer chain failed — see the preceding image-compose warn for the \
                         model and reason"
                    } else {
                        "no [tasks.chat_image_prompt_compose] configured"
                    }
                );
                (String::new(), None, None, None, None)
            }
        };
    let composed_prompt =
        crate::pipeline::handlers::compose_image_prompt(inputs.style, persona, &final_subject);
    DelegatedImagePrompt {
        subject: final_subject,
        caption,
        aspect_ratio: inputs.aspect_ratio,
        composed_prompt,
        compose_variant,
        compose_model,
        compose_generation_id,
    }
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
            ghost_fallback: false,
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
            ghost_fallback: false,
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
    /// Where this turn's main history is anchored (resolved from the request's
    /// `reply_to_message_id`). `Latest` for ordinary turns.
    pub history_anchor: eros_engine_core::types::HistoryAnchor,
}

/// Produce a stream of `ProtocolFrame` events for a single burst. The
/// generator owns its `AppState` clone so it stays `'static` and survives
/// `Sse`'s body lifetime. Task 10 implements the Ghost branch; T11/T12
/// fill in Reply.
pub fn run_stream(
    state: Arc<AppState>,
    user_msg: PersistedUserMessage,
    prefetched_persona: Option<eros_engine_core::persona::CompanionPersona>,
) -> impl futures_util::Stream<Item = ProtocolFrame> + Send + 'static {
    async_stream::stream! {
        let chat_repo = ChatRepo { pool: &state.pool };
        let persona_repo = PersonaRepo { pool: &state.pool };
        let affinity_repo = AffinityRepo { pool: &state.pool };

        // Reuse the persona the entry handler already loaded for its
        // existence/active check, so a turn hits `load_companion` once, not
        // twice. Fall back to a DB load only when no prefetch was threaded
        // through — the direct-`run_stream` test paths pass `None`.
        let persona = match prefetched_persona {
            Some(p) => p,
            None => match persona_repo.load_companion(user_msg.instance_id).await {
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
            },
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
                history_anchor: user_msg.history_anchor,
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
        // Delegate-only: the chat stream never draws, so image-action
        // availability keys on the PRESENCE of the request `image` block (the
        // consumer signalling "I handle images this turn"). The engine holds
        // no image configuration at all. Image capability = that PLUS a
        // composer configured to write the prompt (#212).
        let req_image = user_msg.image.as_ref();
        let image_executor_available =
            image_capability_available(req_image.is_some(), &state.model_config);
        let force_image = req_image.is_some_and(|i| i.force) && !is_tip;
        // Skip resolution on tip turns: the judge won't run, and resolve_pde()
        // advances the round-robin model cursor as a side effect — resolving on a
        // skipped turn would skew which model later (non-tip) judge calls pick.
        let resolved_pde = if is_tip {
            None
        } else {
            state.model_config.resolve_pde()
        };
        // product_qa executor: hard-gated on the judge being live (spec §1.1).
        // `resolve_product_qa()` advances the chat_product_qa round-robin model
        // cursor as a side effect (like `resolve_pde()` above) — every judged
        // turn would consume a cursor position even when the action taken is
        // ordinary chat, skewing the model sequence actual product-QA
        // executions see. Use the side-effect-free `product_qa_enabled()` here
        // for availability; the executor itself is resolved only in the
        // ProductQa arm below, where the action is actually taken.
        let product_qa_available = resolved_pde.is_some() && state.model_config.product_qa_enabled();
        // One fetch per enabled turn, reused by judge ctx AND the executor arm.
        let product_qa_pairs: Vec<(String, String)> = if product_qa_available {
            chat_repo
                .recent_product_qa_pairs(user_msg.session_id, user_msg.user_message_id, 3)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!("stream: recent_product_qa_pairs failed: {e}");
                    Vec::new()
                })
        } else {
            Vec::new()
        };
        let product_qa_recent: Option<String> =
            product_qa_available.then(|| render_product_qa_pairs(&product_qa_pairs));
        // Shared history transcript: built once, reused by the judge here, the
        // input filter below (which previously fetched its own), AND the image
        // composer's `[最近场景]`. Fetched whenever any of them can need it this
        // turn: a judge turn (`resolved_pde` — already None on tips), or a
        // forced image turn. The composer generates from the recent scene now
        // that no seed exists, so a forced image with the judge disabled must
        // not lose that context; rule-based `pde::decide` never picks image
        // actions, so `force_image` is the only judge-less image path.
        let pde_transcript: JudgeTranscript = if resolved_pde.is_some() || force_image {
            build_input_filter_transcript(&chat_repo, user_msg.session_id, user_msg.user_message_id).await
        } else {
            JudgeTranscript::default()
        };
        let mut killswitch_hints: Vec<String> = Vec::new();
        let (mut plan, pde_run): (eros_engine_core::types::ActionPlan, Option<PdeDecisionRun>) =
            match (is_tip, resolved_pde.as_ref()) {
                (false, Some(p)) => {
                    let ctx = build_pde_ctx(
                        &pde_transcript,
                        &input,
                        image_executor_available,
                        product_qa_recent.as_deref(),
                    );
                    let run = run_pde_decision(&state.openrouter, p, &ctx).await;
                    let plan = match (&run.status, &run.verdict) {
                        (PdeStatus::Ok, Some(v)) => {
                            let action = guard_action(
                                v.action,
                                &input.affinity,
                                &input.signals,
                                image_executor_available,
                                product_qa_available,
                            );
                            let hints = {
                                let s = sanitize_inner_state(&v.inner_state);
                                if s.is_empty() { Vec::new() } else { vec![s] }
                            };
                            killswitch_hints = hints.clone();
                            // Same sanitizer, same discipline: judge-authored
                            // prose never carries section markers into the prompt.
                            let tone = v
                                .tone
                                .as_deref()
                                .map(sanitize_inner_state)
                                .filter(|s| !s.is_empty());
                            // Only image actions carry the judge's image_ref /
                            // aspect_ratio; `v` is still borrowed here (the
                            // run/verdict is moved into the audit task below).
                            let is_image = matches!(
                                action,
                                ActionType::ReplyImage | ActionType::ReplyTextImage
                            );
                            let img_ref = if is_image {
                                v.image_ref
                            } else {
                                eros_engine_core::types::ImageRef::Face
                            };
                            let img_aspect = if is_image { v.aspect_ratio.clone() } else { None };
                            pde::plan_for(&input, action, hints, tone, img_ref, img_aspect)
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
        // Always ReplyImage: image only, no text reply (spec 2026-08-03 §1) —
        // the ReplyImage/ReplyTextImage split belongs to the judge, which the
        // consumer has overridden by forcing. There is no subject to carry
        // through here — the composer decides what the picture shows from turn
        // context alone (`resolve_image_turn_inputs`).
        if force_image {
            plan = pde::plan_for(
                &input,
                ActionType::ReplyImage,
                plan.context_hints.clone(),
                plan.reply_tone.clone(),
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
            // Snapshot of the affinity state the judge's payload was built
            // from (issue #254). The prompt carries only the buckets; this row
            // keeps the numbers.
            let inputs = Some(pde_inputs_snapshot(&input.affinity));
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
                        inputs,
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
                    ghost_fallback: false,
                };
                let final_frame = build_final_frame(false, None, user_msg.tier.clone(), 0, 0);
                yield final_frame;
            }
            ActionType::ProductQa => {
                // Out-of-character product answer (spec §1.4): mark the user row,
                // run the dedicated executor, persist with channel='product_qa'.
                // Skips the entire companion chain — no vision, no input filter, no
                // persona prompt, no output filter, no post_process.
                let p = state
                    .model_config
                    .resolve_product_qa()
                    .expect("guard passed ⇒ chat_product_qa resolvable");
                if let Err(e) = chat_repo
                    .mark_user_message_product_qa(user_msg.user_message_id)
                    .await
                {
                    tracing::warn!("stream: product_qa mark failed: {e}");
                }

                let mid = Ulid::new();
                let message_id = ulid_string(mid);
                let assistant_uuid: Uuid = mid.into();
                yield ProtocolFrame::Meta {
                    message_id: message_id.clone(),
                    action_type: FrameActionType::ProductQa,
                    model: None,
                    continues_from: None,
                };

                // Executor payload: recent product-QA pairs (shared fetch) + question.
                let question = match &input.event {
                    eros_engine_core::types::Event::UserMessage { content, .. } => content.clone(),
                    _ => String::new(),
                };
                let recent = product_qa_recent.as_deref().unwrap_or("");
                let user_payload = if recent.is_empty() {
                    format!("[用户提问]\n{question}")
                } else {
                    format!("[最近产品咨询]\n{recent}\n\n[用户提问]\n{question}")
                };
                let messages = vec![
                    eros_engine_llm::openrouter::ChatMessage {
                        role: "system".into(),
                        content: p.answer_prompt.clone(),
                    },
                    eros_engine_llm::openrouter::ChatMessage {
                        role: "user".into(),
                        content: user_payload,
                    },
                ];

                // Candidate chain walk + streaming — mirrors voice.rs:137-206.
                let mut candidates = Vec::with_capacity(1 + p.fallback_model.len());
                candidates.push(p.model.clone());
                candidates.extend(p.fallback_model.iter().cloned());
                let mut acc = String::new();
                let mut last_usage: Option<eros_engine_llm::openrouter::UsageBlock> = None;
                let mut last_gen_id: Option<String> = None;
                let mut served_model: Option<String> = None;
                let mut truncated = false;
                // Built once; only the served model differs per candidate, so
                // execute_stream_as borrows it (no per-candidate prompt clone).
                let qa_req = eros_engine_llm::openrouter::ChatRequest {
                    messages,
                    temperature: p.temperature as f32,
                    max_tokens: p.max_tokens,
                    sampling: p.sampling,
                    reasoning: p.reasoning.clone(),
                    task: Some("chat_product_qa".into()),
                    ..Default::default()
                };
                'candidates: for model_id in candidates {
                    last_usage = None;
                    last_gen_id = None;
                    served_model = None;
                    truncated = false;
                    let stream = match tokio::time::timeout(
                        STREAM_OPEN_TIMEOUT,
                        state.openrouter.execute_stream_as(&qa_req, &model_id),
                    )
                    .await
                    {
                        Ok(Ok(s)) => s,
                        Ok(Err(e)) => {
                            tracing::warn!(model = %model_id, error = %e, "product_qa: open stream failed");
                            if acc.is_empty() { continue 'candidates; }
                            truncated = true;
                            break 'candidates;
                        }
                        Err(_) => {
                            tracing::warn!(model = %model_id, "product_qa: open timeout");
                            if acc.is_empty() { continue 'candidates; }
                            truncated = true;
                            break 'candidates;
                        }
                    };
                    futures_util::pin_mut!(stream);
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
                                tracing::warn!(model = %model_id, "product_qa: total timeout");
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
                                    yield ProtocolFrame::Delta {
                                        message_id: message_id.clone(),
                                        content: text,
                                    };
                                }
                                if matches!(chunk.finish_reason.as_deref(), Some("length") | Some("content_filter")) { truncated = true; }
                            }
                            Some(Err(e)) => {
                                tracing::warn!(model = %model_id, error = %e, "product_qa: mid-stream error");
                                if acc.is_empty() { continue 'candidates; }
                                truncated = true;
                                break 'candidates;
                            }
                            None => {
                                if acc.is_empty() { continue 'candidates; }
                                break 'candidates;
                            }
                        }
                    }
                }

                // Chain exhausted with nothing streamed: error_handling fallback
                // phrase, persisted WITH the channel marker so replay/idempotency
                // hold (spec §4). Never degrade to the companion reply path — the
                // companion doesn't know the product facts.
                if acc.is_empty() {
                    let phrase = ErrorHandlingRepo { pool: &state.pool }
                        .pick_chat_stream_fallback_phrase()
                        .await
                        .ok()
                        .flatten();
                    match phrase {
                        Some(text) => {
                            acc = text.clone();
                            truncated = false;
                            // A final candidate can reach here having streamed
                            // metadata (usage/model/generation_id) with zero
                            // content — e.g. a terminal SSE chunk that reports
                            // usage but no delta. That trio belongs to a call
                            // that produced nothing; leaving it set would plant
                            // a real generation_id/model/usage on a row whose
                            // content is actually this canned phrase, poisoning
                            // OpenRouter-log reconciliation (audit attribution
                            // noise). Reset before persistence — this is the
                            // ONLY branch reached with `acc` non-empty despite
                            // no candidate having produced it.
                            last_usage = None;
                            last_gen_id = None;
                            served_model = None;
                            yield ProtocolFrame::Delta {
                                message_id: message_id.clone(),
                                content: text,
                            };
                        }
                        None => {
                            // No phrase configured: same terminal shape as the voice
                            // path's all-candidates failure. (Parity note: like a
                            // normal chat failure, retry of this client_msg_id will
                            // 409 until a row exists.)
                            yield ProtocolFrame::Error {
                                code: StreamErrorCode::UpstreamUnavailable,
                                retryable: true,
                                message: "product_qa generation failed on all candidates".into(),
                                user_message: "服务暂时不可用，请稍后再试".into(),
                            };
                            return;
                        }
                    }
                }

                let usage_full = last_usage.as_ref().and_then(|u| serde_json::to_value(u).ok());
                if let Err(e) = chat_repo
                    .insert_product_qa_assistant_message(
                        user_msg.session_id,
                        user_msg.user_message_id,
                        assistant_uuid,
                        &acc,
                        served_model.as_deref(),
                        usage_full.as_ref(),
                        last_gen_id.as_deref(),
                        truncated,
                    )
                    .await
                {
                    tracing::warn!("stream: product_qa persist failed: {e}");
                }
                super::log_openrouter_usage(
                    "chat_product_qa",
                    Some(user_msg.session_id),
                    &eros_engine_llm::openrouter::ChatResponse {
                        reply: String::new(), // usage log only — never echo content
                        generation_id: last_gen_id.clone(),
                        model: served_model.clone(),
                        usage: usage_full.clone(),
                        finish_reason: None,
                    },
                );

                let mut usage_wire = usage_full;
                filter_usage_keys(&mut usage_wire, &state.config.openrouter_usage_hidden_keys);
                yield ProtocolFrame::Done {
                    message_id,
                    truncated,
                    usage: usage_wire,
                    generation_id: last_gen_id,
                    ghost_fallback: false,
                };
                let final_frame = build_final_frame(false, None, user_msg.tier.clone(), 0, 0);
                yield final_frame;
            }
            ActionType::ReplyText | ActionType::ReplyImage | ActionType::ReplyTextImage => {
                // ── Image-reply wiring (delegate-only) ────────────────────────
                // `resolved_image_gen` / `req_image` were resolved in the decision
                // block above and are REUSED here. The chat stream never draws:
                // for ReplyImage we compose the prompt, persist the minimal marker,
                // emit `meta → done → image_request`, and skip the text path
                // entirely; for ReplyTextImage the text reply runs as usual and a
                // single `image_request` is appended after the text `done`. Persona
                // comes from `input.persona` (the local `persona` binding was moved
                // into `input` above).
                let mut image_only_done = false;
                let mut image_only_produced: Vec<crate::pipeline::post_process::ProducedMessage> =
                    Vec::new();
                let mut image_only_caption: Option<String> = None;

                if matches!(plan.action_type, ActionType::ReplyImage) {
                    // Delegate-only: compose the prompt and emit `image_request`;
                    // the engine never draws. Pre-allocate the assistant id so
                    // the persisted row and the delegated frames share it.
                    let msg_ulid = Ulid::new();
                    let msg_uuid: Uuid = msg_ulid.into();
                    let img_mid = ulid_string(msg_ulid);
                    let img = build_delegated_image_prompt(
                        &state,
                        &input.persona,
                        &plan,
                        req_image,
                        &pde_transcript.transcript,
                        &user_msg.content,
                    )
                    .await;
                    let subject = img.subject;
                    let aspect = img.aspect_ratio;
                    let composed_prompt = img.composed_prompt;
                    // Persist the marker (subject + caption + aspect + compose
                    // audit trio on success) so the PDE stays image-aware
                    // (§5); the composed prompt and the draw result live with
                    // the consumer.
                    let marker = build_delegated_image_marker(
                        &subject,
                        img.caption.as_deref(),
                        aspect.as_deref(),
                        img.compose_variant.as_deref(),
                        img.compose_model.as_deref(),
                        img.compose_generation_id.as_deref(),
                    );
                    image_only_caption = img.caption.clone();
                    let row = eros_engine_store::chat::AssistantInsert {
                        id: msg_uuid,
                        content: String::new(),
                        assistant_action_type: "reply".into(),
                        continues_from_message_id: None,
                        truncated: false,
                        model: None,
                        usage: None,
                        generation_id: None,
                        filter_audit: None,
                        metadata: Some(serde_json::json!({ "image": marker })),
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
                    // full_text="" so insight/memory extraction skips this row;
                    // affinity uses plan.image_caption (set below, from the
                    // picture's caption) as the proxy.
                    image_only_produced.push(crate::pipeline::post_process::ProducedMessage {
                        message_id: msg_uuid,
                        full_text: String::new(),
                        action: ActionType::ReplyImage,
                    });
                    for frame in delegated_image_only_frames(
                        img_mid.clone(),
                        &composed_prompt,
                        plan.image_ref,
                        aspect.as_deref(),
                    ) {
                        yield frame;
                    }
                    image_only_done = true;
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
                    let final_frame =
                        build_final_frame(false, None, user_msg.tier.clone(), 0, 0);
                    yield final_frame;

                    let state_bg = (*state).clone();
                    let mut plan_bg = plan.clone();
                    plan_bg.image_caption = image_only_caption;
                    let event_bg = Event::UserMessage {
                        content: user_msg.content.clone(),
                        message_id: user_msg.user_message_id,
                        prompt_traits: user_msg.prompt_traits.clone(),
                        audit: user_msg.audit.clone(),
                        tier: user_msg.tier.clone(),
                        memory_scope: user_msg.memory_scope,
                        affinity_scope: user_msg.affinity_scope,
                        tips_amount_usd: user_msg.tips_amount_usd,
                        history_anchor: user_msg.history_anchor,
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
                //
                // The text the model will actually see this turn. The input
                // filter persists its rewrite and `build_reply_request` re-reads
                // it from the DB; the composer runs concurrently and must not
                // pay a second read, so track it locally. Keeping the composer
                // on the SAME text as the chat model is what stops the picture
                // drifting from the reply.
                let mut effective_user_msg = user_msg.content.clone();
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
                        let transcript = if !pde_transcript.transcript.is_empty() {
                            pde_transcript.transcript.clone()
                        } else {
                            build_input_filter_transcript(
                                &chat_repo,
                                user_msg.session_id,
                                user_msg.user_message_id,
                            )
                            .await
                            .transcript
                        };
                        if let Some(rw) =
                            run_input_filter(&state, &f, &transcript, &user_msg.content).await
                        {
                            match chat_repo
                                .set_user_input_rewrite(
                                    user_msg.user_message_id,
                                    &rw.rewritten_text,
                                    &rw.filter_model,
                                    rw.reason.as_deref(),
                                    rw.f_generation_id.as_deref(),
                                )
                                .await
                            {
                                // The chat model reads the effective text back
                                // from the DB row (build_reply_request), so the
                                // composer may track the rewrite only once it is
                                // persisted — on a failed write the chat model
                                // will see the ORIGINAL text, and handing the
                                // composer the unpersisted rewrite would let the
                                // picture drift from the reply.
                                Ok(()) => effective_user_msg = rw.rewritten_text.clone(),
                                Err(e) => {
                                    tracing::warn!(
                                        "stream: input-filter rewrite persist failed: {e}"
                                    );
                                }
                            }
                        }
                    }
                }
                // ── Concurrent image composition (reply_text_image only) ──────
                // Fired here, after the input filter (so the composer sees the
                // same text as the chat model) and before build_reply_request —
                // which alone does a 20-row history fetch, an embedding call and
                // memory/world recall before the chat stream even starts. One
                // short composer call hides completely underneath, taking the
                // turn's serial LLM hops from 3 to 2.
                //
                // `reply_image` is deliberately excluded: it has no text task to
                // overlap with and returns early via `image_only_done`.
                let mut compose_handle: AbortOnDrop<DelegatedImagePrompt> =
                    if matches!(plan.action_type, ActionType::ReplyTextImage) {
                        let state_c = (*state).clone();
                        let persona_c = input.persona.clone();
                        let plan_c = plan.clone();
                        let req_image_c = req_image.cloned();
                        let scene_c = pde_transcript.transcript.clone();
                        let latest_c = effective_user_msg.clone();
                        AbortOnDrop(Some(tokio::spawn(async move {
                            build_delegated_image_prompt(
                                &state_c,
                                &persona_c,
                                &plan_c,
                                req_image_c.as_ref(),
                                &scene_c,
                                &latest_c,
                            )
                            .await
                        })))
                    } else {
                        AbortOnDrop(None)
                    };
                let req_res = crate::pipeline::handlers::build_reply_request(
                    &state, &input, &plan,
                    user_msg.session_id, user_msg.user_id, user_msg.instance_id,
                    user_msg.user_message_id,
                ).await;
                let (req, injected_tags) = match req_res {
                    Ok(r) => r,
                    Err(e) => {
                        // Dropping `compose_handle` here (via the enclosing
                        // `return` below) aborts the spawned compose task
                        // through `AbortOnDrop`'s `Drop` impl — no manual
                        // `.abort()` needed, and every OTHER early exit
                        // between here and the join point (chat-burst error,
                        // ghost fallback with no produced row, client
                        // disconnect) gets the same coverage for free.
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
                let (produced, did_filter, retries_chat, retries_filter, ghost_fallback) = {
                    let g = outcome.lock().unwrap();
                    (g.produced.clone(), g.filtered, g.retries_chat, g.retries_filter, g.ghost_fallback)
                };

                // Reset ghost streak (mirrors sync pipeline policy). A ghost
                // fallback (empty served reply) is affinity-neutral — do NOT
                // reset, per the design's "既不加也不清零".
                if !ghost_fallback {
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
                }

                // ── ReplyTextImage: append the generated image AFTER the text ──
                // The text reply has already streamed (meta → delta* → done). Now
                // generate the image, merge metadata.image onto the LAST produced
                // assistant row, and yield the Image frame BEFORE `final`. Frame
                // order: meta → delta* → done → image → final. On image failure
                // (or zero images / empty produced) we emit NO Image frame — the
                // text reply already reached the client, so the turn is complete.
                let mut image_caption: Option<String> = None;
                if matches!(plan.action_type, ActionType::ReplyTextImage) {
                    if let Some(last) = produced.last() {
                        let msg_uuid = last.message_id;
                        let img_mid = ulid_string(Ulid::from(msg_uuid));
                        // The composer was spawned before the chat call; by now
                        // it has almost always finished, so this await is ~free.
                        // Awaited THROUGH the guard so a client disconnect at
                        // this exact point still aborts the in-flight call. A
                        // panicked or cancelled task degrades exactly like a
                        // failed compose — never a dropped frame.
                        let img = match compose_handle.join().await {
                            Some(Ok(v)) => v,
                            Some(Err(e)) => {
                                tracing::warn!("image-compose task failed: {e}");
                                build_delegated_image_prompt(
                                    &state,
                                    &input.persona,
                                    &plan,
                                    req_image,
                                    &pde_transcript.transcript,
                                    &effective_user_msg,
                                )
                                .await
                            }
                            None => {
                                build_delegated_image_prompt(
                                    &state,
                                    &input.persona,
                                    &plan,
                                    req_image,
                                    &pde_transcript.transcript,
                                    &effective_user_msg,
                                )
                                .await
                            }
                        };
                        let subject = img.subject;
                        let aspect = img.aspect_ratio;
                        let composed_prompt = img.composed_prompt;
                        image_caption = img.caption.clone();
                        // Merge the marker (subject + caption + aspect + compose
                        // audit trio on success) onto the already-persisted
                        // text row so the PDE stays image-aware (§5). The text
                        // already reached the client; `final` follows below.
                        let marker = build_delegated_image_marker(
                            &subject,
                            img.caption.as_deref(),
                            aspect.as_deref(),
                            img.compose_variant.as_deref(),
                            img.compose_model.as_deref(),
                            img.compose_generation_id.as_deref(),
                        );
                        if let Err(e) = chat_repo
                            .merge_assistant_image_meta(user_msg.session_id, msg_uuid, &marker)
                            .await
                        {
                            tracing::warn!("stream(text_image): merge marker failed: {e}");
                        }
                        yield build_image_request_frame(
                            img_mid.clone(),
                            &composed_prompt,
                            plan.image_ref,
                            aspect.as_deref(),
                        );
                    }
                }

                let final_frame = build_final_frame(
                    did_filter,
                    prompt_injected.clone(),
                    user_msg.tier.clone(),
                    retries_chat,
                    retries_filter,
                );
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
                plan_bg.image_caption = image_caption;
                let event_bg = Event::UserMessage {
                    content: user_msg.content.clone(),
                    message_id: user_msg.user_message_id,
                    prompt_traits: user_msg.prompt_traits.clone(),
                    audit: user_msg.audit.clone(),
                    tier: user_msg.tier.clone(),
                    memory_scope: user_msg.memory_scope,
                    affinity_scope: user_msg.affinity_scope,
                    tips_amount_usd: user_msg.tips_amount_usd,
                    history_anchor: user_msg.history_anchor,
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
                let final_frame = build_final_frame(false, None, user_msg.tier.clone(), 0, 0);
                yield final_frame;
            }
        }
    }
}

/// Build the spec's `final` frame. Assembled purely from turn-local values
/// since the lead/CTA teardown (spec 2026-08-11) — no DB reads.
fn build_final_frame(
    filtered: bool,
    prompt_injected: Option<Vec<String>>,
    tier: Option<String>,
    retries_chat: u32,
    retries_filter: u32,
) -> ProtocolFrame {
    ProtocolFrame::Final {
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
/// `final` assembled from turn-local constants (no DB reads — see
/// `build_final_frame`). Ghost replay emits a synthetic Meta+Done(no usage,
/// not truncated) followed by Final.
pub fn replay_stream(
    state: Arc<AppState>,
    // Unused since the lead/CTA teardown (spec 2026-08-11): `build_final_frame`
    // no longer needs session/user state. Kept in the signature — every call
    // site already has both values in scope — rather than churning every caller.
    _session_id: Uuid,
    _user_id: Uuid,
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
                ghost_fallback: false,
            };
        } else {
            for row in &rows {
                let msg_ulid = Ulid::from(row.id);
                let prev_ulid = row.continues_from_message_id.map(Ulid::from);
                let action = if row.channel.as_deref() == Some("product_qa") {
                    FrameActionType::ProductQa
                } else {
                    FrameActionType::Reply
                };
                yield ProtocolFrame::Meta {
                    message_id: ulid_string(msg_ulid),
                    action_type: action,
                    // When the persisted row carries no model (e.g. the
                    // pseudo-ghost fallback path), the live stream emitted
                    // model: None — preserve that on replay so idempotent
                    // retries are wire-identical regardless of any
                    // display_override config.
                    model: row.model.as_deref().and_then(|m| {
                        display_override
                            .as_ref()
                            .and_then(|d| d.display(&eros_engine_llm::provider::bare_model_id(m)))
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
                    // Re-emit the ghost-fallback flag so an idempotent replay of an
                    // empty-reply fallback turn is wire-identical to the original
                    // live stream (a real ghost likewise re-emits its ghost frames
                    // on replay). Match ONLY the ghost-fallback reasons — the
                    // pseudo-ghost ("stream_failure") and garble-repaired
                    // ("garble_repaired") rows also carry a fallback_reason but are
                    // non-empty canned/ salvaged replies, not ghosts.
                    ghost_fallback: matches!(
                        row.metadata
                            .as_ref()
                            .and_then(|m| m.get("fallback_reason"))
                            .and_then(|v| v.as_str()),
                        Some("regex_strip") | Some("empty_completion")
                    ),
                };
            }
            // If every persisted assistant row was truncated, emit the same
            // terminal Error that the original burst emitted so the client
            // knows retrying is appropriate. This is companion multi-candidate
            // chain semantics (every fallback model exhausted, all truncated).
            // A product-QA turn persists exactly one assistant row, and a
            // truncated product-QA answer is still a served answer — the live
            // burst emits Meta → Delta → Done(truncated:true) → Final, no
            // Error — so exclude product-QA chains from this rule (chains
            // never mix companion and product_qa rows under one
            // user_message_id).
            let product_qa_chain = rows
                .iter()
                .any(|r| r.channel.as_deref() == Some("product_qa"));
            if !rows.is_empty() && !product_qa_chain && rows.iter().all(|r| r.truncated) {
                yield ProtocolFrame::Error {
                    code: StreamErrorCode::UpstreamUnavailable,
                    retryable: true,
                    message: "all fallback models truncated (replayed)".into(),
                    user_message: "AI 服务暂时不可用，稍后再试".into(),
                };
                return;
            }
        }
        let final_frame = build_final_frame(false, None, None, 0, 0);
        yield final_frame;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_err_outcome_separates_error_frame_and_idle_timeout() {
        use eros_engine_llm::LlmError;
        // Provider error frames (a 200 SSE frame carrying `error`) are their
        // own failure mode, distinct from transport drops (spec §4.2).
        assert_eq!(
            chunk_err_outcome(&LlmError::Provider(
                "openrouter mid-stream error: code=Some(502): upstream".into()
            )),
            "error_frame"
        );
        // The byte-level idle watchdog's io::Error rides through
        // eventsource-stream's `Transport error: {inner}` Display into
        // LlmError::Stream; the shared marker constant is the contract.
        let idle = LlmError::Stream(format!(
            "Transport error: {}: no bytes for 45s",
            eros_engine_llm::openrouter::STREAM_IDLE_TIMEOUT_MSG
        ));
        assert_eq!(chunk_err_outcome(&idle), "idle_timeout");
        // Everything else stays the generic transport/parse label.
        assert_eq!(
            chunk_err_outcome(&LlmError::Stream(
                "Transport error: connection reset by peer".into()
            )),
            "chunk_error"
        );
        assert_eq!(
            chunk_err_outcome(&LlmError::StreamParse("not a delta".into())),
            "chunk_error"
        );
    }

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
    fn compose_user_payload_includes_all_parts() {
        let p = compose_user_payload(
            "freckled, red hair",
            "（无）",
            "（无）",
            "realistic",
            "9:16",
        );
        assert!(p.contains("freckled, red hair"));
        assert!(p.contains("（无）"));
        assert!(p.contains("realistic"));
        assert!(p.contains("9:16"));
    }

    #[test]
    fn compose_user_payload_includes_latest_user_message() {
        let p = compose_user_payload(
            "freckled, red hair",
            "（无）",
            "给我看看你现在的样子",
            "realistic",
            "9:16",
        );
        assert!(p.contains("[对方最新消息]\n给我看看你现在的样子"), "{p}");
    }

    #[test]
    fn compose_user_payload_has_no_seed_section() {
        let p = compose_user_payload(
            "freckled, red hair",
            "（无）",
            "给我看看你现在的样子",
            "realistic",
            "9:16",
        );
        assert!(
            !p.contains("画面主题种子"),
            "the seed concept is deleted — no seed section may render: {p}"
        );
        assert!(p.contains("[对方最新消息]\n给我看看你现在的样子"), "{p}");
        assert!(p.contains("[人物外观]\nfreckled, red hair"), "{p}");
        assert!(p.contains("[风格]\nrealistic"), "{p}");
        assert!(p.contains("[画幅]\n9:16"), "{p}");
    }

    #[test]
    fn parse_compose_reply_reads_direct_json() {
        let (p, c) =
            parse_compose_reply(r#"{"prompt":"on a rooftop at dusk","caption":"在天台看夕阳"}"#);
        assert_eq!(p, "on a rooftop at dusk");
        assert_eq!(c.as_deref(), Some("在天台看夕阳"));
    }

    #[test]
    fn parse_compose_reply_salvages_json_in_prose() {
        let raw = "Sure! Here you go:\n{\"prompt\":\"a selfie in a cafe\",\"caption\":\"咖啡店自拍\"}\nHope that helps.";
        let (p, c) = parse_compose_reply(raw);
        assert_eq!(p, "a selfie in a cafe");
        assert_eq!(c.as_deref(), Some("咖啡店自拍"));
    }

    #[test]
    fn parse_compose_reply_plain_text_becomes_prompt_with_no_caption() {
        // Migration fallback: an EXPAND-era composer prompt still yields a
        // working image prompt, just no caption.
        let (p, c) = parse_compose_reply("  a windswept portrait on the cliffs  ");
        assert_eq!(p, "a windswept portrait on the cliffs");
        assert_eq!(c, None);
    }

    #[test]
    fn parse_compose_reply_blank_caption_is_none() {
        let (p, c) = parse_compose_reply(r#"{"prompt":"x","caption":"   "}"#);
        assert_eq!(p, "x");
        assert_eq!(c, None, "a blank caption is absent, not empty-string");
    }

    #[test]
    fn delegated_image_marker_carries_caption_when_present() {
        let m = build_delegated_image_marker(
            "on a rooftop",
            Some("在天台"),
            Some("3:4"),
            None,
            None,
            None,
        );
        assert_eq!(m["prompt"], "on a rooftop");
        assert_eq!(m["caption"], "在天台");
        assert_eq!(m["aspect_ratio"], "3:4");
    }

    #[test]
    fn delegated_image_marker_omits_absent_caption() {
        let m = build_delegated_image_marker("on a rooftop", None, None, None, None, None);
        assert_eq!(m["prompt"], "on a rooftop");
        assert!(
            m.get("caption").is_none(),
            "absent caption must not be written: {m}"
        );
    }

    // ─── resolve_image_turn_inputs ───────────────────────────────────────

    fn img_plan(aspect: Option<&str>) -> eros_engine_core::types::ActionPlan {
        eros_engine_core::types::ActionPlan {
            action_type: ActionType::ReplyImage,
            reply_style: eros_engine_core::types::ReplyStyle::Neutral,
            affinity_deltas: Default::default(),
            energy_cost: 0.0,
            context_hints: vec![],
            reply_tone: None,
            image_caption: None,
            image_ref: eros_engine_core::types::ImageRef::Face,
            aspect_ratio: aspect.map(str::to_string),
        }
    }

    fn img_params(
        style: Option<eros_engine_llm::model_config::StyleKey>,
        aspect: Option<&str>,
    ) -> crate::routes::companion_stream::ImageReplyParams {
        crate::routes::companion_stream::ImageReplyParams {
            style,
            aspect_ratio: aspect.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn image_turn_style_prefers_request_then_default() {
        use eros_engine_llm::model_config::StyleKey;

        let params = img_params(Some(StyleKey::SemiRealistic), None);
        let r = resolve_image_turn_inputs(&img_plan(None), Some(&params));
        assert_eq!(r.style, StyleKey::SemiRealistic, "request wins");

        let r = resolve_image_turn_inputs(&img_plan(None), None);
        assert_eq!(r.style, StyleKey::default(), "no request ⇒ type default");
    }

    #[test]
    fn image_turn_aspect_prefers_plan_then_request_then_none() {
        let params = img_params(None, Some("16:9"));

        let r = resolve_image_turn_inputs(&img_plan(Some("3:4")), Some(&params));
        assert_eq!(r.aspect_ratio.as_deref(), Some("3:4"), "plan wins");

        let r = resolve_image_turn_inputs(&img_plan(Some("  ")), Some(&params));
        assert_eq!(
            r.aspect_ratio.as_deref(),
            Some("16:9"),
            "blank plan ⇒ request"
        );

        let r = resolve_image_turn_inputs(&img_plan(None), None);
        assert_eq!(r.aspect_ratio, None, "nothing anywhere ⇒ None");

        // Blank request value ⇒ the request-level blank filter still fires.
        let blank_params = img_params(None, Some("  "));
        let r = resolve_image_turn_inputs(&img_plan(None), Some(&blank_params));
        assert_eq!(r.aspect_ratio, None, "blank request ⇒ None");
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
            ghost_fallback: false,
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
            ghost_fallback: false,
        };
        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        // Spec §1.5 done schema permits `usage: null` — do NOT omit.
        assert!(v.get("usage").is_some());
        assert!(v["usage"].is_null());
    }

    #[test]
    fn done_frame_omits_ghost_fallback_when_false() {
        let f = ProtocolFrame::Done {
            message_id: "m".into(),
            truncated: false,
            usage: None,
            generation_id: None,
            ghost_fallback: false,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(!s.contains("ghost_fallback"), "false must be omitted: {s}");
    }

    #[test]
    fn done_frame_serializes_ghost_fallback_when_true() {
        let f = ProtocolFrame::Done {
            message_id: "m".into(),
            truncated: false,
            usage: None,
            generation_id: None,
            ghost_fallback: true,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(
            s.contains("\"ghost_fallback\":true"),
            "true must serialize: {s}"
        );
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
    fn parse_pde_verdict_product_qa_action() {
        let v =
            parse_pde_verdict(r#"{"action":"product_qa","inner_state":"想介绍"}"#).expect("parses");
        assert_eq!(v.action, PdeAction::ProductQa);
        assert_eq!(PdeAction::ProductQa.as_str(), "product_qa");
    }

    #[test]
    fn parse_pde_verdict_image_ref_and_aspect() {
        // defaults when omitted (backward compat)
        let v = parse_pde_verdict("{\"action\":\"reply_image\",\"inner_state\":\"ok\"}").unwrap();
        assert_eq!(v.image_ref, eros_engine_core::types::ImageRef::Face);
        assert_eq!(v.aspect_ratio, None);

        // explicit values
        let j = "{\"action\":\"reply_image\",\"inner_state\":\"x\",\"image_ref\":\"previous\",\"aspect_ratio\":\"9:16\"}";
        let v = parse_pde_verdict(j).unwrap();
        assert_eq!(v.image_ref, eros_engine_core::types::ImageRef::Previous);
        assert_eq!(v.aspect_ratio.as_deref(), Some("9:16"));
    }

    #[test]
    fn parse_pde_verdict_tone_roundtrip() {
        // With tone.
        let v =
            parse_pde_verdict(r#"{"action":"reply_text","inner_state":"ok","tone":"敷衍一点"}"#)
                .unwrap();
        assert_eq!(v.tone.as_deref(), Some("敷衍一点"));
        // Without tone (old prompts) and explicit null (strict providers).
        let v = parse_pde_verdict(r#"{"action":"reply_text","inner_state":"ok"}"#).unwrap();
        assert_eq!(v.tone, None);
        let v =
            parse_pde_verdict(r#"{"action":"reply_text","inner_state":"ok","tone":null}"#).unwrap();
        assert_eq!(v.tone, None);
    }

    #[test]
    fn verdict_audit_serializes_tone_when_present() {
        let with: PdeVerdict =
            serde_json::from_str(r#"{"action":"ghost","inner_state":"想躲","tone":"冷淡"}"#)
                .unwrap();
        let j = serde_json::to_value(VerdictAudit::from(&with)).unwrap();
        assert_eq!(
            j["tone"], "冷淡",
            "audit records what the judge said even when the plan drops it (ghost)"
        );
        let without: PdeVerdict =
            serde_json::from_str(r#"{"action":"ghost","inner_state":"想躲"}"#).unwrap();
        let j = serde_json::to_value(VerdictAudit::from(&without)).unwrap();
        assert!(
            j.get("tone").is_none(),
            "absent tone is omitted from audit: {j}"
        );
    }

    #[test]
    fn verdict_audit_includes_image_ref_and_aspect() {
        let j = "{\"action\":\"reply_image\",\"inner_state\":\"x\",\"image_ref\":\"previous\",\"aspect_ratio\":\"3:4\"}";
        let v = parse_pde_verdict(j).unwrap();
        let payload = serde_json::to_value(VerdictAudit::from(&v)).unwrap();
        assert_eq!(payload["image_ref"], "previous");
        assert_eq!(payload["aspect_ratio"], "3:4");
    }

    #[test]
    fn assistant_transcript_line_marks_image_turns() {
        // image turn with a caption: the CAPTION surfaces, never the prompt
        let meta = serde_json::json!({"image":{
            "prompt":"Photorealistic, ultra-detailed, on the beach at sunset",
            "caption":"在沙滩看日落",
            "aspect_ratio":"3:4"
        }});
        let line = assistant_transcript_line("", Some(&meta));
        assert!(line.contains("在沙滩看日落"), "caption surfaced: {line}");
        assert!(
            !line.contains("Photorealistic"),
            "the image prompt must never reach the transcript: {line}"
        );
        assert!(line.contains("3:4"), "aspect surfaced: {line}");

        // image turn WITHOUT a caption: bare marker, never a prompt fallback
        let meta2 = serde_json::json!({"image":{"prompt":"a very long english image prompt"}});
        let line2 = assistant_transcript_line("", Some(&meta2));
        assert_eq!(
            line2, "（发送了一张图片）",
            "bare marker when caption absent: {line2}"
        );

        // blank caption counts as absent
        let meta3 = serde_json::json!({"image":{"prompt":"x","caption":"  "}});
        assert_eq!(
            assistant_transcript_line("", Some(&meta3)),
            "（发送了一张图片）"
        );

        // plain text turn: content passes through unchanged
        assert_eq!(assistant_transcript_line("hi there", None), "hi there");

        // metadata present but no image key: content passes through
        let meta4 = serde_json::json!({"tip": 5});
        assert_eq!(assistant_transcript_line("hello", Some(&meta4)), "hello");
    }

    /// Codex-review P2 regression (PR #216): the current turn is already
    /// persisted when the transcript is built, so it always occupies the
    /// newest fetched slot before being excluded — fetching exactly the window
    /// size left 7 prior messages while `[近期图片]` told the judge it counted
    /// 8. Seeds exactly 8 prior rows whose OLDEST is an image turn: with the
    /// off-by-one that image fell out of the window and the count read 0.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn transcript_window_covers_the_full_advertised_eight(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        let user_id = Uuid::new_v4();
        let (_g, _instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        // 8 prior rows, oldest first at now()-8min … now()-1min. Row 0 (the
        // oldest, exactly 8th-most-recent prior) is the image turn.
        for i in 0..8i32 {
            let (role, content, meta) = if i == 0 {
                (
                    "assistant",
                    "",
                    Some(serde_json::json!({"image":{"prompt":"x","caption":"在沙滩"}})),
                )
            } else if i % 2 == 1 {
                ("user", "文字消息", None)
            } else {
                ("assistant", "文字回复", None)
            };
            sqlx::query(
                "INSERT INTO engine.chat_messages (session_id, role, content, metadata, sent_at) \
                 VALUES ($1, $2, $3, $4, now() - make_interval(mins => $5))",
            )
            .bind(session_id)
            .bind(role)
            .bind(content)
            .bind(meta)
            .bind(8 - i)
            .execute(&pool)
            .await
            .unwrap();
        }
        let chat_repo = ChatRepo { pool: &pool };
        let current = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01J9000000000000000000000C",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        let t = build_input_filter_transcript(&chat_repo, session_id, current).await;
        assert_eq!(
            t.images_in_window, 1,
            "the 8th-most-recent prior message is an image and must be counted: {t:?}"
        );
        assert!(
            !t.last_assistant_is_image,
            "the newest assistant row is text: {t:?}"
        );
        assert_eq!(
            t.transcript.lines().count(),
            8,
            "the judge must see the full advertised 8 prior messages: {}",
            t.transcript
        );
        assert!(
            !t.transcript.contains("hi"),
            "the current turn must be excluded: {}",
            t.transcript
        );
    }

    /// Channel-marked rows (voice / product_qa) are out of companion context:
    /// they must neither render in the transcript nor count as images. Pins
    /// the exclusion the counting facts now silently depend on.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn transcript_excludes_channel_rows_from_text_and_counts(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        let user_id = Uuid::new_v4();
        let (_g, _instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
             VALUES ($1, 'user', '普通消息', now() - interval '3 minutes')",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();
        // A voice-channel image row: out of companion context entirely.
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, metadata, channel, sent_at) \
             VALUES ($1, 'assistant', '语音旁路', '{\"image\":{\"prompt\":\"v\"}}', 'voice', now() - interval '2 minutes')",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, metadata, sent_at) \
             VALUES ($1, 'assistant', '', '{\"image\":{\"prompt\":\"y\",\"caption\":\"在天台\"}}', now() - interval '1 minute')",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        let chat_repo = ChatRepo { pool: &pool };
        let current = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01J9000000000000000000000D",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        let t = build_input_filter_transcript(&chat_repo, session_id, current).await;
        assert_eq!(
            t.images_in_window, 1,
            "the voice-channel image row must not count: {t:?}"
        );
        assert!(
            t.last_assistant_is_image,
            "the newest COMPANION assistant row is the image turn: {t:?}"
        );
        assert!(
            !t.transcript.contains("语音旁路"),
            "channel rows must not render: {}",
            t.transcript
        );
        assert!(t.transcript.contains("普通消息"), "{}", t.transcript);
    }

    /// Build a `JudgeTranscript` from (role, content, metadata) triples,
    /// exercising the same folding logic the DB path uses.
    fn judge_transcript_from_parts(
        rows: &[(&str, &str, Option<serde_json::Value>)],
    ) -> JudgeTranscript {
        let mut acc = JudgeTranscriptAcc::default();
        for (role, content, meta) in rows {
            acc.push(role, content, meta.as_ref());
        }
        acc.finish()
    }

    #[test]
    fn judge_transcript_counts_images_and_last_flag() {
        // 3 assistant rows, 2 of them image turns, newest one an image.
        let rows = vec![
            (
                "assistant",
                "",
                Some(serde_json::json!({"image":{"prompt":"a"}})),
            ),
            ("assistant", "just text", None),
            (
                "assistant",
                "",
                Some(serde_json::json!({"image":{"prompt":"b"}})),
            ),
        ];
        let t = judge_transcript_from_parts(&rows);
        assert_eq!(t.images_in_window, 2);
        assert!(t.last_assistant_is_image);
    }

    #[test]
    fn judge_transcript_last_flag_false_when_newest_is_text() {
        let rows = vec![
            (
                "assistant",
                "",
                Some(serde_json::json!({"image":{"prompt":"a"}})),
            ),
            ("assistant", "just text", None),
        ];
        let t = judge_transcript_from_parts(&rows);
        assert_eq!(t.images_in_window, 1);
        assert!(!t.last_assistant_is_image, "newest assistant row is text");
    }

    #[test]
    fn judge_transcript_empty_is_zero() {
        let t = judge_transcript_from_parts(&[]);
        assert_eq!(t.images_in_window, 0);
        assert!(!t.last_assistant_is_image);
        assert_eq!(t.transcript, "");
    }

    #[test]
    fn pde_ctx_renders_recent_image_facts_in_messages_not_turns() {
        let input = fixture_decision_input();
        let t = JudgeTranscript {
            transcript: "用户：hi\nMia：hey".into(),
            images_in_window: 2,
            last_assistant_is_image: true,
        };
        let ctx = build_pde_ctx(&t, &input, true, None);
        assert!(
            ctx.contains("[近期图片] 最近8条消息内已发图=2 张；上一条 AI 消息是图片=是"),
            "facts line must render with a message-based unit: {ctx}"
        );
        assert!(
            ctx.contains("以本行计数为准"),
            "the override clause must render: {ctx}"
        );
        // The unit must NOT claim turns — the window is rows.
        assert!(!ctx.contains("轮内已发图"), "unit must not say 轮: {ctx}");
    }

    #[test]
    fn pde_ctx_renders_recent_image_facts_negative_case() {
        let input = fixture_decision_input();
        let t = JudgeTranscript {
            transcript: "用户：hi".into(),
            images_in_window: 0,
            last_assistant_is_image: false,
        };
        let ctx = build_pde_ctx(&t, &input, false, None);
        assert!(
            ctx.contains("[近期图片] 最近8条消息内已发图=0 张；上一条 AI 消息是图片=否"),
            "the negative case is a signal too and must always render: {ctx}"
        );
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
                history_anchor: Default::default(),
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
            guard_action(PdeAction::Ghost, &a, &sigs(50, Some(5.0)), false, false),
            ActionType::Ghost
        );
        // ghost vetoed by new-relationship floor
        assert_eq!(
            guard_action(PdeAction::Ghost, &a, &sigs(3, None), false, false),
            ActionType::ReplyText
        );
        // image actions degrade to text when no executor chain
        assert_eq!(
            guard_action(PdeAction::ReplyImage, &a, &sigs(50, None), false, false),
            ActionType::ReplyText
        );
        assert_eq!(
            guard_action(PdeAction::ReplyTextImage, &a, &sigs(50, None), false, false),
            ActionType::ReplyText
        );
        assert_eq!(
            guard_action(PdeAction::ReplyText, &a, &sigs(50, None), false, false),
            ActionType::ReplyText
        );
    }

    #[test]
    fn guard_action_keeps_image_when_executor_available() {
        let aff = test_affinity();
        let sig = test_signals();
        assert_eq!(
            guard_action(PdeAction::ReplyImage, &aff, &sig, true, false),
            ActionType::ReplyImage
        );
        assert_eq!(
            guard_action(PdeAction::ReplyTextImage, &aff, &sig, true, false),
            ActionType::ReplyTextImage
        );
        // executor unavailable → degrade (today's behaviour)
        assert_eq!(
            guard_action(PdeAction::ReplyImage, &aff, &sig, false, false),
            ActionType::ReplyText
        );
        assert_eq!(
            guard_action(PdeAction::ReplyTextImage, &aff, &sig, false, false),
            ActionType::ReplyText
        );
    }

    #[test]
    fn image_capability_requires_a_configured_composer() {
        use eros_engine_llm::model_config::ModelConfig;
        // Executor present (request carries an `image` block) but no composer task.
        let no_composer =
            ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel = \"m\"\n").expect("parses");
        assert!(
            !image_capability_available(true, &no_composer),
            "no composer task ⇒ no image capability: nothing can write a prompt"
        );
        // Composer configured ⇒ capability follows the executor.
        let with_composer =
            ModelConfig::from_toml_str("[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n")
                .expect("parses");
        assert!(image_capability_available(true, &with_composer));
        assert!(
            !image_capability_available(false, &with_composer),
            "no executor ⇒ no capability, unchanged"
        );
        // A variant-shaped filter_prompt is still a configured composer.
        let variants = ModelConfig::from_toml_str(
            "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\nfilter_prompt = { a = \"X\" }\n",
        )
        .expect("parses");
        assert!(image_capability_available(true, &variants));
    }

    #[test]
    fn guard_product_qa_available_passes_unavailable_degrades() {
        let a = pde_test_affinity();
        let s = sigs(50, None);
        assert_eq!(
            guard_action(PdeAction::ProductQa, &a, &s, false, true),
            ActionType::ProductQa
        );
        assert_eq!(
            guard_action(PdeAction::ProductQa, &a, &s, false, false),
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
        let acted = guard_action(
            PdeAction::Ghost,
            &input.affinity,
            &input.signals,
            false,
            false,
        );
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
                history_anchor: Default::default(),
            },
            None,
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
            channel: None,
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

    /// Codex P2 (PR #141): an idempotent replay of an empty-reply ghost-fallback
    /// row must re-emit Done{ghost_fallback:true} — wire-identical to the original
    /// live stream (a real ghost likewise re-emits its ghost frames on replay). A
    /// pseudo-ghost / garble row also carries a fallback_reason but is a non-empty
    /// canned/salvaged reply, so it must replay as ghost_fallback:false.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn replay_reemits_ghost_fallback_only_for_empty_reply_fallbacks(pool: PgPool) {
        use futures_util::StreamExt;

        let user_id = Uuid::new_v4();
        let (_g, _instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;
        let state = std::sync::Arc::new(crate::routes::companion::test_state(pool.clone()));

        let mk = |content: &str, reason: &str| eros_engine_store::chat::ChatMessage {
            id: Uuid::new_v4(),
            session_id,
            role: "assistant".into(),
            content: content.into(),
            sent_at: chrono::Utc::now(),
            client_msg_id: None,
            ghost_decision: false,
            user_message_id: None,
            continues_from_message_id: None,
            truncated: false,
            model: Some("m/x".into()),
            usage: None,
            generation_id: Some("gen-x".into()),
            assistant_action_type: Some("reply".into()),
            channel: None,
            pre_filter_content: None,
            metadata: Some(serde_json::json!({ "fallback_reason": reason })),
        };

        let done_flag = |frames: &[ProtocolFrame]| -> bool {
            frames
                .iter()
                .find_map(|f| match f {
                    ProtocolFrame::Done { ghost_fallback, .. } => Some(*ghost_fallback),
                    _ => None,
                })
                .expect("a Done frame")
        };

        // Empty-reply ghost fallback → replay re-emits ghost_fallback:true.
        let ghost: Vec<ProtocolFrame> = replay_stream(
            state.clone(),
            session_id,
            user_id,
            false,
            vec![mk("", "empty_completion")],
        )
        .collect()
        .await;
        assert!(
            done_flag(&ghost),
            "empty_completion fallback row must replay as ghost_fallback:true"
        );

        // Pseudo-ghost: a fallback_reason is present but the content is a real
        // canned reply → must NOT replay as a ghost.
        let pseudo: Vec<ProtocolFrame> = replay_stream(
            state,
            session_id,
            user_id,
            false,
            vec![mk("稍后再聊", "stream_failure")],
        )
        .collect()
        .await;
        assert!(
            !done_flag(&pseudo),
            "pseudo-ghost row (non-empty, stream_failure) must replay as ghost_fallback:false"
        );
    }

    /// A persisted row marked `channel = "product_qa"` must replay with
    /// `Meta { action_type: FrameActionType::ProductQa }` — matching the live
    /// burst's product-QA labeling — while a normal (channel-NULL) row
    /// continues to replay as `FrameActionType::Reply`.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn replay_maps_product_qa_channel_to_meta_action_type(pool: PgPool) {
        use futures_util::StreamExt;

        let user_id = Uuid::new_v4();
        let (_g, _instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;
        let state = std::sync::Arc::new(crate::routes::companion::test_state(pool.clone()));

        let mk = |content: &str, channel: Option<&str>| eros_engine_store::chat::ChatMessage {
            id: Uuid::new_v4(),
            session_id,
            role: "assistant".into(),
            content: content.into(),
            sent_at: chrono::Utc::now(),
            client_msg_id: None,
            ghost_decision: false,
            user_message_id: None,
            continues_from_message_id: None,
            truncated: false,
            model: Some("m/x".into()),
            usage: None,
            generation_id: Some("gen-x".into()),
            assistant_action_type: Some("reply".into()),
            channel: channel.map(String::from),
            pre_filter_content: None,
            metadata: None,
        };

        let rows = vec![
            mk("product answer", Some("product_qa")),
            mk("normal reply", None),
        ];

        let frames: Vec<ProtocolFrame> = replay_stream(state, session_id, user_id, false, rows)
            .collect()
            .await;

        assert!(
            matches!(
                &frames[0],
                ProtocolFrame::Meta {
                    action_type: FrameActionType::ProductQa,
                    ..
                }
            ),
            "channel='product_qa' row must replay as Meta(action_type=product_qa); got {:?}",
            frames[0]
        );
        // Each row with non-empty content emits Meta, Delta, Done (3 frames),
        // so the second row's Meta lands at index 3.
        assert!(
            matches!(
                &frames[3],
                ProtocolFrame::Meta {
                    action_type: FrameActionType::Reply,
                    ..
                }
            ),
            "channel=NULL row must replay as Meta(action_type=reply); got {:?}",
            frames[3]
        );
    }

    /// Codex P2: a product-QA turn persists exactly one assistant row, and a
    /// truncated (finish_reason == "length") product-QA row is still a served
    /// answer — the live burst emits Meta → Delta → Done(truncated:true) →
    /// Final, no Error. The pre-existing "every persisted row truncated ⇒
    /// Error(UpstreamUnavailable)" rule is companion multi-candidate-chain
    /// semantics (all fallback models exhausted, truncated); it must not fire
    /// for a single truncated product_qa row, or replay would diverge from
    /// live by injecting a spurious terminal Error with no Final.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn replay_product_qa_truncated_row_replays_answer_not_error(pool: PgPool) {
        use futures_util::StreamExt;

        let user_id = Uuid::new_v4();
        let (_g, _instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;
        let state = std::sync::Arc::new(crate::routes::companion::test_state(pool.clone()));

        let row = eros_engine_store::chat::ChatMessage {
            id: Uuid::new_v4(),
            session_id,
            role: "assistant".into(),
            content: "product answer, cut off mid-".into(),
            sent_at: chrono::Utc::now(),
            client_msg_id: None,
            ghost_decision: false,
            user_message_id: None,
            continues_from_message_id: None,
            truncated: true,
            model: Some("qa/exec".into()),
            usage: None,
            generation_id: Some("gen-qa".into()),
            assistant_action_type: Some("product_qa".into()),
            channel: Some("product_qa".into()),
            pre_filter_content: None,
            metadata: None,
        };

        let frames: Vec<ProtocolFrame> =
            replay_stream(state, session_id, user_id, false, vec![row])
                .collect()
                .await;

        assert!(
            matches!(
                &frames[0],
                ProtocolFrame::Meta {
                    action_type: FrameActionType::ProductQa,
                    ..
                }
            ),
            "first frame must be Meta(action_type=product_qa); got {:?}",
            frames[0]
        );
        assert!(
            frames.iter().any(|f| matches!(
                f,
                ProtocolFrame::Delta { content, .. } if content == "product answer, cut off mid-"
            )),
            "must replay the persisted content as a Delta; got {frames:?}"
        );
        assert!(
            frames.iter().any(|f| matches!(
                f,
                ProtocolFrame::Done {
                    truncated: true,
                    ..
                }
            )),
            "must replay Done(truncated:true); got {frames:?}"
        );
        assert!(
            matches!(frames.last(), Some(ProtocolFrame::Final { .. })),
            "terminal frame must be Final, not an Error; got {frames:?}"
        );
        assert!(
            !frames.iter().any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "a truncated product_qa row is still a served answer — must not emit Error; got {frames:?}"
        );
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
                history_anchor: Default::default(),
            },
            None,
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
    async fn normal_reply_resets_ghost_streak(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hey\"}}],\"id\":\"gen-r\",\"model\":\"primary\"}\n\ndata: [DONE]\n\n";
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
        // Seed a non-zero ghost streak on the session's affinity row. The row
        // doesn't exist yet at this point (created lazily by run_stream's
        // `AffinityRepo::load_or_create`), so upsert rather than UPDATE —
        // a bare UPDATE here would silently affect 0 rows and the later
        // load_or_create would just insert a fresh ghost_streak=0 row,
        // making the assertion below pass trivially regardless of the
        // reset/gate logic under test.
        sqlx::query(
            "INSERT INTO engine.companion_affinity (session_id, user_id, instance_id, ghost_streak) \
             VALUES ($1, $2, $3, 3) \
             ON CONFLICT (session_id) DO UPDATE SET ghost_streak = 3",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(instance_id)
        .execute(&pool)
        .await
        .unwrap();

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let umid = match chat_repo
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

        let _frames: Vec<ProtocolFrame> = run_stream(
            std::sync::Arc::new(state),
            PersistedUserMessage {
                user_message_id: umid,
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
                history_anchor: Default::default(),
            },
            None,
        )
        .collect()
        .await;

        let gs: i32 = sqlx::query_scalar(
            "SELECT ghost_streak FROM engine.companion_affinity WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(gs, 0, "a real reply must reset ghost_streak");
    }

    /// Filtered mode, case (a): a reply that's entirely a bracketed artifact
    /// strips to "" via `output_regex`, so nothing is served. That must
    /// surface as `Done{ghost_fallback:true}` (no `Delta` at all), tag the
    /// persisted assistant row with `metadata.fallback_reason = "regex_strip"`,
    /// and — per the design's "既不加也不清零" — leave `ghost_streak` untouched
    /// (gated in `run_stream` via `BurstOutcome.ghost_fallback`, asserted
    /// separately by `normal_reply_resets_ghost_streak` for the real-reply
    /// case). `[tasks.chat_companion].model = "primary"` is set explicitly so
    /// the `output_regex` rule's `models` list actually targets the model
    /// `state.model_config.resolve` picks — the default `ModelConfig` (no
    /// `[tasks.chat_companion]` block) falls through to the compiled-in
    /// `x-ai/grok-4-mini`, which wouldn't match a rule scoped to "primary" and
    /// would silently fall back to LIVE mode instead of filtered.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn regex_strip_to_empty_becomes_ghost_fallback(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        // Reply is entirely a bracketed artifact.
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"[你给对方发送了一张照片]\"}}],\"id\":\"gen-a\",\"model\":\"primary\"}\n\ndata: [DONE]\n\n";
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
        // Seed ghost_streak = 2 on the affinity row (upsert, not a bare UPDATE:
        // the row doesn't exist yet — created lazily by run_stream's
        // `AffinityRepo::load_or_create` — same rationale as
        // `normal_reply_resets_ghost_streak` above). This also happens to sit
        // at the ghost anti-streak veto's threshold (`ghost_streak >= 2` in
        // `eros_engine_core::ghost::ghost_permitted`), but that veto is moot
        // here anyway: a brand-new session's `message_count < 10` already
        // forces `pde::decide` to `ActionType::ReplyText` deterministically.
        sqlx::query(
            "INSERT INTO engine.companion_affinity (session_id, user_id, instance_id, ghost_streak) \
             VALUES ($1, $2, $3, 2) \
             ON CONFLICT (session_id) DO UPDATE SET ghost_streak = 2",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(instance_id)
        .execute(&pool)
        .await
        .unwrap();

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        // One config carries both the resolved chat model ("primary") and the
        // output_regex rule scoped to it — built via `ModelConfig::from_toml_str`
        // + `compile_output_regex()` rather than constructing `CompiledRegexRule`
        // by hand, so the test doesn't need `regex` as a direct dependency of
        // eros-engine-server (mirrors the `regex_target_buffers_without_...`
        // / `regex_strips_artifact_from_client_and_memory` tests above). The
        // pattern matches a WHOLE reply that's just one bracketed artifact
        // (TOML literal string — `'...'` — so the backslashes reach `regex`
        // unescaped).
        let regex_cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(
            r#"
            [tasks.chat_companion]
            model = "primary"

            [[tasks.chat_companion.output_regex]]
            models = ["primary"]
            pattern = '^\s*\[[^\]]*\]\s*$'
            "#,
        )
        .unwrap();
        state.model_config = std::sync::Arc::new(regex_cfg.clone());
        state.output_regex = std::sync::Arc::new(
            regex_cfg
                .compile_output_regex()
                .expect("bracket-only pattern compiles"),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let umid = match chat_repo
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
                user_message_id: umid,
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
                history_anchor: Default::default(),
            },
            None,
        )
        .collect()
        .await;

        // Done carries ghost_fallback; no Delta was emitted.
        assert!(
            frames.iter().any(|f| matches!(
                f,
                ProtocolFrame::Done {
                    ghost_fallback: true,
                    ..
                }
            )),
            "expected Done{{ghost_fallback:true}}, got {frames:?}"
        );
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Delta { .. })),
            "no Delta for an empty reply"
        );
        // Audit row: empty content + fallback_reason.
        let (content, reason): (String, Option<String>) = sqlx::query_as(
            "SELECT content, metadata->>'fallback_reason' FROM engine.chat_messages \
             WHERE user_message_id = $1 AND role = 'assistant'",
        )
        .bind(umid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(content, "");
        assert_eq!(reason.as_deref(), Some("regex_strip"));
        // Affinity-neutral: ghost_streak untouched.
        let gs: i32 = sqlx::query_scalar(
            "SELECT ghost_streak FROM engine.companion_affinity WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(gs, 2, "ghost fallback must not reset ghost_streak");
    }

    /// Filtered mode, case (b): the served model returns a 200 OK stream
    /// whose delta never carries a `content` field, so `acc` stays empty and
    /// `finish_reason` is never `"length"` — an empty completion, distinct
    /// from case (a)'s regex-strip-to-empty above. On the LAST chain attempt
    /// (single-model chain here, so this is also the first) that must
    /// surface as `Done{ghost_fallback:true}` tagged
    /// `metadata.fallback_reason = "empty_completion"`, NOT the
    /// pseudo-ghost/Error truncation path. The `output_regex` rule below
    /// targets the chain model ("primary") purely so `regex_targets_chain`
    /// forces FILTERED mode — an unpinned/untargeted rule would silently
    /// fall through to LIVE mode instead (see
    /// `regex_strip_to_empty_becomes_ghost_fallback` above) — but its
    /// pattern never matches anything, so it's a pure mode-selection no-op
    /// and the empty-completion branch (which returns before the regex is
    /// ever applied) never touches it.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn empty_completion_last_attempt_becomes_ghost_fallback(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        // 200 stream with a delta that carries no `content` at all → empty completion.
        let body = "data: {\"choices\":[{\"delta\":{}}],\"id\":\"gen-e\",\"model\":\"primary\"}\n\ndata: [DONE]\n\n";
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
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        // Pin the chain model to "primary" (matching the mock) and target it
        // with a never-matching output_regex rule: forces FILTERED mode via
        // `regex_targets_chain` without ever altering the (already empty)
        // reply. Built via `ModelConfig::from_toml_str` +
        // `compile_output_regex()` — not `regex::Regex::new` by hand — since
        // `regex` isn't a direct dependency of eros-engine-server.
        let regex_cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(
            r#"
            [tasks.chat_companion]
            model = "primary"

            [[tasks.chat_companion.output_regex]]
            models = ["primary"]
            pattern = '^THIS_PATTERN_NEVER_MATCHES_ANYTHING$'
            "#,
        )
        .unwrap();
        state.model_config = std::sync::Arc::new(regex_cfg.clone());
        state.output_regex = std::sync::Arc::new(
            regex_cfg
                .compile_output_regex()
                .expect("never-matching pattern compiles"),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let umid = match chat_repo
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
                user_message_id: umid,
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
                history_anchor: Default::default(),
            },
            None,
        )
        .collect()
        .await;

        assert!(
            frames.iter().any(|f| matches!(
                f,
                ProtocolFrame::Done {
                    ghost_fallback: true,
                    ..
                }
            )),
            "expected Done{{ghost_fallback:true}}, got {frames:?}"
        );
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "empty completion must not error"
        );
        let reason: Option<String> = sqlx::query_scalar(
            "SELECT metadata->>'fallback_reason' FROM engine.chat_messages \
             WHERE user_message_id = $1 AND role = 'assistant'",
        )
        .bind(umid)
        .fetch_optional(&pool)
        .await
        .unwrap()
        .flatten();
        assert_eq!(reason.as_deref(), Some("empty_completion"));
    }

    /// LIVE mode (primary-risk), case (b): the served model returns a 200 OK
    /// stream whose delta never carries a `content` field, so `acc` stays
    /// empty and `finish_reason` is never `"length"` — an empty completion on
    /// the LAST (here, only) chain attempt in the un-buffered path, which
    /// interleaves persist → Done → accept/advance per attempt. Unlike the
    /// filtered-mode sibling above, this test carries NO `output_regex` rule
    /// and no LLM `filter` — `test_state`'s bare defaults leave both
    /// `regex_targets_chain` and `llm_filter_arms` false, so `filtered_mode`
    /// is false and the turn runs the LIVE branch under test. Must surface as
    /// `Done{ghost_fallback:true}` tagged `metadata.fallback_reason =
    /// "empty_completion"`, NOT the pseudo-ghost/Error truncation path that
    /// the sibling `run_stream_reply_terminates_cleanly_with_mock_openrouter`
    /// / multi-attempt tests exercise.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn empty_completion_live_last_attempt_becomes_ghost_fallback(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        // 200 stream with a delta that carries no `content` at all → empty completion.
        let body = "data: {\"choices\":[{\"delta\":{}}],\"id\":\"gen-el\",\"model\":\"primary\"}\n\ndata: [DONE]\n\n";
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
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        // No output_regex, no filter → live mode.

        let chat_repo = ChatRepo { pool: &pool };
        let umid = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01J5555555555555555555555B",
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
                content: "hi".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
                image_url: None,
                image: None,
                history_anchor: Default::default(),
            },
            None,
        )
        .collect()
        .await;

        assert!(
            frames.iter().any(|f| matches!(
                f,
                ProtocolFrame::Done {
                    ghost_fallback: true,
                    ..
                }
            )),
            "expected Done{{ghost_fallback:true}}, got {frames:?}"
        );
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "empty completion must not error"
        );
        let reason: Option<String> = sqlx::query_scalar(
            "SELECT metadata->>'fallback_reason' FROM engine.chat_messages \
             WHERE user_message_id = $1 AND role = 'assistant'",
        )
        .bind(umid)
        .fetch_optional(&pool)
        .await
        .unwrap()
        .flatten();
        assert_eq!(reason.as_deref(), Some("empty_completion"));
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
                history_anchor: Default::default(),
            },
            None,
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

    // ── Delegate-only image drawing — the two arms end-to-end ──────────────
    // These drive `run_stream` with a `force`d image action (mode selects the
    // arm), asserting the delegated frame sequence, that NO in-engine draw
    // happens, and that only the minimal `metadata.image` marker is persisted.
    // The model config carries no image task — the chat stream always
    // delegates: it still emits `image_request` (and the marker) with nothing
    // image-related configured. It also omits the judge (`pde_decision`) and the
    // composer (`chat_image_prompt_compose`), so the image-only turn makes zero
    // LLM calls and the outcome is deterministic.

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn reply_image_emits_image_request_and_marker_no_draw(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Any OpenRouter call 500s — so an ERRONEOUS draw attempt would
        // surface as an extra frame in the sequence (the exact 4-frame
        // sequence is asserted below, leaving no room for one). The correct
        // delegated image-only path makes no provider call at all.
        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"primary\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "draw me",
                "01J9000000000000000000000A",
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
                content: "draw me".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
                image_url: None,
                image: Some(crate::routes::companion_stream::ImageReplyParams {
                    force: true,
                    ..Default::default()
                }),
                history_anchor: Default::default(),
            },
            None,
        )
        .collect()
        .await;

        // Exact delegated image-only sequence: meta → done → image_request → final.
        let types: Vec<String> = frames
            .iter()
            .map(|f| {
                serde_json::to_value(f).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            types,
            ["meta", "done", "image_request", "final"],
            "delegated image-only sequence, got {frames:?}"
        );
        // meta carries reply_image and no model (the consumer chooses the model).
        let (action, model) = frames
            .iter()
            .find_map(|f| match f {
                ProtocolFrame::Meta {
                    action_type, model, ..
                } => Some((*action_type, model.clone())),
                _ => None,
            })
            .expect("meta present");
        assert_eq!(action, FrameActionType::ReplyImage);
        assert!(model.is_none(), "delegated meta carries no model");
        // image_request: face ref + base64 composed wire prompt (no composer
        // configured and no seed left ⇒ portrait fallback: style preset only).
        let (composed_b64, image_ref) = frames
            .iter()
            .find_map(|f| match f {
                ProtocolFrame::ImageRequest {
                    composed_prompt,
                    image_ref,
                    ..
                } => Some((composed_prompt.clone(), *image_ref)),
                _ => None,
            })
            .expect("image_request present");
        assert_eq!(image_ref, eros_engine_core::types::ImageRef::Face);
        let composed = {
            use base64::Engine as _;
            String::from_utf8(
                base64::engine::general_purpose::STANDARD
                    .decode(&composed_b64)
                    .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(
            composed,
            eros_engine_llm::model_config::STYLE_REALISTIC,
            "no composer configured and no seed ⇒ portrait fallback (style preset only): {composed}"
        );

        // Persistence: minimal marker only (empty subject under `prompt`,
        // portrait fallback), and NOT the composed wire prompt / model /
        // generation_id / url.
        let meta_row: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT metadata FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant' \
             ORDER BY sent_at DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let img = meta_row.expect("assistant row has metadata")["image"].clone();
        assert_eq!(
            img["prompt"], "",
            "no compose configured and no seed ⇒ empty subject (portrait fallback)"
        );
        assert!(img.get("model").is_none(), "marker must not store a model");
        assert!(
            img.get("generation_id").is_none(),
            "marker must not store a generation id"
        );
        assert!(img.get("url").is_none(), "marker must not store a url");
        assert_ne!(
            img["prompt"],
            serde_json::json!(composed),
            "the composed wire prompt (style preset) must not be persisted as the marker subject"
        );
    }

    /// Shared setup for the two `prompt_variant` tests: a keyed composer
    /// config, a mock that records every outbound call, and a forced
    /// image-only turn. Returns the recorded requests and the emitted frames.
    ///
    /// The turn's user content is deliberately kept below
    /// `post_process::AFFINITY_EVAL_MIN_CHARS` (4 chars). A successful forced
    /// `ReplyImage` turn `tokio::spawn`s `post_process::run` in the background
    /// and returns before that task finishes; `post_process::run` unconditionally
    /// attempts an `affinity_evaluation` LLM call (against this same mock)
    /// whenever `eval_skip_reason` lets it through — and `resolve()` never
    /// returns `None` for an unconfigured task, so there is no config-side gate
    /// to lean on here. A longer message (e.g. "draw me", 7 chars) clears the
    /// length gate, and because `ReplyImage` proxies the assistant text with
    /// `plan.image_caption` — falling back to a generic marker when the
    /// caption is absent, so the proxy text is always non-blank — it would
    /// also clear the empty-assistant gate — so the eval call fires, racing
    /// the test's `mock.received_requests()` against a `tokio::spawn`ed task.
    /// Keeping the content short makes "the composer is the only possible
    /// call" true by construction, not by scheduling luck.
    async fn run_variant_turn(
        pool: &PgPool,
        prompt_variant: Option<&str>,
        composer_response: wiremock::ResponseTemplate,
    ) -> (Vec<wiremock::Request>, Vec<ProtocolFrame>) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer};

        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(composer_response)
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(pool, user_id).await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"primary\"\n\
                 [tasks.chat_image_prompt_compose]\nmodel = \"composer\"\n\
                 filter_prompt = { a = \"PROMPT_A\", b = \"PROMPT_B\" }\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let chat_repo = ChatRepo { pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01J9000000000000000000000A",
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
                image: Some(crate::routes::companion_stream::ImageReplyParams {
                    force: true,
                    prompt_variant: prompt_variant.map(str::to_string),
                    ..Default::default()
                }),
                history_anchor: Default::default(),
            },
            None,
        )
        .collect()
        .await;

        let reqs = mock.received_requests().await.expect("recorded requests");
        (reqs, frames)
    }

    /// Codex-review P1 regression (PR #216): a forced image turn with the PDE
    /// judge DISABLED must still fetch the history transcript. The composer
    /// generates from the recent scene now that no seed exists, so leaving the
    /// transcript fetch keyed on `resolved_pde.is_some()` alone would hand the
    /// composer `[最近场景]\n（无）` on every judge-less deployment — silently
    /// stripping its main context. Rule-based `pde::decide` never picks image
    /// actions, so `force_image` is the only judge-less image path and the one
    /// this pins.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn forced_image_without_pde_still_feeds_the_scene(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        // A prior exchange the composer's scene must carry.
        for (role, content) in [
            ("user", "我们明天去天台看日落吧"),
            ("assistant", "好呀，我先去踩个点"),
        ] {
            sqlx::query(
                "INSERT INTO engine.chat_messages (session_id, role, content) VALUES ($1, $2, $3)",
            )
            .bind(session_id)
            .bind(role)
            .bind(content)
            .execute(&pool)
            .await
            .unwrap();
        }

        // Composer configured, judge NOT — `resolve_pde()` is None, so before
        // the fix the transcript was never fetched on this path.
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"primary\"\n\
                 [tasks.chat_image_prompt_compose]\nmodel = \"composer\"\nfilter_prompt = \"COMPOSE\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01J9000000000000000000000B",
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
                tips_amount_usd: None,
                image_url: None,
                image: Some(crate::routes::companion_stream::ImageReplyParams {
                    force: true,
                    ..Default::default()
                }),
                history_anchor: Default::default(),
            },
            None,
        )
        .collect()
        .await;

        let reqs = mock.received_requests().await.expect("recorded requests");
        assert_eq!(reqs.len(), 1, "the composer is the only provider call");
        let body: serde_json::Value =
            serde_json::from_slice(&reqs[0].body).expect("composer request body is json");
        let payload = body["messages"][1]["content"]
            .as_str()
            .expect("composer user payload");
        assert!(
            payload.contains("天台看日落"),
            "the composer's scene must carry the prior exchange: {payload}"
        );
        assert!(
            !payload.contains("[最近场景]\n（无）"),
            "the scene must not be empty when history exists: {payload}"
        );
    }

    /// `image.prompt_variant = "b"` must send variant b's text as the
    /// composer's system message — proof the wire value reaches
    /// `PromptSpec::select`.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn prompt_variant_selects_the_configured_composer_prompt(pool: PgPool) {
        use wiremock::ResponseTemplate;
        // 500 on every call: the composer fails open to an empty subject —
        // there is no seed left to fall back to. This test asserts on what was
        // SENT, so the response body is irrelevant.
        let (reqs, _frames) = run_variant_turn(&pool, Some("b"), ResponseTemplate::new(500)).await;
        assert_eq!(
            reqs.len(),
            1,
            "an image-only turn makes exactly one provider call (the composer)"
        );
        let body: serde_json::Value =
            serde_json::from_slice(&reqs[0].body).expect("composer request body is json");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(
            body["messages"][0]["content"], "PROMPT_B",
            "composer must use variant b, got {}",
            body["messages"][0]["content"]
        );

        // Spec 2026-08-02 absence semantics: no successful compose ⇒ none of
        // the audit keys, and `prompt` is the empty subject (portrait fallback).
        let meta: serde_json::Value = sqlx::query_scalar(
            "SELECT metadata FROM engine.chat_messages WHERE role = 'assistant'",
        )
        .fetch_one(&pool)
        .await
        .expect("assistant image row persisted");
        let img = meta["image"].as_object().expect("image marker present");
        assert_eq!(img["prompt"], "");
        assert!(img.get("compose_variant").is_none());
        assert!(img.get("compose_model").is_none());
        assert!(img.get("compose_generation_id").is_none());
    }

    /// `prompt_variant = "raw"` used to be a reserved wire escape that skipped
    /// the composer entirely. That escape is gone (#212 Task 6): with no seed
    /// to draw verbatim, `"raw"` is an ordinary variant name. `run_variant_turn`
    /// configures only `a` and `b`, so `"raw"` is a miss like any unconfigured
    /// key — the composer still runs, on the built-in prompt.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn prompt_variant_raw_falls_back_to_builtin_when_unconfigured(pool: PgPool) {
        use wiremock::ResponseTemplate;
        // 500 on every call: the composer fails open to an empty subject —
        // there is no seed left to fall back to. This test asserts on what was
        // SENT, so the response body is irrelevant.
        let (reqs, _frames) =
            run_variant_turn(&pool, Some("raw"), ResponseTemplate::new(500)).await;
        assert_eq!(
            reqs.len(),
            1,
            "\"raw\" no longer skips the composer — it still makes the one provider call"
        );
        let body: serde_json::Value =
            serde_json::from_slice(&reqs[0].body).expect("composer request body is json");
        assert_eq!(body["messages"][0]["role"], "system");
        let sent = body["messages"][0]["content"]
            .as_str()
            .expect("system content is a string");
        assert_ne!(sent, "PROMPT_A");
        assert_ne!(sent, "PROMPT_B");
        assert!(
            sent.contains("You compose the image for a picture"),
            "unconfigured \"raw\" key must fall back to the built-in prompt, got {sent}"
        );

        // Same fail-open shape as a configured-variant compose failure: no
        // successful compose ⇒ none of the audit keys, and `prompt` is the
        // empty subject (portrait fallback).
        let meta: serde_json::Value = sqlx::query_scalar(
            "SELECT metadata FROM engine.chat_messages WHERE role = 'assistant'",
        )
        .fetch_one(&pool)
        .await
        .expect("assistant image row persisted");
        let img = meta["image"].as_object().expect("image marker present");
        assert_eq!(img["prompt"], "");
        assert!(img.get("compose_variant").is_none());
        assert!(img.get("compose_model").is_none());
        assert!(img.get("compose_generation_id").is_none());
    }

    /// Spec 2026-08-02 (revised): a SUCCESSFUL composer call persists the
    /// audit trio to `metadata.image` — the selected variant key, the served
    /// model, and the generation id — AND `prompt` now carries the composer's
    /// subject, not the pre-compose seed. The original design pinned `prompt`
    /// to the seed to keep the DB row short; that job now belongs to
    /// `caption`, so `prompt` is free to reflect what the composer actually
    /// decided. The composed WIRE prompt (style preset + appearance + this
    /// subject) is still never persisted — it goes out on the wire only.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn compose_success_persists_audit_trio(pool: PgPool) {
        use wiremock::ResponseTemplate;
        let (reqs, frames) = run_variant_turn(
            &pool,
            Some("b"),
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "gen-xyz",
                "model": "served/model",
                "choices": [{"message": {"content": "ENRICHED SUBJECT"}}],
            })),
        )
        .await;
        assert_eq!(reqs.len(), 1, "composer is the only provider call");

        // The composer's plain-text reply ("ENRICHED SUBJECT", no JSON) parses
        // as the whole reply becoming `prompt` with no caption (migration
        // fallback) — so both the wire and the DB below carry it.
        let composed_b64 = frames
            .iter()
            .find_map(|f| match f {
                ProtocolFrame::ImageRequest {
                    composed_prompt, ..
                } => Some(composed_prompt.clone()),
                _ => None,
            })
            .expect("image_request present");
        let composed = {
            use base64::Engine as _;
            String::from_utf8(
                base64::engine::general_purpose::STANDARD
                    .decode(&composed_b64)
                    .unwrap(),
            )
            .unwrap()
        };
        assert!(
            composed.contains("ENRICHED SUBJECT"),
            "composed wire prompt must carry the composer's enriched text, got {composed}"
        );

        // sqlx::test gives this test its own database — the one assistant row
        // is the image turn's.
        let meta: serde_json::Value = sqlx::query_scalar(
            "SELECT metadata FROM engine.chat_messages WHERE role = 'assistant'",
        )
        .fetch_one(&pool)
        .await
        .expect("assistant image row persisted");
        let img = &meta["image"];
        assert_eq!(
            img["prompt"], "ENRICHED SUBJECT",
            "prompt is the composer's subject, not the pre-compose seed"
        );
        assert_eq!(img["compose_variant"], "b");
        assert_eq!(img["compose_model"], "served/model");
        assert_eq!(img["compose_generation_id"], "gen-xyz");
    }

    /// Sibling of `compose_success_persists_audit_trio`, but the mock composer
    /// returns real JSON (`{"prompt":..., "caption":...}`) instead of plain
    /// text, so `caption` is actually populated end to end. Proves the seam
    /// neither the parser unit tests (`parse_compose_reply`) nor the marker
    /// unit tests (`assistant_transcript_line` / `model_facing_assistant_text`)
    /// exercise on their own: a real composer reply persists `caption` to the
    /// row, and both history-facing renders surface it — never the prompt.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn compose_success_with_json_caption_surfaces_in_both_renders(pool: PgPool) {
        use wiremock::ResponseTemplate;
        let (reqs, _frames) = run_variant_turn(
            &pool,
            Some("b"),
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "gen-cap",
                "model": "served/model",
                "choices": [{"message": {"content":
                    r#"{"prompt":"on a rooftop at dusk","caption":"在天台看夕阳"}"#
                }}],
            })),
        )
        .await;
        assert_eq!(reqs.len(), 1, "composer is the only provider call");

        // sqlx::test gives this test its own database — the one assistant row
        // is the image turn's.
        let row: eros_engine_store::chat::ChatMessage =
            sqlx::query_as("SELECT * FROM engine.chat_messages WHERE role = 'assistant'")
                .fetch_one(&pool)
                .await
                .expect("assistant image row persisted");

        let img = &row.metadata.as_ref().expect("metadata present")["image"];
        assert_eq!(img["prompt"], "on a rooftop at dusk");
        assert_eq!(
            img["caption"], "在天台看夕阳",
            "composer's caption persisted"
        );

        // Both history-facing renders surface the caption, never the prompt.
        let content = row.content.clone();
        let metadata = row.metadata.clone();
        let transcript_line = assistant_transcript_line(&content, metadata.as_ref());
        assert!(
            transcript_line.contains("在天台看夕阳"),
            "judge transcript surfaces the caption: {transcript_line}"
        );
        assert!(
            !transcript_line.contains("rooftop"),
            "the prompt must never reach the judge transcript: {transcript_line}"
        );

        let model_text = crate::pipeline::handlers::model_facing_assistant_text(row);
        assert!(
            model_text.contains("在天台看夕阳"),
            "chat history surfaces the caption: {model_text}"
        );
        assert!(
            !model_text.contains("rooftop"),
            "the prompt must never reach the chat model's history: {model_text}"
        );
    }

    /// Spec 2026-08-02-provider-body-params: an [[providers.openrouter.body]]
    /// rule scoped to a task reaches that task's wire body end-to-end
    /// (config parse → boot accessor → client → per-attempt merge).
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn provider_body_rule_reaches_the_wire_for_its_task(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "ENRICHED"}}],
            })))
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"primary\"\n\
                 [tasks.chat_image_prompt_compose]\nmodel = \"composer\"\n\
                 [[providers.openrouter.body]]\n\
                 tasks = [\"chat_image_prompt_compose\"]\n\
                 params = { venice_parameters = { include_venice_system_prompt = false } }\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            )
            .with_openrouter_body_rules(state.model_config.openrouter_body_rules()),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01J9000000000000000000000B",
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
                tips_amount_usd: None,
                image_url: None,
                image: Some(crate::routes::companion_stream::ImageReplyParams {
                    force: true,
                    ..Default::default()
                }),
                history_anchor: Default::default(),
            },
            None,
        )
        .collect()
        .await;

        let reqs = mock.received_requests().await.expect("recorded requests");
        assert_eq!(
            reqs.len(),
            1,
            "image-only turn makes exactly one call (the composer)"
        );
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(
            body["venice_parameters"]["include_venice_system_prompt"],
            serde_json::Value::Bool(false),
            "task-scoped body rule must reach the composer wire"
        );
        assert_eq!(body["messages"][0]["role"], "system", "engine body intact");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn reply_text_image_appends_image_request_and_marker(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // The judge picks reply_text_image — `force` can only produce
        // reply_image now (spec 2026-08-03 §1), so the judge is the only road
        // to the text+image action. The text reply streams from the chat mock
        // (≥ MIN_FILTERED_OUTPUT_CHARS so it is not degraded as too-short).
        // The delegated image path makes NO extra (draw) call; a draw would
        // reuse this endpoint, but the frame-sequence assertions below leave
        // no room for an extra frame between `image_request` and `final`.
        // The three mocks are routed by MODEL ID so they are mutually
        // exclusive (mount order/precedence cannot matter).
        let mock = MockServer::start().await;
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"I would absolutely love that for you, \"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"let me slip into something far more comfortable and show you every bit of it\"}}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":9,\"total_tokens\":11},\"id\":\"gen-r\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("\"model\":\"primary\""))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("\"model\":\"pde/judge\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content":
                    "{\"action\":\"reply_text_image\",\"inner_state\":\"想给你看\"}"}}],
            })))
            .mount(&mock)
            .await;
        // Composer: configured (guard_action keeps the judge's image action
        // only when the task exists) but FAILING — 500 on the whole chain, so
        // the compose fails open to an empty subject and the marker stays
        // empty exactly as the assertions below pin.
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("\"model\":\"composer\""))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"primary\"\n\
                 [tasks.pde_decision]\nmodel=\"pde/judge\"\nfilter_prompt=\"Decide the action and inner_state.\"\n\
                 [tasks.chat_image_prompt_compose]\nmodel = \"composer\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01J9111111111111111111111A",
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
                // Presence of the image block signals "consumer handles images
                // this turn"; with the composer task configured, guard_action
                // keeps the judge's reply_text_image. NOT forced — force now
                // means reply_image (spec 2026-08-03 §1).
                image: Some(crate::routes::companion_stream::ImageReplyParams::default()),
                history_anchor: Default::default(),
            },
            None,
        )
        .collect()
        .await;

        let types: Vec<String> = frames
            .iter()
            .map(|f| {
                serde_json::to_value(f).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        // meta(reply_text_image) → delta* → done → image_request → final.
        assert_eq!(types.first().map(String::as_str), Some("meta"), "{types:?}");
        assert_eq!(types.last().map(String::as_str), Some("final"), "{types:?}");
        assert!(
            types.iter().any(|t| t == "delta"),
            "text burst delta present: {types:?}"
        );
        let ir_pos = types
            .iter()
            .position(|t| t == "image_request")
            .expect("image_request present");
        let done_pos = types
            .iter()
            .position(|t| t == "done")
            .expect("done present");
        assert!(
            done_pos < ir_pos,
            "image_request comes after done: {types:?}"
        );
        assert_eq!(
            types[ir_pos + 1],
            "final",
            "image_request immediately before final"
        );
        assert_eq!(
            types
                .iter()
                .filter(|t| t.as_str() == "image_request")
                .count(),
            1,
            "exactly one image_request"
        );
        let action = frames
            .iter()
            .find_map(|f| match f {
                ProtocolFrame::Meta { action_type, .. } => Some(*action_type),
                _ => None,
            })
            .expect("meta present");
        assert_eq!(action, FrameActionType::ReplyTextImage);

        // The minimal marker was MERGED onto the assistant TEXT row (content
        // non-empty), carrying only the empty subject (the composer chain
        // failed ⇒ fail-open empty-subject portrait fallback; no seed left).
        let row: (String, Option<serde_json::Value>) = sqlx::query_as(
            "SELECT content, metadata FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant' \
             ORDER BY sent_at DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!row.0.is_empty(), "the text reply row has content");
        let img = row.1.expect("row has metadata")["image"].clone();
        assert_eq!(img["prompt"], "");
        assert!(img.get("model").is_none(), "marker must not store a model");
        assert!(
            img.get("generation_id").is_none(),
            "marker must not store a generation id"
        );
        assert!(img.get("url").is_none(), "marker must not store a url");
    }

    /// Review finding (2026-08-02, issue #212 fix wave): the sibling test above
    /// gives the composer a failing (500) mock, so the concurrently spawned
    /// compose task returns almost instantly — the join at the end of the
    /// burst is trivially satisfied and proves nothing about ordering under a
    /// REAL in-flight call. This test gives the composer's mocked response a
    /// delay that outlasts the (instant, mocked) chat burst, so the join at
    /// `compose_handle.join().await`
    /// genuinely waits on a still-running task — the actual race the concurrent
    /// spawn in `run_stream` is meant to survive. Asserts the wire frame order
    /// still holds (`meta → delta* → done → image_request → final`) and that
    /// the `image_request` frame plus the persisted marker carry the racing
    /// composer's real output, not the empty-subject portrait fallback.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn reply_text_image_concurrent_composer_races_chat_burst(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // Chat burst: streams back immediately, no delay. The composer mock
        // below is delayed well past this, so by the time `run_stream` reaches
        // the join point the compose task is still in flight — the join must
        // actually wait on it rather than observe an already-resolved handle.
        let chat_body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"I would absolutely love that for you, \"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"let me slip into something far more comfortable and show you every bit of it\"}}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":9,\"total_tokens\":11},\"id\":\"gen-r\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        // Route the two calls by the MODEL ID present in the request body so the
        // two mocks are MUTUALLY EXCLUSIVE (mount order/precedence cannot matter):
        // chat call body contains "primary"; composer call body contains "composer".
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("\"model\":\"primary\""))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(chat_body, "text/event-stream"),
            )
            .mount(&mock)
            .await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("\"model\":\"composer\""))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(150))
                    .set_body_json(serde_json::json!({
                        "id": "gen-compose-race",
                        "model": "served/composer-model",
                        "choices": [{"message": {"content":
                            r#"{"prompt":"CONCURRENT COMPOSED SUBJECT","caption":"并发合成的图片"}"#
                        }}],
                    })),
            )
            .mount(&mock)
            .await;
        // Judge: instant reply_text_image verdict — the only road to the
        // text+image action now that `force` always means reply_image.
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("\"model\":\"pde/judge\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content":
                    "{\"action\":\"reply_text_image\",\"inner_state\":\"想给你看\"}"}}],
            })))
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"primary\"\n\
                 [tasks.pde_decision]\nmodel=\"pde/judge\"\nfilter_prompt=\"Decide the action and inner_state.\"\n\
                 [tasks.chat_image_prompt_compose]\nmodel = \"composer\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01J9222222222222222222222A",
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
                // Presence of the image block signals "consumer handles images
                // this turn"; with the composer task configured, guard_action
                // keeps the judge's reply_text_image. NOT forced — force now
                // means reply_image (spec 2026-08-03 §1).
                image: Some(crate::routes::companion_stream::ImageReplyParams::default()),
                history_anchor: Default::default(),
            },
            None,
        )
        .collect()
        .await;

        let reqs = mock.received_requests().await.expect("recorded requests");
        assert_eq!(
            reqs.len(),
            3,
            "reply_text_image makes exactly three provider calls: judge + chat + composer, {reqs:?}"
        );

        let types: Vec<String> = frames
            .iter()
            .map(|f| {
                serde_json::to_value(f).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        // meta(reply_text_image) → delta* → done → image_request → final,
        // preserved even though the composer is still running when the chat
        // burst's `done` frame is emitted.
        assert_eq!(types.first().map(String::as_str), Some("meta"), "{types:?}");
        assert_eq!(types.last().map(String::as_str), Some("final"), "{types:?}");
        assert!(
            types.iter().any(|t| t == "delta"),
            "text burst delta present: {types:?}"
        );
        let ir_pos = types
            .iter()
            .position(|t| t == "image_request")
            .expect("image_request present");
        let done_pos = types
            .iter()
            .position(|t| t == "done")
            .expect("done present");
        assert!(
            done_pos < ir_pos,
            "image_request comes after done even with a real in-flight composer: {types:?}"
        );
        assert_eq!(
            types[ir_pos + 1],
            "final",
            "image_request immediately before final"
        );
        assert_eq!(
            types
                .iter()
                .filter(|t| t.as_str() == "image_request")
                .count(),
            1,
            "exactly one image_request"
        );

        // The image_request frame carries the COMPOSER'S output (the wire
        // prompt embeds the enriched subject) — not the empty-subject portrait
        // fallback the no-composer sibling test exercises.
        let composed_b64 = frames
            .iter()
            .find_map(|f| match f {
                ProtocolFrame::ImageRequest {
                    composed_prompt, ..
                } => Some(composed_prompt.clone()),
                _ => None,
            })
            .expect("image_request present");
        let composed = {
            use base64::Engine as _;
            String::from_utf8(
                base64::engine::general_purpose::STANDARD
                    .decode(&composed_b64)
                    .unwrap(),
            )
            .unwrap()
        };
        assert!(
            composed.contains("CONCURRENT COMPOSED SUBJECT"),
            "composed wire prompt must carry the racing composer's subject, got {composed}"
        );

        // The marker MERGED onto the assistant text row carries the composer's
        // actual subject/caption/audit trio — proof the join actually picked up
        // the still-in-flight task's result rather than racing past it.
        let row: (String, Option<serde_json::Value>) = sqlx::query_as(
            "SELECT content, metadata FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant' \
             ORDER BY sent_at DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!row.0.is_empty(), "the text reply row has content");
        let img = row.1.expect("row has metadata")["image"].clone();
        assert_eq!(img["prompt"], "CONCURRENT COMPOSED SUBJECT");
        assert_eq!(img["caption"], "并发合成的图片");
        assert_eq!(img["compose_model"], "served/composer-model");
        assert_eq!(img["compose_generation_id"], "gen-compose-race");
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
                history_anchor: Default::default(),
            },
            None,
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
            channel: None,
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
                history_anchor: Default::default(),
            },
            None,
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
                history_anchor: Default::default(),
            },
            None,
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
                history_anchor: Default::default(),
            },
            None,
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
                history_anchor: Default::default(),
            },
            None,
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
                history_anchor: Default::default(),
            },
            None,
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
                history_anchor: Default::default(),
            },
            None,
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
                history_anchor: Default::default(),
            },
            None,
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
                history_anchor: Default::default(),
            },
            None,
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
                history_anchor: Default::default(),
            },
            None,
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
                history_anchor: Default::default(),
            },
            None,
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
                history_anchor: Default::default(),
            },
            None,
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
                history_anchor: Default::default(),
            },
            None,
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
                history_anchor: Default::default(),
            },
            None,
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
    // (`[亲密度]`); the chat body carries the chat model id (`deepseek/x`). Those
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
                history_anchor: Default::default(),
            },
            None,
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
            judge_sent.contains("pde/judge") && judge_sent.contains("[亲密度]"),
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
                history_anchor: Default::default(),
            },
            None,
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
        assert!(
            !chat_sent.contains("[reply_tone]"),
            "a verdict without tone must not render a [reply_tone] section; got {chat_sent}",
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_stream_pde_judge_product_qa_routes_to_dedicated_executor(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // Judge ("pde/judge"): a `product_qa` verdict, empty inner_state.
        let verdict = serde_json::json!({ "action": "product_qa", "inner_state": "" }).to_string();
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

        // Product-QA executor ("qa/exec"): streams the out-of-character answer.
        let qa_body = "data: {\"choices\":[{\"delta\":{\"content\":\"这是产品说明\"}}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":5,\"total_tokens\":8},\"id\":\"gen-qa\",\"model\":\"qa/exec\"}\n\ndata: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("qa/exec"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(qa_body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // Companion chat ("deepseek/x"): MUST NOT be called — a product_qa verdict
        // skips the entire companion chain. `.expect(0)` fails the test on any hit.
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

        let mut state = crate::routes::companion::test_state(pool.clone());
        // PDE ON (routes the judge) + chat_product_qa ON (routes the executor).
        // Both need a non-blank filter_prompt to resolve to `Some`.
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel=\"deepseek/x\"\n\
                 [tasks.pde_decision]\nmodel=\"pde/judge\"\nfilter_prompt=\"Decide the action and inner_state.\"\n\
                 [tasks.chat_product_qa]\nmodel=\"qa/exec\"\nfilter_prompt=\"Answer using the product docs below.\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "这个产品支持退货吗",
                "01JPDEQA0000000000000000A",
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
                content: "这个产品支持退货吗".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
                image_url: None,
                image: None,
                history_anchor: Default::default(),
            },
            None,
        )
        .collect()
        .await;

        // frame order: meta(product_qa) → delta+ → done → final
        assert!(
            matches!(
                &frames[0],
                ProtocolFrame::Meta {
                    action_type: FrameActionType::ProductQa,
                    ..
                }
            ),
            "first frame must be Meta{{action_type: ProductQa}}, got {frames:?}",
        );
        let types: Vec<String> = frames
            .iter()
            .map(|f| {
                serde_json::to_value(f).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            types,
            ["meta", "delta", "done", "final"],
            "product_qa sequence, got {frames:?}"
        );

        // The companion chat mock's `.expect(0)` already proves the companion call
        // never fired; belt-and-suspenders: only the judge + executor calls landed.
        let reqs = mock.received_requests().await.unwrap();
        assert_eq!(
            reqs.len(),
            2,
            "exactly two upstream calls (judge + product_qa executor); got {} calls",
            reqs.len(),
        );

        // rows: user row marked, assistant row marked + linked.
        let user_ch: Option<String> =
            sqlx::query_scalar("SELECT channel FROM engine.chat_messages WHERE id = $1")
                .bind(user_message_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(user_ch.as_deref(), Some("product_qa"));

        let (a_ch, a_action): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT channel, assistant_action_type FROM engine.chat_messages \
             WHERE user_message_id = $1 AND role = 'assistant'",
        )
        .bind(user_message_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(a_ch.as_deref(), Some("product_qa"));
        assert_eq!(a_action.as_deref(), Some("reply"));

        // post_process did not run: no affinity event rows for this turn. The
        // events table has no session_id column, so join through the affinity
        // row (mirrors post_process.rs's own test query).
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.companion_affinity_events e \
             JOIN engine.companion_affinity a ON a.id = e.affinity_id \
             WHERE a.session_id = $1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 0, "product_qa must skip post_process / affinity events");

        // Decision audit recorded the action. The write is `tokio::spawn`ed
        // fire-and-forget (see the ghost/reply tests above), so poll briefly
        // for it to land rather than asserting immediately.
        let mut decision_row: Option<(Option<String>, Option<String>)> = None;
        for _ in 0..50 {
            if let Ok(row) = sqlx::query_as::<_, (Option<String>, Option<String>)>(
                "SELECT proposed_action, action FROM engine.companion_decision_events \
                 WHERE message_id = $1",
            )
            .bind(user_message_id)
            .fetch_one(&pool)
            .await
            {
                decision_row = Some(row);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let (proposed, acted) =
            decision_row.expect("companion_decision_events row must land within timeout");
        assert_eq!(proposed.as_deref(), Some("product_qa"));
        assert_eq!(acted.as_deref(), Some("product_qa"));
    }

    /// Spec §6 failure path: "executor exhausted → fallback text emitted AND
    /// persisted with the channel marker." Both product_qa candidates fail to
    /// produce usable content — the primary 500s outright, the fallback opens
    /// a 200 stream but only ever emits a metadata chunk (usage/model/id) with
    /// an EMPTY delta before `[DONE]`, mirroring a real OpenRouter completion
    /// that reports usage without content. That second shape is deliberate:
    /// it's exactly the "final candidate streamed metadata but zero content"
    /// case the stale-audit-trio bug (Fix 2) was about — `last_usage`/
    /// `last_gen_id`/`served_model` get set from that chunk even though `acc`
    /// stays empty, so a naive persist would leak a real
    /// generation_id/model/usage onto a row whose content is actually the
    /// canned error_handling phrase. The fallback phrase itself is pinned to
    /// a single deterministic entry (migration 0020 seeds 10 and picks at
    /// random) so the test can assert on exact content.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_stream_product_qa_executor_exhausted_persists_fallback_phrase(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const FALLBACK_PHRASE: &str = "稍后再答你";

        let mock = MockServer::start().await;

        // Judge ("pde/judge"): a `product_qa` verdict, empty inner_state — same
        // routing setup as the happy-path E2E above.
        let verdict = serde_json::json!({ "action": "product_qa", "inner_state": "" }).to_string();
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

        // Primary product-QA executor ("qa/exec-a"): hard failure, HTTP 500.
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("qa/exec-a"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        // Fallback product-QA executor ("qa/exec-b"), last in the chain: opens
        // fine (200) but the only SSE frame is metadata-only — usage/model/id
        // set, `delta` empty — before `[DONE]`. `acc` stays empty ⇒ chain
        // exhausted, but `last_usage`/`last_gen_id`/`served_model` are left
        // holding real values from this candidate.
        let exhausted_body = "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":0,\"total_tokens\":3},\"id\":\"gen-exhausted\",\"model\":\"qa/exec-b\"}\n\ndata: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("qa/exec-b"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(exhausted_body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // Companion chat ("deepseek/x"): MUST NOT be called — product_qa never
        // degrades to the companion chain, even on total executor failure.
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

        // Pin the error_handling fallback phrase to a single deterministic
        // entry (the seeded migration row has 10, picked at random) so the
        // Delta content / row content assertions below are exact-match.
        sqlx::query(
            "UPDATE engine.error_handling_config \
             SET payload = $1 \
             WHERE kind = 'chat_stream_failure_fallback_phrases'",
        )
        .bind(serde_json::json!([FALLBACK_PHRASE]))
        .execute(&pool)
        .await
        .unwrap();

        let mut state = crate::routes::companion::test_state(pool.clone());
        // PDE ON (routes the judge) + chat_product_qa ON (routes the executor)
        // with a two-candidate chain (primary + one fallback), both of which
        // must fail before the pseudo-ghost path fires.
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel=\"deepseek/x\"\n\
                 [tasks.pde_decision]\nmodel=\"pde/judge\"\nfilter_prompt=\"Decide the action and inner_state.\"\n\
                 [tasks.chat_product_qa]\nmodel=\"qa/exec-a\"\nfallback=[\"qa/exec-b\"]\nretry_depth=1\n\
                 filter_prompt=\"Answer using the product docs below.\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "这个产品能用几年",
                "01JPDEQAEXHAUSTED000000001",
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
                content: "这个产品能用几年".into(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
                image_url: None,
                image: None,
                history_anchor: Default::default(),
            },
            None,
        )
        .collect()
        .await;

        // frame order: meta(product_qa) → delta(phrase) → done → final.
        assert!(
            matches!(
                &frames[0],
                ProtocolFrame::Meta {
                    action_type: FrameActionType::ProductQa,
                    ..
                }
            ),
            "first frame must be Meta{{action_type: ProductQa}}, got {frames:?}",
        );
        let types: Vec<String> = frames
            .iter()
            .map(|f| {
                serde_json::to_value(f).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            types,
            ["meta", "delta", "done", "final"],
            "exhausted-chain product_qa sequence, got {frames:?}"
        );

        let delta_text: String = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            delta_text, FALLBACK_PHRASE,
            "the Delta frame must carry the seeded error_handling phrase verbatim"
        );

        // Done frame: the audit trio must be None — no leaked generation_id/
        // model/usage from the exhausted fallback candidate's metadata-only
        // chunk (Fix 2).
        let done = frames
            .iter()
            .find_map(|f| match f {
                ProtocolFrame::Done {
                    usage,
                    generation_id,
                    ..
                } => Some((usage.clone(), generation_id.clone())),
                _ => None,
            })
            .expect("a Done frame");
        assert_eq!(
            done,
            (None, None),
            "Done frame must carry usage:None, generation_id:None on chain exhaustion, got {done:?}"
        );

        // Assistant row: channel marker + phrase content + null audit trio.
        let (content, channel, model, usage, generation_id): (
            String,
            Option<String>,
            Option<String>,
            Option<serde_json::Value>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT content, channel, model, usage, generation_id FROM engine.chat_messages \
             WHERE user_message_id = $1 AND role = 'assistant'",
        )
        .bind(user_message_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(content, FALLBACK_PHRASE);
        assert_eq!(channel.as_deref(), Some("product_qa"));
        assert_eq!(
            model, None,
            "the fallback row must not carry the exhausted candidate's model"
        );
        assert_eq!(
            usage, None,
            "the fallback row must not carry the exhausted candidate's usage"
        );
        assert_eq!(
            generation_id, None,
            "the fallback row must not carry the exhausted candidate's generation_id"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_stream_pde_judge_reply_injects_reply_tone(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // Judge ("pde/judge"): a `reply_text` verdict carrying BOTH an
        // inner_state and a tone. Both are plain prose (no headers/brackets)
        // so they survive `sanitize_inner_state` unchanged and land in the
        // prompt's `[inner_state]` / `[reply_tone]` sections via
        // `pde::plan_for` → `build_prompt`.
        let verdict = serde_json::json!({
            "action": "reply_text",
            "inner_state": "有点开心",
            "tone": "撒娇一点，句子短一点"
        })
        .to_string();
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
        // we capture its request body afterward to assert the injected tone.
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
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let chat_repo = ChatRepo { pool: &pool };
        let umid = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "你今天怎么样",
                "01JPDETONE0000000000000000",
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
                history_anchor: Default::default(),
            },
            None,
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
        assert!(
            chat_sent.contains("[reply_tone]")
                && chat_sent.contains("这一轮回复的语气：撒娇一点，句子短一点。"),
            "the judge's tone must be injected as [reply_tone] in the chat system prompt; got {chat_sent}",
        );
        assert!(
            chat_sent.contains("[inner_state]") && chat_sent.contains("有点开心"),
            "inner_state still injected alongside tone; got {chat_sent}",
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
                history_anchor: Default::default(),
            },
            None,
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
                history_anchor: Default::default(),
            },
            None,
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
                history_anchor: Default::default(),
            },
            None,
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

    /// Codex P2 (PR #141): a NON-last empty completion in LIVE mode is a
    /// superseded (truncated) attempt that advances the chain — NOT a spurious
    /// successful empty turn, and NOT a ghost. Only the LAST empty attempt is the
    /// ghost fallback; here a later model replies, so there is no ghost at all.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn live_nonlast_empty_completion_advances_as_truncated(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_partial_json, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        // Primary "e/x" returns a 200 stream with NO content (empty completion);
        // fallback "f/x" streams clean text.
        let empty = "data: {\"choices\":[{\"delta\":{}}],\"id\":\"gen-e\",\"model\":\"e/x\"}\n\ndata: [DONE]\n\n";
        let clean = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi there\"}}],",
            "\"id\":\"gen-f\",\"model\":\"f/x\"}\n\n",
            "data: [DONE]\n\n"
        );
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "e/x"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(empty, "text/event-stream"),
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
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        let state = std::sync::Arc::new(state);

        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01JEMPTYADVANCE000000000A",
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
            model: "e/x".into(),
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

        // One Done per attempt: the non-last empty attempt is truncated (a
        // superseded "replace me" signal), NOT a ghost; the clean fallback is a
        // normal accepted reply.
        let dones: Vec<(bool, bool)> = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Done {
                    truncated,
                    ghost_fallback,
                    ..
                } => Some((*truncated, *ghost_fallback)),
                _ => None,
            })
            .collect();
        assert_eq!(dones.len(), 2, "one Done per attempt: {frames:?}");
        assert_eq!(
            dones[0],
            (true, false),
            "non-last empty attempt must be truncated, never a spurious success or ghost: {frames:?}"
        );
        assert_eq!(
            dones[1],
            (false, false),
            "the clean fallback is a normal accepted reply: {frames:?}"
        );
        assert!(
            !frames.iter().any(|f| matches!(
                f,
                ProtocolFrame::Done {
                    ghost_fallback: true,
                    ..
                }
            )),
            "no ghost fallback when a later model replies: {frames:?}"
        );
        assert!(
            frames.iter().any(
                |f| matches!(f, ProtocolFrame::Delta { content, .. } if content == "hi there")
            ),
            "the clean fallback text must be delivered: {frames:?}"
        );
        let produced = &outcome.lock().unwrap().produced;
        assert_eq!(
            produced.len(),
            1,
            "only the accepted fallback remains in produced; got {produced:?}"
        );
        assert_eq!(produced[0].full_text, "hi there");
    }

    /// Stream-hardening A1: a mid-generation `finish_reason:"content_filter"`
    /// (Gemini/OpenAI safety cut) is an incomplete reply. It must ride the same
    /// truncation → chain-advance path as "length" — never persist as a clean
    /// success — restoring parity with the sync path's filter_output_invalidity
    /// gate (production chat is 100% streaming).
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn live_content_filter_finish_advances_chain_as_truncated(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_partial_json, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        // Primary "cf/x" streams partial text then a content_filter cut;
        // fallback "f/x" streams clean text.
        let cut = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"部分回\"}}],",
            "\"id\":\"gen-cf\",\"model\":\"cf/x\"}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let clean = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi there\"}}],",
            "\"id\":\"gen-f\",\"model\":\"f/x\"}\n\n",
            "data: [DONE]\n\n"
        );
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "cf/x"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(cut, "text/event-stream"),
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
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        let state = std::sync::Arc::new(state);

        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01JCONTENTFILTER00000000A",
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
            model: "cf/x".into(),
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

        let dones: Vec<(bool, bool)> = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Done {
                    truncated,
                    ghost_fallback,
                    ..
                } => Some((*truncated, *ghost_fallback)),
                _ => None,
            })
            .collect();
        assert_eq!(dones.len(), 2, "one Done per attempt: {frames:?}");
        assert_eq!(
            dones[0],
            (true, false),
            "content_filter cut must be truncated (replace-me), not a clean success: {frames:?}"
        );
        assert_eq!(
            dones[1],
            (false, false),
            "the clean fallback is a normal accepted reply: {frames:?}"
        );
        let produced = &outcome.lock().unwrap().produced;
        assert_eq!(
            produced.len(),
            1,
            "only the accepted fallback feeds post-process; got {produced:?}"
        );
        assert_eq!(
            produced[0].full_text, "hi there",
            "the safety-cut partial must never reach memory/insight/affinity"
        );
    }

    /// Codex P2 (PR #141, round 3): the FILTERED empty-completion ghost fallback
    /// must retain an (empty) produced row like the live/regex-strip paths —
    /// otherwise a ReplyTextImage turn's trailing image_request (gated on
    /// `produced.last()`) silently drops the image half in filtered mode only.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn filtered_empty_completion_ghost_retains_produced_row(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let body = "data: {\"choices\":[{\"delta\":{}}],\"id\":\"gen-e\",\"model\":\"primary\"}\n\ndata: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let (_g, _instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        // Never-matching regex targeting "primary" forces FILTERED mode.
        let regex_cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(
            r#"
            [tasks.chat_companion]
            model = "primary"

            [[tasks.chat_companion.output_regex]]
            models = ["primary"]
            pattern = '^THIS_PATTERN_NEVER_MATCHES_ANYTHING$'
            "#,
        )
        .unwrap();
        state.output_regex =
            std::sync::Arc::new(regex_cfg.compile_output_regex().expect("compiles"));
        let state = std::sync::Arc::new(state);

        let chat_repo = ChatRepo { pool: &pool };
        let umid = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01JFILTEREDIMG0000000000A",
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
            model: "primary".into(),
            fallback_model: vec![],
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
            umid,
            FrameActionType::ReplyTextImage,
            "reply",
            ActionType::ReplyTextImage,
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

        assert!(
            frames.iter().any(|f| matches!(
                f,
                ProtocolFrame::Done {
                    ghost_fallback: true,
                    ..
                }
            )),
            "filtered empty completion must ghost: {frames:?}"
        );
        let produced = &outcome.lock().unwrap().produced;
        assert_eq!(
            produced.len(),
            1,
            "filtered empty-completion ghost must retain a produced row so ReplyTextImage's \
             image_request still fires; got {produced:?}"
        );
        assert_eq!(
            produced[0].full_text, "",
            "the retained produced row is empty (memory/insight/eval-neutral)"
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

    fn fixture_decision_input() -> eros_engine_core::types::DecisionInput {
        use eros_engine_core::types::{DecisionInput, Event};
        DecisionInput {
            event: Event::UserMessage {
                content: "在吗".into(),
                message_id: Uuid::new_v4(),
                prompt_traits: vec![],
                audit: None,
                tier: None,
                memory_scope: Default::default(),
                affinity_scope: Default::default(),
                tips_amount_usd: None,
                history_anchor: Default::default(),
            },
            affinity: test_affinity(),
            persona: test_persona(),
            signals: test_signals(),
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
                history_anchor: Default::default(),
            },
            affinity: test_affinity(),
            persona: p,
            signals: test_signals(),
        };
        let t = JudgeTranscript {
            transcript: "用户：hi\nMia：hey".into(),
            ..Default::default()
        };
        let ctx = build_pde_ctx(&t, &input, true, None);
        let persona_at = ctx.find("[角色人格]").expect("persona block present");
        let rel_at = ctx.find("[亲密度]").expect("relationship bucket present");
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

    /// Both engine-owned bucket lines render before [信号], as labels ONLY —
    /// affinity 3.0 removed every number from the judge's payload (the六轴
    /// [关系状态] line and the bond/chemistry parenthetical): the buckets are
    /// authoritative, and the numeric state lives in the decision row's
    /// `inputs` snapshot instead.
    #[test]
    fn pde_ctx_renders_buckets_only_no_numbers() {
        let mut input = fixture_decision_input();
        input.affinity.warmth = 0.9;
        input.affinity.trust = 0.9;
        input.affinity.intrigue = 0.9;
        input.affinity.intimacy = 0.0;
        input.affinity.tension = 0.0; // bond 0.90, chemistry 0.30
        input.affinity.patience = 0.8;
        let ctx = build_pde_ctx(&JudgeTranscript::default(), &input, true, None);
        assert!(
            ctx.contains("[亲密度] 当前档位=第 3 档\n"),
            "intimacy line carries the rung and nothing else: {ctx}"
        );
        assert!(
            ctx.contains("[耐心] 当前档位=高"),
            "patience band line present: {ctx}"
        );
        assert!(
            !ctx.contains("[关系状态]") && !ctx.contains("warmth=") && !ctx.contains("bond="),
            "no numeric affinity state may reach the judge: {ctx}"
        );
        let rung_at = ctx.find("[亲密度]").expect("intimacy line present");
        let patience_at = ctx.find("[耐心]").expect("patience line present");
        let signal_at = ctx.find("[信号]").expect("signal block present");
        assert!(
            rung_at < patience_at && patience_at < signal_at,
            "buckets sit before [信号]: {ctx}"
        );
    }

    /// The rendered buckets follow the affinity, rather than being pinned to one
    /// value. The rung's own cuts belong to the core crate — the composites are
    /// a `/3` fold, so exact edge values are not reachable from here — but the
    /// patience band reads a raw axis, so its edges render exactly.
    #[test]
    fn pde_ctx_bucket_lines_track_the_thresholds() {
        let mut input = fixture_decision_input();
        for (score, want) in [(0.05, 1), (0.5, 2), (0.95, 3)] {
            input.affinity.warmth = score;
            input.affinity.trust = score;
            input.affinity.intrigue = score;
            input.affinity.intimacy = 0.0;
            input.affinity.tension = 0.0;
            let ctx = build_pde_ctx(&JudgeTranscript::default(), &input, true, None);
            assert!(
                ctx.contains(&format!("当前档位=第 {want} 档")),
                "S={score} renders rung {want}: {ctx}"
            );
        }
        for (patience, want) in [(0.349, "低"), (0.35, "中"), (0.649, "中"), (0.65, "高")] {
            input.affinity.patience = patience;
            let ctx = build_pde_ctx(&JudgeTranscript::default(), &input, true, None);
            assert!(
                ctx.contains(&format!("[耐心] 当前档位={want}")),
                "patience={patience} renders {want}: {ctx}"
            );
        }
    }

    /// Unconditional: a brand-new session renders 第 1 档 rather than omitting
    /// the line, so an absent line can never be read as the bottom rung.
    #[test]
    fn pde_ctx_renders_bucket_lines_for_a_fresh_session() {
        let mut input = fixture_decision_input();
        // migration-0029 seed: every axis at 0.033.
        input.affinity.warmth = 0.033;
        input.affinity.trust = 0.033;
        input.affinity.intrigue = 0.033;
        input.affinity.intimacy = 0.033;
        input.affinity.tension = 0.033;
        input.affinity.patience = 0.5;
        let ctx = build_pde_ctx(&JudgeTranscript::default(), &input, false, None);
        assert!(
            ctx.contains("[亲密度] 当前档位=第 1 档"),
            "fresh session renders the bottom rung: {ctx}"
        );
        assert!(
            ctx.contains("[耐心] 当前档位=中"),
            "patience line renders too: {ctx}"
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
                history_anchor: Default::default(),
            },
            affinity: test_affinity(),
            persona: p,
            signals: test_signals(),
        };
        let ctx = build_pde_ctx(&JudgeTranscript::default(), &input, false, None);
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

    #[test]
    fn pde_ctx_renders_product_qa_blocks_only_when_enabled() {
        let input = pde_test_input();
        let t = JudgeTranscript {
            transcript: "t".into(),
            ..Default::default()
        };
        // feature off → no lines at all
        let off = build_pde_ctx(&t, &input, true, None);
        assert!(!off.contains("[产品咨询]"));
        assert!(!off.contains("[最近产品咨询]"));
        // on, no history → availability line only
        let on_empty = build_pde_ctx(&t, &input, true, Some(""));
        assert!(on_empty.contains("[产品咨询] 本轮可答产品问题=是"));
        assert!(!on_empty.contains("[最近产品咨询]"));
        // on, with history → both blocks, before [用户最新消息]
        let recent = render_product_qa_pairs(&[("这是什么".into(), "这是……".into())]);
        let on_recent = build_pde_ctx(&t, &input, true, Some(&recent));
        assert!(on_recent.contains("[最近产品咨询]\n用户: 这是什么\n回答: 这是……"));
        assert!(on_recent.find("[产品咨询]").unwrap() < on_recent.find("[用户最新消息]").unwrap());
    }

    // ── Task-4 PDE schema + chain-walk tests ─────────────────────────────────

    #[test]
    fn pde_response_format_schema_shape() {
        let v = pde_response_format();
        assert_eq!(v["type"], "json_schema");
        assert_eq!(v["json_schema"]["name"], "pde_verdict");
        assert_eq!(v["json_schema"]["strict"], true);
        let req = v["json_schema"]["schema"]["required"].as_array().unwrap();
        assert_eq!(req.len(), 6, "all six properties required: {v}");
        assert!(
            req.iter().any(|x| x == "image_ref"),
            "image_ref required: {v}"
        );
        assert!(req.iter().any(|x| x == "tone"), "tone required: {v}");
        assert_eq!(
            v["json_schema"]["schema"]["properties"]["tone"]["type"],
            serde_json::json!(["string", "null"]),
            "tone is nullable for strict providers: {v}"
        );
        assert!(
            req.iter().any(|x| x == "aspect_ratio"),
            "aspect_ratio required: {v}"
        );
        let actions = v["json_schema"]["schema"]["properties"]["action"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(actions.len(), 5, "five actions: {v}");
        assert!(
            actions.iter().any(|x| x == "product_qa"),
            "product_qa in action enum: {v}"
        );
    }

    #[test]
    fn pde_response_format_has_no_image_prompt() {
        let f = pde_response_format();
        let schema = &f["json_schema"]["schema"];
        let required = schema["required"].as_array().expect("required array");
        assert!(
            !required.iter().any(|v| v == "image_prompt"),
            "the judge must not be asked for an image prompt: {required:?}"
        );
        assert!(
            schema["properties"].get("image_prompt").is_none(),
            "image_prompt must be gone from properties"
        );
        // The two enums the judge still owns stay.
        assert!(schema["properties"].get("image_ref").is_some());
        assert!(schema["properties"].get("aspect_ratio").is_some());
    }

    #[test]
    fn parse_pde_verdict_ignores_stray_image_prompt() {
        // A non-strict provider may still emit the old key; it must not break parsing.
        let j = r#"{"action":"reply_image","inner_state":"x","image_prompt":"leftover","image_ref":"previous","aspect_ratio":"9:16"}"#;
        let v = parse_pde_verdict(j).expect("verdict parses despite the stray key");
        assert_eq!(v.action, PdeAction::ReplyImage);
        assert_eq!(v.aspect_ratio.as_deref(), Some("9:16"));
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
            sampling: Default::default(),
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
            format!("{}/api/v1/chat/completions", mock.uri()),
        );
        let p = test_resolved_pde(vec!["model-a".into(), "model-b".into()]);
        let run = run_pde_decision(&client, &p, "ctx").await;
        assert_eq!(run.status, PdeStatus::Ok);
        assert_eq!(run.verdict.unwrap().action, PdeAction::ReplyText);
        assert_eq!(run.model.as_deref(), Some("model-b"));
    }

    #[tokio::test]
    async fn pde_request_puts_configured_sampling_on_the_wire() {
        // Issue #246 end-to-end lock. ChatRequest derives Default and every
        // call-site literal ends in `..Default::default()`, so a site that
        // forgets `sampling` still COMPILES and silently sends nothing — the
        // compiler cannot catch this class. Assert the outbound body instead.
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "{\"action\":\"reply_text\",\"inner_state\":\"x\"}"}}],
                "id": "g", "model": "m"
            })))
            .mount(&mock)
            .await;

        let client = eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
            "k".into(),
            format!("{}/api/v1/chat/completions", mock.uri()),
        );
        let mut p = test_resolved_pde(vec!["model-a".into()]);
        p.sampling = eros_engine_llm::model_config::Sampling {
            top_p: Some(0.55),
            frequency_penalty: Some(0.35),
            presence_penalty: Some(0.15),
            repetition_penalty: Some(1.25),
        };
        let run = run_pde_decision(&client, &p, "ctx").await;
        assert_eq!(run.status, PdeStatus::Ok);

        let sent: serde_json::Value = mock
            .received_requests()
            .await
            .expect("recorded requests")
            .first()
            .expect("one request")
            .body_json()
            .expect("body is json");
        assert_eq!(sent["top_p"], 0.55);
        assert_eq!(sent["frequency_penalty"], 0.35);
        assert_eq!(sent["presence_penalty"], 0.15);
        assert_eq!(sent["repetition_penalty"], 1.25);
    }

    #[tokio::test]
    async fn pde_unset_sampling_stays_off_the_wire() {
        // The other half of the contract: an untuned task must produce a
        // byte-identical body to before #246 — no key, not a default value.
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "{\"action\":\"reply_text\",\"inner_state\":\"x\"}"}}],
                "id": "g", "model": "m"
            })))
            .mount(&mock)
            .await;

        let client = eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
            "k".into(),
            format!("{}/api/v1/chat/completions", mock.uri()),
        );
        let p = test_resolved_pde(vec!["model-a".into()]);
        assert_eq!(
            run_pde_decision(&client, &p, "ctx").await.status,
            PdeStatus::Ok
        );

        let sent: serde_json::Value = mock
            .received_requests()
            .await
            .expect("recorded requests")
            .first()
            .expect("one request")
            .body_json()
            .expect("body is json");
        for k in [
            "top_p",
            "frequency_penalty",
            "presence_penalty",
            "repetition_penalty",
        ] {
            assert!(
                sent.get(k).is_none(),
                "unset {k} must not reach the wire: {sent}"
            );
        }
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

        // ── 1. Mock OpenRouter: returns "hello world" across TWO deltas ──
        let mock = MockServer::start().await;
        // Two separate content deltas. `\bNOPE\b` is a complex pattern (word
        // boundaries → Opaque in StreamScrubber), so the turn still buffers: a
        // multi-chunk stream must collapse to exactly ONE Delta bubble. (A
        // stream-safe pattern would emit per-chunk here — see
        // `regex_span_pattern_streams_live_across_chunks`.)
        let chat_body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hello \"}}],\"id\":\"gen-t5\",\"model\":\"mock/euryale\"}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"world\"}}],\
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
                history_anchor: Default::default(),
            },
            None,
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
        // `\bNOPE\b` is Opaque (word boundaries), so the turn buffers: the two
        // upstream deltas must collapse to exactly ONE Delta bubble. A
        // stream-safe pattern would have emitted two here — that divergence is
        // exactly what this asserts.
        assert_eq!(
            deltas.len(),
            1,
            "an Opaque-pattern turn must buffer to one Delta bubble; got {deltas:?}",
        );
        // Content is the raw reply, unchanged (pattern doesn't match).
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

    /// Batch C: a stream-safe SPAN pattern (the production `\[[^\]]*\]`) streams
    /// LIVE — the client gets multiple Delta bubbles as pre-artifact text
    /// arrives, the bracketed artifact is stripped mid-stream (no delta for it),
    /// and the persisted row + regex audit are byte-identical to the old
    /// buffered path. This is the whole point of Batch C: regex-targeted chat
    /// turns get TTFT back instead of buffering the whole reply.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn regex_span_pattern_streams_live_across_chunks(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        // Four separate content deltas; the third is the whole bracket artifact.
        let chat_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}],\"id\":\"gen-s\",\"model\":\"mock/euryale\"}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"呀\"}}],\"id\":\"gen-s\",\"model\":\"mock/euryale\"}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"[你给对方发送了一张照片：海边]\"}}],\"id\":\"gen-s\",\"model\":\"mock/euryale\"}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"今天如何\"}}],",
            "\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":6,\"total_tokens\":8},",
            "\"id\":\"gen-s\",\"model\":\"mock/euryale\"}\n\n",
            "data: [DONE]\n\n"
        );
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(chat_body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        let user_id = uuid::Uuid::new_v4();
        let (_g, _instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;
        let mut state = crate::routes::companion::test_state(pool.clone());
        // Production span pattern (unanchored bracket) → StreamScrubber Span.
        // Single-quoted TOML literal: backslashes reach `regex` verbatim, so the
        // Rust `\\` (one backslash) is exactly the regex escape wanted.
        let regex_cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel=\"mock/euryale\"\n\
             [[tasks.chat_companion.output_regex]]\n\
             models=[\"mock/euryale\"]\n\
             pattern='\\[[^\\]]*\\]'\n",
        )
        .unwrap();
        state.output_regex = std::sync::Arc::new(
            regex_cfg
                .compile_output_regex()
                .expect("span pattern compiles"),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        let state = std::sync::Arc::new(state);

        let chat_repo = ChatRepo { pool: &pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(
                session_id,
                "hi",
                "01JSPANSTREAM0000000000A",
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
            model: "mock/euryale".into(),
            fallback_model: vec![],
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
            None, // regex-only turn
            vec![],
            None,
            Default::default(),
            Default::default(),
            None,
            outcome.clone(),
        );
        let frames: Vec<ProtocolFrame> = Box::pin(burst).collect().await;

        let deltas: Vec<&str> = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();

        // LIVE: more than one Delta bubble (a buffered turn would emit exactly
        // one) — the pre-artifact text streamed per-chunk.
        assert!(
            deltas.len() > 1,
            "a span-pattern turn must stream multiple Delta bubbles, not buffer; got {deltas:?}",
        );
        // The bracket artifact never reaches the client; the visible text is the
        // cleaned reply.
        assert_eq!(
            deltas.concat(),
            "你好呀今天如何",
            "concatenated deltas must equal the artifact-stripped reply; got {deltas:?}",
        );
        assert!(
            !deltas.iter().any(|d| d.contains('[')),
            "no delta may carry the bracket artifact; got {deltas:?}",
        );

        // produced (extract/memory input) is the cleaned text.
        {
            let o = outcome.lock().unwrap();
            assert_eq!(
                o.produced.len(),
                1,
                "one produced message; got {:?}",
                o.produced
            );
            assert_eq!(o.produced[0].full_text, "你好呀今天如何");
            assert!(
                o.filtered,
                "outcome.filtered must be true when a rule fired"
            );
        }

        // DB row: cleaned content, raw on pre_filter_content, regex audit.
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
            content, "你好呀今天如何",
            "persisted content is the stripped text"
        );
        assert_eq!(
            pre_filter.as_deref(),
            Some("你好呀[你给对方发送了一张照片：海边]今天如何"),
            "pre_filter_content is the raw original",
        );
        assert_eq!(filter_model.as_deref(), Some("<regex>"));
        assert_eq!(
            filter_triggers,
            Some(serde_json::json!({ "regex": [0usize] }))
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

    /// When the reply is ENTIRELY the artifact (a bare `[...]` with nothing
    /// else), the strip empties it. There is no fail-safe: the client receives
    /// NO content bubble (no Delta), the row persists empty `content` (""), and
    /// the audit still records the strip (`pre_filter_content` = raw,
    /// `filter_model` = "<regex>"). Downstream renders the empty reply however
    /// it likes (the web client just doesn't show it — a ghost-like effect).
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn regex_artifact_only_reply_persists_empty_no_bubble(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // ── 1. Mock OpenRouter: reply is ONLY the bracket artifact ─────────────
        let mock = MockServer::start().await;
        let chat_body = "data: {\"choices\":[{\"delta\":{\"content\":\"[你给对方发送了一张照片：海边自拍]\"}}],\
\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":8,\"total_tokens\":10},\
\"id\":\"gen-bo\",\"model\":\"mock/cydonia\"}\n\n\
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

        // ── 3. AppState with a rule that drops any [...] for mock/cydonia ──────
        let mut state = crate::routes::companion::test_state(pool.clone());
        let regex_cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel=\"mock/cydonia\"\n\
             [[tasks.chat_companion.output_regex]]\n\
             models=[\"mock/cydonia\"]\n\
             pattern=\"\\\\[[^\\\\]]*\\\\]\"\n",
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
                "01JT5REGEXBONLY0000000000C",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        // ── 5. Drive drive_chat_burst (ReplyText, no LLM filter) ───────────────
        let req = eros_engine_llm::openrouter::ChatRequest {
            model: "mock/cydonia".into(),
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
        // No error frame, and crucially NO Delta (no content bubble reaches the client).
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "no error frame expected; got {frames:?}",
        );
        let deltas: Vec<&str> = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            deltas.is_empty(),
            "an artifact-only reply must emit NO Delta bubble; got {deltas:?}",
        );

        // The strip fired (filtered=true) and extract sees the empty text.
        {
            let o = outcome.lock().unwrap();
            assert!(o.filtered, "outcome.filtered must be true: the strip ran");
            assert_eq!(
                o.produced.len(),
                1,
                "one produced message; got {:?}",
                o.produced
            );
            assert_eq!(
                o.produced[0].full_text, "",
                "extract/memory must see the empty (stripped) text",
            );
        }

        // DB row: content == "" (empty, not the raw artifact); audit recorded.
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
            content, "",
            "persisted content must be empty; got {content:?}"
        );
        assert_eq!(
            pre_filter.as_deref(),
            Some("[你给对方发送了一张照片：海边自拍]"),
            "pre_filter_content must hold the raw artifact; got {pre_filter:?}",
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
            sampling: Default::default(),
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

    /// When the regex strip empties the WHOLE reply (artifact-only) AND an LLM
    /// `output_filter` is configured and fires, the LLM filter must be SKIPPED:
    /// handing "" to a rewrite model could resurrect a bubble. The client sees
    /// no Delta, the row persists empty `content`, the audit stays the regex
    /// one (`filter_model = "<regex>"`), and the filter model is never called.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn regex_strip_to_empty_skips_llm_filter(pool: PgPool) {
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
        use futures_util::StreamExt;
        use wiremock::matchers::{body_string_contains, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let raw_reply = "[你给对方发送了一张照片：海边自拍]"; // artifact-only

        // ── 1. Dual mock: chat model (SSE, artifact-only) + filter model (JSON). ─
        let mock = MockServer::start().await;
        let chat_body = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{raw_reply}\"}}}}],\
\"usage\":{{\"prompt_tokens\":2,\"completion_tokens\":8,\"total_tokens\":10}},\
\"id\":\"gen-skip\",\"model\":\"mock/euryale\"}}\n\n\
data: [DONE]\n\n"
        );
        // The filter model WOULD return a valid (>=80 char) rewrite if called —
        // proving that, absent the skip, an empty reply resurrects a bubble.
        let filt_text = "FILT_START 她轻轻地望向窗外，思绪飘向了远方。阳光洒在她的脸上，温柔而明亮。她记得那个夏天的每一天，岁月如流水般逝去，带走了所有的悲欢离合。 FILT_END";
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

        // ── 3. AppState: rule drops any [...] for mock/euryale → empties reply. ──
        let mut state = crate::routes::companion::test_state(pool.clone());
        let regex_cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel=\"mock/euryale\"\n\
             [[tasks.chat_companion.output_regex]]\n\
             models=[\"mock/euryale\"]\n\
             pattern=\"\\\\[[^\\\\]]*\\\\]\"\n",
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
                "01JT5REGEXSKIPLLM0000000C",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        // ── 5. LLM output filter that fires on mock/euryale. ───────────────────
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
            sampling: Default::default(),
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
            Some(filter),
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
        let deltas: Vec<&str> = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            deltas.is_empty(),
            "artifact-only reply must emit NO Delta even with an LLM filter armed; got {deltas:?}",
        );

        // The filter model must NEVER have been called (empty reply is terminal).
        let received = mock
            .received_requests()
            .await
            .expect("recording enabled by default");
        assert!(
            !received
                .iter()
                .any(|r| String::from_utf8_lossy(&r.body).contains("fast/m")),
            "LLM filter model must not be called when the regex strip emptied the reply",
        );

        // DB row: empty content, regex audit (NOT the LLM model).
        let (content, pre_filter, filter_model): (String, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT content, pre_filter_content, filter_model \
                 FROM engine.chat_messages \
                 WHERE session_id = $1 AND role = 'assistant' \
                 ORDER BY sent_at DESC LIMIT 1",
            )
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            content, "",
            "persisted content must be empty; got {content:?}"
        );
        assert_eq!(
            pre_filter.as_deref(),
            Some(raw_reply),
            "pre_filter_content must hold the raw artifact; got {pre_filter:?}",
        );
        assert_eq!(
            filter_model.as_deref(),
            Some("<regex>"),
            "filter_model must be '<regex>' (LLM filter skipped); got {filter_model:?}",
        );
    }

    #[test]
    fn image_request_frame_serializes_with_base64_and_snake_ref() {
        use base64::Engine as _;
        let prompt = "写实风格，海边少女，画幅 3:4"; // CJK, exercises base64 of UTF-8
        let f = build_image_request_frame(
            "01ABC".into(),
            prompt,
            eros_engine_core::types::ImageRef::Previous,
            Some("3:4"),
        );
        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        assert_eq!(v["type"], "image_request");
        assert_eq!(v["message_id"], "01ABC");
        assert_eq!(v["image_ref"], "previous");
        assert_eq!(v["aspect_ratio"], "3:4");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(v["composed_prompt"].as_str().unwrap())
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), prompt);
    }

    #[test]
    fn delegated_marker_preserves_image_awareness() {
        // Subject under `prompt` (persisted, but NOT what
        // `assistant_transcript_line` reads — see below), plus aspect — and
        // NOTHING else (no caption / composed prompt / model / gen id).
        let marker =
            build_delegated_image_marker("beach at sunset", None, Some("3:4"), None, None, None);
        assert_eq!(marker["prompt"], "beach at sunset");
        assert_eq!(marker["aspect_ratio"], "3:4");
        assert_eq!(
            marker.as_object().unwrap().len(),
            2,
            "marker must be minimal"
        );
        // The §5 regression guard: transcript still annotates it as a prior
        // image. With no caption, that annotation is the bare marker — the
        // subject/aspect do NOT surface (caption contract: never fall back
        // to `prompt`).
        let wrapped = serde_json::json!({ "image": marker });
        let line = assistant_transcript_line("", Some(&wrapped));
        assert_eq!(
            line, "（发送了一张图片）",
            "bare marker when caption absent: {line}"
        );
        assert_ne!(line.trim(), "", "image turn must not be a blank line");

        // No aspect => still a valid one-key marker that annotates (bare, same reason).
        let m2 = build_delegated_image_marker("a portrait", None, None, None, None, None);
        assert_eq!(m2.as_object().unwrap().len(), 1);
        let w2 = serde_json::json!({ "image": m2 });
        assert_eq!(
            assistant_transcript_line("", Some(&w2)),
            "（发送了一张图片）"
        );
    }

    /// Spec 2026-08-02: a successful composer call adds exactly three audit
    /// keys to the marker; absent values (no gen id) omit the key — no nulls.
    #[test]
    fn delegated_marker_carries_compose_audit_when_present() {
        let m = build_delegated_image_marker(
            "beach at sunset",
            None,
            Some("3:4"),
            Some("b"),
            Some("served/model"),
            Some("gen-xyz"),
        );
        assert_eq!(m["prompt"], "beach at sunset");
        assert_eq!(m["aspect_ratio"], "3:4");
        assert_eq!(m["compose_variant"], "b");
        assert_eq!(m["compose_model"], "served/model");
        assert_eq!(m["compose_generation_id"], "gen-xyz");
        assert_eq!(m.as_object().unwrap().len(), 5);

        // No generation id from the provider → key absent, not null.
        let m2 = build_delegated_image_marker(
            "beach at sunset",
            None,
            None,
            None,
            Some("served/model"),
            None,
        );
        assert_eq!(m2["compose_model"], "served/model");
        assert!(m2
            .as_object()
            .unwrap()
            .get("compose_generation_id")
            .is_none());
        assert!(m2.as_object().unwrap().get("compose_variant").is_none());
        assert_eq!(
            m2.as_object().unwrap().len(),
            2,
            "prompt + compose_model only"
        );
    }

    #[test]
    fn delegated_image_only_frames_are_meta_done_image_request() {
        let frames = delegated_image_only_frames(
            "01XYZ".into(),
            "a wire prompt",
            eros_engine_core::types::ImageRef::Face,
            Some("1:1"),
        );
        let types: Vec<String> = frames
            .iter()
            .map(|f| {
                serde_json::to_value(f).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(types, ["meta", "done", "image_request"]);
        let meta = serde_json::to_value(&frames[0]).unwrap();
        assert_eq!(meta["action_type"], "reply_image");
        assert!(
            meta.get("model").is_none(),
            "delegated meta carries no model"
        );
    }
}
