# Bring your own character (BYOC)

[English](byoc.md) · [中文](byoc.zh.md)

A downstream can let its end users bring a character the engine never stores.
The downstream keeps the card, wherever it keeps its cards. The engine keeps
the instance, everything the conversation produces, and one key per instance.
Each request that needs the character carries the whole card sealed under that
key; the engine opens it for that request and keeps none of it.

An imported card always starts a new character: a new instance with no history
from any other instance. The engine does not know where a card came from.

Design: [BYOC design](superpowers/specs/2026-10-16-byoc-design.md). Card
format: [Eros character card v1](eros-character-v1.md).

## Turning it on

| Env var | Meaning |
|---|---|
| `BYOC_CALLER_SECRET` | Turns BYOC on. At least 32 bytes; a shorter value refuses boot. Unset or blank ⇒ BYOC is off: the two key routes and every card route request carrying `character` get `403 byoc_forbidden`. |

The engine authenticates end users by their own bearer token, so an end user
can call the engine directly. The key routes and any request that carries
`character` are honoured only with `X-Byoc-Caller-Secret: <BYOC_CALLER_SECRET>`,
compared in constant time. Keep the secret on the downstream's server and add
the header there; never send it to a client. On the key routes and on a card
route request that carries `character`, admission runs before every other
check, so a missing or wrong secret is `403 byoc_forbidden` whatever else is
wrong with the request.

## What BYOC covers

Routes that take a card, on a BYOC instance:

- `POST /comp/chat/{session_id}/message/stream` — the whole turn: the reply
  prompt, the PDE judge, the turn's `image_request` (the card's `appearance`),
  tips (`tip_personality`), preset items and the post-turn affinity evaluation.
  BYOK combines with it: the card supplies the character, the `byok` block the
  reply model (see [byok.md](byok.md)).
- `POST /persona/{instance_id}/image/compose` — the composer's persona input.
- `POST /v2/comp/session/{session_id}/message/{message_id}/image/edit` — the
  edit's persona input.

Routes that work on a BYOC instance with no card and no caller secret, because
they read only rows the engine owns: `POST /comp/chat/start` with an
`instance_id`, history, session lists, read receipts, reactions, session
effects, `GET …/image-request`, character and user insight, the character
profile, items, and archive. They keep working if the secret is later unset.
`GET /v2/comp/instance/{instance_id}/items` lists the instance's own rows only;
the preset `possessions` and `wardrobe` live in the card, which the downstream
already holds. `POST` and `DELETE` on items are unchanged.

Routes that refuse a BYOC instance with `400 byoc_unsupported` before writing
anything; the message names the route:

- `POST /v2/comp/session/{session_id}/message/async` — its turn is generated
  later by a worker, which never holds the card. The route answers
  `400 invalid_payload` to any request carrying `character`.
- `POST /v2/comp/session/{session_id}/open`
- `POST /comp/voice/{session_id}/turn/stream`
- `POST /v2/comp/session/{session_id}/events` (life events)
- Group chat: creating a group with a BYOC member, or adding one.

World Memories, World Town and World Stories never include a BYOC instance.

### What a client sees

A BYOC instance has no genome, so two response fields have nothing to say:

- `persona_name` on `POST /comp/chat/start` and `POST /bff/v1/comp/chat/start`
  is `null`. The name lives in the card.
- `genome_id` on each item of `GET /bff/v1/comp/affinities/{user_id}` is
  `null`. Every item carries `instance_id`, so a client can key a relationship
  of either kind on it.

Both fields are always present in the JSON. The OpenAPI schema lists them as
optional because of how the generator treats nullable fields. A client that
never creates a BYOC instance never sees a `null`.

## Creating an instance

Two calls, both with the user's bearer token and the caller secret. The first
returns a key; the second turns it into an instance once the downstream has
stored the key.

```bash
curl -X POST -H "Authorization: Bearer $JWT" \
  -H "X-Byoc-Caller-Secret: $BYOC_CALLER_SECRET" \
  http://localhost:8080/v2/comp/byoc/init
```

```json
{ "key": "<base64url, 32 bytes>", "ticket": "<base64url, 32 bytes>" }
```

Init writes nothing. Store `key`, then create:

```bash
curl -X POST -H "Authorization: Bearer $JWT" \
  -H "X-Byoc-Caller-Secret: $BYOC_CALLER_SECRET" \
  -H "Content-Type: application/json" \
  -d '{"key":"<key>","ticket":"<ticket>"}' \
  http://localhost:8080/v2/comp/byoc/create
```

```json
{ "instance_id": "8a1f0c2e-4b6d-4f8a-9c31-2d5e7f0a1b3c" }
```

- The ticket is `HMAC-SHA256(BYOC_CALLER_SECRET, "eros-byoc-init-v1" ‖ user_id ‖ key)`
  for the bearer's user. Create recomputes it, so a key the engine did not
  issue to this user cannot make an instance: `403 byoc_forbidden`.
- `key` and `ticket` are base64url without padding; padding is accepted on
  input. Anything that is not 32 bytes after decoding is `400 invalid_payload`.
- Create is idempotent per key. Resending the same `key` and `ticket` returns
  the same `instance_id`, so a downstream that lost the response resends
  safely. Re-creating an archived BYOC instance reactivates it and returns the
  same id, so a stored key keeps working after an archive.
- One user may hold any number of BYOC instances.
- The engine never shows the key again, and v1 has no rotation or recovery. A
  downstream that loses a key can no longer use that instance's card routes;
  it creates a new instance, which is a new character with no history.

After create, open sessions with `POST /comp/chat/start` and the new
`instance_id`, as for any explicit instance.

## Sealing a card

The plaintext is the UTF-8 JSON of an
[Eros character card v1](eros-character-v1.md), at most 64 KiB. Third-party
formats are converted by the downstream before sealing.

```
character = base64url( nonce[12] ‖ AES-256-GCM(key, nonce, plaintext) ‖ tag[16] )
```

- `key` is the 32 bytes from init.
- No associated data. A fresh random nonce for every seal.
- Encode without padding; the engine accepts padding on input.
- The sealed string is at most 96 KiB and at least 28 bytes once decoded.

WebCrypto's AES-GCM returns the ciphertext with the 16-byte tag appended, which
is the layout above:

```js
const key = await crypto.subtle.importKey("raw", fromB64url(keyB64), "AES-GCM", false, ["encrypt"]);
const nonce = crypto.getRandomValues(new Uint8Array(12));
const ct = new Uint8Array(await crypto.subtle.encrypt({ name: "AES-GCM", iv: nonce }, key,
  new TextEncoder().encode(JSON.stringify(card))));          // ciphertext ‖ 16-byte tag
const character = toB64url(new Uint8Array([...nonce, ...ct]));
```

`fromB64url` and `toB64url` are base64url codecs; any implementation works.

Send the result as `character` on a card route, with the caller secret:

```bash
curl -N -X POST \
  -H "Authorization: Bearer $JWT" \
  -H "X-Byoc-Caller-Secret: $BYOC_CALLER_SECRET" \
  -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{"content":"hi","client_msg_id":"01J3333333333333333333333A","character":"<sealed card>"}' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

Every request that needs the character carries it. The key is the card's only
identity: whatever opens under the instance's key is that instance's character
for that request, and the engine never compares a card with an earlier one.
Changing the card between requests is the downstream's business; the memories,
insights and affinity built under the earlier card carry over unchanged.

A card is checked after the session and its instance are resolved and
ownership is confirmed, and before any row is written:

| Case | Result |
|---|---|
| BYOC instance, `character` absent | `400 invalid_payload` |
| genome instance, `character` present | `400 invalid_payload` |
| `character` not base64url, shorter than 28 bytes decoded, or longer than 96 KiB | `400 invalid_payload` |
| plaintext longer than 64 KiB | `400 invalid_payload` |
| authentication tag fails (wrong key, altered bytes) | `403 byoc_card_rejected` |
| plaintext not a valid card | `400 invalid_payload`, naming the field |

## Errors

Every BYOC refusal carries the coded body `{code, message, user_message}`, on
any route. No error response quotes the sealed string, the key or any part of
the card.

| Status | Code | When |
|---|---|---|
| 403 | `byoc_forbidden` | BYOC is off; the caller secret header is missing or wrong (the key routes, or a request carrying `character`); create with a key and ticket the engine did not issue to this user |
| 403 | `byoc_card_rejected` | the card does not open under the instance's key |
| 400 | `invalid_payload` | create: `key` or `ticket` is not 32 bytes of base64url; a card route: `character` missing, unexpected, malformed or not a valid card (table above); the async route: any `character` |
| 400 | `byoc_unsupported` | a route listed above as refusing a BYOC instance |

A stream turn on a BYOC instance that a deploy interrupts cannot be driven
again: the card died with the process, and the deployment has no character to
fall back to. Its queue row goes `failed` with `last_error = 'byoc_unavailable'`
and the usual `system_error` row is written. Resend with a fresh
`client_msg_id` and the card.

## What is stored

No row the engine reads back to drive a request holds any part of the card:
not `chat_messages`, `chat_turn_queue` (`params` and `last_error` included),
`persona_instances` or `persona_instance_items`. The card exists only in the
memory of the request that carries it, including the work that runs after the
reply in the same process. Per instance the engine stores the 32-byte key and
nothing else of the character.

- What the conversation produces is stored as on any turn, even where it quotes
  the card: reply text, memories, insights, PDE verdicts, affinity.
- Audit tables and logs record a BYOC request as they record any other. They
  are backups the engine never reads back.
  - The prompt log (`PROMPT_LOG_DIR`, off unless set) writes a BYOC turn's full
    entry, the card included.
  - `engine.chat_images_events` keeps the final image prompt in
    `composed_prompt` — the style preset, the card's `appearance` verbatim and
    the composer's subject — and `appearance` again in its `inputs` snapshot,
    for the stream, compose and image-edit routes.
- Audit tables never feed generation. `GET …/image-request` reads
  `chat_images_events` only to hand a stored prompt back to its caller
  verbatim. A feature that wants a character's `appearance` to drive generation
  takes it from the request's card, never from this table.

## Security

- Init relies on TLS alone. The key travels in its response; the card's
  encryption protects the cards sent after create, not the key's delivery.
- The caller secret never leaves the downstream's server. It is the only thing
  that lets a request carry a card, so an end user who calls the engine
  directly cannot.
- The key is stored as is. A reader of the database finds keys, and of the
  cards only the `appearance` the image audit keeps.
- A BYOC turn sends the whole reply prompt, the card included, to the model
  provider, as it does for any persona.
- Where cards come from, who may bring one and what content is acceptable are
  the downstream's decisions.
