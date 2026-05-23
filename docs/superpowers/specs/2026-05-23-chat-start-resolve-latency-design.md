# eros-engine — chat/start session-resolution latency cuts (Spec)

**Status**: design, pending implementation plan
**Target release**: 0.3.x patch (pure server-side, no API/contract change)
**Issues**: #35 — `perf(bff): chat/start session resolution does ~8 sequential DB round-trips`; #37 — `chat/start 500s when the user has an archived persona_instance for the genome` (latent bug, fixed here as a side effect of the genome-path rewrite)
**Audience**: anyone implementing the engine-side speedup of `POST /comp/chat/start` and `POST /bff/v1/comp/chat/start`

---

## 0. Background

`resolve_or_create_session` (`crates/eros-engine-server/src/routes/companion.rs`) is the shared
resolution flow behind two endpoints:

- `POST /comp/chat/start` — canonical, returns session metadata only.
- `POST /bff/v1/comp/chat/start` — cold-mount bundle, returns the same plus slim history.

For the FE cold-mount path (`{ genome_id }`), it currently issues a **sequential waterfall** of
DB round-trips before the response is built. At an inter-region RTT of ~30 ms, ~8 serial hops are
~240 ms of pure DB wait on every conversation open — dominating time-to-first-paint of the thread.

The handlers are already async; the cost is **round-trip count**, not handler concurrency. This
spec removes that waterfall with three levers, all pure server-side:

1. **Drop redundant reads** — two reads query the same row twice.
2. **Parallelize independent reads** — `tokio::try_join!` (pool is 20 connections, so this is real
   parallelism, not serialized-on-one-conn).
3. **Fold the resume write** — collapse `SELECT latest session` + `UPDATE last_active_at` into one
   `UPDATE … RETURNING`.

As a side effect, the genome-path rewrite also fixes **#37**: today the create-fallback can hit
the unconditional `UNIQUE(genome_id, owner_uid)` and 500 when an archived instance exists for the
pair (see §2.1 / §4.1). The new upsert reactivates instead of erroring.

**No migration, no schema change, no API/contract change.**

### Terminology

Throughout, a **round-trip (RT)** means one *serial await wave* — one hop of latency. A
`tokio::try_join!` wave issues **two** DB queries but costs **one** wave of latency. The "before"
counts in §1 are serial awaits (each its own wave); the "after" counts in §3 are waves, with
`try_join!` waves explicitly noted as carrying two queries.

---

## 1. Current state (the waterfall)

### 1.1 `genome_id` path (FE hot path)

| # | Call | Query | Redundancy |
|---|------|-------|------------|
| 1 | `get_genome` | `SELECT … FROM persona_genomes WHERE id` | — |
| 2 | `get_asset_id_for_genome` | `SELECT asset_id FROM persona_genomes WHERE id` | **same row as #1** |
| 3 | `enforce_nft_ownership → owns` | gate join | NFT genomes only (legacy seed personas have `asset_id IS NULL` → skipped) |
| 4 | find instance | `SELECT id FROM persona_instances WHERE genome_id AND owner_uid AND status='active'` | independent of #1/#2 |
| 5 | `create_instance` | INSERT | new instance only |
| 6 | `load_companion` | `persona_instances ⋈ persona_genomes` | **re-fetches the genome already loaded at #1. The only field `resolve_or_create_session` *consumes* from it is `genome.name` (companion.rs:630), which #1 already has. It also re-asserts the instance is active+exists — but the instance was just produced by #4 (active-filtered) or #5 (created active), so that re-assert only covers a benign sub-ms archive race; see §2.1 TOCTOU note** |
| 7 | find latest session | `SELECT … FROM chat_sessions WHERE user_id AND instance_id ORDER BY last_active_at DESC LIMIT 1` | single table |
| 8 | bump | `UPDATE chat_sessions SET last_active_at = now()` | separate write, foldable into #7 |
| 9 | (BFF) `history_slim` | `SELECT … FROM chat_messages …` | runs even for brand-new sessions (history is empty) |

Legacy returning user (no NFT, instance + session both exist): 6 RT canonical / 7 RT BFF.

### 1.2 `instance_id` path (not the FE hot path)

`load_companion` is called **twice** (once for the owner check, again at #6), and
`get_asset_id_for_genome` re-reads `persona_genomes` that `load_companion`'s JOIN already touched.

---

## 2. Target design

### 2.1 `genome_id` path

```
RT-A (tokio::try_join!, parallel):
  · get_genome_gate(genome_id)      → { name, is_active, asset_id }   (one narrow read; replaces #1+#2)
  · find_active_instance(genome_id, user_id) → Option<Uuid>           (genome_id is from the request,
                                                                        so independent of the gate read)
validate: genome missing → 404; is_active=false → 400
RT-B (NFT only, asset_id.is_some()): owns() gate
instance_id = existing OR ensure_active_instance(genome_id, user_id)  (RT only when no active instance; gate already passed)
persona_name = gate.name                                              (no load_companion in this path)
RT-C: resume_latest_session(user_id, instance_id)                     (UPDATE … RETURNING; folds #7+#8)
        Some → resume (is_new=false)
        None → create_session_with_metadata (RT only on first-ever open, is_new=true)
RT-D (BFF only, and only when !is_new): history_slim
```

**NFT-gate ordering is preserved (load-bearing).** `owns()` still runs **before** any
`ensure_active_instance` write, so a non-owner who hits the create/reactivate fallback never leaves
(or revives) a `persona_instances` row. Reads (the instance lookup) may precede the gate — reads
create nothing.

**Fixes #37 (archived-instance 500).** The fallback uses `ensure_active_instance` — an
`INSERT … ON CONFLICT (genome_id, owner_uid) DO UPDATE SET status='active' RETURNING id` (§4.1) —
instead of a plain `create_instance`. Because the `UNIQUE(genome_id, owner_uid)` is unconditional
(`0004_persona.sql:19`), a user with an **archived** instance for the pair makes the active-filtered
lookup (RT-A) miss, and today's plain INSERT then violates the constraint → 500. The upsert
reactivates the single existing row instead. At most one row exists per pair, so this never
duplicates. This runs only after the gate, so the gate invariant above still holds.

**Dropping the final `load_companion` — TOCTOU note (why this is safe, not blocking).** Today's code
calls `load_companion` after find/create purely to read `genome.name` (which RT-A already has); its
`status='active'` filter (persona.rs:123) re-asserts something just established by RT-A's
active-filtered lookup or the fresh upsert. The only thing it can catch that the earlier steps
cannot is a concurrent archive landing in the sub-millisecond window *within the same request* — a
TOCTOU race that already exists today (the archive could equally land *after* `load_companion`
returns; it takes no lock). The consequence is benign: `chat_sessions.instance_id` has **no foreign
key** to `persona_instances` (`0001_chat.sql` — no `REFERENCES`) and no status coupling, so a session
row pointing at a just-archived instance corrupts nothing. The real fail-closed gate is at
**message time**: the chat pipeline calls `load_companion` per turn and an archived instance yields
`None`, so it cannot actually chat. We accept widening this benign window by microseconds in exchange
for removing the read.

### 2.2 `instance_id` path

```
RT-A: load_instance_gate(instance_id)  → { instance_id, genome_id, owner_uid, genome_name, asset_id }
        (one JOIN persona_instances ⋈ persona_genomes; replaces load_companion + get_asset_id_for_genome,
         and removes the duplicate load_companion call)
owner check: gate.owner_uid != user_id → 403
RT-B (NFT only): owns() gate
persona_name = gate.genome_name
RT-C: resume_latest_session(user_id, instance_id)  (same fold as genome path)
RT-D (BFF only, and only when !is_new): history_slim
```

Not parallelized: the gate read yields `genome_id`, which `asset_id` and `owns()` depend on; and the
session write must follow authz. Parallelizing here would either re-split the folded write or risk
writing before authz passes (same invariant as §2.1). Query-collapse (merging the two genome reads)
captures the safe win without that hazard.

### 2.3 Skip history for brand-new sessions

When `is_new == true`, the session has no messages — `history` is unconditionally empty. The BFF
handler returns `Vec::new()` instead of issuing `history_slim`.

---

## 3. Round-trip accounting

| Scenario | Before | After |
|----------|--------|-------|
| genome path, legacy, returning user (canonical) | 6 | **2** (RT-A parallel + RT-C) |
| genome path, legacy, returning user (BFF) | 7 | **3** (+ history) |
| genome path, NFT, returning user (BFF) | 8 | **4** (+ owns) |
| genome path, legacy, brand-new instance+session (BFF) | 8 | **4** (RT-A + ensure_active_instance + resume-miss + create_session; history skipped) |
| instance_id path, legacy, returning user (BFF) | 6 | **3** |

RT counts are *latency waves* (see Terminology). The "after" RT-A is one wave carrying two DB queries
(`get_genome_gate` + `find_active_instance`) via `try_join!`; the "before" column counts each query as
its own serial wave. So the dominant hot path drops from 7 serial waves to 3, two of which (RT-A) are
parallel.

Meets issue #35's "~2–3 round-trips" target on the dominant (legacy, returning) hot path.

---

## 4. Code surface

### 4.1 `crates/eros-engine-store/src/persona.rs`

- **New** `get_genome_gate(genome_id) -> sqlx::Result<Option<GenomeGate>>` where
  `GenomeGate { name: String, is_active: bool, asset_id: Option<String> }`.
  `SELECT name, is_active, asset_id FROM engine.persona_genomes WHERE id = $1`.
- **New** `find_active_instance(genome_id, user_id) -> sqlx::Result<Option<Uuid>>`.
  Lifts the existing inline `SELECT id FROM persona_instances WHERE genome_id AND owner_uid AND status='active'`
  into a method so it can sit inside `try_join!`.
- **New** `ensure_active_instance(genome_id, owner_uid) -> sqlx::Result<Uuid>` (fixes #37):

  ```sql
  INSERT INTO engine.persona_instances (genome_id, owner_uid)
  VALUES ($1, $2)
  ON CONFLICT (genome_id, owner_uid) DO UPDATE SET status = 'active'
  RETURNING id
  ```

  Replaces `create_instance` in the resolve genome-path fallback. Creates a new active instance, or
  reactivates an archived one (at most one row per pair, per the unconditional UNIQUE). `create_instance`
  itself **remains** for other callers (tests/testutil, etc.); only the resolve fallback switches.
- **New** `load_instance_gate(instance_id) -> sqlx::Result<Option<InstanceGate>>` where
  `InstanceGate { instance_id, genome_id, owner_uid, genome_name: String, asset_id: Option<String> }`.
  `persona_instances ⋈ persona_genomes`, `WHERE pi.id = $1 AND pi.status = 'active'`.
- `get_asset_id_for_genome` and `load_companion` **remain** (other callers, e.g. the pipeline, still
  use `load_companion`). `resolve_or_create_session` simply stops calling them.

### 4.2 `crates/eros-engine-store/src/chat.rs`

- **New** `resume_latest_session(user_id, instance_id) -> sqlx::Result<Option<ChatSession>>`:

  ```sql
  UPDATE engine.chat_sessions SET last_active_at = now()
  WHERE id = (
    SELECT id FROM engine.chat_sessions
    WHERE user_id = $1 AND instance_id = $2
    ORDER BY last_active_at DESC
    LIMIT 1
  )
  RETURNING *
  ```

  `fetch_optional`: `Some` = resumed (bumped), `None` = no session (caller creates). The `ORDER BY …
  LIMIT 1` subselect already accommodates a future "multiple sessions per user×instance" model — it
  resumes the most recent. `create_or_resume` (the older `SELECT`+`UPDATE` helper) is left untouched;
  only the resolve path moves to the folded method.

  Note: `RETURNING *` yields the **post-bump** row, whereas today's code reads the pre-bump row from
  the SELECT and then bumps separately (companion.rs:596-612). The only field consumed downstream is
  `id` (immutable), so pre- vs post-bump `last_active_at` is immaterial — behavior is identical.
  `RETURNING *` mirrors the existing `create_session_with_metadata` (`SELECT */RETURNING *`) and maps
  to `ChatSession` by column name, so the columns added by later migrations (0007/0008) come along
  automatically — same fragility profile as the existing `SELECT *` calls, nothing new.

### 4.3 `crates/eros-engine-server/src/routes/companion.rs`

- Rewrite `resolve_or_create_session` per §2.1 / §2.2. Public signature and `ResolvedSession`
  unchanged. `tokio::try_join!` for the two independent genome-path reads.

### 4.4 `crates/eros-engine-server/src/routes/bff/companion.rs`

- In `bff_start_chat`, gate the `history_slim` call on `!resolved.is_new` (§2.3).

---

## 5. Explicitly out of scope (considered, rejected for now)

- **Composite index `chat_sessions(user_id, instance_id, last_active_at DESC)`.** *(Decision: skip.
  Rationale corrected from an earlier draft.)* Note `chat_sessions` has **no** uniqueness on
  `(user_id, instance_id)` — only `idx_chat_sessions_user` and `idx_chat_sessions_instance`
  (`0001_chat.sql:12-13`) — and `create_session_with_metadata` unconditionally inserts a new row on
  every resume-miss, so **multiple sessions per pair are possible** (this spec itself contemplates
  that model). The `UNIQUE(genome_id, owner_uid)` on `persona_instances` constrains *instances*, not
  *sessions* — it does **not** bound the per-pair session count. So the resume lookup
  (`WHERE user_id AND instance_id ORDER BY last_active_at DESC LIMIT 1`) uses
  `idx_chat_sessions_instance` to narrow by `instance_id` and then **sorts** the matches; a composite
  index would turn that into a single top-1 index scan. We still skip it because: (a) today the
  per-pair session count is small, so the sort is cheap; (b) the dominant win is the round-trip fold
  (RT-C), not this query's plan; (c) it is the *only* DB-touching change here — migration + shared-DB
  drift risk + `CREATE INDEX CONCURRENTLY` cannot run inside sqlx's transactional migrations — for
  marginal benefit. Revisit if a pair accumulates many sessions or this query is measured as a
  bottleneck.
- **Parallelizing the `instance_id` path** (§2.2 rationale).
- **In-process cache of immutable genome metadata** — adds cache-invalidation hazard vs. manual
  `seed-personas` UPSERTs, for one saved round-trip.
- **Single big CTE / Postgres function** collapsing everything to ~2 RT — moves the load-bearing
  NFT-gate ordering invariant into SQL, harming readability/testability for a marginal gain over §2.

---

## 6. Testing

**Existing tests must continue to pass (behavioral contract):**

- `bff_start_brand_new_session_returns_empty_history`
- `bff_start_resumed_session_returns_history`
- `bff_start_history_limit_clamped_to_50`
- `bff_start_does_not_bundle_affinity_even_with_debug_open`
- `bff_start_403_on_nft_unowned_genome`
- `bff_start_matches_canonical_start_session_id` (both endpoints resolve the same session)
- `resolve_or_create_session_returns_resolved_for_legacy_genome`
- `resolve_or_create_session_nft_gate_blocks_unowned_genome` (no session AND no stray instance for a
  non-owner — the load-bearing gate-ordering invariant)
- `start_chat_creates_session_for_jwt_user_id`, `start_chat_403_on_unowned_nft_genome`,
  `start_chat_passes_for_legacy_genome`

**New tests:**

- `resume_latest_session` returns the most-recent of multiple sessions for a `(user, instance)` pair
  and bumps its `last_active_at` (store-level).
- Resume path bumps `last_active_at` (assert it advances across two `start` calls).
- `genome_id` path with no existing instance takes the create-fallback and returns `is_new=true` with
  empty history.
- `instance_id` path still 403s on a non-owner instance (net-new — no existing test covers this).
- **#37 regression:** `genome_id` path when the user already has an **archived** instance for the
  pair — assert it now **reactivates** (instance becomes `status='active'`, returns a session,
  `is_new=true`) instead of 500-ing on the `UNIQUE(genome_id, owner_uid)` violation. Pair this with a
  store-level test that `ensure_active_instance` flips an archived row back to active and returns its
  (stable) id.
- **Store-level gate-helper tests** (these carry load-bearing fields):
  - `get_genome_gate` returns `name`, `is_active`, and **`asset_id`** — guard against dropping
    `asset_id`, which would silently un-gate every NFT genome (security regression). Template:
    `persona.rs` `ownership_lookup_tests`.
  - `load_instance_gate` returns owner/genome-name/asset_id **and filters `status='active'`** (returns
    `None` for an archived instance). Template: `load_companion_skips_non_active_instances`.

---

## 7. Risks

- **`try_join!` and pool size.** Pool is 20 connections; the two parallel reads each acquire one. If
  the pool were ever shrunk to 1, the two queries serialize — still correct, just not parallel.
- **Brand-new-session race.** Two concurrent first-ever opens could both miss `resume_latest_session`
  and both `INSERT`, yielding two sessions. This race exists **today** (`SELECT` then `INSERT`); the
  fold does not worsen it. Not addressed here.
- **`UPDATE … RETURNING` on resume.** Concurrent opens of an existing session both bump
  `last_active_at` — idempotent, no correctness issue.
- **Reactivation semantics (#37 fix).** `ensure_active_instance` silently flips an archived instance
  back to `active` on chat-start. Given the unconditional `UNIQUE(genome_id, owner_uid)`, reactivation
  is the *only* way a user can ever chat with that genome again, so this is the intended "re-open"
  behavior, not a surprise. It runs only after the NFT gate, so a user who lost ownership cannot
  revive an instance. The explicit `instance_id` path does **not** reactivate (an archived id → 404).
- **Dropped active re-check (TOCTOU).** See §2.1 — widens an already-open, benign sub-ms race; the
  message-time `load_companion` is the real fail-closed gate. No new correctness exposure.
