# The passion group: a customisable `sex_together` — Design

- **Date:** 2026-11-27
- **Status:** Approved
- **Type:** Engine change. No migration. Additive wire change: optional
  fields on the `sex_together` action. Behaviour changes: `sex_together`
  leaves the naturism group for a group of its own, regains a romance gate
  (with a bypass), and a named act's refusal drops both lines (§6).
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-11-27-together-action-groups-design.md` (the groups,
  `refusal_lines`, the picture table), `2026-11-26-more-together-actions-bath-outfit-chemistry-drop-and-custom-together-design.md`
  §8 (`custom_together`: free text that rides on the user's message),
  `2026-11-24-focused-tone-four-together-actions-and-user-gender-design.md`
  §7 (`user_gender`, `[对方]`).

## 1. Intent

Sex gets a group of its own, separate from naturism, so the engine tells a
sexual act from simply being naked. It is the upgrade of the plain
`sex_together`: the downstream client can name the act, say who leads and
what each of them still wears, and the picture is composed from that.

The engine ships **no act names**. The downstream passes the name it wants
with each request. Reasons *(owner's)*: nothing to maintain in the engine,
nothing in a public repository that its host's rules would object to, and
room for each client to define its own acts.

**Naming.** The code calls the group `Sex` (`TogetherGroup::Sex`).
Documentation calls it **passion** (中文「激情组」) — not "intimacy", which
already names an affinity axis. *(Owner's rulings.)* With `sex_together`
gone, every naturism action is now naturism in the plain sense.

## 2. Goals, non-goals, anti-goals

**Goals.** §4–§8.

**Non-goals.** A catalogue, enum or table of act names; the act affecting
anything but prompts and pictures (no act-specific affinity, timing or
events); a passion version of any action other than `sex_together`; group
chat; the act, the leader or the clothing on group or event pictures
(§7.3).

**Anti-goals.** Any act name, explicit word or sample act in the
repository (code, tests, docs, OpenAPI) — tests use neutral placeholders; a
second stored copy of the act (it lives on the user's message only).

## 3. Approaches

Three ways: (a) optional fields on the existing `sex_together`; (b) a
second action type for named acts beside a plain `sex_together`; (c) the
act passed as a per-turn request parameter outside the action. (a) is
taken. (b) duplicates the gate, the lists and the event kind for one
fact. (c) loses the act on later turns unless the client resends it, and
the owner chose to keep it on the user's message (§5.3).

## 4. The group

`TogetherGroup` gains `Sex`. Its one kind is `having_sex_together`, which
leaves `Nude`. The passion group:

- **has a romance gate**: a `sex_together` from a pair that is not
  romance-compatible is refused before the judge, unless the action says
  `ignore_orientation: true` (§5.4). This restores, for this group only,
  the gate the groups spec removed from naturism. *(Owner's ruling.)*
- **speaks in the `emotional` tone**, accepted or refused, as before.
- **ends on a chemistry drop** (`ENDS_ON_CHEMISTRY_DROP` = the naturism
  kinds and the passion kind).
- **is kept out of the story director**, as before.
- **is rolled on her period and holds her asks** through the explicit
  `sex_together` / `having_sex_together` terms those two rules already
  carry; it is not wet.

*(Owner's rulings: the group, its separation from naturism, the gate.
Engine's calls: tone, chemistry-drop end, story exclusion and the period
and held-ask rules carried over unchanged.)*

A row her own accepted `sleep_together` ask places is a passion row with
no act: the defaults of §5.

## 5. The action

### 5.1 Wire fields

`sex_together` takes, all optional:

| field | value |
|---|---|
| `name` | the act, plain text, 1..=`MAX_ACTION_TEXT_CHARS` after trim |
| `name_base64` | the same, base64 (STANDARD) of UTF-8 text; not with `name` |
| `active` | `"user"` or `"persona"`: who leads; the other is passive |
| `remain_clothing` | `false` (default), `true`, or an object (§5.2) |
| `ignore_orientation` | `false` (default) or `true` (§5.4) |

`name` and `name_base64` together, base64 that does not decode, decoded
bytes that are not UTF-8, or a decoded name outside the length bound: 422,
as any malformed action today. *(Owner's ruling: plain or base64, for the same
reason the engine hands the composed prompt back as base64. Engine's call:
two fields rather than a guessed encoding, since short text can be valid
base64 by accident.)*

### 5.2 Clothing

- `false`: both fully naked.
- `true`: both keep what they wear.
- `{"active": <side>, "passive": <side>}`, each side one of
  `"full_naked"`, `"half_naked"`, `{"custom": "<text>"}` or
  `{"custom_base64": "<base64>"}`; a side left out is `"full_naked"`.
  Custom text follows `name`'s bound and decoding rules.

*(Owner's rulings: the default, `true` / `false`, and the per-side object
with full naked, half naked or custom. Engine's calls: the field shapes, a
missing side reading full naked, base64 for custom text.)*

### 5.3 What is stored

The action rides on the user's message as `custom_together`'s text does:
`metadata.action` holds `name` (decoded to plain text), `active`,
`remain_clothing` (custom text decoded) and `ignore_orientation`. No column,
no table. The `having_sex_together` row a user's `sex_together` places
points at that message (`message_id`), as a `custom_together` row does, so
every 1:1 picture while the event lasts reads the same act and clothing.
The asynchronous send route's queue row (`chat_turn_queue.params`) keeps
the request as sent, the action included, so a queued or revived turn runs
as asked; that snapshot is the request's, is never read back by any route,
and goes with its message. It is not a second copy of the act. *(Owner's
ruling.)*
*(Owner's ruling: stored with the user's message, used for the whole event.
Engine's call: the row pointer.)*

**Reading it back.** Whatever the client sent, a read of the action (the
history routes' action view) returns the act as `name_base64` and custom
clothing as `custom_base64`, never in plain text, as the composed prompt
leaves the engine. The session-events view carries no act text for a
`having_sex_together` row. *(Owner's ruling: reads are always base64.
Engine's call: the events view carries none.)*

### 5.4 `ignore_orientation`

`true` skips the passion group's romance gate for that action; the judge
still decides whether she accepts. Meant for debugging. *(Owner's
ruling.)*

### 5.5 Who leads

`active`, when given. Otherwise, from the turn's `user_gender` and the
persona's `art_metadata.gender`: a male user and a female persona → the
user leads; a female user and a male persona → the persona leads; any
other case → the user leads. Resolved when used, never stored. *(Owner's
ruling.)*

## 6. Refusal lines

| `sex_together` | lines |
|---|---|
| with a name | bond and chemistry |
| without a name | chemistry, as before |

The gate's refusal (§4) costs the same lines. `ignore_orientation` changes
nothing here. *(Owner's rulings.)*

## 7. Prompts and pictures

### 7.1 Words

- **Action marker** (history, judge, reply): `想和你上床` as before; with a
  name, `想和你上床：「{name}」`.
- **Event label** (`[此刻状态]`) on a 1:1 picture: `和对方上床`; with a
  name, `和对方上床：「{name}」`.

### 7.2 A 1:1 chat picture

For a `having_sex_together` foreground on a 1:1 chat turn, the act, the
leader and the clothing come from the action the row points at (none for
her own ask's row: the defaults).

- **`[此刻穿着]`**, from her side: full naked → 什么都没穿; half naked →
  半裸; custom → the text; `remain_clothing: true` → 穿着衣服.
- **`[亲密]`**, right after `[对方]` (or after `[此刻状态]` when `[对方]`
  is absent):

  ```
  [亲密]
  主动的一方：对方
  对方穿着：什么都没穿
  ```

  `主动的一方` is `角色` or `对方` (§5.5). `对方穿着` reads the user's side as
  `[此刻穿着]` reads hers.
- **The built-in composer prompt** (`DEFAULT_COMPOSE_PROMPT`) gains one
  line: when `[亲密]` is present, her clothing is `[此刻穿着]` and the
  user's is `[亲密]`'s, over what `[此刻状态]` would imply, and the
  composition follows who leads. The line names no act and no body. The
  `examples/model_config.toml` comment and the composer's input snapshot
  record `[亲密]` as they record `[对方]`.

### 7.3 Group and event pictures

They have no action in hand and no `user_gender`, and a group picture can
seat a second persona beside her. A `having_sex_together` foreground there
reads 什么都没穿, the plain label, and no `[亲密]`, as before.

*(Owner's ruling: the act, who leads and the clothing shape the picture.
Engine's calls: the wording, the section's name and place, 半裸, 穿着衣服
for `true` (a missing line would lose to the state), 角色 rather than a
pronoun (a male persona can lead), the built-in prompt line, and keeping
group and event pictures as they are.)*

## 8. Release notes

The passion group is documented in the docs and the migrating guide only;
GitHub release notes do not list it. *(Owner's ruling.)*

## 9. What changes for a live user

- An incompatible pair's `sex_together` is refused before the judge again
  (it reached the judge only on the unreleased groups change); a client
  can pass `ignore_orientation: true`.
- A refused named act drops bond and chemistry.
- A 1:1 sex picture's clothing and composition follow the action; with no
  fields, it reads as before (nothing on), plus the `[亲密]` section with
  the default leader. Group and event pictures are unchanged.
- The history view returns a `sex_together` action's act and custom
  clothing as base64.

## 10. Tests

- Core: `UserAction::SexTogether` round-trips with and without each
  field; `together_group` of `HavingSexTogether` is `Sex`; the naturism
  list no longer holds it; `ENDS_ON_CHEMISTRY_DROP` holds it; the gate
  refuses an incompatible pair's `sex_together` unless
  `ignore_orientation`; `refusal_lines` with and without a name.
- Server DTO: `name` vs `name_base64` (both → 422, bad base64 → 422, bad
  UTF-8 → 422, length bound after decoding), `remain_clothing` in all three
  shapes, custom and custom_base64; the history view returns
  `name_base64` / `custom_base64` for an action sent in plain text.
- Leader resolution for every `user_gender` × persona gender pair, and an
  explicit `active` winning.
- Pictures: a named act's label and `[亲密]` section; each clothing value
  on both sides (`true` → 穿着衣服); a later 1:1 picture in the same event
  reading the stored act; her own ask's row reading the defaults; a group
  picture during sex reading 什么都没穿 with no `[亲密]`.
- Placeholder act names only.

## 11. Order of work

One code PR after the groups PR (#591) merges. Then one documentation PR
covering the swim-ladder, groups and passion specs, none of which has been
released.

## 12. Migrating notes to write

The new optional fields; the restored gate and its bypass; the two-line
refusal for named acts; the `[亲密]` section; a user-placed
`having_sex_together` row's `message_id` is now the user's message. Not
listed in the GitHub release notes (§8). No migration; rolling back is
redeploying the previous image.

## 13. Left to the builder

- The Rust shape of the payload (a struct variant or an inner struct) and
  of the clothing type. The built-in prompt lives in `eros-engine-llm`
  (`model_config.rs`), a fourth crate this change touches.
- Where the leader resolution and the clothing-to-text mapping live.
- Validation error wording.
