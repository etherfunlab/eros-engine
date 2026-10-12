# PDE judge reasoning — Design

- **Date:** 2026-10-13
- **Status:** Draft — ready for review
- **Type:** Engine change. No migration.
  - **HTTP:** the 1:1 SSE stream gains one optional frame. The group stream's
    `director_reasoning` frame is renamed `reasoning`; group chat has not been
    released, so no client depends on the old name.
  - **Config:** `[tasks.pde_decision]` gains an optional `timeout_secs`.
  - **Audit:** `companion_decision_events.payload` gains a `reasoning` key.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — the PDE judge call in the 1:1 text turn, and the
  group director call, which now share one streamed-call helper.
- **Builds on:** `2026-10-12-group-chat-design.md`. Its director call is the
  implementation this design reuses.

## 1. Motivation

The group director reasons before it decides, and its reasoning streams to
the client. The 1:1 PDE judge cannot: it is a one-shot call bounded at 15
seconds per attempt, it discards any reasoning text the provider returns, and
the 1:1 stream has no frame to carry one. A deployment that turns on
reasoning for the judge gains nothing it can read or show.

With this change, the judge streams through the same call the director uses:
- its reasoning reaches the client as `reasoning` frames, ahead of the reply;
- the reasoning is kept on the decision audit row;
- the per-attempt time limit becomes configurable, so a reasoning model has
  room to finish.

## 2. Principles applied

1. **One implementation.** The director's streamed chain walk becomes a
   generic helper. The director and the judge differ only in their task name,
   their parser and their time limits.
2. **Reasoning follows the model.** Whatever reasoning the judge's model
   returns is forwarded; a body rule can turn it on, and
   `reasoning = { enabled = false }` or `{ exclude = true }` turns it off.
   When the model returns none, the 1:1 stream carries no new frame. The
   default time limit equals today's `FILTER_TIMEOUT`.
3. **The time limit is a safety net, not a cap on capability.** A judge that
   runs out of time fails open to the rule engine, as it does today, and the
   user still gets a reply.

## 3. The shared call — `pipeline::reasoning_call`

```rust
pub(crate) struct JudgeCall {
    pub task: &'static str,          // "pde_decision" | "group_director"
    pub session_id: Option<Uuid>,    // for llm_generations
    pub chain: Vec<String>,          // primary + fallbacks, truncated to retry_depth
    pub system: String,
    pub user: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub sampling: Sampling,
    pub response_format: Option<serde_json::Value>,
    pub open_cap: Duration,          // wait for the stream to open
    pub attempt_budget: Duration,    // whole attempt, measured from its start
}
pub(crate) enum JudgeStep<V> { Reasoning(String), Finished(JudgeRun<V>) }
pub(crate) struct JudgeRun<V> {
    pub status: PdeStatus,
    pub verdict: Option<V>,
    pub raw: Option<String>,         // last unparseable content, on ParseError
    pub reasoning: String,           // every attempt's reasoning, concatenated
    pub generation_id: Option<String>,
    pub failures: Vec<AttemptFailure>,
}
pub(crate) fn reasoning_call<V, F>(state: Arc<AppState>, call: JudgeCall, parse: F)
    -> impl Stream<Item = JudgeStep<V>> + Send + 'static
where V: Send + 'static, F: Fn(&str) -> Option<V> + Send + 'static;
```

Behavior per attempt, walking the chain in order:

- **Opening.** `execute_stream_as` is given `min(open_cap, attempt_budget)`.
  If it does not open in time, the attempt records `GatewayKind::OpenTimeout`.
  An open error records `AttemptFailure::from_llm_error`.
- **Reading.** One deadline, `attempt start + attempt_budget`, covers the
  read loop. Running past it records `GatewayKind::TotalTimeout`. A chunk
  error records `from_llm_error`. Either way the call moves on to the next
  model.
- **Reasoning.** Each `DeltaChunk.reasoning` is yielded as
  `JudgeStep::Reasoning` as it arrives, and appended to `JudgeRun.reasoning`.
- **Completion.** A completed stream records one `llm_generations` row
  (`record_generation`, task = `call.task`, `session_id = call.session_id`).
  The id that call returns is the generation id from then on.
- **Content.**
  - The accumulated content is repaired when it is byte-BPE garbled. A garbled
    attempt that ended truncated (`finish_reason` = `length`) is incomplete,
    is not repaired, and moves to the next model.
  - The content then passes through the response cleaner. Everything after
    this point (the empty check, `parse`, `raw`) sees the cleaned text.
  - Cleaned content that is empty sets status `Empty` and moves to the next
    model.
  - Otherwise `parse` runs. A `Some` result finishes with `Ok`.
  - A `None` result sets status `ParseError`, keeps the cleaned text and its
    generation id as `raw`, and moves to the next model.
  - On a multi-model chain, the first model whose content parses after repair
    wins. The chain does not try a fallback first to prefer a clean reply.
- **Chain exhausted.** The status becomes `ParseError` (with its `raw` and
  generation id) when the last content failure was a parse error. Otherwise it
  is `PdeStatus::for_operation(&failures)`.
- **Finishing.** Every path ends by yielding exactly one
  `JudgeStep::Finished`.

`response_format` rides the stream request. The streaming path has forwarded
`ChatRequest.response_format` since group chat landed.

### 3.1 Callers

| | Group director | PDE judge |
|---|---|---|
| `task` | `group_director` | `pde_decision` |
| `parse` | `core::group::parse_verdict` | `parse_pde_verdict` |
| `open_cap` | `STREAM_OPEN_TIMEOUT` (20s) | `timeout_secs` |
| `attempt_budget` | `STREAM_OPEN_TIMEOUT + STREAM_TOTAL_TIMEOUT` (140s) | `timeout_secs` (default 15s) |
| `response_format` | `director_response_format(seats)` when structured | `pde_response_format()` when structured |
| `session_id` | the group session | the 1:1 session |

- **Director.** It keeps its context builder, schema and audit writer in
  `group_director.rs`. `DirectorRun` becomes `JudgeRun<DirectorVerdict>`.
  Its read limit is now measured from the start of the attempt (140s total)
  instead of from the moment the stream opens (120s after opening). The two
  are equal when opening takes the full 20s.
- **Judge.** `run_pde_decision` becomes a thin wrapper that builds the
  `JudgeCall` and returns the stream. `PdeDecisionRun` becomes
  `JudgeRun<PdeVerdict>`. Its per-attempt limit keeps `FILTER_TIMEOUT`'s
  meaning: one deadline from opening the call to finishing reading it.

## 4. The 1:1 turn

In `run_stream_with`, the judge block consumes the stream:

- `JudgeStep::Reasoning(c)` yields `ProtocolFrame::Reasoning { content: c }`.
- `JudgeStep::Finished(run)` feeds the unchanged logic that follows:
  `guard_action`, `plan_for`, the ghosting kill-switch, the forced image, the
  dice, and the decision audit.

These parts are unchanged:
- the read receipt (`mark_user_message_consumed`) spawned before the call;
- the rule-engine fallback on any non-`Ok` status;
- the no-judge path. With `[tasks.pde_decision]` unconfigured, the turn emits
  no `reasoning` frame.

The async send route runs the same pipeline without a client tap, so its
reasoning frames are discarded, as all of its frames are.

## 5. SSE frame

`ProtocolFrame::DirectorReasoning` is renamed `ProtocolFrame::Reasoning`:

```json
{"type":"reasoning","content":"…"}
```

- **1:1 stream.** Zero or more `reasoning` frames come first, ahead of
  `action_response`, `body_state` and `meta`. They appear whenever the judge's
  model returns reasoning; a body rule can turn reasoning on, and
  `reasoning = { enabled = false }` or `{ exclude = true }` turns it off.
  When the judge returns none, the 1:1 frame sequence is unchanged.
- **Group stream.** The frame keeps its place before `cast` and carries the
  director's reasoning.
- **Display.** The content is display-only and decides nothing. How it is
  shown — in full, collapsed, or as a "thinking" indicator — is the client's
  decision.
- **Fallback.** When the call falls back to another model, that model's
  reasoning keeps appending. No reset frame is sent.

## 6. Audit

`companion_decision_events.payload` for a judge run:

| Outcome | Payload |
|---|---|
| Parsed verdict | The existing verdict object plus `"reasoning"`: the text, or `null` when none was returned |
| Parse error, no reasoning | The raw content string, unchanged |
| Parse error, with reasoning | `{"raw": "…", "reasoning": "…"}` |
| No content (failures only), with reasoning | `{"reasoning": "…"}` |
| No content, no reasoning | NULL, unchanged |

The director's rows differ in one case: a parse error with no reasoning is `{"raw": "…", "reasoning": null}`, where the judge writes the bare raw string.

## 7. Configuration

```toml
[tasks.pde_decision]
model       = "anthropic/claude-haiku-4.5"
max_tokens  = 400          # raise it when reasoning is on; reasoning tokens count
#timeout_secs = 15         # per-attempt limit, open through finish
#filter_prompt = """…"""

#[[providers.openrouter.body]]
#tasks  = ["pde_decision"]
#params = { reasoning = { effort = "low" } }
```

- **`timeout_secs`.** Optional, default 15. It is read only on
  `[tasks.pde_decision]` and ignored elsewhere, like `ghosting`.
  - `ResolvedPde` gains `timeout: Duration`.
  - A value of `0` refuses boot: the judge would time out on every turn and
    always fall back.
- **Enabling reasoning.** Reasoning is turned on per deployment by a body rule
  on `pde_decision`. The example ships it commented out, as it ships the judge
  itself. A deployment that enables it raises `max_tokens` and `timeout_secs`
  together.

## 8. Latency

- **First token.** With reasoning on, a 1:1 turn's first `meta`/`delta` waits
  for the judge to finish thinking. The `reasoning` frames give the client
  something to show meanwhile.
- **Bound.** `timeout_secs` bounds each attempt, and `retry_depth` (default
  1) bounds the number of attempts.
- **Failure.** On failure the turn falls back to the rule engine and replies
  normally.

## 9. Where it lives

| File | Change |
|---|---|
| `server/src/pipeline/reasoning_call.rs` | New: `JudgeCall`, `JudgeStep`, `JudgeRun`, `reasoning_call` |
| `server/src/pipeline/group_director.rs` | `run_director` builds a `JudgeCall`; the chain walk moves out |
| `server/src/pipeline/stream.rs` | `run_pde_decision` wraps `reasoning_call`; the judge block consumes steps; `DirectorReasoning` → `Reasoning` |
| `server/src/pipeline/group.rs` | Consumes the renamed step and frame |
| `llm/src/model_config.rs` | `TaskConfig.timeout_secs`, `ResolvedPde.timeout`, boot validation |
| `server/src/main.rs` | Calls the boot validation |
| `examples/model_config.toml` | `#timeout_secs` and the commented reasoning body rule |

## 10. Testing

**Helper** (wiremock):
- reasoning frames come in order, then a parsed `Finished`;
- the primary returns 500, the fallback answers, and its content is
  unparseable → `ParseError` with `raw` and one failure;
- the attempt budget expires mid-stream → `TotalTimeout`, and the next model
  runs;
- `Finished` is always the last step.

**1:1 route** (wiremock, judge configured):
- the judge streams reasoning then a verdict → the frame order is
  `reasoning*` before `meta`, and the decision row's `payload.reasoning` holds
  the text;
- the judge returns no reasoning → no `reasoning` frame, and the existing 1:1
  frame assertions hold.

**Config:**
- `timeout_secs` resolves, and defaults to 15 when absent;
- `0` refuses boot;
- the committed example still loads.

**Regression:**
- the existing `run_pde_decision` unit tests change only in call form;
- the group tests change only the frame name;
- the full server suite passes.

## 11. Documentation

- `api-reference` (en/zh): the `reasoning` frame, its place and its
  conditions in the 1:1 sequence; the group section renames
  `director_reasoning`.
- `group-chat` (en/zh), the group-chat spec, and the OpenAPI description of
  the group send route: `director_reasoning` → `reasoning`, written as current
  state.
- `model-config` (en/zh): `timeout_secs` and the PDE reasoning body rule.
- `llm-audit` (en/zh): the judge payload shapes in §6.

## 12. Rollback

The change is code and config only.
- **Rolling back.** An older image ignores `timeout_secs`, so a config that
  sets it still loads. Its 1:1 stream emits no `reasoning` frame. Audit rows
  already written keep their `reasoning` key.
- **Deploying.** No migration. The new frame appears only after a deployment
  turns judge reasoning on.

## 13. Non-goals

- **Reasoning for the 1:1 reply model** (`chat_companion`). It stays off; the
  judge carries the turn's deliberation.
- **Passing the judge's reasoning text into the reply prompt.** The judge's
  `inner_state` remains the only channel from judge to reply.
- **A reset frame** when a fallback model starts reasoning again.
- **Reasoning for any other judge-like task.** This covers the input filter,
  the output filter and the image composer.

## 14. Decisions taken

1. **Shared helper.** One streamed call serves the director and the judge.
   Two rejected alternatives: copying the director's code into the judge, and
   keeping the judge one-shot and sending its reasoning as a single frame
   after it finishes.
2. **One frame name.** Both streams use `reasoning`; the group frame is
   renamed before release.
3. **Configurable time limit.** The judge's per-attempt limit is
   `timeout_secs`, default 15s, so behavior without reasoning is unchanged.
4. **Always streamed.** The judge streams whether or not reasoning is on,
   because whether a model reasons is not known before the call.
5. **Audit shape.** Reasoning goes into the payload additively; the existing
   payload shapes are kept when there is no reasoning.
