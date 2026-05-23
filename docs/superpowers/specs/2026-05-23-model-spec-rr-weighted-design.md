# Design: round-robin / weighted model selection + fallback dedup + config-file split

Date: 2026-05-23

## Problem

Three related changes to `model_config`:

1. **Static primary model.** Each task/tier picks exactly one primary `model`
   string. We want to spread load (or A/B) across several models without a
   downstream proxy: pick the primary by **round-robin** or **weighted
   random**, while `fallback` stays a simple ordered chain.

2. **Wasted fallback retries.** If the selected primary id also appears in the
   `fallback` chain, trying it again after it just failed is pointless.

3. **The committed example leaks our real model choices.** `examples/model_config.toml`
   is tracked and reflects production exactly (e.g. `gemini-3.1-flash-lite`
   primary on the extraction/affinity tasks). Publishing real choices + tuned
   values invites the suspicion that our own downstream product is "watering
   down" models. The repo build is currently **red**: a regression test was
   pre-written for the sanitized example
   (`committed_example_config_parses_and_has_affinity_task` asserts
   `haiku-4.5`) while `include_str!` still reads the real (gemini) file.

## Non-goals

- No distributed/cross-replica coordination for round-robin. Per-process
  counter; resets on restart; each Fly instance round-robins independently
  (global distribution stays even). "Keep it simple."
- `fallback` semantics unchanged: still an **ordered** `FallbackSpec`
  (string or array), tried sequentially, first success wins. Only the
  *primary* gains round-robin/weighted.
- No deeper sanitization of `examples/...example` beyond the haiku/gemini swap
  (chat_companion's grok/glm choices stay as-is).
- **Where OSS users deploy is not our concern.** The public repo ships only a
  Docker image (GHCR) + the public `docker/Dockerfile`. No fly.io guidance in
  public docs/README.
- The **private fly.io deploy** (real config + `fly.toml` + a private
  Dockerfile) lives in this repo **gitignored** for now, to be moved to a
  separate repo later. (See memory: feedback-oss-scope-not-private-prod.)

---

## Feature A — `model` accepts Fixed / round-robin / weighted

### TOML surface (applies to task-level and tier-level `model`)

```toml
# Fixed — current behaviour, fully backward compatible
model = "x-ai/grok-4.20"

# Round-robin — deterministic alternation, zero variance over time
model = ["x-ai/grok-4.20", "z-ai/glm-4.7-flash"]

# Weighted random — any positive numbers, auto-normalized by sum
model = { "x-ai/grok-4.20" = 0.8, "z-ai/glm-4.7-flash" = 0.2 }
```

Semantics:

- `["a","b"]` and `{a=1,b=1}` are **distribution-equivalent** (~50/50) but use
  **different mechanisms**: array = deterministic round-robin (no variance);
  table = random draw (has variance). This is the user's "结果等价".
- Weights: any positive numbers, normalized by their sum (`{a=8,b=2}` ==
  `{a=0.8,b=0.2}`). Entries with weight ≤ 0 are filtered out at parse time.
- A single-entry array/table behaves like `Fixed`.

**TOML gotcha to document:** inline-table bare keys allow only
`A-Za-z0-9_-`, but model ids contain `/` and `.`, so weighted keys **must be
quoted**: `{ "x-ai/grok-4.20" = 0.8 }`. The array form has no such issue.

### Types (`crates/eros-engine-llm/src/model_config.rs`)

```rust
pub enum ModelSpec {
    Fixed(String),
    RoundRobin { models: Vec<String>, cursor: Arc<AtomicUsize> },
    Weighted(Vec<(String, f64)>),  // non-positive weights filtered at parse
}
```

- Custom `Deserialize`: deserialize an untagged intermediate
  `enum Raw { Fixed(String), RoundRobin(Vec<String>), Weighted(HashMap<String,f64>) }`
  (String vs array vs table are unambiguous to serde untagged), then map to
  `ModelSpec`, attaching `Arc::new(AtomicUsize::new(0))` to each round-robin.
- `TaskConfig.model: ModelSpec` (still required), `TierConfig.model: Option<ModelSpec>`.
- `Clone`/`Default` on the config structs stay valid: the cursor is an `Arc`,
  so cloning a `TaskConfig`/`TierConfig` shares the same counter.

### Selection (inside `resolve()`, signature unchanged)

`resolve()` first picks the **winning spec** by the existing precedence
(tier > task default block > `defaults.fallback_model` (as `Fixed`) >
compiled-in `FALLBACK_MODEL`), then selects a concrete id from it:

```rust
fn select(spec: &ModelSpec) -> Option<String> {
    match spec {
        Fixed(s) => Some(s.clone()),
        RoundRobin { models, cursor } if !models.is_empty() => {
            let i = cursor.fetch_add(1, Ordering::Relaxed) % models.len();
            Some(models[i].clone())
        }
        Weighted(entries) if !entries.is_empty() => Some(weighted_pick(entries)),
        _ => None,  // empty array/table, or all weights <= 0
    }
}
```

- Selection runs on **only the winning spec**, so only the counter actually
  used is advanced.
- `select() == None` (empty collection) → fall through to the next precedence
  level and `tracing::warn!` (mirrors the existing "unknown task uses
  defaults" forgiving style).
- Weighted draw is split for testability: a pure
  `pick_weighted(entries: &[(String,f64)], position: f64) -> &str` that walks
  the cumulative sum, with the RNG isolated in the caller
  (`thread_rng().gen_range(0.0..sum)`).
- `ResolvedModel.model` stays a `String`; **all 5 `resolve()` callers are
  untouched** (handlers.rs ×2, post_process.rs ×2, dreaming.rs).

### Dependency

- Add `rand` to `crates/eros-engine-llm/Cargo.toml` (not currently a workspace
  dependency).

### Overhead

Per request, `resolve()` is called once (not in a hot loop), immediately
before a multi-hundred-ms LLM call. Round-robin adds one `fetch_add` (atomic,
~ns, negligible contention); weighted adds one thread-local RNG draw (~ns).
Both are noise relative to the network call.

---

## Feature B — fallback dedup

In `resolve()`, after the primary is selected, drop it from the resolved
fallback chain:

```rust
fallback_model.retain(|m| m != &selected_primary);
```

- Removes **all** occurrences; order otherwise preserved.
- Runs per-call, so with round-robin/weighted it is **dynamic**: only the id
  actually chosen this turn is removed. Example — `model = ["a","b"]`,
  `fallback = ["a","c"]`:
  - turn selects `a` → fallback becomes `["c"]`
  - turn selects `b` → fallback stays `["a","c"]`
- Rationale: the primary just failed; retrying the identical id in the
  fallback chain wastes an attempt (and a slot under
  `MAX_STREAM_FALLBACK_DEPTH`).

---

## Feature C — config-file split + sanitize

### Public vs private build split

| | Public (GHCR) | Private (fly.io) |
|---|---|---|
| Dockerfile | `docker/Dockerfile` (committed) | `docker/Dockerfile.fly` (**gitignored**) |
| config baked in | `examples/model_config.toml.example` | `examples/model_config.toml` (**gitignored**, real) |
| deploy config | — | `fly.toml` (**gitignored**) |

Both Dockerfiles are identical except the runtime config `COPY` line.

### File moves

- Commit **`examples/model_config.toml.example`** (sanitized template).
- `git rm --cached examples/model_config.toml` and gitignore it. The real file
  stays on disk locally (content **unchanged**) but leaves the public repo.
- `git rm --cached fly.toml` and gitignore it. Keep its current contents but
  point `[build] dockerfile = "docker/Dockerfile.fly"` (was `docker/Dockerfile`).
- Create **`docker/Dockerfile.fly`** (new, gitignored): a copy of
  `docker/Dockerfile` whose runtime line copies the **real**
  `examples/model_config.toml`. Fly remote builds upload the on-disk
  (gitignored) file via the build context (.dockerignore, not .gitignore, gates
  the context — and it does not exclude `examples/`), so the real config bakes in.
- **Fix the existing `.gitignore` typo** (`example/...` → `examples/...`).
  Entries to add:
  ```
  examples/model_config.toml
  fly.toml
  docker/Dockerfile.fly
  ```

### Sanitization (literal position-swap)

In `examples/model_config.toml.example`, swap `anthropic/claude-haiku-4.5`
and `google/gemini-3.1-flash-lite` **positions** in the four tasks where
gemini is the real primary — `insight_extraction`, `memory_extraction`,
`affinity_evaluation`, `pde_decision`:

| | real (gitignored) | sanitized `.example` |
|---|---|---|
| primary | `google/gemini-3.1-flash-lite` | `anthropic/claude-haiku-4.5` |
| fallback | `["deepseek/deepseek-v4-flash", "anthropic/claude-haiku-4.5"]` | `["deepseek/deepseek-v4-flash", "google/gemini-3.1-flash-lite"]` |

(`pde_decision`'s fallback is commented out — swap the names inside the
comment too, for consistency.)

Also **rewrite the leaking comments** in those blocks (e.g. "gemini-3.1-flash-lite
leads here: cheaper, more NSFW-tolerant…") so the `.example` reads coherently
as a haiku-primary config and does not betray the real choice.

`chat_companion` (grok/glm) is left unchanged — out of scope.

### Reference repointing

- `include_str!` in both `committed_example_*` tests → `examples/model_config.toml.example`.
- `docker/Dockerfile` line 42 (public): `COPY examples/model_config.toml.example /etc/eros-engine/model_config.toml`
  (the env var on line 50 is unchanged). The private `docker/Dockerfile.fly`
  keeps `COPY examples/model_config.toml ...`.
- Boot/dev default (`main.rs` and the `ModelConfig::load()` helper): default to
  `examples/model_config.toml.example`. Fresh OSS clones boot out-of-box; the
  real config is opted into via `MODEL_CONFIG_PATH`. Prod (fly) gets the real
  config baked in by `docker/Dockerfile.fly` at the path `fly.toml [env]`
  already points to. **No fallback chain** — one simple default.
- README.md / README.zh.md: `MODEL_CONFIG_PATH` default note + the "routed by
  examples/model_config.toml" lines → `examples/model_config.toml.example`.

### Strip fly.io from public docs

The public repo provides only a Docker image + `docker/Dockerfile`; where users
deploy is out of scope.

- **README.md / README.zh.md**: remove the fly.io deployment description
  entirely; replace with "pull the GHCR image (or build `docker/Dockerfile`)
  and run it on any container host."
- **docs/deploying.md / docs/deploying.zh.md**: trim to Docker-only (image +
  `docker/Dockerfile` + required env vars); remove fly.io-specific steps and
  the `fly.toml` references (those now describe the gitignored private deploy).

---

## Test impact (`model_config.rs`)

Update for the `model: ModelSpec` type change:

- `parse_minimal_config`, `COMPAT_FIXTURE` (`compat_fixture_locks_full_schema`),
  and any `resolve_*` that reads `.model`/`gold.model` as `String` → compare
  against `ModelSpec::Fixed(...)`, or add a small `ModelSpec::as_fixed()` test
  helper.
- `committed_example_config_parses_and_has_affinity_task`: repoint
  `include_str!` → `.example`; keep `model == "anthropic/claude-haiku-4.5"`;
  **flip the fallback assertion** to
  `["deepseek/deepseek-v4-flash", "google/gemini-3.1-flash-lite"]` (literal
  swap). This turns the currently-red test green.
- `committed_example_chat_companion_disables_reasoning`: repoint `include_str!`
  → `.example` (chat_companion is unchanged, assertions stand).

New tests:

- Parse all three forms (`Fixed`/`RoundRobin`/`Weighted`); legacy
  `model = "..."` still parses as `Fixed`.
- Round-robin alternates `a,b,a,b` across successive `resolve()` calls.
- Task-level and tier-level round-robin counters are independent.
- `pick_weighted` boundary/normalization: `{a=8,b=2}` and `{a=0.8,b=0.2}`
  produce identical cut points; non-positive weights filtered.
- Empty array/table primary → falls through to next precedence + (no panic).
- Single-entry array/table behaves like `Fixed`.
- Fallback dedup: selected primary removed from fallback, including the
  dynamic round-robin case (different selection → different dedup).

## Docs

- `docs/model-config.md` / `docs/model-config.zh.md`: document the three
  `model` forms, the quoted-key gotcha, round-robin vs weighted semantics, the
  fallback-dedup behaviour, and that the committed file is now
  `examples/model_config.toml.example`. This is a **backward-compatible schema
  widening** (a plain string still parses), not a breaking change to the 0.x
  stability commitment.
- Update `examples/model_config.toml.example` inline comments to show a
  round-robin and a weighted example.

## File-change summary

- `crates/eros-engine-llm/src/model_config.rs` — `ModelSpec`, custom
  `Deserialize`, `select()`/`pick_weighted()`, dedup in `resolve()`, test updates.
- `crates/eros-engine-llm/Cargo.toml` — add `rand`.
- `crates/eros-engine-server/src/main.rs` — default path → `.example`.
- `examples/model_config.toml.example` — new committed sanitized file
  (haiku/gemini swap + comment rewrites + RR/weighted examples).
- `.gitignore` — fix typo; ignore `examples/model_config.toml`, `fly.toml`,
  `docker/Dockerfile.fly`.
- `git rm --cached examples/model_config.toml fly.toml`.
- `docker/Dockerfile` (public) — COPY `.example`.
- `docker/Dockerfile.fly` (new, gitignored) — copy of public Dockerfile, COPY
  real `examples/model_config.toml`.
- `fly.toml` (gitignored) — `[build] dockerfile = "docker/Dockerfile.fly"`.
- `README.md`, `README.zh.md` — remove fly.io; `.example` reference updates.
- `docs/model-config.md`, `docs/model-config.zh.md` — three `model` forms,
  dedup, `.example` rename.
- `docs/deploying.md`, `docs/deploying.zh.md` — trim to Docker-only.
