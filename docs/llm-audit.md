# LLM audit passthrough

eros-engine exposes an opaque OpenRouter passthrough on the chat
message endpoints. Three caller-supplied fields ride to
`openrouter.ai/api/v1/chat/completions` unchanged, three OpenRouter wire
echoes come back on the sync response, and two deployer-set env vars
add app-attribution headers to every outbound call.

The engine never inspects content. PII scrubbing, hashing, and
metadata semantics are the caller's responsibility.

## Inbound: the `audit` request field

Both `POST /comp/chat/{session_id}/message` and
`POST /comp/chat/{session_id}/message_async` accept an optional `audit`
object:

```jsonc
{
  "message": "...",
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

Violations return `400 BadRequest`, and no user message row is persisted.

**Privacy:** do not put raw email / wallet address / real name in
`user` — send a hash. OpenRouter retains request metadata (token
counts, latency) but not prompts / responses by default.

## Outbound: the `usage` echo on the sync response

`POST /comp/chat/{session_id}/message` adds three optional fields to
its 200 body:

| Field           | Type      | Meaning                                                                                       |
|-----------------|-----------|-----------------------------------------------------------------------------------------------|
| `usage`         | `object?` | OpenRouter `usage` block verbatim (tokens / cost / cached / reasoning). Engine does not flatten. |
| `generation_id` | `string?` | OpenRouter `response.id`. Query `/api/v1/generation` with it for full request metadata later. |
| `model`         | `string?` | Model OpenRouter actually served. When `fallback_model` was hit, this is the fallback.        |

The async route (`/message_async`) and the polling route
(`/pending/{message_id}`) do **not** carry these fields. Use the sync
route if you need per-turn audit data.

### Hiding fields from the response

Deployers can strip specific top-level keys from the `usage` echo by
setting `OPENROUTER_USAGE_HIDDEN_KEYS` (comma-separated) on the server.
Typical use: hide wholesale `cost` / `cost_details` from downstream
customers without losing visibility for the operator.

```bash
OPENROUTER_USAGE_HIDDEN_KEYS=cost,cost_details
```

Behaviour:

- Applies to **both** the sync `/comp/chat/{id}/message` response and the
  SSE streaming `done` frame (`/comp/chat/{id}/message/stream`).
- The full unfiltered `usage` is still persisted to the DB; only the
  client-facing payload is filtered.
- Does **not** affect `tracing::info!` output — operator observability
  stays intact regardless of this setting.
- Async route (`/message_async`), polling route, and background paths
  (dreaming / post_process) already don't return `usage` to clients,
  so the env var has no effect on them.
- Only top-level keys are stripped; to suppress a whole subtree, list
  its parent key (`cost_details` removes the entire object, not just
  its members).
- Unset or empty → today's pass-through behaviour.

Background paths (`pipeline::dreaming`, `pipeline::post_process`) emit
usage only as `tracing::info!` fields:

```
openrouter: call completed session=… generation_id=… model=…
prompt_tokens=… completion_tokens=… total_tokens=… cost=…
```

## App-attribution headers

Two optional env vars add headers to every outbound OpenRouter call:

| Env                       | Header         | Purpose                                          |
|---------------------------|----------------|--------------------------------------------------|
| `OPENROUTER_APP_REFERER`  | `HTTP-Referer` | App identifier on OpenRouter dashboards          |
| `OPENROUTER_APP_TITLE`    | `X-Title`      | Display name in OpenRouter app analytics         |

Both unset → today's behaviour (no attribution headers). They are set
per deployment, not per request — App-Attribution is intended for
app-level aggregation. Per-user attribution belongs in `audit.user`.

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
