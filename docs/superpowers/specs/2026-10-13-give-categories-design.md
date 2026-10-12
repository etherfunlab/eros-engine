# More things to give — Design

- **Date:** 2026-10-13
- **Status:** Draft — ready for review
- **Type:** Engine change. No migration. The `give` action's `item` enum gains
  fourteen values; nothing is removed. `eros-engine-core`'s `GivenItem` gains
  fourteen variants, which breaks downstream code that matches on it
  exhaustively; the crates release that carries it bumps accordingly.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — the `give` action, its lingering effects and
  its prompt phrases.
- **Builds on:** `2026-10-05-chat-user-actions-design.md` (*the action spec*)
  and `2026-10-12-given-item-lingering-effects-design.md` (*the effects
  spec*).

## 1. Motivation

`give` hands over three kinds of things: a cigarette, a drink, a pill. A user
who wants to bring food, make tea, or hand over a watch has no category for
it. This design adds fourteen: five consumables that linger on the persona the
way the existing three do, and nine things of value that do not.

## 2. Categories

| `item` | unit | word without a name | group | curve shape |
|---|---|---|---|---|
| `cigarette` | 支 | 烟 | consumable | front |
| `alcohol` | 杯 | 酒 | consumable | back |
| `medicine` | 片 | 药 | consumable | middle |
| `food` | 份 | 吃的 | consumable | front |
| `soft_drink` | 瓶 | 饮料 | consumable | front |
| `tea` | 杯 | 茶 | consumable | front |
| `coffee` | 杯 | 咖啡 | consumable | middle |
| `herb` | 包 | 药草 | consumable | middle |
| `watch` | 块 | 手表 | valuable | — |
| `gem` | 颗 | 宝石 | valuable | — |
| `gold` | 块 | 黄金 | valuable | — |
| `camera` | 台 | 相机 | valuable | — |
| `cash` | 张 | 钞票 | valuable | — |
| `antique` | 件 | 古董 | valuable | — |
| `artwork` | 件 | 艺术品 | valuable | — |
| `handmade` | 份 | 手作礼物 | valuable | — |
| `credit_card` | 张 | 信用卡 | valuable | — |

The first three rows are unchanged. `given_phrase` renders every category the
same way: `{quantity} {unit}「{name}」` with a name, `{quantity} {unit}{word}`
without one (「2 份「蛋糕」」, 「1 杯咖啡」, 「3 颗宝石」).

## 3. Curves by shape

The effects spec's three tables are three shapes:

| shape | today's table | the effect is |
|---|---|---|
| front | `CIGARETTE` | strongest at first, then fades |
| back | `ALCOHOL` | builds up, ends at its peak |
| middle | `MEDICINE` | rises, peaks, fades |

`effects.rs` keys its tables by shape instead of by item:

```rust
enum Shape { Front, Back, Middle }

/// The curve shape of a consumable; `None` for a valuable.
fn shape(item: GivenItem) -> Option<Shape>;
```

- The rows and the quantity > 5 rows are moved, not changed: `FRONT` holds
  today's `CIGARETTE` rows, `BACK` today's `ALCOHOL`, `MIDDLE` today's
  `MEDICINE`.
- `curve(item, quantity)` returns the shape's row, and `&[]` for a valuable
  at every quantity.
- `active_effects` is unchanged. An empty row already yields no effect, so an
  accepted valuable never appears in `body_state`.
- Direction (`rising` / `peak` / `fading`) still comes from the row's peak, so
  each new consumable takes its shape's directions.

## 4. Effect words

`effect_word(item, level)` gains a row per new consumable. As with the
existing three, quantity sets only how long an effect lasts: one portion of
food is at level 3 on its first step, and level 4 is reached only by a
quantity above 5. Each word says how strong, not what it feels like.

| `item` | 1 | 2 | 3 | 4 |
|---|---|---|---|---|
| `food` | 还留着点饱 | 挺饱的 | 刚吃饱 | 吃撑了 |
| `soft_drink` | 嘴里还有点甜 | 挺解渴 | 正爽口 | 喝得肚子发胀 |
| `tea` | 茶香还在 | 心静了些 | 神清气爽 | 喝得有点心慌 |
| `coffee` | 提了点神 | 精神起来了 | 精神很足 | 心跳得有点快 |
| `herb` | 药草劲还轻 | 药草见效了 | 药草劲很足 | 药草劲太冲 |

The `[身体状态]` / `身上还带着` rendering is unchanged:

```
2 份「蛋糕」，刚吃饱，正在劲头上；1 杯咖啡，提了点神，还在往上走
```

## 5. Valuables

- A valuable is a `give` like any other. The judge accepts or refuses it,
  the accepting turn renders `[user_action]` 「…，你接受了」, and a refusal
  drops the bond line (`refusal_line` is unchanged).
- It has no lingering effect. Nothing renders after the accepting turn.
- It does not join the persona's items. `gift` is the action for something
  she keeps.
- `cash` is money handed over in person, counted in notes and never valued.
  `tip` is the action that carries an amount and is valued by the engine
  (`2026-10-11-tip-as-action-design.md`). The two stay separate.

## 6. Wire

- `GivenItemDto` gains the fourteen values and maps one-to-one onto
  `GivenItem`. The `quantity` rules (1–99) and the `name` rules are unchanged.
- `EffectView.item` uses the same enum, so its schema lists every value. Only
  consumables ever appear there.
- `openapi.json` is regenerated. Its diff shows only the enum.

## 7. Where it lives

| change | file |
|---|---|
| `GivenItem` variants | `core/src/types.rs` |
| `Shape`, `shape`, tables keyed by shape | `core/src/effects.rs` |
| units, words, effect words | `server/src/prompt.rs` |
| `GivenItemDto` and its mapping | `server/src/routes/companion_stream.rs` |
| OpenAPI | `crates/eros-engine-server/openapi.json` (regenerated) |

## 8. Documentation

- `docs/api-reference.md` / `.zh.md`, under the `give` action: the
  category list with each unit; which categories linger and their shape; that
  valuables do not linger and do not join the persona's items, with a pointer
  to `gift`; `cash` beside `tip`.
- The concept sweep (give, 递给你, category names, effects) across `docs/`,
  `examples/` and the READMEs.

No migrating guide: the change is additive.

## 9. Rollback

No migration. An older image that reads a stored action with a new `item`
value fails to parse it and treats the turn as having no action: the history
marker and any lingering effect of that give disappear. Nothing else changes.

## 10. Testing

- **core.** `shape` for every item. Each new consumable's `curve` equals its
  shape's row for quantities 0, 1–5, 6 and 99. Every valuable's `curve` is
  empty for every quantity. `active_effects` skips an accepted valuable and
  still returns the consumables around it. The existing per-item table test is
  unchanged.
- **prompt.** `given_phrase` for every new item, with and without a name.
  `effect_word` for every new consumable at every level. `action_phrase` for a
  new give. The existing `given_phrase` and `action_phrase` assertions are
  unchanged.
- **route.** Each new `item` value is accepted and stored. An unknown value
  still returns 422.
- **stream.** A turn after an accepted `coffee` carries its effect into the
  judge context, the reply prompt and the `body_state` frame. A turn after an
  accepted `watch` carries none.
- **Regression.** No existing assertion changes.

## 11. Non-goals

- Valuables joining the persona's items.
- Any value or price for a valuable.
- New curve shapes, or quantity raising a peak beyond the quantity > 5 row.
- Any direct effect of a give on the persona's needs. The judge reads
  `[身体状态]` and rates the needs itself (`2026-10-13-persona-needs-design.md`).

## 12. Decisions taken

- **Tables keyed by shape.** The existing tables are three shapes, and five
  of the new items reuse them, so the tables are keyed by shape rather than
  given a near-duplicate table per item.
- **Valuables do not linger and are not kept.** Keeping is `gift`'s job, and
  what a valuable does to the persona belongs to the judge.
- **`cash` is not `tip`.** One is a physical hand-over, the other a valued
  transfer.
