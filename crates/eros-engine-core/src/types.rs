// SPDX-License-Identifier: AGPL-3.0-only
//! Public types for the companion engine.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::affinity::{Affinity, AffinityDeltas};
use crate::persona::CompanionPersona;

/// A caller-supplied system-prompt fragment. The engine treats `text` as
/// opaque — it is inserted verbatim under the `【附加指引】` section of
/// the persona system prompt. `tag` is for logging/observability only and
/// is constrained to `[a-z0-9_]{1,32}` by the HTTP layer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptTrait {
    pub tag: String,
    pub text: String,
}

/// Caller-supplied OpenRouter passthrough for per-request audit /
/// analytics. Engine never inspects these fields; they ride straight to
/// `openrouter.ai/api/v1/chat/completions` as wire-level `user`,
/// `session_id`, `metadata`. The HTTP layer applies size/shape caps;
/// content remains opaque to the engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct LlmAudit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Events that drive the engine pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    UserMessage {
        content: String,
        message_id: Uuid,
        /// Optional caller-supplied prompt traits. Empty for clients that
        /// don't send the field — preserves the legacy system-prompt output
        /// byte-for-byte.
        #[serde(default)]
        prompt_traits: Vec<PromptTrait>,
        /// Optional caller-supplied OpenRouter audit passthrough.
        /// `None` for clients that don't send the field. Engine carries it
        /// opaquely from request → handler → ChatRequest; content is never
        /// inspected.
        #[serde(default)]
        audit: Option<LlmAudit>,
    },
    Gift {
        gift_id: Uuid,
        amount: i64,
    },
    ProactiveTrigger,
    AppOpen,
}

/// Action decision produced by the PDE.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ActionType {
    Reply,
    Ghost,
    Proactive,
    GiftReaction,
}

/// Tone directive for the reply generation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReplyStyle {
    Warm,
    Neutral,
    Cold,
    Tsundere,
    Excited,
}

/// Output of the PDE.
#[derive(Debug, Clone)]
pub struct ActionPlan {
    pub action_type: ActionType,
    pub reply_style: ReplyStyle,
    pub affinity_deltas: AffinityDeltas,
    pub energy_cost: f64,
    pub context_hints: Vec<String>,
}

/// Conversation signals computed from chat history.
#[derive(Debug, Clone)]
pub struct ConversationSignals {
    pub message_count: i64,
    pub hours_since_last_message: f64,
    pub ghost_streak: i32,
    pub hours_since_last_ghost: Option<f64>,
}

/// Response from the chat engine — text reply plus opaque OpenRouter
/// wire echoes (`generation_id`, `model`, `usage`). The three echo
/// fields are `None` when no LLM call was made (handler returned None)
/// or when upstream omitted them.
#[derive(Debug, Clone, Default)]
pub struct ChatResponse {
    pub reply: String,
    pub generation_id: Option<String>,
    pub model: Option<String>,
    pub usage: Option<serde_json::Value>,
}

/// Input bundle consumed by the PDE.
#[derive(Clone)]
pub struct DecisionInput {
    pub event: Event,
    pub affinity: Affinity,
    pub persona: CompanionPersona,
    pub signals: ConversationSignals,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_user_message_defaults_prompt_traits_to_empty_vec() {
        let raw = r#"{"UserMessage":{"content":"hi","message_id":"00000000-0000-0000-0000-000000000001"}}"#;
        let ev: Event = serde_json::from_str(raw).expect("legacy body deserialises");
        match ev {
            Event::UserMessage { prompt_traits, .. } => {
                assert!(prompt_traits.is_empty(), "missing field must default to []");
            }
            _ => panic!("expected UserMessage"),
        }
    }

    #[test]
    fn prompt_trait_round_trips_through_serde() {
        let t = PromptTrait {
            tag: "nsfw_boost".into(),
            text: "be more daring".into(),
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: PromptTrait = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn llm_audit_serde_roundtrip_full() {
        let mut metadata = serde_json::Map::new();
        metadata.insert("plan".into(), serde_json::Value::String("pro".into()));
        let a = LlmAudit {
            user: Some("u_abc".into()),
            session_id: Some("conv_xyz".into()),
            metadata: Some(metadata),
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: LlmAudit = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn llm_audit_serde_roundtrip_empty_yields_all_none() {
        let a: LlmAudit = serde_json::from_str("{}").unwrap();
        assert!(a.user.is_none());
        assert!(a.session_id.is_none());
        assert!(a.metadata.is_none());
    }

    #[test]
    fn event_user_message_defaults_audit_to_none() {
        let raw = r#"{"UserMessage":{"content":"hi","message_id":"00000000-0000-0000-0000-000000000001"}}"#;
        let ev: Event = serde_json::from_str(raw).expect("legacy body deserialises");
        match ev {
            Event::UserMessage { audit, .. } => {
                assert!(audit.is_none(), "missing audit field must default to None");
            }
            _ => panic!("expected UserMessage"),
        }
    }

    #[test]
    fn chat_response_defaults_audit_fields_to_none() {
        let r = ChatResponse { reply: "hi".into(), ..Default::default() };
        assert!(r.usage.is_none());
        assert!(r.generation_id.is_none());
        assert!(r.model.is_none());
    }
}
