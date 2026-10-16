# Bring-your-own-character (BYOC) — Design

- **Date:** 2026-10-16
- **Status:** Draft — ready for review
- **Type:** Engine change. Two new routes, one optional request field on three
  routes, one request header, one env var, one migration.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — persona instance storage, the stream send route,
  the image compose and image edit routes, the chat queue's recovery path,
  and every route that resolves a persona instance.

## 1. Motivation

Three parties are involved, as in BYOK. The **engine** is this repo. A
**downstream** is a product that runs the engine and calls it from its own
server. An **end user** is a person using that product.

Today a character reaches the engine only as a row of
`engine.persona_genomes`, written by the downstream that owns the catalog, and
every persona instance points at one. BYOC adds a second kind of instance
whose character the engine never stores. The card lives wherever the deployer
decides: the downstream's own database, a third-party character library, the
end user's device. The engine stores the instance and everything the
conversation produces, plus one key per instance. Each request that needs the
character carries the whole card, encrypted with that key; the engine
decrypts it for that request and keeps none of it.

This opens the engine to character catalogs that live outside the
deployment: third-party character markets now, and later a market where a
card is held by its owner, for example as an NFT.

An imported card always starts a new character: a new instance with no
history from any other instance, whatever the card says. The engine does not
know where a card came from and has no notion of two instances being "the
same character".

## 2. Principles applied

1. **The engine holds the mechanism; the downstream holds the policy.** Which
   end users may bring a card, where cards are kept, how third-party formats
   are converted to the engine's, and what content is acceptable are the
   downstream's decisions. The engine enforces who may send a card and that a
   card belongs to its instance.
2. **No stored card on the call path.** The engine never reads card content
   back from storage to drive generation or judgement: no table or queue row
   that any request, worker or sweeper reads back holds card content, so the
   card exists only in the memory of the request that carries it. Audit
   tables and logs are backups the engine never reads back; they are outside
   this rule (§8).
3. **The key is the card's only identity.** The engine never compares a
   request's card with an earlier one. Whatever decrypts under the instance's
   key is that instance's character for that request. Changing the card
   between requests is the downstream's business; memories, insights and
   affinity built under the earlier card carry over unchanged.
4. **A request against a genome instance is unchanged.** Every new path
   branches on the instance's kind.
5. **Smallest useful surface first.** v1 covers the routes in §3.1. Every
   other route that needs the character refuses a BYOC instance explicitly
   (§3.3) and gains support later, one surface at a time.

## 3. Scope

### 3.1 Routes that take a card

| Route | What reads the card |
|---|---|
| `POST /comp/chat/{session_id}/message/stream` | The whole turn: reply prompt, PDE judge, the turn's `image_request` (`appearance`), tips (`tip_personality`), preset items, post-turn affinity evaluation (name). |
| `POST /persona/{instance_id}/image/compose` | The composer's persona input. |
| `POST /v2/comp/session/{session_id}/message/{message_id}/image/edit` | The edit payload's persona input (`appearance`). |

BYOK and BYOC combine on a stream turn: the card supplies the character, the
`byok` field the reply model.

### 3.2 Routes that work on a BYOC instance without a card

Every route that reads only engine-owned rows keeps working on a BYOC
instance, with no card and no caller secret: `POST /comp/chat/start` with an
`instance_id`, history, session lists, read receipts, reactions, session
effects, `GET …/image-request`, character and user insight, the character
profile, items, and archive. They must not fail on a missing genome (§7.2).

On the items routes, `GET` lists the instance's own rows only. The preset
`possessions` and `wardrobe` live in the card, which the downstream already
holds. `POST` and `DELETE` are unchanged.

### 3.3 Routes that refuse a BYOC instance

These answer 400 `byoc_unsupported` (the message names the route) before any
write:

- `POST /v2/comp/session/{session_id}/message/async` — its turn is generated
  later by the worker, which never holds the card.
- `POST /v2/comp/session/{session_id}/open`
- `POST /comp/voice/{session_id}/turn/stream`
- `POST /v2/comp/session/{session_id}/events` (life events)
- Group chat: creating a group with a BYOC member, or adding one.

### 3.4 Background work

The World Memories director, World Town and stories never see a BYOC
instance. Their roster and persona queries inner-join `persona_genomes`,
which leaves out an instance with no genome; they keep that join. The one
exception is the stories backfill, `StoryRepo::ensure_insight_rows`, which
ranks `persona_instances` directly: it gains a `genome_id IS NOT NULL`
filter, so a BYOC instance neither gets a story row nor takes a roster slot
from a genome instance. Tests pin the exclusion (§12). Dreaming and the insights snapshot read user-side
data only and are unaffected.

### 3.5 Route audit rule

The routes above are the known set. Every handler that resolves a persona
instance is classified by one rule: if it reads the character's content it is
in §3.1 or §3.3; if it reads only the instance's id, owner, status or the
engine's own rows it is in §3.2. The audit starts from every caller of
`load_companion`, `load_instance_gate` and `get_genome`, every query that
joins `persona_genomes`, and every query that reads
`persona_instances.genome_id` into a non-optional type.

The last group includes the affinity list, `GET /bff/v1/comp/affinities/{user_id}`
(`AffinityRepo::list_for_user`). It lists BYOC relationships like any other:
`genome_id` is null on their items, and every item gains `instance_id` so a
client can key a BYOC relationship without a second round trip.

## 4. Admission

`BYOC_CALLER_SECRET` (env, read in `ServerConfig::from_env`) gates the
feature, as `BYOK_CALLER_SECRET` gates BYOK:

- Unset or blank: BYOC is off. The two key routes (§5) and every card-route
  request carrying `character` get 403 `byoc_forbidden`.
- Set: those requests must carry the header `X-Byoc-Caller-Secret` with the
  same value, compared in constant time; missing or different → 403
  `byoc_forbidden`.
- A value shorter than 32 bytes refuses boot.
- Admission runs before every other check.

The engine authenticates end users by their own bearer token, so an end user
could otherwise call the engine directly with a card the downstream never saw.
The secret lives only on the downstream's server; only that server sends
cards.

The §3.2 routes take no secret: they are ordinary bearer-authenticated reads
and writes of engine-owned data. A deployment that unsets the secret after
creating BYOC instances keeps serving those routes; the card routes answer
403.

## 5. Key lifecycle

### 5.1 Init

`POST /v2/comp/byoc/init` — bearer token and caller secret, empty body. The
engine draws 32 random bytes and answers:

```json
{ "key": "<base64url, 32 bytes>", "ticket": "<base64url, 32 bytes>" }
```

`ticket` is `HMAC-SHA256(BYOC_CALLER_SECRET, "eros-byoc-init-v1" ‖ user_id ‖ key)`,
with `user_id` the bearer's, entering the HMAC as its 16 raw bytes. Init writes nothing. It relies on TLS alone; the
card encryption (§6) protects the cards sent after create, not the key's
delivery.

### 5.2 Create

`POST /v2/comp/byoc/create` — bearer token and caller secret, body
`{ "key": "…", "ticket": "…" }`.

1. The engine recomputes the ticket for the bearer's user and compares it in
   constant time. A mismatch is 403 `byoc_forbidden`: a key the engine did
   not issue to this user cannot create an instance.
2. It inserts the instance — `genome_id` NULL, `byoc_key` the key, owner the
   bearer — and answers `{ "instance_id": "…" }`.

Create is idempotent per key. A retry with the same key and ticket returns
the existing instance's id, so a downstream that lost the response resends
safely. The ticket binds the key to one user, so no other user can claim it.

Two steps so that an instance exists only once the downstream holds its key.
Init stores nothing, so a downstream that fails before saving the key leaves
nothing behind.

After create the downstream opens sessions with `POST /comp/chat/start` and
the new `instance_id`, as for any explicit instance.

### 5.3 After create

The key never leaves the engine again. v1 has no rotation and no recovery: a
downstream that loses the key can no longer use the instance's card routes,
and creates a new instance — a new character, with no history. Archiving
works as for any instance.

## 6. The sealed card

### 6.1 Wire form

The §3.1 routes gain an optional request field `character`, a string:

```
character = base64url( nonce[12] ‖ AES-256-GCM(key, nonce, plaintext) ‖ tag[16] )
```

No associated data. A fresh random nonce for every seal. The plaintext is the
UTF-8 JSON of an Eros character card v1 document,
[`docs/eros-character-v1.md`](../../eros-character-v1.md): the four fields of
a genome inside a `{spec, spec_version, data}` envelope. Third-party formats
are converted by the downstream.

### 6.2 Validation

Admission first (§4). Then, once the session and its instance are resolved
and ownership is checked, and before the user row is persisted:

| Case | Result |
|---|---|
| BYOC instance, `character` absent | 400 `invalid_payload` |
| genome instance, `character` present | 400 `invalid_payload` |
| `character` not base64url, shorter than 28 bytes decoded, or longer than 96 KiB | 400 `invalid_payload` |
| authentication tag fails | 403 `byoc_card_rejected` |
| plaintext not a valid eros-character v1 document | 400 `invalid_payload`, naming the field |

No error response quotes the sealed string, the key, or any part of the
card: a response travels the same path the card is encrypted against.

### 6.3 Building the persona

The handler builds the `CompanionPersona` from the instance row and the
decrypted card. Items merge as for a genome: `merge_items` takes the card's
`art_metadata` and the instance's item rows, unchanged. The persona is
threaded in memory through the request, including the work that runs after
the reply in the same process. No path reachable from a §3.1 route reloads
the persona by instance id for a BYOC instance; today's post-turn name read
in `post_process` is one such path.

## 7. Storage

### 7.1 Migration

```sql
ALTER TABLE engine.persona_instances ALTER COLUMN genome_id DROP NOT NULL;
ALTER TABLE engine.persona_instances ADD COLUMN byoc_key BYTEA;
ALTER TABLE engine.persona_instances
    ADD CONSTRAINT persona_instances_kind_chk
    CHECK ((genome_id IS NULL) <> (byoc_key IS NULL));
ALTER TABLE engine.persona_instances
    ADD CONSTRAINT persona_instances_byoc_key_len_chk
    CHECK (byoc_key IS NULL OR octet_length(byoc_key) = 32);
CREATE UNIQUE INDEX persona_instances_byoc_key_uniq
    ON engine.persona_instances (byoc_key) WHERE byoc_key IS NOT NULL;
```

- An instance is exactly one kind. "This instance is BYOC" is
  `byoc_key IS NOT NULL`, derived wherever it is needed; no flag column and no
  marker on queue rows.
- `genome_id` keeps `ON DELETE RESTRICT`. Its NULL marks the other kind of
  instance, not a genome instance allowed to outlive its genome, and
  `SET NULL` would violate the kind check.
- `UNIQUE (genome_id, owner_uid)` treats NULLs as distinct, so one user may
  hold any number of BYOC instances; the genome path's `ON CONFLICT` is
  unaffected.
- The key is stored as is. The threat it answers is a card read in transit; a
  reader of the database finds keys, and of the cards only the `appearance`
  the image audit keeps (§8).

### 7.2 Code

- `PersonaInstance.genome_id` becomes `Option<Uuid>` in `eros-engine-core`.
  This changes a published crate's public type; the release bumps it
  accordingly.
- Every query on a §3.2 path that joins `persona_genomes` becomes a left join
  with a nullable name — at least `load_instance_gate`, which gates chat
  start, the character profile, insight and items.
- §3.4's background queries keep their inner join.

### 7.3 Response schemas

Existing response schemas change for every client, genome-only ones
included:

- `persona_name` becomes nullable in `StartChatResponse` and the BFF start
  response; it is null for a BYOC instance, whose name only the card holds.
- `BffAffinityListItem.genome_id` becomes nullable, and the item gains a
  required `instance_id` (§3.5).

A client that never creates a BYOC instance never sees a null.

## 8. What is stored

- What the conversation produces is stored as on any turn, even where it
  quotes the card: reply text, memories, insights, PDE verdicts, affinity.
  That is engine output, not the card.
- **Audit tables and logs record a BYOC request as they record any other.**
  They are backups the engine never reads back, outside §2.2's rule:
  - the prompt log (`PROMPT_LOG_DIR`) writes a BYOC turn's full entry, card
    included;
  - `engine.chat_images_events` keeps the final image prompt in
    `composed_prompt` — a plain concatenation of the style preset, the card's
    `appearance` verbatim and the composer's subject — and `appearance` again
    in its `inputs` snapshot, on all three §3.1 routes.
- **Audit tables never feed generation.** `GET …/image-request` reads
  `chat_images_events`, but only to return a stored prompt to its caller
  verbatim, so it stays on the right side of §2.2. A feature that wants a
  character's `appearance` to drive generation — keeping her looks
  consistent across images, say — takes it from the request's card, never
  from this table.
- The model provider receives the card inside the prompt, as it receives any
  persona.

## 9. Queue and recovery

The stream route writes its queue row as today; `params` holds nothing from
the card. The worker reads `params` back to drive a turn, so the queue row is
on the call path and §2.2 applies to it. The card reaches the drive task in
memory, beside BYOK's `ByokTurn`.

The worker drives a stream turn only through crash recovery, after the card
has died with its process. For a BYOC instance `drive_turn` returns
`TurnOutcome::Unservable("byoc_unavailable")`: terminal in any mode, the queue
row goes `failed` and the usual `system_error` row is written. There is no
platform fallback; the deployment has no character to fall back to.

## 10. Where it lives

| Change | File |
|---|---|
| `genome_id: Option<Uuid>` | `crates/eros-engine-core/src/persona.rs` |
| migration | `crates/eros-engine-store/migrations/` (next number) |
| create, idempotent lookup, left-joined gates | `crates/eros-engine-store/src/persona.rs` |
| admission, init/create routes, ticket, sealing, card validation | `crates/eros-engine-server/src/byoc.rs` (new), a new route module |
| `character` field and persona build | `routes/companion_stream.rs`, `routes/persona.rs`, `routes/image_edit.rs` |
| `byoc_unsupported` refusals | `routes/companion_async.rs`, `routes/session_open.rs`, `routes/voice.rs`, `routes/session_events.rs`, `routes/group.rs` |
| in-memory persona through the turn | `pipeline/stream.rs`, `pipeline/post_process.rs` |
| `byoc_unavailable` recovery | `pipeline/chat_queue.rs` |
| `BYOC_CALLER_SECRET` | `state.rs`, `main.rs`, `.env.example` |
| OpenAPI | `openapi.json` (regenerated) |
| docs | `docs/eros-character-v1.md`; `docs/byoc.md` / `.zh.md` (new: flow, a WebCrypto sealing example, admission, scope, security); `docs/api-reference.md` / `.zh.md`; README feature lists |

## 11. Configuration and rollback

One env var. Without `BYOC_CALLER_SECRET` nothing changes for any existing
request.

The migration relaxes one constraint and adds a column, two checks and an
index. An older engine image reads instances through inner joins on
`persona_genomes`, so it should treat a BYOC instance as missing (404) rather
than fail; the builder confirms this against the previous release before
shipping and records the result in the release notes.

## 12. Testing

- **Ticket and create.** Init then create succeeds; a made-up key, a key with
  another user's ticket, and a tampered ticket each get 403; a repeated
  create returns the same instance; nothing is written by init.
- **Admission.** Secret unset, header missing, header wrong → 403 on init,
  create and each §3.1 route; a short secret fails boot.
- **Card validation.** Every row of §6.2, including a card sealed under
  another instance's key (403) and an unknown `spec_version` (400); no error
  body contains card text.
- **Turn.** A BYOC stream turn's reply prompt carries the card's
  `system_prompt`; preset items from the card merge with instance rows; the
  post-turn affinity evaluation gets the card's name; BYOK on the same turn
  routes the reply to the BYOK mock.
- **Image.** Compose and image edit see the card's `appearance`.
- **No stored card on the call path.** After served and failed BYOC turns,
  every `chat_messages`, `chat_turn_queue`, `persona_instances` and
  `persona_instance_items` row of the test user, rendered to text, contains
  no marker string planted in the card's `system_prompt` and `art_metadata`.
  The mocked model must not echo the marker.
- **Image recovery.** After a BYOC image turn, `GET …/image-request` returns
  the prompt the turn streamed, `appearance` included.
- **Refusals.** Each §3.3 route answers `byoc_unsupported` with nothing
  written.
- **§3.2 routes.** Chat start with the BYOC `instance_id`, history, profile,
  insight and items work; the genome name fields are null.
- **Background.** The world roster, town and story queries return no BYOC
  instance for an owner who has one.
- **Recovery.** A reaped BYOC stream turn goes `failed` with
  `byoc_unavailable`.
- **Affinity list.** A user with a genome relationship and a BYOC
  relationship gets both items; the BYOC item has a null `genome_id` and its
  `instance_id`.
- `openapi.json` regenerated; the diff shows the new routes, the `character`
  field, its schemas, the header, and the §7.3 schema changes.

## 13. Non-goals

- Card formats other than eros-character v1, including SillyTavern's V2/V3
  cards and eros-nft cards. A later card version decides.
- Key rotation and key recovery.
- BYOC on the async route, `/open`, voice, life events, group chat, World
  Memories, World Town or stories.
- Checking that a card matches an earlier one.
- Encrypting stored keys.
- Card provenance, marketplace logic, takedowns and content moderation — all
  downstream.

## 14. Decisions taken

- **The key belongs to the instance**, not to a genome: the instance is the
  BYOC subject, and BYOC has no genome row.
- **Two-step init/create with a stateless ticket**, over a pending-key table:
  init writes nothing, so an abandoned init leaves no row and needs no
  cleanup. The ticket is keyed with the caller secret rather than a second,
  engine-only secret: the downstream that holds the caller secret is already
  trusted, and the check exists to refuse keys the engine did not issue, not
  to defend against that downstream.
- **Init over TLS alone**; the card's own encryption protects the card on
  every later request.
- **AES-256-GCM**, because every platform a downstream or an end user's device
  runs on has it built in, WebCrypto included.
- **The card is the genome's four fields** in a `{spec, spec_version, data}`
  envelope. The envelope lets a later version, or a foreign format, be told
  apart on the first read.
- **A caller secret header**, as in BYOK: only the downstream's server may
  send a card.
- **"BYOC" is derived from `byoc_key`**, over a flag column or a queue-row
  marker.
- **"No stored card" covers the call path only.** Audit tables and logs are
  backups the engine never reads back, so BYOC records into them as any
  request does: no branch in the image audit or the prompt log, and
  `GET …/image-request` returns the prompt the turn streamed. The line is
  held where it matters: nothing that drives generation reads card content
  from storage.
- **Unsupported routes refuse loudly** with `byoc_unsupported`, over degrading
  silently, so a downstream learns the boundary on its first call.

## 15. Left to the builder

- Exact route paths and field names, keeping the semantics above.
- Status codes the design does not fix.
- How a BYOC persona's genome id is represented in `CompanionPersona`, as
  long as no BYOC value is ever written where a genome id belongs.
- The full route classification under §3.5's rule.
