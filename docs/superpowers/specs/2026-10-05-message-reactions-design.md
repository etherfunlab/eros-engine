# Message reactions — Design

- **Date:** 2026-10-05
- **Status:** Draft — ready for review
- **Type:** Engine change. Three new routes, one history field, one env var,
  one migration (`0065`, two nullable columns on `engine.chat_messages`).
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — chat messages, the history routes, and the
  text reply prompt.

## 1. Motivation

A user can react to one of the persona's messages with a standard emoji,
skin-tone variants included, the way messaging apps let a reader tap a
reaction onto a message. The persona notices it on the next turn the user
drives. A reaction never triggers a reply of its own and never moves
affinity. A deployment can narrow which emoji are allowed.

## 2. Principles applied

1. **One source per fact.** The reaction lives in two columns on the
   reacted row. The prompt section is derived from them at read time; the
   allowed range lives in one env var and is served to clients rather than
   copied into them.
2. **No LLM in the loop.** Setting or clearing a reaction is a row update.
   The next reply turn reads it like any other context.
3. **Fail fast on configuration.** An allowlist entry the engine cannot
   resolve stops boot rather than silently narrowing or widening the range.

## 3. Storage

Migration `0065_chat_messages_reaction.sql`:

```sql
ALTER TABLE engine.chat_messages
    ADD COLUMN reaction   TEXT,
    ADD COLUMN reacted_at TIMESTAMPTZ;

ALTER TABLE engine.chat_messages
    ADD CONSTRAINT chat_messages_reaction_pair
        CHECK ((reaction IS NULL) = (reacted_at IS NULL)),
    ADD CONSTRAINT chat_messages_reaction_assistant_only
        CHECK (reaction IS NULL OR role = 'assistant');

CREATE INDEX idx_chat_messages_session_reacted_at
    ON engine.chat_messages (session_id, reacted_at)
    WHERE reaction IS NOT NULL;
```

- One reaction per message. `reaction` holds the fully-qualified emoji
  string; `reacted_at` is when the current reaction was set.
- The CHECKs are plain. A migration runs in one transaction, so splitting
  them into `NOT VALID` + `VALIDATE` would not shorten the `ACCESS
  EXCLUSIVE` lock the `ALTER` already holds; every existing row has both
  columns `NULL`, and the validating scan is a single pass.
- Neither column references another table; the FK rules do not apply.
- The partial index serves the per-turn read in §6.

`ChatMessage` (`store/src/chat.rs`) gains `reaction: Option<String>` and
`reacted_at: Option<DateTime<Utc>>`.

## 4. Validation and the allowlist

### 4.1 What counts as an emoji

The `emojis` crate (0.9, Unicode 17.0) is added to the server crate. A
request's `emoji` is valid when `emojis::get(s)` resolves the **whole
string** to one emoji: two emoji, text, or an emoji plus text resolve to
nothing. `get` maps unqualified and minimally-qualified input to the
fully-qualified emoji (`❤` → `❤️`); the stored and returned value is
`Emoji::as_str()`. ZWJ sequences and skin-tone variants are emoji in their
own right in this data set.

The plan verifies with tests that `get` resolves a skin-tone variant
(`👍🏽`) to an `Emoji` whose `skin_tone()` is `Some`, and that
`with_skin_tone(SkinTone::Default)` returns its base.

### 4.2 `CHAT_REACTION_EMOJI`

A comma-separated list, read once in `ServerConfig::from_env`.

- Unset or blank: every emoji §4.1 accepts is allowed.
- Each entry is trimmed and resolved through `emojis::get`. An entry that
  does not resolve refuses boot, naming the entry.
- Each entry is reduced to its **base**: `with_skin_tone(SkinTone::Default)`
  when the emoji has skin tones, else itself. A request emoji is allowed
  when its base is in the set. Listing 👍 therefore allows 👍🏻 through 👍🏿;
  listing 👍🏽 means the same thing as listing 👍.

`ServerConfig` holds the parsed set (`ServerConfig::from_env` is fallible and boot fails on a bad entry) as `Option<…>` (`None` = all) and its
listing order for §5.3.

## 5. API

All three routes sit on the v2 tree under the existing auth middleware.

### 5.1 `PUT /v2/comp/session/{session_id}/message/{message_id}/reaction`

Body `{"emoji": "👍🏽"}` → 200 `{"message_id", "emoji", "reacted_at"}`.

1. `require_session_for_user` — ownership before anything about the body.
2. `message_by_id_in_session` → 404 `no such message`.
3. Row is not `role = 'assistant'` → 409 `not an assistant message`.
4. `emoji` invalid (§4.1) or not allowed (§4.2) → 400.
5. Write:
   ```sql
   UPDATE engine.chat_messages
   SET reaction = $2,
       reacted_at = CASE WHEN reaction IS NOT DISTINCT FROM $2 THEN reacted_at ELSE now() END
   WHERE id = $1
   RETURNING reaction, reacted_at
   ```
   Re-sending the current emoji keeps its `reacted_at`, so it is not
   surfaced again on the next turn. A different emoji replaces it and
   restamps the time.

Any assistant row in the session can be reacted to, including
`product_qa` and voice rows; §6 decides what the persona sees.

### 5.2 `DELETE /v2/comp/session/{session_id}/message/{message_id}/reaction`

Same ownership and existence checks (404); 409 on a non-assistant row.
Sets both columns to `NULL`. 204, including when there was no reaction.

### 5.3 `GET /v2/comp/reactions`

`{"allowed": null}` when every emoji is allowed, else
`{"allowed": ["👍", "❤️", …]}`: the bases in env-var order, deduplicated.
A client builds its picker from this and offers skin tones on the entries
that have them.

### 5.4 History

`ChatHistoryEntry` (`routes/companion.rs`) and `BffHistoryEntry`
(`routes/bff/companion.rs`) gain

```
reaction: Option<ReactionView>   // { emoji, reacted_at }; skip_serializing_if none
```

present only on assistant rows that carry one, under the same key-presence
contract as `reply_to_message_id`.

## 6. What the persona sees

### 6.1 Which reactions

On a text reply turn driven by a user message, `build_reply_request` reads (its callers say whether to — `/open` greetings and edit turns do not)
the session's reactions set after the **previous** user row and at or before the **current** one:

```sql
WITH cur AS (
    SELECT sent_at FROM engine.chat_messages WHERE id = $2 AND session_id = $1
)
SELECT m.content, COALESCE(m.metadata ? 'image', false) AS is_image, m.reaction
FROM engine.chat_messages m, cur
WHERE m.session_id = $1
  AND m.reaction IS NOT NULL
  AND m.channel IS DISTINCT FROM 'product_qa'
  AND m.reacted_at <= cur.sent_at
  AND m.reacted_at > COALESCE(
        (SELECT p.sent_at FROM engine.chat_messages p
          WHERE p.session_id = $1 AND p.role IN ('user', 'gift_user')
            AND p.sent_at < cur.sent_at
          ORDER BY p.sent_at DESC LIMIT 1),
        '-infinity'::timestamptz)
ORDER BY m.reacted_at
```

"Previous" is the newest `user` / `gift_user` row sent strictly before the
current turn's own user row. The upper bound keeps a queued turn from seeing
reactions set after its own message, so a reaction renders on exactly one
turn. A reaction cleared before the turn is not there to be read; a reaction
the user set and never followed with a message is never seen. A
`user_message_id` that is not in the session reads nothing.

### 6.2 `[reactions]`

When the read returns rows, a per-turn fragment is appended after
`build_prompt`, in the same tail as `tips_reaction_context`, outside the
cached prefix:

```
[reactions]
你说的「今晚想吃什么…」，对方回了 ❤️
你发的照片，对方回了 🔥
```

- One line per reaction, oldest first. 「你」 is the persona's own message,
  「对方」 the reactor.
- The quote is the message as injected history shows it — leading sentence
  and action blocks stripped unless `CHAT_NOISE_CANCELLATION_DISABLED`, the
  raw text when stripping would leave nothing — with every whitespace run
  collapsed to one space, then cut to the first 20 chars, with 「…」
  appended when longer.
- A row with empty `content` and a `metadata.image` marker reads
  「你发的照片，对方回了 {emoji}」.

The fragment is rendered for this turn only. Nothing about the reaction
enters the history window on later turns; the persona's reply on this turn
is the record.

### 6.3 Who does not see it

The PDE judge, the affinity judge, memory and insight extraction, the input
and output filters, `/open` greetings, edit turns and voice turns are
unchanged.

## 7. Where it lives

| change | file |
|---|---|
| migration | `store/migrations/0065_chat_messages_reaction.sql` |
| columns on `ChatMessage`; set / clear / since queries | `store/src/chat.rs` |
| `emojis` dependency | `server/Cargo.toml` |
| `CHAT_REACTION_EMOJI` parse and boot refusal | `server/src/state.rs` |
| three routes, DTOs, router registration | `server/src/routes/reaction.rs` (new), `routes/mod.rs` |
| history field | `server/src/routes/companion.rs`, `routes/bff/companion.rs` |
| `[reactions]` fragment | `server/src/prompt.rs`, `pipeline/handlers.rs` |
| OpenAPI | `crates/eros-engine-server/openapi.json` (regenerated) |
| docs | `docs/api-reference.md` / `.zh.md`, `docs/deploying.md` / `.zh.md`, `.env.example`; README feature lists checked |

## 8. Configuration and rollback

`CHAT_REACTION_EMOJI` is the only knob. Without it every standard emoji is
allowed. A client that never calls the routes sees no change, and turns
with no reactions render no fragment. Rolling back the image leaves two
unused nullable columns; the migration is additive and needs no reverse.

## 9. Testing

- **validation.** One emoji accepted; two emoji, text, and emoji plus text
  rejected; `❤` stored as `❤️`; a skin-tone variant resolves and reduces to
  its base.
- **config.** Unset and blank allow all; an unresolvable entry fails boot
  and names it; a skin-toned entry reduces to its base; duplicates collapse
  in listing order.
- **store** (`#[sqlx::test]`). PUT sets both columns; the same emoji keeps
  `reacted_at`; a different emoji restamps it; DELETE clears both; a
  reaction on a `user` row violates the CHECK; the since-query honours the
  bound, the order and the `product_qa` exclusion.
- **routes.** 403 on someone else's session, 404 on a missing session and on an unknown message; 409 on a
  user row; 400 on invalid and on disallowed emoji; DELETE without a
  reaction is 204; `GET /v2/comp/reactions` in both shapes; history carries
  `reaction` only where set.
- **prompt.** Line format, the 20-char cut and its ellipsis, the image-row
  wording, ordering, no fragment when nothing is new, and a reaction set
  before the previous user row not rendered.
- `openapi.json` regenerated; the snapshot diff shows only the three routes,
  their schemas and the history field.

## 10. Non-goals

- More than one reaction per message.
- Per-tier allowlists.
- Any affinity effect.
- A reply triggered by a reaction.
- Reactions on the user's own messages.
- Custom (non-Unicode) emoji or stickers.

## 11. Decisions taken

- **The persona sees a reaction on the next user-driven turn**, over a
  reply triggered per reaction (a full turn per tap, spammable) and over a
  client-only feature (the persona would never know).
- **One reaction per message, in dedicated columns**, over storing it in
  `metadata`.
- **The allowlist is an env var**, not `model_config.toml`, which configures
  LLM tasks; non-LLM server knobs live in `ServerConfig::from_env`.
- **Allowlist entries match on base emoji**, so skin tones are always
  included with an allowed emoji.
- **The engine serves the allowed range** so a client picker never keeps a
  second copy.
- **Re-sending the same emoji is a no-op** on `reacted_at`.
