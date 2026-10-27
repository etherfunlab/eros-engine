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
- `group` — group chat sessions and their members.
- `pool` — `PgPool` construction helpers.

SQL migrations ship inside this crate under [`migrations/`](https://github.com/etherfunlab/eros-engine/tree/main/crates/eros-engine-store/migrations) (a baseline plus increments) and are applied with `eros_engine_store::migrate(&pool)`, which also folds a pre-baseline history onto the baseline; raw `sqlx migrate run` against the directory only works on a fresh database.

## Use it

```toml
[dependencies]
eros-engine-store = "2.4"
eros-engine-core  = "2.4"
```

## License

AGPL-3.0-only. See [LICENSE](https://github.com/etherfunlab/eros-engine/blob/main/LICENSE).
