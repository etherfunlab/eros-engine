# Migrating to v2.9.0: Affinity 5.0, the relationship status and the cold war

**Applies to:**
- deployments (one migration, and an image swap that must follow it at once);
- clients that show life-choice controls, send user actions, or read affinity snapshots or history rows;
- callers with a custom PDE judge prompt.

Migration `0088` must run before the new image serves, and every machine still on the previous image must be replaced right after it. Every field on the wire is additive, but two behaviours change: her ask for space (`ask_for_space`) is retired, and the user's `propose` is refused unless the pair's status allows it.
**Design:** [`2026-11-23-affinity-50-design.md`](../superpowers/specs/2026-11-23-affinity-50-design.md)

Each session now stores a relationship status, moved only by answered asks, with a cold war that can sit on top of it. The rules are in:
- [the relationship status](../affinity-model.md#relationship-status) and [the composed relation](../affinity-model.md#the-composed-relation);
- [life choices](../life-events-needs.md#life-choices), [the cold war](../life-events-needs.md#the-cold-war) and [the romantic choices](../life-events-needs.md#the-romantic-choices);
- [the life-choice route](../api-reference.md#post-v2compsessionsession_idlife-choiceschoice_id), [the user's actions](../api-reference.md#post-compchatsession_idmessagestream) and [the affinity snapshot](../api-reference.md#get-bffv1compaffinitysession_id).

## What users will notice

- **She asks what they are to each other.** Three new asks: `confess` (she confesses and asks to be together), `partner_up` (she offers to become partners) and `break_up`. With `propose`, they are the only things that move the status: `none`, `dating`, `partner`, `peer`, `fwb`, `situationship`, `engaged` or `ex`. `engaged` is the top status.
  - An accepted `confess` makes the pair `dating` (from `none` or `ex`), `fwb` (from `partner`) or `situationship` (from `peer`). A decline drops chemistry one tier.
  - An accepted `partner_up` makes them `partner`, a declined one `peer`. Neither costs anything.
  - Her `propose` is asked only of a pair that is `dating`, `fwb` or `situationship`. Accepted, they are `engaged`.
  - `confess` and `partner_up` wait 14 days after a status change and after their own last answer. `propose` waits 14 days after an accept or an ignore.
- **No more asking for space.** When the user wears her patience out, the pair falls into a cold war instead: three judged turns in a row at the judge's lowest patience level, or five of the last seven. She says so in one message, then keeps replying, coldly. While it lasts, positive growth on the four line axes is halved and she asks for nothing warm. From a breakable status (`dating`, `partner`, `fwb`, `situationship`, `engaged`) she can ask to break up. It ends after three judged turns above the lowest level, or with an accepted break-up. There is no separation, no offline stretch and no 72-hour slowdown.
- **Her bed ask follows the status.** She asks `sleep_together` (sex) of `dating`, `engaged`, `fwb` and `situationship`, and of `partner` at chemistry tier 4 or more. Otherwise she asks the new `fall_asleep_together`: to fall asleep side by side, placing an `asleep_together` of one to four hours. Each is asked once per session.
- **The user's `propose` needs a status.** It is refused before the judge unless the pair is `dating`, `fwb` or `situationship`; the refusal costs nothing. Accepted, the pair is `engaged`.
- **The user can break up.** A new user action, `break_up`, from a breakable status (`dating`, `partner`, `fwb`, `situationship`, `engaged`). Accepted, the pair is `ex`; refused, a cold war starts. Neither costs a tier.
- **Her prompt names the relationship.** `[relationship]` now states the status, a name for the pair's bond tier × chemistry tier (the *composed relation*), and the cold war. The judge reads the status and the cold war.

## Deployments

### Migration `0088` and deploy order

`0088` adds two columns to `engine.companion_affinity`:
- `relationship_status text NOT NULL DEFAULT 'none'`, checked against the eight status keys;
- `cold_war_since timestamptz`, `NULL` when no cold war is on.

It also widens `companion_affinity_events_event_type_check` to admit `relationship`. It rewrites no row.

The migration sets `lock_timeout = '3s'`. If it cannot take its locks within 3 seconds, for example behind a long transaction on `companion_affinity`, it fails with nothing changed; run it again.

1. **Run `migrate`.** The new image reads both columns, so against a database without `0088` every affinity read fails.
2. **Replace every machine running the previous image right away.** The previous image reads `companion_affinity` with `SELECT *` through cached prepared statements. Once the columns are added, each pooled connection's cached statement fails (`cached plan must not change result type`) until that connection is recycled or the process restarts. Until the swap, the previous image's affinity reads and writes fail: turns lose their affinity write and the snapshot routes answer with errors. Keep the window between the two steps as short as you can.

### What the upgrade does to existing data

- **No backfill.** Every existing session reads `none` with no cold war, including pairs whose proposal was accepted before. Judged turns from before the upgrade count toward the new triggers, so an established pair can be asked to confess or partner up, or fall into a cold war, on its first judged turn after deploy.
- **Pending asks for space.** No `ask_for_space` is asked any more. One still pending at deploy lapses within its 24 hours, or a typed reply converts it to `ignored` at no cost, with no `fell_out`. Answering it at the endpoint returns `409 life_choice_closed`.
- **Separations under way run out.** A `taking_space` already started keeps her offline until it ends, and her return is marked as usual. Stored `ask_for_space`, `taking_space` and `fell_out` rows stay readable in history receipts and the story director. The 72-hour slowdown windows end at deploy: from then on growth is halved only during a cold war.
- **Your judge prompt** needs no change. The judge context gains a `[关系]` line when there is something to say (`[关系] 状态=已订婚；冷战中=否`, or `[关系] 冷战中=是` at `none` during a cold war), and the audit row's `inputs` gain `relationship_status` and `cold_war`.

### Rollback

The previous image ignores the two columns once its connections are fresh, and it never returns `relationship` event rows from the per-turn delta route. It cannot answer the new kinds: its answer route fails with `500` (`stored life choice kind unreadable`) for a `confess`, `partner_up`, `break_up` or `fall_asleep_together`, pending or lapsed. So before rolling back, let every pending ask of those kinds be answered or lapse:

```sql
SELECT count(*) FROM engine.life_choices
 WHERE status = 'pending'
   AND kind IN ('confess', 'partner_up', 'break_up', 'fall_asleep_together')
   AND asked_at > now() - interval '24 hours';
```

Roll back when it returns `0`. The previous image does not read or write the status, so on a later upgrade each session's status and cold war are what they were at rollback; answers given on the previous image move neither.

## Clients

1. **Support the new asks before this image is deployed.** The `kind` of the affinity delta's `life_choice` field, and of the history receipt, can now be `confess`, `partner_up`, `break_up` or `fall_asleep_together`; the options are still `accept` and `decline`. A client without controls for them degrades as before: the ask shows as an ordinary message and a typed answer settles it as `ignored`. But a typed "yes" to her confession moves nothing and starts the 14-day wait, and the user's `propose` is refused until the pair is `dating`, `fwb` or `situationship`, statuses only an accepted `confess` reaches. Until a client can accept `confess` through the endpoint, no session can get engaged.
2. **Read the three snapshot fields.** Every `AffinitySnapshot` (the affinity routes, the life-choice answer and its `resolved` frame, the date's end route) carries `relationship_status`, `cold_war` (boolean) and `composed_relation`, a key such as `good_friend` or `null` at tier 1 on both lines. `composed_relation` is always present, as a string or `null`. A client that deserializes snapshots strictly must accept them. `composed_relation` is the cell of the real tiers, with no compatibility applied; the keys and their Chinese names are in [the composed relation](../affinity-model.md#the-composed-relation).
3. **Handle the new `409`s.** The answer route returns `409 life_choice_closed` when the status has moved to one the kind is no longer asked from (for example, the user's own `break_up` was accepted while her `propose` waited), and for a stored `ask_for_space`. Nothing is written; drop the controls.
4. **Offer `break_up`, and gate `propose`.** `{"type": "break_up"}` is a new action with no fields. `propose` and `break_up` are refused before the judge outside their statuses, at no cost; a client that reads `relationship_status` can hide them there. On a BYOC relationship the status is always `none` and `break_up` is always refused.
5. **Accept `occasion: "cold_war"`.** On both history routes and in the start bundle, the message she sends when a cold war starts carries `occasion: "cold_war"` and no `life_choice` receipt. A client that deserializes `occasion` as a closed enum must accept the new value.
6. **Read `event` after a `fall_asleep_together` accept.** It is an `asleep_together` `EventView`, as after the user's own accepted `fall_asleep_together`.
7. **Retire separation controls.** No new ask is `ask_for_space`, and no new event is `taking_space` or `fell_out`. Keep reading them on older history rows.
