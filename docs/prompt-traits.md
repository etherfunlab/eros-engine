# Prompt traits

A per-request hook for injecting caller-controlled fragments into the
persona system prompt.

## Shape

```jsonc
{
  "content": "...",
  "client_msg_id": "01J3333333333333333333333A",
  "prompt_traits": [
    { "tag": "ascii_identifier", "text": "verbatim text to inject" }
  ]
}
```

Accepted on `POST /comp/chat/{session_id}/message/stream`.

## What the engine does

For each turn, the validated `text` of every trait is rendered as a
bullet under a `[additional_guidance]` section inside the persona system
prompt, positioned between `[topics]` and `[turn_style]`. Empty list →
the section is omitted and the prompt is byte-for-byte identical to
the legacy output.

## What the engine does NOT do

- **No persistence.** Traits are not written to any DB table.
  Each request re-supplies them.
- **No content interpretation.** The engine treats `text` as opaque.
  Sanitisation and user-consent gating are the caller's responsibility.
- **No semantic categories.** `tag` is a logging key — the engine does
  not interpret `"nsfw_boost"` differently from `"politics_open"`.
- **Engine-side allow-listing (defense-in-depth only).** The engine now
  drops trait tags that are not in the resolved tier's `allow_traits` list —
  those tags are silently excluded from injection and the reply is still
  generated normally. Caller-side blocking is still recommended as the
  primary guard; engine-side filtering is defense-in-depth.

## Tier gating

Tags outside the request tier's `allow_traits` are not injected into the
system prompt. Specifically:

- Dropped tags are logged (tag name only, never the `text` body).
- The reply is generated normally even if all traits are dropped.
- Configure allow-lists per tier in `model_config.toml`:
  ```toml
  [tasks.chat_companion.tiers.gold]
  allow_traits = ["allow_nsfw", "allow_politics"]
  ```
- **Three-state semantics:**
  - `allow_traits` absent — no gating; all traits are injected.
  - `allow_traits = []` — drop all traits.
  - `allow_traits = ["a", "b"]` — whitelist; only listed tags are injected.
- The `tier` field on the request selects the tier block (see
  [api-reference.md](api-reference.md#post-compchatsession_idmessagestream)).
  Unknown or absent tier → task default block's `allow_traits`.

## Limits

| Field                 | Limit                              |
|-----------------------|------------------------------------|
| `prompt_traits` count | ≤ 8                                |
| `tag`                 | regex `^[a-z0-9_]{1,32}$`          |
| `text`                | 1 ≤ chars ≤ 2000 (after trim)      |
| `text` content        | no control characters (incl. `\n`) |

Violations return `400 BadRequest` and **no user message row is persisted**.

## Observability

When at least one trait is supplied, the engine logs an `info`-level
event with `traits_count` and `trait_tags`. The `text` body is never
logged.

## Threat model

A compromised client can attempt prompt-injection through `text`. The
engine now performs server-side tag filtering via `allow_traits` (see
"Tier gating" above), but protecting persona internals is ultimately the
caller's responsibility. Use deployer-side allow-listing of `tag`
values as the primary guard; the engine-side filtering is defense-in-depth.

## Why not persist?

By design, the engine's persona table is the long-lived contract. Traits
are intentionally ephemeral so caller-side experimentation, A/B testing,
and per-session policy don't pollute persona rows or require migrations.
