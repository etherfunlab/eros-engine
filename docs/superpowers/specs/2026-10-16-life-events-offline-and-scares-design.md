# Life events: offline kinds and false alarms — Design

- **Date:** 2026-10-16
- **Status:** Draft — ready for review
- **Type:** Engine change. No migration. Additive API changes (five kinds,
  `EventView.offline`). `eros-engine-core::events` gains five `EventKind`
  variants (breaks exhaustive matches) and two functions;
  `eros-engine-store::events` renames `unexplained_away` and gains
  `unresolved_scare`.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — an `offline` attribute on kinds, which
  generalises `away_from_phone`'s silence; three new offline kinds; two new
  short kinds that are false alarms resolved after they end.
- **Builds on:** `2026-10-15-life-events-v2-design.md` (*the v2 spec*). This
  spec extends its kind table (§3.2), its exclusions (§3.3), its cues (§3.4),
  §7 (`away_from_phone`, now one of the offline kinds) and the rendering of
  §8.2. Everything else stands.

## 1. Motivation

`away_from_phone` is the one event that makes the persona unreachable. A day
has more of those: she is busy, she is in the middle of something hard, she
has fallen asleep. Some of them she announces, if she is close enough to the
user to bother. And some things that happen to her are scares that turn out
to be nothing: the user should be able to worry with her, then hear that it
was a false alarm.

## 2. Decisions

1. **Offline is an attribute of a kind, not a category.** A category is the
   event's length and its share of the first draw stage; offline is how the
   persona behaves while the event lasts. `busy` is `mid` by length and
   offline by behaviour.
2. **Every offline kind behaves as `away_from_phone` does** (v2 spec §7):
   the same draw gates, the same silence, the same exemptions, the same
   explanation afterwards. Only the wording and whether she announces it
   differ.
3. **`busy` and `in_battle` are announced at intimacy rung 2 and up**; below
   that she goes quiet without a word. `asleep` and `away_from_phone` are
   never announced.
4. **A shared offline cooldown of 12 hours** bounds how often she goes
   silent, the way the category cooldowns bound trips and body states.
5. **The two false alarms are told as the scare, then resolved.** The
   persona does not know it is a false alarm while it lasts; after it ends,
   her next message in the session where she told it carries the
   resolution.
6. **Offline-ness and announcement derive from the kind** (and the rung).
   Neither is stored.

## 3. Kinds

### 3.1 New kinds

| kind | 中文 | category | weight | cooldown | lasts | gate | offline |
|---|---|---|---|---|---|---|---|
| `period_scare` | 月经没来 | `short` | 0.5 | 28 d | 1 h | `female`; rung ≥ 2 | no |
| `pregnancy_scare` | 验出怀孕 | `short` | 0.5 | 60 d | 1 h | `female`; rung 3 | no |
| `busy` | 正在忙 | `mid` | 2 | 12 h | 1–3 h | the offline gates | yes; announced at rung ≥ 2 |
| `in_battle` | 在打一场硬仗 | `mid` | 1 | 48 h | 2–4 h | the offline gates | yes; announced at rung ≥ 2 |
| `asleep` | 睡着了 | `mid` | 2 | 24 h | 1–4 h | the offline gates | yes; never announced |

`away_from_phone` keeps its rule (`mid`, 1, 48 h, 1–3 h) and becomes offline,
never announced. That makes 33 kinds, four of them offline.

`female` is `PersonaTraits::female` (`art_metadata.gender == "female"`), as
for `period`. The rung gates reuse `CONFIDE_MIN_RUNG` (2) and
`DESIRE_MIN_RUNG` (3).

`in_battle` is worded so that the persona's world decides what it is: a
modern persona reads 「一场硬仗」 as a hard negotiation, a business dinner or
a difficult matter; a historical or fantasy persona as a real fight.

### 3.2 Exclusions

- While `period` is the background under way, `period_scare` and
  `pregnancy_scare` are not drawn.
- `in_battle` joins the kinds ruled out during `hospitalized` and
  `post_surgery` (v2 spec §3.3).

Both are cases of the existing `rules_out(background, kind)`.

### 3.3 Needs

`asleep` weighs ×2 when `stamina` is `low`, beside `unwell`, `sick` and
`bathing`. The other new kinds have no needs factor.

### 3.4 The offline cooldown

A shared offline cooldown runs from the start of the instance's newest
offline event, of any offline kind: for 12 hours no offline kind is
eligible. Per-kind cooldowns still apply. `lookback()` is unchanged (90 d
already covers 12 h).

### 3.5 Core API

```rust
impl EventKind {
    /// Offline kinds make the persona unreachable while they last.
    pub fn is_offline(self) -> bool;
}

/// Whether a drawn kind is told in a message when it starts: every
/// non-offline kind; `busy` and `in_battle` from rung 2; never `asleep` or
/// `away_from_phone`.
pub fn is_told(kind: EventKind, rung: u8) -> bool;
```

`EventKind::ALL` grows to 33. `draw` applies §3.2–§3.4.

## 4. Offline kinds

### 4.1 Drawn

- **Gates.** The two v2 gates (§7.1 of the v2 spec) apply to every offline
  kind: with the deployment's ghosting switch off, or in a session with
  fewer than `GHOST_MIN_MESSAGES` user messages, offline kinds are removed
  from the allowed kinds before the draw.
- **Told.** When `is_told(kind, rung)`, the event is generated and written
  with its message like any told kind (the v1 generation path; the cue of
  §4.3). Otherwise it is written with no message (`message_id` NULL), as
  `away_from_phone` is today. A blank or failed generation writes nothing,
  as for any told kind.

### 4.2 While it lasts, afterwards

Every check that reads `kind == AwayFromPhone` reads `kind.is_offline()`:

- the stream's ghost before the judge, with its exemptions (action turns,
  forced-image turns, the ghosting switch off);
- `/open` returning null;
- `EventRepo::unexplained_offline` (renamed from `unexplained_away`): the
  newest offline event of the instance that ended within `lookback()`, with
  a ghosted user row of the session inside its window and no assistant row
  of the session after its end. The announcement, written at the start, is
  before the end and does not count.

The judge's item for an unexplained offline event keeps its shape with the
kind's label: 「睡着了（2 小时前结束，期间对方的消息你没有回）」.

### 4.3 Wording

| kind | label | announcement cue | `刚才` |
|---|---|---|---|
| `busy` | 正在忙 | （你接下来要忙一阵，这段时间顾不上看手机。） | 一直在忙，对方那时发来的消息你现在才看到 |
| `in_battle` | 在打一场硬仗 | （你接下来要去打一场硬仗，一段时间顾不上手机。） | 在打一场硬仗，对方那时发来的消息你现在才看到 |
| `asleep` | 睡着了 | — | 睡着了，对方那时发来的消息你现在才看到 |
| `away_from_phone` | 没看手机 | — | 有段时间没看手机，对方那时发来的消息你现在才看到 |

The `刚才` line reads `- 刚才：{text}`.

## 5. False alarms

### 5.1 Told as the scare

| kind | label | cue |
|---|---|---|
| `period_scare` | 月经没来 | （你发现月经推迟了好几天还没来，心里有点慌。） |
| `pregnancy_scare` | 验出怀孕 | （你刚验了孕，验出两条杠，整个人都懵了。） |

While it lasts, `正在经历` and `[近期事件]` carry the label only. Nothing in
any prompt says it is a false alarm before it ends.

### 5.2 Resolved after it ends

`EventRepo::unresolved_scare(session_id, instance_id, at)`: the newest
`period_scare` or `pregnancy_scare` of the instance that ended within
`lookback()`, whose `session_id` is this session, with no assistant row of
the session in `(ends_at, at]`.

While one is unresolved, every persona message generated in that session (a
turn's reply, an `/open` greeting, an event message) carries its `刚才` text,
and the judge gets an item:

| kind | `刚才` | judge item |
|---|---|---|
| `period_scare` | 以为月经没来，后来来了，是虚惊一场 | 月经没来（{span}前结束，结果是虚惊一场） |
| `pregnancy_scare` | 验出两条杠，后来复查是虚惊一场 | 验出怀孕（{span}前结束，结果是虚惊一场） |

The first assistant row written in the session after the end resolves it.
Other sessions of the instance never carry it: she told it in one.

### 5.3 Both at once

When an offline event is unexplained and a scare unresolved in the same
session, both texts go in one line, the offline text first, joined by `；`:
`- 刚才：睡着了，对方那时发来的消息你现在才看到；以为月经没来，后来来了，是虚惊一场`.
The judge gets both items, offline first.

## 6. Rendering

`EventLines.away_unexplained: bool` becomes `just_now: Option<String>`, the
joined `刚才` text of §5.3, built from the two store reads. The `[近期事件]`
items after the background and the foreground are the unexplained offline
item, then the unresolved scare item. Every reader that loads the
unexplained away event (the stream, `greet`) loads both.

## 7. API

- `EventKindDto` gains the five kinds; `kinds` accepts them; absent `kinds`
  means all 33.
- `EventView.offline: bool`, derived from the kind, on the events route and
  on `event` / `background_event` of `GET …/effects`.
- OpenAPI regenerated.

## 8. Where it lives

| change | file |
|---|---|
| kinds, rules, gates, exclusions, needs factor, offline cooldown, `is_offline`, `is_told` | `core/src/events.rs` |
| `unexplained_offline`, `unresolved_scare` | `store/src/events.rs` |
| offline gates, told vs silent | `server/src/routes/session_events.rs` |
| cues | `server/src/pipeline/proactive.rs` |
| `/open` null | `server/src/routes/session_open.rs` |
| the ghost, the judge items | `server/src/pipeline/stream.rs` |
| labels, `刚才` texts, `just_now`, judge items | `server/src/prompt.rs` |
| `EventView.offline` | `server/src/routes/session_events.rs`, `session_effects.rs` |

## 9. Rollback and deploy

- **No migration.** A previous image reads an event of a kind it does not
  know as absent: it does not go silent for `asleep`, `busy` or `in_battle`,
  does not resolve a scare, and may draw inside such an event's window.
- **Deploying.** With `kinds` absent, the five kinds are drawn from the
  first draw, within their gates.

## 10. Testing

- **core.** The gates (`female`, rungs) and the `period` exclusion for both
  scares; `in_battle` ruled out in hospital and after surgery; the shared
  offline cooldown (an `asleep` 11 h ago blocks `busy`; 12 h ago does not);
  `asleep` ×2 at low stamina; `is_told` for every offline kind at rungs 1
  and 2; `ALL` holds 33 distinct kinds.
- **store.** `unexplained_offline` for each offline kind; `unresolved_scare`
  only in the telling session, resolved by an assistant row after the end,
  not by one before it.
- **route.** The offline gates remove all four offline kinds; `busy` at rung
  2 writes a message and at rung 1 writes none; `asleep` never does;
  `EventView.offline`.
- **stream.** Each offline kind ghosts a text turn; an announced `busy`
  still ghosts; after it, the kind's `刚才` text; a scare's resolution in the
  telling session only, once.
- **prompt.** Labels; each `刚才` text; the joined line; the judge items.
- **Regression.** Whole-body assertions on the events and effects routes
  gain `"offline": …`. Every other edit to an existing test is a call form
  or setup change. `openapi.json` regenerated.

## 11. Documentation

- `docs/api-reference.md` / `.zh.md`: the kind table, the gates and
  exclusions, an "Offline kinds" section generalising `away_from_phone`,
  the false alarms, `EventView.offline`.
- `docs/ghost-mechanics.md` / `.zh.md`: the engine-imposed ghost covers the
  offline kinds.
- `docs/model-config.md` / `.zh.md`: the `[近期事件]` items.
- `docs/life-events-needs.md` / `.zh.md`.
- The README highlight (en/zh/ja): some kinds take her offline.

## 12. Non-goals

- Offline backgrounds (an offline trip).
- Time-of-day awareness (`asleep` at night only).
- A message when an offline event ends; the explanation stays derived.
- A resolution for any kind other than the two false alarms.
