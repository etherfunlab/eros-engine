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

`voyage-3-lite` via Voyage's API. 512 dimensions, multilingual, ~$0.02 per 1M input tokens.

```rust
// crates/eros-engine-llm/src/voyage.rs
pub async fn embed_document(&self, text: &str) -> Result<Vec<f32>, LlmError>;
pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>, LlmError>;
```

`embed_document` and `embed_query` use different `input_type` hints to Voyage — documents get optimised for storage retrieval, queries for cosine match. This is why the engine has both methods and not just one.

The engine **fails loud** on empty `VOYAGE_API_KEY` — boot refuses if the secret is missing. The closed-source eros-gateway has a known regression where an empty key silently disables embeddings; eros-engine declined to inherit that.

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
SELECT id, content, 1 - (embedding <=> $2::vector) AS similarity
FROM engine.companion_memories
WHERE user_id = $1 AND instance_id IS NULL
ORDER BY embedding <=> $2::vector
LIMIT $3;
```

Relationship-layer search adds `instance_id = $4`. The `1 - distance` lets you sort or threshold on similarity directly without remembering pgvector's distance-not-similarity convention.

`lists = 100` is a balanced default for small-to-medium tables (≲ 1M rows). Tune up for larger corpuses (rule of thumb: `lists ≈ √rows`).

## What gets embedded

Post-process inserts memories during the background phase of every turn. Two paths:

1. **Insight extraction** — the LLM identifies factual nuggets ("user mentioned they're a librarian"). These go into the profile layer (`instance_id = NULL`).
2. **Relationship moments** — anything specific to this session (a callback the persona made, a small confession). Goes into the relationship layer.

Embeddings are NOT generated for every message — only the ones the insight extractor surfaces as worth remembering. Volume stays modest.

## What doesn't get stored

Raw chat messages live in `engine.chat_messages` (full transcript, plain text). They are **not** embedded. The memory tables hold *summaries* and *facts*, not the full message log. If you want to retrieve the actual transcript, query `chat_messages` directly — that's the source of truth for what was said.

## Lazy retrieval

The pipeline does NOT proactively look up memories before each chat turn. The system prompt is built from the persona genome + affinity vector + relationship label only. Memories surface when the LLM (in the insight or chat task) asks for them via a future tool-use API — not yet wired in v0.1, planned for a later phase.

For now, memory is write-mostly: the engine accumulates a structured record of the relationship, and serving it back into the LLM is a separate workstream. The frontend's `/comp/user/{user_id}/profile` endpoint returns the structured `companion_insights` JSONB which is the human-readable view of what's been collected.

## Source

- `crates/eros-engine-store/src/memory.rs` — `MemoryRepo` (upsert + search, 3 sqlx::test integration tests)
- `crates/eros-engine-llm/src/voyage.rs` — embedding client
- `crates/eros-engine-server/src/pipeline/post_process.rs` — write path
- `crates/eros-engine-store/migrations/0003_memory.sql` — schema + index DDL
