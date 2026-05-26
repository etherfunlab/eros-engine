# chat_output_filter — Output Validity Gate Design

**Date:** 2026-05-26
**Status:** Implemented (feat/v0.5.0-filter-validity-gate)

---

## §0 Background

`run_output_filter` calls the filter LLM and, on any HTTP 200 response, persists
the reply text into `chat_messages.content`.  A filter model can return an HTTP
200 with a refusal text ("抱歉，我无法协助完成您的请求。") — the engine has no way
to distinguish that from a valid rewrite.  This has been observed in production.

A single-model swap is insufficient as a fix: any model's safety alignment policy
can shift at any time and begin producing refusals.  The fix must be structural.

---

## §1 Goal / Non-goals

**Goal:** After each per-model filter response, run a lightweight validity check.
On failure, walk to the next model in the chain exactly as the chat burst already
does.  The check is cheap (no extra LLM call) and covers the three known failure
modes.

**Non-goals:**

- The fail-open semantic is unchanged: when the whole chain exhausts (every model
  fails the gate, errors, or times out), `run_output_filter` returns `None` and
  the caller emits and persists the original reply.
- No schema changes; no config-file changes; no new environment variables.
- No change to how the streaming chat burst walks its own chain.

---

## §2 Validity Gate Spec

`filter_output_invalidity(text, finish_reason) -> Option<&'static str>`

Returns `Some(label)` when invalid, `None` when valid.  Checks run in order
(cheapest first):

### 2.1 `finish_reason == "content_filter"` → label `"content_filter"`

Gemini and OpenAI both use this string when mid-response safety truncation fires.
Checked before any text scan so it is O(1).

### 2.2 Refusal pattern in head → label `"refusal_pattern"`

Scan the first `REFUSAL_HEAD_SCAN_CHARS = 120` Unicode characters of the text.
If any pattern in `REFUSAL_PATTERNS_HEAD` appears, the response is a refusal.

Curated list covers Chinese shapes observed in production and the standard
OpenAI/Anthropic English apology forms.  Anchored to the head to avoid false
positives: a clean rewrite that incidentally contains "won't" or "sorry" past
character 120 is NOT flagged.

### 2.3 Short-text checks

`total_chars = text.chars().count()` (Unicode code points — correct for CJK).

If `total_chars < MIN_FILTERED_OUTPUT_CHARS (= 80)`:

- Contains a verb in `REFUSAL_SHORT_VERBS` anywhere in the text →
  label `"refusal_pattern"`.
- Otherwise → label `"too_short"`.

A valid filter rewrite is at least 80 characters.  Clean rewrites shorter than
that are flagged as `"too_short"` and cause the next model to be tried.  This
is an acceptable trade-off: the filter task is full-text rewriting, not
summarisation.

---

## §3 Chain Walking

`run_output_filter` previously delegated chain walking to
`OpenRouterClient::execute()`, which walks internally and returns the first
non-error HTTP 200.  The validity gate requires seeing each model's response
individually before deciding to continue.

The function now builds its own chain:

```text
chain = [f.model] ++ f.fallback_model
```

For each model, it builds a `ChatRequest` with `fallback_model = vec![]` and
calls `execute()` (single-model, no internal walking).  On a validity gate
failure it logs the reason and advances to the next model.  If the chain
exhausts, it returns `None` (fail-open).

`f_client_msg_id` (the `f_` ULID) is generated once before the loop and reused
across all model attempts within a single logical filter call.  `retries_filter`
reflects the index of the model that passed the validity gate — 0 for the
primary, 1 for the first fallback, etc.

### §3.5 Fail-open audit (`chat_messages.metadata`)

When the whole filter chain fails validity (or every model errors / times out),
the engine still fails open — emits + persists the original reply — but now also
writes a fail-open audit into `chat_messages.metadata` so ops can identify these
rows without ambiguity:

```json
{
  "prompt_traits": ["..."],
  "tier": "...",
  "filter_outcome": "fail_open",
  "f_client_msg_id": "f_01J...",
  "filter_attempts": [
    {"model": "openai/gpt-5.4-nano", "reason": "refusal_pattern"},
    {"model": "google/gemini-3.1-flash-lite", "reason": "content_filter"}
  ]
}
```

Reason vocabulary:
- `"refusal_pattern"`, `"too_short"`, `"content_filter"` — from
  `filter_output_invalidity`
- `"empty"` — model returned an empty `reply` field (checked before the
  validity gate; distinguishes "model returned literally nothing" from
  "model returned a short but non-empty response")
- `"error"` — HTTP error / non-2xx response from the OpenRouter call
- `"timeout"` — exceeded the 15s per-model filter timeout

`run_output_filter` now returns `Result<RunFilterOutcome, FilterFailOpen>`
instead of `Option<RunFilterOutcome>`. The `FilterFailOpen` value carries the
`f_client_msg_id` (generated once before the loop) and the `Vec<FilterAttemptFailure>`
audit log. The caller writes these into metadata only when `Err(fail)` is
received — i.e., when filter was triggered AND every model in the chain failed.

Rows where the filter trigger NEVER fired (trigger predicate returned `None`,
or no filter is configured) are unchanged: `filter_outcome` / `filter_attempts`
are absent. The audit is specifically for "we tried to filter and failed,"
not "we chose not to filter." Ops query:

```sql
SELECT * FROM engine.chat_messages
WHERE metadata->>'filter_outcome' = 'fail_open';
```

---

## §4 `finish_reason` Plumbing

The non-streaming `execute()` path did not previously surface `finish_reason`.
This change threads it from the wire:

- `WireChoice` gains `#[serde(default)] finish_reason: Option<String>`.
- `ChatResponse` gains `pub finish_reason: Option<String>`.
- `call_once` copies `choices[0].finish_reason` into the returned `ChatResponse`.

The streaming path (`DeltaChunk`) already carries `finish_reason` — that field
is not touched.

---

## §5 Testing

Seven unit tests on the pure `filter_output_invalidity` function cover:

| Test | Input | Expected |
|------|-------|----------|
| Chinese refusal in head | `"抱歉，我无法…"` | `Some("refusal_pattern")` |
| English refusal in head | `"I'm sorry, but I can't…"` | `Some("refusal_pattern")` |
| `content_filter` finish_reason | long clean text + `"content_filter"` | `Some("content_filter")` |
| Short + refusal verb | `"我无法。"` | `Some("refusal_pattern")` |
| Short + no refusal verb | `"她笑了。"` | `Some("too_short")` |
| Long clean rewrite | 200+ char text + `"stop"` | `None` |
| Refusal word past char 120 | clean rewrite with `"won't"` at position >120 | `None` (regression guard) |

---

## §6 Rollout

- Additive change only: new field on `ChatResponse` (default `None`), new
  constants, new helper function, refactored loop in `run_output_filter`.
- No OpenAPI schema changes (the filter is internal stream logic; `ChatResponse`
  does not surface in any DTO).
- No database schema changes.
- No operator config changes required — the gate is always-on.
- Deployed by building and releasing a new image as usual; no migration needed.
