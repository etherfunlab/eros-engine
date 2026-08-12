# Voice `affinity_scope` — align the voice turn's scope field with the chat stream

**Date:** 2026-08-12
**Status:** approved
**Scope:** `eros-engine-server` (routes/voice, pipeline/voice, store/chat), docs, openapi

## Problem

The voice turn endpoint (`POST /comp/voice/{session_id}/turn/stream`) named its
relationship-line switch `relationship_scope` with its own value vocabulary
(`none | bond | chemistry | both`). The chat stream already had the right name
and vocabulary for the same concept: `affinity_scope`
(`full | bond_and_chemistry | bond | chemistry | none`, or an axes array).
`relationship_scope` was a naming mistake — this spec converges voice on
`affinity_scope` over two releases.

Migration principle: **old values keep flowing unchanged; new values flow only
under new names.** A deployer still sending the old field and reading the old
audit key changes nothing this release. A deployer who adopts the new field (or
notices the new audit keys) discovers the change from the new surface itself.
Next release both old surfaces disappear at once.

## Request contract (this release)

`VoiceTurnRequest` gains one field and keeps one:

- **`affinity_scope`** (new, optional) — exactly the chat stream's
  `AffinityScopeDto`: a named value `full | bond_and_chemistry | bond |
  chemistry | none`, **or** an axes array such as `["warmth", "trust"]`. The
  DTO and its `resolve()` are reused from `routes/companion_stream.rs`
  (`resolve()` becomes `pub(crate)`); no new type is introduced.
- **`relationship_scope`** (kept one release, deprecated) — unchanged
  vocabulary `none | bond | chemistry | both`.

Rules:

- The two vocabularies do not cross: `affinity_scope: "both"` → 422,
  `relationship_scope: "full"` → 422. Both fall out of the existing typed
  serde enums — no validation code.
- Both fields present → `affinity_scope` wins, silently. The losing
  `relationship_scope` still passes type validation first (a garbage value
  422s even when overridden).
- Neither present → default **`bond`**, matching the chat stream. This is a
  deliberate behavior change: today an omitted field injects both halves
  (`both`); after this release an omitted request injects only the bond half.

## Resolution and internal representation

The route resolves to the shared 6-bool `AffinityScope` struct, priority:

1. `affinity_scope` present → `AffinityScopeDto::resolve()`.
2. else `relationship_scope` present → map `None→none() / Bond→bond() /
   Chemistry→chemistry() / Both→full()`.
3. else → `AffinityScope::bond()`.

Internals rename and retype accordingly:

- `VoiceTurn.relationship_scope: RelationshipScope` →
  `affinity_scope: AffinityScope`.
- `build_voice_prompt` / `relationship_line` take `AffinityScope`.
- `relationship_line` flattens the six axes to the two halves it can inject:
  **bond half** iff any of warmth / intimacy / tension is active;
  **chemistry half** iff any of trust / intrigue / patience is active.
  The four legacy values map onto identical behavior through this flattening.

`RelationshipScope` in `eros-engine-core` survives only as the wire type of
the deprecated field; the pipeline no longer touches it.

## Audit metadata (deliberately asymmetric this release)

- **assistant row** — the old key **`relationship_scope` keeps being written**,
  its value back-projected from the resolved halves into the old vocabulary
  (`both / bond / chemistry / none`). Old-field requests audit byte-identical
  to today; new-field and axes-array requests still land truthfully in the old
  vocabulary. Additionally the row gains resolved **`memory_scope`** — the
  audit the voice path always lacked, brought in line with the chat stream.
- **user row** — `insert_voice_user_message` gains a metadata parameter and
  writes **`affinity_scope_raw`** (the DTO verbatim: named string or axes
  array) and **`memory_scope_raw`**, each only when the request carried the
  field (sparse, matching the chat stream). Written on INSERT only: a repair
  retry does not rewrite the first attempt's raw snapshot; the assistant row
  records what was actually served.

No resolved `affinity_scope` key is written anywhere this release — the new
resolved key appears next release when the old key is removed (see below).

## Tests

New:

- 422 on `affinity_scope: "both"` (and keep the existing
  `relationship_scope: "romance"` case; add `relationship_scope: "full"`).
- Precedence: both fields sent → assistant metadata reflects
  `affinity_scope`'s projection.
- Default: neither field → `bond` projection in metadata, bond half only in
  the prompt.
- Axes array resolves (e.g. `["trust"]` → chemistry half only).
- Metadata audit on both rows: new keys present when fields sent, absent
  otherwise; assistant row carries resolved `memory_scope`.

Adapted:

- `pipeline/voice.rs` test fixtures swap `RelationshipScope::*` for the
  equivalent `AffinityScope` constructors (mechanical, ~30 sites).
- Default-scope prompt expectations change from both halves to bond half.

## Documentation

- `docs/api-reference.md` / `docs/api-reference.zh.md` voice section:
  document `affinity_scope` (same value space as `message/stream`, default
  `bond`), mark `relationship_scope` deprecated with its removal slated for
  the next minor release, document the audit keys on both rows.
- `openapi.json` regenerated.
- Historical specs/plans that mention `relationship_scope` are archives — not
  updated.

## Next release (not in this PR)

- Delete the `relationship_scope` request field and the `RelationshipScope`
  enum from `eros-engine-core`.
- Assistant-row audit key `relationship_scope` is replaced by resolved
  `affinity_scope` (6-bool, fully symmetric with the chat stream).
