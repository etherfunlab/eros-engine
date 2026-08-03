# eros-engine — boot gate for tier blocks under non-tiering tasks

Refuses to boot when a `[tasks.<name>.tiers.<tier>]` block exists for any task
outside the two that actually resolve with a tier (`chat_companion`,
`chat_output_filter`). Such a block parses, boots, and can never be selected —
the same dead-config failure class #210 removed at the task level, one level
deeper in the config tree.

Closes #215. Found during #214's review and deliberately deferred: patching it
for `affinity_evaluation` alone would have left the other twelve non-tiering
tasks silent, so this change covers them all at once.

---

## 0. Decisions (settled in the issue)

- **The gate covers the whole tier block, not just `filter_prompt`.** Every
  `TierConfig` field — `model`, `fallback`, `allow_traits`, `output_filter`,
  `filter_prompt`, `trigger`, `timing`, `retry_depth` — is equally unreachable
  under a task that never resolves with a tier. Gating one key would leave the
  same silence for the other seven.
- **The allowlist is data, not a match arm.** `TIER_CONSUMING_TASKS` sits
  beside `KNOWN_CHAT_TASKS` in `model_config.rs`, with a doc comment naming the
  two resolvers that consume a tier, so adding tier support to a future task is
  a one-line change in an obvious place.
- **Refuse, don't warn.** Consistent with `validate_prompt_variants` and
  `validate_affinity_prompt_unset`: dead config must never silently no-op.
- **Breaking for any deployment carrying such a block** — but the block was
  never doing anything, so deleting it is the whole migration.

## 1. Who consumes a tier

Exactly two resolvers take a tier and reach a `TierConfig`:

| Resolver | Task | Call site |
|---|---|---|
| `ModelConfig::resolve(task, tier)` (`model_config.rs:1479`) | `chat_companion` | `pipeline/handlers.rs:771` |
| `ModelConfig::resolve_output_filter(tier)` (`model_config.rs:1604`) | `chat_output_filter` | `pipeline/stream.rs:3844` |

Every other task resolves tier-free: `affinity_evaluation` and
`insight_extraction` pass `None` explicitly (`post_process.rs:680`,
`post_process.rs:987`), and the dedicated resolvers (`resolve_pde`,
`resolve_vision`, `resolve_product_qa`, `resolve_input_filter`,
`resolve_extract`, `resolve_voice`, `resolve_image_prompt_compose`,
`resolve_world_*`, `resolve_embedding`) take no tier argument at all.

`resolve(COMPOSE_TASK, None)` in `stream.rs:1770` is the composer — it names a
task but hands the resolver `None`, so a tier block under it is unreachable
too. `validate_prompt_variants` already says exactly this about the composer's
own tiers; this gate finishes the sentence.

## 2. The gate

```rust
/// Tasks whose config the engine ever resolves with a tier. Everything else
/// resolves tier-free, so a `[tasks.<other>.tiers.*]` block is dead config.
///
/// Consumed by `resolve(task, tier)` (chat_companion, `handlers.rs`) and
/// `resolve_output_filter(tier)` (chat_output_filter, `stream.rs`). If a
/// future task starts resolving with a tier, add it here.
pub const TIER_CONSUMING_TASKS: &[&str] = &["chat_companion", "chat_output_filter"];

pub fn validate_tier_blocks(&self) -> Result<(), String>
```

Walks `self.tasks` in sorted name order (and each task's tiers in sorted order)
so the reported failure is deterministic across restarts — `self.tasks` is a
`HashMap`, matching `validate_prompt_variants`'s existing rule. The first
offending `(task, tier)` pair is reported.

Error text names the task and the tier, states that the task never resolves
with a tier, and gives the two ways out (move the settings to the task level,
or delete the block):

```
[tasks.insight_extraction.tiers.premium] is a tier block under a task that
never resolves with a tier — only [tasks.chat_companion] and
[tasks.chat_output_filter] read tier blocks, so nothing in it could ever be
selected. eros-engine refuses to boot rather than let it silently no-op. Move
the settings to [tasks.insight_extraction], or delete the block. Rationale:
https://github.com/etherfunlab/eros-engine/issues/215
```

## 3. Boot wiring (`main.rs`)

Runs **after** `validate_providers`, not with the other config-shape gates.
`validate_providers` already refuses `[tasks.embedding.tiers.*]` with a
task-specific message ("`tiers` are not supported on the embedding task"), and
`main`'s established ordering rule is that the more specific message is the one
the operator sees — the same reason `validate_affinity_prompt_unset` runs
before `validate_prompt_variants` (`main.rs:272`). Placing the generic gate
first would shadow the embedding message and make it unreachable from the boot
path.

## 4. Verification

- Unit tests mirroring the `validate_affinity_prompt_unset` set: tier block
  under `chat_companion` boots; under `chat_output_filter` boots; under any
  other task refuses (naming task and tier); a tier block carrying only
  non-`filter_prompt` fields refuses too; no tier blocks anywhere boots.
- Determinism: two dead tier blocks report the sorted-first pair.
- Shipped-example pin in `main.rs`, matching every other gate.
- Boot-order regression pinning that `[tasks.embedding.tiers.*]` keeps the
  embedding-specific message.

## 5. Docs

`docs/model-config.md` / `.zh.md` document `[tasks.<name>.tiers.<tier>]` as a
generic per-task facility, which is now wrong. Both get:

- the schema skeleton and the field table row qualified to the two
  tier-consuming tasks, with the boot refusal stated;
- the resolution-order section's "matched tier block" bullet scoped the same
  way.

`examples/model_config.toml` ships no active tier block (both
`[tasks.chat_companion.tiers.*]` and `[tasks.chat_output_filter.tiers.gold]`
are commented out), so it passes unchanged; the tier comment block gains the
same one-line scope note.
