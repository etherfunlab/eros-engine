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
  "version": "1.0.x",
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
  "instance_id": "…",
  "persona_name": "Aria",
  "is_new": true
}
```

`is_new=false` if you call `/start` again with the same `genome_id` for the same user — the engine resumes the existing session rather than creating a duplicate.

Optional `channel` field: `"text"` (default) or `"voice"`. Start/resume is
channel-scoped — a voice-channel start never resumes a text session (and
vice versa). Voice clients must obtain their session here with
`"channel": "voice"` before calling the voice turn endpoint.

Optional `force_new` field: when `true`, skip resume entirely and always
create a fresh session (`is_new: true`), even if a resumable one exists for
this user × instance × channel. Default `false`/omitted keeps the normal
resume-or-create behavior. Recommended for voice calls — start every call
with `{"channel": "voice", "force_new": true}` so each call gets its own
session instead of continuing a previous one. `POST /comp/chat/start` has no
built-in rate limit, so deployments that expose `force_new` may want
request-level rate limiting downstream.

Optional `instance_id` field: an explicit `persona_instance` id. When absent,
the server picks (or auto-creates) the user's instance for the supplied
`genome_id`; `genome_id` is required only when `instance_id` is absent.

Optional `is_demo` field: marks the new session as a demo. Persisted to the
session's `metadata.is_demo` and read by the affinity pipeline to multiply
positive judge scores by `AFFINITY_DEMO_BOOST` (default `1.4`), so meters
move visibly within a demo's turn budget. Ignored when resuming an existing
session.

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

data: {"type":"final","filtered":false,"prompt_injected":null,"tier":null,"retries_chat":0,"retries_filter":0}
```

Frame fields worth noting:

- **`meta`** — `message_id`, `action_type`, `model` (the served model id; may be omitted), and `continues_from` (optional — the previous message id when this turn continues a retry chain). `action_type` is one of `reply` | `ghost` | `reply_image` | `reply_text_image` | `product_qa` (a plain-text reply is reported as `reply`, not `reply_text` — there is no `reply_text` on the wire). `product_qa` marks an out-of-character product answer routed by the PDE judge (see [model-config.md](model-config.md)); it is excluded from companion context/memory but reported the same way on both the live stream and replay. Clients must tolerate unknown `action_type` values (new ones may be added without a major-version bump).
- **`done`** — `truncated`, `usage` (after `OPENROUTER_USAGE_HIDDEN_KEYS` filtering; always present — `null` when not applicable), `generation_id` (OpenRouter id; always present — `null` when not applicable), and `ghost_fallback` (bool; omitted when `false`). `ghost_fallback: true` marks a reply that resolved empty and was delivered as a silent fallback — this is **not** an `action_type=ghost` turn, and it leaves the ghost counters untouched. The cause is recorded on the persisted row's `metadata.fallback_reason`. A turn that promises a photo (`action_type=reply_text_image`) is exempt: an empty text half is an image-only reply, not silence, so it reports `ghost_fallback: false`, carries no `fallback_reason`, and the trailing `image_request` still fires.
- **`final`** — turn summary: `filtered` (bool — was the reply output-filtered), `prompt_injected` (array of the trait tags that injected this turn, or `null`), `tier` (echo of the request `tier`, or `null`), `retries_chat` (zero-based index of the chat attempt that succeeded), and `retries_filter` (index of the filter-model attempt that served). No profile/lead signal rides this frame — `lead_score`, `should_show_cta`, and `agent_training_level` were removed (companion_insights teardown, spec 2026-08-11); read profile state from `GET /comp/user/{user_id}/profile` instead.

Concurrent active streams per user are capped at 3. The keep-alive heartbeat
(`: ping`) is emitted every 15 s so reverse-proxies don't time out the
idle connection.

Pre-stream errors (HTTP 4xx/5xx before the first SSE byte) carry a JSON
body with `code`, `message`, `user_message` and — for
`409 duplicate_in_progress` — an `original_user_message_id`. See the
[spec](superpowers/specs/2026-05-19-sse-streaming-chat-0.2-design.md#13-pre-stream-errors-http-status-json-body)
for the full code table.

**This endpoint is text-channel only.** A `session_id` belonging to a
voice-channel session is rejected with `409 wrong_channel` before any row is
persisted — it writes text-channel messages, and letting them land in a voice
conversation would interleave both channels in one transcript. Voice turns go
to [`POST /comp/voice/{session_id}/turn/stream`](#post-compvoicesession_idturnstream)
instead. The gate mirrors the voice endpoint's, so the two channels are
symmetric: neither endpoint will write into the other's sessions.

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

> **Since 1.3.0 the field is injection-only again.** The 3.1 write-side
> steering (1.2.1) is retired: `affinity_scope` gates prompt injection and
> `length_score` and has no effect on scoring. See
> [Affinity model → Scope steering: retired](affinity-model.md#scope-steering-retired).

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

**Optional: reply anchor (rewind).** The body may include
`reply_to_message_id` — the UUID of a `chat_messages` row in this session to
anchor this turn's context on. When it resolves, history rewinds to (and
includes) that message: rows sent after it are excluded from the prompt, and
the anchor is recorded on the persisted user row's
`metadata.reply_to_message_id`. A present-but-unresolvable id (unknown, or
belonging to another session) does not fail the request — history is dropped
for this turn (only the current message is in context) and the row's
`metadata.reply_to_error` is set to `"not_found"`. Omit the field for the
normal latest-history behavior.

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "wait, about that earlier plan",
        "client_msg_id": "01J3333333333333333333333A",
        "reply_to_message_id": "3cc06c53-9d2e-4f8a-b3c1-0a1b2c3d4e5f"
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
mutually exclusive on a single turn. A malformed URL returns `422 Unprocessable
Entity` (`code: "unprocessable"`) as a pre-stream error. Vision is active only
if `[tasks.chat_vision]` is configured with a non-blank `filter_prompt` (see
[model-config.md](model-config.md)).

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
The `image` block is also the per-turn opt-in: **omit it to suppress image
generation for the turn** (the PDE may then only `reply_text` / `ghost`), or
send `image: {}` to enable it with the engine's built-in defaults. This lets a caller's own
per-turn policy gate images independently of the PDE's content decision.

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "give me a smile",
        "client_msg_id": "01J3333333333333333333333A",
        "image": {
          "force": true,
          "style": "realistic",
          "aspect_ratio": "3:4"
        }
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

An `image` block signals the consumer handles image drawing this turn; the
engine composes the prompt and emits a single `image_request` frame (it never
draws on the chat stream).

`ImageReplyParams` fields (all optional):

| Field | Type | Default | Notes |
|---|---|---|---|
| `force` | `bool` | `false` | Override the PDE decision for this turn — the turn is always `reply_image` (image only, no text reply). Requires `[tasks.chat_image_prompt_compose]` to be configured (`422` otherwise), and `content` follows the ordinary non-empty rule. When `false` the PDE decides. A leftover `mode` key from the pre-1.0.1 contract deserializes and is silently ignored. |
| `style` | `"realistic"` \| `"semi_realistic"` \| `"anime"` | `"realistic"` | One of the three engine-owned style presets; `"realistic"` is the engine's built-in default. |
| `aspect_ratio` | `String` | none | Allowed: `1:1`, `3:4`, `4:3`, `9:16`, `16:9`; absent when omitted (PDE plan → request → absent). Returns `422` if invalid. |
| `prompt_variant` | `String` | none | Selects a `[tasks.chat_image_prompt_compose].filter_prompt` variant: an index (`"0"`, `"1"`) or a key (`"a"`, `"b"`), depending on how that task is configured (see [model-config.md](model-config.md)). `"raw"` carries no special meaning: it selects a prompt only if the deployment configures a variant under that literal key, exactly like any other name. An index/key that doesn't match — `"raw"` included — falls back to the engine's built-in composer prompt, never a `422` or other error. Ignored when the task isn't configured, or configures a single plain prompt. |

**Reference selection (`image_ref`).** The PDE verdict carries `image_ref`
(`"face"` | `"previous"`, default `"face"`) and rides on the `image_request`
frame (below) — the chat stream never resolves it to a URL itself. The
`previous`-with-no-image → `face` fallback, and the `face_ref_url` /
`prev_image_url` reference URLs, belong to the consumer's own image-vendor
call (the engine has no draw endpoint). The persisted `metadata.image` marker
records the composer's picture subject, the aspect ratio, and its `caption`
(the short line the composer returned alongside the prompt, or `None` when it
gave none — the chat history and the judge transcript read back only the
caption, never the long prompt), plus — only when the composer LLM call
succeeded — the audit trio `compose_variant` (the `filter_prompt` key/index
that was selected, absent for a plain or built-in prompt), `compose_model`,
and `compose_generation_id`. Absence of the trio means the turn had no
successful compose (fail-open degradation, or composer not configured). A
`compose_event_id` pointer is present whenever the audit write itself
succeeded — independent of whether the compose call did — and is the
reachable link to `engine.chat_images_events`, where the composed **wire**
prompt actually lives (the `metadata.image` marker never duplicates it); see
[LLM audit → Image-path event tables](llm-audit.md#image-path-event-tables).
The reference kind is not recorded.

Validation: `force` + `tips_amount_usd` on the same turn → `422`. `force`
while `[tasks.chat_image_prompt_compose]` is not configured → `422` (the
composer is the only prompt source; without it a forced image could only be a
generic portrait). An unsupported `aspect_ratio` returns `422 Unprocessable
Entity` (`code: "unprocessable"`) as a pre-stream error. All are pre-stream:
no user row is persisted.

**`image_request` SSE frame** — emitted once per image turn in place of any
in-engine draw. The engine composes the prompt; the consumer draws it via its
own image vendor (there is no engine draw endpoint). The chat stream itself
draws nothing, streams no image bytes, and persists no draw result.

```
data: {"type":"image_request","message_id":"01J...","composed_prompt":"5YaZ5a6e...","image_ref":"face","aspect_ratio":"3:4"}
```

| Field | Type | Notes |
|-------|------|-------|
| `type` | `"image_request"` | Frame type discriminator. |
| `message_id` | `String` | The real assistant `message_id`; key the draw and storage to it. |
| `composed_prompt` | `String` | base64(`STANDARD`, unwrapped) of the UTF-8 final wire prompt. Decode at the last hop and use verbatim as the provider text prompt — reconstruct no prompt logic. |
| `image_ref` | `"face"` \| `"previous"` | Which reference image the plan chose; the consumer resolves the actual URL. |
| `aspect_ratio` | `String` \| absent | The semantic aspect (`1:1`,`3:4`,`4:3`,`9:16`,`16:9`) or absent. The consumer owns aspect→resolution mapping; no width/height is sent. |

**Full SSE frame sequences:**

- image-only: `meta(reply_image) → done → image_request → final`
- text + image: `meta(reply_text_image) → delta* → done → image_request → final`
- `ghost`: `meta(action_type=ghost) → done → final` — no `delta`, no `model` in `meta`, `usage` and `generation_id` are `null` in `done`. The companion stayed silent this turn; no LLM was called.
- `product_qa`: `meta(action_type=product_qa) → delta* → done → final` — same shape as a normal text reply, streamed by an independent model chain (`[tasks.chat_product_qa]`) instead of `chat_companion`; persisted with `channel='product_qa'` and reported as `product_qa` again on replay.

The engine never draws and no draw-lifecycle frames exist: the consumer
receives `image_request` and calls its own image vendor.

### `GET /comp/chat/{session_id}/history?limit=20&offset=0`

Paginated message history, newest first. `limit` defaults to 20 (capped at 50).

```json
{
  "messages": [
    { "id": "…", "role": "assistant", "content": "Bishop.", "sent_at": "…" },
    { "id": "…", "role": "user",      "content": "hi…",     "sent_at": "…" },
    { "id": "…", "role": "assistant", "content": "…", "sent_at": "…", "channel": "product_qa" }
  ]
}
```

`role` ∈ `user | assistant | gift_user | system_error`. `gift_user` is a tip
turn (sent via `tips_amount_usd` on the stream route, above). Each entry also
carries an optional `channel` field — `"product_qa"` marks an
out-of-character product answer (excluded from companion context/memory,
same as its live-stream `action_type`); the field is omitted for normal
turns.

## Voice

### `POST /comp/voice/{session_id}/turn/stream`

Lean voice-channel turn: one transcribed user utterance in, one streamed
text reply out. STT and TTS are entirely the caller's job — the engine
never touches audio (see the
[voice-call parts design spec](superpowers/specs/2026-07-07-voice-call-parts-design.md)).

Returns `text/event-stream` with a reduced frame set: `delta`* then a
terminal `done`, or a single `error` — the same frame shapes as the chat
message stream above, but with **no** `meta` frame and no `action_type`.

The session must be a **voice-channel** session (`409 wrong_channel`
otherwise) — obtain one via `POST /comp/chat/start` with
`"channel": "voice"`. Voice is opt-in per deployment: without a
`[tasks.chat_voice]` block in the model config the endpoint returns
`501 voice_disabled`.

The prompt is lean but not memoryless: persona + voice directive + a
first-turn **bootstrap snapshot** (frozen once per session, then re-injected
verbatim every turn) + one relationship line derived from the session's
affinity (bond/chemistry tiers) + this turn's **recall block**. History is
the last 8 messages (4 exchanges) — shorter than the chat path's window,
since the bootstrap and recall carry the longer-range memory instead. A
voice **turn** writes no memories (no insight extraction, no vector
writes), but a finished **call** does: once the session goes idle, the
dreaming-lite sweeper distills its transcript into profile-layer memories,
so later calls and text chats can recall it. Operators opt out with
`DREAMING_VOICE_DISABLED=1` — see
[memory-layers.md](memory-layers.md#voice-turns).

**Bootstrap snapshot** (first turn only, then frozen into
`chat_sessions.metadata.voice_bootstrap` and replayed on every later turn —
the provider is stateless, so there is no "inject once" on the wire): a
`[关于他]` block of `human_insights` bullets (Neutral tier by default; see
`memory_scope` below) plus a `[上次通话]` block, the previous voice call's
last 8 messages rendered as a transcript. The two parts degrade
independently and silently — a failed assembly leaves the marker unwritten
so the next turn retries.

**Per-turn recall** (every turn, read-only): a small vector-search pass over
the same `companion_memories` layers the chat path uses, gated by
`memory_scope` and budgeted at 300 ms — a timeout or search failure just
drops the block for that turn, never an error. An utterance under 4
alphanumeric characters after stripping whitespace/punctuation (嗯 / 好啊 /
哈哈-style backchannels) skips recall entirely, with no embedding call.
Deployments can force recall off regardless of the request via
`[tasks.chat_voice] recall = false` (default `true` — see
[model-config.md](model-config.md)).

Body fields:

- `content` — the user utterance. Max 4096 chars.
- `client_msg_id` — 26..36 ASCII-printable chars (any UUID or ULID).
  Replaying the same `(session_id, client_msg_id)` is a conflict **only when
  the turn already produced something**: an assistant reply already exists
  (retrying would double-bill), or the turn was deliberately interrupted (see
  [`turn/interrupt`](#post-compvoicesession_idturninterrupt) below) — either
  way returns `409 duplicate`. With neither — an abnormal disconnect, or an
  upstream failure that exhausted every candidate model — the replay
  **regenerates**: it reuses the persisted user row and issues a fresh call
  rather than erroring. On that repair path the request body's `content` is
  **ignored**; the previously persisted utterance is authoritative (a
  mismatch is only logged as a warning, never rejected), because the
  per-turn recall embeds that text as its query and must not drift between
  attempts. This also means a retry after an `Error { retryable: true }`
  frame now actually succeeds, instead of being turned away by the very
  duplicate check the client was told it could pass.
- `affinity_scope` (optional) — same field name, value space, and default
  (`"bond"`) as the
  [chat message stream](#post-compchatsession_idmessagestream): a named
  value `"full" | "bond_and_chemistry" | "bond" | "chemistry" | "none"`,
  or an array of axis names such as `["warmth", "trust"]`. Voice injects
  at half granularity, so the resolved axes flatten to the two halves of
  the relationship line: any bond axis (warmth / intimacy / tension) ⇒
  the bond half, any chemistry axis (trust / intrigue / patience) ⇒ the
  chemistry half. The audit trail: the **user** row records the raw value
  under `metadata.affinity_scope_raw` (and `metadata.memory_scope_raw`),
  each only when the request carried the field; the **assistant** row
  keeps the resolved **`metadata.affinity_scope`** — the same 6-bool
  object (`warmth` / `trust` / `intrigue` / `intimacy` / `patience` /
  `tension`), byte-identical in shape to what the
  [chat message stream](#post-compchatsession_idmessagestream) writes —
  plus the resolved `metadata.memory_scope`.
- `memory_scope` (optional) — same field name, enum, and default
  (`"neutral_and_relationship"`) as the
  [chat message stream](#post-compchatsession_idmessagestream). On the
  session's **first successfully-assembling** turn, the resolved insight
  tier picks the bootstrap snapshot's `[关于他]` tier and is frozen for the
  rest of the call; every turn it also gates that turn's recall block.
  Later turns cannot change the snapshot's tier.

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{"content":"你今天在干嘛？","client_msg_id":"01JABCDEFGHJKMNPQRSTVWXYZ0","affinity_scope":"bond","memory_scope":"neutral_and_relationship"}' \
  http://localhost:8080/comp/voice/{session_id}/turn/stream
```

### `POST /comp/voice/{session_id}/turn/interrupt`

Reports a deliberate barge-in: the user started talking while the client was
still playing the companion's reply.

**This endpoint does not stop generation.** Aborting the client's SSE
connection to `turn/stream` already does that — the stream generator is
dropped at its current await point, which drops the upstream connection with
it. This endpoint's only job is to record **what was actually heard**, which
the aborted generator can no longer do itself: its persist step sits after
the streaming loop and never runs on a drop. Plain JSON in, plain JSON out —
this is not an SSE route.

**No `501 voice_disabled` gate**, unlike `turn/stream`. This endpoint makes
no LLM call, so gating it on `[tasks.chat_voice]` would make an in-flight
call's interrupt fail if the deployment's config changed mid-call.

Body:

```json
{ "client_msg_id": "01JABCDEFGHJKMNPQRSTVWXYZ0", "spoken_text": "你今天过得" }
```

- `client_msg_id` — the turn being interrupted, same 26..36 ASCII-printable
  format as `turn/stream`. It **must be the session's latest user turn** —
  see the guard below.
- `spoken_text` — what TTS actually played, verbatim. MAY be empty (the user
  cut in before any audible word); an empty string writes no assistant
  content at all — only the marker on the user row records that an interrupt
  happened. Max 4096 chars.

Response `200`:

```json
{ "message_id": "01JABCDEFGHJKMNPQRSTVWXYZ0" }
```

`message_id` is the assistant row that now holds the spoken text, or `null`
when nothing was played and no reply row exists to point at.

**Latest-turn guard.** A `client_msg_id` naming anything other than the
session's most recent user row is rejected with `409 not_latest_turn`.
Without this guard, the upsert below would let a client overwrite the
`content` of **any** past assistant reply — you can only barge in on what is
currently being spoken. This puts an ordering requirement on the client:
**send the interrupt before starting the next turn.** A late interrupt is
rejected and the turn simply degrades to the abnormal-disconnect state
(recoverable via `turn/stream`'s regeneration, described above) rather than
rewriting history.

**Upsert semantics (completion race).** The abort and the interrupt POST are
two separate round trips, so the server may not have processed the SSE
disconnect yet — `turn/stream`'s own post-stream persist can still land
concurrently. Both writers target the same assistant row (keyed on the user
row's id, `ON CONFLICT (user_message_id) WHERE role='assistant' AND
channel='voice'`), so exactly one row survives regardless of arrival order,
and its `content` always ends up as the interrupt's report:

| Assistant row | `spoken_text` | Result |
|---|---|---|
| absent | non-empty | inserted, `truncated = true` |
| absent | empty | no assistant row written |
| already exists (race) | non-empty | `content` overwritten, `truncated = true`; `model` / `usage` / `generation_id` and `affinity_scope` / `memory_scope` metadata preserved |
| already exists (race) | empty | `content` left untouched |

Repeated interrupt calls for the same turn are idempotent — the marker and
the upsert both key off the user row's id, so a retry cannot multiply rows.

**A race outside this table: two `turn/stream` generators for the same
turn**, e.g. an orphaned generator from a dead connection still alive
(TCP retransmission) when the client's retry starts its own. No interrupt is
involved, so none of the four rows above apply — there is no marker to make
one writer authoritative. That case is last-writer-wins on `content`,
`truncated`, and the audit columns together (never a mix of one generation's
text with a different generation's `generation_id`); see
`insert_voice_assistant_message` in `crates/eros-engine-store/src/chat.rs`.

Status ladder:

| Status | Code | When |
|---|---|---|
| 200 | — | interrupt recorded (see body above) |
| 400 | `invalid_payload` | `client_msg_id` outside 26..36 ASCII-printable chars |
| 401 | `unauthorized` | missing / malformed / expired / wrong-secret JWT |
| 403 | `session_forbidden` | session not owned by the JWT user |
| 404 | `session_not_found` | unknown `session_id` |
| 404 | `turn_not_found` | `client_msg_id` names no user row in this session |
| 409 | `wrong_channel` | session is not a voice-channel session |
| 409 | `not_latest_turn` | the named turn is not the session's latest user row |
| 422 | `unprocessable` | `spoken_text` longer than 4096 chars |

```bash
curl -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{"client_msg_id":"01JABCDEFGHJKMNPQRSTVWXYZ0","spoken_text":"你今天过得"}' \
  http://localhost:8080/comp/voice/{session_id}/turn/interrupt
```

## Persona

### `POST /persona/{instance_id}/image/compose`

Standalone image-prompt composition for a persona instance — a consumer that
wants a prompt for arbitrary text, not a chat turn. **No chat state is
touched — no session, no messages, no affinity runs, no memory is written.**
Every call is audited, though, with one caveat for streaming mode on client
disconnect (see below). The instance must belong to the JWT user (`403`
otherwise; `404` when it does not exist). Requires
`[tasks.chat_image_prompt_compose]` (`501 compose_disabled` without it).

The endpoint doubles as a composer test surface: the response carries `model`
and `generation_id`, and streaming passes the composer's raw output through
verbatim — the most common failure when tuning a `filter_prompt` is the model
not emitting valid JSON, and the operator needs to see what it actually
returned.

Body fields:

| Field | Type | Required | Notes |
|---|---|---|---|
| `content` | `String` | yes | Non-empty after trim, max 4096 chars. Lands in the `[对方最新消息]` composer slot. |
| `scene` | `String` | no | Lands in `[最近场景]`; omitted or blank ⇒ `（无）`. Max 8192 chars (`422` over). A composer *input*, not the prompt — the engine never copies it into `composed_prompt`; only the composer's own output is assembled. |
| `style` | `String` | no | Same three presets as the chat path; default `realistic`. |
| `aspect_ratio` | `String` | no | Same allow-list as the chat path; `422` on anything else. |
| `prompt_variant` | `String` | no | Same variant selection as the chat path, including the unknown-key-falls-back-to-built-in rule. |
| `stream` | `bool` | no | Default `true`. |

The composer payload is identical to the chat path's five slots, so one
`filter_prompt` contract serves both callers (see
[model-config.md](model-config.md)).

Every call — success or failure — is recorded in `engine.chat_images_events`
(`source = "compose_endpoint"` or `"compose_endpoint_stream"`): the non-stream
mode writes synchronously before its HTTP response returns, with no gap. The
streaming mode's writes live inside the SSE generator itself, so a client
that disconnects before the generator reaches one loses that row too, even
though the call was billed. See [LLM audit → Image-path event
tables](llm-audit.md#image-path-event-tables).

Both modes return the same five fields:

| Field | Meaning |
|---|---|
| `composed_prompt` | Style preset + persona appearance + subject — the string to hand an image vendor |
| `subject` | The composer's own prompt field, before assembly |
| `caption` | The composer's short caption, `null` when it produced none |
| `model` | The model that actually answered |
| `generation_id` | For reconciling against provider logs |

`stream: false` returns them as one JSON body. `stream: true` returns
`text/event-stream`:

```
data: {"type":"delta","content":"{\"prompt\":\"…"}
data: {"type":"done","composed_prompt":"…","subject":"…","caption":"…","model":"…","generation_id":"…"}
```

- `delta` frames carry the composer's raw output as it arrives, verbatim and
  unparsed.
- one terminal `done` frame carries the five fields — its payload minus the
  `type` discriminator is byte-identical to the `stream: false` body.
- a single `{"type":"error",…}` frame on failure after streaming has begun,
  matching the chat stream's in-band error shape (`code`, `retryable`,
  `message`, `user_message`).

There is no `meta` frame. A consumer that only wants the result ignores the
deltas and reads the terminal frame.

A successful-but-non-JSON composer reply keeps the chat path's behaviour:
`subject` is the whole raw reply, `caption` is `null`, and `composed_prompt`
is assembled from it as usual.

**Treat the output as model-generated, not sanitized.** "The engine never
copies `content` / `scene` into `composed_prompt`" is a routing property, not
a safety boundary: the composer is a language model reading caller-supplied
text, so those slots can steer it, and it can echo them back through the
`delta` frames and `subject`. The length caps bound cost, not influence. A
caller that forwards `composed_prompt` to an image vendor owns whatever policy
that vendor requires.

Failure modes:

| Condition | Response |
|---|---|
| `[tasks.chat_image_prompt_compose]` absent | `501 compose_disabled` |
| Instance not owned by the JWT user | `403` |
| Instance not found | `404` |
| Blank `content`, over-cap `scene`, bad `aspect_ratio` | `422` |
| Over the per-user in-flight cap (shared with chat/voice, ≤3) | `429` |
| Composer chain exhausted | `502` — `{"error":"upstream","message":…}` — or an in-band `error` frame if streaming has begun. **No portrait fallback here**: the fallback exists to keep a chat turn moving, and this endpoint has no turn to protect. |

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{"content":"在海边，黄昏","style":"realistic","aspect_ratio":"3:4","stream":false}' \
  http://localhost:8080/persona/{instance_id}/image/compose
```

## Profile

### `GET /comp/chat/{user_id}/sessions`

All chat sessions for `user_id`. The path's `user_id` MUST match the JWT's user_id; otherwise 403.

### `GET /comp/user/{user_id}/profile`

The flat, typed `human_insights` row for this user — the same columns the insight extractor UPSERTs incrementally after each turn. Same `user_id` equality check as above.

```json
{
  "user_id": "8a1f0c2e-4b6d-4f8a-9c31-2d5e7f0a1b3c",
  "city": "Hong Kong",
  "location": null,
  "hometown": null,
  "nationality": null,
  "occupation": "graphic designer",
  "mbti_guess": "INFP",
  "love_values": null,
  "emotional_needs": null,
  "life_rhythm": null,
  "interests": ["jazz", "long walks"],
  "personality_traits": [],
  "preferred_gender": null,
  "age_min": null,
  "age_max": null,
  "deal_breakers": [],
  "education": null,
  "family": null,
  "relationship_history": null,
  "social_pattern": null,
  "future_plans": null,
  "finance_status": null,
  "updated_at": "2026-08-11T12:00:00Z"
}
```

`updated_at: null` means the user has no `human_insights` row yet — no extraction has landed — and every other field is `null`/`[]` in that response. There is no aggregate "training level" score anymore; `agent_training_level` and the raw `companion_insights` JSONB were removed with the companion_insights teardown (spec 2026-08-11) — the typed columns above are the whole surface now.

### `GET /comp/instance/{instance_id}/profile`

The flat, typed `character_insights` row for one relationship (`persona_instances.id`) — the AI character's own conversation-derived profile, the mirror of the human profile above. The instance's `owner_uid` MUST match the JWT's user_id; otherwise 403. An unknown or archived (`status <> 'active'`) instance is 404. Experimental (v1.3.0) — see [2026-08-15-character-insights-design.md](superpowers/specs/2026-08-15-character-insights-design.md).

```json
{
  "instance_id": "8a1f0c2e-4b6d-4f8a-9c31-2d5e7f0a1b3c",
  "location": "the office, working late",
  "occupation": null,
  "current_situation": null,
  "desires": null,
  "vulnerabilities": null,
  "habits": null,
  "personal_values": null,
  "likes": [],
  "dislikes": [],
  "relationships": [],
  "updated_at": "2026-08-15T12:00:00Z"
}
```

`updated_at: null` means this instance has no `character_insights` row yet — the character extraction chain has not produced a result — and every other field is `null`/`[]` in that response, same convention as the human profile above. Results are database-only: nothing here is read back into any chat prompt.

> **Tips replaced gift events.** The standalone gift routes
> (`POST /comp/chat/{session_id}/event/gift`, `GET /comp/chat/{session_id}/gifts`)
> were removed. A tip is now part of a normal stream turn — set
> `tips_amount_usd` on `POST /comp/chat/{session_id}/message/stream` (see above).

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
- `force_new` — optional, same as canonical. Passed through to
  `StartChatRequest::force_new` — skip resume and always create a fresh
  session (`is_new: true`); recommended for voice calls (see the
  [voice section](#post-compvoicesession_idturnstream) above).

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
separately via the two affinity routes below, so a cold mount that does not
need a relationship pays nothing for one.

### `GET /bff/v1/comp/chat/{session_id}/history?limit=50&offset=0`

Slim history projection for the chat screen: `id` / `client_msg_id` /
`role` / `content` / `sent_at` (no `extracted_facts`), plus `tips_amount_usd`
on tip rows (present only when `role = gift_user`; omitted otherwise), and an
optional `channel` field — `"product_qa"` marks an out-of-character product
answer (excluded from companion context/memory); omitted for normal turns.
`id` is the
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
    { "id": "9f2e7a10-…", "client_msg_id": null,    "role": "assistant", "content": "beta",  "sent_at": "…" },
    { "id": "a1b2c3d4-…", "client_msg_id": null,    "role": "assistant", "content": "gamma", "sent_at": "…", "channel": "product_qa" }
  ],
  "total": 3
}
```

`total` is the count of `messages` in **this** response (`== messages.len()`),
not the grand total of rows in the session.

### `GET /bff/v1/comp/affinity/{session_id}/event`

Latest user-turn affinity delta (the applied per-axis change) **plus the
post-turn absolute state**, for per-turn frontend observation. JWT + ownership
checked.

Query parameters (both optional):

- `after` — long-poll baseline: the `event_id` the caller already has. While
  the session's latest turn event still matches it (or none exists yet), the
  request is held open until a newer event lands or `wait` elapses — a
  timed-out response returns the unchanged state, same shape as the immediate
  path. Absent ⇒ the latest event is returned immediately.
- `wait` — how long to hold the request open, in milliseconds. Only
  meaningful with `after`. Default 10000, server-capped at 25000.

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
      "bond": { "from": "friend", "to": "close_friend" }
    },
    "state_after": {
      "warmth": 0.31, "trust": 0.44, "intrigue": 0.40,
      "intimacy": 0.19, "patience": 0.27, "tension": 0.17,
      "bond": 0.42, "chemistry": 0.18,
      "bond_tier": 3, "chem_tier": 2,
      "warmth_grade": 2, "patience_grade": 2,
      "ghost_streak": 0, "total_ghosts": 2,
      "updated_at": "2026-08-17T14:02:11.412Z"
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
  the stored event column. Composite-score units — the same 0..1 scale as the
  snapshot's `bond`/`chemistry`. Good for a "+X bond / +Y chemistry" per-turn
  pulse. May be absent on pre-migration rows.
- `label_changes` — engine-authoritative tier transition (`null` / absent when
  no tier crossed this turn). Frontend stops computing this itself.
- `state_after` — the post-turn absolute state, read from the stored event
  column (absent on rows written before migration `0049`). This replaces
  client-side accumulation: adopt it as the new absolute value each turn
  instead of adding deltas to a running total. It is a **write-time**
  snapshot — after an absence only `GET /bff/v1/comp/affinity/{session_id}`
  is correct, because that route refreshes the derived endpoints at read.

### `GET /bff/v1/comp/affinity/{session_id}`

Absolute affinity for the session, **refreshed at read time** — the supported
way for a client to render a relationship. JWT + ownership checked, same
status codes as the event route above (404 unknown session, 403 someone
else's).

```json
{
  "session_id": "…",
  "affinity": {
    "warmth": 0.3106, "trust": 0.4402, "intrigue": 0.4024,
    "intimacy": 0.1901, "patience": 0.2740, "tension": 0.1703,
    "bond": 0.4213, "chemistry": 0.1802,
    "bond_tier": 3, "chem_tier": 2,
    "bond_label": "close_friend", "chemistry_label": "flirtation",
    "ghost_streak": 0, "total_ghosts": 2,
    "updated_at": "2026-08-17T14:02:11Z"
  }
}
```

`affinity` is `null` when the session has no affinity row yet — the row is
created on the first turn, so a just-started session legitimately has none.

- `bond` / `chemistry` — the real stored composite scores (0–1); no display
  curve (the pacing nonlinearity lives in the write-side tier decay — see
  [affinity-model.md](affinity-model.md)).
- `bond_tier` / `chem_tier` — 1..=5. Returned alongside the keys so a client
  needs neither the thresholds nor an ordered tier array. **Do not re-derive
  the tier from the score**; the thresholds are engine-owned and a local copy
  will drift.
- `bond_label` ∈ `acquaintance | friend | close_friend | confidant | soulmate`
- `chemistry_label` ∈ `spark | flirtation | crush | lover | beloved`

`apply_time_decay()` + `refresh_endpoints()` run before the response is
serialised, and that is the reason to call this rather than read
`engine.companion_affinity` directly: `warmth` and `patience` are derived from
the judge level, the counterpart line and the elapsed gap, with the stored
columns holding only a write-time cache. A direct `SELECT` returns a
relationship that reads warmer the longer the user has been away.

## Error responses

Most errors are JSON with `{"error": "<code>", "message": "<human-readable>"}`.
The streaming routes (`POST /comp/chat/{session_id}/message/stream`, `POST
/comp/voice/{session_id}/turn/stream`, and `POST
/persona/{instance_id}/image/compose`) are the exception: most of the
failure modes on all three routes use the `code` / `message` /
`user_message` shape described under "Pre-stream errors" above, with no
`"error"` key. `POST /comp/voice/{session_id}/turn/interrupt` shares that
same error body shape even though it is not itself a stream (its success
response is plain JSON) — it reuses the voice turn's precondition checks and
error type. The table below covers the plain shape:

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | Malformed body, invalid UUID, missing required field |
| 401 | `unauthorized` | Missing / malformed / expired / wrong-secret JWT |
| 403 | `forbidden` | Path-user vs JWT-user mismatch, or trying to read a session you don't own |
| 404 | `not_found` | Unknown session / persona / message id |
| 500 | `internal` | Anything else (DB error, LLM API error, etc.) |
| 502 | `upstream` | The upstream provider failed the call (currently only the persona compose endpoint — its composer chain was exhausted) |

## Source

- `crates/eros-engine-server/src/routes/companion.rs` — chat-lifecycle / profile handlers
- `crates/eros-engine-server/src/routes/companion_stream.rs` — streaming chat turn (`message/stream`), incl. tip + `image_url` handling
- `crates/eros-engine-server/src/routes/voice.rs` — voice-channel turn (`voice/{session_id}/turn/stream`)
- `crates/eros-engine-server/src/routes/persona.rs` — standalone image-prompt composition (`/persona/{instance_id}/image/compose`)
- `crates/eros-engine-server/src/routes/bff/companion.rs` — BFF `/bff/v1/comp/chat/*`
- `crates/eros-engine-server/src/routes/bff/affinity.rs` — BFF `/bff/v1/comp/affinity/*`
- `crates/eros-engine-server/src/routes/health.rs` — `/healthz`
- `crates/eros-engine-server/src/openapi.rs` — Scalar UI spec metadata
