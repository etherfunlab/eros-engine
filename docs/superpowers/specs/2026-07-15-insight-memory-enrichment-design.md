# Insight + memory enrichment: fact `details`, memory `metadata`, Stage-2 slot expansion

**Date:** 2026-07-15
**Status:** Design approved, ready for implementation plan
**Related:** eros-reports `dev-logs/2026-07-15_dreaming_full_design_autobio.md` (design
rationale; §3.1 is this spec's scope), eros-engine-web issue #181 (dreaming-full
constraints)

## Summary

Thicken what the extraction pipeline captures about the user, moving from
"栏位式、简单描述" to "方向式、丰富描述", without touching the pipeline's
mechanics. Three engine changes:

1. **Stage-1 fact `details`** — the per-turn facts extraction may now return a
   sibling `details[]` array carrying per-fact structured metadata (category /
   domain / evidence_type / temporality / persistence / confidence). The engine
   persists it **opaquely** in the existing `companion_insights_events` audit
   stream; it validates structure only, never vocabulary.
2. **Memory `metadata`** — session-end memory extraction may now return the same
   metadata dimensions per memory. A new nullable `metadata JSONB` column on
   `engine.companion_memories` stores them opaquely; the recall path ignores
   them for now.
3. **Stage-2 slot expansion** — six new string slots in the structured insights
   schema (`education`, `family`, `relationship_history`, `social_pattern`,
   `future_plans`, `finance_status`), mirrored into `human_insights`, rendered
   into the 基础画像 chat-prompt section with per-field intimacy tiers, and
   folded into a rebalanced `training_level` weight table.

This is PR A of a two-PR cross-repo effort. PR B (private, eros-engine-web
`infra/engine`) redeploys downstream with the new engine image plus production
prompts that actually emit `details` / `metadata`. The OSS example config ships
reference versions of both prompts so the default deployment exercises the new
paths.

Explicitly **not** in scope: autobio / dreaming-full (dev-log §3.2), any
recall-time use of memory metadata, any A2A-match consumer, engine-side
enforcement of the metadata vocabularies, backfill of historical event rows.

## Background

### Insight extraction today (per-turn, chat post-process)

`extract_insights` (`crates/eros-engine-server/src/pipeline/post_process.rs`)
runs two stages tied by a shared `run_id`:

- **Stage 1 "facts"** — system prompt from `insight_extraction.filter_prompt`
  (downstream-owned config); the engine parses only `{"facts": ["…"]}` via
  `extract_facts_array` and writes the bare string array as the
  `stage='facts'` event payload.
- **Stage 2 "structured"** — hardcoded prompt
  (`extract_structured_insights_prompt` + `COMPANION_INSIGHTS_SCHEMA` in
  `prompt.rs`) fills a fixed 12-slot JSONB, shallow-merged into
  `companion_insights`, then mirrored to the flat `human_insights` row
  (`project_from_insights`), which both the chat prompt's 基础画像 section and
  matching queries read.

**The gap:** if a downstream prompt asks the model for richer per-fact
metadata, the engine parses `facts` and silently drops everything else. The
metadata is generated, paid for, and discarded.

### Memory extraction today (session-end, dreaming sweeper)

`pipeline/dreaming.rs` runs `memory_extraction.filter_prompt` over a finished
session and parses `{"memories": [{content, category}]}` into
`MemoryCandidate { content, category }`. Rows land in
`engine.companion_memories` (content + free-text category + 512-dim embedding).
Same gap: any additional per-memory fields the prompt requests are dropped by
serde.

### The slot schema's origin

The 12 Stage-2 slots were designed for heuristic matching (geo filters, hard
filters, soft scoring). They are adequate for the chat prompt but thin for
matching: nothing captures education, family situation, relationship history,
social patterns, future plans, or financial context. Dev-log §3.1 calls for
adding slots (safe under shallow-merge semantics) while keeping the mechanism
untouched.

## Design principles

- **Engine stores shape, prompts own vocabulary.** The six metadata dimensions
  and their enum values (e.g. `evidence_type: roleplay_expression`) live
  entirely in the prompt text. The engine treats `details` items and memory
  `metadata` as opaque JSON. Downstream can evolve the taxonomy without an
  engine release.
- **Stage-2 keeps its contract.** `facts` (plain strings) drives Stage 2
  exactly as today. The engine does **not** enforce `facts[i] ==
  details[i].content` or equal lengths; a drifting prompt degrades the audit
  stream, never the chat path.
- **No behavior change without the new prompts.** A deployment running the old
  facts-only / memories-only prompts on the new engine sees `details: []` and
  `metadata: NULL` — everything else identical (modulo the A3 schema additions,
  which are themselves inert until facts support them).

## A1. Persist Stage-1 `details` (no migration)

In `extract_facts` (`post_process.rs`), after parsing the reply JSON:

- Read `details` as a sibling of `facts`: `v.get("details")` as an array of
  arbitrary JSON values, cloned as-is. Missing key, non-array, or parse
  weirdness ⇒ `[]`. No per-item validation, no zipping against `facts`.
- Change the `stage='facts'` event payload from the bare facts array to a
  uniform object:

  ```json
  { "facts": ["用户在深圳工作"], "details": [ { "content": "…", "category": "…", … } ] }
  ```

- Status taxonomy unchanged and keyed on `facts` alone: `ok` (facts
  non-empty), `empty` (facts empty — payload still written with whatever
  parsed), `parse_error` (no JSON found — payload stays `NULL`).
- `extract_facts_array`, the early-return on empty facts, Stage 2, and the
  merge/projection chain are untouched.

**Payload-shape compat note:** `companion_insights_events.payload` for
`stage='facts'` changes from JSONB array to JSONB object. Historical rows stay
arrays. Audit/analysis readers (eros-audit) must branch on
`jsonb_typeof(payload)` or read `payload->'facts'` with an array fallback.
This is a one-time, downstream-internal break, accepted in review.

## A2. Memory `metadata` column (migration 0032)

**Migration `0032_companion_memories_metadata.sql`:**

```sql
ALTER TABLE engine.companion_memories
    ADD COLUMN metadata JSONB;
```

Nullable, no default, no index — metadata-only ALTER, no table rewrite.
`NULL` = extractor supplied no metadata (raw-turn rows, relationship rows,
old-prompt deployments).

**Code changes:**

- `MemoryCandidate` (`dreaming.rs`) gains
  `#[serde(flatten)] metadata: serde_json::Map<String, serde_json::Value>` —
  captures every key besides `content` / `category` opaquely (the prompt's
  dimensions today: domain, evidence_type, temporality, persistence,
  confidence; anything future rides along free). Malformed items are still
  skipped by the existing `filter_map` deserialization.
- `MemoryRepo::upsert` (`store/src/memory.rs`) gains a
  `metadata: Option<&serde_json::Value>` parameter, bound into the INSERT.
  The dreaming sweeper passes `Some(obj)` when the flattened map is non-empty,
  else `None`. The raw-turn writer and the relationship-memory writer pass
  `None`.
- `MemoryRow` gains `pub metadata: Option<serde_json::Value>`; the explicit
  SELECT column lists in `memory.rs` add the column. No recall logic reads it
  yet — `search_profile_grouped` still partitions by `category` only.
- **`category` stays a first-class column and keeps its current 5-value
  vocabulary** (fact / preference / event / emotion / relation) because it is
  the recall grouping key (`k_per_category`). The new dimensions deliberately
  exclude `category` from `metadata`.

## A3. Stage-2 slot expansion (migration 0033)

### New slots

Six string slots join the structured schema. Reference intent (exact prompt
prose is an implementation detail; keep the existing "写具体、带细节、禁孤立
标签" discipline and one example each):

| key | 含义 | 例 |
|---|---|---|
| `education` | 学历/学校/专业/在读状态 | 985 本科计算机，毕业五年 |
| `family` | 婚育状况、家庭成员、与家人关系概况 | 独生子，父母在老家，未婚 |
| `relationship_history` | 过往恋情/上一段怎么结束/单身多久 | 去年和异地恋三年的前任分手，之后一直单身 |
| `social_pattern` | 独处/聚会倾向、线上线下社交习惯 | 周末宅家，社交主要靠线上游戏开黑 |
| `future_plans` | 近期目标/人生方向 | 想两年内跳去外企，攒钱在老家买房 |
| `finance_status` | 收入水平/消费习惯/经济压力，仅当用户明确提到 | 月薪两万出头，房贷压力大 |

### Touch points

1. **`COMPANION_INSIGHTS_SCHEMA`** (`prompt.rs`): six field descriptions added.
   填写规范 unchanged (only listed fields, no invention, nested objects
   returned whole).
2. **`companion_insights` JSONB**: free — shallow merge just starts carrying
   the new keys. No migration, no backfill. The daily snapshot table copies
   whole blobs and follows automatically.
3. **Migration `0033_human_insights_profile_expansion.sql`:**

   ```sql
   ALTER TABLE engine.human_insights
       ADD COLUMN education            TEXT,
       ADD COLUMN family               TEXT,
       ADD COLUMN relationship_history TEXT,
       ADD COLUMN social_pattern       TEXT,
       ADD COLUMN future_plans         TEXT,
       ADD COLUMN finance_status       TEXT;
   ```

   `ProjectedColumns`, `project_columns`, `HumanInsightsRow`, and the UPSERT
   in `project_from_insights` (16 → 22 binds) extend accordingly. No backfill
   needed: existing users' JSONB lacks the keys, and the mirror is
   full-overwrite on every merge, so columns populate on each user's next
   extracting turn.
4. **Chat-prompt rendering** (`human_insights_to_bullets` in `handlers.rs`,
   plus the test-only parity renderer `insights_to_bullets`). Labels and
   placement in the bullet order, with intimacy tiers:

   | 档 | 渲染顺序 |
   |---|---|
   | Neutral + Full | 城市 → 所在地 → 老家 → 国籍 → 职业 → **教育** → MBTI |
   | Full only | 感情观 → **感情经历** → 兴趣 → 情感需求 → **家庭** → **经济状况** |
   | Neutral + Full | 作息 → **社交模式** → 性格特质 → **未来计划** |

   New labels: 教育 / 感情经历 / 家庭 / 经济状况 / 社交模式 / 未来计划.
   Rationale: `education`/`social_pattern`/`future_plans` are behavioral or
   background facts on par with 职业/作息; `relationship_history`/`family`/
   `finance_status` are intimate and join the Full-only cluster. The
   Full-mode byte-parity contract between the two renderers is preserved
   (both get the same additions). Neutral output for existing users is
   unchanged until the new fields populate.
5. **`WEIGHTS` rebalance** (`store/src/insight.rs`) — 15 weighted fields
   summing to 1.0. The geo trio (location / hometown / nationality) stays
   unweighted, as historically decided:

   | field | old | new | | field | old | new |
   |---|---|---|---|---|---|---|
   | city | .05 | **.04** | | matching_preferences | .10 | **.08** |
   | occupation | .05 | **.04** | | education | — | **.04** |
   | interests | .10 | **.08** | | family | — | **.04** |
   | mbti_guess | .15 | **.10** | | relationship_history | — | **.06** |
   | love_values | .15 | **.12** | | social_pattern | — | **.04** |
   | emotional_needs | .15 | **.12** | | future_plans | — | **.04** |
   | life_rhythm | .10 | **.06** | | finance_status | — | **.02** |
   | personality_traits | .15 | **.12** | | | | |

   **Accepted consequence:** a user who had filled the entire old schema drops
   from `training_level` 1.0 to 0.76 on the next recompute and climbs back as
   new slots fill. Downstream display sees a one-time regression; approved in
   review. `finance_status` is weighted lowest because it is the least often
   disclosed — it must not gate progress.

## A4. OSS example config refresh (`examples/model_config.toml`)

- **`insight_extraction.filter_prompt`** → a product-neutral **facts+details
  reference prompt**: dual-track output `{"facts": […], "details": […]}` with
  `facts[i] == details[i].content` as a *prompt-level* contract; the six
  dimensions with reference vocabularies (category ×13, domain ×18,
  evidence_type ×4, temporality ×5, persistence ×5, confidence ×3); atomicity
  rules; roleplay boundary rules (RP signals become bounded
  `roleplay_expression` facts — fictional identities/events never become real
  facts); anti-attribution 铁律; faithful recording of sensitive/NSFW content
  without dilution; ≤12 facts; JSON only, `{"facts":[],"details":[]}` when
  empty.
- **`memory_extraction.filter_prompt`** → thickened: content discipline
  (specific, close to the user's words), category vocabulary **unchanged**
  (the 5 recall-grouping values), plus the five shared metadata dimensions
  per item: `{"memories": [{content, category, domain, evidence_type,
  temporality, persistence, confidence}]}`.
- **Budgets:** `insight_extraction.max_tokens` 400 → **1200** (one task block
  serves both stages; Stage 1 now emits details, Stage 2 now fills up to 18
  fields), `memory_extraction.max_tokens` 800 → **1200**. Temperatures stay
  0.2.

The full production prompts (including the reviewed 双轨 insight prompt) ship
downstream in PR B; the OSS reference versions carry the same structure so the
default deployment is self-documenting.

## Testing

- **A1:** details parsed and persisted inside the object payload; absent /
  non-array `details` ⇒ `[]`; parse_error still writes `NULL` payload;
  existing two-events-per-run and empty-facts tests updated to the new payload
  shape; Stage-2 input unaffected by details presence.
- **A2:** migration adds the column (insert + read-back with metadata);
  `MemoryCandidate` flatten captures extra keys and tolerates their absence;
  sweeper writes metadata / writes NULL when the map is empty; raw-turn and
  relationship writers keep writing NULL; recall functions unaffected.
- **A3:** `project_columns` maps the six new keys (and leaves them
  `None`/absent-safe); UPSERT round-trips 22 columns; bullet renderers emit
  new labels in the specified order and tiers, Neutral excludes the three
  intimate additions, Full-mode parity test extended; `WEIGHTS` sums to 1.0
  (add a unit test asserting the sum); `compute_training_level` expectations
  updated (e.g. `training_level_partial` 0.15 → 0.12, full-schema cap test
  now requires all 15 fields).
- **A4:** boot-time config gates already cover non-blank prompts; no new gates.

Standard pre-PR gate: `cargo fmt` / `clippy` / full test suite / openapi
regen check.

## Rollout & sequencing

1. **PR A** (this spec) merges to `dev`, rides the normal release flow; the
   engine image containing it reaches GHCR with the next tag.
2. **PR B** (private): bump the downstream deployment to that image **and**
   ship the production facts+details insight prompt, thickened memory prompt,
   and real max_tokens in `infra/engine`. Boot applies migrations 0032/0033.

Order tolerance: new prompt on an old engine ⇒ details generated then dropped
(wasted tokens, no breakage); new engine on old prompts ⇒ empty
details/NULL metadata (no breakage). Full value requires both, but no
lockstep deploy is needed.
