// SPDX-License-Identifier: AGPL-3.0-only
//! Public types for the companion engine.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::affinity::{Affinity, AffinityDeltas};
use crate::persona::CompanionPersona;
use crate::scope::{AffinityScope, MemoryScope};

/// A caller-supplied system-prompt fragment. The engine treats `text` as
/// opaque — it is inserted verbatim under the `[additional_guidance]` section of
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
        /// Optional caller-supplied tier (downstream-defined, e.g. "free" /
        /// "gold"). `None` for clients that don't send it. Routed opaquely to
        /// `model_config.resolve` to pick the per-tier model + allow_traits.
        #[serde(default)]
        tier: Option<String>,
        /// Optional caller-supplied memory injection scope (#40). Defaults to
        /// `neutral_and_relationship` when absent.
        #[serde(default)]
        memory_scope: MemoryScope,
        /// Optional caller-supplied affinity-axis injection scope (#40).
        /// Defaults to `bond` when absent.
        #[serde(default)]
        affinity_scope: AffinityScope,
        /// Optional caller-supplied tip amount in USD. When `Some`, this turn
        /// is a tip: the PDE forces a reply (never ghost) and the reply prompt
        /// gets a tip fragment. `None` for normal messages.
        #[serde(default)]
        tips_amount_usd: Option<f64>,
    },
    ProactiveTrigger,
    AppOpen,
}

/// Which reference image an image turn should build on. `Face` = the static
/// avatar / face reference; `Previous` = the previously generated image
/// (iteration). Defaults to `Face`. Internal-only (mapped from the PDE verdict);
/// not serialized to any DB/SSE wire path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageRef {
    #[default]
    Face,
    Previous,
}

/// Action decision produced by the PDE. NOT serialized to any DB/SSE wire path
/// (the persisted action string and `FrameActionType` are separate) — so the
/// rename is internal-only and the serde derive is intentionally absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionType {
    ReplyText,
    Ghost,
    ReplyImage,     // reserved — degrades to ReplyText until the image executor ships
    ReplyTextImage, // reserved — degrades to ReplyText until the image executor ships
    Proactive, // KEPT — built by pde::decide for ProactiveTrigger/AppOpen; matched in post_process
}

impl ActionType {
    /// True for actions that produce a text reply downstream. Image variants are
    /// included so they route through the reply path once the executor ships;
    /// today the PDE guardrails degrade them to `ReplyText` first.
    pub fn is_text_reply(self) -> bool {
        matches!(
            self,
            ActionType::ReplyText | ActionType::ReplyImage | ActionType::ReplyTextImage
        )
    }
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
    /// Subject for the image executor (`reply_image`/`reply_text_image`); `None`
    /// for text/ghost/proactive. Carried from the PDE verdict or a forced turn.
    pub image_prompt: Option<String>,
    /// Which reference image an image turn builds on (avatar vs previous gen).
    /// `Face` for non-image actions.
    pub image_ref: ImageRef,
    /// Aspect ratio chosen by the PDE for an image turn; `None` ⇒ caller falls
    /// back (request → config default). Always `None` for non-image actions.
    pub aspect_ratio: Option<String>,
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
    fn event_user_message_defaults_tier_to_none() {
        let raw = r#"{"UserMessage":{"content":"hi","message_id":"00000000-0000-0000-0000-000000000001"}}"#;
        let ev: Event = serde_json::from_str(raw).expect("legacy body deserialises");
        match ev {
            Event::UserMessage { tier, .. } => {
                assert!(tier.is_none(), "missing tier must default to None")
            }
            _ => panic!("expected UserMessage"),
        }
    }

    #[test]
    fn user_message_defaults_scopes_when_absent() {
        let raw = r#"{"UserMessage":{"content":"hi","message_id":"00000000-0000-0000-0000-000000000001"}}"#;
        let ev: Event = serde_json::from_str(raw).unwrap();
        match ev {
            Event::UserMessage {
                memory_scope,
                affinity_scope,
                ..
            } => {
                assert_eq!(
                    memory_scope,
                    crate::scope::MemoryScope::NeutralAndRelationship
                );
                assert_eq!(affinity_scope, crate::scope::AffinityScope::bond());
            }
            _ => panic!("expected UserMessage"),
        }
    }

    #[test]
    fn event_user_message_defaults_tips_amount_to_none() {
        let raw = r#"{"UserMessage":{"content":"hi","message_id":"00000000-0000-0000-0000-000000000001"}}"#;
        let ev: Event = serde_json::from_str(raw).expect("legacy body deserialises");
        match ev {
            Event::UserMessage {
                tips_amount_usd, ..
            } => {
                assert!(
                    tips_amount_usd.is_none(),
                    "missing field must default to None"
                );
            }
            _ => panic!("expected UserMessage"),
        }
    }

    #[test]
    fn chat_response_defaults_audit_fields_to_none() {
        let r = ChatResponse {
            reply: "hi".into(),
            ..Default::default()
        };
        assert!(r.usage.is_none());
        assert!(r.generation_id.is_none());
        assert!(r.model.is_none());
    }

    #[test]
    fn is_text_reply_truth_table() {
        assert!(ActionType::ReplyText.is_text_reply());
        assert!(ActionType::ReplyImage.is_text_reply());
        assert!(ActionType::ReplyTextImage.is_text_reply());
        assert!(!ActionType::Ghost.is_text_reply());
        assert!(!ActionType::Proactive.is_text_reply());
    }
}
