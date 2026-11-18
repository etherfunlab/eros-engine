# Trip vehicles, four actions, four species — Design

- **Date:** 2026-11-19
- **Status:** Approved
- **Type:** Engine change. No migration. Six new event kinds and four new
  action types on the wire; four `species` words gain a label. Two behavior
  changes need a migrating note: `on_the_road` stops being drawn for a
  `modern` persona, and a `robot` or `android` persona stops drawing three
  kinds.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-11-16-prune-life-events-design.md` (§4.2, the
  trip-only kinds), `2026-11-08-strip-and-world-setting-design.md` (§4,
  `setting`), `2026-11-08-five-actions-two-items-gluttony-card-1-1-design.md`
  (§2.3, the picture of an accepted `photo`),
  `2026-10-10-persona-origin-and-birthday-design.md` (`species` and iron
  rule ⓪).

## 1. Intent

A trip has her "on the road" and nothing more specific. The message she
sends from a train, a plane or a cruise ship should say which, so the user
has something concrete to answer and the client something concrete to show.

The user also gets four more things to do to her: two with the eyes, two
about dancing.

A persona card can name four more kinds of being. Two of them are machines,
and the prompt and the events should not contradict that.

## 2. Six vehicle kinds

| kind | she is | lasts | setting | drawn during |
|---|---|---|---|---|
| `on_a_boat` | 坐船 | 1–2 h | both | any trip |
| `on_a_cruise` | 坐游轮 | 2–4 h | modern | `short_trip`, `long_trip`, `hometown_visit`, `traveling_together` |
| `on_a_bus` | 坐大巴 | 2–4 h | modern | any trip |
| `on_a_train` | 坐火车 | 2–4 h | modern | any trip |
| `on_a_plane` | 坐飞机 | 2–4 h | modern | any trip |
| `driving` | 开车 | 1–3 h | modern | any trip |

"Any trip" is the list `missed_ride` uses: `business_trip`, `short_trip`,
`long_trip`, `hometown_visit`, `traveling_together`. The cruise takes the
list `got_lost` uses, which leaves the business trip out.

Common to all six:

- category `mid`, weight 1.0, cooldown 48 hours;
- told, not offline, no picture, not in the emotional list, no gate on
  intimacy or persona keys;
- `place()` is `Outdoor`, as `on_the_road` is, so `outdoorsy` moves them the
  way it moved `on_the_road`;
- no row in `follows`: they are drawn only during a trip already. Six kinds
  at weight 1.0 weigh what `on_the_road` weighed during a trip (2.0 × the
  follow factor 3), so a `modern` persona's trip carries as much travel as
  before;
- being `mid`, they can be named in a persona's `prefer_events`.

Cues and labels:

| kind | cue | during `traveling_together` | label |
|---|---|---|---|
| `on_a_boat` | （你正在坐船。） | （你们一起出来玩，正在坐船。） | 在船上 |
| `on_a_cruise` | （你正在游轮上。） | （你们一起出来玩，正在游轮上。） | 在游轮上 |
| `on_a_bus` | （你正在坐大巴。） | （你们一起出来玩，正在坐大巴。） | 在坐大巴 |
| `on_a_train` | （你正在坐火车。） | （你们一起出来玩，正在坐火车。） | 在坐火车 |
| `on_a_plane` | （你正在飞机上。） | （你们一起出来玩，正在飞机上。） | 在飞机上 |
| `driving` | （你正在开车。） | （你们一起出来玩，正开车在路上。） | 在开车 |

During her own trip the cue opens with the background's state as every
foreground cue does: 「（你正在短途旅行。你正在坐火车。）」. The label is
what she calls the event in her state block (`prompt::event_label`).

## 3. `on_the_road` is fantasy-only

`EventKind::fits` gains a fantasy-only list with one kind, `on_the_road`.
A `modern` persona draws the six vehicles in its place; a `fantasy` persona
draws `on_the_road` and `on_a_boat`.

- `on_the_road` keeps its weight, cooldown, length, cue, label and its
  `follows` rows from the leisure trips. The `(business_trip, on_the_road)`
  row goes: a business trip is modern-only, so the pair can no longer meet.
- `on_the_way` and `missed_ride` are untouched and stay in both settings.
- The kind stays valid everywhere it is named: in `kinds`, in
  `prefer_events`, in stored rows. An `on_the_road` under way at the upgrade
  runs out as it is. A `modern` persona that lists it in `prefer_events`
  keeps a value that no longer does anything.

## 4. Four actions

| `type` | what the user does | phrase |
|---|---|---|
| `wink` | flirts with a wink | 冲你抛了个媚眼 |
| `blink` | bats their eyes at her, playing cute | 冲你眨巴眨巴眼睛 |
| `ask_for_dance` | asks her to dance together | 伸出手，想请你跳支舞 |
| `ask_dancing` | asks her to dance for them | 想看你跳支舞 |

None carries a field. All four behave as `pat` does unless said otherwise
here: the judge decides accept or refuse, an undecided answer is accept,
there is no intimacy gate, a refusal drops the chemistry line a tier, the
reply is not `emotional`, and nothing is written to the journal.

**`ask_dancing`, accepted, shows a picture** the way an accepted `photo`
does: with the request's `image` and a configured
`[tasks.chat_image_prompt_compose]` the turn is `reply_text_image`, her
words first and then an `image_request` built on her face reference. The
composer already reads the action and its outcome, so the picture is of her
dancing. Refused, or without the image capability, the turn is text only
even if the judge proposed a picture. `image.force = true` still makes the
turn `reply_image`, drawn to the refusal when she refused.

## 5. Four species

`art_metadata.species` gains four recognised words, matched as the existing
ones are (whitespace folded, then the exact word):

| `species` | identity label | machine |
|---|---|---|
| `cyborg` | 半机械人 | no |
| `robot` | 机器人 | yes |
| `android` | 仿生人 | yes |
| `zombie` | 丧尸 | no |

The identity line reads 「你是 {name}，仿生人，女性，…」 as it does for
`elf`. The English word goes into iron rule ⓪ as written.

**A machine** (`robot`, `android`) differs in two places:

- Iron rule ⓪ reads "You are a real {species}, not an AI assistant and not
  an actor. You have your own life, feelings, and quirks; you live, feel,
  and react as your kind would." Every other species, and the human
  default, keeps today's sentence byte for byte.
- The draw never picks `period`, `period_scare` or `pregnancy_scare` for
  her. Every other gate stands: she can still be `sick`, `hospitalized` or
  `post_surgery`, and a female machine still sees bags and clothes. The draw
  learns it as `PersonaTraits.machine`, read from the same folded `species`
  value the prompt reads.

`species` was free text already, so a persona that carried one of these
words before the upgrade rendered it verbatim. After it, the identity line
shows the Chinese label, and a `robot` or `android` gets the machine rule ⓪
and stops drawing the three kinds. A `period` under way at the upgrade runs
out as it is. No card version changes.

## 6. Approaches

One way fits: every piece here is a row in a table the engine already has
(`needs_background`, `fits`, `rule`, the action enum, the photo picture
path, `species_labels`, `passes_gate`). Two alternatives for `on_the_road`
were set aside by the owner: keeping it beside the six for every persona
(the same trip leg would have two names), and deleting it (a `fantasy` trip
would be left with the boat alone). For the machines, leaving events alone
(the caller excludes kinds through `kinds`) was set aside because it hands
an engine judgement to the client.

## 7. Tests

- Each vehicle kind: its wire name, category, rule, setting and place; drawn
  during each of its backgrounds and not outside a trip; `on_a_cruise` not
  drawn during a business trip.
- `on_the_road` is drawn for a `fantasy` persona on a trip and never for a
  `modern` one; the five modern vehicles are never drawn for a `fantasy`
  persona.
- The cue and label of each vehicle kind, alone and during
  `traveling_together`.
- The four actions parse from and serialise to their wire names; their
  phrases; `refusal_line` is chemistry; `speaks_emotionally` is false.
- An accepted `ask_dancing` with the image capability is `reply_text_image`;
  refused, or without the capability, it is `reply_text`.
- The four species labels; rule ⓪ for a machine and for a non-machine new
  species; `machine` read from `species`; a machine never draws the three
  kinds and still draws `sick`.

Existing tests that draw `on_the_road` for a default (`modern`) persona move
to a `fantasy` persona or to a vehicle kind.

## 8. Documentation

A separate pull request after the code: `docs/life-events-needs.md`,
`docs/api-reference.md`, `docs/eros-character-v1.md` and
`docs/eros-character-v1.1.md` with their `.zh` twins where they exist (the
catalog count, the kind tables, the trip-only list, the setting lists, the
gates of the three kinds, the action table, the picture paragraph, the
`species` row), and one migrating note for `on_the_road` and the machines.

## 9. Decisions

Ruled by the owner:

- `wink` is the flirting wink, `blink` the cute one; `ask_for_dance` is
  dancing together, `ask_dancing` is her dancing for the user. The four
  phrases of §4.
- `on_the_road` is fantasy-only (§3).
- An accepted `ask_dancing` shows a picture (§4).
- The four species labels of §5.
- A machine's rule ⓪ says "not an AI assistant"; the other species keep the
  sentence they have.
- A machine draws no `period`, `period_scare` or `pregnancy_scare`, and
  keeps the body category.

Calls made in this document, open to correction:

- The six kind names and the four species words.
- All six vehicles are `mid`, online, at weight 1.0 with a 48-hour
  cooldown, and `Outdoor`; the lengths of §2.
- `on_a_boat` fits both settings; the other five are modern-only.
- The cruise is not drawn during a business trip.
- The cue and label wording of §2.
- Each vehicle has its own cooldown, so two different vehicles can follow
  each other on one trip.
- The four actions have no intimacy gate, are not `emotional`, and drop
  chemistry when refused.
- `cyborg` and `zombie` are not machines.

## 10. Left to the builder

- How `fits` names the fantasy-only list next to the modern-only one.
- Where the `traveling_together` cues live in `event_cue`.
- How the two `Photo | Strip` matches in `pipeline/stream.rs` take in
  `AskDancing`.
- How `species_labels` and `persona_traits` share the folded `species`
  value and the machine test.
