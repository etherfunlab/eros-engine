# The `erotic` tone — Design

- **Date:** 2026-11-27
- **Status:** Approved
- **Type:** Engine change. No migration. No HTTP or SSE change. One new
  `model_config.toml` key (`tones.erotic`), off when absent. One new
  `metadata.tone` value. `art_metadata.sex_addict` and
  `art_metadata.is_virgin` gain a second reading; the card gains no key.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-11-12-chat-tone-emotional-design.md` (the tone draw,
  the `emotional` template, the record),
  `2026-11-12-emotional-action-replies-design.md` and
  `2026-11-12-emotional-event-messages-design.md` (fixed per-action and
  per-kind tones), `2026-11-24-focused-tone-four-together-actions-and-user-gender-design.md`
  §5 (`focused`: a key that is both a probability and a switch, a card key
  read a second way, a constant 1 in 8),
  `2026-11-27-passion-group-design.md` (`sex_together`),
  `2026-10-30-give-needs-romantic-choices-and-answer-receipts-design.md`
  (her asks and her reply to the answer).

## 1. Intent

A third non-default tone, `erotic`: the alluring version of `emotional`.
It is `emotional` and more. Everything the `emotional` template withholds
stays withheld; two things are added, who the user is to her and one line
saying she wants them.

It is used at three fixed moments (she accepts sex, the user accepts her
ask to go to bed or to bathe, she says she suddenly wants the user) and,
for a persona with a strong desire, now and then in everyday chat, as a
career-minded persona is now and then `focused` off work.

## 2. Goals, non-goals, anti-goals

**Goals.**

- The three fixed moments of §4 are written in the `erotic` tone.
- A `sex_addict = true` persona's everyday 1:1 text turn has a 1 in 8
  chance of it, from chemistry tier 3 (§5).
- A persona whose card says `is_virgin = true` is never `erotic` below
  chemistry tier 4, at a fixed moment or in everyday chat (§4.1).
- When several tones could apply, `erotic` wins over `emotional`, and
  `emotional` over `focused` (§6).
- Every rolled message leaves a reading on its row (§8).
- A deployment that does not set `tones.erotic` sees no change at all.

**Non-goals.**

- A tone on a turn that has none today other than the moments of §4:
  group turns, `/open` greetings, edit turns, image-edit turns, voice
  turns, any other proactive or event message.
- `erotic` for the whole time an event lasts. Only the one message of each
  fixed moment is `erotic`; the turns that follow draw as any turn does.
- `erotic` on a refusal: a refused `sex_together`, the user declining her
  ask.
- A change to the judge, to post-processing, to the model, the sampling
  parameters, the output filter, the frames or the image composer.
- A new persona key.

**Anti-goals.**

- Any explicit word in the repository. The three lines of §7.2 are the
  whole wording, and they are suggestive, not explicit.
- The tone recorded in two places, or handed to the judge.
- An `erotic` reply between a pair that is not romantically compatible, or
  during a cold war.

## 3. Approaches

**What makes an `erotic` reply.** Three ways were weighed, all on the
`emotional` template:

1. the template plus one line in `[this_turn]`, as `focused` has its work
   line. Steady and smallest; every tier hears the same line.
2. the same, plus the relationship conclusion handed back, so that what
   she is told scales with how far the pair has come.
3. no line, only a present-state entry in `[character_state]`. Closest to
   the least-hint idea, weakest steer.

The owner picked 2. The text prompt has no chemistry-only sentence (the
voice prompt does); what it has is the `[relationship]` block, whose
composed relation moves with the chemistry tier. That block is what comes
back. *(Engine's call: the whole block, §7.1.)*

**How far a fixed moment reaches.** For `sex_together` and for
`sudden_desire` the owner weighed the one message against every turn while
the event lasts, and picked the one message: an `erotic` turn carries no
chat history, and an hour of replies written without it costs more than it
gives.

**Which of her ask's three messages.** The ask itself, her reply to an
accept, her reply to a decline: the owner picked the reply to an accept
alone.

**The line's wording.** Three candidates were offered; the owner keeps all
three, one for each of chemistry tiers 3, 4 and 5.

Everything else has one sensible way: the draw, the config table and the
record follow `emotional` and `focused`.

## 4. The fixed moments

| moment | the message | today |
|---|---|---|
| the user's `sex_together`, accepted | her reply, on the 1:1 user-message turn, when the plan bears text | `emotional` when `tones.emotional` > 0 |
| her `sleep_together` or `bathe_together` ask, accepted by the user | her reply to the answer, on the JSON and the streamed route | no tone |
| `sudden_desire` | the message the event sends when it starts, on the JSON and the streamed route | `emotional` when `tones.emotional` > 0 |

`fall_asleep_together` is not `sleep_together` and is not a fixed moment.
No other action, ask or kind is one: the other passion- or naturism-group
actions, `strip`, `just_have_sex` and the rest keep the tone they have
today.

At each moment, when the common conditions of §4.1 hold, one roll against
`tones.erotic` decides. Won: the message is `erotic`. Lost, or a condition
failing: the message is what it is today, to the byte.

`1.0` makes every qualifying fixed moment `erotic`.

### 4.1 Common conditions

They gate every `erotic` message, fixed or drawn:

- `tones.erotic` is above 0;
- the pair is romantically compatible
  (`scope::romance_compatible(affinity_scope)` for the message). A
  `sex_together` let through by `ignore_orientation` between an
  incompatible pair is therefore answered as today;
- no cold war is on;
- when the card's `is_virgin` is `true`, the pair's chemistry tier, as
  this message's prompt reads it, is at least 4.

**The virgin's floor.** A virgin persona's `erotic` floor is chemistry
tier 4, at the fixed moments and in the everyday draw alike. Below it she
answers the fixed moments as today: `emotional` for `sex_together` and
`sudden_desire`, no tone for her reply to an accepted ask. At tier 4 she
starts straight from the tier-4 line and never reads the tier-3 one
(§7.2): flustered before the line, wanting after it, with nothing in
between. *(Owner's ruling.)*

**The floor reads the card alone.** `is_virgin = true` holds the floor at
tier 4 whether or not she has since had sex: having slept with the user
does not change who she is, and the contrast stays. A virgin persona at
chemistry tier 3 who has already had sex is still below her floor.
*(Owner's ruling.)*

This differs on purpose from how the life-event draw reads the same key.
There her virginity ends with the first `having_sex`, `just_have_sex` or
`having_sex_together` row, and the sex kinds stop being down-weighted:

- the event draw reads it as a **fact**: once it has happened, a first
  time is no longer unlikely;
- the tone reads it as **character**: once it has happened, she is still
  the same person.

The two readings are not to be unified.

For any other persona a fixed moment has no chemistry floor of its own:
the action's gate, the ask's rule and the kind's rung already stand in
front of it.

## 5. The everyday draw

A persona whose card says `sex_addict = true` has a strong desire. Beyond
what the key already does to her life events, her everyday turns may come
up `erotic`.

A turn rolls when all of these hold:

- it is a turn the `emotional` draw takes (its spec §3.1): the 1:1
  user-message text turn, a text-bearing plan, no user action;
- the common conditions of §4.1;
- `art_metadata.sex_addict` is `true` (`false` and absent never roll);
- the pair's chemistry tier, as this turn's prompt reads it, is at least
  3 (a virgin persona's floor of 4 is already a common condition);
- no foreground incident is under way, or the one under way is one of
  `sudden_desire`, `strip`, `streaking`, `just_have_sex`, `masturbating`,
  or a kind of the naturism or the passion together group
  (`EventKind::together_group` is `Nude` or `Sex`). Any other incident,
  solo `showering` and `bathing` included, makes no roll.

The chance is 1/8, a constant of the engine: `tones.erotic` switches it on
and does not scale it, as `tones.focused` does not scale the career-minded
1/8. A virgin persona rolls at the same 1/8; only her floor is a tier
higher.

The probability is a pure function of those inputs, in `eros-engine-core`
beside the other two.

*(Owner's rulings: the card key, the 1/8 by analogy with `focused`, tier 3,
a tier higher for a virgin at the same chance, `is_virgin` read from the
card alone. Engine's calls: the compatibility and cold-war conditions;
which foreground incidents
allow a roll — an incident says what she is doing, and a quarrel or a
fever is not this.)*

## 6. Order

On a 1:1 user-message turn whose plan bears text:

1. **A user action.** An accepted `sex_together` rolls as §4 says; won, the
   turn is `erotic`. Otherwise the action is answered as today: a listed
   action in the `emotional` tone under `tones.emotional`, any other action
   with no tone.
2. **No user action.** The everyday `erotic` roll first (§5), when the turn
   qualifies. Then the `emotional` draw, as today. Then the `focused` roll,
   as today. The first to win names the turn; none winning, it is
   `default`.

So `erotic` > `emotional` > `focused`. A turn that came up `erotic` makes no
later roll. Each key works without the others.

## 7. The prompt

An `erotic` message is built as that moment's `emotional` message is: the
chat turn's request is the least-hint system prompt and the driving user
message, with `[user_action]` appended on an action turn; an event
message's is the system prompt and the cue. Her reply to an accepted ask
has no `emotional` form today; its `erotic` request is the system prompt
and the ask's cue as the one user message, as an event message's is. The
cues are unchanged. The `sleep_together` cue does not name what was agreed
to; the event the accept has just placed is under way and its `[now]`
line says it.

The system prompt is the `emotional` one (its spec §4.2) with two
differences.

### 7.1 `[relationship]` comes back

The block renders as the default prompt renders it, in its usual place:
the status line, the composed relation of the pair's two tiers, from the
affinity and the scopes of this message.

`[mood]`, `[feelings]`, `[inner_state]`, `[emotional_context]`,
`[user_profile]`, the chat history, the anniversary lines and
`[reactions]` stay out. `[reply_length]` stays at the lowest tier, `最多 1
句，不超过 40 字`.

### 7.2 One line in `[this_turn]`

The nudge dice are not rolled. The block is present, under its usual
header, holding one line, set by the pair's chemistry tier as this
message's prompt reads it:

| chemistry tier | the line |
|---|---|
| 3 and below | `- 你此刻对对方有欲望：话里带着撩拨和暗示，慢一点，贴近一点` |
| 4 | `- 你现在很想要对方：语气放软、放慢，带着勾人的意味` |
| 5 | `- 你的心思和身体都在对方身上：说话带着诱惑，不必遮掩` |

Nothing is drawn. A drawn turn is at tier 3 or above by its floor; a fixed
moment of a persona who is not a virgin has no floor (§4.1) and can fall
below tier 3, where it reads the tier-3 line. A virgin persona is at tier
4 or above on every `erotic` message, so she reads the second or the third
line only. The line for a tier is the same at a fixed moment and on a
drawn turn, whatever `reply_mode` is.

*(Owner's rulings: the three lines, one per tier, not random. Engine's
calls: which line goes with which tier, by how far each goes; tiers 1 and
2 read the tier-3 line.)*

### 7.3 Everything that holds for `emotional` holds

`erotic` is a superset of `emotional`. Wherever the engine treats an
`emotional` message specially, it treats an `erotic` one the same way:
three latest episodes and no similarity search, no embedding call, world
and stories from their digests, no nudges on the judge's audit row,
post-processing and the image request untouched.

How an `erotic` reply sounds is judged by reading real replies
(`PROMPT_LOG_DIR`, a test deployment); no test asserts on it.

## 8. Configuration and the record

```toml
[tasks.chat_companion]
tones = { emotional = 0.05, focused = 1.0, erotic = 1.0 }
```

`erotic` is a number in `[0.0, 1.0]`, validated as the other two are;
absent or `0` switches the tone off altogether. The shipped examples carry
it commented out. Turning it off is removing the key and restarting.

`metadata.tone` holds the final tone (`erotic`, `emotional`, `focused` or
`default`) on the assistant rows of every message for which at least one
roll was made, and is absent otherwise. A lost roll on her reply to an
accepted ask therefore writes `default`. A fixed moment where a common
condition fails makes no `erotic` roll and records what it records today.
It stays the only place the tone is stored.

## 9. What changes for a live user

Nothing until the deployment sets `tones.erotic`. With it set: the three
fixed moments read alluring instead of `emotional` or plain; a
strong-desire persona at chemistry tier 3 or more answers that way about
one everyday turn in eight, and on those turns she does not have the chat
history in front of her.

## 10. Tests

- The everyday probability: 1/8 for `sex_addict = true` at tier 3 and
  above; no roll at tier 2; a virgin persona, no roll at tier 3 and 1/8 at
  tier 4; no roll for `false` or absent; no roll for an incompatible pair,
  during a cold war, or under a foreground incident outside the list of
  §5 (`quarrel`, solo `showering`); a roll under each listed kind; the
  key's value above
  0 does not scale it; the key at 0 means no roll.
- Order: a won `erotic` roll makes no `emotional` or `focused` roll; a lost
  one leaves both as today.
- The fixed moments: each is `erotic` at `1.0`; a refused `sex_together`,
  a declined ask, `fall_asleep_together` and an `ignore_orientation` turn
  between an incompatible pair are not; with the key absent every one of
  them is byte-identical to before.
- The virgin's floor: at tier 3 each fixed moment of a virgin persona is
  what it is today (`emotional`, `emotional`, no tone) and makes no
  `erotic` roll; at tier 4 each is `erotic` with the tier-4 line. A
  persona who is not a virgin is `erotic` at a fixed moment at tier 2,
  with the tier-3 line. A virgin persona at tier 3 with a
  `having_sex_together` row in her past is still below her floor.
- The prompt: `[relationship]` present; `[mood]`, `[feelings]`,
  `[inner_state]`, `[user_profile]` and history absent; the lowest
  `[reply_length]`; `[this_turn]` holding exactly the line of the pair's
  chemistry tier (tiers 1, 2 and 3 the first, 4 the second, 5 the third);
  `[user_action]` kept on the action turn.
- The record: `erotic` on a won roll, the site's own tone or `default` on a
  lost one, no key where no roll was made.
- Config: range refusal; absent means off; the shipped example passes.

## 11. Order of work

Starts after v2.8.4 is released. One pull request at a time into `dev`:
the code, then the docs.

The docs: the tone sections of `docs/model-config.md` (+ `.zh`) and
`docs/life-events-needs.md` (+ `.zh`), with the `sex_addict` and
`is_virgin` rows of the persona-settings table;
`docs/eros-character-v1.1.md`; `metadata.tone` in `docs/api-reference.md`
(+ `.zh`); a migrating note for the Rust surface (`Tone`, `Tones`). No
migration; rolling back is removing the key or redeploying the previous
image.

The tone is documented in the docs and the migrating guide only; GitHub
release notes do not list it. *(Owner's ruling.)*

A web-side issue lists the `tones.erotic` key, the new `metadata.tone`
value and what `sex_addict = true` and `is_virgin = true` now also mean.

## 12. Decisions

Ruled by the owner:

- The tone exists, is named `erotic`, and is the alluring version and a
  superset of `emotional`.
- The fixed moments: an accepted `sex_together`; her reply to an accepted
  `sleep_together` or `bathe_together` ask; the `sudden_desire` message.
  One message each, not the event's whole span.
- A refused `sex_together` stays `emotional`; her ask itself and her reply
  to a decline are not `erotic`.
- `sex_addict = true` draws it in everyday chat, as `full_time` draws
  `focused`; chemistry tier 3, the same chance for a virgin.
- A virgin persona's floor is chemistry tier 4 everywhere, the fixed
  moments included; below it they are answered as today.
- The floor reads the card's `is_virgin` alone, whatever has happened
  since: having had sex does not change her character, and the contrast
  stays. The life-event draw reads the key as a fact that ends with her
  first time; the tone reads it as character. The difference is
  deliberate (§4.1).
- `erotic` > `emotional` > `focused`.
- The reply is the `emotional` template plus one line plus the
  relationship conclusion.
- The three lines of §7.2, one for each of chemistry tiers 3, 4 and 5.
- Docs and migrating guide only; not in the release notes.

Calls made in this document, open to correction:

- "The relationship conclusion" is the whole `[relationship]` block.
- `tones.erotic` is the fixed moments' probability and the tone's switch;
  the everyday 1/8 is a constant it does not scale.
- The common conditions: a romantically compatible pair and no cold war,
  at the fixed moments too.
- A fixed moment of a persona who is not a virgin has no chemistry floor
  of its own.
- The everyday roll is made with no foreground incident, or under the
  kinds listed in §5.
- The life-choice cues are unchanged; the `sleep_together` reply leans on
  the event's `[now]` line.
- A lost roll at a fixed moment falls back to what the moment is today;
  on her reply to an accepted ask it records `default`.
- `[reply_length]` stays at the lowest tier.
- Which line goes with which tier; a fixed moment below tier 3 reads the
  tier-3 line.

## 13. Left to the builder

- Rust names; how `render_prompt` and the `emotional` request builders
  take the tone, the affinity and the line. The default, `emotional` and
  `focused` outputs must not move.
- Where the three moments decide the tone: the chat turn's draw site, the
  proactive message's preparation, the life-choice reply.
- How the life-choice reply path learns that the answered ask was an
  accepted `sleep_together` or `bathe_together`.
- Where the helper that says "this tone is built on the `emotional`
  template" lives.
