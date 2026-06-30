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
  "version": "0.6.x",
  "timestamp": "2026-05-05T19:06:05.309302232+00:00"
}
```

`version` is the running build's crate version (compiled in from `CARGO_PKG_VERSION`), so it tracks the deployed release.

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

data: {"type":"final","lead_score":0.42,"should_show_cta":false,"agent_training_level":0.18,"filtered":false,"prompt_injected":null,"tier":null,"retries_chat":0,"retries_filter":0}
```

Frame fields worth noting:

- **`meta`** — `message_id`, `action_type`, `model` (the served model id; may be omitted), and `continues_from` (optional — the previous message id when this turn continues a retry chain).
- **`done`** — `truncated`, `usage` (after `OPENROUTER_USAGE_HIDDEN_KEYS` filtering; may be omitted), `generation_id` (optional OpenRouter id).
- **`final`** — turn summary: `lead_score`, `should_show_cta`, `agent_training_level`, plus `filtered` (bool — was the reply output-filtered), `prompt_injected` (array of the trait tags that injected this turn, or `null`), `tier` (echo of the request `tier`, or `null`), `retries_chat` (zero-based index of the chat attempt that succeeded), and `retries_filter` (index of the filter-model attempt that served).

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

**Optional: memory injection scope.** The body may include a `memory_scope`
string to control which memory layers are injected into the prompt. Accepted
values:

| Value | Injected |
|-------|----------|
| `full` | Full user profile (including intimate fields) + relationship memory |
| `neutral_and_relationship` | Neutral profile (city/occupation/MBTI only) + relationship memory **(default)** |
| `relationship_only` | Relationship memory only; no profile |
| `neutral_only` | Neutral profile only; no relationship memory |
| `insights_only` | Full user profile only (intimate fields included); no relationship memory |
| `none` | No memory injection |

> **Important (#40 mitigation):** The default `neutral_and_relationship` is
> intentionally narrower than the pre-#40 behavior (which injected everything).
> Omitting `memory_scope` is **not** equivalent to the old behavior — it
> applies the narrowed default. Use `full` explicitly if you need the
> full-injection behavior.

**Optional: affinity injection scope.** The body may include an
`affinity_scope` value to control which of the six affinity axes are injected
into the prompt. Accepted values:

- Named presets: `"bond"` **(default)** — warmth + intimacy + tension;
  `"chemistry"` — trust + intrigue + patience; `"bond_and_chemistry"` / `"full"` — all six axes; `"none"` — no affinity injection.
- Axis array: any subset of `["warmth", "trust", "intrigue", "intimacy", "patience", "tension"]`.

> **Important (#40 mitigation):** The default `bond` (3 axes) is intentionally
> narrower than the pre-#40 behavior (which injected all six axes). Omitting
> `affinity_scope` is **not** equivalent to the old behavior. Use
> `"bond_and_chemistry"` or `"full"` explicitly if you need all axes.

Example using both fields:

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "hi",
        "client_msg_id": "01J3333333333333333333333A",
        "memory_scope": "full",
        "affinity_scope": "bond_and_chemistry"
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

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

**Optional: tip.** The body may include `tips_amount_usd` (a finite number,
`> 0` and `≤ 1_000_000`) to mark this turn as a tip. The turn is persisted with
`role = gift_user`: if `content` is empty the stored content becomes
`(打赏 $<amount>)`, otherwise your `content` is kept. The tip amount rides to the
model so the persona can react in its reply, and it is echoed back on the BFF
history row (`tips_amount_usd`). A tip and an image cannot be sent on the same
turn. Replaces the old `POST /comp/chat/{session_id}/event/gift` route, which has
been removed.

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "",
        "client_msg_id": "01J3333333333333333333333A",
        "tips_amount_usd": 9.99
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

**Optional: image input (vision).** The body may include `image_url` — an
absolute `http(s)` URL with a host, no embedded whitespace, ≤ 2048 chars. When
present, the engine runs a vision *describe* pre-stage (the `chat_vision` task)
and feeds the description into the reply. `image_url` and `tips_amount_usd` are
mutually exclusive on a single turn. A malformed URL returns `400 BadRequest` as
a pre-stream error. Vision is active only if `[tasks.chat_vision]` is configured
with a non-blank `filter_prompt` (see [model-config.md](model-config.md)).

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "what is in this picture?",
        "client_msg_id": "01J3333333333333333333333A",
        "image_url": "https://example.com/cat.jpg"
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

**Optional: companion image reply.** The body may include an `image` object —
`ImageReplyParams` — to request or force a companion-generated image this turn.
Requires `[tasks.chat_image_generation]` to be configured (see
[model-config.md](model-config.md)); the executor is off by default. The `image`
block is also the per-turn opt-in: **omit it to suppress image generation for the
turn** (the PDE may then only `reply_text` / `ghost`), or send `image: {}` to
enable it with the config model. This lets a caller's own per-turn policy gate
images independently of the PDE's content decision.

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "give me a smile",
        "client_msg_id": "01J3333333333333333333333A",
        "image": {
          "force": true,
          "mode": "text_image",
          "style": "realistic",
          "model": "google/gemini-2.5-flash-image",
          "image_prompt": "warm candid selfie, soft indoor light",
          "aspect_ratio": "3:4",
          "resolution": "1024x1365",
          "face_ref_url": "https://cdn.example/aria_avatar.png"
        }
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

`ImageReplyParams` fields (all optional):

| Field | Type | Default | Notes |
|---|---|---|---|
| `force` | `bool` | `false` | Override the PDE decision for this turn — force an image. When `false` the PDE decides. |
| `mode` | `"text_image"` \| `"image_only"` | `"text_image"` | `text_image` = text reply + image; `image_only` = image only (no text). `image_only` permits an empty `content` field. |
| `style` | `"realistic"` \| `"semi_realistic"` \| `"anime"` | task `default_style` | One of the three engine-owned style presets. |
| `model` | `String` | config | Per-turn single-id override of the config `ModelSpec`. Wins over config `model` but falls back to config `fallback`. |
| `image_prompt` | `String` | PDE judge / user text | Subject for the forced path. On the PDE path the judge's own `image_prompt` is used. |
| `aspect_ratio` | `String` | task `default_aspect_ratio` | Allowed: `1:1`, `3:4`, `4:3`, `9:16`, `16:9`. Returns `422` if invalid. |
| `resolution` | `String` | task `default_resolution` | Model-specific hint (e.g. `"1024x1365"`). Shape-validated only — opaque beyond that. |
| `face_ref_url` | `String` | absent | image2image face/appearance reference (absolute `http(s)`, ≤ 2048 chars). Returns `422` if malformed. |
| `prev_image_url` | `String` | absent | The previously generated image, for iteration (absolute `http(s)`, ≤ 2048 chars; validated like `face_ref_url`). Used only when the PDE picks `image_ref = "previous"` (see below); otherwise ignored. Clients backed by a private object store should pass a short-lived signed URL — the engine does not fetch it; it embeds the URL in the OpenRouter body and the image provider fetches it at generation time. Returns `422` if malformed. |

**Reference selection (`image_ref`).** The PDE verdict carries `image_ref`
(`"face"` | `"previous"`, default `"face"`). At draw time the engine picks the
reference: `previous` + a present `prev_image_url` ⇒ iterate on the previous
image; otherwise fall back to `face_ref_url` (the avatar). The chosen kind is
recorded in `metadata.image`.

Validation: `force` + `tips_amount_usd` on the same turn → `422`. A malformed
`face_ref_url` or `prev_image_url`, unsupported `aspect_ratio`, or invalid
`resolution` shape returns `422 BadRequest` as a pre-stream error.

**`image` SSE frame** — emitted after the text's `done` frame when image generation
succeeds:

```text
data: {"type":"image","message_id":"01J...","data_url":"data:image/png;base64,...","mime":"image/png","image_prompt":"warm candid selfie, soft indoor light","model":"google/gemini-2.5-flash-image","generation_id":"gen-xyz"}
```

| Field | Type | Notes |
|---|---|---|
| `type` | `"image"` | Frame type discriminator. |
| `message_id` | `String` | Matches the `meta` frame's `message_id`. |
| `data_url` | `String` | Base64 data-URL of the generated image (`"data:image/png;base64,..."`). |
| `mime` | `String` | MIME type of the image (e.g. `"image/png"`). |
| `image_prompt` | `String` \| `null` | The subject used for generation (also persisted). |
| `model` | `String` \| `null` | Image model actually served. |
| `generation_id` | `String` \| `null` | OpenRouter generation id. |

**`image_pending` SSE frame** — emitted when the engine commits to generating an
image for a message, before generation begins. Start a "generating…" indicator:

```text
data: {"type":"image_pending","message_id":"01J..."}
```

| Field | Type | Notes |
|---|---|---|
| `type` | `"image_pending"` | Frame type discriminator. |
| `message_id` | `String` | The message the image is being generated for. For `reply_image` this is the *intended* image id; on failure the turn degrades to a different text message (see below). |

**`image_attempt` SSE frame** — emitted as the model fallback chain walks, one
per attempt as it begins:

```text
data: {"type":"image_attempt","message_id":"01J...","model":"google/gemini-2.5-flash-image","variant":"composed","index":1,"total":3}
```

| Field | Type | Notes |
|---|---|---|
| `type` | `"image_attempt"` | Frame type discriminator. |
| `message_id` | `String` | Matches the `image_pending` frame's `message_id`. |
| `model` | `String` | The model being tried for this attempt. |
| `variant` | `"composed"` \| `"original"` \| `"single"` | Which prompt variant is being tried (`single` = no compose retry). |
| `index` | `Number` | 1-based position in the attempt plan. |
| `total` | `Number` | Total planned attempts. |

**`image_failed` SSE frame** — emitted when image generation gives up. Clear the
pending state and render a "couldn't generate" state:

```text
data: {"type":"image_failed","message_id":"01J...","reason":"chain_exhausted"}
```

| Field | Type | Notes |
|---|---|---|
| `type` | `"image_failed"` | Frame type discriminator. |
| `message_id` | `String` | Matches the `image_pending` frame's `message_id`. |
| `reason` | `"chain_exhausted"` \| `"zero_images"` \| `"config_error"` | `chain_exhausted` = every candidate model failed; `zero_images` = a success response carried no image (defensive); `config_error` = no key / no models. |

**Full SSE frame sequences:**

- `reply_text_image`: `meta(action_type=reply_text_image) → delta* → done → image_pending → image_attempt* → (image | image_failed) → final`
- `reply_image` (success): `image_pending → image_attempt* → meta(action_type=reply_image) → image → done → final`
- `reply_image` (image failed): `image_pending → image_attempt* → image_failed → meta(action_type=reply_text) → delta* → done → final` — the turn degrades to a normal text reply with a **new** `message_id` (see below).
- `ghost`: `meta(action_type=ghost) → done → final` — no `delta`, no `model` in `meta`, `usage` and `generation_id` are `null` in `done`. The companion stayed silent this turn; no LLM was called.

**Failed-image client contract** — no new error frame is emitted on image failure.
The `meta` frame's `action_type` declares the intended shape:

- **`reply_text_image`** — the `image` frame arrives *after* `done`. If the stream
  reaches `final` with no `image` frame, the image generation failed (fail-open);
  the text was still delivered — render it.
- **`reply_image`** — `image_pending` and `image_attempt*` arrive first, carrying
  the *intended* image id `X`. On success, `meta(action_type=reply_image) → image
  → done` follow with that same `X`. On failure, an `image_failed` frame (also
  `X`) is emitted and the turn degrades to a normal text reply whose `meta` /
  `delta` / `done` carry a **different** `message_id` `Y` (`meta.action_type =
  reply_text`). A consumer clears the pending state for `X` on `image_failed`,
  then renders `Y` as an ordinary text message. (`X` is never persisted; the
  failure diagnostic is persisted on `Y`'s row.)

**Write-back endpoint** — after receiving the `image` frame, the client should
upload the `data_url` to its own storage and post the resulting URL back:

### `POST /comp/chat/{session_id}/message/{message_id}/image`

Stores the CDN URL of a companion-generated image. Called by the client after
uploading the received `data_url` to its own storage.

```bash
curl -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{"url":"https://cdn.example/gen/abc.png"}' \
  http://localhost:8080/comp/chat/<session_id>/message/<message_id>/image
```

Returns `204 No Content` on success. `url` must be an absolute `http(s)` URL
(≤ 2048 chars). Returns `422` on a malformed URL, `404` if `message_id` is not an
assistant row in this session, `403` if the session is not owned by the JWT user.
The call is idempotent — re-posting overwrites the same key.

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

`role` ∈ `user | assistant | gift_user | system_error`. `gift_user` is a tip
turn (sent via `tips_amount_usd` on the stream route, above).

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

> **Tips replaced gift events.** The standalone gift routes
> (`POST /comp/chat/{session_id}/event/gift`, `GET /comp/chat/{session_id}/gifts`)
> were removed. A tip is now part of a normal stream turn — set
> `tips_amount_usd` on `POST /comp/chat/{session_id}/message/stream` (see above).

## Debug

### `GET /comp/affinity/{session_id}`

Live 6-axis vector + Bond/Chemistry bars and labels + ghost stats + legacy
relationship label. Gated by `EXPOSE_AFFINITY_DEBUG=true` env var; returns 404
when disabled.

```json
{
  "warmth": 0.42,
  "trust": 0.08,
  "intrigue": 0.12,
  "intimacy": 0.05,
  "patience": 0.55,
  "tension": 0.04,
  "bond": 0.32,
  "chemistry": 0.28,
  "bond_label": "friend",
  "chemistry_label": "flirtation",
  "ghost_streak": 0,
  "total_ghosts": 0,
  "relationship_label": "friend",
  "updated_at": "2026-06-30T12:00:00.000000Z"
}
```

- `bond` / `chemistry` — bar values (0–1, curve-applied).
- `bond_label` ∈ `acquaintance | friend | close_friend | confidant`
- `chemistry_label` ∈ `spark | flirtation | crush | lover`
- `relationship_label` — legacy mapped value (`stranger | friend | slow_burn | romantic`; `frenemy` retired from emission).

Production deploys typically keep this off. Turn it on if your frontend wants
to render a live radar or inspect the derived lines.

### `GET /comp/affinity/{session_id}/event?limit=20&offset=0&event_type=message`

Paginated affinity **event log** for the session, newest first. Same
`EXPOSE_AFFINITY_DEBUG=true` gate as the vector route (404 when disabled). Each
entry carries the raw per-turn `deltas` (pre-EMA), the applied
`effective_deltas` (post-EMA), the folded `effective_deltas_computed`, and
`label_changes` when a tier crossed. Optional `event_type` filters the log;
`limit` defaults to 20 (capped at 100).

```json
{
  "events": [
    {
      "event_id": "…",
      "event_type": "message",
      "deltas":           { "warmth": 0.06, "trust": 0.02, "intrigue": 0.0, "intimacy": 0.0, "patience": 0.0, "tension": -0.02 },
      "effective_deltas": { "warmth": 0.03, "trust": 0.01, "intrigue": 0.0, "intimacy": 0.0, "patience": 0.0, "tension": -0.01 },
      "effective_deltas_computed": { "bond": 0.02, "chemistry": 0.006 },
      "label_changes": null,
      "created_at": "…"
    }
  ]
}
```

The `event_type` filter accepts `message | gift | proactive | ghost |
time_decay` (`time_decay` is reserved — not written by current code). For a
per-turn frontend surface that is **not** debug-gated and returns only the
latest event (post-EMA only), use the BFF route
`GET /bff/v1/comp/affinity/{session_id}/event` below.

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
`role` / `content` / `sent_at` (no `extracted_facts`), plus `tips_amount_usd`
on tip rows (present only when `role = gift_user`; omitted otherwise). `id` is the
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
    "effective_deltas_computed": {
      "bond": 0.013,
      "chemistry": 0.006
    },
    "label_changes": {
      "bond": { "from": "acquaintance", "to": "friend" }
    },
    "created_at": "…"
  }
}
```

`event` is `null` when there is no user-turn event yet (brand-new session,
or only time-decay), or when the latest event predates affinity migration
`0014`. `event_type` ∈ `message | gift | proactive | ghost`; a ghost turn
reports all-zero `effective_deltas`.

- `effective_deltas_computed` — exact floored per-turn line delta computed at
  persist time from the floored before/after bond/chemistry scores; read from
  the stored event column. Raw-composite units (not bar-percent). Good for a
  "+X bond / +Y chemistry" per-turn pulse. May be absent on pre-migration rows.
- `label_changes` — engine-authoritative tier transition (`null` / absent when
  no tier crossed this turn). Frontend stops computing this itself.

## Error responses

All errors are JSON with `{"error": "<code>", "message": "<human-readable>"}`:

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | Malformed body, invalid UUID, missing required field |
| 401 | `unauthorized` | Missing / malformed / expired / wrong-secret JWT |
| 403 | `forbidden` | Path-user vs JWT-user mismatch, or trying to read a session you don't own |
| 404 | `not_found` | Unknown session / persona / message id |
| 500 | `internal` | Anything else (DB error, LLM API error, etc.) |

## Source

- `crates/eros-engine-server/src/routes/companion.rs` — chat-lifecycle / profile handlers
- `crates/eros-engine-server/src/routes/companion_stream.rs` — streaming chat turn (`message/stream`), incl. tip + `image_url` handling
- `crates/eros-engine-server/src/routes/bff/companion.rs` — BFF `/bff/v1/comp/chat/*`
- `crates/eros-engine-server/src/routes/bff/affinity.rs` — BFF `/bff/v1/comp/affinity/*`
- `crates/eros-engine-server/src/routes/debug.rs` — affinity debug routes (vector + event log)
- `crates/eros-engine-server/src/routes/health.rs` — `/healthz`
- `crates/eros-engine-server/src/openapi.rs` — Scalar UI spec metadata
