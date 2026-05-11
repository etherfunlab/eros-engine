# eros-engine-core

[![Crates.io](https://img.shields.io/crates/v/eros-engine-core.svg)](https://crates.io/crates/eros-engine-core)
[![Docs.rs](https://docs.rs/eros-engine-core/badge.svg)](https://docs.rs/eros-engine-core)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)

Pure-domain types and rules for the [`eros-engine`](https://github.com/etherfunlab/eros-engine) AI companion engine. No I/O, no async, no database — just data structures and the logic that operates on them.

## What's in here

- `persona` — persona definitions and instance state.
- `affinity` — the six-dimensional relationship vector with EMA smoothing and decay rules.
- `pde` — the Persona Decision Engine: rules that decide *how* a persona responds before the LLM call.
- `ghost` — proactive (unsolicited) message scheduling logic.
- `types` — shared IDs, timestamps, and enums.

## Use it

```toml
[dependencies]
eros-engine-core = "0.1"
```

This crate is the foundation for the rest of the workspace:

- [`eros-engine-store`](https://crates.io/crates/eros-engine-store) — Postgres + pgvector persistence
- [`eros-engine-llm`](https://crates.io/crates/eros-engine-llm) — OpenRouter / Voyage HTTP clients

## License

AGPL-3.0-only. See [LICENSE](https://github.com/etherfunlab/eros-engine/blob/main/LICENSE).
