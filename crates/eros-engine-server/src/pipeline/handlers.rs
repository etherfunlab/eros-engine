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

// TODO(T11): handlers are exercised once chat routes are wired in.
#![allow(dead_code)]

use async_trait::async_trait;
use uuid::Uuid;

use eros_engine_core::affinity::AffinityDeltas;
use eros_engine_core::types::{ActionPlan, DecisionInput};
use eros_engine_llm::openrouter::{ChatMessage, ChatRequest};
use eros_engine_store::chat::ChatRepo;

use crate::error::AppError;
use crate::prompt::{build_prompt, PendingGift};
use crate::state::AppState;

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

// ─── Reply ──────────────────────────────────────────────────────────

pub struct ReplyHandler<'a> {
    pub state: &'a AppState,
    pub session_id: Uuid,
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

        // Phase 2 in the gateway: memory search is stubbed. T13/T14 in OSS
        // will wire pgvector lookups via MemoryRepo::search.
        let profile_facts: Vec<String> = vec![];
        let relationship_facts: Vec<String> = vec![];

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
    /// Caller-supplied deltas — passed through to the post-process step
    /// via the ActionPlan / event channel; not consumed here.
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

        let tip_personality = input
            .persona
            .genome
            .tip_personality
            .as_deref()
            .unwrap_or("normal");

        let system_prompt = build_prompt(
            &input.persona,
            &[],
            &[],
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
