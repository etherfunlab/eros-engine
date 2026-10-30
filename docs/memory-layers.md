# Memory layers

[English](memory-layers.md) · [中文](memory-layers.zh.md)

Two layers hold what the persona remembers about you. They serve different recall needs and are queried separately.

## Profile vs Relationship

| Layer | `instance_id` | What it holds | Lifetime |
|-------|---------------|---------------|----------|
| **Profile** | `NULL` | Cross-session facts about the user — things any persona can come to know as it gets close to the user. | Permanent |
| **Relationship** | `<uuid>` | What passed between this specific persona and this user: what the user said to it, plus the arc, episodes and journal the [relationship scan](#relationship-memory) draws from the conversation. | Until the relationship is archived |

The distinction matters because **persona stability across personas** is different from **intimacy within a relationship**. If you tell Aria you're allergic to peanuts, that's a profile fact — Kenji can know it too, once he and the user are past strangers (see [Affinity gate](#affinity-gate)). If Aria mentioned she's reading Bishop tonight, that's a relationship memory — Kenji shouldn't pretend to know.

## Storage

Single table, two layers distinguished by `instance_id`:

```sql
CREATE TABLE engine.companion_memories (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id   UUID NOT NULL REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    user_id      UUID NOT NULL,
    instance_id  UUID,                         -- NULL = profile layer
    content      TEXT NOT NULL,
    embedding    VECTOR(512) NOT NULL,
    category     TEXT,                         -- fact|preference|event|emotion|relation; NULL on raw-turn rows
    metadata     JSONB,                        -- opaque per-memory extraction payload; NULL on raw-turn rows
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

Two filtered indexes — one per layer — keep retrieval cheap on the hot path:

```sql
CREATE INDEX idx_memories_user_profile
  ON engine.companion_memories(user_id)
  WHERE instance_id IS NULL;

CREATE INDEX idx_memories_session
  ON engine.companion_memories(session_id)
  WHERE instance_id IS NOT NULL;
```

The relationship scan's own products live in separate tables keyed by the persona instance; see [Relationship memory](#relationship-memory).

## Embedding

`voyage-4-lite` via Voyage's native API by default. 512 dimensions, multilingual — the schema is `VECTOR(512)`, and non-Voyage routes have `dimensions: 512` forced on the wire.

`[tasks.embedding]` picks the route. A `@provider` suffix on `model` sends the calls elsewhere: `@openrouter` uses the built-in OpenRouter embeddings endpoint (URL overridable via `[providers].openrouter.embeddings`), and `@<name>` uses any `[providers]` entry that declares an `embeddings` URL (key read from `<NAME>_API_KEY`). Alternatively a `model_read`/`model_write` pair splits the recall and storage models independently — the pair must appear together, is mutually exclusive with `model`, and is restricted to Voyage models of the voyage-4 series and above, the only lineup guaranteeing one shared vector space across model sizes.

```rust
// crates/eros-engine-llm/src/embedding.rs — EmbeddingRouter
pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>, LlmError>;    // read backend
pub async fn embed_document(&self, text: &str) -> Result<Vec<f32>, LlmError>; // write backend
```

`embed_query` serves the per-turn recall path, `embed_document` the storage paths. On the Voyage backend the two become different `input_type` hints (`query` vs `document`); the OpenRouter-compatible wire has no such hint, so there the split only matters when `model_read`/`model_write` actually differ.

The engine still **fails loud** on missing secrets — but the `VOYAGE_API_KEY` boot gate fires only when the resolved route includes a Voyage backend; OpenRouter and custom routes are gated on their own keys (`OPENROUTER_API_KEY` / `<NAME>_API_KEY`) instead. The closed-source eros-gateway has a known regression where an empty key silently disables embeddings; eros-engine declined to inherit that.

## Retrieval

Cosine similarity via pgvector's `<=>` operator with an IVFFlat index:

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

Relationship-layer search filters on `instance_id = $2` instead and adds `content NOT LIKE '%\nAI：%'` (`$3`) to exclude legacy verbatim-transcript rows (issue #113), shifting the embedding to `$4` and the limit to `$5`. No similarity score is computed anywhere — results are ordered by the raw `<=>` cosine distance and retrieval is pure top-K via `LIMIT`; there is no similarity threshold, so the K nearest rows come back regardless of absolute distance.

`lists = 100` is a balanced default for small-to-medium tables (≲ 1M rows). Tune up for larger corpora (rule of thumb: `lists ≈ √rows`).

## What gets embedded

Two independent writers, on different schedules:

1. **Raw-turn writer** (`write_turn`, post-process) — runs on **every** substantive turn (non-empty user utterance and at least one non-empty assistant message). It embeds **the user's utterance only** into *both* layers — never the assistant's prose, which fed back into the model's own prompt via recall and collapsed replies to a repeated line (issue #113). The relationship-layer copy is prefixed `用户：` so a recalled line stays readable as "what the user said." These rows carry `category = NULL` and `metadata = NULL`.
2. **Relationship scan** (`[tasks.dreaming_scan]`, `pipeline::dreaming`) — a per-relationship pass over the conversation after its cursor ([Relationship memory](#relationship-memory)). Of its three outputs only the profile memories land in this table: **profile-layer rows**, tagged `category ∈ {fact, preference, event, emotion, relation}` (anything else the model invents collapses to `fact`) plus an opaque `metadata` payload. Its episodes and journal moments go to their own tables.

`insight_extraction` is a **separate** pipeline and writes nothing to this table: it records atomic facts about the user in a fact ledger (`human_facts`), and a scheduled rebuild derives the flat `human_insights` profile from that ledger; none of it is embedded here (see [insights.md](insights.md)). Two further experimental, per-relationship chains (`character_insight_*`, `user_insight_*`) UPSERT into their own flat tables — `character_insights` (see [api-reference.md](api-reference.md#get-v2compinstanceinstance_idinsightcharacter)) and `user_insights` (see [api-reference.md](api-reference.md#get-v2compinstanceinstance_idinsightuser)); like `human_insights`, neither table is embedded into this one. `user_insights` is not injected into any chat, voice, or PDE prompt; `character_insights` is — four of its fields render the chat prompt's `[character_state]` block, and how completely the row is filled sizes the injected history window (see [deploying.md](deploying.md#operational-notes)).

**Group turns** write through the same raw-turn writer, with the user's message embedded once. The profile layer gets one row (`instance_id` NULL). The relationship layer gets one row for every seated member, whether or not it spoke, because every member was present. A member whose persona instance is inactive is not seated that turn. Group-sourced rows carry `session_id` = the group (where it was said) and the content prefix `（群聊）用户：`, so a later 1:1 recall knows where the line came from. They are keyed to each member's `instance_id`, so `DELETE /comp/instance/{instance_id}/sessions` removes them with the rest of that relationship. See [group-chat.md](group-chat.md#what-reaches-11).

So embeddings *are* generated once per substantive turn, not only for LLM-surfaced highlights.

## What doesn't get stored

Raw chat messages live in `engine.chat_messages` (full transcript, plain text). They are **not** embedded. The memory tables hold *summaries* and *facts*, not the full message log. If you want to retrieve the actual transcript, query `chat_messages` directly — that's the source of truth for what was said.

## Relationship memory

The relationship scan (`[tasks.dreaming_scan]`, `pipeline::dreaming`) reads one relationship's conversation — one persona instance and its owner — and writes three products besides the profile memories. All of them are extracted from the conversation; the user never writes them.

| Table | One row per | What it holds |
|-------|-------------|---------------|
| `relationship_episodes` | scanned stretch | A summary of that stretch, at most 400 characters, with `started_at` / `ended_at` taken from the first and last row it covers. Append-only. |
| `relationship_arcs` | relationship | `opening` (how it began, at most 150 characters, written by the first rewrite and never again), `arc` (the story so far, at most 900 characters) and `previous_arc`. |
| `relationship_journal` | moment | `occurred_on` (the user's local day), `kind`, `realm`, a one-sentence `summary` (at most 200 characters) and an embedding for semantic recall. |

`relationship_scan_state` holds each relationship's cursor, pending-turn count and claim, and `relationship_scan_events` one counts-only row per scan or arc attempt ([LLM audit](llm-audit.md#relationship-scanner)). None of these tables has a `user_id`: the instance determines the owner.

### When a relationship is scanned

- Every 1:1 text turn counts toward its relationship's pending turns, and so does a voice turn unless `DREAMING_VOICE_DISABLED` is set. Group turns do not count. The first counted turn creates the relationship's scan state with the cursor just before that turn; earlier history is the [backfill](deploying.md#subcommands)'s.
- At **20** pending turns the text turn's post-processing scans inline, after the turn's other writes, up to the turn's own newest row. A voice turn never scans inline.
- The dreaming sweeper wakes every `DREAMING_TICK_SECS` (default 300) and scans up to 10 relationships with at least **3** pending turns whose last counted turn is older than `DREAMING_IDLE_SECS` (default 3600), longest-waiting first. A relationship under 3 pending turns waits until it reaches 3.
- Both paths claim the relationship first, so two workers never scan it at once. `DREAMING_DISABLED=1` (or `DREAMING_TICK_SECS=0`) stops both, and neither runs without `[tasks.dreaming_scan]`.

### What a scan reads and writes

A scan reads at most **60** rows after the cursor: the user and assistant rows of the pair's non-archived text and voice sessions, in time order, never product-QA rows. They go to the model as one transcript, a line per row stamped `[#n YYYY-MM-DD HH:MM]` in the user's timezone (the last valid one a text turn of the relationship carried; UTC without). Voice assistant rows lose their TTS audio tags; a user row that carried an action shows its marker and, once answered, the outcome. One call returns profile memories, one episode summary and journal moments; the contract is in [Model config](model-config.md#tasksdreaming_scan--relationship-scan-opt-in).

The memories and journal summaries are embedded in one batch call; then the episode is written, then the memories, then the journal rows, and the cursor moves to the last row read. Rows past the 60-row cap wait for the next scan. A stretch with nothing readable moves the cursor without a call.

A failed call, an unparsable reply, a failed embedding call or a database error before the writes leaves the cursor in place, and the sweeper retries after one idle period. The third consecutive failure skips the stretch with a warning; its raw rows stay. An archive or erasure that lands during a scan wins: the scan writes nothing further.

### The arc

Every fourth episode triggers a rewrite (`[tasks.arc_rewrite]`, engine-owned prompt) from the opening, the previous arc and the episodes not yet folded, each with its dates. The prompt asks for 400–600 characters in time order, with an earlier state kept as 曾经……后来……, older events more compressed, each act attributed to the right side, and roleplay written as roleplay. The engine rejects a rewrite that does not parse, runs over 900 characters, comes in under 70% of a previous arc longer than 300 characters, or, on the first rewrite, lacks an opening or has one over 150 characters. A rejected or failed rewrite leaves the previous arc serving and is retried at the next fourth episode, with the waiting episodes carried along; after three in a row they are marked folded without a new arc. Without `[tasks.arc_rewrite]` episodes stay unfolded.

### The journal

The engine writes some rows itself, with no LLM call and `realm = real`:

| Moment | `kind` | Comes back |
|--------|--------|------------|
| The relationship's first chat, journaled when its first turn is counted and dated by its earliest chat row | `first_meet` | monthly, then yearly |
| A bond or chemistry label reached for the first time, in the affinity write's transaction | `tier_reached` | no |
| An accepted `give`, `gift` or `tip` action (a tip never states its amount) | `gift` | no |
| The first accepted `kiss` action | `first_intimacy` | no |

A label already journaled for the relationship is not journaled again, even when a later session reaches it anew. A line that falls takes back its `tier_reached` rows for the labels it fell below while they are under 24 hours old, whether the drop came from a judged turn, a refused action or a life choice. `hug`, `touch`, `lick` and `custom` actions are not journaled.

The scan's model writes the rest: `confession`, `first_intimacy`, `conflict`, `reconciliation`, `promise`, `gift`, `shared_plan` and `user_shared_milestone`, each dated by a stamp of the transcript it came from. `realm` follows initiative, never physical reality: `real` when the character initiates, or the user invites and the character agrees; `roleplay` when the user supplies the script, sets the scene or narrates the moment and the character performs it. A `promise` or `shared_plan` with a due date comes back on that day. Where only one row makes sense, a key keeps the first writer: one `first_meet`, one `first_intimacy` per realm, one `gift` per message, one `tier_reached` per line and label.

Engine rows are written without a vector, and the sweeper embeds them at its next tick, up to 100 per tick in one call. Until then, and on a deployment whose sweeper does not run, they are left out of semantic recall; the anniversary lines do not need a vector.

### In the prompt

On a 1:1 prompt (text turns, proactive messages, image-edit replies) the relationship part of `[shared_memories]` becomes:

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
- Up to three episodes, the latest by end time, oldest first, each dated by its start in the request's timezone (UTC without one).
- Up to two journal moments, the nearest to the turn's query embedding first, dated by `occurred_on`; a `roleplay` moment is marked `（扮演）`.
- The raw recalled rows, as before.
- **Budget: 1,200 characters** for this part; the `[world_memories]`, `[world_stories]` and `[character_state]` sections under the same header are outside it. Over budget, lines go in this order: episodes oldest first, then raw rows from the lowest-ranked, then journal moments from the farthest. The arc is never cut.
- **Nothing new, nothing changed.** A relationship with no arc, no episode and no journal moment near the turn gets the block exactly as before: the raw rows with no budget, or the placeholder when there are none.

**Anniversary lines** go under `[now]`, after the birthday line, at most two per turn, the first meeting's first. They read the relationship's recurring `real` journal rows and need the request's `user_timezone`, like the holiday line:

- `first_meet`, months 1–11: on the same day of the month (clamped to the month's end) and only that day, `今天是你们认识满 N 个月的日子。`
- `first_meet`, from 12 months: the yearly date, from six days before it, `今天` / `明天` / `N 天后是你们认识满 Y 年的日子。`
- A due `promise` or `shared_plan`: on its day, `今天是你们约好的日子：{summary}。`

All of it, anniversary lines included, follows `memory_scope`'s relationship flag: none under `neutral_only`, `insights_only` or `none`. The [affinity gate](#affinity-gate) does not trim it — the arc, episodes and journal render at every tier, being this relationship's own memory. Group seats get none of it. The voice bootstrap carries the opening and arc as `[你们的故事]` ([Voice turns](#voice-turns)); per-turn voice recall does not search the journal.

### Earlier history

The scan starts at each relationship's first counted turn. The `backfill-dreaming` subcommand reads what came before ([Deploying → Subcommands](deploying.md#subcommands)); it is a manual, paid operator step that `serve` never runs.

## Retrieval and injection

Memory is read back into the prompt on each chat turn, gated by the per-request
`memory_scope` (values and default in [api-reference.md](api-reference.md);
default `neutral_and_relationship`). The reply handler (`pipeline::handlers`)
builds a profile/relationship context block from two sources:

- **Profile layer** — two merged sources. The 基础画像 bullets come directly
  from the flat **`human_insights`** table, the profile derived from the
  user's fact ledger ([insights.md](insights.md#readers)). `memory_scope` decides
  whether intimate fields are included (`full` / `insights_only`) or only the
  neutral subset (`neutral_*`). Alongside them, profile-layer
  `companion_memories` rows are pulled by similarity search and grouped by
  `category`, but only when the scope keeps profile memory (`full` /
  `neutral_and_relationship`) — `relationship_only`, `neutral_only`,
  `insights_only`, and `none` skip this half entirely, leaving only the
  `human_insights` bullets (where the scope still produces any).
- **Relationship layer** — `companion_memories` rows pulled by semantic
  (embedding) similarity search against the current turn, included when the
  scope keeps relationship memory (`full` / `neutral_and_relationship` /
  `relationship_only`). On 1:1 prompts the relationship's arc, latest
  episodes and nearest journal moments join them under the same flag
  ([Relationship memory → In the prompt](#in-the-prompt)).

`memory_scope = none` skips memory injection entirely. **`memory_scope` gates
prompt injection only — never writes.** Even under `none`, the raw-turn writer
still embeds and stores this turn, and insight extraction and the affinity
evaluation still run; there is currently no scope value that suppresses
writing. The frontend's `/comp/user/{user_id}/profile` endpoint returns the
typed `human_insights` row as a human-readable view of what's been collected.

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

- **基础画像 intimate fields** — each field reads one line and has a minimum
  tier on it; the table is in [insights.md](insights.md#chat-prompt).
- **Profile layer** — K follows the higher of the two tiers: tier 1 recalls
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
  ([model-config.md](model-config.md)); it is matched trimmed and
  case-insensitively. Any other value, a non-string, or no domain at all
  passes on the K ramp alone. Uncategorized rows (raw turns) carry no
  domain, so they are only K-limited. Withheld rows still count toward their category's fetched K:
  the search runs once at the usual K and the gate trims its result.
- **Relationship layer** — K is the usual K capped at the higher tier
  (text: 1 / 2 / 3 at tiers 1 / 2 / 3+; voice: 1 / 2). Content is not
  filtered: it is what the user said to this persona. The arc, episodes and
  journal are not gated at all; they render at every tier.

### Voice turns

The voice endpoint (`POST /comp/voice/{session_id}/turn/stream`) reads from
the same two layers through a leaner, voice-specific path — see
[api-reference.md](api-reference.md#post-compvoicesession_idturnstream) for
the request shape and [model-config.md](model-config.md) for the
`[tasks.chat_voice]` block.

- **Bootstrap snapshot** — assembled once, on a session's first turn, and
  frozen into `chat_sessions.metadata.voice_bootstrap`; re-injected verbatim
  on every later turn (OpenRouter is stateless, so there is no "inject once"
  on the wire — later turns cannot change it). Five parts: `human_insights`
  bullets at Neutral tier by default (tunable via that first turn's
  `memory_scope`); the relationship's opening and arc as `[你们的故事]`,
  when it has an arc and that first turn's `memory_scope` keeps relationship
  memory; the previous voice call's last 8 messages rendered as a
  plain transcript; the lingering effects of the text session the call
  reads its relationship line from (see
  [API reference](api-reference.md#get-v2compsessionsession_ideffects));
  and the persona's needs at the call's start.
  Each part degrades independently; a failed assembly leaves the marker
  unwritten so the next turn retries. The marker never changes, but each
  turn passes its 基础画像 bullets through the [affinity gate](#affinity-gate)
  with that turn's tiers.
- **Per-turn recall** — the same `companion_memories` search as the chat
  path (`memory_scope`-gated, cross-layer dedup via `memory_hygiene`) but at
  a much smaller K: grouped profile 1/category + raw fallback 2 +
  relationship 2 (chat's tier is 2/4/3), then trimmed by the
  [affinity gate](#affinity-gate) once the pair's affinity (read in parallel,
  time-decayed like text) has landed; wrapped in a 300 ms budget — a
  timeout or search error just drops that turn's recall block, with a warn
  log, never an error frame. An utterance under 4 alphanumeric characters
  after stripping whitespace/punctuation (嗯 / 好啊 / 哈哈-style
  backchannels) skips recall entirely, with no embedding call. Deployment
  kill-switch: `[tasks.chat_voice] recall = false` (default `true`) always
  wins over the request's `memory_scope`.
- **Writes go through the scan** — a live voice turn writes no memory: no
  raw-turn embed, no insight extraction, no affinity eval. It counts toward
  its relationship's [scan](#relationship-memory) but never scans inline;
  the sweeper scans the relationship once it has gone idle with at least 3
  pending turns, and a text turn's inline scan reads any voice rows in its
  range. The scan reads the call together with the text rows of the same
  relationship and writes profile memories, an episode and journal moments
  from it, exactly as for text. Those are ordinary memories — later calls and
  text chats both recall them. Inline TTS audio tags (`[laughs]`, `[sighs]`,
  when `tts_audio_tags` is on) are stripped from voice assistant rows first,
  so stage directions never become memory text. `DREAMING_VOICE_DISABLED=1`
  leaves voice sessions out of the scan and stops voice turns from counting.
  The cursor moves by time, so voice rows passed over while the flag is set
  are not read after it is cleared. No raw relationship-layer row is ever
  written from voice.
- **What a barge-in leaves behind** — an interrupted turn contributes the text
  the client reported as actually spoken, not the full generation, so the
  scan reads what the user *heard*. A turn cut off before the first
  audible word contributes only the user's line, because no assistant row is
  written at all. Either way the transcript matches the call, which is the
  whole point of recording the interrupt.

Design specs:
[2026-08-09-voice-memory-bootstrap-recall-design.md](superpowers/specs/2026-08-09-voice-memory-bootstrap-recall-design.md)
(bootstrap + recall) and
[2026-08-09-voice-dreaming-ingestion-design.md](superpowers/specs/2026-08-09-voice-dreaming-ingestion-design.md)
(voice rows in dreaming). The relationship scan:
[2026-10-28-memory-2-dreaming-plus-design.md](superpowers/specs/2026-10-28-memory-2-dreaming-plus-design.md).

## Source

- `crates/eros-engine-store/src/memory.rs` — `MemoryRepo` (upsert + search, 7 sqlx::test integration tests)
- `crates/eros-engine-store/src/human_insight.rs` — flat, typed `human_insights` store; the profile rebuild's `replace_profile` writes it, the reply handler reads it at injection; the fact ledger is in `human_fact.rs`
- `crates/eros-engine-store/src/scan_state.rs`, `episode.rs`, `arc.rs`, `journal.rs`, `scan_event.rs` — the relationship scan's tables: cursor and claims, episodes and the segment read, the arc, the journal (including the engine-written rows), the event rows
- `crates/eros-engine-llm/src/voyage.rs` — native Voyage client
- `crates/eros-engine-llm/src/embedding.rs` — `EmbeddingRouter` + OpenRouter-compatible client
- `crates/eros-engine-server/src/pipeline/post_process.rs` — raw-turn write path; counts each text turn toward its relationship's scan
- `crates/eros-engine-server/src/pipeline/dreaming/` — the relationship scan: triggers and sweeper (`mod.rs`), the scan and the arc rewrite (`scan.rs`), the reply's parsing and guards (`parse.rs`), the transcript (`render.rs`)
- `crates/eros-engine-server/src/relationship_context.rs` — loads the arc, episodes and journal for a turn and renders the relationship part of `[shared_memories]`
- `crates/eros-engine-server/src/anniversary.rs` — the anniversary lines under `[now]`
- `crates/eros-engine-server/src/backfill.rs` — the `backfill-dreaming` subcommand
- `crates/eros-engine-server/src/pipeline/handlers.rs` — retrieval + prompt injection
- `crates/eros-engine-server/src/pipeline/voice.rs` — voice-turn bootstrap snapshot + per-turn recall
- `crates/eros-engine-store/migrations/0000_baseline.sql` — the table, its indexes, and the `category` / `metadata` columns (folded from the pre-baseline chain)
- `crates/eros-engine-store/migrations/0082_relationship_memory.sql` — the relationship scan's five tables
