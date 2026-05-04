// SPDX-License-Identifier: AGPL-3.0-only
//! Public types for the companion engine.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::affinity::{Affinity, AffinityDeltas};
use crate::persona::CompanionPersona;

/// Events that drive the engine pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    UserMessage { content: String, message_id: Uuid },
    Gift { gift_id: Uuid, amount: i64 },
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

/// Response from the chat engine — pure text reply.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub reply: String,
}

/// Input bundle consumed by the PDE.
#[derive(Clone)]
pub struct DecisionInput {
    pub event: Event,
    pub affinity: Affinity,
    pub persona: CompanionPersona,
    pub signals: ConversationSignals,
}
