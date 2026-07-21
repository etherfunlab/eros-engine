# LLM audit passthrough

eros-engine exposes an opaque OpenRouter passthrough on the streaming
chat endpoint. Three caller-supplied fields ride to
`openrouter.ai/api/v1/chat/completions` unchanged, three OpenRouter wire
echoes come back on the SSE `done` frame, and two deployer-set env vars
add app-attribution headers to every outbound call.

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

- Applies to the SSE streaming `done` frame
  (`/comp/chat/{id}/message/stream`).
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
`pipeline::world_director`) emit usage only as `tracing::info!` fields:

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

## App-attribution headers

Three optional env vars add headers to every outbound OpenRouter call:

| Env                         | Header                    | Purpose                                          |
|-----------------------------|---------------------------|--------------------------------------------------|
| `OPENROUTER_APP_REFERER`    | `HTTP-Referer`            | App identifier on OpenRouter dashboards          |
| `OPENROUTER_APP_TITLE`      | `X-OpenRouter-Title`      | Display name in OpenRouter app analytics         |
| `OPENROUTER_APP_CATEGORIES` | `X-OpenRouter-Categories` | Comma-separated marketplace categories           |

All unset → today's behaviour (no attribution headers). They are set
per deployment, not per request — App-Attribution is intended for
app-level aggregation. Per-user attribution belongs in `audit.user`.

`OPENROUTER_APP_CATEGORIES` is passed through verbatim; OpenRouter
silently ignores unrecognised values and only honours it when
`OPENROUTER_APP_REFERER` is also set.

Invalid values (control characters, non-ASCII outside header rules)
are dropped at construction time with a `tracing::warn!`; the client
still works.

## What the engine does NOT do

- **Persist.** No DB column stores `audit`, `usage`, or attribution.
  Surface fields only.
- **Hash.** The engine does not transform `user` — callers are
  responsible for sending a hash.
- **Sanitise.** `metadata` keys and values are size / shape-checked,
  not content-checked.
- **Interpret.** The engine does not group, aggregate, or alert on
  any audit field. Callers wire that themselves.

## Observability

When `audit` is supplied, the engine logs an `info`-level event with
`audit_user_present`, `audit_session_present`, and `audit_metadata_keys`
(keys only — never values). On every successful OpenRouter call the
engine also logs `generation_id`, `model`, and best-effort parsed
token/cost fields from `usage`.

## Why not persist?

The engine's persona / chat / affinity tables are the long-lived
contract. Audit context is intentionally ephemeral so callers can
experiment with `user` hashing, metadata schemas, and per-deployment
analytics without engine-side migrations or business logic.
