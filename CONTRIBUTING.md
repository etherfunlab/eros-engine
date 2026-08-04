# Contributing to eros-engine

Thanks for your interest. A few ground rules:

## CLA required

Every contributor must accept the [Contributor License Agreement](CLA.md). The cla-assistant.io bot will prompt you on your first PR — accept it once and you're cleared for all future PRs to this repo.

The CLA grants etherfunlab the right to relicense your contribution. This is required because eros-engine is dual-licensed: AGPL-3.0 for the public, and a separate commercial license for users who can't comply with AGPL terms.

## Development

```bash
git clone https://github.com/etherfunlab/eros-engine
cd eros-engine
docker run -d --name eros-pg -p 5432:5432 \
  -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=eros_engine_test \
  pgvector/pgvector:pg16
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/eros_engine_test
cargo run -p eros-engine-server -- migrate
cargo test --workspace
```

## OpenAPI snapshot

The HTTP surface is checked in CI against a committed snapshot to catch handlers or schemas added without `#[utoipa::path]` / `ToSchema` wiring. If you add, remove, or change any `/comp/*` (or `/healthz`) route or any request/response struct, regenerate the snapshot:

```bash
cargo run -p eros-engine-server -- print-openapi > crates/eros-engine-server/openapi.json
```

Commit the updated `openapi.json` alongside the change. The CI job `openapi-snapshot` fails fast on drift.

## PR checklist

- [ ] CLA accepted via cla-assistant.io
- [ ] `cargo fmt --all`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] If the HTTP surface changed: `cargo run -p eros-engine-server -- print-openapi > crates/eros-engine-server/openapi.json`
- [ ] New behavior has tests
- [ ] DCO sign-off on every commit (`git commit -s`)
