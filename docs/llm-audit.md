# LLM audit passthrough

eros-engine exposes an opaque OpenRouter passthrough on the streaming
chat endpoint. Three caller-supplied fields ride to
`openrouter.ai/api/v1/chat/completions` unchanged, three OpenRouter wire
echoes come back on the SSE `done` frame, and a deployer-declared
`headers` table adds app-attribution headers to every outbound call.

The engine never inspects content. PII scrubbing, hashing, and
metadata semantics are the caller's responsibility.

## Inbound: the `audit` request field

`POST /comp/chat/{session_id}/message/stream` accepts an optional `audit`
object alongside the required `content` / `client_msg_id`:

```jsonc
{
  "content": "...",
  "client_msg_id": "01J3333333333333333333333A",
  "audit": {
    "user": "u_<hash-of-internal-id>",     // optional
    "session_id": "conv_xyz",               // optional, ≠ URL session UUID
    "metadata": {                           // optional
      "feature": "chat",
      "plan": "pro"
    }
  }
}
```

Caps enforced at the engine before forwarding:

| Field                  | Cap                                              |
|------------------------|--------------------------------------------------|
| `audit.user`           | `chars ≤ 256`                                    |
| `audit.session_id`     | `chars ≤ 256`                                    |
| `audit.metadata` keys  | `≤ 16`                                           |
| `audit.metadata` key   | regex `^[A-Za-z0-9_.-]{1,64}$`                   |
| `audit.metadata` value | JSON string, `chars ≤ 512`                       |

Violations return `400 BadRequest` as a pre-stream error, and no user
message row is persisted.

**Privacy:** do not put raw email / wallet address / real name in
`user` — send a hash. OpenRouter retains request metadata (token
counts, latency) but not prompts / responses by default.

## Outbound: the `usage` echo on the SSE `done` frame

The streaming endpoint's `done` frame carries three optional fields:

| Field           | Type      | Meaning                                                                                       |
|-----------------|-----------|-----------------------------------------------------------------------------------------------|
| `usage`         | `object?` | OpenRouter `usage` block verbatim (tokens / cost / cached / reasoning). Engine does not flatten. |
| `generation_id` | `string?` | OpenRouter `response.id`. Query `/api/v1/generation` with it for full request metadata later. |
| `model`         | `string?` | Model OpenRouter actually served. When `fallback_model` was hit, this is the fallback.        |

These fields appear on the `done` frame (the per-turn terminal frame
before `final`). Background paths (dreaming / post_process) do **not**
surface them to clients.

### Hiding fields from the response

Deployers can strip specific top-level keys from the `usage` echo by
setting `OPENROUTER_USAGE_HIDDEN_KEYS` (comma-separated) on the server.
Typical use: hide wholesale `cost` / `cost_details` from downstream
customers without losing visibility for the operator.

```bash
OPENROUTER_USAGE_HIDDEN_KEYS=cost,cost_details
```

Behaviour:

- Applies to the SSE streaming `done` frame on both
  `/comp/chat/{id}/message/stream` and
  `/comp/voice/{session_id}/turn/stream`.
- The full unfiltered `usage` is still persisted to the DB; only the
  client-facing payload is filtered.
- Does **not** affect `tracing::info!` output — operator observability
  stays intact regardless of this setting.
- Background paths (dreaming / post_process) already don't return
  `usage` to clients, so the env var has no effect on them.
- Only top-level keys are stripped; to suppress a whole subtree, list
  its parent key (`cost_details` removes the entire object, not just
  its members).
- Unset or empty → today's pass-through behaviour.

Background paths (`pipeline::dreaming`, `pipeline::post_process`,
`pipeline::world`, `pipeline::story`) emit usage only as `tracing::info!`
fields:

```
openrouter: call completed session=… generation_id=… model=…
prompt_tokens=… completion_tokens=… total_tokens=… cost=…
```

- `world_director` — World Memories director sweeper (background). One call
  per enrolled owner per `interval_hours`. `user` =
  `11111111-1111-1111-1111-111111111112` (world subsystem sentinel, distinct
  from dreaming's `11111111-1111-1111-1111-111111111111`). Usage/cost emitted
  as tracing fields via `log_openrouter_usage("world_director", None, …)`;
  nothing on any client frame.
- `world_comment` — World Town hourly comment round (background). One
  batched call per owner with new feed activity. `user` =
  `11111111-1111-1111-1111-111111111112` (shared world-subsystem sentinel).
  Usage/cost emitted as tracing fields via
  `log_openrouter_usage("world_comment", None, …)`; nothing on any client
  frame.
- `world_reply` — World Town reply responder (background). One call per
  debounced user comment, capped per owner per UTC day. Same sentinel user;
  usage/cost emitted as tracing fields via
  `log_openrouter_usage("world_reply", None, …)`; nothing on any client
  frame.
- `world_stories_director` — World Stories director (background), module
  `pipeline::story`. Runs as the second phase of the same sweeper tick as
  `world_director`; one call per claimed persona instance per its own
  `interval_hours`. `user` = `11111111-1111-1111-1111-111111111113` (story
  subsystem sentinel, continuing the dreaming/world sequence). Usage/cost
  emitted as tracing fields via
  `log_openrouter_usage("world_stories_director", None, …)`; nothing on any
  client frame.

## App-attribution headers

Declare a `headers` table on `[providers.openrouter]` in the model config
(`docs/model-config.md` → "Built-in endpoint overrides via
`[providers].openrouter`") to add headers to every outbound OpenRouter
call:

```toml
[providers.openrouter]
headers = {
  "HTTP-Referer" = "https://eros.example",
  "X-OpenRouter-Title" = "Eros",
  "X-OpenRouter-Categories" = "companion,roleplay",
}
```

| Purpose-built header      | Meaning                                          |
|----------------------------|--------------------------------------------------|
| `HTTP-Referer`             | App identifier on OpenRouter dashboards          |
| `X-OpenRouter-Title`       | Display name in OpenRouter app analytics         |
| `X-OpenRouter-Categories`  | Comma-separated marketplace categories           |

No `[providers.openrouter]` entry, or one without `headers` → today's
behaviour (no attribution headers). They are set per deployment, not per
request — App-Attribution is intended for app-level aggregation. Per-user
attribution belongs in `audit.user`.

`X-OpenRouter-Categories` is passed through verbatim; OpenRouter silently
ignores unrecognised values and only honours it when `HTTP-Referer` is
also set.

Header names/values are validated at boot — engine-owned names
(`Authorization`, `Content-Type`, case-insensitive) or anything that isn't
valid HTTP header material refuses the load, rather than the older
construction-time warn-and-drop.

**Migration note:** the `OPENROUTER_APP_REFERER` / `OPENROUTER_APP_TITLE` /
`OPENROUTER_APP_CATEGORIES` env vars are soft-deprecated — a still-set
value is silently ignored, never a boot error, but it no longer does
anything. Re-declare the same headers under `[providers.openrouter].headers`
as shown above.

## What the engine does NOT do

- **Persist the `audit` object.** No DB column stores the caller-supplied
  `audit` (`user` / `session_id` / `metadata`) or the attribution headers —
  those are surface fields only, forwarded upstream and then dropped. The
  OpenRouter `model` / `usage` / `generation_id` triple **is** persisted, on
  `chat_messages.model` / `.usage` / `.generation_id` for the chat completion
  (mirrored on `companion_affinity_events` for the affinity eval, on
  `companion_insights_events` for each `insight_extraction` call, on
  `engine.character_insights_events` for each call of the experimental
  character chain (§"Character insights" below), on
  `companion_decision_events` for each `pde_decision` judge run, on
  `chat_images_events` for every image-composer call, and on
  `chat_vision_events` for every `chat_vision` describe — see
  [Image-path event tables](#image-path-event-tables) below;
  `chat_messages.metadata.image` keeps its existing `compose_*` keys plus a
  `compose_event_id` pointer into `chat_images_events`) — see the `usage`
  filtering note above. Since the companion_insights
  teardown (spec 2026-08-11), `companion_insights_events` rows can contain
  keys the typed `human_insights` store never persists: the extractor's
  `existing_insights` context is now reverse-projected from `human_insights`
  (not the retired JSONB blob), and any off-schema key the LLM emits still
  lands in the event payload but nowhere else — a future events↔store
  reconciliation has to compare on payload keys ∩ `human_insights` column
  set, not full equality.
- **Hash.** The engine does not transform `user` — callers are
  responsible for sending a hash.
- **Sanitise.** `metadata` keys and values are size / shape-checked,
  not content-checked.
- **Interpret.** The engine does not group, aggregate, or alert on
  any audit field. Callers wire that themselves.

## Failed attempts: `llm_attempts` / `gateway_errors`

Since **v1.4.0** (store migration `0050`) five tables carry the same two
nullable `JSONB` columns, recording every LLM attempt that **failed** —
including attempts a fallback recovered from, which leave no other trace:

| Table | Call sites it hosts |
|---|---|
| `engine.chat_messages` | the chat model chain, the output filter, the input filter |
| `engine.chat_vision_events` | `chat_vision` describe |
| `engine.chat_images_events` | image-prompt composer |
| `engine.companion_decision_events` | PDE judge |
| `engine.companion_affinity_events` | affinity eval |

Identical shape on all five, so a fleet-wide error view is one `UNION ALL`.
Additive, nullable, no backfill, no index. **`NULL` means "nothing to
record"** — an empty array is never written, so there is exactly one way to
say "no failure". Design:
[LLM error audit spec](superpowers/specs/2026-08-18-llm-error-audit-design.md);
consumer-facing changes:
[migrating/llm-error-audit-v1-4-0.md](migrating/llm-error-audit-v1-4-0.md).

### Three homes, split by who authored the fact

Not by which code path produced it. Each fact has exactly one home, so the
three never overlap.

| Home | Holds | Boundary |
|---|---|---|
| `llm_attempts` | What the **upstream** said | The provider spoke: a non-2xx status, or a `200` body carrying an error envelope |
| `gateway_errors` | Where the engine's **path to the provider** broke | The provider said nothing usable: timeout, connection reset, TLS error, unparseable body |
| The table's own coarse marker (`last_failure`, `status`, `fallback_reason`, `filter_attempts[].reason`) | That row's **business verdict** | The call *succeeded* and was billed: empty completion, byte-BPE garble, refusal, length cut |

A `200 OK` stream carrying `{"error": {...}}` mid-stream, or terminating with
`finish_reason: "error"`, is an `llm_attempts` entry at `http_status: 200` —
the provider spoke, just not in the status line.

Every coarse vocabulary gained the same two **pointer values** —
`upstream_error` / `gateway_error` — which say only "an attempt failed; the
detail is in that column". The transport-shaped labels they replace are
retired; the full before/after table is in the migration guide.

**Which of the two a marker reads is decided by the whole operation, not by
its last hop.** Where the marker is operation-scoped — `last_failure` on both
event tables, `companion_decision_events.status` — it reads `upstream_error`
if the operation produced *any* `llm_attempts` entry, and `gateway_error`
otherwise. Upstream wins: "did a provider misbehave during this turn" is the
question the coarse value exists to answer at a glance, and a chain that took
a `529` and then timed out is still a turn where a provider misbehaved. The
per-hop truth is one column away. `filter_attempts[].reason` is the exception
and describes exactly one attempt each, because that array already has a row
per hop.

A **byte-BPE garble is not a `decode` gateway error**, tempting though the
name is. The call succeeded and was billed, so it is a content verdict: the
coarse marker owns it (`garbled`, or `fallback_reason = "garble_repaired"`)
and neither column carries an entry for it.

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

`task` / `model` / `http_status` / `message` are always present; the other
four are omitted when absent, never nulled. `model` keeps the full config
slug including any `@provider` suffix.

`http_status` is a raw number, **never an enum**: OpenRouter reports overload
as `529` while Venice uses `429 MODEL_OVERLOADED` and `503 MODEL_AT_CAPACITY`,
and the next provider will disagree differently. Classification is by HTTP
convention (4xx client, 5xx upstream, 408/429/5xx retryable), so an
unrecognised `570` behaves sensibly. There is deliberately **no `retryable`
field** — with the status present the consumer applies the convention itself,
and the engine does not editorialise inside a column whose contract is "what
the upstream said".

`message` reuses the existing scrubbing guarantees: flattened to one bounded
line, and `metadata.flagged_input` dropped — a moderation rejection must never
echo the user's prompt back into an audit row.

### `gateway_errors` element

```jsonc
{
  "task": "chat_companion",
  "model": "x-ai/grok-4.20",
  "kind": "open_timeout",
  "message": "stream open timeout after 20s"
}
```

`task` / `kind` / `message` are always present; `model` is omitted when the
failure precedes model selection (a config error) and on the chain-scoped
`chain_exhausted`.

| `kind` | Scope | Meaning |
|---|---|---|
| `open_timeout` | attempt | Connect / queue / response-headers timeout |
| `total_timeout` | attempt | One attempt's whole generation exceeded its cap |
| `idle_timeout` | attempt | Byte-level idle watchdog fired mid-stream |
| `transport` | attempt | Connection reset, TLS failure, SSE body interrupted |
| `decode` | attempt | A response arrived but could not be parsed |
| `config` | attempt | Local misconfiguration (empty model slug, unresolvable provider) |
| `chain_exhausted` | chain | Every candidate failed. Carries no `model` |

The three timeouts stay distinct: folding them together once made idle
timeouts invisible. Here the distinction is SQL-queryable, not log-only.

`message` never carries a provider payload. `decode` is the one kind that could:
a `data:` frame that fails to parse is raw provider output and may hold reply
text, so the frame stays in the log line and the recorded message names only
what broke. Read the log when you need the bytes.

### The `task` discriminator

`engine.chat_messages` hosts three call sites in one pair of columns; each
element's `task` tells them apart. It is the existing `[tasks.*]` config key —
the same vocabulary already carried on the wire as `ChatRequest.task` and
already keying per-task model resolution — not a new discriminator, and
strictly finer than one would have been: it separates `chat_companion` from
`chat_voice` from `chat_product_qa`.

| `task` | Table | Row |
|---|---|---|
| `chat_companion` / `chat_voice` / `chat_product_qa` | `chat_messages` | assistant |
| `chat_output_filter` | `chat_messages` | assistant |
| `chat_input_filter` | `chat_messages` | **user** |
| `chat_vision` | `chat_vision_events` | — |
| `chat_image_prompt_compose` | `chat_images_events` | — |
| `pde_decision` | `companion_decision_events` | — |
| `affinity_evaluation` | `companion_affinity_events` | — |

The input filter is the one call site that had **no failure record of any
kind** before v1.4.0. Its failures land on the `role='user'` row, beside the
`pre_filter_content` / `filter_model` / `filter_triggers` / `f_generation_id`
audit its *successes* already wrote there — so query the user row for them,
not the assistant row.

### Two counting traps

**`chain_exhausted` does not mean the turn was served nothing.** It is written
both on a pseudo-ghost turn and on a garble-repaired one. In the latter every
candidate genuinely did fail — the entry is correct — and the turn was then
served from a salvaged garbled hop. Read
`chat_messages.metadata.fallback_reason` alongside it to tell `stream_failure`
(canned phrase) from `garble_repaired` (salvaged text).

**One turn contributes exactly one list.** The accumulated failures are written
only on the row that **concludes** a turn — served, ghost, pseudo-ghost, or
garble-repaired. A superseded truncated bubble carries `NULL` in both columns
even though its own attempt sits in the concluding row's list. Do not sum
across a turn's rows.

### On the wire

`ProtocolFrame::Final` carries the identical structures from the identical
Rust types — one serializer, not two. `llm_attempts` / `gateway_errors` on the
`final` frame are **non-fatal**: see
[api-reference.md](api-reference.md#post-compchatsession_idmessagestream).
Affinity-eval and `chat_vision` failures are written to their tables but never
surface on the wire, by design.

## Character insights (experimental)

`engine.character_insights_events` is the audit trail for the experimental
character-insight chain (v1.3.0,
[design spec](superpowers/specs/2026-08-15-character-insights-design.md)) —
the mirror of `companion_insights_events` but for the AI character rather
than the human. This is the whole point of that chain having two separate
config task names (`character_insight_extraction` /
`character_insight_structuring`) instead of one shared one the way the human
chain shares `insight_extraction` for both its stages: it lets OpenRouter
accounting and this table tell the extraction call apart from the structuring
call. Concretely:

- **`stage`** is `'extraction'` or `'structuring'` — naming the config block
  each row came from, so `stage='structuring'` tells you which
  `[tasks.*]` block to go tune with no lookup table in between. This is
  unlike the human chain's `companion_insights_events.stage`, which uses
  `'facts'` / `'structured'` and predates the config split.
- **Both rows of one extraction run share a `run_id`** — the extraction call
  and the structuring call it fed are joinable without going through
  `session_id`/`message_id`.

## Image-path event tables

Two append-only tables record every image-composer and `chat_vision`
describe call. Both are **best-effort telemetry, not a guaranteed ledger**:
the INSERT is awaited, bounded by a short `AUDIT_WRITE_TIMEOUT`, and its
error OR timeout is `warn!`ed and dropped — a failed or stalled audit
write costs the event row, never the turn (same discipline as
`companion_decision_events`). Neither table carries a foreign key: a row may
outlive or precede anything it refers to.

Unlike `chat_messages`, which cascades from `chat_sessions`, deleting a
session does not reach either table — both persist verbatim user text
(`chat_images_events.inputs.latest_user_msg` / `.recent_scene`;
`chat_vision_events.image_url`, often a signed, token-bearing URL). **Deployers
must include both tables in their own user-data erasure routine** — the
engine does not do this for you; erasure policy is the deployer's
responsibility, not the engine's. Neither table ships with a pruning policy
either: both grow without bound for as long as the deployment runs, so plan a
partition scheme or a pruning cron yourselves if that matters for your
deployment.

### `engine.chat_images_events`

One row per image-composer LLM call, from **any** caller — a chat turn's
delegated image prompt, or the standalone `POST
/persona/{instance_id}/image/compose` endpoint in either streaming mode.

| Column | Type | Meaning |
|---|---|---|
| `id` | `UUID` | Row id — see linkage below. |
| `source` | `TEXT` | `chat_reply_text_image` \| `chat_reply_image` \| `compose_endpoint` \| `compose_endpoint_stream`. |
| `user_id` | `UUID` | |
| `instance_id` | `UUID?` | Persona instance; NULL when the caller has none in scope. |
| `session_id` | `UUID?` | Chat session; NULL on the standalone endpoint (no session). |
| `status` | `TEXT` | `ok` \| `exhausted` \| `not_configured`. |
| `inputs` | `JSONB` | The five composer slots, structured: `{appearance, recent_scene, latest_user_msg, style, aspect_ratio}`. Empty slots are `""`, not the `（无）` placeholder the prompt renders — that substitution is a rendering detail, not an input. `latest_user_msg` differs by `source`: `chat_reply_image` passes the raw `user_msg.content`, while `chat_reply_text_image` passes `effective_user_msg` — the post-input-filter rewrite. Both faithfully record what the composer actually saw on that turn; an operator diffing rows across the two sources should expect the difference, not read it as an inconsistency. |
| `subject` | `TEXT?` | The composer's own `prompt` field. NULL unless `status = "ok"`. |
| `caption` | `TEXT?` | NULL when the composer produced none, including the non-JSON fallback reply, where the whole reply becomes `subject` instead. |
| `composed_prompt` | `TEXT?` | The assembled wire string — style preset + persona appearance + subject, i.e. exactly what the downstream consumer is handed. Stored on **every** row that produced one, including `exhausted` and `not_configured` on the chat path (the portrait fallback still assembles a wire prompt, and this column is then the only record anywhere of what was drawn). NULL only on the standalone endpoint's `exhausted` rows, which fail without assembling anything. |
| `variant` | `TEXT?` | The resolved `prompt_variant` key; `"raw"` is an ordinary key, not a skip. |
| `model` | `TEXT?` | The model that answered, on success. Also populated on `exhausted` whenever the LAST attempt got a response back at all: `empty`/`empty_prompt` on the chat path and the non-stream endpoint (both walk `run_image_prompt_compose`'s shared chain), and on the standalone endpoint's streaming mode also a candidate that streamed metadata (model/generation_id/usage) and then broke — that evidence is retained because the provider answered and may have been billed. NULL when no response ever came back on any path: a break or timeout that captured nothing at all, or `not_configured` (no call was made). **Since v1.4.0 the "did the provider answer?" test lives ENTIRELY in this trio** (`model` / `generation_id` / `usage`); `last_failure` no longer doubles as a second, coarser copy of it, which is why the two labels that used to encode it (`stream_open_failed` / `stream_died_midway`) are gone. |
| `usage` | `JSONB?` | Full unfiltered OpenRouter usage block, `serde_json::to_value`'d — `OPENROUTER_USAGE_HIDDEN_KEYS` filters the wire copy only, never this. Travels with `model`: populated exactly where `model` is. |
| `generation_id` | `TEXT?` | Travels with `model`. |
| `attempts` | `SMALLINT` | Models actually called off `[primary, ...fallback]`; `0` for `not_configured`. |
| `last_failure` | `TEXT?` | Why the last attempt failed; NULL when `status = "ok"` or `"not_configured"` (no attempt was made, so there is nothing to have failed). Values: `empty` \| `empty_prompt` \| `upstream_error` \| `gateway_error`. A free column, not a CHECK — the vocabulary grows as new failure modes get labeled. The first two are **content verdicts**: the call succeeded and was billed, its output was just unusable. The last two are **pointer values** naming which of the two columns below holds the per-hop detail; since v1.4.0 they replace `model_error` / `timeout` and the streaming mode's `stream_open_failed` / `stream_died_midway` — each of those covered a provider status AND a local timeout under one label, so all four are retired and the column now reads the same whichever endpoint wrote the row (see [Failed attempts](#failed-attempts-llm_attempts--gateway_errors)). **`empty` is also reachable in streaming mode**, not just the chain-walk paths — a candidate that never opens (no content chunk) but whose stream ends normally reports `empty`, same label as the chain-walk's content-level blank-reply arm, because both mean the same thing: the provider answered and may have been billed. |
| `llm_attempts` | `JSONB?` | Every hop where the provider answered with a failure, `task = "chat_image_prompt_compose"`. NULL when there were none — see [Failed attempts](#failed-attempts-llm_attempts--gateway_errors). |
| `gateway_errors` | `JSONB?` | Every hop where our path to the provider broke. NULL when there were none. |
| `created_at` | `TIMESTAMPTZ` | |

`status` is deliberately three values: `exhausted` means "no usable compose
result from this call" for every reason including a mid-stream death on the
streaming endpoint, and `last_failure` carries the distinction.

**`not_configured` is unreachable on this table under current gating.**
`build_delegated_image_prompt` only runs for `ReplyImage`/`ReplyTextImage`,
which the action-plan guard only produces when
`[tasks.chat_image_prompt_compose]` is configured (`model_config` is a
boot-fixed `Arc`, no hot reload); a forced image without the task 422s at the
route before any composer call. The non-stream endpoint likewise 501s on a
missing task before doing any work. The value stays in the CHECK — narrowing
it later costs a migration, and this repo's standing bias is to keep
capability rather than remove it — and exists for a future caller that could
reach the composer without this gate, not as a state you will observe on a
live deployment today. Contrast `chat_vision_events` below, where
`not_configured` is both reachable and the single most valuable status.

**No `message_id` column.** The composer runs *before* the assistant row
exists — on the chat path the composition is `tokio::spawn`ed ahead of the
chat call so its latency hides underneath, and the assistant message id only
materialises afterward at the join point. Rather than defer the audit write
to that join point, the composer writes its event row first and returns the
row id, which is then stamped onto the assistant row as
`chat_messages.metadata.image.compose_event_id`. Reverse lookup goes
**assistant row → `compose_event_id` → this table**, never the other
direction; `chat_messages.metadata.image` otherwise keeps its existing
`compose_variant` / `compose_model` / `compose_generation_id` keys unchanged.
This direction also keeps the table reachable from callers with no message to
attach to at all (the standalone endpoint's `session_id` is NULL for exactly
that reason), and it widens coverage **on the chat path**: because
`build_delegated_image_prompt` writes its row before returning to its caller —
independent of the chat SSE stream — a chat turn whose image is never shipped
(client disconnect, a ghost fallback firing after the compose call returned)
still leaves its row behind — "model call paid for, no picture shipped"
becomes visible. **This guarantee does not extend to the standalone
endpoint's own streaming mode** (`compose_endpoint_stream`): its
`record_compose_event` calls live inside the SSE generator itself
(`routes/persona.rs::compose_stream`), so a client that disconnects before the
generator reaches one of those calls loses that row too, even though the call
was billed. The endpoint's non-stream mode (`compose_endpoint`) writes
synchronously before its HTTP response returns and is not exposed to this
gap.

### `engine.chat_vision_events`

One row per image-carrying, non-tipped chat turn **that reaches the
text-reply path**, recording the `chat_vision` describe call. A turn with no
image writes nothing — "carries an image" is the denominator, and a text turn
is not a missed describe. Neither does a turn that never reaches the
text-reply path at all: a ghosted turn, one routed to `product_qa`, or an
image-only reply all skip this write, because the describe never runs on
those paths either (running one on a ghost turn would waste a paid call).
Computing a describe success rate off this table means treating
"image-carrying turns that reached the text-reply path" as the denominator,
not every image-carrying turn — a query that doesn't exclude ghosted /
product_qa / image-only turns will overstate coverage.

| Column | Type | Meaning |
|---|---|---|
| `id` | `UUID` | |
| `user_id` | `UUID` | |
| `session_id` | `UUID` | |
| `message_id` | `UUID` | The `role='user'` row carrying the image. |
| `status` | `TEXT` | `ok` \| `exhausted` \| `not_configured`. |
| `image_url` | `TEXT` | |
| `vision` | `JSONB?` | The parsed describe (`description` / `ocr_text` / `people` / `scene`). Duplicates `chat_messages.metadata.vision` on success — the accepted price of this table answering "how many describes ran, on what, at what success rate" without joining `chat_messages` to establish a denominator. |
| `model` | `TEXT?` | The model that answered, on success. Also populated on `exhausted` when the last attempt answered but its content was unusable (`empty` / `unparseable` / `content_filter` / `blank_description` / `refusal_pattern`). NULL only when nothing ever answered — a provider status, a transport break or a timeout (`upstream_error` / `gateway_error`) — or `not_configured` (no call was made at all). |
| `usage` | `JSONB?` | Full unfiltered usage block, same rule as `chat_images_events.usage`. |
| `generation_id` | `TEXT?` | |
| `attempts` | `SMALLINT` | Models actually called off `[primary, ...fallback]`; `0` when `[tasks.chat_vision]` is not configured. |
| `last_failure` | `TEXT?` | NULL when `status = "ok"` or `"not_configured"`. Values: `upstream_error` \| `gateway_error` \| `empty` \| `unparseable` \| `content_filter` \| `blank_description` \| `refusal_pattern` — the last three are `image_vision_invalidity`'s existing reason strings, reused verbatim. The first two are the v1.4.0 **pointer values** that replaced `model_error` / `timeout`; they name which of the two columns below holds the per-hop detail (see [Failed attempts](#failed-attempts-llm_attempts--gateway_errors)). |
| `llm_attempts` | `JSONB?` | Every hop where the provider answered with a failure, `task = "chat_vision"`. NULL when there were none. |
| `gateway_errors` | `JSONB?` | Every hop where our path to the provider broke. NULL when there were none. Vision failures are audited here only — they never ride the chat stream's `final` frame: the describe is a fail-open pre-stage, and an exhausted chain simply keeps the turn text-only. |
| `created_at` | `TIMESTAMPTZ` | |

**Keeps `message_id`, deliberately breaking symmetry with
`chat_images_events`.** The two tables are not structurally alike: vision
runs *after* the `role='user'` row exists, so `message_id` is already in
hand, and there is exactly one call site with no prospect of a standalone
vision entry point. The user's accompanying text is **not** duplicated here —
`message_id` points at `chat_messages.content` for that.

Written even when the describe never ran — `not_configured`, `attempts = 0`
— because that is one of three reasons `chat_messages.metadata.vision` can be
absent on a turn, and this is the only way to tell it apart from "the
describe ran and failed on every chain model" (`exhausted`).

## Observability

On every successful OpenRouter call *other than* the primary chat reply
(`chat_companion`) and the voice turn (`chat_voice`), the engine logs an
`info`-level event carrying `generation_id`, `model`, and best-effort parsed
token/cost fields from `usage`. Those two highest-volume tasks never call
that logging helper — their own per-attempt log is a `stream_metrics` event
(`model` / `attempt` / `ttft_ms` / `total_ms` / `outcome`) with no
`generation_id` or cost breakdown. The `audit` object itself is not logged —
it is forwarded upstream and never echoed into engine logs.

## Why not persist?

The engine's persona / chat / affinity tables are the long-lived
contract. Audit context is intentionally ephemeral so callers can
experiment with `user` hashing, metadata schemas, and per-deployment
analytics without engine-side migrations or business logic.
