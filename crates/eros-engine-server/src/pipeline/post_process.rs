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
//! - Ghost-streak reset on Reply/Proactive/GiftReaction happens in the
//!   orchestrator (`pipeline::run`) before this function is spawned, since
//!   the store crate's `AffinityRepo::persist_with_event` deliberately
//!   does not touch `ghost_streak`.
//! - `gift_records.affinity_applied` flip is dropped — there is no gift
//!   ledger table in OSS.

use uuid::Uuid;

use eros_engine_core::types::{ActionPlan, ActionType, Event};
use eros_engine_llm::model_config::ModelConfig;
use eros_engine_llm::openrouter::{ChatMessage, ChatRequest, OpenRouterClient};
use eros_engine_llm::voyage::VoyageClient;
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::ChatRepo;
use eros_engine_store::human_insight::HumanInsightRepo;
use eros_engine_store::insight::InsightRepo;
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

        // Semantic eval: Reply turns only, with a non-trivial user message
        // and a non-empty produced assistant message. Other actions
        // (Proactive / GiftReaction / Ghost) keep rule-only deltas in v1.
        let run_eval = plan.action_type == ActionType::Reply
            && user_msg.chars().count() >= AFFINITY_EVAL_MIN_CHARS
            && !assistant_msg.trim().is_empty();

        let (llm_deltas, reason) = if run_eval {
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
                ),
            }
        } else {
            (
                eros_engine_core::affinity::AffinityDeltas::default(),
                String::new(),
            )
        };

        let combined = merge_deltas(&plan.affinity_deltas, &llm_deltas);
        let context = if reason.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::json!({ "affinity_reason": reason })
        };

        persist_affinity(
            &state,
            session_id,
            user_id,
            instance_id,
            plan.action_type,
            combined,
            context,
        )
        .await;
    };

    let should_update_lead = matches!(
        plan.action_type,
        ActionType::Reply | ActionType::GiftReaction | ActionType::Proactive,
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
async fn persist_affinity(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    action: ActionType,
    deltas: eros_engine_core::affinity::AffinityDeltas,
    context: serde_json::Value,
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
        ActionType::Reply | ActionType::GiftReaction | ActionType::Proactive => {
            let event_type = match action {
                ActionType::GiftReaction => "gift",
                ActionType::Proactive => "proactive",
                ActionType::Reply => "message",
                ActionType::Ghost => unreachable!(),
            };
            if let Err(e) = repo
                .persist_with_event(&mut affinity, &deltas, ema_inertia, event_type, context)
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
fn find_json_block(raw: &str) -> Option<&str> {
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
) -> (eros_engine_core::affinity::AffinityDeltas, String) {
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
        ..Default::default()
    };

    let raw = match tokio::time::timeout(AFFINITY_EVAL_TIMEOUT, state.openrouter.execute(req)).await
    {
        Ok(Ok(resp)) => {
            super::log_openrouter_usage(AFFINITY_TASK, Some(session_id), &resp);
            resp.reply
        }
        Ok(Err(e)) => {
            tracing::warn!("affinity eval LLM call failed: {e}");
            return (AffinityDeltas::default(), String::new());
        }
        Err(_elapsed) => {
            tracing::warn!(
                "affinity eval timed out after {AFFINITY_EVAL_TIMEOUT:?}; using rule-only deltas"
            );
            return (AffinityDeltas::default(), String::new());
        }
    };

    let (deltas, reason) = parse_affinity_eval(&raw);
    tracing::debug!(affinity_reason = %reason, "affinity eval parsed");
    (deltas, reason)
}

const INSIGHT_TASK: &str = "insight_extraction";

/// Top-level entry: extract facts → structured insights → InsightRepo merge.
async fn extract_insights(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    user_msg: &str,
    assistant_msg: &str,
    audit_user: Option<&str>,
) {
    let facts = extract_facts(
        &state.openrouter,
        &state.model_config,
        session_id,
        user_msg,
        assistant_msg,
        audit_user,
    )
    .await;
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

    let new_insights = extract_structured_insights(
        &state.openrouter,
        &state.model_config,
        session_id,
        &facts,
        existing.as_ref(),
        audit_user,
    )
    .await;
    if new_insights.as_object().is_none_or(|o| o.is_empty()) {
        return;
    }

    match insights_repo.merge(user_id, new_insights).await {
        Ok(row) => {
            // Write-through: project the just-merged JSONB into the flat
            // matching table. No extra read — merge returned the merged row.
            // Failure only warns; it must not break the chat turn.
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

async fn extract_facts(
    llm: &OpenRouterClient,
    model_config: &ModelConfig,
    session_id: Uuid,
    user_msg: &str,
    assistant_msg: &str,
    audit_user: Option<&str>,
) -> Vec<String> {
    if user_msg.trim().is_empty() {
        return vec![];
    }
    let prompt = crate::prompt::extract_facts_prompt(user_msg, assistant_msg);

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
        ..Default::default()
    };

    let raw = match llm.execute(req).await {
        Ok(resp) => {
            super::log_openrouter_usage(INSIGHT_TASK, Some(session_id), &resp);
            resp.reply.trim().to_string()
        }
        Err(e) => {
            tracing::warn!("fact extraction LLM call failed: {e}");
            return vec![];
        }
    };

    parse_facts(&raw)
}

fn parse_facts(raw: &str) -> Vec<String> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        return extract_facts_array(&v);
    }
    if let Some(block) = find_json_block(raw) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(block) {
            return extract_facts_array(&v);
        }
    }
    vec![]
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
) -> serde_json::Value {
    if facts.is_empty() {
        return serde_json::Value::Object(serde_json::Map::new());
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
        ..Default::default()
    };

    let raw = match llm.execute(req).await {
        Ok(r) => {
            super::log_openrouter_usage(INSIGHT_TASK, Some(session_id), &r);
            r.reply.trim().to_string()
        }
        Err(_) => return serde_json::Value::Object(serde_json::Map::new()),
    };

    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
        if v.is_object() {
            return v;
        }
    }
    if let Some(block) = find_json_block(&raw) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(block) {
            if v.is_object() {
                return v;
            }
        }
    }
    serde_json::Value::Object(serde_json::Map::new())
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
        };
        assert_eq!(client_id_from_event(&event), None);
    }

    #[test]
    fn client_id_from_event_none_for_non_user_message() {
        let event = Event::Gift {
            gift_id: Uuid::new_v4(),
            amount: 100,
        };
        assert_eq!(client_id_from_event(&event), None);
    }

    #[test]
    fn parse_facts_empty_array() {
        assert!(parse_facts(r#"{"facts": []}"#).is_empty());
    }

    #[test]
    fn parse_facts_with_items() {
        let raw = r#"{"facts": ["住在上海", "喜欢咖啡"]}"#;
        let facts = parse_facts(raw);
        assert_eq!(facts, vec!["住在上海".to_string(), "喜欢咖啡".to_string()]);
    }

    #[test]
    fn parse_facts_regex_fallback_in_fenced_code() {
        let raw = "Here you go:\n```json\n{\"facts\": [\"爱猫\"]}\n```";
        let facts = parse_facts(raw);
        assert_eq!(facts, vec!["爱猫".to_string()]);
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
}
