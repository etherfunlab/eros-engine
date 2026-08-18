# LLM Error Audit and Upstream Status Passthrough

**Spec:** 2026-08-18
**Status:** design
**Target release:** v1.4.0 (store migration `0050`)
**Crates:** `eros-engine-llm`, `eros-engine-server`, `eros-engine-store`

---

## §0 Background

Every LLM call in the engine goes through a fallback chain: `[primary] + fallback_model[]`,
first success wins. `LlmError::Status(reqwest::StatusCode, String)`
(`crates/eros-engine-llm/src/error.rs:11-12`) carries the exact upstream HTTP status —
429, 502, 503, 529 — from the moment the response headers arrive.

That status never reaches Postgres, and never reaches a downstream consumer.

Four places drop it:

1. **`StreamErrorCode`** (`crates/eros-engine-server/src/pipeline/stream.rs:22-27`) is a
   four-value enum. Two of its variants — `RateLimited`, `Timeout` — are **never
   constructed anywhere in the codebase**. Every upstream failure collapses to
   `UpstreamUnavailable`.
2. **`attempt_outcome: &'static str`** (`stream.rs:390`). The `Ok(Err(e))` arm at
   `stream.rs:468` reduces a status-bearing `LlmError` to the literal `"open_error"`.
3. **Chain-internal failures reach only `tracing::warn!`** (`openrouter.rs:737`/`744`,
   `stream.rs:456`/`469`). Only `last_err` survives the walk; every earlier hop is gone.
4. **`AppError::Upstream` always renders 502** (`crates/eros-engine-server/src/error.rs:80`),
   whatever the upstream actually returned.
5. **The input filter chain persists nothing at all.** `run_input_filter`
   (`stream.rs:2440-2523`) walks its own chain, `warn!`s each failure, and returns `None` on
   exhaustion. Unlike the output filter it has no `filter_attempts` equivalent — not even a
   coarse marker.

A sixth swallow is deliberate: on chain exhaustion the engine serves a pseudo-ghost phrase
("huh?") as a normal reply (`2026-05-26-error-fallback-config-design.md`), so the consumer
is never told the upstream failed. That spec's own §1 states "do NOT silently swallow infra
failures" — the implementation is the largest swallow in the tree.

### What the upstreams actually return

**OpenRouter** — `{"error": {"code": <number>, "message": <string>, "metadata": {...}}}`,
where `error.code` equals the HTTP status.

| Status | Meaning |
| --- | --- |
| 402 | Insufficient credits |
| 403 | Permission / guardrail / moderation |
| 408 | Request timeout |
| 429 | Rate limited (`Retry-After` header) |
| 502 | Chosen model is down or returned an invalid response |
| 503 | No provider meets routing requirements (`Retry-After` header) |
| 529 | Overloaded |

`error.metadata` carries two fields the engine currently discards entirely:
`error_type` (`rate_limit_exceeded`, `context_length_exceeded`, …) and `provider_code`
(the *upstream's own upstream* code; omitted on 500-class errors).

**Venice** — 40 named codes over 9 HTTP statuses, in four different body shapes
(`{"error": "..."}`; the OpenAI-compatible `{"error": {message, type, param, code}}`;
plus `details` / `credits_refunded` variants). Venice has **no 529**: it reports overload
as `429 MODEL_OVERLOADED` and `503 MODEL_AT_CAPACITY`, and timeout as `504 REQUEST_TIMEOUT`.

The two vendors disagree on which status carries which semantic. A third will disagree
differently. **The engine must therefore not model upstream status as an enum.** It stores
the raw `u16` and classifies by HTTP convention (4xx client, 5xx upstream, 408/429/5xx
retryable), so an unrecognised 570 behaves sensibly without a code change.

---

## §1 Goals and Non-Goals

**Goals**

1. Persist the upstream HTTP status and provider error codes for every failed LLM attempt,
   **including attempts a fallback recovered from**.
2. Pass the upstream status through to the downstream consumer verbatim on non-streaming
   endpoints, and as structured data on the SSE stream.
3. Give gateway-side failures (timeouts, transport drops, decode errors) a single
   uniform, queryable home across every call site, so the engine itself can be debugged
   without log archaeology.
4. Accept unknown status codes. HTTP status semantics are a public convention; the engine
   assumes every upstream honours them.

**Non-Goals**

- **No status-aware retry or backoff.** The engine has none today; this spec adds none.
  Honouring `Retry-After` is the consumer's decision, so the value is recorded and passed
  on, not acted upon.
- **No new audit rows where none exist today.** The 10 tables in the insight / memory /
  world families skip the write entirely on failure. They stay that way.
- **The pseudo-ghost stays.** Chain exhaustion still serves a phrase as a normal reply. What
  changes is that the failure now rides along in the `final` frame and in the row, instead
  of vanishing.
- **No upstream-status-driven model selection.**

---

## §2 The split: three homes for three kinds of fact

The organising principle is **who authored the fact**, not which code path produced it.

| Home | Holds | Example |
| --- | --- | --- |
| `llm_attempts` (new column) | What the **upstream** said | `http_status: 529`, `provider_code: "529"`, `error_type: "overloaded"` |
| `gateway_errors` (new column) | Where the engine's **path to the provider** broke | `kind: "open_timeout"`, `kind: "transport"` |
| Existing per-table coarse markers | Each table's **own business verdict** | `last_failure: "refusal_pattern"`, `eval_skip_reason: "short_user_msg"` |

The three never overlap, so each fact has exactly one authoritative home.

The boundary between the first two is whether the upstream spoke at all:

- A non-2xx response → the upstream spoke. `llm_attempts`.
- A `200 OK` SSE stream carrying `{"error": {...}}` mid-stream → the upstream spoke, just not
  in the status line. `llm_attempts`, with `http_status: 200`.
- A `200 OK` stream terminating with `finish_reason: "error"` and no error object → the
  upstream signalled a failure without a code. `llm_attempts`, `http_status: 200`, no
  `provider_code`.
- A connection reset, a TLS error, a header timeout, an unparseable body → the upstream said
  nothing usable. `gateway_errors`.
- An empty completion, byte-BPE garble, a refusal, a length cut → the call **succeeded**.
  Neither column. This is a content verdict and belongs to the table's coarse marker.

### Why the coarse markers survive

They are not projections of the failure lists. `companion_affinity_events.context.eval_skip_reason`
records **intentional skips** — `proactive`, `short_user_msg`, `empty_assistant` — where no
call was ever made. `chat_vision_events.last_failure` records content verdicts like
`refusal_pattern`. Those facts have no other home, and no failure list can derive them.

What the markers lose is the transport-layer values that were shoehorned into them because
there was nowhere else to put them (§7).

### §2.1 The three homes are a stack

The homes are ordered by distance from the engine, and each layer is more abstract than the
one above it:

| Layer | Home | Answers |
| --- | --- | --- |
| Provider | `llm_attempts` | What did the far end say? |
| Gateway | `gateway_errors` | Where did our path to the far end break? |
| Business | Coarse marker | What is this row's verdict? |

**Why `gateway_errors` and not `engine_errors`.** Broadly read, every defect in this codebase
is an engine error, so that name would attract anything a contributor felt was the engine's
fault. `gateway_errors` names the role instead of the process: the engine acting as a gateway
to LLM providers, and only failures of that role. A panic in the affinity math is an engine
error and does not belong here; a TLS reset while reaching OpenRouter does.

The layer names also resist the obvious OSI reading, which inverts them: genuine transport
failures (connection reset, TLS error, interrupted SSE body) belong to the **gateway** layer,
not to `llm_attempts`, which carries application-layer HTTP status and provider error codes.
The far end is the one that spoke; the path to it is what we own.

### §2.2 Contagion: one incident, several layers

"One fact, one home" governs a fact, not an incident. A single incident can produce a
distinct fact at more than one layer, and each of those facts belongs in its own home. What
must never happen is the *same* fact being written twice, or a fact being attributed to a
layer that did not produce it.

**Contagion runs down the stack only** — far to near, specific to abstract:

```
llm_attempts  →  gateway_errors  →  coarse marker
```

An upstream failure may also produce an engine-structural fact, which may also force a
business verdict. Never the reverse: an engine timeout is never written as an upstream
error, and a content verdict never manufactures an entry in either column.

The canonical case is chain exhaustion. Three models each return `529`:

| Layer | What it records | Why it is a distinct fact |
| --- | --- | --- |
| `llm_attempts` | three entries, `http_status: 529` each | What each upstream said, per hop |
| `gateway_errors` | one entry, `kind: "chain_exhausted"` | The chain died — true regardless of *why* each hop failed, and the one cross-table signal an engine operator queries |
| coarse marker | `fallback_reason: "stream_failure"` (chat) / `status: "exhausted"` (vision) | This table's business verdict for this row |

Contrast a recovered turn: the primary returns `529`, the fallback succeeds. One
`llm_attempts` entry, **no** `gateway_errors` entry (nothing in the engine broke — the chain
did its job), and no coarse failure marker at all. Contagion fires only when the lower
layer's own contract requires an entry.

**Labelling rule for the pointer values (§7):** if the operation produced *any*
`llm_attempts` entry, the coarse marker reads `upstream_error`; otherwise `gateway_error`.
Upstream wins, because "did a provider misbehave during this turn" is the question the coarse
value exists to answer at a glance. The per-hop truth is one column away.

---

## §3 Schema

Migration `crates/eros-engine-store/migrations/0050_llm_attempt_audit.sql`.

```sql
ALTER TABLE engine.chat_messages             ADD COLUMN llm_attempts JSONB,
                                             ADD COLUMN gateway_errors JSONB;
ALTER TABLE engine.chat_vision_events        ADD COLUMN llm_attempts JSONB,
                                             ADD COLUMN gateway_errors JSONB;
ALTER TABLE engine.chat_images_events        ADD COLUMN llm_attempts JSONB,
                                             ADD COLUMN gateway_errors JSONB;
ALTER TABLE engine.companion_decision_events ADD COLUMN llm_attempts JSONB,
                                             ADD COLUMN gateway_errors JSONB;
ALTER TABLE engine.companion_affinity_events ADD COLUMN llm_attempts JSONB,
                                             ADD COLUMN gateway_errors JSONB;
```

Additive, nullable, no backfill, no index. Five tables, identical shape — a fleet-wide error
view is one `UNION ALL`.

`NULL` means "nothing to record". An empty array is never written, so there is one way to say
"no failure".

`engine.chat_messages` hosts three of the seven call sites. They share the columns and are
told apart by each element's `task` field.

**Why `task` and not a new discriminator.** `[tasks.*]` in the model config is already the
authoritative vocabulary for "which LLM call is this", it is already carried on the wire as
`ChatRequest.task`, and it already keys per-task model resolution. All seven call sites map
to exactly one task each:

| Call site | Table | Row | `task` |
| --- | --- | --- | --- |
| Chat model chain | `chat_messages` | assistant | `chat_companion` / `chat_voice` / `chat_product_qa` |
| Output filter chain | `chat_messages` | assistant | `chat_output_filter` |
| Input filter chain | `chat_messages` | **user** | `chat_input_filter` |
| Vision describe | `chat_vision_events` | — | `chat_vision` |
| Image prompt compose | `chat_images_events` | — | `chat_image_prompt_compose` |
| PDE judge | `companion_decision_events` | — | `pde_decision` |
| Affinity eval | `companion_affinity_events` | — | `affinity_evaluation` |

The input filter is the one call site with **no failure record of any kind today**.
`run_input_filter` (`stream.rs:2440-2523`) walks its own chain, logs each failure with
`tracing::warn!`, and returns `None` on exhaustion; nothing is persisted. Its *success* audit
already lands on the `role='user'` row via `ChatRepo::set_user_input_rewrite`
(`crates/eros-engine-store/src/chat.rs:758`, writing `pre_filter_content` / `filter_model` /
`filter_triggers` / `f_generation_id`), so its failures belong on the same row. That call is
made only on success, so a sibling `ChatRepo::set_user_llm_failures(user_message_id,
llm_attempts, gateway_errors)` is needed — an `UPDATE … WHERE id = $1 AND role = 'user'`,
issued whenever the chain had at least one failure, whether or not a rewrite was produced.

`task` is strictly finer than a hand-rolled discriminator would be — it separates
`chat_companion` from `chat_voice` from `chat_product_qa`, which a coarser `stage: "chat"`
would have flattened.

---

## §4 Shapes

Both columns hold a JSON array ordered by the time each attempt failed.

### 4.1 `llm_attempts`

```json
[
  {
    "task": "chat_companion",
    "model": "x-ai/grok-4.20",
    "http_status": 529,
    "provider_code": "529",
    "error_type": "overloaded",
    "upstream_provider_code": "anthropic:overloaded_error",
    "retry_after_s": 30,
    "message": "code=529: Overloaded"
  },
  {
    "task": "chat_output_filter",
    "model": "some/filter-model@venice",
    "http_status": 429,
    "provider_code": "MODEL_OVERLOADED",
    "message": "code=\"MODEL_OVERLOADED\": The model is currently overloaded"
  }
]
```

| Field | Required | Source |
| --- | --- | --- |
| `task` | yes | The `[tasks.*]` key for this call |
| `model` | yes | The full config slug of the attempted model, `@provider` suffix kept |
| `http_status` | yes | The status the upstream actually returned. `200` for a mid-stream error |
| `message` | yes | Scrubbed, flattened, length-capped. Never contains prompt text |
| `provider_code` | no | OpenRouter `error.code`, Venice `error.code` |
| `error_type` | no | OpenRouter `metadata.error_type` |
| `upstream_provider_code` | no | OpenRouter `metadata.provider_code` |
| `retry_after_s` | no | Parsed from the `Retry-After` response header |

There is deliberately **no `retryable` field**. With `http_status` present, the consumer
applies the HTTP convention itself; the engine does not editorialise inside a column whose
contract is "what the upstream said".

`message` reuses the existing scrubbing guarantees: `metadata.flagged_input` is dropped
(a moderation rejection must never echo the user's prompt back into an audit row), and
provider-controlled fields are flattened to a single bounded line.

### 4.2 `gateway_errors`

```json
[
  {
    "task": "chat_companion",
    "model": "x-ai/grok-4.20",
    "kind": "open_timeout",
    "message": "stream open timeout after 20s"
  },
  {
    "task": "chat_vision",
    "model": "some/vision-model",
    "kind": "transport",
    "message": "connection reset by peer"
  }
]
```

| Field | Required | Notes |
| --- | --- | --- |
| `task` | yes | Same vocabulary as `llm_attempts` |
| `kind` | yes | Closed enum, below |
| `message` | yes | Bounded, single line |
| `model` | no | Absent when the failure precedes model selection (a config error) |

`kind` — engine-structural failure modes only, uniform across all five tables:

| `kind` | Scope | Meaning |
| --- | --- | --- |
| `open_timeout` | attempt | Connect / queue / response-headers timeout |
| `total_timeout` | attempt | One attempt's whole generation exceeded its cap |
| `idle_timeout` | attempt | Byte-level idle watchdog fired mid-stream |
| `transport` | attempt | Connection reset, TLS failure, SSE body interrupted |
| `decode` | attempt | Response arrived but could not be parsed |
| `config` | attempt | Local misconfiguration (empty model slug, unresolvable provider) |
| `chain_exhausted` | chain | Every candidate failed. Carries no `model` |

`open_timeout` / `total_timeout` / `idle_timeout` stay distinct: issue #188 split them
apart precisely because folding them together made idle timeouts invisible. Here the
distinction becomes SQL-queryable rather than log-only.

`chain_exhausted` is the contagion entry of §2.2 — the one chain-scoped kind. Each table
already states chain exhaustion in its own dialect (`fallback_reason: "stream_failure"`,
`status: "exhausted"`, `status: "upstream_error"`), and none of those can be queried
together. This is the same incident recorded once at the link layer, in the one vocabulary
that spans all five tables.

### 4.3 The same shape on the wire

`ProtocolFrame::Final` and the non-streaming error body serialise the identical structures
from the identical Rust types. There is one serializer, not two.

---

## §5 Producing the data

### 5.1 `ParsedErrorBody` replaces the stringified body

`scrub_error_body(&str) -> String` (`crates/eros-engine-llm/src/openrouter.rs:209`) becomes
`parse_error_body(&str) -> ParsedErrorBody`:

```rust
pub struct ParsedErrorBody {
    pub code: Option<String>,
    pub error_type: Option<String>,
    pub provider_code: Option<String>,
    pub message: String,
}
```

Its `Display` impl emits the exact string the function returns today, so every existing log
line and every existing assertion is byte-identical. It must parse all four observed body
shapes (OpenRouter envelope; Venice OpenAI-compatible; Venice bare `{"error": "..."}`;
non-JSON, which falls through to a bounded preview of the raw text).

The existing scrubbing tests — `scrub_error_body_drops_moderation_flagged_input`,
`scrub_error_body_bounds_and_flattens_hostile_metadata`,
`scrub_error_body_handles_numeric_code_and_non_envelope` — are retained against `Display`.

### 5.2 `LlmError` keeps the structure

```rust
Status(reqwest::StatusCode, ParsedErrorBody)   // was (StatusCode, String)
Provider(ParsedErrorBody)                      // was (String)
```

`Provider` currently loses the mid-stream error code to
`format!("openrouter mid-stream error: code={:?}: {}", ...)` at `openrouter.rs:1190`. Holding
`ParsedErrorBody` fixes that at the source. `ParsedErrorBody::message_only(&str)` covers the
call sites that have prose rather than an envelope (`decode_or_api_error`, the
`finish_reason=error` terminator).

`Retry-After` is read off the response headers at the three status checks
(`openrouter.rs:849`, `:994`, `:1138`) and carried alongside.

### 5.3 One conversion point

```rust
pub enum AttemptFailure {
    Upstream(UpstreamAttempt),   // → llm_attempts
    Gateway(GatewayError),       // → gateway_errors
}

impl AttemptFailure {
    pub fn from_llm_error(task: &str, model: &str, e: &LlmError) -> Self;
}
```

An exhaustive `match` over `LlmError`, in one place. Adding an `LlmError` variant later fails
to compile until it is classified, which is the point.

### 5.4 Chain walks collect instead of discarding

`OpenRouterClient::execute` and `execute_vision` (`openrouter.rs:678`, `:784`) accumulate a
`Vec<AttemptFailure>` beside the existing `last_err`, and:

- `ChatResponse` gains `failures: Vec<AttemptFailure>` — **populated on success too**, so a
  turn that recovered on the second model still reports what the first one said.
- Chain exhaustion returns `LlmError::Chain { failures }` instead of the bare `last_err`. Its
  `Display` renders the final failure plus the attempt count, so existing `tracing::warn!`
  output stays readable.

The streaming chat path walks its chain in the server (`drive_chat_burst`, `stream.rs:284`),
not in the client, so it accumulates its own `Vec<AttemptFailure>` from
`AttemptFailure::from_llm_error` at each failed attempt.

---

## §6 Wire contract

### 6.1 SSE `final` frame

```rust
Final {
    filtered: bool,
    prompt_injected: Option<Vec<String>>,
    tier: Option<String>,
    retries_chat: u32,
    retries_filter: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    llm_attempts: Vec<UpstreamAttempt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    gateway_errors: Vec<GatewayError>,
}
```

Emitted on **every** turn that had a failure, including a pseudo-ghost turn and including a
turn that recovered. This is the non-fatal channel: the consumer can alert and account
without the end user seeing an error banner, and the pseudo-ghost keeps working exactly as
it does today.

#### What `final` carries, and what it deliberately does not

It carries the chains whose failure changes what the consumer received: the chat model chain,
the input filter, the output filter, the PDE judge, and the image prompt composer.

**Affinity eval is excluded on purpose.** It runs in `post_process`, after the response is
already out, but timing is not the reason — even if it ran earlier it would stay out. Its
failure is not a downstream fact:

- It is already fail-open. The only consequence is that this one turn contributes no affinity
  delta, and the rule-based deltas still land.
- Turns legitimately contribute no delta for ordinary reasons — `short_user_msg`,
  `proactive`, `empty_assistant`. "No affinity judgment this turn" is a normal state, not an
  incident, and a consumer has no way to act differently on the two cases.

So its `llm_attempts` / `gateway_errors` are written to `companion_affinity_events` for
engine-side debugging only, and never surface on the wire. Do not "fix" this by deferring the
`final` frame or by adding a post-turn event — the exclusion is the design.

**Vision (`chat_vision`) is likewise absent**, but for a different reason than an earlier
draft of this spec claimed. There is no image-upload request: `image_url` is a body field on
`POST /comp/chat/{session_id}/message/stream`, and `run_vision` executes *inside* the chat
turn's generator, ahead of the input filter. The describe is a **fail-open pre-stage** — an
exhausted chain does not fail the turn, it keeps it text-only and a placeholder covers the
undescribed image, so the consumer still receives a normal reply. Its `llm_attempts` /
`gateway_errors` land in `chat_vision_events` for engine-side debugging and are never folded
into the turn's accumulated list.

> **Open follow-up, deliberately not settled here.** Unlike affinity eval, a failed describe
> means the companion never saw the user's photo, and whether the consumer should be told
> that is a genuine product question — one this spec's original (incorrect) reasoning
> concealed rather than answered. **The exclusion holds as implemented**; revisiting it is a
> separate decision with its own design pass, not a late amendment to this one.

### 6.2 SSE `error` frame

`code` is retained for compatibility. Two fields are added:

```rust
Error {
    code: StreamErrorCode,
    retryable: bool,
    message: String,
    user_message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_code: Option<String>,
}
```

`StreamErrorCode`'s two dead variants come alive, derived from the failure rather than
hardcoded: `429` → `RateLimited`, an `gateway_errors` entry whose `kind` ends in `_timeout` →
`Timeout`, any other upstream status → `UpstreamUnavailable`, everything else → `Internal`.

### 6.3 Non-streaming endpoints

```rust
AppError::Upstream(Box<UpstreamAttempt>)   // was Upstream(String)
```

`IntoResponse` emits the upstream's own status verbatim. `StatusCode::from_u16` accepts
100–999, so 529 goes out as 529. When the failure has no status (an `gateway_errors` case),
a timeout maps to 504 and everything else to 502.

`response_status_for` bounds the passthrough at `>= 400`, by range and not by allow-list.
A `Provider` failure records `http_status: 200` — true to what the provider answered —
and forwarding a 2xx/3xx would report the failure as a success. Those synthesise 502; the
recorded value is untouched, since `llm_attempts` owns the fact and the status line owns
the verdict.

`Retry-After` is forwarded verbatim when the upstream sent one.

```json
{
  "error": "upstream",
  "message": "upstream failure: code=529: Overloaded",
  "upstream_status": 529,
  "provider_code": "529"
}
```

**This is the breaking change of the release.** A consumer branching on `status === 502` to
detect provider failure will now miss 529, 429 and 503.

---

## §7 Retired vocabulary

With `llm_attempts` and `gateway_errors` in place, the transport-layer values that were
shoehorned into business markers are redundant, and several of them were never precise
(`open_error` covers status, transport, decode and config failures alike).

Every coarse vocabulary gains the same two **pointer values** — `upstream_error` and
`gateway_error` — which say only "an attempt failed; the detail is in that column".

| Marker | Retired | Added |
| --- | --- | --- |
| `attempt_outcome` (tracing, `stream.rs:390`) | `open_error`, `open_timeout`, `total_timeout`, `idle_timeout`, `chunk_error`, `error_frame` | `upstream_error`, `gateway_error` |
| `filter_attempts[].reason` (`chat_messages.metadata`) | `error`, `timeout` | `upstream_error`, `gateway_error` |
| `last_failure` (`chat_vision_events`) | `model_error`, `timeout` | `upstream_error`, `gateway_error` |
| `last_failure` (`chat_images_events`) | `model_error`, `timeout`, `stream_open_failed`, `stream_died_midway` | `upstream_error`, `gateway_error` |
| `companion_decision_events.status` | `error`, `timeout` (stop writing; CHECK keeps them for historical rows) | `upstream_error`, `gateway_error` |
| `context.eval_skip_reason` (`companion_affinity_events`) | `eval_error`, `eval_timeout` | nothing |

What survives in each vocabulary is exactly the content-level verdicts: `served`, `length`,
`content_filter`, `empty`, `garbled`, `refusal_pattern`, `too_short`, `unparseable`,
`empty_prompt`, `blank_description`, `parse_error`.

**`stream_open_failed` / `stream_died_midway` were found during implementation**, in the
compose endpoint's streaming mode (`routes/persona.rs`) — a path the original audit did not
reach. They belong to exactly the class this section retires: one label each covering a
provider status *and* a local timeout, transport facts sitting in a content vocabulary. Both
are retired with the other two, so `chat_images_events.last_failure` reads the same whichever
endpoint wrote the row.

`eval_skip_reason` gets no pointer values, because its contract is "why no call was made",
and a failed call is not a skip. **Four values remain reachable in a persisted row**, and all
four are genuine intentional skips: `proactive`, `short_user_msg`, `empty_assistant` (from
`eval_skip_reason`, `post_process.rs`) and `eval_no_generation_id` (from `meta_skip_reason` —
a *successful* eval whose response carried no join key). `eval_skip_reason`'s two other arms,
`ghost` and `product_qa`, exist only for `match` exhaustiveness and are never persisted: a
Ghost turn takes the `record_ghost` path, which ignores `context` entirely, and a
`product_qa` turn is filtered out by `persist_affinity` before the helper is called. The
invariant it upholds is restated: **a `companion_affinity_events` row with a NULL
`generation_id` is always explained — by an `eval_skip_reason` (no call attempted) or by a
non-empty `llm_attempts` / `gateway_errors` (a call attempted and failed).**

`companion_decision_events.status` is `NOT NULL` with a `CHECK`, so its CHECK widens to
`('ok','empty','parse_error','timeout','error','upstream_error','gateway_error')`. `timeout`
and `error` stay legal so historical rows remain valid; the engine stops writing them. No
backfill — rewriting production audit history to gain vocabulary uniformity is not worth it,
and the migration guide documents the cutover date instead.

`filter_attempts` also changes when it is written: today it appears only on fail-open (the
whole filter chain exhausted). It will be written whenever **any** filter attempt failed,
including a chain that recovered — the same rule the chat chain follows.
`metadata.filter_outcome = "fail_open"` remains the authority on whether the chain as a whole
succeeded.

---

## §8 Testing

The one test that defines "done":

> wiremock returns **529** for the primary model, the second model succeeds, the turn is
> served normally, and `chat_messages.llm_attempts[0].http_status == 529`.

Around it:

**`eros-engine-llm`**
- HTTP classification for **unknown** codes: 570 and 599 classify as upstream, 418 as client.
  No enumerated allow-list anywhere in the assertion or the implementation.
- `parse_error_body` over all four body shapes, including a bare `{"error": "..."}` and
  non-JSON junk.
- `Display` output is byte-identical to today's `scrub_error_body` for the three retained
  scrubbing tests, and `flagged_input` still never leaks.
- Mid-stream `{"error": {...}}` produces an `llm_attempts` entry with `http_status: 200` and
  the provider code intact.
- `execute` returns `ChatResponse.failures` populated when a fallback recovered.
- `AttemptFailure::from_llm_error` is exhaustive over `LlmError`.

**`eros-engine-server`**
- `AppError::Upstream` with a 529 renders HTTP 529; with a timeout renders 504.
- `Retry-After` is forwarded.
- `final` frame serialisation with and without failures; the empty case omits both keys.
- `StreamErrorCode` derivation: 429 → `RateLimited`, `open_timeout` → `Timeout`.
- A pseudo-ghost turn still emits its phrase **and** carries `llm_attempts` on `final`.
- An input-filter chain that fails and then recovers writes `llm_attempts` with
  `task: "chat_input_filter"` onto the `role='user'` row, and leaves the assistant row's
  columns untouched.

**`eros-engine-store`**
- Round-trip both columns on all five tables.
- `companion_decision_events` accepts the two new `status` values and still accepts the
  historical ones.

---

## §9 Documentation and rollout

| File | Change |
| --- | --- |
| `docs/migrating/llm-error-audit-v1-4-0.md` | New. English only, per `docs/migrating/README.md` |
| `docs/llm-audit.md` + `.zh.md` | New columns, both shapes, the three-home split, retired vocabulary |
| `docs/api-reference.md` + `.zh.md` | `final` frame fields, `error` frame fields, non-streaming error body and status passthrough |

The migration guide must lead with the two things that break a consumer that does nothing:

1. Non-streaming provider failures no longer always return 502.
2. `stream_metrics` dashboards keyed on `outcome = "open_error"` / `"chunk_error"` /
   `"idle_timeout"` go to zero; those attempts now report `upstream_error` / `gateway_error`,
   with the detail in the new columns.

**Rollout.** Migration `0050` is additive and nullable, so it is safe to apply ahead of the
engine deploy. There is no ordering constraint and no reverse-migration hazard: rolling the
engine back leaves two unread columns.

---

## §10 Inventory: every failure-recording site today, and what it becomes

Business-layer markers keep their table's own dialect. Only the two structured columns are
uniform across tables.

### 10.1 `engine.chat_messages`

| Site | Today | After |
| --- | --- | --- |
| `metadata.fallback_reason` | `stream_failure` \| `garble_repaired` | Unchanged. Business verdict: the chain died / a garble was salvaged |
| `metadata.retries_chat` | int, written only on the two paths above | Unchanged. Not derivable from the new columns — recovered hops and content-verdict hops leave no entry |
| `metadata.filter_outcome` | `fail_open` | Unchanged. Authority on whether the filter chain as a whole succeeded |
| `metadata.filter_attempts[].reason` | `error`, `timeout`, `empty`, `content_filter`, `refusal_pattern`, `too_short` | `error`/`timeout` retired → `upstream_error`/`gateway_error`. Now written whenever any filter attempt failed, not only on fail-open |
| `truncated` | bool | Unchanged |
| `model` / `usage` / `generation_id` | NULL on failure paths | Unchanged |
| `role = 'system_error'` | Permitted by `CHECK`, never written | Unchanged (§11) |
| Input filter failures (user row) | **Nothing at all.** `warn!` then `continue` / `None` (`stream.rs:2478-2484`) | **new** `llm_attempts` / `gateway_errors` on the `role='user'` row, `task: "chat_input_filter"` |
| `pre_filter_content` / `filter_model` / `filter_triggers` / `f_generation_id` (user row) | Written only when a rewrite succeeded | Unchanged |
| — | — | **new** `llm_attempts`, `gateway_errors` |

### 10.2 `engine.chat_vision_events` / `engine.chat_images_events`

| Site | Today | After |
| --- | --- | --- |
| `status` | `ok` \| `exhausted` \| `not_configured` | Unchanged |
| `attempts` | SMALLINT count of hops | Unchanged. Counts every hop; the new columns hold only failed ones |
| `last_failure` | `model_error`, `timeout`, `empty`, `unparseable`, `empty_prompt`, `content_filter`, `blank_description`, `refusal_pattern`, plus `stream_open_failed`, `stream_died_midway` on `chat_images_events` only | `model_error`/`timeout` retired → `upstream_error`/`gateway_error`, and with them `stream_open_failed`/`stream_died_midway` (§7); the six content verdicts unchanged |
| — | — | **new** `llm_attempts`, `gateway_errors` |

### 10.3 `engine.companion_decision_events`

| Site | Today | After |
| --- | --- | --- |
| `status` | `ok` \| `empty` \| `parse_error` \| `timeout` \| `error` | `timeout`/`error` no longer written → `upstream_error`/`gateway_error`. `CHECK` widens and keeps the old two for historical rows |
| `payload` | NULL on failure | Unchanged |
| No row when the judge did not run (tip turn, feature off) | — | Unchanged |
| — | — | **new** `llm_attempts`, `gateway_errors` |

### 10.4 `engine.companion_affinity_events`

| Site | Today | After |
| --- | --- | --- |
| `context.eval_skip_reason` | 6 values reachable in a row | `eval_error`/`eval_timeout` retired, **no** pointer values added. The remaining 4 — `proactive`, `short_user_msg`, `empty_assistant`, `eval_no_generation_id` — are genuine intentional skips (§7) |
| `model` / `generation_id` NULL | Always paired with a skip reason | Invariant restated: a NULL `generation_id` is explained by a skip reason **or** by a non-empty `llm_attempts` / `gateway_errors` |
| — | — | **new** `llm_attempts`, `gateway_errors` |

### 10.5 Tracing

| Site | Today | After |
| --- | --- | --- |
| `stream_metrics.outcome` | `served`, `open_error`, `open_timeout`, `total_timeout`, `idle_timeout`, `chunk_error`, `error_frame`, `length`, `content_filter`, `empty`, `garbled` | Six transport values retired → `upstream_error`, `gateway_error`. Five content verdicts survive |
| `tracing::warn!(error = %e)` | Unstructured `Display` | Unchanged, and now redundant with the columns rather than the only record |

### 10.6 Left alone

`character_insights_events` / `companion_insights_events` (`status`: `ok`/`empty`/`parse_error`,
`payload = {"raw": …}` on parse error, **no row at all** on a transport failure),
`companion_memories`, and the eight world / persona-story tables. All write on success only.
Giving them an audit trail is separate work (§11).

---

## §11 Out of scope

- The 10 insight / memory / world tables that write no row at all on failure. Giving them an
  audit trail is a separate, larger piece of work.
- Status-aware retry, backoff, or `Retry-After` honouring inside the engine.
- `chat_messages.role = 'system_error'`, a value the `CHECK` has permitted since migration
  `0001` and which has never been written. Removing it is unrelated cleanup.
