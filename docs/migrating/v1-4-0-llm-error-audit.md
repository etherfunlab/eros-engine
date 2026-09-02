# Migrating to the v1.4.0 LLM error audit

**Applies to:** clients of `eros-engine` upgrading to `v1.4.0`, the release that
carries store migration `0050`.
**Design:** [`docs/superpowers/specs/2026-08-18-llm-error-audit-design.md`](../superpowers/specs/2026-08-18-llm-error-audit-design.md)

**Every LLM failure the engine already knew about now has a home.** The upstream
HTTP status, the provider's own error code, and the engine-side transport
failures used to reach nothing but a `tracing::warn!`. They now reach the audit
tables, the SSE `final` frame, and — on non-streaming endpoints — the HTTP status
line itself.

**If you do nothing, two things break:** a consumer branching on `status === 502`
to detect a provider failure stops seeing one, because the provider's own status
(529, 503, 429, …) now passes through verbatim; and any dashboard keyed on the
`stream_metrics` `outcome` values `open_error` / `open_timeout` / `total_timeout`
/ `idle_timeout` / `chunk_error` / `error_frame` goes to zero, because those six
labels are retired.

**Rollout has no ordering constraint.** Migration `0050` is additive and
nullable, so it is safe to apply ahead of the engine deploy, and rolling the
engine back leaves two unread columns behind. There is no reverse-migration
hazard and no three-deploy dance.

---

## 1. Non-streaming provider failures no longer always return 502

`POST /persona/{instance_id}/image/compose` is the one endpoint that surfaces a
provider failure as an HTTP error rather than absorbing it. It used to render
`502` whatever the upstream said. It now returns **the upstream's own status,
verbatim** — `StatusCode::from_u16` accepts 100–999, so a `529` goes out as
`529`.

One bound, expressed as a range and not an allow-list: a status **below `400`
is not forwarded** and becomes `502`. This is not hypothetical — a provider that
answers `200` and puts the error in the body is recorded with
`upstream_status: 200`, and forwarding that would tell a client branching on
`res.ok` that the call succeeded. The recorded value is unchanged; only the
response status is synthesised.

A failure with no status of its own (a timeout, a transport drop, a decode
failure — the `gateway_errors` cases) still gets a synthesised one: **`504` for
the three timeout kinds, `502` for everything else.**

This covers the endpoint's streaming mode too: the composer chain is walked while
*opening* the stream, so a chain that never yields a first token fails before the
SSE response exists and comes back as this HTTP error, not as an in-band frame.

The body gained five keys, never all at once — the two arms are disjoint:

```jsonc
// upstream arm — the provider answered, with a status
{
  "error": "upstream",
  "message": "upstream failure: code=529: Overloaded",
  "upstream_status": 529,
  "provider_code": "529",
  "error_type": "overloaded",
  "retryable": true
}
```

```jsonc
// gateway arm — our path to the provider broke; no upstream status exists
{
  "error": "upstream",
  "message": "upstream failure: compose stream open timeout after 15s",
  "gateway_kind": "open_timeout",
  "retryable": true
}
```

| Key | Present | Meaning |
| --- | --- | --- |
| `error` | always | Still the literal `"upstream"`. Unchanged. |
| `message` | always | Unchanged shape: `upstream failure: <scrubbed one-line detail>`. |
| `upstream_status` | upstream arm only | The status the provider returned, echoing the HTTP status line. |
| `provider_code` | upstream arm, when the body carried one | OpenRouter `error.code` / Venice `error.code`. |
| `error_type` | upstream arm, when the body carried one | OpenRouter `metadata.error_type`. |
| `gateway_kind` | gateway arm only | `open_timeout` \| `total_timeout` \| `idle_timeout` \| `transport` \| `decode` \| `config` \| `chain_exhausted`. |
| `retryable` | always | Derived from the same status the response carries, by HTTP convention: every `5xx`, plus `408` and `429`. |

**`Retry-After` is forwarded verbatim** as a response header whenever the
provider sent one and it parsed as delay-seconds. The engine records it and
passes it on; it never acts on it. Honouring it is your decision.

**Detect a provider failure by the `error` key, not by the status.**
`{"error": "upstream"}` is written by exactly one branch and is the reliable
discriminator. A status check now needs to cover the whole retryable range, and
an unrecognised code — a `570` a future provider invents — will reach you
unmodified, because the engine classifies by range and not by an allow-list.

**`429` and `403` now have two causes each — this is the passthrough's sharpest
edge.** Before this release an upstream `429` was flattened to `502`, so a `429`
from this endpoint could only mean the engine's own limit. Now both reach you as
`429`.

| Status | Engine's own meaning (**what your current handling was written for**) | Newly also means |
| --- | --- | --- |
| `429` | Per-user concurrent-stream cap (3, shared with chat and voice). Back off *your* concurrency. | The **provider** rate-limited us. Your concurrency is not the problem. |
| `403` | The persona instance does not belong to the JWT user. Permanent; do not retry. | The **provider** refused on permission / guardrail / moderation grounds. |

**The field that settles it is the top-level `error` key.** `"error":
"upstream"` means the passthrough; the engine's own errors on this endpoint
either carry a different `error` value (`not_found`) or the pre-stream
`code` / `message` / `user_message` shape with no `error` key at all. The same
test covers any status a future provider adds, so branch on the key once rather
than adding a case per code.

**`Retry-After` rides only the provider's `429`.** The engine never attaches it
to its own — so a present `Retry-After` is itself a reliable second signal, and
the delay it names is the *provider's* limit, not a statement about how many
streams you may hold open. Backing your own concurrency off on it is the wrong
response.

No other status collides: the engine's remaining codes on this endpoint (`401`,
`404`, `422`, `501`) are not among the ones a provider failure produces, and the
`502` / `504` the gateway arm synthesises are inside the `"error": "upstream"`
family already.

## 2. `stream_metrics` dashboards on the six transport outcomes go to zero

The `stream_metrics` tracing event's `outcome` field loses its transport-shaped
labels. One label used to cover a provider status, a TLS reset, a decode failure
and a local misconfiguration alike, which told a dashboard nothing.

| Retired `outcome` | Now reports |
| --- | --- |
| `open_error` | `upstream_error` or `gateway_error` |
| `open_timeout` | `gateway_error` |
| `total_timeout` | `gateway_error` |
| `idle_timeout` | `gateway_error` |
| `chunk_error` | `gateway_error` |
| `error_frame` | `upstream_error` |

The five content verdicts — `served`, `length`, `content_filter`, `empty`,
`garbled` — are unchanged.

Which of the two pointer values fires is decided by **classifying the failure**,
not by which code arm produced it: a connection reset caught inside what used to
be the `open_error` arm is a gateway fact, and a provider status caught inside
what used to be the `chunk_error` arm is an upstream fact.

The distinction the six labels used to carry is not lost — it moved into
`gateway_errors[].kind`, where it is SQL-queryable instead of log-only. If your
dashboard needs the three timeouts apart, read the column (§5), not the log.

## 3. New `final` frame fields: `llm_attempts` and `gateway_errors`

The SSE `final` frame gains two arrays. **Both are omitted entirely when empty**,
so a turn with no failures is byte-identical to what you receive today.

```text
data: {"type":"final","filtered":false,"prompt_injected":null,"tier":null,"retries_chat":1,"retries_filter":0,"llm_attempts":[{"task":"chat_companion","model":"x-ai/grok-4.20","http_status":529,"provider_code":"529","error_type":"overloaded","upstream_provider_code":"anthropic:overloaded_error","retry_after_s":30,"message":"code=529: Overloaded"}]}
```

> **These fields are not fatal, and must not be rendered as an error.**
> A turn carrying them may have been served perfectly normally — the example
> above is a turn whose primary model returned `529` and whose fallback answered,
> which the user experienced as an ordinary reply. Nothing about the turn failed.

The frame is the **non-fatal channel**: it lets you alert, count and reconcile
without showing the reader anything. Three cases carry the fields:

- **A recovered turn.** One or more hops failed, a later one served. The reply is
  real and complete.
- **A pseudo-ghost turn.** The whole chain exhausted and the engine served a
  canned phrase as a normal reply (`metadata.fallback_reason = "stream_failure"`
  on the persisted row). To the end user this is an ordinary short reply, and the
  frame is now the only **on-the-wire** signal that it was not one. The
  pseudo-ghost itself is unchanged and deliberately keeps looking normal.
- **A garble-repaired turn.** The chain exhausted and the reply was salvaged from
  a byte-BPE-garbled hop (`metadata.fallback_reason = "garble_repaired"`).

**What rides the frame, and what deliberately does not.** It carries the five
chains whose failure changes what you received: the chat model chain, the input
filter, the output filter, the PDE judge, and the image prompt composer. Affinity
eval and the `chat_vision` describe are excluded by design — see §11.

**The voice stream has no `final` frame at all** (`delta*` → `done`, or a single
`error`), so these two fields never appear there. Voice failures still land in
`chat_messages` under `task: "chat_voice"`.

## 4. New `error` frame fields, and two `code` values you have never seen

The in-band SSE `error` frame on the chat and voice streams gains two optional
fields, both omitted when absent:

```text
data: {"type":"error","code":"rate_limited","retryable":true,"message":"…","user_message":"…","upstream_status":429,"provider_code":"429"}
```

| Field | Present |
| --- | --- |
| `upstream_status` | Only when the failure was an upstream one. A gateway failure has no status the provider actually returned, so both fields are absent — the internal 504/502 routing of §1 is not echoed here as if a provider had sent it. |
| `provider_code` | Same, and only when the provider's body carried a code. |

**`code` can now be `rate_limited` or `timeout`.** Both values have been declared
on the wire enum since the streaming spec and were **never constructed** — every
failure collapsed to `upstream_unavailable`. They are now derived from the
failure in hand:

| Failure | `code` |
| --- | --- |
| Upstream, `http_status == 429` | `rate_limited` |
| Upstream, any other status | `upstream_unavailable` |
| Gateway, `open_timeout` / `total_timeout` / `idle_timeout` | `timeout` |
| Gateway, `config` | `internal` |
| Gateway, `transport` / `decode` / `chain_exhausted` | `upstream_unavailable` |

If your client switches on `code`, add the two arms. If it has a default arm,
check that the default is not "show a permanent failure" — `rate_limited` and
`timeout` are both `retryable: true`.

The compose endpoint's own in-band `error` frame (`ComposeFrame::Error`) keeps
its four fields and does **not** gain `upstream_status` / `provider_code`. When
that endpoint's chain fails before the SSE response exists, you get the §1 HTTP
error instead, which does carry them.

## 5. Two new columns on five tables

Migration `0050` adds `llm_attempts JSONB` and `gateway_errors JSONB` to
`engine.chat_messages`, `engine.chat_vision_events`, `engine.chat_images_events`,
`engine.companion_decision_events` and `engine.companion_affinity_events`.

Additive, nullable, no backfill, no index. Identical shape on all five, so a
fleet-wide error view is one `UNION ALL`.

**`NULL` means "nothing to record".** An empty array is never written, so there
is exactly one way to say "no failure".

### The three-home split

The organising principle is **who authored the fact**, not which code path
produced it. Each fact has exactly one home.

| Home | Holds | Boundary |
| --- | --- | --- |
| `llm_attempts` | What the **upstream** said | The provider spoke: a non-2xx status, or a `200` body carrying an error envelope |
| `gateway_errors` | Where the engine's **path to the provider** broke | The provider said nothing usable: timeout, connection reset, TLS error, unparseable body |
| Each table's existing coarse marker | That row's **business verdict** | The call *succeeded*: an empty completion, a garble, a refusal, a length cut |

A `200 OK` stream that carries `{"error": {...}}` mid-stream, or that terminates
with `finish_reason: "error"`, is an `llm_attempts` entry with
`http_status: 200` — the provider spoke, just not in the status line.

### `llm_attempts` element

```jsonc
{
  "task": "chat_companion",
  "model": "x-ai/grok-4.20",
  "http_status": 529,
  "provider_code": "529",
  "error_type": "overloaded",
  "upstream_provider_code": "anthropic:overloaded_error",
  "retry_after_s": 30,
  "message": "code=529: Overloaded"
}
```

`task`, `model`, `http_status` and `message` are always present; the other four
are omitted when absent, never nulled.

`http_status` is a **raw `u16`, never an enum**, deliberately. OpenRouter reports
overload as `529` while Venice uses `429 MODEL_OVERLOADED` and
`503 MODEL_AT_CAPACITY`; the next provider will disagree differently. Classify by
HTTP convention and an unrecognised code still behaves sensibly.

There is deliberately **no `retryable` field** here. With `http_status` present,
you apply the convention yourself — the engine does not editorialise inside a
column whose contract is "what the upstream said". (The non-streaming error body
of §1 does carry `retryable`, because there the engine has already had to choose
a status.)

`message` is scrubbed, flattened and length-capped, and **never contains prompt
text** — a moderation rejection's `metadata.flagged_input` is dropped before the
value is built.

### `gateway_errors` element

```jsonc
{
  "task": "chat_companion",
  "model": "x-ai/grok-4.20",
  "kind": "open_timeout",
  "message": "stream open timeout after 20s"
}
```

`task`, `kind` and `message` are always present. `model` is omitted when the
failure precedes model selection (a config error) and on the chain-scoped
`chain_exhausted`.

| `kind` | Scope | Meaning |
| --- | --- | --- |
| `open_timeout` | attempt | Connect / queue / response-headers timeout |
| `total_timeout` | attempt | One attempt's whole generation exceeded its cap |
| `idle_timeout` | attempt | Byte-level idle watchdog fired mid-stream |
| `transport` | attempt | Connection reset, TLS failure, SSE body interrupted |
| `decode` | attempt | A response arrived but could not be parsed |
| `config` | attempt | Local misconfiguration (empty model slug, unresolvable provider) |
| `chain_exhausted` | chain | Every candidate failed. Carries no `model` |

The three timeouts stay distinct on purpose: folding them together once made idle
timeouts invisible.

**`decode` does not cover a byte-BPE garble**, despite the name. That call
succeeded and was billed — it is a content verdict, so the row's coarse marker
owns it (`garbled`, or `fallback_reason = "garble_repaired"`) and neither column
gets an entry. A chain that garbled once and then recovered writes nothing at
all; do not read an empty `gateway_errors` as "no garble happened".

### The `task` discriminator

`engine.chat_messages` hosts three call sites in one pair of columns. They are
told apart by each element's `task` — the existing `[tasks.*]` config key, the
same value already carried on the wire as `ChatRequest.task`. No new
discriminator was invented, and `task` is strictly finer than one would have
been: it separates `chat_companion` from `chat_voice` from `chat_product_qa`,
which a coarser `stage: "chat"` would have flattened.

| `task` | Table | Row |
| --- | --- | --- |
| `chat_companion` / `chat_voice` / `chat_product_qa` | `chat_messages` | assistant |
| `chat_output_filter` | `chat_messages` | assistant |
| `chat_input_filter` | `chat_messages` | **user** |
| `chat_vision` | `chat_vision_events` | — |
| `chat_image_prompt_compose` | `chat_images_events` | — |
| `pde_decision` | `companion_decision_events` | — |
| `affinity_evaluation` | `companion_affinity_events` | — |

The input filter is the one call site that had **no failure record of any kind**
before this release. Its failures now land on the `role='user'` row, beside the
`pre_filter_content` / `filter_model` / `filter_triggers` / `f_generation_id`
audit that its *successes* already wrote there. Query the user row for those, not
the assistant row.

## 6. Retired marker values

The transport-layer values that had been shoehorned into business vocabularies
are retired. Each vocabulary gains the same two **pointer values** —
`upstream_error` and `gateway_error` — which say only "an attempt failed; the
detail is in that column".

| Marker | Retired | Added |
| --- | --- | --- |
| `stream_metrics.outcome` (tracing) | `open_error`, `open_timeout`, `total_timeout`, `idle_timeout`, `chunk_error`, `error_frame` | `upstream_error`, `gateway_error` |
| `chat_messages.metadata.filter_attempts[].reason` | `error`, `timeout` | `upstream_error`, `gateway_error` |
| `chat_vision_events.last_failure` | `model_error`, `timeout` | `upstream_error`, `gateway_error` |
| `chat_images_events.last_failure` | `model_error`, `timeout`, `stream_open_failed`, `stream_died_midway` | `upstream_error`, `gateway_error` |
| `companion_decision_events.status` | `error`, `timeout` — no longer written; the `CHECK` still accepts them for rows written before `0050` | `upstream_error`, `gateway_error` |
| `companion_affinity_events.context.eval_skip_reason` | `eval_error`, `eval_timeout` | **nothing** — see below |

What survives in each vocabulary is exactly the content-level verdicts: `served`,
`length`, `content_filter`, `empty`, `garbled`, `refusal_pattern`, `too_short`,
`unparseable`, `empty_prompt`, `blank_description`, `parse_error`.

**Which pointer value you get is decided by the whole operation.** For the
markers that describe one operation — `chat_vision_events.last_failure`,
`chat_images_events.last_failure`, `companion_decision_events.status` — the value
is `upstream_error` if the operation produced *any* `llm_attempts` entry, and
`gateway_error` only when it produced none. A chain that took a `529` and then
timed out reads `upstream_error`, not `gateway_error`: "did a provider misbehave
during this turn" is what the coarse value is for, and the per-hop order is in
the columns. `filter_attempts[].reason` and `stream_metrics.outcome` are the
per-attempt exceptions — each entry there describes exactly one attempt.

**`stream_open_failed` and `stream_died_midway`** are specific to the standalone
compose endpoint's streaming mode. Each covered a provider status *and* a local
timeout under one label — exactly the class the retirement targets — so both are
gone, and `chat_images_events.last_failure` now reads the same whichever endpoint
wrote the row. (These two were found during implementation and are not in the
spec's original §7 table; the spec has been trued up.)

**No backfill.** Rewriting production audit history to gain vocabulary uniformity
is not worth it. `companion_decision_events.status` is `NOT NULL` with a `CHECK`,
so the `CHECK` widened to
`('ok','empty','parse_error','timeout','error','upstream_error','gateway_error')`
— the old two stay legal so historical rows remain valid, and the engine simply
stops writing them. Queries spanning the migration must accept both vocabularies.

**`eval_skip_reason` gets no pointer values, deliberately.** Its contract is "why
no call was made", and a failed call is not a skip. Its remaining values are all
genuine intentional skips (`proactive`, `short_user_msg`, `empty_assistant`) plus
`eval_no_generation_id`, which marks a *successful* eval whose response carried
no join key. A failed affinity eval writes no marker there at all — it explains
itself through the two columns instead (§7).

## 7. The affinity invariant, restated

A `companion_affinity_events` row with a **NULL `generation_id`** is always
explained, by exactly one of:

- an `eval_skip_reason` in `context` — no call was ever attempted; or
- a non-empty `llm_attempts` / `gateway_errors` — a call was attempted and
  failed.

Before this release the second case was silently indistinguishable from the
first, because a failed eval wrote `eval_error` / `eval_timeout` into the skip
vocabulary. It no longer does. If you have a query that treats "NULL
`generation_id` and no `eval_skip_reason`" as a data defect, it now needs the
second arm — that combination is a legitimate, fully-explained state.

## 8. Two counting traps

If you build a dashboard on the new columns, these two will bite.

**`chain_exhausted` does not mean the turn was served nothing.** A
`gateway_errors` entry with `kind: "chain_exhausted"` is written both on a
pseudo-ghost turn *and* on a garble-repaired turn. In the garble-repaired case
every candidate genuinely did fail — the chain really was exhausted, the entry is
correct — and the turn was then served from a salvaged garbled hop rather than
from nothing. Read `chat_messages.metadata.fallback_reason` alongside it to
separate the two: `"stream_failure"` (canned phrase) from `"garble_repaired"`
(salvaged text).

**One turn contributes exactly one list.** The accumulated failures are written
only on the row that **concludes** a turn — the served reply, the ghost, the
pseudo-ghost, or the garble-repaired row. A superseded truncated bubble carries
`NULL` in both columns, even though its own attempt is inside the concluding
row's list. Do not sum across rows to count a turn's failures, and do not read a
truncated row's `NULL` as "this bubble had no failures".

## 9. Replay does not carry the failures

A replayed turn (`POST /comp/chat/{session_id}/message/stream` with a
`client_msg_id` already seen within 24 h) emits its `final` frame with **both
lists empty**, so both keys are omitted — even when the original turn's persisted
row holds them.

This is consistent, not an oversight: replay already recomputes `retries_chat`,
`tier` and `prompt_injected` from turn-local state rather than reading them back
off the row, a documented non-goal in
[`2026-05-26-error-fallback-config-design.md`](../superpowers/specs/2026-05-26-error-fallback-config-design.md)
§1. A consumer that needs a replayed turn's failures queries
`engine.chat_messages` directly.

## 10. One log string changed

Exactly one log line's text is different. A mid-stream provider failure used to
log:

```
provider error: openrouter mid-stream error: code=Some(Number(529)): Overloaded
```

and now logs:

```
provider error: mid-stream error: code=529: Overloaded
```

Two changes. The vendor name is gone because this client also serves Venice and
any custom OpenAI-compatible endpoint via the `@provider` suffix, so
`"openrouter …"` on a Venice stream was simply false. And the code is no longer
`Debug`-formatted — it renders as its JSON text, so a numeric `529` reads `529`
and a string code reads `"MODEL_OVERLOADED"`, quotes included, which keeps the
two distinguishable.

**Every other log line is byte-identical.** The error-body scrubbing that used to
return a flattened string now returns a parsed struct, and that struct's
`Display` reproduces the old output exactly — pinned by the same scrubbing tests
that guarded it before, precisely so log-based alerting does not move.

## 11. Two failure kinds that never reach the wire, by design

**Affinity eval failures.** They are written to
`companion_affinity_events.llm_attempts` / `.gateway_errors` for engine-side
debugging — including hops a fallback recovered from, so a row with a populated
audit trio can still carry a `529` — and never appear in the `final` frame. Two reasons, and neither is
timing: the eval is already fail-open, so the only consequence is that this one
turn contributes no affinity delta while the rule-based deltas still land; and
turns legitimately contribute no delta for entirely ordinary reasons
(`short_user_msg`, `proactive`, `empty_assistant`). "No affinity judgment this
turn" is a normal state, not an incident, and a consumer has no way to act
differently on the two cases. Do not expect a post-turn event to fill this in —
the exclusion is the design.

**Vision failures.** The `chat_vision` describe is a **fail-open pre-stage** of an
image-carrying chat turn. An exhausted describe chain does not fail the turn — it
keeps it text-only and a placeholder covers the undescribed image, so the reply
you receive is a normal reply. Its failures land in `chat_vision_events` and are
never folded into the turn's accumulated list. Use
`chat_vision_events.status = "exhausted"` if you need to count them.

Both are reachable by query. Neither is an alerting signal you receive on the
stream.

## 12. Checklist

- [ ] Replace `status === 502` provider-failure checks with a check on the body's
      `"error": "upstream"` key, or widen the status handling to the full
      retryable range (§1).
- [ ] **Split your existing `429` and `403` handling on the `error` key** — each
      now has a provider cause as well as the engine one your code was written
      for, and backing your concurrency off on a provider rate limit is the
      wrong response (§1).
- [ ] Decide whether to honour the forwarded `Retry-After` header. It rides only
      the provider's `429`, and the engine does not act on it (§1).
- [ ] Repoint any `stream_metrics` dashboard off the six retired `outcome`
      labels onto `upstream_error` / `gateway_error`, and onto
      `gateway_errors[].kind` where the timeout distinction matters (§2).
- [ ] Tolerate `llm_attempts` / `gateway_errors` on the `final` frame, and make
      sure they render as **nothing user-facing** (§3).
- [ ] Add `rate_limited` and `timeout` arms to any `error`-frame `code` switch,
      and confirm the default arm does not report a permanent failure (§4).
- [ ] Apply migration `0050`. No ordering constraint against the engine deploy.
- [ ] Update any query that reads the retired marker values, and accept both
      vocabularies across the `companion_decision_events.status` cutover (§6).
- [ ] Fix any affinity query that treats "NULL `generation_id`, no
      `eval_skip_reason`" as a defect (§7).
- [ ] If you build on the new columns, handle the two counting traps: read
      `metadata.fallback_reason` beside `chain_exhausted`, and do not sum lists
      across a turn's rows (§8).
