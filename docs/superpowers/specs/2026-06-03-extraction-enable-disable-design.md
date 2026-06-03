# eros-engine — section-presence toggle for `insight_extraction` / `memory_extraction`

**Status**: design, pending implementation plan
**Target release**: `0.6.x` dev track. **No migration.**
**Scope**: let an operator turn `insight_extraction` and/or `memory_extraction` off by
**omitting** the task section, while keeping today's "present ⇒ `filter_prompt` required"
boot guarantee. No new config field, no schema relaxation. Reasoning control on extraction
tasks already exists and is only documented here.

---

## 0. Background

The OSS engine runs two LLM extraction tasks driven by `model_config.toml`:

- **`insight_extraction`** (facts stage) — inline, per chat turn, in
  `pipeline::post_process` (`post_process.rs:678`). Pulls structured user facts.
- **`memory_extraction`** — background, via the dreaming-lite `sweeper`
  (`pipeline::dreaming`, `dreaming.rs:170`). Classifies idle sessions into
  `companion_memories` rows.

Both read their system prompt from `filter_prompt` (Spec B2, PR #75) and are currently
**mandatory**: the server refuses to boot if either section is missing or its `filter_prompt`
is unset.

### What this spec changes

An operator may want to run the engine **without** one or both extraction tasks (cost,
privacy, a deployment that does its own profiling). The switch is the **presence of the task
section**:

- **Section present** → `filter_prompt` is required; boot-fail if it is blank/absent (today's
  behavior, now scoped to "when the section exists").
- **Section absent** → the corresponding extraction feature is **off**; the engine boots and
  runs without it (no error).

This is a deliberate behavior change from Spec B2, where an absent section boot-failed. The
shipped `examples/model_config.toml` keeps both sections (with prompts), so default behavior
— both extractions on — is unchanged; only an operator who deletes a section turns it off.

### The three original requirements, mapped

1. **Disable an extraction task** — delete its `[tasks.*_extraction]` section. (No `enabled`
   field; the earlier `enabled = true/false` design is dropped.)
2. **When the section is present, `filter_prompt` is required or boot fails** — the existing
   gate (`main.rs:282-293`), re-scoped to fire only when the section exists.
3. **`reasoning = { enabled = false }` on extraction tasks** — **already implemented.** No
   code change; documentation only. See §4.

### Current state (verified)

- **`TaskConfig`** (`model_config.rs:346-402`): `model: ModelSpec` is required; a generic
  `reasoning: Option<ReasoningConfig>` (line 372) is inherited by every task. **Unchanged by
  this spec** — no new field, `model` stays required (to enable a task you write a full
  section; to disable you omit it, so there is no partial "disabled" section to support).
- **`tasks`** is a public field (`pub tasks: HashMap<String, TaskConfig>`, line 409), so
  section presence is observable as `model_config.tasks.contains_key(name)`.
- **`resolve_extract(task)`** (`model_config.rs:846-862`) returns `None` when the section is
  absent **or** `filter_prompt` is blank; otherwise builds `ResolvedExtract` (already carrying
  `reasoning: m.reasoning`). **Unchanged by this spec.**
- **Boot gate** (`main.rs:282-293`): currently `bail!` whenever `resolve_*_extract().is_none()`
  — i.e. mandatory. This is what changes (§2).
- **Call sites already forward reasoning** to the wire: `post_process.rs:701`,
  `dreaming.rs:192`. Requirement 3 works end-to-end today.
- **Disabled runtime paths**:
  - `post_process.rs:678` — `let Some(resolved) = resolve_insight_extract() else { return
    (vec![], None); }`. A `None` already skips cleanly; inline per turn, no retry concept.
  - `dreaming.rs:170-175` — a `None` returns `Err(...)` which deliberately does **not** stamp
    `classified_at`, so the row retries next tick. With `memory_extraction` now omittable, the
    sweeper would otherwise retry forever — handled in §3.
- **`sweeper`** (`dreaming.rs:54-74`) already early-returns when `dreaming_tick` is zero; the
  "memory_extraction absent" early-return mirrors that guard.
- **Tests**: `shipped_model_config_satisfies_extraction_boot_gate` (`main.rs:346`) asserts the
  shipped config resolves both extractions — stays green (shipped config keeps both sections).
  `compat_fixture_locks_full_schema` (`model_config.rs:1182`) — unaffected (no schema change).

---

## 1. Schema — no change

No new field. `model` stays required. The only "config surface" change is **semantic**: a
missing `[tasks.insight_extraction]` or `[tasks.memory_extraction]` section is now valid and
means "feature off," rather than a boot error.

## 2. Boot gate (`main.rs`) — requirement 1 + 2

Replace the unconditional gate with a presence-scoped one:

```rust
// Extraction prompts are required ONLY when the task section exists. A missing
// section now means "this extraction is off" (boots fine). A present section
// with no usable filter_prompt is a misconfiguration → refuse to boot.
for name in ["insight_extraction", "memory_extraction"] {
    if model_config.tasks.contains_key(name) && resolve_extract_by_name(name).is_none() {
        anyhow::bail!(
            "[tasks.{name}] is present but its filter_prompt is unset — eros-engine refuses \
             to boot. Set a filter_prompt in {model_config_path}, or remove the \
             [tasks.{name}] section to disable {name}."
        );
    }
}
```

`resolve_extract_by_name` is the existing `resolve_insight_extract()` / `resolve_memory_extract()`
pair (inlined, or a tiny `match`). Truth table per task:

| `[tasks.X]` section | `filter_prompt` | `contains_key` | `resolve.is_none()` | Result |
|---|---|---|---|---|
| present | set | true | false | **on** |
| present | blank/absent | true | true | **boot error** |
| absent | — | false | true | **off** (gate skipped) |

Because the bail is guarded by `contains_key`, the only way to reach it is a present section
whose `resolve` is `None`, which (section present) can only be a blank/absent `filter_prompt`.

## 3. Runtime when off

- **`insight_extraction`** (`post_process.rs`): no change. Absent section ⇒
  `resolve_insight_extract()` → `None` → existing `return (vec![], None)`.
- **`memory_extraction`** (`dreaming::sweeper`): add an early-return at sweeper start, after
  the existing `interval.is_zero()` guard:

  ```rust
  if state.model_config.resolve_memory_extract().is_none() {
      tracing::info!("memory_extraction not configured — dreaming sweeper inert");
      return;
  }
  ```

  Safe and unambiguous: post-boot, `resolve_memory_extract().is_none()` ⟺ the section is
  absent (a present-but-blank section would have failed the boot gate). The sweeper never
  claims rows, so the per-row `Err`-no-stamp path (which stays a pure defensive guard) can't
  loop. Sessions keep `classified_at = NULL`; re-adding the section later makes them eligible
  again.

## 4. Reasoning (requirement 3) — documentation only

Already works: `TaskConfig.reasoning` is parsed, `resolve_extract` forwards it, and both call
sites pass it to the OpenRouter request. Three states:

```toml
# (reasoning omitted)            → param omitted, model's own default  ← default (Q1: option A)
reasoning = { enabled = true }   → force reasoning on
reasoning = { enabled = false }  → force reasoning off
```

No code change. `examples/model_config.toml` gets a commented `# reasoning = { enabled =
false }` on both extraction tasks; docs describe the three states.

## 5. Docs + examples

- **`examples/model_config.toml`**: keep both extraction sections (default = on). Add a short
  comment on each — "remove this section to disable; `filter_prompt` is required while it is
  present" — plus a commented `# reasoning = { enabled = false }`.
- **`docs/model-config.md`** + **`docs/model-config.zh.md`** (new insertions in 简体中文 per the
  zh-doc convention): document the section-presence rule (present ⇒ `filter_prompt` required,
  boot-fail otherwise; absent ⇒ feature off), call out that this changed from the previous
  mandatory behavior, and document the reasoning three-state.

## 6. Testing

Boot gate (`main.rs` tests, or a `ModelConfig`-level validation helper if extracted):
- Section present + `filter_prompt` set ⇒ gate passes.
- Section present + blank/absent `filter_prompt` ⇒ gate trips (assert via `contains_key &&
  resolve.is_none()`).
- Section **absent** ⇒ gate passes (feature off) — the key new case.
- `shipped_model_config_satisfies_extraction_boot_gate` stays green.

Resolve (`model_config.rs`): existing `resolve_*_extract()` behavior is unchanged; add/keep a
test that an absent section ⇒ `None` and a present+prompt section ⇒ `Some`.

Sweeper: a focused test that `resolve_memory_extract()` is `None` when the section is absent
(the early-return condition); a full sweeper-inert integration test only if it fits the
existing dreaming harness without a live DB.

## 7. Out of scope / non-goals

- No `enabled` field, no per-task generic enable/disable, no `model`-optional relaxation
  (all dropped from the earlier draft of this spec).
- No change to reasoning forwarding (already done).
- No migration, API surface, or OpenAPI change.
