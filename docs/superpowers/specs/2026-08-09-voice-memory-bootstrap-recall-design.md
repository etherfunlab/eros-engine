# Voice memory: bootstrap snapshot + per-turn recall — design

- Date: 2026-08-09
- Status: design agreed, implementation plan pending
- Repo: `eros-engine`
- Amends: `2026-07-07-voice-call-parts-design.md` (thin-prompt & memory-exclusion sections)

## Background & goals

The voice turn's prompt is deliberately thin: persona + voice directive + one
relationship line + the last 12 in-session turns. Users notice the companion is
amnesiac on calls — no structured profile, no long-term memory, no awareness of
the previous call. This spec adds *just enough* memory without losing the
thin/low-latency character:

1. **Session bootstrap (first turn)** — freeze a snapshot of the user's
   structured profile (`human_insights` bullets) plus the tail of the previous
   voice call, and inject it every turn as a static prompt prefix.
2. **Per turn** — vector recall only (read-only), time-budgeted, silently
   degrading. No writes of any kind on the voice path.
3. **Best practice becomes one session per call** — enabled by a new
   `force_new` flag on `chat/start`.

So a voice call's memory comes from: the bootstrap snapshot (insights +
previous-call transcript), the per-turn vector recall, and the in-session
8-message window.

## Non-goals

- No per-turn memory writes, no insight extraction, no affinity eval on voice
  (all unchanged).
- No dreaming changes. Post-call vector writes of voice content are a planned
  **follow-up PR** (below); this spec only lays its hooks.
- No STT/TTS or audio concerns (unchanged).
- No changes to the text path's prompt, recall tiers, or write pipeline.

## Prerequisite (separate small PR, ships first)

**Affinity dead-row fix.** `companion_affinity` is keyed one row per session
(`session_id UNIQUE`), and rows are only ever created by the text pipeline —
so a voice-channel session never has one, `AffinityRepo::load(session_id)` is
always `None` on voice, and the relationship line (PR #209) never renders in
production. Fix: resolve affinity for voice via user × instance (the latest
text-session affinity row). That resolution is naturally compatible with
per-call voice sessions. No hard ordering dependency with this spec, but it
ships first as an independent bug fix.

## Follow-up (out of scope, designed for)

**Post-call vector writes via dreaming-lite.** A later PR flips the
`channel IS NULL` filters in `pipeline/dreaming.rs` (session claim + message
read) so ended voice sessions are swept and their content becomes
`companion_memories` rows — making previous calls reachable by per-turn recall
too. Voice content must only ever be written **after** a call ends, never
per-turn. Hooks this spec lays:

- Voice inserts bump `last_active_at`, giving the sweeper a correct
  "call ended + idle" signal (without it, a long call would be swept
  mid-call, and per-call ordering would be wrong).
- Per-call sessions give dreaming a natural one-call classification unit.

Once that lands, the previous call appears both verbatim (bootstrap tail) and
semantically (recall snippets) — complementary, not redundant; minor overlap
is acceptable.

## 1. `force_new` on `chat/start`

`StartChatRequest` gains `force_new: Option<bool>` (default `false`, backwards
compatible). `true` skips `resume_latest_session` and always creates a fresh
session (`is_new: true`). Works for any channel — this completes the half of
issue #157 that the channel-scoping PR (#158) left out.

Consumer best practice: start every call with
`{"channel": "voice", "force_new": true}`.

OpenAPI + `docs/api-reference.md` (+ zh mirror) updated.

## 2. History window 12 → 8

`VOICE_HISTORY_WINDOW` drops from 12 to 8 — same unit as today (8 messages =
4 user/assistant exchanges).

## 3. First-turn bootstrap snapshot

**Trigger**: the session row's `metadata` lacks the `voice_bootstrap` key.
Checked every turn for free (the route already loads the full session row);
in practice only the first successful turn assembles it. Keying on the
metadata marker — not on history length — self-heals a failed first turn
(user row persisted, generation died, client retried).

**Assembly** (two parts, each degrades independently — a failed read omits
that part this turn and leaves the marker unwritten so the next turn retries;
a successful-but-empty read writes the marker with that part empty):

- `insights` — `human_insights` rendered by the existing
  `human_insights_to_bullets`, tier = the **first turn's** resolved
  `InsightMode` (default `neutral_and_relationship` ⇒ Neutral).
- `prev_call` — the latest sibling voice session
  (`user_id × instance_id`, `channel = 'voice'`, `id != current`,
  `ORDER BY last_active_at DESC LIMIT 1`), its last 8 messages rendered as a
  plain transcript. No sibling ⇒ part omitted (first-ever call).

**Persistence**: one conditional jsonb UPDATE that only writes when the key is
absent — idempotent under concurrent first turns:
`metadata.voice_bootstrap = { insights, prev_call, prev_session_id, created_at }`.
UPDATE failure (a query `Err`) ⇒ use the in-memory copy this turn, warn, retry
next turn. `rows_affected == 0` (the key was already there — a concurrent
first turn won the race) is different: the loser reloads and injects the
*winner's* stored snapshot instead of its own, falling back to its in-memory
copy only if that reload itself fails (query `Err`, key missing, malformed).

**Injection** (every turn) — system prompt order:

```
persona → directive → bootstrap block → relationship line → recall block
```

The bootstrap block renders as `[关于他]` (insights) + `[上次通话]`
(prev_call transcript). Static content first, per-turn recall last: the
prefix is byte-stable for the whole call, so provider-side prefix caching
keeps working. Reading the snapshot costs zero extra queries.

**Frozen semantics**: the snapshot's insight tier is decided by the first
successfully-assembling turn's `memory_scope` and never changes mid-call.
Later turns' `memory_scope` gates that turn's recall layers only.

**Reused sessions**: per-call sessions are a best practice the engine cannot
enforce. A downstream that keeps reusing one voice session gets the correct
no-re-injection behavior (the marker never rewrites) but keeps the original
snapshot forever; `created_at` is recorded in the snapshot should a refresh
policy ever be wanted.

## 4. `memory_scope` on the voice request

`VoiceTurnRequest` gains `memory_scope: Option<MemoryScope>` — same field
name, enum, wire values, and default as the chat stream
(`neutral_and_relationship`). On voice it means:

- **First successfully-assembling turn**: the resolved `InsightMode` picks the
  bootstrap insight tier — Neutral by default; `full` / `insights_only` give
  Full; `relationship_only` / `none` give Off (no insight part for the whole
  call).
- **Every turn**: the resolved `(x_on, y_on)` gates that turn's recall layers,
  exactly like chat.
- `relationship_scope` is unchanged and orthogonal.

Neutral-by-default is deliberate: a call is a more intimate setting than text,
and a companion that pre-knows the Full-tier fields (love values, relationship
history, family, finances) reads as creepy. Downstream can opt up via
`memory_scope`.

## 5. Per-turn vector recall (read-only)

- **Query text**: `req.content` verbatim (voice has no input-filter rewrite).
- **Backchannel skip**: calls are full of backchannel utterances
  (嗯 / 好啊 / 哈哈). After trimming whitespace and punctuation, content
  shorter than a named threshold (default 4 chars) skips recall entirely —
  no embedding round trip, no queries. Bootstrap + history still carry the
  turn. This removes a sizable share of per-turn round trips at zero risk.
- **Concurrency**: `embed_query` runs `join!`-ed with the persona / affinity /
  history loads. The whole recall future (embed + pgvector) is wrapped in a
  **300 ms budget** (named constant; tune down with real-world latency data);
  timeout or error ⇒ no recall block, warn log, the reply is never blocked
  (the text path's silent-degradation pattern).
- **Queries**: reuse `recall_memory_with_embedding` + `memory_hygiene` with
  the K constants parameterized. Voice tier: grouped 1/category + raw
  fallback 2 + relationship 2. Text tier (2 / 4 / 3) unchanged.
- **Rendering**: reuse the `[user_profile]` / `[shared_memories]` vocabulary
  via a helper extracted from `build_prompt`; the block sits at the end of the
  system prompt (dynamic content last).
- **No similarity threshold** — deliberate: the store returns pure top-K and
  plumbs no score (parity with the text path), and the backchannel skip
  removes the worst garbage-nearest-neighbor case (meaningless short
  queries). Revisit only if call transcripts show noise.
- **Config kill-switch**: `[tasks.chat_voice] recall = false` (default true).
  Config wins over any per-request `memory_scope`. Embedding availability
  follows the same `[tasks.embedding]` resolution as the text path.
- **No writes**: no `embed_document`, no memory inserts, no post-process on
  voice (unchanged).

## 6. `last_active_at` bump (reverses a 2026-07-07 decision)

`insert_voice_user_message` / `insert_voice_assistant_message` now bump
`chat_sessions.last_active_at` (parity with `append_message`). Motivations:
correct "previous call" ordering for the bootstrap; the idle signal the
dreaming follow-up needs; no effect on the text path. The original reason for
not bumping — keeping voice out of the recency/dreaming machinery — is
obsoleted by the follow-up premise. Dreaming still filters voice out until the
follow-up PR flips it.

## 7. Error handling

Every new path degrades and none blocks: bootstrap parts are omitted
independently; a snapshot write failure falls back to the in-memory copy and
retries next turn; recall timeout/failure just drops the recall block. All
warn-level logs. Never an `error` frame for a memory problem; the reply always
ships.

## 8. Testing

- `force_new`: `true` creates a fresh session; default still resumes; both
  channels covered.
- Bootstrap: first turn writes the metadata marker; second turn does not
  rewrite; no sibling ⇒ insights-only snapshot; sibling selection excludes
  the current session and non-voice channels; tier follows the first turn's
  `memory_scope` (default Neutral; `none` ⇒ no insight part); failed first
  turn retries on the next turn.
- Injection: every turn's system prompt contains the bootstrap block; recall
  block is last; window is 8.
- Recall: embed failure and timeout degrade to no block; voice K tier
  asserted; `recall = false` kills it; `memory_scope: "none"` issues no
  embedding call and no pgvector queries.
- Bump: both voice inserts advance `last_active_at`.
- Regression locks: text-path prompt unchanged; dreaming still excludes voice
  at this spec's stage.

## Resolved decisions (Q&A log)

- **Bootstrap source = previous voice session**, not the latest text session:
  the text side is already covered by insights + vector recall, while a
  previous call's content is otherwise unreachable (voice is never written to
  memory until the follow-up PR).
- **Insight tier default = Neutral**, downstream-tunable via `memory_scope`
  (Full pre-knowledge is creepy on the more intimate voice channel).
- **Field name = `memory_scope`** for exact chat parity — same enum, same
  default, no new concepts.
- **"8 turns" = 8 messages** (4 exchanges), the same unit as the old
  `VOICE_HISTORY_WINDOW = 12`.
- **Per-turn writes rejected**; voice content enters vector memory only via
  the post-call dreaming follow-up.
- **Cross-turn dedup of recalled memories rejected**: every turn is a
  stateless re-send — earlier turns' system prompts are not in the message
  history, so a memory suppressed at turn N is simply absent from turn N's
  request. Re-injecting the currently-relevant snippets each turn is correct,
  not waste. Same-turn cross-layer dedup (`memory_hygiene`) still applies.
- **Injecting both `companion_insights` and `human_insights` rejected**:
  `human_insights` is the projected rendering of the same JSONB — both would
  be double tokens, zero information.
- **Field-latency fallback recorded, not chosen**: if per-turn recall proves
  too slow in the field, fold a one-shot top-K memory digest into the
  bootstrap and disable per-turn recall — zero per-turn cost, at the price of
  losing mid-call topicality.
