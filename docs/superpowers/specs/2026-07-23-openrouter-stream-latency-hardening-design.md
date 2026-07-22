# eros-engine — OpenRouter stream hardening: correctness, timeouts, TTFT

Driven by an external static review plus an independent code audit of
`crates/eros-engine-llm/src/openrouter.rs` and the chat streaming pipeline
(`crates/eros-engine-server/src/pipeline/stream.rs`). Four findings dominate,
ordered by real-world impact:

1. **Mid-stream OpenRouter error frames are silently swallowed** — a partial
   reply persists as a complete non-truncated success, fallback never runs,
   and the `content_filter` safety signal is dropped on the streaming path
   (the non-streaming path gates on it; production chat is 100% streaming).
2. **No timeout anywhere on the chat generation path** — a provider that
   accepts the connection and stalls hangs the turn forever, holding one of
   the user's 3 concurrent stream slots; `StreamErrorCode::Timeout` is
   documented as "not yet implemented".
3. **`reqwest` is compiled without the `http2` feature** — confirmed via
   `cargo tree -i h2 --edges normal,build` (no `h2` in the production graph;
   the lockfile `h2` comes from dev-only wiremock feature-unification). Every
   concurrent SSE stream burns its own TCP+TLS connection.
4. **`output_regex` forces buffered mode chain-wide** — with the real
   downstream config, every `chat_companion` model appears in a rule's
   `models` list, so **every chat turn buffers and TTFT equals full
   generation time**. The actual production patterns are streaming-safe with
   a small bounded holdback.

Work is split into four batches, landed in order:

- **Batch A** (correctness + safety net): A1 error frames, A2 timeouts, A3 http2.
- **Batch B** (observability): TTFT/latency structured logging, header
  generation-id capture — lands before C so C's win is measurable.
- **Batch C** (the TTFT lever): streaming-safe output_regex; buffered mode
  gated on the LLM output_filter only.
- **Batch D** (tuning, opt-in): borrow instead of clone per fallback attempt;
  provider routing `sort` knob.

---

## 0. Decisions (settled during review)

- **Local fallback chain stays; OpenRouter native `models: [...]` array is
  rejected.** Native failover would collapse the chain into one request but
  loses per-attempt audit rows (a deliberate design — one `AssistantInsert`
  per attempt) and the business-semantic gates OpenRouter cannot perform:
  empty-completion ghost fallback, byte-BPE garble repair, length/error
  truncation. Not worth it.
- **No SSE parser rewrite.** `eventsource-stream` correctly swallows
  `: OPENROUTER PROCESSING` comment keepalives and `[DONE]`; parsing cost is
  noise next to generation time.
- **No `stream_options: {include_usage: true}`.** Usage/cost reconciliation
  goes through OpenRouter's own logs by design (the audit records
  `generation_id` as the join key); in-band streaming usage stays best-effort.
- **Timeout values are module consts, not config.** Follows the existing
  `FILTER_TIMEOUT` precedent (`stream.rs`). No new config surface until a
  downstream deployment actually needs to tune them.
- **The regex streaming applier governs the wire only; persistence always
  re-runs the existing whole-text `apply_output_regex`.** The DB row and the
  regex audit are byte-identical to today's buffered behavior in every case;
  only what streams to the client is scrubbed incrementally. In the one
  pathological fail-open case (unclosed `[` span exceeding the holdback cap)
  the wire may briefly show what the persisted row stripped — accepted, the
  cap is far above the real artifact length.
- **Provider routing exposes `sort` only, boot-level, off by default.**
  Mirrors the existing `ignore_providers` plumbing. `preferred_max_latency`
  is NOT adopted until verified against current OpenRouter docs (the external
  review's citation is unconfirmed). Setting `sort` disables OpenRouter's
  price-based load balancing — a cost decision that belongs to the deployer,
  hence opt-in with no default.

---

## 1. Batch A1 — mid-stream error frames (correctness)

OpenRouter signals a mid-stream provider failure on an HTTP-200 SSE stream as
a data frame carrying a top-level `error` object, typically alongside
`choices: [{delta: {...}, finish_reason: "error"}]`. Today
`WireStreamFrame` (`openrouter.rs`) has no `error` field, serde ignores it,
and the frame becomes an all-`None` `DeltaChunk`; the pipeline only treats
`finish_reason == "length"` as truncation (`stream.rs` live/filtered/QA
loops), so the attempt ends as a *success* with partial text.

### 1.1 Wire layer (`openrouter.rs`)

```rust
#[derive(Debug, Deserialize)]
struct WireStreamError {
    #[serde(default)]
    code: Option<serde_json::Value>, // int or string upstream; opaque here
    #[serde(default)]
    message: String,
}
```

- Add `error: Option<WireStreamError>` to `WireStreamFrame`.
- In `execute_stream`'s `filter_map`: when `frame.error` is `Some`, emit
  `Err(LlmError::Provider(format!("openrouter mid-stream error: {code:?}: {message}")))`
  **instead of** a `DeltaChunk`. The existing consumer `Err` arms already set
  `truncated = true` and advance the chain — no new pipeline plumbing for
  this case.
- When a choice carries `finish_reason: "error"` *without* a top-level error
  object, also emit `Err(LlmError::Provider("finish_reason=error"))` — same
  treatment.

### 1.2 Pipeline layer (`stream.rs`)

- `finish_reason == "content_filter"` mid-stream (Gemini/OpenAI safety cut):
  treat exactly like `"length"` — set `truncated = true` — in all three
  consume loops (live burst, filtered burst, product-QA). This restores the
  parity the non-streaming path already has via `filter_output_invalidity`.
- No metadata/schema change; the existing `truncated` flag + pseudo-ghost /
  chain-advance machinery carries the behavior.

### 1.3 Why not handle `content_filter` in the client layer

`content_filter` is not a transport failure — the text up to the cut is
valid, and `LlmError::Garbled`-style salvage semantics don't apply. The
pipeline owns the "is this reply acceptable" decision (mirrors the sync
path's validity gates), so the client keeps passing `finish_reason` through
verbatim.

## 2. Batch A2 — timeouts (bound the hang)

Three layers, all constants in the crate that owns them:

### 2.1 reqwest client (`openrouter.rs`, both constructors)

```rust
.connect_timeout(Duration::from_secs(5))
.pool_idle_timeout(Duration::from_secs(300))
```

No global `.timeout()` and no client-level `.read_timeout()`: both would
also bound non-streaming calls (image generation legitimately spends its
whole wall-time before the first body byte).

### 2.2 SSE byte-level idle timeout (`openrouter.rs::execute_stream`)

Insert an idle-gap watchdog on the **bytes** stream, *before*
`.eventsource()`:

```rust
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
```

Implemented with `tokio_stream::StreamExt::timeout` (or an equivalent
hand-rolled poll wrapper if adding `tokio-stream` is unwanted); an elapsed
gap maps to `Err(LlmError::Stream("idle timeout: no bytes for 45s"))`.
Byte-level placement is deliberate: OpenRouter's `: OPENROUTER PROCESSING`
comment keepalives count as bytes and reset the timer (so a reasoning model
thinking for minutes stays alive), while a dead peer trips it.

### 2.3 Per-attempt open + total caps (`stream.rs`, all three consume loops)

```rust
const STREAM_OPEN_TIMEOUT:  Duration = Duration::from_secs(20);  // headers
const STREAM_TOTAL_TIMEOUT: Duration = Duration::from_secs(120); // spec §1.5
```

- Wrap `state.openrouter.execute_stream(...)` in
  `tokio::time::timeout(STREAM_OPEN_TIMEOUT, ...)`; timeout ⇒ same handling
  as the existing open-`Err` arm (`truncated = true`, next model).
- Take `let deadline = tokio::time::Instant::now() + STREAM_TOTAL_TIMEOUT;`
  per attempt and replace `s.next().await` with
  `tokio::time::timeout_at(deadline, s.next()).await`; elapsed ⇒ same
  handling as the existing chunk-`Err` arm.
- Timeouts are *attempt-level* failures that ride the existing fallback /
  pseudo-ghost machinery. `StreamErrorCode::Timeout` stays reserved — no new
  wire error is emitted (chain exhaustion still surfaces as today's
  pseudo-ghost or `UpstreamUnavailable`).

## 3. Batch A3 — enable HTTP/2

`Cargo.toml` workspace dep gains the feature:

```toml
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls", "stream", "http2"] }
```

ALPN negotiates h2 automatically against OpenRouter; HTTP/1.1 remains for
servers that don't offer h2 (wiremock tests are unaffected). No builder
changes (`http2_adaptive_window` deferred until B-metrics show window
stalls). Acceptance: `cargo tree -i h2 -e normal` now shows a
`reqwest → hyper` edge in the production graph; after deploy, Batch B's
`http_version` field should read `HTTP/2.0`.

## 4. Batch B — latency observability

No metrics crate exists in-tree; everything is structured `tracing` fields
(downstream scrapes logs). One `info!` event per attempt, one `debug!` in
the client:

### 4.1 Client (`openrouter.rs::execute_stream`)

- Around `send()`: record `headers_ms` (request-start → response-headers) and
  `http_version = ?resp.version()`; emit as
  `tracing::debug!(target: "openrouter_stream", model, headers_ms, ?http_version, "stream opened")`.
- Capture the **`X-Generation-Id` response header** the moment headers
  arrive. If present, prepend it to the delta stream as a synthetic first
  chunk (`futures_util::stream::once(... DeltaChunk { generation_id, ..Default::default() })`
  chained before the SSE stream). The pipeline's existing
  "latest non-None wins" accumulation picks it up; a body `id` later
  overwrites with the same value. This closes the "stream died before its
  first id-bearing chunk ⇒ no generation handle for audit reconciliation"
  gap.

### 4.2 Pipeline (`stream.rs`, per attempt)

One structured event per attempt in each consume loop:

```rust
tracing::info!(
    target: "stream_metrics",
    model = %model_id,
    attempt = idx,
    ttft_ms,            // execute_stream call → first content delta; None if none arrived
    total_ms,           // execute_stream call → stream end
    outcome,            // "served" | "length" | "content_filter" | "error_frame"
                        //   | "open_error" | "chunk_error" | "open_timeout"
                        //   | "idle_timeout" | "total_timeout" | "empty" | "garbled"
    "chat stream attempt"
);
```

`outcome` is a `&'static str` assigned where each condition is detected
(the existing `warn!` lines stay; this event is the aggregate-friendly one).
The live burst also carries `ttft_ms` as true time-to-client; the filtered
burst carries it as time-to-first-*upstream*-token (the client sees nothing
until the rewrite completes) and tags the event `filtered = true`. Scope: the
event is emitted in the two **companion** bursts (live + filtered) — the
TTFT-critical paths and Batch C's optimization target. The out-of-character
**product-QA** executor (a low-volume path Batch C does not touch, with a
labeled-`continue`/`break` chain walk) keeps its existing per-attempt `warn`
logs rather than a fragile multi-exit-point structured emit.

### 4.3 Error-body bounding & redaction (riders folded into B)

Two independent investigations of the reference SDK
`realmorrisliu/openrouter-rs` (its `ApiErrorContext` / `is_retryable` /
`ApiErrorKind` normalization) concluded **PARTIAL**: eros-engine's pipeline is
*uniform-advance* — the server crate holds **zero** `LlmError` references and
consumes errors only as a `Display` string in `tracing` logs, so a normalized
error taxonomy nobody branches on is dead weight, and wiring moderation as a
"terminal" kind would actively *regress* companion fallback (a laxer model
down the chain is exactly what you want). Rejected: `ApiErrorContext`,
`is_retryable`, `ApiErrorKind` control flow, `api_code`, `x-request-id`
header, `normalize_error_status`, `merge_top_level_metadata`. Adopted here is
only the subset that closes a **real privacy defect** or a small correctness
gap. All changes are inside `eros-engine-llm`; `LlmError::Status`'s
`(StatusCode, String)` shape is unchanged (its existing tests match on status
only), so nothing in the server crate moves.

**The defect:** `LlmError::Status`'s `Display` is `"non-success status {0}:
{1}"` where `{1}` is the **full, unbounded provider body**, logged verbatim on
the chain-advance path (`openrouter.rs` `execute` warn lines; `stream.rs`
`"upstream open err: {e}"`). OpenRouter's *moderation* error body carries
`metadata.flagged_input` — an excerpt of the user's flagged message — so today
a slice of raw user chat content lands in ordinary logs, unbounded. Direct
violation of the "never echo NSFW content" rule.

- **B-err1 — bound + scrub at the `LlmError::Status` construction sites**
  (`call_once`, `execute_stream`, `execute_vision`). New helpers in
  `openrouter.rs`:
  - `body_preview(&str) -> String`: flatten `\r`/`\n`, cap `ERROR_PREVIEW_MAX
    = 200` chars with an ellipsis marker.
  - `scrub_error_body(raw) -> String`: best-effort parse the OpenRouter
    `{"error":{code,message,metadata}}` envelope; keep `code` (as
    `serde_json::Value` — satisfies the non-i64 requirement, and is *better*
    than the reference's `Option<i64>`), a `body_preview`'d `message`, and —
    from moderation metadata — only `provider_name` + `reasons`, **never
    `flagged_input`**. Non-envelope bodies fall back to `body_preview(raw)`.
    Store the scrubbed string in `Status`, so every downstream `%e`/`{e}` log
    is bounded and redacted with no log-site edits.
- **B-err2 — close the non-streaming `finish_reason == "error"` gap.** Batch A
  fixed only the streaming path; `call_once` still returns `Ok` with
  `finish_reason: Some("error")`, so non-stream callers (PDE / output-filter /
  affinity / world) accept a partial reply from a mid-generation provider
  death. Add: `finish_reason == "error"` ⇒ `Err(LlmError::Provider(..))` so
  `execute`'s chain advances (mirror of the Batch A stream fix), placed before
  the garble check.
- **B-err3 — surface an embedded error on a 200-decode failure.** A 200 body
  that is actually `{"error":...}` with no `choices` currently fails as
  `Decode("missing field choices")`, discarding the provider's message. In
  `call_once` and `execute_vision`, read the body as text, `from_str`; on
  parse failure route through `decode_or_api_error(status, body, err)`: if the
  body contains a top-level `error` object ⇒ `Provider(scrub_error_body(body))`
  (chain advances with a useful, redacted reason); otherwise the existing
  `Decode(err)` (its `Display` carries only a serde offset, not the body — no
  leak, no new variant).

Skipped from the reference (no consumer / would regress): the whole
`ApiErrorContext` struct, `is_retryable`/`is_client_error`/`is_server_error`,
`ApiErrorKind` as control flow, `x-request-id` capture (different key from the
audit's `generation_id`, which §4.1 already captures), `normalize_error_status`,
and moderation-as-terminal.

## 5. Batch C — streaming-safe `output_regex` (the TTFT lever)

### 5.1 Problem shape

`filtered_mode = llm_filter_arms || regex_targets_chain` (`stream.rs`)
buffers the whole reply whenever *any* chain model has *any* regex rule. The
real downstream config's bracket rule lists every production chat model, so
live mode is dead code in production. The active patterns:

| pattern | anchor | streaming strategy |
|---|---|---|
| `\[[^\]]*\]` | anywhere | span holdback: emit freely until `[`, hold from `[` until `]` (strip span) or 256-char cap (flush, fail-open) |
| `^嗯(?:\.{3,6}\|…{1,2})\s*` | prefix | head holdback: buffer first 64 chars, apply once, then passthrough |
| `^(?:[(（][^)）]*[)）]\|\.{3,6}\s*\|…{1,2}\s*)` | prefix | same head holdback |

### 5.2 `StreamScrubber` (new, `eros-engine-llm`)

```rust
pub struct StreamScrubber { /* rules for this model, head/span state */ }

impl StreamScrubber {
    /// Rules pre-filtered to `model_id`. Empty rules ⇒ pure passthrough.
    pub fn new(rules: &[CompiledRegexRule], model_id: &str) -> Self;
    /// Feed a delta; returns the text now safe to emit (possibly empty).
    pub fn push(&mut self, delta: &str) -> String;
    /// Stream ended; flush and clean whatever is still held.
    pub fn finish(&mut self) -> String;
}
```

- **Classification** (at `Self::new`, via `regex-syntax`'s HIR — already a
  transitive dep of `regex`): a rule whose HIR is start-anchored
  (`look_set_prefix` contains `Look::Start`) is a **head rule**; a rule whose
  possible matches all start with a literal `[` (prefix-literal extraction)
  is a **span rule** with open `[` / close `]`. Any other shape makes the
  scrubber **degenerate to full buffering** for this attempt (emit nothing
  from `push`, everything from `finish` after a whole-text
  `apply_output_regex`) — correct for arbitrary rules, just without the TTFT
  win; downstream deployments with exotic patterns lose nothing vs today.
- **Head phase:** hold the first `HEAD_HOLDBACK = 64` chars; once exceeded
  (or at `finish`), apply the head rules to the held prefix once, emit, and
  leave head phase permanently.
- **Span phase (steady state):** emit passthrough until an un-emitted `[`
  appears; hold from there; on `]`, run the span rule over the held segment,
  emit the cleaned remainder; if `SPAN_HOLDBACK_CAP = 256` chars accumulate
  with no `]`, flush held text verbatim (fail-open) and resume scanning
  *after* it.
- Char-boundary safe (operates on `char` indices, never splits a CJK
  scalar); a marker straddling any chunk boundary must be stripped
  identically to the whole-text result (test matrix below).

### 5.3 Pipeline rewiring (`stream.rs`)

- `filtered_mode = llm_filter_arms;` — regex alone no longer buffers.
- Live burst, per attempt: build `StreamScrubber::new(&state.output_regex, model_id)`;
  each content delta is `let emit = scrubber.push(&content);` — `acc` still
  accumulates the **raw** text; `ProtocolFrame::Delta` carries the scrubbed
  `emit` (skip the frame when empty). After the loop, `scrubber.finish()`
  emits the tail.
- Persist path (unchanged semantics, new location): before the
  `AssistantInsert`, run the existing whole-text
  `apply_output_regex(&state.output_regex, model_id, &acc)`; persist
  `cleaned`, carry the regex audit exactly as the filtered path does today
  (raw on the audit field when rules matched), and port the
  **strip-to-empty ⇒ ghost-fallback** branch (`ghost_fallback_metadata`
  with the regex reason) into the live burst. The whitespace-only-deltas
  edge (raw `"\n\n[...]"` streams two blank deltas, then strips to empty ⇒
  ghost) is accepted — clients render nothing for whitespace.
- Filtered burst (LLM output_filter armed): completely unchanged — it
  already buffers for the rewrite and applies regex on the buffered text.
- Beta-tier turns (output_filter trigger passing) therefore still buffer —
  inherent to an LLM rewrite; default/free-tier turns get true streaming.

### 5.4 Invariants preserved (test-gated)

1. Persisted content, regex audit rows, and ghost/suppression decisions are
   byte-identical to today's buffered mode for every input (modulo the
   documented span-cap fail-open, which affects the wire only).
2. Concatenated emitted deltas == `apply_output_regex(raw)` for all
   classifiable rules, across arbitrary chunk splits.
3. A reply that strips to empty emits no non-whitespace delta.

## 6. Batch D — tuning

### 6.1 D1: stop cloning `ChatRequest` per fallback attempt

New borrowing entry point; the owned-`req` version stays for API compat
(published crate):

```rust
/// Open a stream for one specific model, borrowing the shared request.
pub async fn execute_stream_as(&self, req: &ChatRequest, model: &str)
    -> Result<DeltaStream, LlmError>;

pub async fn execute_stream(&self, req: ChatRequest) -> Result<DeltaStream, LlmError> {
    let model = req.model.clone();
    self.execute_stream_as(&req, &model).await
}
```

`WireRequest` already borrows everything, so `execute_stream_as` just feeds
`model` into the wire body (and sends no fallback array — same as today's
cleared `fallback_model`). The three burst loops switch to
`execute_stream_as(&req, model_id)` and drop the per-attempt
`req.clone()` / field surgery.

### 6.2 D2: provider routing `sort` (opt-in)

- `ProviderPrefs` gains `#[serde(skip_serializing_if = "Option::is_none")] sort: Option<&'a str>`.
- Plumbed like `ignore_providers`: a boot-time
  `OpenRouterClient::with_provider_sort(Option<String>)` consuming builder,
  sourced from the same engine config that supplies the exclusion list;
  absent ⇒ wire body byte-identical to today.
- Accepted values passed through verbatim (`"latency"` / `"throughput"` /
  `"price"`); OpenRouter validates. Documented tradeoff: any explicit sort
  disables OpenRouter's default price load-balancing.
- Field name/shape verified against the live provider-routing docs at
  implementation time; if the API differs, adjust to the documented schema
  rather than this sketch.

## 7. Testing

- **A1 (wiremock SSE):** mid-stream frame with top-level `error` after two
  content deltas ⇒ attempt fails, chain advances, partial text is not
  persisted as success; `finish_reason: "error"` without error object ⇒
  same; `finish_reason: "content_filter"` ⇒ `truncated`, chain advances;
  last-attempt versions land on the pseudo-ghost path.
- **A2 (wiremock delays):** response headers delayed past
  `STREAM_OPEN_TIMEOUT` ⇒ open-timeout outcome, next model; a stream that
  sends one delta then stalls past the idle window ⇒ idle-timeout, next
  model (wiremock supports chunk delays via `set_body_raw` +
  `set_delay`-style helpers; if per-chunk stalls prove un-mockable, the
  idle watchdog gets a direct unit test around a hand-built
  `futures::stream` instead). Existing suites stay green (no global
  timeout regressions on slow-but-healthy mocks).
- **A3:** compile-time only (`cargo tree` acceptance above); wiremock tests
  keep negotiating HTTP/1.1 and must stay green.
- **B:** unit-test the synthetic first chunk (header id present ⇒ first
  `DeltaChunk.generation_id` matches; absent ⇒ stream unchanged). Log
  events are not test-gated.
- **C (the big matrix):** `StreamScrubber` unit tests — every production
  pattern × chunk splits at every char boundary (property-style loop, not
  hand-picked splits) asserting invariant 5.4-2; artifact-only reply ⇒
  everything held, `finish` returns empty; span-cap overflow ⇒ fail-open
  flush; non-classifiable rule ⇒ degenerate buffering equals
  `apply_output_regex`. Burst-level (wiremock): regex model streams live
  (Delta frames arrive before `[DONE]`), marker stripped on the wire,
  persisted row + audit identical to the pre-change buffered run;
  strip-to-empty ⇒ ghost fallback frames.
- **D1:** existing fallback-chain tests pass against `execute_stream_as`;
  wire-body assertion that the fallback attempt sends the fallback model id.
- **D2:** wire body omits `provider.sort` when unset (byte-identical guard);
  includes it when configured; composes with a non-empty `ignore` list.

## 8. Out of scope

- OpenRouter native `models: [...]` request-level fallback (rejected, §0).
- SSE parser replacement or byte-oriented reparse (verified fine).
- `stream_options.include_usage` (usage reconciliation stays in OpenRouter
  logs).
- Per-task provider preferences, `preferred_max_latency` / `order` routing
  knobs (YAGNI until a deployment asks; `sort` covers the latency case).
- Any change to image-gen / vision / non-streaming task timeouts beyond the
  shared `connect_timeout` (they keep their existing `FILTER_TIMEOUT`
  wrappers or deliberate unboundedness).
- Server-side axum `TimeoutLayer` (the SSE route is long-lived by design;
  per-attempt caps above bound the real failure mode).
