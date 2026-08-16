// SPDX-License-Identifier: AGPL-3.0-only
//! Post-processing — runs after a chat response. All tasks are
//! fire-and-forget and executed concurrently via `tokio::join!`.
//!
//! Ported from `eros-gateway/src/engine/post_process/{mod,affinity_persist,
//! memory,insight}.rs` with these OSS-specific changes:
//!
//! - All DB writes go through `eros-engine-store` repos (`AffinityRepo`,
//!   `MemoryRepo`, `HumanInsightRepo`, `InsightEventRepo`, `ChatRepo`) instead
//!   of inline `sqlx::query`.
//! - Insight extraction (`extract_insights`) writes `human_insights` directly
//!   via `HumanInsightRepo::apply_extraction`; the audit trail still lands in
//!   `companion_insights_events`.
//! - Ghost-streak reset on Reply/Proactive happens in the orchestrator
//!   (`pipeline::run`) before this function is spawned, since the store
//!   crate's `AffinityRepo::persist_with_event` deliberately does not
//!   touch `ghost_streak`.

use uuid::Uuid;

use eros_engine_core::types::{ActionPlan, ActionType, Event};
use eros_engine_llm::embedding::EmbeddingRouter;
use eros_engine_llm::model_config::ModelConfig;
use eros_engine_llm::openrouter::{ChatMessage, ChatRequest, OpenRouterClient};
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::character_insight::{
    existing_as_extraction_json as character_existing_json, existing_keys, parse_error_payload,
    CharacterInsightEventInsert, CharacterInsightEventRepo, CharacterInsightRepo,
};
use eros_engine_store::chat::ChatRepo;
use eros_engine_store::human_insight::{existing_as_extraction_json, HumanInsightRepo};
use eros_engine_store::insight::{InsightEventInsert, InsightEventRepo};
use eros_engine_store::memory::{MemoryLayer, MemoryRepo};
use eros_engine_store::persona::PersonaRepo;

use crate::state::AppState;

// ─── ProducedMessage ───────────────────────────────────────────────

/// One assistant message persisted during a burst (sync or streaming path).
/// `action` mirrors the spec's `meta.action_type` discriminator. `message_id`
/// and `action` are unused by today's per-message side-effects but are kept
/// on the struct for the audit hooks that a future task will thread
/// per-message.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ProducedMessage {
    pub message_id: Uuid,
    pub full_text: String,
    pub action: ActionType,
}

// ─── Top-level dispatcher ──────────────────────────────────────────

/// The OpenRouter `user` (client id) to attribute this turn's post-process
/// LLM calls to. Forwards ONLY the caller's `audit.user` — never session_id
/// or metadata (audit decision: client id only). Reuses the extractor in
/// `handlers` so there's a single definition of "audit off an Event".
fn client_id_from_event(event: &Event) -> Option<String> {
    super::handlers::audit_from_event(event).and_then(|a| a.user.clone())
}

/// Spawned by `pipeline::run`. Owned `state` so the future is `'static`.
pub async fn run(
    state: AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    event: Event,
    plan: ActionPlan,
    produced: Vec<ProducedMessage>,
) {
    let user_msg = match &event {
        Event::UserMessage { content, .. } => content.clone(),
        _ => String::new(),
    };
    // As of 4.0 the request's affinity scope is read-side only (prompt
    // injection gating); nothing in the write path consumes it any more.
    let client_id = client_id_from_event(&event);

    let fut_insight = async {
        for m in &produced {
            if !user_msg.is_empty() && !m.full_text.is_empty() {
                extract_insights(
                    &state,
                    session_id,
                    user_id,
                    m.message_id,
                    &user_msg,
                    &m.full_text,
                    client_id.as_deref(),
                )
                .await;
            }
        }
    };

    let fut_memory = async {
        if should_write_user_turn(&user_msg, &produced) {
            write_turn(&state, session_id, user_id, instance_id, &user_msg).await;
        }
    };

    let fut_affinity = async {
        // Join the (possibly multi-message) assistant burst into one text;
        // run ONE eval per turn → ONE combined event.
        let assistant_msg = produced
            .iter()
            .map(|m| m.full_text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        // For reply_image the assistant text is empty; use the picture's
        // caption as the assistant-content proxy so the photo-send still
        // moves affinity. Captionless image turns fall back to a generic
        // photo marker so they are still evaluated rather than tripping the
        // `empty_assistant` gate.
        let eval_text = affinity_eval_text(
            plan.action_type,
            &assistant_msg,
            plan.image_caption.as_deref(),
        );

        // Semantic eval gate: Reply turns only, with a non-trivial user message
        // and a non-empty produced assistant message (or image_caption proxy for
        // reply_image). Other actions (Proactive / Ghost) keep rule-only deltas
        // in v1. `pre_skip == None` ⇒ the gate passes and an eval call is
        // attempted; otherwise it carries the reason the trio will be NULL
        // (stamped into `context`).
        let pre_skip = eval_skip_reason(
            plan.action_type,
            user_msg.chars().count(),
            eval_text.trim().is_empty(),
        );

        let (grades, levels, reason, affinity_meta, skip_reason) = if pre_skip.is_none() {
            let persona_repo = PersonaRepo { pool: &state.pool };
            let affinity_repo = AffinityRepo { pool: &state.pool };
            let persona_name = match persona_repo.load_companion(instance_id).await {
                Ok(Some(p)) => p.genome.name,
                _ => String::new(),
            };
            // Snapshot the current vector for prompt context only; the
            // authoritative value is re-read under lock in persist_with_event.
            match affinity_repo.load(session_id).await {
                Ok(Some(current)) if !persona_name.is_empty() => {
                    evaluate_affinity(
                        &state,
                        session_id,
                        &persona_name,
                        &current,
                        &user_msg,
                        &eval_text,
                        client_id.as_deref(),
                    )
                    .await
                }
                _ => (
                    eros_engine_core::affinity::AxisGrades::default(),
                    eros_engine_core::affinity::EndpointLevelReads::default(),
                    String::new(),
                    None,
                    Some("no_persona_or_affinity"),
                ),
            }
        } else {
            (
                eros_engine_core::affinity::AxisGrades::default(),
                eros_engine_core::affinity::EndpointLevelReads::default(),
                String::new(),
                None,
                pre_skip,
            )
        };

        // Grades, rule deltas and endpoint levels travel separately into the
        // store, which runs the 4.0 pipeline (convert → decay → penalty →
        // gate, then the endpoint derivation) under the row lock so the tier
        // lookups always read committed state. On a skipped/failed eval the
        // levels are `None` and the stored levels hold.
        let rule_deltas = plan.affinity_deltas.clone();
        let context = build_affinity_context(&reason, skip_reason);

        persist_affinity(
            &state,
            session_id,
            user_id,
            instance_id,
            plan.action_type,
            grades,
            rule_deltas,
            context,
            affinity_meta,
            levels,
        )
        .await;
    };

    let fut_character_insight = async {
        for m in &produced {
            if !user_msg.is_empty() && !m.full_text.is_empty() {
                extract_character_insights(
                    &state,
                    session_id,
                    instance_id,
                    m.message_id,
                    &user_msg,
                    &m.full_text,
                    client_id.as_deref(),
                )
                .await;
            }
        }
    };

    tokio::join!(fut_insight, fut_memory, fut_affinity, fut_character_insight);
}

// ─── Affinity persistence ──────────────────────────────────────────

/// Run the graded turn through the 4.0 pipeline (or ghost counters) and write
/// to DB.
///
/// NOTE: `ghost_streak = 0` reset for non-Ghost actions happens in
/// `pipeline::run` before this is spawned. The store crate intentionally
/// does not touch ghost_streak in `persist_with_event` — that's a caller
/// responsibility because the streak reset is a pipeline-policy concern,
/// not a row-update concern.
#[allow(clippy::too_many_arguments)] // each arg is a distinct affinity-persist concern
async fn persist_affinity(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    action: ActionType,
    grades: eros_engine_core::affinity::AxisGrades,
    rule_deltas: eros_engine_core::affinity::AffinityDeltas,
    context: serde_json::Value,
    meta: Option<eros_engine_store::OpenRouterCallMeta>,
    levels: eros_engine_core::affinity::EndpointLevelReads,
) {
    let repo = AffinityRepo { pool: &state.pool };

    // Demo sessions get boosted positive raw scores so meters move within the
    // turn budget. Stored on the session as `metadata.is_demo` at start-chat.
    let chat_repo = ChatRepo { pool: &state.pool };
    let is_demo = match chat_repo.get_session(session_id).await {
        Ok(Some(s)) => s
            .metadata
            .get("is_demo")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        _ => false,
    };
    let boost = if is_demo {
        state.config.affinity_tuning.demo_boost
    } else {
        1.0
    };

    // No pre-read decay here: persist_with_event re-reads the row under a
    // lock and applies time decay from that locked row (design spec §6.2).
    let mut affinity = match repo.load_or_create(session_id, user_id, instance_id).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("affinity load_or_create failed: {e}");
            return;
        }
    };

    match action {
        ActionType::Ghost => {
            if let Err(e) = repo.record_ghost(&mut affinity).await {
                tracing::warn!("affinity record_ghost failed: {e}");
            }
        }
        ActionType::ProductQa => {
            // product_qa turns never run post_process (the stream arm skips
            // it); keep the match exhaustive without side effects.
            tracing::warn!("persist_affinity called with ProductQa — ignoring");
        }
        ActionType::ReplyText
        | ActionType::ReplyImage
        | ActionType::ReplyTextImage
        | ActionType::Proactive => {
            let event_type = match action {
                ActionType::Proactive => "proactive",
                ActionType::ReplyText | ActionType::ReplyImage | ActionType::ReplyTextImage => {
                    "message"
                }
                ActionType::Ghost => unreachable!(),
                ActionType::ProductQa => unreachable!(),
            };
            if let Err(e) = repo
                .persist_with_event(
                    &mut affinity,
                    &grades,
                    &rule_deltas,
                    boost,
                    &state.config.affinity_tuning,
                    event_type,
                    context,
                    meta.as_ref(),
                    levels,
                )
                .await
            {
                tracing::warn!("affinity persist_with_event failed: {e}");
            }
        }
    }
}

// ─── Memory layer ──────────────────────────────────────────────────

/// Relationship-layer memory content for a turn. Stores only the user's
/// utterance — never the assistant's prose, which would feed back into the
/// model's own prompt via recall and collapse replies to a repeated line
/// (see issue #113). The `用户：` label keeps a recalled line readable as
/// "what the user said."
fn relationship_memory_content(user_msg: &str) -> String {
    format!("用户：{user_msg}")
}

/// Whether to record this turn's user utterance as a companion memory.
/// One decision per turn (not per produced message): the relationship/profile
/// rows store the user's utterance only (#113), so a multi-message assistant
/// burst must not insert duplicate rows. Mirrors the one-eval-per-turn shape
/// of the affinity path.
fn should_write_user_turn(user_msg: &str, produced: &[ProducedMessage]) -> bool {
    !user_msg.is_empty() && produced.iter().any(|m| !m.full_text.is_empty())
}

/// Write a full conversation turn into both pgvector layers.
async fn write_turn(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    user_msg: &str,
) {
    let repo = MemoryRepo { pool: &state.pool };

    // Relationship layer (user × persona): user turn only (see #113).
    let rel_content = relationship_memory_content(user_msg);
    if let Err(e) = embed_and_upsert(
        &repo,
        &state.embed,
        MemoryLayer::Relationship,
        session_id,
        user_id,
        Some(instance_id),
        &rel_content,
    )
    .await
    {
        tracing::warn!("relationship memory upsert failed: {e}");
    }

    // Profile layer — store the user's half only.
    if !user_msg.trim().is_empty() {
        if let Err(e) = embed_and_upsert(
            &repo,
            &state.embed,
            MemoryLayer::Profile,
            session_id,
            user_id,
            None,
            user_msg,
        )
        .await
        {
            tracing::warn!("profile memory upsert failed: {e}");
        }
    }
}

async fn embed_and_upsert(
    repo: &MemoryRepo<'_>,
    embed: &EmbeddingRouter,
    layer: MemoryLayer,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Option<Uuid>,
    content: &str,
) -> Result<(), String> {
    if content.trim().is_empty() {
        return Ok(());
    }
    let embedding = embed
        .embed_document(content)
        .await
        .map_err(|e| format!("embed failed: {e}"))?;
    // category=None: this writer dumps raw turns. The classifier extraction
    // step (future) will write its own rows with category populated.
    repo.upsert(
        layer,
        session_id,
        user_id,
        instance_id,
        content,
        &embedding,
        None,
        None, // metadata: raw-turn writer supplies none
    )
    .await
    .map_err(|e| format!("memory insert failed: {e}"))?;
    Ok(())
}

// ─── Insight extraction ────────────────────────────────────────────

/// One axis's graded verdict as the judge emits it: `{"grade": 0..4,
/// "direction": "up"|"down"}`. The judge picks buckets, never numbers — the
/// engine owns the conversion to raw scores (affinity 3.0). `grade` is kept as
/// a raw JSON value so a quoted integer (`"grade":"2"`) can be salvaged in
/// `fold_grade` without failing the whole eval on a formatting slip.
#[derive(Debug, Default, serde::Deserialize)]
struct LlmAxisGrade {
    #[serde(default)]
    grade: serde_json::Value,
    #[serde(default)]
    direction: Option<String>,
}

/// Fold one axis's `{grade, direction}` into a signed grade −4..=4.
/// Accepts a JSON integer or an integer-valued numeric string; an omitted
/// axis / null grade means "nothing happened" (0). Returns `None` on anything
/// else — a non-integer, out-of-range bucket, or unknown direction — which
/// rejects the whole verdict (the engine refuses to guess what a malformed
/// bucket meant).
fn fold_grade(axis: &LlmAxisGrade) -> Option<i8> {
    let n = match &axis.grade {
        serde_json::Value::Number(n) => n.as_f64()?,
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok()?,
        serde_json::Value::Null => 0.0,
        _ => return None,
    };
    if !n.is_finite() || n.fract() != 0.0 || !(0.0..=4.0).contains(&n) {
        return None;
    }
    let sign = match axis.direction.as_deref().map(str::trim) {
        None | Some("up") => 1,
        Some("down") => -1,
        _ => return None,
    };
    Some(sign * n as i8)
}

/// Raw shape of the affinity evaluator's JSON output. Missing line axes
/// default to grade 0. `warmth`/`patience` are absolute LEVELS (1..=3, 4.0),
/// kept as raw JSON values so a quoted integer can be salvaged in
/// `fold_level` without failing the whole eval on a formatting slip.
#[derive(Debug, Default, serde::Deserialize)]
struct LlmAffinityEval {
    #[serde(default)]
    warmth: serde_json::Value,
    #[serde(default)]
    trust: LlmAxisGrade,
    #[serde(default)]
    intrigue: LlmAxisGrade,
    #[serde(default)]
    intimacy: LlmAxisGrade,
    #[serde(default)]
    tension: LlmAxisGrade,
    #[serde(default)]
    patience: serde_json::Value,
    #[serde(default)]
    reason: String,
}

/// Fold one endpoint's absolute level. Integer (or integer-valued string)
/// 1..=3 → `Ok(Some)`; null / omitted → `Ok(None)` (hold the stored level);
/// anything else → `Err` — which rejects the whole verdict, same policy as
/// `fold_grade` (the engine refuses to guess what a malformed level meant).
fn fold_level(v: &serde_json::Value) -> Result<Option<i16>, ()> {
    let n = match v {
        serde_json::Value::Number(n) => n.as_f64().ok_or(())?,
        serde_json::Value::String(s) => s.trim().parse::<f64>().map_err(|_| ())?,
        serde_json::Value::Null => return Ok(None),
        _ => return Err(()),
    };
    if !n.is_finite() || n.fract() != 0.0 || !(1.0..=3.0).contains(&n) {
        return Err(());
    }
    Ok(Some(n as i16))
}

/// Parse the evaluator output into signed judge grades plus the two absolute
/// endpoint levels. Any failure — non-JSON, no object, ANY malformed axis
/// (non-integer / out-of-range grade, unknown direction) or malformed level —
/// rejects the whole verdict: all-zero grades, no level reads, empty reason,
/// so the rule deltas still persist and the affinity write never fails
/// because the evaluator failed. Returns (grades, levels, reason).
fn parse_affinity_eval(
    raw: &str,
) -> (
    eros_engine_core::affinity::AxisGrades,
    eros_engine_core::affinity::EndpointLevelReads,
    String,
) {
    use eros_engine_core::affinity::{AxisGrades, EndpointLevelReads};
    let rejected = (
        AxisGrades::default(),
        EndpointLevelReads::default(),
        String::new(),
    );
    let parsed: Option<LlmAffinityEval> = super::parse_llm_json(raw);
    let Some(e) = parsed else {
        return rejected;
    };
    let folded = [
        fold_grade(&e.trust),
        fold_grade(&e.intrigue),
        fold_grade(&e.intimacy),
        fold_grade(&e.tension),
    ];
    let [Some(trust), Some(intrigue), Some(intimacy), Some(tension)] = folded else {
        return rejected;
    };
    let (Ok(warmth), Ok(patience)) = (fold_level(&e.warmth), fold_level(&e.patience)) else {
        return rejected;
    };
    (
        AxisGrades {
            trust,
            intrigue,
            intimacy,
            tension,
        },
        EndpointLevelReads { warmth, patience },
        e.reason,
    )
}

const AFFINITY_TASK: &str = "affinity_evaluation";

/// Skip the haiku eval on trivially short user turns (e.g. "k" / "ok") —
/// there is nothing semantic to score and the rule deltas still apply.
/// Tunable; small enough that any real sentence runs the eval.
const AFFINITY_EVAL_MIN_CHARS: usize = 4;

/// Upper bound on the evaluator LLM call. The OpenRouter client has no
/// request timeout of its own, and the affinity write (incl. the already-
/// computed rule deltas) waits on this call — so an unbounded stall would
/// delay or lose the turn's affinity event. On elapse we fall back to
/// rule-only deltas (the spec §4.5 "timeout → default" path).
const AFFINITY_EVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The text the affinity evaluator scores as "what the assistant said".
///
/// Image turns carry no assistant text, so the picture's caption stands in —
/// otherwise a photo-send would trip the `empty_assistant` gate and never move
/// affinity. A caption is a short natural-language line in the conversation's
/// language, which is exactly the register the first-person evaluator reads.
/// Captionless image turns (legacy rows, `raw` variant, failed compose) fall
/// back to a generic photo marker so they are still evaluated.
fn affinity_eval_text(
    action: ActionType,
    assistant_msg: &str,
    image_caption: Option<&str>,
) -> String {
    if !assistant_msg.trim().is_empty() {
        return assistant_msg.to_string();
    }
    if matches!(action, ActionType::ReplyImage | ActionType::ReplyTextImage) {
        let caption = image_caption.map(str::trim).unwrap_or("");
        if caption.is_empty() {
            return "[发送了一张照片]".to_string(); // consistent with the engine's Chinese image markers
        }
        return caption.to_string();
    }
    String::new()
}

/// Stable marker explaining why a `message`/`proactive` affinity event carries
/// no OpenRouter audit trio (`model`/`usage`/`generation_id` all NULL). The trio
/// is populated only from a *successful* `affinity_evaluation` call; whenever
/// that call is never made (gating below) the trio is legitimately NULL, and
/// this reason is stamped into the event `context` so the NULL is always
/// explainable ("no eval call was made", not "data lost"). `None` ⇒ the gate
/// passes and a call is attempted.
///
/// The reasons here are the *pre-attempt* ones, mirroring the old `run_eval`
/// gate exactly. Reasons only knowable after attempting
/// (`no_persona_or_affinity`, `eval_error`, `eval_timeout`) are decided at the
/// call site / in `evaluate_affinity`.
fn eval_skip_reason(
    action: ActionType,
    user_msg_chars: usize,
    assistant_empty: bool,
) -> Option<&'static str> {
    match action {
        // Proactive turns keep rule-only deltas in v1 (no semantic eval).
        ActionType::Proactive => Some("proactive"),
        // Ghost takes the `record_ghost` path, which ignores `context` entirely —
        // this arm exists only for match exhaustiveness and is never persisted.
        ActionType::Ghost => Some("ghost"),
        // product_qa turns never reach the affinity-eval gate — post_process's
        // top-level match (persist_affinity) skips them before this helper is
        // called. Exhaustiveness only.
        ActionType::ProductQa => Some("product_qa"),
        // Image variants route through the same gate as ReplyText. For reply_image
        // the caller passes `image_caption` as the assistant-content proxy so an
        // image-send still moves affinity (assistant_empty=false when the caption
        // is set — and the generic photo marker keeps it false even when it isn't).
        ActionType::ReplyText | ActionType::ReplyImage | ActionType::ReplyTextImage => {
            if user_msg_chars < AFFINITY_EVAL_MIN_CHARS {
                Some("short_user_msg")
            } else if assistant_empty {
                Some("empty_assistant")
            } else {
                None
            }
        }
    }
}

/// Marker for a *successful* eval whose response still carried no OpenRouter
/// `generation_id` — the join key to the OpenRouter log. The salvaged-garble
/// fallback in `OpenRouterClient::execute` returns `Ok` with `generation_id:
/// None` (and `usage: None`), so "the call returned `Ok`" does not by itself
/// guarantee an audit trail. Without the id the row can't be tied to an
/// OpenRouter record, so it still needs an explanation. `None` ⇒ a usable id is
/// present.
fn meta_skip_reason(meta: &eros_engine_store::OpenRouterCallMeta) -> Option<&'static str> {
    meta.generation_id
        .is_none()
        .then_some("eval_no_generation_id")
}

/// Build the affinity event `context` JSON: the model's `affinity_reason` when a
/// successful eval produced one, and/or an `eval_skip_reason` marker when the
/// audit trio has no usable join key. By construction a row with a NULL
/// `generation_id` always gets a marker, so it is never silently unexplained.
fn build_affinity_context(reason: &str, skip_reason: Option<&str>) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if !reason.is_empty() {
        map.insert(
            "affinity_reason".into(),
            serde_json::Value::String(reason.to_string()),
        );
    }
    if let Some(s) = skip_reason {
        map.insert(
            "eval_skip_reason".into(),
            serde_json::Value::String(s.to_string()),
        );
    }
    serde_json::Value::Object(map)
}

/// Build the affinity evaluator's two-message request: the static in-character
/// instruction as `system`, this turn's data as `user`. Split out as a pure
/// function so the call shape is unit-testable without an `AppState`.
fn affinity_eval_messages(
    persona_name: &str,
    affinity: &eros_engine_core::affinity::Affinity,
    user_msg: &str,
    assistant_msg: &str,
) -> Vec<ChatMessage> {
    vec![
        ChatMessage {
            role: "system".into(),
            content: crate::prompt::affinity_eval_system_prompt().to_string(),
        },
        ChatMessage {
            role: "user".into(),
            content: crate::prompt::affinity_eval_user_payload(
                persona_name,
                affinity,
                user_msg,
                assistant_msg,
            ),
        },
    ]
}

/// Run the haiku affinity evaluator for one Reply turn. Returns the signed
/// judge grades, the snapped absolute patience read (`None` when the model
/// omitted it), and the model's reason. Any failure (LLM error, non-JSON,
/// malformed grades) yields all-zero grades + no patience read + empty reason
/// so the rule deltas still persist and the affinity write never fails
/// because the evaluator failed.
async fn evaluate_affinity(
    state: &AppState,
    session_id: Uuid,
    persona_name: &str,
    affinity: &eros_engine_core::affinity::Affinity,
    user_msg: &str,
    assistant_msg: &str,
    audit_user: Option<&str>,
) -> (
    eros_engine_core::affinity::AxisGrades,
    eros_engine_core::affinity::EndpointLevelReads,
    String,
    Option<eros_engine_store::OpenRouterCallMeta>,
    Option<&'static str>,
) {
    use eros_engine_core::affinity::{AxisGrades, EndpointLevelReads};

    let resolved = state.model_config.resolve(AFFINITY_TASK, None);
    let req = ChatRequest {
        model: resolved.model,
        fallback_model: resolved.fallback_model,
        messages: affinity_eval_messages(persona_name, affinity, user_msg, assistant_msg),
        temperature: resolved.temperature as f32,
        max_tokens: resolved.max_tokens,
        sampling: resolved.sampling,
        user: audit_user.map(String::from),
        reasoning: resolved.reasoning,
        task: Some(AFFINITY_TASK.into()),
        ..Default::default()
    };

    let (raw, meta) =
        match tokio::time::timeout(AFFINITY_EVAL_TIMEOUT, state.openrouter.execute(req)).await {
            Ok(Ok(resp)) => {
                super::log_openrouter_usage(AFFINITY_TASK, Some(session_id), &resp);
                let meta = eros_engine_store::OpenRouterCallMeta {
                    generation_id: resp.generation_id.clone(),
                    model: resp.model.clone(),
                    usage: resp.usage.clone(),
                };
                (resp.reply, Some(meta))
            }
            Ok(Err(e)) => {
                tracing::warn!("affinity eval LLM call failed: {e}");
                return (
                    AxisGrades::default(),
                    EndpointLevelReads::default(),
                    String::new(),
                    None,
                    Some("eval_error"),
                );
            }
            Err(_elapsed) => {
                tracing::warn!(
                "affinity eval timed out after {AFFINITY_EVAL_TIMEOUT:?}; using rule-only deltas"
            );
                return (
                    AxisGrades::default(),
                    EndpointLevelReads::default(),
                    String::new(),
                    None,
                    Some("eval_timeout"),
                );
            }
        };

    let (grades, levels, reason) = parse_affinity_eval(&raw);
    tracing::debug!(affinity_reason = %reason, "affinity eval parsed");
    // Eval ran, but a salvaged response can still lack a generation_id — mark it
    // so a NULL audit join key is never left unexplained.
    let skip = meta.as_ref().and_then(meta_skip_reason);
    (grades, levels, reason, meta, skip)
}

const INSIGHT_TASK: &str = "insight_extraction";

/// Per-call audit captured from one insight_extraction OpenRouter call that
/// returned a response. `None` (at the call site) means the call got no response
/// (transport error / timeout) → no row is written.
struct CallAudit {
    status: &'static str,
    payload: Option<serde_json::Value>,
    meta: eros_engine_store::OpenRouterCallMeta,
}

fn call_meta(
    resp: &eros_engine_llm::openrouter::ChatResponse,
) -> eros_engine_store::OpenRouterCallMeta {
    eros_engine_store::OpenRouterCallMeta {
        generation_id: resp.generation_id.clone(),
        model: resp.model.clone(),
        usage: resp.usage.clone(),
    }
}

/// Top-level entry: extract facts → structured insights → incremental human_insights apply.
/// Writes one companion_insights_events row per OpenRouter call that returned a
/// response (facts, then structured), tied by a shared run_id. Fail-open: an
/// audit-row insert failure only warns and never breaks the turn.
async fn extract_insights(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    message_id: Uuid,
    user_msg: &str,
    assistant_msg: &str,
    audit_user: Option<&str>,
) {
    let run_id = Uuid::new_v4();

    let (facts, facts_audit) = extract_facts(
        &state.openrouter,
        &state.model_config,
        session_id,
        user_msg,
        assistant_msg,
        audit_user,
    )
    .await;
    if let Some(a) = facts_audit {
        write_insight_event(
            &state.pool,
            run_id,
            user_id,
            session_id,
            message_id,
            "facts",
            a,
        )
        .await;
    }
    if facts.is_empty() {
        return;
    }

    let human_repo = HumanInsightRepo { pool: &state.pool };
    let existing = match human_repo.load(user_id).await {
        Ok(row) => row.map(|r| existing_as_extraction_json(&r)),
        Err(e) => {
            tracing::warn!("human_insights load failed: {e}");
            None
        }
    };

    let (new_insights, struct_audit) = extract_structured_insights(
        &state.openrouter,
        &state.model_config,
        session_id,
        &facts,
        existing.as_ref(),
        audit_user,
    )
    .await;
    if let Some(a) = struct_audit {
        write_insight_event(
            &state.pool,
            run_id,
            user_id,
            session_id,
            message_id,
            "structured",
            a,
        )
        .await;
    }
    if new_insights.as_object().is_none_or(|o| o.is_empty()) {
        return;
    }

    if let Err(e) = human_repo.apply_extraction(user_id, &new_insights).await {
        tracing::warn!("human_insights apply failed: {e}");
    }
}

/// Fail-open insert of one companion_insights_events row. Never returns an
/// error to the caller — an audit-row failure must not break the chat turn.
async fn write_insight_event(
    pool: &sqlx::PgPool,
    run_id: Uuid,
    user_id: Uuid,
    session_id: Uuid,
    message_id: Uuid,
    stage: &'static str,
    audit: CallAudit,
) {
    let repo = InsightEventRepo { pool };
    let ev = InsightEventInsert {
        run_id,
        user_id,
        session_id: Some(session_id),
        message_id: Some(message_id),
        stage,
        status: audit.status,
        payload: audit.payload,
        meta: audit.meta,
    };
    if let Err(e) = repo.record(ev).await {
        tracing::warn!("insight event ({stage}) persist failed: {e}");
    }
}

async fn extract_facts(
    llm: &OpenRouterClient,
    model_config: &ModelConfig,
    session_id: Uuid,
    user_msg: &str,
    assistant_msg: &str,
    audit_user: Option<&str>,
) -> (Vec<String>, Option<CallAudit>) {
    if user_msg.trim().is_empty() {
        return (vec![], None);
    }
    let Some(resolved) = model_config.resolve_insight_extract() else {
        // Defensive skip: production configs always set insight_extraction.filter_prompt
        // (enforced by the boot gate added in this change set — see main.rs). Without it
        // there is no instruction to extract with, so do nothing rather than guess.
        return (vec![], None);
    };

    let req = ChatRequest {
        model: resolved.model,
        fallback_model: resolved.fallback_model,
        messages: vec![
            ChatMessage {
                role: "system".into(),
                content: resolved.extract_prompt,
            },
            ChatMessage {
                role: "user".into(),
                content: crate::prompt::facts_user_message(user_msg, assistant_msg),
            },
        ],
        temperature: resolved.temperature as f32,
        max_tokens: resolved.max_tokens,
        sampling: resolved.sampling,
        user: audit_user.map(String::from),
        reasoning: resolved.reasoning,
        task: Some(INSIGHT_TASK.into()),
        ..Default::default()
    };

    let (raw, meta) = match llm.execute(req).await {
        Ok(resp) => {
            super::log_openrouter_usage(INSIGHT_TASK, Some(session_id), &resp);
            (resp.reply.trim().to_string(), call_meta(&resp))
        }
        Err(e) => {
            tracing::warn!("fact extraction LLM call failed: {e}");
            return (vec![], None);
        }
    };

    // Parse once; distinguish parse_error (no JSON at all) from empty/ok.
    let parsed = super::parse_llm_json::<serde_json::Value>(&raw);
    match parsed {
        Some(v) => {
            let facts = extract_facts_array(&v);
            // Opaque sibling of `facts`: per-fact structured metadata emitted by
            // dual-track prompts. The engine never validates items or zips them
            // against `facts` — vocabulary and the facts[i]==details[i].content
            // contract are prompt-level concerns.
            let details = extract_details_array(&v);
            let status = if facts.is_empty() { "empty" } else { "ok" };
            let audit = CallAudit {
                status,
                payload: Some(serde_json::json!({ "facts": facts, "details": details })),
                meta,
            };
            (facts, Some(audit))
        }
        None => (
            vec![],
            Some(CallAudit {
                status: "parse_error",
                payload: None,
                meta,
            }),
        ),
    }
}

fn extract_facts_array(v: &serde_json::Value) -> Vec<String> {
    v.get("facts")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Extracts the `details` array; missing or non-array `details` ⇒ `[]`.
fn extract_details_array(v: &serde_json::Value) -> Vec<serde_json::Value> {
    v.get("details")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default()
}

async fn extract_structured_insights(
    llm: &OpenRouterClient,
    model_config: &ModelConfig,
    session_id: Uuid,
    facts: &[String],
    existing_insights: Option<&serde_json::Value>,
    audit_user: Option<&str>,
) -> (serde_json::Value, Option<CallAudit>) {
    let empty = || serde_json::Value::Object(serde_json::Map::new());
    if facts.is_empty() {
        return (empty(), None);
    }

    let prompt = crate::prompt::extract_structured_insights_prompt(facts, existing_insights);

    let resolved = model_config.resolve(INSIGHT_TASK, None);
    let req = ChatRequest {
        model: resolved.model,
        fallback_model: resolved.fallback_model,
        messages: vec![ChatMessage {
            role: "user".into(),
            content: prompt,
        }],
        temperature: resolved.temperature as f32,
        max_tokens: resolved.max_tokens,
        sampling: resolved.sampling,
        user: audit_user.map(String::from),
        reasoning: resolved.reasoning,
        task: Some(INSIGHT_TASK.into()),
        ..Default::default()
    };

    let (raw, meta) = match llm.execute(req).await {
        Ok(r) => {
            super::log_openrouter_usage(INSIGHT_TASK, Some(session_id), &r);
            (r.reply.trim().to_string(), call_meta(&r))
        }
        Err(_) => return (empty(), None),
    };

    let parsed = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .filter(|v| v.is_object())
        .or_else(|| {
            super::find_json_block(&raw)
                .and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok())
                .filter(|v| v.is_object())
        });
    match parsed {
        Some(v) => {
            let status = if v.as_object().is_some_and(|o| o.is_empty()) {
                "empty"
            } else {
                "ok"
            };
            let audit = CallAudit {
                status,
                payload: Some(v.clone()),
                meta,
            };
            (v, Some(audit))
        }
        None => (
            empty(),
            Some(CallAudit {
                status: "parse_error",
                payload: None,
                meta,
            }),
        ),
    }
}

// ─── Character insights ────────────────────────────────────────────

const CHARACTER_EXTRACTION_TASK: &str = "character_insight_extraction";
const CHARACTER_STRUCTURING_TASK: &str = "character_insight_structuring";

/// Top-level entry for the character chain: extraction → structuring →
/// incremental `character_insights` apply. Writes one
/// `character_insights_events` row per OpenRouter call that returned a
/// response, tied by a shared `run_id`.
///
/// Fail-open throughout: an audit insert, a load, or an apply that fails only
/// warns. Nothing here may break the turn.
///
/// The result is DB-only by design — nothing reads `character_insights` back
/// into a prompt (spec §7).
#[allow(clippy::too_many_arguments)] // each arg is a distinct audit/context key
async fn extract_character_insights(
    state: &AppState,
    session_id: Uuid,
    instance_id: Uuid,
    message_id: Uuid,
    user_msg: &str,
    assistant_msg: &str,
    audit_user: Option<&str>,
) {
    let run_id = Uuid::new_v4();

    let (facts, facts_audit) =
        extract_character_facts(state, session_id, user_msg, assistant_msg, audit_user).await;
    if let Some(a) = facts_audit {
        write_character_event(
            &state.pool,
            run_id,
            instance_id,
            session_id,
            message_id,
            "extraction",
            a,
        )
        .await;
    }
    if facts.is_empty() {
        return;
    }

    let repo = CharacterInsightRepo { pool: &state.pool };
    // A failed load must ABORT the run, not degrade to "no existing profile".
    // The structuring prompt asks for complete replacement values, so running it
    // without the stored row yields fields derived from this turn alone — and
    // `apply_extraction` would then overwrite however many turns of accumulated
    // profile with that narrower answer. A transient DB blip must not cost data.
    // `Ok(None)` is different: it genuinely means no row yet, and proceeds.
    let existing = match repo.load(instance_id).await {
        Ok(row) => row.map(|r| character_existing_json(&r)),
        Err(e) => {
            tracing::warn!("character_insights load failed, skipping structuring: {e}");
            return;
        }
    };

    let (structured, struct_audit) =
        structure_character_insights(state, session_id, &facts, existing.as_ref(), audit_user)
            .await;
    if let Some(a) = struct_audit {
        write_character_event(
            &state.pool,
            run_id,
            instance_id,
            session_id,
            message_id,
            "structuring",
            a,
        )
        .await;
    }
    if structured.as_object().is_none_or(|o| o.is_empty()) {
        return;
    }

    if let Err(e) = repo.apply_extraction(instance_id, &structured).await {
        tracing::warn!("character_insights apply failed: {e}");
    }
}

/// Fail-open insert of one `character_insights_events` row.
async fn write_character_event(
    pool: &sqlx::PgPool,
    run_id: Uuid,
    instance_id: Uuid,
    session_id: Uuid,
    message_id: Uuid,
    stage: &'static str,
    audit: CallAudit,
) {
    let repo = CharacterInsightEventRepo { pool };
    let ev = CharacterInsightEventInsert {
        run_id,
        instance_id,
        session_id: Some(session_id),
        message_id: Some(message_id),
        stage,
        status: audit.status,
        payload: audit.payload,
        meta: audit.meta,
    };
    if let Err(e) = repo.record(ev).await {
        tracing::warn!("character insight event ({stage}) persist failed: {e}");
    }
}

/// Stage 1. `None` for the resolved task is the feature's off switch — no
/// block, no calls, no rows.
async fn extract_character_facts(
    state: &AppState,
    session_id: Uuid,
    user_msg: &str,
    assistant_msg: &str,
    audit_user: Option<&str>,
) -> (Vec<String>, Option<CallAudit>) {
    if assistant_msg.trim().is_empty() {
        return (vec![], None);
    }
    let Some(resolved) = state.model_config.resolve_character_insight_extract() else {
        return (vec![], None);
    };

    let req = ChatRequest {
        model: resolved.model,
        fallback_model: resolved.fallback_model,
        messages: vec![
            ChatMessage {
                role: "system".into(),
                content: resolved.extract_prompt,
            },
            ChatMessage {
                role: "user".into(),
                content: crate::prompt::facts_user_message(user_msg, assistant_msg),
            },
        ],
        temperature: resolved.temperature as f32,
        max_tokens: resolved.max_tokens,
        sampling: resolved.sampling,
        user: audit_user.map(String::from),
        reasoning: resolved.reasoning,
        task: Some(CHARACTER_EXTRACTION_TASK.into()),
        ..Default::default()
    };

    let (raw, meta) = match state.openrouter.execute(req).await {
        Ok(resp) => {
            super::log_openrouter_usage(CHARACTER_EXTRACTION_TASK, Some(session_id), &resp);
            (resp.reply.trim().to_string(), call_meta(&resp))
        }
        Err(e) => {
            tracing::warn!("character fact extraction LLM call failed: {e}");
            return (vec![], None);
        }
    };

    match super::parse_llm_json::<serde_json::Value>(&raw) {
        Some(v) => {
            let facts = extract_facts_array(&v);
            // `details` is opaque: the engine never validates its items nor
            // zips them against `facts`. That contract is prompt-level.
            let details = extract_details_array(&v);
            let status = if facts.is_empty() { "empty" } else { "ok" };
            // Build the payload BEFORE moving `facts` into the return tuple.
            let payload = serde_json::json!({ "facts": facts, "details": details });
            (
                facts,
                Some(CallAudit {
                    status,
                    payload: Some(payload),
                    meta,
                }),
            )
        }
        // Unlike the human chain, the unparseable reply is KEPT: a whole-turn
        // refusal and malformed JSON are otherwise the same row.
        None => (
            vec![],
            Some(CallAudit {
                status: "parse_error",
                payload: Some(parse_error_payload(&raw)),
                meta,
            }),
        ),
    }
}

/// Stage 2. Parameters come from the dedicated block when present, else stage
/// 1's — never from global defaults (see `resolve_structuring`).
async fn structure_character_insights(
    state: &AppState,
    session_id: Uuid,
    facts: &[String],
    existing: Option<&serde_json::Value>,
    audit_user: Option<&str>,
) -> (serde_json::Value, Option<CallAudit>) {
    let empty = || serde_json::Value::Object(serde_json::Map::new());
    if facts.is_empty() {
        return (empty(), None);
    }

    let prompt = crate::prompt::extract_character_insights_prompt(facts, existing);
    let resolved = state
        .model_config
        .resolve_structuring(CHARACTER_STRUCTURING_TASK, CHARACTER_EXTRACTION_TASK);

    let req = ChatRequest {
        model: resolved.model,
        fallback_model: resolved.fallback_model,
        messages: vec![ChatMessage {
            role: "user".into(),
            content: prompt,
        }],
        temperature: resolved.temperature as f32,
        max_tokens: resolved.max_tokens,
        sampling: resolved.sampling,
        user: audit_user.map(String::from),
        reasoning: resolved.reasoning,
        task: Some(CHARACTER_STRUCTURING_TASK.into()),
        ..Default::default()
    };

    let (raw, meta) = match state.openrouter.execute(req).await {
        Ok(r) => {
            super::log_openrouter_usage(CHARACTER_STRUCTURING_TASK, Some(session_id), &r);
            (r.reply.trim().to_string(), call_meta(&r))
        }
        Err(e) => {
            tracing::warn!("character structuring LLM call failed: {e}");
            return (empty(), None);
        }
    };

    let parsed = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .filter(|v| v.is_object())
        .or_else(|| {
            super::find_json_block(&raw)
                .and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok())
                .filter(|v| v.is_object())
        });

    match parsed {
        Some(v) => {
            let status = if v.as_object().is_some_and(|o| o.is_empty()) {
                "empty"
            } else {
                "ok"
            };
            // Record which columns arrived pre-filled. Without it, a fact that
            // does not appear in the output is ambiguous between "dropped" and
            // "judged already covered".
            let mut audited = v.clone();
            if let Some(o) = audited.as_object_mut() {
                o.insert(
                    "_existing_keys".into(),
                    serde_json::json!(existing_keys(existing)),
                );
            }
            (
                v,
                Some(CallAudit {
                    status,
                    payload: Some(audited),
                    meta,
                }),
            )
        }
        None => (
            empty(),
            Some(CallAudit {
                status: "parse_error",
                payload: Some(parse_error_payload(&raw)),
                meta,
            }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::companion::testutil::seed_persona_instance;
    use uuid::Uuid;

    #[test]
    fn client_id_from_event_forwards_user_only() {
        use eros_engine_core::types::LlmAudit;
        let mut metadata = serde_json::Map::new();
        metadata.insert("feature".into(), serde_json::Value::String("chat".into()));
        let event = Event::UserMessage {
            content: "hi".into(),
            message_id: Uuid::new_v4(),
            prompt_traits: Vec::new(),
            audit: Some(LlmAudit {
                user: Some("u_abc".into()),
                session_id: Some("s_xyz".into()),
                metadata: Some(metadata),
            }),
            tier: None,
            memory_scope: Default::default(),
            affinity_scope: Default::default(),
            tips_amount_usd: None,
            history_anchor: Default::default(),
        };
        // Only `user` is taken; session_id/metadata are ignored by design.
        assert_eq!(client_id_from_event(&event).as_deref(), Some("u_abc"));
    }

    #[test]
    fn client_id_from_event_none_when_no_audit() {
        let event = Event::UserMessage {
            content: "hi".into(),
            message_id: Uuid::new_v4(),
            prompt_traits: Vec::new(),
            audit: None,
            tier: None,
            memory_scope: Default::default(),
            affinity_scope: Default::default(),
            tips_amount_usd: None,
            history_anchor: Default::default(),
        };
        assert_eq!(client_id_from_event(&event), None);
    }

    #[test]
    fn client_id_from_event_none_for_non_user_message() {
        let event = Event::ProactiveTrigger;
        assert_eq!(client_id_from_event(&event), None);
    }

    #[test]
    fn extract_details_array_valid_array_returned_as_is() {
        let v = serde_json::json!({"facts": ["f1"], "details": [{"content": "f1"}]});
        assert_eq!(
            extract_details_array(&v),
            vec![serde_json::json!({"content": "f1"})]
        );
    }

    #[test]
    fn extract_details_array_missing_key_is_empty() {
        let v = serde_json::json!({"facts": ["f1"]});
        assert!(extract_details_array(&v).is_empty());
    }

    #[test]
    fn extract_details_array_string_value_is_empty() {
        let v = serde_json::json!({"facts": ["f1"], "details": "oops"});
        assert!(extract_details_array(&v).is_empty());
    }

    #[test]
    fn extract_details_array_number_value_is_empty() {
        let v = serde_json::json!({"facts": ["f1"], "details": 42});
        assert!(extract_details_array(&v).is_empty());
    }

    #[test]
    fn parse_affinity_eval_valid_grades_levels_and_reason() {
        let raw = r#"{"warmth":3,"trust":{"grade":1,"direction":"up"},"intimacy":{"grade":3,"direction":"up"},"intrigue":{"grade":0,"direction":"up"},"tension":{"grade":1,"direction":"down"},"patience":2,"reason":"暖"}"#;
        let (g, lv, reason) = parse_affinity_eval(raw);
        assert_eq!(g.trust, 1);
        assert_eq!(g.intimacy, 3);
        assert_eq!(g.intrigue, 0);
        assert_eq!(g.tension, -1, "direction down folds to a negative grade");
        assert_eq!(lv.warmth, Some(3));
        assert_eq!(lv.patience, Some(2));
        assert_eq!(reason, "暖");
    }

    /// A malformed bucket rejects the WHOLE verdict — the engine refuses to
    /// guess what an out-of-range / fractional grade or an unknown direction
    /// meant. Rule deltas persist regardless, so nothing is lost but the eval.
    #[test]
    fn parse_affinity_eval_rejects_malformed_grades_wholesale() {
        use eros_engine_core::affinity::{AxisGrades, EndpointLevelReads};
        for raw in [
            // out-of-range bucket
            r#"{"trust":{"grade":5,"direction":"up"},"intimacy":{"grade":1,"direction":"up"},"reason":"x"}"#,
            // negative bucket (sign belongs to direction, not the grade)
            r#"{"trust":{"grade":-1,"direction":"up"},"reason":"x"}"#,
            // fractional bucket — 3.0 explicitly stopped asking for numbers
            r#"{"trust":{"grade":1.5,"direction":"up"},"reason":"x"}"#,
            // unknown direction
            r#"{"trust":{"grade":1,"direction":"sideways"},"reason":"x"}"#,
            // wrong-typed grade
            r#"{"trust":{"grade":true,"direction":"up"},"reason":"x"}"#,
        ] {
            let (g, lv, reason) = parse_affinity_eval(raw);
            assert_eq!(g, AxisGrades::default(), "whole verdict zeroed: {raw}");
            assert_eq!(
                lv,
                EndpointLevelReads::default(),
                "levels distrusted with the verdict: {raw}"
            );
            assert!(reason.is_empty(), "reason distrusted too: {raw}");
        }
    }

    /// A malformed LEVEL rejects the whole verdict too — same policy as the
    /// grades: 4.0's judge contract has exactly two shapes, and anything else
    /// is not worth guessing about.
    #[test]
    fn parse_affinity_eval_rejects_malformed_levels_wholesale() {
        use eros_engine_core::affinity::{AxisGrades, EndpointLevelReads};
        for raw in [
            r#"{"warmth":5,"trust":{"grade":2,"direction":"up"},"reason":"x"}"#, // out of 1..=3
            r#"{"warmth":0,"reason":"x"}"#,                                      // below range
            r#"{"warmth":1.5,"reason":"x"}"#,                                    // fractional
            r#"{"warmth":true,"reason":"x"}"#,                                   // wrong type
            r#"{"patience":{"grade":2},"reason":"x"}"#, // 3.x object shape on an endpoint
            r#"{"patience":"abc","reason":"x"}"#,       // non-numeric string
            r#"{"patience":"NaN","reason":"x"}"#,       // non-finite
        ] {
            let (g, lv, reason) = parse_affinity_eval(raw);
            assert_eq!(g, AxisGrades::default(), "whole verdict zeroed: {raw}");
            assert_eq!(lv, EndpointLevelReads::default(), "levels zeroed: {raw}");
            assert!(reason.is_empty(), "reason distrusted too: {raw}");
        }
    }

    /// Formatting slips that are unambiguous ARE salvaged: a quoted integer
    /// grade or level, an omitted direction (defaults up), an omitted grade (0).
    #[test]
    fn parse_affinity_eval_salvages_unambiguous_slips() {
        let raw = r#"{"warmth":"3","trust":{"grade":"2"},"intimacy":{"direction":"down"},"patience":"2","reason":"x"}"#;
        let (g, lv, _) = parse_affinity_eval(raw);
        assert_eq!(
            g.trust, 2,
            "quoted integer grade + missing direction salvaged"
        );
        assert_eq!(g.intimacy, 0, "missing grade means nothing happened");
        assert_eq!(lv.warmth, Some(3), "quoted integer level is salvaged");
        assert_eq!(lv.patience, Some(2));
    }

    /// Omitted / null levels are a HOLD, not an error: the judge saying
    /// nothing about an endpoint keeps the stored level.
    #[test]
    fn parse_affinity_eval_absent_levels_hold() {
        use eros_engine_core::affinity::EndpointLevelReads;
        for raw in [
            r#"{"trust":{"grade":1,"direction":"up"},"reason":"x"}"#,
            r#"{"warmth":null,"patience":null,"trust":{"grade":1,"direction":"up"},"reason":"x"}"#,
        ] {
            let (g, lv, _) = parse_affinity_eval(raw);
            assert_eq!(g.trust, 1, "grades unaffected: {raw}");
            assert_eq!(
                lv,
                EndpointLevelReads::default(),
                "omitted level → hold: {raw}"
            );
        }
    }

    #[test]
    fn parse_affinity_eval_garbage_returns_default() {
        use eros_engine_core::affinity::{AxisGrades, EndpointLevelReads};
        let (g, lv, reason) = parse_affinity_eval("not json at all");
        assert_eq!(g, AxisGrades::default());
        assert_eq!(lv, EndpointLevelReads::default());
        assert!(reason.is_empty());
    }

    #[test]
    fn parse_affinity_eval_missing_fields_default_zero() {
        let raw = r#"{"warmth":2,"reason":"only a level"}"#;
        let (g, lv, _) = parse_affinity_eval(raw);
        assert_eq!(lv.warmth, Some(2));
        assert_eq!(g.trust, 0);
        assert_eq!(g.intimacy, 0);
    }

    #[test]
    fn parse_affinity_eval_extracts_from_fenced_block() {
        let raw = "```json\n{\"warmth\":2,\"trust\":{\"grade\":1,\"direction\":\"up\"},\"reason\":\"fenced\"}\n```";
        let (g, lv, reason) = parse_affinity_eval(raw);
        assert_eq!(g.trust, 1);
        assert_eq!(lv.warmth, Some(2));
        assert_eq!(reason, "fenced");
    }

    #[test]
    fn eval_skip_reason_none_only_for_substantive_text_reply() {
        // The one path that DOES run the eval (→ trio populated).
        assert_eq!(eval_skip_reason(ActionType::ReplyText, 10, false), None);
    }

    #[test]
    fn eval_skip_reason_text_reply_gates() {
        // Short user message (< AFFINITY_EVAL_MIN_CHARS) skips the eval.
        assert_eq!(
            eval_skip_reason(ActionType::ReplyText, AFFINITY_EVAL_MIN_CHARS - 1, false),
            Some("short_user_msg")
        );
        // Boundary: exactly the threshold runs.
        assert_eq!(
            eval_skip_reason(ActionType::ReplyText, AFFINITY_EVAL_MIN_CHARS, false),
            None
        );
        // Empty assistant text skips even with a long user message.
        assert_eq!(
            eval_skip_reason(ActionType::ReplyText, 50, true),
            Some("empty_assistant")
        );
    }

    #[test]
    fn eval_runs_on_image_reply_with_text_or_prompt() {
        // reply_text_image with real text + adequate user msg → not skipped
        assert_eq!(
            eval_skip_reason(ActionType::ReplyTextImage, 10, false),
            None
        );
        // reply_image with empty assistant text but the caller supplies a non-empty
        // proxy (assistant_empty=false because image_caption is used) → not skipped
        assert_eq!(eval_skip_reason(ActionType::ReplyImage, 10, false), None);
        // image reply with empty proxy → empty_assistant
        assert_eq!(
            eval_skip_reason(ActionType::ReplyImage, 10, true),
            Some("empty_assistant")
        );
        // still gated by short user msg
        assert_eq!(
            eval_skip_reason(ActionType::ReplyTextImage, 2, false),
            Some("short_user_msg")
        );
        // Proactive and Ghost keep their dedicated skip reasons.
        assert_eq!(
            eval_skip_reason(ActionType::Proactive, 50, false),
            Some("proactive")
        );
        assert_eq!(
            eval_skip_reason(ActionType::Ghost, 50, false),
            Some("ghost")
        );
    }

    #[test]
    fn affinity_eval_text_prefers_text_then_caption_then_marker() {
        // assistant text wins whenever present
        assert_eq!(
            affinity_eval_text(ActionType::ReplyTextImage, "我在这儿", Some("在天台")),
            "我在这儿"
        );
        // image turn with no text: the caption is the proxy
        assert_eq!(
            affinity_eval_text(ActionType::ReplyImage, "", Some("在天台看夕阳")),
            "在天台看夕阳"
        );
        // image turn with no text and no caption: generic marker, so the turn
        // is still evaluated rather than tripping the empty_assistant gate
        assert_eq!(
            affinity_eval_text(ActionType::ReplyImage, "", None),
            "[发送了一张照片]"
        );
        assert_eq!(
            affinity_eval_text(ActionType::ReplyImage, "", Some("   ")),
            "[发送了一张照片]"
        );
        // non-image action with no text stays empty
        assert_eq!(affinity_eval_text(ActionType::ReplyText, "", None), "");
    }

    #[test]
    fn meta_skip_reason_flags_missing_generation_id() {
        // Salvaged-garble fallback: Ok response, but no generation_id ⇒ marked,
        // even though model is present.
        let salvaged = eros_engine_store::OpenRouterCallMeta {
            generation_id: None,
            model: Some("m".into()),
            usage: None,
        };
        assert_eq!(meta_skip_reason(&salvaged), Some("eval_no_generation_id"));
        // Clean response with a join key ⇒ no marker.
        let clean = eros_engine_store::OpenRouterCallMeta {
            generation_id: Some("gen-1".into()),
            model: Some("m".into()),
            usage: Some(serde_json::json!({"total_tokens": 9})),
        };
        assert_eq!(meta_skip_reason(&clean), None);
    }

    #[test]
    fn build_affinity_context_shapes() {
        // Successful eval: reason only, no skip marker.
        assert_eq!(
            build_affinity_context("他主动分享", None),
            serde_json::json!({ "affinity_reason": "他主动分享" })
        );
        // Skipped/failed eval (NULL trio): marker only, always explainable.
        assert_eq!(
            build_affinity_context("", Some("short_user_msg")),
            serde_json::json!({ "eval_skip_reason": "short_user_msg" })
        );
        // Empty reason + no skip → {} (only when an eval ran but returned no reason).
        assert_eq!(build_affinity_context("", None), serde_json::json!({}));
        // Defensive: both present coexist.
        assert_eq!(
            build_affinity_context("r", Some("eval_timeout")),
            serde_json::json!({ "affinity_reason": "r", "eval_skip_reason": "eval_timeout" })
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn insight_extraction_writes_two_events_sharing_run_id(pool: sqlx::PgPool) {
        use wiremock::matchers::{body_string_contains, method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // Stage-1 facts call → non-empty facts. Matched by a substring unique to
        // the system message (filter_prompt sentinel).
        let facts_body = serde_json::json!({
            "id": "gen-facts", "model": "ins/m",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content":
                "{\"facts\":[\"用户在深圳工作\"],\"details\":[{\"content\":\"用户在深圳工作\",\"category\":\"fact\",\"domain\":\"career\",\"evidence_type\":\"explicit_statement\",\"temporality\":\"current\",\"persistence\":\"stable\",\"confidence\":\"high\"}]}"
            }}],
        });
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("facts-sys-prompt-sentinel"))
            .respond_with(ResponseTemplate::new(200).set_body_json(facts_body))
            .mount(&mock)
            .await;

        // Stage-2 structured call. Matched by a substring unique to
        // extract_structured_insights_prompt.
        let struct_body = serde_json::json!({
            "id": "gen-struct", "model": "ins/m",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": "{\"city\":\"深圳\"}"}}],
        });
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("填充以下 schema"))
            .respond_with(ResponseTemplate::new(200).set_body_json(struct_body))
            .mount(&mock)
            .await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.insight_extraction]\nmodel=\"ins/m\"\nfilter_prompt=\"facts-sys-prompt-sentinel\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let user_id = uuid::Uuid::new_v4();
        let session_id = uuid::Uuid::new_v4();
        let message_id = uuid::Uuid::new_v4();

        extract_insights(
            &state,
            session_id,
            user_id,
            message_id,
            "我在深圳工作",
            "嗯嗯",
            None,
        )
        .await;

        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            uuid::Uuid,
            String,
            String,
            Option<String>,
            Option<serde_json::Value>,
        )> = sqlx::query_as(
            "SELECT run_id, stage, status, generation_id, payload \
                 FROM engine.companion_insights_events WHERE user_id = $1 ORDER BY stage",
        )
        .bind(user_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2, "facts + structured rows; got {rows:?}");
        assert_eq!(rows[0].1, "facts");
        assert_eq!(rows[1].1, "structured");
        assert_eq!(rows[0].0, rows[1].0, "both rows share one run_id");
        assert_eq!(rows[0].3.as_deref(), Some("gen-facts"));
        assert_eq!(rows[1].3.as_deref(), Some("gen-struct"));
        // facts-stage payload is now a {facts, details} object.
        let payload = rows[0].4.as_ref().expect("facts payload present");
        assert_eq!(payload["facts"], serde_json::json!(["用户在深圳工作"]));
        assert_eq!(payload["details"][0]["category"], "fact");
        assert_eq!(payload["details"][0]["confidence"], "high");

        // Direct write: the structured result landed in human_insights.
        let city: Option<String> =
            sqlx::query_scalar("SELECT city FROM engine.human_insights WHERE user_id = $1")
                .bind(user_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(city.as_deref(), Some("深圳"));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn insight_extraction_empty_facts_writes_one_event(pool: sqlx::PgPool) {
        use wiremock::matchers::{body_string_contains, method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // Facts call returns an empty list ⇒ status='empty', no structured call.
        let facts_body = serde_json::json!({
            "id": "gen-facts", "model": "ins/m",
            "usage": {"total_tokens": 2},
            "choices": [{"message": {"content": "{\"facts\":[]}"}}],
        });
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("facts-sys-prompt-sentinel"))
            .respond_with(ResponseTemplate::new(200).set_body_json(facts_body))
            .mount(&mock)
            .await;
        // Structured mock must NOT be hit.
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("填充以下 schema"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&mock)
            .await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.insight_extraction]\nmodel=\"ins/m\"\nfilter_prompt=\"facts-sys-prompt-sentinel\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let user_id = uuid::Uuid::new_v4();
        extract_insights(
            &state,
            uuid::Uuid::new_v4(),
            user_id,
            uuid::Uuid::new_v4(),
            "hi there",
            "嗯嗯",
            None,
        )
        .await;

        let rows: Vec<(String, String, Option<serde_json::Value>)> = sqlx::query_as(
            "SELECT stage, status, payload FROM engine.companion_insights_events WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "only the facts row; got {rows:?}");
        assert_eq!(rows[0].0, "facts");
        assert_eq!(rows[0].1, "empty");
        assert_eq!(
            rows[0].2,
            Some(serde_json::json!({"facts": [], "details": []})),
            "empty run still writes the uniform object payload"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn insight_extraction_facts_parse_error_writes_one_event(pool: sqlx::PgPool) {
        use wiremock::matchers::{body_string_contains, method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // Facts call returns non-JSON garbage ⇒ status='parse_error', payload NULL,
        // and the structured call is never made.
        let facts_body = serde_json::json!({
            "id": "gen-facts", "model": "ins/m",
            "usage": {"total_tokens": 2},
            "choices": [{"message": {"content": "这不是 JSON"}}],
        });
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("facts-sys-prompt-sentinel"))
            .respond_with(ResponseTemplate::new(200).set_body_json(facts_body))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("填充以下 schema"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0)
            .mount(&mock)
            .await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.insight_extraction]\nmodel=\"ins/m\"\nfilter_prompt=\"facts-sys-prompt-sentinel\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let user_id = uuid::Uuid::new_v4();
        extract_insights(
            &state,
            uuid::Uuid::new_v4(),
            user_id,
            uuid::Uuid::new_v4(),
            "hi there",
            "嗯嗯",
            None,
        )
        .await;

        let rows: Vec<(String, String, Option<serde_json::Value>)> = sqlx::query_as(
            "SELECT stage, status, payload FROM engine.companion_insights_events WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "only the facts row; got {rows:?}");
        assert_eq!(rows[0].0, "facts");
        assert_eq!(rows[0].1, "parse_error");
        assert_eq!(rows[0].2, None, "parse_error ⇒ NULL payload");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn insight_extraction_details_absent_persists_empty_array(pool: sqlx::PgPool) {
        use wiremock::matchers::{body_string_contains, method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // Stage-1 facts call → non-empty facts, old-prompt shape (no `details`
        // key). Matched by a substring unique to the system message
        // (filter_prompt sentinel).
        let facts_body = serde_json::json!({
            "id": "gen-facts", "model": "ins/m",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": "{\"facts\":[\"用户在深圳工作\"]}"}}],
        });
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("facts-sys-prompt-sentinel"))
            .respond_with(ResponseTemplate::new(200).set_body_json(facts_body))
            .mount(&mock)
            .await;

        // Stage-2 structured call. Matched by a substring unique to
        // extract_structured_insights_prompt.
        let struct_body = serde_json::json!({
            "id": "gen-struct", "model": "ins/m",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": "{\"city\":\"深圳\"}"}}],
        });
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("填充以下 schema"))
            .respond_with(ResponseTemplate::new(200).set_body_json(struct_body))
            .mount(&mock)
            .await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.insight_extraction]\nmodel=\"ins/m\"\nfilter_prompt=\"facts-sys-prompt-sentinel\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let user_id = uuid::Uuid::new_v4();
        let session_id = uuid::Uuid::new_v4();
        let message_id = uuid::Uuid::new_v4();

        extract_insights(
            &state,
            session_id,
            user_id,
            message_id,
            "我在深圳工作",
            "嗯嗯",
            None,
        )
        .await;

        let rows: Vec<(String, Option<serde_json::Value>)> = sqlx::query_as(
            "SELECT status, payload FROM engine.companion_insights_events \
             WHERE user_id = $1 AND stage = 'facts'",
        )
        .bind(user_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "ok");
        let payload = rows[0].1.as_ref().unwrap();
        assert_eq!(payload["facts"], serde_json::json!(["用户在深圳工作"]));
        assert_eq!(
            payload["details"],
            serde_json::json!([]),
            "no details key ⇒ []"
        );
    }

    #[test]
    fn relationship_memory_content_stores_user_turn_only() {
        let c = relationship_memory_content("今天好累");
        assert_eq!(c, "用户：今天好累");
        assert!(
            !c.contains("AI："),
            "relationship memory must not carry assistant prose (#113): {c}"
        );
    }

    fn make_produced(full_text: &str) -> ProducedMessage {
        ProducedMessage {
            message_id: Uuid::new_v4(),
            full_text: full_text.to_string(),
            action: ActionType::ReplyText,
        }
    }

    #[test]
    fn should_write_user_turn_empty_user_msg_is_false() {
        // even if produced has text, an empty user utterance must not write
        let produced = vec![make_produced("assistant reply")];
        assert!(!should_write_user_turn("", &produced));
    }

    #[test]
    fn should_write_user_turn_empty_produced_is_false() {
        assert!(!should_write_user_turn("hello", &[]));
    }

    #[test]
    fn should_write_user_turn_all_produced_empty_text_is_false() {
        // produced present but every full_text is empty → no write
        let produced = vec![make_produced(""), make_produced("")];
        assert!(!should_write_user_turn("hello", &produced));
    }

    #[test]
    fn should_write_user_turn_single_produced_with_text_is_true() {
        let produced = vec![make_produced("assistant reply")];
        assert!(should_write_user_turn("hello", &produced));
    }

    #[test]
    fn should_write_user_turn_multi_produced_with_text_is_true() {
        // regression case: multi-message burst must yield ONE decision (true),
        // not loop N times as the old code did
        let produced = vec![
            make_produced("first assistant message"),
            make_produced("second assistant message"),
            make_produced("third assistant message"),
        ];
        assert!(should_write_user_turn("hello", &produced));
    }

    /// Locks the *accepted* partial affinity-neutrality contract for a
    /// fallback-ghost turn (design spec
    /// `docs/superpowers/specs/2026-07-06-empty-reply-ghost-fallback-design.md`
    /// §6). `post_process::run` has no visibility into
    /// `BurstOutcome.ghost_fallback` — from here a fallback-ghost turn (regex-
    /// strip-to-empty or empty-completion) is indistinguishable from any other
    /// `ReplyText` turn that happens to carry empty `produced` text. The
    /// maintainer explicitly accepted that this path is NOT fully affinity-
    /// neutral: `persist_affinity` still writes an `event_type = "message"`
    /// event and applies the user-derived rule delta (`predict_reply_deltas`),
    /// even though the LLM eval / memory / insight writes are all skipped. If
    /// a future change makes this fully neutral (or silently regresses further,
    /// e.g. by resurrecting a `ghost` event here), this test must fail and
    /// force an explicit decision rather than drifting quietly.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn fallback_ghost_turn_writes_message_event_with_eval_skipped(pool: sqlx::PgPool) {
        let state = crate::routes::companion::test_state(pool.clone());

        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // "hi" is short enough to trip both the PDE's short-message rule delta
        // (a patience penalty) AND `eval_skip_reason`'s `short_user_msg` gate,
        // so the LLM affinity eval never fires while the rule delta still does.
        let event = Event::UserMessage {
            content: "hi".into(),
            message_id: Uuid::new_v4(),
            prompt_traits: Vec::new(),
            audit: None,
            tier: None,
            memory_scope: Default::default(),
            affinity_scope: Default::default(),
            tips_amount_usd: None,
            history_anchor: Default::default(),
        };

        // Mirrors what `pde::decide` would compute for a short user message
        // (`predict_reply_deltas`'s short-message patience penalty) — built
        // directly here since `run` takes an already-decided `ActionPlan`.
        let plan = ActionPlan {
            action_type: ActionType::ReplyText,
            reply_style: eros_engine_core::types::ReplyStyle::Neutral,
            affinity_deltas: eros_engine_core::affinity::AffinityDeltas {
                patience: -0.02,
                ..Default::default()
            },
            energy_cost: 0.0,
            context_hints: Vec::new(),
            reply_tone: None,
            image_caption: None,
            image_ref: eros_engine_core::types::ImageRef::Face,
            aspect_ratio: None,
        };

        // EMPTY text — this is exactly what a fallback-ghost turn produces when
        // it is served through the `ReplyText` arm.
        let produced = vec![ProducedMessage {
            message_id: Uuid::new_v4(),
            full_text: String::new(),
            action: ActionType::ReplyText,
        }];

        run(
            state,
            session_id,
            user_id,
            instance_id,
            event,
            plan,
            produced,
        )
        .await;

        let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
            "SELECT e.event_type, e.context \
             FROM engine.companion_affinity_events e \
             JOIN engine.companion_affinity a ON a.id = e.affinity_id \
             WHERE a.session_id = $1",
        )
        .bind(session_id)
        .fetch_all(&pool)
        .await
        .unwrap();

        assert_eq!(
            rows.len(),
            1,
            "a fallback-ghost turn still writes exactly one affinity event \
             (accepted, NOT neutral); got {rows:?}"
        );
        assert_eq!(
            rows[0].0, "message",
            "NOT 'ghost' — post_process::run can't see BurstOutcome.ghost_fallback, \
             so a fallback-ghost turn is indistinguishable from a real empty reply \
             and takes the same event_type=\"message\" path"
        );
        assert_eq!(
            rows[0].1.get("eval_skip_reason").and_then(|v| v.as_str()),
            Some("short_user_msg"),
            "the LLM affinity eval must still be skipped (the guaranteed-neutral \
             half of the contract); context: {:?}",
            rows[0].1
        );

        // 4.0: patience is a derived endpoint. A skipped eval holds the
        // stored level (default 2) and no rule delta touches it, so the row
        // reads exactly the level-2 derivation over bond 0.
        let patience: f64 = sqlx::query_scalar(
            "SELECT patience FROM engine.companion_affinity WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let expect = (1.0 / 3.0) * eros_engine_core::affinity::endpoint_boost(0.0);
        assert!(
            (patience - expect).abs() < 1e-6,
            "patience stays the held-level derivation on a skipped eval; got {patience}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn persist_affinity_sets_levels_and_discards_rule_patience(pool: sqlx::PgPool) {
        use eros_engine_store::affinity::AffinityRepo;

        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        // persist_affinity calls load_or_create, but seeding the row first is
        // explicit and lets us assert against a known level-2 seed.
        AffinityRepo { pool: &pool }
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();

        // A (hypothetical) rule patience nudge must be discarded; the judge's
        // level 3 must land as the derivation 2/3·B(bond=0).
        let state = crate::routes::companion::test_state(pool.clone());
        let rule_deltas = eros_engine_core::affinity::AffinityDeltas {
            patience: 0.03,
            ..Default::default()
        };
        persist_affinity(
            &state,
            session_id,
            user_id,
            instance_id,
            ActionType::ReplyText,
            eros_engine_core::affinity::AxisGrades::default(),
            rule_deltas,
            serde_json::json!({}),
            None,
            eros_engine_core::affinity::EndpointLevelReads {
                warmth: None,
                patience: Some(3),
            },
        )
        .await;

        let (patience, patience_grade): (f64, i16) = sqlx::query_as(
            "SELECT patience, patience_grade FROM engine.companion_affinity WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(patience_grade, 3);
        let expect = (2.0 / 3.0) * eros_engine_core::affinity::endpoint_boost(0.0);
        assert!(
            (patience - expect).abs() < 1e-6,
            "level 3 derives through the server layer (rule nudge discarded); got {patience}"
        );
    }

    fn fixture_eval_affinity() -> eros_engine_core::affinity::Affinity {
        let now = chrono::Utc::now();
        eros_engine_core::affinity::Affinity {
            id: Uuid::nil(),
            session_id: Uuid::nil(),
            user_id: Uuid::nil(),
            instance_id: Uuid::nil(),
            warmth: 0.42,
            trust: 0.31,
            intrigue: 0.55,
            intimacy: 0.22,
            patience: 0.66,
            tension: 0.13,
            warmth_grade: 2,
            patience_grade: 2,
            ghost_streak: 0,
            last_ghost_at: None,
            total_ghosts: 0,
            relationship_label: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn affinity_eval_messages_is_system_then_user() {
        let a = fixture_eval_affinity();
        let msgs = affinity_eval_messages("Mia", &a, "我今天好累", "抱抱你");
        assert_eq!(msgs.len(), 2, "instructions and data are separate messages");
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].role, "user");
        // The static instruction goes in the system slot verbatim.
        assert_eq!(
            msgs[0].content,
            crate::prompt::affinity_eval_system_prompt()
        );
        // The per-turn data goes in the user slot.
        assert!(msgs[1].content.contains("角色名：Mia"));
        assert!(msgs[1].content.contains("对方：我今天好累"));
        assert!(msgs[1].content.contains("Mia：抱抱你"));
        // The turn's data must NOT be smuggled into the system message.
        assert!(
            !msgs[0].content.contains("我今天好累"),
            "system message must stay static across turns"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn character_chain_is_off_when_the_stage_one_task_is_absent(pool: sqlx::PgPool) {
        // The stage-1 block is the whole on/off switch: with no
        // [tasks.character_insight_extraction] there must be no LLM call and
        // no rows at all. test_state()'s config carries no such block.
        use crate::routes::companion::testutil::seed_persona_instance;
        // Already imported at the top of this tests module by the human-chain
        // tests; the inner `use` is harmless if it is.
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let state = crate::routes::companion::test_state(pool.clone());
        let session_id = Uuid::new_v4();

        extract_character_insights(
            &state,
            session_id,
            instance_id,
            Uuid::new_v4(),
            "你今天在忙什么",
            "还在公司，加班到十点",
            None,
        )
        .await;

        let events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM engine.character_insights_events WHERE instance_id = $1",
        )
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(events, 0, "no task block ⇒ no calls, no audit rows");

        let profiles: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM engine.character_insights WHERE instance_id = $1",
        )
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(profiles, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn character_chain_writes_both_events_and_the_profile(pool: sqlx::PgPool) {
        use crate::routes::companion::testutil::seed_persona_instance;
        use wiremock::matchers::{body_string_contains, method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // Stage 1 — matched by the sentinel planted in filter_prompt AND by its
        // own resolved model, so a `resolve_structuring` regression that sends
        // stage 1's model on the stage-2 call cannot accidentally match here
        // too (this mock still requires the sentinel, which the structuring
        // prompt never contains).
        let facts_body = serde_json::json!({
            "id": "gen-ch-facts", "model": "ch/stage-one",
            "usage": {"total_tokens": 2},
            "choices": [{"message": {"content":
                "{\"facts\":[\"角色说她今天在公司加班到十点\"],\"details\":[]}"}}],
        });
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("char-facts-sentinel"))
            .and(body_string_contains("\"model\":\"ch/stage-one\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(facts_body))
            .mount(&mock)
            .await;

        // Stage 2 — matched by a substring unique to CHARACTER_INSIGHTS_SCHEMA
        // AND by its own resolved model. Without the model matcher, a broken
        // `resolve_structuring` that falls back to stage 1's block would still
        // send the structuring prompt (so the schema substring still matches)
        // and this mock would return "ok" regardless of which block resolved —
        // the model matcher is what makes that regression fail the request
        // instead of passing silently.
        let struct_body = serde_json::json!({
            "id": "gen-ch-struct", "model": "ch/stage-two",
            "usage": {"total_tokens": 3},
            "choices": [{"message": {"content": "{\"location\":\"公司\"}"}}],
        });
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("character_insights schema"))
            .and(body_string_contains("\"model\":\"ch/stage-two\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(struct_body))
            .mount(&mock)
            .await;

        let instance_id = seed_persona_instance(&pool, uuid::Uuid::new_v4()).await;
        let mut state = crate::routes::companion::test_state(pool.clone());
        // Two DISTINCT blocks with two DISTINCT, non-prefix model ids (neither
        // is a substring of the other), so the audit rows AND the mock match
        // itself prove each stage resolved to its own block rather than
        // sharing one.
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.character_insight_extraction]\nmodel=\"ch/stage-one\"\n\
                 filter_prompt=\"char-facts-sentinel\"\n\n\
                 [tasks.character_insight_structuring]\nmodel=\"ch/stage-two\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        extract_character_insights(
            &state,
            uuid::Uuid::new_v4(),
            instance_id,
            uuid::Uuid::new_v4(),
            "你今天在忙什么",
            "还在公司，加班到十点",
            None,
        )
        .await;

        #[allow(clippy::type_complexity)]
        let rows: Vec<(uuid::Uuid, String, String, Option<String>)> = sqlx::query_as(
            "SELECT run_id, stage, status, model FROM engine.character_insights_events \
             WHERE instance_id = $1 ORDER BY stage",
        )
        .bind(instance_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2, "extraction + structuring; got {rows:?}");
        assert_eq!(rows[0].1, "extraction");
        assert_eq!(rows[1].1, "structuring");
        assert_eq!(rows[0].2, "ok");
        assert_eq!(rows[1].2, "ok");
        assert_eq!(rows[0].0, rows[1].0, "both stages must share one run_id");
        // The stage-2 request could only have matched its mock (via the
        // "\"model\":\"ch/stage-two\"" body matcher above) if
        // resolve_structuring actually resolved to the dedicated stage-2
        // block — a fallback to stage 1's block would have sent
        // "ch/stage-one" instead, matched neither mock, and made the whole
        // call error out (so `rows.len() == 2` above would already have
        // failed). These assertions confirm what landed in the audit row.
        assert_eq!(rows[0].3.as_deref(), Some("ch/stage-one"));
        assert_eq!(rows[1].3.as_deref(), Some("ch/stage-two"));

        // The structuring payload carries the audit addition.
        let payload: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT payload FROM engine.character_insights_events \
             WHERE instance_id = $1 AND stage = 'structuring'",
        )
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            payload
                .expect("structuring payload")
                .get("_existing_keys")
                .is_some(),
            "structuring payload must record which columns arrived pre-filled"
        );

        // And the profile actually landed.
        let row = eros_engine_store::character_insight::CharacterInsightRepo { pool: &pool }
            .load(instance_id)
            .await
            .unwrap()
            .expect("profile written");
        assert_eq!(row.location.as_deref(), Some("公司"));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn character_chain_aborts_when_the_existing_profile_cannot_be_loaded(pool: sqlx::PgPool) {
        // A failed load must abort the run rather than degrade to "no existing
        // profile". The structuring prompt asks for complete replacement values,
        // so running stage 2 blind would produce fields derived from this turn
        // alone, and apply_extraction would overwrite however many turns of
        // accumulated profile with that narrower answer. A transient DB failure
        // must not cost data.
        //
        // Fault injection: drop the profile table so `load` genuinely errors
        // while everything else — the mocks, the audit table — still works.
        use crate::routes::companion::testutil::seed_persona_instance;
        use wiremock::matchers::{body_string_contains, method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        let facts_body = serde_json::json!({
            "id": "gen-ch-facts", "model": "ch/stage-one",
            "usage": {"total_tokens": 2},
            "choices": [{"message": {"content":
                "{\"facts\":[\"角色说她今天在公司加班到十点\"],\"details\":[]}"}}],
        });
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("char-facts-sentinel"))
            .respond_with(ResponseTemplate::new(200).set_body_json(facts_body))
            .mount(&mock)
            .await;

        // Mounted so that a regression which DOES reach stage 2 succeeds and
        // writes a second audit row — making the assertion below fail loudly
        // rather than passing because the call happened to error out.
        let struct_body = serde_json::json!({
            "id": "gen-ch-struct", "model": "ch/stage-two",
            "usage": {"total_tokens": 3},
            "choices": [{"message": {"content": "{\"location\":\"公司\"}"}}],
        });
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("character_insights schema"))
            .respond_with(ResponseTemplate::new(200).set_body_json(struct_body))
            .mount(&mock)
            .await;

        let instance_id = seed_persona_instance(&pool, uuid::Uuid::new_v4()).await;
        sqlx::query("DROP TABLE engine.character_insights")
            .execute(&pool)
            .await
            .expect("drop the profile table to make load() fail");

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.character_insight_extraction]\nmodel=\"ch/stage-one\"\n\
                 filter_prompt=\"char-facts-sentinel\"\n\n\
                 [tasks.character_insight_structuring]\nmodel=\"ch/stage-two\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        extract_character_insights(
            &state,
            uuid::Uuid::new_v4(),
            instance_id,
            uuid::Uuid::new_v4(),
            "你今天在忙什么",
            "还在公司，加班到十点",
            None,
        )
        .await;

        // Stage 1 still audited (it ran before the load); stage 2 never did.
        let stages: Vec<(String,)> = sqlx::query_as(
            "SELECT stage FROM engine.character_insights_events \
             WHERE instance_id = $1 ORDER BY stage",
        )
        .bind(instance_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            stages.len(),
            1,
            "a failed load must abort before structuring; got {stages:?}"
        );
        assert_eq!(stages[0].0, "extraction");
    }
}
