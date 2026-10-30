# API reference

[English](api-reference.md) · [中文](api-reference.zh.md)

A live, browsable reference is at **`/docs`** on any running instance (Scalar UI generated from utoipa annotations).

This page is a hand-written summary of the endpoints worth knowing. The Scalar UI is the authoritative spec.

## Authentication

Every `/comp/*` and `/bff/v1/*` endpoint requires `Authorization: Bearer <Supabase JWT>`. The token's `alg` header selects the validator: asymmetric signatures (ES256/RS256/EdDSA) are checked against the project's JWKS, resolved from `SUPABASE_JWKS_URL` or derived from `SUPABASE_URL` — this is the default for Supabase projects since the 2025 JWT Signing Keys rollout. Legacy HS256 against a shared `SUPABASE_JWT_SECRET` is still accepted for deployments that have not migrated; at least one of the two sources must be configured or the server fails to boot. The `sub` claim must be a UUID; that becomes the user_id for the request.

When the deployment sets `CALLER_SECRET`, every bearer-authenticated endpoint also requires `x-caller-secret: <one of its secrets>`. It is checked before the bearer: a missing or wrong value is `403` with `code: caller_forbidden` in the coded body, whether or not a token was sent. `/healthz`, `/docs` and `/svc/v1/*` never require it. See [Deploying](deploying.md).

`/healthz` and `/docs` are public. `/svc/v1/*` is a third class: admitted by a shared secret header, never by a bearer token — see [Service endpoints](#service-endpoints).

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
`instance_id` may name a bring-your-own-character instance (see
[byoc.md](byoc.md)): the session opens with no card and no caller secret, and
`persona_name` is `null` because only the card holds the name. The field is
always present in the response; it is a string for a genome instance.

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
any UUID or ULID). Replays of the same `(session_id, client_msg_id)`
reconstruct the original frames from the database without re-calling
OpenRouter; the key never expires.

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

The generation is detached from the connection: if the client disconnects mid-turn, the reply still completes and persists to history (pick it up via history or Supabase Realtime), and a turn that fails after disconnect usually leaves a `system_error` row (exceptions under **A failed turn in history** below). Stream turns are recorded in the same queue table as the async endpoint (single attempt, no background retry).

Frame fields worth noting:

- **`meta`** — `message_id`, `action_type`, `model` (the served model id; may be omitted), and `continues_from` (optional — the previous message id when this turn continues a retry chain). `action_type` is one of `reply` | `ghost` | `reply_image` | `reply_text_image` | `product_qa` (a plain-text reply is reported as `reply`, not `reply_text` — there is no `reply_text` on the wire). `product_qa` marks an out-of-character product answer routed by the PDE judge (see [model-config.md](model-config.md)); it is excluded from companion context/memory but reported the same way on both the live stream and replay. Clients must tolerate unknown `action_type` values (new ones may be added without a major-version bump).
- **`done`** — `truncated`, `usage` (after `OPENROUTER_USAGE_HIDDEN_KEYS` filtering; always present — `null` when not applicable), `generation_id` (OpenRouter id; always present — `null` when not applicable), and `ghost_fallback` (bool; omitted when `false`). `ghost_fallback: true` marks a reply that resolved empty and was delivered as a silent fallback — this is **not** an `action_type=ghost` turn, and it leaves the ghost counters untouched. The cause is recorded on the persisted row's `metadata.fallback_reason`. A turn that promises a photo (`action_type=reply_text_image`) is exempt: an empty text half is an image-only reply, not silence, so it reports `ghost_fallback: false`, carries no `fallback_reason`, and the trailing `image_request` still fires.
- **`final`** — turn summary: `filtered` (bool — was the reply output-filtered), `prompt_injected` (array of the trait tags that injected this turn, or `null`), `tier` (echo of the request `tier`, or `null`), `retries_chat` (zero-based index of the chat attempt that succeeded), `retries_filter` (index of the filter-model attempt that served), and — since v1.4.0 — `llm_attempts` / `gateway_errors` (both **omitted when empty**; see below). No profile/lead signal rides this frame — `lead_score`, `should_show_cta`, and `agent_training_level` were removed (companion_insights teardown, spec 2026-08-11); read profile state from `GET /comp/user/{user_id}/profile` instead.
- **`speaker_session_id`** (on `meta`, group turns only) — the member session that speaks this message; omitted on 1:1 turns, so 1:1 frames are unchanged.
- **`cast`**, **`speaker_error`** — group turns only; see [group-chat.md](group-chat.md#one-turn). A 1:1 stream never emits them.
- **`reasoning`** — `content`: a piece of the PDE judge's reasoning text (`{"type":"reasoning","content":"…"}`). Zero or more, sent first, ahead of `action_response`, `body_state` and the first `meta`. They appear whenever the judge's model returns reasoning; a body rule can turn reasoning on, and `reasoning = { enabled = false }` or `{ exclude = true }` turns it off (see [model-config.md](model-config.md)). When the judge returns none, the 1:1 frame sequence is unchanged. The content is display-only and decides nothing; whether to show it in full, collapsed or as a "thinking" indicator is the client's choice. When the judge falls back to another model, that model's reasoning keeps appending; no reset frame is sent. A group turn's director streams the same frame before `cast`.
- **`action_response`** — on a turn that carries an `action` only: `user_message_id` and `response` (`"accept"` | `"refuse"`). Sent once, before the first `meta`, and re-sent on an idempotent replay (`reasoning` frames are not persisted and are not re-sent). Clients must tolerate frame types they do not know. A turn that then ends in `error` did not take effect, and history carries no `response` for it.
- **`body_state`** — the persona's state this turn's reply was written with: `user_message_id`; `effects`, the accepted gives still at work, newest first, each `{item, name?, quantity, level, phase, step, steps}` (`level` 1–4, `phase` `rising` | `peak` | `fading`, `step` of `steps`), `[]` when none; and `needs`, `{hunger, social, stamina, emotional, outing, indulgence}` each `low` | `mid` | `high`, or `null` when there is no rating (needs exist only with the LLM judge, see [model-config.md](model-config.md#taskspde_decision--opt-in-llm-pde-judge)). Sent once, when effects are at work or needs are known, after `action_response` (if any) and before the first `meta`, and re-sent on an idempotent replay.

**A failed turn in history.** A turn that ends in `error` did not take effect: affinity did not move, and an action turn's user row carries no `response`. It usually also leaves a `system_error` notice whose `user_message_id` names that user row. It leaves none when a failed reply attempt was already saved before the turn failed: the reply chain ran out with no fallback phrase to serve (none is configured, or the chain is BYOK-only, see [byok.md](byok.md)), or the turn timed out after a hop had already failed. The queue row then settles `done`, and history keeps each failed attempt's assistant row: empty, or the partial text it streamed. An async turn runs the same pipeline but gets up to `CHAT_QUEUE_MAX_ATTEMPTS` attempts: one that fails before any reply attempt was saved is retried, and only its last attempt is settled as above. One that fails after a reply attempt was saved is not retried.

**Filtered turns stream too.** When the resolved LLM `output_filter` arms for
a turn (see [model-config.md](model-config.md)), the rewrite reaches the
client as ordinary `delta` frames, not as one block at the end — the engine
holds back the first 120 characters of the filter model's output long enough
to validate them, then streams the rest as it arrives. If an attempt fails
after it has already emitted text, the turn supersedes that bubble instead of
continuing it:

```text
data: {"type":"done","message_id":"01J...","truncated":true,"usage":null,"generation_id":null}

data: {"type":"meta","message_id":"01K...","action_type":"reply"}
```

— followed by a fresh `delta*` → `done` for the next attempt: either the next
filter model, or, if the chain exhausts, the fail-open original. `continues_from`
is absent here — unlike the retry-chain case above, the superseded partial was
never persisted, so there is no row to continue from. The superseded bubble
itself never appears in history on reload; only the reply that finally serves
does. `final.filtered` is `true` whenever the client received any rewritten
text this turn, including a bubble that was later superseded and fell open to
the original.

**`final.llm_attempts` / `final.gateway_errors` — the non-fatal failure
channel.** Every LLM attempt that failed this turn, across the five chains
whose failure changes what you received: the chat model chain, the input
filter, the output filter, the PDE judge, and the image prompt composer. Both
keys are omitted entirely when there were none, so a clean turn is byte-identical
to what you receive today. The element shapes are documented in
[llm-audit.md](llm-audit.md#failed-attempts-llm_attempts--gateway_errors) — the
frame serialises the identical structures the audit columns hold.

```text
data: {"type":"final","filtered":false,"prompt_injected":null,"tier":null,"retries_chat":1,"retries_filter":0,"llm_attempts":[{"task":"chat_companion","model":"x-ai/grok-4.20","http_status":529,"provider_code":"529","message":"code=529: Overloaded"}]}
```

> **Do not render these as an error.** A turn carrying them may have been served
> perfectly normally — the example above is a turn whose primary model returned
> `529` and whose fallback answered, which the user experienced as an ordinary
> reply. They exist so a consumer can alert and account without the reader
> seeing anything.

Three cases carry them: a **recovered** turn (a later hop served — the reply is
real and complete); a **pseudo-ghost** turn (the whole chain exhausted and a
canned phrase was served as a normal reply, `metadata.fallback_reason =
"stream_failure"` on the persisted row — to the end user this looks like an
ordinary short reply, and this frame is the only signal that it is not); and a
**garble-repaired** turn (`metadata.fallback_reason = "garble_repaired"`, the
reply salvaged from a garbled hop). `chat_vision` and affinity-eval failures
never ride this frame by design — both are fail-open and land in their own audit
tables. A **replayed** turn emits `final` with both lists empty, the same way it
recomputes `retries_chat` / `tier` / `prompt_injected` rather than reading them
off the row; query `engine.chat_messages` for a replayed turn's failures.

Concurrent active streams per user are capped at 3. The keep-alive heartbeat
(`: ping`) is emitted every 15 s so reverse-proxies don't time out the
idle connection.

Pre-stream errors (HTTP 4xx/5xx before the first SSE byte) carry a JSON
body with `code`, `message`, `user_message` and — for
`409 duplicate_in_progress` / `409 duplicate_failed` — an
`original_user_message_id`. `duplicate_in_progress` means the same
`(session_id, client_msg_id)` is still generating (back off and re-POST);
`duplicate_failed` means that turn terminally failed and a same-id retry can
never succeed — send a fresh `client_msg_id`. See the
[spec](superpowers/specs/2026-05-19-sse-streaming-chat-0.2-design.md#13-pre-stream-errors-http-status-json-body)
for the full code table.

**This endpoint is text-channel only.** A `session_id` belonging to a
voice-channel or group session is rejected with `409 wrong_channel` before any
row is persisted — it writes text-channel messages, and letting them land in a
voice conversation would interleave both channels in one transcript. Voice turns go
to [`POST /comp/voice/{session_id}/turn/stream`](#post-compvoicesession_idturnstream)
instead, and group turns to [`POST /v2/comp/group/{group_id}/message/stream`](#group-chat).
The gate mirrors the voice endpoint's, so the two channels are
symmetric: neither endpoint will write into the other's sessions.

Once the first SSE byte has been written, terminal failures arrive as an
in-band `error` frame and the stream closes; the HTTP response has already
committed `200 OK`.

**`error` frame fields.** `code`, `retryable`, `message`, `user_message`, plus —
since v1.4.0 — `upstream_status` and `provider_code`, both omitted when absent.

```text
data: {"type":"error","code":"rate_limited","retryable":true,"message":"…","user_message":"…","upstream_status":429,"provider_code":"429"}
```

`upstream_status` is the provider's own HTTP status, verbatim, and appears only
when the provider actually answered. A gateway-layer failure (timeout, transport
drop, decode failure) has no status the provider returned, so both fields are
absent there. `provider_code` is the provider's own error code when its body
carried one.

`code` is derived from the failure rather than hardcoded, which brings two values
alive that were declared and **never constructed** before v1.4.0:

| Failure | `code` |
|---|---|
| Provider answered `429` | `rate_limited` **(new)** |
| Provider answered any other status | `upstream_unavailable` |
| Gateway `open_timeout` / `total_timeout` / `idle_timeout` | `timeout` **(new)** |
| Gateway `config` (local misconfiguration) | `internal` |
| Gateway `transport` / `decode` / `chain_exhausted` | `upstream_unavailable` |
| Whole-turn generation budget exceeded (`CHAT_QUEUE_GEN_TIMEOUT_SECS`) | `generation_timeout` **(new)** |

Add arms for `rate_limited`, `timeout`, and `generation_timeout`; all three
carry `retryable: true`, so a default arm that reports a permanent failure is
now wrong. `generation_timeout` is distinct from `timeout`: `timeout` is a
single upstream call's own gateway-level deadline, while `generation_timeout`
is the detached stream task's whole-turn wall-clock budget expiring — it can
fire even when every individual upstream call so far succeeded, just
cumulatively too slowly.

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
| `full` | Full user profile (the neutral fields plus the intimate ones) + relationship memory |
| `neutral_and_relationship` | Neutral user profile + relationship memory **(default)** |
| `relationship_only` | Relationship memory only; no profile |
| `neutral_only` | Neutral profile only; no relationship memory |
| `insights_only` | Full user profile only; no relationship memory |
| `none` | No memory injection |

Relationship memory means the recalled relationship-layer lines plus, on 1:1 prompts, the relationship's arc, latest episodes, nearest journal moments and anniversary lines ([Memory layers → Relationship memory](memory-layers.md#relationship-memory)).

The neutral profile is 职业, 教育, 学历, MBTI, 意识形态, 作息, 社交模式, 性格特质 and 未来计划. The full profile adds 感情观, 感情经历, 兴趣, 情感需求, 家庭, 经济状况, 性癖 and 心理健康. See [Insights](insights.md#chat-prompt).

`memory_scope` is a ceiling. Within it, the relationship's affinity narrows what is injected: each intimate field needs a tier on its own affinity line, and how many profile-layer and relationship memories are recalled grows with the higher tier. A new relationship gets the neutral profile and one relationship memory. The arc, episodes and journal are not gated: they render at every tier. See [Memory layers → Affinity gate](memory-layers.md#affinity-gate).

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
> [Affinity model → Scope steering: retired](affinity-model.md#affinityrs-vs-scopers).

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

**Optional: quote.** The body may include `reply_to_message_id` — the UUID of a
`chat_messages` row in this session that this turn replies to. When it
resolves, the quoted line is rendered into the reply prompt as a `[quote]`
block (its text, whether the persona or the user said it, and how long ago),
and the anchor is recorded on the persisted user row's
`metadata.reply_to_message_id`, from which both history routes echo it back.

**A quote points at one line; it does not rewind the conversation.** The
history window is the same newest-N with or without it, so quoting something
from last week costs the model nothing that was said since. A
present-but-unresolvable id (unknown, or belonging to another session) does not
fail the request and does not change the context either: the turn simply runs
without a `[quote]` block, and the row's `metadata.reply_to_error` is set to
`"not_found"`.

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

**Optional: user locale.** Three optional strings tell the engine where the
user is. The prompt reads them from the turn alone; send them on every turn.
(The relationship scan keeps the last valid `user_timezone` a text turn
carried, to date its transcript and journal; see
[Memory layers → Relationship memory](memory-layers.md#relationship-memory).)

- `user_timezone` — IANA timezone (`Asia/Taipei`). The prompt's `[now]` clock
  uses the persona's own `art_metadata.timezone` first, then this, then
  `Asia/Singapore`. It also dates the user-side holiday line: holidays on the
  user's local date and the six days after it
  (`对方那边今天是平安夜；明天是Christmas Day。`), stated as facts, and the
  relationship's anniversary lines (`今天是你们认识满 3 个月的日子。`),
  which render only with it. The persona's birthday line (`3 天后是你的生日。`) is dated on the same
  persona clock as `[now]`.
- `user_country` — ISO 3166-1 alpha-2 (`TW`), as the client received it. Absent
  ⇒ derived from `user_timezone`. Unknown ⇒ no public holidays.
- `user_region` — ISO 3166-2 subdivision without the country prefix (`CA`,
  `ENG`). Read only beside `user_country`. Unknown ⇒ national holidays.

Invalid values never fail the turn: an unparseable timezone is treated as
absent. The raw values are recorded on the user row's metadata.

Holidays come from three sources: the thirteen Chinese lunisolar festivals
(`tyme4rs`, every user), the country's public holidays (`py-holidays-rs`,
in-lieu days off and political days — national, founding, war, revolution
and remembrance days, elections — dropped), and four fixed dates — 02-14 情人节
and 12-24 平安夜 for everyone, plus 01-01 新年快乐 and 12-25
`Season's Greetings` when none of that day's names contains `New Year` /
`Christmas`. The async endpoint and image edit accept the same three fields.

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

**Optional: action.** The body may include `action` — something the user does to
the persona besides talking. The persona accepts it or not; a refusal costs the
relationship one tier on the line the action touches (`give` and `gift` → bond, every
physical or custom action → chemistry), and a refused `tip` costs nothing (see
[affinity-model.md](affinity-model.md#refused-actions)).

| `type` | fields |
|---|---|
| `give` | `item`: one of the categories below (required); `name`: optional, 1–32 chars; `quantity`: optional, 1–99, default 1 |
| `kiss`, `hug`, `touch`, `lick` | none |
| `custom` | `text`: required, 1–100 chars |
| `tip` | `amount_usd`: required, finite, `> 0` and `≤ 1_000_000_000` |
| `gift` | `kind`: `item` \| `clothing` (required); `name`: required, 1–64 chars |

**What `give` hands over.** Consumables linger on the persona (below). Valuables do not, and they do not join her items: `gift` is the action for something she keeps. `cash` is money handed over in person, counted in notes; `tip` is the action that carries an amount.

| group | `item` (unit) |
|---|---|
| consumable | `cigarette` (支), `alcohol` (杯), `medicine` (片), `food` (份), `soft_drink` (瓶), `tea` (杯), `coffee` (杯), `herb` (包) |
| valuable | `watch` (块), `gem` (颗), `gold` (块), `camera` (台), `cash` (张), `antique` (件), `artwork` (件), `handmade` (份), `credit_card` (张), `flower` (朵) |

**Lingering effects of a `give`.** An accepted consumable keeps affecting the persona on the next `min(quantity, 5)` text turns of the session; the turn that accepts it is not one of them. Each turn the persona is at one step of a curve, at a level from 1 to 4:

| turns | front: `cigarette`, `food`, `soft_drink`, `tea` | back: `alcohol` | middle: `medicine`, `coffee`, `herb` |
|---|---|---|---|
| 1 | 3 | 1 | 3 |
| 2 | 3 1 | 1 3 | 3 3 |
| 3 | 3 2 1 | 1 2 3 | 1 3 1 |
| 4 | 3 2 2 1 | 1 2 2 3 | 1 3 3 1 |
| 5 | 3 3 2 2 1 | 1 2 2 3 3 | 1 2 3 2 1 |
| quantity > 5 | 4 3 3 2 1 | 1 2 3 4 4 | 1 3 4 2 1 |

A turn is a user message of the text session; product-QA messages and voice calls do not count, and there is no time limit. The state reaches the judge, the reply prompt and the image composer, and the caller through the `body_state` frame and [`GET /v2/comp/session/{session_id}/effects`](#get-v2compsessionsession_ideffects). A refused give has no effect. Design: [lingering effects spec](superpowers/specs/2026-10-12-given-item-lingering-effects-design.md), [give categories spec](superpowers/specs/2026-10-13-give-categories-design.md).

**A give of something she does not use.** The persona's `art_metadata.smoking`, `alcoholic`, `drug` and `coffee` say whether she smokes, drinks, uses weed and drinks coffee (table [below](#post-v2compsessionsession_idevents)). A `give` of `cigarette`, `alcohol`, `herb` or `coffee` whose key is `false` is rolled once before the judge runs. One time in three the turn goes on as usual. Otherwise the response is `refuse`, whatever the judge answers and with or without the LLM judge: the `action_response` frame and the `response` the history routes return say `refuse`, the give leaves no effect, and it costs the bond line a tier like any refused give. The judge is told the outcome (see [model-config.md](model-config.md#taskspde_decision--opt-in-llm-pde-judge)). A key that is `true` or absent never rolls, nor does any other item or action.

With an action, `content` may be empty, and it can be combined with
`image_url` and `image`. Every value violation (bounds, missing
`item`/`text`/`amount_usd`/`kind`/`name`) is a `422` with `code: "unprocessable"`; a body whose `type` or `item` is not one of the
listed values is rejected as unparseable JSON (also `422`). The action is stored on the user
row and returned by both history routes as `action`, with `response` once the
turn has taken effect.

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "来，陪我喝点",
        "client_msg_id": "01J4444444444444444444444A",
        "action": {"type": "give", "item": "alcohol", "name": "威士忌", "quantity": 2}
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

A tip never reaches a model as a number: the judge and the reply see what it
is worth (one of seven things it would buy, from a drink to a country) and how
much it feels like to the persona. The engine records `amount_usd` and never
verifies payment; what happens to the money of a refused tip is the caller's
decision.

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "",
        "client_msg_id": "01J3333333333333333333333A",
        "action": {"type": "tip", "amount_usd": 9.99}
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

An accepted `gift` joins the persona's items (`kind: "item"`) or wardrobe
(`kind: "clothing"`), and she knows it from the next turn on; a refused one
joins nothing. Like a tip, the engine trusts the field: a caller that sells
gifts must strip a client-supplied `gift` and settle a refused one itself.

**Optional: outfit.** `outfit` names what the persona wears this turn,
typically an entry of her wardrobe (see the items routes below); it is not
checked against it. 1–64 chars after trim, no line-break or control
characters (`422` otherwise); blank reads as absent. With an outfit, the
reply and any picture this turn wear it whatever the judge decides; without
one, the judge dresses her, preferring her wardrobe.

**Optional: image input (vision).** The body may include `image_url` — an
absolute `http(s)` URL with a host, no embedded whitespace, ≤ 2048 chars. When
present, the engine runs a vision *describe* pre-stage (the `chat_vision` task)
and feeds the description into the reply. A malformed URL returns `422 Unprocessable
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
| `force` | `bool` | `false` | Override the PDE decision for this turn — the turn is always `reply_image` (image only, no text reply), even while the persona is offline. Requires `[tasks.chat_image_prompt_compose]` to be configured (`422` otherwise), and `content` follows the ordinary non-empty rule. When `false` the PDE decides. A leftover `mode` key from the pre-1.0.1 contract deserializes and is silently ignored. |
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
The marker also records `image_ref` (`"face"` | `"previous"`), so the full
`image_request` payload stays recoverable after the fact (absent on rows
persisted before it was added).

Validation: `force`
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
| `companion` | `{member_session_id, instance_id}` \| absent | On a [group](group-chat.md#the-director) image turn whose director seated a second person: fetch that persona's face reference too and draw both. Absent on every 1:1 turn, on a group turn with one person in the picture, and when the composer fell back to the speaker's solo portrait. |

**Full SSE frame sequences:**

- image-only: `meta(reply_image) → done → image_request → final`
- text + image: `meta(reply_text_image) → delta* → done → image_request → final`
- `ghost`: `meta(action_type=ghost) → done → final` — no `delta`, no `model` in `meta`, `usage` and `generation_id` are `null` in `done`. The companion stayed silent this turn; no chat model was called.
- reasoning: when the judge returned reasoning, `reasoning*` precedes every shape in this list, ahead of `action_response` and `body_state`: `reasoning* → meta(reply) → delta* → done → final`
- action turn: the same shapes with `action_response` ahead of the first `meta` — `action_response → meta(reply) → delta* → done → final`; never `ghost` or `product_qa`
- a turn with lingering effects or known needs: `body_state` sits ahead of the first `meta`, after `action_response` when both are sent — e.g. `action_response → body_state → meta(reply) → delta* → done → final`
- `product_qa`: `meta(action_type=product_qa) → delta* → done → final` — same shape as a normal text reply, streamed by an independent model chain (`[tasks.chat_product_qa]`) instead of `chat_companion`; persisted with `channel='product_qa'` and reported as `product_qa` again on replay.

The engine never draws and no draw-lifecycle frames exist: the consumer
receives `image_request` and calls its own image vendor.

The frame itself is wire-only. A consumer that was not connected when it
fired — an async turn, or a stream client that disconnected mid-turn — can
recover the same payload afterwards via
[`GET /comp/chat/{session_id}/messages/{message_id}/image-request`](#get-compchatsession_idmessagesmessage_idimage-request):
an image turn is recognizable in history by `metadata.image` on the
assistant row.

**Optional: bring your own key.** The body may include `byok` — an end
user's own OpenAI-compatible endpoints, model selection, fallback chain,
`retry_depth`, `output_regex` and `fallback_to_platform` — so this turn's
reply runs on the end user's account. The engine honours it only when the
request also carries `X-Byok-Caller-Secret` matching the deployment's
`BYOK_CALLER_SECRET`; otherwise `403 byok_forbidden`. An invalid block is
`400 invalid_payload`. Both are refused before any row is written. See
[byok.md](byok.md).

**Optional: bring your own character.** The body may include `character` — a
character card sealed under the instance's key. It is required when the
session's instance is a BYOC instance and refused (`400 invalid_payload`) on
any other. The engine honours it only when the request also carries
`X-Byoc-Caller-Secret` matching the deployment's `BYOC_CALLER_SECRET`;
otherwise `403 byoc_forbidden`. A card that does not open under the instance's
key is `403 byoc_card_rejected`; one that is malformed or not a valid card is
`400 invalid_payload`. All are refused before any row is written. It combines
with `byok`: the card supplies the character, the `byok` block the reply
model. See [byoc.md](byoc.md) and [BYOC](#bring-your-own-character-byoc).

### `POST /v2/comp/session/{session_id}/message/async`

Enqueue-only chat turn — the async alternative to the stream endpoint above,
for callers that cannot hold an SSE connection open (bot gateways, background
senders). Same body (`StreamSendRequest`) and the same validation and
auth/ownership/`wrong_channel` checks as `message/stream` (`client_msg_id` is
required on both), and no reply is ever returned here: a background worker
drives the same generation pipeline and the reply lands in
`engine.chat_messages`, picked up via the history route or Supabase
Realtime.

The pre-rename path `POST /v2/comp/chat/{session_id}/message/async` was removed in 1.6.0 and now 404s (for an authenticated request; `require_auth` still 401s first) — see [`docs/migrating/v1-6-0-async-chat.md`](migrating/v1-6-0-async-chat.md).

```bash
curl -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{"content":"hi","client_msg_id":"01J3333333333333333333333A"}' \
  http://localhost:8080/v2/comp/session/<session_id>/message/async
```

| Case | Response |
|---|---|
| New message enqueued | `202 {"status":"queued","user_message_id":...}` |
| Redelivery of a still-pending/claimed `client_msg_id` | `202 {"status":"already_queued",...}` |
| Redelivery of an already-processed `client_msg_id` | `200 {"status":"already_completed",...}` |
| Redelivery of a terminally failed `client_msg_id` | `200 {"status":"failed",...}` — retry with a fresh `client_msg_id` |
| Per-session pending depth over `CHAT_QUEUE_PENDING_CAP` | `429 rate_limited` |
| Body carries `byok` | `400 invalid_payload` — BYOK is stream-only |
| Body carries `character` | `400 invalid_payload` — BYOC is stream-only |
| Session belongs to a BYOC instance | `400 byoc_unsupported` — its turn would run in a worker that never holds the card |
| Other pre-stream failures | same `StreamPreError` shape and codes as `message/stream` above |

Queue order is strict per-session LIFO — the newest pending message in a
session is processed first — and every enqueued message eventually gets a
turn; nothing is coalesced or dropped, only reordered. Worker config
(`CHAT_QUEUE_*` env vars): see
[Deploying → Operational notes](deploying.md#operational-notes). Design:
[async chat endpoint spec](superpowers/specs/2026-08-20-async-chat-endpoint-design.md).

### `POST /v2/comp/session/{session_id}/message/{message_id}/image/edit`

Revise a picture the character already sent. `{message_id}` is an image turn —
a message whose history entry carries `"image": true`. The engine composes a new
image prompt from that picture's subject plus the instruction, persists a new
image-only assistant message, and returns its `image_request` payload; the
consumer draws it, exactly as for a chat image turn.

```json
{
  "instruction": "换套衣服",
  "style": "realistic",
  "aspect_ratio": "3:4",
  "prompt_variant": "a",
  "persist_instruction": true,
  "evaluate_affinity": true,
  "reply_with_text": 0.4,
  "tier": "gold",
  "prompt_traits": [{ "tag": "allow_nsfw", "text": "…" }]
}
```

- `instruction` — required, non-blank, ≤4096 chars. What to change, in the
  user's words. Without `persist_instruction` it is an input to a picture, not
  a chat message: recorded on the audit row and never persisted as
  conversation.
- `style` — `realistic` (default) | `semi_realistic` | `anime`. Pass the style
  the source was drawn with; the engine does not record it on the message.
- `aspect_ratio` — `1:1` | `3:4` | `4:3` | `9:16` | `16:9`. Defaults to the
  source turn's.
- `prompt_variant` — selects a `[tasks.chat_image_edit_compose].filter_prompt`
  variant, with the same rules as the chat path's `image.prompt_variant`.
- `persist_instruction` — default `false`. When set, the instruction is
  persisted as an ordinary `role='user'` message quoting the source turn
  (`metadata.reply_to_message_id`, the same key the chat path writes), and the
  new image row hangs off it instead of inheriting the source's turn. History
  then replays the exchange: user instruction → assistant picture. The row is
  a real user message — visible to companion context, memory extraction and
  later affinity evaluation like anything else the user said.
- `evaluate_affinity` — default `false`. When set, the turn goes through the
  standard per-turn affinity judge: the instruction is what the user said, the
  new picture's caption is what the character answered. Detached — the response
  never waits on the judge. Orthogonal to `persist_instruction` (the judge
  reads the instruction either way); when an instruction row was persisted it
  anchors the affinity event's `user_message_id`. Overridden by
  `reply_with_text` — the full pipeline it starts already includes the judge.
- `reply_with_text` — absent by default; `0.0`–`1.0`, else 422. Requires
  `persist_instruction` (silently inert without it). When present, the edit is
  a **full chat turn**: the whole post-turn pipeline runs afterward — affinity,
  memory, insights, exactly as after a chat turn — and the value is the
  probability that the character also says something with the picture. `0`
  never (`reply_image` every time), `1` always (`reply_text_image` every
  time), in between the engine rolls — the dice stand in for the PDE, which
  this endpoint never consults. The text half is a real companion-model reply
  driven by the persisted instruction row (history, memory recall and
  relationship context included) and lands in the same assistant row as the
  picture, chat-shaped, after the same `output_regex` strip as a chat reply
  (see [model-config.md](model-config.md)). A failed or blank text half degrades the turn to
  `reply_image` — the picture is the turn's substance — and the row's failure
  columns keep the evidence.
- `tier` / `prompt_traits` — the text half's, with the chat path's meaning
  exactly: `tier` selects the `[tasks.chat_companion.tiers.<tier>]` model and
  `allow_traits`, and `prompt_traits` are injected after that gating (see
  [prompt-traits.md](prompt-traits.md)). `prompt_traits` are validated on every
  call (400 when malformed); both are inert unless `reply_with_text` rolls a
  text half. The assistant row's `metadata.prompt_traits` / `metadata.tier`
  carry the same audit copy as a chat turn.
- `user_timezone`, `user_country`, `user_region` — the chat body's user locale
  (see the stream endpoint). Only a rolled text half reads them; they are
  recorded on that row's metadata.
- `character` — a sealed character card, for a session whose instance is a
  BYOC instance: required there, refused on any other. Needs
  `X-Byoc-Caller-Secret`. The picture is revised from the card's `appearance`.
  See [byoc.md](byoc.md).

```json
{
  "message_id": "…",
  "edit_of": "…",
  "composed_prompt": "<base64>",
  "image_ref": "previous",
  "aspect_ratio": "3:4",
  "caption": "换了条裙子",
  "instruction_message_id": "…",
  "action_type": "reply_text_image",
  "reply_text": "新造型来啦，你看看喜欢吗"
}
```

`instruction_message_id` is present only when `persist_instruction` was set:
the persisted instruction message, so a client can render the user bubble
without refetching history.

`action_type` says what the turn is: `reply_text_image` when the
`reply_with_text` dice rolled a text half and it landed, `reply_image`
otherwise — including every call without `reply_with_text`. `reply_text` is
present exactly on `reply_text_image` turns.

`composed_prompt` is base64(STANDARD) of the UTF-8 wire prompt — the same
encoding as the SSE `image_request` frame and the recovery endpoint, so an
existing draw path consumes it unchanged. `image_ref` is always `previous`, and
on an edit turn that means **the `edit_of` picture**, not whatever the consumer
drew last.

The new message is an ordinary image turn: it appears in history with
`"image": true` and its prompt is recoverable via
`GET /comp/chat/{session_id}/messages/{message_id}/image-request`. Its
`metadata.image` additionally carries `edit_of`. An edit can itself be edited.

Nothing else about a turn runs on this call by default: no PDE decision, no
affinity movement, no insight or memory extraction. Two opt-ins widen that:
`evaluate_affinity` runs the affinity pass alone, nothing else; and
`reply_with_text` turns the whole call into a full chat turn (see above).
Without `persist_instruction` the
new row inherits the source's `user_message_id` — the edit belongs to the turn
the original picture answered; with it, the new row's `user_message_id` is the
persisted instruction message. A life event's picture (`POST …/events`) answers
no user message, so editing one needs `persist_instruction: true`; without it
the call is a `409`.

| Status | Meaning |
|---|---|
| 400 | malformed `prompt_traits`; `character` missing on a BYOC instance, present on any other, or not a valid sealed card (`invalid_payload`) |
| 401 | missing or invalid bearer |
| 403 | not your session; `byoc_forbidden` (the caller secret is missing or wrong); `byoc_card_rejected` (the card does not open under the instance's key) |
| 404 | unknown session, or no such message in it |
| **409** | the message exists but is not an image turn; or it answers no user message (a life event's picture) and `persist_instruction` is not set; or the session is a group session (`message` contains `wrong_channel`) |
| 422 | blank `instruction`, or unsupported `aspect_ratio` |
| 501 | neither `[tasks.chat_image_edit_compose]` nor `[tasks.chat_image_prompt_compose]` is configured |
| 429 | per-user in-flight cap reached (3, shared with chat/voice/compose) |
| 5XX | composer chain exhausted — the provider's own status and body, as on the compose endpoint. **No message is persisted**; retry is safe |

A body that fails to deserialize (missing `instruction`, wrong type for
`style`) or a malformed path UUID is rejected by axum's extractors before any
of the above, with a framework-shaped plain-text 400/422 — not the
`{"error", "message"}` shape this endpoint returns. The 422 cases in the table
above (blank `instruction`, unsupported `aspect_ratio`) are ones the request
deserialized successfully into, gated after ownership and state as described.

Requires `[tasks.chat_image_edit_compose]` **or** `[tasks.chat_image_prompt_compose]`:
with only the latter, edits run on the chat composer's chain using the engine's
built-in edit prompt.

### `POST /v2/comp/session/{session_id}/open`

Tell the engine the user just entered a session. The persona can speak first
for one of three reasons:

- **The persona's birthday — the engine's call.** Without `occasion`: when the
  persona's `art_metadata.birthday` (`"MM-DD"`) is today on the persona clock
  and nobody has spoken in the session since that local midnight, the persona
  speaks first. The persona clock is `art_metadata.timezone`, then
  `user_timezone`, then `Asia/Singapore`. A birthday outranks a holiday on the
  same day.
- **A holiday — the engine's call.** Without `occasion`: when it is a holiday
  in the user's timezone (same sources as the chat body's holiday line, today
  only) and nobody has spoken in the session since the user's local midnight,
  the persona speaks first. An opener from an earlier `occasion` call today,
  or a life event's message, counts as having spoken.
- **An occasion — the caller's call.** With `occasion`, the persona speaks
  first when the occasion is true of the session.

Either way one assistant message is generated, persisted and returned.
Otherwise `greeting` is `null`. It is also `null`, without generating, while
an offline life event of the persona is under way (see
[the events route](#post-v2compsessionsession_idevents)).

```json
{
  "user_timezone": "Asia/Taipei",
  "user_country": "TW",
  "tier": "gold",
  "memory_scope": "neutral_and_relationship",
  "occasion": "returning",
  "open_key": "visit-01J9ZK3"
}
```

Every field is optional. The three locale fields follow the chat body's rules.
`tier`, `prompt_traits`, `memory_scope`, `affinity_scope` and `audit` shape the
message as they shape a chat reply. The engine does not keep them between
calls (the message row records them as an audit copy), so send the same
values a chat turn would. A holiday needs a valid `user_timezone`; a birthday
does not. Without `occasion`, on a day that is neither, the call returns
`null`.

| `occasion` | The persona speaks first when | The engine tells the persona |
|---|---|---|
| `first_meet` | the session has no messages at all | you have not talked before |
| `returning` | the user has sent at least one message in the session | how many days since the user's last message |
| `just_opened` | always | the user just opened the chat |

`open_key` is required with `occasion` and rejected without it: 1–128
characters of `[A-Za-z0-9_.:-]`, scoped to the session. The same key returns
the same message, whatever `occasion` the repeat names; a new key can produce
a new one. How often and when to ask is the caller's policy — the engine sets
no cap, and it does not check whether a reply to the user's last message is
still on its way. Don't name an occasion while one is: the persona would
answer that message in the opener, and the real reply would follow. An
occasion that is not true of the session returns `null` without generating.

```json
{ "greeting": { "message_id": "01J…", "content": "好久不见，最近忙什么呢", "sent_at": "…", "occasion": "returning" } }
```

`occasion` in the response is `birthday`, `holiday`, `first_meet`,
`returning` or `just_opened`. The same message shape carries `event` on a life
event's message (see the events route below) and `life_choice` on her ask for
space and her answer to the user's reply (see the life-choice route below).

- **Synchronous:** one non-streaming generation, a few seconds. Don't block UI on it.
- **Idempotent:** per session and local date for a birthday or holiday
  greeting (the persona's date for a birthday, the user's for a holiday), per
  session and `open_key` for an occasion. Once a message is written, a
  repeat call returns it and does not generate again. A call that returned
  `null` wrote nothing, so a repeat can still generate.
- **The user wins a race.** If a message other than a birthday or holiday
  greeting lands in the session while an occasion's message is generating,
  nothing is written and the call returns `null` — or, when that message is a
  concurrent call's opener under the same key, returns it.
- **Persisted like any reply.** The message is an ordinary assistant row
  (`assistant_action_type = 'proactive'`, no `user_message_id`), unread until
  `POST /comp/chat/{session_id}/read`. Change feeds and both history routes
  see it.
- **Errors:** `400` with `code: "invalid_payload"` for `occasion` without
  `open_key`, `open_key` without `occasion`, or a malformed `open_key`; `422`
  for an unknown `occasion`. Session checks as on the stream endpoint (`403`,
  `404`, `409` for a voice or group session), `429` at the per-user in-flight cap. A
  failed generation returns the upstream error the image endpoints return and
  persists nothing; the next open (with the same key) retries. A BYOC
  instance's session is `400 byoc_unsupported`: the route needs a character
  the request does not carry.

### `POST /v2/comp/session/{session_id}/events`

Ask for something to happen in the persona's day. The caller decides when to
ask; the engine decides whether and what. It draws one kind of event, or none,
and the persona tells the user in one proactive message — except the offline
kinds, which make her unreachable and are announced only in part (below). Nothing
is drawn while she waits for the user's answer to her ask for space (see
[the life-choice route](#post-v2compsessionsession_idlife-choiceschoice_id)). While
an event lasts, the judge and the reply know she is in it (see
[model-config.md](model-config.md#taskspde_decision--opt-in-llm-pde-judge)).
How the draw works and how events and the persona's needs fit together is in
[life-events-needs.md](life-events-needs.md).

```json
{
  "event_key": "day-01J9ZK3",
  "kinds": ["weather_change", "animal_encounter", "business_trip"],
  "user_timezone": "Asia/Taipei",
  "tier": "gold"
}
```

| Field | Rules |
|---|---|
| `event_key` | required: 1–128 characters of `[A-Za-z0-9_.:-]`, scoped to the session. The same key returns the same event |
| `kinds` | optional. Absent or `null`: every kind. An array allows the listed kinds (`[]`: nothing is drawn). An object `{"allow": […], "disallow": […]}` allows what `allow` lists (absent or `null`: every kind; `[]`: none) minus what `disallow` lists; a kind in both is not drawn, and `{}` allows every kind. An unknown key in the object or an unknown kind is a `422`, and so is `taking_space` or `fell_out`: the engine places those itself and never draws them |
| `user_timezone`, `user_country`, `user_region`, `tier`, `prompt_traits`, `memory_scope`, `affinity_scope`, `audit` | as on `/open`, for generating the message |
| `image` | optional: `{"style", "aspect_ratio", "prompt_variant"}`, each as on a chat turn's `image` (no `force`; an unsupported `aspect_ratio` is a `422`). The caller handles images: a `taking_selfie` message then carries her picture, when `[tasks.chat_image_prompt_compose]` is configured (below). Absent, or no composer: words only. Every other kind ignores it |
| `outfit` | optional: what she wears, as on a chat turn — typically one of her wardrobe entries, not checked against it; 1–64 characters after trim, blank reads as absent, longer is a `422`. Her words follow it, and so does her picture. Absent: no outfit in her prompt, and the composer dresses her from the scene |

Kinds fall into four categories. `short` and `mid` are incidents; `body` and
`trip` are backgrounds that last days, during which incidents still happen.

| `category` | `kind` | Lasts | What happens to her |
|---|---|---|---|
| `short` | `police_encounter` | 30 min | the police stop her |
| `short` | `animal_encounter` | 1 h | she meets a stray animal |
| `short` | `hit_someone` | 1 h | she hit someone |
| `short` | `sudden_desire` | 1 h | she suddenly wants it |
| `short` | `grocery_shopping` | 1 h | she is out buying groceries |
| `short` | `showering` | 30 min | she is about to shower |
| `short` | `bathing` | 1 h | she is in the bath |
| `short` | `in_class` | 1 h | she is in class |
| `short` | `on_the_way` | 30 min | she is on her way somewhere |
| `short` | `period_scare` | 1 h | she thinks her period is late — a false alarm |
| `short` | `pregnancy_scare` | 1 h | a pregnancy test comes up positive — a false alarm |
| `short` | `got_bumped` | 30 min | someone or a vehicle brushes into her on foot |
| `short` | `bumped_someone` | 30 min | she knocks into someone, on foot or in whatever she rides or drives — a car, a horse, a flight, by her world |
| `short` | `quarrel` | 1 h | she had a row with someone |
| `short` | `scolded_by_superior` | 1 h | someone above her told her off — a boss, a manager, a master, a sovereign, by her world |
| `short` | `scolded_superior` | 1 h | she told off someone above her |
| `short` | `saw_bag` | 1 h | she has her eye on a bag (female persona) |
| `short` | `saw_clothes` | 1 h | she has her eye on a piece of clothing (female persona) |
| `short` | `admiring_scenery` | 1 h | she is taking in a view |
| `short` | `admiring_art` | 1 h | she is at an exhibition, in front of a piece |
| `short` | `taking_selfie` | 30 min | she has just taken a selfie and wants to show it; with `image` in the request, the message carries it |
| `mid` | `temp_work` | 4 h | she is called in to work |
| `mid` | `ex_message` | 3 h | her ex texts her |
| `mid` | `weather_change` | 4 h | the weather turns |
| `mid` | `got_hit` | 4 h | someone hit her |
| `mid` | `breakup` | 4 h | she just broke up |
| `mid` | `working_out` | 1–2 h | she is working out |
| `mid` | `reading` | 1–3 h | she is reading |
| `mid` | `gaming` | 2–4 h | she is gaming |
| `mid` | `away_from_phone` | 1–3 h | she is not looking at her phone (offline) |
| `mid` | `unwell` | 1–4 h | she feels unwell |
| `mid` | `irritable` | 2–4 h | she is irritable |
| `mid` | `busy` | 1–3 h | she is busy (offline) |
| `mid` | `in_battle` | 2–4 h | she is in the thick of a hard fight — a business dinner, a negotiation, or a real battle, by her world (offline) |
| `mid` | `asleep` | 1–4 h | she has fallen asleep (offline) |
| `mid` | `out_shopping` | 2–4 h | she is out shopping (offline for a female persona) |
| `mid` | `bros_gathering` | 2–4 h | she is at a gathering of brothers (offline) |
| `mid` | `sis_gathering` | 2–4 h | she is at a gathering of sisters (offline) |
| `mid` | `on_the_road` | 2–4 h | she is on a long stretch of travel: a commute, a coach, a train, a flight |
| `mid` | `watching_match` | 2–3 h | she is watching a match |
| `mid` | `watching_show` | 2–3 h | she is at a show |
| `mid` | `having_sex` | 1–2 h | she is having sex (offline, never announced) |
| `mid` | `want_to_go_out` | 1–3 h | she badly wants to get out |
| `mid` | `want_to_go_home` | 1–3 h | she badly wants to go home |
| `body` | `sick` | 1–3 d | she is ill |
| `body` | `period` | 3–7 d | her period |
| `body` | `hospitalized` | 2–5 d | she is in hospital |
| `body` | `post_surgery` | 3–7 d | she is recovering from surgery |
| `trip` | `business_trip` | 24 h | a business trip |
| `trip` | `short_trip` | 48 h | a short trip |
| `trip` | `long_trip` | 3–7 d | a long trip |
| `trip` | `hometown_visit` | 72 h | a visit home |

A range is drawn when the event starts. That is 52 drawable kinds. Both genders
draw both gatherings, at the same weight; that is deliberate.

Two more kinds are never drawn: `taking_space` (a separation she asked for and
the user agreed to, 12–24 h, offline) and `fell_out` (the falling-out when the
user ignores her ask, 1 h). The engine writes them for the
[life-choice flow](#post-v2compsessionsession_idlife-choiceschoice_id). They
show up as the `event` of `…/effects` and of the life-choice response, and
cannot be listed in `kinds`.

`sudden_desire`, `having_sex` and `pregnancy_scare`, which implies sex, carry adult themes;
a caller that serves them only to some users leaves them out with
`{"disallow": ["sudden_desire", "having_sex", "pregnancy_scare"]}`. `scolded_by_superior` and
`scolded_superior` assume someone above her; for a persona with no one above
her, the caller disallows them. A `disallow` list leaves every other kind in,
including kinds the engine adds later. An array or an `allow` list draws only
the kinds it names: a caller that allow-lists adds `want_to_go_out` and
`want_to_go_home` to draw them.

The draw:

- **Two stages.** The engine picks a category, then a kind within it, each by
  weight. Adding kinds to one category leaves the others' share alone.
- **One incident at a time, one background at a time.** Nothing is drawn while
  any session of the persona has an incident under way. While a background is
  under way, only incidents are drawn; in hospital or after surgery, not the
  ones that need her out and about (`temp_work`, `police_encounter`,
  `animal_encounter`, `hit_someone`, `got_hit`, `working_out`,
  `grocery_shopping`, `in_class`, `on_the_way`, `in_battle`, `got_bumped`,
  `bumped_someone`, `saw_bag`, `saw_clothes`, `out_shopping`,
  `bros_gathering`, `sis_gathering`, `admiring_scenery`, `admiring_art`,
  `watching_show`, `on_the_road`, `having_sex`); during `period`,
  neither `period_scare` nor `pregnancy_scare`.
- **While she waits for an answer, nothing is drawn.** From her ask for space
  until it is answered or lapses (24 h), the draw returns `{"event": null,
  "message": null}` for every session of the persona. A repeat of an
  `event_key` that already drew still returns its event.
- **Cooldowns.** Each kind has a cooldown from when it started. A trip starts
  at most once a week, and a `body` state at most once a fortnight, whichever
  kind. Any offline kind starts at most once every 12 hours, whichever kind.
- **Her needs weight the pick** within a category, when the deployment runs the
  LLM judge and the persona has a rating: a high `outing` favours
  `want_to_go_out`, a low one `want_to_go_home`, and a high `indulgence`
  favours `want_to_go_out` and `irritable` (the full table is in
  [life-events-needs.md](life-events-needs.md#1-needs-weight-the-kind-pick)).
- **Closeness gates.** `ex_message` and `breakup` are not drawn for someone she
  barely knows (intimacy rung 2 and up); `sudden_desire` only at the closest
  rung (3). `period` is drawn only for a persona whose `art_metadata.gender` is
  `"female"`. `period_scare` (rung 2 and up) and `pregnancy_scare` (rung 3) are
  also drawn only for such a persona, and so are `saw_bag` and `saw_clothes`;
  from rung 2 she says she wants what she found, in her `tip_personality`'s
  register when she has one. The offline kinds follow the two ghost gates
  (below).
- **Persona settings** in the genome's `art_metadata` shape the draw:

  | Key | Value | Effect |
  |---|---|---|
  | `prefer_events` | array of `mid` kinds | the listed kinds come up three times as often within `mid` |
  | `health` | `"weak"`, `"normal"` (default), `"strong"` | `body` states come up twice / once / half as often |
  | `prefer_trip` | `true` / `false` (default) | trips come up three times as often; still once a week at most |
  | `homesick` | `true` / `false` / absent | where a trip goes: `true` favours `hometown_visit` and thins the leisure trips, `false` thins `hometown_visit`; `business_trip` untouched |
  | `is_virgin` | `true` / `false` (default) | `having_sex` is a fifth as likely until her first `having_sex` event, after which the key no longer applies |
  | `sex_addict` | `true` / `false` / absent | `sudden_desire` comes up three times / a third as often; still only at the closest rung |
  | `smoking`, `alcoholic`, `drug`, `coffee` | each `true` / `false` / absent | whether she smokes, drinks, uses weed, drinks coffee: the more are `true`, the sooner her `indulgence` comes back to `high` and weighs the pick; a `false` key can make her refuse a `give` of that thing ([above](#post-compchatsession_idmessagestream)) |

  A value of the wrong shape, an unknown kind or a kind outside `mid` is
  ignored. A preference never lifts a gate.
- **A draw yields an event whenever a kind is eligible.** How eventful her day
  is follows from when the caller asks.

When nothing is drawn, the response is `{"event": null, "message": null}` and
no message is generated. Otherwise, one non-streaming generation, a few
seconds:

```json
{"event": {"kind": "business_trip", "category": "trip", "offline": false, "started_at": "2026-10-15T12:00:00Z", "ends_at": "2026-10-16T12:00:00Z", "message_id": "…"},
 "message": {"message_id": "…", "content": "要去外地出差一天，晚上再找你", "sent_at": "2026-10-15T12:00:00Z", "occasion": "event"}}
```

`message` has the `/open` greeting's shape, with `occasion: "event"`.
`started_at` is the message's `sent_at`; `ends_at` is that moment plus the
drawn length. An incident drawn during a background is told from inside it
(「出差途中遇到一只小猫」). `offline` is `true` for the offline kinds (below)
and `false` for every other kind.

**Her picture.** A `taking_selfie` drawn with `image` in the request, on a
deployment that configures `[tasks.chat_image_prompt_compose]`, is told in
words plus one delegated picture, like a chat turn's `reply_text_image`. Her
message is written first; the composer then reads it as the end of the recent
scene, so the picture shows what she says. No user message asked for it, so
the composer's latest-message slot is empty. A failed composer degrades to a
plain portrait prompt, as on a chat turn. The response gains `image_request`,
the payload of [`GET …/image-request`](#get-compchatsession_idmessagesmessage_idimage-request)
(base64 `composed_prompt`, `image_ref: "face"`, `aspect_ratio`); render it as
a turn's `image_request` frame. The same `event_key` returns it again. It is
absent when the message carries no picture, or when its composed prompt was
never recorded (the audit write is fail-open). The stored message is an image
row like any other: history flags it with `image: true`. Engine-side this is
one more serial model call on the route.

```json
{"event": {"kind": "taking_selfie", "category": "short", "offline": false, "started_at": "2026-10-26T09:00:00Z", "ends_at": "2026-10-26T09:30:00Z", "message_id": "…"},
 "message": {"message_id": "…", "content": "刚在海边拍的，好看吗", "sent_at": "2026-10-26T09:00:00Z", "occasion": "event"},
 "image_request": {"message_id": "…", "composed_prompt": "<base64>", "image_ref": "face", "aspect_ratio": "3:4"}}
```

**Offline kinds.** `away_from_phone`, `busy`, `in_battle`, `asleep`,
`bros_gathering`, `sis_gathering`, `having_sex` and, for a female persona, `out_shopping` make
her unreachable while they last; for any other persona `out_shopping` is an
ordinary incident (`"offline": false`, told at every rung) and she keeps
replying. The gatherings exist so that she has a life of her own and does not
orbit the user. `busy`, `in_battle` and a female persona's `out_shopping` are
announced in one message at intimacy rung 2 and up. Otherwise, and always for
`away_from_phone`, `asleep` and the gatherings, nothing is generated: the
response carries the event with `"message": null` and `"message_id": null`, and
`started_at` is the moment of the call.

```json
{"event": {"kind": "asleep", "category": "mid", "offline": true, "started_at": "2026-10-15T15:00:00Z", "ends_at": "2026-10-15T18:00:00Z", "message_id": null},
 "message": null}
```

While one lasts, every text turn in any session of the persona ghosts, without
the judge and without counting towards the ghost streak, and `/open` returns
`null`. An announced `busy`, `in_battle` or `out_shopping` still ghosts. A turn that carries a
user `action` (any `type`: `give`, `gift`, `tip`, `kiss`, `hug`, `touch`,
`lick`, `custom`) or `image.force = true` is the exception: it takes the normal
path and is answered, because an action is interaction in person, which needs
no phone, and a forced image turn is always `reply_image`. The persona goes
silent this way only where the ghost protection allows it, so with the
deployment's ghosting switch off (`ghosting = false`) no offline kind is drawn
and one already under way no longer silences turns, and none is drawn in a
session with fewer than 10 user messages; a caller that does not want them
leaves them out of `kinds`. A message ghosted this way carries no read receipt
(no judge read it), and it still carries none after the persona explains. Once
the event is over, in a session where the user's messages were ghosted, the
persona's next message — a reply, an `/open` greeting or an event message —
explains it in the kind's words: she was asleep, busy, in the thick of a hard
fight, shopping, at a gathering of her brothers or sisters, having sex, or not
looking at her phone, and only now sees what was sent. The
announcement of `busy`, `in_battle` or `out_shopping` does not count as the explanation: it was
written before the event ended. After she has written a message with text there,
that is done; an image-only reply does not count. The mechanics are in
[ghost-mechanics.md](ghost-mechanics.md#ghosting-the-engine-imposes).

**False alarms.** `period_scare` and `pregnancy_scare` are told as the scare
itself, in the persona's own words. While one lasts, nothing in the judge's or
the reply's context says it is false. The events and effects routes report its
`kind` (`period_scare`, `pregnancy_scare`) all the same, and the client decides
how to show it: showing the kind name tells the user it will turn out to be
nothing. Once it is over, her next message in the session that told it — a
reply, an `/open` greeting or an event message — says it was a false alarm.
After she has written a message with text there, that is done; an image-only
reply does not count. Her other sessions never carry it.

- **Idempotent per session and `event_key`.** A repeat call returns the same
  event and message, with no draw and no generation. A repeat that arrives
  while the first is still generating may generate once more, but only one
  event is written and both calls return it. A call that returned nulls wrote
  nothing, so a repeat can still draw.
- **Persisted like any reply.** The message is an ordinary assistant row
  (`assistant_action_type = 'proactive'`, no `user_message_id`), unread until
  `POST /comp/chat/{session_id}/read`. Change feeds and both history routes
  see it. The event itself is a row of `engine.persona_events`.
- **The user does not win a race.** An event does not yield to a user message
  that lands while it is generated: it happened whether or not the user
  spoke.
- **Next to the greetings.** An event message counts as a message for the
  `/open` birthday and holiday check, so neither greeting follows it on the
  same local day. An opener that is generating when an event message lands
  yields and returns `null`.
- **While it lasts.** The judge's context carries a `[近期事件]` line and the
  reply's `[now]` block a `- 正在经历：…` line, each listing the
  background and the incident under way; `/open` greetings carry the
  `- 正在经历：…` line too. `GET …/effects` shows both. After an offline kind
  has ended, in a session where it ghosted her messages, or a false alarm has
  ended, in the session that told it, the `[近期事件]` line adds an item for it
  and the reply a `- 刚才：…` line until she has written there once (a message
  with text; see
  [model-config.md](model-config.md#taskspde_decision--opt-in-llm-pde-judge)).
- **Errors:** `400` with `code: "invalid_payload"` for a malformed
  `event_key`; `422` for an unknown `kind`, or for `taking_space` or `fell_out`. Session checks as on `/open`
  (`403`, `404`, `409` for a voice or group session), `429` at the per-user
  in-flight cap. A BYOC instance's session is `400 byoc_unsupported`. A failed
  generation returns the upstream error the image endpoints return and
  persists nothing. A blank reply persists nothing and returns nulls.

### `GET /v2/comp/session/{session_id}/effects`

The persona's lingering state from accepted gives, her needs and the events under way, as a text turn sent now would carry them: for a page that opens on a conversation, and for async callers, which receive no frames. Right after the turn that accepted a consumable, this route already lists it at step 1, while that turn's `body_state` frame does not list it: the frame describes the reply just written, the route the persona as she is now. `needs` is the persona's needs carried to now (the same shape as on the `body_state` frame), or `null` with no rating or without the LLM judge. `event` is the incident under way now and `background_event` the background under way now (the same shape as on the events route), each `null` when there is none. An offline kind under way is reported as `event`, like any other incident, with `"offline": true`.

`changes_at` is the earliest time at which this response changes with no new turn and no new event: the next time a need steps toward its resting level, or the `ends_at` of `event` or `background_event`, whichever comes first. It is `null` when nothing changes on its own. Lingering effects move only with turns and do not count. `as_of` is the server time the response was computed for: the instant the carried needs and `changes_at` derive from. A client that keeps a copy of the response re-reads it after a turn, after an event draw, and when `changes_at - as_of` has elapsed since it received the response: the duration, not the absolute time, so the device clock does not matter.

```json
{"effects": [{"item": "alcohol", "name": "威士忌", "quantity": 3, "level": 2, "phase": "rising", "step": 2, "steps": 3}],
 "needs": {"hunger": "high", "social": "mid", "stamina": "low", "emotional": "mid", "outing": "high", "indulgence": "mid"},
 "event": null,
 "background_event": {"kind": "short_trip", "category": "trip", "offline": false, "started_at": "2026-10-15T08:00:00Z", "ends_at": "2026-10-17T08:00:00Z", "message_id": "…"},
 "as_of": "2026-10-16T13:00:00Z",
 "changes_at": "2026-10-16T14:00:00Z"}
```

`effects` is `[]` when nothing is at work. Errors as `/open`: `404 session_not_found`, `403 session_forbidden`, `409 wrong_channel` for a voice or group session.

### `POST /v2/comp/session/{session_id}/life-choices/{choice_id}`

Answer her ask for space. When chemistry runs two tiers past bond and her
patience stays low for three judged turns, the persona asks the user for a
break in a proactive message (`metadata.proactive = "life_choice"`) and the
session carries a pending choice for 24 hours; she has one such ask pending at
a time across all her sessions with the user. The user answers here, answers
in words, or says nothing. How and when she asks, and what each path costs, is
in [life-events-needs.md](life-events-needs.md#the-ask-for-space-choice).

A client learns of the choice from the `life_choice` field of
[`GET /bff/v1/comp/affinity/{session_id}/event`](#get-bffv1compaffinitysession_idevent),
which carries the choice's `id` and the answers it accepts. The ask lands
seconds after the turn that triggered it, so the delta that turn releases
usually misses it: **re-fetch the delta when a history row carries
`occasion: "life_choice"`** (both history routes expose the row's
`metadata.proactive` as `occasion`). A client that never adapts loses
nothing. It shows the ask as an ordinary message, the user replies in words,
and that is the ignore path (below).

```json
{
  "response": "accept",
  "user_timezone": "Asia/Taipei",
  "tier": "gold"
}
```

| Field | Rules |
|---|---|
| `response` | required: `"accept"` or `"decline"`; anything else is a `422` |
| `user_timezone`, `user_country`, `user_region`, `tier`, `prompt_traits`, `memory_scope`, `affinity_scope`, `audit` | as on `/open`, for generating her reply. The engine keeps nothing between calls |

1:1 text sessions only. One transaction answers the choice and pays for it:

- **`accept`:** the choice becomes `accepted`. A `taking_space` separation
  starts now and lasts 6 hours per tier of the gap she asked at (gap 2, 3, 4:
  12, 18, 24 hours); she is offline for all her sessions while it lasts, and an
  incident under way ends. Bond and chemistry each drop one tier, and for 72
  hours after the separation ends positive growth in this session is halved.
- **`decline`:** the choice becomes `declined`. The same tier drop; no
  separation and no slowdown.

Then she answers in one proactive message (a goodbye after an accept, a hurt
reply after a decline), persisted like any reply and returned:

```json
{"status": "accepted",
 "message": {"message_id": "…", "content": "好，那我们先各自安静一下。", "sent_at": "2026-10-20T12:00:00Z", "occasion": "life_choice"},
 "affinity": {"bond_tier": 1, "chem_tier": 3, "bond_label": "acquaintance", "chemistry_label": "crush", "…": "…"},
 "event": {"kind": "taking_space", "category": "mid", "offline": true, "started_at": "2026-10-20T12:00:00Z", "ends_at": "2026-10-21T06:00:00Z", "message_id": null}}
```

`status` is `accepted` or `declined`. `affinity` is the relationship after the
drop, in the shape of
[`GET /bff/v1/comp/affinity/{session_id}`](#get-bffv1compaffinitysession_id)'s
`affinity`. `event` is the separation an accept started and `null` after a
decline. `message` is `null` when her generation failed, came back blank, or a
newer message of the user's landed first: the answer, the separation and the
drop stand either way.

If the user does not call this route, two other things resolve the choice:

- **Ignore.** The first ordinary user text message that arrives while it is
  pending (a turn without an `action`) converts it: status `ignored`, and a
  one-hour `fell_out` incident starts at that message, so the very turn that
  ignored her is answered with the falling-out in her context. No tier changes.
  Only a message sent after her ask converts it. An action turn does not
  resolve it, and neither does `/open` or a reaction. During a separation a
  message only ghosts and never converts a pending ask.
- **Lapse.** 24 hours after her ask the choice reads as expired everywhere:
  `life_choice` is no longer attached, this route answers `409`, a later
  message converts nothing, and it costs nothing.

A resolution and an incoming message race transactionally; the first writer
wins and this route answers the loser `409`.

Errors. These refusals carry the pre-stream body `{code, message, user_message}`:

| Status | Code | When |
|---|---|---|
| 400 | `invalid_payload` | malformed `tier`, `prompt_traits` or `audit` |
| 400 | `byoc_unsupported` | the session belongs to a BYOC instance |
| 403 | `session_forbidden` | the session is not the caller's |
| 404 | `session_not_found` | unknown session |
| 404 | `life_choice_not_found` | the choice is not a choice of this session |
| 409 | `wrong_channel` | a voice or group session |
| 409 | `life_choice_closed` | the choice is already resolved (by this route, or by the user's message), or lapsed |

`422` for a `response` that is not `accept` or `decline`, or another malformed
field; `429` at the per-user in-flight cap, taken before anything is written, so
the choice stays open.

### `PUT /v2/comp/session/{session_id}/message/{message_id}/reaction`

React to one of the persona's messages with one emoji. Body `{"emoji": "👍🏽"}`;
the emoji must be exactly one standard emoji (skin-tone variants and ZWJ
sequences included) and inside the deployment's allowed range
(`GET /v2/comp/reactions`). One reaction per message: sending another emoji
replaces it; re-sending the current one changes nothing.

```json
{"message_id": "…", "emoji": "👍🏽", "reacted_at": "2026-10-05T12:00:00Z"}
```

`emoji` comes back fully qualified (`❤` is stored as `❤️`). The persona sees
reactions set since the user's previous message on the next text reply turn;
a reaction never triggers a reply and never moves affinity.

Errors: `400` (not exactly one standard emoji, or not allowed), `403` (not your
session), `404` (session or message not found), `409` (not an assistant message;
or a group session, with a `message` containing `wrong_channel`).

### `DELETE /v2/comp/session/{session_id}/message/{message_id}/reaction`

Clears the reaction. `204`, also when there was none. Same `403` / `404` / `409`.

### `GET /v2/comp/reactions`

The allowed range: `{"allowed": null}` when every standard emoji is allowed,
otherwise `{"allowed": ["👍", "❤️", …]}` — base emoji in the deployment's
order; every skin tone of a listed emoji is allowed.

### `GET /comp/chat/{session_id}/history?limit=20&offset=0`

Paginated message history: the newest `limit` rows (`offset` pages further back), returned oldest first. `limit` defaults to 20 (capped at 50).

```json
{
  "session_id": "…",
  "messages": [
    { "id": "…", "role": "assistant", "content": "",  "sent_at": "…", "image": true },
    { "id": "…", "role": "assistant", "content": "…", "sent_at": "…", "channel": "product_qa" },
    { "id": "…", "role": "user",      "content": "hi…",     "sent_at": "…", "read_at": "…" },
    { "id": "…", "role": "assistant", "content": "Bishop.", "sent_at": "…", "read_at": "…" }
  ],
  "total": 4
}
```

`id` is the stable message id (the `engine.chat_messages` row key) — it is
what the `image-request` route below takes. `role` ∈
`user | assistant | system_error`. Each entry also
carries an optional `channel` field — `"product_qa"` marks an
out-of-character product answer (excluded from companion context/memory,
same as its live-stream `action_type`); the field is omitted for normal
turns. `read_at` is a read receipt and is **omitted entirely while unread** —
see the route below.

`image: true` is present only on assistant rows whose turn delegated an image
to the consumer; every other row omits the key (never `false`). It is the
discovery half of the `image-request` route: feed the flagged entry's `id` to
that route instead of probing every assistant message. A `404` from that
route **on a flagged row** means the composed prompt was never recorded —
genuinely unrecoverable; surface it as such rather than silently dropping
the image. The flag also tells a rehydrating client that a turn promised an
image (the `image_request` SSE frame is the turn's last frame and wire-only —
a disconnect before it fires would otherwise be undetectable from history).

`reply_to_message_id` echoes the quote a `user` row was sent with (see
**Optional: quote** above): the id of the row it quoted, always in this same
session.

`action` (user rows only, omitted otherwise) is the action the row was sent
with, plus `response` (`"accept"` | `"refuse"`) once the turn took effect.

`reaction` (assistant rows only, omitted otherwise) is `{emoji, reacted_at}`.

`speaker_session_id` (assistant rows of a group session only, omitted otherwise) is the member session that spoke the line; resolve a departed speaker's name through session, then instance.

`occasion` (assistant rows the persona wrote on her own only, omitted otherwise) says why she spoke: the row's `metadata.proactive`, one of `birthday`, `holiday`, `first_meet`, `returning`, `just_opened`, `event` or `life_choice`. A client adapted to the [life choice](#post-v2compsessionsession_idlife-choiceschoice_id) re-fetches the affinity delta when it reads `life_choice`.

`user_message_id` names the `role='user'` row whose turn produced this row. It
is present on assistant rows and on `system_error` notices, and omitted on the
user rows themselves. The notices matter most: the queue worker and the
stale-claim reaper write them after the client has already disconnected, so
history is the only place a client can find them — and a notice is useless
without knowing which turn failed. Do not assume the newest user row is the one. Same key-presence contract — omitted on ordinary turns,
and omitted when the anchor failed to resolve, since there is no bubble to
point at. That failure is recorded as `metadata.reply_to_error` on the row and
stays audit-only; a client that needs to know its quote was dropped should read
the SSE turn it sent, not history.

### `GET /comp/chat/{session_id}/messages/{message_id}/image-request`

The delegated `image_request` payload for one persisted image turn — the
recovery path for consumers that never received the SSE frame (async turns;
stream clients that disconnected before the frame fired). Auth and session
ownership checks are the same as `history`.

```json
{ "message_id": "01J…", "composed_prompt": "5YaZ5a6e…", "image_ref": "face", "aspect_ratio": "3:4",
  "companion": {"member_session_id": "…", "instance_id": "…"} }
```

Fields mirror the SSE frame: `composed_prompt` is base64(`STANDARD`) of the
UTF-8 wire prompt; `companion` is present only on a group picture of two;
`image_ref` may be absent on rows persisted before the
marker carried it. **Absent `image_ref` means unknown, not `face`** — a
consumer that defaults it to `face` will redraw a `previous`-ref turn against
the wrong reference image. `404` when the message is not an image turn — or when the
turn's compose event was never recorded (the audit write is fail-open); in
that case the prompt is genuinely unrecoverable and the consumer should
treat the turn as text-only.

### `POST /comp/chat/{session_id}/read`

Mark every message in the session the user did not author as read. Same auth,
ownership check and 404-on-archived behaviour as the history route above.

```json
{
  "session_id": "8a1f0c2e-4b6d-4f8a-9c31-2d5e7f0a1b3c",
  "marked": 7
}
```

Write-only: the engine records a timestamp and stops there. It does not count
unread messages, does not notify, and does not decide what "read" looks like —
a client reads `read_at` off the history entries and renders whatever it wants.

`marked` counts the rows *this* call stamped, so a repeat call returns `200`
with `0` and an existing stamp is never pushed forward. Call it on every mount
without inventing an event.

One role is out of reach here: the one the user writes. A `user` row carries
the engine's own stamp — set the instant the turn handed the message to its
first model, which is the *delivered* tick and not that model's
acknowledgement.

Voice sessions are accepted, and their `assistant` rows are stamped like any
other — only the engine's own writer is text-only, so it is voice **`user`**
rows that stay unstamped.

**Read `read_at: absent` as "no receipt", never as "not yet read."** Voice
`user` rows, a `user` row ghosted during an offline stretch (no judge
read it), and — on a deployment running without the PDE judge — every `user`
row stay unstamped permanently.

See [2026-08-19-chat-message-read-at-design.md](superpowers/specs/2026-08-19-chat-message-read-at-design.md).

## Group chat

A group is a chat session with `channel = 'group'`, no persona instance, and up to
three members, each an existing 1:1 text session of the caller. Every route below
requires the JWT user to own the group; any other group id is
`404 group_not_found`. The turn, the director and the frames are described in
[group-chat.md](group-chat.md).

| Method | Path | Body / effect |
|---|---|---|
| `POST` | `/v2/comp/group` | `{title?, member_session_ids: [1..=3]}` → group DTO |
| `GET` | `/v2/comp/groups` | `{groups: [...]}`: the caller's non-archived groups, newest `last_active_at` first |
| `GET` | `/v2/comp/group/{group_id}` | group DTO |
| `PATCH` | `/v2/comp/group/{group_id}` | `{title}`; `null` clears. `200` group DTO |
| `POST` | `/v2/comp/group/{group_id}/members` | `{member_session_id}` → `200` group DTO |
| `DELETE` | `/v2/comp/group/{group_id}/members/{member_session_id}` | kick; `204` |
| `DELETE` | `/v2/comp/group/{group_id}` | dissolve (`archived = true`); `204` |
| `POST` | `/v2/comp/group/{group_id}/message/stream` | SSE turn, see [group-chat.md](group-chat.md#send) |

Group DTO: `group_id`, `title` (nullable), `members[]` (`member_session_id`,
`instance_id`, `joined_at`), `created_at`, `last_active_at`, and `last_message`
(`null` until the group has a visible message: `message_id`, `role`,
`speaker_session_id`, `preview` cut to 120 characters, `is_image`, `sent_at`).

A title is trimmed and must be 1 to 64 characters with no control character;
otherwise `422 invalid_title`.

Every refusal below carries the pre-stream body `{code, message, user_message}`
(plus `original_user_message_id` on the two `duplicate_*` codes).

| Status | Code | When |
|---|---|---|
| 400 | `invalid_payload` | send: malformed `client_msg_id`, `tier`, `prompt_traits` or `audit`; a `byok` block; `image.force = true` |
| 400 | `byoc_unsupported` | create / add member: a member session belongs to a BYOC instance |
| 404 | `group_not_found` | the group is unknown, dissolved or not the caller's |
| 404 | `session_not_found` | create / add member: a member session is unknown or not the caller's |
| 404 | `member_not_found` | kick: the session is not a member of the group |
| 409 | `group_full` | the group already has three members |
| 409 | `wrong_channel` | member session is not a text session; on send, the session is not a group |
| 409 | `session_archived` | member session is archived |
| 409 | `instance_already_member` | another member has the same persona instance |
| 409 | `group_empty` | send to a group with no member |
| 409 | `duplicate_in_progress` | send: the same `client_msg_id` is still running |
| 409 | `duplicate_failed` | send: that `client_msg_id`'s turn failed terminally; send a fresh one |
| 409 | `members_changed` | send: `expected_members` is not the group's set of active members |
| 422 | `invalid_title` | blank, over-long or control-character title |
| 422 | `unprocessable` | create with an empty member list; send with blank or over-long `content`, an unsupported `image.aspect_ratio`, or a mention that is not a member |
| 429 | `rate_limited` | send: the per-user in-flight stream cap is reached |

The send body takes the 1:1 `audit` block, which rides each speaker's reply
call. A failed speaker emits `meta → done{truncated:true}` for each attempt and
then `speaker_error` when its chat chain is exhausted, or a bare
`speaker_error` when its reply request could not be built or its image row
could not be persisted. A turn that hits the generation timeout runs no
post-process. A resend of a running turn's `client_msg_id` is
`409 duplicate_in_progress`; after it settles, the resend replays the persisted
rows (see [group-chat.md](group-chat.md#send)).

Reused unchanged on a group session id: `GET /comp/chat/{session_id}/history`,
`GET /bff/v1/comp/chat/{session_id}/history` (including its cursors),
`POST /comp/chat/{session_id}/read` and
`GET /comp/chat/{session_id}/messages/{message_id}/image-request`.

## Voice

### `POST /comp/voice/{session_id}/turn/stream`

Lean voice-channel turn: one transcribed user utterance in, one streamed
text reply out. STT and TTS are entirely the caller's job — the engine
never touches audio (see the
[voice-call parts design spec](superpowers/specs/2026-07-07-voice-call-parts-design.md)).

Returns `text/event-stream` with a reduced frame set: `delta`* then a
terminal `done`, or a single `error` — the same frame shapes as the chat
message stream above, but with **no** `meta` frame and no `action_type`.
On every turn of a call that carries lingering effects or needs, a `body_state` frame
of the same shape as in the chat message stream precedes the first `delta`.

The session must be a **voice-channel** session (`409 wrong_channel`
otherwise) — obtain one via `POST /comp/chat/start` with
`"channel": "voice"`. Voice is opt-in per deployment: without a
`[tasks.chat_voice]` block in the model config the endpoint returns
`501 voice_disabled`. A session whose instance is a BYOC instance is
`400 byoc_unsupported`.

The prompt is lean but not memoryless: persona + voice directive + a
first-turn **bootstrap snapshot** (frozen once per session, then re-injected
verbatim every turn) + one relationship line derived from the session's
affinity (bond/chemistry tiers) + this turn's **recall block**. History is
the 8 messages (4 exchanges) ending at this turn's own user row — shorter
than the chat path's window,
since the bootstrap and recall carry the longer-range memory instead. A
voice **turn** writes no memories (no insight extraction, no vector
writes), but it counts toward its relationship's scan, which reads the call
with the text rows of the same relationship and writes profile memories, an
episode and journal moments from it, so later calls and text chats can recall
it. Operators opt out with `DREAMING_VOICE_DISABLED=1` — see
[memory-layers.md](memory-layers.md#voice-turns).

**Bootstrap snapshot** (first turn only, then frozen into
`chat_sessions.metadata.voice_bootstrap` and replayed on every later turn —
the provider is stateless, so there is no "inject once" on the wire): a
`[关于他]` block of `human_insights` bullets (Neutral tier by default; see
`memory_scope` below) plus a `[你们的故事]` block, the relationship's opening
and arc (only once the relationship has an arc, and only when that turn's
`memory_scope` keeps relationship memory), plus a `[上次通话]` block, the
previous voice call's last 8 messages rendered as a transcript, plus a
`[身体状态]` block, the lingering effects of the text session the call reads
its relationship line from and the persona's needs at the call's start. The
call keeps that state and does not advance it. The five parts
degrade independently and silently — a failed assembly leaves the marker unwritten
so the next turn retries. Each turn filters the `[关于他]` bullets through the
[affinity gate](memory-layers.md#affinity-gate) with that turn's tiers.

**Per-turn recall** (every turn, read-only): a small vector-search pass over
the same `companion_memories` layers the chat path uses, gated by
`memory_scope` and the [affinity gate](memory-layers.md#affinity-gate), and
budgeted at 300 ms — a timeout or search failure just
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
| already exists (race) | non-empty | `content` overwritten, `truncated = true`; `generation_id` (the join key into `llm_generations`) and `affinity_scope` / `memory_scope` metadata preserved |
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
| 401 | — | missing / malformed / expired / wrong-secret JWT (empty body) |
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

## Bring your own character (BYOC)

A BYOC instance is a persona instance whose character the engine never stores.
Two routes create one; the stream, compose and image-edit routes then take the
character as a sealed card in their `character` field (see the stream
endpoint's "bring your own character" paragraph above). Both routes need the
bearer token and `X-Byoc-Caller-Secret`, which must match the deployment's
`BYOC_CALLER_SECRET`. Their refusals use the pre-stream body
`{code, message, user_message}`. The flow, the sealing format and what is
stored are in [byoc.md](byoc.md); the card format is in
[eros-character-v1.md](eros-character-v1.md).

### `POST /v2/comp/byoc/init`

Empty body. Draws the key for a new instance and writes nothing.

```bash
curl -X POST -H "Authorization: Bearer $JWT" \
  -H "X-Byoc-Caller-Secret: $BYOC_CALLER_SECRET" \
  http://localhost:8080/v2/comp/byoc/init
```

```json
{ "key": "<base64url, 32 bytes>", "ticket": "<base64url, 32 bytes>" }
```

`key` opens every card sent for the instance; the engine never shows it again,
so store it before calling create. `ticket` proves to create that the engine
issued `key` to this user.

| Status | Meaning |
|---|---|
| 200 | `{key, ticket}` |
| 401 | missing or invalid bearer |
| 403 | `byoc_forbidden`: BYOC is off, or the caller secret is missing or wrong |

### `POST /v2/comp/byoc/create`

Body `{"key": "…", "ticket": "…"}`, both from init (base64url, padding
optional). Creates the instance and returns its id.

```json
{ "instance_id": "8a1f0c2e-4b6d-4f8a-9c31-2d5e7f0a1b3c" }
```

Create is idempotent per key: a repeat with the same `key` and `ticket`
returns the same `instance_id`, and re-creating an archived instance
reactivates it. Open sessions on it with `POST /comp/chat/start` and the
`instance_id`.

| Status | Meaning |
|---|---|
| 200 | `{instance_id}` |
| 400 | `invalid_payload`: `key` or `ticket` is not base64url of 32 bytes |
| 401 | missing or invalid bearer |
| 403 | `byoc_forbidden`: BYOC is off, the caller secret is missing or wrong, or the engine did not issue this `key` to this user |
| 422 | the body is not `{key, ticket}` |

### BYOC error codes

| Status | Code | When |
|---|---|---|
| 403 | `byoc_forbidden` | the two routes above, or any card route request carrying `character`: BYOC is off, or the caller secret is missing or wrong |
| 403 | `byoc_card_rejected` | `character` does not open under the instance's key |
| 400 | `invalid_payload` | a card route: `character` missing on a BYOC instance, present on any other, malformed, over its size limit, or not a valid card, with a message saying which rule failed; the async route: any `character` |
| 400 | `byoc_unsupported` | the route cannot serve a BYOC instance: async send, `/open`, voice turns, life events, group chat |

A stream turn on a BYOC instance that a deploy interrupts is settled `failed`
with `chat_turn_queue.last_error = 'byoc_unavailable'`, plus the usual
`system_error` row; the card died with the process, and the deployment has no
character to fall back to.

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
| `character` | `String` | BYOC only | A sealed character card. Required when the instance is a BYOC instance, refused (`400 invalid_payload`) on any other. Needs `X-Byoc-Caller-Secret`. The composer reads the card's `appearance`. See [byoc.md](byoc.md). |

The composer payload is identical to the chat path's seven slots, so one
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
| `character` missing on a BYOC instance, present on any other, or not a valid sealed card | `400 invalid_payload` |
| `character` sent without the caller secret, or a card that does not open under the instance's key | `403 byoc_forbidden` / `403 byoc_card_rejected` |
| Blank `content`, over-cap `scene`, bad `aspect_ratio` | `422` |
| Over the per-user in-flight cap (shared with chat/voice, ≤3) | `429` |
| Composer chain exhausted | **The provider's own status, verbatim** (see below) — or an in-band `error` frame if streaming has begun. **No portrait fallback here**: the fallback exists to keep a chat turn moving, and this endpoint has no turn to protect. |

**Status passthrough (v1.4.0).** This endpoint used to render `502` whatever the
upstream said. It now returns the provider's own status — `529`, `503`, `429`,
or anything else it sent, including a code the engine has never seen. A failure
with no status of its own (the gateway cases) is synthesised: **`504` for the
three timeout kinds, `502` for everything else.** Detect a provider failure by
the body's `"error": "upstream"` key, not by the status.

Passthrough has one bound, and it is a range rather than an allow-list: a status
**below `400` is never forwarded**. A provider that answers `200` with an error
envelope in the body has spoken, but not in the status line, and forwarding that
`200` would tell you the call succeeded. Those answer `502`. The real value is
still on `upstream_status`, which is why you branch on the body.

```jsonc
// the provider answered, with a status
{"error":"upstream","message":"upstream failure: code=529: Overloaded",
 "upstream_status":529,"provider_code":"529","error_type":"overloaded","retryable":true}

// our path to the provider broke; no upstream status exists
{"error":"upstream","message":"upstream failure: compose stream open timeout after 15s",
 "gateway_kind":"open_timeout","retryable":true}
```

`error` and `message` are unchanged. `upstream_status` / `provider_code` /
`error_type` appear on the upstream arm (the latter two only when the provider's
body carried them); `gateway_kind` appears on the gateway arm — one of
`open_timeout` | `total_timeout` | `idle_timeout` | `transport` | `decode` |
`config` | `chain_exhausted`. `retryable` is always present and derived from the
status by HTTP convention (every `5xx`, plus `408` and `429`). **`Retry-After` is
forwarded verbatim** as a response header when the provider sent one — the engine
records and passes it on, never acts on it.

The streaming mode's in-band `error` frame keeps its four fields and does **not**
gain `upstream_status` / `provider_code`: the composer chain is walked while
opening the stream, so a total failure happens before the SSE response exists and
comes back as the HTTP error above.

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{"content":"在海边，黄昏","style":"realistic","aspect_ratio":"3:4","stream":false}' \
  http://localhost:8080/persona/{instance_id}/image/compose
```

## Profile

### `GET /comp/chat/{user_id}/sessions`

All chat sessions for `user_id`. The path's `user_id` MUST match the JWT's user_id; otherwise 403. Returns `{ "sessions": [...] }`, each entry carrying `session_id`, `instance_id`, `is_converted`, `last_active_at`, and `channel`. Group sessions are not listed; they come from `GET /v2/comp/groups`.

### `GET /comp/user/{user_id}/profile`

The flat, typed `human_insights` row for this user — the profile the engine derives from the user's fact ledger and rebuilds on a schedule (see [Insights](insights.md#the-derived-profile)). Same `user_id` equality check as above.

```json
{
  "user_id": "8a1f0c2e-4b6d-4f8a-9c31-2d5e7f0a1b3c",
  "occupation": "graphic designer",
  "mbti_guess": "INFP",
  "love_values": null,
  "emotional_needs": null,
  "life_rhythm": null,
  "interests": ["jazz", "long walks"],
  "personality_traits": [],
  "deal_breakers": [],
  "education": null,
  "education_level": "bachelor",
  "education_status": "graduated",
  "family": null,
  "relationship_history": null,
  "social_pattern": null,
  "future_plans": null,
  "finance_status": null,
  "kinks": [],
  "ideology": null,
  "mental_health": null,
  "updated_at": "2026-08-11T12:00:00Z"
}
```

`education_level` is one of `secondary_or_below`, `associate`, `bachelor`, `master`, `doctorate`, or `null`; `education_status` is `enrolled`, `graduated`, or `null`. `kinks`, `ideology` and `mental_health` are returned here because this is the user's own data; the service export never carries them. `updated_at` is when the last rebuild replaced the row. `updated_at: null` means the user has no `human_insights` row yet — no rebuild has landed — and every other field is `null`/`[]` in that response. There is no aggregate "training level" score anymore; `agent_training_level` and the raw `companion_insights` JSONB were removed with the companion_insights teardown (spec 2026-08-11) — the typed fields above are the whole surface now.

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

`updated_at: null` means this instance has no `character_insights` row yet — the character extraction chain has not produced a result — and every other field is `null`/`[]` in that response, same convention as the human profile above. Four of these fields — `current_situation`, `occupation`, `location`, `relationships` — are read back into the chat prompt as the `[character_state]` block, and the fill level of all ten sizes the injected history window (see [Deploying → Operational notes](deploying.md#operational-notes)); the other six are database-only.

This route is v1 and frozen — it keeps working and is not going away. The v2
equivalent, with the same fields under the v2 path convention, is
`GET /v2/comp/instance/{instance_id}/insight/character` (below).

### `GET /v2/comp/instance/{instance_id}/insight/character`

`CharacterInsightResponse` — the `character_insights` row, under the v2 API
convention (the entity segment before the id names what the id belongs to;
`insight` replaces v1's overloaded `profile`). Same fields as v1's
[`GET /comp/instance/{instance_id}/profile`](#get-compinstanceinstance_idprofile)
above. `GET`, authenticated, keyed on the relationship rather than the user:
the instance's `owner_uid` must equal the JWT's user_id, else `403`; an
unknown or archived (`status <> 'active'`) instance is `404`; no row yet is
`200` with every field `null`/`[]` and `updated_at: null` — same convention
as the v1 routes.

```json
{
  "instance_id": "8a1f0c2e-4b6d-4f8a-9c31-2d5e7f0a1b3c",
  "location": null,
  "occupation": null,
  "current_situation": null,
  "desires": null,
  "vulnerabilities": null,
  "habits": null,
  "personal_values": null,
  "likes": [],
  "dislikes": [],
  "relationships": [],
  "updated_at": null
}
```

### `GET /v2/comp/instance/{instance_id}/insight/user`

`UserInsightResponse` — the `user_insights` row, the real user's profile
*inside this one relationship*. Same ten fields, same shape, same ownership/
404/no-row rules as `.../insight/character` above. **This is not
`human_insights`**, the global profile served at
[`GET /comp/user/{user_id}/profile`](#get-compuseruser_idprofile) above:
`user_insights` is per-relationship, database-only, and record-only —
nothing reads it back into any chat, voice, or PDE prompt. Experimental
(v1.6.0) — see
[2026-08-22-user-insights-and-api-v2-design.md](superpowers/specs/2026-08-22-user-insights-and-api-v2-design.md).

### `GET /v2/comp/instance/{instance_id}/items`

The persona's items and wardrobe for this instance. Same gate as the insight
routes: `404` for an unknown or archived instance, `403` for another user's.

```json
{"items": [
  {"id": null, "kind": "clothing", "name": "白色开衫", "source": "preset",
   "user_message_id": null, "created_at": null},
  {"id": "…", "kind": "item", "name": "摩托车", "source": "gift",
   "user_message_id": "…", "created_at": "…"}
]}
```

Genome presets (`art_metadata.possessions` / `art_metadata.wardrobe`) come
first with `source: "preset"` and no `id`; then every instance row, newest
first, neither deduplicated nor capped. A BYOC instance has no presets here:
they live in its card, which the caller already holds, so the list is the
instance's own rows only. `source: "gift"` is an accepted gift
action; `user_message_id` is its turn, `null` once that message is deleted.
The prompt shows each list deduplicated, at most 20 instance names per list.

### `POST /v2/comp/instance/{instance_id}/items`

Body `{"kind": "item" | "clothing", "name": "…"}`, name 1–64 chars after
trim. `201` with the new entry (`source: "caller"`); `422` for a bad name
(out of bounds, or with line-break or control characters). Same `404` / `403`
gate as `GET`.

### `DELETE /v2/comp/instance/{instance_id}/items/{item_id}`

`204`; `404` when the id is not a row of this instance, with the same `404` /
`403` instance gate as `GET`. Gift and caller rows alike; presets are edited on the genome.

The engine trusts the caller on these routes. A caller that sells items keeps
its clients from reaching `POST` and `DELETE` directly.

### `DELETE /comp/instance/{instance_id}/sessions`

Soft-delete every session this user holds with one persona instance. The instance's `owner_uid` MUST match the JWT's user_id; otherwise 403. An unknown instance is 404 — unlike the profile route above, a dormant (`status <> 'active'`) instance is accepted, because a client may clear the conversations of a relationship it has already marked over.

```json
{
  "instance_id": "8a1f0c2e-4b6d-4f8a-9c31-2d5e7f0a1b3c",
  "archived_sessions": 2
}
```

`archived_sessions` counts the sessions flipped by *this* call, so a repeat call returns `200` with `0`. Every channel is archived, voice included — the unit is the relationship, not one client's view of it.

**What is deleted:** `companion_affinity` (and its events, by cascade), the relationship layer of `companion_memories`, `character_insights`, `user_insights`, the relationship scan's state, episodes, arc and journal (`relationship_scan_state`, `relationship_episodes`, `relationship_arcs`, `relationship_journal`), and the persona's seat in every [group](group-chat.md); the persona's story digest (`persona_story_insights.digest`) is cleared — the state that would otherwise carry into the next conversation. A turn still running when the archive request arrives writes none of it back: its affinity, relationship-memory and character and user insight writes find the session archived and write nothing, and a relationship scan in flight finds its scan state gone and writes nothing further. **What is not:** `chat_messages`. The transcript stays, so the audit tables that reference a session by id (`companion_decision_events`, `character_insights_events`, `chat_vision_events`, `chat_images_events`) still point at something readable. `relationship_scan_events` stays too (counts and ids only). The profile layer of `companion_memories` (`instance_id IS NULL`) also stays — those are cross-persona facts about the user. A conversation started on the same instance later begins a new scan state and a new first meeting, dated by its first non-archived row.

Afterwards the sessions are invisible everywhere: history, affinity, the session list, the chat and voice turn routes all return 404 or omit them, and `POST /comp/chat/start` creates a new session rather than resuming an archived one.

There is no restore endpoint by design. Reviving one session is an operator action:

```sql
UPDATE engine.chat_sessions SET archived = false WHERE id = '<session-uuid>';
```

The transcript comes back; the relationship state does not. See [2026-08-18-session-soft-delete-design.md](superpowers/specs/2026-08-18-session-soft-delete-design.md).

> **Tips replaced gift events.** The standalone gift routes
> (`POST /comp/chat/{session_id}/event/gift`, `GET /comp/chat/{session_id}/gifts`)
> were removed. A tip is now a `tip` action on a normal send turn (see above).

## World Town

The user's world publishes a town feed: posts written by their persona
instances, each with a comment thread. Both routes carry the same JWT contract
as `/comp/*` — the path `user_id` must equal the JWT `sub`, otherwise `403`.
Rendering is entirely downstream's job; these endpoints only move data. What
decides that a post gets written at all — enrolment, tick intervals, cooldowns,
the daily cap — is in [World system](world-system.md).

### `GET /world/town/{user_id}/feed?limit=20&cursor=`

Published posts, newest first, each with its full comment thread inlined.
`limit` defaults to 20 and is clamped to 50. A user who is not enrolled, or
whose town is switched off, gets an **empty feed, not an error**.

```json
{
  "user_id": "…",
  "posts": [
    {
      "post_id": "…",
      "instance_id": "…",
      "author_name": "Ada",
      "content": "…",
      "published_at": "…",
      "comments": [
        {
          "comment_id": "…",
          "author_instance_id": "…",
          "author_name": "Bishop",
          "content": "…",
          "created_at": "…"
        }
      ]
    }
  ],
  "next_cursor": "2026-08-17T04:12:00+00:00|<post uuid>"
}
```

- `next_cursor` — present only when another page may exist; feed it back
  verbatim as `cursor`. Treat it as opaque. A cursor that does not parse is a
  `400 bad_request`, not an empty page.
- `comments[].author_instance_id` — `null` when the comment is the user's own;
  otherwise the persona instance that wrote it.

### `POST /world/town/{user_id}/posts/{post_id}/comments`

Adds a user comment to a post in that user's own town.

```json
{ "content": "…" }
```

`content` is trimmed, then must be non-empty and at most 1000 characters —
`400` otherwise. Returns the created comment in the same shape as a
`comments[]` entry above. A post that is not visible to this user is `404`,
including one that exists in someone else's town.

## BFF (`/bff/v1/*`)

A frontend-shaped mirror of selected `/comp/*` routes for first-party
clients. Same Supabase JWT auth and the same per-user ownership checks as
the canonical routes — only the **response shape** differs (slimmer DTOs,
bundled payloads). Canonical `/comp/*` routes are never reshaped to fit a
frontend; a BFF route is added alongside instead. Five routes exist today.

### `POST /bff/v1/comp/chat/start`

Cold-mount bundle: resolves (or creates) the session **and** returns its
recent history in one round-trip, collapsing the frontend's separate
`start` + `history` calls. For the same user + input it resolves to the
exact same session as the canonical `POST /comp/chat/start`.

The body is the canonical start body plus one BFF-only field:

- `genome_id` / `instance_id` — identify the persona (same as canonical).
- `is_demo` — optional, same as canonical.
- `history_limit` — optional bundled-history page size; default 50, capped at 50.
- `channel` — optional, same as canonical (`"text"` by default, or `"voice"`).
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

`persona_name` is always present and is `null` for a BYOC instance, whose
name only its card holds.

Affinity is intentionally **not** bundled here — the frontend reads it
separately via the two affinity routes below, so a cold mount that does not
need a relationship pays nothing for one.

### `GET /bff/v1/comp/chat/{session_id}/history?limit=50&offset=0`

Slim history projection for the chat screen: `id` / `client_msg_id` /
`role` / `content` / `sent_at` (no `extracted_facts`), an
optional `channel` field — `"product_qa"` marks an out-of-character product
answer (excluded from companion context/memory); omitted for normal turns —
and `read_at`, the read receipt described under `POST /comp/chat/{session_id}/read`
(omitted while unread). `id` is the
`chat_messages` row primary key (UUID); `client_msg_id` is the id the FE
sent during streaming (`null` for rows that never carried one, e.g.
assistant turns). `image: true` marks an assistant row whose turn delegated
an image (omitted on every other row, never `false`) — the same key-presence
contract and `image-request` discovery semantics as the canonical history
route above, including the "404 on a flagged row = unrecoverable" reading.
The `POST /bff/v1/comp/chat/start` bundle serializes history through this
same entry shape and therefore carries the flag too. `user_message_id` names the
`role='user'` row whose turn produced this row — present on assistant rows and
on `system_error` notices, omitted on user rows. `speaker_session_id` marks the
speaker of a group-session assistant row and is omitted everywhere else. `reply_to_message_id`
echoes the quote a `user` row was sent with (see the stream route's
**Optional: quote** section) — the id of the row it quoted, always in this same
session; omitted on ordinary turns and on turns whose anchor failed to resolve,
so a cold mount can re-render the quote without keeping local state.
`action` (user rows only, omitted otherwise) is the action the row was sent
with, plus `response` (`"accept"` | `"refuse"`) once the turn took effect. `reaction` (assistant rows only, omitted otherwise) is `{emoji, reacted_at}`. `occasion` (assistant rows the persona wrote on her own only, omitted otherwise) is the canonical route's field of the same name, `life_choice` included; the start bundle carries it too. Same auth, ownership check, and
`limit ∈ [1, 50]` clamp as the canonical history route. **Intentional
divergence:** the default `limit` is 50 (the canonical route defaults to 20),
because the BFF exists for a cold mount that wants a full backscroll in one
round-trip.

```json
{
  "session_id": "…",
  "messages": [
    { "id": "3cc06c53-…", "client_msg_id": "c_abc", "role": "user",      "content": "alpha", "sent_at": "…", "read_at": "…" },
    { "id": "9f2e7a10-…", "client_msg_id": null,    "role": "assistant", "content": "beta",  "sent_at": "…", "read_at": "…" },
    { "id": "a1b2c3d4-…", "client_msg_id": null,    "role": "assistant", "content": "gamma", "sent_at": "…", "channel": "product_qa" },
    { "id": "b5c6d7e8-…", "client_msg_id": null,    "role": "assistant", "content": "",      "sent_at": "…", "image": true },
    { "id": "c9d0e1f2-…", "client_msg_id": "c_def", "role": "user",      "content": "wait, about that", "sent_at": "…", "reply_to_message_id": "3cc06c53-…" }
  ],
  "total": 5
}
```

`total` is the count of `messages` in **this** response (`== messages.len()`),
not the grand total of rows in the session.

Instead of an offset, a request can carry one keyset cursor, a message id;
rows are ordered by `(sent_at, id)` and returned oldest-first as above, and
`offset` is not used:

- `before=<id>` — up to `limit` rows strictly older than that row. Pass the
  oldest row the client holds to scroll up; an empty `messages` means there is
  nothing older.
- `after=<id>` — up to `limit` rows strictly newer than that row. Pass the
  newest row the client holds to scroll down; an empty `messages` means the
  client has caught up.
- `around=<id>` — that row plus up to `(limit - 1) / 2` rows older than it and
  up to the rest newer (24 + 1 + 25 at the default `limit`), to jump straight
  to a message however deep it is. A short side is not made up from the other.
  Continue with `before` / `after` from the window's ends.

An id that is not a message of this session answers `404`; more than one
cursor answers `400`.

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
    "user_message_id": "…",
    "life_choice": {
      "id": "…",
      "kind": "ask_for_space",
      "options": ["accept", "decline"],
      "asked_at": "…",
      "expires_at": "…",
      "message_id": "…"
    },
    "created_at": "…"
  }
}
```

`event` is `null` when there is no user-turn event yet (brand-new session,
or only time-decay), or when the latest event predates affinity migration
`0014`. `event_type` ∈ `message | gift | proactive | ghost`; a ghost turn
reports all-zero `effective_deltas`. The tier drop of a life choice is an
affinity event of its own type, `life_choice`, which this route never returns,
like `time_decay`: the new state comes back in the resolution response, and
from the absolute route below on any other device.

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
- `user_message_id` — the user message (`chat_messages.id`) that drove this
  turn; attach the delta to that message or to its replies
  (`chat_messages.user_message_id`). Absent on `proactive` / `time_decay`
  events and on rows written before migration `0056`.
- `life_choice` — the session's pending choice, composed at serve time and
  attached to `event` whenever one is unanswered and not past its 24 hours;
  absent otherwise. `options` lists the answers the
  [life-choice route](#post-v2compsessionsession_idlife-choiceschoice_id)
  takes; `message_id` is the chat message that asked it (absent once that row
  is gone). The ask lands seconds after the turn that triggered it, so re-fetch
  the delta when a history row carries `occasion: "life_choice"`. A client
  that ignores the field
  degrades to the ignore path described there.

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

### `GET /bff/v1/comp/affinities/{user_id}`

Every companion's current affinity for one user, in one round trip — the list
form of the route above, for a client rendering one row per companion. The
`{user_id}` path parameter MUST match the JWT's user_id; mismatch returns 403.

| Query param | Default | Notes |
|-------------|---------|-------|
| `limit` | 50 | Clamped to 1..=200, never rejected. |
| `cursor` | — | Opaque, taken verbatim from the previous page's `next_cursor`. Malformed ⇒ 400. |

```json
{
  "user_id": "…",
  "items": [
    {
      "session_id": "…",
      "genome_id": "…",
      "instance_id": "…",
      "affinity": { "…": "identical to the single-session shape above" }
    }
  ],
  "next_cursor": null
}
```

- **Every row gets the same read-time refresh** as the single-session route. A
  row that skipped it would be no better than the stored columns.
- `genome_id` is the companion the relationship is with — a list keyed by
  companion joins on it without a second round trip. It is always present and
  is `null` for a BYOC relationship, which has no genome.
- `instance_id` is the persona instance the relationship is with, always set.
  Key a relationship of either kind on it.
- Ordered `updated_at DESC, session_id DESC`, so page one holds the rows a list
  actually shows first. The cursor is a keyset on that pair; `session_id`
  breaks timestamp ties, which is what keeps a page boundary landing on a tie
  from skipping a row or serving it twice.
- `next_cursor` is `null` on the last page — walk until it is.
- **A session with no affinity row is absent**, not present with
  `affinity: null`. The list is driven by the relationships themselves, so
  "missing from the response" means "no relationship yet" — the state the
  single-session route reports as `affinity: null`.
- No per-session 403/404: the response only ever contains rows the caller owns,
  so a foreign or unknown session is simply absent.

## Service endpoints

Service-to-service routes under `/svc/v1/*`. They are admitted by a shared secret header, not a user JWT. The routes form three sub-trees, each behind its own secret so a consumer holds only the credential for the streams it reads, and each **not mounted** unless the deployment configured its secret — an unconfigured sub-tree answers exactly as it does for any unknown path: never `403`, never `200`. A secret is trimmed, blank counts as unset, and fewer than 32 bytes refuses to boot.

| Secret | Header | Routes |
|---|---|---|
| `INSIGHTS_EXPORT_SECRET` | `x-insights-export-secret` | [`insights/changes`](#get-svcv1insightschanges), [`memories/changes`](#get-svcv1memorieschanges), [`insights/deletions`](#get-svcv1insightsdeletions) |
| `FACTS_EXPORT_SECRET` | `x-facts-export-secret` | [`facts/changes`](#get-svcv1factschanges), [`facts/deletions`](#get-svcv1factsdeletions) |
| `USER_ERASE_SECRET` | `x-user-erase-secret` | [`POST users/{user_id}/erase`](#post-svcv1usersuser_iderase) |

All the streams are keyset-paged the same way; the pipeline behind them is in [Insights](insights.md#exports-and-deletions).

### `GET /svc/v1/insights/changes`

`human_insights` rows changed since a keyset cursor, for a downstream service that mirrors the conversation-derived profile. Requires `x-insights-export-secret: <INSIGHTS_EXPORT_SECRET>` (403 when missing or wrong). Reads the live table, not the snapshot archive.

| Query | Default | Meaning |
|---|---|---|
| `since` | Unix epoch | RFC 3339 timestamp; rows strictly after `(since, after_user)` |
| `after_user` | nil UUID | tie-break within `since` — pass `next.user_id` |
| `limit` | 500 | clamped to 1..=1000 |

```bash
curl -H "x-insights-export-secret: $INSIGHTS_EXPORT_SECRET" \
  "http://localhost:8080/svc/v1/insights/changes?limit=1"
```

```json
{
  "profiles": [
    {
      "user_id": "8d1f…",
      "occupation": null, "mbti_guess": null, "love_values": null,
      "emotional_needs": null, "life_rhythm": null,
      "interests": ["jazz", "hiking"], "personality_traits": ["calm"],
      "deal_breakers": ["smoking"],
      "education": null, "education_level": "master", "education_status": "enrolled",
      "family": null, "relationship_history": null,
      "social_pattern": null, "future_plans": null, "finance_status": null,
      "updated_at": "2026-10-13T12:00:00Z"
    }
  ],
  "next": { "updated_at": "2026-10-13T12:00:00Z", "user_id": "8d1f…" }
}
```

A row has the profile fields of [`GET /comp/user/{user_id}/profile`](#get-compuseruser_idprofile) **except `kinks`, `ideology` and `mental_health`, which are never exported**. `updated_at` moves when a rebuild replaces the profile. A user's erasure removes the row without a trace in this feed; read [`insights/deletions`](#get-svcv1insightsdeletions) to learn of it.

Rows are ordered by `(updated_at, user_id)`. `next` is present iff the page is full; within one walk, pass both of its fields back verbatim as `since` and `after_user` — at full microsecond precision, URL-encoded — and stop when `next` is `null`. A client that truncates the timestamp (JavaScript `Date` keeps milliseconds) can re-fetch the same full page forever once 1000 rows share one `now()`.

**Resuming the next poll.** Keep the last row you received — its `(updated_at, user_id)` — as your resume point; on an empty page keep the previous one. Start each new poll a few seconds *before* it: `since = last.updated_at − 5s`, no `after_user`. `updated_at` is stamped when the write starts, but the row is visible only when it commits, so a late-committing row can carry a timestamp you have already walked past; the overlap window picks it up. Re-returned rows are harmless — each is the user's current profile, keyed by `user_id`. Passing `since` alone deliberately re-returns rows at exactly that timestamp (the nil UUID sorts below every real one).

### `GET /svc/v1/memories/changes`

The extracted profile-layer memories created since a keyset cursor: the facts the relationship scan wrote about a user, for a downstream service that builds on them. These are the `companion_memories` rows with no persona instance and a `category`; raw-turn rows and relationship-layer rows are not exported, and neither are embeddings. The relationship scan's episodes, arcs and journal are never exported. Same secret as the insights export (`x-insights-export-secret`, 403 when missing or wrong).

| Query | Default | Meaning |
|---|---|---|
| `since` | Unix epoch | RFC 3339 timestamp; rows strictly after `(since, after_id)` |
| `after_id` | nil UUID | tie-break within `since` — pass `next.id` |
| `limit` | 500 | clamped to 1..=1000 |

```bash
curl -H "x-insights-export-secret: $INSIGHTS_EXPORT_SECRET" \
  "http://localhost:8080/svc/v1/memories/changes?limit=1"
```

```json
{
  "memories": [
    {
      "id": "3c2a…",
      "user_id": "8d1f…",
      "content": "用户目前正在找工作",
      "category": "fact",
      "metadata": { "domain": "career", "confidence": "high" },
      "created_at": "2026-10-13T12:00:00.123456Z"
    }
  ],
  "next": { "created_at": "2026-10-13T12:00:00.123456Z", "id": "3c2a…" }
}
```

`metadata` is what the extraction wrote, verbatim, or `null` when it wrote none. Rows are ordered by `(created_at, id)`, and paging works as on the insights export: `next` is present iff the page is full; pass both of its fields back verbatim as `since` and `after_id`, at full microsecond precision, until `next` is `null`.

**Resuming the next poll.** Keep the last row's `(created_at, id)`; start each new poll a few seconds before it, `since = last.created_at − 5s` with no `after_id`. `created_at` is the inserting transaction's start time and the row is visible only at its commit; the engine's insert transactions are short, so the overlap covers that gap. Re-returned rows are harmless: a row never changes, so deduplicate by `id`.

**No deletions in this feed.** A row goes away with its session (a hard delete cascades) or with an erase of its user, and this export reports neither. A mirror drops erased users by reading [`insights/deletions`](#get-svcv1insightsdeletions) with the same secret.

### `GET /svc/v1/insights/deletions`

The users whose data the engine erased since a keyset cursor, for a mirror of profiles and memories to drop in turn. Requires `x-insights-export-secret` (403 when missing or wrong). It is the same stream as [`facts/deletions`](#get-svcv1factsdeletions), readable with the insights secret so that mirror needs no second credential. There is no per-fact deletion: the stream is user-level.

| Query | Default | Meaning |
|---|---|---|
| `since` | Unix epoch | RFC 3339 timestamp; rows strictly after `(since, after_user)` |
| `after_user` | nil UUID | tie-break within `since` — pass `next.user_id` |
| `limit` | 500 | clamped to 1..=1000 |

```json
{
  "erasures": [
    { "user_id": "8d1f…", "erased_at": "2026-10-13T12:00:00.123456Z" }
  ],
  "next": { "erased_at": "2026-10-13T12:00:00.123456Z", "user_id": "8d1f…" }
}
```

Rows are ordered by `(erased_at, user_id)`; `next` is present iff the page is full, and is passed back verbatim as `since` and `after_user` until it is `null`. **A mirror must drop everything it holds for each `user_id` it reads here**: an erase deletes the user's profile, memory and fact rows, which none of the changes feeds can report. A repeat erase of the same user moves `erased_at` forward, so a user can reappear in the stream; dropping an already-dropped user is a no-op. Resume as on the other feeds: keep the last `(erased_at, user_id)` and start the next poll a few seconds before it.

### `GET /svc/v1/facts/changes`

Ledger facts first seen or seen again since a keyset cursor, for a downstream service that mirrors what the engine mined about each user ([Insights](insights.md#the-fact-ledger)). Requires `x-facts-export-secret: <FACTS_EXPORT_SECRET>` (403 when missing or wrong).

| Query | Default | Meaning |
|---|---|---|
| `since` | Unix epoch | RFC 3339 timestamp; rows strictly after `(since, after_id)` |
| `after_id` | nil UUID | tie-break within `since` — pass `next.id` |
| `limit` | 500 | clamped to 1..=1000 |

```bash
curl -H "x-facts-export-secret: $FACTS_EXPORT_SECRET" \
  "http://localhost:8080/svc/v1/facts/changes?limit=1"
```

```json
{
  "facts": [
    {
      "id": "5b7e…",
      "user_id": "8d1f…",
      "content": "用户养了一只猫",
      "category": "fact",
      "domain": "lifestyle",
      "evidence_type": "explicit_statement",
      "temporality": "current",
      "persistence": "ongoing",
      "confidence": "high",
      "occurrences": 3,
      "session_count": 2,
      "recurrence": "repeated_across_sessions",
      "first_seen_at": "2026-10-13T12:00:00.123456Z",
      "last_seen_at": "2026-10-20T09:30:00.654321Z"
    }
  ],
  "next": { "last_seen_at": "2026-10-20T09:30:00.654321Z", "id": "5b7e…" }
}
```

`recurrence` is `once`, `repeated_in_session` or `repeated_across_sessions`; the six labels are the engine's closed vocabularies. The session, message and run pointers stay inside the engine. Rows are ordered by `(last_seen_at, id)`, and paging works as on the insights export: `next` is present iff the page is full; pass both fields back verbatim as `since` and `after_id`, at full microsecond precision, until `next` is `null`.

A repeat moves `last_seen_at`, so a row reappears each time its counters change; deduplicate by `id`, last one wins. Rows leave the ledger only through an erase; see [`facts/deletions`](#get-svcv1factsdeletions).

**Resuming the next poll.** Keep the last row's `(last_seen_at, id)`; start each new poll *before* it with no `after_id`. `last_seen_at` is stamped by the ledger write itself (`now()`; a recurrence never moves it backwards), so it precedes the commit by one short statement: an overlap of a few seconds is enough.

### `GET /svc/v1/facts/deletions`

The same erasure stream as [`insights/deletions`](#get-svcv1insightsdeletions) — same query, same response, same meaning — admitted with `x-facts-export-secret`, for the mirror of facts. A mirror that holds facts must drop every `user_id` it reads here.

### `POST /svc/v1/users/{user_id}/erase`

Erase everything the engine derived about a user, and list the user on the deletion streams. For a deployment to call when the user deletes their account. Requires `x-user-erase-secret: <USER_ERASE_SECRET>` (403 when missing or wrong; 400 for a `user_id` that is not a UUID). The sub-tree is mounted under its own secret, so erasure works on a deployment that enables no export.

```bash
curl -X POST -H "x-user-erase-secret: $USER_ERASE_SECRET" \
  "http://localhost:8080/svc/v1/users/8d1f…/erase"
```

```json
{
  "user_id": "8d1f…",
  "erased_at": "2026-10-13T12:00:00.123456Z",
  "deleted": { "human_facts": 41, "human_insights": 1, "human_profile_state": 1, "companion_memories": 63, "user_insights": 2, "relationship_journal": 9, "…": 0 },
  "blanked": { "human_insights_events": 87, "companion_affinity": 2, "…": 0 }
}
```

`deleted` counts rows removed and `blanked` counts rows kept with their content cleared (ids and timestamps stay), each by table name. Every covered table is listed (the example abbreviates), with `0` when it had nothing to do; `blanked` counts only rows that still carried content. The tables are listed in [Insights → Erasure](insights.md#erasure). Chat sessions and messages belong to the caller and are not touched.

- **Idempotent.** A repeat finds nothing left, reports `0` everywhere, and moves `erased_at` forward.
- **Call it before deleting the user's sessions or persona instances.** The per-relationship insight tables and the relationship scan's tables key on the instance alone and are reachable only while the instance row exists.
- **Stop the user's traffic first.** A stage-1 extraction already in flight can write facts after the erase commits; call again to clean up. (A rebuild in flight cannot write a profile back.)

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
error type. The group routes (`/v2/comp/group*`) use the coded shape for their
refusals too, and so does every BYOC refusal (`byoc_forbidden`,
`byoc_card_rejected`, `byoc_unsupported` and the `invalid_payload` of a card),
on whichever route it comes from, as does the caller-secret refusal
(`caller_forbidden`). The table below covers the plain shape:

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | A value the handler checks and rejects (a bad cursor, bad `prompt_traits`, …). A body or path that fails to parse gets the framework's plain-text rejection instead |
| 401 | — | Missing / malformed / expired / wrong-secret JWT. The body is empty: the auth layer answers before any handler |
| 403 | `forbidden` | Path-user vs JWT-user mismatch, or trying to read a session you don't own; or a `/svc/v1/*` secret header (`x-insights-export-secret`, `x-facts-export-secret`, `x-user-erase-secret`) missing or wrong |
| 404 | `not_found` | Unknown session / persona / message id |
| 409 | `conflict` | The resource's state does not permit the action. The reaction route and image edit return it for a group session, with a `message` containing `wrong_channel`. The 1:1 stream and async send routes, `/open`, `/events`, `/effects` and the life-choice route refuse a group session with the coded body instead (`code: wrong_channel`), as the group routes refuse a non-group session |
| 422 | `unprocessable` | A well-formed request whose values are not acceptable |
| 429 | `too_many_requests` | A per-user concurrency cap was reached |
| 500 | `internal` | Anything else (DB error, LLM API error, etc.) |
| 501 | `not_implemented` | A capability the deployment did not configure |
| *the provider's own status* | `upstream` | The upstream provider failed the call (the persona compose endpoint and image edit when their composer chain is exhausted; `/open`, `/events` and the life-choice route when their companion chain is). Since v1.4.0 this is **not always 502**: the provider's status passes through verbatim (`529`, `503`, `429`, …), and a gateway-layer failure with no upstream status maps to `504` for a timeout and `502` otherwise. The body carries `upstream_status` / `provider_code` / `error_type` or `gateway_kind`, plus `retryable` — see [the compose endpoint](#post-personainstance_idimagecompose). Branch on the `"error": "upstream"` key, not on the status. |

## Source

- `crates/eros-engine-server/src/routes/companion.rs` — chat-lifecycle / profile handlers
- `crates/eros-engine-server/src/routes/companion_stream.rs` — streaming chat turn (`message/stream`), incl. actions + `image_url` handling
- `crates/eros-engine-server/src/routes/companion_async.rs` — enqueue-only chat turn (`v2/comp/session/{session_id}/message/async`)
- `crates/eros-engine-server/src/routes/session_open.rs` — the persona speaking first: birthday and holiday greetings and caller-named occasions (`v2/comp/session/{session_id}/open`)
- `crates/eros-engine-server/src/routes/session_events.rs` — life events the persona tells: the draw, and the proactive message that tells it (none for an offline kind she does not announce) (`v2/comp/session/{session_id}/events`)
- `crates/eros-engine-server/src/routes/session_effects.rs` — the persona's lingering state from accepted gives, her needs and the events under way (`v2/comp/session/{session_id}/effects`)
- `crates/eros-engine-server/src/routes/life_choices.rs` — the user's answer to her ask for space: accept or decline (`v2/comp/session/{session_id}/life-choices/{choice_id}`)
- `crates/eros-engine-server/src/routes/group.rs` — group management and the group send route (`v2/comp/group*`)
- `crates/eros-engine-server/src/routes/reaction.rs` — message reactions: set, clear and the allowlist (`v2/comp/session/{session_id}/message/{message_id}/reaction`, `v2/comp/reactions`)
- `crates/eros-engine-server/src/routes/insight.rs` — v2 relationship-scoped insight profiles (`v2/comp/instance/{instance_id}/insight/character`, `.../insight/user`)
- `crates/eros-engine-server/src/pipeline/chat_queue.rs` — async chat-turn queue worker
- `crates/eros-engine-server/src/routes/voice.rs` — voice-channel turn (`voice/{session_id}/turn/stream`)
- `crates/eros-engine-server/src/routes/persona.rs` — standalone image-prompt composition (`/persona/{instance_id}/image/compose`)
- `crates/eros-engine-server/src/routes/byoc.rs` — BYOC key lifecycle (`v2/comp/byoc/init`, `v2/comp/byoc/create`); `crates/eros-engine-server/src/byoc.rs` holds admission, the ticket and card opening
- `crates/eros-engine-server/src/routes/world_town.rs` — World Town feed and comments (`/world/town/*`)
- `crates/eros-engine-server/src/routes/bff/companion.rs` — BFF `/bff/v1/comp/chat/*`
- `crates/eros-engine-server/src/routes/bff/affinity.rs` — BFF `/bff/v1/comp/affinity/*`
- `crates/eros-engine-server/src/routes/svc.rs` — service-to-service export (`/svc/v1/*`)
- `crates/eros-engine-server/src/routes/health.rs` — `/healthz`
- `crates/eros-engine-server/src/openapi.rs` — Scalar UI spec metadata
