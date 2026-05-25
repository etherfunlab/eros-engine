# eros-engine — Optional chat-reply output filter layer (Spec)

**Status**: design, pending implementation plan
**Target release**: `0.4.x` dev track (`0.4.21-dev`); additive TOML schema, no store migration
**Audience**: anyone implementing the engine-side `output_filter` + `[tasks.chat_output_filter]` knobs

---

## 0. Background

The streaming chat path (`crates/eros-engine-server/src/pipeline/stream.rs`,
`drive_chat_burst`) generates an assistant reply token-by-token: it forwards
each OpenRouter chunk to the client as a `Delta` frame, accumulates the full
text in `acc`, persists one `chat_messages` row (`content = acc`), emits
`Done`, and pushes a `ProducedMessage { full_text: acc }` to `produced_out`.
After the burst, `post_process::run` consumes `produced[].full_text` for the
three "extract" jobs: insight extraction, two-layer memory write, and the
six-axis affinity eval.

Downstream operators want an **optional** layer that rewrites the assistant's
reply through a second LLM before the client sees it — purpose defined entirely
by downstream (de-AI-ify tone, style-normalize, strip/soften content, watermark,
etc.). It must be opt-in: if `model_config.toml` declares nothing, behavior is
exactly as today.

The transformation is purpose-agnostic to the engine: downstream supplies the
**filter prompt** and the **model**; the engine just runs `filter_prompt`
against the reply.

---

## 1. Goal / Non-goals

**Goal:** a TOML-driven output filter for `chat_companion` replies with:
- an `output_filter` on/off switch on `[tasks.chat_companion]` (global default +
  per-tier override),
- a `[tasks.chat_output_filter]` task holding the filter's `model` / `fallback` /
  `temperature` / `max_tokens` / `filter_prompt` / `trigger` / `timing`
  (default block + per-tier overrides),
- a combinable per-turn `trigger`,
- two extract-timing modes (`after_extract` default, `before_extract`),
- a `retry_depth` on `[tasks.chat_output_filter]` capping the filter's fallback
  chain, **default 1** (primary + first fallback only),
- five new `final`-frame status fields: `filtered` (bool), `prompt_injected`
  (array | null), `tier` (echo, string | null), `retries_chat` (u32),
  `retries_filter` (u32). **`prompt_injected` and `tier` are independent of the
  filter** — they surface the existing prompt-trait injection and the request
  tier (both oversights from earlier designs) and are bundled here for
  convenience. (See §2.8.)

**Non-goals / explicit boundaries:**
- **Not enabled by default.** Absent config ⇒ no filtering, byte-identical to
  today's stream.
- **No store migration.** Only the **filtered** text is persisted (`content`);
  the original is in-memory only (fed to after-extract, then dropped). The
  original is intentionally **not** recoverable. (See §2.6.)
- **No new failure knob.** Filter LLM error/timeout ⇒ **fail-open**: emit the
  original reply. (See §2.5.)
- Scope is the chat reply path only (`Reply` + `GiftReaction`, both via
  `drive_chat_burst`). `Ghost` has no content; `Proactive` is not on this path.
- Not applied to non-chat tasks (insight/affinity/memory/dreaming).

---

## 2. Design

### 2.1 Config schema (`model_config.toml`)

```toml
[tasks.chat_companion]
# ...existing fields...
output_filter = false                 # global on/off, default false when omitted (#7)

[tasks.chat_companion.tiers.gold]
output_filter = true                  # per-tier override; beats the task default (#3)

[tasks.chat_output_filter]            # whole table absent ⇒ filter never runs (#6)
model        = "anthropic/claude-haiku-4.5"     # fast model recommended
fallback     = ["deepseek/deepseek-v4-flash", "x/y"]  # optional, like other tasks
retry_depth  = 1                       # fallbacks to try on filter failure; default 1
temperature  = 0.3
max_tokens   = 400
filter_prompt = """
Rewrite the assistant reply below. <downstream-authored instruction>.
Output only the rewritten reply, no preamble.
"""
trigger      = { random = 0.3, models = ["x/y"], traits = { any = ["nsfw_boost"], when = "present" } }
timing       = "after_extract"        # or "before_extract"; default after_extract (#2)

[tasks.chat_output_filter.tiers.gold]
model         = "..."                 # per-tier overridable: model/fallback/retry_depth/
filter_prompt = "..."                 #   filter_prompt/trigger/timing (fall back to default
trigger       = { random = 1.0 }      #   block when omitted, #5). temperature/max_tokens are
                                      #   task-level only (no per-tier override).
```

### 2.2 Config types (`model_config.rs`)

Follows the existing "generic field on `TaskConfig`, inert on other tasks"
pattern (same as `model_name_display_override`). Reuse the existing
`model` / `fallback` / `temperature` / `max_tokens` / `tiers` plumbing; add the
filter-specific fields:

- On `TaskConfig` **and** `TierConfig`:
  - `output_filter: Option<bool>` (chat_companion uses it; inert elsewhere)
  - `filter_prompt: Option<String>`
  - `trigger: Option<OutputFilterTrigger>`
  - `timing: Option<FilterTiming>`
  - `retry_depth: Option<u32>` (chat_output_filter uses it; **default 1** when
    unset — the filter tries the primary + at most the first `fallback` entry)

```rust
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OutputFilterTrigger {        // all fields optional; AND of those present
    #[serde(default)] pub random: Option<f64>,        // probability 0.0..=1.0
    #[serde(default)] pub models: Option<Vec<String>>,// producing model ∈ list
    #[serde(default)] pub traits: Option<TraitPredicate>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TraitPredicate {
    #[serde(default)] pub any: Vec<String>,           // tags to look for
    #[serde(default)] pub when: TraitWhen,            // Present (default) | Absent
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TraitWhen { #[default] Present, Absent }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FilterTiming { #[default] AfterExtract, BeforeExtract }
```

### 2.3 Resolution & gating (`resolve_output_filter`)

A request resolves to `Option<ResolvedOutputFilter>` (the `chat` handler reuses
the existing `resolve()` machinery for model/fallback/temp/max_tokens):

```rust
pub struct ResolvedOutputFilter {
    pub model: String,
    pub fallback_model: Vec<String>,   // already truncated to retry_depth entries
    pub temperature: f64,
    pub max_tokens: u32,
    pub filter_prompt: String,
    pub trigger: OutputFilterTrigger,
    pub timing: FilterTiming,
    pub retry_depth: u32,              // tier → default → 1
}
```

`retry_depth` resolves tier → default block → **1**. The filter's effective
chain is `[model] + fallback_model[..retry_depth]` — so `retry_depth = 1`
(default) ⇒ primary + first fallback only; `0` ⇒ primary only. The resolver
truncates `fallback_model` to `retry_depth` entries so callers can pass the
chain straight through.

Decision procedure, per request + resolved `tier`:

1. **Enabled?** `output_filter` = `chat_companion` tier override → task default →
   `false` (#7, #3). If `false` ⇒ return `None` (no filtering).
2. **Table present?** If `[tasks.chat_output_filter]` is absent
   (`!tasks.contains_key("chat_output_filter")`) ⇒ return `None` (#6) — regardless
   of the `output_filter` value.
3. **Usable prompt?** Resolve `filter_prompt` (tier → default). If it resolves
   empty/blank ⇒ return `None` (a filter with no instruction is a no-op; treated
   like §2.3.2).
4. Otherwise resolve `model` / `fallback` / `temperature` / `max_tokens`
   (tier → default block → `[defaults]` → compiled-in, via the existing
   `resolve("chat_output_filter", tier)`), and `trigger` / `timing`
   (tier → default; `timing` default `after_extract`; a missing `trigger` ⇒
   empty trigger = "filter every turn"). Return `Some(ResolvedOutputFilter)`.

`output_filter = true` with no `[tasks.chat_output_filter]` (or no
`filter_prompt`) is therefore **inert**, exactly per requirement #6.

### 2.4 Trigger evaluation (`should_filter`)

Pure, unit-testable (scope.rs-style). All **specified** predicates must pass
(AND); unspecified predicates impose no constraint; an entirely empty trigger
⇒ always `true`.

```rust
impl OutputFilterTrigger {
    pub fn should_filter(&self, model_id: &str, traits: &[PromptTrait],
                         random_pass: bool) -> bool { ... }
}
```

- `random`: drawn **once per turn** (before the fallback loop) into
  `random_pass = rng.gen::<f64>() < p`; stable across all attempts of that turn.
  Absent ⇒ no random gate.
- `models`: `model_id` ∈ `models` — checked **per attempt** (the model about to
  be called is known before any token is requested).
- `traits`: `Present` ⇒ at least one of `any` is in the turn's **injected** trait
  tags (the `kept_traits` after tier `allow_traits` gating — the same set as
  `prompt_injected`, NOT the raw requested tags, so a trait the tier dropped does
  not count); `Absent` ⇒ none present. Empty `any` ⇒ predicate passes.

Split the predicates by when they're knowable:
- **Turn-level** (`random`, `traits`): constant for the whole turn, known before
  any generation. If either is *specified and fails*, **no attempt can ever be
  filtered** this turn.
- **Per-attempt** (`models`): depends on the model about to be called.

This split drives the live-vs-buffer decision in §2.5 and guarantees no original
token reaches the client on an attempt that will be filtered.

### 2.5 Runtime flow (`drive_chat_burst`)

`run_stream` resolves `Option<ResolvedOutputFilter>` once (mirroring how it
already fetches `display_override`) and threads it + the turn's **injected** trait
tags (the `kept_traits` from `build_*_request`, post `allow_traits` gating) into
`drive_chat_burst`. Draw `random_pass` once.

The burst picks **one of two modes** up front:

**Live mode** — today's behavior, byte-identical: stream `Delta`s live per
attempt, persist `content = acc` (original) per bubble, multi-bubble fallback
chaining, `produced.full_text = acc`. Entered when **any** of:
- `resolved_filter` is `None`; or
- a turn-level predicate is specified and fails (`random_pass == false`, or the
  `traits` predicate fails) ⇒ no attempt can be filtered this turn.

**Filtered mode** — entered when `resolved_filter` is `Some` and the turn-level
predicates pass (so at least one attempt *could* be filtered). Produces **one**
logical bubble; every attempt is buffered (no live `Delta`s):

1. Walk the fallback chain. For each attempt, generate the original with the
   existing streaming call **but suppress client `Delta` frames** (accumulate
   `acc` only). Truncation/usage bookkeeping is unchanged.
2. If an attempt truncates ⇒ discard it silently (no client frames, **not
   persisted** — a partial unfiltered reply must never be stored/shown) and
   continue. If the whole chain truncates ⇒ emit the existing
   `Error{UpstreamUnavailable}`.
3. On the first non-truncated `acc`, branch on the **per-attempt** `models`
   predicate (`trigger.should_filter(model_id, traits, random_pass)`, with the
   turn-level parts already known true):
   - **Filter this attempt** (`should_filter` true) ⇒ call the filter LLM via
     non-streaming `execute()` on `ResolvedOutputFilter`'s model + (depth-capped)
     `fallback_model`, messages `[system: filter_prompt, user: acc]`. `execute()`
     walks the chain itself; `ChatResponse.model` reports the model served, so
     `retries_filter` = its index in the chain (`0` = primary). Emit the filtered
     text as the bubble's `Delta` (one frame; replies are short — §0). Log usage
     as task `"chat_output_filter"`.
     - **Success** ⇒ persist `content = filtered`; `produced.full_text =`
       `after_extract ? acc (original) : filtered`; set `filtered = true`,
       `retries_filter`.
     - **Failure / timeout** ⇒ **fail-open**: emit `acc` as the bubble's `Delta`,
       persist `content = acc`, `produced.full_text = acc`; `filtered = false`,
       `retries_filter = 0`.
   - **Don't filter this attempt** (only the `models` predicate failed for this
     fallback model) ⇒ emit `acc` (original) as the bubble's `Delta`s, persist
     `content = acc`, `produced.full_text = acc`.
   - Either way persist `model = original model_id`, `usage =` the **original**
     chat-gen usage (the filter's usage is logged, not stored on the row). Emit
     `Done`, `return`.

`Meta`/`Done` framing is emitted once around the single bubble; `continues_from`
is unused in filtered mode.

In **both** modes the burst records `retries_chat` = the 0-based index of the
successful (non-truncated) chat attempt onto the `BurstOutcome` (`0` = primary).
`retries_filter` is set only on the filtered path (`0` otherwise).

### 2.6 Persistence, replay & extract (explicit consequences)

- `chat_messages.content` = what the client saw (filtered on success, original
  on fail-open). **Replay needs no change** — it already replays stored
  `content`.
- The original on a filtered success is **in-memory only**: handed to
  `post_process` via `produced.full_text` when `timing = after_extract`, then
  dropped. Not stored, not recoverable. (User-approved tradeoff: no migration.)
- `after_extract` (default): insight/memory/affinity run on the **original** raw
  text; the client/history shows filtered.
- `before_extract`: those jobs run on the **filtered** text.
- Multi-bubble fallback chaining is **collapsed to one bubble** on filtered
  turns (intermediate truncated attempts are invisible) — a deliberate deviation
  from the unfiltered multi-attempt behavior, required to avoid leaking
  unfiltered partials.

### 2.7 Scope

Honored for `chat_companion` replies streamed through `drive_chat_burst`:
both `Reply` and `GiftReaction`. `Ghost` (no content) and `Proactive` (not on
this path) are unaffected. The new fields live on the generic
`TaskConfig`/`TierConfig`; setting them on other task blocks parses but is inert
(consistent with the rest of the config — no `deny_unknown_fields`).

### 2.8 `final`-frame status fields (`filtered`, `prompt_injected`)

`ProtocolFrame::Final` gains five **always-present** fields:

```rust
Final {
    lead_score: f64,
    should_show_cta: bool,
    agent_training_level: f64,
    filtered: bool,                        // client received filtered output this turn
    prompt_injected: Option<Vec<String>>,  // injected trait tags; null when none
    tier: Option<String>,                  // echo of the request tier; null when none
    retries_chat: u32,                     // fallback retries used for the chat reply
    retries_filter: u32,                   // fallback retries used for the filter call
}
```

- **`prompt_injected`** = JSON array of the trait tags **actually injected** into
  the system prompt — i.e. `kept_traits` *after* tier `allow_traits` gating in
  `build_reply_request` / `build_gift_request`, mapped to their `tag`. `null`
  when nothing was injected. **No `skip_serializing_if`** → always present
  (array or `null`). Independent of the filter feature; reflects only trait
  injection. (Example: `"prompt_injected": ["nsfw_boost"]`.)
- **`filtered`** = `true` only when the client actually received filtered output
  (filtered mode + per-attempt `models` pass + filter LLM success). `false` for
  live mode, a `models`-miss, fail-open, ghost, and replay.
- **`tier`** = the request's `tier` echoed back (`Option<String>`, `null` when
  the request sent none). Independent of the filter — lets the frontend confirm
  the backend actually received the tier it sent (it was never echoed before;
  added here for convenience).
- **`retries_chat`** = how many fallbacks the chat reply used: `0` = primary
  succeeded, `1` = first fallback succeeded, etc. (= the successful attempt's
  0-based index in the chain).
- **`retries_filter`** = same for the filter call (`0` when primary filter model
  served, `1` when its first fallback served, …). `0` whenever no filter ran.

**Plumbing:**
- `build_reply_request` / `build_gift_request` additionally return the injected
  tags (today they return only the `ChatRequest`), so `run_stream` can carry
  them into the Final frame.
- `drive_chat_burst` reports `filtered`, `retries_chat`, `retries_filter` via the
  shared burst result (`BurstOutcome { produced, filtered, retries_chat,
  retries_filter }`). `retries_chat` = successful attempt index; `retries_filter`
  = position of `ChatResponse.model` (model actually served) in the depth-capped
  filter chain (§2.5).
- `compute_final_frame` takes `filtered` / `prompt_injected` / `tier` /
  `retries_chat` / `retries_filter`. The Reply/GiftReaction branch passes the
  burst values + the request's injected tags + `user_msg.tier`; Ghost / Proactive
  pass `false` / `None` / `user_msg.tier` / `0` / `0` (tier is still echoed).
- `replay_stream`: `filtered=false`, `prompt_injected=null`, `tier=null`,
  `retries_*=0` — none are persisted, so replay cannot reconstruct them
  (consistent with `lead_score` / `agent_training_level` already being
  recomputed-from-current on replay).

Additive change to the `final` frame contract in
`docs/superpowers/specs/2026-05-19-sse-streaming-chat-0.2-design.md` §1.5.

---

## 3. Testing

- **Parse:** `output_filter` bool on task + tier; full `[tasks.chat_output_filter]`
  incl. inline-table `trigger` (all sub-forms), `timing` enum, `filter_prompt`
  multiline; per-tier partial overrides parse.
- **Resolve/gating:** `output_filter` precedence tier>task>false (#7/#3);
  `true` + no table ⇒ `None` (#6); `true` + table but blank `filter_prompt` ⇒
  `None`; tier missing in `chat_output_filter` ⇒ default block used (#5);
  `timing` default `after_extract`; `retry_depth` default `1` and
  `fallback_model` truncated to it (e.g. `fallback=["a","b","c"]` + default ⇒
  resolved chain `[model,"a"]`).
- **`should_filter` (pure):** empty trigger ⇒ true; each predicate alone;
  AND combination; `traits` present/absent + empty `any`; `models` membership;
  `random_pass` true/false gates.
- **Stream (wiremock for chat model + filter model):**
  - filtered success ⇒ client receives filtered deltas, row persists filtered,
    original never emitted;
  - `after_extract` ⇒ `produced.full_text` = original; `before_extract` ⇒
    filtered;
  - filter LLM error ⇒ fail-open emits + persists original;
  - **live mode** when a turn-level predicate fails (`random_pass` false or
    `traits` fail) ⇒ byte-identical to today (live deltas, multi-bubble);
  - **filtered mode, per-attempt `models` miss** ⇒ single bubble emits the
    original (no filter call), persists original;
  - whole chain truncates ⇒ existing error frame; truncated intermediate
    attempt in filtered mode is not persisted/emitted.
- **Committed example config:** parses; with `output_filter` unset, resolve ⇒
  `None` (off by default).
- **Final frame fields:** `filtered` true on filtered-success; false on
  fail-open / live mode / `models`-miss / ghost. `prompt_injected` = array of
  injected tags; reflects **kept** (post-gating) tags, not requested; `null`
  when none or all dropped by tier gating. `tier` echoes the request tier
  (string | null), present on Reply/Ghost/Gift/Proactive, `null` on replay.
  `retries_chat` = successful chat-attempt index (0 = primary); `retries_filter`
  = served-filter-model index (0 when no filter / primary served). Replay Final
  ⇒ `filtered=false`, `prompt_injected=null`, `tier=null`, `retries_*=0`. `null`
  fields serialize as `null` (always present). Update existing `final_frame_*`
  constructor tests for the new fields.

---

## 4. Rollout

- `examples/model_config.toml`: ship `[tasks.chat_output_filter]` **commented
  out** (off by default) with a doc-comment covering `output_filter` (task +
  per-tier), `filter_prompt`, the combinable `trigger` (random/models/traits +
  `when`), and `timing`. Leave `chat_companion.output_filter` absent (⇒ false).
- `docs/model-config.md`: document the two knobs, the gating rules (#5/#6/#7),
  the trigger semantics, the fail-open behavior, and the persistence/replay
  consequence (only filtered text is stored; after- vs before-extract).
- Additive TOML schema; default behavior unchanged (off unless configured).
- Update `docs/superpowers/specs/2026-05-19-sse-streaming-chat-0.2-design.md`
  §1.5: the `final` frame now carries `filtered` (bool), `prompt_injected`
  (array | null), `tier` (string | null), `retries_chat` (u32), `retries_filter`
  (u32). Note `prompt_injected` + `tier` reflect pre-existing features
  (trait injection / request tier) and are unrelated to the filter.
- Dev-track feature: lands on `feat/chat-output-filter` → PR into `dev` →
  ships in a `0.4.21-dev` cut before promotion to stable `0.4.21`.
