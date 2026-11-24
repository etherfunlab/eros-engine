# Modern-only kinds, the `focused` tone, four together actions and the user's gender in a picture — Design

- **Date:** 2026-11-24
- **Status:** Approved
- **Type:** Engine change. No migration. Additive wire change: four new
  action types, four new event kinds (placed, never drawn), one optional
  request field (`user_gender`), one new `metadata.tone` value. One new
  `model_config.toml` key (`tones.focused`), off when absent. Five
  drawn kinds stop being drawn for a `fantasy` persona.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-11-08-strip-and-world-setting-design.md` §4
  (`setting`), `2026-11-12-chat-tone-emotional-design.md` (the tone draw),
  `2026-11-23-twelve-together-actions-and-propose-design.md` and
  `2026-11-23-hot-spring-fight-challenge-bet-design.md` (the together
  actions, the world check),
  `2026-11-20-together-actions-state-in-picture-and-genome-routes-design.md`
  §7 (her state in the picture).

## 1. Intent

Four things, independent of each other:

- A fantasy persona stops drawing five kinds that only make sense in
  modern life.
- While she is at work she answers like someone at work: a second
  non-default tone, `focused`, beside `emotional`. A persona whose card
  says `employed = full_time` is career-minded, and now and then answers
  that way off work too.
- The user can ask her to work out, swim, cycle or hike together.
- A picture that shows her with the user draws the user as the gender the
  caller names. Today the composer hears only 「和对方一起…」 and picks a
  gender at random.

## 2. Goals, non-goals, anti-goals

**Goals.**

- `saw_bag`, `saw_clothes`, `out_shopping`, `want_to_quit_job` and
  `admiring_art` fit `modern` only.
- A 1:1 text turn taken while she is at work, that did not come up
  `emotional`, is written in the `focused` tone, and its assistant row
  says so.
- A `full_time` persona's turn off work has a 1 in 8 chance of the same,
  with no new persona key.
- Four together actions on the `jog_together` path.
- The chat send body takes an optional `user_gender`; the image composer is
  told it when the user is in the picture.

**Non-goals.**

- A tone on any turn the `emotional` draw skips today: group turns, `/open`
  greetings, proactive and event messages, edit turns, image-edit turns,
  voice turns, action turns.
- A change to which kinds speak emotionally when they start, or to the
  listed actions answered in the `emotional` tone.
- `user_gender` on any route other than the two chat send routes.
- A user gender beyond `male` and `female`.

**Anti-goals.**

- `user_gender` reaching the judge, the chat prompt, romance compatibility
  or any gate. It is read by the image composer and by nothing else.
- `user_gender` stored in a column of its own.
- `focused` replacing a turn that drew `emotional`.
- A refusal that costs the relationship for an action her world lacks.

## 3. Approaches

Four points had real alternatives; the owner picked.

**What makes a `focused` reply.** `emotional` is made by absence
alone. For `focused` the owner weighed (a) the default prompt plus one
work line, (b) the default prompt with the relationship conclusions taken
out and no line, (c) both, and picked a mix: the work line, and two of the
conclusion blocks taken out (§5.4). She still knows who the user is to her
and how she may act with them; what she feels this minute is withheld.

**Which events are work.** (a) `temp_work`, and a `business_trip` with no
incident under way; (b) `temp_work` alone; (c) those plus
`scolded_by_superior`, `scolded_superior` and `want_to_quit_job`, whose own
messages are already `emotional`. The owner picked (a).

**Who is `focused` off work.** A career-minded persona needs a reading on
the card. A new boolean key, as `moody` is for `emotional`, was set aside:
the owner reuses `employed`, reading `full_time` as career-minded, so the
card gains no field. Against the 1 in 8 the owner weighed three places for
it: off work as well as at work (picked), the card alone with the events
dropped, and the at-work probability by employment.

**Which further kinds are modern-only.** Beyond the three the owner named,
the candidates were `want_to_quit_job`, `admiring_art`, `working_out` and
`away_from_phone`. The owner took the first two.

Everything else has one sensible way: the four actions reuse the
`jog_together` path and the `fight_together` world check; the tone reuses
the `emotional` draw's turn rules, config table and record; the gender
rides the chat request as `user_timezone` and its siblings do and reaches
the composer as one more labeled section.

## 4. Modern-only kinds

`EventKind::fits` gains five `modern`-only kinds:

| kind | label |
|---|---|
| `saw_bag` | 看中了一个包 |
| `saw_clothes` | 看中了一件衣服 |
| `out_shopping` | 在逛街 |
| `want_to_quit_job` | 想辞职 |
| `admiring_art` | 在看展 |

As for every kind that does not fit: dropped before any weighting, whatever
the request's `kinds` allows; `prefer_events` naming one has no effect.
Stored rows of these kinds stay as they are. A `modern` persona (the
default) draws exactly as today.

## 5. The `focused` tone

### 5.1 At work

She is at work when either holds for the events under way at the turn (the
same reading the turn's `[now]` lines come from):

- the foreground incident is `temp_work`;
- the background is `business_trip` and no foreground incident is under
  way.

Both kinds are `modern`-only and neither is offline, so only a modern
persona is ever at work, and never on a turn she does not answer.

### 5.2 Career-minded

`art_metadata.employed` already says how much of her day is work and
scales the work kinds. It gains a second reading: `full_time` is a
career-minded persona. `part_time` (the default when the key is absent or
unknown) and `free` are not. No key is added, and the event draw reads
`employed` exactly as today.

A career-minded persona may be `focused` off work, in either world, when no
foreground incident is under way: an incident says what she is doing, and
it is not work. Any background allows it, a holiday trip included. *(Engine's
call: the no-incident condition.)*

### 5.3 The draw

`tones` on `[tasks.chat_companion]` gains `focused`, a number in
`[0.0, 1.0]` validated as `emotional` is; absent or `0` switches the tone
off altogether.

```toml
[tasks.chat_companion]
tones = { emotional = 0.05, focused = 1.0 }
```

The probability that a turn is `focused`:

| the turn | p |
|---|---|
| she is at work (§5.1) | `tones.focused` |
| off work, `employed = full_time`, no foreground incident | 1/8 |
| any other | no roll |

`1.0` makes every eligible turn at work `focused`. The 1/8 is a constant of
the engine: the key switches it on, and does not scale it. *(Engine's call:
unscaled.)* The probability is a pure function of the two kinds under way,
`employed` and the key, in `eros-engine-core` beside the `emotional` one.

The turns that may draw are the ones the `emotional` draw takes (its spec
§3.1): the 1:1 user-message text turn, a text-bearing plan, no user action.
On such a turn:

1. With `tones.emotional` above 0, the `emotional` draw runs as today. A
   turn that comes up `emotional` is `emotional`.
2. Otherwise, with `tones.focused` above 0 and a row of the table
   applying, one roll against its p decides `focused` or `default`.

A listed user action is answered in the `emotional` tone as today, which
needs `tones.emotional` above 0; with `tones.focused` alone it draws
nothing and records no tone. Either key works without the other.

**The record.** `metadata.tone` on the turn's assistant rows holds the final
tone (`emotional`, `focused` or `default`) on every turn where at least
one of the two rolls was made, and is absent otherwise. It stays the only
place the tone is stored.

### 5.4 The prompt

The default request and system prompt, with these differences:

| block | `focused` |
|---|---|
| chat history, `[user_profile]`, `[shared_memories]`, `[world_memories]`, `[world_stories]`, `[character_state]` | as default |
| `[relationship]`, `[mood]`, `[inner_state]` | as default |
| `[feelings]` | dropped |
| `[emotional_context]` | dropped |
| `[reply_length]` | fixed at the lowest tier, `最多 1 句，不超过 40 字`, whatever the affinity or the mode |
| `[this_turn]` | the dice are not rolled; the block holds one line, whatever `reply_mode` is |
| `[quote]`, `[now]`, iron rules, rule ① by `reply_mode`, `[output]`, `[reactions]` | as default |

The `[this_turn]` line, under the block's usual header:

```
- 你手上有工作，心思在工作上：回得简短利落，先顾手上的事
```

*(Engine's call: the wording. It names what she does, and nothing she
should not do.)* The line is the same at work and off work.

The judge's audit row records no nudges on a `focused` turn, as on
any turn that did not roll them. Post-processing, the model, the sampling
parameters and the frames are untouched. An image turn's composer reads the
scene as it does today.

How a `focused` reply sounds is judged by reading real replies
(`PROMPT_LOG_DIR`, a test deployment); no test asserts on it.

## 6. Four together actions

| action `type` | marker | places | label | category | lasts | fits |
|---|---|---|---|---|---|---|
| `workout_together` | 想和你一起健身 | `working_out_together` | 和对方一起健身 | `short` | 1 h | both |
| `swim_together` | 想和你一起游泳 | `swimming_together` | 和对方一起游泳 | `short` | 1 h | both |
| `cycle_together` | 想和你一起骑单车 | `cycling_together` | 和对方一起骑单车 | `short` | 1 h | `modern` only |
| `hike_together` | 想和你一起爬山 | `hiking_together` | 和对方一起爬山 | `mid` | 2–3 h | both |

Each behaves as `jog_together` does: no payload; an accept places the kind
in the write that stores the answer (no event inside a date) and ends the
incident under way; the accepted turn is a picture turn; a refusal drops
chemistry; no romance gate, no species gate; not answered emotionally. The
four kinds are `SPECIAL`: weight 0, no cooldown, `Neutral` place, never
told, not offline, and they reach the story director as the other together
kinds do.

**A fantasy persona's `cycle_together`** is ruled out before the judge as a
modern persona's `fight_together` is: refused, places nothing, shows no
picture, drops no line.

*(Engine's calls: every value in the table, and the world check on
`cycle_together`.)*

## 7. The user's gender in a picture

### 7.1 The field

`user_gender` on the chat send body, top-level beside `user_timezone`,
`user_country` and `user_region`; the streaming and the async send routes
both take it. Optional.
`"male"` and `"female"` are read; any other value, a non-string included,
reads as absent, and never fails the request.

It is handed to the image composer only (§7.2).

**Where it is kept.** Both send routes snapshot the request's optional tail
into the turn's queue row (`chat_turn_queue.params`), so that a turn run
from the queue, or revived after a deploy, is the turn that was asked for.
`user_gender` joins that snapshot as `user_timezone`, `user_country` and
`user_region` do, and stays there for the life of the row, which goes when
its session or its message does. *(Owner's ruling.)* It is not copied onto
the user row's metadata, and no column is added. The only other place it
lands is the composer's input snapshot (§7.3).

### 7.2 The composer

The composer's payload gains a section right after `[此刻状态]`:

```
[对方]
男性
```

(`女性` for `female`.) It is present when both hold:

- the turn carries a readable `user_gender`;
- one of the kinds named in `[此刻状态]` is a kind she shares with the
  user: `traveling_together`, `on_a_date`, and every `…_together` kind,
  the four of §6 included. The kind an accepted together action places in
  this turn counts, as it already does for `[此刻状态]`.

Otherwise the section is absent and the payload is byte-identical to
today's.

The section says what the user is, not that the user is in the frame.
Whether they appear stays the composer's reading of `[此刻状态]`, the scene
and the latest message, as today: a selfie she takes on a trip together, or
a `photo` during a walk together, carries the section and may still show
her alone. *(Engine's call.)*

The built-in composer prompt names the section: when it is present and the
partner appears in the picture, the partner is drawn as that gender. The
comment block in `examples/model_config.toml` (and its `.d` copy) lists
it. An operator with a custom composer prompt should add a line for it;
without one the model still sees the section.

The paths whose request has no `user_gender` (an event message's picture, a
proactive message's, a group turn's) send no such section and draw as
today. The standalone compose endpoint and the image-edit endpoint are
unchanged.

### 7.3 Audit

The composer's input snapshot gains `partner_gender`: the section's body
as sent (`男性` / `女性`), or the empty string when the section was absent.

## 8. Tests

- `fits`: the five kinds are `modern`-only; a fantasy persona never draws
  one.
- At work: the two cases of §5.1 and their near misses (`business_trip`
  with an incident under way, `temp_work` under any background, other work
  kinds).
- The probability: the table of §5.3. At work it is the key's value for any
  `employed`; off work it is 1/8 for `full_time` with no foreground
  incident, in either world, whatever the key's value above 0; no roll for
  `part_time`, `free` or an absent key off work, nor for `full_time` with
  an incident under way.
- The draw: `emotional` wins over `focused`; `focused` alone; off when the
  key is absent; an action turn never draws it; the record holds the final
  tone.
- The `focused` prompt: no `[feelings]`, no `[emotional_context]`, the
  lowest `[reply_length]` at any affinity and mode, the one `[this_turn]`
  line in every `reply_mode`, and `[relationship]`, `[mood]`,
  `[inner_state]`, `[user_profile]` and history present. With the key
  absent the default prompt and request are byte-identical to before.
- Config: `tones.focused` range refusal; absent means off; the shipped
  example passes with the key commented out.
- The four actions: wire names, markers, labels, placed kinds and
  durations; a fantasy persona's `cycle_together` is refused before the
  judge and drops nothing.
- The composer: the section appears with a gender and a shared kind, is
  absent with either missing, and an unknown `user_gender` reads as absent;
  the snapshot carries `partner_gender`.

## 9. Order of work

One pull request at a time into `dev`: the code, then the docs. The code
may land whole or split along §4 + §6, §5 and §7, one open at a time.

The docs: the her-world table, the together-actions table, the tone
section and the `employed` row of the persona-settings table in
`docs/life-events-needs.md` (+ `.zh`); the `employed` key in
`docs/eros-character-v1.1.md`; the action list, the
`user_gender` field and `metadata.tone` in `docs/api-reference.md`
(+ `.zh`); `tones.focused` in `docs/model-config.md` (+ `.zh`); a
migrating note under `v2.8.2` for the five kinds a fantasy
persona stops drawing and for the Rust surface (`EventKind`, `UserAction`,
`EventKind::SPECIAL`'s length, `Tone`, `Tones`).

A web-side issue lists the four action types, the four kinds,
`user_gender`, `tones.focused`, what `employed = full_time` now also
means, and the line a custom composer prompt should add.

## 10. Decisions

Ruled by the owner:

- The five modern-only kinds.
- `focused` exists opposite `emotional`; a turn that is `emotional`
  stays `emotional`.
- At work is `temp_work`, or a `business_trip` with no incident under way.
- `employed = full_time` reads as career-minded, with no new key; such a
  persona is `focused` 1 time in 8 off work, and at work as anyone is.
- The `focused` prompt: one work line; `[feelings]` and
  `[emotional_context]` dropped; `[relationship]`, `[mood]`,
  `[inner_state]`, history and profile kept.
- The four together actions.
- The user's gender is passed on the chat request, shapes the picture only,
  and takes no part in romance compatibility.
- `user_gender` stays in the queue row's params, as the other `user_*`
  fields do.
- The tone's name is `focused`.

Calls made in this document, open to correction:

- `tones.focused` is the at-work probability and the tone's switch, off
  when absent; the off-work 1/8 is a constant it does not scale.
- Off work, a career-minded persona draws only with no foreground incident
  under way, under any background and in either world.
- `[reply_length]` is fixed at the lowest tier on a `focused` turn.
- The wording of the work line.
- `metadata.tone` records the final tone whenever either roll was made.
- Action names, kind names, markers, labels, categories and durations of
  §6; `hike_together` is `mid` and lasts 2–3 hours; `cycle_together` is
  `modern`-only and its refusal for a fantasy persona drops nothing.
- `user_gender` is top-level, lenient on unknown values, and limited to the
  two chat send routes.
- The section is `[对方]`, shown only when a shared kind is in
  `[此刻状态]`; it gives the user's gender and does not assert that the
  user is in the frame.
- `partner_gender` in the composer's input snapshot.

## 11. Left to the builder

- Rust names, and where the at-work check and the shared-kind check live.
- How `render_prompt` takes the tone; the default path's output must not
  move.
- How the world check generalises from one action to two.
- How `user_gender` travels from the request and the queued params to the
  composer call sites of the chat turn.
