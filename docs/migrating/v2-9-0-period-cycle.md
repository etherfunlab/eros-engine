# Migrating to v2.9.0: her period on a 28-day cycle

**Applies to:**
- deployments (one index-only migration, run before the new image starts);
- deployments with female personas of a species other than human (they stop having a period unless their card sets `period: true`);
- clients that read `GET …/effects`, show `period` or `period_scare`, or schedule event draws by the `ends_at` of the last event;
- crate users who call `EventRepo::current`, build `CurrentEvents`, `DrawInput`, `PersonaTraits` or `ActionPlan` by hand, or read `lasts` for `period` or `period_scare`.

Migration `0089` adds one index and changes no data; run it before the new image starts. One field on the wire is additive (`cycle_event` on the effects response), but three behaviours change: a `period` under way is no longer `background_event`, `period_scare` lasts days and no longer stops the draw, and a female persona that is not human stops having a period unless her card says so. One card key, `period`, joins the 1.1 card.
**Design:** [`2026-11-24-period-cycle-design.md`](../superpowers/specs/2026-11-24-period-cycle-design.md)

The other notes for this release are [Affinity 5.0, the relationship status and the cold war](v2-9-0-affinity-5-0.md) and [the focused tone, four together actions and the user's gender in a picture](v2-9-0-focused-tone-together-actions-user-gender.md).

The rules are in [her cycle](../life-events-needs.md#her-cycle), [false alarms](../life-events-needs.md#false-alarms), [persona settings](../life-events-needs.md#persona-settings), [together actions](../life-events-needs.md#showering-bathing-and-the-sauna-together), [the romantic choices](../life-events-needs.md#the-romantic-choices), [the card](../eros-character-v1.1.md#art_metadata) and [the effects route](../api-reference.md#get-v2compsessionsession_ideffects).

## What users will notice

- **Her period comes every 28 days.** The first `period` an instance draws anchors a cycle, and `period` is never drawn for that instance again. Every later period starts 28 days after the one before was due, at the anchor's time of day, and lasts 1, 3 or 5 days for `health` `strong`, `normal` (or absent) or `weak`; the drawn one lasts the same, not 3 to 7 days. A later period is derived when read: it is never written as a row and never told. She is simply in it, and `正在经历`, the judge's `[近期事件]` and the picture's `[此刻状态]` say `月经中`.
- **Who has a period follows a card key.** `art_metadata.period` is a boolean; any other value reads as absent. Absent, a female human (`species` absent, blank or `human`) has a period and a female of any other species does not. `true` or `false` decides for a female who is not a machine; a `robot`, an `android` and a persona whose `gender` is not `"female"` never has one. A female elf, beastkin, deity and so on drew `period` and `period_scare` before and stops unless her card sets `period: true`. A persona with no period has no cycle whatever rows her instance holds, so a `period` row of hers still under way stops showing at once.
- **Her period runs beside a trip or an illness.** It was a background, and one background kept out the other. It now has a slot of its own: she can be on a trip and on her period, and the prompts list the background, her period, then the incident. During her period, the drawn one or a derived one, no trip or `body` kind is drawn, neither false alarm is drawn, and `irritable` and `unwell` are likelier while it lasts and for 48 hours after, as before. An incident's cue opens with `你正在经期`, after the background's state when there is one.
- **She falls ill more often once she has an anchor.** `period` is in the `body` category with `sick`, `hospitalized` and `post_surgery`, and every draw of it used to start the category's 14-day cooldown. With `period` out of the draw once she has an anchor, and a derived period starting no cooldown, every `body` pick is an illness: she falls ill as often as a persona with no period, roughly 1.4 to 2 times as often as before.
- **`period_scare` is a late period.** It is drawn only for an instance with an anchor, from intimacy rung 2, in the three days before her next period is due; a `having_sex`, `just_have_sex` or `having_sex_together` that started in those days makes it three times as likely. It lasts until one to three whole days after the due moment, not an hour, and that period starts when it ends; the due moments after it do not move. Its opening cue is now `（按日子月经这两天就该来了，身体却一点动静都没有，你心里有点慌。）`; the label `月经没来` and the aftermath `以为月经没来，后来来了，是虚惊一场` are unchanged. While it lasts it is no incident: other incidents and backgrounds are drawn, and a write that ends the incident under way leaves it running. It has no cooldown of its own: the window allows one a cycle. `pregnancy_scare` is unchanged.
- **Sex and water during her period.** The user's `sex_together`, `bath_together`, `hot_spring_together`, `sauna_together` and `swim_together` are rolled before the judge while her period is under way. Two times in three the action is refused, whatever the judge answers and with or without the LLM judge: the `[用户动作]` line ends with `，这次你不会答应`, and the refusal places nothing and drops no line. The rules that already refused these actions (a pair that is not romance-compatible, an action her world lacks) come first and keep their own outcomes. The other third goes to the judge as on any day. A late period rolls nothing.
- **Her asks during her period.** She does not ask `sleep_together`, nor `fall_asleep_together` in its place, and does not ask `bathe_together`; the ask waits for a turn after her period. `propose`, `fall_asleep_together` and `travel_together` are asked as usual. Her period is no background, so it no longer holds back `travel_together`.
- **The world system hears of her first period only.** The story and world directors read event rows: they see the drawn `period` as any event and no derived one, and a `period` row is no longer the `current_background` they read, so it cannot take a trip's place.

## Deployments

### Migration `0089` and deploy order

`0089` creates one partial index, `persona_events_cycle_idx`, on `engine.persona_events (instance_id, kind, started_at DESC) WHERE kind IN ('period', 'period_scare')`. It adds no column and rewrites no row. Every turn reads an instance's newest `period` and `period_scare` rows however old they are; without the index that read walks the instance's whole event history. The index is built with a plain `CREATE INDEX`, which holds back writes to `engine.persona_events` while it builds.

1. **Run `migrate`.** The previous image ignores the index. On a database without `0088`, `migrate` applies it first, so the window that [Affinity 5.0](v2-9-0-affinity-5-0.md#migration-0088-and-deploy-order) asks to keep short includes this build.
2. **Start the new image.**

### What the upgrade does to existing data

- **Instances that already hold `period` rows** join the grid of their newest one the moment the new image runs. Some are on a derived period at once, with no message announcing it: `cycle_event`, `正在经历` and the judge show it from the first read. None of them draws `period` again.
- **A `period` row under way** keeps its stored span, up to seven days, and is read as her period: it moves from `background_event` to `cycle_event`.
- **Old one-hour `period_scare` rows** are long over and delay nothing.
- **Non-human female personas** lose their cycle and their rows stop counting, unless their card is set to `period: true`.
- **A deployment that keeps `period` out of `kinds`** gives no instance an anchor, so it never draws `period_scare` either.
- **Your judge prompt** needs no change. The period roll uses the `，这次你不会答应` ending of the other refusals ruled before the judge, and `[近期事件]` gains `月经中（…）` between the background and the incident.

### Rollback

The index can stay; the previous image ignores it. On the previous image:
- `period` is drawn by the old gate (a female persona that is not a machine) every 28 days at most and lasts 3 to 7 days; the derived periods are gone, since none was written;
- a `period_scare` row of several days written by the new image reads as a foreground incident: it stops that instance's draws until the row ends, and the previous image's writes that end the incident under way (an accepted together action, an accepted ask) cut the row short;
- `cycle_event` is gone from the effects response, and a `period` row under way is `background_event` again.

**Two image versions on one database.** Everything above also holds while a machine of each version serves the same database, during a rolling deploy or with a test deployment left on the previous image. A `period` the previous image draws for an instance that already has an anchor becomes the newest `period` row, and the new image re-anchors the cycle on it.

## Clients

1. **Read `cycle_event`.** `GET /v2/comp/session/{session_id}/effects` gains `cycle_event`, always present: an `EventView` of `kind` `period` or `period_scare` with `"offline": false`, or `null`. A period her cycle derived has `"message_id": null`. A client that deserializes the response strictly must accept the field.
2. **Stop reading her period from `background_event`.** It is now only a trip or an illness. A client that showed her period from `background_event` shows it from `cycle_event`.
3. **Re-read at `changes_at`.** `changes_at` also counts the end of `cycle_event` and the start of her next period. A derived period starts with no event and no message, so this re-read is how a client learns of it.
4. **Do not wait for a `period_scare`'s `ends_at` before drawing again.** A drawn `period_scare` comes back with an `ends_at` days away, and it does not stop the draw. A client that schedules its next draw after the last event's `ends_at` would leave her day empty for days.
5. **Keep `period` in `kinds`** for personas that should have a cycle. Without it no instance gets an anchor, and `period_scare` is never drawn.
6. **Offer the `period` switch** where a client edits cards: for a female persona that is not a machine, on by default for a human and off for any other species.
7. **Expect more refusals of the five actions.** During her period a `sex_together`, `bath_together`, `hot_spring_together`, `sauna_together` or `swim_together` is refused before the judge two times in three, and the judge decides the rest. The refusal arrives as any other, the `action_response` frame and the stored `response` both `refuse`; one the roll decided drops no tier, one the judge decided drops chemistry as before.

## Crate users

- `eros_engine_store::events::EventRepo::current` takes a fourth argument, `period: Option<Health>`: the persona's `PersonaTraits::cycle_health()`. With `None` the cycle rows are not read, and a `period` or `period_scare` row under way lands in no slot.
- `CurrentEvents` gains `cycle: Option<CycleEvent>` (her period or a late period under way; `message_id` is `None` for a derived period) and `cycle_rows: CycleRows` (the two rows the cycle was derived from). A struct literal adds both, or starts from the new `CurrentEvents::none(at)`. A `period` or `period_scare` row is never `foreground` or `background`.
- `EventRepo::insert` lets a `period_scare` under way block no write; `end_foreground_in_tx` ends only foreground kinds and leaves a `period_scare` running; `backgrounds_at` leaves `period` out; `unresolved_scare` returns the false alarm that ended last rather than the one that started last.
- `eros_engine_core::events` gains `Slot` (`Foreground`, `Background`, `Cycle`), `EventKind::slot()` and `kinds_in(slot)`. `EventKind::is_background()` and `background_kinds()` still count `period`, whose category is `body`; use `slot()` for where a row under way sits.
- `DrawInput` gains `cycle: CycleRows`: the newest `period` and `period_scare` rows started by `now`, however old (`CurrentEvents::cycle_rows`), or `CycleRows::default()` for a persona with no period. `draw` draws `period` only without an anchor, for `cycle::period_days(health)`, and `period_scare` only in the window before the next due moment, until `cycle::scare_ends`. A `period_scare` under way no longer stops the draw. `lasts(Period, _)` and `lasts(PeriodScare, _)` now only bound `lookback()` (1 to 5 days and 1 to 6 days) and are not the drawn length; `period_scare` has no cooldown of its own.
- `PersonaTraits` gains `period: bool`, the gate on `period` and `period_scare` in place of `female && !machine`, and `cycle_health()`, `Some(health)` for a persona with a period. A struct literal adds the field; the default is `false`.
- The new module `eros_engine_core::cycle` holds the derivation: `CYCLE_DAYS` (28), `SCARE_WINDOW_DAYS` (3), `SCARE_MAX_LATE_DAYS` (3), `period_days`, `CycleRows`, `state`, `latest_period`, `next_due`, `next_period_start` and `scare_ends`.
- `eros_engine_core::pde` gains `period_refuses(event, on_period, roll)` and `PERIOD_ACCEPT` (`1/3`). `ActionPlan` gains `refusal_free: bool`, set on a refusal ruled before the judge that drops no line; a struct literal adds it.
