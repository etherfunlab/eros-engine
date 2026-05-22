# API reference

[English](api-reference.md) · [中文](api-reference.zh.md)

A live, browsable reference is at **`/docs`** on any running instance (Scalar UI generated from utoipa annotations).

This page is a hand-written summary of the endpoints worth knowing. The Scalar UI is the authoritative spec.

## Authentication

Every `/comp/*` and `/bff/v1/*` endpoint requires `Authorization: Bearer <Supabase JWT>`. The JWT must be HS256-signed against `SUPABASE_JWT_SECRET`. The `sub` claim must be a UUID; that becomes the user_id for the request.

`/healthz` and `/docs` are public.

## Public endpoints

### `GET /healthz`

Liveness. No auth.

```bash
curl http://localhost:8080/healthz
```

```json
{
  "status": "ok",
  "service": "eros-engine",
  "version": "0.3.1",
  "timestamp": "2026-05-05T19:06:05.309302232+00:00"
}
```

## Personas

### `GET /comp/personas`

List active persona genomes. Auth required.

```bash
curl -H "Authorization: Bearer $JWT" \
  http://localhost:8080/comp/personas
```

```json
{
  "personas": [
    {
      "id": "11d6a45a-1fd9-4fe6-a943-3f049035eb68",
      "name": "Aria",
      "system_prompt": "…",
      "tip_personality": "warm-but-reserved",
      "avatar_url": "https://avatars.etherfun.xyz/aria.png",
      "art_metadata": { "age": 27, "mbti": "INFJ", "model": "x-ai/grok-4-fast", … },
      "is_active": true
    }
  ]
}
```

## Chat lifecycle

### `POST /comp/chat/start`

Open a new chat session against a persona genome. The server creates a `persona_instance` for `(genome_id, jwt_user_id)` if it doesn't already exist, then a `chat_session` referencing that instance.

```bash
curl -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{"genome_id":"11d6a45a-1fd9-4fe6-a943-3f049035eb68"}' \
  http://localhost:8080/comp/chat/start
```

```json
{
  "session_id": "5f7e…",
  "persona_name": "Aria",
  "is_new": true
}
```

`is_new=false` if you call `/start` again with the same `genome_id` for the same user — the engine resumes the existing session rather than creating a duplicate.

### `POST /comp/chat/{session_id}/message/stream`

Streaming chat turn. Returns `text/event-stream` with the
`meta → delta* → done → final` state machine described in the
[SSE streaming chat 0.2 design spec](superpowers/specs/2026-05-19-sse-streaming-chat-0.2-design.md).

The body **must** include `client_msg_id` (26..36 ASCII-printable chars,
any UUID or ULID). Replays of the same `(session_id, client_msg_id)` within
24 h reconstruct the original frames from the database without re-calling
OpenRouter.

```bash
curl -N -X POST \
  -H "Authorization: Bearer $JWT" \
  -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{"content":"hi","client_msg_id":"01J3333333333333333333333A"}' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

Sample frames (one JSON object per `data:` line):

```text
data: {"type":"meta","message_id":"01J...","action_type":"reply","model":"x-ai/grok-4-fast"}

data: {"type":"delta","message_id":"01J...","content":"你好"}

data: {"type":"done","message_id":"01J...","truncated":false,"usage":{"prompt_tokens":12,"completion_tokens":4,"total_tokens":16},"generation_id":"gen-abc"}

data: {"type":"final","lead_score":0.42,"should_show_cta":false,"agent_training_level":0.18}
```

Concurrent active streams per user are capped at 3. The keep-alive heartbeat
(`: ping`) is emitted every 15 s so reverse-proxies don't time out the
idle connection.

Pre-stream errors (HTTP 4xx/5xx before the first SSE byte) carry a JSON
body with `code`, `message`, `user_message` and — for
`409 duplicate_in_progress` — an `original_user_message_id`. See the
[spec](superpowers/specs/2026-05-19-sse-streaming-chat-0.2-design.md#13-pre-stream-errors-http-status-json-body)
for the full code table.

Once the first SSE byte has been written, terminal failures arrive as an
in-band `error` frame and the stream closes; the HTTP response has already
committed `200 OK`.

**Optional: tier selection.** The body may include a `tier` string —
type `String`, regex `^[a-z0-9_]{1,32}$` (returns `400` if malformed).
Selects the per-tier model and `allow_traits` from `model_config.toml`
(`[tasks.chat_companion.tiers.<tier>]`). An unknown or absent tier falls
back to the task default block (a warn is logged). Example:

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "hi",
        "client_msg_id": "01J3333333333333333333333A",
        "tier": "gold"
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

**Optional: per-request prompt traits.** The body may include a
`prompt_traits` array — see [prompt-traits.md](prompt-traits.md). Example:

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "hi",
        "client_msg_id": "01J3333333333333333333333A",
        "prompt_traits": [
          {"tag": "nsfw_boost", "text": "<your injection text here>"}
        ]
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

Limits: ≤ 8 entries, `tag` matches `[a-z0-9_]{1,32}`, `text` ≤ 2000 chars
(non-blank). Violations return `400 BadRequest` as a pre-stream error.

**Optional: OpenRouter audit passthrough.** The body may include an
`audit` object that rides directly to OpenRouter as wire-level `user` /
`session_id` / `metadata` — see [llm-audit.md](llm-audit.md). Example:

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "hi",
        "client_msg_id": "01J3333333333333333333333A",
        "audit": {
          "user": "u_<hash>",
          "session_id": "conv_xyz",
          "metadata": { "feature": "chat", "plan": "pro" }
        }
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

Caps: `audit.user` and `audit.session_id` ≤ 256 chars; `audit.metadata`
≤ 16 keys, key matches `[A-Za-z0-9_.-]{1,64}`, value is a string ≤ 512
chars. Violations return `400 BadRequest` as a pre-stream error.

### `GET /comp/chat/{session_id}/history?limit=50&offset=0`

Paginated message history, newest first.

```json
{
  "messages": [
    { "id": "…", "role": "assistant", "content": "Bishop.", "sent_at": "…" },
    { "id": "…", "role": "user",      "content": "hi…",     "sent_at": "…" }
  ]
}
```

`role` ∈ `user | assistant | gift_user | system_error`.

## Profile

### `GET /comp/chat/{user_id}/sessions`

All chat sessions for `user_id`. The path's `user_id` MUST match the JWT's user_id; otherwise 403.

### `GET /comp/user/{user_id}/profile`

Current `companion_insights` JSONB plus a weighted `training_level`. Same `user_id` equality check as above.

```json
{
  "insights": {
    "city": "Hong Kong",
    "occupation": "graphic designer",
    "interests": ["jazz", "long walks"],
    "mbti_guess": "INFP"
  },
  "training_level": 0.42
}
```

`training_level` is a weighted score across nine fields (city 0.05, occupation 0.05, interests 0.10, mbti_guess 0.15, love_values 0.15, emotional_needs 0.15, life_rhythm 0.10, personality_traits 0.15, matching_preferences 0.10). Weights sum to 1.0.

## Gift events

### `POST /comp/chat/{session_id}/event/gift`

Apply affinity deltas from an out-of-band event (a virtual gift, a reaction, anything you want to model as "this user did something nice"). The route writes a `chat_messages` row with `role='gift_user'` and applies the deltas via the affinity persistence path.

```bash
curl -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{
        "deltas": {"warmth": 0.05, "intimacy": 0.03, "tension": -0.02},
        "label": "rose",
        "metadata": {"source": "frontend-shop", "amount": 100}
      }' \
  http://localhost:8080/comp/chat/<session_id>/event/gift
```

The gift route does **not** invoke an LLM reaction in v0.1 (`reply` is `null`). The persona acknowledges the gift on the next user turn, where the new affinity state shapes the reply. A synchronous-reaction variant is a future enhancement.

### `GET /comp/chat/{session_id}/gifts`

List all gift events on this session, paginated.

## Debug

### `GET /comp/affinity/{session_id}`

Live 6-dim vector + ghost stats + relationship label. Gated by `EXPOSE_AFFINITY_DEBUG=true` env var; returns 404 when disabled.

```json
{
  "warmth": 0.42,
  "trust": 0.28,
  "intrigue": 0.61,
  "intimacy": 0.15,
  "patience": 0.55,
  "tension": 0.18,
  "ghost_streak": 0,
  "total_ghosts": 0,
  "relationship_label": "stranger",
  "updated_at": "2026-05-05T19:42:00.000000Z"
}
```

Production deploys typically keep this off (the affinity vector is part of the magic — exposing it ruins the illusion). Turn it on if your frontend wants to render a live radar of the vector.

## BFF (`/bff/v1/*`)

A frontend-shaped mirror of selected `/comp/*` routes for first-party
clients. Same Supabase JWT auth and the same per-user ownership checks as
the canonical routes — only the **response shape** differs (slimmer DTOs,
bundled payloads). Canonical `/comp/*` routes are never reshaped to fit a
frontend; a BFF route is added alongside instead. Three routes exist today.

### `POST /bff/v1/comp/chat/start`

Cold-mount bundle: resolves (or creates) the session **and** returns its
recent history in one round-trip, collapsing the frontend's separate
`start` + `history` calls. For the same user + input it resolves to the
exact same session as the canonical `POST /comp/chat/start`.

The body is the canonical start body plus one BFF-only field:

- `genome_id` / `instance_id` — identify the persona (same as canonical).
- `is_demo` — optional, same as canonical.
- `history_limit` — optional bundled-history page size; default 50, capped at 50.

```json
{
  "session_id": "5f7e…",
  "instance_id": "…",
  "persona_name": "Aria",
  "is_new": false,
  "history": [
    { "id": "3cc06c53-…", "client_msg_id": "c_abc", "role": "user",      "content": "hello",   "sent_at": "…" },
    { "id": "9f2e7a10-…", "client_msg_id": null,    "role": "assistant", "content": "hi back", "sent_at": "…" }
  ]
}
```

Affinity is intentionally **not** bundled here — the frontend reads it
separately (see the affinity event route below), which keeps bootstrap
independent of `EXPOSE_AFFINITY_DEBUG`.

### `GET /bff/v1/comp/chat/{session_id}/history?limit=50&offset=0`

Slim history projection for the chat screen: `id` / `client_msg_id` /
`role` / `content` / `sent_at` (no `extracted_facts`). `id` is the
`chat_messages` row primary key (UUID); `client_msg_id` is the id the FE
sent during streaming (`null` for rows that never carried one, e.g.
assistant turns). Same auth, ownership check, and
`limit ∈ [1, 50]` clamp as the canonical history route. **Intentional
divergence:** the default `limit` is 50 (the canonical route defaults to 20),
because the BFF exists for a cold mount that wants a full backscroll in one
round-trip.

```json
{
  "session_id": "…",
  "messages": [
    { "id": "3cc06c53-…", "client_msg_id": "c_abc", "role": "user",      "content": "alpha", "sent_at": "…" },
    { "id": "9f2e7a10-…", "client_msg_id": null,    "role": "assistant", "content": "beta",  "sent_at": "…" }
  ],
  "total": 2
}
```

`total` is the count of `messages` in **this** response (`== messages.len()`),
not the grand total of rows in the session.

### `GET /bff/v1/comp/affinity/{session_id}/event`

Latest user-turn affinity delta (post-EMA), for per-turn frontend
observation. Unlike the canonical `/comp/affinity/{session_id}` debug
route, this is **not** gated by `EXPOSE_AFFINITY_DEBUG` (the frontend owns
this surface) — but it is still JWT + ownership checked.

```json
{
  "session_id": "…",
  "event": {
    "event_id": "…",
    "event_type": "message",
    "effective_deltas": {
      "warmth": 0.03, "trust": 0.01, "intrigue": 0.0,
      "intimacy": 0.0, "patience": 0.0, "tension": -0.01
    },
    "created_at": "…"
  }
}
```

`event` is `null` when there is no user-turn event yet (brand-new session,
or only time-decay), or when the latest event predates affinity migration
`0014`. `event_type` ∈ `message | gift | proactive | ghost`; a ghost turn
reports all-zero `effective_deltas`.

## Error responses

All errors are JSON with `{"error": "<code>", "message": "<human-readable>"}`:

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | Malformed body, invalid UUID, missing required field |
| 401 | `unauthorized` | Missing / malformed / expired / wrong-secret JWT |
| 403 | `forbidden` | Path-user vs JWT-user mismatch, or trying to read a session you don't own |
| 404 | `not_found` | Unknown session / persona / message id |
| 500 | `internal` | Anything else (DB error, LLM API error, etc.) |

## Server-to-server (`/s2s/*`)

Mounted at `/s2s/*` and gated by HMAC-SHA256, not the Supabase JWT layer.
Intended exclusively for `eros-marketplace-svc`; see
[deploying.md](deploying.md#marketplace-coordination-optional) for env vars.
The OpenAPI spec at `/docs` is the authoritative reference; this section
is a quick orientation.

Four routes:

- `POST /s2s/ownership/upsert` — apply a single ownership change (NFT bought / sold).
- `GET  /s2s/ownership/since?cursor_ts=&cursor_pk=&limit=` — keyset-paginated pull of recent ownership rows.
- `POST /s2s/wallets/upsert` — apply a single wallet-link change (user linked / unlinked a wallet).
- `GET  /s2s/wallets/since?cursor_ts=&cursor_pk=&limit=` — keyset-paginated pull of recent wallet-link rows.

Example upsert bodies:

```json
// POST /s2s/ownership/upsert
{
  "asset_id":         "<base58 32-byte>",
  "persona_id":       "<base58 32-byte>",
  "owner_wallet":     "<base58 32-byte>",
  "source_updated_at": "2026-05-13T08:00:00Z"
}
```

```json
// POST /s2s/wallets/upsert
{
  "user_id":           "11d6a45a-1fd9-4fe6-a943-3f049035eb68",
  "wallet_pubkey":     "<base58 32-byte>",
  "linked":            true,
  "source_updated_at": "2026-05-13T08:00:00Z"
}
```

### HMAC headers

Each request must carry:

- `x-s2s-timestamp` — RFC3339, `±5 min` skew tolerated.
- `x-s2s-signature` — hex HMAC-SHA256 over the canonical signing string,
  using `MARKETPLACE_SVC_S2S_SECRET`.

The canonical signing string is a five-line ASCII layout (see
`crates/eros-engine-server/src/auth/s2s.rs` for the authoritative
definition and helper functions):

```
METHOD\n
path\n
canonical_query\n
timestamp\n
body_sha256_hex
```

where `canonical_query` is the request's query string with `&`-separated
pairs sorted lexicographically (empty if no query), and `body_sha256_hex`
is the lowercase hex SHA-256 of the raw request body (empty body still
hashes to the SHA-256 of zero bytes). Body is buffered up to 1 MiB; larger
requests are rejected with 413 without computing the hash.

During secret rotation both `MARKETPLACE_SVC_S2S_SECRET` and
`MARKETPLACE_SVC_S2S_SECRET_PREVIOUS` are accepted for inbound; outbound
calls always sign with the current secret only.

## Source

- `crates/eros-engine-server/src/routes/companion.rs` — handler implementations
- `crates/eros-engine-server/src/routes/bff/companion.rs` — BFF `/bff/v1/comp/chat/*`
- `crates/eros-engine-server/src/routes/bff/affinity.rs` — BFF `/bff/v1/comp/affinity/*`
- `crates/eros-engine-server/src/routes/debug.rs` — affinity debug route
- `crates/eros-engine-server/src/routes/health.rs` — `/healthz`
- `crates/eros-engine-server/src/routes/s2s.rs` — `/s2s/*` handlers
- `crates/eros-engine-server/src/auth/s2s.rs` — HMAC canonical signing layout
- `crates/eros-engine-server/src/openapi.rs` — Scalar UI spec metadata
