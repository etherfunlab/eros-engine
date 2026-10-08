# Bring your own key (BYOK)

[English](byok.md) · [中文](byok.zh.md)

A downstream can let its end users bring their own OpenAI-compatible chat
endpoint and API key. On a turn that carries a `byok` block, the companion's
reply is generated on the end user's account. The engine keeps no part of the
block: URLs and keys live only in the memory of the turn that uses them.

Design: [BYOK chat spec](superpowers/specs/2026-10-08-byok-chat-design.md).

## Turning it on

| Env var | Meaning |
|---|---|
| `BYOK_CALLER_SECRET` | Turns BYOK on. At least 32 bytes; a shorter value refuses boot. Unset ⇒ every `byok` block gets `403 byok_forbidden`. |
| `BYOK_ALLOW_PRIVATE_NETWORK` | `true` lifts the address guard and allows `http` endpoints. For self-hosted networks and tests. |

The engine authenticates end users by their own bearer token, so an end user
can call the engine directly. A `byok` block is honoured only when the request
also carries `X-Byok-Caller-Secret: <BYOK_CALLER_SECRET>`. Keep the secret on
the downstream's server and add the header there; never send it to a client.

## What BYOK covers

Only the reply on `POST /comp/chat/{session_id}/message/stream`, tip turns and
the text half of `reply_text_image` included. The PDE judge, the input and
output filters, vision, image prompt composition, product QA, affinity,
insight and memory extraction, and embeddings keep running on the deployment's
own chains and keys.

The async route refuses a `byok` block with `400 invalid_payload`. `/open`
greetings, voice turns and image edits do not take one.

## The `byok` block

```json
"byok": {
  "providers": {
    "mine": {
      "chat": "https://api.example.com/v1/chat/completions",
      "api_key": "sk-…",
      "headers": { "X-Org": "…" }
    }
  },
  "model": "gpt-x@mine",
  "fallback": ["other-model@mine"],
  "retry_depth": 1,
  "output_regex": [{ "pattern": "\\s*\\[note:[^\\]]*\\]\\s*$" }],
  "fallback_to_platform": false
}
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `providers` | map name → provider | yes | The end user's endpoints. |
| `providers.<name>.chat` | string | yes | Complete chat-completions URL, posted verbatim. |
| `providers.<name>.api_key` | string | yes | Sent as `Authorization: Bearer <api_key>`. |
| `providers.<name>.headers` | map | no | Sent verbatim on every request to this provider. |
| `model` | string \| array \| map | yes | Fixed, round-robin, or weighted random — the shapes of `[tasks.*].model`. |
| `fallback` | string \| array | no | Sequential fallback chain. |
| `retry_depth` | integer | no | Fallbacks tried after the primary. Default `2`. |
| `output_regex` | array | no | Strip rules for this chain's replies, each `{ pattern, replacement?, models? }`. |
| `fallback_to_platform` | bool | no | Default `false`. |

An `output_regex` rule replaces every match of `pattern` with `replacement`
(default empty), which is literal text. A rule can never lengthen a reply:
`replacement` may not contain `$`, and it may be no longer in bytes than the
shortest text `pattern` can match, so a pattern that can match the empty
string takes only an empty replacement. `models` lists bare model ids —
`gpt-x`, never `gpt-x@mine`; omitted ⇒ every model on the BYOK chain.

Every slug in `model` and `fallback` must end in `@<name>` naming one of
`providers`. A bare slug, an `@openrouter` suffix, or a name from the
deployment's `[providers]` is refused, so a BYOK turn can never spend the
deployment's keys. `allow_traits`, the display override, `temperature`,
`max_tokens`, the sampling knobs, `output_filter` and body params are not
accepted in the block; they keep coming from `[tasks.chat_companion]` and the
request's `tier`.

## Validation

Every check runs before the user row is written. A failure is
`400 invalid_payload` whose message names the field and never contains a key,
a header value or a URL.

| Item | Rule |
|---|---|
| `providers` | 1–4 entries; names match `[a-z0-9_]{1,32}`; `openrouter` is reserved |
| `chat` | ≤ 2048 bytes; https (http only with `BYOK_ALLOW_PRIVATE_NETWORK`); no credentials in the URL; a literal-IP host must pass the address guard |
| `api_key` | 1–1024 bytes of valid header text |
| `headers` | ≤ 8 entries, values ≤ 1024 bytes; `Authorization` and `Content-Type` refused |
| `model`, `fallback` | ≤ 8 entries each; weights finite and > 0, with a finite sum |
| every slug | ends in `@<name>` naming one of `providers`; ≤ 256 bytes |
| `output_regex` | ≤ 16 rules; `pattern` 1–512 chars; `replacement` ≤ 512 chars, no `$`, and no longer in bytes than the shortest text `pattern` can match; `models` ≤ 8 entries, each ≤ 256 bytes; compiled under a 1 MiB size limit and a 1 MiB DFA size limit |

## How the chain runs

- **Primary.** Fixed, round-robin, or weighted, as in config. The
  round-robin cursor is kept per end user, per process; it resets on restart
  and each replica counts on its own.
- **Fallbacks.** The selected primary is dropped from `fallback`, which is
  then cut to `retry_depth`.
- **`fallback_to_platform: true`.** When every BYOK hop fails, the walk
  continues into the deployment's chain for the request's `tier`.
- **Exhaustion.** A BYOK-only chain that fails ends in an `error` frame
  carrying the last hop's `upstream_status` and `provider_code`. It never
  serves the deployment's fallback phrase, which would hide from the end user
  that their key or endpoint failed. The turn is not retried; each failed hop
  leaves a truncated bubble in history, and the queue row settles `done`, as
  for any live chain whose hops all failed.
- **`meta.model`.** A BYOK hop always shows its real model id, on the live
  stream and on replay; `model_name_display_override` applies to the
  deployment's own hops only.
- **Strip rules.** A BYOK hop applies the block's `output_regex`; a
  deployment hop applies the config's.
- **Parameters.** BYOK hops receive the deployment's `chat_companion`
  `temperature`, `max_tokens` and sampling knobs over the strict
  OpenAI-compatible wire. A provider that rejects one of them fails the hop
  with its own 400.

## Where the engine connects

BYOK requests go out through a dedicated client: redirects are not followed,
proxies are not used, and only https is allowed. Host names are resolved
first; loopback, private, link-local, CGNAT, unique-local, unspecified and
multicast addresses (IPv4-mapped and NAT64 forms included) are refused, and
the connection goes to an address that passed. A refused host fails its hop
as a `transport` gateway error. `BYOK_ALLOW_PRIVATE_NETWORK` lifts all of
this.

The engine also bounds how much it reads from an end user's endpoint:

- a non-2xx response body is read up to 64 KiB;
- a streamed reply body larger than `max(1 MiB, max_tokens × 1 KiB)` fails
  its hop as a `transport` gateway error, and the chain advances.

Time limits are those of every chat hop; `BYOK_ALLOW_PRIVATE_NETWORK` does
not lift the byte limits.

## What is recorded

- BYOK hops are recorded as `<id>@byok` in `engine.llm_generations.model`,
  `llm_attempts[].model` and `gateway_errors[].model`, in the prompt log and
  in log lines. The provider name the end user chose is not recorded. `byok`
  is a reserved `[providers]` name, so the label always means a BYOK hop.
- A BYOK hop's `engine.llm_generations.generation_id` is minted by the
  engine as `byok-<32 hex digits>`; the provider's own id is not kept.
- `usage` is stored as the provider sent it, `cost` included. That cost is
  the end user's spend, so a deployment's spend query over
  `engine.llm_generations` excludes `model LIKE '%@byok'`.
- That a turn asked for BYOK is recorded once, on its queue row:
  `engine.chat_turn_queue.params->'byok'` = `{"fallback_to_platform": …}`.
- URLs, keys and header values are never recorded: not in tables, queue
  params, logs or the prompt log. A provider error that echoes the key, a
  header value, the URL or a value from its query string is stored with
  each replaced by `<redacted>`; header and query values shorter than 4
  bytes are left as they are. Reply text is the end user's model output and
  is stored as returned.

If a deploy interrupts a BYOK turn, the worker that recovers it no longer has
the block: it fails the turn (`last_error = 'byok_unavailable'`), or, with
`fallback_to_platform`, answers it on the deployment's chain.

## Security

A BYOK turn sends the whole reply prompt — persona definition, recalled
memories, insights, world state — to an endpoint the end user controls. The
end user can read all of it. Which personas, tiers and end users may use
BYOK is the downstream's decision.
