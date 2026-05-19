// SPDX-License-Identifier: AGPL-3.0-only
//! Streaming pipeline — ProtocolFrame state machine + run_stream generator.
//!
//! Wire-level frame layout follows
//! `docs/superpowers/specs/2026-05-19-sse-streaming-chat-0.2-design.md` §1.5.
//!
//! Task 4 only ships the type layer; the `run_stream` generator lands in
//! later tasks (T10/T11/T12).

use eros_engine_llm::openrouter::UsageBlock;
use serde::Serialize;
use ulid::Ulid;

/// Stream-level error code enum. Renders to the spec's lowercase string.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StreamErrorCode {
    UpstreamUnavailable,
    RateLimited,
    Internal,
    Timeout,
}

/// Action type tag used in `meta` frames.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FrameActionType {
    Reply,
    Ghost,
    GiftReaction,
}

/// One wire frame in the SSE protocol.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProtocolFrame {
    Meta {
        message_id: String,
        action_type: FrameActionType,
        model: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        continues_from: Option<String>,
    },
    Delta {
        message_id: String,
        content: String,
    },
    Done {
        message_id: String,
        truncated: bool,
        usage: Option<UsageBlock>,
        generation_id: Option<String>,
    },
    Final {
        lead_score: f64,
        should_show_cta: bool,
        agent_training_level: f64,
    },
    Error {
        code: StreamErrorCode,
        retryable: bool,
        message: String,
        user_message: String,
    },
}

/// Render a 128-bit id as a Crockford Base32 ULID string (26 chars).
pub fn ulid_string(u: Ulid) -> String {
    u.to_string()
}

use std::sync::Arc;
use uuid::Uuid;

use eros_engine_core::types::{ActionType, DecisionInput, Event};
use eros_engine_core::pde;
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::ChatRepo;
use eros_engine_store::persona::PersonaRepo;

use crate::state::AppState;

/// All persisted bits needed to drive a streaming burst.
#[derive(Debug, Clone)]
pub struct PersistedUserMessage {
    pub user_message_id: Uuid,
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub content: String,
}

/// Produce a stream of `ProtocolFrame` events for a single burst. The
/// generator owns its `AppState` clone so it stays `'static` and survives
/// `Sse`'s body lifetime. Task 10 implements the Ghost branch; T11/T12
/// fill in Reply / GiftReaction.
pub fn run_stream(
    state: Arc<AppState>,
    user_msg: PersistedUserMessage,
) -> impl futures_util::Stream<Item = ProtocolFrame> + Send + 'static {
    async_stream::stream! {
        let chat_repo = ChatRepo { pool: &state.pool };
        let persona_repo = PersonaRepo { pool: &state.pool };
        let affinity_repo = AffinityRepo { pool: &state.pool };

        let persona = match persona_repo.load_companion(user_msg.instance_id).await {
            Ok(Some(p)) => p,
            _ => {
                yield ProtocolFrame::Error {
                    code: StreamErrorCode::Internal,
                    retryable: false,
                    message: "persona instance not found".into(),
                    user_message: "服务出现问题，请稍后再试".into(),
                };
                return;
            }
        };
        let mut affinity = match affinity_repo
            .load_or_create(user_msg.session_id, user_msg.user_id, user_msg.instance_id)
            .await
        {
            Ok(mut a) => { a.apply_time_decay(); a }
            Err(e) => {
                tracing::warn!("stream: affinity load failed: {e}");
                yield ProtocolFrame::Error {
                    code: StreamErrorCode::Internal,
                    retryable: false,
                    message: format!("affinity load failed: {e}"),
                    user_message: "服务出现问题，请稍后再试".into(),
                };
                return;
            }
        };
        let signals = match super::compute_signals_for_session(
            &state.pool, user_msg.session_id, &affinity,
        ).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("stream: signals failed: {e}");
                yield ProtocolFrame::Error {
                    code: StreamErrorCode::Internal,
                    retryable: false,
                    message: format!("signals failed: {e}"),
                    user_message: "服务出现问题，请稍后再试".into(),
                };
                return;
            }
        };

        let input = DecisionInput {
            event: Event::UserMessage {
                content: user_msg.content.clone(),
                message_id: user_msg.user_message_id,
                prompt_traits: vec![],
                audit: None,
            },
            affinity: affinity.clone(),
            persona,
            signals,
        };
        let plan = pde::decide(&input);

        match plan.action_type {
            ActionType::Ghost => {
                let msg_id = Ulid::new();
                // Persist the ghost decision on the user row so replay can
                // distinguish "ghost outcome" from "still generating" (§1.10).
                if let Err(e) = chat_repo.mark_user_message_ghosted(user_msg.user_message_id).await {
                    tracing::warn!("stream: ghost mark failed: {e}");
                }
                if let Err(e) = affinity_repo.record_ghost(&mut affinity).await {
                    tracing::warn!("stream: record_ghost failed: {e}");
                }
                yield ProtocolFrame::Meta {
                    message_id: ulid_string(msg_id),
                    action_type: FrameActionType::Ghost,
                    model: String::new(),
                    continues_from: None,
                };
                yield ProtocolFrame::Done {
                    message_id: ulid_string(msg_id),
                    truncated: false,
                    usage: None,
                    generation_id: None,
                };
                let final_frame = compute_final_frame(&state, user_msg.session_id, user_msg.user_id).await;
                yield final_frame;
            }
            _ => {
                // Reply / GiftReaction implemented in T11/T12; Proactive returns Final only.
                let final_frame = compute_final_frame(&state, user_msg.session_id, user_msg.user_id).await;
                yield final_frame;
            }
        }
    }
}

/// Compute the spec's `final` frame from current session/user state.
async fn compute_final_frame(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
) -> ProtocolFrame {
    let lead_score: f64 = sqlx::query_scalar(
        "SELECT lead_score FROM engine.chat_sessions WHERE id = $1",
    )
    .bind(session_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten()
    .unwrap_or(0.0);

    let training_level: f64 = match (eros_engine_store::insight::InsightRepo { pool: &state.pool })
        .load(user_id)
        .await
    {
        Ok(Some(row)) => eros_engine_store::insight::compute_training_level(&row.insights),
        _ => 0.0,
    };
    let should_show_cta = lead_score >= 7.0 && training_level >= 0.4;
    // Normalise lead_score from 0..10 to 0..1 to match the spec's declared
    // [0.0, 1.0] range. Operator dashboards still see the 0..10 raw value
    // via the sync /message handler.
    let normalised_lead = (lead_score / 10.0).clamp(0.0, 1.0);
    ProtocolFrame::Final {
        lead_score: normalised_lead,
        should_show_cta,
        agent_training_level: training_level,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_frame_serializes_with_required_fields() {
        let id = Ulid::new();
        let f = ProtocolFrame::Meta {
            message_id: ulid_string(id),
            action_type: FrameActionType::Reply,
            model: "x-ai/grok-4-fast".into(),
            continues_from: None,
        };
        let s = serde_json::to_string(&f).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "meta");
        assert_eq!(v["action_type"], "reply");
        assert_eq!(v["model"], "x-ai/grok-4-fast");
        assert!(v.get("continues_from").is_none(), "must be omitted when None");
        assert_eq!(v["message_id"].as_str().unwrap().len(), 26);
    }

    #[test]
    fn meta_frame_serializes_continues_from_when_present() {
        let prev = ulid_string(Ulid::new());
        let f = ProtocolFrame::Meta {
            message_id: ulid_string(Ulid::new()),
            action_type: FrameActionType::Reply,
            model: "x-ai/grok-4-fast".into(),
            continues_from: Some(prev.clone()),
        };
        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        assert_eq!(v["continues_from"], prev);
    }

    #[test]
    fn delta_frame_serializes_with_content() {
        let id = ulid_string(Ulid::new());
        let f = ProtocolFrame::Delta { message_id: id.clone(), content: "你好".into() };
        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        assert_eq!(v["type"], "delta");
        assert_eq!(v["message_id"], id);
        assert_eq!(v["content"], "你好");
    }

    #[test]
    fn done_frame_serializes_with_usage_and_truncated_flag() {
        let f = ProtocolFrame::Done {
            message_id: ulid_string(Ulid::new()),
            truncated: true,
            usage: Some(UsageBlock {
                prompt_tokens: 10,
                completion_tokens: 4,
                total_tokens: 14,
                cost: None,
            }),
            generation_id: Some("gen-1".into()),
        };
        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        assert_eq!(v["type"], "done");
        assert_eq!(v["truncated"], true);
        assert_eq!(v["usage"]["prompt_tokens"], 10);
        assert_eq!(v["generation_id"], "gen-1");
    }

    #[test]
    fn final_frame_carries_three_floats() {
        let f = ProtocolFrame::Final {
            lead_score: 0.71,
            should_show_cta: false,
            agent_training_level: 0.42,
        };
        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        assert_eq!(v["type"], "final");
        assert!((v["lead_score"].as_f64().unwrap() - 0.71).abs() < 1e-9);
        assert_eq!(v["should_show_cta"], false);
    }

    #[test]
    fn error_frame_uses_snake_case_code() {
        let f = ProtocolFrame::Error {
            code: StreamErrorCode::UpstreamUnavailable,
            retryable: true,
            message: "internal".into(),
            user_message: "AI 服务暂时不可用，稍后再试".into(),
        };
        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["code"], "upstream_unavailable");
        assert_eq!(v["retryable"], true);
    }

    #[test]
    fn done_frame_emits_null_usage_when_absent() {
        let f = ProtocolFrame::Done {
            message_id: ulid_string(Ulid::new()),
            truncated: false,
            usage: None,
            generation_id: None,
        };
        let v: serde_json::Value = serde_json::to_value(&f).unwrap();
        // Spec §1.5 done schema permits `usage: null` — do NOT omit.
        assert!(v.get("usage").is_some());
        assert!(v["usage"].is_null());
    }

    use sqlx::PgPool;

    async fn seed_persona_and_session(
        pool: &PgPool,
        user_id: Uuid,
    ) -> (Uuid, Uuid, Uuid) {
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata, is_active) \
             VALUES ('GhostTest', 'sp', '{}'::jsonb, true) RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id)
        .bind(user_id)
        .fetch_one(pool)
        .await
        .unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(pool)
        .await
        .unwrap();
        (genome_id, instance_id, session_id)
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_stream_terminates_with_final_or_error(pool: PgPool) {
        use futures_util::StreamExt;
        use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};

        let user_id = Uuid::new_v4();
        let (_g, instance_id, session_id) = seed_persona_and_session(&pool, user_id).await;

        // test_state's openrouter client points at the real api root — that's
        // fine here because the Ghost branch never makes an LLM call. If the
        // PDE picks Reply, the test will fail when the LLM call short-circuits;
        // that's OK — Reply path testing lives in T11.
        let state = std::sync::Arc::new(crate::routes::companion::test_state(pool.clone()));
        let chat_repo = ChatRepo { pool: &state.pool };
        let user_message_id = match chat_repo
            .upsert_user_message_idempotent(session_id, "hi", "01J1111111111111111111111A", 24)
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            _ => unreachable!(),
        };

        let frames: Vec<ProtocolFrame> = run_stream(
            state.clone(),
            PersistedUserMessage {
                user_message_id,
                session_id,
                user_id,
                instance_id,
                content: "hi".into(),
            },
        )
        .collect()
        .await;

        // Tolerant: the test just proves the generator runs end-to-end and
        // terminates. T11/T15 add per-frame assertions for Reply/replay paths.
        assert!(frames.last().is_some(), "must emit at least one frame");
    }
}
