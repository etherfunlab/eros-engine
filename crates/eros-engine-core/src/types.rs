// SPDX-License-Identifier: AGPL-3.0-only
//! Public types for the companion engine.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::affinity::{Affinity, AffinityDeltas, AffinityLine};
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
#[allow(clippy::large_enum_variant)] // UserMessage is intentionally large; boxing would add indirection
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
        /// The message this turn quotes, resolved from the caller's
        /// `reply_to_message_id`. `None` on ordinary turns and when the id did
        /// not resolve. Never affects which history rows are injected — a
        /// quote points at one line, it does not rewind the conversation.
        #[serde(default)]
        quote: Option<QuotedMessage>,
        /// What the user did this turn besides talking. `None` on ordinary
        /// turns.
        #[serde(default)]
        action: Option<UserAction>,
        /// The outfit the caller named for this turn (spec
        /// 2026-10-12-persona-items-and-gift-design.md §6). `None` ⇒ the judge
        /// picks.
        #[serde(default)]
        outfit: Option<String>,
    },
    ProactiveTrigger,
    AppOpen,
}

/// A message the caller quoted via `reply_to_message_id`, resolved to its text.
/// Rendered as the prompt's `[quote]` block so the model knows which line the
/// user is pointing at — the history window is untouched either way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotedMessage {
    pub message_id: Uuid,
    /// `chat_messages.role` of the quoted row — decides whether the prompt
    /// attributes the line to the user or to the persona.
    pub role: String,
    pub content: String,
    /// When the quoted line was sent. Renders as a relative age ("3 天前") so
    /// the model can tell a callback to an old message from a same-breath one.
    pub sent_at: DateTime<Utc>,
    /// The user line the quoted message answered, when the quote should carry
    /// the exchange that produced it — a quoted picture on an image-edit full
    /// turn arrives with the message that asked for it. Rendered as a `用户：`
    /// line above the quoted one; `None` leaves the block a single line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
}

/// What a user's turn does to the persona besides talking (spec
/// 2026-10-05-chat-user-actions-design.md). Stored verbatim as the user row's
/// `metadata.action`, so the serde shape is a persisted contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserAction {
    Give {
        item: GivenItem,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        quantity: u8,
    },
    Kiss,
    Hug,
    Touch,
    Lick,
    Custom {
        text: String,
    },
    /// Money handed over (spec 2026-10-11-tip-as-action-design.md). The amount
    /// is the caller's; the engine records it and never verifies it.
    Tip {
        amount_usd: f64,
    },
    /// Something the persona keeps (spec
    /// 2026-10-12-persona-items-and-gift-design.md). Accepted, it joins her
    /// items or her wardrobe by `kind`.
    Gift {
        kind: ItemKind,
        name: String,
    },
}

impl UserAction {
    /// The line a refusal of this action drops: handing something over or
    /// giving something to keep is friendship, every physical gesture is
    /// romance. A refused tip drops nothing (spec 2026-10-11 §6).
    pub fn refusal_line(&self) -> Option<AffinityLine> {
        match self {
            UserAction::Give { .. } | UserAction::Gift { .. } => Some(AffinityLine::Bond),
            UserAction::Tip { .. } => None,
            _ => Some(AffinityLine::Chemistry),
        }
    }
}

/// What a `give` action hands over. The category is fixed; the optional
/// `name` on the action is free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GivenItem {
    Cigarette,
    Alcohol,
    Medicine,
}

/// Which of the persona's lists an item belongs to (spec
/// 2026-10-12-persona-items-and-gift-design.md): `item` for what she owns,
/// `clothing` for her wardrobe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Item,
    Clothing,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Item => "item",
            ItemKind::Clothing => "clothing",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "item" => Some(ItemKind::Item),
            "clothing" => Some(ItemKind::Clothing),
            _ => None,
        }
    }
}

/// Whether the persona accepts a user action. Decided by the PDE judge before
/// the reply; accept whenever the judge did not decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionResponse {
    Accept,
    Refuse,
}

impl ActionResponse {
    pub fn as_str(self) -> &'static str {
        match self {
            ActionResponse::Accept => "accept",
            ActionResponse::Refuse => "refuse",
        }
    }
}

/// Which reference image an image turn should build on. `Face` = the static
/// avatar / face reference; `Previous` = the previously generated image
/// (iteration). Defaults to `Face`. Serialized (snake_case) into the delegated
/// `image_request` SSE frame; deserialized from the PDE verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
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
    /// Out-of-character product answer (PDE action `product_qa`): a dedicated
    /// executor replies from operator-authored product docs; the turn is
    /// invisible to the companion brain. Never produced by the rule engine.
    ProductQa,
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

    /// True for actions that promise a photo this turn. An empty text
    /// completion on these is the expected shape of an image-only reply, not a
    /// silent turn — the stream's ghost-fallback tagging must not fire for
    /// them (the trailing `image_request` is the turn's real payload).
    pub fn promises_image(self) -> bool {
        matches!(self, ActionType::ReplyImage | ActionType::ReplyTextImage)
    }

    /// True for actions whose turn carries a text reply the persona speaks.
    /// Narrower than `is_text_reply`: a bare `ReplyImage` routes through the
    /// reply path but bears no text, so mode/dice/tone-like inputs drop.
    pub fn bears_text(self) -> bool {
        matches!(self, ActionType::ReplyText | ActionType::ReplyTextImage)
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

/// How this turn's reply relates to the user's message — the judge's call
/// (spec 2026-09-13 §3.1). `None` on the wire means the judge is unsure and
/// the engine's dice take over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyMode {
    Listen,
    Follow,
    Ask,
    Judge,
}

impl ReplyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ReplyMode::Listen => "listen",
            ReplyMode::Follow => "follow",
            ReplyMode::Ask => "ask",
            ReplyMode::Judge => "judge",
        }
    }
}

/// Engine-rolled per-turn nudges, rendered as the prompt's `[this_turn]`
/// block (all-false ⇒ block omitted). Rolled at plan finalisation in the
/// stream — never inside `build_prompt` — and serialised into the decision
/// audit payload (spec 2026-09-13 §3.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TurnNudges {
    pub affirm: bool,
    pub share_slice: bool,
    pub open_question: bool,
}

/// Output of the PDE.
#[derive(Debug, Clone)]
pub struct ActionPlan {
    pub action_type: ActionType,
    pub reply_style: ReplyStyle,
    pub affinity_deltas: AffinityDeltas,
    pub energy_cost: f64,
    pub context_hints: Vec<String>,
    /// Judge-decided reply mode for this turn. `Some` only on LLM-judge,
    /// text-bearing turns (reply_text / reply_text_image); `None` everywhere
    /// else — rule PDE, fail-open, tips, ghost, reply_image — and `None`
    /// means the dice own the turn's cadence.
    pub reply_mode: Option<ReplyMode>,
    /// The dice as rolled at plan finalisation. All-false when a definite
    /// reply_mode suppressed the roll, when the action bears no text, or on
    /// paths that never roll (proactive, edit turns).
    pub nudges: TurnNudges,
    /// What the persona has on this turn: the caller's named outfit when the
    /// event carries one, else the judge's (free text, sanitized by the
    /// server). Carried on every reply plan, a bare image turn included,
    /// since the composer dresses the picture with it; `None` on ghost,
    /// product_qa and proactive plans.
    pub clothing: Option<String>,
    /// What the picture showed, for post-process affinity on image turns.
    /// Written by the stream AFTER the composer resolves — always `None` at
    /// decision time, since the composer has not run yet. `None` also on the
    /// `raw` variant and on a failed compose, where the affinity evaluator
    /// falls back to a generic photo marker.
    pub image_caption: Option<String>,
    /// Which reference image an image turn builds on (avatar vs previous gen).
    /// `Face` for non-image actions.
    pub image_ref: ImageRef,
    /// Aspect ratio chosen by the PDE for an image turn; `None` ⇒ caller falls
    /// back (request → config default). Always `None` for non-image actions.
    pub aspect_ratio: Option<String>,
    /// The response in effect for this turn's user action: `Some` exactly when
    /// the event carries an action and the plan replies; `None` otherwise.
    pub action_response: Option<ActionResponse>,
}

/// Conversation signals computed from chat history.
#[derive(Debug, Clone, Default)]
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
    fn image_ref_serializes_snake_case() {
        use crate::types::ImageRef;
        assert_eq!(serde_json::to_value(ImageRef::Face).unwrap(), "face");
        assert_eq!(
            serde_json::to_value(ImageRef::Previous).unwrap(),
            "previous"
        );
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

    #[test]
    fn quote_defaults_to_none_and_event_omits_it() {
        // An event JSON without `quote` still parses, defaulting to None.
        let raw = r#"{"UserMessage":{"content":"hi","message_id":"00000000-0000-0000-0000-000000000001"}}"#;
        let ev: Event = serde_json::from_str(raw).unwrap();
        match ev {
            Event::UserMessage { quote, .. } => assert_eq!(quote, None),
            _ => panic!("expected UserMessage"),
        }

        // A resolved quote round-trips through serde — the queue path
        // serializes the whole event.
        let q = QuotedMessage {
            message_id: uuid::Uuid::nil(),
            role: "assistant".into(),
            content: "那我们礼拜六去看展".into(),
            sent_at: chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap(),
            trigger: None,
        };
        let s = serde_json::to_string(&q).unwrap();
        // `trigger: None` serializes without the key, so this also proves a
        // pre-`trigger` payload (the queue may hold old events) still parses.
        assert!(!s.contains("trigger"), "got {s}");
        assert_eq!(serde_json::from_str::<QuotedMessage>(&s).unwrap(), q);
    }

    #[test]
    fn user_action_serializes_flat_with_type_tag() {
        let give = UserAction::Give {
            item: GivenItem::Alcohol,
            name: Some("威士忌".into()),
            quantity: 2,
        };
        assert_eq!(
            serde_json::to_value(&give).unwrap(),
            serde_json::json!({"type": "give", "item": "alcohol", "name": "威士忌", "quantity": 2})
        );
        let nameless = UserAction::Give {
            item: GivenItem::Cigarette,
            name: None,
            quantity: 1,
        };
        assert_eq!(
            serde_json::to_value(&nameless).unwrap(),
            serde_json::json!({"type": "give", "item": "cigarette", "quantity": 1})
        );
        assert_eq!(
            serde_json::to_value(&UserAction::Kiss).unwrap(),
            serde_json::json!({"type": "kiss"})
        );
        assert_eq!(
            serde_json::to_value(&UserAction::Custom {
                text: "捏了捏你的脸".into()
            })
            .unwrap(),
            serde_json::json!({"type": "custom", "text": "捏了捏你的脸"})
        );
        let back: UserAction = serde_json::from_value(serde_json::json!({"type": "lick"})).unwrap();
        assert_eq!(back, UserAction::Lick);
    }

    #[test]
    fn user_action_line_maps_give_to_bond_and_the_rest_to_chemistry() {
        use crate::affinity::AffinityLine;
        let give = UserAction::Give {
            item: GivenItem::Medicine,
            name: None,
            quantity: 1,
        };
        assert_eq!(give.refusal_line().unwrap(), AffinityLine::Bond);
        for a in [
            UserAction::Kiss,
            UserAction::Hug,
            UserAction::Touch,
            UserAction::Lick,
            UserAction::Custom { text: "x".into() },
        ] {
            assert_eq!(a.refusal_line().unwrap(), AffinityLine::Chemistry, "{a:?}");
        }
    }

    #[test]
    fn gift_action_serde_shape() {
        let gift = UserAction::Gift {
            kind: ItemKind::Clothing,
            name: "红色旗袍".into(),
        };
        assert_eq!(
            serde_json::to_value(&gift).unwrap(),
            serde_json::json!({"type": "gift", "kind": "clothing", "name": "红色旗袍"})
        );
        let back: UserAction = serde_json::from_value(
            serde_json::json!({"type": "gift", "kind": "item", "name": "摩托车"}),
        )
        .unwrap();
        assert_eq!(
            back,
            UserAction::Gift {
                kind: ItemKind::Item,
                name: "摩托车".into()
            }
        );
    }

    #[test]
    fn a_refused_gift_drops_bond_like_give() {
        use crate::affinity::AffinityLine;
        let gift = UserAction::Gift {
            kind: ItemKind::Item,
            name: "摩托车".into(),
        };
        assert_eq!(gift.refusal_line(), Some(AffinityLine::Bond));
    }

    #[test]
    fn item_kind_round_trips_its_wire_value() {
        for k in [ItemKind::Item, ItemKind::Clothing] {
            assert_eq!(ItemKind::parse(k.as_str()), Some(k));
            assert_eq!(
                serde_json::to_value(k).unwrap(),
                serde_json::json!(k.as_str())
            );
        }
        assert_eq!(ItemKind::parse("shoes"), None);
    }

    #[test]
    fn given_item_keeps_its_wire_values() {
        assert_eq!(
            serde_json::to_value(GivenItem::Cigarette).unwrap(),
            "cigarette"
        );
        assert_eq!(serde_json::to_value(GivenItem::Alcohol).unwrap(), "alcohol");
        assert_eq!(
            serde_json::to_value(GivenItem::Medicine).unwrap(),
            "medicine"
        );
    }

    #[test]
    fn tip_action_serde_shape() {
        let tip = UserAction::Tip { amount_usd: 20.0 };
        assert_eq!(
            serde_json::to_value(&tip).unwrap(),
            serde_json::json!({"type": "tip", "amount_usd": 20.0})
        );
        let back: UserAction =
            serde_json::from_value(serde_json::json!({"type": "tip", "amount_usd": 9.99})).unwrap();
        assert_eq!(back, UserAction::Tip { amount_usd: 9.99 });
    }

    #[test]
    fn a_refused_tip_drops_no_line() {
        assert_eq!(UserAction::Tip { amount_usd: 20.0 }.refusal_line(), None);
    }

    #[test]
    fn action_response_wire_strings() {
        assert_eq!(ActionResponse::Accept.as_str(), "accept");
        assert_eq!(ActionResponse::Refuse.as_str(), "refuse");
        assert_eq!(
            serde_json::to_value(ActionResponse::Refuse).unwrap(),
            "refuse"
        );
        assert_eq!(
            serde_json::from_str::<ActionResponse>("\"accept\"").unwrap(),
            ActionResponse::Accept
        );
    }

    #[test]
    fn event_user_message_defaults_action_to_none() {
        let raw = r#"{"UserMessage":{"content":"hi","message_id":"00000000-0000-0000-0000-000000000001"}}"#;
        let ev: Event = serde_json::from_str(raw).expect("legacy body deserialises");
        match ev {
            Event::UserMessage { action, .. } => assert!(action.is_none()),
            _ => panic!("expected UserMessage"),
        }
    }
}
