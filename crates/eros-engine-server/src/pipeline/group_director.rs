// SPDX-License-Identifier: AGPL-3.0-only

//! The group director: one streamed call per group turn that casts who speaks
//! (spec 2026-10-12-group-chat-design.md §5.3–§5.4). Reasoning deltas are
//! yielded as they arrive; the content is parsed once the stream ends.

use std::sync::Arc;

use eros_engine_core::affinity::{BondLabel, ChemistryLabel};
use eros_engine_core::group::{handle, parse_verdict, DirectorVerdict};
use eros_engine_llm::model_config::ResolvedGroupDirector;
use uuid::Uuid;

use crate::pipeline::reasoning_call::{reasoning_call, JudgeCall, JudgeRun, JudgeStep};
use crate::pipeline::stream::{STREAM_OPEN_TIMEOUT, STREAM_TOTAL_TIMEOUT};
use crate::state::AppState;

pub(crate) const DIRECTOR_TASK: &str = "group_director";

pub(crate) struct DirectorSeat<'a> {
    pub name: &'a str,
    pub personality: Option<&'a str>,
    pub bond: BondLabel,
    pub chemistry: ChemistryLabel,
    pub rung: u8,
}

/// The engine-owned user message (spec §5.3). Byte layout pinned by test.
pub(crate) fn director_context(
    title: Option<&str>,
    seats: &[DirectorSeat<'_>],
    image_available: bool,
    transcript: &str,
    mention_names: &[String],
    latest: &str,
) -> String {
    let mut s = String::new();
    if let Some(t) = title {
        s.push_str(&format!("[群聊] {t}\n"));
    }
    s.push_str("[成员]\n");
    for (i, seat) in seats.iter().enumerate() {
        let who = match seat.personality.filter(|p| !p.trim().is_empty()) {
            Some(p) => format!("{}（{}）", seat.name, p.trim()),
            None => format!("{} ", seat.name),
        };
        s.push_str(&format!(
            "- {}：{}bond={} chemistry={} 亲密度=第 {} 档\n",
            handle(i),
            who,
            seat.bond.as_key(),
            seat.chemistry.as_key(),
            seat.rung
        ));
    }
    s.push_str(&format!(
        "[图片能力] 本轮可发图={}\n",
        if image_available { "是" } else { "否" }
    ));
    s.push_str("[最近群聊]\n");
    s.push_str(if transcript.trim().is_empty() {
        "（无）"
    } else {
        transcript
    });
    s.push('\n');
    if !mention_names.is_empty() {
        s.push_str(&format!("[用户点名] {}\n", mention_names.join("、")));
    }
    s.push_str(&format!("\n[用户最新消息]\n{latest}"));
    s
}

/// Strict json_schema for the verdict; `member` is an enum over the handles
/// actually seated this turn.
pub(crate) fn director_response_format(seat_count: usize) -> serde_json::Value {
    let handles: Vec<String> = (0..seat_count).map(handle).collect();
    serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "group_director",
            "strict": true,
            "schema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["speakers", "reason"],
                "properties": {
                    "speakers": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["member", "action", "inner_state", "reply_mode", "clothing"],
                            "properties": {
                                "member": { "type": "string", "enum": handles },
                                "action": { "type": "string",
                                    "enum": ["reply_text", "reply_image", "reply_text_image"] },
                                "inner_state": { "type": ["string", "null"] },
                                "reply_mode": { "type": ["string", "null"],
                                    "enum": ["listen", "follow", "ask", "judge", null] },
                                "clothing": { "type": ["string", "null"] }
                            }
                        }
                    },
                    "reason": { "type": ["string", "null"] }
                }
            }
        }
    })
}

/// A director run is a judge-call run over the director's verdict type.
pub(crate) type DirectorRun = JudgeRun<DirectorVerdict>;

pub(crate) fn run_director(
    state: Arc<AppState>,
    group_session_id: Uuid,
    p: ResolvedGroupDirector,
    ctx: String,
    seat_count: usize,
) -> impl futures_util::Stream<Item = JudgeStep<DirectorVerdict>> + Send + 'static {
    let chain: Vec<String> = std::iter::once(p.model.clone())
        .chain(p.fallback_model.iter().cloned())
        .collect();
    let call = JudgeCall {
        task: DIRECTOR_TASK,
        session_id: Some(group_session_id),
        chain,
        system: p.director_prompt.clone(),
        user: ctx,
        temperature: p.temperature as f32,
        max_tokens: p.max_tokens,
        sampling: p.sampling,
        response_format: p
            .structured_output
            .then(|| director_response_format(seat_count)),
        open_cap: STREAM_OPEN_TIMEOUT,
        attempt_budget: STREAM_OPEN_TIMEOUT + STREAM_TOTAL_TIMEOUT,
    };
    reasoning_call(state, call, parse_verdict)
}

/// `companion_decision_events.payload` for a director run: the verdict with a
/// `reasoning` key, or `{raw, reasoning}` when nothing parsed.
pub(crate) fn director_audit_payload(run: &DirectorRun) -> Option<serde_json::Value> {
    let reasoning = if run.reasoning.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(run.reasoning.clone())
    };
    match (&run.verdict, &run.raw) {
        (Some(v), _) => {
            let mut p = serde_json::to_value(v).ok()?;
            p["reasoning"] = reasoning;
            Some(p)
        }
        (None, Some(raw)) => Some(serde_json::json!({ "raw": raw, "reasoning": reasoning })),
        (None, None) if !run.reasoning.is_empty() => {
            Some(serde_json::json!({ "reasoning": reasoning }))
        }
        (None, None) => None,
    }
}

pub(crate) async fn record_director_event(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    group_session_id: Uuid,
    user_message_id: Uuid,
    status: &str,
    run: &DirectorRun,
    inputs: serde_json::Value,
) {
    let (llm_attempts, gateway_errors) = crate::pipeline::stream::split_failures(&run.failures);
    let repo = eros_engine_store::decision::DecisionEventRepo { pool };
    if let Err(e) = repo
        .record(eros_engine_store::decision::DecisionEventInsert {
            run_id: Uuid::new_v4(),
            user_id,
            session_id: Some(group_session_id),
            message_id: Some(user_message_id),
            status,
            action: None,
            proposed_action: None,
            payload: director_audit_payload(run),
            inputs: Some(inputs),
            generation_id: run.generation_id.as_deref(),
            llm_attempts,
            gateway_errors,
        })
        .await
    {
        tracing::warn!("group director: decision-event audit write failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::stream::PdeStatus;
    use eros_engine_core::affinity::{BondLabel, ChemistryLabel};
    use futures_util::StreamExt as _;
    use sqlx::PgPool;
    use wiremock::matchers::{body_partial_json, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn director_context_is_byte_exact() {
        let seats = [
            DirectorSeat {
                name: "小美",
                personality: Some("活泼"),
                bond: BondLabel::Friend,
                chemistry: ChemistryLabel::Crush,
                rung: 2,
            },
            DirectorSeat {
                name: "阿哲",
                personality: None,
                bond: BondLabel::Acquaintance,
                chemistry: ChemistryLabel::Spark,
                rung: 1,
            },
        ];
        let got = director_context(
            Some("周末爬山"),
            &seats,
            true,
            "用户：早\n小美：早呀",
            &["阿哲".into()],
            "去哪爬？",
        );
        let want = "[群聊] 周末爬山\n\
[成员]\n\
- m1：小美（活泼）bond=friend chemistry=crush 亲密度=第 2 档\n\
- m2：阿哲 bond=acquaintance chemistry=spark 亲密度=第 1 档\n\
[图片能力] 本轮可发图=是\n\
[最近群聊]\n\
用户：早\n小美：早呀\n\
[用户点名] 阿哲\n\
\n\
[用户最新消息]\n\
去哪爬？";
        assert_eq!(got, want);
    }

    #[test]
    fn director_context_omits_optional_sections() {
        let seats = [DirectorSeat {
            name: "小美",
            personality: None,
            bond: BondLabel::Friend,
            chemistry: ChemistryLabel::Spark,
            rung: 1,
        }];
        let got = director_context(None, &seats, false, "", &[], "嗨");
        assert!(!got.contains("[群聊]"));
        assert!(!got.contains("[用户点名]"));
        assert!(got.contains("[图片能力] 本轮可发图=否\n"));
        assert!(got.contains("[最近群聊]\n（无）\n"));
    }

    #[test]
    fn response_format_enumerates_the_seated_handles() {
        let f = director_response_format(2);
        let items = &f["json_schema"]["schema"]["properties"]["speakers"]["items"];
        assert_eq!(
            items["properties"]["member"]["enum"],
            serde_json::json!(["m1", "m2"])
        );
    }

    fn sse(frames: &[&str]) -> String {
        let mut s: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
        s.push_str("data: [DONE]\n\n");
        s
    }

    fn resolved(fallback: Vec<String>) -> eros_engine_llm::model_config::ResolvedGroupDirector {
        eros_engine_llm::model_config::ResolvedGroupDirector {
            model: "dir-a".into(),
            fallback_model: fallback,
            temperature: 0.5,
            max_tokens: 4000,
            director_prompt: "direct".into(),
            structured_output: true,
            sampling: Default::default(),
        }
    }

    async fn state_with(pool: PgPool, server: &MockServer) -> std::sync::Arc<AppState> {
        let mut state = crate::routes::companion::test_state(pool);
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", server.uri()),
            ),
        );
        std::sync::Arc::new(state)
    }

    async fn group(pool: &PgPool) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, channel) VALUES ($1, 'group') RETURNING id",
        )
        .bind(Uuid::new_v4())
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn reasoning_streams_then_the_verdict_finishes(pool: PgPool) {
        let server = MockServer::start().await;
        let verdict = r#"{\"speakers\":[{\"member\":\"m1\",\"action\":\"reply_text\",\"inner_state\":null,\"reply_mode\":null,\"clothing\":null}],\"reason\":null}"#;
        Mock::given(path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "dir-a", "stream": true,
                "response_format": {"type": "json_schema"}})))
            .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream")
                .set_body_raw(sse(&[
                    r#"{"choices":[{"delta":{"reasoning":"小美最熟，"}}]}"#,
                    r#"{"choices":[{"delta":{"reasoning":"让她先说"}}]}"#,
                    &format!(r#"{{"choices":[{{"delta":{{"content":"{verdict}"}}}}],"id":"gen-dir","model":"dir-a","usage":{{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}}}"#),
                ]), "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
        let g = group(&pool).await;
        let state = state_with(pool.clone(), &server).await;
        let steps: Vec<JudgeStep<DirectorVerdict>> =
            run_director(state, g, resolved(vec![]), "ctx".into(), 1)
                .collect()
                .await;
        let reasoning: Vec<&str> = steps
            .iter()
            .filter_map(|s| match s {
                JudgeStep::Reasoning(r) => Some(r.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, vec!["小美最熟，", "让她先说"]);
        let JudgeStep::Finished(run) = steps.last().unwrap() else {
            panic!("finished last")
        };
        assert_eq!(run.status, PdeStatus::Ok);
        assert_eq!(run.verdict.as_ref().unwrap().speakers[0].member, "m1");
        assert_eq!(run.reasoning, "小美最熟，让她先说");
        assert_eq!(run.generation_id.as_deref(), Some("gen-dir"));
        let task: String = sqlx::query_scalar(
            "SELECT task FROM engine.llm_generations WHERE generation_id = 'gen-dir'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(task, "group_director");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_failed_primary_falls_back_and_unparseable_is_parse_error(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "dir-a"})))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "dir-b"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        sse(&[r#"{"choices":[{"delta":{"content":"m1 先说"}}],"id":"gen-b"}"#]),
                        "text/event-stream",
                    ),
            )
            .mount(&server)
            .await;
        let g = group(&pool).await;
        let state = state_with(pool.clone(), &server).await;
        let steps: Vec<JudgeStep<DirectorVerdict>> =
            run_director(state, g, resolved(vec!["dir-b".into()]), "ctx".into(), 1)
                .collect()
                .await;
        let JudgeStep::Finished(run) = steps.last().unwrap() else {
            panic!()
        };
        assert_eq!(run.status, PdeStatus::ParseError);
        assert_eq!(run.raw.as_deref(), Some("m1 先说"));
        assert_eq!(run.failures.len(), 1, "the primary's 500");
        let payload = director_audit_payload(run).unwrap();
        assert_eq!(payload["raw"], "m1 先说");
    }
}
