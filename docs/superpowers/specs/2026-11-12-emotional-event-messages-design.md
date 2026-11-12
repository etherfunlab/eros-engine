# Emotional event messages — Design

- **Date:** 2026-11-12
- **Status:** Approved
- **Type:** Engine change. No migration. No HTTP, SSE or config change.
  Event message rows of the listed kinds gain `metadata.tone`. Off unless
  the deployment has `tones.emotional` above 0.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-11-12-chat-tone-emotional-design.md` (the `emotional`
  tone, its prompt and its request) and the life-events designs (an event
  that is told starts with a message of hers, generated from a cue).
  Everything there stands except where this document says otherwise.

## 1. Intent

Some things that happen to her are not things she would report calmly. When
one of these events starts, the message she sends about it is written in the
`emotional` tone: from who she is and what just happened, without the chat
history, the user's profile or the affinity conclusions in front of her.

This is fixed per event kind, not drawn. The tone draw of the chat turn is
untouched.

## 2. The list

An `EventKind` property, `speaks_emotionally(self) -> bool`, true for these
sixteen kinds and false for every other:

| kind | label |
|---|---|
| `quarrel` | 吵架了 |
| `scolded_by_superior` | 被上级骂了 |
| `scolded_superior` | 骂上级了 |
| `got_hit` | 被打了 |
| `hit_someone` | 打人了 |
| `police_encounter` | 被警察拦下问话 |
| `ex_wants_back` | 前任来求复合 |
| `irritable` | 烦躁 |
| `want_to_quit_job` | 想辞职 |
| `unwell` | 不舒服 |
| `sick` | 生病中 |
| `period` | 月经中 |
| `sudden_desire` | 突然很想要 |
| `drinking` | 在喝酒 |
| `smoking_weed` | 在抽大麻 |
| `just_have_sex` | 刚做完爱 |

The list lives in `eros-engine-core` next to the kind's other properties
(category, place, offline). It is not configurable.

## 3. When it applies

An event message is written in the `emotional` tone when all of these hold:

- it is the message an event sends when it starts (the `Event` occasion of
  the proactive path, streamed or not);
- the event's kind is on the list;
- `[tasks.chat_companion].tones.emotional` is above 0.

With `tones` absent or 0, every event message is what it is today, byte for
byte, and no row gets `metadata.tone`. One key still turns every tone off.

Nothing is drawn: a listed kind is always `emotional` under the switch, and
the base probability's value plays no other part. Whether a kind is told at
all (`is_told`) is unchanged; an untold kind sends no message and is not
affected.

Not affected: the user's turns while the event is under way (they keep the
chat turn's draw), every other proactive occasion (holiday, birthday,
openers, check-in, life choices), and how the event is drawn, placed or
stored.

## 4. The message

The event message's request today is the default reply request (history,
recall, profile, affinity) with the event's cue appended as the last user
message. An `emotional` event message is built the way an `emotional` chat
turn is (§4 of the tone design), with the cue in the place of the user's
line:

- messages: the `emotional` system prompt, then the cue as the one user
  message; no earlier row of the session. The user message is always the
  cue: never the recall query (the user's latest line), which is what
  `build_emotional_reply_request` would fall back to with a nil driving id;
- the system prompt is `build_emotional_prompt` with the event message's own
  inputs: the event's outfit as the clothing, the event lines (`正在经历` /
  `刚才`) under `[now]`, no `reply_mode` (rule ① in its default form), no
  quote, no body state, no needs line (as today for an event message). The
  event lines pass through as `prepare` reads them today: the event being
  started is not stored until its message is, so it never appears there;
  the cue is what tells her about it;
- `[shared_memories]`: the three latest episodes; none with the
  relationship layer off;
- no embedding call; world and stories render from their digests.

The persona, prompt traits, tier, model and sampling resolve as they do for
the event message today. Generation, streaming, storage, the event row and
everything the client receives are unchanged.

## 5. The record

The event message row of a listed kind, written under the switch, carries
`metadata.tone = "emotional"`. Every other event message, and every other
proactive message, carries no `tone`.

So on chat turns the key's presence means "drawn", and on event messages it
means "a listed kind". A reader separates the two by `metadata.proactive`,
which an event message already carries.

## 6. Tests

- `speaks_emotionally` is true for exactly the sixteen kinds.
- With `tones.emotional > 0`, a listed kind's event message request is the
  system prompt and the cue alone (the user message equals the cue, not the
  user's latest line); its prompt has no `[user_profile]`, and a background
  event already under way still shows under `[now]`; the row carries
  `tone = "emotional"`. Both the streamed and the non-streamed path.
- With the switch on, an unlisted kind's request keeps its history and its
  row has no `tone`.
- With `tones` absent, a listed kind's request keeps its history and its row
  has no `tone`.

## 7. Documentation

In the docs pull request of the tone feature, or one after it: the tone
section of `docs/model-config.md` (+ `.zh`) names the switch's effect on
event messages, and `docs/life-events-needs.md` (+ `.zh`) lists the sixteen
kinds. No migrating note.

## 8. Decisions

Ruled by the owner:

- The list is fixed per kind in the engine, and applies only when the
  deployment has turned `tones.emotional` on.
- A listed kind's message is always `emotional`; nothing is drawn.
- The sixteen kinds of §2. `breakup`, `ex_message`, `pregnancy_scare`,
  `period_scare`, `hospitalized`, `missed_ride`, `got_lost`, `got_bumped`,
  `bumped_someone`, `temp_work`, `pondering_life` and `want_to_go_home`
  were offered and left out.

Calls made in this document, open to correction:

- Only the message an event sends when it starts; the user's turns during
  the event keep the ordinary draw.
- The cue takes the user line's place in the `emotional` request.
- `metadata.tone = "emotional"` on listed event messages, no key on the rest
  (§5).
- Rule ① renders in its default form, as event messages have no
  `reply_mode`.

## 9. Left to the builder

- How `prepare` in the proactive path chooses between the two request
  builders, and how `build_emotional_reply_request` takes a cue in place of
  a driving row (a parameter, or a small sibling).
- Where in the proactive path `tone` is added to the row's metadata.
