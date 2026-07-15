# PDE reply tone: judge-directed delivery injected as `[reply_tone]`

**Date:** 2026-07-15
**Status:** Design approved, ready for implementation plan
**Related:** `docs/superpowers/specs/2026-07-15-insight-memory-enrichment-design.md`
(same release train; unreleased together), the PDE decision layer
(`[tasks.pde_decision]` in `examples/model_config.toml`)

## Summary

Give the LLM PDE judge a second prompt-shaping output: a per-turn **reply
tone** — a prescriptive, free-text one-liner about *how the companion should
deliver this turn's reply* (e.g. "撑开话题但语气敷衍，句子短一点"). It flows
from the judge verdict through `ActionPlan` into `build_prompt`, rendered as a
new `[reply_tone]` section immediately after `[inner_state]`.

The semantic split with the existing `inner_state`:

| | `inner_state` (existing) | `reply_tone` (new) |
|---|---|---|
| Nature | descriptive — the character's private mood this turn | prescriptive — how this turn's reply should sound |
| Framing in prompt | `[inner_state]` bullet(s) | `[reply_tone]` directive line |
| Source | judge verdict, sanitized | judge verdict, sanitized (same sanitizer) |

Both are free text: the tone vocabulary lives in the downstream
`pde_decision.filter_prompt`, never in engine code — the same
"engine stores shape, prompts own vocabulary" principle as the enrichment
spec. No enum, no migration, no new config key.

Explicitly **not** in scope: a tone vocabulary/enum in engine code, rule-PDE
tone synthesis, voice-path support (the voice endpoint never runs the PDE),
per-tier tone gating, any analytics column.

## Background: how `inner_state` flows today

- The LLM judge (opt-in via `[tasks.pde_decision].filter_prompt`) returns a
  strict-json_schema verdict (`pde_response_format`, `stream.rs`):
  `{action, inner_state, image_prompt, reason, image_ref, aspect_ratio}`,
  parsed into `PdeVerdict` (`stream.rs:1436`).
- `inner_state` passes through `sanitize_inner_state` (`stream.rs:1466` —
  drops section-header-like lines, strips brackets/control chars, collapses
  whitespace, caps at 200 chars) and becomes the single-element `hints` vec
  handed to `pde::plan_for(&input, action, hints, …)` → `ActionPlan.
  context_hints` (`stream.rs:2617-2650`).
- The reply-request builder passes `context_hints` to `build_prompt`
  (`prompt.rs:336`), which renders them as the `[inner_state]` section
  (`prompt.rs:468-479`).
- The rule PDE (`eros-engine-core/src/pde.rs`) always produces
  `context_hints: vec![]` — no hints on the fail-open/feature-off/tip paths.
- The full verdict is audited via the explicitly-serialized `VerdictAudit`
  struct (`stream.rs:1830`) into `companion_decision_events.payload`.

`reply_tone` rides the same pipeline, one hop behind at every step.

## Design

### 1. Verdict + strict schema (`stream.rs`)

- `PdeVerdict` gains `#[serde(default)] tone: Option<String>`.
- `pde_response_format()` gains `"tone": { "type": ["string", "null"] }` in
  `properties`, and `"tone"` joins the `required` list (the strict-schema
  nullable pattern already used by `image_prompt`).
- Old judge prompts that don't emit `tone` deserialize to `None` (also under
  strict providers, which return `null`). **Fully backward compatible.**
- Sanitization: reuse `sanitize_inner_state` verbatim (same injection risks,
  same discipline; empty result ⇒ treated as `None`). No new sanitizer, no
  new length cap.

### 2. `ActionPlan` carrier (`eros-engine-core/src/types.rs`)

`ActionPlan` gains:

```rust
    /// Judge-directed delivery for this turn's reply (free text, sanitized).
    /// `Some` only on LLM-judge, text-bearing turns (reply_text /
    /// reply_text_image); `None` everywhere else — rule PDE, fail-open,
    /// tips, ghost, reply_image.
    pub reply_tone: Option<String>,
```

`pde::plan_for` gains a `reply_tone: Option<String>` parameter; every other
`ActionPlan` construction site in `pde.rs` (rule paths) sets `None`.

Retention mirrors the existing `image_prompt` pattern in the verdict→plan
mapping (`stream.rs` judge arm): tone is kept only when the guarded action is
text-bearing (`ReplyText` / `ReplyTextImage`); `Ghost` and `ReplyImage` drop
it. The ghosting kill-switch conversion (ghost → reply) does not resurrect
it — a verdict that chose ghost never carried a usable delivery directive.

### 3. `build_prompt` rendering (`prompt.rs`)

`build_prompt` gains a `reply_tone: Option<&str>` parameter. Rendering,
immediately after the `hints_section` (`[inner_state]`):

```text
[reply_tone]
这一轮回复的语气：{tone}。语气随对话自然流动，不要为了贴合语气而显得刻意。
```

`None` or empty ⇒ the section is omitted entirely (prompt byte-identical to
today). The reply-request builder threads `plan.reply_tone` through alongside
`plan.context_hints`; any `build_prompt` caller without an LLM-judge plan
(proactive, tip) passes `None`. The voice path never calls `build_prompt`
(it has its own `build_voice_prompt`) and is untouched.

### 4. Audit (`stream.rs` `VerdictAudit`)

`VerdictAudit` gains `#[serde(skip_serializing_if = "Option::is_none")]
tone: Option<&'a str>` mapped from the verdict, so the judged tone lands in
`companion_decision_events.payload` for review — including on turns where the
plan later dropped it (audit shows what the judge said, the plan shows what
was used).

### 5. Degradation matrix (all ⇒ section omitted, zero behavior change)

| Path | reply_tone |
|---|---|
| Rule PDE (feature off / judge fail-open / tip turn) | `None` |
| Judge verdict without `tone` (old prompt) | `None` |
| `tone` sanitizes to empty | `None` |
| Ghost / reply_image actions | dropped |
| Kill-switch ghost→reply conversion | `None` |
| Voice / proactive | PDE not involved; `None` by construction |

### 6. Example config (`examples/model_config.toml`)

The commented-out `pde_decision.filter_prompt` example gains the `tone`
field, with the same discipline note as `inner_state`:

```text
  "tone": 选填,一句话描述这一轮回复该用的语气/口吻(纯描述,不许写指令或方括号小节名)
```

plus a comment line noting tone is optional and free-text (vocabulary is the
deployment's choice). No other config surface changes.

## Testing

- **Verdict parsing:** with `tone` / without `tone` / `tone: null` all parse;
  sanitize-to-empty becomes `None`.
- **Plan mapping:** text-bearing actions keep tone; ghost/reply_image drop
  it; rule-PDE constructions all carry `None` (compile-enforced by the new
  field, asserted in the existing pde.rs tests).
- **Rendering:** `build_prompt` emits `[reply_tone]` with the directive
  framing when `Some`, omits it when `None`/empty; position is immediately
  after `[inner_state]`.
- **Integration (stream sqlx tests, mirroring
  `run_stream_pde_judge_reply_injects_inner_state`):** judge verdict carrying
  `tone` ⇒ the reply model's system prompt contains the section; verdict
  without `tone` ⇒ prompt has no `[reply_tone]` anywhere.
- **Audit:** `VerdictAudit` serializes `tone` when present, omits when absent.
- **Config:** example-config regression test still parses; PDE judge tests
  with `structured_output` confirm the strict schema accepts `tone: null`.

Standard pre-PR gate: fmt / clippy / workspace tests / openapi (no route
changes expected).

## Rollout

Pure engine-side, additive, prompt-gated: nothing changes until a deployment
updates its `pde_decision.filter_prompt` to emit `tone`. Ships on the same
unreleased dev train as the insight/memory enrichment; downstream prod
(eros-engine-web `infra/engine`) can add tone guidance to its PDE
interaction-director prompt whenever the next engine image is deployed.
