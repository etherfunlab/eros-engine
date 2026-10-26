# `as_of` on `/effects`, and seven more life events — Design

- **Date:** 2026-10-25
- **Status:** Approved
- **Type:** Engine change. No migration. Additive API changes: one field on
  `GET /v2/comp/session/{session_id}/effects`; seven drawable kinds
  (`eros-engine-core::events::EventKind` gains seven variants, which breaks
  exhaustive matches). Ships as v2.3.1.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-10-15-life-events-v2-design.md` (*the v2 spec*),
  `2026-10-16-life-events-offline-and-scares-design.md` (*the offline
  spec*), `2026-10-17-life-events-run-ins-and-kind-filters-design.md`, and
  the `changes_at` field (#427). Everything there stands; this spec adds to
  the kind table and to one response.

## 1. `as_of` on the effects response (#434)

### 1.1 Why

`changes_at` is an absolute server time. A client that re-reads at that
time can only compare it with its own clock; when the device clock runs
ahead of the server, the re-read arrives early, gets the same `changes_at`
back, and the client has to stop rescheduling, leaving the needs and the
event state stale until the next turn. The response carries no server
time, and the HTTP `Date` header is not readable cross-origin.

### 1.2 The field

`EffectsResponse` gains `as_of`: RFC 3339, never null, the `now` the handler
already takes once and derives `changes_at` and the carried needs from. A
client schedules its re-read by the duration `changes_at − as_of`, counted
from when it received the response, and judges a cached copy by its local
read time plus that duration. Nothing else on the route changes.

```json
{"effects": [], "needs": {…}, "event": null, "background_event": null,
 "as_of": "2026-10-16T13:00:00Z", "changes_at": "2026-10-16T14:00:00Z"}
```

Invariant: `changes_at`, when present, is strictly after `as_of`. The
route's existing reads already use one instant, so this holds by
construction; a test pins it.

## 2. Seven more kinds

| kind | label | category | weight | cooldown | lasts | offline | told | gate |
|---|---|---|---|---|---|---|---|---|
| `admiring_scenery` | 在看风景 | `short` | 2 | 24 h | 1 h | no | yes | none |
| `admiring_art` | 在看展 | `short` | 1 | 48 h | 1 h | no | yes | none |
| `taking_selfie` | 刚拍了张自拍 | `short` | 2 | 24 h | 30 min | no | yes | none |
| `on_the_road` | 在路上 | `mid` | 2 | 24 h | 2 to 4 h | no | yes | none |
| `watching_match` | 在看比赛 | `mid` | 2 | 24 h | 2 to 3 h | no | yes | none |
| `watching_show` | 在看表演 | `mid` | 1 | 72 h | 2 to 3 h | no | yes | none |
| `having_sex` | 在做爱 | `mid` | 1 | 48 h | 1 to 2 h | every persona | never | none |

All seven join the `kinds` filter vocabulary; the four `mid` kinds join
the `prefer_events` vocabulary. The drawable count goes from 43 to 50.

### 2.0 The four told incidents of the day

She is looking at a view, at art, at a show, or has just taken a selfie.
Each is told in one message, she keeps replying, and the details are hers:

```
（你正在看眼前的风景，看得有点出神。）
（你正在看展，眼前有一件作品让你停了下来。）
（你刚拍了一张自拍，想发给对方看看。）
（你正在看一场演出。）
```

`taking_selfie` is words only: the message tells of the photo and no image
is composed, since the proactive path carries no `image_request`. An event
message with an image is a separate issue. `admiring_scenery`,
`admiring_art` and `watching_show` are ruled out during `hospitalized` and
`post_surgery`; a selfie can be taken from a bed.

### 2.1 `on_the_road`

A long stretch of travel inside a day: a long commute, a coach, a train, a
flight. `on_the_way` (30 minutes) stays as the short errand it is; the two
coexist. The cue states the fact and leaves the vehicle to her world:

```
（你正在路上，这一趟要走好几个小时。）
```

She keeps replying. Ruled out during `hospitalized` and `post_surgery`, like
every kind that needs her out and about.

### 2.2 `watching_match`

She is watching a match. Which sport, where, and with whom are hers; a
historical or fantasy persona reads it in her world. Not ruled out during a
hospital stay: a match can be watched from a bed.

```
（你正在看一场比赛。）
```

### 2.3 `having_sex`

An offline kind for every persona, never announced, no gate: a decision in
the spirit of default-open. Who she is with is hers to say when she
explains afterwards; the engine states only the fact. It rides the offline
machinery wholesale (the offline spec §3–§4): the ghosting switch and the
10-message floor gate the draw; turns ghost without the judge while it
lasts; `/open` returns `null`; it starts the shared 12-hour offline
cooldown; the first assistant row with text afterwards closes it. Ruled
out during `hospitalized` and `post_surgery`.

Afterwards, in a session it ghosted:

| surface | text |
|---|---|
| `- 刚才：` | `在做爱，对方那时发来的消息你现在才看到` |
| judge `[近期事件]` | `在做爱（{span}前结束，期间对方的消息你没有回）` |

It carries an adult theme. The engine keeps no group of such kinds; the
docs list it beside `sudden_desire` and `pregnancy_scare` as a kind a caller
serves only to some users through `disallow`.

### 2.4 What does not change

No needs factor touches the seven. No persona setting beyond
`prefer_events`. The categories' weights and cooldowns, the two-stage draw
and the one-incident rule stand. All seven reach the story director like
any other kind (`2026-10-23-life-events-world-wiring-design.md` §3.1),
`having_sex` included: what happened in her life happened, and who she was
with is for the director to settle against her `romance_history`. No
kind-specific exclusion in the story read.

## 3. Release

v2.3.1, a patch: the nine-file bump, README image tags, OpenAPI regenerated;
the `= "2.3"` crate snippets stay. No migration guide: a downstream reads
`as_of` when it wants to and sees seven more kinds on an unfiltered draw.

## 4. Out of scope

- Any retry or backoff on the client side: `as_of` is the whole fix.
- Exposing the `Date` header through CORS.
- A gender gate or an intimacy gate on `having_sex`; a kind grouping for
  adult themes.
- Changes to `on_the_way`.
- An image on an event message (`taking_selfie` with a real photo): its own
  issue and spec.

## 5. Testing

- `as_of` present on every response, equal to the instant `changes_at` is
  derived from; `changes_at > as_of` when `changes_at` is present; the
  stream frame and every other route untouched.
- Core: the seven kinds parse, categorise as above, carry the rules above;
  `having_sex` is offline for every persona and never told; the other six
  are told at every rung and never offline; `rules_out` during
  `hospitalized` covers `on_the_road`, `having_sex`, `admiring_scenery`,
  `admiring_art` and `watching_show` but not `watching_match` or
  `taking_selfie`; `ALL` has 50 entries and the `kinds` filter accepts the
  seven.
- Server: labels, cues, the aftermath text and the judge suffix for
  `having_sex`; `away`-style ghosting of a turn during `having_sex`; the
  gates leave it out of the draw when ghosting is off or the session is
  under 10 user messages.

## 6. Documentation

`docs/api-reference{,.zh}.md` (the kind table, the `kinds` prose, the
offline list, the hospital list, the effects example and `as_of`),
`docs/model-config{,.zh}.md` (the offline list and the per-kind text table),
`docs/life-events-needs{,.zh}.md` (the counts, the offline table, the
hospital list, the `/effects` paragraph), `docs/ghost-mechanics{,.zh}.md`
(the offline list), and `examples/model_config.toml` if it names the
offline kinds. The seven kinds and `as_of` in the v2.3.1 release notes.
