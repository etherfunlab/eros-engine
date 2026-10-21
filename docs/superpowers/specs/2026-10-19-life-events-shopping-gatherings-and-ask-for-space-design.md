# Life events: shopping, gatherings, and the ask-for-space choice — Design

- **Date:** 2026-10-19
- **Status:** Approved
- **Type:** Engine change. Two migrations (`engine.life_choices`; the affinity
  `event_type` check widened by one value, `life_choice`). Additive
  API changes (five drawable kinds, two non-drawable kinds, a `life_choice`
  field on the affinity delta, one new endpoint).
  `eros-engine-core::events` gains seven `EventKind` variants (breaks
  exhaustive matches); `is_offline` becomes persona-dependent for one kind.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — five new drawable kinds; the first life
  choice: she asks to take a break, the user answers accept or decline.
- **Builds on:** `2026-10-15-life-events-v2-design.md` (*the v2 spec*),
  `2026-10-16-life-events-offline-and-scares-design.md` (*the offline
  spec*), `2026-10-17-life-events-run-ins-and-kind-filters-design.md`,
  `2026-08-16-affinity-40-design.md` and `2026-10-13-give-categories-design.md`
  (the rejected-action tier drop). This spec extends the v2 kind table and
  the offline spec's mechanics, and adds the first `life_choices` flow,
  v1 deliberately parasitic on the life-events machinery. Everything
  else stands.

## 1. Motivation

Two separate pushes, one page.

**More of her own life.** Offline events exist so the persona is not
always there: a character who goes dark has a life of her own and does not
orbit the user, and a user whose companion is at a gathering has a reason
to talk to another character. This spec adds a shopping afternoon and two
gatherings to the offline repertoire, plus two small shopping finds that
give a close relationship something to want (and the downstream's gifting
a hook).

**The first life choice.** When chemistry has run two tiers past bond and
her patience has been worn low for three judged turns running, the
relationship is structurally lopsided: passion ahead of friendship, and
she is tired. She asks for a break — a real request with a real cost on
every path, delivered through the machinery that already exists. The full
`life_choices` system (richer choice types, its own tables of options) is
deliberately deferred; v1 proves the shape by riding on life events and
the affinity delta.

## 2. New drawable kinds

| kind | label | category | weight | cooldown | lasts | gate |
|---|---|---|---|---|---|---|
| `saw_bag` | 看中了一个包 | `short` | 2 | 48 h | 1 h | female |
| `saw_clothes` | 看中了一件衣服 | `short` | 2 | 48 h | 1 h | female |
| `out_shopping` | 在逛街 | `mid` | 2 | 24 h | 2–4 h | none |
| `bros_gathering` | 兄弟聚会 | `mid` | 1 | 72 h | 2–4 h | none |
| `sis_gathering` | 姐妹聚会 | `mid` | 1 | 72 h | 2–4 h | none |

That makes 43 drawable kinds. The female gate reads
`art_metadata.gender == "female"`, the gate `period` already uses. No new
needs factor; the two gatherings join `prefer_events`' vocabulary like any
`mid` kind. All five are excluded during `hospitalized` and `post_surgery`
(she is not out and about), joining the v2 spec exclusion list.

**Both genders draw both gatherings, at the same weight.** This is
deliberate: it reads as equality, and the collisions are content — a male
persona at a 姐妹聚会 is exactly the kind of thing a user asks about.
There is no gender factor anywhere in the draw.

### 2.1 Offline behaviour

| kind | who goes offline | announced |
|---|---|---|
| `out_shopping` | a female persona; others keep replying | like `busy`: told at intimacy rung 2 and up, silent below |
| `bros_gathering` | everyone | never, like `asleep` |
| `sis_gathering` | everyone | never, like `asleep` |

A gathering means she is with her people and not looking at her phone;
she explains afterwards through the existing unexplained-offline line. A
female persona absorbed in shopping goes dark the same way; any other
persona wanders the shops with phone in hand, and the event is an
ordinary told incident.

`is_offline` therefore stops being a pure function of the kind: it takes
the persona (`is_offline(self, persona: &PersonaTraits)`). The four
existing offline kinds and the two gatherings ignore the persona;
`out_shopping` is offline iff `persona.female`. Everything downstream of
offline-ness follows the persona-aware answer: the ghost-protection gates
on the draw (ghosting switch, 10-message floor), the shared 12-hour
offline cooldown, the silence while it lasts, and the explanation
afterwards. For a non-female persona `out_shopping` is simply never
offline: no gates, no silence, told like `reading`.

### 2.2 Cues

Told kinds, same proactive path as every event message. `{p}` is the
persona's `tip_personality`, trimmed; the rung is the intimacy rung
(1–3) already read by the draw.

| kind | condition | cue |
|---|---|---|
| `saw_bag` | rung 1 | （你在逛街，随手看到一个挺好看的包。） |
| `saw_bag` | rung ≥ 2, no `tip_personality` | （你在逛街，看到一个特别喜欢的包，很想要。） |
| `saw_bag` | rung ≥ 2, `tip_personality` | （你在逛街，看到一个特别喜欢的包，很想要。代入你「{p}」的打赏人格，把这份心动告诉对方。） |
| `saw_clothes` | same three | the same, with 一件…衣服 for 一个…包 |
| `out_shopping` | female, rung ≥ 2 | （你接下来要出门逛街，这段时间顾不上看手机。） |
| `out_shopping` | female, rung < 2 | not told: no model call, `message_id` NULL, `"message": null` |
| `out_shopping` | not female | （你正在外面逛街。） |
| `bros_gathering`, `sis_gathering` | always | not told |

At rung 1 the find is daily-life texture, nothing more. From rung 2 she
is allowed to want it out loud, in her tip personality's register when
she has one — the same register the tip action already closes on. Whether
anything comes of the wanting is the downstream's business; the engine
only sets the scene.

### 2.3 Prompt surfaces

Labels as in §2's table. The unexplained-offline texts follow the
existing format:

| kind | `- 刚才：` text | judge suffix |
|---|---|---|
| `out_shopping` | 在逛街，对方那时发来的消息你现在才看到 | 在逛街（{span}前结束，期间对方的消息你没有回） |
| `bros_gathering` | 去了兄弟聚会，对方那时发来的消息你现在才看到 | 兄弟聚会（{span}前结束，期间对方的消息你没有回） |
| `sis_gathering` | 去了姐妹聚会，对方那时发来的消息你现在才看到 | 姐妹聚会（{span}前结束，期间对方的消息你没有回） |

## 3. The ask-for-space choice

### 3.1 Trigger

Checked by the engine at the end of every 1:1 text turn whose affinity
evaluation committed (a "judged turn"), after the commit. It fires when
all of these hold, read from the committed, time-decayed state:

- **Gap:** chemistry tier − bond tier ≥ 2, on the two five-tier ladders
  (signed: only chemistry above bond counts).
- **Patience:** the derived `patience` endpoint is < 0.5 on this turn and
  on the two prior judged turns — three consecutive judged turns.
  Unjudged turns (eval skipped or failed) neither count nor break the
  run. The per-turn values are read from the session's affinity turn
  events (`state_after.patience`), not stored anew. 0.5 is a rule
  threshold owned by the engine, not a judge band.
- **Fresh streak:** all three turns postdate the session's newest episode:
  the request, its resolution, and any separation it started (the latest
  of `asked_at`, `resolved_at` and, for an accepted choice, the separation's
  `ends_at`). Firing a request consumes the streak; the next request needs
  three fresh low-patience judged turns. A judged action turn that fell
  while she waited, or during the separation, belongs to the episode and
  counts for nothing; the turn that converts an ignored ask is the first
  of the new streak.
- **Clear runway:** no session of the instance has a pending un-expired
  choice, and the instance has no `taking_space` event under way. (An
  offline stretch silences the judge anyway, so no trigger can fire inside
  one.) She has one life, so the instance holds at most one pending ask.
- **Session:** a 1:1 text session of a genome instance. A BYOC session never
  triggers: the resolution endpoint refuses BYOC, so an ask there could never
  be answered.

While a choice is pending and un-expired, the events route draws nothing
for the instance (`{"event": null, "message": null}`, like an incident
under way): the ask is itself the foreground drama, and it keeps an
offline stretch from starting mid-question. (A repeat of an `event_key` that
already drew still returns its event: the replay reads the stored row before
the pending check.) An ordinary incident that was
already under way when the ask fired (she can be `reading` on a judged
turn) keeps running; §3.4 says what happens to it at resolution.

There is **no cooldown beyond the mechanics**: if the user keeps
re-creating the condition, she keeps asking. The tier drop on accept and
decline lowers both lines, so the gap only closes once bond is at tier 1
(a tier-1 line cannot drop further while chemistry still can) or the user
raises bond; repeated asks are the realistic consequence of a
relationship kept lopsided, not a bug.

### 3.2 The ask

When the trigger fires, in order:

1. **Her words.** The companion chat-task model generates the request,
   through the same proactive path as an event message (persona, history
   window, memory recall, `[now]`; no judge, no streaming, no user row).
   Stage cue, final `user` turn, never persisted:

   ```
   （这段关系最近让你很累：心动还在，但交情和耐心都被磨薄了。你决定郑重地告诉对方：你想先分开一阵子，希望对方给你一个答复。）
   ```

   The message is an ordinary assistant row, `assistant_action_type =
   'proactive'`, `metadata.proactive = 'life_choice'`, unread until the
   read call, no post-process. A blank or stripped-empty generation, or a
   failed one, writes nothing and no choice row: the trigger may fire
   again on a later qualifying turn.
2. **The choice row.** One row in `engine.life_choices` (§3.6): kind
   `ask_for_space`, status `pending`, the tier gap at this moment
   (`tier_gap`, 2–4), `asked_at` now. The pending window is 24 hours;
   `expires_at` is derived (`asked_at + 24 h`), never stored.

The turn's own affinity persist (state + event row, one transaction) is
untouched: the trigger runs after it, and the ask's generation and writes
follow on their own time. The turn's long-poll release therefore usually
does not yet carry the choice; delivery is §3.3's serve-time composition.

### 3.3 Delivery

The notification rides the per-turn affinity delta fetch.
`BffAffinityDelta` gains one optional field:

```json
"life_choice": {
  "id": "…",
  "kind": "ask_for_space",
  "options": ["accept", "decline"],
  "asked_at": "…",
  "expires_at": "…",
  "message_id": "…"
}
```

The route composes it at serve time: the session's pending, un-expired
choice, attached whenever one exists (a pending choice is only ever
attached to the latest event, because any later ordinary message would
have resolved it). `null` / absent otherwise. The ask lands seconds after
the triggering turn's event commits, so the fetch that the turn itself
releases usually misses it; the adapted client's contract is to re-fetch
the delta when it reads a history row whose `occasion` is `life_choice`.
Both history routes (`/comp/chat/{session_id}/history` and the BFF history
and start routes) carry a proactive assistant row's `metadata.proactive`
as an optional `occasion` (omitted on every other row, like `read_at`).
The tier-drop affinity event (§3.5)
is **not** served on this route — `latest_turn_event` excludes it like
`time_decay`, so an unadapted client never meets an unknown `event_type`;
an adapted client gets the new state in the resolution response, and any
other device from the absolute affinity route.

**Degradation is the design.** A downstream that has not adapted ignores
the unknown field, renders the ask as an ordinary proactive message, and
its user replies in words — which is the ignore path (§3.4c). Nothing
breaks; the choice UI is strictly additive.

### 3.4 Resolution

A new endpoint answers it:

```
POST /v2/comp/session/{session_id}/life-choices/{choice_id}
{"response": "accept" | "decline", …}
```

with the same locale / tier / scope fields the events route carries (the
engine keeps nothing between calls). 1:1 text sessions only (`409
wrong_channel` otherwise, as on the events route). A choice that is not
pending or has expired is `409`; a resolution and an incoming user
message race transactionally — first writer wins, the loser gets the
`409`. The response returns the resolution, her generated reply, the
affinity state after the drop, and (on accept) the separation event. All
four paths:

**(a) accept.** Status → `accepted`. In one transaction:

- A `taking_space` event row in `engine.persona_events`, `started_at`
  now, `ends_at` now + **6 × tier_gap hours** (gap 2 → 12 h, 3 → 18 h,
  4 → 24 h; the gap is the one stored at ask time). If an incident is
  under way its `ends_at` is capped to now — she drops what she was
  doing; a background (trip, sickness) keeps running underneath.
- The tier drop (below), recorded as an affinity event of a new
  non-turn `event_type = 'life_choice'` with
  `context.life_choice = "accept"` and `context.choice_id`. Migration `0077`
  widens the table's `event_type` CHECK by this one value.

Then her goodbye is generated through the proactive path (an ordinary
proactive row, `metadata.proactive = 'life_choice'`) and returned:

```
（对方同意了你提出的分开请求，接下来 {n} 小时你们不联系。你跟对方道别。）
```

A failed or empty goodbye still leaves the resolution, the event and the
drop in place: the choice was made.

**(b) decline.** Status → `declined`. The same tier drop, in the same
transaction, `context.life_choice = "decline"`; no event row, no
slowdown. Her hurt reply is generated and returned:

```
（对方不同意你提出的分开请求，你心里不好受。）
```

**(c) ignore.** The first ordinary user **text** message that arrives
while the choice is pending and un-expired resolves it: status →
`ignored`, and a `fell_out` event row (label 为分开的事闹得不愉快, an
incident shaped like `quarrel`: 1 hour, not offline, no message of its
own) is written in the same transaction, before the turn runs. The clock is
the user row's `sent_at`, not the wall clock of the turn: only a message sent
after the ask (`asked_at <= sent_at`) converts it, so a message already in
flight when the ask landed answers nothing, and a queued or re-driven turn
reads the same. The `fell_out` row starts at that `sent_at`. So the
very turn that ignored her is answered with the falling-out in
`[近期事件]` and `正在经历`. An incident still under way from before the
ask is capped to that `sent_at` in that transaction, as on accept: the falling-out
displaces it, and the one-incident invariant holds. **No tier change**:
the judge sees the context and rates the turn itself. An **action turn** (a give, a tip, a
kiss) does not resolve the choice; it is answered by its own rules and
the ask stays pending — a gift while she waits is an attempt to mend,
not an answer. `/open` greetings and reactions resolve nothing. The
conversion never runs while a separation is under way at the message's
`sent_at`: such a message only ghosts, and it would otherwise cap the
separation.

**(d) expiry.** 24 hours with no answer and no message: the choice
simply lapses. Status stays `pending` and reads as expired everywhere
(`expires_at` past), the delta field stops being attached, the endpoint
answers `409`, a later message does **not** convert to `fell_out`. No
cost. A later ask needs the full trigger again.

### 3.5 The costs

**The tier drop (accept and decline).** Bond and chemistry each drop one
tier, with the rejected-action landing rule applied to each line: the
line lands at the same relative position one tier down, its two axes
scaled by the same factor; a tier-1 line goes to 0. Both lines' pending
deltas are cleared. Applied at resolution time, atomically with it — on
accept the drop is the state she comes back to; nothing further happens
at return. The drop is computed from the snapshot after time decay and the
endpoint refresh, so the drop functions take the deployment's affinity
tuning.

**The slowdown (accept only).** For 72 hours after the separation ends,
every **positive** committed delta on the four line axes is halved before
it is applied, and the cut part is dropped, not parked in `pending_deltas`;
negative deltas apply in full. It applies in the choice's session
only — the costs are that relationship's, like the drop; another session
of the same instance pays nothing. The window is derived through the
choice row (`life_choices.event_id` → the `taking_space` row's `ends_at`
to `ends_at + 72 h`) — no new state, and the returned `effective_deltas`
honestly show the halved values. The window exists even if the user only
returns later: it is wall-clock, not turn-count.

**The separation, while it lasts.** `taking_space` is a non-drawable
offline incident and reuses the offline machinery wholesale: every 1:1
text turn of the instance ghosts without the judge (`meta(ghost)`,
`done`, `final`; no needs rating, no ghost-streak write), action turns
and forced-image turns are exempt, `/open` returns `null`, no event draw
happens while it is under way, and it starts the shared 12-hour offline
cooldown. It is instance-scoped like every `persona_events` row: she has
one life, and taking space means her phone is down everywhere.

**The return is acknowledged either way.** The standard unexplained rule
needs a ghosted user row inside the window — but a user who honors the
break sends nothing, and she must still mark the return, or the arc
fizzles. So in the choice's session, `taking_space` is unexplained from
`ends_at` until the first assistant row with text after it, ghosted rows
or none; the wording follows whether the user wrote during the window.
Other sessions of the instance use the standard ghosted-row rule with the
left-column texts.

| | `- 刚才：` | judge suffix |
|---|---|---|
| user wrote during the window | 你们之前说好分开一阵，现在时间到了；对方那时发来的消息你现在才看到 | 说好暂时分开（{span}前结束，期间对方的消息你没有回） |
| user honored the break | 你们之前说好分开一阵，现在时间到了，你们重新开始联系 | 说好暂时分开（{span}前结束，对方等到了约定之后） |

The label seen by exempt action turns while it lasts is 说好暂时分开.
Messages sent during the separation ghost as in any offline stretch; they
never convert any pending ask, in this session or a sibling one.

### 3.6 Storage

One new table:

```sql
CREATE TABLE engine.life_choices (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id  UUID NOT NULL REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,              -- 'ask_for_space'
    status      TEXT NOT NULL DEFAULT 'pending',
                -- 'pending' | 'accepted' | 'declined' | 'ignored'
    tier_gap    SMALLINT NOT NULL,          -- 2..4, at ask time
    asked_at    TIMESTAMPTZ NOT NULL,
    resolved_at TIMESTAMPTZ,
    message_id  UUID REFERENCES engine.chat_messages(id) ON DELETE SET NULL,
    event_id    UUID REFERENCES engine.persona_events(id) ON DELETE SET NULL
);
CREATE INDEX life_choices_session_idx ON engine.life_choices (session_id, asked_at DESC);
CREATE INDEX life_choices_message_idx ON engine.life_choices (message_id);
CREATE INDEX life_choices_event_idx   ON engine.life_choices (event_id);
```

Per the FK rules: `session_id` is non-null (a choice belongs to one
relationship — the trigger is that session's affinity) and cascades;
`message_id` is the audit thread to the ask, `event_id` to the
`taking_space` row an accept wrote (NULL on every other path), and both
outlive what they point at. Expiry is derived from `asked_at`, never
stored. The separation and falling-out rows live in
`engine.persona_events` like any event.

`taking_space` (说好暂时分开) and `fell_out` (为分开的事闹得不愉快) are
`EventKind` variants that are **never drawn**: they have no draw rule,
are not accepted in the events route's `kinds` filter, and exist for
storage, rendering and the offline machinery only.

### 3.7 Out of scope for v1

- The full `life_choices` system: more choice kinds, configurable
  consequences, a choices listing route. This flow is the proof.
- Voice: a voice session cannot trigger, deliver or resolve a choice
  (voice turns write no affinity). A pending choice is invisible to it.
- Group sessions: no affinity writes, no trigger.
- Downstream UI (buttons, copy, how the quarrel is shown) — the web
  client adapts on its own schedule; until then the degradation path is
  the behaviour.
- No judge involvement in the trigger: it is engine math over committed
  state, per the engine/judge division of labour.

## 4. Documentation

`docs/life-events-needs.md` (+ `.zh`): the five kinds, the
persona-dependent offline rule, the gathering easter egg.
`docs/api-reference.md` (+ `.zh`): the kind table, the new endpoint, the
`life_choice` delta field. `docs/affinity-model.md` (+ `.zh`, `.ja`): the
`life_choice` event type, the tier drop and slowdown.
`docs/model-config.md` (+ `.zh`): cue and explanation texts.
`examples/*.toml`, `.env.example`: nothing — no new config.

## 5. Decisions already made (do not reopen)

- Both gatherings offline for everyone; `out_shopping` offline only for
  female personas. Announcement rules exactly as §2.1.
- Equal gathering weights for all personas — the collision is a feature.
- Trigger thresholds (gap ≥ 2 tiers, patience < 0.5 × 3 judged turns),
  the 24 h pending window, 6 × gap hours of separation, one-tier drop on
  both lines on accept and decline, 72 h halved positive growth after
  accept only, no direct tier change on ignore, no cost on expiry, no
  extra cooldown on re-asks.
- Delivery on the affinity delta fetch; unadapted downstreams degrade to
  the ignore path by construction.
- Her ask, goodbye and hurt reply are generated by the chat-task model;
  the shopping finds want out loud from rung 2, in the
  `tip_personality` register when there is one.
