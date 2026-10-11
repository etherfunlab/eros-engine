# Persona items, wardrobe and the gift action — Design

- **Date:** 2026-10-12
- **Status:** Draft — ready for review
- **Type:** Engine change. One migration (a new table, and `gift_reaction`
  leaves the assistant action-type CHECK). The HTTP API gains one action
  variant, one request field and three routes; nothing is removed from it.
  `eros-engine-core` breaks: `UserAction` gains a variant, `GiftItem` is
  renamed `GivenItem`, `CompanionPersona` gains a field.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — the persona instance's state, the text chat turn
  (SSE stream and async entry points), its decision, its reply prompt, and
  the chat image composer.
- **Builds on:** `2026-10-05-chat-user-actions-design.md` (*the action spec*)
  and `2026-10-11-tip-as-action-design.md` (*the tip spec*).

## 1. Motivation

A persona owns nothing and has nothing to wear. The judge invents an outfit
every turn from nothing, and what the user hands over with `give` (a
cigarette, a drink) is consumed on the spot.

Two lists make the persona's things part of her state:

- **Items** (持有物品), e.g. a motorbike the user gave her.
- **Wardrobe** (可选衣物), e.g. a red qipao.

Entries come from three places: a gift in the conversation, presets the
persona's author puts on the genome, and the caller writing through the
engine. A new `gift` action is the conversational path. The persona knows
what she owns and wears, the judge dresses her from her wardrobe, the user
may name the outfit for a turn, and the outfit reaches the picture.

`gift_reaction`, an assistant action type with no writer, is removed in the
same change.

## 2. Principles applied

1. **One source per fact.** A gift lives on the user row's `metadata.action`;
   the item it leaves behind is a row in `engine.persona_instance_items`. They
   are different facts: the row is what she owns now, and it outlives both a
   deleted message and a later removal. Genome presets are read from the
   genome, never copied onto instances.
2. **The judge decides, the engine folds.** Whether a gift is accepted is the
   action spec's `action_response`; the gift adds no verdict field.
3. **Default-open.** The judge is guided toward the wardrobe, not held to it;
   the engine checks neither the judge's outfit nor the caller's against the
   list.
4. **The engine judges, the caller settles.** A refused gift is reported; what
   happens to anything the user paid for is the caller's decision.
5. **Engine-owned state, two writers.** The engine writes gift items; the
   caller writes its own through the engine's routes. Nobody writes the table
   directly.

## 3. Storage

### 3.1 `engine.persona_instance_items`

```sql
CREATE TABLE engine.persona_instance_items (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    instance_id     UUID NOT NULL
                    REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    kind            TEXT NOT NULL CHECK (kind IN ('item', 'clothing')),
    name            TEXT NOT NULL,
    source          TEXT NOT NULL CHECK (source IN ('gift', 'caller')),
    user_message_id UUID
                    REFERENCES engine.chat_messages(id) ON DELETE SET NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX persona_instance_items_instance_idx
    ON engine.persona_instance_items (instance_id, created_at);
CREATE UNIQUE INDEX persona_instance_items_user_message_uidx
    ON engine.persona_instance_items (user_message_id)
    WHERE user_message_id IS NOT NULL;
```

- A dependent table: `instance_id` cascades.
- `user_message_id` is the gift turn. Nullable (caller rows have none), so
  `ON DELETE SET NULL`: the item outlives its message. The partial unique
  index makes the gift write idempotent and serves as the FK's index.
- `source` is stored because it survives a `SET NULL` on `user_message_id`;
  "she was given this" is not derivable once the message is gone.
- No `owner_uid`: `instance_id` determines it.
- Same-name rows are allowed (two qipaos given twice are two rows); renderers
  deduplicate (§5.1).
- Supabase lockdown as in 0059: `REVOKE ALL` from `anon` and `authenticated`
  (guarded by `pg_roles`), `ENABLE ROW LEVEL SECURITY`.

### 3.2 Names

An item name, wherever it enters the engine (gift action, caller route,
`outfit`), is trimmed and must be 1–64 chars after trimming and pass the
`validate_prompt_traits` character rule (no `is_control` chars, no
U+2028/U+2029). A violation is a 422 `unprocessable`.

### 3.3 Genome presets

Two optional keys on `engine.persona_genomes.art_metadata`:

| key | value |
|---|---|
| `possessions` | array of strings |
| `wardrobe` | array of strings |

As with every `art_metadata` key, the engine validates nothing: a missing key
is an empty list, and non-string or blank entries are skipped. Presets are
merged in at read time (§5.1). They have no id and cannot be removed per
instance; the genome is edited instead.

### 3.4 Core types

- `ItemKind` — `Item`, `Clothing` (`snake_case`).
- `PersonaItem { kind: ItemKind, name: String, gifted: bool }` — one entry of
  the merged lists as the prompt renders them.
- `CompanionPersona` gains `items: Vec<PersonaItem>`. Every struct-literal
  construction fills it.

## 4. The gift action

### 4.1 Request

`UserActionDto` gains

```json
{"type": "gift", "kind": "item" | "clothing", "name": "红色旗袍"}
```

`kind` and `name` are required on `gift`; `name` follows §3.2. They are
ignored on every other `type`, as `item` is on a non-`give` action. As with
every action, `content` may be empty, and `image_url` and `image` are
allowed alongside.

### 4.2 Core types

- `UserAction::Gift { kind: ItemKind, name: String }`.
- `GiftItem` is renamed `GivenItem`: it is the category of `give`, and next
  to `Gift` the old name misleads. Its wire values are unchanged.
- `refusal_line()`: `Gift` → `Some(Bond)`, as `Give`.
- `pde::decide` needs no new branch: its action branch already returns
  `ReplyText` with `Accept` for any action.

### 4.3 Renderings

The action spec's renderers gain the `gift` arm. Both kinds read the same;
`kind` decides only which list the item joins.

| renderer | gift |
|---|---|
| `action_phrase` (history marker, inside 「（）」) | 送你「{name}」 |
| `action_subject` (judge `[用户动作]`, `[user_action]`) | 对方送你「{name}」 |
| conclusion | 你接受了 / 你没有接受 (unchanged) |

```
[用户动作] 对方送你「摩托车」
[user_action]
对方送你「摩托车」，你接受了。用你自己的话回应。
```

The fragment, like every action's, never contains 「拒绝」.

### 4.4 The item write

`ChatRepo::set_action_response` runs at the point #393 placed it (right
before post-process is spawned, on both reply paths) and becomes one
transaction:

1. write `metadata.action.response`, as today;
2. on a `gift` row with `accept`: insert the item, reading `kind` and `name`
   from the row's own `metadata.action` and `instance_id` through
   `chat_sessions`, with `source = 'gift'` and
   `ON CONFLICT (user_message_id) WHERE user_message_id IS NOT NULL DO NOTHING`;
3. on a `gift` row with `refuse`: delete the item whose `user_message_id` is
   this row, so a re-driven turn whose decision flips leaves no item.

A non-gift action inserts and deletes nothing. A write failure is logged and
does not fail the turn, as today.

The turn's own reply learns of the gift from `[user_action]`; the item
appears in `[character_state]` from the next turn on.

### 4.5 Affinity

No new mechanism. An accepted gift is graded by the affinity judge from the
marker in the user text; a refused one drops the Bond line one tier through
the action spec's §6.2.

## 5. Prompt

### 5.1 Loading and merging

`PersonaRepo::load_companion` reads the instance's items in the same query
(a correlated `json_agg` subquery), adding no round trip.

`CompanionPersona.items` is built per kind:

1. genome presets, in genome order;
2. the instance's rows, distinct by name, newest first, at most
   `PROMPT_ITEMS_PER_KIND = 20` names, skipping names already listed.

`gifted` is true when any of the instance's rows with that kind and name has
`source = 'gift'`. Only the prompt is capped; the route returns every row
(§7).

### 5.2 Reply prompt

`[character_state]` gains two lines, before the existing outfit line:

```
[character_state]（这段关系里目前的状况，不是人设；与上面冲突时以上面为准）
- 你有的东西：「摩托车」（对方送的）、「吉他」
- 你的衣柜：「红色旗袍」（对方送的）、「白色开衫」
- 此刻的穿着：红色旗袍
```

- Names are quoted with 「」, the action spec's convention for text a model
  must read as data.
- An empty list renders no line. A persona with no items, on a turn without
  an outfit, builds a prompt byte-identical to today's.
- The block is in the volatile tail; the cache boundary does not move.

The lines render wherever `[character_state]` renders.

### 5.3 Judge context

`build_pde_ctx` adds, after the persona brief and only when non-empty, the
lists rendered as in §5.2:

```
[你的物品] 「摩托车」（对方送的）、「吉他」
[你的衣柜] 「红色旗袍」（对方送的）、「白色开衫」
```

and, on a turn that names an outfit (§6), before `[用户最新消息]`:

```
[指定穿着] 「红色旗袍」
```

The reference `filter_prompt` in `examples/model_config.toml` extends its
`clothing` guidance: 「有 [你的衣柜] 时优先从里面挑一件或搭配；有 [指定穿着]
时写 null（引擎以指定为准）」. The engine does not check the judge's outfit
against the wardrobe. An empty wardrobe leaves the judge as today.

## 6. The per-turn outfit

### 6.1 Request

`StreamSendRequest` gains

```
outfit: Option<String>
```

on `POST /comp/chat/{session_id}/message/stream` and
`POST /v2/comp/session/{sid}/message/async`. It follows §3.2; blank reads as
absent. The engine does not check it against the wardrobe: the caller's
picker is the gate.

### 6.2 Flow

- `Event::UserMessage` gains `outfit: Option<String>`; the async path carries
  it through `QueuedTurnParams` as it carries `action`.
- `build_user_row_metadata` writes it as `metadata.outfit`. No history route
  returns it.
- The judge sees `[指定穿着]` (§5.3).
- `pde::plan_for` and `pde::decide` read the outfit from the event. On every
  reply plan a named outfit is `plan.clothing` and the judge's `clothing` is
  not used; without one, `plan_for` keeps the judge's sanitized `clothing` as
  today. Every path that builds a plan (the judge, the rule PDE, a judge
  failure, the ghost kill-switch, the forced-image override) therefore wears
  the named outfit.
- `plan_for` stops dropping `clothing` on `ReplyImage`. The drop existed
  because nothing downstream of a bare image read the outfit; the composer now
  does (§6.3). `reply_mode` is still dropped there.
- The outfit is not rewritten: §3.2 already keeps it to one line.

### 6.3 The picture

`compose_user_payload` gains a `[此刻穿着]` slot after `[人物外观]`, filled
from `plan.clothing` whatever its source; empty renders `（无）` like every
other slot. `compose_inputs_json` records it as `clothing`. The judge's own
outfit, which until now reached only the text, therefore reaches the picture
too.

`DEFAULT_COMPOSE_PROMPT` names the new section among its inputs and adds:
dress the character in what they are wearing right now unless the partner's
latest message or the recent scene calls for something else; when that
section is `（无）`, decide the clothing yourself.

`POST /persona/{instance_id}/image/compose` passes no clothing; its slot
renders `（无）`. The image-edit payload is unchanged.

## 7. Routes

Auth as `/v2/comp/instance/{instance_id}/insight/*`: `load_instance_gate`;
404 for a missing or archived instance, 403 for another user's.

**`GET /v2/comp/instance/{instance_id}/items`**

```json
{"items": [
  {"id": null, "kind": "clothing", "name": "白色开衫", "source": "preset",
   "user_message_id": null, "created_at": null},
  {"id": "…", "kind": "item", "name": "摩托车", "source": "gift",
   "user_message_id": "…", "created_at": "…"}
]}
```

A flat list: genome presets first (`source = "preset"`, no id), then every
instance row newest first. Neither deduplicated nor capped; no pagination.

**`POST /v2/comp/instance/{instance_id}/items`** — body `{kind, name}`, name
per §3.2. 201 with the new entry, `source = "caller"`.

**`DELETE /v2/comp/instance/{instance_id}/items/{item_id}`** — 204. An id
that is not a row of this instance is 404. Gift and caller rows alike.

The routes take the user's JWT, and the engine trusts the caller as it trusts
`tip` and `gift`. A caller that sells items must keep its clients from
reaching POST and DELETE directly, and must strip a client-supplied `gift`.

## 8. Migration `0067_persona_instance_items.sql`

1. §3.1's table, indexes and lockdown.
2. `gift_reaction` leaves the assistant action-type CHECK:

```sql
UPDATE engine.chat_messages SET assistant_action_type = 'reply'
 WHERE assistant_action_type = 'gift_reaction';

ALTER TABLE engine.chat_messages
    DROP CONSTRAINT chat_messages_assistant_action_type_check;
ALTER TABLE engine.chat_messages
    ADD CONSTRAINT chat_messages_assistant_action_type_check
    CHECK (assistant_action_type IS NULL
           OR assistant_action_type IN ('reply', 'proactive'));
```

The production deployment holds no `gift_reaction` row; the `UPDATE` is for
deployments whose older versions wrote some, so the constraint validates
everywhere. Applied migrations are not edited.

The comment at `routes/companion.rs` naming `reply/gift_reaction` as the
allowed values is corrected.

## 9. Data flow, one gift turn

```
request.action {gift}   validate kind/name → metadata.action on a user row
  → run_stream          Event::UserMessage { action: Gift, outfit, .. }
  → PDE judge ctx       [你的物品] [你的衣柜] [指定穿着] [用户动作] 对方送你「…」
  → verdict             action_response accept | refuse | null
  → finalise            reply_text; null → Accept; no dice; frame;
                        plan.clothing = outfit, else the judge's
  → build_reply_request [character_state] lists; [user_action]
  → (image turn)        composer [此刻穿着] = plan.clothing
  → set_action_response response + item insert (accept) / delete (refuse)
  → post_process        marker in user_msg; tier_drop = Bond on refuse
```

## 10. Where it lives

| change | file |
|---|---|
| `Gift`, `ItemKind`, `GivenItem`, `refusal_line`, `Event` `outfit` | `core/src/types.rs` |
| `PersonaItem`, `CompanionPersona.items` | `core/src/persona.rs` |
| named outfit in `decide` / `plan_for`, `clothing` kept on `ReplyImage` | `core/src/pde.rs` |
| migration | `store/migrations/0067_persona_instance_items.sql` |
| items in `load_companion`, item list / insert / delete | `store/src/persona.rs` |
| `set_action_response` transaction | `store/src/chat.rs` |
| DTO, validation, `outfit`, metadata | `server/src/routes/companion_stream.rs`, `routes/companion_async.rs`, `pipeline/chat_queue.rs` |
| items routes | `server/src/routes/persona_items.rs` (new), `routes/mod.rs` |
| judge lines, outfit override, composer slot, inputs audit | `server/src/pipeline/stream.rs` |
| gift phrases, `[character_state]` lines | `server/src/prompt.rs` |
| composer prompt | `llm/src/model_config.rs` |
| `gift_reaction` comment | `server/src/routes/companion.rs` |
| OpenAPI | `crates/eros-engine-server/openapi.json` (regenerated) |

## 11. Documentation

Swept by concept (gift, 礼物, 衣柜, wardrobe, outfit, 穿着, `gift_reaction`,
`[character_state]`), not by identifier:

- `docs/api-reference.md` / `.zh.md`: the `gift` action, `outfit`, the items
  routes and their trust note, the assistant action types.
- `docs/model-config.md` / `.zh.md`, `examples/model_config.toml`: the
  judge's `clothing` guidance, the composer's `[此刻穿着]` slot.
- `docs/affinity-model.md` / `.zh.md` / `.ja.md`: a refused gift drops Bond.
- `docs/architecture.md` / `.zh.md` where `[character_state]` is described.
- `examples/personas/aria.toml`: `possessions` and `wardrobe`, commented.

No migrating guide: the HTTP change is additive. The release notes name the
core breaks listed under **Type** at the top.

## 12. Rollback and deploy

- The engine ships first; the caller adopts `gift`, `outfit` and the routes
  after. A `gift` sent to the previous engine is rejected; an `outfit` is
  silently ignored.
- The migration is forward-only and additive. The previous binary's
  `migrate` refuses a database that holds 0067, so a rollback deploys the
  previous image without running it. That image ignores the table, and
  reads a gift row as an unparseable action, a blank user line, as a tip
  row did in the tip spec's rollback.

## 13. Observability

- `persona_instance_items` answers what she owns, from where, and through
  `user_message_id` which turn gave it.
- `metadata.outfit` on the user row answers what the user named.
- `companion_decision_events.payload` keeps the judge's raw `clothing` and
  `action_response`.
- `chat_images_events` inputs record the `clothing` that reached the picture.
- `PROMPT_LOG_DIR` dumps show the lists, the judge lines and the composer
  payload.

## 14. Testing

- **core.** `Gift`'s serde shape (`{"type":"gift","kind":"clothing","name":"…"}`);
  `GivenItem`'s wire values unchanged; `refusal_line` for `Gift` is `Bond`;
  `decide` on a gift turn returns `ReplyText` with `Accept`; `decide` and
  `plan_for` put a named outfit in `clothing` over the judge's; `plan_for`
  keeps `clothing` on `ReplyImage` and still drops `reply_mode` there.
- **store** (`#[sqlx::test]`).
  - Migration: a `gift_reaction` row becomes `reply`; the CHECK rejects
    `gift_reaction`; deleting an instance cascades its items; deleting the
    gift message nulls `user_message_id` and keeps `source = 'gift'`.
  - `set_action_response`: an accepted gift inserts one row with the
    metadata's kind and name; a second call inserts nothing; a refusal
    deletes it; a non-gift action touches no item; other metadata keys
    survive.
  - `load_companion`: presets first, dedup, the cap of 20 per kind, `gifted`
    on a preset name that was also given, malformed preset entries skipped.
  - Item list / insert / delete.
- **validation.** `gift` requires `kind` and `name`; name bounds and the
  character rule; `outfit` bounds, blank as absent.
- **prompt.** The two `[character_state]` lines (quoting, 「（对方送的）」,
  omitted when empty, before 「此刻的穿着」); the byte-identical prompt with no
  items and no outfit; the gift phrase, subject, marker and fragment; no
  「拒绝」 in the fragment.
- **stream.** The judge lines appear only when non-empty; an outfit overrides
  the judge's `clothing` and holds on the rule PDE, a judge failure and a
  forced image; the composer payload carries `[此刻穿着]` from the judge, from
  an outfit, and `（无）`; `compose_inputs_json` records it.
- **routes.** GET / POST / DELETE, including 403, 404, 422 and preset rows.
- **Regression.** New fields on `CompanionPersona` and `Event::UserMessage`
  and the `GivenItem` rename change the form of existing call sites. No
  existing assertion is weakened or changes its expected value, except two
  that this design reverses: composer-payload assertions that now include the
  `[此刻穿着]` slot, and `plan_for` dropping `clothing` on `ReplyImage`. Each is
  listed in the PR.
- `openapi.json` regenerated; the diff shows only the `gift` variant,
  `outfit`, the items routes and their schemas, and edited descriptions.

## 15. Non-goals

- The judge removing an item (sold, broken, given away).
- Quantities on items.
- Pagination of the items route.
- Removing a genome preset for one instance.
- An outfit on the image-edit route or the compose endpoint.
- Voice, edit turns and `/open` as gift entry points.
- Checking an outfit, the judge's or the caller's, against the wardrobe.
- Client-side work, including the downstream `gift_reaction` type cleanup.

## 16. Decisions taken

- **A child table**, over two JSONB columns on `persona_instances` (no FK
  from inside JSONB, deletion by name or position, and engine-produced state
  in the one table whose ownership is the caller's) and over deriving the
  lists from gift messages (no home for caller writes or removal).
- **Three sources**: gift, genome presets, caller routes. The conversation
  only adds; the caller adds and removes.
- **The client declares `kind`**, over the judge classifying the gift.
- **Gift refusal drops Bond**, as `give`.
- **`GiftItem` → `GivenItem`.**
- **The outfit is per turn and not stored as instance state**; the client
  remembers it. A named outfit wins; otherwise the judge picks, guided by the
  wardrobe.
- **The judge's outfit reaches the picture**, not only a named one.
- **The prompt caps each list at 20 instance names**; the route is uncapped.
- **`gift_reaction` leaves the CHECK**, with any old rows rewritten to
  `reply`.
