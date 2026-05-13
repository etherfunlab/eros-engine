# API reference

[English](api-reference.md) · [中文](api-reference.zh.md)

A live, browsable reference is at **`/docs`** on any running instance (Scalar UI generated from utoipa annotations).

This page is a hand-written summary of the endpoints worth knowing. The Scalar UI is the authoritative spec.

## Authentication

Every `/comp/*` endpoint requires `Authorization: Bearer <Supabase JWT>`. The JWT must be HS256-signed against `SUPABASE_JWT_SECRET`. The `sub` claim must be a UUID; that becomes the user_id for the request.

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
  "version": "0.1.0",
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

### `POST /comp/chat/{session_id}/message`

Synchronous chat turn. Blocks until the LLM responds.

```bash
curl -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{"content":"hi, what are you reading today?"}' \
  http://localhost:8080/comp/chat/<session_id>/message
```

```json
{
  "reply": "Bishop. The same volume I always come back to in March.",
  "lead_score": 4.2,
  "should_show_cta": false,
  "typing_delay_ms": 1340,
  "agent_training_level": 0.18
}
```

`reply: null` when the persona ghosted this turn (see [ghost mechanics](ghost-mechanics.md)). The HTTP status is still 200.

### `POST /comp/chat/{session_id}/message_async`

Same shape as `/message` but returns a `message_id` immediately. The LLM call runs in background; poll `/pending/{message_id}` until it's ready.

### `GET /comp/chat/{session_id}/pending/{message_id}`

```json
{ "ready": false }
```

or:

```json
{ "ready": true, "reply": { /* same shape as /message response */ } }
```

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
- `crates/eros-engine-server/src/routes/debug.rs` — affinity debug route
- `crates/eros-engine-server/src/routes/health.rs` — `/healthz`
- `crates/eros-engine-server/src/routes/s2s.rs` — `/s2s/*` handlers
- `crates/eros-engine-server/src/auth/s2s.rs` — HMAC canonical signing layout
- `crates/eros-engine-server/src/openapi.rs` — Scalar UI spec metadata
