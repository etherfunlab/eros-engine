# Activity: a date as a sub-session — design

## 1. Intent

A user can ask the persona out. If she says yes, the two of them step out of the
main chat into a scene of its own, with a place, a plan and a length, and talk
there until it ends. When it ends, the date is judged once as a whole, moves the
relationship, and becomes part of what the two of them remember.

The mechanism is called an **activity**. Internally an activity runs in a
**sub-session**: a second message stream that belongs to a chat session. In this
version the only activity is a date, and only the user can propose one.

## 2. Goals, non-goals, anti-goals

**Goals**

- An invitation with three parts: a duration, a place, a plan.
- She accepts or refuses. The engine holds a threshold; past it, the judge
  reads the invitation.
- An accepted date has its own message stream and its own history window. The
  main chat is paused while it runs.
- A date ends on the wall clock, or earlier when the user ends it.
- One judgement of the whole date decides what it did to the relationship.
- A date leaves one episode in relationship memory however long it ran, so
  what happens inside a date does not crowd other memories out of recall.
- Relationship milestones come out of dates. A date is an invitation the engine
  recorded and an acceptance the engine recorded, so a moment inside it is
  `real` by construction; nothing has to guess between `real` and `roleplay`.

**Non-goals (this version)**

- Dates she proposes.
- A `long` duration (1 to 7 days).
- A message from her when the date ends.
- Dates in World Stories.
- Any kind of activity other than a date. No `kind` column, no registry.
- Changing how the main chat is remembered. Only what is said inside a date is
  compressed; main-chat turns are written and scanned as before.

**Anti-goals**

- A date must never be a worse deal than the same number of ordinary turns, and
  must not be a way to farm affinity by sending invitations without talking.
- No continuous number comes out of an LLM. The activity judge answers in the
  same ordinal protocol as the per-turn affinity judge.
- Date turns must not move the lines turn by turn; the date is judged once.
- No new background task.

## 3. Vocabulary

| Term | Meaning |
|---|---|
| activity | Public name of the mechanism, and of one accepted date. |
| sub-session | The `engine.chat_sessions` row an activity's messages live in; `channel = 'activity'`. |
| parent session | The 1:1 text session the invitation was sent in. |
| invitation | A chat turn in the parent session carrying the `date` action. |
| settlement | The one-time step after a date ends: judge, affinity, journal row. |

## 4. The invitation

### 4.1 Shape

The invitation is a user action on an ordinary chat turn in the parent session,
sent through the existing send routes:

```json
{"type": "date", "duration": "short", "place": "海边的餐厅", "content": "吃一顿大餐"}
```

- `duration`: `short` (1 hour) or `mid` (4 hours). These are the caps of the
  life-event categories of the same names; the date lasts exactly that long
  unless ended early.
- `place`: 1 to 40 characters after trimming.
- `content`: 1 to 120 characters after trimming.

Anything else is rejected with `422`. The turn may carry user text as any action
turn may.

The action is stored in the user row's `metadata.action` with the settled
response, as every action is. The judge's line is:

```
[用户动作] 对方邀请你去约会：地点「{place}」，安排「{content}」，大约 {1|4} 小时
```

### 4.2 Preconditions

Checked at route entry; a failure writes nothing.

| Condition | Response |
|---|---|
| The session is not a 1:1 text session (voice, group, or a sub-session) | `409 wrong_channel` |
| The session is a BYOC session | `400 byoc_unsupported` |
| A date of this persona instance is in progress | `409 activity_in_progress` |
| Any session of the instance holds a pending, unexpired life choice | `409 choice_pending` |

A date of the instance that has ended but is not yet settled is settled first
(§6.1), then the turn proceeds.

### 4.3 The gate

The engine checks the gate before the judge runs, on the parent session's stored
affinity after time decay, the same read the turn uses:

| Duration | Gate |
|---|---|
| `short` | none |
| `mid` | bond at tier 2 or above, on the five-tier ladder |

For both durations the gate also fails while an offline-kind event of the
instance is in progress, `taking_space` included: she cannot be reached, so she
cannot go.

Compatibility of the pair's `affinity_scope` is not read.

A failed gate forces a refusal the way the unused-substance roll on `give` does:
the judge's `[用户动作]` line gains `，这次你不会答应`, and the effective response is
`refuse` whatever the judge answers. Without an LLM judge, the rule engine's
accept becomes `refuse`.

### 4.4 The judge

Past the gate, the PDE judge decides as it does for any action: it reads the
line above with everything else it reads on a turn (`[近期事件]`, needs, persona)
and answers `accept` or `refuse`. No explicit refusal reads as accept. A
background in progress (a trip, an illness) does not fail the gate; the judge
sees it and decides.

### 4.5 Refused

A refused invitation is an ordinary refused action turn with no tier drop, the
same footing as a refused tip or social action:

- her reply is written to the refusal;
- the per-turn affinity judge scores the turn as usual;
- nothing else is written: no activity row, no sub-session, no event.

There is no cooldown. The user may ask again.

### 4.6 Accepted

After her reply is stored and before the turn's `final` frame, one transaction
records her answer (`metadata.action.response = "accept"`) and, under the
instance lock, writes:

1. A sub-session row: `channel = 'activity'`, same `user_id` and `instance_id`
   as the parent.
2. An `on_a_date` event (§5.4) from her reply's `sent_at` for the duration. It
   ends any incident in progress, as an accepted life choice does; a background
   carries on underneath.
3. An `engine.activities` row linking the invitation, the sub-session and the
   event (§9).

The insert re-checks, under the lock, that no date of the instance is in
progress. A turn that loses that check writes no activity; its reply stands.
The invitation message is unique on the activity row, so a re-driven turn
finds the activity it already opened instead of opening a second.

The accepted turn is **not** scored by the per-turn affinity judge
(`eval_skip_reason = "activity"`): it belongs to the date, and settlement reads
it. Everything else in its post-process runs as usual.

The turn's `final` frame carries an `activity` object: `id`, `sub_session_id`,
`duration`, `place`, `content`, `status`, `started_at`, `ends_at`. The async
send route answers before any reply exists, so its clients find the date the
way every client can after the fact: on the invitation row's history receipt
and on the parent's effects (§10). No picture is produced.

## 5. The date

### 5.1 A date turn

Date turns are sent to the sub-session id through the existing send routes,
which admit an `activity` session while its date is in progress. A date turn is
a text turn with these differences:

- **No per-turn affinity judgement, and no rule nudges.** The turn persists as
  a skipped evaluation (`eval_skip_reason = "activity"`) on the parent's
  affinity row with the PDE's rule deltas zeroed (the long-message and
  long-gap nudges a skipped turn otherwise still applies), so the per-turn
  delta route answers for it with zero movement. The same holds for the
  accepted invitation turn. Skipped date turns neither count toward nor break
  a life-choice streak.
- **Never ghosted.** She is with the user. A ghost verdict from the judge is
  replaced by a reply.
- **No life-choice trigger.**
- **Actions work as usual**, each under its own rule: `strip` still needs a
  romance-compatible pair, and a refused `give`, `gift` or physical action
  still drops its line a tier, which already applies when evaluation is
  skipped. The invitation's gate adds nothing for what the date's text may
  contain. A `date` action inside a date is rejected with
  `409 activity_in_progress`.
- **No verbatim memory rows.** The user's line is not embedded into either
  layer.
- **Not counted toward dreaming**, and invisible to the relationship scan
  (§7).

Needs are rated and insight extraction (the fact ledger) runs, as on any text
turn: what the user says about themselves on a date still reaches the profile.

**Relationship state lives on the parent.** Everything the turn pipeline keys by
session other than messages (the affinity row, the newest needs rating,
lingering effects of gives, the recorded scopes) is read and written for the
parent, in both directions: a date turn starts from the state the main chat left,
and the main chat after the date starts from the state the date left.

### 5.2 Prompt

- **History window.** The sub-session's own rows, newest first; slots left over
  are filled with the parent's rows up to and including her accepting reply. The
  date opens knowing what was just said.
- **`[now]`.** `正在经历` reads the event as any incident:
  `和对方约会（约 {span}后结束）`. A line follows it:

  ```
  - 约会：地点「{place}」；安排「{content}」
  ```

- Persona, memory recall, `[shared_memories]` and the rest are those of the
  relationship, unchanged.

### 5.3 What pauses

While a date is in progress:

- A send to any 1:1 text session of the instance is rejected with
  `409 activity_in_progress`. The parent's effects carry the `activity`
  object, sub-session id included.
- `/open` on those sessions returns `null`.
- The events route draws nothing: an incident is in progress.

Voice and group sessions are not paused. The events route and `/open` refuse the
sub-session itself with `409 wrong_channel`.

### 5.4 The event

`on_a_date` (label `和对方约会`) is a non-drawable incident, like
`bathing_together`: no draw rule, refused in the caller's `kinds`, not offline,
not told (her accepting reply already said it), and kept out of the story
director's input. Its length is given by the activity, 1 or 4 hours. It is
per instance, as every event is.

The event row is the only record of when the date starts and ends.

### 5.5 How it ends

- **On the clock.** At `ends_at` the date is over. Nothing is generated and
  nothing is emitted. A send to the sub-session after that is rejected with
  `409 activity_ended`.
- **Early.** `POST /v2/comp/session/{session_id}/activities/{activity_id}/end`
  (the parent's id) sets the event's `ends_at` to now and settles the date in
  the same call.

## 6. Settlement

### 6.1 Triggers

Settlement runs once per activity, from whichever of these comes first:

- the end route, called early or after the clock ran out (a client whose timer
  fires calls it to get the result);
- the next send to a 1:1 text session of the instance after `ends_at`, before
  that turn does anything else.

Both run the same idempotent step behind a claim on the activity row. There is
no sweeper: a date whose user never returns stays unsettled and leaves nothing
in memory until someone comes back.

The end route answers an already settled activity with the stored result. While
another caller holds the claim, the end route and a send both answer
`409 activity_settling`; nothing waits.

Settlement judges the rows that are there. A date turn admitted before the end
and still generating, or revived from the queue later, lands its rows in the
sub-session afterwards; they are kept and not judged.

### 6.2 The judge call

Skipped when the date had no substantive turn (a non-empty user row with at
least one non-empty reply): the activity is marked settled and nothing moves.

Otherwise one call to `[tasks.activity_judge]`, or to `[tasks.affinity_evaluation]`
when that section is absent. The prompt is held by the engine, written in her
first person, under the same `reason` hygiene as the per-turn judge. It reads:

- what the per-turn judge gets about her and the relationship, no more: the
  persona's name and the four banded axis readings (low / mid / high); never
  raw numbers, never the current warmth or patience;
- the invitation and her accepting reply;
- the date's transcript: at most the newest 120 rows of the sub-session, with a
  note of how many earlier rows were left out.

It answers with three parts, each read on its own:

- **The verdict**, in exactly the per-turn shape and read by the same parser:
  a grade 0 to 4 and a direction for each of the four line axes, a grade 1 to 3
  for warmth and for patience, and a `reason`. The grades describe the date as
  a whole.
- **`episode`**: what happened on the date, at most 400 characters, in the
  voice the relationship scan's episodes use.
- **`moments`**: zero or more of `{kind, summary, due_on?}`, `kind` chosen from
  `confession`, `first_intimacy`, `conflict`, `reconciliation`, `promise`,
  `shared_plan`, `user_shared_milestone`; `summary` one sentence of at most 200
  characters. It names no `realm` and no date: the engine supplies both (§7).
  `gift` is not on the list, because an accepted give is journaled at its turn.

A rejected verdict moves nothing, the way a failed per-turn evaluation leaves
the stored state, and the episode and moments are still written. A moment with
an unknown kind or an oversized summary is dropped; a missing or oversized
episode writes none. A failed call settles the date with no movement, no
episode and no moments. The attempt is recorded on the event row and is not
retried.

### 6.3 Folding the verdict into affinity

The verdict goes through the per-turn write pipeline once, weighted:

```
weight = min(turns, cap(duration)) × AFFINITY_ACTIVITY_TURN_SHARE
```

| Knob | Default |
|---|---|
| `AFFINITY_ACTIVITY_TURN_SHARE` | `0.25` |
| `AFFINITY_ACTIVITY_TURN_CAP_SHORT` | `12` |
| `AFFINITY_ACTIVITY_TURN_CAP_MID` | `32` |

`turns` is the number of substantive turns in the sub-session. A `short` date
is therefore worth at most 3 turns of its verdict, a `mid` date at most 8, and a
date with one exchange a quarter of one.

It starts from the stored state at settlement, after time decay: a tier a
refused action dropped during the date stays dropped. Every raw term scales by
`weight`, the cross-line penalty included; negative scores take
`AFFINITY_NEG_FACTOR`; positive scores take the tier decay of the line's tier
at settlement; the threshold gate, the clamp, the demo boost and the
post-separation slowdown apply as on a turn. The endpoint grades overwrite the
stored grades and both endpoints are re-derived.

**A date moves each line by at most one tier.** After the pipeline, each line is
capped at a one-tier shift from where it stood at settlement: it rises no
further than the same relative position in the tier above, and falls no further
than the same relative position in the tier below, which is the landing rule of
a refused action and its mirror. A capped line's two axis deltas are scaled by
one ratio so the line lands on the cap. A tier-1 line can fall to 0; a tier-5
line is bounded by the clamp. What the cap cuts off is discarded, not kept in
`pending_deltas`.

The share exists because the per-turn judge returns grade 0 on most axes on
most turns, while the date's verdict is one reading for all of them. The cap
exists because at full weight a single grade would otherwise carry a line
across several tiers or empty an axis.

### 6.4 The event row

One `companion_affinity_events` row on the parent's affinity,
`event_type = 'activity'`, with the usual deltas, line deltas, label changes and
state snapshots. `context` carries `activity_id`, `turns`, `weight` and the
judge's `reason`, and names any line the cap held. It has no `user_message_id`, is never returned by the per-turn
delta route, and is not a judged turn for life-choice streaks. Its `reason` does
not feed `[emotional_context]`.

A label first reached at settlement writes its `tier_reached` journal row in the
same transaction, as on a turn.

The end route returns the activity and the new affinity state with the
effective deltas, in the shape the life-choice route uses.

## 7. Memory

A date is remembered as a whole, not line by line. While it runs, nothing of it
enters relationship memory: no verbatim rows (§5.1), and the relationship scan
neither counts its turns nor reads its rows, at any later time. The invitation
turn itself is a main-chat turn and is scanned as one.

Settlement writes what the date leaves behind, in its own transaction, when the
date had at least one substantive turn:

| What | Row | From |
|---|---|---|
| The date | one `relationship_episodes` row; `started_at` / `ended_at` are the sub-session's first and last rows | the judge's `episode` |
| That it took place | one `relationship_journal` row, `kind = 'date'`, `summary = 约会：在{place}，{content}`, one per activity | the engine, no LLM |
| Its milestones | one `relationship_journal` row per moment | the judge's `moments` |

- Every journal row of a date is `realm = 'real'`, dated the local date the
  date started. The judge is not asked which realm: the user invited and she
  agreed, which is what `real` means.
- The keys that keep one row where only one belongs apply: a `first_intimacy`
  from a date competes for `first_intimacy:real`, and the earlier row wins. A
  moment with `due_on` returns on that day like any `promise` or `shared_plan`.
  The `date` row does not recur.
- The rows are written without embeddings and picked up by the dreaming
  sweeper, as the engine's other journal rows are.
- The episode is an episode like any other: it shows among the newest three in
  `[shared_memories]` and is folded into the arc by the next rewrite. Settlement
  does not run a rewrite.
- A date produces no profile-layer memory rows. Facts about the user reach the
  profile through insight extraction (§5.1).

None of this needs `[tasks.dreaming_scan]`.

## 8. Back in the main chat

The main chat's history window holds only its own rows: the invitation and her
answer, then whatever comes after the date. What she knows of the date comes
from memory: settlement always completes before the next main-chat turn (§6.1),
so the date's episode and journal rows are in `[shared_memories]` by then.

In the parent session, from the date's end until the first assistant row with
text after it:

- the reply's `[now]` carries `- 刚才：你们刚约会回来（{place}，{content}）`;
- the judge's `[近期事件]` carries `和对方约会（{span}前结束）`.

## 9. Storage

One migration.

- `engine.chat_sessions.channel` admits `'activity'`.
- `companion_affinity_events.event_type` admits `'activity'`.
- `engine.activities`, one row per accepted date:

  | Column | | |
  |---|---|---|
  | `id` | uuid PK | |
  | `session_id` | NOT NULL, FK `chat_sessions` ON DELETE CASCADE | the parent |
  | `sub_session_id` | NOT NULL, FK `chat_sessions` ON DELETE CASCADE, unique | an activity without its messages is nothing |
  | `message_id` | nullable, FK `chat_messages` ON DELETE SET NULL, unique | the invitation; a re-driven turn cannot create a second date |
  | `event_id` | nullable, FK `persona_events` ON DELETE SET NULL | start and end live there |
  | `duration` | text NOT NULL | `short` or `mid` |
  | `place`, `content` | text NOT NULL | |
  | `settle_claimed_at` | timestamptz | |
  | `settled_at` | timestamptz | |

  Every FK column is indexed. No `user_id`, no `instance_id`: the parent
  session determines both. No status column: `ongoing`, `ended` and `settled`
  are derived from the event's `ends_at` and `settled_at`.

A refused invitation has no row here; the user row's `metadata.action` is its
whole record.

## 10. API surface

Additive except for the two new `409`s on send.

- `action.type = "date"` on the send routes; `activity` on the accepted turn's
  `final` frame.
- `POST /v2/comp/session/{session_id}/activities/{activity_id}/end`.
- `GET /v2/comp/session/{session_id}/effects` on the parent gains an optional
  `activity` object while a date is `ongoing` or `ended` (not yet settled), with
  that `status`.
- Both history routes give the invitation's user row an `activity` receipt
  (`id`, `sub_session_id`, `status`, `started_at`, `ends_at`) when it was
  accepted, and serve the sub-session's rows under its own id.
- The per-turn affinity delta route accepts a sub-session id.
- Session listings omit `activity` sessions.
- Archiving a relationship archives its sub-sessions; erasure covers them.

## 11. Without an LLM judge

| | No `[tasks.pde_decision].filter_prompt` |
|---|---|
| The gate | unchanged |
| Accept or refuse | the rule engine accepts whatever passes the gate |
| Date turns | unchanged |
| Settlement | unchanged: the activity judge is the affinity evaluator's kind, not the PDE judge |

## 12. Left to the builder

- How the "relationship state lives on the parent" rule (§5.1) is implemented:
  one resolution helper is expected, not a change per call site.
- Which existing `channel = 'text'` guards admit `activity`. The rule: the send
  path, needs and insight extraction treat it as text; verbatim memory, the
  relationship scan and its counter, life-choice triggers, the events route,
  `/open`, session listings and per-turn affinity evaluation do not.
- The wording of the activity judge prompt and its reply contract, and of the
  transcript marker for the invitation row.
- The claim's staleness window.
- Whether `relationship_journal.kind` needs a constraint change for `date`.
- Error-body shapes beyond the codes named here.

## 13. Follow-ups, not built

- Dates she proposes.
- `long` dates of 1 to 7 days, as a background; gate: bond at tier 3 or above
  and the newest 3 judged turns all warm.
- A closing message from her when a date ends.
- Dates in World Stories.

## 14. Who decided what

**Decided by the product owner**

- The six points of the request: sub-session under a chat session; a date,
  user-proposed only; duration, place and plan; into relationship memory;
  accept or refuse; a judge after the date, none after a refusal.
- Gate by rule, then the PDE judge.
- A separate message stream; the main chat paused during the date.
- Wall clock plus early end.
- No per-turn judgement during the date.
- A refused invitation is an ordinary judged turn with no tier drop.
- `short` and `mid` only. Gate: none for `short`, bond tier 2 for `mid`.
- The verdict is weighted by turns, capped per duration, and a date moves each
  line by at most one tier.
- Milestones come out of dates and are `real`; a date is compressed into one
  memory, so content inside a date does not dilute recall.
- Insight extraction still runs on date turns.
- No chemistry or compatibility gate on the invitation; actions inside a date
  keep their own rules.

**The design's calls**

- One judge call returns the verdict, the episode and the moments; the three
  are read independently. The moment vocabulary is the scan's, without `gift`.
- Date rows never reach the relationship scan, including after the date; the
  rows written at settlement carry no embedding until the sweeper's next pass.

- The invitation is a `date` user action on a chat turn, not a route of its
  own; nothing of `engine.life_choices` is reused as storage.
- The sub-session is a `chat_sessions` row with a new channel; relationship
  state stays on the parent.
- The date is an `on_a_date` event, and that row is the authority on its span.
- Durations are exactly 1 and 4 hours; the length limits on `place` and
  `content`.
- The gate also fails while she is offline; a pending life choice or a BYOC
  session rejects the invitation outright.
- Date turns are never ghosted.
- All 1:1 text sessions of the instance pause, not only the parent.
- Settlement is lazy (end route, or the next send) with no sweeper; a failed
  judge call settles with no movement; rows that land after settlement are
  kept and not judged; a held claim answers `409` instead of waiting.
- Settlement starts from the stored state, so tier drops during the date
  stand; date turns carry no rule nudges; the activity judge gets the per-turn
  judge's inputs (name and bands), not a persona block.
- The weight formula and its three defaults (share 0.25, caps 12 and 32).
- The `date` journal row is templated by the engine and written at settlement.
- The history window of a date is topped up from the parent; the main chat
  gets a `刚才` line instead of the date's rows.
- `on_a_date` stays out of World Stories; the verdict's `reason` stays out of
  `[emotional_context]`.
