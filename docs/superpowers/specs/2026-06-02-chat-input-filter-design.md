# eros-engine — Chat user-input rewrite filter (`chat_input_filter`) (Spec)

**Status**: design, pending implementation plan
**Target release**: `0.5.x` dev track (`0.5.3-dev`). **No migration** — reuses the
`engine.chat_messages` audit columns added in `0019_chat_tip_marker_and_filter_audit.sql`.
**Audience**: anyone implementing the optional user-input rewrite path that mirrors
`chat_output_filter` (`2026-05-25-chat-output-filter-design.md`) on the input side.

---

## 0. Background

### The bug

The companion builds each chat prompt from `[system persona] + history + current
user turn`. When the current user turn carries **no communicative intent** —
`1111`, `？？？`, key-mashing, punctuation noise — the model has nothing to
respond to, so it latches onto the surrounding history and **parrots back things
the user (or the companion) said earlier**. From the user's seat this reads as
the companion "repeating itself."

The fix: when a user turn is meaningless, **rewrite it into one natural,
context-appropriate user message before it reaches `tasks.chat_companion`**, so
the model has a real thing to answer. When the turn is already meaningful, leave
it untouched.

Hard constraint: **the persisted `content` and the client-visible message must
always remain the user's original text.** If the user typed `1111`, they must see
`1111` in their history — never the rewrite. The rewrite is an internal,
model-only substitution.

### Why this mirrors `chat_output_filter` but is not identical

`chat_output_filter` rewrites the **assistant reply** before the client sees it.
It is:
- stateless (the filter LLM sees only the single reply to rewrite),
- gated by a `trigger` (random / models / traits) and per-tier `output_filter`,
- timing-aware (`before_extract` / `after_extract`).

The input filter is the input-side analogue, but deliberately simpler and
different where the problem differs:
- **Context-aware** — to rewrite `1111` into something *natural*, the filter LLM
  needs the recent conversation, not just the bare token.
- **One global switch, no tiers, no `trigger`** — it is either on or off for the
  whole deployment.
- **LLM is the sole decider via a JSON verdict** — never a heuristic, never an
  echo. This guarantees zero false positives on normal short messages
  (`嗯` / `好啊` / `在吗` / genuine short questions): the LLM is explicitly told to
  return `{"rewrite": false}` for anything meaningful, and the engine then uses
  the original verbatim.

---

## 1. Goals / non-goals

**Goals**
- Optional, off-by-default user-input rewrite, configured like `chat_output_filter`.
- A single global switch (`input_filter = true` on `[tasks.chat_companion]`).
- Original text always persisted as `content` and shown to the client.
- Rewritten text persisted in the existing `pre_filter_content` audit column and
  fed to the model for the current turn **and** future turns' history.
- Fail-open: a slow / broken / refusing filter LLM never blocks or corrupts a reply.

**Non-goals**
- No per-tier or `trigger`-gated input filtering (YAGNI; one global switch).
- No new DB columns or migration (the `0019` audit columns already exist and are
  always NULL on `role='user'` rows today).
- No change to what the extraction pipeline (insight / memory / affinity) reads —
  it continues to read the **original** user text (see §6).
- No client/protocol change — the rewrite is invisible to the wire.

---

## 2. Config surface — `model_config.toml`

### 2.1 Global switch on `[tasks.chat_companion]`

Add one task-level boolean, mirroring `output_filter`, **task-level only (no
per-tier override)**:

```toml
[tasks.chat_companion]
# ... existing fields ...
input_filter = true        # global default: false
```

### 2.2 The filter task block `[tasks.chat_input_filter]`

Reuses the existing `TaskConfig` shape (`model` / `fallback` / `retry_depth` /
`temperature` / `max_tokens` / `filter_prompt` / `reasoning`). `trigger`,
`timing`, `tiers`, and `allow_traits` are **ignored** if present (input filter has
no triggers, no timing, no tiers).

Shipped commented-out / OFF in `examples/model_config.toml`, in the same
documented style as the `chat_output_filter` block:

```toml
# ── Optional user-input rewrite filter (OFF by default) ──────────────────────
# Rewrites a MEANINGLESS user turn (e.g. "1111", "？？？", key-mashing) into one
# natural, context-appropriate user message before it reaches chat_companion, so
# the companion answers a real message instead of parroting history. Meaningful
# turns — including short ones like "嗯"/"好啊"/"在吗" and genuine short
# questions — are left untouched.
#
# Enable with a single global switch on [tasks.chat_companion]:
#   [tasks.chat_companion]
#   input_filter = true                  # global default: false
#
# The filter runs only if BOTH input_filter is true AND this table exists with a
# non-blank filter_prompt; otherwise it is inert. The user's ORIGINAL text is
# always what is persisted (content) and shown to the client; the rewrite is
# stored separately in pre_filter_content and seen only by the model.
#
# Pick a fast, cheap model — this runs on every user turn before generation.
# Recommended: google/gemini-3.1-flash-lite or deepseek/deepseek-v4-flash.
#[tasks.chat_input_filter]
#model        = "google/gemini-3.1-flash-lite"
#fallback     = ["deepseek/deepseek-v4-flash"]
#retry_depth  = 1
#temperature  = 0.3
#max_tokens   = 400
# Reasoning is OFF by default for this task (the filter only needs a short JSON
# verdict; reasoning adds latency/cost for no benefit). Same shape as
# OpenRouter's `reasoning` object — flip to `{ enabled = true }` to opt in.
#reasoning    = { enabled = false }
#filter_prompt = """
#You are an input-cleaning filter for a companion chat. You receive the recent
#conversation and the user's latest input. Decide whether the latest input is a
#meaningful message.
#
#- If it IS meaningful (any real intent, INCLUDING short replies like "嗯",
#  "好啊", "在吗", or a short question), respond EXACTLY:
#    {"rewrite": false}
#- If it is NOT meaningful (random characters, key-mashing, repeated chars,
#  punctuation-only noise), rewrite it into ONE short, natural user message that
#  fits the conversation, and give a brief reason for the rewrite. Respond EXACTLY:
#    {"rewrite": true, "content": "<the rewritten user message>", "reason": "<why it was meaningless / basis for the rewrite>"}
#
# Output JSON only. No explanation outside the JSON, no code fences.
#"""
```

---

## 3. Resolver — `crates/eros-engine-llm/src/model_config.rs`

Mirror `resolve_output_filter`, minus `trigger` / `timing` / tier.

### 3.1 New struct

```rust
#[derive(Debug, Clone)]
pub struct ResolvedInputFilter {
    pub model: String,
    pub fallback_model: Vec<String>, // already truncated to retry_depth
    pub temperature: f64,
    pub max_tokens: u32,
    pub filter_prompt: String,
    pub retry_depth: u32,
    pub reasoning: Option<ReasoningConfig>,
}
```

### 3.2 New field on `TaskConfig`

```rust
/// Global switch for the user-input rewrite filter. Task-level only on
/// chat_companion (no per-tier override, unlike `output_filter`). Default false.
#[serde(default)]
pub input_filter: Option<bool>,
```

### 3.3 New methods on `ModelConfig`

```rust
/// chat_companion `input_filter` (task-level → false). No tier param.
pub fn input_filter_enabled(&self) -> bool {
    self.tasks
        .get("chat_companion")
        .and_then(|t| t.input_filter)
        .unwrap_or(false)
}

/// Resolve the input filter. `None` (disabled) when: chat_companion
/// `input_filter` is false, OR `[tasks.chat_input_filter]` is absent, OR its
/// `filter_prompt` is blank.
pub fn resolve_input_filter(&self) -> Option<ResolvedInputFilter> {
    const FILTER_TASK: &str = "chat_input_filter";
    if !self.input_filter_enabled() {
        return None;
    }
    let task_cfg = self.tasks.get(FILTER_TASK)?;
    let filter_prompt = task_cfg.filter_prompt.clone().unwrap_or_default();
    if filter_prompt.trim().is_empty() {
        return None;
    }
    let retry_depth = task_cfg.retry_depth.unwrap_or(1);
    let m = self.resolve(FILTER_TASK, None);
    let mut fallback_model = m.fallback_model;
    fallback_model.truncate(retry_depth as usize);
    Some(ResolvedInputFilter {
        model: m.model,
        fallback_model,
        temperature: m.temperature,
        max_tokens: m.max_tokens,
        filter_prompt,
        retry_depth,
        reasoning: task_cfg.reasoning.clone(),
    })
}
```

---

## 4. JSON contract + `run_input_filter`

### 4.1 Contract

The filter LLM returns **JSON only**. Two shapes:

```json
{"rewrite": false}
{"rewrite": true, "content": "<改写后的用户消息>", "reason": "<改写依据>"}
```

`reason` is required only when `rewrite == true`; it is captured for audit and
persisted to the `filter_triggers` column (see §6). A missing/blank `reason` does
**not** block the rewrite — `content` is what matters; `filter_triggers` is then
left NULL.

Parsing reuses the existing tolerant path
(`serde_json::from_str(raw).or_else(|| find_json_block(raw)...)`, as in
`post_process.rs`).

**The engine never uses an echoed original.** "Keep" carries no text; the engine
falls back to the raw input it already holds. This removes any chance that the
filter model silently drops, truncates, or safety-mangles a legitimate message —
the engine only ever takes the filter's text when `rewrite == true`.

### 4.2 Decision table (all non-rewrite paths ⇒ use original, stamp nothing)

| Filter outcome | Action |
|---|---|
| JSON parse fails (neither `from_str` nor `find_json_block`) | keep → original |
| `rewrite` missing or `!= true` | keep → original |
| `rewrite == true`, `content` missing / blank / whitespace | keep → original |
| `rewrite == true`, `content` fails the refusal gate (`rewrite_content_invalidity`) | keep → original |
| model error / timeout / empty reply (per chain entry) | walk to next entry; chain exhausts ⇒ keep → original |
| `rewrite == true`, `content` valid | **rewrite** → use `content` |

> **Validity gate:** the input filter uses a dedicated `rewrite_content_invalidity`
> (refusal-head scan + `content_filter` finish reason), **not** the output
> filter's `filter_output_invalidity` — the latter enforces an 80-char minimum
> (`MIN_FILTERED_OUTPUT_CHARS`) that would reject every (naturally short)
> rewritten user message as `too_short`. The input gate has no length floor.

### 4.3 Function

In `stream.rs`, structured like `run_output_filter`:

```rust
struct InputRewrite {
    rewritten_text: String,           // -> content's replacement (model-only)
    filter_model: String,             // -> filter_model
    reason: Option<String>,           // -> filter_triggers = {"reason": ...} (NULL if absent)
    f_generation_id: Option<String>,  // -> f_generation_id (always passed through)
}

/// Returns Some(rewrite) only when the filter LLM explicitly asked to rewrite
/// with valid content. Every other outcome returns None (caller uses the
/// original input). Never errors out of the chat path.
async fn run_input_filter(
    state: &AppState,
    f: &ResolvedInputFilter,
    recent_transcript: &str,
    raw_input: &str,
) -> Option<InputRewrite>
```

- **System message** = `f.filter_prompt` (operator policy).
- `reasoning: f.reasoning.clone()` is forwarded on the request (default off via
  the example config; see §2.2).
- **User message** = engine-formatted payload: a compact recent-transcript block
  (last few `用户:` / `AI:` turns, same labelling as `dreaming.rs`) followed by the
  raw latest input, clearly delimited. The engine only *formats context*; the
  *task* lives in `filter_prompt` (mirrors output filter: system=policy,
  user=payload).
- Walks the depth-capped fallback chain with a per-model timeout
  (reuse `FILTER_TIMEOUT`), applying §4.2 per entry.

---

## 5. Execution placement (Axis B1) — inside `run_stream`, after the idempotency gate

### 5.1 Current flow (unchanged entry points)

1. `companion_stream::send_message_stream` → `upsert_user_message_idempotent` (the
   **idempotency gate**): `Inserted` ⇒ generate; `DuplicateInProgress` / `Replay`
   ⇒ do **not** generate.
2. Only `Inserted` reaches `run_stream`.

### 5.2 New step (Reply turns only)

Near the top of `run_stream`, before prompt assembly, **for `UserMessage`-driven
Reply turns only** (not gift / proactive):

1. `let Some(f) = state.model_config.resolve_input_filter() else { /* skip */ }`.
2. Build `recent_transcript` from a recent-history slice (reuse the history fetch
   `run_stream`/handlers already perform where practical; otherwise a small
   `history(session_id, K, 0)` slice).
3. `run_input_filter(&state, &f, &recent_transcript, &user_msg.content)`.
4. On `Some(rewrite)`: `UPDATE` the user row to set `pre_filter_content` +
   `filter_model` (see §6), **before** the handler fetches history for prompt
   assembly, so the rewritten text is picked up via the new `ChatMessage` field
   (§7). `content` is untouched.
5. On `None`: no-op (original input flows through unchanged).

### 5.3 Why B1 (vs running before the idempotency gate)

- Runs **exactly once per genuinely-new turn** — replays/duplicates never reach
  `run_stream`, so retries/reconnects never re-pay the filter LLM call.
- Lives beside `run_output_filter`; the whole filter feature is in one file.
- Invariant: **the input filter runs iff a reply is generated.**

Cost of B1: one extra `UPDATE` per real rewrite. Accepted (cheap, bounded).

### 5.4 New store method

```rust
/// Stamp the input-filter rewrite onto an existing user row. content is left
/// untouched (the client always sees the original). f_client_msg_id stays NULL
/// (the user row is updated in place rather than inserting a separate filter
/// row). `reason`, when present, is stored as `filter_triggers = {"reason": ...}`.
pub async fn set_user_input_rewrite(
    &self,
    user_message_id: Uuid,
    pre_filter_content: &str,        // the rewritten effective text
    filter_model: &str,
    reason: Option<&str>,            // -> filter_triggers {"reason": ...}, NULL if None/blank
    f_generation_id: Option<&str>,
) -> Result<(), sqlx::Error>;
```

---

## 6. Persistence — reuse the `0019` audit columns on the `role='user'` row

| Column | User row (input filter) | Assistant row (output filter, today) |
|---|---|---|
| `content` | **original** user text (shown to client) | filtered reply (shown to client) |
| `pre_filter_content` | **rewritten** effective text (model-only) | original pre-filter reply |
| `filter_model` | rewrite model that produced `pre_filter_content` | output-filter model |
| `filter_triggers` | `{"reason": "<llm rewrite reason>"}` (NULL if no/blank reason) | fired predicates JSONB |
| `f_generation_id` | rewrite call's OpenRouter generation id (always stamped) | output-filter generation id |
| `f_client_msg_id` | NULL (row updated in place) | per output-filter call |

**Documented semantic note (important):** on a **user** row, `pre_filter_content`
holds the *post*-rewrite effective text — the **inverse direction** of an
assistant row, where it holds the *pre*-filter original. Same columns, opposite
filter. This is the intended reuse (these columns are otherwise always NULL on
`role='user'` rows), not a bug.

Only a real rewrite stamps these columns. A "keep" verdict leaves all of them
NULL — the original `content` already is the effective text, so there is nothing
to store.

---

## 7. Prompt + recall wiring

### 7.1 `ChatMessage` gains the column

```rust
// crates/eros-engine-store/src/chat.rs — struct ChatMessage
#[serde(default)]
pub pre_filter_content: Option<String>,
```

`history()` uses `SELECT *`, so `FromRow` maps the existing column with no SQL
change.

### 7.2 Effective-text selection (user rows only)

Define effective text for a history row:

```
effective(msg) =
    if msg.role in {"user", "gift_user"}: msg.pre_filter_content.filter(non-blank) ?? msg.content
    else (assistant): msg.content
```

- `assemble_chat_request` (`handlers.rs`) feeds `effective(msg)` to the model.
- The memory-recall **query text** (`handlers.rs` ~L583, currently the latest
  `role=="user"` row's `content`) uses `effective(...)` too — recall on `1111` is
  as useless as a reply to it.
- **Assistant rows are untouched** — this preserves `chat_output_filter`'s
  `content = filtered` contract; we must NOT feed assistant `pre_filter_content`
  (the unfiltered reply) back into history.

Because effective-text selection keys on the persisted column, it applies
uniformly to the **current turn** (just stamped in §5) and to **all past
meaningless turns**, which is what actually drains the "meaningless token sits in
history → repetition" mechanism.

### 7.3 Client read path unchanged

`history_slim` (BFF/UI) selects only `content` — the client always sees the
original. No change.

---

## 8. Extraction / post-process reads the original

Insight / memory / affinity extraction continue to read **`content`** (original),
mirroring `chat_output_filter`'s default `after_extract`. Rationale:

- A rewrite is an inference, not a fact. Extracting "memories" from fabricated
  text would seed false profile/relationship rows.
- The affinity "non-trivial user message" gate (`post_process.rs` ~L116) should
  keep seeing the trivial original and skip scoring noise — correct behaviour.

No change to post-process is required: it already reads `content`, and `content`
stays original.

---

## 9. Fail-open + latency

- **Fail-open**: every §4.2 non-rewrite outcome degrades to the original input.
  A broken/slow/refusing filter LLM reproduces today's behaviour for that turn
  (the repetition bug may surface), but **never blocks or corrupts** the reply.
- **Latency**: "every turn calls the LLM" puts one rewrite round-trip (with recent
  history) on the critical path before generation. This is the accepted cost of
  the zero-false-positive design; mitigated by recommending a fast/cheap filter
  model and by the per-model `FILTER_TIMEOUT`.

---

## 10. No migration

The `pre_filter_content`, `filter_model`, `filter_triggers`, `f_generation_id`,
`f_client_msg_id` columns already exist (`0019`). This feature only starts
**writing** `pre_filter_content` / `filter_model` / `filter_triggers` (the
`{"reason": ...}` audit) / `f_generation_id` on `role='user'` rows and
**reading** `pre_filter_content` in prompt assembly. No schema change.

---

## 11. Testing

**Unit (`model_config.rs`)**
- `resolve_input_filter`: `None` when switch off / table absent / blank prompt;
  `Some` with correct model/fallback (truncated to `retry_depth`)/prompt when on.
- `input_filter_enabled`: default false; true when set.
- Compat fixture / committed-example parse test updated to include
  `chat_input_filter` + `input_filter` (schema lock).

**Unit (`stream.rs`)**
- JSON decision table (§4.2): `{"rewrite":false}` ⇒ None; `{"rewrite":true,
  "content":"x"}` ⇒ Some; missing/blank `content` ⇒ None; refusal `content`
  (caught by `rewrite_content_invalidity`) ⇒ None; unparseable ⇒ None;
  `find_json_block` fallback path ⇒ Some.
- `reason` extraction: `{"rewrite":true,"content":"x","reason":"r"}` ⇒
  `reason = Some("r")`; reason absent/blank ⇒ `reason = None`.
- effective-text selection: user row with `pre_filter_content` ⇒ rewritten;
  user row without ⇒ original; assistant row ⇒ always `content`.

**Integration (`#[sqlx::test]`)**
- Rewrite turn: `content` stays original; `pre_filter_content` + `filter_model` +
  `f_generation_id` stamped; `filter_triggers` = `{"reason": ...}` when the
  verdict carried a reason (NULL when it did not); assembled prompt (and recall
  query) use the rewritten text.
- Keep turn: all audit columns NULL, prompt uses original.
- Filter-off (no switch / blank prompt): behaviour byte-identical to today.
- Past-turn carry-through: a prior rewritten user row feeds its rewrite into a
  later turn's history.
- `history_slim` still returns original `content` for a rewritten row.

---

## 12. Docs

- `docs/model-config.md`: add `chat_input_filter` + `input_filter` to the stability
  table (§ "Stability commitments") and a short section describing the global
  switch, JSON contract, and column reuse.
- `examples/model_config.toml`: the commented-out block in §2.2.
- Release notes link this spec (per the OSS specs-are-public convention).

---

## 13. Stability commitments

- `input_filter` (bool, `[tasks.chat_companion]`, task-level) and
  `[tasks.chat_input_filter]` (reusing the `TaskConfig` shape) join the committed
  0.x config schema. Adding optional fields stays compatible; renaming/removing
  or making required breaks the compat fixture test.
- JSON contract field names (`rewrite`, `content`, `reason`) are an
  engine↔filter-prompt contract documented in the example `filter_prompt`;
  changing them is a prompt+engine change, not a public API break (operators own
  their `filter_prompt`).

---

## 14. Open questions

None blocking. Possible future extensions (explicitly out of scope here):
per-tier enablement, a `trigger`-style gate, or a `timing`-style toggle to let
extraction read the rewrite. All deferred under YAGNI until a real need appears.
