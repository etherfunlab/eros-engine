# Sampling parameters for every non-embedding task

**Date:** 2026-08-12
**Issue:** [#246](https://github.com/etherfunlab/eros-engine/issues/246)
**Status:** design approved, ready for planning

## Problem

`top_p` / `frequency_penalty` / `presence_penalty` are generic `TaskConfig`
fields. `ModelConfig::resolve()` computes all three for any task. Only
`ResolvedModel` carries them onward — the twelve other non-embedding
`Resolved*` structs carry `temperature` and `max_tokens` and nothing else. So

```toml
[tasks.chat_image_prompt_compose]
frequency_penalty = 0.3
```

parses, boots, and silently does nothing: the call site builds its
`ChatRequest` with `..Default::default()`, the three keys go out as `None`,
and they never reach the wire. Same silent-no-op class as #215 (dead tier
blocks) and #225 (vision bypassing body rules).

`repetition_penalty` does not exist anywhere in the config or in
`ChatRequest`, even though it is the parameter most directly aimed at the
failure that motivated this: a JSON-emitting task looping on its own
boilerplate until it hits the `max_tokens` ceiling. Temperature is the wrong
knob for that — degenerate repetition is characteristic of near-greedy
decoding, so the obvious remedy pushes the wrong way.

## Non-goals

- **Tier-level override.** The four knobs stay task-level only; tiers inherit
  and cannot override, matching the existing three fields' documented
  semantics and `temperature`/`max_tokens`.
- **`[defaults]` fallback.** Absent means absent — the wire param is omitted,
  never sent with an engine-chosen default.
- **`embedding`.** Not chat-shaped; takes none of these. Setting one there is
  a boot refusal (below), not a silent ignore.
- **Changing how `[[providers.<name>.body]]` overrides work.** That mechanism
  already does what §5 documents; this spec only writes it down and locks it.

## §1 Config surface

Four sampling fields on `TaskConfig`, valid on **every task except
`embedding`**:

| Field | Type | Legal range |
|---|---|---|
| `top_p` | `f32` | `(0.0, 1.0]` |
| `frequency_penalty` | `f32` | `[-2.0, 2.0]` |
| `presence_penalty` | `f32` | `[-2.0, 2.0]` |
| `repetition_penalty` | `f32` *(new)* | `(0.0, 2.0]` |

Semantics, unchanged from the existing three: task-level only (tiers inherit,
no per-tier override), no `[defaults]` fallback, `None` ⇒ the wire param is
omitted entirely.

All four are standard OpenAI-format chat/completions parameters accepted by
every OpenAI-compatible endpoint (OpenRouter, Venice, and the rest). None of
them is a vendor extension, so none is subject to the strict-OpenAI-subset
strip applied to custom `[providers]` endpoints.

## §2 Data model

```rust
// crates/eros-engine-llm/src/model_config.rs
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sampling {
    pub top_p: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub presence_penalty: Option<f32>,
    pub repetition_penalty: Option<f32>,
}
```

`Copy + Default`: call sites write `sampling: c.sampling` with no clone, and
`ChatRequest`'s `..Default::default()` keeps working untouched.

| Struct | Change |
|---|---|
| `ResolvedModel` | three loose fields → `pub sampling: Sampling` |
| `ResolvedOutputFilter`, `ResolvedInputFilter`, `ResolvedVision`, `ResolvedVoice`, `ResolvedImagePromptCompose`, `ResolvedPde`, `ResolvedProductQa`, `ResolvedExtract`, `ResolvedWorldDirector`, `ResolvedWorldComment`, `ResolvedWorldReply`, `ResolvedWorldStories` | each gains `pub sampling: Sampling` |
| `ResolvedEmbedding` | unchanged — no sampling |
| `ChatRequest` | three loose fields → `pub sampling: Sampling` |
| `VisionRequest` | gains `pub sampling: Sampling` |
| `WireRequest` | **stays flat** — four sibling fields, each `skip_serializing_if = "Option::is_none"`, populated from `sampling` at build time |

The wire shape is not nested, so `WireRequest` must not be. One struct owns
the grouping (`Sampling`); the serializer owns the flattening. This is the
whole point of the refactor: adding a fifth knob later touches `Sampling`,
`TaskConfig`, `WireRequest`, and nothing else — not thirteen structs and
sixteen call sites.

## §3 Plumbing

Every `resolve_*()` already calls `self.resolve(TASK, None)` internally and
holds the resolved `m`. Each gains one line: `sampling: m.sampling`.

Each of the sixteen production `ChatRequest` construction sites gains one
line: `sampling: c.sampling`.

| Site | Task |
|---|---|
| `pipeline/handlers.rs:280` | `chat_companion` (already carries three; switches to `sampling`) |
| `pipeline/stream.rs` | `chat_output_filter`, `pde_decision`, `chat_input_filter`, `chat_image_prompt_compose`, `chat_product_qa` |
| `pipeline/post_process.rs` | `affinity_evaluation`, `insight_extraction` (×2) |
| `pipeline/voice.rs` | `chat_voice` |
| `pipeline/story.rs` | `world_stories_director` |
| `pipeline/dreaming.rs` | `memory_extraction` |
| `pipeline/world.rs` | `world_director` |
| `pipeline/world_town.rs` | `world_comment`, `world_reply` |
| `routes/persona.rs` | `chat_image_prompt_compose` |

`build_vision_body()` inserts the four keys conditionally — `None` inserts
nothing, so a deployment that sets none produces a byte-identical body to
today.

`WireRequest::for_endpoint()` is **not** touched: all four are standard
OpenAI-format fields, not OpenRouter extensions. The
`custom_endpoint_wire_is_strict_openai_subset` lock test gains
`repetition_penalty` in its allowed-field assertion; the strip logic itself
does not change.

`prompt_log.rs` currently snapshots and prints `top_p` alone. It extends to
all four, printing each only when `Some`.

## §4 Validation — boot refusal

A new `validate_sampling()` runs over every `[tasks.<name>]` block at load,
with error text matching the existing `validate_*` style:

```
[tasks].pde_decision.top_p: must be in (0.0, 1.0], got 1.5
```

Rules:

1. Each field must fall in its §1 range.
2. Non-finite values (`NaN`, `inf`) are refused regardless of range.
3. Any of the four present on `[tasks.embedding]` refuses to boot, with text
   naming embedding as not accepting chat-shaped sampling parameters.

Rule 3 exists because leaving `embedding` as the one remaining silent black
hole would reproduce exactly the bug this spec closes.

## §5 `[[providers.<name>.body]]` override — already works, now documented

`apply_body_rules()` merges rule params into the **serialized** wire body via
`Map::insert`, after the engine has built it. `temperature` and `max_tokens`
are ordinary keys — only `model` / `messages` / `stream` are engine-owned and
refused at boot — so body params already override `[tasks.*]` values for
them, and will equally override the four sampling params once §3 lands. All
three call paths (`chat`, `stream`, `vision`) share the same strip-then-merge
order, so the behavior is uniform.

`openrouter.rs`'s `apply_body_rules_merges_in_order_later_wins` already
asserts a rule's `temperature: 0.9` beats the engine-built `0.5`.

No behavior change. Two additions:

- A sibling lock test asserting the same for `max_tokens`.
- Documentation (§7 item 4) naming `temperature`, `max_tokens`, and the four
  sampling params as overridable — the current prose says "merged params win
  over engine-built fields" but only ever demonstrates `reasoning`.

The escape hatch's real limits stay as issue #246 describes them and are not
addressed here: body rules are provider-scoped while `fallback` chains cross
providers, and `params` is unschema'd passthrough. Those are the reasons the
task block needs its own knobs, not defects to fix in the body mechanism.

## §6 Testing

**`model_config.rs`**

- The four fields resolve on non-chat tasks — `pde_decision`, `world_reply`,
  and `chat_vision` as representatives of the three resolver shapes.
- `repetition_penalty` resolves on `chat_companion`.
- Boot refusal, one case per field, above and below its range.
- Boot refusal on non-finite values.
- Boot refusal on `[tasks.embedding]`.
- `compat_fixture_locks_full_schema` extends to cover all four fields.

**`openrouter.rs`**

- `repetition_penalty = Some(x)` reaches the wire; `None` omits the key.
- `custom_endpoint_wire_is_strict_openai_subset` updated for the new field.
- `max_tokens` body-rule override lock (§5).
- Vision body carries the four when set, omits them when unset.

**server crates**

- At least one non-chat call site asserts `sampling` passthrough end to end;
  `handlers.rs` already covers `chat_companion`.

## §7 Documentation

**`docs/model-config.md`**

| # | Location | Change |
|---|---|---|
| 1 | `## Schema` TOML skeleton (~L44) | add the four sampling lines to the `[tasks.<name>]` skeleton |
| 2 | Field details table L93–95 | rekey `tasks.chat_companion.*` → `tasks.<name>.*`; drop "Chat task only" and "parses but has no effect"; add a `repetition_penalty` row; state each legal range and that out-of-range refuses to boot |
| 3 | Field details table L97 (`tiers.<tier>` row) | "Does not override `temperature` or `max_tokens`" → add the four sampling params |
| 4 | `[[providers.<name>.body]]` section L221–274 | name `temperature`, `max_tokens`, and the four sampling params as overridable by body params |
| 5 | Per-task field tables L363 (`chat_output_filter`), L675 (`chat_image_prompt_compose`), L826 (`chat_product_qa`) | add the four rows to each |
| 6 | `[tasks.embedding]` field table (L872+) | add: any of the four refuses to boot |
| 7 | `## Resolution rules` L975–1005 | add a precedence block for the sampling params (task default block only — no `[defaults]`, no compiled-in); extend "temperature and max_tokens are task-level only" to all six |
| 8 | `## Stability commitments` item 5 | same extension |
| 9 | `### Changelog note` | new entry: four params now apply to every non-embedding task; `repetition_penalty` added; out-of-range now refuses to boot (**behavior change**); `[tasks.embedding]` refuses them |

**`docs/model-config.zh.md`** — items 1–9 mirrored, in Simplified Chinese.

**`examples/model_config.toml`**

| # | Location | Change |
|---|---|---|
| 10 | L108–112 comment | "Sampling knobs (chat task only …)" no longer holds. The clause `we deliberately do NOT use repetition_penalty — it is provider-inconsistent and distorts CJK at the useful range` must drop the provider-inconsistency claim, which is false: `repetition_penalty` is a standard OpenAI-format parameter accepted by every OpenAI-compatible endpoint. The CJK-distortion half is an independent empirical judgement and is kept as the stated reason this deployment leaves it unset. |
| 11 | L221 tier comment | `(NOT temperature/max_tokens — those are task-level only)` → add the four sampling params |

The example config continues to set sampling values on `chat_companion`
only. No other task gets a shipped default — a deployment that changes
nothing must see byte-identical wire bodies. The comment is rewritten to say
any non-embedding task may now set them.

`README` / `.env.example` / the other `docs/*.md` mention none of these
parameters and are not touched.

## Known deviations and risks

- **Public API shape changes.** `ChatRequest`, `ResolvedModel`, and the twelve
  other `Resolved*` structs are `pub` in `eros-engine-llm`. Replacing three
  loose fields with `sampling` is breaking for any downstream crate consumer.
  The engine's own server crate is the only known consumer. Lands in
  `1.0.5-dev`.
- **Out-of-range validation is a new boot gate.** A config carrying an
  out-of-range value boots today and will refuse after this change. Such
  values are already rejected or silently clamped provider-side, so nothing
  working stops working — but it is a behavior change and belongs in the
  release note, not only in the docs changelog.
- **`[tasks.embedding]` refusal is likewise a new gate**, for a key that has
  never done anything.
- **Audit/prompt-log records the resolved value, not the wire value.** When a
  `[[providers.<name>.body]]` rule overrides a sampling param, the log shows
  the config-resolved number while the provider received the rule's. That
  divergence predates this spec (it already applies to `temperature` and
  `reasoning`) and is not addressed here.

## Files touched

```
crates/eros-engine-llm/src/model_config.rs    Sampling, TaskConfig, 13 Resolved*, validate_sampling
crates/eros-engine-llm/src/openrouter.rs      ChatRequest, VisionRequest, WireRequest, build_vision_body
crates/eros-engine-server/src/prompt_log.rs   snapshot + param line
crates/eros-engine-server/src/pipeline/{handlers,stream,post_process,voice,story,dreaming,world,world_town}.rs
crates/eros-engine-server/src/routes/persona.rs
docs/model-config.md
docs/model-config.zh.md
examples/model_config.toml
```
