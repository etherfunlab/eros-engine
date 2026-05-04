# Contributing to eros-engine

Thanks for your interest. A few ground rules:

## CLA required

Every contributor must accept the [Contributor License Agreement](CLA.md). The cla-assistant.io bot will prompt you on your first PR — accept it once and you're cleared for all future PRs to this repo.

The CLA grants etherfunlab the right to relicense your contribution. This is required because eros-engine is dual-licensed: AGPL-3.0 for the public, and a separate commercial license for users who can't comply with AGPL terms.

## Development

```bash
git clone https://github.com/etherfunlab/eros-engine
cd eros-engine
docker compose -f docker/docker-compose.yml up -d postgres
cargo test --workspace
```

## PR checklist

- [ ] CLA accepted via cla-assistant.io
- [ ] `cargo fmt --all`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] New behavior has tests
- [ ] DCO sign-off on every commit (`git commit -s`)
