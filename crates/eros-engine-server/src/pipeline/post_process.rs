// SPDX-License-Identifier: AGPL-3.0-only
//! Post-processing — runs after a chat response. All tasks are
//! fire-and-forget and executed concurrently via `tokio::join!`.
//!
//! Ported from `eros-gateway/src/engine/post_process/{mod,affinity_persist,
//! memory,insight}.rs` with these OSS-specific changes:
//!
//! - All DB writes go through `eros-engine-store` repos (`AffinityRepo`,
//!   `MemoryRepo`, `InsightRepo`, `ChatRepo`) instead of inline `sqlx::query`.
//! - `companion_insights` lives in its own table, not on `user_profiles`.
//!   `InsightRepo::merge` handles the JSONB merge + training-level update.
//! - Ghost-streak reset on Reply/Proactive happens in the orchestrator
//!   (`pipeline::run`) before this function is spawned, since the store
//!   crate's `AffinityRepo::persist_with_event` deliberately does not
//!   touch `ghost_streak`.

use uuid::Uuid;

use eros_engine_core::types::{ActionPlan, ActionType, Event};
use eros_engine_llm::model_config::ModelConfig;
use eros_engine_llm::openrouter::{ChatMessage, ChatRequest, OpenRouterClient};
use eros_engine_llm::voyage::VoyageClient;
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::ChatRepo;
use eros_engine_store::human_insight::HumanInsightRepo;
use eros_engine_store::insight::{InsightEventInsert, InsightEventRepo, InsightRepo};
use eros_engine_store::memory::{MemoryLayer, MemoryRepo};
use eros_engine_store::persona::PersonaRepo;

use crate::state::AppState;

// ─── ProducedMessage ───────────────────────────────────────────────

/// One assistant message persisted during a burst (sync or streaming path).
/// `action` mirrors the spec's `meta.action_type` discriminator. `message_id`
/// and `action` are unused by today's per-message side-effects but are kept
/// on the struct for the audit / lead-score hooks that a future task will
/// thread per-message.
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
        for m in &produced {
            if !user_msg.is_empty() && !m.full_text.is_empty() {
                write_turn(
                    &state,
                    session_id,
                    user_id,
                    instance_id,
                    &user_msg,
                    &m.full_text,
                )
                .await;
            }
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

        // Semantic eval gate: Reply turns only, with a non-trivial user message
        // and a non-empty produced assistant message. Other actions
        // (Proactive / Ghost) keep rule-only deltas in v1. `pre_skip == None`
        // ⇒ the gate passes and an eval call is attempted; otherwise it carries
        // the reason the trio will be NULL (stamped into `context`).
        let pre_skip = eval_skip_reason(
            plan.action_type,
            user_msg.chars().count(),
            assistant_msg.trim().is_empty(),
        );

        let (llm_deltas, reason, affinity_meta, skip_reason) = if pre_skip.is_none() {
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
                        &assistant_msg,
                        client_id.as_deref(),
                    )
                    .await
                }
                _ => (
                    eros_engine_core::affinity::AffinityDeltas::default(),
                    String::new(),
                    None,
                    Some("no_persona_or_affinity"),
                ),
            }
        } else {
            (
                eros_engine_core::affinity::AffinityDeltas::default(),
                String::new(),
                None,
                pre_skip,
            )
        };

        let combined = merge_deltas(&plan.affinity_deltas, &llm_deltas);
        let context = build_affinity_context(&reason, skip_reason);

        persist_affinity(
            &state,
            session_id,
            user_id,
            instance_id,
            plan.action_type,
            combined,
            context,
            affinity_meta,
        )
        .await;
    };

    let should_update_lead = matches!(
        plan.action_type,
        ActionType::ReplyText | ActionType::Proactive,
    );
    let fut_lead = async {
        if should_update_lead {
            refresh_lead_score(&state, session_id, user_id).await;
        }
    };

    tokio::join!(fut_insight, fut_memory, fut_affinity, fut_lead);
}

// ─── Affinity persistence ──────────────────────────────────────────

/// Apply EMA-smoothed deltas (or ghost counters) and write to DB.
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
    deltas: eros_engine_core::affinity::AffinityDeltas,
    context: serde_json::Value,
    meta: Option<eros_engine_store::OpenRouterCallMeta>,
) {
    let repo = AffinityRepo { pool: &state.pool };

    // Demo sessions get a faster blend so meters move within the turn budget.
    // Stored on the session as `metadata.is_demo` at start-chat time.
    let chat_repo = ChatRepo { pool: &state.pool };
    let is_demo = match chat_repo.get_session(session_id).await {
        Ok(Some(s)) => s
            .metadata
            .get("is_demo")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        _ => false,
    };
    let ema_inertia = if is_demo {
        state.config.demo_ema_inertia
    } else {
        state.config.ema_inertia
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
            };
            if let Err(e) = repo
                .persist_with_event(
                    &mut affinity,
                    &deltas,
                    ema_inertia,
                    event_type,
                    context,
                    meta.as_ref(),
                )
                .await
            {
                tracing::warn!("affinity persist_with_event failed: {e}");
            }
        }
    }
}

// ─── Memory layer ──────────────────────────────────────────────────

/// Write a full conversation turn into both pgvector layers.
async fn write_turn(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    user_msg: &str,
    assistant_msg: &str,
) {
    let repo = MemoryRepo { pool: &state.pool };

    // Relationship layer (user × persona).
    let rel_content = format!("用户：{user_msg}\nAI：{assistant_msg}");
    if let Err(e) = embed_and_upsert(
        &repo,
        &state.voyage,
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
            &state.voyage,
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
    voyage: &VoyageClient,
    layer: MemoryLayer,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Option<Uuid>,
    content: &str,
) -> Result<(), String> {
    if content.trim().is_empty() {
        return Ok(());
    }
    let embedding = voyage
        .embed_document(content)
        .await
        .map_err(|e| format!("voyage embed failed: {e}"))?;
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
    )
    .await
    .map_err(|e| format!("memory insert failed: {e}"))?;
    Ok(())
}

// ─── Insight extraction ────────────────────────────────────────────

/// Locate the first balanced `{...}` block in `raw`. Returned as a borrowed
/// slice. Replaces the gateway's `regex::Regex::new(r"(?s)\{.*\}")` so the
/// OSS server crate doesn't pick up a `regex` dependency just for this.
pub(crate) fn find_json_block(raw: &str) -> Option<&str> {
    let bytes = raw.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    // Walk forward, tracking nesting depth + string state, to find the
    // paired close brace. Mirrors the greedy `\{.*\}` behaviour but stays
    // balanced rather than running to EOF.
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&raw[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Per-axis safety cap on the LLM's raw delta, applied before the pacing
/// gain. A guardrail against a misbehaving model — independent of
/// `ema_inertia`. The two jobs (safety cap vs. pacing) are deliberately
/// separate (see the design spec §5).
const LLM_AXIS_CAP: f64 = 0.15;

/// Raw shape of the affinity evaluator's JSON output. `patience` is
/// intentionally absent — it is rule-owned, so any `patience` the model
/// emits is dropped by serde (unknown field). Missing axes default to 0.
#[derive(Debug, Default, serde::Deserialize)]
struct LlmAffinityEval {
    #[serde(default)]
    warmth: f64,
    #[serde(default)]
    trust: f64,
    #[serde(default)]
    intrigue: f64,
    #[serde(default)]
    intimacy: f64,
    #[serde(default)]
    tension: f64,
    #[serde(default)]
    reason: String,
}

/// Parse + per-axis clamp the evaluator output into rule-mergeable deltas.
/// Any failure (non-JSON, no object) → all-zero deltas + empty reason, so
/// the rule deltas still persist and the affinity write never fails because
/// the evaluator failed. `patience` is forced to 0 (rule-owned).
fn parse_affinity_eval(raw: &str) -> (eros_engine_core::affinity::AffinityDeltas, String) {
    use eros_engine_core::affinity::AffinityDeltas;
    let parsed: Option<LlmAffinityEval> = serde_json::from_str(raw)
        .ok()
        .or_else(|| find_json_block(raw).and_then(|b| serde_json::from_str(b).ok()));
    let Some(e) = parsed else {
        return (AffinityDeltas::default(), String::new());
    };
    let cap = |v: f64| v.clamp(-LLM_AXIS_CAP, LLM_AXIS_CAP);
    (
        AffinityDeltas {
            warmth: cap(e.warmth),
            trust: cap(e.trust),
            intrigue: cap(e.intrigue),
            intimacy: cap(e.intimacy),
            tension: cap(e.tension),
            patience: 0.0,
        },
        e.reason,
    )
}

/// Sum the rule (behavioral) and LLM (semantic) contributions per axis.
/// `patience` is rule-owned — the evaluator always passes 0 for it — so
/// the sum naturally keeps the rule value.
fn merge_deltas(
    rule: &eros_engine_core::affinity::AffinityDeltas,
    llm: &eros_engine_core::affinity::AffinityDeltas,
) -> eros_engine_core::affinity::AffinityDeltas {
    eros_engine_core::affinity::AffinityDeltas {
        warmth: rule.warmth + llm.warmth,
        trust: rule.trust + llm.trust,
        intrigue: rule.intrigue + llm.intrigue,
        intimacy: rule.intimacy + llm.intimacy,
        patience: rule.patience + llm.patience,
        tension: rule.tension + llm.tension,
    }
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
        // Reserved image variants: PDE degrades them to ReplyText today
        // (`stream::guard_action`), so these arms are forward-looking — they only
        // fire once the image executor ships.
        ActionType::ReplyImage | ActionType::ReplyTextImage => Some("image_reply"),
        // Proactive turns keep rule-only deltas in v1 (no semantic eval).
        ActionType::Proactive => Some("proactive"),
        // Ghost takes the `record_ghost` path, which ignores `context` entirely —
        // this arm exists only for match exhaustiveness and is never persisted.
        ActionType::Ghost => Some("ghost"),
        ActionType::ReplyText => {
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

/// Build the affinity event `context` JSON: the model's `affinity_reason` when a
/// successful eval produced one, and/or an `eval_skip_reason` marker when the
/// audit trio is NULL. By construction a NULL-trio row always gets a marker, so
/// a NULL `model`/`generation_id` is never silently unexplained.
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

/// Run the haiku affinity evaluator for one Reply turn. Returns the clamped
/// per-axis LLM deltas + the model's reason. Any failure (LLM error,
/// non-JSON) yields all-zero deltas + empty reason so the rule deltas still
/// persist and the affinity write never fails because the evaluator failed.
async fn evaluate_affinity(
    state: &AppState,
    session_id: Uuid,
    persona_name: &str,
    affinity: &eros_engine_core::affinity::Affinity,
    user_msg: &str,
    assistant_msg: &str,
    audit_user: Option<&str>,
) -> (
    eros_engine_core::affinity::AffinityDeltas,
    String,
    Option<eros_engine_store::OpenRouterCallMeta>,
    Option<&'static str>,
) {
    use eros_engine_core::affinity::AffinityDeltas;

    let prompt =
        crate::prompt::affinity_eval_prompt(persona_name, affinity, user_msg, assistant_msg);
    let resolved = state.model_config.resolve(AFFINITY_TASK, None);
    let req = ChatRequest {
        model: resolved.model,
        fallback_model: resolved.fallback_model,
        messages: vec![ChatMessage {
            role: "user".into(),
            content: prompt,
        }],
        temperature: resolved.temperature as f32,
        max_tokens: resolved.max_tokens,
        user: audit_user.map(String::from),
        reasoning: resolved.reasoning,
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
                    AffinityDeltas::default(),
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
                    AffinityDeltas::default(),
                    String::new(),
                    None,
                    Some("eval_timeout"),
                );
            }
        };

    let (deltas, reason) = parse_affinity_eval(&raw);
    tracing::debug!(affinity_reason = %reason, "affinity eval parsed");
    (deltas, reason, meta, None)
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

/// Top-level entry: extract facts → structured insights → InsightRepo merge.
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

    let insights_repo = InsightRepo { pool: &state.pool };
    let existing = match insights_repo.load(user_id).await {
        Ok(row) => row.map(|r| r.insights),
        Err(e) => {
            tracing::warn!("companion_insights load failed: {e}");
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

    match insights_repo.merge(user_id, new_insights).await {
        Ok(row) => {
            let human_repo = HumanInsightRepo { pool: &state.pool };
            if let Err(e) = human_repo
                .project_from_insights(user_id, &row.insights)
                .await
            {
                tracing::warn!("human_insights projection failed: {e}");
            }
        }
        Err(e) => tracing::warn!("companion_insights merge failed: {e}"),
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
        user: audit_user.map(String::from),
        reasoning: resolved.reasoning,
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
    let parsed = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .or_else(|| find_json_block(&raw).and_then(|b| serde_json::from_str(b).ok()));
    match parsed {
        Some(v) => {
            let facts = extract_facts_array(&v);
            let status = if facts.is_empty() { "empty" } else { "ok" };
            let audit = CallAudit {
                status,
                payload: Some(serde_json::json!(facts)),
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
        user: audit_user.map(String::from),
        reasoning: resolved.reasoning,
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
            find_json_block(&raw)
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

// ─── Lead score refresh ────────────────────────────────────────────

async fn refresh_lead_score(state: &AppState, session_id: Uuid, user_id: Uuid) {
    let repo = InsightRepo { pool: &state.pool };
    let level = match repo.load(user_id).await {
        Ok(Some(row)) => row.training_level,
        Ok(None) => 0.0,
        Err(e) => {
            tracing::warn!("lead score refresh: insights load failed: {e}");
            return;
        }
    };

    let new_lead = (level * 10.0).clamp(0.0, 10.0);

    if let Err(e) = sqlx::query("UPDATE engine.chat_sessions SET lead_score = $2 WHERE id = $1")
        .bind(session_id)
        .bind(new_lead)
        .execute(&state.pool)
        .await
    {
        tracing::warn!("lead score update failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        };
        assert_eq!(client_id_from_event(&event), None);
    }

    #[test]
    fn client_id_from_event_none_for_non_user_message() {
        let event = Event::ProactiveTrigger;
        assert_eq!(client_id_from_event(&event), None);
    }

    #[test]
    fn find_json_block_balanced_with_nested_string_braces() {
        // The `}` inside the string literal must not close the outer block.
        let raw = r#"prefix {"a": "b}c", "d": {"e": 1}} trailing"#;
        let block = find_json_block(raw).unwrap();
        let v: serde_json::Value = serde_json::from_str(block).unwrap();
        assert_eq!(v["a"], "b}c");
        assert_eq!(v["d"]["e"], 1);
    }

    #[test]
    fn find_json_block_returns_none_when_no_object() {
        assert!(find_json_block("no json here").is_none());
    }

    #[test]
    fn parse_affinity_eval_valid_clamps_and_keeps_reason() {
        let raw = r#"{"warmth":0.08,"trust":0.03,"intimacy":0.06,"intrigue":0.02,"tension":-0.01,"reason":"暖"}"#;
        let (d, reason) = parse_affinity_eval(raw);
        assert!((d.warmth - 0.08).abs() < 1e-9);
        assert!((d.trust - 0.03).abs() < 1e-9);
        assert!((d.intimacy - 0.06).abs() < 1e-9);
        assert!((d.intrigue - 0.02).abs() < 1e-9);
        assert!((d.tension - (-0.01)).abs() < 1e-9);
        assert_eq!(d.patience, 0.0);
        assert_eq!(reason, "暖");
    }

    #[test]
    fn parse_affinity_eval_clamps_out_of_range() {
        let raw = r#"{"warmth":5.0,"trust":-2.0,"reason":"x"}"#;
        let (d, _) = parse_affinity_eval(raw);
        assert!((d.warmth - 0.15).abs() < 1e-9, "warmth caps at +0.15");
        assert!(
            (d.trust - (-0.15)).abs() < 1e-9,
            "trust delta caps at -0.15"
        );
    }

    #[test]
    fn parse_affinity_eval_ignores_patience_field() {
        let raw = r#"{"warmth":0.1,"patience":0.99,"reason":"x"}"#;
        let (d, _) = parse_affinity_eval(raw);
        assert_eq!(d.patience, 0.0, "patience from the model is ignored");
        assert!((d.warmth - 0.1).abs() < 1e-9);
    }

    #[test]
    fn parse_affinity_eval_garbage_returns_default() {
        let (d, reason) = parse_affinity_eval("not json at all");
        assert_eq!(d.warmth, 0.0);
        assert_eq!(d.trust, 0.0);
        assert_eq!(d.intrigue, 0.0);
        assert_eq!(d.intimacy, 0.0);
        assert_eq!(d.tension, 0.0);
        assert_eq!(d.patience, 0.0);
        assert!(reason.is_empty());
    }

    #[test]
    fn parse_affinity_eval_missing_fields_default_zero() {
        let raw = r#"{"warmth":0.1,"reason":"only warmth"}"#;
        let (d, _) = parse_affinity_eval(raw);
        assert!((d.warmth - 0.1).abs() < 1e-9);
        assert_eq!(d.trust, 0.0);
        assert_eq!(d.intimacy, 0.0);
    }

    #[test]
    fn parse_affinity_eval_extracts_from_fenced_block() {
        let raw = "```json\n{\"warmth\":0.05,\"reason\":\"fenced\"}\n```";
        let (d, reason) = parse_affinity_eval(raw);
        assert!((d.warmth - 0.05).abs() < 1e-9);
        assert_eq!(reason, "fenced");
    }

    #[test]
    fn merge_deltas_sums_per_axis_patience_from_rule_only() {
        use eros_engine_core::affinity::AffinityDeltas;
        let rule = AffinityDeltas {
            intrigue: 0.02,
            patience: 0.02,
            ..Default::default()
        };
        let llm = AffinityDeltas {
            warmth: 0.08,
            intrigue: 0.03,
            tension: 0.01,
            ..Default::default()
        };
        let c = merge_deltas(&rule, &llm);
        assert!((c.warmth - 0.08).abs() < 1e-9);
        assert!((c.intrigue - 0.05).abs() < 1e-9, "0.02 rule + 0.03 llm");
        assert!((c.tension - 0.01).abs() < 1e-9);
        assert!(
            (c.patience - 0.02).abs() < 1e-9,
            "rule only (llm patience is 0)"
        );
        assert_eq!(c.trust, 0.0);
        assert_eq!(c.intimacy, 0.0);
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
    fn eval_skip_reason_non_text_reply_actions() {
        // Image variants are forward-looking (degraded to ReplyText today).
        assert_eq!(
            eval_skip_reason(ActionType::ReplyImage, 50, false),
            Some("image_reply")
        );
        assert_eq!(
            eval_skip_reason(ActionType::ReplyTextImage, 50, false),
            Some("image_reply")
        );
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
                Default::default(),
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

        let rows: Vec<(uuid::Uuid, String, String, Option<String>)> = sqlx::query_as(
            "SELECT run_id, stage, status, generation_id \
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
                Default::default(),
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

        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT stage, status FROM engine.companion_insights_events WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "only the facts row; got {rows:?}");
        assert_eq!(rows[0].0, "facts");
        assert_eq!(rows[0].1, "empty");
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
                Default::default(),
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
}
