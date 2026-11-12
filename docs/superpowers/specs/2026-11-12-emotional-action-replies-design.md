# Emotional action replies — Design

- **Date:** 2026-11-12
- **Status:** Approved
- **Type:** Engine change. No migration. No HTTP, SSE or config change.
  Assistant rows of the listed action turns gain `metadata.tone`. Off unless
  the deployment has `tones.emotional` above 0.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-11-12-chat-tone-emotional-design.md` (the `emotional`
  tone; an action turn never draws) and
  `2026-11-12-emotional-event-messages-design.md` (a fixed per-kind list
  under the same switch). Everything there stands except where this
  document says otherwise.

## 1. Intent

Some things the user does to her are not things she answers calmly,
whether she lets them happen or not. Her reply to one of these actions is
written in the `emotional` tone: from who she is, what the user just did
and what she decided, without the chat history, the user's profile or the
affinity conclusions in front of her.

This is fixed per action, not drawn, and it does not depend on whether she
accepted or refused.

## 2. The list

A `UserAction` property, `speaks_emotionally(&self) -> bool`, true for
these seven and false for every other action:

| action | marker |
|---|---|
| `french_kiss` | 凑过来想和你舌吻 |
| `tease` | 伸手想挑逗你 |
| `hug_from_behind` | 悄悄走到你身后，想抱住你 |
| `touch` | 伸手想摸你 |
| `strip` | 伸手想脱你的衣服 |
| `threaten` | 威胁你 |
| `tip` | 打赏 |

It lives in `eros-engine-core` next to the action's other properties. It is
not configurable.

## 3. When it applies

A reply is written in the `emotional` tone when all of these hold:

- it is the 1:1 user-message turn, and the user message carries a listed
  action;
- the finalised plan's action bears text (`reply_text` or
  `reply_text_image`);
- `[tasks.chat_companion].tones.emotional` is above 0.

Accept and refuse alike. Nothing is drawn. How the response is decided (the
judge, the give roll, the forced refusals of an incompatible strip) is
unchanged, and so is everything that follows from it: affinity, the
picture of an accepted strip, the outfit an accepted strip leaves her in.

The tone is decided where the draw is, on the finalised plan. A bare
`reply_image` plan bears no text, so its turn is not `emotional` even if a
failed picture later leaves it a text reply; the draw treats that case the
same way.

An unlisted action turn is untouched and still never draws. A turn without
an action keeps the draw of the tone design. With `tones` absent or 0,
every action turn is what it is today, byte for byte.

## 4. The reply

The request is the `emotional` chat-turn request of the tone design (§4
there): the least-hint system prompt and the driving user message, with no
earlier row. Two things an action turn adds:

- the `[user_action]` block is appended to the system prompt, exactly as
  the default request appends it (the action, her response, and for a tip
  the tip-personality closing). Without it she would not know what she
  decided;
- the driving user message keeps its action marker, as it already does in
  its model-facing form. The fallback for a driving row that is not in the
  session renders the event's text with the marker too
  (`prompt::user_text_with_action`), so the marker never drops; with no
  action that function returns the text unchanged, so a turn without an
  action is unaffected.

Everything else the `emotional` prompt drops stays dropped. An action turn
rolls no nudges, as today.

## 5. The record

The assistant rows of a listed action turn under the switch carry
`metadata.tone = "emotional"`. An unlisted action turn's rows carry no
`tone`. On chat turns the key therefore means "drawn, or a listed action";
`metadata.action` on the user row tells the two apart.

## 6. Tests

- `speaks_emotionally` is true for exactly the seven actions.
- With `tones.emotional > 0`: a listed action turn, accepted and refused,
  sends the system prompt and the driving line alone; its system prompt has
  `[user_action]` with the matching outcome sentence and no
  `[user_profile]`; its row carries `tone = "emotional"`. A tip's
  `[user_action]` keeps the tip-personality closing.
- With the switch on, an unlisted action turn keeps its history and has no
  `tone`.
- With `tones` absent, a listed action turn keeps its history and has no
  `tone`.

## 7. Documentation

In the one docs pull request of the tone features: the tone section of
`docs/model-config.md` (+ `.zh`) and the actions section of
`docs/api-reference.md` (+ `.zh`) name the seven actions. No migrating
note.

## 8. Decisions

Ruled by the owner:

- A listed action's reply is `emotional` whether she accepts or refuses.
- The seven actions of §2. `kiss`, `lick`, `hug`, `pat`, `bluff`, `trick`,
  `give`, `gift`, `photo` and `custom` were offered and left out.

Calls made in this document, open to correction:

- The list is fixed in the engine and applies only under the
  `tones.emotional` switch, as the event list does.
- `[user_action]` is kept in the `emotional` prompt (§4).
- `metadata.tone = "emotional"` on listed action turns, no key on the rest.
- 1:1 only: a group turn never uses the tone.

## 9. Left to the builder

- Where `run_stream` decides the tone for an action turn, next to the draw.
- How `build_emotional_reply_request` appends `[user_action]` (it reads the
  event's action and the plan's `action_response`, as the default builder
  does).
