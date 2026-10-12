// SPDX-License-Identifier: AGPL-3.0-only

//! A streamed judge call (spec 2026-10-13-pde-reasoning-design.md §3): walk a
//! model chain, yield the model's reasoning deltas as they arrive, parse the
//! content once each stream ends. The PDE judge and the group director both
//! run on it; they differ in task name, parser and time limits.

use std::sync::Arc;
use std::time::Duration;

use eros_engine_llm::failure::{AttemptFailure, GatewayError, GatewayKind};
use eros_engine_llm::model_config::Sampling;
use eros_engine_llm::openrouter::{clean_response, ChatMessage, ChatRequest};
use eros_engine_llm::LlmError;
use uuid::Uuid;

use crate::pipeline::stream::PdeStatus;
use crate::state::AppState;

pub(crate) struct JudgeCall {
    pub task: &'static str,
    pub session_id: Option<Uuid>,
    /// Primary model first, then the fallbacks (already truncated to retry_depth).
    pub chain: Vec<String>,
    pub system: String,
    pub user: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub sampling: Sampling,
    pub response_format: Option<serde_json::Value>,
    /// Longest wait for a stream to open (capped by `attempt_budget`).
    pub open_cap: Duration,
    /// One attempt, from its start through the end of the read.
    pub attempt_budget: Duration,
}

pub(crate) enum JudgeStep<V> {
    Reasoning(String),
    Finished(JudgeRun<V>),
}

pub(crate) struct JudgeRun<V> {
    pub status: PdeStatus,
    pub verdict: Option<V>,
    /// The last unparseable content, on `ParseError`.
    pub raw: Option<String>,
    /// Every attempt's reasoning, concatenated.
    pub reasoning: String,
    /// The `engine.llm_generations` join key of the serving attempt.
    pub generation_id: Option<String>,
    /// Feeds the audit row's two failure columns and the turn's `final` frame.
    pub failures: Vec<AttemptFailure>,
}

fn timeout_failure(
    task: &str,
    model: &str,
    kind: GatewayKind,
    limit: Duration,
    what: &str,
) -> AttemptFailure {
    AttemptFailure::Gateway(GatewayError {
        task: task.to_string(),
        model: Some(model.to_string()),
        kind,
        message: format!("{task} {what} timeout after {}s", limit.as_secs()),
    })
}

pub(crate) fn reasoning_call<V, F>(
    state: Arc<AppState>,
    call: JudgeCall,
    parse: F,
) -> impl futures_util::Stream<Item = JudgeStep<V>> + Send + 'static
where
    V: Send + 'static,
    F: Fn(&str) -> Option<V> + Send + 'static,
{
    async_stream::stream! {
        use futures_util::StreamExt as _;
        let task = call.task;
        let mut failures: Vec<AttemptFailure> = Vec::new();
        let mut reasoning = String::new();
        let mut last = PdeStatus::GatewayError;
        let mut last_parse: Option<(String, Option<String>)> = None;
        for model_id in &call.chain {
            let req = ChatRequest {
                model: model_id.clone(),
                fallback_model: vec![],
                messages: vec![
                    ChatMessage { role: "system".into(), content: call.system.clone() },
                    ChatMessage { role: "user".into(), content: call.user.clone() },
                ],
                temperature: call.temperature,
                max_tokens: call.max_tokens,
                sampling: call.sampling,
                response_format: call.response_format.clone(),
                task: Some(task.into()),
                ..Default::default()
            };
            let deadline = tokio::time::Instant::now() + call.attempt_budget;
            let open_limit = call.open_cap.min(call.attempt_budget);
            let mut content = String::new();
            let mut usage = None;
            let mut gen_id: Option<String> = None;
            let mut served: Option<String> = None;
            let mut finish: Option<String> = None;
            let mut failed = false;
            match tokio::time::timeout(open_limit, state.openrouter.execute_stream_as(&req, model_id)).await {
                Ok(Ok(mut s)) => loop {
                    let next = tokio::time::timeout_at(deadline, s.next()).await;
                    // `timeout_at` polls the stream before the clock, so an item that
                    // is ready after a suspension past the deadline would be accepted.
                    let next = match next {
                        Ok(_) if tokio::time::Instant::now() >= deadline => Err(()),
                        Ok(item) => Ok(item),
                        Err(_) => Err(()),
                    };
                    match next {
                        Ok(Some(Ok(c))) => {
                            if let Some(r) = c.reasoning {
                                reasoning.push_str(&r);
                                yield JudgeStep::Reasoning(r);
                            }
                            if let Some(t) = c.content {
                                content.push_str(&t);
                            }
                            if c.usage.is_some() { usage = c.usage; }
                            if c.generation_id.is_some() { gen_id = c.generation_id; }
                            if c.model.is_some() { served = c.model; }
                            if c.finish_reason.is_some() { finish = c.finish_reason; }
                        }
                        Ok(Some(Err(e))) => {
                            failures.push(AttemptFailure::from_llm_error(task, model_id, &e));
                            failed = true;
                            break;
                        }
                        Ok(None) => break,
                        Err(_) => {
                            failures.push(timeout_failure(task, model_id, GatewayKind::TotalTimeout,
                                call.attempt_budget, "total"));
                            failed = true;
                            break;
                        }
                    }
                },
                Ok(Err(e)) => {
                    failures.push(AttemptFailure::from_llm_error(task, model_id, &e));
                    failed = true;
                }
                Err(_) => {
                    failures.push(timeout_failure(task, model_id, GatewayKind::OpenTimeout, open_limit, "open"));
                    failed = true;
                }
            }
            if failed {
                tracing::warn!(task, model = %model_id, error = %failures.last().map(|f| f.to_string()).unwrap_or_default(), "judge call: attempt failed; next");
                last = PdeStatus::for_operation(&failures);
                continue;
            }
            let usage_json = usage.as_ref().and_then(|u| serde_json::to_value(u).ok());
            let generation_id = super::record_generation(
                &state.pool,
                super::GenerationRecord {
                    task,
                    session_id: call.session_id,
                    generation_id: gen_id.as_deref(),
                    model: served.as_deref(),
                    usage: usage_json.as_ref(),
                },
            )
            .await;
            // The one-shot client's cleaning, in its order: undecoded
            // byte-level BPE is repaired, then fences and quotes are stripped.
            // A garble cut off at `length` is incomplete and is not salvaged:
            // the attempt fails as an exhausted one-model chain.
            let garbled = eros_engine_llm::byte_bpe::looks_byte_garbled(&content);
            if garbled && finish.as_deref() == Some("length") {
                tracing::error!(task, model = %model_id, "judge call: truncated byte-BPE garble; next");
                failures.push(AttemptFailure::from_llm_error(task, model_id,
                    &LlmError::Chain { failures: Vec::new() }));
                last = PdeStatus::for_operation(&failures);
                continue;
            }
            let content = if garbled {
                tracing::error!(task, model = %model_id, "judge call: byte-BPE garbled completion; repaired");
                eros_engine_llm::byte_bpe::repair_byte_bpe(&content)
            } else {
                content
            };
            let text = clean_response(content.trim()).trim().to_string();
            if text.is_empty() {
                tracing::warn!(task, model = %model_id, "judge call: empty reply; next");
                last = PdeStatus::Empty;
                continue;
            }
            match parse(&text) {
                Some(v) => {
                    yield JudgeStep::Finished(JudgeRun {
                        status: PdeStatus::Ok,
                        verdict: Some(v),
                        raw: None,
                        reasoning,
                        generation_id,
                        failures,
                    });
                    return;
                }
                None => {
                    tracing::warn!(task, model = %model_id, "judge call: unparseable verdict; next");
                    last = PdeStatus::ParseError;
                    last_parse = Some((text, generation_id));
                }
            }
        }
        let (raw, generation_id) = match (last, last_parse) {
            (PdeStatus::ParseError, Some((r, g))) => (Some(r), g),
            _ => (None, None),
        };
        yield JudgeStep::Finished(JudgeRun { status: last, verdict: None, raw, reasoning, generation_id, failures });
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    /// A one-shot chat completion JSON replayed as SSE with the same content:
    /// an optional reasoning frame (from `choices[0].message.reasoning`), one
    /// content frame, then a terminal frame carrying `id`, `model`, `usage`.
    pub(crate) fn sse_completion(body: serde_json::Value) -> wiremock::ResponseTemplate {
        let msg = &body["choices"][0]["message"];
        let mut frames: Vec<serde_json::Value> = Vec::new();
        if let Some(r) = msg["reasoning"].as_str() {
            frames.push(serde_json::json!({"choices":[{"delta":{"reasoning": r}}]}));
        }
        if let Some(c) = msg["content"].as_str() {
            frames.push(serde_json::json!({"choices":[{"delta":{"content": c}}]}));
        }
        let mut last = serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
        for k in ["id", "model", "usage"] {
            if !body[k].is_null() {
                last[k] = body[k].clone();
            }
        }
        frames.push(last);
        let mut sse: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
        sse.push_str("data: [DONE]\n\n");
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_raw(sse, "text/event-stream")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;
    use sqlx::PgPool;
    use wiremock::matchers::{body_partial_json, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sse(frames: &[&str]) -> String {
        let mut s: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
        s.push_str("data: [DONE]\n\n");
        s
    }

    fn call(chain: &[&str], budget: Duration) -> JudgeCall {
        JudgeCall {
            task: "pde_decision",
            session_id: None,
            chain: chain.iter().map(|s| s.to_string()).collect(),
            system: "sys".into(),
            user: "ctx".into(),
            temperature: 0.3,
            max_tokens: 400,
            sampling: Default::default(),
            response_format: Some(serde_json::json!({"type": "json_object"})),
            open_cap: budget,
            attempt_budget: budget,
        }
    }

    async fn state(pool: PgPool, server: &MockServer) -> Arc<AppState> {
        let mut st = crate::routes::companion::test_state(pool);
        st.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", server.uri()),
            ),
        );
        Arc::new(st)
    }

    fn number(s: &str) -> Option<i64> {
        s.trim().parse().ok()
    }

    fn split<V>(steps: Vec<JudgeStep<V>>) -> (Vec<String>, JudgeRun<V>) {
        let mut reasoning = Vec::new();
        let mut finished = None;
        let n = steps.len();
        for (i, s) in steps.into_iter().enumerate() {
            match s {
                JudgeStep::Reasoning(r) => {
                    assert!(finished.is_none(), "reasoning after Finished");
                    reasoning.push(r)
                }
                JudgeStep::Finished(run) => {
                    assert_eq!(i, n - 1, "Finished is the last step");
                    finished = Some(run)
                }
            }
        }
        (reasoning, finished.expect("exactly one Finished"))
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn reasoning_streams_then_the_verdict_parses(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(body_partial_json(
                serde_json::json!({"model": "a", "stream": true,
                "response_format": {"type": "json_object"}}),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        sse(&[
                            r#"{"choices":[{"delta":{"reasoning":"先想"}}]}"#,
                            r#"{"choices":[{"delta":{"reasoning":"再定"}}]}"#,
                            r#"{"choices":[{"delta":{"content":"42"}}],"id":"gen-a","model":"a"}"#,
                        ]),
                        "text/event-stream",
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;
        let st = state(pool.clone(), &server).await;
        let steps: Vec<JudgeStep<i64>> =
            reasoning_call(st, call(&["a"], Duration::from_secs(10)), number)
                .collect()
                .await;
        let (reasoning, run) = split(steps);
        assert_eq!(reasoning, vec!["先想", "再定"]);
        assert_eq!(run.status, PdeStatus::Ok);
        assert_eq!(run.verdict, Some(42));
        assert_eq!(run.reasoning, "先想再定");
        assert_eq!(run.generation_id.as_deref(), Some("gen-a"));
        let task: String = sqlx::query_scalar(
            "SELECT task FROM engine.llm_generations WHERE generation_id = 'gen-a'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(task, "pde_decision");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn reasoning_from_every_attempt_is_kept(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "a"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        sse(&[
                            r#"{"choices":[{"delta":{"reasoning":"甲"}}]}"#,
                            r#"{"choices":[{"delta":{"content":"不是数字"}}],"id":"gen-a"}"#,
                        ]),
                        "text/event-stream",
                    ),
            )
            .mount(&server)
            .await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "b"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        sse(&[
                            r#"{"choices":[{"delta":{"reasoning":"乙"}}]}"#,
                            r#"{"choices":[{"delta":{"content":"7"}}],"id":"gen-b"}"#,
                        ]),
                        "text/event-stream",
                    ),
            )
            .mount(&server)
            .await;
        let st = state(pool, &server).await;
        let steps: Vec<JudgeStep<i64>> =
            reasoning_call(st, call(&["a", "b"], Duration::from_secs(10)), number)
                .collect()
                .await;
        let (reasoning, run) = split(steps);
        assert_eq!(reasoning, vec!["甲", "乙"]);
        assert_eq!(run.verdict, Some(7));
        assert_eq!(run.reasoning, "甲乙");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_failed_primary_falls_back_and_unparseable_is_parse_error(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "a"})))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "b"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        sse(&[r#"{"choices":[{"delta":{"content":"nope"}}],"id":"gen-b"}"#]),
                        "text/event-stream",
                    ),
            )
            .mount(&server)
            .await;
        let st = state(pool, &server).await;
        let steps: Vec<JudgeStep<i64>> =
            reasoning_call(st, call(&["a", "b"], Duration::from_secs(10)), number)
                .collect()
                .await;
        let (_, run) = split(steps);
        assert_eq!(run.status, PdeStatus::ParseError);
        assert_eq!(run.raw.as_deref(), Some("nope"));
        assert_eq!(run.generation_id.as_deref(), Some("gen-b"));
        assert_eq!(run.failures.len(), 1, "the primary's 500");
    }

    fn reply_field(s: &str) -> Option<String> {
        serde_json::from_str::<serde_json::Value>(s).ok()?["reply"]
            .as_str()
            .map(str::to_string)
    }

    /// Undecoded byte-level BPE: every space and newline of a JSON verdict
    /// arrives as `Ġ` / `Ċ`, which no JSON parser accepts. Escaped for
    /// embedding in an SSE frame's `content` string.
    const GARBLED_VERDICT: &str = r#"{ĊĠĠĠ\"reply\":Ġ\"HelloĠthere.\"Ċ}"#;

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_garbled_verdict_is_repaired_before_parsing(pool: PgPool) {
        let server = MockServer::start().await;
        let frame = format!(
            r#"{{"choices":[{{"delta":{{"content":"{GARBLED_VERDICT}"}}}}],"id":"gen-g"}}"#
        );
        Mock::given(path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "a"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse(&[&frame]), "text/event-stream"),
            )
            .mount(&server)
            .await;
        let st = state(pool, &server).await;
        let steps: Vec<JudgeStep<String>> =
            reasoning_call(st, call(&["a"], Duration::from_secs(10)), reply_field)
                .collect()
                .await;
        let (_, run) = split(steps);
        assert_eq!(run.status, PdeStatus::Ok);
        assert_eq!(run.verdict.as_deref(), Some("Hello there."));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_truncated_garble_is_not_salvaged(pool: PgPool) {
        let server = MockServer::start().await;
        let frame = format!(
            r#"{{"choices":[{{"delta":{{"content":"{GARBLED_VERDICT}"}},"finish_reason":"length"}}],"id":"gen-a"}}"#
        );
        Mock::given(path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "a"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse(&[&frame]), "text/event-stream"),
            )
            .mount(&server)
            .await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "b"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        sse(&[r#"{"choices":[{"delta":{"content":"{\"reply\":\"b\"}"}}],"id":"gen-b"}"#]),
                        "text/event-stream",
                    ),
            )
            .mount(&server)
            .await;
        let st = state(pool, &server).await;
        let steps: Vec<JudgeStep<String>> =
            reasoning_call(st, call(&["a", "b"], Duration::from_secs(10)), reply_field)
                .collect()
                .await;
        let (_, run) = split(steps);
        assert_eq!(run.verdict.as_deref(), Some("b"));
        assert_eq!(
            run.failures.len(),
            1,
            "the truncated garble fails its attempt"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn unparseable_content_keeps_its_cleaned_text_as_raw(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "a"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        sse(&[r#"{"choices":[{"delta":{"content":"```text\n不是数字\n```"}}],"id":"gen-a"}"#]),
                        "text/event-stream",
                    ),
            )
            .mount(&server)
            .await;
        let st = state(pool, &server).await;
        let steps: Vec<JudgeStep<i64>> =
            reasoning_call(st, call(&["a"], Duration::from_secs(10)), number)
                .collect()
                .await;
        let (_, run) = split(steps);
        assert_eq!(run.status, PdeStatus::ParseError);
        assert_eq!(
            run.raw.as_deref(),
            Some(eros_engine_llm::openrouter::clean_response("```text\n不是数字\n```").as_str())
        );
        assert_eq!(run.raw.as_deref(), Some("不是数字"));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_attempt_budget_bounds_opening_and_the_chain_moves_on(pool: PgPool) {
        let server = MockServer::start().await;
        // The primary answers only after the budget; wiremock delays the
        // whole response, so this is the open half of the budget.
        Mock::given(path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "slow"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        sse(&[r#"{"choices":[{"delta":{"content":"1"}}]}"#]),
                        "text/event-stream",
                    )
                    .set_delay(Duration::from_secs(5)),
            )
            .mount(&server)
            .await;
        Mock::given(path("/api/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({"model": "fast"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        sse(&[r#"{"choices":[{"delta":{"content":"2"}}]}"#]),
                        "text/event-stream",
                    ),
            )
            .mount(&server)
            .await;
        let st = state(pool, &server).await;
        let mut c = call(&["slow", "fast"], Duration::from_millis(300));
        c.open_cap = Duration::from_secs(60); // the budget is the binding limit
        let steps: Vec<JudgeStep<i64>> = reasoning_call(st, c, number).collect().await;
        let (_, run) = split(steps);
        assert_eq!(run.verdict, Some(2));
        assert_eq!(run.failures.len(), 1);
        match &run.failures[0] {
            AttemptFailure::Gateway(g) => {
                assert_eq!(g.kind, GatewayKind::OpenTimeout);
                assert_eq!(g.message, "pde_decision open timeout after 0s");
            }
            other => panic!("expected a gateway timeout, got {other:?}"),
        }
    }
}
