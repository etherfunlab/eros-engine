# Memory layers

[English](memory-layers.md) · [中文](memory-layers.zh.md)

A persona remembers two kinds of things: what it has learned about the user,
and what has happened between the two of them. The first is the **profile
layer**, which any persona the user talks to can draw on. The second is the
**relationship layer**, kept per persona instance: the user's own words to
this persona, and the relationship's story as the engine writes it up from
the conversation — a running arc, dated episodes, and a journal of moments
that come back on their anniversaries.

This page covers how both layers are written, stored, recalled and put into
the prompt. The model before v2.6.0 is documented as
[memory (legacy)](memory-layers_legacy.md); the
[v2.6.0 migration guide](migrating/v2-6-0-memory.md) covers the upgrade.

## How it fits together

Four writers, each on its own schedule:

| Writer | When it runs | What it writes |
|--------|--------------|----------------|
| [Raw-turn writer](#the-raw-turn-writer) | Every substantive turn, in post-processing | The user's line, embedded, into both layers of `companion_memories` |
| [Dreaming](#dreaming) (`[tasks.dreaming_scan]`) | Per relationship, at 20 pending turns or once the relationship has gone idle | Profile memories, one episode, journal moments |
| [Arc rewrite](#the-arc) (`[tasks.arc_rewrite]`) | Every fourth episode | The relationship's arc |
| [The engine itself](#the-journal) | At the turn the moment happens, no LLM call | Journal rows for the first meeting, tiers reached, gifts and the first kiss |

And what reaches a 1:1 prompt:

| Prompt part | Comes from |
|-------------|------------|
| 基础画像 bullets | `human_insights`, the profile the [insights](insights.md) pipeline derives (not part of this page's tables) |
| `[shared_memories]` | Profile memories recalled by similarity and grouped by category; the relationship's recalled raw lines; its arc, latest episodes and nearest journal moments |
| Anniversary lines under `[now]` | The journal's recurring rows |

How much of it reaches the prompt is set by the request's `memory_scope` and,
within that, by the relationship's affinity ([Affinity gate](#affinity-gate)).

## Profile and relationship

| Layer | Keyed by | What it holds | Lifetime |
|-------|----------|---------------|----------|
| **Profile** | the user (`instance_id` NULL) | Facts about the user that any persona can come to know as it gets close to them | Permanent, until the user is erased |
| **Relationship** | the persona instance | What the user said to this persona, plus the relationship's arc, episodes and journal | Until the relationship is archived |

The split exists because **stability across personas** and **intimacy within
a relationship** are different things. If you tell Aria you're allergic to
peanuts, that is a profile fact: Kenji can know it too, once he and the user
are past strangers. If Aria mentioned she is reading Bishop tonight, that
belongs to the relationship: Kenji shouldn't pretend to know.

## Storage

**`companion_memories`** holds both layers' recallable rows, told apart by
`instance_id`:

```sql
CREATE TABLE engine.companion_memories (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id   UUID NOT NULL REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    user_id      UUID NOT NULL,
    instance_id  UUID,                         -- NULL = profile layer
    content      TEXT NOT NULL,
    embedding    VECTOR(512) NOT NULL,
    category     TEXT,                         -- fact|preference|event|emotion|relation; NULL on raw-turn rows
    metadata     JSONB,                        -- opaque per-memory payload from the scan; NULL on raw-turn rows
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

Two filtered indexes, one per layer, keep the hot path cheap:

```sql
CREATE INDEX idx_memories_user_profile
  ON engine.companion_memories(user_id)
  WHERE instance_id IS NULL;

CREATE INDEX idx_memories_session
  ON engine.companion_memories(session_id)
  WHERE instance_id IS NOT NULL;
```

**The relationship's story** lives in its own tables, keyed by the persona
instance. None of them carries a `user_id`; the instance determines the
owner.

| Table | One row per | What it holds |
|-------|-------------|---------------|
| `relationship_episodes` | scanned stretch of conversation | A summary of at most 400 characters, with `started_at` / `ended_at` taken from the first and last row it covers, and whether the arc has folded it in yet. Append-only. |
| `relationship_arcs` | relationship | `opening` (how it began, at most 150 characters, written once), `arc` (the story so far, at most 900 characters) and `previous_arc`. |
| `relationship_journal` | moment | `occurred_on` (the user's local day), `kind`, `realm` (`real` or `roleplay`), a one-sentence `summary` of at most 200 characters, an optional `due_on`, and an embedding for semantic recall. |
| `relationship_scan_state` | relationship | The scan's bookkeeping: cursor, pending-turn count, claim, failure counts, the user's last known timezone. |
| `relationship_scan_events` | scan or arc attempt | Counts and ids only, for operators ([LLM audit](llm-audit.md#relationship-scanner)). Kept when the relationship is archived. |

## Embedding

`voyage-4-lite` through Voyage's native API by default: 512 dimensions,
multilingual. The schema is `VECTOR(512)`, and non-Voyage routes have
`dimensions: 512` forced on the wire.

`[tasks.embedding]` picks the route. A `@provider` suffix on `model` sends the
calls elsewhere: `@openrouter` uses the built-in OpenRouter embeddings
endpoint (URL overridable via `[providers].openrouter.embeddings`), and
`@<name>` uses any `[providers]` entry that declares an `embeddings` URL (key
read from `<NAME>_API_KEY`). Alternatively a `model_read`/`model_write` pair
splits the recall and storage models. The pair must appear together, is
mutually exclusive with `model`, and is restricted to Voyage models of the
voyage-4 series and above, the only lineup that guarantees one shared vector
space across model sizes.

```rust
// crates/eros-engine-llm/src/embedding.rs — EmbeddingRouter
pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>, LlmError>;    // read backend
pub async fn embed_document(&self, text: &str) -> Result<Vec<f32>, LlmError>; // write backend
```

`embed_query` serves per-turn recall, `embed_document` every storage path. On
the Voyage backend the two become different `input_type` hints (`query` vs
`document`); the OpenRouter-compatible wire has no such hint, so there the
split only matters when `model_read` and `model_write` actually differ.

A missing key fails the boot. The `VOYAGE_API_KEY` check fires only when the
resolved route includes a Voyage backend; OpenRouter and custom routes are
checked on their own keys (`OPENROUTER_API_KEY` / `<NAME>_API_KEY`).

## The raw-turn writer

`write_turn` runs in post-processing on **every** substantive turn (a
non-empty user utterance and at least one non-empty assistant message). It
embeds **the user's utterance only**, into both layers, with
`category = NULL` and `metadata = NULL`. The relationship-layer copy is
prefixed `用户：`, so a recalled line reads as what the user said. The
assistant's prose is never embedded: fed back into the prompt through recall,
it pulled replies toward repeating a line.

**Group turns** use the same writer and embed the user's message once. The
profile layer gets one row. The relationship layer gets one row for every
seated member, whether or not it spoke, since every member was present; a
member whose persona instance is inactive is not seated that turn. Group rows
carry the group as `session_id` and the prefix `（群聊）用户：`, so a later
1:1 recall knows where the line was said. See
[group-chat.md](group-chat.md#what-reaches-11).

**What is not embedded.** `engine.chat_messages` holds the full transcript as
plain text and is never embedded; it is the source of truth for what was
said. The insight pipelines write their own tables and nothing here:
`insight_extraction` keeps a fact ledger (`human_facts`) from which a
scheduled rebuild derives `human_insights` ([insights.md](insights.md)), and
the experimental per-relationship chains UPSERT `character_insights` and
`user_insights` (see [api-reference.md](api-reference.md#get-v2compinstanceinstance_idinsightcharacter)).
Of these, only `human_insights` (the 基础画像 bullets) and four fields of
`character_insights` (the `[character_state]` block, see
[deploying.md](deploying.md#operational-notes)) reach a prompt.

## Relationship memory

Dreaming reads one relationship's conversation, one persona instance and its
owner, and writes its story up as episodes, an arc and a journal; it also
writes the profile memories it finds along the way. Everything here is
extracted from the conversation. The user never writes it, and no endpoint
exposes it.

### Dreaming

Dreaming is the relationship scan, `pipeline::dreaming`. It keeps one cursor
per relationship and reads forward from it.

- **Counting.** Every 1:1 text turn adds one pending turn to its
  relationship, and so does a voice turn unless `DREAMING_VOICE_DISABLED` is
  set. Group turns do not count. A relationship's first counted turn creates
  its scan state, with the cursor just before that turn; earlier history is
  read by the [backfill](#earlier-history).
- **At 20 pending turns** the text turn's post-processing scans inline, after
  the turn's other writes, up to the turn's own newest row. A voice turn never
  scans inline.
- **Idle flush.** The dreaming sweeper wakes every `DREAMING_TICK_SECS`
  (default 300) and scans up to 10 relationships with at least **3** pending
  turns whose last counted turn is older than `DREAMING_IDLE_SECS` (default
  3600), longest-waiting first. A relationship under 3 pending turns waits
  until it has 3; its rows are read by the scan that eventually reaches it.
- **One at a time.** Both paths claim the relationship before scanning, so two
  workers never scan it at once.
- **Off switches.** Without `[tasks.dreaming_scan]` nothing is scanned.
  `DREAMING_DISABLED=1` (or `DREAMING_TICK_SECS=0`) stops both the sweeper and
  the inline scan. Turns are still counted either way, and the engine's own
  journal rows are still written.

A scan reads at most **60** rows after the cursor: the user and assistant rows
of the pair's non-archived text and voice sessions, in time order, never
product-QA rows. They go to the model as one transcript, a line per row
stamped `[#n YYYY-MM-DD HH:MM]` in the user's timezone (the last valid one a
text turn of the relationship carried; UTC without one). Voice assistant rows
lose their TTS audio tags first. A user row that carried an action shows its
marker and, once the persona answered, the outcome.

One `dreaming_scan` call returns all three products: profile memories, one
episode summary and any journal moments. The reply contract is in
[Model config](model-config.md#tasksdreaming_scan--relationship-scan-opt-in).
The memories and journal summaries are embedded in one batch call; the
episode is written, then the memories (to the profile layer, under the
instance's owner), then the journal rows, and the cursor moves to the last
row read. Rows past the 60-row cap wait for the next scan. A stretch with
nothing readable moves the cursor without a call.

A failed call, an unparsable reply, a failed embedding call or a database
error before the writes leaves the cursor where it was, and the sweeper
retries after one idle period. The third consecutive failure skips the
stretch with a warning; its raw rows stay in `chat_messages`. An archive or
erasure that lands while a scan is running wins: the scan writes nothing
further.

### The arc

The arc is the relationship's story so far, rewritten from the episodes as
they accumulate. Every fourth episode triggers a rewrite
(`[tasks.arc_rewrite]`, an engine-owned prompt) from the opening, the previous
arc and the episodes not yet folded in, each with its dates. The prompt asks
for 400–600 characters in time order: an earlier state kept as
曾经……后来……, older events compressed more, each act attributed to the
side that did it, and roleplay written as roleplay. The first rewrite also
writes the **opening**, how the relationship began, which is never rewritten.

The engine rejects a rewrite that does not parse, runs over 900 characters,
comes in under 70% of a previous arc longer than 300 characters, or, on the
first rewrite, lacks an opening or has one over 150 characters. A rejected or
failed rewrite leaves the previous arc serving and is retried at the next
fourth episode, with the waiting episodes carried along; after three in a row
they are marked folded without a new arc. Without `[tasks.arc_rewrite]`
episodes stay unfolded, and the first rewrite after the block is added folds
all of them.

### The journal

The journal records the relationship's moments, one dated row each. Every row
has a `realm`: `real` for what happened between the persona and the user,
`roleplay` for a scene they played. The realm follows **initiative**, not
physical reality: `real` when the character initiates, or the user invites
and the character agrees; `roleplay` when the user supplies the script, sets
the scene or narrates the moment and the character performs it.

The engine writes some rows itself, with no LLM call and `realm = real`:

| Moment | `kind` | Comes back |
|--------|--------|------------|
| The relationship's first chat, journaled when its first turn is counted and dated by its earliest chat row | `first_meet` | monthly, then yearly |
| A bond or chemistry label reached for the first time, in the affinity write's transaction | `tier_reached` | no |
| An accepted `give`, `gift` or `tip` action (a tip never states its amount) | `gift` | no |
| The first accepted `kiss` action | `first_intimacy` | no |

A label already journaled for the relationship is not journaled again, even
when a later session reaches it anew. When a line falls, its `tier_reached`
rows for the labels it fell below are taken back while they are under 24
hours old, whether the drop came from a judged turn, a refused action or a
life choice. `hug`, `touch`, `lick` and `custom` actions are not journaled.

The scan's model writes the rest: `confession`, `first_intimacy`, `conflict`,
`reconciliation`, `promise`, `gift`, `shared_plan` and
`user_shared_milestone`, each dated by a stamp of the transcript it came
from. A `promise` or `shared_plan` with a due date comes back on that day.
Where only one row makes sense, a key keeps the first writer: one
`first_meet`, one `first_intimacy` per realm, one `gift` per message, one
`tier_reached` per line and label.

The engine's own rows are written without a vector; the dreaming sweeper
embeds them at its next tick, up to 100 per tick in one call. Until then, and
on a deployment whose sweeper does not run, they stay out of semantic recall.
Anniversary lines do not need a vector.

### Earlier history

Live scanning starts at each relationship's first counted turn. The
`backfill-dreaming` subcommand reads the history before it
([Deploying → Subcommands](deploying.md#subcommands)): a manual, paid
operator step that `serve` never runs.

## Retrieval and injection

Memory is read back into the prompt on each turn, gated by the request's
`memory_scope` (values in [api-reference.md](api-reference.md); default
`neutral_and_relationship`). The reply handler (`pipeline::handlers`) builds
the context from these sources:

- **Profile layer.** The 基础画像 bullets come from `human_insights`
  ([insights.md](insights.md#readers)); `memory_scope` decides whether
  intimate fields are included (`full` / `insights_only`) or only the neutral
  subset (`neutral_*`). Alongside them, profile-layer `companion_memories`
  rows are recalled by similarity and grouped by `category`, but only when the
  scope keeps profile memory (`full` / `neutral_and_relationship`).
  `relationship_only`, `neutral_only`, `insights_only` and `none` skip this
  half, leaving only the bullets the scope still allows.
- **Relationship layer.** Relationship-layer `companion_memories` rows recalled
  by similarity to the current turn, plus, on 1:1 prompts, the relationship's
  arc, latest episodes, nearest journal moments and anniversary lines. All of
  it needs a scope that keeps relationship memory (`full` /
  `neutral_and_relationship` / `relationship_only`).

`memory_scope = none` skips memory injection entirely. **`memory_scope` gates
the prompt only, never writes:** under `none` the raw-turn writer still
stores the turn, the turn still counts toward dreaming, and insight
extraction and the affinity evaluation still run. The
`/comp/user/{user_id}/profile` endpoint returns the `human_insights` row as a
readable view of what has been collected.

### Recall

Similarity search uses pgvector's `<=>` cosine distance over an IVFFlat
index:

```sql
CREATE INDEX idx_memories_embedding
  ON engine.companion_memories
  USING ivfflat (embedding vector_cosine_ops)
  WITH (lists = 100);
```

Profile-layer search:

```sql
SELECT id, session_id, user_id, instance_id, content, category, metadata, created_at
FROM engine.companion_memories
WHERE user_id = $1 AND instance_id IS NULL
ORDER BY embedding <=> $2::vector
LIMIT $3;
```

Relationship-layer search filters on `instance_id` instead and leaves out
rows whose content holds `\nAI：`, verbatim transcripts from an early writer.
No score is computed and there is no threshold: rows are ordered by raw
cosine distance and the K nearest come back however far they are. The
journal is searched the same way, by the turn's query embedding, over its own
IVFFlat index.

`lists = 100` suits small-to-medium tables (≲ 1M rows); raise it for larger
ones (rule of thumb: `lists ≈ √rows`).

### `[shared_memories]`

On a 1:1 prompt (text turns, proactive messages, image-edit replies) the
relationship part of `[shared_memories]` reads:

```
[shared_memories]
{opening}
{arc}
- 2026-10-12：{episode summary}
- 2026-10-20：{episode summary}
- 2026-08-15（扮演）：{journal summary}
- 用户：…
```

- The arc paragraph: the opening and the arc, each on its own line.
- Up to three episodes, the latest by end time, oldest first, each dated by
  its start in the request's timezone (UTC without one).
- Up to two journal moments, the nearest to the turn first, dated by
  `occurred_on`; a `roleplay` moment is marked `（扮演）`.
- The recalled raw lines.
- **Budget: 1,200 characters** for this part. The `[world_memories]`,
  `[world_stories]` and `[character_state]` sections under the same header are
  outside it. Over budget, lines go in this order: episodes oldest first, then
  raw lines from the lowest-ranked, then journal moments from the farthest.
  The arc is never cut.
- A relationship with no arc, no episode and no journal moment near the turn
  gets the raw lines alone, with no budget, or the placeholder when there are
  none.

Group seats get the recalled raw lines but no arc, episodes or journal.

### Anniversary lines

Anniversary lines go under `[now]`, after the birthday line, at most two per
turn, the first meeting's first. They read the relationship's recurring `real`
journal rows and need the request's `user_timezone`, like the holiday line:

- `first_meet`, months 1–11: on the same day of the month (clamped to the
  month's end), and only that day: `今天是你们认识满 N 个月的日子。`
- `first_meet`, from 12 months: the yearly date, from six days before it:
  `今天` / `明天` / `N 天后是你们认识满 Y 年的日子。`
- A due `promise` or `shared_plan`, on its day:
  `今天是你们约好的日子：{summary}。`

### Affinity gate

`memory_scope` is the ceiling. Within it, the relationship's current affinity
decides how much reaches the prompt: a persona the user barely knows doesn't
know what they told other personas, and doesn't hold on to much of what they
say. The gate reads the bond and chemistry tiers (1–5) of the time-decayed
affinity ([affinity-model.md](affinity-model.md)); a pair with no affinity
row is tier 1 on both lines. It only removes — nothing the scope excludes is
ever added — and it applies on every injection path: text turns, each group
seat (with that seat's own affinity), voice, and the text half of an image
edit.

- **基础画像 intimate fields.** Each field reads one line and has a minimum
  tier on it; the table is in [insights.md](insights.md#chat-prompt).
- **Profile layer.** K follows the higher of the two tiers: tier 1 recalls
  nothing from this layer, tier 2 half the usual K (rounded up), tier 3 and
  above the usual K. Both the per-category grouped recall and the
  uncategorized fallback follow it. On top of that, a row whose
  `metadata.domain` mirrors an intimate field needs that field's tier:

  | `metadata.domain` | Line | From tier | Mirrors |
  |-------------------|------|-----------|---------|
  | `sexuality`, `kink` | chemistry | 4 | `kinks` |
  | `romance` | chemistry | 2 | `love_values` |
  | `family` | bond | 3 | `family` |
  | `finance` | bond | 4 | `finance_status` |
  | `health`, `mental_health` | bond | 4 | `mental_health` |

  The domain is the label the `dreaming_scan` prompt writes on each memory
  ([model-config.md](model-config.md)), matched trimmed and
  case-insensitively. Any other value, a non-string, or no domain passes on
  the K ramp alone; raw-turn rows carry no domain, so they are only
  K-limited. Withheld rows still count toward their category's fetched K: the
  search runs once at the usual K and the gate trims its result.
- **Relationship layer.** K for the raw lines is the usual K capped at the
  higher tier (text: 1 / 2 / 3 at tiers 1 / 2 / 3+; voice: 1 / 2). Their
  content is not filtered: it is what the user said to this persona. The arc,
  episodes, journal and anniversary lines are not gated at all; they render at
  every tier.

### Voice turns

The voice endpoint (`POST /comp/voice/{session_id}/turn/stream`) reads the
same layers through a leaner path — see
[api-reference.md](api-reference.md#post-compvoicesession_idturnstream) for
the request and [model-config.md](model-config.md) for `[tasks.chat_voice]`.

- **Bootstrap snapshot.** Assembled once, on a session's first turn, and
  frozen into `chat_sessions.metadata.voice_bootstrap`; every later turn
  re-injects it verbatim. Five parts: the `human_insights` bullets at the
  neutral tier by default (tunable through that first turn's
  `memory_scope`); the relationship's opening and arc as `[你们的故事]`, when
  it has an arc and that first turn's `memory_scope` keeps relationship
  memory; the previous voice call's last 8 messages as a plain transcript;
  the lingering effects of the text session the call reads its relationship
  line from ([API reference](api-reference.md#get-v2compsessionsession_ideffects));
  and the persona's needs at the call's start. Each part degrades on its own,
  and a failed assembly leaves the marker unwritten so the next turn retries.
  The snapshot never changes, but each turn passes its 基础画像 bullets
  through the [affinity gate](#affinity-gate) with that turn's tiers.
- **Per-turn recall.** The same `companion_memories` search as text
  (`memory_scope`-gated, cross-layer dedup via `memory_hygiene`) at a much
  smaller K: grouped profile 1 per category, raw fallback 2, relationship 2
  (text uses 2 / 4 / 3), then trimmed by the [affinity gate](#affinity-gate)
  once the pair's affinity, read in parallel and time-decayed like text, has
  landed. It runs inside a 300 ms budget: a timeout or search error drops that
  turn's recall block with a warn log, never an error frame. An utterance
  under 4 alphanumeric characters after stripping whitespace and punctuation
  (嗯 / 好啊 / 哈哈) skips recall and the embedding call. Per-turn voice recall
  does not search the journal. `[tasks.chat_voice] recall = false` (default
  `true`) turns recall off and wins over the request's `memory_scope`.
- **Writes go through dreaming.** A live voice turn writes no memory itself:
  no raw-turn row, no insight extraction, no affinity evaluation. It counts
  toward its relationship's [dreaming](#dreaming) but never scans inline; the
  sweeper scans the relationship once it has gone idle with at least 3 pending
  turns, and a text turn's inline scan reads any voice rows in its range. The
  scan reads the call together with the relationship's text rows and writes
  memories, an episode and journal moments from it exactly as for text; later
  calls and text chats both recall them. `DREAMING_VOICE_DISABLED=1` leaves
  voice sessions out of the scan and stops voice turns from counting. The
  cursor moves by time, so voice rows passed over while the flag is set are
  not read after it is cleared.
- **What a barge-in leaves behind.** An interrupted turn stores the text the
  client reported as actually spoken, not the full generation, so dreaming
  reads what the user *heard*. A turn cut off before the first audible word
  stores only the user's line, because no assistant row is written.

## Archive and erasure

- **`DELETE /comp/instance/{instance_id}/sessions`** archives the
  relationship: its sessions are marked archived, and its relationship-layer
  `companion_memories` rows and its rows in `relationship_scan_state`,
  `relationship_episodes`, `relationship_arcs` and `relationship_journal` are
  deleted. Profile memories stay. A relationship restarted on the same
  instance starts a new scan state and a new first meeting.
- **`POST /svc/v1/users/{user_id}/erase`** deletes the user's memories in both
  layers and the four relationship tables of their persona instances
  ([Insights → Erasure](insights.md#erasure)).
- `relationship_scan_events` survives both: it holds counts and ids only.

## Design specs

- [2026-10-28-memory-2-dreaming-plus-design.md](superpowers/specs/2026-10-28-memory-2-dreaming-plus-design.md) — dreaming, the arc, the journal and their place in the prompt
- [2026-08-09-voice-memory-bootstrap-recall-design.md](superpowers/specs/2026-08-09-voice-memory-bootstrap-recall-design.md) — the voice bootstrap and per-turn recall
- [2026-08-09-voice-dreaming-ingestion-design.md](superpowers/specs/2026-08-09-voice-dreaming-ingestion-design.md) — voice rows in dreaming

## Source

- `crates/eros-engine-store/src/memory.rs` — `MemoryRepo`: `companion_memories` writes and search
- `crates/eros-engine-store/src/scan_state.rs`, `episode.rs`, `arc.rs`, `journal.rs`, `scan_event.rs` — the relationship tables: cursor and claims, episodes and the segment read, the arc, the journal (including the engine's own rows), the event rows
- `crates/eros-engine-store/src/human_insight.rs` — the `human_insights` store behind the 基础画像 bullets
- `crates/eros-engine-llm/src/voyage.rs` — native Voyage client
- `crates/eros-engine-llm/src/embedding.rs` — `EmbeddingRouter` + OpenRouter-compatible client
- `crates/eros-engine-server/src/pipeline/post_process.rs` — the raw-turn writer; counts each text turn toward dreaming
- `crates/eros-engine-server/src/pipeline/dreaming/` — dreaming: triggers and sweeper (`mod.rs`), the scan and the arc rewrite (`scan.rs`), reply parsing and guards (`parse.rs`), the transcript (`render.rs`)
- `crates/eros-engine-server/src/relationship_context.rs` — loads the arc, episodes and journal for a turn and renders the relationship part of `[shared_memories]`
- `crates/eros-engine-server/src/anniversary.rs` — the anniversary lines
- `crates/eros-engine-server/src/backfill.rs` — the `backfill-dreaming` subcommand
- `crates/eros-engine-server/src/pipeline/handlers.rs` — recall and prompt injection
- `crates/eros-engine-server/src/pipeline/voice.rs` — the voice bootstrap snapshot and per-turn recall
- `crates/eros-engine-store/migrations/0000_baseline.sql` — `companion_memories` and its indexes
- `crates/eros-engine-store/migrations/0082_relationship_memory.sql` — the five relationship tables
