# Activities: dates

[English](activities.md) · [中文](activities.zh.md)

The user can ask the persona out. A **date** names a place, a plan and a length. If she says
yes, the two of them step out of the main chat into a conversation of their own and talk there
until it ends. Then the date is judged once, as a whole: the verdict moves the relationship, and
the date becomes one memory with its milestones.

The mechanism is called an **activity**. A date is the only activity, and only the user can
propose one. It runs in a **sub-session**: a chat session of its own (`channel = 'activity'`)
that belongs to the 1:1 text session the invitation was sent in, its **parent**. The date's
messages live in the sub-session; the relationship (affinity, lingering effects, memory) stays
with the parent.

Request and response shapes are in [api-reference.md](api-reference.md), the judge's model
block in [model-config.md](model-config.md#tasksactivity_judge--the-dates-judge). This page is
about how a date works, from the invitation to the memory it leaves.

## At a glance

| | |
|---|---|
| Asked by | the user, with a `date` action on a turn in a 1:1 text session |
| Decided by | an engine gate, then the PDE judge (the rule engine accepts whatever passes the gate) |
| Lasts | 1 hour (`short`) or 4 hours (`mid`) from her accepting reply, or until the user ends it |
| Played in | a sub-session, whose id the turn's `final` frame, the invitation's history row and `GET …/effects` carry |
| Meanwhile | every 1:1 text session of the persona is paused; voice and group sessions are not |
| Judged | once, after it ends, by `[tasks.activity_judge]` |
| Moves | each affinity line by at most one tier, weighted by the date's turns |
| Leaves | one episode, a `date` journal row and its milestones, all `real` |
| Scope | one date at a time per persona instance, across all its sessions |

## The invitation

### Sending it

The invitation is a user action on an ordinary turn in the parent session, sent through the
send routes:

```json
{"type": "date", "duration": "short", "place": "海边的餐厅", "content": "吃一顿大餐"}
```

| Field | Rules |
|---|---|
| `duration` | required: `short` (1 hour) or `mid` (4 hours). The date lasts exactly that long unless the user ends it |
| `place` | required: 1–40 characters after trim |
| `content` | required: what the two of them do, 1–120 characters after trim |

`place` and `content` may not hold line breaks or control characters. A missing field or a value
out of bounds is a `422` with `code: "unprocessable"`; a `duration` that is neither value fails
to parse and is a plain `422`, like any unknown action value. The turn's own text may be empty,
as on any action turn. The action is stored on the user row's `metadata.action`, with the
response once the turn has taken effect.

### Who can send one

The send routes check these before they write anything; a refusal writes nothing.

| Condition | Response |
|---|---|
| The session is a voice or group session | `409 wrong_channel` |
| The session belongs to a BYOC instance | `400 byoc_unsupported` |
| A date of the persona is under way, in any of her sessions | `409 activity_in_progress` |
| Any session of the persona holds a pending, unexpired life choice | `409 choice_pending` |

A date of the persona that has ended but is not settled yet is settled first
([Settlement](#settlement)), then the turn goes on. A send under a `client_msg_id` the session
already holds is a retry, and it is answered from what is stored whatever the date's state: the
invitation's own retry still gets her reply once the date it opened is running.

### The gate

When the turn is driven, before the judge runs, the engine checks a gate on the relationship's
stored affinity after time decay, the same read the turn makes:

| `duration` | Gate |
|---|---|
| `short` | none |
| `mid` | bond at tier 2 or above, on the five-tier ladder |

For either length the gate also fails while she is out of reach: an offline incident under way,
`taking_space` included, or another date of hers that has not ended. The second case is an
invitation queued or sent while the turn that opened the other date was still being generated;
once a date is open, the send routes turn invitations away. The pair's `affinity_scope` and the
chemistry line are not read.

A failed gate refuses the invitation the way the habit roll refuses a give: the judge's
`[用户动作]` line gains `，这次你不会答应`, so the judge writes for that outcome, and the response
in effect is `refuse` whatever it answers. Without the LLM judge, the rule engine's accept
becomes `refuse`.

### Her answer

Past the gate, the PDE judge decides as it does for any action. Its context carries

```
[用户动作] 对方邀请你去约会：地点「{place}」，安排「{content}」，大约 {1|4} 小时
```

beside everything else it reads on a turn (`[近期事件]`, `[需求]`, her persona), and it answers
`accept` or `refuse`; no explicit refusal reads as accept. A background under way, such as a
trip or an illness, does not fail the gate: the judge sees it and decides. The `action_response`
frame carries the answer as on any action turn.

**Refused.** The turn is an ordinary refused action that costs nothing: her reply is written to
the refusal, the per-turn affinity judge scores the turn as usual, and no line drops. Nothing
else is written: no activity, no sub-session, no event. There is no cooldown; the user may ask
again.

**Accepted.** Once her reply is stored, and before the turn's `final` frame, one transaction
stores her answer on the invitation row (`metadata.action.response = "accept"`) and, under the
persona instance's lock, writes:

1. the sub-session: an `engine.chat_sessions` row with `channel = 'activity'` and the parent's
   user and instance;
2. the `on_a_date` event, from her reply's `sent_at`, lasting 1 or 4 hours. It ends an incident
   under way, as an accepted life choice does; a background carries on underneath;
3. the `engine.activities` row that ties the invitation, the sub-session and the event
   together.

The write checks again, under the lock, that no other date of the persona is still on. A turn
that loses that check opens nothing, and her reply stands; so does one whose relationship was
archived meanwhile. A turn that is driven again finds the date it already opened instead of
opening a second.

The turn's `final` frame carries `activity`: `id`, `sub_session_id`, `duration`, `place`,
`content`, `status` (`ongoing`), `started_at` and `ends_at`. Every other turn's `final` omits
it. The async send route answers before any reply exists, so its clients find the date the way
every client can after the fact: on the invitation row's `activity` receipt in history, and on
`GET …/effects`.

The accepted turn is not scored by the per-turn affinity judge, because it belongs to the date,
which is judged once at the end. It is recorded as a skipped evaluation
(`eval_skip_reason = "activity"`) with no rule nudges. The rest of its post-process runs as on
any turn; it is a main-chat turn, so it is written to memory and counted toward dreaming.

An invitation never produces a picture, whatever her answer: the turn is a text reply, and
`image.force` is ignored on it.

## The date

### Date turns

Date turns are sent to `sub_session_id` through the same send routes
(`POST /comp/chat/{session_id}/message/stream` and
`POST /v2/comp/session/{session_id}/message/async`), which take a date's sub-session while the
date is `ongoing`. A date turn is a text turn with these differences:

- **No per-turn judgement.** The turn is recorded on the parent's affinity as a skipped
  evaluation (`eval_skip_reason = "activity"`) with no rule nudges: the long-message and
  long-gap nudges a skipped turn otherwise applies are zeroed too. The per-turn delta route
  shows it moving nothing. The date is judged once, at [settlement](#settlement).
- **Never ghosted.** She is with the user: a ghost verdict becomes a reply.
- **No life choice.** A date turn neither triggers one nor answers one. Skipped turns neither
  count toward nor break a choice's streak.
- **Actions keep their own rules.** A refused `give`, `gift` or physical action still drops its
  line a tier on the parent, and `strip` still needs a romance-compatible pair. A `date` action
  inside a date is `409 activity_in_progress`.
- **No verbatim memory.** The user's line is not embedded into either memory layer.
- **No dreaming.** The turn does not count toward the relationship scan, and the scan never
  reads the sub-session's rows ([Memory](#memory)).

Needs are rated (with the LLM judge) and insight extraction runs, as on any text turn: what the
user says about themselves on a date still reaches the profile.

**The relationship lives on the parent.** Everything the turn pipeline keys by session, other
than the messages, is read and written on the parent: the affinity row and its ghost streak, the
lingering effects of gives (a date's turns count where the parent's do), the scopes the romantic
choices read, and the reasons behind `[emotional_context]`. A date turn starts from the state the
main chat left, and the main chat after the date starts from the state the date left. Needs are
per persona instance already.

### Her prompt on a date

- **History window.** The sub-session's own rows, newest first. Slots they leave are filled with
  the parent's rows up to and including her accepting reply, so the date opens knowing what was
  just said.
- **`[now]`.** `正在经历` reads the date like any incident, and a line with its place and plan
  follows:

  ```
  - 正在经历：和对方约会（约 3 小时后结束）
  - 约会：地点「海边的餐厅」；安排「吃一顿大餐」
  ```

  The `约会` line is there only while the date runs: a turn driven after the end, revived from
  the queue or still generating, is not told that the date is under way.
- **The judge** reads the event in `[近期事件]` like any incident:
  `和对方约会（不到 1 小时前开始，约 3 小时后结束）`.
- Persona, memory recall, `[shared_memories]` and the rest are the relationship's, unchanged.

### What pauses

While a date is `ongoing`:

- A send to any 1:1 text session of the persona is `409 activity_in_progress`, and nothing is
  written. A retry of a send the session already holds is still answered from what is stored.
- `/open` in those sessions returns `greeting: null`.
- The events route draws nothing. This lasts until the date is settled, past its end.
- No life choice is asked, in any session.

A main-chat message sent before the date opened is not affected: it still gets its reply, even
when that reply lands during the date. Voice and group sessions are not paused. The sub-session
itself takes no `/open`, no event draw, no `GET …/effects` and no life-choice answer: each is
`409 wrong_channel` there.

### The event

`on_a_date` (label `和对方约会`) is a non-drawable incident, like `bathing_together`. It has no
draw rule, and a caller's `kinds` that names it is a `422`. It is not offline, she does not tell
it (her accepting reply already did), and the story director never reads it. Its category is
`short`, but its length is the date's, 1 or 4 hours. It is per persona instance, as every event
is, and it shows as `event` on `GET …/effects` while it lasts. The event row is the only record
of when the date starts and ends.

## How it ends

- **On the clock.** At `ends_at` the date is over. Nothing is generated and nothing is sent. A
  send to the sub-session after that, or an image edit there, is `409 activity_ended`.
- **Early.** `POST /v2/comp/session/{session_id}/activities/{activity_id}/end`, with the
  parent's id, moves the event's end to now and settles the date in the same call. Called after
  the clock ran out, it keeps the end the date had and settles it; a client whose timer fires
  calls it to collect the result.

## Settlement

### When it runs

Settlement runs once per date, from whichever of these comes first after the date ends:

- the end route;
- the next send to a 1:1 text session of the persona, before that turn does anything else;
- `/open` in such a session, before it decides whether she speaks.

There is no sweeper. A date nobody comes back to stays unsettled and leaves nothing in memory,
and until it is settled the events route draws nothing for the persona.

All three run the same step behind a claim on the activity row. While another caller holds the
claim, the end route and a send answer `409 activity_settling` and `/open` returns
`greeting: null`; nothing waits. A claim older than two minutes counts as abandoned and is taken
over. The step runs in a task of its own, so it completes even when the request that started it
disconnects, and an error gives the claim back for the next caller. A settled date answers the
end route with the result it stored, and a repeat changes nothing. The send or `/open` that
settles a date waits for the judge call before its own work starts.

Settlement judges the rows that are there. A date turn still being generated when it runs, or
revived from the queue afterwards, keeps its rows in the sub-session; they are not judged.

### The judge call

A date with no substantive turn (a user row with text that has at least one reply with text) is
marked settled, and nothing else happens: no call, no movement, no memory.

Otherwise the engine makes one call to `[tasks.activity_judge]`; without that block the call
runs on `[tasks.affinity_evaluation]`'s model chain and parameters, and `max_tokens` is at least
1200 either way ([model-config.md](model-config.md#tasksactivity_judge--the-dates-judge)). The
prompt is the engine's, written in her first person under the same `reason` rules as the
per-turn judge. It reads:

- what the per-turn judge reads about her and the relationship, no more: her name and the four
  line axes as bands (`低` / `中` / `高`), never raw numbers, never the current warmth or
  patience;
- the invitation and her accepting reply;
- the date: the newest 120 rows of the sub-session, with a note of how many earlier rows were
  left out.

The reply is one JSON object with three parts, each read on its own:

- **The verdict**, in the per-turn shape and read by the same parser: a grade 0 to 4 and a
  direction for each of `trust`, `intrigue`, `intimacy` and `tension`, a level 1 to 3 for
  `warmth` and for `patience`, and a `reason`. The grades describe the date as a whole.
- **`episode`**: what happened on the date, at most 400 characters, in the third-person voice of
  the scan's episodes.
- **`moments`**: zero or more `{kind, summary, due_on}`. `kind` is one of `confession`,
  `first_intimacy`, `conflict`, `reconciliation`, `promise`, `shared_plan` and
  `user_shared_milestone`; `summary` is one sentence of at most 200 characters; `due_on`
  (`YYYY-MM-DD`) is read on a `promise` or a `shared_plan` only. A moment names no realm and no
  date: the engine supplies both. `gift` is not on the list, because an accepted give is
  journaled at its own turn.

A verdict that does not parse moves nothing, as a failed per-turn evaluation leaves the stored
state, and the episode and moments are still written. A moment of any other kind, or with a
blank or oversized summary, is dropped; a `due_on` that is not a date is dropped and the moment
kept. A missing, blank or oversized episode writes none. A failed call, or one that runs past 45
seconds, settles the date with no movement, no episode and no moments; its failed attempts are
recorded on the date's affinity event, and it is not retried.

### What it moves

The verdict goes through the per-turn write pipeline once, weighted:

```
weight = min(turns, cap(duration)) × AFFINITY_ACTIVITY_TURN_SHARE
```

| Env var | Default | Meaning |
|---|---|---|
| `AFFINITY_ACTIVITY_TURN_SHARE` | `0.25` | the weight of one substantive turn of a date |
| `AFFINITY_ACTIVITY_TURN_CAP_SHORT` | `12` | the most turns that count on a `short` date |
| `AFFINITY_ACTIVITY_TURN_CAP_MID` | `32` | the most turns that count on a `mid` date |

`turns` is the number of substantive turns in the sub-session. At the defaults a `short` date is
worth at most 3 turns of its verdict, a `mid` date at most 8, and a date with one exchange a
quarter of one. A cap of `0` makes a date of that length move nothing. An invalid value keeps
its default and logs a warning.

The write starts from the stored state at settlement, after time decay, so a tier a refused
action dropped during the date stays dropped. Every raw term scales by `weight`, the cross-line
penalty included; negative scores take `AFFINITY_NEG_FACTOR`; positive scores take the tier
decay of the line's tier at settlement; the threshold gate, the clamp, the demo boost and the
slowdown after a separation apply as on a turn, and there are no rule nudges. The verdict's
`warmth` and `patience` levels overwrite the stored ones, and both endpoints are derived again.
Each stage is described in [affinity-model.md](affinity-model.md#write-pipeline).

**A date moves each line by at most one tier.** After the pipeline, each line is held to a
one-tier shift from where it stood at settlement. It rises no further than the same relative
position in the tier above, and falls no further than the same relative position in the tier
below: the landing rule of a refused action, and its mirror. A capped line's two axes are pulled
back along their own deltas by one ratio, so the line lands on the cap. A tier-1 line can fall to
`0`, and a tier-5 line is bounded by the clamp. What the cap cuts off is gone: the capped line
keeps nothing in `pending_deltas`.

The share is there because the per-turn judge gives grade 0 on most axes on most turns, while
the date's verdict is one reading of all of them; the cap, because at full weight a single grade
could carry a line across several tiers.

**The event row.** One `companion_affinity_events` row on the parent's affinity,
`event_type = 'activity'`, with the usual deltas, line deltas, label changes and state
snapshots, and the judge call's failed attempts when there were any. Its `context` carries
`activity_id`, `turns`, `weight`, the judge's `reason`, and `capped`, the lines the cap held
(absent when none), beside the usual grades, gate balance and endpoint audit. It has no
`user_message_id`: the per-turn delta route never returns it, it is not a judged turn for a life
choice's streak, and its `reason` never reaches `[emotional_context]`. A label first reached at
settlement writes its `tier_reached` journal row in the same transaction, as on a turn. A parent
with no affinity row gets no event.

The end route returns the date (`status: "settled"`), the relationship after it, and the deltas
its event committed, `null` when no event was written.

## Memory

A date is remembered as a whole, not line by line. While it runs, nothing of it enters
relationship memory: its turns write no raw-turn rows and do not count toward dreaming, and the
scan never reads the sub-session's rows, then or later. The invitation turn is a main-chat turn
and is remembered as one.

When the date had at least one substantive turn, settlement writes what it leaves, in the
transaction that writes its affinity event:

| What | Row | From |
|---|---|---|
| The date | one `relationship_episodes` row; `started_at` / `ended_at` are the sub-session's first and last rows | the judge's `episode` |
| That it took place | one `relationship_journal` row, `kind = 'date'`, summary `约会：在{place}，{content}`, pointing at the invitation | the engine, no LLM; written even when the judge call failed |
| Its milestones | one `relationship_journal` row per moment | the judge's `moments` |

- Every journal row of a date is `realm = 'real'`, dated the user's local day the date started
  (in the timezone the relationship scan keeps; UTC without one). The judge is not asked: the
  user invited and she agreed, which is what `real` means.
- The keys that keep one row where only one belongs still apply: a `first_intimacy` from a date
  competes for the relationship's one real first intimacy, and the earlier row wins. A moment
  with `due_on` comes back on that day like any `promise` or `shared_plan`; the `date` row does
  not come back.
- The rows are written without a vector and embedded later, as the engine's other journal rows
  are ([memory-layers.md](memory-layers.md#the-journal)).
- The episode is an episode like any other: it is among the latest three in
  `[shared_memories]`, and a later arc rewrite folds it in. Settlement runs no rewrite.
- A date writes no profile-layer memory. What the user says about themselves on a date reaches
  the profile through insight extraction.

None of this needs `[tasks.dreaming_scan]`.

## Back in the main chat

The main chat's history window holds only its own rows: the invitation and her answer, then
whatever follows the date. What she knows of the date comes from memory. A send or `/open`
settles the date before the main chat says anything, so its episode and journal rows are in
`[shared_memories]` by then.

In the parent session, from the date's end until her first message with text after it:

- the reply's `[now]` carries `- 刚才：你们刚约会回来（{place}，{content}）`, after any offline
  or false-alarm text on the same line;
- the judge's `[近期事件]` ends with `和对方约会（{span}前结束）`.

`/open` greetings and event messages carry the same `刚才` line.

## Storage

Migration `0085`:

- `engine.chat_sessions.channel` admits `'activity'`, and
  `engine.companion_affinity_events.event_type` admits `'activity'`. Both widened checks are
  added `NOT VALID`: existing rows satisfy the narrower checks they replace, so neither table is
  scanned under the migration's lock.
- `engine.activities` holds one row per accepted date:

  | Column | Type | |
  |---|---|---|
  | `id` | uuid, primary key | |
  | `session_id` | not null, FK `chat_sessions`, `ON DELETE CASCADE` | the parent |
  | `sub_session_id` | not null, unique, FK `chat_sessions`, `ON DELETE CASCADE` | where the date is played |
  | `message_id` | unique, FK `chat_messages`, `ON DELETE SET NULL` | the invitation; a turn driven again cannot open a second date |
  | `event_id` | FK `persona_events`, `ON DELETE SET NULL` | the `on_a_date` event, which holds the start and the end |
  | `duration` | text, not null | `short` or `mid` |
  | `place`, `content` | text, not null | |
  | `settle_claimed_at` | timestamptz | the settlement claim |
  | `settled_at` | timestamptz | |
  | `created_at` | timestamptz, not null | |

The row stores no user and no instance (the parent determines both) and no status: `ongoing`,
`ended` and `settled` derive from the event's `ends_at` and from `settled_at`. A refused
invitation has no row; the user row's `metadata.action` is its whole record. The date's messages
are ordinary `engine.chat_messages` rows of the sub-session.

## API surface

Everything is additive except the new `409`s on the send routes.

| Where | What |
|---|---|
| [the send routes](api-reference.md#post-compchatsession_idmessagestream) | `action.type = "date"`; a date's sub-session id while the date runs; `409 activity_in_progress`, `activity_ended`, `activity_settling` and `choice_pending` |
| the stream's `final` frame | `activity`, on the turn whose invitation she accepted |
| [`POST /v2/comp/session/{session_id}/activities/{activity_id}/end`](api-reference.md#post-v2compsessionsession_idactivitiesactivity_idend) | ends a date early, or collects one that ran out, and settles it |
| [`GET /v2/comp/session/{session_id}/effects`](api-reference.md#get-v2compsessionsession_ideffects) | `activity` while a date of the persona is `ongoing`, or `ended` and not yet settled |
| both history routes and the start bundle | an `activity` receipt on the invitation's user row; the sub-session's rows under its own id |
| [`GET /bff/v1/comp/affinity/{session_id}/event`](api-reference.md#get-bffv1compaffinitysession_idevent) | takes a sub-session id and answers with the relationship's latest turn |
| [image edit](api-reference.md#post-v2compsessionsession_idmessagemessage_idimageedit) | edits a picture from a date while it runs; `409 activity_ended` after |
| `GET /comp/chat/{user_id}/sessions` | leaves sub-sessions out |
| `/open`, `/events`, `/effects`, the life-choice route | `409 wrong_channel` on a sub-session |

`activity` has one shape wherever it appears: `id`, `sub_session_id`, `duration`, `place`,
`content`, `status` (`ongoing`, `ended` or `settled`, as of the response), `started_at` and
`ends_at` (both `null` once the event row is gone).

## Archive and erasure

Archiving the relationship (`DELETE /comp/instance/{instance_id}/sessions`) archives its
sub-sessions with its other sessions and ends a date under way. A date whose relationship is
archived is never settled, and one being settled when the archive lands is marked settled with
nothing else written. A date's episode and journal rows are relationship rows: the archive
deletes them, and erasure covers them. The sub-session's messages and the `engine.activities`
row are chat data, which neither touches.

## Without the LLM judge

| | Without `[tasks.pde_decision].filter_prompt` |
|---|---|
| The gate | unchanged |
| Accept or refuse | the rule engine accepts whatever passes the gate |
| Date turns | unchanged, with no needs rating |
| Settlement | unchanged: the activity judge belongs with the affinity evaluator, not the PDE judge |

## Code pointers

- `crates/eros-engine-core/src/types.rs`: `UserAction::Date`, `DateDuration`.
- `crates/eros-engine-core/src/pde.rs`: `date_ruled_out`, the gate.
- `crates/eros-engine-core/src/events.rs`: `EventKind::OnADate`.
- `crates/eros-engine-core/src/affinity.rs`: `AffinityTuning::activity_weight`,
  `grade_weighted`, `Affinity::cap_line_shift`, `tier_raise_target`.
- `crates/eros-engine-server/src/routes/companion_stream.rs`: the `date` action and its
  validation; `resolve_send_session`, which lets a sub-session through to the send routes.
- `crates/eros-engine-server/src/pipeline/activity.rs`: `admit_send` (who may send),
  `turn_session` (the parent's state for a date turn), `settle`, the judge call and its parser.
- `crates/eros-engine-server/src/pipeline/stream.rs::run_stream`: the gate at drive time and the
  forced refusal, opening the date before `final`, no ghosting and no life-choice conversion on
  a date turn, the `[now]` lines.
- `crates/eros-engine-server/src/pipeline/post_process.rs`: the `activity` skip, no raw-turn
  row and no dreaming count on a date turn.
- `crates/eros-engine-server/src/pipeline/handlers.rs`: the date's history window.
- `crates/eros-engine-server/src/routes/activities.rs`: the end route.
- `crates/eros-engine-server/src/routes/session_open.rs`, `session_events.rs`,
  `session_effects.rs`: what pauses, and `activity` on the effects.
- `crates/eros-engine-server/src/prompt.rs`: `activity_judge_system_prompt`,
  `activity_judge_user_payload`, `action_subject`, `date_line`, `just_now_text`,
  `events_judge_text`.
- `crates/eros-engine-store/src/activity.rs::ActivityRepo`: `accept`, `date_on`,
  `unsettled_for_instance`, `returned_from`, `end_early`, `claim_settlement`, `transcript`,
  `substantive_turns`.
- `crates/eros-engine-store/src/affinity.rs::AffinityRepo::persist_activity_in_tx`: the weighted,
  capped write and its event row.
- `crates/eros-engine-store/src/journal.rs`: `date`, `date_moment`;
  `episode.rs::EpisodeRepo::insert_in_tx`.
- `crates/eros-engine-store/src/session_archive.rs`: the archive ends a date and archives its
  sub-session.
- `crates/eros-engine-llm/src/model_config.rs`: `resolve_activity_judge`,
  `ACTIVITY_JUDGE_MIN_TOKENS`.
- `crates/eros-engine-server/src/state.rs`: the three `AFFINITY_ACTIVITY_*` knobs.
- `crates/eros-engine-store/migrations/0085_activities.sql`.
- Design spec:
  [2026-11-12-activity-sub-session-design.md](superpowers/specs/2026-11-12-activity-sub-session-design.md).
