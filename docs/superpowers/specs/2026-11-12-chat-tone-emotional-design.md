# Chat tone variants and the `emotional` tone — Design

- **Date:** 2026-11-12
- **Status:** Approved
- **Type:** Engine change. No migration. No HTTP or SSE change. One new
  `model_config.toml` key on `[tasks.chat_companion]`. The character card
  1.1 gains the `moody` (boolean) key. Assistant rows gain
  `metadata.tone`. Off unless a deployment sets the key: with it absent,
  every prompt and every row is what it is today.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-10-13-persona-needs-design.md` and
  `2026-10-28-outing-indulgence-needs-and-habits-design.md` (needs,
  habits), `2026-10-28-memory-2-dreaming-plus-design.md`
  (`[shared_memories]`), the reply-mode and nudge dice of the 2026-09-13
  design (`reply_mode`, `TurnNudges`).

## 1. Intent

A companion's reply is written today from one system prompt whose
*context* the judge picks per turn (`reply_mode`: listen, follow, ask,
judge, or none). Every reply is also a considered one: it is written with
the whole history window, the user's profile and the recalled memories in
front of the model.

People are not always considered. This change adds a second axis, the
turn's **tone**, and its first non-default value, `emotional`: on such a
turn she answers from who she is and what the user just said, with almost
nothing else in front of her.

Two ideas carry the design, both continuing the standing prompt rules
(the engine decides what a model cannot perceive; a behaviour is removed
by absence, never by a sentence naming it):

- **What the judge cannot judge goes to a deterministic mechanism.** The
  judge keeps choosing the context. It does not choose the tone: the
  engine draws it, from a deployment-set base probability moved by the
  persona's needs, a new persona key, and whether the pair is
  romantically compatible.
- **Take the hints away and let her be herself.** The `emotional`
  template is a least-hint template. It is defined by what it leaves out,
  not by an instruction to be emotional: persona, the user's line, three
  recent memories, her own present state, and little else. Everything the
  engine normally concludes for her about the user and the relationship
  is withheld on that turn.

## 2. Goals, non-goals, anti-goals

Goals:

- One draw per eligible turn decides `default` or `emotional`.
- An `emotional` turn's request carries no chat history, no user profile,
  no affinity conclusion, and only the three latest episodes as memory.
- Every drawn turn leaves a reading on its assistant row.
- A deployment that does not set the key sees no change at all.

Non-goals:

- No second tone beyond `emotional`.
- No per-tier probability.
- No change to the judge, its prompt, its schema or its audit row.
- No change to post-processing: memory, insights and affinity evaluation
  run on an `emotional` turn as on any other, and the reply enters history
  like any other.
- No change to the model, the sampling parameters, the output filter or
  the frames sent to the client.
- Group turns, `/open` greetings, proactive and event messages, edit
  turns, image-edit turns and voice turns never draw.

Anti-goals:

- The judge picking the tone, or the tone being handed to the judge.
- The tone recorded in two places.
- A tone the deployment cannot switch off with one key.

## 3. The draw

### 3.1 Which turns draw

A turn draws when all of these hold:

- it is the 1:1 user-message text turn (the turn `run_stream` serves, the
  one that renders `[reactions]`);
- the finalised plan's action bears text (`ActionType::bears_text`:
  `ReplyText` or `ReplyTextImage`);
- the user message carries no action (the same exclusion the nudge dice
  use);
- the base probability (§5) is above 0.

Any other turn is the default tone and records nothing.

The draw happens once, at plan finalisation, after the turn's needs are
settled and ahead of the nudge roll. It does not read `reply_mode`, and
`reply_mode` is decided exactly as today whatever it returns. An
`emotional` turn rolls no nudges (§4.2); the judge's audit row then
records none, as on any turn that did not roll.

### 3.2 The probability

```
p = min(1, base × moody × needs × compatible)
```

| factor | value |
|---|---|
| `base` | `[tasks.chat_companion].tones.emotional` (§5) |
| `moody` | persona `art_metadata.moody`: `true` ×3, `false` ×⅓, absent ×1 |
| `needs` | ×3 when at least one of the four holds, else ×1; several holding is still ×3 |
| `compatible` | ×2 when `scope::romance_compatible(affinity_scope)` for this turn, else ×1 |

The four needs, read from the needs this turn's reply is written with
(the judge's rating for this turn, else the carried prior):

| need | level | the reply's words for it |
|---|---|---|
| `emotional` | `high` | 很想被在乎 |
| `stamina` | `low` | 很累 |
| `hunger` | `high` | 饿了 |
| `indulgence` | `high` | 很想抽根烟 / 喝一杯 / … |

A turn with no needs at all (no rating yet) takes ×1.

With `base = 0.05`:

| persona and moment | p |
|---|---|
| not compatible, no need, `moody` absent | 0.05 |
| compatible | 0.10 |
| compatible, a need holds | 0.30 |
| compatible, a need holds, `moody = true` | 0.90 |
| compatible, a need holds, `moody = false` | 0.10 |

`hunger` settles at `high` with time, and so does `indulgence` for a
persona with a habit, so the needs factor usually holds on the first turns
after a gap, until the judge rates her otherwise.

The probability is a pure function of those four inputs and lives in
`eros-engine-core`.

### 3.3 `moody`

A new persona key, `art_metadata.moody`: `true`, `false` or absent; a
value that is not a boolean reads as absent. It is read by the tone draw
and by nothing else. It reaches no prompt, and it is not a habit: it does
not move `indulgence` and has no give item.

## 4. The `emotional` prompt

The default prompt and its request are untouched. An `emotional` turn
builds this instead.

### 4.1 Messages

The system prompt, then the user message that drives the turn, in its
model-facing form (an image turn keeps its image preamble). No earlier
row of the session is sent.

### 4.2 System prompt

Same block order as the default prompt, so the stable prefix (authored
head, guards, identity, persona blocks) stays shared.

| block | `emotional` |
|---|---|
| authored head, `PERSONA_GUARD`, `ANTI_REFUSAL_GUARD`, identity line | kept |
| `[backstory]`, `[speech_style]`, `[quirks]`, `[topics]` | kept |
| `[additional_guidance]` (prompt traits) | kept |
| `[user_profile]` | **dropped, header included**; the "just met" placeholder is not rendered either |
| `[shared_memories]` | the three latest episodes (§4.3), nothing else |
| `[world_memories]`, `[world_stories]` | kept, digest only (§4.4) |
| `[character_state]` (insight lines, items, wardrobe, outfit, lingering effects, needs) | kept |
| `[reply_length]` | **fixed** at the lowest tier, `最多 1 句，不超过 40 字`, whatever the affinity or the mode |
| the relationship line, `[mood]` directives, `[feelings]` | dropped |
| `[inner_state]` (the judge's hints) | dropped |
| `[emotional_context]` | dropped |
| `[quote]` | kept |
| `[now]`: time, holiday, birthday, events under way and just now | kept |
| `[now]`: anniversary lines | dropped (they come from the journal) |
| `[this_turn]` (nudges) | dropped; the dice are not rolled |
| iron rules, rule ① by `reply_mode`, `[output]` | kept |
| `[reactions]` (appended after the prompt) | dropped |

No block is added. There is no directive telling her to be emotional: the
tone is what the model does with the persona once the conclusions are
gone.

With the affinity conclusions dropped, a lover and a stranger are told
the same thing on this turn; only the persona and the three memories set
them apart. A pair the affinity has judged cold may get a warm reply.
That is the intended effect.

### 4.3 The three memories

The three latest episodes of this persona instance: the same rows, read
the same way, as the episodes the default prompt already shows
(`EpisodeRepo::recent`, newest three by `ended_at`). They render under
`[shared_memories]` as the default prompt renders an episode, one
`- {date}：{summary}` line each, oldest first; none ⇒ the block's existing
placeholder.

- The relationship arc, the journal and the raw relationship rows are not
  loaded. A raw row is the user's own line from an earlier turn, which is
  chat history by another road.
- No embedding call is made on an `emotional` turn.
- The turn's `memory_scope` still rules: with the relationship layer off,
  no episode is read.
- The count is 3 whatever the affinity tier.
- A relationship the scan has not reached yet has no episode, and the turn
  runs on the persona and the user's line alone.

### 4.4 World and stories

`[world_memories]` and `[world_stories]` render from their digests alone.
Their recalled fragments and episodes ride on the turn's query embedding,
and an `emotional` turn has none; this is the degradation both blocks
already have when recall is skipped. The same switches and enrolment
rules decide whether a digest is there at all.

### 4.5 Image turns

A `reply_text_image` turn that draws `emotional` loses history in its
text request only. The image composer reads the scene as it does today.

How an `emotional` reply sounds is judged by reading real replies
(`PROMPT_LOG_DIR`, a test deployment); no test asserts on it.

## 5. Configuration

```toml
[tasks.chat_companion]
tones = { emotional = 0.05 }
```

- A number in `[0.0, 1.0]`. Anything else, or a tone name other than
  `emotional`, fails the boot.
- Task-level only; a tier does not override it.
- Read on `[tasks.chat_companion]` only.
- Absent, or `0`: no turn draws, no row gets `metadata.tone`, and every
  prompt is byte-identical to today's.

The shipped examples (`examples/model_config.toml`,
`examples/model_config.d/chat.toml`) carry the key commented out.

Turning it off is removing the key and restarting.

## 6. The record

Every assistant row of a turn that drew carries the result:

```json
{ "tone": "emotional" }
{ "tone": "default" }
```

- `default` is written on a drawn turn that came up default, so the rows
  with the key are the draw's denominator.
- A turn that did not draw (§3.1) has no `tone` key.
- The key goes in the metadata bag the chat burst already builds for its
  rows (`prompt_traits`, `memory_scope`, `affinity_scope`, `tier`). The
  canned fallback-phrase rows are not required to carry it.
- This is the only place the tone is stored. The judge's decision row and
  the frames do not repeat it; a query joins on the message.

## 7. Tests

- The probability: the factor table of §3.2, the cap at 1, absent needs.
- `moody` reads three states; a non-boolean reads as absent.
- With the key absent, the default prompt and request are byte-identical
  to before.
- The `emotional` request holds exactly the system prompt and the driving
  user message; its prompt has none of the dropped headers, and its
  `[reply_length]` is the lowest tier at any affinity and any mode.
- An `emotional` turn rolls no nudges.
- The memories: three episodes and no arc, journal or raw row; none with
  the relationship layer off; the placeholder with no episode.
- A drawn turn's assistant row carries `tone`; an undrawn turn's does not.
- Config: range and unknown-name refusals; absent means off.

## 8. Documentation

In a separate docs pull request after the code merges:
`docs/model-config.md` (+ `.zh`), the persona-settings table in
`docs/life-events-needs.md` (+ `.zh`), `docs/eros-character-v1.1.md`,
`docs/api-reference.md` (+ `.zh`) where message metadata is described.
The change is additive and off by default, so there is no migrating note.

## 9. Decisions

Ruled by the owner:

- The engine draws the tone; the judge keeps the context.
- One draw: base probability times factors.
- Needs that count: `emotional` high, `stamina` low, `hunger` high,
  `indulgence` high; any one ×3, not stacking.
- `moody`: `true` ×3, `false` ×⅓, absent ×1.
- Romantically compatible ×2.
- The two ideas of §1.
- `emotional` drops history, `[user_profile]`, the affinity conclusions
  (the relationship line, `[mood]`, `[feelings]`), the judge's
  `[inner_state]` and the nudge dice.
- It keeps the character's present state, world and stories, `[quote]`,
  prompt traits, and rule ① as `reply_mode` writes it.
- Its reply length is fixed at the lowest tier.
- Memories: the three latest episodes, no similarity search, no raw
  relationship rows.
- The record is `metadata.tone`, with `default` written on a lost draw.

Calls made in this document, open to correction:

- No directive block tells her to be emotional (§4.2): the template is
  least-hint, so the tone comes from absence alone.
- Which turns draw (§3.1): the 1:1 text turn only, never an action turn.
- The config shape `tones = { emotional = p }`, task-level, no tier.
- `[emotional_context]`, `[reactions]` and anniversary lines are dropped.
- World and stories are digest-only on an `emotional` turn, since no
  embedding call is made.
- The three episodes ignore the affinity gate.
- No `tone` on the judge's audit row or in any frame.

## 10. Left to the builder

- Where the tone type and the draw live in `eros-engine-core`, and how
  `moody` is read.
- How `build_prompt` and `build_reply_request_with` take the tone (a
  parameter, a second function); the default path's output must not move.
- Whether the three episodes go through the existing `[shared_memories]`
  renderer and its character budget.
