# eros-engine — `chat_image_prompt_compose` filter_prompt variants

Lets a deployment configure **several** image-composer prompts under one
`[tasks.chat_image_prompt_compose].filter_prompt` and lets the downstream
consumer pick one per turn, plus a reserved `raw` escape that skips the
composer LLM entirely.

Today `filter_prompt` is a single string: every image turn in a deployment gets
the same composer instruction, and the only way to skip the composer is to
remove the whole task block (a deploy-wide, all-or-nothing switch). This design
adds a per-turn axis without touching either property for deployments that keep
the plain-string form.

---

## 0. Decisions (settled during brainstorm)

- **Only `chat_image_prompt_compose` gets variants.** `TaskConfig` is one
  shared struct, so the *type* of `filter_prompt` necessarily widens for every
  `[tasks.*]` block; "only compose" is therefore enforced at **boot
  validation**, not by the type system. Any other task written in a variant
  form refuses to boot.
- **Variant forms are rejected inside `[tasks.*.tiers.*]` blocks — including
  compose's own.** `resolve_image_prompt_compose` calls `self.resolve(TASK,
  None)` and reads `task_cfg.filter_prompt` directly; it never consults tiers.
  A tier-level variant would be silently dead config, which §1.2 exists to
  prevent.
- **Wire location is `image.prompt_variant`** (inside `ImageReplyParams`), not
  a top-level request field. The composer only runs on image turns, and the
  presence of the `image` block is already the "this turn may produce an image"
  signal — so the parameter cannot land somewhere it has no meaning.
- **Wire type is `Option<String>`.** Numeric variants are written `"0"` /
  `"1"`. A JSON number is a 400. This keeps the OpenAPI schema a plain string
  and avoids `1` vs `1.0` vs `01` normalization rules.
- **Unknown / out-of-range variants fall back silently + `warn`.** The whole
  composer path is already fail-open (model error, timeout, and empty reply all
  degrade to the seed subject); a typo in a client-supplied variant name must
  not take down a chat turn. A 400 would also mean deleting a variant from
  config instantly breaks already-shipped clients.
- **Blank values inside a variant form refuse to boot.** The plain-string
  form's leniency (`"   "` → built-in default) exists so a deployer can comment
  a prompt out. Once they have deliberately written a *list* of variants, a
  blank entry is a typo, and silently substituting the generic built-in prompt
  is the hardest class of bug to notice.
- **No selected variant ⇒ the built-in `DEFAULT_COMPOSE_PROMPT`.** One rule
  covers both variant forms and both miss cases (nothing passed / passed but
  not found). The originally-sketched "index 0" and "`default` key" special
  cases are both dropped: the engine already ships a fallback prompt, so a
  second per-form fallback mechanism earns nothing.
- **`default` is an ordinary key.** In the keyed form it matches only when the
  client literally sends `"default"`. Documented, not boot-checked — it is a
  legal key name.
- **`raw` is a reserved word, matched case-insensitively on both sides.** The
  client value `raw` skips the composer; a config key `raw` (any casing)
  refuses to boot. Case-insensitivity on the config side removes the trap where
  a deployer writes `Raw = "…"`, believes they have defined the escape hatch,
  and never sees it used.
- **The selected variant is not persisted.** `tracing` only.
  `build_delegated_image_marker` stays deliberately minimal (seed subject +
  aspect ratio), consistent with its existing doc comment and with the
  `chat_vision` audit's same stance. The value is client-supplied anyway.
- **The two delegated-image call sites are extracted into one helper first.**
  They are byte-identical for ~45 lines modulo indentation and one comment that
  exists in only one copy — evidence the manual sync has already drifted.
  Landing the variant in both by hand would double that drift surface.

---

## 1. Config layer

### 1.1 `PromptSpec`

New type in `crates/eros-engine-llm/src/model_config.rs`, placed next to
`ModelSpec` and mirroring its untagged-deserialize pattern
(`model_config.rs:49`), which already establishes "one field, three TOML
shapes" in this config file:

```rust
/// A task's `filter_prompt`. Accepts three TOML shapes, mirroring `ModelSpec`:
/// `"xxx"` (plain), `["aaa","bbb"]` (index-keyed variants), or
/// `{ a = "aaa", b = "bbb" }` (string-keyed variants). Only
/// `chat_image_prompt_compose` may use the variant shapes; see
/// `validate_prompt_variants`.
pub enum PromptSpec {
    Plain(String),
    Indexed(Vec<String>),
    Keyed(BTreeMap<String, String>),
}
```

Deserialize order `Plain → Indexed → Keyed`; TOML string / array / table are
unambiguous to serde. `BTreeMap` (not `HashMap`) so key lists in boot-failure
messages are ordered deterministically.

`TaskConfig.filter_prompt` (`model_config.rs:521`) and
`TierConfig.filter_prompt` (`model_config.rs:350`) both become
`Option<PromptSpec>`.

Two accessors:

| Method | Consumer |
| --- | --- |
| `as_plain(&self) -> Option<&str>` | The 13 non-compose read sites. Returns `Some` only for `Plain` — and after §1.2 those sites can only ever hold `Plain`. A `None` from a variant shape is indistinguishable from an absent/blank `filter_prompt`, which every one of those sites already handles (almost always: feature off), so the unreachable branch degrades safely rather than panicking. |
| `select(&self, variant: Option<&str>) -> Option<&str>` | `resolve_image_prompt_compose` only. |

### 1.2 Boot validation

New `ModelConfig::validate_prompt_variants() -> Result<(), String>`, called
from `main.rs` alongside the existing `validate_*` sequence
(`main.rs:299-322`), unconditionally — no feature flag gates it, and like its
neighbours it sits in the serve path only, so `print-openapi` / backfill
subcommands are unaffected.

It walks every `[tasks.*]` block plus every tier block within them, and refuses
to boot when:

| Condition | Rationale |
| --- | --- |
| A task other than `chat_image_prompt_compose` uses `Indexed` / `Keyed` | Nothing reads variants there |
| **Any** tier block uses `Indexed` / `Keyed` (compose included) | Compose resolves with `tier = None`; tier variants can never be selected |
| `[]` or `{}` | Empty container |
| Any `Indexed` entry blank after trim | §0 zero-tolerance for blanks in variant forms |
| Any `Keyed` value blank after trim | Same |
| Any `Keyed` key blank after trim | Same |
| Any `Keyed` key equal to `raw` (ASCII case-insensitive) | Reserved wire word |

Message format follows the existing convention (`model_config.rs:1519`):
`[tasks.{name}] … — eros-engine refuses to boot. …`

Key matching is otherwise **case-sensitive and exact**.

---

## 2. Selection semantics

The client value is trimmed first; blank after trim is treated as absent.

### 2.1 `raw` short-circuit

Checked before any shape dispatch: `variant.eq_ignore_ascii_case("raw")` ⇒ the
composer LLM does not run and the seed subject is used verbatim. This holds for
**every** shape, including `Plain` and including a compose block with no
`filter_prompt` at all (built-in prompt).

Implementation: `resolve_image_prompt_compose(variant)` returns `None` on
`raw`. The existing `None => subject.clone()` arm at the call sites
(`stream.rs:3353`, `stream.rs:3712`) absorbs it with no new branch. `raw` and
"feature off" therefore share a code path — deliberate: they produce identical
output, and the two are distinguished in `tracing`, not in the type.

Skipping the composer does **not** skip `compose_image_prompt`
(`handlers.rs:276`), the deterministic style-preset + appearance + subject
wrapper. `raw` saves exactly one LLM call, which is its stated purpose.

### 2.2 `select`

`select(&self, variant: Option<&str>) -> Option<&str>`, where `None` means "no
variant selected":

| Shape | Behavior |
| --- | --- |
| `Plain("xxx")` | Always `Some("xxx")`; the variant parameter is ignored |
| `Indexed([…])` | `variant.parse::<usize>()` succeeds and is in range ⇒ `Some(entry)`; otherwise `None` |
| `Keyed({…})` | `variant` matches a key exactly ⇒ `Some(value)`; otherwise `None` |

`None` resolves to `DEFAULT_COMPOSE_PROMPT` one level up. The existing
`unwrap_or_else` line (`model_config.rs:1567`) is untouched; only the
expression feeding it changes:

```rust
let compose_prompt = task_cfg
    .filter_prompt
    .as_ref()
    .and_then(|s| s.select(variant))
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .map(str::to_string)
    .unwrap_or_else(|| DEFAULT_COMPOSE_PROMPT.to_string());
```

The `trim` / `is_empty` pair is retained from the current code: it is what makes
a blank **plain-string** `filter_prompt` fall through to the built-in prompt,
which an existing test asserts (`model_config.rs:4274`). It is redundant for the
variant shapes — §1.2 already rejects blanks there at boot — but removing it
would be a behavior regression for the plain shape.

Misses covered by "otherwise `None`": non-numeric variant against `Indexed`
(`"a"`), out-of-range (`"5"`), unparseable (`"-1"`), and unknown key against
`Keyed`. `"01"` parses to `1` — the ordinary `usize::from_str` behavior, not
special-cased.

### 2.3 Logging

| Level | Event |
| --- | --- |
| `debug` | Variant hit (shape + resolved variant) |
| `debug` | `raw` requested — composer skipped |
| `warn` | Variant supplied but not found; falling back to the built-in prompt |

---

## 3. Wire and call-site wiring

### 3.1 DTO

`ImageReplyParams` (`companion_stream.rs:90`) gains:

```rust
/// Which `[tasks.chat_image_prompt_compose].filter_prompt` variant to use this
/// turn. `"raw"` (case-insensitive) skips the composer LLM entirely and draws
/// the seed subject as-is. Unknown values fall back to the built-in prompt.
#[serde(default)]
pub prompt_variant: Option<String>,
```

`openapi.json` is regenerated.

`resolve_image_prompt_compose(&self)` becomes
`resolve_image_prompt_compose(&self, variant: Option<&str>)`.

### 3.2 Commit 1 — extraction (no behavior change)

`stream.rs:3311-3355` (`ReplyImage`) and `stream.rs:3671-3714`
(`ReplyTextImage`) currently duplicate the computation of `subject`, `style`,
`style_str`, `aspect`, `final_subject`, and `composed_prompt`. `style` /
`style_str` are dead after `composed_prompt`, so the two arms need only three
values from the shared region. Split along the pure/IO seam:

```rust
struct ImageTurnInputs {
    seed_subject: String,
    style: StyleKey,
    aspect_ratio: Option<String>,
}

/// Pure: resolve the three per-turn image inputs from plan → request → config.
fn resolve_image_turn_inputs(
    plan: &ActionPlan,
    req_image: Option<&ImageReplyParams>,
    resolved_image_gen: Option<&ResolvedImageGen>,
) -> ImageTurnInputs

struct DelegatedImagePrompt {
    seed_subject: String,          // → build_delegated_image_marker
    aspect_ratio: Option<String>,  // → marker + frame
    composed_prompt: String,       // → frame
}

/// Runs the composer (skipped on `raw`) and wraps the result into the final
/// wire prompt.
async fn build_delegated_image_prompt(
    state: &AppState,
    persona: &CompanionPersona,
    plan: &ActionPlan,
    req_image: Option<&ImageReplyParams>,
    resolved_image_gen: Option<&ResolvedImageGen>,
    pde_transcript: &str,
) -> DelegatedImagePrompt
```

The pure/IO split is the point: `resolve_image_turn_inputs` holds three
three-level precedence chains (`plan.image_prompt → req_image.image_prompt →
""`; `req_image.style → default_style → Default`; `plan.aspect_ratio →
req_image.aspect_ratio → default_aspect_ratio`) that today have **no unit
coverage at all**, because they are buried inside `async gen` match arms and
reachable only through a full mocked pipeline. Precedent for the shape:
`compose_user_payload` (`stream.rs:2448`, tested at `stream.rs:4068`).

Both match arms shrink to a call plus their divergent tail (insert a new row +
three frames, versus merge the marker + one frame). This commit must land with
every existing assertion unchanged.

### 3.3 Commit 2 — variant threading

The variant is read exactly once, inside `build_delegated_image_prompt`, as
`req_image.and_then(|i| i.prompt_variant.as_deref())`, and passed to
`resolve_image_prompt_compose`.

`req_image == None` (the PDE chose an image turn without a client `image`
block) yields `variant = None` — the pre-existing behavior.

Both commits go in one PR.

---

## 4. Testing

**`eros-engine-llm` (`model_config.rs`)**

- `PromptSpec` deserialization for all three shapes.
- `select`: `Plain` ignores the variant; `Indexed` hit / out-of-range /
  non-numeric / `"-1"` / `"01"`→`1` / absent; `Keyed` hit / miss / absent;
  `default` as an ordinary key hits only on a literal `"default"`.
- `raw`: `resolve_image_prompt_compose(Some("raw"))` ⇒ `None`, for `"RAW"` and
  `"Raw"` too, and under `Plain` as well as an entirely absent `filter_prompt`.
- `resolve_image_prompt_compose`: each shape × variant resolves the right
  `compose_prompt`; every miss resolves `DEFAULT_COMPOSE_PROMPT`.
- `validate_prompt_variants`: variant on a non-compose task ⇒ `Err`; variant in
  any tier block ⇒ `Err`; `[]` / `{}` ⇒ `Err`; blank entry / blank value /
  blank key ⇒ `Err`; `raw` / `Raw` / `RAW` as a key ⇒ `Err`; valid config ⇒
  `Ok`.
- **Regression lock:** every existing `resolve_*` test must pass **with no
  edits**. Their TOML is all plain-string; any required edit means the
  abstraction leaked.

**`eros-engine-server` (`stream.rs`)**

- Unit tests for `resolve_image_turn_inputs` covering all three precedence
  chains (new coverage).
- `sqlx::test` + `wiremock`: a keyed config, a request carrying
  `image.prompt_variant = "b"`, asserting via `received_requests()` that the
  composer call's system message is variant `b`'s prompt.
- `sqlx::test` + `wiremock`: `prompt_variant = "raw"` makes **zero** composer
  calls, and the `image_request` frame's `composed_prompt` contains the seed
  subject verbatim.

---

## 5. Documentation

- `examples/model_config.toml`, the compose block (~`258-289`): all three
  shapes, the `raw` escape, an explicit note that `default` is **not** a
  reserved key (only a literal `"default"` selects it), and that variants are
  honored by this task alone and never inside a tier block.
- `docs/model-config.md:435` and the matching section of
  `docs/model-config.zh.md` (Simplified Chinese).
- `docs/api-reference.md` and `docs/api-reference.zh.md`: `prompt_variant` on
  `ImageReplyParams`.
- Regenerate `openapi.json`. Run fmt / clippy / test / openapi before the PR.

---

## 6. Non-goals

- The selected variant is not written to `metadata.image` (§0).
- Variants are not extended to any other task — boot actively rejects them
  (§1.2).
- No tier × variant matrix.
- The `/draw` endpoint is untouched: it receives an already-composed
  `composed_prompt` and never re-composes.
- Plain-string `filter_prompt` behavior is unchanged in every respect, so
  existing deployments are unaffected until they opt in.
