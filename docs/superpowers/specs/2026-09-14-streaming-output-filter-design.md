# Streaming output filter — design

**Date:** 2026-09-14
**Scope:** `crates/eros-engine-server/src/pipeline/stream.rs` (FILTERED mode of
`drive_chat_burst`, `run_output_filter`); docs. No schema change, no wire-frame
vocabulary change, no config change.

## Problem

When the LLM `output_filter` arms for a turn, the burst runs in FILTERED mode: the
chat generation is buffered (inherent — the filter needs the complete original as
input), then the filter LLM is called **non-streaming** (`execute`, 15s wall per
model), and the entire rewritten reply reaches the client as one `delta`. The user
stares at a typing indicator through both the chat generation and the whole rewrite,
then gets a text dump. The chat-generation half cannot stream; the filter's own
output can, and today doesn't.

## Goal

Stream the filter LLM's output to the client as it is produced, while keeping the
validity gate, the fallback-chain semantics, the fail-open contract, and the audit
trail behaviorally identical to the batch path wherever the client has not yet seen
text.

Non-goals: streaming the chat generation in filtered mode (impossible by
construction); segment-level pipelined filtering (loses global rewrite context,
multiplies cost); any change to LIVE mode, the voice path (`run_output_filter` has a
single call site, the chat burst), regex `output_regex` handling, `timing`/extract
semantics, or the wire frame vocabulary.

## Design

### Filter call becomes a stream

`run_output_filter` switches from `execute` to `execute_stream_as`, one filter model
at a time along the already-depth-capped chain. Chunk fields captured: `content`
(accumulates), `usage`, `generation_id`, `finish_reason`, and `model` — latest
non-`None` wins, as in the existing loops.

`finish_reason` handling does NOT mirror the chat loops' truncation rules; it
mirrors the batch gate, which is deliberately narrower:

| `finish_reason` on the filter stream | Disposition |
|---|---|
| `"content_filter"` | Attempt invalid (pre-emit: walk silently; post-emit: supersede) |
| `"length"` | **Served as-is** — the batch gate never rejected a length-cut rewrite |
| anything else / absent | Served |

Shape: `run_output_filter` cannot stay a plain `async fn` returning a string — it
becomes an event-yielding generator (its own `async_stream`, yielded through by the
burst): events for delta text, attempt failure (with whether it had emitted), served
outcome, and fail-open. The burst translates events to wire frames and owns message-
id minting, so wire framing stays in one place. The closest in-tree shape is the
persona candidate-draining chain walk in `routes/persona.rs`.

### Parity pre-processing: `clean_response ∘ trim`, incrementally

The batch path gates and serves `clean_response(raw.trim())`
(`eros-engine-llm/src/openrouter.rs`), not the raw completion: leading/trailing
trim, a leading ``` fence (with optional language tag) and — **when a leading fence
was present** — everything from the *last* ``` onward, then surrounding `"` quotes,
then surrounding `「`/`」`. The streaming path replicates this incrementally so the
gate input and the served text stay byte-identical to batch:

- **Leading strip (before any counting):** whitespace → the fence line if present →
  whitespace → the `"` run → the `「`/`」` run, in exactly `clean_response`'s order.
- **Trailing suspense:** a trailing run of whitespace (`char::is_whitespace`, what
  `str::trim` uses) or the quote characters (`"`, `「`, `」`) is held — neither
  counted nor emitted — until a character outside the set follows; end-of-stream
  resolves it by the batch tail rules (dropped/trimmed).
- **Fence-tail suspense:** when a leading fence was stripped, everything from the
  most recently seen ``` onward is additionally held; a later ``` releases the
  previously held region (batch keeps interior fences and cuts at the last one);
  end-of-stream drops the held region.

Counting, the emission threshold, and the head-scan all run on the post-leading-
strip, suspense-excluded text, in Unicode scalar values (Rust `chars()`) — the same
unit as `filter_output_invalidity`.

### Validity holdback

The gate's verdicts are front-loaded by construction (`refusal_in_head` scans the
first `REFUSAL_HEAD_SCAN_CHARS` = 120 chars; `MIN_FILTERED_OUTPUT_CHARS` = 80 <
120), so a bounded holdback makes every pre-emission verdict identical to batch:

- **Buffering phase.** Accumulate (under the parity pre-processing above) without
  emitting.
- **At ≥ 120 counted chars:** run the refusal head-scan exactly once, over exactly
  the first 120 counted chars (same truncated window as batch — a phrase straddling
  the boundary does not match, in batch or here).
  - Refusal → abort the attempt with zero client-visible artifacts and walk the
    chain, exactly as batch does.
  - Clean → flush the buffer as the first `delta`, then pass chunks through live
    (still subject to the suspense rules).
- **Stream ends under 120 counted chars:** the full text is in hand and equals the
  batch gate input; run today's whole gate unchanged (`empty`, `too_short`,
  short-refusal-verb, head refusal) and walk on any invalidity. Nothing was emitted.

Because emission requires ≥ 120 counted chars, an attempt that started emitting can
never later be `empty`, `too_short`, or a head refusal. The post-emission invalidity
surface is exactly: late `finish_reason == "content_filter"`, mid-stream transport
error, and deadline expiry.

### Message lifecycle and supersede

The first `meta` is still emitted before the filter runs, carrying the chat
attempt's message id; filter attempt 1's deltas ride it. When an attempt fails
**after** emitting, the wire sequence is:

```
done  {message_id: id_n, truncated: true, usage: null, generation_id: null}
meta  {message_id: id_{n+1}, continues_from: null}
```

then the next filter model re-runs from scratch on the same input with its own full
holdback; chain exhausted → the same closing `done`, then a fail-open `meta` + the
regex-cleaned original. `meta`↔`done` stays strictly 1:1 per message id (the
cardinality every existing mode has), and `continues_from` is always null on
filter-attempt metas: the superseded partial is never persisted, so there is no row
to stitch to (and `continues_from_message_id` carries an FK). The persisted row's id
is the finally-serving `meta`'s id. The served bubble's `done` carries the chat
attempt's usage and generation id, as today.

A live client renders the sequence exactly like a LIVE-mode fallback (truncated
bubble replaced by the next attempt's bubble); unlike LIVE mode the superseded
bubble is absent from history on reload, because it never persisted. Terminal
outcomes are exactly today's set: full filtered text, or fail-open original.

Attempt isolation is structural: the burst is one sequential generator, each
attempt's upstream stream is dropped before the next begins, and all per-attempt
state is local. `f_client_msg_id` remains shared across the filter chain's attempts
by design (it is the chain's idempotency id, unchanged).

### Deadline

One 15s deadline per filter attempt (`FILTER_TIMEOUT`, unchanged constant) from
attempt start, covering open through finish — the same wall budget the batch call
had. Pre-emit expiry walks the chain with no client-visible artifacts; post-emit
expiry lands in the supersede path. Client speed cannot stall the attempt: the burst
feeds a detached, unbounded frame channel (`routes/companion_stream.rs`), so yields
never block on the socket.

Keeping the batch budget also keeps the turn's outer envelope intact: the filter
phase still costs at most `FILTER_TIMEOUT × chain` (30s at the default depth) inside
the whole-turn `generation_timeout` (300s), exactly as today. A longer post-emit
allowance was considered and rejected: it lets a two-model emitting chain add
minutes, within reach of the outer timeout — whose expiry drops the burst
mid-flight, persists nothing, and fails the queue row — a terminal state the batch
path can never produce.

### Stream-end semantics

A clean end-of-stream (no error item) is treated as completion regardless of whether
a `finish_reason` arrived — the engine's established streaming discipline (LIVE mode
does exactly this). This is a deliberate divergence from the batch HTTP framing,
where a connection cut mid-body surfaced as a parse error: a provider that closes an
SSE stream cleanly mid-generation is indistinguishable from success here, exactly as
it already is for LIVE-mode chat.

### The `final` frame

`final.filtered` is documented as "the client received non-raw output this turn"
(`model-config.md`), so it is set at the filter's **first emitted delta**, not on
filter success: a post-emit fail-open turn showed the user rewritten text before the
supersede, and reports `filtered: true`. Pre-emit fail-open (nothing shown) stays
`false` (unless the regex strip set it, as today). `retries_filter` semantics are
unchanged.

### Audit

Unchanged: `pre_filter_content` (raw original), `filter_triggers`,
`f_client_msg_id`, `filter_outcome` on fail-open, and the `filter_attempts[].reason`
vocabulary.

`filter_model` keeps batch provenance: the response-echoed model when the stream
carries one — passed through the same direct-endpoint `@provider` escaping the
non-streaming response builder applies — else the requested slug. (The raw stream
chunk's `model` echo is unescaped; reuse the batch builder's escape logic rather
than storing the raw echo.)

Generation recording **changes deliberately**: every filter attempt that obtained a
generation id (the streaming client surfaces the `x-generation-id` header as a
synthetic first chunk, so failed attempts now have one) records exactly once, at
attempt termination, with usage if the stream delivered it and without otherwise.
Today only HTTP-200 filter responses record; billed-but-failed calls are invisible
in `llm_generations`, and this closes that. Termination-only matters mechanically:
the insert is `ON CONFLICT (generation_id) DO NOTHING`, so an early defensive write
would freeze a permanently usage-less row.

One addition: `filter_attempts[]` entries for attempts that failed **after**
emitting carry `"emitted": true` (serialized only when true; defined as "at least
one non-empty delta was handed to the turn's frame channel" — the channel is
detached and unbounded, so this cannot prove client receipt or rendering). Two
attempts with the same `reason` differ materially when one showed the user a bubble
that was then replaced; this is the readout that lets the default-open ship be
measured and, if the data says so, later ratcheted (e.g. a larger holdback).

### Config

None. Streaming is the filtered-mode behavior; the batch filter path is deleted, not
kept behind a flag. Rollback is the standard image pin. One code path, one test
surface.

## Invariants (the testable claims)

1. Concatenation of emitted filter deltas == persisted `content` ==
   `clean_response(trim(full filter output))` for every served filtered turn
   (the old batch path applied one further outer `trim()` on top of this value,
   which diverges only on a quote-wrapped, padded tail — e.g. `"say "` served
   as `say` there vs `say ` here).
2. No pre-emission disposition differs from batch: for any complete filter output,
   the walk/serve/reason decision made before the first delta is identical to
   `filter_output_invalidity` on the batch-processed text. (`length` is served in
   both, `content_filter` rejected in both.)
3. An emitting attempt can only fail by late `content_filter`, transport error, or
   deadline expiry; each lands in the supersede sequence, and the turn still
   terminates in today's outcome set (served filtered / fail-open original) within
   today's filter-phase budget (`FILTER_TIMEOUT × chain`).
4. The original reply reaches the client only via fail-open, as today.
5. No partial filter text is ever persisted; the persisted row id equals the last
   `meta`'s message id; `continues_from` is null on every filter-attempt `meta`.
6. `meta`↔`done` cardinality stays 1:1 per message id; frame field shapes are
   unchanged; the served `done` carries the chat attempt's usage/generation id as
   today; a superseded id's `done` is `{truncated: true, usage: null,
   generation_id: null}`.
7. Client disconnect changes nothing: the burst runs detached behind an unbounded
   channel and completes + persists regardless of the client, in every mode, as
   today.

## Accepted trade-offs

- **Disclosure window.** A filter attempt can show a clean ≥120-char prefix and
  then fail (late `content_filter`, transport), exposing rejected rewrite text that
  batch would never have shown, before the supersede replaces it. Inherent to any
  streaming design; bounded by the head gate; measured via `emitted`; and strictly
  milder than the existing, deliberate fail-open (which shows the unfiltered
  original).
- **Budget parity, not acceptance parity.** A batch call that completed at 14.9s
  may, as a stream, cross the 15s line and walk or supersede; marginal by
  construction, and the flip side (streams the deadline kills mid-emit that batch
  would have completed) resolves into the supersede path, never a hung turn.
- **Clean-EOF-as-completion** (see Stream-end semantics) follows the engine's
  streaming discipline rather than the batch framing's implicit
  truncation-detection.
- **No byte-BPE repair on the filter stream.** The batch client's internal
  byte-BPE garble repair does not run on streamed output; garbled filter output
  (unobserved in production for filter-class models) streams as-is, matching LIVE
  mode's raw-delta discipline.

## Testing

- Parity pre-processing: fenced output (with and without language tag, with and
  without closing fence), interior ``` with a later closing fence, quote-wrapped
  (`"…"`, `「…」`) output, and the 78-chars-inside-a-fence case that must walk as
  `too_short` exactly like batch; CJK inputs (char, not byte, counting); non-ASCII
  whitespace tails (U+3000, U+00A0).
- Holdback boundary: 119 vs 120 counted chars; refusal phrase inside / straddling /
  after the 120-char window; refusal phrase split across chunks inside the window;
  whitespace-padded short outputs.
- `finish_reason`: `length` on the filter stream is served (no walk, no supersede);
  `content_filter` pre-emit walks silently, post-emit supersedes.
- Supersede: post-emit transport error → next model serves; chain exhausted after
  emit → fail-open original in a fresh bubble; the full frame sequence asserted
  (`done{truncated,null,null}` then fresh `meta`, 1:1 cardinality); `emitted: true`
  on exactly the post-emit failures; superseded partial absent from the DB;
  `final.filtered == true` on post-emit fail-open.
- Deadline: pre-emit expiry walks with no client-visible artifacts; post-emit
  expiry supersedes.
- Every filtered turn asserts ≥ 1 `delta` (existing tests guard delta assertions
  behind `if frames.iter().any(Delta)` and would pass vacuously on a
  no-delta regression).
- Mock migration: every filtered-mode test currently mocks the filter as a
  non-streaming JSON completion body; all of them move to SSE bodies (`data:`
  frames, id, usage, `[DONE]`). Assertions already concatenate deltas; keep
  asserting on concatenation, not frame count.

## Docs impact

`docs/api-reference.md` + `.zh` (SSE section: filtered turns now deliver
incremental deltas and may supersede mid-turn with the sequence above);
`docs/model-config.md` + `.zh` (output_filter section: new prose stating delivery is
streamed — the section currently describes no delivery mechanics, so this is an
addition, not a correction); `docs/llm-audit.md` + `.zh` (`filter_attempts[]`: the
`emitted` key). Scan `docs/`, `examples/*.toml`, `.env.example`, `README` for any
other description of filtered delivery.

## Rejected alternatives

- **Segment-level pipelined filtering** — rewrites per chunk lose global context;
  multiplies filter calls and cost.
- **Stream the original live, replace with the rewrite** — leaks the unfiltered
  original on every filtered turn; defeats the filter.
- **Commit-once-emitted (persist partial, `truncated` flag)** — simpler than
  supersede but persists truncated bubbles and invents a terminal state the batch
  path never had; supersede reuses an existing client behavior and keeps the
  terminal-outcome set closed.
- **`stream = false` escape hatch** — keeps the batch path alive forever as a
  second code path and test surface for a hypothetical; rollback via image pin
  already exists and is the deployment discipline's standard lever.
- **Two-phase deadline (15s pre-emit / 120s post-emit)** — see Deadline; reachable
  collision with the whole-turn `generation_timeout` creates a
  nothing-persisted terminal state outside today's outcome set.
