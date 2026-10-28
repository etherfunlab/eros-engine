# Memory 2.0: dreaming-plus — episodes, relationship arc and journal — Design

- **Date:** 2026-10-28
- **Status:** Approved
- **Type:** Engine change: the per-session dreaming sweeper is replaced by a
  per-relationship scanner with a cursor; four new tables (scan state,
  episodes, arcs, journal) plus one lean event table; a new three-output
  extraction contract; mechanism-written journal rows; a new
  `[shared_memories]` shape with a character budget; an anniversary line in
  `[now]`; a backfill subcommand; archive and erasure coverage. Ships with a
  migration guide.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — `eros-engine-store` (tables, repos, archive,
  erasure), `eros-engine-server` (`pipeline::dreaming` rewritten,
  `pipeline::post_process`, `pipeline::handlers`, `prompt`, `holiday`/`now`,
  the CLI), `eros-engine-llm` (two task names), `eros-engine-core` (journal
  vocab).
- **Builds on:** `2026-10-26-insights-2-fact-ledger-design.md` (the
  K/idle trigger shape, `human_profile_state`, erasure rule),
  `2026-10-27-affinity-gated-profile-recall-design.md` (recall K per tier),
  `2026-09-26-user-locale-and-holiday-greeting-design.md` (the `[now]`
  holiday line and its timezone rule), `2026-08-09-voice-memory-bootstrap-
  recall-design.md` (the voice snapshot).
- **Version:** v2.6.0.

## 1. Motivation

The engine has three write paths into memory and all three feed the
**profile layer** (cross-character user facts): per-turn raw rows, the
dreaming sweeper's categorised memories, and the Insights 2.0 fact ledger.
The **relationship layer** (user × character) holds raw user utterances
only — no LLM product of any kind, and nothing dated.

Measured on a production deployment:

- **78% of conversation rows sit outside the 20-row history window**, and
  the window actually injected is 3–17 rows. Half of all relationships fit
  in the window; the other half run to a median of 77 rows and a p90 of 261.
  Everything outside the window reaches the model only through top-3 vector
  recall.
- **Dreaming runs once per session, and sessions are resumed.** A
  relationship is swept exactly once, after its first 30-minute idle; 56% of
  user messages were sent after that stamp and were never extracted. There
  is no reset path.
- **Nothing records "what happened between us".** `event` is 2.6% of
  categorised memories in both model eras, structurally: roleplay content is
  diverted to other categories, the output schema has no date field, and the
  prompt's "worth remembering long-term" framing skips one-off happenings.
  Affinity tier transitions are timestamped but never read back as memories;
  accepted actions are recorded only in message metadata.
- **Relationships last.** 170 relationships span two or more days (median
  10 days, p90 67); 49 span 30+ days. Anniversaries have an audience.

Memory 2.0 makes the relationship layer a first-class product of the scan:
a running **relationship arc**, dated **episode summaries**, and a dated
**journal** of moments — all extracted passively from conversation, never
written by the user. Raw rows and their recall stay.

Replay on production relationships (48 scan points × 2 models, 360 calls)
settled the open prompt questions: one call producing memories, episode and
journal together had zero truncation at 1024 output tokens and zero parse
failures; journal candidates were 16/16 present and correctly attributed,
15/16 flagged real vs roleplay the way the judge read it under the earlier
"actually happened" definition (not re-scored under the initiative rule in
§5.1); splitting the journal into its own
call was strictly worse. One of the two candidate models never produces
journal entries at all, so a fallback to it silences the journal — the
engine must make that visible. Sequential arc rewrites grew without bound
(final versions 1,300–1,800 chars against a 400–600 brief), lost about
half of early key events, and after hand-checking the judge's flags still
carried roughly five genuine defects per final arc in each of three
classes (claims the transcript does not support, swapped attribution,
staged scenes written as lived); the engine caps the arc, rewrites less
often, pins an opening paragraph, and the rewrite prompt keeps roleplay
marked as roleplay.

## 2. Decisions

1. **Scan unit is the relationship, with a cursor.** One scan state row per
   persona instance (`instance_id` identifies the pair: an instance has one
   owner). The scan reads the pair's text rows after its cursor across all
   its sessions. `classified_at` is no longer written; its columns stay this
   release and are dropped in v2.6.1 (two-step removal).
2. **Trigger follows Insights 2.0:** scan inline when a relationship has
   accumulated **K = 20** new user turns since its cursor; a sweeper flushes
   relationships idle for **1 hour** that have **at least 3** pending user
   turns. The floor exists to stop one- and two-turn visits from producing
   an episode each (24–47% of flushed scans would otherwise cover ≤3 turns);
   a relationship that never reaches three pending turns is never scanned,
   accepted.
3. **One call, three outputs.** The scan prompt produces profile memories
   (the existing contract and iron rules), exactly one episode summary, and
   0–3 journal candidates. Measured: bundling costs ~15% of judged memory
   coverage and buys journal precision that a separate journal call loses.
4. **Episodes are append-only; the arc is a capped, rewritten narrative
   with a pinned opening.** Rewrite every **M = 4** episodes, reading only
   the opening, the previous arc and the unfolded episodes. Engine-side hard
   cap of 900 characters and a shrink guard; a rejected rewrite keeps the
   previous version. The opening (≤150 chars, "how it began") is written by
   the first rewrite and never rewritten.
5. **Journal has two sources.** LLM candidates from the scan (9 kinds,
   `realm` mandatory, date copied from a transcript stamp, source pointer
   to a message) and engine-written mechanism rows with no
   LLM: first meeting, first reach of an affinity tier (first-reach only,
   undone if the line drops back within 24 hours), accepted gifts, first
   accepted kiss. Journal rows carry a dedup key where only one per
   relationship makes sense.
6. **Recall:** the arc, the three most recent episodes, journal rows by
   semantic top-2 and raw rows by top-K form the new `[shared_memories]`,
   under a 1,200-character budget. Measured overlap between summaries and
   raw/categorised recall is zero under exact, Jaccard-0.6 and cosine-0.94
   rules, so `prune_recalled` stays exact-match and raw rows stay.
7. **Anniversaries render in `[now]` beside the holiday line**, user
   timezone required, at most two lines per turn. `first_meet` monthly
   anniversaries render on the day only; from the first year on, yearly with
   the holiday line's 6-day lookahead. Simulated, monthly × 7-day window
   would put first-year relationships in-window 23% of days and the launch
   cohort's month-anniversaries land together at 5× the median. Roleplay
   rows never render here.
8. **Fallback silence is surfaced, not prevented.** The scan records which
   configured hop served and how many journal rows it yielded; a scan served
   by a fallback hop logs at warn. A deployment that wants no fallback sets
   `fallback = []` on the task (already supported).
9. **Cursor starts at upgrade; history is an operator backfill.** The state
   row is created on a relationship's first post-upgrade turn with the
   cursor just before it. A `backfill-dreaming` subcommand walks older rows
   in scan-sized segments, with `--dry-run` reporting the call count first.
10. **Relationship products stay in the engine.** No export; the memories
    export keeps serving profile-layer categorised rows only. Archive and
    erasure remove everything new.
11. **Voice rows are scanned with the text rows of the same relationship;
    group sessions are not.** Voice calls reach memory only through
    dreaming (the 2026-08-09 voice ingestion design), so the scan keeps
    them: a voice turn bumps the pending count (no inline K-trigger on the
    voice path; the sweeper picks it up) and its rows enter the transcript
    with audio tags stripped, as today. `DREAMING_VOICE_DISABLED` stays as
    the exclusion switch. The relationship context (arc, episodes, journal)
    is injected on 1:1 text turns and the voice bootstrap; group seats keep
    raw recall only.
12. **Mechanism journal rows never call out to the embedding service.** The
    journal's `embedding` is nullable; engine rows are inserted inside the
    transaction that already knows the moment (the affinity event write, the
    action-response write) and the sweeper tick fills missing embeddings in
    one batch call. Semantic recall ignores rows without one.

## 3. Tables

Migration `0082`. All new tables get the standard lockdown block (RLS on,
REVOKE from anon/authenticated). No `user_id` column anywhere: the instance
determines the owner, and the house rule bars new external-identity columns.
Names are the builder's to adjust.

### 3.1 Scan state

```sql
CREATE TABLE engine.relationship_scan_state (
    instance_id          UUID PRIMARY KEY REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    scanned_through_at   TIMESTAMPTZ NOT NULL,   -- exclusive cursor on chat_messages.sent_at
    pending_turns        INT NOT NULL DEFAULT 0, -- user rows after the cursor
    last_turn_at         TIMESTAMPTZ,
    last_scanned_at      TIMESTAMPTZ,
    scan_failures        INT NOT NULL DEFAULT 0,
    claimed_at           TIMESTAMPTZ,
    user_timezone        TEXT,                   -- last valid IANA tz seen on a turn
    episodes_since_arc   INT NOT NULL DEFAULT 0,
    arc_failures         INT NOT NULL DEFAULT 0,
    backfilled_through_at TIMESTAMPTZ            -- §9; NULL = backfill not started
);
```

- `user_timezone` exists because the scan runs without a request: transcript
  stamps and journal dates need the user's day. Every 1:1 text turn that
  carries a valid tz refreshes it. Without one, stamps are UTC and the
  anniversary line never renders for that user anyway (same rule as the
  holiday line).
- Creation: on the first bump for an instance, `scanned_through_at` is set
  to the `sent_at` of the newest text row older than the triggering user
  row, or that user row's `sent_at` minus one microsecond when none exists.
  History before the cursor is the backfill's job (§9).

### 3.2 Episodes

```sql
CREATE TABLE engine.relationship_episodes (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    instance_id      UUID NOT NULL REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    summary          TEXT NOT NULL,              -- ≤ 400 chars, truncated on write
    started_at       TIMESTAMPTZ NOT NULL,       -- sent_at of first covered row
    ended_at         TIMESTAMPTZ NOT NULL,       -- sent_at of last covered row
    first_message_id UUID REFERENCES engine.chat_messages(id) ON DELETE SET NULL,
    last_message_id  UUID REFERENCES engine.chat_messages(id) ON DELETE SET NULL,
    folded           BOOLEAN NOT NULL DEFAULT false,  -- folded into the arc
    generation_id    TEXT,                       -- correlation id, not a reference
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- index (instance_id, ended_at DESC); indexes on both message FKs
```

`started_at`/`ended_at` come from the rows the engine read, not from the
model; the episode contract is a bare summary string.

### 3.3 Arc

```sql
CREATE TABLE engine.relationship_arcs (
    instance_id      UUID PRIMARY KEY REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    opening          TEXT NOT NULL,              -- ≤ 150 chars, written once
    arc              TEXT NOT NULL,              -- ≤ 900 chars
    previous_arc     TEXT,                       -- last accepted version before this one
    folded_through   TIMESTAMPTZ NOT NULL,       -- ended_at of the newest folded episode
    generation_id    TEXT,
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

### 3.4 Journal

```sql
CREATE TABLE engine.relationship_journal (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    instance_id      UUID NOT NULL REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    occurred_on      DATE NOT NULL,              -- user's local day
    kind             TEXT NOT NULL,              -- §5.1 vocabulary
    realm            TEXT NOT NULL CHECK (realm IN ('real','roleplay')),
    summary          TEXT NOT NULL,              -- one sentence, ≤ 200 chars
    source           TEXT NOT NULL CHECK (source IN ('llm','engine')),
    message_id       UUID REFERENCES engine.chat_messages(id) ON DELETE SET NULL,
    affinity_event_id BIGINT REFERENCES engine.companion_affinity_events(id) ON DELETE SET NULL,
    dedup_key        TEXT,                       -- §5.3; NULL = repeatable kind
    due_on           DATE,                       -- promise / shared_plan with a date
    recurrence       TEXT NOT NULL CHECK (recurrence IN ('none','monthly_then_yearly','on_due')),
    embedding        vector(512),                -- NULL until embedded (decision 12)
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- UNIQUE (instance_id, dedup_key) WHERE dedup_key IS NOT NULL
-- index (instance_id, occurred_on); partial index WHERE embedding IS NULL;
-- indexes on both FK columns; ivfflat cosine on embedding
```

`affinity_event_id` takes whatever type that table's `id` has. The journal
keeps its own embedding column and its own search, so the relationship raw
search in `companion_memories` needs no new filter. LLM rows are embedded
before insert (the scan is already off any transaction); engine rows are
inserted without an embedding and the sweeper tick embeds every
`embedding IS NULL` row in one `embed_documents` call before claiming
relationships. Searches add `embedding IS NOT NULL`.

### 3.5 Scan events

```sql
CREATE TABLE engine.relationship_scan_events (
    id               BIGSERIAL PRIMARY KEY,
    instance_id      UUID REFERENCES engine.persona_instances(id) ON DELETE SET NULL,
    kind             TEXT NOT NULL CHECK (kind IN ('scan','arc','backfill')),
    status           TEXT NOT NULL CHECK (status IN ('ok','empty','parse_error','rejected','failed')),
    model            TEXT,
    served_fallback  BOOLEAN NOT NULL DEFAULT false,
    rows_covered     INT NOT NULL DEFAULT 0,
    memories_written INT NOT NULL DEFAULT 0,
    episode_written  BOOLEAN NOT NULL DEFAULT false,
    journal_written  INT NOT NULL DEFAULT 0,
    generation_id    TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

Counts and ids only, no free text. This is the readout that lets an
operator see journal silence under fallback (decision 8) and the effect of
the arc guards; it carries nothing erasure would need to blank.

## 4. The scan

### 4.1 Trigger and claim

- **Bump.** `post_process::run` on a 1:1 text turn whose user row was
  persisted (not group; ghost and proactive turns do not count) upserts the
  state row: `pending_turns + 1`, `last_turn_at = now()`, `user_timezone`
  refreshed when the turn carried a valid one (the locale rides on the
  turn's `UserMsg`, so `run` gains a parameter for it). The voice route does
  the same bump after persisting a voice user row, without the K-trigger
  below. When the bump creates the row, the engine also writes the
  `first_meet` journal row (§5.2).
- **K-trigger.** When a text-turn bump returns `pending_turns ≥ 20`, the
  post-process task claims the row and scans inline (off the reply path, as
  the profile rebuild does).
- **Idle flush.** The dreaming sweeper (existing envs `DREAMING_DISABLED`,
  `DREAMING_TICK_SECS` default 300, `DREAMING_IDLE_SECS` default now 3600,
  `DREAMING_CLAIM_STALE_SECS` default 600, `DREAMING_VOICE_DISABLED`
  unchanged in meaning: voice rows leave the transcript and voice turns stop
  bumping) first embeds pending journal rows (decision 12), then claims rows
  with `pending_turns ≥ 3 AND last_turn_at < now() - idle AND (claimed_at IS
  NULL OR stale)`, batch 10, `FOR UPDATE SKIP LOCKED`, refreshing the claim
  before each relationship as `profile_rebuild` does. K, the floor and M are
  code constants beside `REBUILD_K`.
- The sweeper and the inline trigger are both inert when
  `[tasks.dreaming_scan]` is absent from the model config (today's
  behaviour for `memory_extraction`).

### 4.2 What a scan reads

Rows of the pair's non-archived text and voice sessions
(`chat_sessions.channel IN ('text','voice')`, `instance_id` = this
instance; voice sessions dropped when `DREAMING_VOICE_DISABLED` is set) with
`role IN ('user','assistant')`, `chat_messages.channel IS NULL OR 'voice'`,
`sent_at > scanned_through_at AND sent_at <= bound`, ordered by `sent_at`
across sessions, capped at **60 rows** per scan. `bound` is the triggering
turn's assistant row `sent_at` on the K path (the user row's when the turn
produced no assistant row) and `now()` on the idle path, so a turn in
flight is never half-read. When the cap cuts, the cursor advances to the
last row read, `pending_turns` is decremented by the user rows covered, and
the remainder waits for the next claim (the row is still due). The owner id
the profile-layer upsert needs comes from `persona_instances.owner_uid`.

Each row renders as `[#<index> YYYY-MM-DD HH:MM] 用户：…` / `AI：…` in the
state's timezone (UTC when none); the index is the journal source pointer.
The same timezone dates `occurred_on` when a journal candidate is
validated, so a stamp near midnight and the stored day can never disagree.
Voice assistant rows go through `strip_audio_tags` as today. User rows
render through `user_text_with_action`, and an action with a recorded
`response` appends the existing `action_outcome` wording, so the model sees
what was offered and whether it was accepted.

Writes at the end of a scan (episode, journal, cursor) are conditional on
the state row still existing and carrying this job's claim, mirroring
`replace_profile_if_claimed`: an archive landing mid-scan deletes the row
and the scan's products are discarded.

### 4.3 The call

Task **`dreaming_scan`**: deployment-owned `filter_prompt` (the contract
below; the in-repo example prompt is the full text), `max_tokens` example
1024, sent with the existing system audit user. `[tasks.memory_extraction]`
is retired: a block still present is ignored with a boot-time warning.

Contract (engine-parsed; the example prompt states it):

```json
{"memories": [{"content": "...", "category": "fact", "...": "metadata as today"}],
 "episode": "...",
 "journal": [{"kind": "confession", "realm": "real", "occurred_at": "2026-10-01",
              "summary": "...", "source": "#12", "due": "2026-12-24"}]}
```

- `memories`: exactly today's `MemoryCandidate` handling (category
  normalised to the five values, extra keys flattened into `metadata`),
  written to the profile layer with `session_id` = the session of the last
  covered row.
- `episode`: a string; empty or missing → the scan writes no episode but
  everything else proceeds (status `ok` with `episodes 0` is visible in the
  event row). Truncated to 400 chars on write.
- `journal`: each item validated engine-side — `kind` in the vocabulary (§5.1,
  `first_meet` excluded: it is system-written), else dropped; `realm` not
  `real`/`roleplay` → `roleplay`; `occurred_at` must equal the date of some
  covered row's stamp, else dropped; `source` must resolve to a covered row,
  else `message_id` NULL but the row is kept; `due` parsed into `due_on`
  only for `promise`/`shared_plan`. Summary embedded with `embed_document`.
- Outcome semantics mirror the profile rebuild: `ok`/`empty` → cursor
  advances, `pending_turns -= covered user rows`, `scan_failures = 0`,
  `claimed_at = NULL`. `parse_error` / transport failure → cursor stays,
  `scan_failures + 1`, `last_turn_at = now()` (one idle period of backoff),
  `claimed_at = NULL`; at three consecutive failures the cursor advances past
  the segment with a warning (the segment is lost to the scan but its raw
  rows remain), `scan_failures = 0`.
- Every scan writes one `relationship_scan_events` row (`kind = 'scan'`)
  with `served_fallback = (served_hop != primary)`. A fallback-served scan
  logs at warn with the journal count.

### 4.4 Arc rewrite

After an episode is written, `episodes_since_arc + 1`. At **4** the same job
runs task **`arc_rewrite`** (engine-owned prompt in `prompt.rs`; the task
block supplies model and parameters, `max_tokens` example 1500 — it must
sit well above the 900-char cap, because a model that writes long is cut
by the engine guard, not by the token limit: measured, every rewrite
"failure" at a tight token limit was a truncation, not a contract break).
Input: the
opening if one exists (marked read-only: "already stated, do not repeat"),
the previous arc if any, and every unfolded episode in time order with its
dates. Output `{"arc": "..."}`; on the first rewrite (no arc row)
`{"opening": "...", "arc": "..."}`.

Prompt rules carried over from the replay version: rewrite only from these
inputs; chronological; later state overrides earlier ("曾经…后来…"), earlier
state stays in the narrative; the further back, the more compressed;
subjects kept straight; roleplay visibly roleplay; 400–600 characters, no
lists or headings.

Engine guards, checked in this order:

1. Parse failure / transport failure → `rejected`/`failed`.
2. `arc` longer than **900** chars → `rejected`.
3. A previous arc exists, is over 300 chars, and the new one is shorter than
   **70%** of it → `rejected`.
4. First rewrite: `opening` missing or over 150 chars → `rejected`.

Accepted: `previous_arc = arc`, `arc = new`, `folded_through` = newest
folded episode's `ended_at`, those episodes `folded = true`,
`episodes_since_arc = 0`, `arc_failures = 0`. Rejected or failed:
`arc_failures + 1`, nothing written; at three consecutive the pending
episodes are marked folded anyway with a warning and the counter resets —
the previous arc keeps serving. One `relationship_scan_events` row per
attempt (`kind = 'arc'`).

## 5. Journal

### 5.1 Vocabulary

`first_meet` (engine), `tier_reached` (engine), `confession`,
`first_intimacy`, `conflict`, `reconciliation`, `promise`, `gift`,
`shared_plan`, `user_shared_milestone`. The LLM may emit the last eight;
`gift` and `first_intimacy` also arrive from the engine.

**`realm` is about initiative, not physical reality.** Everything between
the user and the character is virtual; the field separates two modes of
interaction inside that virtual relationship:

- `real` — the user treats the character as the other person: the user
  invites and the character agrees, or the character initiates, and the
  moment grows out of that exchange. These are the relationship's own
  milestones; they may recur as anniversaries.
- `roleplay` — the user treats the character as stage and cast: the user
  supplies the script, sets the scene, issues directions, or narrates the
  situation outright, and the character performs. Weddings, break-ups and
  the like inside such scenes are plot, not milestones; they never render in
  `[now]` and are marked as staged when recalled.

The test is who initiated and how the moment progressed, never how
explicit the content is: an intimate scene the character initiated within
its own persona is `real`; a coffee date the user staged line by line is
`roleplay`. A user's own first-person narration of what "happened" is
scene-setting by the user, hence `roleplay`. When in doubt the prompt says
`roleplay` — a staged moment celebrated as real is the worse failure. The
in-repo example prompt states the rule in these terms and never phrases
`real` as "actually happened": a model reading that as physical reality
flags everything as roleplay. Engine rows are
always `real` because the mechanism itself (an action offered and accepted,
a tier the relationship reached) is the exchange.

### 5.2 Engine-written rows (no LLM)

All `source = 'engine'`, `realm = 'real'`, summary text engine-rendered in
the prompt language with the label wording the `[relationship]` block already
uses, inserted without an embedding (decision 12), dated in the turn's user
timezone (UTC when absent). The tier rows are inserted by the store inside
`persist_with_event`'s transaction (it is where `label_changes` is known;
the method gains the inputs it needs — a published-crate signature change
covered by the release), the gift and kiss rows right after
`set_action_response` in the same store call.

| Moment | Where it is written | `kind` | `dedup_key` | `recurrence` |
|---|---|---|---|---|
| First message of the relationship | state-row creation (§4.1); `occurred_on` = date of the pair's earliest text row across non-archived sessions, so pre-existing relationships get their real date | `first_meet` | `first_meet` | `monthly_then_yearly` |
| A line's label moves up (`label_changes.bond.to` / `.chemistry.to` higher than `from`) | the affinity write in post-process, same transaction as the event row; `affinity_event_id` set | `tier_reached` | `bond:<label>` / `chemistry:<label>` | `none` |
| A line's label moves down | same place: delete this instance's `tier_reached` rows on that line whose tier is above the new label **and** `created_at > now() - 24h` | — | — | — |
| `give` / `gift` / `tip` action with `response = accept` | post-process, after `set_action_response` | `gift` | `gift:<user message id>` | `none` |
| `kiss` action with `response = accept` | same | `first_intimacy` | `first_intimacy:real` | `none` |

`hug`, `touch`, `lick` and `custom` actions are not journaled. First-reach
plus the 24-hour undo is the measured rule: 31% of tier ups reverse within a
day; the rule keeps 1,126 of 1,630 historical ups and 972 after first-reach
dedup, p50 4 per relationship. Affinity is per session; the dedup key is per
instance, so a tier re-reached in a later session is not re-celebrated.

### 5.3 LLM rows

`source = 'llm'`, `message_id` from `source`. `dedup_key` is
`first_intimacy:<realm>` for `first_intimacy`, `gift:<message_id>` for a
`gift` whose source resolved to a message (so a gift the engine already
journaled at the turn is not journaled twice by the scan), and NULL
otherwise. Inserts use `ON CONFLICT DO NOTHING`: whichever source lands
first wins, which for mechanism moments is always the engine. `recurrence`
is `on_due` when `due_on` is set, else `none`.

### 5.4 Anniversary line in `[now]`

Loaded with the relationship context (§6): this instance's journal rows with
`recurrence <> 'none'` and `realm = 'real'`. Requires the request's user
timezone, like the holiday line; the clock line keeps its persona-tz rule.
With local date `D`:

- `first_meet`: for months 1–11 after `occurred_on`, the same day-of-month
  (clamped to month end) renders **only when it equals `D`**; from 12 months
  on, the yearly date renders when it falls in `[D, D+6]`, phrased like the
  holiday line (今天 / 明天 / N 天后).
- `on_due`: renders when `due_on = D`.

At most **two** lines per turn, `first_meet` first, then by date. Wording
states the fact only, no instruction, one line each, emitted right after the
birthday line. `NowContext` gains an `anniversaries` list that the turn
computes from the loaded rows and passes in; `for_user` stays sync.

## 6. Injection

### 6.1 Loading

One new store call, `load_relationship_context(instance_id)`, returns the
arc row, the three most recent episodes by `ended_at` (folded or not), and
the recurring journal rows — a handful of indexed reads, run beside
`character_insights` in the sequential chain of `build_reply_request_with`
(1:1 text turns only; group passes `None`). Journal semantic recall reuses
the turn's query embedding (as world and stories do): top-2 by cosine over
this instance's journal rows, only when the relationship layer is on.

### 6.2 `[shared_memories]`

```
[shared_memories]
{opening}{arc}                         ← constant, one paragraph
- 2026-10-12：{episode summary}        ← up to 3, oldest first
- 2026-10-20：{episode summary}
- 2026-08-15（扮演）：{journal summary}  ← semantic top-2, dated; roleplay marked
- 用户：…                              ← raw top-K, as today
```

- `memory_scope` resolves as today: everything here is relationship-layer
  and follows its Y flag; the affinity gate's `relationship_k` applies to
  raw rows only. The arc, episodes and journal render at every tier: they
  are this relationship's own memory, not profile leakage.
- `prune_recalled` stays exact-match; it runs over raw rows only.
- **Budget: 1,200 characters** for the relationship part of the block (the
  `[world_memories]` / `[world_stories]` / `[character_state]` sections that
  follow under the same header are outside it). Cut order when over:
  episodes oldest-first, then raw rows lowest-ranked first, then journal
  rows. The arc is never cut here (it is capped on write).
- Empty everything → today's placeholder. Any part present → no placeholder.
- The ordering test ("everything before `[user_profile]` is byte-identical
  across turns") is unaffected; the block already varies per turn.

### 6.3 Voice

The bootstrap snapshot gains a `[你们的故事]` section carrying opening + arc
(constant text, not on the 300 ms per-turn budget). Per-turn voice recall is
unchanged; journal search does not run on voice.

## 7. Prompt contract changes for deployments

- `[tasks.memory_extraction]` → `[tasks.dreaming_scan]`, `filter_prompt`
  rewritten to the three-output contract (§4.3). The memories section keeps
  the production rules verbatim (only-the-user, roleplay-as-expression,
  refusal-text exclusion); the journal section tells the model that gifts
  and kisses offered through the action mechanism are recorded by the
  system (the dedup key enforces it regardless). Required for the scanner
  to run at all.
- New `[tasks.arc_rewrite]`: model, `max_tokens`, sampling. No prompt field
  (engine-owned). Missing block → arc rewrites are skipped with a one-time
  warning; episodes and journal still run.
- Both names join `KNOWN_CHAT_TASKS`; `validate_extraction_prompts` covers
  `dreaming_scan`.

## 8. Archive and erasure

- `archive_relationship(user, instance)` additionally deletes this
  instance's rows in `relationship_scan_state`, `relationship_episodes`,
  `relationship_arcs`, `relationship_journal`. Scan events stay (telemetry,
  `instance_id` set NULL only when the instance itself goes). A relationship
  restarted on the same instance gets a fresh `first_meet` from its first
  non-archived row.
- `erasure.rs` `DERIVED` gains the same four tables under `BY_INSTANCE`;
  `relationship_scan_events` is listed as kept (no payload to blank). The
  erasure test's table lists and the docs' per-table report example are
  updated in the same change.

## 9. Backfill

`eros-engine backfill-dreaming [--dry-run] [--min-rows 20] [--limit N]
[--instance <uuid>]`, run by the operator against the same config and
database as `serve`:

- Candidates: instances whose non-archived text rows older than
  `scanned_through_at` (or all rows, for instances with no state row yet —
  the command creates the state row with the cursor at the newest row)
  number at least `--min-rows`, excluding rows already before
  `backfilled_through_at`.
- Each instance's history is walked oldest-first in segments of at most 60
  rows cut at user-turn boundaries, each segment through the normal scan
  path (§4.2–4.4, `kind = 'backfill'` on the event row), advancing
  `backfilled_through_at`. Arc rewrites fold as episodes accumulate.
- Profile memories are written only from segments whose last row is later
  than the covering session's `classified_at` (or whose session was never
  stamped), so rows the retired sweeper already extracted are not extracted
  twice; episode and journal are always written. This is the last reader of
  `classified_at`.
- `--dry-run` prints per-instance segment counts and the total number of
  scan calls (plus the implied arc rewrites) and exits. A real run prints
  the same table, then runs, then prints per-status counts. Resumable: an
  interrupted run restarts from each instance's `backfilled_through_at`.
- The command is never scheduled; it is a manual operator step, like
  `seed-personas`.

## 10. Rollback and versioning

- New tables are invisible to the previous image. `classified_at` is no
  longer written, so a rolled-back image re-sweeps unstamped sessions once
  under the old per-session rule — accepted. The old image must not run
  `migrate`.
- v2.6.1 drops `chat_sessions.classified_at` and
  `classification_claimed_at` once 2.6.0 has held.
- **Version v2.6.0.** Migration guide covers: the retired
  `memory_extraction` block and the new `dreaming_scan` contract, the
  `arc_rewrite` block, the new idle default, the backfill command and its
  cost model (calls ≈ segments,
  reported by `--dry-run`), the fallback/journal asymmetry, and the archive
  and erasure coverage.

## 11. Out of scope

- User-facing read or delete of episodes, arcs or journal rows; any export
  of relationship products.
- Group scanning; journal semantic recall on voice.
- Semantic dedup between summaries and raw/categorised recall (measured
  unnecessary).
- Any change to the profile layer's write paths, the fact ledger, or the
  memories export.
- Model and parameter choice for the two tasks (deployment config).
- Dropping the `classified_at` columns (v2.6.1).

## 12. Testing

- **Store:** state bump/claim/claim_due/finish semantics including the
  3-turn floor and the three-failure cursor advance; cursor placement on
  creation; segment read respects sessions, channel, bound and the 60-row
  cap; episode truncation; arc guards (cap, shrink, opening-once,
  three-failure fold); journal validation (vocab, realm fold, date must match
  a stamp, source resolution, dedup conflict); tier-reached first-reach and
  24-hour undo; gift and kiss rows; archive deletes the four tables and not
  the events; erasure covers the four and keeps events.
- **Render:** transcript stamp format and timezone; `[shared_memories]`
  shape, budget cut order, placeholder only when empty; anniversary line
  day-of vs lookahead rules, clamping, two-line cap, no tz → no line,
  roleplay excluded; voice bootstrap section.
- **Pipeline:** K-trigger inline, idle flush with floor, voice bump without
  K-trigger and voice rows in the transcript (the existing voice ingestion
  locks carry over), `DREAMING_VOICE_DISABLED` excludes both, pending
  journal embeddings filled on tick, fallback-served event row and warn,
  missing task blocks → inert / skipped, mid-scan archive discards the
  scan's writes.
- **CLI:** dry-run counts; segment boundaries; `classified_at` memory
  skip; resumability.
- **OpenAPI:** unchanged paths; the archive endpoint's doc text.

## 13. Documentation (separate PR after the code PR)

- `docs/memory-layers.md` + `.zh.md`: the relationship layer's new products,
  the scan trigger, the new block shape and budget, the anniversary line.
- `docs/model-config.md` + `.zh.md`: `dreaming_scan` contract and
  `arc_rewrite`; `examples/model_config.toml` and `model_config.d/memory.toml`
  carry the full example prompt.
- `docs/deploying.md` + `.zh.md`: env changes, backfill command.
- `docs/llm-audit.md` + `.zh.md`: `relationship_scan_events`.
- `docs/api-reference.md` + `.zh.md`: archive semantics, erase report.
- `.env.example`; `docs/migrating/v2-6-0-memory-2-0.md`.

## 14. Left to the builder

- Final table, column, task-constant and function names; whether
  `folded_through` lives on the arc row or the state row.
- Exact wording of engine-rendered journal summaries, the anniversary lines
  and the transcript stamp; the opening's placement when rendering (first
  sentence vs separate line).
- How the backfill walks sessions in `sent_at` order across a pair's
  sessions and where it cuts segments at user-turn boundaries.
- Claim refresh cadence in the backfill; whether it reuses the sweeper's
  claim or runs with the sweeper disabled (document the expectation either
  way).
- Test fixtures and placement; the example prompt's final text (contract
  fixed by §4.3).
