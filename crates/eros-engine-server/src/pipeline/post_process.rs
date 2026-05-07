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

use eros_engine_core::types::{ActionPlan, ActionType, ChatResponse, Event};
use eros_engine_llm::model_config::ModelConfig;
use eros_engine_llm::openrouter::{ChatMessage, ChatRequest, OpenRouterClient};
use eros_engine_llm::voyage::VoyageClient;
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::insight::InsightRepo;
use eros_engine_store::memory::{MemoryLayer, MemoryRepo};

use crate::state::AppState;

// ─── Top-level dispatcher ──────────────────────────────────────────

/// Spawned by `pipeline::run`. Owned `state` so the future is `'static`.
pub async fn run(
    state: AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    event: Event,
    plan: ActionPlan,
    response: Option<ChatResponse>,
) {
    let user_msg = match &event {
        Event::UserMessage { content, .. } => content.clone(),
        _ => String::new(),
    };
    let reply = response
        .as_ref()
        .map(|r| r.reply.clone())
        .unwrap_or_default();

    let fut_insight = async {
        if !user_msg.is_empty() && !reply.is_empty() {
            extract_insights(&state, user_id, &user_msg, &reply).await;
        }
    };

    let fut_memory = async {
        if !user_msg.is_empty() && !reply.is_empty() {
            write_turn(&state, session_id, user_id, instance_id, &user_msg, &reply).await;
        }
    };

    let fut_affinity = persist_affinity(
        &state,
        session_id,
        user_id,
        instance_id,
        plan.action_type,
        plan.affinity_deltas.clone(),
    );

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
) {
    let repo = AffinityRepo { pool: &state.pool };

    let mut affinity = match repo.load_or_create(session_id, user_id, instance_id).await {
        Ok(mut a) => {
            a.apply_time_decay();
            a
        }
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
                .persist_with_event(
                    &mut affinity,
                    &deltas,
                    state.config.ema_inertia,
                    event_type,
                    serde_json::json!({}),
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
    repo.upsert(layer, session_id, user_id, instance_id, content, &embedding)
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

const SCHEMA_DESC: &str = r#"
companion_insights schema (all fields optional, only include if confident):
{
  "city": "string — user's city",
  "occupation": "string — job/career",
  "mbti_guess": "string — e.g. INFP",
  "love_values": "string — attitude toward love & relationships",
  "interests": ["list", "of", "hobbies"],
  "emotional_needs": "string — what emotional support they need",
  "life_rhythm": "string — e.g. 夜貓子, 早睡早起",
  "matching_preferences": {
    "preferred_gender": "string",
    "age_range": [min_int, max_int],
    "deal_breakers": ["list"]
  },
  "personality_traits": ["list", "of", "traits"]
}
Return ONLY a JSON object with the fields you are confident about.
Do not invent or guess anything not clearly supported by the facts.
"#;

const INSIGHT_TASK: &str = "insight_extraction";

/// Top-level entry: extract facts → structured insights → InsightRepo merge.
async fn extract_insights(state: &AppState, user_id: Uuid, user_msg: &str, assistant_msg: &str) {
    let facts = extract_facts(
        &state.openrouter,
        &state.model_config,
        user_msg,
        assistant_msg,
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
        &facts,
        existing.as_ref(),
    )
    .await;
    if new_insights.as_object().is_none_or(|o| o.is_empty()) {
        return;
    }

    if let Err(e) = insights_repo.merge(user_id, new_insights).await {
        tracing::warn!("companion_insights merge failed: {e}");
    }
}

async fn extract_facts(
    llm: &OpenRouterClient,
    model_config: &ModelConfig,
    user_msg: &str,
    assistant_msg: &str,
) -> Vec<String> {
    if user_msg.trim().is_empty() {
        return vec![];
    }
    let prompt = format!(
        "分析以下一轮对话，列出你对用户的新事实发现（仅限用户，不是 AI）。\n\n\
         用户: {user_msg}\n\
         AI:   {assistant_msg}\n\n\
         如果没有新的用户事实，返回空数组 []。\n\
         严格输出 JSON，格式: {{\"facts\": [\"事实1\", \"事实2\"]}}",
    );

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
    };

    let raw = match llm.execute(req).await {
        Ok(resp) => resp.reply.trim().to_string(),
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
    facts: &[String],
    existing_insights: Option<&serde_json::Value>,
) -> serde_json::Value {
    if facts.is_empty() {
        return serde_json::Value::Object(serde_json::Map::new());
    }

    let facts_str = facts
        .iter()
        .map(|f| format!("- {f}"))
        .collect::<Vec<_>>()
        .join("\n");
    let existing_str = existing_insights
        .map(|v| serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".into()))
        .unwrap_or_else(|| "{}".into());

    let prompt = format!(
        "以下是從對話中提取的用戶事實：\n\
         {facts_str}\n\n\
         現有的 companion_insights（供參考，不要重複已知信息）：\n\
         {existing_str}\n\n\
         請根據上方的事實，填充以下 schema 中你有信心的字段：\n\
         {SCHEMA_DESC}\n\n\
         僅輸出 JSON，不要任何解釋。",
    );

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
    };

    let raw = match llm.execute(req).await {
        Ok(r) => r.reply.trim().to_string(),
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
}
