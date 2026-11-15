# Remove the `hunger` and `stamina` needs — Design

- **Date:** 2026-11-16
- **Status:** Approved
- **Type:** Engine change. No migration. Breaking for HTTP and SSE clients:
  the needs object loses `hunger` and `stamina`.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-10-13-persona-needs-design.md`,
  `2026-10-28-outing-indulgence-needs-and-habits-design.md`,
  `2026-10-30-give-needs-romantic-choices-and-answer-receipts-design.md`,
  `2026-11-12-chat-tone-emotional-design.md`.

## 1. Intent

Two of the six needs work against the companion.

- **Hunger** rests at `high`: it climbs one level every 4 hours and settles
  there. Outside the hours right after a meal, the reply's `此刻的需要` line
  says `饿了` on nearly every turn, and the persona keeps asking for food.
  In a production deployment this crowds out the companionship itself.
- **Stamina** only shifts a few event weights and adds `很累` / `精神很好`
  to the reply. In practice it changes nothing a user notices.

The needs become four: `social`, `emotional`, `outing`, `indulgence`.

## 2. Goals, non-goals, anti-goals

- **Goal:** the two needs are gone everywhere: the core type, the judge,
  the reply prompt, event weights, the tone draw, the API and the docs.
- **Non-goals:** no replacement need; no rewrite of stored rows; no change
  to food's body effect; no compensation for the lower emotional-tone rate
  (§4.6). The persona keys stay as they are: the five habit keys
  (`smoking`, `alcoholic`, `drug`, `coffee`, `gluttony`) keep driving
  `indulgence`, the craving named in the reply, the give roll and the habit
  events; `health` keeps scaling the `body` category. Neither reads
  `hunger` or `stamina`.
- **Anti-goal:** keeping `hunger` / `stamina` in the API as placeholders.

## 3. Approaches

- **A — remove both end to end (chosen).**
- **B — hide them from the reply prompt only.** Stamina already has no
  visible effect; hunger would stay in client UIs, the event weights and the
  tone draw, and the judge would still rate it every turn.
- **C — keep the two keys in the API, always `mid`.** Dead fields in a
  public contract. The client-side change is small and works with both the
  old and the new engine, so ordering the deploys (§5) is cheaper.

## 4. Design

### 4.1 Core type

`Needs` (`eros-engine-core/src/needs.rs`) drops `hunger` and `stamina`, and
`HUNGER_DRIFT` / `STAMINA_DRIFT` go with them; `carried` and `next_change`
cover the four that remain. Serde: `social` and `emotional` are required;
`outing` and `indulgence` default to `mid` as today. A stored value that
still carries `hunger` / `stamina` parses, with the two keys ignored. Old
rows are left as written: no migration, no backfill.

### 4.2 Gives

`give_move` moves (social, indulgence):

| `GivenItem` | social | indulgence |
|---|---|---|
| `food` | | |
| `cigarette`, `alcohol`, `herb`, `cuisine` | +1 | +1 |
| `medicine`, `coffee` | −1 | −1 |

Food moves no need: `after_give` passes the judge's rating through (or
writes nothing), as for a valuable. Food's body effect (`刚吃饱` and the
rest of its curve) is unchanged.

### 4.3 Judge

`VerdictNeeds` drops the two fields; `social` and `emotional` are required,
`outing` and `indulgence` keep falling back to the level shown. The strict
schema requires `social`, `emotional`, `outing`, `indulgence`. A judge that
still emits `hunger` / `stamina` (a non-strict judge on an old prompt) has
them ignored. The context line becomes
`[需求] 社交欲=… 情感需求=… 外出=…`, with ` 享受=高` appended as today.

### 4.4 Reply prompt

`needs_line` drops `不饿` / `饿了` and `很累` / `精神很好`.

### 4.5 Life events

`needs_factor` drops three rules: `TempWork` / `WorkingOut` ×0.5 on low
stamina; `Unwell` / `Sick` / `Bathing` / `Asleep` / `WantHotSpring` /
`WantSauna` ×2 on low stamina; `GroceryShopping` ×2 on high hunger. Those
kinds weigh like the rest of their category.

### 4.6 Tone

`emotional_probability` counts her in need when `emotional` or
`indulgence` is high. Hunger resting at `high` used to apply the ×3 on most
turns, so a deployment that sets `tones.emotional` sees fewer emotional
turns and can raise the base. Nothing in code compensates.

### 4.7 API

`NeedsView` (the effects endpoint and the `body_state` SSE frame) has four
keys; `openapi.json` is regenerated.

### 4.8 Docs

Current-state docs drop the two needs: the three READMEs (six needs →
four), `life-events-needs`, `model-config`, `api-reference` (each in both
languages), and the judge prompt in `examples/model_config.toml` and
`examples/model_config.d/decision.toml`. The release carries a migration
guide under `docs/migrating/`. Older specs stay as written.

## 5. Rollout

- A client that treats a needs object without `hunger` or `stamina` as
  invalid must stop requiring them before the engine upgrades. Once it
  does, it works with both the old and the new engine.
- The deployment's judge prompt drops `hunger` / `stamina` together with
  the engine upgrade, not before: the old engine's strict schema still asks
  for them.
- **Rollback:** the previous image. It requires the two keys, so ratings
  the new engine wrote read as no rating, and the judge rates again on the
  next judged turn. A voice call whose snapshot the new engine froze with
  needs reads as malformed on the previous image, so that call starts with
  no injected context; the snapshot is written once, so rolling forward
  again is what restores it.

## 6. Left to the builder

Test rewrites; doc wording.
