# Group chat

[English](group-chat.md) · [中文](group-chat.zh.md)

One user talks with up to three personas in a single conversation. The user
says something, and the personas who have a reason to answer do so in turn,
each in their own voice and with the relationship it already has with the user.
Later speakers hear what earlier speakers just said.

A group builds no relationship of its own. Each persona's affinity lives in its
1:1 session and is only read here.

## What a group is

- A group is a chat session with `channel = 'group'` and no persona instance
  (`instance_id` is NULL). It reuses the turn queue, history cursors, read
  markers, image-request recovery and `llm_generations` attribution of any
  other session.
- Its members are the caller's own 1:1 text sessions, **at most three**
  (`MAX_GROUP_MEMBERS`). Member order is `joined_at`. A persona instance can
  appear once per group.
- It has a nullable `title`: trimmed, 1 to 64 characters, no control
  character. An untitled group is still a group; a client can render a name from
  its members. `PATCH` changes the title, and `null` clears it.
- Dissolving a group sets `archived = true`. Member sessions are untouched.
- Archiving a relationship (`DELETE /comp/instance/{instance_id}/sessions`)
  removes that persona from every group in the same transaction, along with its
  affinity and memories. Later turns cast only the remaining members.
- Group storage is `engine.chat_sessions` (`channel = 'group'`, `title`),
  `engine.chat_session_members` (group, member, `joined_at`) and
  `engine.chat_messages.speaker_session_id` (who spoke each assistant line).

## Routes

Every route requires the JWT user to own the group; anything else is
`404 group_not_found`. Refusals on these routes carry the body
`{code, message, user_message}`.

### Management

| Method | Path | Body / effect |
|---|---|---|
| `POST` | `/v2/comp/group` | `{title?, member_session_ids: [1..=3]}` → group DTO |
| `GET` | `/v2/comp/groups` | the caller's non-archived groups, newest `last_active_at` first |
| `GET` | `/v2/comp/group/{group_id}` | group DTO |
| `PATCH` | `/v2/comp/group/{group_id}` | `{title}`; `null` clears |
| `POST` | `/v2/comp/group/{group_id}/members` | `{member_session_id}` |
| `DELETE` | `/v2/comp/group/{group_id}/members/{member_session_id}` | kick |
| `DELETE` | `/v2/comp/group/{group_id}` | dissolve |

Group DTO:

```json
{"group_id": "…", "title": "周末爬山",
 "members": [{"member_session_id": "…", "instance_id": "…", "joined_at": "…"}],
 "created_at": "…", "last_active_at": "…"}
```

Create and add-member refusals:

| Condition | Response |
|---|---|
| The group already has three members | `409 group_full` |
| Member session is unknown or not the caller's | `404 session_not_found` |
| Member session is not a `text` session | `409 wrong_channel` |
| Member session is archived | `409 session_archived` |
| Another member has the same persona instance | `409 instance_already_member` |
| Blank, over-long or control-character title | `422 invalid_title` |
| `member_session_ids` is empty (create) | `422 unprocessable` |

Kicking a session that is not a member of the group returns
`404 member_not_found`.

A kicked persona's past lines stay attributed through
`speaker_session_id`. Dissolving a group keeps the memory rows it produced:
they belong to the members' relationships.

### Send

`POST /v2/comp/group/{group_id}/message/stream` → SSE. Stream only; there is no
async variant.

Request body: `content`, `client_msg_id`, `tier`, `prompt_traits`,
`user_timezone` / `user_country` / `user_region`, the 1:1 `image` block, the
1:1 `audit` block, and an optional `mentions: [member_session_id]` for a
client-side @.

- Mentions must name members of the group; an unknown id returns
  `422 unprocessable`.
- `audit` takes the 1:1 caps (a violation returns `400 invalid_payload`) and
  rides each speaker's reply call; see [llm-audit.md](llm-audit.md).
- `image` means what it means on the 1:1 route: its presence, together with a
  configured `[tasks.chat_image_prompt_compose]`, makes images available this
  turn. `image.force = true` returns `400 invalid_payload`, since a group turn
  has no single persona to force.
- A `byok` block returns `400 invalid_payload`.
- `409 group_empty` when the group has no member; `409 wrong_channel` when the
  session is not a group.
- A group stream counts toward the per-user in-flight stream cap;
  `429 rate_limited` at the cap.
- Resending a `client_msg_id`: while its turn is running, `409
  duplicate_in_progress`. Once the turn has settled, a replay re-emits each
  persisted row's `meta` (with `speaker_session_id`), `delta` and `done`, then
  `final`, or a terminal `error` when every row is a failed attempt. A replay
  carries no `cast`, `director_reasoning`, `speaker_error` or `image_request`.
  A turn that failed terminally without any assistant row returns
  `409 duplicate_failed`; send a fresh `client_msg_id`.

### Reused unchanged

These take a group session id as they are:

- `GET /comp/chat/{session_id}/history`;
- `GET /bff/v1/comp/chat/{session_id}/history`, with its cursors;
- `POST /comp/chat/{session_id}/read`;
- `GET /comp/chat/{session_id}/messages/{message_id}/image-request`.

History entries of a group carry `speaker_session_id` on assistant rows
(omitted otherwise). A client resolves a departed speaker's name through
session, then instance.

### Routes that refuse a group session

A group session sent to a 1:1 route returns `409`. The 1:1 stream and async
send routes, `/v2/comp/session/{id}/open` and `/effects` answer with
`code: wrong_channel`. The reaction route and image edit answer with their
plain body, `{"error": "conflict", "message": …}`, whose `message` contains
`wrong_channel`. `GET /comp/chat/{user_id}/sessions` leaves groups out;
`/v2/comp/groups` lists them. The dreaming sweeper skips group sessions.

## One turn

```
POST /v2/comp/group/{gid}/message/stream
  └─ persist user row + claimed queue row (group session)
       └─ detached turn
            ├─ seat members: persona + affinity (read-only, decayed) → labels, rung
            ├─ director (streamed) ──► director_reasoning* ──► normalize | rule fallback
            │     └─ companion_decision_events (payload + reasoning)
            ├─ cast
            ├─ for each speaker, in order:
            │     build the reply request (this member's keys + [群聊] + group history)
            │     chat_companion stream ──► meta(speaker) delta* done [image_request]
            │     persist assistant rows (speaker_session_id)
            ├─ final
            ├─ settle the queue row
            └─ spawn post-process: memory (every seated member), human insight (once),
                                   character/user insight (per speaker); no affinity
```

The turn runs detached: a client that disconnects does not cancel it, and reads
the result from history. A resend of the same `client_msg_id` is answered as
described under [Send](#send).

Turns of one group are not serialized. A message sent while a relay is still
running starts a second relay at once; the two can interleave in history, and
a persona can speak in both.

**Seating.** The turn loads each member's persona and its 1:1 affinity. A
member whose persona instance is inactive is not seated that turn. Affinity is
read only: it is time-decayed and refreshed in memory, a member with no
affinity row yet gets the default in memory, and no row is created or written.
The snapshot is fixed for the turn, so a member kicked mid-turn may finish it.

**Speakers run one after another.** Each finishes, and its rows are persisted,
before the next starts. Every request is the 1:1 reply request, built with that
member's own keys: affinity and feeling clause, relationship and profile
memory recall, insights, world and stories context, items and wardrobe, the
local time and holiday, `prompt_traits` filtered by the tier, the 1:1 reply
plan and `TurnNudges` rolled from that member's affinity. Clothing comes from
the director's entry. `chat_companion` keeps its tier routing, retry chain,
fallback phrase, output filter and multi-bubble chaining. Every assistant row
a speaker writes carries `speaker_session_id`.

**The `[群聊]` block** is appended to each speaker's system prompt:

```

[群聊]
你现在在群聊「{title}」里。          ← 你现在在一个群聊里。 when untitled
在场的还有：
- {name}：{personality}            ← - {name} when no personality
只说你自己的话，不替别人说话，不在开头写名字。
```

A speaker alone in the group gets `群里只有你和对方。` in place of the
`在场的还有：` line and its list.

**History a speaker sees.** The last `HISTORY_WINDOW` (20) group rows up to
the user row, plus the assistant rows earlier speakers persisted in this same
turn. A newer user message that arrived mid-relay is not included. This
speaker's own lines are `assistant`; the user's lines are `user` with the
prefix `用户：`; other personas' lines are `user` with the prefix `{name}：`
(`某位群友` when that persona is gone). The group transcript the director and
composer read skips empty non-image rows; an image row reads
`（发了一张照片）`.

**Images.** Image availability is one value for the turn, computed as on 1:1.
For a `reply_text_image` entry the composer is spawned before the chat call
with that speaker's context, and the `image_request` frame follows the
speaker's `done`. The director's entry carries no image reference or aspect
ratio, so the 1:1 defaults apply.

**Prompt logs.** With `PROMPT_LOG_DIR` set, each speaker writes its own prompt
file keyed by the group session, with the speaker's member session id on a
`# speaker:` header line and at the end of the file name.

### Frames

```
director_reasoning*                       (0..n, only with reasoning on)
cast
( meta → delta* → done → image_request?  |  meta → done{truncated:true} → speaker_error  |  speaker_error )  × speakers
final
```

A failed speaker emits `meta → done{truncated:true}` for each attempt and then
`speaker_error` when its chat chain is exhausted, or a bare `speaker_error`
when it leaves no row: its reply request could not be built, or its image row
could not be persisted. A terminal `error` can replace anything
after the first frame.

| Frame | Payload |
|---|---|
| `director_reasoning` | `{content}`: a reasoning delta |
| `cast` | `{speakers: [{member_session_id, action_type}]}`; `action_type` uses the `meta` wire values `reply` / `reply_image` / `reply_text_image`. Emitted after the rule fallback too, so a client can always show who is about to speak |
| `meta` | gains `speaker_session_id` on group turns; omitted on 1:1 turns, whose frames are unchanged |
| `delta` / `done` / `image_request` | unchanged, keyed by `message_id` |
| `speaker_error` | `{speaker_session_id, code, message}`; non-terminal. A new frame rather than an `error` variant, because clients treat `error` as the end of the turn |
| `final` | the 1:1 shape; counters are summed over speakers, and `filtered` / `prompt_injected` are true if any speaker's was |

The engine only emits frames. Whether reasoning is shown in full, collapsed or
as a "thinking" indicator is the client's decision.

### Failure and settle

| Failure | Behavior |
|---|---|
| Director fails, is empty or is absent | Rule fallback; the audit row records the status |
| One speaker's chain is exhausted | `meta → done{truncated:true} → speaker_error` (the attempt row is persisted, as on 1:1); the relay continues with the next speaker |
| A speaker's request cannot be built, or its image row cannot be persisted | A bare `speaker_error`; the relay continues with the next speaker |
| The composer fails | The text half lands; no `image_request` |
| No speaker produced a reply | The stream ends with an `error` frame instead of `final`, and no post-process runs |
| `CHAT_QUEUE_GEN_TIMEOUT_SECS` expires | One budget covers the director and every speaker. Speakers who finished stay persisted, the rest are skipped, and the terminal frame is `error generation_timeout`. No post-process runs: the finished speakers' lines stay, but the user's line reaches no member's memory or insights for that turn |
| Settle | The turn settles `done` if any assistant row landed. A `system_error` notice, which carries no speaker, is written only when none did |
| Client disconnect | The turn completes and persists |
| The process dies mid-turn | The stale-claim reaper releases the claim, and the chat-queue worker settles a released group turn as unservable: failed, with one notice. A group turn is never re-driven in the background |

A failed speaker does not stop the relay: the other members are independent and
still answer. A group turn is a single attempt with no retry, and each speaker
is a single attempt too.

## The director

One `[tasks.group_director]` call replaces the 1:1 PDE judge for a group turn:
it returns an ordered speaker list with a per-speaker action.

**Config.** The task takes `model`, `fallback`, `temperature`, `max_tokens`,
`structured_output` (default on) and a `filter_prompt` that sets the director's
taste. The shipped `examples/model_config.toml` carries an enabled example.

- Section absent: the rule fallback runs and group chat still works.
- Section present with a blank `filter_prompt`: the server refuses to boot.

Each attempt is bounded by the chat stream's per-attempt constants, and
OpenRouter's keepalives count as stream bytes, so a model that reasons for a
long time is not cut off.

**Reasoning** is turned on per deployment by a body rule; there is no separate
key:

```toml
[[providers.openrouter.body]]
tasks  = ["group_director"]
params = { reasoning = { effort = "medium" } }
```

Reasoning tokens stream to the client as `director_reasoning` frames, and the
full text is kept in the decision audit row. Content deltas are buffered and
parsed when the stream ends. If the call falls back to another model, that
model's reasoning keeps appending. Reasoning is display-only: it is not passed
to speakers and decides nothing. `reasoning = { exclude = true }` makes the
model reason but return no text, so no frames are sent. Reasoning tokens count
against `max_tokens`; bound them through the rule (`effort` or
`reasoning.max_tokens`) rather than by lowering `max_tokens`.

**The user message** is built by the engine and is not configurable, because
the engine parses what comes back. Sections appear in this order; an optional
section is omitted when it has nothing to say.

```
[群聊] {title}                                         ← only when a title is set
[成员]
- m1：{name}（{personality}）bond={bond_key} chemistry={chem_key} 亲密度=第 {rung} 档
- m2：{name} bond={bond_key} chemistry={chem_key} 亲密度=第 {rung} 档   ← no personality ⇒ no （…）
[图片能力] 本轮可发图={是|否}
[最近群聊]
{transcript}                                           ← "（无）" when empty
[用户点名] {name}、{name}                               ← only when mentions are present

[用户最新消息]
{content}
```

Handles `m1`, `m2`, `m3` follow seat order. `bond_key` and `chem_key` are the
Bond and Chemistry tier keys; affinity reaches the director as tier labels and
a rung, never as numbers. Transcript lines are `用户：{content}` and
`{name}：{content}` (`{name}：（发了一张照片）` for an empty image row), joined
with `\n`.

**Output:**

```json
{"speakers": [{"member": "m2", "action": "reply_text_image",
               "inner_state": "…", "reply_mode": null, "clothing": null}],
 "reason": "…"}
```

`member` is an enum over the handles present. `action` is `reply_text`,
`reply_image` or `reply_text_image`. `inner_state`, `reply_mode` and `clothing`
are optional, with the 1:1 judge's semantics.

**Normalization** (pure, in `eros-engine-core`): an unknown or unseated handle
is dropped; a repeated handle keeps its first occurrence; an image action on a
turn without image availability degrades to `reply_text` (an action only goes
down); a list that is empty afterwards counts as a director failure.

**Rule fallback.** Every seated member speaks `reply_text`, in join order. It
applies when the director fails, times out, returns unparseable output or an
empty list, and when `[tasks.group_director]` is absent.

**Audit.** One `companion_decision_events` row per director run: the parsed
verdict, a `reasoning` key, the roster as the director saw it, and the
director's `generation_id`. See [llm-audit.md](llm-audit.md#group-director).

## What a group never does

- It runs **no affinity judge**, writes **no affinity** and touches **no ghost
  streak**. No member's `companion_affinity` row is created or written.
- Affinity is read from each member's 1:1 session: when the 1:1 relationship
  moves, the persona sounds different in the group from the next turn on.
- A relationship conducted only in a group does not warm up there, and it cools
  under the 1:1 time decay, because no write refreshes its `updated_at`.
- A persona active only in groups does not count as active for stories.
- The member sessions' `last_active_at` does not move; the group's does.

## What reaches 1:1

Post-process is spawned once after `final` and never blocks the stream. A
turn that ends without `final` (no reply, or the generation timeout) runs
none.

- **Memory.** The user's message is embedded once. The profile layer gets one
  row (`instance_id` NULL). The relationship layer gets one row for **every
  seated member**, whether or not it spoke, with the content
  `（群聊）用户：{message}` and `session_id` = the group. Each member write
  rechecks that the member session is not archived. See
  [memory-layers.md](memory-layers.md).
- **Insights.** `human_insights` runs once per turn, over the user's message and
  each produced reply as `{name}：{full_text}`. `character_insights` and
  `user_insights` run once per speaker, from the user's message and that
  speaker's reply, keyed by that speaker's instance. Before it writes, each
  rechecks that the speaker's member session is not archived. A turn with N
  speakers makes 1 + 2N extraction calls.
- **Not run:** affinity evaluation, the feeling-clause summary and ghost-streak
  updates.

## Not in v1

- Async send, voice, and user image upload (vision).
- User actions, gifts and tips.
- Reactions and image edit.
- BYOK on the group route.
- Proactive group messages: the director only answers the user.
- More than one round per user turn: each member speaks at most once.
- Must-speak / may-speak dice weighted by affinity.
- Join and leave lines in transcripts: the roster states who is present.
- Dreaming over group sessions.
- Persona-to-persona relationship state.
