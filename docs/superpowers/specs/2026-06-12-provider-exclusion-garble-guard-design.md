# eros-engine — Global provider exclusion + byte-BPE garble guard

**Status**: design, pending implementation plan
**Target release**: `0.6.x` dev track. **No schema migration.**
**Scope**: three independent-but-related deliverables motivated by GitHub issue #84
(an OpenRouter provider returns **undecoded byte-level-BPE** completions for
`thedrummer/cydonia-24b-v4.1` — every space is `Ġ` U+0120, every newline is `Ċ`
U+010A — which the engine persists verbatim and re-feeds into history):

1. **Global `ignore_providers`** — a config-driven list of OpenRouter provider
   slugs sent as `provider.ignore` on **every** outbound OpenRouter call
   (chat / stream / extraction / vision), so a deployment can route around a
   known-bad provider without changing models.
2. **Byte-BPE garble guard** — detect a garbled completion, prefer a fallback
   model, never persist raw glyphs as a successful turn, and log at error level
   (today the failure is silent). Defense-in-depth for when a not-yet-excluded
   provider regresses.
3. **One-off repair runbook** — a documented SQL statement (run manually via
   `supabase db query --linked`) that repairs already-persisted garbled rows.
   **Not** committed as a migration — see §4.

Out of scope (deferred to a later spec): all AI-companion reply-quality work
(sampling penalties, anti-repetition, reply-action layer, memory-extraction
specificity). This spec is the *provider-quality* half only.

---

## 0. Background & evidence

### 0.1 The bug (issue #84)

Starting 2026-06-11, `cydonia-24b-v4.1` completions arrive as raw byte-level-BPE
vocabulary strings. `crates/eros-engine-llm/src/openrouter.rs` reads
`choices[].message.content` (`call_once` line 504-507) and `delta.content`
(`execute_stream` line 577) verbatim, so the mangled text lands in
`engine.chat_messages.content`. Affected rows have `truncated=false`, so the
existing truncation/fallback path never engages and nothing is logged.

### 0.2 Measured prevalence (last 200 `chat_messages`, 2026-06-04 → 06-11)

- **10 / 100** assistant turns are >5% byte-BPE glyphs; **7** are `cydonia`.
- **8 / 100** leak a ```` ``` ````/`{"status"…}` envelope; **5** are `cydonia`.
- `cydonia-24b-v4.1` is **47%** of assistant turns in the window (27/60 for the
  persona "Aria" specifically).

### 0.3 Why this matters beyond the cosmetic glitch

`crates/eros-engine-server/src/pipeline/handlers.rs:191` feeds a persisted
assistant `content` straight back into the next prompt's history
(`HISTORY_WINDOW = 20`, line 40). A garbled or JSON-leaked turn therefore
**poisons the next request**, teaching the model to continue the pattern. Cutting
the bad provider and refusing to persist garble breaks that feedback loop.

### 0.4 Repair-transform validation (10 real garbled rows)

A naive two-substitution repair (`Ġ`→space, `Ċ`→newline) leaves **no residual
byte-glyphs on 10/10** sampled rows: in practice only the whitespace tokens fail
to detokenize; CJK characters arrive as literal glyphs. The full GPT-2
`bytes_to_unicode` inverse also round-trips, **but is unsafe for blanket use** —
legitimate pinyin `ā` (U+0101) sits inside the U+0100–U+017F byte-marker range
and would be corrupted. **Decision: use the naive two-substitution repair only,
and only on rows/strings detected as garbled.**

---

## 1. Component 1 — global `ignore_providers`

### 1.1 Config surface

Add one field to the global defaults block. `crates/eros-engine-llm/src/model_config.rs`,
`DefaultConfig` (the struct behind `[defaults]`):

```rust
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DefaultConfig {
    #[serde(default)]
    pub fallback_model: Option<String>,
    #[serde(default)]
    pub fallback_temperature: Option<f64>,
    #[serde(default)]
    pub fallback_max_tokens: Option<u32>,
    #[serde(default)]
    pub ignore_providers: Vec<String>,   // NEW — OpenRouter provider slugs
}
```

TOML:

```toml
[defaults]
# OpenRouter provider slugs to exclude from routing on EVERY task (issue #84).
# Sent as provider.ignore on every call; allow_fallbacks stays true so the model
# is still served by a healthy provider. Empty/unset = no exclusion.
ignore_providers = ["some-bad-provider-slug"]
```

- **Global only.** No per-task / per-tier override (YAGNI; the user asked for a
  global switch). It is *not* a field on `TaskConfig`.
- It does **not** participate in the task→defaults→constant resolution chain that
  `temperature`/`fallback_model` use; it is read once at boot and handed to the
  client (§1.2).

### 1.2 Threading — client-held, not per-request

The list is genuinely global, so store it on the `OpenRouterClient` at
construction rather than threading it through every `ChatRequest` / `VisionRequest`
(which would touch every call site and pollute the public request structs).

- `OpenRouterClient::new(...)` gains an `ignore_providers: Vec<String>` parameter
  (server boot reads `ModelConfig.defaults.ignore_providers` and passes it).
- `call_once`, `execute_stream`, `execute_vision` inject it into the wire body.

### 1.3 Wire body

`WireRequest` (openrouter.rs ~line 122) and `build_vision_body` gain an optional
`provider` object:

```rust
#[derive(Debug, Serialize)]
struct ProviderPrefs<'a> {
    ignore: &'a [String],
    // allow_fallbacks omitted → OpenRouter default (true)
}

// in WireRequest:
#[serde(skip_serializing_if = "Option::is_none")]
provider: Option<ProviderPrefs<'a>>,
```

`provider` is `Some(ProviderPrefs { ignore })` only when the client's list is
non-empty; otherwise `None` so the key is omitted entirely (byte-identical body
to today for the default deployment).

Resulting body fragment when configured:

```json
{ "model": "...", "messages": [...], "provider": { "ignore": ["some-bad-provider-slug"] } }
```

### 1.4 Identifying which slug to exclude

Out of scope to capture the serving provider into the row (would add a column).
The operator identifies the bad slug via the OpenRouter generation API / the
`eros-audit` repo (holds the OpenRouter management key). Documented in the config
comment, not solved in code.

---

## 2. Component 2 — byte-BPE garble guard

### 2.1 New pure module `crates/eros-engine-llm/src/byte_bpe.rs`

```rust
/// True when `s` carries a high density of byte-level-BPE whitespace glyphs
/// (`Ġ` U+0120 = space, `Ċ` U+010A = newline). Keyed ONLY on these two
/// unambiguous markers — NOT the whole U+0100–017F range — because legitimate
/// pinyin (`ā` U+0101) lives in that range and must not trip the detector.
pub fn looks_byte_garbled(s: &str) -> bool {
    if s.is_empty() { return false; }
    let total = s.chars().count();
    let bad = s.chars().filter(|c| *c == '\u{0120}' || *c == '\u{010A}').count();
    bad * 100 >= total * GARBLE_PCT_THRESHOLD   // default 3%
}

/// Safe, idempotent repair: `Ġ`→space, `Ċ`→newline. No-op on clean text.
/// Deliberately does NOT attempt the full gpt2 bytes_to_unicode inverse (unsafe
/// for pinyin — see §0.4).
pub fn repair_byte_bpe(s: &str) -> String {
    s.replace('\u{0120}', " ").replace('\u{010A}', "\n")
}
```

`GARBLE_PCT_THRESHOLD = 3` (tunable const). A clean reply scores ~0; a garbled
reply scores far higher (every space/newline is a marker).

### 2.2 New error variant

`crates/eros-engine-llm/src/error.rs`. The variant **carries the raw text** so the
all-exhausted last-resort path (§2.3) can repair it without re-fetching:

```rust
#[error("openrouter: model {model} returned byte-BPE garbled output")]
Garbled { model: String, raw: String },
```

### 2.3 Sync path (`call_once`, `execute_vision`)

After reading `raw` and before returning `ChatResponse` (openrouter.rs line
504-515 / 443-455):

```rust
if looks_byte_garbled(&raw) {
    tracing::error!(model, generation_id = ?parsed.id, "openrouter: byte-BPE garbled completion; trying next candidate");
    return Err(LlmError::Garbled { model: model.to_string(), raw });
}
```

The existing `execute` candidate loop (line 343-387) already treats any `Err` as
"try the next candidate", so this drops in cleanly. **Last-resort net:** if the
whole chain is exhausted on `Garbled`, `execute` repairs the final attempt and
returns it as a (now-clean) success rather than failing the turn — so the user
never sees raw glyphs even in the all-bad case. Implementation note: `execute`'s
loop must keep the last `Garbled { raw, .. }` (alongside `last_err`); on exhaustion,
if the final error was `Garbled`, return
`ChatResponse { reply: repair_byte_bpe(&raw), generation_id: None, .. }` plus an
error-level log, instead of propagating the error. (`generation_id`/`usage` are
unavailable on the synthesized response — acceptable for an all-bad fallback.)

### 2.4 Streaming path (`stream.rs`, live mode) — **post-accumulation, reuse fallback**

`crates/eros-engine-server/src/pipeline/stream.rs` accumulates deltas into `acc`
(line 203, 226) and drives the model fallback chain off a `truncated` flag
(line 206/234/245), with an existing "replace the bad attempt in the client view"
UX (line 321-324). Hook the guard into that machinery:

1. When the stream for an attempt completes, before persisting (`insert_assistant_batch`,
   line 269), evaluate `looks_byte_garbled(&acc)`.
2. If garbled: treat the attempt as a failed attempt — drive the **same fallback
   transition** the `truncated` path uses (try the next model in `chain`), reuse
   the truncated-attempt replacement UX, and **do not** persist the garbled `acc`
   as a clean success. Log at error level.
3. If the **last** chain entry is still garbled: persist `repair_byte_bpe(acc)`
   (clean), emit a corrected `Done`/`full_text` frame carrying the repaired text,
   and log at error level. The user ends on clean text.

Tradeoff accepted (per design decision): in live mode a garbled attempt's deltas
may be briefly visible before replacement — identical to today's truncated-attempt
behavior. The rejected alternative (first-delta early-abort) was more responsive
but required rewriting the delta-emit loop; not worth the invasiveness.

The buffered/filtered branch (live=false, line ~352) accumulates `acc` the same
way and gets the same pre-emit check, which is simpler there (nothing has reached
the client yet).

---

## 3. Component 3 — repair existing garbled rows (runbook, not migration)

One-off, idempotent SQL run manually via `supabase db query --linked`. Same
transform as the guard; touches only rows that actually contain a marker.

```sql
-- Repair byte-BPE garble in already-persisted assistant turns (issue #84).
-- Idempotent: re-running is a no-op once no Ġ/Ċ remain.
UPDATE engine.chat_messages
SET content = replace(replace(content, 'Ġ', ' '), 'Ċ', E'\n')
WHERE content ~ '[ĠĊ]';

-- Same for the pre-output-filter original, when present.
UPDATE engine.chat_messages
SET pre_filter_content = replace(replace(pre_filter_content, 'Ġ', ' '), 'Ċ', E'\n')
WHERE pre_filter_content ~ '[ĠĊ]';
```

**Why a runbook, not a committed sqlx migration:** eros-engine is OSS and does
not own downstream production data. A committed `UPDATE` migration would run on
every downstream deployment's DB to repair a transient provider bug they may
never have hit. The repair targets *this* deployment's existing rows, so it is an
operational one-off the operator runs once. (If a deployment prefers an idempotent
backfill migration, the same statement can be wrapped as one — but the OSS repo
ships the runbook only.)

---

## 4. Error handling & observability

- `LlmError::Garbled(model)` — new, drives fallback; never surfaced to the client
  (always either superseded by a clean fallback or repaired).
- Every guard hit logs at **error** level with `model` + `generation_id` +
  density, replacing today's silent failure.
- No new DB column, no schema migration, no new metric. The error logs + the fact
  that only clean/repaired text is persisted are the observability surface.

---

## 5. Testing

**`byte_bpe.rs` (pure unit tests):**
- `looks_byte_garbled`: a heavily-`Ġ`/`Ċ` string → true; a clean CJK reply → false;
  a clean reply containing pinyin `ā`/`ē` → **false** (no false positive); empty
  string → false; threshold boundary.
- `repair_byte_bpe`: `Ġ`→space, `Ċ`→newline; idempotent on already-clean text;
  the issue-#84 JSON sample round-trips to valid JSON.

**openrouter.rs:**
- `[defaults].ignore_providers` deserializes; non-empty → `provider.ignore` present
  in serialized `WireRequest`; empty → `provider` key omitted (body byte-identical
  to today).
- A mocked garbled completion → `call_once` returns `Garbled`; `execute` advances
  to the next candidate; all-garbled chain → returns repaired (clean) text.

**stream.rs:**
- A garbled attempt → triggers the fallback transition and the next model is tried;
  the persisted row is the clean/repaired text, never raw glyphs; error logged.
- All-garbled chain → persists `repair_byte_bpe(acc)` and emits a corrected
  `full_text` frame.

**Pre-PR gate** (per repo convention): `fmt` / `clippy` / `test` / `openapi` —
no API surface change expected, but run `openapi` to confirm.

---

## 6. File-touch summary

| File | Change |
| --- | --- |
| `crates/eros-engine-llm/src/byte_bpe.rs` | **new** — `looks_byte_garbled` / `repair_byte_bpe` + tests |
| `crates/eros-engine-llm/src/lib.rs` | `mod byte_bpe;` (+ re-export) |
| `crates/eros-engine-llm/src/error.rs` | `LlmError::Garbled` variant |
| `crates/eros-engine-llm/src/model_config.rs` | `DefaultConfig.ignore_providers` |
| `crates/eros-engine-llm/src/openrouter.rs` | `ProviderPrefs`, `WireRequest.provider`, client field + ctor param, sync-path guard, `execute` last-resort repair, vision body |
| `crates/eros-engine-server/src/pipeline/stream.rs` | post-accumulation garble guard on both live + buffered branches |
| server boot (client construction) | pass `defaults.ignore_providers` into `OpenRouterClient::new` |
| `examples/model_config.toml` | documented `[defaults].ignore_providers` example (commented) |
| `docs/` (this spec) + config comment | runbook SQL for existing-row repair |

---

## 7. Open decisions — all resolved

- Streaming guard: **post-accumulation detection, reuse existing fallback/replace**
  (not first-delta early-abort).
- Existing-row repair: **manual runbook, not committed migration**.
- Repair transform: **naive `Ġ`/`Ċ` two-substitution only** (full gpt2 inverse
  rejected — unsafe for pinyin).
- Detector keys on **`Ġ`/`Ċ` only** at a 3% density threshold (not the whole
  byte-marker range).
- `ignore_providers` is **global only** (`[defaults]`), client-held, injected on
  every OpenRouter call; `allow_fallbacks` left at OpenRouter's default.
