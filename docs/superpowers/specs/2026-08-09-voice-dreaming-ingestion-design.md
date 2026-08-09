# Voice post-call memory ingestion via dreaming-lite — design

- Date: 2026-08-09
- Status: design agreed, implementation plan pending
- Repo: `eros-engine`
- Flips the "single, reversible switch" reserved by
  `2026-07-07-voice-call-parts-design.md` (Memory exclusion); implements the
  Follow-up section of `2026-08-09-voice-memory-bootstrap-recall-design.md`.

## Background & goals

Since #235 the voice path **reads** memory (first-turn bootstrap snapshot +
per-turn vector recall) but still never **writes** it: the dreaming sweeper's
message read filters `channel IS NULL`, so voice content never becomes
`companion_memories` rows. Consequence: what a user says on a call is
unreachable from any later conversation except the very next call's verbatim
bootstrap tail.

This spec makes an ended voice call feed the same dreaming-lite pipeline text
sessions use: post-call, the sweeper extracts categorized profile-layer
memories from the call transcript. Later calls' per-turn recall — and text
chats' recall — then retrieve previous calls semantically.

**Invariant preserved**: voice writes happen **after the call only**, never
per-turn. The per-turn voice path stays read-only exactly as #235 shipped it.

## Non-goals

- No per-turn writes on the voice path (unchanged invariant).
- No insight extraction from voice — `post_process` still does not run on
  voice turns; `companion_insights` / `human_insights` are untouched by this
  change. (Whether voice should eventually feed insights is a separate,
  later decision.)
- No relationship-layer writes from voice. Dreaming writes profile-layer
  rows only (existing behavior, kept); the relationship layer's raw-turn
  writer remains text-only.
- No affinity eval on voice (unchanged).
- No changes to the `[tasks.memory_extraction]` prompt. Spoken-style
  transcripts are a tuning surface for later, not this spec.
- No change to the claim/eligibility machinery beyond what the flag gates.

## Design

### 1. The flip — one filter, one place

`classify_session`'s message read in `pipeline/dreaming.rs` (today
`AND channel IS NULL`) becomes:

```sql
AND (channel IS NULL OR channel = 'voice')
```

`product_qa` stays excluded. The claim/eligibility query is already
channel-blind — voice sessions are claimed and stamped today, reading zero
rows — so the claim side does not change.

### 2. "Call ended" signal — already in place

Eligibility remains `last_active_at` idle ≥ `DREAMING_IDLE_SECS` (default
1800 s). Voice inserts bump `last_active_at` since #235, so a live call
re-arms the timer on every turn and a session is swept no earlier than the
idle window after its final turn. Per-call sessions (the `force_new` best
practice) give dreaming a natural one-call classification unit. No new guard
is needed.

### 3. Opt-out flag — `DREAMING_VOICE_DISABLED`

New env var in the `DREAMING_*` family (`ServerConfig`), default **false**
(voice ingestion ON). When true, `classify_session` keeps the old
`channel IS NULL` filter — voice sessions are still claimed and stamped with
zero rows, byte-for-byte today's behavior.

The flag exists because this is a privacy-relevant behavior change: call
content moves from "never persisted as memory" to "distilled into memory
post-call". Deployments keep a zero-code way back to text-only memories.

Semantics note: classification stamps `classified_at` once. Calls swept
while the flag was ON (disabled) are never re-swept after turning it OFF —
consistent with existing classification semantics; documented, not worked
around.

### 4. Transcript hygiene — audio-tag strip

Deployments with `tts_audio_tags = true` have assistant voice rows carrying
inline TTS tags (`[laughs]`, `[whispers]`, …). Before the transcript is fed
to the extraction prompt, strip bracketed audio tags from **voice assistant
rows only** (conservative pattern: `[` + lowercase ASCII words/spaces + `]`),
collapsing doubled whitespace. User rows and non-voice rows are untouched.
This keeps stage directions out of extracted memory text.

### 5. What gets written — nothing new

The existing pipeline unchanged: profile-layer `companion_memories` rows
with `category` (fact/preference/event/emotion/relation) + `metadata`, one
`embed_document` per candidate, `session_id` = the voice session. Recall
needs zero changes — voice-derived memories are ordinary rows, retrievable
by text and voice alike.

### 6. Interaction with the bootstrap `prev_call` block

Once this lands, the previous call is reachable both verbatim (bootstrap
tail) and semantically (recalled snippets). Complementary by design, as the
voice-memory spec states; overlap is acceptable and cross-block dedup stays
rejected (the frozen snapshot's byte-stability forbids it).

### 7. Cost note

Each non-empty voice call now costs one post-call `memory_extraction` LLM
call plus one `embed_document` per extracted candidate (today: zero). An
empty call stamps without an LLM call (existing behavior). Levers:
`DREAMING_VOICE_DISABLED`, `DREAMING_IDLE_SECS`. Heavy per-call voice
traffic also transiently occupies claim-batch slots (oldest-first ordering);
self-clearing, noted from the #235 final review.

### 8. Error handling

Dreaming's existing semantics carry over untouched: atomic claim with stale
recovery, per-candidate embed, `memory_extraction` status audit rows,
stamp-on-completion.

## Testing

- **Flip**: voice rows now reach the extraction transcript. The existing
  regression lock `classify_session_excludes_voice_rows` is **inverted by
  this spec** — rewritten as `classify_session_includes_voice_rows` — and a
  new `classify_session_excludes_product_qa_rows` pins the boundary that
  must not move. (Assertion change is spec-mandated; reviewers should treat
  it as sanctioned, not as a weakened lock.)
- **Flag**: `DREAMING_VOICE_DISABLED=1` restores the old filter — voice rows
  excluded again, stamping unchanged.
- **Audio-tag strip**: unit table — single tag, multiple/interspersed tags,
  tag-only content, user rows untouched, non-voice assistant rows untouched,
  no double spaces left behind.
- **E2E**: idle voice session with a seeded transcript → sweeper tick →
  categorized profile-layer rows exist and the session is stamped; a
  recently-active voice session is not swept; an ended call's memories are
  retrievable via the existing recall path.
- Regression: text-session sweeping byte-identical; `product_qa` excluded.

## Docs

- `docs/memory-layers.md` (+zh): the voice section's "voice writes no
  memories" claim becomes "distilled post-call by dreaming-lite" with the
  flag documented.
- `docs/api-reference.md` (+zh): the voice-section sentence on memory
  writes updated.
- `.env.example`: `DREAMING_VOICE_DISABLED` documented beside the other
  `DREAMING_*` vars.
- New zh text in 简体中文; en+zh in the same commit.
- The 2026-07-07 spec's "Memory exclusion" section is historical and stays
  as written; this spec records that its reserved switch is now flipped.

## Resolved decisions (Q&A log)

- **Opt-out = env flag `DREAMING_VOICE_DISABLED`** (default: ingestion on) —
  chosen over "no switch" (deployments would need a source edit to reclaim
  text-only memories) and over a model_config key (dreaming's switches
  historically live in env).
- **Post-call only, never per-turn** — user-set premise, restated as the
  spec's core invariant.
- **Profile layer only, extraction prompt reused as-is** — parity with text
  dreaming; divergence is future tuning, not this change.
