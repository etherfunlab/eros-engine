# Migrating to v2.5.0: the migration baseline

**Applies to:** operators upgrading from `v2.4.x` or directly from `v2.3.1`,
and code that embeds the `eros-engine-store` crate and runs its migrations
itself. Nothing on the HTTP surface changes.
**Design:** [`2026-10-27-migration-baseline-squash-design.md`](../superpowers/specs/2026-10-27-migration-baseline-squash-design.md)

Migrations `0000–0077` — the `v2.3.1` schema — are folded into one
`0000_baseline.sql`; `0078+` stay incremental. The first `migrate` on an
existing database rewrites its `_sqlx_migrations` bookkeeping (rows 0–77
become one baseline row); the schema itself is untouched by that rewrite.

## From v2.4.x

Swap images and run `migrate` as usual. The collapse is bookkeeping only:
no schema change, metadata-speed, idempotent. A history that carries a
failed migration row is refused with the version named — repair it first
(unreachable under normal operation).

## From v2.3.1, directly (newly supported)

One deploy takes a `v2.3.1` database to this release: a single `migrate`
runs `0078–0081` plus the collapse. Everything the
[`v2-4-0`](v2-4-0-insights-2-0.md) (and its
[sibling](v2-4-0-event-selfie-image.md)) and
[`v2-4-1`](v2-4-1-insights-cleanup.md) guides say applies in this one step —
read them first. In particular:

- **Back up before `migrate`.** A restore is the only rollback from this
  jump: `0081` drops columns in the same run.
- Export the seven retired profile columns beforehand if their frozen
  `v2.3.x` values matter to you.
- Ship the new stage-1 `filter_prompt` (object-array contract) with the
  image, and update anything that queries the audit table by its old
  `companion_insights_events` name in the same deploy.

## Older than v2.3.1

Not supported directly: the pending migrations below the baseline floor no
longer exist as files, and `migrate` refuses loudly ("this database predates
v2.3.1"). Upgrade to `v2.3.1`, run its migrations, then come back here.

## Embedders

`sqlx migrate run --source …` and a raw `sqlx::migrate!` against the crate's
`migrations/` directory no longer work on an **existing** database (sqlx
reports the folded versions as missing; against a populated schema the
baseline's own guard aborts). Call the crate's entry instead — it is what
the engine binary's `migrate` subcommand runs:

```rust
eros_engine_store::migrate(&pool).await?;
```

A **fresh** database works either way; the supported path is the same call.

## Rollback

- **To `v2.4.x`:** the rolled-back image `serve`s normally — the schema is
  exactly what it expects. Do **not** run its `migrate`: on a collapsed
  database it fails hard at the version-0 checksum before changing anything.
  On a platform that runs `migrate` as a release step (Fly's
  `release_command`), override or skip that step for the rollback deploy.
- **Shared database, several apps** (a staging and a production app): the
  first deploy of this release collapses the bookkeeping for all of them,
  and from that moment an older image can no longer deploy through a normal
  release command on any of the apps. Upgrade them together.
- The collapse itself loses nothing: it rewrites bookkeeping rows, not
  schema or data. Un-collapsing (for an old image's `migrate`) is not
  supported; roll forward instead.
