# eros-engine-store

[![Crates.io](https://img.shields.io/crates/v/eros-engine-store.svg)](https://crates.io/crates/eros-engine-store)
[![Docs.rs](https://docs.rs/eros-engine-store/badge.svg)](https://docs.rs/eros-engine-store)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)

Postgres + pgvector persistence for the [`eros-engine`](https://github.com/etherfunlab/eros-engine) AI companion engine. Uses [`sqlx`](https://crates.io/crates/sqlx) with `runtime-tokio` + `tls-rustls`.

## What's in here

- `chat` — message history per session.
- `memory` — two-layer long-term memory (profile + relationship) with pgvector retrieval.
- `affinity` — persisted six-dimensional relationship state per session.
- `insight` — structured JSONB user profile.
- `persona` — persona instances per user.
- `pool` — `PgPool` construction helpers.

SQL migrations ship inside this crate under [`migrations/`](https://github.com/etherfunlab/eros-engine/tree/main/crates/eros-engine-store/migrations) and can be applied with [`sqlx migrate run --source <path>`](https://docs.rs/sqlx).

## Use it

```toml
[dependencies]
eros-engine-store = "0.1"
eros-engine-core  = "0.1"
```

## License

AGPL-3.0-only. See [LICENSE](https://github.com/etherfunlab/eros-engine/blob/main/LICENSE).
