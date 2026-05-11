# eros-engine-llm

[![Crates.io](https://img.shields.io/crates/v/eros-engine-llm.svg)](https://crates.io/crates/eros-engine-llm)
[![Docs.rs](https://docs.rs/eros-engine-llm/badge.svg)](https://docs.rs/eros-engine-llm)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)

External LLM + embedding HTTP clients for the [`eros-engine`](https://github.com/etherfunlab/eros-engine) AI companion engine.

## What's in here

- `openrouter` — chat-completion client for [OpenRouter](https://openrouter.ai).
- `voyage` — embedding client for [Voyage AI](https://voyageai.com) (`voyage-3-lite`, 512-d).
- `model_config` — TOML schema mapping logical tasks (chat, summarize, classify, ...) to concrete model slugs. The on-disk schema is frozen for the OSS 0.x line.
- `error` — `LlmError` for HTTP and parse failures.

`reqwest` is configured with `rustls-tls` so this works on `scratch` Docker images and Fly.io.

## Use it

```toml
[dependencies]
eros-engine-llm  = "0.1"
eros-engine-core = "0.1"
```

## License

AGPL-3.0-only. See [LICENSE](https://github.com/etherfunlab/eros-engine/blob/main/LICENSE).
