# Life events and the world system — Design

- **Date:** 2026-10-23
- **Status:** Approved
- **Type:** Engine change. No migration, no API change. The World Stories
  director's payload gains three keys and one rule; the World Memories
  director's payload gains one optional per-persona key and one conditional
  rule; the stories chat evidence stops reading proactive rows.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — life events (`engine.persona_events`) and the
  ask-for-space choice (`engine.life_choices`) become inputs of the World
  Stories director (L3) and, for a background under way, of the World
  Memories director (L4).
- **Builds on:** `2026-07-23-world-stories-design.md` (*the stories spec*),
  `2026-07-21-world-memories-design.md`, `2026-10-15-life-events-v2-design.md`
  (*the v2 spec*), `2026-10-16-life-events-offline-and-scares-design.md` (*the
  offline spec*), `2026-10-19-life-events-shopping-gatherings-and-ask-for-space-design.md`
  (*the ask-for-space spec*). Everything in them stands except §3.4 here,
  which narrows the stories spec's chat evidence, and the v2 spec's out-of-scope
  line "Writing events into `persona_story_events`", which this spec answers
  without writing there.

## 1. Motivation

Life events describe the persona's day, from half an hour to a week: drawn by
rule, with exact start and end, written before she says a word. World Stories
describes her life: an LLM director every 8 hours writes her work, romance and
family. The two have no data path between them today.

The life-event specs assumed one: the story director reads the pair's recent
chat, so an event message would reach the life story through chat. It does not.

- **Event messages rarely reach the story director, and arrive mispaired when
  they do.** `StoryRepo::chat_evidence` pairs a user row with the next assistant
  row in the same session and drops an assistant row with no unpaired user row
  before it. An event message is a proactive assistant row with no user row of
  its own. Usually the previous turn was answered, so it is dropped. When the
  previous user row went unanswered (ghosted, or a failed turn), the event
  message is paired with it, and the director reads her answering an unrelated
  line with a sudden incident. Offline kinds she does not announce write no
  message and do not exist for the director at all. Openers, holiday and
  birthday greetings and the ask-for-space messages (her ask, her goodbye, her
  hurt reply) are proactive rows with the same mispairing.
- **The ask-for-space choice is half visible.** An accept writes a
  `taking_space` event and an ignore a `fell_out` event, neither with a
  message; a decline writes no event. The relationship turn the story's
  `user_relationship` field exists to track is missing or garbled.
- **The persona's present situation has two authors.** A trip or an illness
  under way is a `persona_events` row; the story insight's `location` field and
  the digest are LLM text that knows nothing of it. The director can write her
  at home while the rule director has her on a business trip. Downstream, the
  World Memories director scripts her into a gathering while she is in
  hospital.

The rule from the world-system design applies: the closer to the raw fact, the
further upstream, and data flows only downstream. `persona_events` and
`life_choices` are engine-recorded facts, the same nature as `chat_messages`;
the story director's output is narrative. So they sit upstream of L3, next to
chat evidence, and a background under way also reaches L4 directly:

```
art_metadata (persona, three event settings)
   │                         │
   ▼                         ▼
persona_events ─────► World Stories (L3) ─────► World Memories (L4) ─────► Town (L5)
life_choices   ─────►    insight / digest /        recent_life +
(rule-written)           story events              current_background
                              ▲
chat_messages ────────────────┘ chat evidence, proactive rows excluded
```

## 2. Decisions

1. **The story director reads the event table directly** (§3.1, §3.2), and
   the choice table (§3.3). Chat evidence stops carrying proactive rows
   (§3.4).
2. **Times reach both directors as engine-rendered relative spans**, in the
   wording the judge's `[近期事件]` line already uses (`span_text`,
   `ends_text` in `prompt.rs`). No absolute timestamp is given for an event:
   the director would have to subtract it from `current_time`, which is
   arithmetic an LLM gets wrong, and the stories spec already requires
   relative time in what it writes.
3. **Events are settled fact for the story director** (rule 8, §3.5): not to
   be rewritten, denied or moved, and nothing the director writes may
   contradict the background under way; the digest need not mention it, and
   when it does, writes it as something that ends. The director keeps its
   freedom to write illnesses and travel of its own: a deployment that runs stories and
   draws no events must lose nothing. A story that invents a cold the rule
   director never drew is an accepted residual, not a case for a tighter
   rule.
4. **The World Memories director reads a background under way** (§4),
   whether or not the owner runs stories: it is `persona_events` flowing to
   L4, not a story output.
5. **Archive follows the existing split.** `archive_relationship` keeps the
   persona's own life (story events and memories) and clears what belongs to
   the relationship (the digest). Here: event rows are her life and are read
   whatever happened to the session they were told in; the narration of an
   event told in an archived session is the relationship's transcript, as chat
   evidence already treats it, and is left out; a choice belongs to one
   relationship and is read only from the pair's unarchived sessions. The two
   events a choice writes, `taking_space` and `fell_out`, belong to that
   relationship too and follow the choice, not the rest of her life: they are
   read only while the session they were written in exists and is not
   archived.
6. **No backflow.** Neither director writes `persona_events` or
   `life_choices`, and neither changes `prefer_events`, `health` or
   `prefer_trip`. If a story could change the draw settings, LLM narrative
   would steer the rule director, whose events the story reads back: a
   "she is frail" invented in one round would return as fact in the next and
   grow. The settings belong to whoever authors the persona; when a story and
   the settings disagree (the story has her at a law firm, the draw lands
   `in_class`), a person changes the settings or the caller's `kinds`.
7. **Needs stay out of stories.** Needs drift by the hour and are re-rated
   every judged turn; an 8-hour round would see one snapshot, and chat
   evidence already holds what they made her say and do.
8. **The story activity gate is unchanged.** Event messages, opener and
   greeting rows and the ask-for-space messages bump
   `chat_sessions.last_active_at`, and the 72-hour gate reads it. Something
   happening to her counts as activity: a user who only opens the chat, and
   whose client draws the day's event, keeps her life moving.

## 3. The story director

All of §3 changes `pipeline/story.rs::direct_story` and
`story_user_payload`, plus reads in `eros-engine-store`. The keys below are
added to the payload's `data` object after `recent_chat`, always present
(empty array or `null` when there is nothing), on init and continuation
rounds alike.

### 3.1 `life_events`

The instance's events, newest first, capped at 30:

- **Which.** Every `persona_events` row of the instance that started within
  the round's evidence window (`context_days`, the window chat evidence uses)
  or is still under way at the round's `now`. The second condition keeps a
  long background that started before the window. Rows of a kind this build
  does not know are skipped, as on every other read. Two exclusions:
  - `taking_space` and `fell_out` (`EventKind::SPECIAL`) whose session is
    gone or archived (§2.5).
  - A `period_scare` or `pregnancy_scare` still under way. The persona does
    not know yet that it is nothing, and every prompt surface keeps it that
    way (the offline spec §5); a round inside the scare would write it into
    the digest, the insight or story events, which outlive the hour. It
    reaches the next round once ended, with its `outcome`.
- **Each entry:**

  | key | value |
  |---|---|
  | `label` | the kind's fixed Chinese name, `event_label` (`prompt.rs`) |
  | `category` | `short`, `mid`, `body` or `trip` |
  | `when` | relative span, below |
  | `told` | the narration: `chat_messages.content` of `message_id`. Absent when the event has no message (an offline kind she did not announce, `taking_space`, `fell_out`), the message is gone, or the message's session is archived |
  | `outcome` | only for an ended `period_scare` / `pregnancy_scare`: `虚惊一场` |

- **`when`.** `now` is the round's instant, the one sent as `current_time`.
  - Under way: `{span(now − started_at)}前开始，{ends_text(ends_at − now)}`,
    e.g. `1 天前开始，约 2 天后结束`.
  - Ended: `{span(now − started_at)}前开始，持续 {span(ends_at − started_at)}`,
    e.g. `3 天前开始，持续 2 小时`; a length under an hour reads
    `持续不到 1 小时`.

### 3.2 `current_background`

The background under way at `now`, or `null`: the same entry shape as §3.1
(`label`, `category`, `when`, optional `told`). It repeats the
`life_events` entry on purpose; the separate key states "this is her
situation now" without asking the director to compare spans. Only the
background is singled out: an incident lasts at most four hours
(`taking_space`, 12 to 24, is the one exception) while the digest is resident
for a whole round. An incident under way is visible in `life_events` by its
`when`.

### 3.3 `life_choices`

The pair's choices, newest first, capped at 10:

- **Which.** `engine.life_choices` rows whose session belongs to this owner
  and instance and is not archived, asked within the evidence window. Only
  kind `ask_for_space` is rendered; any other kind is skipped (the engine owns
  the vocabulary).
- **Each entry:**

  | key | value |
  |---|---|
  | `label` | `角色提出想先分开一阵子` |
  | `when` | `{span(now − asked_at)}前` |
  | `asked` | the ask's text, `chat_messages.content` of `message_id`; absent when the message is gone |
  | `outcome` | below |

- **`outcome`**, from the stored status and the derived expiry
  (`LifeChoiceRecord::is_pending`, `expires_at`):

  | state | `outcome` |
  |---|---|
  | `pending`, unexpired | `还在等用户答复` |
  | `pending`, expired | `用户一直没有答复，这件事不了了之` |
  | `accepted` | `用户同意了，两人说好暂时分开 {n} 小时`; `{n}` is the linked `taking_space` row's length in whole hours, and the clause reads `用户同意了，两人说好暂时分开` when that row is gone |
  | `declined` | `用户不同意分开` |
  | `ignored` | `用户没有回应这个请求，接着说了别的，两人为此闹得不愉快` |

The user's words around the choice (a reply that ignored it, what came after
the return) stay in chat evidence; the choice entry gives the director what
chat evidence no longer carries.

### 3.4 Chat evidence skips proactive rows

`StoryRepo::chat_evidence` adds `AND cm.assistant_action_type IS DISTINCT
FROM 'proactive'` to its row query. Every proactive row is excluded: event
messages, ask-for-space messages, openers, holiday and birthday greetings.
Events and choices now arrive through §3.1 and §3.3; a greeting carries no
life fact. The pairing walk is unchanged, so a user row that went unanswered
before an event message now pairs with the next real reply (or collapses into
the next user row), and no proactive row is ever presented as an answer.

The row cap (`cap_turns * 2 + 2` fetched) applies after the filter, so the
evidence keeps its full turn budget.

### 3.5 Rule 8

Appended to `STORY_DIRECTOR_RULES` after rule 7:

```
8) life_events 与 life_choices 是引擎记录的、已经发生的事：不得改写、否认或挪动时间，
也不得与 told / asked 的讲述原文矛盾；可以作为 events 与 insight 的素材展开
（例：刚分手之后更新 romance_history；分开请求之后更新 user_relationship）。
current_background 是角色此刻所处的状态，会按 when 结束：digest 与 insight 不得与它矛盾；
写到它时写成有终点的事（这几天在外地出差），不要写成长期状态。
```

The digest does not have to carry the background. The reply already reads it
live as `正在经历` under `[now]`, and a digest is resident until the next
round: one that states a 24-hour trip would still state it hours after the
trip ended, while `[now]` shows nothing under way.

The wording mirrors rule 1 ("the user's words and deeds come only from chat
records"): what `life_events` records is not the director's to change.
Everything else in her life, illnesses and travel included, remains the
director's to write.

### 3.6 Payload sketch

```json
{
  "current_time": "2026-10-23T08:00:00+00:00",
  "worldview": "…",
  "persona": { "…": "…" },
  "current_insight": { "…": "…" },
  "current_digest": "…",
  "recent_events": [ … ],
  "affinity_reference": { … },
  "recent_chat": [ { "用户": "…", "角色": "…" } ],
  "life_events": [
    { "label": "在看书", "category": "mid", "when": "不到 1 小时前开始，约 2 小时后结束", "told": "…" },
    { "label": "出差", "category": "trip", "when": "1 天前开始，约 2 天后结束", "told": "…" },
    { "label": "睡着了", "category": "mid", "when": "2 天前开始，持续 6 小时" },
    { "label": "验出怀孕", "category": "short", "when": "4 天前开始，持续 1 小时", "told": "…", "outcome": "虚惊一场" }
  ],
  "current_background": { "label": "出差", "category": "trip", "when": "1 天前开始，约 2 天后结束", "told": "…" },
  "life_choices": [
    { "label": "角色提出想先分开一阵子", "when": "5 天前", "asked": "…", "outcome": "用户同意了，两人说好暂时分开 12 小时" }
  ]
}
```

## 4. The World Memories director

`pipeline/world.rs::direct_world` reads, for the roster it already loaded,
the background under way at the round's instant for each instance, and
`director_user_payload` adds it to that persona's JSON entry:

```json
{ "instance_id": "…", "name": "…", "personality": "…", "profile": { … },
  "current_background": "住院（1 天前开始，约 3 天后结束）" }
```

- The value is `{label}（{when}）` with `when` as in §3.1 (under way).
- The key is present only on a persona with a background under way. When no
  roster persona has one, the payload is byte-identical to today's.
- Independent of stories and of `recent_life`: an owner without stories gets
  it too. It is read on a reset round as well; a background is drawn by rule,
  not by the world being reset.
- One rule is appended, only when at least one persona carries the key, after
  the stories rule (numbered 8 like the stories rule is 7; the numbers are
  labels, as the existing comment on `WORLD_STORIES_WM_RULES` says):

  ```
  8) 带 current_background 的角色此刻处在该状态中，剧本必须与之一致
  （例：住院中的角色不会出现在聚会现场）。
  ```

The World Memories director runs every 24 hours and stories every 8, so a
background can start between story rounds; this read is why L4 does not wait
for a story round to learn of it. Data still flows only downstream.

## 5. Out of scope

- Writing `persona_events` or `life_choices` from either director, or
  changing the three persona event settings (§2.6).
- Needs in any world payload (§2.7).
- Any change to the story activity gate (§2.8).
- Start and end times on `persona_story_events`; it keeps `story_date`. To be
  decided once the director's use of `life_events` can be read.
- A persona-level `kinds` allowlist kept by the engine. The caller's `kinds`
  (`allow` / `disallow`) is the existing lever for a persona whose world rules
  a kind out.
- An engine default worldview or enrollment. The engine ships none (world
  system design); a deployment that enrols owners and writes worldviews gets
  these inputs from its first story round.
- Injecting `[world_stories]` differently, or changing the reply prompt. The
  event lines already sit under `[now]`, outside `[character_state]`.

## 6. Testing

- **Store, chat evidence.** A ghosted user row, then an event message, then a
  user row and a reply: the evidence holds one pair (the second user row with
  the reply) and no proactive content. The existing windowing and
  cross-session tests keep passing.
- **Store, events read.** Window boundary; a background started before the
  window and still under way is included; the cap; an unknown kind skipped;
  narration present for a told event, absent for an unannounced offline
  event, absent when the narration's session is archived while the event
  itself is still returned; `taking_space` / `fell_out` of an archived or
  deleted session not returned; a false alarm under way not returned, the
  same one returned with its outcome once ended.
- **Store, choices read.** Pair scope (another owner's or instance's choice is
  not returned), archived sessions excluded, window, each status, and an
  accepted choice whose `taking_space` row is gone.
- **Server, rendering.** `when` for under way and ended; `outcome` for an
  ended false alarm and for each choice state; empty
  inputs give `[]` / `null`; rule 8 present in every story payload.
- **Server, World Memories payload.** Byte-identical with no background under
  way; the key on exactly the personas that have one; the rule appended once
  when any does; present for an owner without stories.
- **Cross-system.** One database test that writes an event with its message
  through `EventRepo::insert` in a session of an enrolled, stories-enabled
  pair, then builds the story director's user payload through the same reads
  `direct_story` uses: the narration appears under `life_events` (and
  `current_background` for a background), and not under `recent_chat`. The
  same for an accepted ask: the `life_choices` entry and the `taking_space`
  event both appear. This is the data path the earlier specs assumed and no
  test checked.

## 7. Documentation

- `docs/world-system.md` and `.zh.md`: the story round's input table gains
  the life events, the background under way and the choices; chat evidence
  excludes proactive rows; rule 8 in prose; World Memories gains the
  per-persona background with its rule; the relation to the activity gate.
- `docs/life-events-needs.md` and `.zh.md`: where events and the choice go
  beyond the judge and the reply: the story director (with stories enabled)
  and the World Memories director (a background under way).
- No migration guide: nothing a downstream calls or stores changes.

## 8. Left to the builder

- The store functions' names and SQL shape, and whether the events read
  extends `EventRepo::recent` or is a new `StoryRepo` read. The background read
  for the World Memories roster should be one query over the roster's
  instance ids, not one per instance.
- Where the shared rendering lives (`event_label`, `span_text` and
  `ends_text` become `pub(crate)` or move beside the story code); wording is
  fixed by this spec.
- Test fixtures and placement.
