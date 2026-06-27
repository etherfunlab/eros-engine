# eros-engine — Per-model deterministic regex output filter (Spec)

**Status**: design, pending implementation plan
**Target release**: `0.6.6-dev` track; additive TOML schema, **no store migration**
**Audience**: anyone implementing the engine-side `[tasks.chat_companion].output_regex` strip

---

## 0. Background

The streaming chat path (`crates/eros-engine-server/src/pipeline/stream.rs`,
`drive_chat_burst`) generates an assistant reply token-by-token, accumulating the
full text in `acc`, persisting one `chat_messages` row (`content = acc`), and
pushing a `ProducedMessage { full_text }` to `post_process` for the three
"extract" jobs (insight extraction, two-layer memory write, six-axis affinity).

On `reply_text_image` turns, the **L3.3-Euryale-70B** chat model (and similar
small roleplay models) appends a self-narrated artifact to the end of its reply,
e.g. `[你给对方发送了一张照片：…]`. This is harmful on two axes:

- **(a) It misleads the user.** The bracketed text is the *chat model's*
  invention, not what the PDE actually decided to draw / send. The user sees a
  description of a photo that does not correspond to the generated image.
- **(b) It pollutes context.** The verbatim reply is fed back into the next
  prompt through two channels (the 20-turn `recent_conversation` window and the
  persistent `companion_memories` write), so the boilerplate becomes recallable
  and self-reinforcing. This is the same long-term channel pollution described in
  **issue #113** (verbatim assistant prose persisted as uncategorized
  `companion_memories`, re-recalled into `[shared_memories]`).

This spec addresses the **targeted artifact**: a deterministic, per-model regex
strip of the assistant's raw output. It is **not** the #113 fix — #113's root
cause (verbatim-memory persistence, recall dedup, full-sentence anti-repetition)
remains a separate, larger spec. The regex strip is a low-cost mitigation: by
stripping before the extract split (§2.4), the bracket artifact never reaches the
memory/history channels at all.

### Relationship to the existing LLM `output_filter`

A separate, already-shipped feature
(`docs/superpowers/specs/2026-05-25-chat-output-filter-design.md`) runs a
**second LLM** to rewrite the reply, storing the original in `pre_filter_content`
and routing the turn through a buffered (no live `Delta`) emit path. This spec is
**distinct**: deterministic (no LLM call), always-on when a rule matches the
producing model (no random/trigger sampling), and purpose-fixed (remove a known
model artifact). It **reuses** the LLM filter's plumbing — the buffered emit
path, the `pre_filter_content` / `filter_model` / `filter_triggers` audit columns
(`FilterAudit`), and the final-frame `filtered` flag — and **composes** with it
when both are configured (§2.5).

---

## 1. Goal / Non-goals

**Goal:** a TOML-driven, per-model **deterministic** regex strip for
`chat_companion` replies with:
- an `output_regex` array of `{ models, pattern, replacement? }` rules on
  `[tasks.chat_companion]`,
- patterns compiled & validated at config load (fail-fast),
- the strip applied as **layer 0** — before the client emit, before the optional
  LLM `output_filter`, and before the extract split — so the artifact reaches
  neither the client, the persisted `content`, nor the memory/insight/affinity
  extract input,
- the raw original retained on the assistant row's `pre_filter_content`,
- audit via the existing `filter_model` / `filter_triggers` columns.

**Non-goals / explicit boundaries:**
- **Not enabled by default.** Absent/empty `output_regex` ⇒ byte-identical to
  today's stream (live deltas, multi-bubble fallback chaining).
- **No store migration.** Reuses `pre_filter_content` / `filter_model` /
  `filter_triggers`.
- **No LLM call.** Pure in-process `regex` replacement.
- **No per-tier override.** A model emits the same artifact regardless of tier;
  rules are resolved at task level only (§2.1).
- Scope is the chat reply path only (`Reply` + `GiftReaction`, both via
  `drive_chat_burst`). `Ghost` (no content) and `Proactive` (not on this path)
  are unaffected. Not applied to non-chat tasks (insight/affinity/memory).
- **Not the #113 fix.** Mitigates the bracket artifact only; the verbatim-memory
  root cause is out of scope.

---

## 2. Design

### 2.1 Config schema (`model_config.toml`)

On `[tasks.chat_companion]`:

```toml
[tasks.chat_companion]
# ...existing fields...
output_regex = [
  { models = ["sao10k/l3.3-euryale-70b"],
    pattern = '\s*\[你给对方发送了一张照片[：:][^\]]*\]\s*$' },
  # { models = ["x/y", "x/z"], pattern = '...', replacement = "…" },
]
```

- `output_regex` is an **array of rules**. Each rule:
  - `models: Vec<String>` — exact OpenRouter model ids the rule applies to.
  - `pattern: String` — a Rust `regex`-crate pattern.
  - `replacement: Option<String>` — substituted for each match; **default `""`**
    (delete the match).
- **Task-level only.** No `[tasks.chat_companion.tiers.<t>]` override for
  `output_regex` (deliberately omitted — see Non-goals). The field is read from
  the task block directly, not via tier resolution.
- **Exact model-id matching.** A rule applies to a producing model iff
  `model_id ∈ rule.models` (string equality), consistent with the existing
  `output_filter` `trigger.models`. Operators list every variant id they want
  covered.
- A model may be named by multiple rules; all matching rules apply, in
  declaration order (§2.4).

### 2.2 Config types (`model_config.rs`)

Follows the existing "generic field on `TaskConfig`, inert on other tasks"
pattern. On `TaskConfig` (NOT `TierConfig`):

```rust
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OutputRegexRule {
    pub models: Vec<String>,
    pub pattern: String,
    #[serde(default)]
    pub replacement: Option<String>,    // None ⇒ "" (delete)
}

// on TaskConfig:
#[serde(default)]
pub output_regex: Vec<OutputRegexRule>,  // empty when absent
```

The raw `pattern` strings are **compiled and validated at config load**
(`Config::load`): each pattern is passed to `regex::Regex::new`; a compile error
aborts load with a descriptive message (`task.chat_companion.output_regex[i]:
invalid pattern: …`). Compiled `Regex` values are cached for reuse — a resolved
structure, e.g.:

```rust
pub struct CompiledRegexRule {
    pub models: Vec<String>,
    pub regex: regex::Regex,
    pub replacement: String,   // "" when rule.replacement is None
}
```

built once and held on the loaded config (alongside the existing resolved
state), exposed via an accessor like
`Config::output_regex_rules() -> &[CompiledRegexRule]`. This adds the `regex`
crate as a dependency of `eros-engine-llm`.

> **`regex`-crate caveat:** no lookaround / backreferences. Operators anchor with
> `$`, `^`, `\s*`, character classes, etc. The euryale pattern above is
> compatible.

### 2.3 Resolution & gating

`output_regex` does **not** go through tier resolution. The chat handler reads
the compiled rules once per request. A turn is subject to the strip iff at least
one rule's `models` intersects the turn's resolved model **chain** (primary +
fallbacks) — this drives the buffered-mode decision in §2.4. Absent/empty rules
⇒ no strip, live mode, unchanged behavior.

### 2.4 Runtime flow (`drive_chat_burst`) — strip is "layer 0"

`drive_chat_burst` already computes the `chain` (`[primary] + fallback_model`) at
the top, and selects **live vs. buffered** mode via
`filtered_mode = filter.trigger.turn_level_pass(...)`.

**Mode selection gains one term:**

```
buffered  iff  filtered_mode (existing LLM-filter condition)
          OR   chain ∩ (⋃ rule.models for rules in output_regex) ≠ ∅
```

- If no rule's `models` intersects the chain, no rule can ever match ⇒ stay
  **live**, byte-identical to today (live deltas, multi-bubble fallback).
- If the chain could produce a targeted model, enter **buffered** mode (whole
  reply accumulated, single bubble, no live deltas) so the artifact is never
  streamed.

Two implementation consequences of this widened condition:

- **The buffered branch must tolerate `filter = None`** (a regex-only turn, where
  the LLM `output_filter` is not configured but a regex rule targets the chain).
  Today's buffered branch unconditionally references the resolved LLM filter `f`
  (`f.trigger`, `f.timing`, `run_output_filter`); it must be refactored to run
  with no LLM filter — skip the filter call, and default the extract timing to
  `after_extract` (with `original = visible = cleaned`, so extract sees the
  cleaned text either way).
- **Targeted turns always emit a single bubble**, even when no pattern actually
  matches (multi-bubble fallback chaining collapses to one bubble, exactly as the
  LLM filter's "filtered mode" already does — see the output-filter spec §2.6).
  This is the price of buffering to hide the artifact; it affects only turns whose
  model chain intersects a rule.

**Per-attempt pipeline order** (buffered mode), extending today's sequence:

```
accumulate acc
  → byte-BPE garble repair (existing: looks_byte_garbled / repair_byte_bpe)
  → REGEX STRIP:  cleaned = apply_output_regex(rules, model_id, repaired_acc)
  → (optional) LLM output_filter runs on `cleaned`  (if filtered_mode + per-attempt models-hit)
  → emit `cleaned` (or LLM-filtered text) as the bubble's Delta
  → persist + extract  (§2.5)
```

`apply_output_regex` is a **pure function**: for each rule whose `models`
contains `model_id`, apply `rule.regex.replace_all(text, &rule.replacement)` in
declaration order, threading the result. The strip runs **after** byte-BPE repair
(so it matches the repaired form) and **before** the LLM filter and the extract
split.

Because the strip is layer 0, `cleaned` becomes the single new baseline for
**everything** downstream — client emit, persisted `content`, LLM-filter input,
and extract input. The artifact therefore reaches none of those channels,
regardless of the LLM filter's `after_extract` / `before_extract` timing.

**Edge case — empty result:** if `cleaned.trim()` is empty (the model emitted
*only* the artifact), **fail-safe to the raw `acc`**: emit/persist the raw text
(never an empty bubble), record no strip, and log at `warn`. This is a defensive
guard; `reply_text_image` text is not expected to be bracket-only.

`extract_text` (the original-vs-visible chooser) is unaffected in shape: its
`original` argument is now `cleaned` (post-strip) rather than the raw `acc`, so
`after_extract` feeds extract the cleaned text and `before_extract` feeds the
LLM-filtered text — both artifact-free.

### 2.5 Persistence & audit (reuse `pre_filter_content`)

When the regex strip changes the text (`cleaned != acc`), the assistant row is
written via the existing `FilterAudit` path:

- `content` = `cleaned` (or the LLM-filtered text if the LLM `output_filter` also
  ran on top).
- `pre_filter_content` = the **raw `acc`** (true original, artifact included).
- `filter_model` / `filter_triggers`:
  - **Regex-only strip** (no LLM filter): `filter_model = "<regex>"` (reserved
    sentinel — lets a reader distinguish "a filter ran" from "no filter", same
    role `filter_model` plays today), `filter_triggers = {"regex": [<matched rule
    indices>]}`.
  - **Regex + LLM filter both fire:** the LLM filter owns `filter_model` (its real
    model id, as today); the regex hit is folded into `filter_triggers`
    (`{"reason": …, "regex": [<indices>]}`). `pre_filter_content` is the raw
    `acc` (strip is layer 0, so raw `acc` is the true pre-everything original).
  - `f_generation_id` / `f_client_msg_id` pertain to the LLM-filter call only;
    `None`/absent on a regex-only strip.
- When the strip makes **no change** (no rule matched, or pattern matched
  nothing): no `FilterAudit` is written — row persists `content = acc`,
  `pre_filter_content` NULL — same as a non-filtered turn.

**Replay** is unchanged: it replays stored `content` (the cleaned text); the raw
remains recoverable from `pre_filter_content` for audit.

### 2.6 Final-frame `filtered`

`ProtocolFrame::Final.filtered` is set `true` when the client received non-raw
output from **either** mechanism — the regex strip *or* the LLM rewrite. The
row's `filter_model` / `filter_triggers` disambiguate which fired. This widens
the field's meaning from "LLM filter rewrote" to "client saw transformed
output"; `retries_filter` stays LLM-filter-specific (`0` on a regex-only strip).

### 2.7 Scope

Honored for `chat_companion` replies streamed through `drive_chat_burst`: both
`Reply` and `GiftReaction`. `Ghost` (no content) and `Proactive` (not on this
path) are unaffected. `output_regex` set on other task blocks parses but is inert
(consistent with the rest of the config — no `deny_unknown_fields`).

---

## 3. Testing

- **Config parse/validate:** `output_regex` array parses with and without
  `replacement`; an invalid `pattern` aborts `Config::load` with a descriptive
  error; absent ⇒ empty rules; multiple rules and multi-model rules parse.
- **`apply_output_regex` (pure):** given (rules, `model_id`, text):
  - non-targeted `model_id` ⇒ text unchanged, no matched indices;
  - single rule strips the euryale bracket suffix; result is artifact-free;
  - multiple matching rules apply in declaration order;
  - `replacement` non-empty is honored;
  - pattern matches nothing ⇒ unchanged, no audit;
  - **empty/blank result ⇒ fail-safe returns raw, flagged "no change".**
- **Mode selection:** chain with a targeted model ⇒ buffered; chain with no
  targeted model ⇒ live (byte-identical to today).
- **Stream (wiremock for chat model):**
  - targeted model emits `…text…[你给对方发送了一张照片：…]` ⇒ client receives
    **only** the cleaned deltas (bracket never streamed); row `content` = cleaned,
    `pre_filter_content` = raw, `filter_model = "<regex>"`, `filter_triggers.regex`
    = matched indices; **`produced.full_text` (extract) = cleaned** (artifact
    absent from the memory channel);
  - non-targeted model's chain ⇒ live mode, byte-identical to today;
  - regex + LLM `output_filter` both configured ⇒ LLM runs on the cleaned text;
    `filter_model` = LLM model, `filter_triggers` carries both `reason` and
    `regex`; `pre_filter_content` = raw;
  - strip empties the reply ⇒ raw emitted/persisted, no empty bubble, no audit.
- **Final frame:** `filtered = true` on a regex strip; `false` when no rule
  matched. Update existing `final_frame_*` constructor tests if needed.
- **Committed example config:** parses; with `output_regex` absent/commented,
  resolve ⇒ no rules (off by default).

---

## 4. Rollout

- `examples/model_config.toml`: ship `output_regex` **commented out** under
  `[tasks.chat_companion]` with a doc-comment covering the rule shape
  (`models` / `pattern` / `replacement`), the exact-model-id matching, the
  worked euryale `reply_text_image` example, and the `regex`-crate caveat
  (no lookaround/backrefs).
- `docs/model-config.md`: document the field, exact-model-id matching, the
  layer-0 / extract semantics (artifact removed from client + history + memory),
  the empty-result fail-safe, and the `pre_filter_content` audit
  (`filter_model = "<regex>"`).
- Additive TOML schema; default behavior unchanged (off unless configured).
- Dev-track feature: lands on a `feat/output-regex-filter` branch → PR into
  `dev` → ships in a `0.6.6-dev` cut.
