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
bullet under a `【附加指引】` section inside the persona system prompt,
positioned between `【擅长话题】` and `【今日情境】`. Empty list →
the section is omitted and the prompt is byte-for-byte identical to
the legacy output.

## What the engine does NOT do

- **No persistence.** Traits are not written to any DB table.
  Each request re-supplies them.
- **No content interpretation.** The engine treats `text` as opaque.
  Sanitisation, allow-listing, and user-consent gating are the caller's
  responsibility.
- **No semantic categories.** `tag` is a logging key — the engine does
  not interpret `"nsfw_boost"` differently from `"politics_open"`.

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
engine is a transport, not a guard — protecting persona internals is
the caller's responsibility. Use deployer-side allow-listing of `tag`
values if you need to lock down which traits a client may send.

## Why not persist?

By design, the engine's persona table is the long-lived contract. Traits
are intentionally ephemeral so caller-side experimentation, A/B testing,
and per-session policy don't pollute persona rows or require migrations.
