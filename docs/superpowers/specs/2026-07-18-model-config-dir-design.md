# Model config directory mode (`MODEL_CONFIG_DIR`)

**Date:** 2026-07-18
**Scope:** engine (this repo). Additive, config-loading only. A new
`MODEL_CONFIG_DIR` environment variable (mutually exclusive with
`MODEL_CONFIG_PATH`) that loads a directory of `.toml` files and merges them
into one `ModelConfig` at boot. No runtime behaviour changes after load; all
existing validators run unchanged on the merged result.

## Problem

The model config is a single TOML file (`MODEL_CONFIG_PATH`, default
`examples/model_config.toml`). A real deployment's file carries `[defaults]`
plus seven-and-growing `[tasks.*]` sections (~500 lines in the example alone),
each with prompts, tier blocks, and sampling knobs. Editing one task means
scrolling one large file, and diffs/reviews mix unrelated tasks.

The fix is a directory mode: split the config by section into small files
(`defaults.toml`, `chat.toml`, `extraction.toml`, …) and have the engine merge
them at boot. Splitting is the only goal — this is **not** a layering/override
mechanism; a key defined twice is a configuration error, not a precedence
question.

## Design

### Environment variable semantics

- New env var `MODEL_CONFIG_DIR`: path to a directory whose `.toml` files are
  merged into the full model config.
- Mutually exclusive with `MODEL_CONFIG_PATH`: setting **both** is a boot
  error telling the operator to pick one. No silent precedence.
- Neither set → behaviour is unchanged (single-file default
  `examples/model_config.toml`).
- `MODEL_CONFIG_DIR` has no default; directory mode is opt-in only.

### File selection rules

- Take `*.toml` files from the directory's **top level only** (no recursion
  into subdirectories).
- Skip dotfiles (names starting with `.`) to avoid editor temp files.
- Sort by filename byte order. Because duplicates are errors, order cannot
  change the merged result — sorting only makes iteration and error messages
  deterministic.
- The directory not existing, or containing no matching `.toml` file, is a
  boot error (fail fast on misconfiguration).

### Merge rules (per-file parse + table-level merge)

- Each file is parsed standalone into a `toml::Table`; a syntax error reports
  the offending filename.
- Merge granularity:
  - `tasks` merges one level deep: each `tasks.<name>` must come from exactly
    one file. The same task name in two files is an error naming both files,
    e.g. `[tasks.chat_companion] in chat.toml already defined in base.toml`.
  - Every other top-level key (`defaults`, and any future top-level section)
    is whole-key unique: defined in at most one file, duplicate is an error
    naming both files.
- The merged table is then deserialized through the existing path into
  `ModelConfig`. Existing boot validators (`validate_extraction_prompts`,
  `validate_voice_model`, `validate_product_qa_prompt`, …) run on the merged
  config exactly as they do today — zero changes to them.

### Code placement

- `crates/eros-engine-llm/src/model_config.rs`:
  - New `ModelConfig::from_toml_dir(dir)` — list/sort/parse/merge as above.
  - A **pure function** for source resolution taking the two env values as
    `Option<String>` (not reading `std::env` itself) and returning
    single-file / directory / both-set-error — unit-testable without touching
    process env.
  - `ModelConfig::load()` (embedder convenience) gains `MODEL_CONFIG_DIR`
    support via the same resolution function, staying behaviour-identical to
    the server.
- `crates/eros-engine-server/src/main.rs`: the inline load block calls the
  same crate entry points, keeping its anyhow-context error style.

### Boot logging

- Directory mode: after a successful merge, one `tracing::info!` line listing
  the directory, the concrete filenames in load order, and the count, e.g.
  `model_config loaded from dir: /etc/eros/model.d [chat.toml, defaults.toml,
  extraction.toml] (3 files merged)` — then boot proceeds normally.
- Single-file mode (including the default path): a symmetric
  `model_config loaded: <path>` line. This log does not exist today; adding it
  makes the two modes uniform.
- Both lines are emitted inside the eros-engine-llm load functions (which hold
  the sorted file list), so the server and library embedders get them without
  duplicating the logging at call sites. The server initializes its tracing
  subscriber well before config load (`main.rs:39` vs `main.rs:275`), so the
  lines are visible at boot.

### Documentation

- `.env.example`: a `MODEL_CONFIG_DIR` comment block next to
  `MODEL_CONFIG_PATH`, stating the mutual exclusion.
- `docs/model-config.md` + `docs/model-config.zh.md`: a "Directory mode"
  subsection covering the rules above, with a short `defaults.toml` +
  `chat.toml` split example snippet.
- `README.md` / `README.zh.md` / `README.ja.md`: mention `MODEL_CONFIG_DIR`
  in the existing sentence that names `MODEL_CONFIG_PATH`; no new section.
- **No** `examples/model_config.d/` example directory — the doc snippet is
  enough, and a second example config would have to be kept in sync with
  `examples/model_config.toml` forever.

### Testing

Unit tests in `model_config.rs`, writing files into a tempdir:

- Split load succeeds (`defaults.toml` + per-task files ≡ the equivalent
  single file).
- Duplicate task across two files errors, message contains both filenames.
- Duplicate `defaults` across two files errors.
- Syntax error in one file errors, message contains that filename.
- Empty directory (and directory with no `.toml`) errors.
- Dotfiles and non-`.toml` files are ignored.
- Source-resolution pure function: all four set/unset combinations, including
  the both-set error.

No integration test and no process-env mutation in tests — the pure resolution
function covers the env logic.

## Out of scope

- Any override/layering semantics (later-file-wins, key-level deep merge).
- Recursive directory walks.
- A default value for `MODEL_CONFIG_DIR`.
- Shipping an example split directory under `examples/`.
- Config hot-reload — load-once-at-boot is unchanged.
