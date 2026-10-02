// SPDX-License-Identifier: AGPL-3.0-only
//! The holiday greeting: one persona-initiated assistant message. The route
//! (`routes::session_open`) decides whether the persona speaks first; this
//! module generates and persists it. Modelled on the image-edit text half:
//! no PDE, no streaming, no user row, no post-process.
//!
//! Spec: docs/superpowers/specs/2026-09-26-user-locale-and-holiday-greeting-design.md §6

use chrono::{DateTime, NaiveDate, Utc};
use eros_engine_core::persona::CompanionPersona;
use eros_engine_core::scope::{AffinityScope, MemoryScope};
use eros_engine_core::types::{
    ActionPlan, ActionType, DecisionInput, Event, ImageRef, LlmAudit, PromptTrait, ReplyStyle,
};
use eros_engine_llm::failure::AttemptFailure;
use eros_engine_llm::model_config::apply_output_regex;
use eros_engine_llm::openrouter::{ChatMessage as WireMessage, ChatResponse};
use eros_engine_llm::provider::bare_model_id;
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::{ChatMessage, ChatRepo, ProactiveInsert};
use ulid::Ulid;
use uuid::Uuid;

use crate::error::AppError;
use crate::pipeline::handlers::{build_reply_request, CHAT_TASK};
use crate::pipeline::stream::split_failures;
use crate::pipeline::{compute_signals_for_session, record_generation, GenerationRecord};
use crate::prompt::NowContext;
use crate::state::AppState;

/// Speaking first is the engine's decision (spec §6.1), delivered as a stage
/// cue after the history. A `user` turn, so the request never ends on an
/// assistant message a provider would continue as prefill. Never persisted.
pub(crate) const OPEN_CUE: &str = "（对方刚打开和你的聊天，还没说话。）";

/// One greeting to generate. `metadata` arrives holding the caller's audit
/// copy of the request (the raw locale); `greet` adds the rest.
pub(crate) struct Greeting<'a> {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub persona: &'a CompanionPersona,
    pub now: &'a NowContext,
    pub local_date: NaiveDate,
    /// The user's local midnight on `local_date`, in UTC — the inactivity
    /// cutoff `insert_proactive_message` re-checks atomically with the write.
    pub since: DateTime<Utc>,
    pub holidays: Vec<String>,
    pub tier: Option<String>,
    pub prompt_traits: Vec<PromptTrait>,
    pub memory_scope: MemoryScope,
    pub affinity_scope: AffinityScope,
    pub audit: Option<LlmAudit>,
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// Generate and persist the greeting. `Ok(None)`: the model returned a blank
/// reply and nothing was written. When a concurrent call for the same local
/// date won the insert, returns that call's row.
pub(crate) async fn greet(
    state: &AppState,
    g: Greeting<'_>,
) -> Result<Option<ChatMessage>, AppError> {
    let mut affinity = AffinityRepo { pool: &state.pool }
        .load_or_create(g.session_id, g.user_id, g.instance_id)
        .await?;
    affinity.apply_time_decay();
    affinity.refresh_endpoints(&state.config.affinity_tuning);
    let signals = compute_signals_for_session(&state.pool, g.session_id, &affinity).await?;

    // Prompt assembly only. The holiday names double as the memory-recall
    // query; the nil driving id makes history fall back to newest-N.
    let input = DecisionInput {
        event: Event::UserMessage {
            content: g.holidays.join("、"),
            message_id: Uuid::nil(),
            prompt_traits: g.prompt_traits,
            audit: g.audit,
            tier: g.tier.clone(),
            memory_scope: g.memory_scope,
            affinity_scope: g.affinity_scope,
            tips_amount_usd: None,
            quote: None,
        },
        affinity,
        persona: g.persona.clone(),
        signals,
    };
    let plan = ActionPlan {
        action_type: ActionType::ReplyText,
        reply_style: ReplyStyle::Neutral,
        affinity_deltas: Default::default(),
        energy_cost: 0.0,
        context_hints: vec![],
        reply_mode: None,
        nudges: Default::default(),
        clothing: None,
        image_caption: None,
        image_ref: ImageRef::Previous,
        aspect_ratio: None,
    };
    let (mut chat_req, injected_tags) = build_reply_request(
        state,
        &input,
        &plan,
        g.session_id,
        g.user_id,
        g.instance_id,
        Uuid::nil(),
        g.now,
    )
    .await?;
    chat_req.messages.push(WireMessage {
        role: "user".into(),
        content: OPEN_CUE.into(),
    });

    let model_for_audit = chat_req.model.clone();
    // `execute`'s candidate list, kept to find the hop that served.
    let chain: Vec<String> = std::iter::once(chat_req.model.clone())
        .chain(chat_req.fallback_model.iter().cloned())
        .filter(|m| !m.is_empty())
        .collect();
    let resp = match state.openrouter.execute(chat_req).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(session_id = %g.session_id, "holiday greeting: companion call failed: {e}");
            let failure = match &e {
                eros_engine_llm::LlmError::Chain { failures } if !failures.is_empty() => {
                    failures.last().cloned().expect("non-empty chain")
                }
                other => AttemptFailure::from_llm_error(CHAT_TASK, &model_for_audit, other),
            };
            return Err(AppError::Upstream(Box::new(failure)));
        }
    };
    let generation_id = record_generation(
        &state.pool,
        GenerationRecord {
            task: CHAT_TASK,
            session_id: Some(g.session_id),
            generation_id: resp.generation_id.as_deref(),
            model: resp.model.as_deref(),
            usage: resp.usage.as_ref(),
        },
    )
    .await;
    // Layer 0, as on every served stream reply. A reply the rules empty is blank.
    let stripped = apply_output_regex(
        &state.output_regex,
        &served_model_key(&chain, &resp),
        &resp.reply,
    );
    let content = stripped.cleaned.trim();
    if content.is_empty() {
        tracing::warn!(session_id = %g.session_id, "holiday greeting: blank reply, nothing written");
        return Ok(None);
    }
    let (llm_attempts, gateway_errors) = split_failures(&resp.failures);

    let local_date = g.local_date.to_string();
    let mut metadata = g.metadata;
    metadata.insert("proactive".into(), "holiday".into());
    metadata.insert("local_date".into(), local_date.clone().into());
    metadata.insert("holidays".into(), serde_json::json!(g.holidays));
    metadata.insert("prompt_traits".into(), serde_json::json!(injected_tags));
    metadata.insert(
        "memory_scope".into(),
        serde_json::to_value(g.memory_scope).expect("MemoryScope serializes"),
    );
    metadata.insert(
        "affinity_scope".into(),
        serde_json::to_value(g.affinity_scope).expect("AffinityScope serializes"),
    );
    if let Some(t) = g.tier {
        metadata.insert("tier".into(), t.into());
    }

    let chat_repo = ChatRepo { pool: &state.pool };
    let row = ProactiveInsert {
        id: Ulid::new().into(),
        content: content.to_string(),
        generation_id,
        metadata: serde_json::Value::Object(metadata),
        llm_attempts,
        gateway_errors,
    };
    match chat_repo
        .insert_proactive_message(g.session_id, g.since, &row)
        .await?
    {
        Some(written) => Ok(Some(written)),
        None => Ok(chat_repo
            .proactive_greeting_on(g.session_id, &local_date)
            .await?),
    }
}

/// Bare id of the chain hop that served `resp`: the key `output_regex` rules
/// are written under, as on the stream path. `execute` does not say which hop
/// served. A reported model that is one of the chain's ids names it; otherwise
/// the served hop is the one after the recorded failures.
fn served_model_key(chain: &[String], resp: &ChatResponse) -> String {
    match resp.model.as_deref().map(bare_model_id) {
        Some(id) if chain.iter().any(|m| bare_model_id(m) == id) => id,
        _ => chain
            .get(resp.failures.len())
            .map(|m| bare_model_id(m))
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_served_hop_keys_output_regex() {
        let chain = vec!["companion".to_string(), "backup@venice".to_string()];
        let reported = |model: &str| ChatResponse {
            model: Some(model.into()),
            ..Default::default()
        };
        // A provider reporting an id of its own: the hop after the recorded
        // failures (none here) served.
        assert_eq!(
            served_model_key(&chain, &reported("served/companion")),
            "companion"
        );
        // A reported id that is one of the chain's names its hop, e.g. a
        // repaired garble, which records no failure for the hops before it.
        assert_eq!(
            served_model_key(&chain, &reported("backup@venice")),
            "backup"
        );
    }
}
