# Memory layers

[English](memory-layers.md) · [中文](memory-layers.zh.md)

Two pgvector tables hold what the persona remembers about you. They serve different recall needs and are queried separately.

## Profile vs Relationship

| Layer | `instance_id` | What it holds | Lifetime |
|-------|---------------|---------------|----------|
| **Profile** | `NULL` | Cross-session facts about the user — things any persona could know. | Permanent |
| **Relationship** | `<uuid>` | Per-session callbacks — the small things this specific persona shared with this user. | Per session |

The distinction matters because **persona stability across personas** is different from **intimacy within a relationship**. If you tell Aria you're allergic to peanuts, that's a profile fact — Kenji should know it too. If Aria mentioned she's reading Bishop tonight, that's a relationship memory — Kenji shouldn't pretend to know.

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
2. **Dreaming-lite sweeper** (`[tasks.memory_extraction]`, `pipeline::dreaming`) — a background, idle-session-triggered pass that asks an LLM for durable memory candidates. It writes **profile-layer rows only**, tagged `category ∈ {fact, preference, event, emotion, relation}` (anything else the model invents collapses to `fact`) plus an opaque `metadata` payload.

`insight_extraction` is a **separate** pipeline and writes nothing to this table — its structured output is merged into `companion_insights` (and mirrored to the flat `human_insights` table), not embedded here.

So embeddings *are* generated once per substantive turn, not only for LLM-surfaced highlights.

## What doesn't get stored

Raw chat messages live in `engine.chat_messages` (full transcript, plain text). They are **not** embedded. The memory tables hold *summaries* and *facts*, not the full message log. If you want to retrieve the actual transcript, query `chat_messages` directly — that's the source of truth for what was said.

## Retrieval and injection

Memory is read back into the prompt on each chat turn, gated by the per-request
`memory_scope` (values and default in [api-reference.md](api-reference.md);
default `neutral_and_relationship`). The reply handler (`pipeline::handlers`)
builds a profile/relationship context block from two sources:

- **Profile layer** — two merged sources. The 基础画像 bullets come from the
  flat **`human_insights`** mirror table (kept in sync from `companion_insights`),
  *not* from the `companion_insights` JSONB directly; `memory_scope` decides
  whether intimate fields are included (`full` / `insights_only`) or only the
  neutral subset (`neutral_*`). Alongside them, profile-layer
  `companion_memories` rows are pulled by similarity search and grouped by
  `category`. Note the intimate/neutral distinction applies to the
  `human_insights` bullets only — it does **not** filter which memory
  categories are injected.
- **Relationship layer** — `companion_memories` rows pulled by semantic
  (embedding) similarity search against the current turn, included when the
  scope keeps relationship memory (`full` / `neutral_and_relationship` /
  `relationship_only`).

`memory_scope = none` skips memory injection entirely. **`memory_scope` gates
prompt injection only — never writes.** Even under `none`, the raw-turn writer
still embeds and stores this turn, and insight extraction and the affinity
evaluation still run; there is currently no scope value that suppresses
writing. The frontend's `/comp/user/{user_id}/profile` endpoint returns the
`companion_insights` JSONB as a human-readable view of what's been collected.

## Source

- `crates/eros-engine-store/src/memory.rs` — `MemoryRepo` (upsert + search, 7 sqlx::test integration tests)
- `crates/eros-engine-store/src/human_insight.rs` — flat `human_insights` mirror read at injection
- `crates/eros-engine-llm/src/voyage.rs` — native Voyage client
- `crates/eros-engine-llm/src/embedding.rs` — `EmbeddingRouter` + OpenRouter-compatible client
- `crates/eros-engine-server/src/pipeline/post_process.rs` — raw-turn write path
- `crates/eros-engine-server/src/pipeline/dreaming.rs` — dreaming-lite sweeper
- `crates/eros-engine-server/src/pipeline/handlers.rs` — retrieval + prompt injection
- `crates/eros-engine-store/migrations/0003_memory.sql` — schema + index DDL
- `crates/eros-engine-store/migrations/0006_memory_category.sql` — `category` column
- `crates/eros-engine-store/migrations/0032_companion_memories_metadata.sql` — `metadata` column
