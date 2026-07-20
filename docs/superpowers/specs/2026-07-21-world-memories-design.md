# eros-engine — World Memories (per-owner world simulation + prompt recall)

**Status**: design, pending implementation plan
**Target release**: `0.8.x` dev track. **Migration: 0035** (three new tables).
**Scope**: give each enrolled owner a "world" — the roster of personas they own —
whose relationships and daily happenings are simulated by a scheduled LLM
"world director". Each simulation round evolves a persistent **world seed**
(relationship graph + arc notes), emits per-persona **script fragments** stored
with embeddings, and per-persona **digests** for resident prompt injection.
At chat time the persona's digest plus top-k recalled fragments are injected as
a new `[world_memories]` prompt block. The whole subsystem is experimental:
generation is gated by a downstream-managed enrollment table, and injection has
its own kill switch so worlds can accumulate data before any prompt is touched.

Companion spec: `2026-07-21-world-town-design.md` (social feed built on the same
director output; implemented after this spec).

---

## 0. Decisions (settled during brainstorm)

- **World = owner roster.** `persona_instances.owner_uid` groups the world; in
  the current deployment the owner IS the chatting user (confirmed), so chat
  `user_id` and `owner_uid` are the same value. All world tables key on
  `owner_uid`.
- **Memory-feedback evolution.** Each director round reads the previous seed +
  persona genome summaries + recent **extracted** relationship-layer
  `companion_memories` (dreaming-lite output — never raw chat), so the world
  echoes what the user actually talked about.
- **The user is off-stage but mentionable.** Scripts describe persona↔persona
  life; personas may naturally reference the user (based on fed-back memories)
  but the director must never invent user actions.
- **Injection = resident digest + embedding recall.** The persona's
  seed-derived digest is always present (world-state continuity); top-k script
  fragments are recalled by cosine similarity against the current user message.
- **Cadence lives in model config, not per owner.** `[tasks.world_director]`
  carries task-specific scheduling fields (`interval_hours`), following the
  established "field read only on one task" pattern (`ghosting`,
  `input_filter`, `tts_audio_tags`).
- **Dedicated tables** (approach B). World memories have owner-level keying and
  date-based retention that don't fit `companion_memories`; existing recall
  paths stay untouched.
- **One structured LLM call per owner per round** (seed + all scripts + digests
  in a single response). Fewer round trips = smaller failure surface; a failed
  round is simply retried at the next due time.

---

## 1. Data model (migration 0035)

### 1.1 `engine.world_enrollments` — downstream writes, engine reads

```sql
CREATE TABLE engine.world_enrollments (
    owner_uid  UUID PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

Row present = world memories enabled for that owner. Downstream maintains rows
over a `service_role` connection (the stated reason: demo users should not burn
director compute). The engine never inserts or deletes here. Unenrolling stops
simulation and injection immediately; accumulated `world_states` /
`world_memories` rows are kept (re-enrolling resumes the same world). Cleanup
of abandoned worlds is an operational concern, out of scope.

### 1.2 `engine.world_states` — engine-private seed + scheduling state

```sql
CREATE TABLE engine.world_states (
    owner_uid    UUID PRIMARY KEY,
    seed         JSONB NOT NULL,
    digests      JSONB NOT NULL,          -- { "<instance_id>": "digest text", ... }
    seed_version INT NOT NULL DEFAULT 1,
    last_run_at  TIMESTAMPTZ,
    claimed_at   TIMESTAMPTZ,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

`seed` stores the director's structured output verbatim (relationship graph +
arc notes; the engine treats it as opaque). `claimed_at` is the
SKIP LOCKED claim stamp, same shape as `chat_sessions.classification_claimed_at`.

### 1.3 `engine.world_memories` — script fragments (recall layer)

```sql
CREATE TABLE engine.world_memories (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_uid   UUID NOT NULL,
    instance_id UUID NOT NULL REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    content     TEXT NOT NULL,
    embedding   VECTOR(512) NOT NULL,     -- voyage embed_document, same dim as companion_memories
    script_date DATE NOT NULL,            -- UTC date of the generating round; retention key
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_world_memories_owner_instance ON engine.world_memories (owner_uid, instance_id);
CREATE INDEX idx_world_memories_embedding ON engine.world_memories
    USING ivfflat (embedding vector_cosine_ops) WITH (lists = 100);
```

Deliberate simplifications: **no world-level event rows** (facts not tied to a
persona live inside seed/digests; `instance_id` stays NOT NULL), and **digests
are not embedded** (they are resident injection, fetched from `world_states`).

### 1.4 Lockdown

All three tables get the 0013 treatment in the same migration: REVOKE from
`anon`/`authenticated`, ENABLE ROW LEVEL SECURITY with no policies. The
`lib.rs` lockdown drift test extends its table list.

---

## 2. World director sweeper

### 2.1 Spawn & tick

`crates/eros-engine-server/src/pipeline/world.rs`, spawned from `run_server`
alongside the dreaming and snapshot sweepers. **Not spawned** when
`WORLD_DISABLED` is set or model config has no `[tasks.world_director]` section.
Tick loop: `tokio::time::interval(WORLD_TICK_SECS)` (env, default 300s),
`MissedTickBehavior::Delay`.

### 2.2 Due scan + claim (multi-instance safe)

Each tick:

1. Backfill state rows: `INSERT INTO engine.world_states (owner_uid, seed, digests)
   SELECT owner_uid, '{}', '{}' FROM engine.world_enrollments ON CONFLICT DO NOTHING`
   (empty seed marks a never-run world; the director prompt takes an
   "initialize world" branch when seed is empty).
2. Claim due owners:

```sql
UPDATE engine.world_states SET claimed_at = now()
WHERE owner_uid IN (
    SELECT ws.owner_uid
    FROM engine.world_states ws
    JOIN engine.world_enrollments we USING (owner_uid)
    WHERE (ws.last_run_at IS NULL OR ws.last_run_at < now() - $interval)
      AND (ws.claimed_at IS NULL OR ws.claimed_at < now() - $stale)
    ORDER BY ws.last_run_at ASC NULLS FIRST
    LIMIT $batch
    FOR UPDATE SKIP LOCKED
)
RETURNING owner_uid;
```

`$interval` = `[tasks.world_director].interval_hours` (default 24).
`$stale` = 30 minutes (code constant `WORLD_CLAIM_STALE`; reclaims claims left
by a crashed instance). `$batch` = 5 (code constant `WORLD_PICK_BATCH`).

### 2.3 Per-owner director call

One structured call per claimed owner. Prompt inputs:

1. Previous `seed` JSONB (or the initialize-world branch when empty).
2. Active persona roster: name / personality / backstory highlights from
   `persona_genomes` + `art_metadata`, for the owner's `status = 'active'`
   instances. **Cap 8 instances** (earliest-created wins); truncation emits a
   `tracing::warn`.
3. Memory feedback: the most recent **K=5** relationship-layer
   `companion_memories` rows per instance (extracted content only).
4. Fixed rules: user is off-stage, may be referenced, never fabricate user
   actions; fragments must be self-contained sentences suitable for standalone
   recall.

Output (`response_format: json_schema`, same mechanism as the PDE judge's
`structured_output`):

```json
{
  "seed": { "...": "opaque relationship graph + arc notes" },
  "personas": [
    {
      "instance_id": "uuid",
      "digest": "world-state summary from this persona's perspective",
      "script_fragments": ["fragment 1", "fragment 2", "..."]
    }
  ]
}
```

(The world-town spec extends this same response with a `posts` array — no
additional director call.)

### 2.4 Persist

1. Batch-embed all fragments via Voyage `embed_document`.
2. Single transaction:
   - `DELETE FROM engine.world_memories WHERE owner_uid = $1 AND script_date < $today - retention_days`
   - INSERT the new fragments (`script_date` = UTC date of the run)
   - `UPDATE engine.world_states SET seed = $2, digests = $3,
     seed_version = seed_version + 1, last_run_at = now(), claimed_at = NULL,
     updated_at = now() WHERE owner_uid = $1`

Any failure (LLM error, schema-parse failure, embed failure, DB error): roll
back, reset `claimed_at` to NULL, `tracing::warn`, and let the owner retry at
its next due time. No retry queue, no partial writes. Unknown `instance_id`s
in the response are dropped with a warn; missing personas simply keep their
previous digest absent.

### 2.5 Model config

```toml
[tasks.world_director]
model = "..."
# fallback / temperature / max_tokens / reasoning as usual
interval_hours = 24     # task-specific: director cadence per owner
retention_days = 30     # task-specific: world_memories script retention
```

New `resolve_world_director()` resolver returning the resolved model params +
the two task-specific fields. Both fields are read only on this task section.

### 2.6 Audit

New constant `WORLD_AUDIT_USER = "11111111-1111-1111-1111-111111111112"`
(distinct from dreaming's `SYSTEM_AUDIT_USER` `...111` so OpenRouter usage is
attributable per subsystem). Every director request sets `user = WORLD_AUDIT_USER`
and logs via `log_openrouter_usage("world_director", None, &raw)`.
`docs/llm-audit.md` gains a row for the task.

---

## 3. Chat injection

### 3.1 Env switches (`ServerConfig` + pure parser fns + unit tests)

| Var | Default | Meaning |
|---|---|---|
| `WORLD_DISABLED` | off | Master switch: no sweeper spawn, no injection. Subsystem is zero-cost when off. |
| `WORLD_PROMPT_DISABLED` | off | Injection-only switch: simulation keeps running and accumulating data, chat prompts untouched. This is the experimental isolation valve. |
| `WORLD_TICK_SECS` | 300 | Sweeper tick. |

Boolean parsing accepts `true`/`1` (exact convention of `DREAMING_DISABLED` /
`EXPOSE_AFFINITY_DEBUG`: `v == "1" || v == "true"`); `.env.example` documents
the `true`/`false` form.

### 3.2 Recall path (zero extra round trips)

In `build_reply_request`, when both switches allow, add one concurrent branch
to the existing recall fan-out:

1. Fetch digest: single query
   `SELECT ws.digests FROM engine.world_states ws JOIN engine.world_enrollments we USING (owner_uid) WHERE ws.owner_uid = $user`
   then pick `digests[instance_id]`. Enrollment check and digest fetch are the
   same query.
2. Fragment recall: cosine top-k on `world_memories` filtered
   `owner_uid = $user AND instance_id = $instance`, `WORLD_RECALL_K = 3`
   (constant, same style as `PROFILE_RECALL_K`), **reusing the query embedding
   already computed** by the standard memory recall — no additional Voyage
   call. When the standard recall path is skipped (memory scope flags), world
   injection degrades to digest-only rather than paying a new embed call.

No enrollment / no state row / digest missing for this instance → branch yields
`None` and the prompt is byte-identical to today. Query failure → `tracing::warn`,
treated as `None`; the chat main path never blocks on world data.

### 3.3 Prompt block

`build_prompt` gains `world: Option<WorldContext>` (digest + fragments). New
block **`[world_memories]`**, placed after `[shared_memories]` and before the
attitude/affinity block:

```
[world_memories]
（你所在小圈子的近况，可自然提及；用户不在场，但通过你们的交流知道这些事）
<digest>
- <fragment 1>
- <fragment 2>
- <fragment 3>
```

Empty → block omitted entirely. The block sits in the variable section of the
prompt; the stable `{head}` cache prefix is unaffected.

---

## 4. Testing

- `ServerConfig` parser unit tests for the three env vars.
- `model_config` unit tests: `interval_hours` / `retention_days` defaults and
  overrides; `resolve_world_director()`.
- Store (sqlx, local pg): enrollment-join due scan; claim SKIP LOCKED + stale
  reclaim; fragment insert/recall ordering; retention delete boundary.
- `build_prompt` snapshot tests: with and without the `[world_memories]` block.
- Director round: schema-parse failure writes nothing and resets the claim.
- Lockdown drift test covers the three new tables.

---

## 5. Out of scope

- World town (posts/comments/replies) — `2026-07-21-world-town-design.md`.
- Per-owner cadence, retry queues, world-level event rows, digest embeddings.
- Any automatic cleanup of unenrolled owners' world data.
- Downstream UI for managing `world_enrollments` (engine ships the table; the
  OSS side documents it in the migration comment only).
