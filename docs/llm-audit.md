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

## Image-path event tables

Two append-only tables record every image-composer and `chat_vision`
describe call. Both are **best-effort telemetry, not a guaranteed ledger**:
the INSERT is awaited and its error is `warn!`ed and dropped — a failed audit
write costs the event row, never the turn (same discipline as
`companion_decision_events`). Neither table carries a foreign key: a row may
outlive or precede anything it refers to.

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
| `inputs` | `JSONB` | The five composer slots, structured: `{appearance, recent_scene, latest_user_msg, style, aspect_ratio}`. Empty slots are `""`, not the `（无）` placeholder the prompt renders — that substitution is a rendering detail, not an input. |
| `subject` | `TEXT?` | The composer's own `prompt` field. NULL unless `status = "ok"`. |
| `caption` | `TEXT?` | NULL when the composer produced none, including the non-JSON fallback reply, where the whole reply becomes `subject` instead. |
| `composed_prompt` | `TEXT?` | The assembled wire string — style preset + persona appearance + subject, i.e. exactly what the downstream consumer is handed. Stored on **every** row that produced one, including `exhausted` and `not_configured` on the chat path (the portrait fallback still assembles a wire prompt, and this column is then the only record anywhere of what was drawn). NULL only on the standalone endpoint's `exhausted` rows, which fail without assembling anything. |
| `variant` | `TEXT?` | The resolved `prompt_variant` key; `"raw"` is an ordinary key, not a skip. |
| `model` | `TEXT?` | The model that answered, on success. Also populated on the standalone endpoint's **streaming** mode when a candidate had already opened and started streaming before failing (`stream_died_midway`, or a post-open `empty`/`empty_prompt`) — that call may already be billed. NULL on every other failure: the chat path, the non-stream endpoint, and the streaming endpoint's own pre-open exhaustion (`stream_open_failed`) never produced a response to attribute usage to. |
| `usage` | `JSONB?` | Full unfiltered OpenRouter usage block, `serde_json::to_value`'d — `OPENROUTER_USAGE_HIDDEN_KEYS` filters the wire copy only, never this. Travels with `model`: populated exactly where `model` is. |
| `generation_id` | `TEXT?` | Travels with `model`. |
| `attempts` | `SMALLINT` | Models actually called off `[primary, ...fallback]`; `0` for `not_configured`. |
| `last_failure` | `TEXT?` | Why the last attempt failed; NULL when `status = "ok"`. Values: `model_error` \| `timeout` \| `empty` \| `empty_prompt` \| `stream_open_failed` \| `stream_died_midway`. A free column, not a CHECK — the vocabulary grows as new failure modes get labeled. `stream_open_failed` / `stream_died_midway` are specific to the standalone endpoint's streaming mode; the chat path and the endpoint's non-stream mode share one chain-walk and only ever report the other four. |
| `created_at` | `TIMESTAMPTZ` | |

`status` is deliberately three values: `exhausted` means "no usable compose
result from this call" for every reason including a mid-stream death on the
streaming endpoint, and `last_failure` carries the distinction.

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
that reason), and it widens coverage: a compose call that completes but whose
image is never shipped (client disconnect, a ghost fallback firing after the
call returned) still leaves its row behind — "model call paid for, no picture
shipped" becomes visible.

### `engine.chat_vision_events`

One row per image-carrying, non-tipped chat turn, recording the
`chat_vision` describe call. A turn with no image writes nothing — "carries
an image" is the denominator, and a text turn is not a missed describe.

| Column | Type | Meaning |
|---|---|---|
| `id` | `UUID` | |
| `user_id` | `UUID` | |
| `session_id` | `UUID` | |
| `message_id` | `UUID` | The `role='user'` row carrying the image. |
| `status` | `TEXT` | `ok` \| `exhausted` \| `not_configured`. |
| `image_url` | `TEXT` | |
| `vision` | `JSONB?` | The parsed describe (`description` / `ocr_text` / `people` / `scene`). Duplicates `chat_messages.metadata.vision` on success — the accepted price of this table answering "how many describes ran, on what, at what success rate" without joining `chat_messages` to establish a denominator. |
| `model` | `TEXT?` | NULL unless `status = "ok"`. |
| `usage` | `JSONB?` | Full unfiltered usage block, same rule as `chat_images_events.usage`. |
| `generation_id` | `TEXT?` | |
| `attempts` | `SMALLINT` | Models actually called off `[primary, ...fallback]`; `0` when `[tasks.chat_vision]` is not configured. |
| `last_failure` | `TEXT?` | NULL when `status = "ok"`. Values: `model_error` \| `timeout` \| `empty` \| `unparseable` \| `content_filter` \| `blank_description` \| `refusal_pattern` — the last three are `image_vision_invalidity`'s existing reason strings, reused verbatim. |
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
