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
}
