// SPDX-License-Identifier: AGPL-3.0-only
//! Action handlers — one per ActionType. Each assembles a ChatRequest
//! (or None if no LLM call is needed) based on the PDE's ActionPlan.
//!
//! Ported from `eros-gateway/src/engine/handlers/{reply,ghost,gift,proactive}.rs`
//! with these OSS-specific changes:
//!
//! - All handlers go through `eros_engine_store::chat::ChatRepo` rather than
//!   inline `sqlx::query_as` against `chat_messages`.
//! - `ChatRequest` is built around the OSS `eros_engine_llm::openrouter::ChatRequest`
//!   shape (`model` / `fallback_model` / `messages` / `temperature` / `max_tokens`),
//!   resolved via `state.model_config` at handler time. The gateway's
//!   `task: String` + `persona_override: Option<String>` indirection lives at
//!   the resolver call instead of being passed downstream.
//! - `GiftHandler` carries `deltas: AffinityDeltas` directly — there is
//!   no shop item / gift-record lookup since the OSS engine has no
//!   credit ledger.

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use eros_engine_core::affinity::AffinityDeltas;
use eros_engine_core::types::{ActionPlan, DecisionInput, Event};
use eros_engine_llm::openrouter::{ChatMessage, ChatRequest};
use eros_engine_store::chat::ChatRepo;
use eros_engine_store::insight::InsightRepo;
use eros_engine_store::memory::MemoryRepo;

use crate::error::AppError;
use crate::prompt::{build_prompt, PendingGift};
use crate::state::AppState;

/// Memory recall fan-out sizes — mirror the gateway's Mem0 era defaults
/// (`profile=4`, `relationship=3`). Tunable later if recall quality drifts.
const PROFILE_RECALL_K: i32 = 4;
const RELATIONSHIP_RECALL_K: i32 = 3;

/// Task key used by all chat handlers. Matches the gateway's task router.
const CHAT_TASK: &str = "chat_companion";

/// Maximum number of recent messages pulled into the prompt.
const HISTORY_WINDOW: i64 = 20;

#[async_trait]
pub trait ActionHandler: Send + Sync {
    async fn handle(
        &self,
        input: &DecisionInput,
        plan: &ActionPlan,
    ) -> Result<Option<ChatRequest>, AppError>;
}

/// Read the persona model override out of `art_metadata.model`, if any.
fn persona_model_override(input: &DecisionInput) -> Option<String> {
    input
        .persona
        .genome
        .art_metadata
        .get("model")
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Materialise a ChatRequest from a system prompt + chronological history.
fn assemble_chat_request(
    state: &AppState,
    input: &DecisionInput,
    system_prompt: String,
    history: Vec<eros_engine_store::chat::ChatMessage>,
) -> ChatRequest {
    let mut messages = Vec::with_capacity(history.len() + 1);
    messages.push(ChatMessage {
        role: "system".to_string(),
        content: system_prompt,
    });
    for msg in history {
        // ChatRepo::history returns ascending chronological order already.
        match msg.role.as_str() {
            "user" | "assistant" => messages.push(ChatMessage {
                role: msg.role,
                content: msg.content,
            }),
            // skip tip_user, gift_user, system_error, etc.
            _ => continue,
        }
    }

    let resolved = state
        .model_config
        .resolve(CHAT_TASK, persona_model_override(input).as_deref());

    ChatRequest {
        model: resolved.model,
        fallback_model: resolved.fallback_model,
        messages,
        temperature: resolved.temperature as f32,
        max_tokens: resolved.max_tokens,
    }
}

// ─── Memory recall + insight injection helpers ────────────────────

/// Embed `query_text` once, then fan out to profile + relationship layers
/// in parallel. All errors degrade silently to empty vecs — recall failure
/// must never block a chat reply (the persona just looks slightly less
/// "with it" for that turn).
async fn recall_memory(
    state: &AppState,
    user_id: Uuid,
    instance_id: Uuid,
    query_text: &str,
) -> (Vec<String>, Vec<String>) {
    if query_text.trim().is_empty() {
        return (vec![], vec![]);
    }
    let embedding = match state.voyage.embed_query(query_text).await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("voyage embed_query failed: {e}");
            return (vec![], vec![]);
        }
    };
    let repo = MemoryRepo { pool: &state.pool };
    let (profile_res, rel_res) = tokio::join!(
        repo.search(user_id, None, &embedding, PROFILE_RECALL_K),
        repo.search(
            user_id,
            Some(instance_id),
            &embedding,
            RELATIONSHIP_RECALL_K,
        ),
    );
    let profile = match profile_res {
        Ok(rows) => rows.into_iter().map(|r| r.content).collect(),
        Err(e) => {
            tracing::warn!("profile-layer memory search failed: {e}");
            vec![]
        }
    };
    let relationship = match rel_res {
        Ok(rows) => rows.into_iter().map(|r| r.content).collect(),
        Err(e) => {
            tracing::warn!("relationship-layer memory search failed: {e}");
            vec![]
        }
    };
    (profile, relationship)
}

/// Load `companion_insights` for the user and render the structured fields
/// as Chinese-language bullets that fit naturally into the
/// `【你对他的了解（通用画像）】` prompt section.
async fn load_insight_bullets(state: &AppState, user_id: Uuid) -> Vec<String> {
    let repo = InsightRepo { pool: &state.pool };
    let row = match repo.load(user_id).await {
        Ok(Some(row)) => row,
        Ok(None) => return vec![],
        Err(e) => {
            tracing::warn!("insight load failed: {e}");
            return vec![];
        }
    };
    insights_to_bullets(&row.insights)
}

/// Render the `companion_insights` JSONB blob as bullet strings. Skips
/// empty / missing fields. `matching_preferences` is intentionally omitted
/// — it's a structured sub-object that doesn't fit a single-line bullet
/// and isn't useful in chat tone anyway.
fn insights_to_bullets(insights: &Value) -> Vec<String> {
    let Some(obj) = insights.as_object() else {
        return vec![];
    };
    let mut out = Vec::new();

    let push_str = |out: &mut Vec<String>, key: &str, label: &str| {
        if let Some(s) = obj.get(key).and_then(Value::as_str) {
            let s = s.trim();
            if !s.is_empty() {
                out.push(format!("{label}：{s}"));
            }
        }
    };
    let push_arr = |out: &mut Vec<String>, key: &str, label: &str| {
        if let Some(arr) = obj.get(key).and_then(Value::as_array) {
            let parts: Vec<&str> = arr
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if !parts.is_empty() {
                out.push(format!("{label}：{}", parts.join("、")));
            }
        }
    };

    push_str(&mut out, "city", "城市");
    push_str(&mut out, "occupation", "职业");
    push_str(&mut out, "mbti_guess", "MBTI");
    push_str(&mut out, "love_values", "感情观");
    push_arr(&mut out, "interests", "兴趣");
    push_str(&mut out, "emotional_needs", "情感需求");
    push_str(&mut out, "life_rhythm", "作息");
    push_arr(&mut out, "personality_traits", "性格特质");

    out
}

// ─── Reply ──────────────────────────────────────────────────────────

pub struct ReplyHandler<'a> {
    pub state: &'a AppState,
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Uuid,
}

#[async_trait]
impl<'a> ActionHandler for ReplyHandler<'a> {
    async fn handle(
        &self,
        input: &DecisionInput,
        plan: &ActionPlan,
    ) -> Result<Option<ChatRequest>, AppError> {
        let chat_repo = ChatRepo {
            pool: &self.state.pool,
        };
        let history = chat_repo
            .history(self.session_id, HISTORY_WINDOW, 0)
            .await?;

        // Use the current user message as the recall query.
        let query_text = match &input.event {
            Event::UserMessage { content, .. } => content.as_str(),
            _ => "",
        };

        let (mut profile_facts, relationship_facts) =
            recall_memory(self.state, self.user_id, self.instance_id, query_text).await;

        // T14: prepend structured insights so the LLM sees both the JSONB
        // profile (e.g. city/MBTI) and the pgvector profile-layer recalls
        // in the same `【你对他的了解（通用画像）】` section.
        let insight_bullets = load_insight_bullets(self.state, self.user_id).await;
        if !insight_bullets.is_empty() {
            let mut combined = insight_bullets;
            combined.append(&mut profile_facts);
            profile_facts = combined;
        }

        let tip_personality = input
            .persona
            .genome
            .tip_personality
            .as_deref()
            .unwrap_or("normal");

        // Reply path never has pending gifts — those flow through GiftHandler.
        let pending_gifts: Vec<PendingGift> = vec![];

        let system_prompt = build_prompt(
            &input.persona,
            &profile_facts,
            &relationship_facts,
            Some(&input.affinity),
            &pending_gifts,
            tip_personality,
            plan.reply_style,
            &plan.context_hints,
        );

        Ok(Some(assemble_chat_request(
            self.state,
            input,
            system_prompt,
            history,
        )))
    }
}

// ─── Ghost ──────────────────────────────────────────────────────────

/// Ghost handler is intentionally a no-op at the chat-request layer:
/// the affinity row update happens in `pipeline::post_process`. The
/// `state` / `session_id` fields are kept for future tracing hooks and
/// for symmetry with the other handlers.
#[allow(dead_code)]
pub struct GhostHandler<'a> {
    pub state: &'a AppState,
    pub session_id: Uuid,
}

#[async_trait]
impl<'a> ActionHandler for GhostHandler<'a> {
    async fn handle(
        &self,
        _input: &DecisionInput,
        _plan: &ActionPlan,
    ) -> Result<Option<ChatRequest>, AppError> {
        tracing::info!("Ghost decision: session={}", self.session_id);
        // Affinity mutation and DB write happen in pipeline::post_process,
        // which sees ActionType::Ghost and calls AffinityRepo::record_ghost.
        Ok(None)
    }
}

// ─── Gift ───────────────────────────────────────────────────────────

/// Gift reaction handler.
///
/// Replaces the gateway's shop-item / gift-record lookup. The OSS engine
/// has no credit ledger — the gift event endpoint (T11) injects the
/// affinity deltas and an optional pending-gift list directly.
pub struct GiftHandler<'a> {
    pub state: &'a AppState,
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Uuid,
    /// Caller-supplied deltas — passed through to the post-process step
    /// via the ActionPlan / event channel; not consumed inside `handle()`.
    #[allow(dead_code)]
    pub deltas: AffinityDeltas,
    /// Caller-supplied pending gifts (possibly empty) for prompt context.
    pub pending: Vec<PendingGift>,
}

#[async_trait]
impl<'a> ActionHandler for GiftHandler<'a> {
    async fn handle(
        &self,
        input: &DecisionInput,
        plan: &ActionPlan,
    ) -> Result<Option<ChatRequest>, AppError> {
        let chat_repo = ChatRepo {
            pool: &self.state.pool,
        };
        let history = chat_repo
            .history(self.session_id, HISTORY_WINDOW, 0)
            .await?;

        // Gift events have no user message of their own. Fall back to the
        // most recent user turn from history so memory recall still has a
        // semantic anchor (e.g. user said "I miss you" → tipped → reaction
        // can reference what they were just talking about).
        let query_text = history
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .unwrap_or("");

        let (mut profile_facts, relationship_facts) =
            recall_memory(self.state, self.user_id, self.instance_id, query_text).await;

        let insight_bullets = load_insight_bullets(self.state, self.user_id).await;
        if !insight_bullets.is_empty() {
            let mut combined = insight_bullets;
            combined.append(&mut profile_facts);
            profile_facts = combined;
        }

        let tip_personality = input
            .persona
            .genome
            .tip_personality
            .as_deref()
            .unwrap_or("normal");

        let system_prompt = build_prompt(
            &input.persona,
            &profile_facts,
            &relationship_facts,
            Some(&input.affinity),
            &self.pending,
            tip_personality,
            plan.reply_style,
            &plan.context_hints,
        );

        Ok(Some(assemble_chat_request(
            self.state,
            input,
            system_prompt,
            history,
        )))
    }
}

// ─── Proactive ──────────────────────────────────────────────────────

/// Proactive handler is a stub today — Phase 6 in the gateway / a
/// later OSS milestone produces an outbound message here. Fields kept
/// for the eventual implementation.
#[allow(dead_code)]
pub struct ProactiveHandler<'a> {
    pub state: &'a AppState,
    pub session_id: Uuid,
}

#[async_trait]
impl<'a> ActionHandler for ProactiveHandler<'a> {
    async fn handle(
        &self,
        _input: &DecisionInput,
        _plan: &ActionPlan,
    ) -> Result<Option<ChatRequest>, AppError> {
        // Stub — Phase 6 in the gateway / a later OSS milestone will
        // produce an outbound message here.
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn insights_to_bullets_empty_object() {
        assert!(insights_to_bullets(&json!({})).is_empty());
    }

    #[test]
    fn insights_to_bullets_non_object_returns_empty() {
        assert!(insights_to_bullets(&json!("just a string")).is_empty());
        assert!(insights_to_bullets(&json!(["array", "not", "object"])).is_empty());
        assert!(insights_to_bullets(&json!(null)).is_empty());
    }

    #[test]
    fn insights_to_bullets_skips_empty_strings() {
        let v = json!({
            "city": "",
            "occupation": "   ",
            "mbti_guess": "INFP",
        });
        let bullets = insights_to_bullets(&v);
        assert_eq!(bullets, vec!["MBTI：INFP".to_string()]);
    }

    #[test]
    fn insights_to_bullets_renders_string_fields() {
        let v = json!({
            "city": "上海",
            "occupation": "产品经理",
            "mbti_guess": "ENFJ",
            "love_values": "重视沟通",
            "emotional_needs": "需要被认可",
            "life_rhythm": "夜猫子",
        });
        let bullets = insights_to_bullets(&v);
        assert_eq!(
            bullets,
            vec![
                "城市：上海".to_string(),
                "职业：产品经理".to_string(),
                "MBTI：ENFJ".to_string(),
                "感情观：重视沟通".to_string(),
                "情感需求：需要被认可".to_string(),
                "作息：夜猫子".to_string(),
            ]
        );
    }

    #[test]
    fn insights_to_bullets_joins_arrays_and_skips_blanks() {
        let v = json!({
            "interests": ["登山", "  ", "精酿"],
            "personality_traits": ["真诚", "敏感"],
        });
        let bullets = insights_to_bullets(&v);
        assert_eq!(
            bullets,
            vec![
                "兴趣：登山、精酿".to_string(),
                "性格特质：真诚、敏感".to_string(),
            ]
        );
    }

    #[test]
    fn insights_to_bullets_omits_matching_preferences() {
        let v = json!({
            "city": "北京",
            "matching_preferences": {
                "preferred_gender": "female",
                "age_range": [25, 35],
                "deal_breakers": ["smoking"],
            },
        });
        let bullets = insights_to_bullets(&v);
        assert_eq!(bullets, vec!["城市：北京".to_string()]);
    }

    #[test]
    fn insights_to_bullets_preserves_canonical_field_order() {
        // Field order matters for prompt readability — city before MBTI,
        // emotional_needs before life_rhythm, etc.
        let v = json!({
            "personality_traits": ["真诚"],
            "city": "上海",
            "interests": ["登山"],
            "occupation": "工程师",
        });
        let bullets = insights_to_bullets(&v);
        assert_eq!(
            bullets,
            vec![
                "城市：上海".to_string(),
                "职业：工程师".to_string(),
                "兴趣：登山".to_string(),
                "性格特质：真诚".to_string(),
            ]
        );
    }
}
