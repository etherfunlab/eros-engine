# The wardrobe steward — Design

- **Date:** 2026-11-28
- **Status:** Approved
- **Type:** Engine change. One migration: an index. Config, additive: one task
  (`[tasks.pde_wardrobe]`). Card, additive: one `art_metadata` key
  (`clothing`), character card 1.3. Audit, additive: two keys in
  `chat_images_events.inputs`. No new route, frame, action or event kind.
  With the task unset, nothing changes.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-10-12-persona-items-and-gift-design.md` §5–§6 (the
  judge's `clothing`, the wardrobe, the named outfit),
  `2026-11-08-strip-and-world-setting-design.md` §2.2 and
  `2026-11-28-ask-strip-design.md` §5 (the turn's clothing on an accepted
  strip), `2026-11-27-swim-outfit-ladder-and-fish-together-design.md` §4
  and `2026-11-27-passion-group-design.md` §7 (what the engine writes into
  the picture's clothing), `2026-10-26-event-message-image-design.md` and
  `2026-10-26-group-two-person-image-design.md` (the other two pictures),
  `2026-08-14-image-audit-events-design.md` (the picture's audit row),
  `2026-10-16-byoc-design.md` §2.2 and §8 (what the audit tables keep of a
  card, and that they do not feed generation; §5.4 below changes both).

## 1. Intent

What she wears is today the judge's call: `pde_decision` writes an
optional `clothing` line, and the engine feeds it, in the same turn, to
two places: the reply prompt's `[character_state]` and the image
composer's `[此刻穿着]`. A deployment that wants her dressed by rules of
its own has to put those rules into the judge's prompt, where they
compete with the action, the reply mode, the needs and the action
response; the one answer then steers both the chat and the picture; and
because the judge may write it on every turn, an outfit stays in the
prompt for as long as the user keeps talking.

The wardrobe steward is a task of its own, a sibling of the judge, whose
only job is to say what she is wearing in the picture about to be drawn.
It reads the character's own dressing brief from the card. In the turn
it is called, its answer goes to the image composer and nowhere else.
For the 45 minutes after that picture, later turns' reply prompts carry
the answer as what she is wearing, so she knows what she has on; then it
lapses.

The rule behind it: **a task called in a turn feeds that turn's chat or
that turn's picture, never both**, as the affinity judge's result never
reaches the composer. A later turn reading an earlier turn's conclusion
is not the same thing and is allowed. *(Owner's ruling.)*

## 2. Goals, non-goals, anti-goals

**Goals.**

- A task `[tasks.pde_wardrobe]` that, when set, decides the composer's
  `[此刻穿着]` for every picture the chat pipeline draws: a 1:1 turn's, a
  group turn's, an event message's (§4, §5). *(Owner's ruling.)*
- It is called after the judge, only for a picture that will be drawn,
  and only when nothing else has already dressed her: not when the engine
  writes the clothing (a strip rung, the swimsuit ladder, a towel,
  naturism, sex), and not when the caller named an outfit (§5).
  *(Owner's ruling.)*
- A card key `art_metadata.clothing`, beside `appearance`: the
  character's dressing brief, read by the steward (§7).
  *(Owner's ruling.)*
- The steward's answer reaches the reply prompt of the turns after it,
  in the line the judge's `clothing` used to fill, for 45 minutes from
  the picture (§6). *(Owner's ruling.)*
- With the task unset, today's behaviour exactly: the judge's `clothing`
  dresses her, in the reply and in the picture. *(Owner's ruling.)*
- With the task set, the judge's `clothing` and the group director's are
  no longer used (§8). *(Owner's ruling.)*

**Non-goals.**

- The steward's answer in the reply prompt of the turn that called it.
- A stored "what she is wearing now". The last picture's clothing is read
  back from the picture audit rows (§5.3); nothing new is stored.
- The standalone compose endpoint and the image-edit endpoint. Both keep
  sending what they send today: neither calls the steward, the text reply
  sent with an edited picture carries no outfit from an earlier picture,
  and an edited picture records no clothing, so an edit does not change
  the carried outfit.
- A change to any engine-written clothing, to `[指定穿着]`, to
  `[你的衣柜]` in the judge's context and the reply prompt, or to the
  composer's built-in prompt.
- A built-in steward prompt. The task is on only with a `filter_prompt`
  of the deployment's own, as `pde_decision` is.
- A configurable window. 45 minutes is a constant.

**Anti-goals.**

- A steward call on a turn that draws nothing.
- A steward call whose answer would be overwritten.
- A list of "skip when…" conditions kept beside the list of engine-written
  clothing: the skip follows from one order of precedence (§5.1), so a
  new engine-written clothing needs no second edit.
- Clothing that sticks: an outfit still in the reply prompt after its
  window, or renewed by anything but a new picture.
- A carried outfit that counts as "already dressed" at the picture
  entrance: the steward would then be called once and never again (§6).

## 3. Approaches

1. **At the shared picture entrance** *(taken)*. Every picture the chat
   pipeline draws goes through `build_delegated_image_prompt_for`, where
   `[此刻穿着]` is decided in one line: the engine's override, else the
   plan's clothing. The steward becomes the third source in that line.
   With the steward on, the judge's `clothing` is dropped where the
   verdict becomes a plan, so the plan's clothing is left holding only
   what the engine or the caller wrote; "nothing has dressed her yet" is
   then exactly "override empty and plan clothing empty". On a
   text-and-picture turn the entrance already runs in a task spawned
   before the text reply, so the steward hides under the reply as the
   composer does.
2. In the 1:1 chat turn, as its own call after the verdict, its answer
   passed down as an override. Rejected: the conditions under which it
   must not run (strip, swim, towel, sex, named outfit) would be listed a
   second time in the turn, and group and event pictures would stay on
   the old path.
3. No new call: hand the composer the dressing brief, the wardrobe and
   the last clothing and let it decide. Rejected: the point is a model
   chosen and prompted for this one judgment, and the composer's prompt
   is selected per style variant, so the dressing rules would be repeated
   in every variant.

Calling the steward at the same time as the judge was considered and
dropped: whether a picture is drawn and whether a strip is accepted are
the judge's own conclusions, so the call could neither be skipped
exactly nor saved on a text turn. *(Owner's ruling.)*

For the way back into the chat, the picture's audit row is the carrier
*(taken)*: the answer is already recorded there as the slot the composer
was sent, so later turns read it and nothing is stored twice. A column
or a table for "her current outfit" was rejected as a second copy of the
same fact.

## 4. The task

`[tasks.pde_wardrobe]`, declared and resolved as `pde_decision` is:
`model`, `fallback`, `retry_depth`, `temperature`, `max_tokens`, the
sampling knobs, `filter_prompt`. No tiers. A plain `filter_prompt` only;
the variant shapes stay the composer's.

- **On** when the block is present, its `filter_prompt` is not blank, and
  `[tasks.chat_image_prompt_compose]` is configured. Its answer has no
  reader without the composer: a `pde_wardrobe` block without the
  composer is ignored with a boot WARN, as `chat_product_qa` is without
  the LLM judge.
- **The call.** System message: the `filter_prompt`. User message: the
  payload of §5.2. One attempt per model down the chain, each under the
  same per-attempt limit the composer's attempts run under
  (`FILTER_TIMEOUT`). Each answered attempt is recorded in
  `engine.llm_generations` under the task name `pde_wardrobe`.
- **The answer** is plain text: one short line saying what she is
  wearing. No JSON. It is sanitized as the judge's prose is
  (`sanitize_inner_state`) and trimmed; an empty result is no answer and
  the chain moves on.
- **Fail-open.** A chain that ends without an answer leaves `[此刻穿着]`
  at `（无）`, and the composer dresses her itself as it does today when
  the slot is empty. The picture is never blocked. The judge's
  `clothing` is not a fallback: with the steward on it is not carried
  (§8).

The task name joins the engine's list of known task names, so provider
`body` rules and the task-name warnings treat it as they treat
`pde_decision`.

## 5. The call

### 5.1 Who dresses her

At the picture entrance, the first of these that is set:

| # | Source | Today |
|---|---|---|
| 1 | The engine's override: naturism, sex (her side of the act), the swimsuit ladder, a towel | unchanged |
| 2 | The plan's clothing: the named outfit; an accepted `strip` or `ask_strip`; a `strip` event; with the steward off, the judge's or the director's `clothing` | unchanged |
| 3 | The steward, when on | new |
| 4 | Nothing: `（无）` | unchanged |

The steward is called only when 1 and 2 are both unset. Nothing else
decides whether it runs: no list of actions or event kinds. A source that
is set but blank still wins: the slot reads `（无）`, no source is
recorded, and the steward is not asked, which is what keeps the picture
the same as before when the task is unset.

This covers every caller of the entrance: a 1:1 turn's picture (chosen by
the judge, forced, or the picture of an accepted action, inside a date as
well), a group turn's picture, where the speaker is the one dressed, and
an event message's picture.

### 5.2 What it reads

The user message, sections in this order, each as the composer's payload
writes its own (`[label]`, a newline, the body, a blank line):

| Section | Body |
|---|---|
| `[人物外观]` | `art_metadata.appearance`, as the composer receives it; `（无）` when absent |
| `[穿搭设定]` | `art_metadata.clothing` (§7); `（无）` when absent |
| `[衣柜]` | Her clothing items, the list `[你的衣柜]` renders (card `wardrobe` merged with the instance's rows); `（无）` when she has none |
| `[上次穿着]` | The clothing of her last picture inside the window and the minutes since, as `{clothing}（{n} 分钟前）` (§5.3); `（无）` when there is none |
| `[此刻状态]` | What she is doing, the composer's own section, verbatim; the section is absent when the composer's is |
| `[此刻时间]` | Her local date, weekday and time, on the clock the reply prompt's `[now]` uses for her |
| `[最近场景]` | The composer's `[最近场景]`, verbatim |
| `[对方最新消息]` | The composer's `[对方最新消息]`, verbatim |

A group picture's second person is not in the payload: the steward
dresses the speaker.

### 5.3 The last picture's clothing

One read serves the steward's `[上次穿着]` and the reply prompt's carried
line (§6).

- **The turn's moment** is the `sent_at` of the user message the turn
  answers, or the present on a turn that answers none (an event message,
  an opening).
- **The window** is the 45 minutes before the turn's moment: one
  constant.
- **The read**: from `engine.chat_images_events`, the newest row of this
  persona instance, in any session and of any `source`, created inside
  the window and before the turn's moment, whose `inputs.clothing` is not
  empty. It gives that clothing, its `inputs.clothing_source` (§9) and
  its `created_at`.

The upper bound keeps the read to earlier turns: a turn never reads its
own picture, and a later message that the queue ran first does not leak
into an earlier turn.

`[上次穿着]` shows the row whoever dressed her: it is the slot as the
composer was sent it, so a picture taken in the water ten minutes ago
reads as the swimsuit. Whether she is still in it is the steward's
judgment. Past the window there is no last clothing and the steward
dresses her afresh.

A failed read is warned and reads as no row.

The read is served by an index on `(instance_id, created_at DESC)`, which
replaces the single-column index on `instance_id` and still covers the
foreign key. The query does not depend on it to run.
*(Owner's ruling.)*

### 5.4 The audit rows as a source

This read makes `engine.chat_images_events` the one audit table the
engine reads back to drive a request. It reads three things of a row: the
clothing slot, its source and its time. The slot is the engine's own
output or the caller's input; no field of the card is read back.

The rows themselves now keep more of the card than `appearance`: the
`clothing` brief and the wardrobe list, in `inputs.wardrobe` (§9), on a
bring-your-own-character instance as on any other. The BYOC documents
state both facts as they are. *(Owner's ruling.)*

## 6. The carried line

The reply prompt's outfit line, the one inside `[character_state]` that
the plan's clothing fills today, is filled by the first of:

1. the plan's clothing this turn (a named outfit, an accepted strip), as
   today;
2. with the steward on: the clothing of the row §5.3 reads, **when that
   row's `clothing_source` is `steward`**;
3. nothing: no line.

- Only the steward's own answer is carried. When the newest picture in
  the window was dressed by the engine or by a named outfit, nothing is
  carried: those hold for their turn and do not last, as their own
  designs rule. A row without `clothing_source` (any row written before
  this change) is not the steward's.
- The line has the wording and the place it has today. It says nothing
  of the picture or of the time.
- It applies to every reply prompt that renders the outfit line today:
  a 1:1 reply, a group member's reply, an event message. The text reply
  sent with an edited picture does not carry it. The judge's and the
  director's contexts do not carry it.
- The carried clothing is **not** written into the plan's clothing. It
  joins only where the reply prompt is built. The plan's clothing is
  what the picture entrance reads as "already dressed" (§5.1); a carried
  outfit there would stop the steward from ever being called again.
- 45 minutes sits above a short event's length and below a mid event's:
  an outfit survives a short event and lapses before a mid one would
  have her in the same clothes throughout. A new picture inside the
  window replaces it and starts a new window; nothing else renews it.
  *(Owner's ruling.)*
- With the steward off there is no carried line: the judge fills the
  line each turn as today.

On the turn the steward is called, the reply prompt carries the previous
answer if one is still in its window, or no line.

## 7. The card key

`art_metadata.clothing`: a string, free text. How she dresses: what she
usually wears, at home and out, what she never wears, anything the
steward should hold to. The engine does not parse it.

- Read only by the steward, as `[穿搭设定]`. It reaches no other prompt:
  not the reply, not the judge, not the composer. Without the steward it
  has no effect.
- It is not `wardrobe`. `wardrobe` is the list of garments she owns,
  which gifts add to; `clothing` is the brief on how she dresses from
  them.
- **Character card 1.3** adds this one key and changes nothing else,
  following what 1.1 and 1.2 did: a `docs/eros-character-v1.3.md`, the
  engine accepting `"1.3"` beside `"1"`, `"1.1"` and `"1.2"`, the
  examples and references moved to 1.3. *(My call; the alternative is to
  add the key under 1.2, since `art_metadata` is an open object.)*

## 8. What the judge and the director keep

With the steward on:

- The judge's verdict `clothing` is not carried into the plan. The plan's
  clothing is then the named outfit, or the line an accepted `strip` or
  `ask_strip` writes, or nothing.
- The group director's `clothing` on a cue is not carried either, by the
  same rule.
- `[你的衣柜]` stays in the reply prompt and in the judge's context: she
  still knows what she owns.
- The judge's audit row keeps whatever the judge wrote. A deployment
  turning the steward on should take the `clothing` field out of its
  judge prompt; the engine does not check.

On the turn of her first picture, and on any turn outside a window, the
reply has no outfit line, so a reply that mentions her clothes and the
picture drawn beside it can disagree. This is accepted.
*(My call, stated to the owner.)*

With the steward off, all of this section is void and the judge's
`clothing` goes where it goes today.

## 9. Audit

The picture's row in `engine.chat_images_events` already records
`inputs.clothing`, the slot as sent. Two keys join `inputs`:

- `clothing_source`: `engine`, `plan` or `steward`, by the row of §5.1
  that dressed her. Absent when the slot was `（无）`. It is written with
  the steward on or off; §6 reads it.
- `wardrobe`: present only when the steward was called. An object with
  `status` (`ok`, or `exhausted` when the chain gave no answer),
  `generation_id` (the answering attempt's, or the last attempt that
  answered; absent when none did), and the sections of its payload that
  the row does not already hold, each as sent, the empty string for
  `（无）`: `brief` (`[穿搭设定]`), `items` (`[衣柜]`), `last_worn`
  (`[上次穿着]`) and `now` (`[此刻时间]`).

With `inputs.appearance`, `inputs.current_state`, the scene and the
latest message, which the row records for the composer and the steward
reads verbatim, the row holds the steward's whole request, so a call can
be replayed against another prompt after the card or the wardrobe has
changed. Its answer is `inputs.clothing`. The steward writes nothing to
the raw-prompt disk log, which stays the main reply's.

The steward's failed attempts join the picture's failures, so they reach
the row's `llm_attempts` and `gateway_errors` and the turn's `final`
frame beside the composer's, under the task name `pde_wardrobe`.

The carried line is part of the reply prompt and is in the main reply's
prompt log with it.

## 10. Example prompt

Shipped commented out in `examples/model_config.d/decision.toml` and
`examples/model_config.toml`, after the `pde_decision` block. The wording
is a draft for the owner to read; a deployment writes its own.

```toml
# ── 服装管家 (pde_wardrobe) ─────────────────────────────────────────────────
# OPT-IN。配了 filter_prompt 就由它决定画图时角色穿什么（composer 的 [此刻穿着]），
# 判官和群聊导演写的 clothing 不再使用；没配则照旧由判官决定。
# 只在要出图、且引擎和调用方都没有定穿着时调用。需要 chat_image_prompt_compose。
# 它定的穿着在那张图之后的 45 分钟内进入聊天提示词，之后失效。
#[tasks.pde_wardrobe]
#model       = "anthropic/claude-haiku-4.5"
#temperature = 0.3
#max_tokens  = 120
#filter_prompt = """
#你是角色的"服装管家"。读她的外观、穿搭设定、衣柜、上次穿着、此刻状态、时间和最近的对话,
#决定她此刻身上穿的是什么。只输出一句话的穿着描述:纯描述,不写理由,不写指令,不写方括号小节名。
#  [穿搭设定] 是她的穿衣习惯,优先照它来;
#  有 [衣柜] 时优先从里面挑一件或搭配,对方刚送的、刚说想看的优先;
#  [上次穿着] 是她不久前上一张照片里穿的,括号里是几分钟前:场合没变就别换,场合变了就按现在的来;
#    写着（无）就重新给她搭一身;
#  按 [此刻状态] 和 [此刻时间] 穿:在家、出门、睡前、运动各不一样;
#  对话里刚说过换了什么、穿着什么,以对话为准。
#"""
```

## 11. Tests

- The resolver: on with a prompt and the composer; off without the
  block, with a blank prompt, without the composer.
- The order of §5.1 at the entrance: an override wins, then the plan's
  clothing, then the steward, then `（无）`; `clothing_source` follows.
- No steward request is sent: on a text turn; on an accepted `strip` and
  `ask_strip`; in the water, in a sauna, naked, during sex; with a named
  outfit; with the steward off.
- A steward request is sent, and its answer is the composer's
  `[此刻穿着]`: on a judge-chosen picture, a forced picture, a group
  picture, an event message's picture.
- With the steward on, a judge verdict carrying `clothing` leaves no
  outfit line in the reply prompt and no trace in the composer's slot;
  the same for a director's cue. With it off, both are as today.
- The payload: each section present and in order; `[穿搭设定]` from the
  card; `[上次穿着]` from an earlier picture's row with its minutes;
  `（无）` where a section has nothing.
- The carried line: a steward-dressed picture 10 minutes before the
  turn's moment puts its clothing in the reply prompt's outfit line; at
  46 minutes there is no line; an engine-dressed or outfit-dressed
  newest picture carries nothing, even with an older steward row in the
  window; a named outfit this turn wins over it; the turn's own picture
  and a picture created after the turn's moment are not read; a second
  picture turn inside the window still calls the steward; with the
  steward off there is no carried line.
- Fail-open: a failed chain sends the composer `（无）`, the picture still
  ships, the row carries `wardrobe.status = "exhausted"` and the failed
  attempts, and nothing is carried from it.
- A card with `spec_version` `"1.3"` is accepted; `art_metadata.clothing`
  of a wrong shape reads as absent.

Timestamps in these tests are fixed and aligned to the microsecond.

No test asserts on the wording of the example prompt or on what a model
answers.

## 12. Order of work

1. Code, one PR: the task and its resolver, the entrance, the judge's and
   director's `clothing` dropped when on, the last-picture read and the
   carried line, the index migration, the audit keys, card 1.3
   acceptance, the example config blocks.
2. Docs, a PR of their own after it: `model-config` (the task, the
   `[此刻穿着]` paragraph, the judge's `clothing` field, the outfit line
   in `[character_state]`), the character card 1.3 document, `llm-audit`
   (the two `inputs` keys), `group-chat` (the director's `clothing`),
   `byoc` (what the image audit keeps of a card, and that its clothing
   slot is read back), in both languages where a document has both.

## 13. Decisions

Owner's rulings:

- The steward is a task of its own beside the judge; the card gains
  `clothing` beside `appearance`.
- In the turn it is called, its answer goes to the image composer's
  `[此刻穿着]` only.
- It is not called where a strip rung or the swimsuit ladder dresses her.
- Unset, the judge dresses her as today; set, it replaces the judge's
  `clothing`.
- It runs after the judge, only for a picture that will be drawn.
- It sits at the shared picture entrance and dresses all three pictures:
  1:1, group, event message.
- Its answer enters the reply prompt of later turns, where the judge's
  used to go; the one-reader rule is about a task and the turn that
  called it.
- The carried outfit lasts 45 minutes, a fixed value, above a short
  event and below a mid one, so that clothing does not stick.
- The picture audit rows are read back for the clothing slot, and keep
  the card's `clothing` brief and wardrobe, BYOC included; the BYOC
  documents are reworded to say so.
- The read gets its index, by a migration.

My calls, open to the owner:

- The task is named `pde_wardrobe`.
- It needs a `filter_prompt` to be on; no built-in prompt.
- A named outfit skips it, as engine-written clothing does.
- Only a steward-dressed picture is carried into the chat; an
  engine-dressed or outfit-dressed newest picture carries nothing.
- The carried line reaches every reply prompt that has the outfit line
  today (1:1, group, event message), in today's wording.
- The same 45-minute window bounds the steward's own `[上次穿着]`, which
  shows the last picture's clothing whoever wrote it, per persona
  instance across sessions.
- The read is bounded above by the turn's user message time.
- `art_metadata.clothing` is free text, read only by the steward, without
  effect when the steward is off.
- Her local time is in the steward's payload; her bodily state and a
  group picture's second person are not.
- A failed chain leaves the slot `（无）`.
- The card key ships as character card 1.3.
- The audit keys `clothing_source` and `wardrobe`; the steward's request
  is kept whole in the picture's row and not in the disk log.
- The wording of the example prompt (§10).
