# Group chat — Design

- **Date:** 2026-10-12
- **Status:** Draft — ready for review
- **Type:** Engine change. One migration (`0068_group_chat.sql`): one new
  table, three new columns, three new constraints. The HTTP API gains a
  `/v2/comp/group*` route family and three SSE frames. Existing routes keep
  their shape. They gain `409 wrong_channel` refusals for group sessions, and
  the per-user session list leaves group sessions out. `eros-engine-llm`'s
  public `DeltaChunk` gains a field. That is a breaking change for any code
  that builds the struct literally.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine`: session storage, a new group turn pipeline, a
  new `[tasks.group_director]` LLM task, the OpenRouter stream parser.

## 1. Motivation

Today a user talks to each persona alone. Group chat puts the user and up to
three personas in one conversation. The user says something, and the
personas who have a reason to answer do so in turn, each in their own voice.
Each one carries the relationship it already has with the user. A persona the
user is close to sounds close. Later speakers hear what earlier speakers just
said, so personas pick up each other's lines.

A group does not build its own relationship. Each persona's affinity toward
the user lives in that persona's 1:1 session and is only read here. When the
1:1 relationship moves, the persona sounds different in the group from the
next turn on. What is said in the group does reach each persona's memory, so
a persona can remember it later in the 1:1 chat.

## 2. Principles applied

1. **One source per fact.** A group member *is* an existing 1:1 session.
   - Affinity is read from that session's `companion_affinity` row and never
     copied, so a 1:1 change shows in the group with no sync step.
   - A group stores only what nothing else can explain: its membership, its
     title, and who spoke each line.
2. **The judge picks, the engine counts.** The director returns discrete
   choices: who speaks, in what order, with which action. Affinity reaches it
   as tier labels, never numbers. The engine owns every gate, such as image
   availability.
3. **Reuse the 1:1 machinery; add channel guards both ways.** A group is a
   `chat_sessions` row with `channel = 'group'`. That gives it the turn
   queue, history cursors, read markers, image-request recovery and
   `llm_generations` attribution as they are. Each 1:1 route refuses a group
   session and the group routes refuse a 1:1 session, as text and voice
   already refuse each other.
4. **Default open, leave readings.** Every non-affinity post-process step
   runs on group turns, and the director's reasoning is kept in its audit
   row. Narrowing comes later and is based on what these show.

## 3. Storage — migration `0068_group_chat.sql`

### 3.1 `engine.chat_sessions`: a group is a session row

```sql
ALTER TABLE engine.chat_sessions
    DROP CONSTRAINT chat_sessions_channel_check,
    ADD  CONSTRAINT chat_sessions_channel_check
         CHECK (channel IN ('text', 'voice', 'group')),
    ADD  CONSTRAINT chat_sessions_group_no_instance
         CHECK (channel <> 'group' OR instance_id IS NULL),
    ADD  COLUMN title TEXT,
    ADD  CONSTRAINT chat_sessions_title_group_only
         CHECK (title IS NULL
                OR (channel = 'group' AND char_length(btrim(title)) BETWEEN 1 AND 64));
```

- **Row columns.** `user_id` is the group's owner, `instance_id` is NULL and
  `channel` is `'group'`.
  - `group_no_instance` constrains one direction only. Pre-existing non-group
    rows are not checked for the converse.
- **`title`** is nullable: an untitled group is still a group, and a client
  can render one from member names. It is set at creation and changed or
  cleared by `PATCH`.
  - The title is user-written text. The director and the speakers read it the
    way they read user content.
- **Dissolving a group** sets `archived = true`, which is the existing soft
  delete. Member sessions are untouched.

### 3.2 `engine.chat_session_members`

```sql
CREATE TABLE engine.chat_session_members (
    group_session_id  UUID NOT NULL REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    member_session_id UUID NOT NULL REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    joined_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (group_session_id, member_session_id)
);
CREATE INDEX idx_chat_session_members_member
    ON engine.chat_session_members (member_session_id);
```

- **Both foreign keys CASCADE.** A membership row has no meaning without
  either session.
  - The PK prefix covers `group_session_id`. `member_session_id` has its own
    index.
- **A kick is a `DELETE`.** A departed persona's past lines stay attributed
  through `chat_messages.speaker_session_id` (§3.3), not through this row. A
  re-add inserts a fresh row.
- **`joined_at`** sets the member order. The rule fallback (§5.3) uses that
  order.
- **Checks done in the add transaction, not by constraints.** The add first
  takes `SELECT … FOR UPDATE` on the group row. A CHECK cannot count rows
  across a table. While holding the lock it checks:
  - the cap of three active members;
  - that the member session belongs to the same user;
  - that the member session has `channel = 'text'` and is not archived;
  - that no other member of the group has the same `instance_id`.
- **Active member** means the member session is not archived. Archiving a
  relationship removes its sessions from every group (§7.3), so in practice
  every membership row is active.

### 3.3 `engine.chat_messages.speaker_session_id`

```sql
ALTER TABLE engine.chat_messages
    ADD COLUMN speaker_session_id UUID
        REFERENCES engine.chat_sessions(id) ON DELETE SET NULL,
    ADD CONSTRAINT chat_messages_speaker_assistant_only
        CHECK (speaker_session_id IS NULL OR role = 'assistant');
CREATE INDEX idx_chat_messages_speaker
    ON engine.chat_messages (speaker_session_id) WHERE speaker_session_id IS NOT NULL;
```

- **What it points at.** The column points at the member *session*, not the
  instance: the instance follows from the session, and affinity is keyed by
  session.
- **Nullable, so `SET NULL`.** If a member session is hard-deleted, its group
  lines remain but lose their speaker.
- **Which rows set it.** User rows in a group leave it NULL. Every assistant
  row in a group carries it; the application guarantees that.
- **`chat_messages.channel`** stays NULL on group rows. Those rows live in the
  group session, and no 1:1 read ever sees them.

### 3.4 Existing tables, keyed by the group session

| Table | Group use |
|---|---|
| `chat_turn_queue` | One row per group turn, `session_id` = group. The send route inserts the row already `claimed`, so the worker's per-session claim guard does not serialize a group's turns: a message sent during a relay starts a second relay at once, and the two can interleave. |
| `llm_generations` | The director call and every speaker call carry `session_id` = group. `task` tells them apart. The speaker follows from `chat_messages.generation_id` → `speaker_session_id`. |
| `companion_decision_events` | One row per group turn for the director verdict (§5.4). |
| `companion_memories` | Relationship rows written from a group carry `session_id` = group (where it was said) and `instance_id` = the member's instance. `archive_relationship` deletes by `(user_id, instance_id)`, so group-sourced rows go with the relationship. |

## 4. Routes

Every route requires the `AuthUser` to own the group session. Anything else
returns `404`, as the 1:1 routes do.

### 4.1 Management

| Method | Path | Body / effect |
|---|---|---|
| `POST` | `/v2/comp/group` | `{title?, member_session_ids: [1..=3]}` → group DTO |
| `GET` | `/v2/comp/groups` | caller's non-archived groups, newest `last_active_at` first |
| `GET` | `/v2/comp/group/{group_id}` | group DTO |
| `PATCH` | `/v2/comp/group/{group_id}` | `{title}`; `null` clears |
| `POST` | `/v2/comp/group/{group_id}/members` | `{member_session_id}` |
| `DELETE` | `/v2/comp/group/{group_id}/members/{member_session_id}` | kick |
| `DELETE` | `/v2/comp/group/{group_id}` | `archived = true` |

Group DTO:

```json
{"group_id": "…", "title": "周末爬山",
 "members": [{"member_session_id": "…", "instance_id": "…", "joined_at": "…"}],
 "created_at": "…", "last_active_at": "…"}
```

Add-member and create refusals:

| Condition | Response |
|---|---|
| Group already has three active members | `409 group_full` |
| Member session not the caller's | `404` |
| Member session is not `channel = 'text'` | `409 wrong_channel` |
| Member session archived | `409 session_archived` |
| Another member has the same instance | `409 instance_already_member` |

Dissolving a group keeps the memory rows it produced. They belong to the
members' relationships, not to the group.

### 4.2 Send

`POST /v2/comp/group/{group_id}/message/stream` → SSE.

- **Request body:** `content`, `client_msg_id`, `tier`, `prompt_traits`,
  `user_timezone` / `user_country` / `user_region`, the 1:1 `image` block,
  and an optional `mentions: [member_session_id]` for a client-side @.
  - Mentions must name active members. An unknown id returns `422`.
  - `image` means the same as on the 1:1 route: its presence, together with
    a configured `[tasks.chat_image_prompt_compose]`, makes images available
    this turn. `image.force = true` returns `400 invalid_payload`, because a
    group turn has no single persona to force.
- **Stream only.** The async route refuses group sessions (§7.1).
- **Refusals:**
  - `409 group_empty` when the group has no active member.
  - `409 wrong_channel` when the session is not a group.
  - `400 invalid_payload` for a `byok` block, as on the async route (§10).
- **Limits.** Group streams count toward the per-user in-flight stream cap.

### 4.3 Reused as is

The following take a group session id unchanged:

- `GET /comp/chat/{session_id}/history`;
- `GET /bff/v1/comp/chat/{session_id}/history`, including the
  before/after/around cursors;
- `POST /comp/chat/{session_id}/read`;
- `GET /comp/chat/{session_id}/messages/{message_id}/image-request`.

History DTO entries gain `speaker_session_id` (omitted when NULL). A client
resolves a departed speaker's name through session → instance.

## 5. The group turn

### 5.1 Persist and enqueue

The route reuses `upsert_user_message_in_tx` and
`enqueue_user_message_claimed` with the group session id. That brings
`client_msg_id` dedupe and replay as they are; a resend while the turn's queue
row is still `claimed` gets `409 duplicate_in_progress`, even once an earlier
speaker's row has landed. Turns of one group are not serialized (§3.4).
The turn runs detached, and the SSE body taps it, as on the 1:1 stream route.

### 5.2 Load members

The turn reads the active members ordered by `joined_at`. For each member it
loads the items below; a member whose persona instance is inactive is not
seated that turn.

- **Persona:** `PersonaRepo::load_companion(instance_id)`.
- **Affinity:** `AffinityRepo::load(member_session_id)`, then
  `apply_time_decay` and endpoint refresh in memory.
  - **Read only.** A member with no affinity row yet gets the default
    `Affinity` in memory and no row is created. The turn never calls
    `load_or_create` and never writes a member's affinity row.
- **Derived values:** the Bond and Chemistry tier labels and the intimacy
  rung (`Affinity::intimacy_rung`). The rung is director context, as it is
  judge context in 1:1. It is not an engine gate.

Image availability is one value for the whole turn, computed as in 1:1: the
request carries `image` and `[tasks.chat_image_prompt_compose]` is
configured.

This member snapshot is fixed for the turn. A kick that lands mid-turn can let
the kicked member finish this turn. That window is accepted.

### 5.3 Director — `[tasks.group_director]`

**Call.** A streamed chat/completions call (`execute_stream_as`) over the
task's model chain.
- Each attempt is bounded by the chat stream's existing per-attempt constants
  (`STREAM_OPEN_TIMEOUT`, `STREAM_TOTAL_TIMEOUT`).
- OpenRouter's processing keepalives count as stream bytes, so a model that
  reasons for a long time is not cut off.
- Structured output is on by default (`structured_output`).

**System prompt** = `filter_prompt`: the deployer's direction style and
priorities.

**User message.** Engine-built and not configurable, because the engine
parses what comes back:

- `[群聊]` the title, if set;
- `[成员]` one line per member: handle (`m1`, `m2`, `m3` in `joined_at`
  order), name, one-line personality (`genome.tip_personality`, the field
  the world-town roster uses), Bond label, Chemistry label, `亲密度=第 N 档`;
- `[图片能力] 本轮可发图=是/否`, the turn's image availability;
- `[最近群聊]` the last `HISTORY_WINDOW` group rows, each prefixed with its
  speaker's name or 「用户」;
- `[用户点名]` the names from `mentions`, when present;
- the user's message.

**Output:**

```json
{"speakers": [{"member": "m2", "action": "reply_text_image",
               "inner_state": "…", "reply_mode": null, "clothing": null}],
 "reason": "…"}
```

- `member` is an enum over the handles that are present.
- `action` is one of `reply_text` | `reply_image` | `reply_text_image`.
- `inner_state`, `reply_mode` and `clothing` are optional. Their semantics
  are the same as the 1:1 judge's.

**Normalization** (pure, in `eros-engine-core`):

- an unknown handle is dropped;
- a repeated handle keeps its first occurrence;
- an image action on a turn without image availability degrades to
  `reply_text`, following the 1:1 `guard_action` rule that an action can only
  go down;
- a list that is empty after normalization counts as a director failure.

**Rule fallback.** Every seated member speaks `reply_text`, in `joined_at`
order. It applies when:

- the director fails, times out, returns unparseable output, or returns an
  empty list;
- `[tasks.group_director]` is absent. This is the same shape as the rule
  engine standing in for an unconfigured PDE.

**Reasoning.** Turned on per deployment by a body rule; there is no new key:

```toml
[[providers.openrouter.body]]
tasks  = ["group_director"]
params = { reasoning = { effort = "medium" } }
```

- Reasoning tokens stream to the client as `director_reasoning` frames (§6).
- Content deltas are buffered and parsed when the stream ends.
- If the call falls back to another model, that model's reasoning keeps
  appending. Reasoning is display-only and decides nothing.
- `reasoning = { exclude = true }` means the model reasons but returns no
  reasoning text, so no frames are sent.
- The reasoning text is not passed to speakers. The director reaches a
  speaker only through that speaker's entry.

**Sizing.** Reasoning tokens count against `max_tokens`. The example sets
`max_tokens = 4000` and limits reasoning through the body rule
(`reasoning.effort` or `reasoning.max_tokens`).

### 5.4 Director audit

The turn writes one `companion_decision_events` row:

| Column | Value |
|---|---|
| `session_id` | the group |
| `message_id` | the user row |
| `status` | the PDE's set: `ok` / `empty` / `parse_error` / `upstream_error` / `gateway_error` |
| `action` / `proposed_action` | NULL |
| `payload` | the parsed output, plus a `reasoning` key holding the full reasoning text (NULL when the model returned none) |
| `inputs` | the roster as the director saw it (handles, labels, rungs), the turn's image availability, and the mentions |
| `generation_id` | the director's call |

No row is written when the task is absent.

### 5.5 Speaker relay

The speakers run sequentially. Each one finishes, and its row is persisted,
before the next one starts.

- **Plan.** Built through the same `pde::plan_for` path the 1:1 judge uses,
  with this speaker's director entry in place of the judge verdict and no
  action response. `TurnNudges` are rolled from this member's affinity.
- **Request.** `build_reply_request`, loaded with this member's keys:
  - affinity and `feeling_clause` (member session);
  - relationship-layer recall `(user_id, instance_id)` and profile-layer
    recall `user_id`, queried with the user's message;
  - `human_insights`, `character_insights`, world and stories context;
  - items and wardrobe; clothing comes from the director entry;
  - `[now]` / holiday from `user_locale`;
  - `prompt_traits` filtered by the request tier.
- **`[群聊]` block** added to the reply prompt:
  - the title;
  - the other seated members' names and one-line personalities;
  - a fixed rule: say only your own lines, never speak for anyone else, and
    write no name prefix.
- **History.** The last `HISTORY_WINDOW` group rows at or before the user
  row, plus the assistant rows earlier speakers persisted in this same turn
  (`user_message_id` = this turn's user row). A newer user row that arrived
  mid-turn is not included.
  - This speaker's own lines go in as `assistant`.
  - The user's and other personas' lines go in as `user`, prefixed
    「名字：」 or 「用户：」.
  - The 1:1 noise strip and echo cancellation apply. The 1:1 window ladder
    (`select_window`) does not apply: it keeps rows around the previous user
    turn and would drop this turn's earlier speakers.
- **Call.** `chat_companion` with its tier routing, retry chain, fallback
  phrase, output filter and multi-bubble `continues_from` chaining, all
  unchanged. Every assistant row this speaker writes carries
  `speaker_session_id`.
- **Image turns.** The composer is spawned before the chat call, with this
  speaker's context, as in 1:1. The `image_request` frame follows the
  speaker's `done`.
  - The director entry carries no `image_ref` or `aspect_ratio`, so the 1:1
    defaults apply.

### 5.6 Post-process

Spawned once after the last speaker. Nothing in it blocks the stream.

- **Not run:** affinity evaluation, `affinity_summary` (the feeling clause),
  ghost-streak updates. No `companion_affinity` row is read for writing or
  written.
- **Memory.** The user's message is embedded once.
  - Profile layer: one row, `instance_id` NULL, content raw.
  - Relationship layer: one row for **every seated member**, whether or not
    it spoke, because every member was present. A member whose persona
    instance is inactive is not seated that turn.
  - Each relationship row starts with the fixed marker 「（群聊）」, so a 1:1
    recall knows where the line was said.
  - Each member write rechecks that the member session is not archived,
    matching the 1:1 recheck.
- **`human_insights`:** once per turn. The input is the user's message plus
  this turn's replies, prefixed with speaker names.
- **`character_insights` and `user_insights`:** once per speaker, from the
  user's message and that speaker's reply, keyed by that speaker's instance.
  Event rows carry `session_id` = group.
- **Cost.** A turn with N speakers makes 1 + 2N extraction calls, against
  about five post-process calls per 1:1 turn. Narrowing would start with
  `user_insights`, which is experimental and not injected anywhere.
- **Session timestamps.** The group session's `last_active_at` moves. Member
  sessions' `last_active_at` does not.

### 5.7 Failure

| Failure | Behavior |
|---|---|
| Director fails / empty / absent | Rule fallback (§5.3); audit row records the status |
| One speaker's chain exhausted | `meta → done{truncated:true} → speaker_error`; the attempt row is persisted, as on 1:1, and the relay continues with the next speaker |
| A speaker's request cannot be built, or its image row cannot be persisted | A bare `speaker_error`; the relay continues with the next speaker |
| Composer fails | Text half lands; no `image_request` (as in 1:1) |
| `CHAT_QUEUE_GEN_TIMEOUT_SECS` expires | One budget covers director and all speakers. Speakers who finished stay persisted, the rest are skipped, and the terminal frame is `error generation_timeout`. No post-process runs, so the user's line reaches no member's memory for that turn. |
| Settle | `settle_turn` unchanged. If any assistant row landed (`turn_already_served`) the turn is `done`. A `system_error` notice, which carries no speaker, is written only when none did. |
| Client disconnect | The turn completes and persists; the client reads history |
| Process dies mid-turn | The reaper releases the claim, and the chat-queue worker settles a group turn as unservable: failed with a `system_error` notice. Group turns are never re-driven in the background. |

The relay does not stop at a failed speaker: the other members are
independent people and still answer. The stream path's rule of a single
attempt with no retry already applies to the whole turn, and here it also
applies to each speaker.

## 6. SSE frames

The group route emits:

```
director_reasoning*                       (0..n, only with reasoning on)
cast
( meta → delta* → done → image_request?  |  meta → done{truncated:true} → speaker_error  |  speaker_error )  × speakers
final
```

A failed speaker emits `meta → done{truncated:true}` for each attempt and then
`speaker_error` when its chain is exhausted, or a bare `speaker_error` when it
leaves no row (its request could not be built, or its image row could not be
persisted). A terminal `error` can replace anything after the
first frame.

| Frame | Payload |
|---|---|
| `director_reasoning` *(new)* | `{content}` — reasoning delta |
| `cast` *(new)* | `{speakers: [{member_session_id, action_type}]}`. `action_type` uses the `meta` wire values `reply` / `reply_image` / `reply_text_image`. Emitted after the fallback too, so a client can always show who is about to speak. |
| `meta` | gains `speaker_session_id` on group turns. The field is omitted on 1:1 turns, so 1:1 frames are byte-identical. |
| `delta` / `done` / `image_request` | unchanged, keyed by `message_id` |
| `speaker_error` *(new)* | `{speaker_session_id, code, message}`; non-terminal. After an exhausted chain it follows that speaker's `meta` and truncated `done`; a speaker that failed without leaving a row emits it alone. It is a new frame rather than an `error` variant, because clients treat `error` as end of turn. |
| `final` | Same shape as 1:1. Counters are summed over speakers; `filtered` / `prompt_injected` are true if any speaker's was. |

The engine only emits frames. How reasoning is shown (full, collapsed, or as
a "thinking" indicator) is the client's decision.

## 7. Guards on existing paths

### 7.1 Routes that refuse a group session with `409 wrong_channel`

- the 1:1 stream send route: `resolve_text_turn` changes
  `channel == "voice"` to `channel != "text"`;
- the async send route;
- `/v2/comp/session/{id}/open`;
- `/v2/comp/session/{id}/effects`;
- `/v2/comp/session/{id}/message/{mid}/reaction`;
- `/v2/comp/session/{id}/message/{mid}/image/edit`.

The voice routes already require `channel = 'voice'`. The chat-queue
worker's `drive_turn` settles a group turn as unservable instead of driving
it (§5.7).

### 7.2 Reads that exclude group sessions

- `ChatRepo::list_sessions`, which backs `/comp/chat/{user_id}/sessions`,
  adds `channel <> 'group'`. Groups are listed by `/v2/comp/groups`.
  - `/comp/instance/{instance_id}/sessions` needs nothing: a NULL
    `instance_id` never matches.
- The dreaming sweeper's pick adds `channel <> 'group'`.
- The stories activity gate matches sessions on `instance_id`, so it never
  sees a group. A persona active only in groups does not count as active for
  stories. This is accepted (§12).

### 7.3 Archive

`SessionArchiveRepo::archive_relationship` also deletes every
`chat_session_members` row whose member is one of the archived sessions, in
the same transaction. Archiving a relationship takes that persona out of
every group, along with its affinity and memories.

## 8. Configuration

```toml
[tasks.group_director]
model       = "anthropic/claude-haiku-4.5"
fallback    = ["deepseek/deepseek-v4-flash"]
temperature = 0.5
max_tokens  = 4000
filter_prompt = """…"""

[[providers.openrouter.body]]
tasks  = ["group_director"]
params = { reasoning = { effort = "medium" } }
```

- **Example `filter_prompt`.** The example in `examples/model_config.toml` is
  written in Chinese, like the other examples, and covers:
  - who has a reason to answer: addressed by name, asked directly, or the
    topic is theirs;
  - a quiet member can stay quiet;
  - a member the user is close to answers more readily;
  - `[用户点名]` members answer;
  - an image only when `[图片能力] 本轮可发图=是`, and only from a member
    whose relationship makes one natural;
  - JSON output only.
- **Opt-in states:**
  - Section absent: the rule fallback runs and group chat still works.
  - Section present with a blank `filter_prompt`: the server refuses to boot,
    following `validate_world_prompts`. A misconfiguration does not quietly
    become the rule fallback.
- **Example ships enabled.** The shipped example has the director turned on.
  Every member answering every line is a poor default.
- **Resolution.** `resolve_group_director()` returns `Option`, following
  `resolve_pde`. `group_director` joins the task-name table.
- **Speakers** use `chat_companion` and its body rules. 1:1 reasoning stays
  off.

## 9. LLM crate

- `DeltaChunk` gains `reasoning: Option<String>`, parsed from
  `choices[0].delta.reasoning`, with a serde alias for `reasoning_content`
  (DeepSeek-style direct endpoints).
- Empty strings map to `None`, like `content`.
- Existing consumers read only `content` and are unaffected.
- The field is a breaking change for code that builds `DeltaChunk`
  literally, so the crate version is bumped accordingly at release.

## 10. Non-goals (first version)

- **Async send, voice, user image upload (vision).**
- **User actions, gifts, tips.** A refused action drops a tier, and a group
  never writes affinity.
- **Reactions, image edit.**
- **BYOK on the group route.**
- **Proactive group messages.** The director only answers the user.
- **More than one round per turn.** Personas never go back and forth within
  one user turn.
- **Must-speak / may-speak dice.** The director could mark mentioned or
  directly addressed members as must-speak and leave others as may-speak,
  with the engine rolling the may-speak members weighted by affinity. This
  waits on readings of how balanced the director's picks are. The output
  shape extends to it without a contract change.
- **Join/leave lines in transcripts.** The roster states who is present.
- **Dreaming over group sessions.**
- **Persona↔persona relationship state.** World context, when enabled,
  already reaches each speaker through its own 1:1 builder.

## 11. Data flow, one group turn

```
POST /v2/comp/group/{gid}/message/stream
  └─ persist user row + claimed queue row (group session)
       └─ detached turn
            ├─ load members: persona + affinity (read-only, decayed) → labels, rung
            ├─ director (streamed) ──► director_reasoning* ──► normalize | rule fallback
            │     └─ companion_decision_events (payload + reasoning)
            ├─ cast
            ├─ for each speaker:
            │     build_reply_request(member keys + [群聊] + group history)
            │     chat_companion stream ──► meta(speaker) delta* done [image_request]
            │     persist assistant rows (speaker_session_id)
            ├─ final
            ├─ settle_turn
            └─ spawn post-process: memory (all members), human insight (once),
                                   character/user insight (per speaker); no affinity
```

## 12. Accepted consequences

- **Affinity cools under group-only contact.** A group never writes
  `companion_affinity`, so its `updated_at` stays old and the 1:1 time decay
  runs on. A relationship carried on only in a group cools, consistent with
  the rule that a group never raises affinity.
- **Stories stop for group-only personas.** A persona active only in groups
  stops getting stories runs (§7.2).
- **A kick can lag one turn.** A member kicked mid-turn may finish that turn
  (§5.2).

## 13. Where it lives

| Crate | Addition |
|---|---|
| core | `group.rs`: director output normalization, rule fallback cast, transcript role mapping |
| llm | `DeltaChunk.reasoning`; `resolve_group_director`; boot validation |
| store | `group.rs` (`GroupRepo`: create / list / get / rename / add / remove / archive / active members); migration 0068; `list_sessions` filter; `archive_relationship` membership delete; dreaming pick filter |
| server | `routes/group.rs` (management + send, utoipa); `pipeline/group.rs` (turn orchestration, director call); new `ProtocolFrame` variants; channel guards |

## 14. Rollback and deploy

- **Migration.** Additive. An older image ignores the new table and columns.
- **Rolling back after groups exist.** The old `list_sessions` would return
  group sessions with `instance_id` NULL. Rolling back therefore means first
  archiving groups or accepting that clients see them.
- **Downstream visibility.** Users see nothing until a downstream client
  calls `/v2/comp/group*`. Deploy the engine first, then the client.
- **Validation.** Validate on the dev deployment before production.

## 15. Observability

- **Director.**
  - `companion_decision_events` rows with `session_id` on a group session,
    filtered by `status`, give the director's success and fallback rates.
  - `payload.reasoning` is the replay corpus for tuning the director prompt.
- **Generations.** `llm_generations` rows with `task = 'group_director'`, and
  `chat_companion` rows on group sessions.
- **Prompt logs.** With `PROMPT_LOG_DIR` set, each speaker writes its own
  prompt file keyed by the group session. The file carries a `# speaker:`
  header line with the speaker's member session id, and the file name ends
  with that id.
- **Speaker balance.** Speaker counts per turn come from `chat_messages`
  grouped by `speaker_session_id`. This is the reading behind the dice
  decision.

## 16. Testing

Every assertion is structural. Reply quality and director taste are judged by
a person on the dev deployment.

**core**
- Normalization: unknown handle, duplicate, image without availability, empty
  → failure.
- Fallback cast order.
- Transcript mapping: own lines → `assistant`, others → `user` with prefix.

**llm** (wiremock)
- `delta.reasoning` and `delta.reasoning_content` both parse.
- A content-only stream is unchanged.

**store** (sqlx::test)
- The three CHECKs.
- Concurrent add: two adds to a two-member group; one succeeds, one gets
  `group_full`.
- `archive_relationship` removes the archived sessions' memberships.
- `list_sessions` excludes groups.
- Read-only affinity load creates no row.

**server** (sqlx::test + wiremock)
- Full turn frame order: `director_reasoning* → cast → (meta → delta* → done
  → image_request?)×N → final`.
- Assistant rows carry `speaker_session_id`.
- Affinity untouched: member `companion_affinity` row count and `updated_at`
  are unchanged after a turn.
- A relationship memory row is written for every seated member, including
  non-speakers.
- The decision row carries `reasoning`.
- A later speaker's request body contains the earlier speaker's line from the
  same turn.
- After a 1:1 affinity change, the next group turn's director input shows the
  new label.
- Director failure → rule fallback; task absent → rule fallback; blank
  `filter_prompt` refuses boot.
- Speaker failure → `speaker_error`, the relay continues, queue `done`.
- Generation timeout mid-relay.
- Every §7.1 refusal, and the group send route refusing a text session.
- The chat-queue worker settles a released group turn as failed with one
  notice and drives nothing.

## 17. Documentation

- **New:** `docs/group-chat.md` + `docs/group-chat.zh.md`.
- **Updated** (each with its `.zh.md` counterpart where one exists):
  - `architecture`: pipeline, data flow, routes, file structure;
  - `api-reference`: the new routes and frames, `speaker_session_id` on
    history, the new 409s, sessions-list exclusion;
  - `model-config`: the task table and the director reasoning body rule;
  - `affinity-model` § What reads affinity: the group director, read-only;
  - `memory-layers`: group-sourced relationship rows;
  - `llm-audit`: director decision rows and `reasoning`;
  - `examples/model_config.toml`.

## 18. Decisions taken

1. **Membership.** The unit is an existing 1:1 text session, at most three
   per group, with members added and kicked after creation. Archiving a
   relationship removes it from its groups.
2. **No affinity in groups.** No affinity judge runs and no affinity is
   written. Each persona's affinity is read from its member session.
3. **Storage.** A group is a `chat_sessions` row (`channel = 'group'`).
   Separate group tables were rejected because they would duplicate the
   queue, history cursors, read markers and image recovery.
4. **The director.** It replaces the PDE for group turns: one call returns an
   ordered speaker list with per-speaker actions. A director that only picks
   speakers, followed by a 1:1 PDE call per speaker (1 + 2N round trips), was
   rejected. So was a director that writes every line in one call, which
   loses each persona's own prompt.
5. **One pass per user turn.** Each member speaks at most once per turn.
6. **Tier labels.** Affinity reaches the director as tier labels; dice are
   deferred.
7. **Group lines reach 1:1 memory.** They go to the relationship layer of
   every present member.
8. **Images on; the rest off.** Persona images are on in v1. Actions,
   reactions and voice are off.
9. **Titles.** Groups have an engine-stored title that can be renamed.
10. **Director reasoning.** It is on through a body rule. Reasoning text
    streams to the client as `director_reasoning` and its display is the
    client's decision. The text is also kept in the decision audit row.
