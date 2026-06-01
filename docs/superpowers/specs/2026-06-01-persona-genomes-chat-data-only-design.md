# persona_genomes: chat-data-only reshape

`engine.persona_genomes` carries two columns that exist for an availability
judgment engine should not be making: `is_active` (a boolean usability gate)
and `avatar_url` (display data engine never reads). Both encode "is this
persona usable / how is it shown" — a downstream concern. This slice strips
the table down to the fields the chat pipeline actually consumes and removes
the engine's persona-availability surface entirely.

After this change the engine no longer answers "which personas exist" or
"can this persona be used." It stores chat data keyed by `genome_id`; the
downstream owns the catalog, availability, creator attribution, public/private
state, and avatars, keyed by the `genome_id` it learned at seed time.

Single slice, single PR onto `dev`. Breaking (schema + API). Mirrors the
`0023_drop_nft_ownership_stack` precedent: engine sheds a responsibility that
belongs downstream.

---

## Final schema

```
engine.persona_genomes (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name            TEXT NOT NULL,
    system_prompt   TEXT NOT NULL,
    tip_personality TEXT,
    art_metadata    JSONB NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
)
```

Exactly the fields the chat pipeline reads: `name` (response + prompt),
`system_prompt` (prompt), `tip_personality` (gift/tip reaction directives),
`art_metadata` (gender/age/mbti/backstory/… plucked into the prompt).

**Dropped:** `is_active`, `avatar_url`. **Not added:** `metadata`.

### Why no `metadata` column

The original idea was a `metadata` JSONB for downstream availability logic
(creator name, public/private). But with `GET /comp/personas` removed (below),
engine returns zero genome data over HTTP — nothing would read an engine-side
`metadata`. Downstream already owns its catalog; it stores creator /
public-private in its own tables keyed by `genome_id`. An engine column the
engine never reads and never exposes is dead weight. So: not added.

`persona_instances` is untouched — `(genome_id, owner_uid, status)` remains the
per-user instance model the chat pipeline relies on.

---

## Behavioral changes (both breaking)

1. **No genome API.** `GET /comp/personas` is removed. It was the *only*
   endpoint that read genome data (there is no `GET /comp/personas/{id}`), so
   the engine now exposes no genome data over HTTP at all. Genome fields are
   internal-only, consumed inside the chat pipeline via `load_companion`.

2. **No availability gate.** The `genome_id` path of
   `resolve_or_create_session` stops returning `400 "genome is not active"`.
   It now only checks existence: a genome row that exists → chat starts; a
   missing `genome_id` → `404 "genome not found"` (unchanged). The engine no
   longer judges whether a persona may be used — downstream gates that before
   it ever sends a `genome_id`.

---

## Migration

New migration `0024_persona_genomes_chat_data_only.sql`:

```sql
-- SPDX-License-Identifier: AGPL-3.0-only
-- Spec: docs/superpowers/specs/2026-06-01-persona-genomes-chat-data-only-design.md
--
-- DESTRUCTIVE. BREAKING. Strips persona_genomes to chat-relevant fields.
-- engine no longer judges persona availability or stores display data;
-- catalog / availability / avatar are downstream concerns keyed by genome_id.

ALTER TABLE engine.persona_genomes DROP COLUMN IF EXISTS is_active;
ALTER TABLE engine.persona_genomes DROP COLUMN IF EXISTS avatar_url;
```

Notes:
- Irreversible. `avatar_url` values are gone after this runs — a deployment
  that wants to keep them must export them out of `persona_genomes` first.
  Engine offers no preservation path because it never owned avatars as data.
- The `0013_supabase_lockdown` REVOKE/RLS statements that name
  `engine.persona_genomes` are unaffected (the table still exists).
- The `0024` number assumes this lands as the next migration; if another
  migration merges first, it becomes the next unused number at merge time.

---

## Files edited

### core — `crates/eros-engine-core/src/persona.rs`
- `PersonaGenome`: remove `avatar_url` and `is_active` fields.

### core — `crates/eros-engine-core/src/pde.rs`
- Fixture `PersonaGenome` literal (~line 181): drop the `avatar_url` and
  `is_active` lines.

### store — `crates/eros-engine-store/src/persona.rs`
- `GenomeRow`: remove `avatar_url`, `is_active`.
- `From<GenomeRow> for PersonaGenome`: remove the two field mappings.
- `GenomeGate`: remove `is_active` → struct carries `name` only. (Kept as a
  struct rather than collapsing to `query_scalar::<String>` for symmetry with
  `InstanceGate` and a stable call site.)
- `get_genome`: drop `avatar_url, is_active` from the SELECT. **Kept** — it is
  test-only today, but retained as the single-genome read for a possible
  future single-genome endpoint.
- `get_genome_gate`: SELECT `name` only.
- `load_companion`: drop `avatar_url, is_active` from the `Joined` struct, the
  SELECT, and the `PersonaGenome { … }` construction.
- `upsert_genome`: remove the `avatar_url` and `is_active` parameters and the
  corresponding INSERT/UPDATE columns (seed always passed `is_active = true`).
- **Remove `list_active`** entirely — its only non-test caller was the dropped
  endpoint.
- Tests: remove `list_active_filters_by_is_active`. In
  `get_genome_gate_returns_name_and_active`, drop the `is_active` assertions
  and the inactive-genome branch (rename to reflect "returns name"). Update the
  `insert_genome` test helper and any raw INSERTs to the new column set.

### server — `crates/eros-engine-server/src/routes/companion.rs`
- Remove `PersonaGenomeDto`, `ListPersonasResponse`, the `list_personas`
  handler, its `#[utoipa::path]`, and `routes!(list_personas)` from `router()`.
- In `resolve_or_create_session`: remove the
  `if !gate.is_active { return Err(BadRequest("genome is not active")) }`
  block. The `gate.ok_or_else(NotFound)` existence check stays.
- Tests: remove `comp_personas_401_without_bearer` and
  `comp_personas_returns_active_genomes`. Update `seed_genome` (~line 946) to
  the new column set. No server test asserts the inactive-genome 400, so
  nothing else changes here; `start_chat_passes_for_legacy_genome` stays. (The
  only `is_active`-based coverage is the store-layer `get_genome_gate` test
  already handled above.)

### server — `crates/eros-engine-server/src/main.rs`
- `run_seed_personas` → `PersonaFile`: remove the `avatar_url` field. Existing
  persona TOML files that still set `avatar_url` keep parsing — `PersonaFile`
  has no `deny_unknown_fields`, so the key is silently ignored.
- `upsert_genome(...)` call: drop the `f.avatar_url.as_deref()` and the
  trailing `true` (is_active) arguments.

### server — `crates/eros-engine-server/src/prompt.rs`
- `fixture_persona` `PersonaGenome` literal (~lines 709/718): drop `avatar_url`
  and `is_active`.

### server — test-fixture INSERTs
Each inserts `(name, system_prompt, art_metadata, is_active)` — drop the
`is_active` column and its bound value:
- `crates/eros-engine-server/src/routes/companion_stream.rs` (3 sites)
- `crates/eros-engine-server/src/pipeline/mod.rs`
- `crates/eros-engine-server/src/pipeline/stream.rs`

### OpenAPI — `crates/eros-engine-server/openapi.json`
- Regenerate via the `print-openapi` path (`cargo run -- print-openapi` or the
  repo's existing generation command). Removes the `/comp/personas` path and
  the `PersonaGenome`(Dto) + `ListPersonasResponse` schemas. CI's openapi
  drift check must pass.

### Docs / README
- Remove `GET /comp/personas` from any API listing.
- seed-personas TOML docs: drop `avatar_url`; note availability/catalog is the
  downstream's responsibility.

---

## Out of scope / deferred
- `persona_instances` shape, `status` semantics — untouched.
- A future single-genome read endpoint — `get_genome` is retained so it is a
  thin handler away, but no endpoint is added now.
- Version bump (this is breaking; workspace is `0.5.2-dev`). Per the release
  rule, the bump and tag are decided at release time, not here.

## Testing
- `cargo fmt`, `clippy`, `cargo test` (sqlx tests run the new migration).
- Regenerate and diff `openapi.json`; confirm CI drift check is green.
- Confirm seeding still works against an existing persona TOML that contains a
  now-ignored `avatar_url` key.
