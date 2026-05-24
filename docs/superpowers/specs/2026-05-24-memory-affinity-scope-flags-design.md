# Per-Request `memory_scope` & `affinity_scope` Flags — Design

**Status:** Draft for review
**Date:** 2026-05-24
**Owner:** @enriquephl
**Issue:** #40 (cross-companion bleed of intimate/NSFW user insights)

## Problem

Issue #40: a male persona surfaces a user's NSFW/intimate preferences that were
only ever established in chats with *other* personas. Two engine-level causes:

1. **`companion_insights` ("基础画像") is user-global** and injected into every
   persona's prompt — its intimate fields (感情观/情感需求/兴趣) bleed across
   personas, every turn, regardless of relevance.
2. **The `companion_memories` profile layer (`instance_id IS NULL`) is also
   user-global** and recalled into every persona's prompt. Same bleed, just
   query-relevance gated rather than dumped every turn. (Not named in #40, but
   the same class of leak.)

This spec implements the **caller-driven interim** from the issue thread: the
calling app already knows the user's gender/orientation and the persona's gender,
so it decides per request how much memory and which affinity axes to inject, and
tells the engine via two optional flags. The engine treats them as opaque inputs
— no engine↔consumer schema coupling.

## Non-Goals

- **No `user_orientation` field / orientation 铁律.** The complementary
  deterministic romance-gating guardrail from #40 is a separate change.
- **No sensitivity tagging of memories.** The precise end-state (tag each
  insight/memory as neutral vs intimate, route intimate to per-instance scope)
  is deferred. This flag is the coarse interim; `affinity_scope` + the
  neutral-default mitigate the worst bleed now.
- **No change to the post-process pipeline.** Regardless of either flag, after
  every turn the engine still extracts insights, writes both memory layers, and
  evaluates + persists all six affinity axes exactly as today. **These flags gate
  prompt *injection* only — never *write* behavior.**
- **No new persistence.** Both flags are re-sent per request; nothing is stored.

## Compatibility — IMPORTANT: defaults change behavior

Unlike `prompt_traits`, **omitting these fields does NOT preserve today's
output.** That is intentional — the new defaults are the #40 mitigation:

| Flag | Omitted-default | vs. today |
|------|-----------------|-----------|
| `memory_scope` | `neutral_and_relationship` | drops intimate insights + global memory layer from the default prompt |
| `affinity_scope` | `bond` | injects 3 axes (warmth/intimacy/tension) instead of all 6 |

Byte-for-byte parity with today is reachable explicitly: `memory_scope:"full"`
**and** `affinity_scope:"bond_and_chemistry"` (≡ `"full"`) together reproduce the
current system prompt. A golden test pins this (see Tests).

## Current injection map (baseline)

Memory-related prompt sections built in `build_prompt` (`prompt.rs:447-478`):

- **① 【你对他的了解（通用画像）】** (`prompt.rs:458`) = `profile_groups`, which
  blends two **user-global** sources (`handlers.rs:358-364`):
  - `基础画像` — `companion_insights` bullets (`load_insight_bullets`,
    `handlers.rs:281-292`; rendered by `insights_to_bullets`,
    `handlers.rs:298-336`).
  - profile-layer memory recalls — `companion_memories WHERE instance_id IS NULL`
    (`handlers.rs:190-202`).
- **② 【你们之间的事（只有你和他知道）】** (`prompt.rs:460`) = `relationship_facts`
  = `companion_memories WHERE instance_id = <persona>` (`handlers.rs:192,204`).
  Instance-scoped; no bleed.

Affinity-related injection:

- **③ 【你此刻的心情】** attitude directives — `affinity_to_attitude_prompt`
  (`prompt.rs:113-163`), per-axis thresholds.
- **④ 【你对他的内心感受】** raw six-axis numbers (`prompt.rs:409-417`).
- **⑤ 铁律① reply length** — `length_rule` (`prompt.rs:98-110`), keyed on
  `intimacy`.

## Building blocks & taxonomy

Authoritative definitions agreed for this spec:

| Block | Meaning | Source (read path) |
|-------|---------|--------------------|
| **W**  | full profile facts | `human_insights` row, all 8 rendered fields |
| **W'** | neutral profile facts | `human_insights` row minus `love_values`, `emotional_needs`, `interests` → `city`, `occupation`, `mbti_guess`, `life_rhythm`, `personality_traits` |
| **X**  | global memory recall | `companion_memories` `instance_id IS NULL` |
| **Y**  | relationship memory recall | `companion_memories` `instance_id = <persona>` |

`U` (section ①) = W/W' + X; `V` (section ②) = Y.

**Read-path change:** W/W' are read from the flat `human_insights` table
(`HumanInsightRepo::load`, `human_insight.rs:165`) — direct column SELECT, no
JSONB flattening. **Write path is unchanged:** post-process still writes the
authoritative `companion_insights` first, then mirrors to `human_insights` via
`project_from_insights` (`human_insight.rs:120`). The matching-only columns
(`preferred_gender`, `age_min/max`, `deal_breakers`) are **never** rendered into
the prompt.

The intimate fields {`love_values`, `emotional_needs`, `interests`} are only ever
injected via W (i.e. `full` / `insights_only`).

## `memory_scope` design

### Wire values & mapping

```jsonc
"memory_scope": "full" | "neutral_and_relationship" | "relationship_only"
              | "neutral_only" | "insights_only" | "none"   // optional
```

| scope | 基础画像 | X (global mem) | Y (relationship mem) | U | V |
|-------|----------|----------------|----------------------|------|-----|
| `full` | W full | on | on | W+X | Y |
| `neutral_and_relationship` *(default)* | W' neutral | on | on | W'+X | Y |
| `relationship_only` | off | off | on | ∅ | Y |
| `neutral_only` | W' neutral | off | off | W' | ∅ |
| `insights_only` | W full | off | off | W | ∅ |
| `none` | off | off | off | ∅ | ∅ |

`insights_only` can still bleed (full user-global insights, no memory) — kept for
enum completeness; callers should prefer the others.

Resolution helper (core): `MemoryScope → (InsightMode, x_on, y_on)` where
`InsightMode ∈ { Off, Neutral, Full }`.

### Recall short-circuit (efficiency, not semantics)

Across all six scopes, **X-on ⟹ Y-on**, so:

- `x_on || y_on == false` (`neutral_only`, `insights_only`, `none`) → skip the
  embedding call **and** all vector searches in `recall_memory`.
- `x_off && y_on` (`relationship_only`) → compute embedding, run **only** the
  relationship-layer search; skip both profile-layer searches.
- `x_on` (`full`, `neutral_and_relationship`) → full recall (current behavior).

The `human_insights` read is independent, gated by `InsightMode`; `Off` skips it.

## `affinity_scope` design

### Wire values

```jsonc
"affinity_scope": "full" | "bond_and_chemistry" | "bond" | "chemistry" | "none"
                | ["warmth","trust", ...]            // optional
```

Axis names are the `Affinity` struct fields (`eros-engine-core/src/affinity.rs`):
`warmth, trust, intrigue, intimacy, patience, tension`.

| named scope | axis set |
|-------------|----------|
| `bond` *(default)* | `{warmth, intimacy, tension}` |
| `chemistry` | `{trust, intrigue, patience}` |
| `full` ≡ `bond_and_chemistry` | all six |
| `none` | `{}` |
| array | the listed axes (subset of the six) |

Resolution helper (core): `AffinityScope → AxisSet`.

### What the axis set gates

The resolved axis set governs **③ + ④** per-axis:

- **③ attitude directives** (`affinity_to_attitude_prompt`): for an axis not in
  the set, its directive block is skipped entirely. Empty set → no
  【你此刻的心情】 section.
- **④ raw numbers**: print only in-set axes. Empty set → omit the
  【你对他的内心感受】 block entirely.

### ⑤ `length_rule` — composite-driven

`length_rule` is recomputed from a composite score selected by the axis set.
Composite formula (all six raw values are always available — the scope gates
injection, not the affinity record):

```
warm01    = clamp01((warmth + 1) / 2)
bond      = clamp01((warm01 + intimacy + tension) / 3)
chemistry = clamp01((trust + intrigue + patience) / 3)

bond_active = axisset ∩ {warmth, intimacy, tension} ≠ ∅
chem_active = axisset ∩ {trust, intrigue, patience} ≠ ∅

score =
  (bond_active && chem_active) → (bond + chemistry) / 2
  (bond_active)                → bond
  (chem_active)                → chemistry
  (neither)                    → DEFAULT (no score)
```

This reduces exactly to the named cases (`full`→avg, `bond`→bond,
`chemistry`→chemistry, `none`→default) and generalizes the array form.

Tier thresholds reuse the existing `0.25 / 0.55` (the composite is also in
`[0,1]`; thresholds tunable, comment to say so). The "DEFAULT (no score)" branch
maps to the current `affinity = None` tier — strictest: `刚认识，1~2 句，≤40 字`.
So `affinity_scope:"none"` and "no affinity data yet" share the strictest tier.

## Type plumbing

### `eros-engine-core/src/types.rs`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Full,
    #[default]
    NeutralAndRelationship,
    RelationshipOnly,
    NeutralOnly,
    InsightsOnly,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AffinityAxis { Warmth, Trust, Intrigue, Intimacy, Patience, Tension }

// Resolved axis set carried on the event (route resolves DTO → this).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AffinityScope { /* 6-bool mask or EnumSet */ }
```

- `MemoryScope::resolve() -> (InsightMode, bool /*x*/, bool /*y*/)`.
- `AffinityScope::contains(axis)`, `::is_empty()`, and the composite helper for
  length live here (pure, unit-tested without a DB).
- Default for `AffinityScope` when the field is omitted = `bond` triad.

Extend `Event::UserMessage` (mirrors the existing `tier`/`audit` pattern,
`types.rs:38-57`):

```rust
UserMessage {
    content: String,
    message_id: Uuid,
    #[serde(default)] prompt_traits: Vec<PromptTrait>,
    #[serde(default)] audit: Option<LlmAudit>,
    #[serde(default)] tier: Option<String>,
    #[serde(default)] memory_scope: MemoryScope,      // NEW (defaults via Default)
    #[serde(default)] affinity_scope: AffinityScope,  // NEW (defaults to bond)
}
```

### `eros-engine-server/src/routes/companion_stream.rs`

`StreamSendRequest` (`:36-46`) gains two optional fields + a DTO for the
string|array affinity form:

```rust
#[derive(Deserialize, ToSchema)]
#[serde(untagged)]
pub enum AffinityScopeDto {
    Named(AffinityScopeName),     // full|bond_and_chemistry|bond|chemistry|none
    Axes(Vec<AffinityAxis>),      // ["warmth", ...]
}

pub struct StreamSendRequest {
    // ... existing ...
    #[serde(default)] pub memory_scope: Option<MemoryScope>,
    #[serde(default)] pub affinity_scope: Option<AffinityScopeDto>,
}
```

Handler (`:139-240`): resolve `None → default`; convert `AffinityScopeDto →
AffinityScope` (expand named → triads; `[]` → empty set ≡ `none`). Validation is
deserialize-driven:

| input | result |
|-------|--------|
| unknown `memory_scope` string | `400 BadRequest` (serde) |
| unknown axis name in array | `400 BadRequest` (serde, untagged fails both arms) |
| unknown `affinity_scope` string | `400 BadRequest` |
| `affinity_scope: []` | empty set ≡ `none` |
| field omitted | per-flag default |

### `eros-engine-server/src/pipeline/stream.rs`

`PersistedUserMessage` (`:245-254`) gains `memory_scope: MemoryScope` and
`affinity_scope: AffinityScope`. `run_stream` (`:313-324`) forwards both into
`Event::UserMessage`. The three test construction sites (`:735,857,931`) get the
defaults.

### `eros-engine-server/src/pipeline/handlers.rs`

`build_reply_request` (`:342-364`):

```rust
let (mem_mode, x_on, y_on) = memory_scope.resolve();

let (mut profile_groups, relationship_facts) =
    recall_memory_gated(state, user_id, instance_id, query_text, x_on, y_on).await;
    // both off → returns (vec![], vec![]) without embedding/search

if mem_mode != InsightMode::Off {
    let bullets = load_human_insight_bullets(&state.pool, user_id, mem_mode).await;
    if !bullets.is_empty() { profile_groups.insert(0, ("基础画像".into(), bullets)); }
}
// ... build_prompt(..., affinity_scope) ...
```

- New `load_human_insight_bullets(pool, user_id, mode)` reads
  `HumanInsightRepo::load` and renders bullets from columns, **replicating
  `insights_to_bullets`' label/order/trim/empty-skip logic** (`Full` = 8 fields;
  `Neutral` = drop the 3 intimate fields). Order pinned to match today:
  城市, 职业, MBTI, 感情观, 兴趣, 情感需求, 作息, 性格特质.
- `recall_memory` / `recall_memory_with_embedding` gain `x_on`/`y_on` (or a new
  gated wrapper) implementing the short-circuit above.

### `eros-engine-server/src/prompt.rs`

`build_prompt` gains an `affinity_scope: AffinityScope` parameter (already
`#[allow(clippy::too_many_arguments)]`). It threads to:

- `affinity_to_attitude_prompt(a, scope)` — skip out-of-scope axis directives;
  empty → "".
- raw-values block — filter to in-scope axes; empty → "".
- `length_rule(affinity, scope)` — composite score per above.

## Observability

In the stream pipeline's existing tracing, append:
`memory_scope=<...> affinity_axes=<n>` (counts/enum only; never insight or memory
content).

## OpenAPI

`StreamSendRequest`, `MemoryScope`, `AffinityScopeDto`, `AffinityAxis` derive
`ToSchema`. The repo has a CI snapshot drift check — regenerate the snapshot or
CI fails.

## Tests

### Core (`eros-engine-core`, unit, no DB)

- `memory_scope_resolves_to_expected_switches` — table test of all 6 →
  `(InsightMode, x, y)`.
- `affinity_scope_named_expands_to_triads` — bond/chemistry/full/none + array.
- `affinity_scope_defaults_to_bond`; `memory_scope_defaults_to_neutral_and_relationship`.
- `length_composite_matches_named_cases` — full→avg, bond→bond, chemistry→
  chemistry, none→default; plus an array case activating both.
- `clamp01_and_warm01_mapping`.

### Prompt (`prompt.rs`, unit)

- `full_scope_matches_current_output` — golden: `memory_scope=full` +
  `affinity_scope=bond_and_chemistry` reproduces today's prompt byte-for-byte
  (note: W comes from a `human_insights`-shaped fixture identical to the
  `companion_insights` fixture).
- `neutral_only_drops_intimate_fields` — 感情观/情感需求/兴趣 absent; 城市/职业
  present.
- `attitude_and_values_filter_by_axis_set` — bond shows only warmth/intimacy/
  tension in ③ and ④; `none` omits both blocks.
- `length_rule_uses_chemistry_composite_when_scoped`.

### Routes / pipeline (`sqlx::test`)

- `defaults_when_flags_omitted` — body without flags →
  neutral_and_relationship + bond (assert intimate insight + global-memory blocks
  absent, neutral facts + relationship present).
- `relationship_only_skips_profile_and_insights`.
- `none_skips_all_memory_and_affinity_blocks`.
- `invalid_memory_scope_400`; `invalid_affinity_axis_400`;
  `empty_affinity_array_equals_none`.
- `flags_do_not_change_post_process` — after a turn with `memory_scope:"none"`,
  insights + both memory layers + all six affinity axes are still written.

## Risks / Open Questions

1. **`human_insights` backfill.** The read path now depends on every active user
   having a mirrored `human_insights` row. The table was added 2026-05-21; users
   who haven't chatted since may lack a row → empty 基础画像 until their next
   post-process. **Mitigation (recommended):** one-time backfill migration
   projecting all `companion_insights` → `human_insights`. **Alternative:**
   fallback to `companion_insights` when the row is missing. Decide before ship.
2. **Byte-identical `full` via `human_insights`.** Switching W's source could
   diverge from today if the projection normalizes a value differently
   (whitespace, empty handling). Mitigated by replicating `insights_to_bullets`'
   trim/skip logic in the new renderer + the golden test. If divergence is found,
   reconsider sourcing W from `companion_insights` while keeping W' from
   `human_insights`.
3. **Default behavior change is a prod-visible shift.** Existing callers that
   send neither flag will immediately get the narrowed default. This is the
   intended #40 mitigation, but coordinate the rollout with eros-chat so the
   change is expected.
4. **Length thresholds reused as-is.** `0.25 / 0.55` were tuned for raw
   `intimacy`; the composite distribution differs slightly. Shipped as tunable;
   revisit if reply lengths feel off.
5. **`affinity_scope:"none"` length.** Falls to the strictest tier (≤40 字). If a
   caller wants "no affinity expressed but normal length," that's not
   expressible — acceptable for the interim.

## Acceptance Criteria

- [ ] `cargo test -p eros-engine-server -p eros-engine-core` green
- [ ] OpenAPI snapshot regenerated; CI drift check green
- [ ] `full` + `bond_and_chemistry` reproduces today's prompt byte-for-byte
- [ ] Omitted flags → `neutral_and_relationship` + `bond` (verified: no intimate
      insight block, no global-memory block, neutral facts + relationship + 3-axis
      affinity present)
- [ ] Post-process writes (insights, both memory layers, all six affinity axes)
      unchanged under every scope, including `none`
- [ ] `human_insights` backfill decision made and (if chosen) migration landed
