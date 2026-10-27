# Migration baseline squash — Design

- **Date:** 2026-10-27
- **Status:** Approved
- **Type:** Store/infra change for v2.5.0: migrations 0001–0077 are folded into
  one baseline file, the `migrate` subcommand gains a collapse step that lets
  existing databases skip the baseline, and the release carries a migration
  guide whose supported paths include a direct upgrade from v2.3.1. No HTTP
  surface changes. The engine's `migrate` subcommand becomes the only
  supported way to apply migrations.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — `crates/eros-engine-store/migrations/`,
  `run_migrations()` in `crates/eros-engine-server/src/main.rs`, store tests,
  `docs/deploying.md`, `docs/migrating/`.
- **Builds on:** `2026-10-26-insights-2-fact-ledger-design.md` §10 (the v2.4.x
  two-step); the v2.4.0/v2.4.1 migration guides, whose upgrade rules this spec
  preserves.

## 1. Motivation

The repository carries 82 migration files (392 KB). A fresh deployment replays
four years of schema history — tables created and dropped again, columns added
and removed, backfills over data that does not exist — and every `sqlx::test`
does the same for every test database. The history only matters to databases
that lived through it; for everyone else it is noise that slows setup and
makes the schema unreadable as a whole.

At the same time, the upgrade path must not regress:

- A deployment on **v2.4.x** keeps upgrading forward unchanged.
- A deployment on **v2.3.1** must be able to upgrade **directly to v2.5.0**,
  without installing v2.4.x first. (It still reads the v2.4.0 and v2.4.1
  guides: every contract change and the loss of the v2.3.x rollback apply
  exactly as in the two-step path; v2.5.0 only makes the jump a supported
  single deploy.)
- A **fresh install** gets the current schema from one baseline plus the few
  post-baseline increments.

## 2. Decisions

1. **The baseline floor is 0077 — the v2.3.1 schema.** Migrations 0000–0077
   (`0000_schema.sql`, one `CREATE SCHEMA` line, included) are deleted and
   replaced by one `0000_baseline.sql` — version 0, taking over the slot the
   old schema stub held — whose result is exactly what replaying 0000–0077
   produces: the schema byte-for-byte, plus the one constant seed the chain
   carries (0020's ten `error_handling_config` fallback rows; every other
   data-touching statement in 0000–0077 is a no-op on an empty database).
   Migrations 0078+ (v2.4.x and later) remain ordinary incremental files,
   which is what makes the direct v2.3.1 → v2.5.0 upgrade work: such a
   database needs exactly those files.
2. **The baseline file is plain, unconditional SQL.** The logic that decides
   whether a database needs it lives in Rust, in the `migrate` subcommand
   (§4), not in the SQL. The file's only guard is a loud abort when it is run
   against a non-empty database outside the engine's `migrate` command (§3).
3. **Existing databases are collapsed, not ignored.** Before running the sqlx
   migrator, the migrate path rewrites `_sqlx_migrations` on a database that
   has 0077 applied: rows 0–77 are deleted (row 0 is the old
   `0000_schema.sql`'s — its checksum differs from the baseline's, so it must
   be rewritten, not kept) and one baseline row (version 0, the embedded
   file's checksum) is inserted, in a single transaction. After the collapse,
   sqlx's own validation — including its applied-but-missing check — passes
   untouched. `Migrator::set_ignore_missing` is **not** used; the integrity
   check stays on.
4. **Databases older than v2.3.1 are refused loudly.** A database whose
   migration history exists but does not reach 0077 cannot upgrade directly:
   its pending migrations below the floor no longer exist as files. `migrate`
   exits with an error telling the operator to upgrade to v2.3.1 first. The
   silent alternative (sqlx skipping to 0078 over a gap) is exactly the
   corruption this check exists to prevent.
5. **The collapse ships as the store crate's public migrate entry** (my
   call): `eros_engine_store::migrate(&pool)` = collapse + `Migrator::run`,
   and `run_migrations()` in main.rs becomes one call to it. The store crate
   gains sqlx's `migrate` feature outside dev-deps. This is what keeps crate
   embedders whole: the docs currently tell them to point `sqlx::migrate!` /
   `sqlx migrate run --source` at the crate's migrations directory, which
   after the squash fails with `VersionMissing` on any existing database.
   The embedder docs (store README, `docs/deploying.md`) switch to calling
   `eros_engine_store::migrate`. Raw sqlx against the directory stays fine
   for fresh databases and unsupported for existing ones; the baseline's
   guard aborts loudly in the odd states that reach it.
6. **v2.5.0 is the release vehicle.** Minor bump (crate snippets move to
   `= "2.5"`), standard release flow. The store crate publishes with the
   squashed migrations directory.

## 3. The baseline file

`crates/eros-engine-store/migrations/0000_baseline.sql`:

- **Header comment:** what it is, the floor (v2.3.1 / 0077), and that
  existing databases never run it — the `migrate` collapse (§4) marks it
  applied instead.
- **Guard first:** one `DO` block that raises an exception when
  `engine.chat_messages` already exists, naming the fix
  (`run 'eros-engine migrate', not raw sqlx, against an existing database`).
  On the supported path the guard is unreachable: fresh databases pass it,
  and collapsed databases never execute the file.
- **Body:** the complete v2.3.1 schema — `CREATE EXTENSION IF NOT EXISTS
  vector`, the `engine` schema, every table/index/constraint/function as of
  0077, RLS enables, and the role-guarded lockdown `DO` blocks (REVOKE from
  `anon`/`authenticated` where those roles exist) carried over by hand, since
  a dump from a non-Supabase scratch database loses them.
- **One seed, no other data:** the body ends with 0020's constant `INSERT` of
  the ten `engine.error_handling_config` fallback phrases — the single
  statement in 0000–0077 that writes rows an empty database would otherwise
  lack (tests and the chat queue rely on them). Not in the body:
  `_sqlx_migrations` (sqlx owns it), `SET`/ownership noise from the dump, and
  every other data-touching step of the old chain (backfills and fixes over
  rows that do not exist on a fresh database).

**Generation procedure** (one-time, documented in the PR, not shipped):
check out `v2.3.1` in a worktree, run its migrations against a scratch
database, `pg_dump --schema-only --no-owner`, strip the dump noise and the
`_sqlx_migrations` table, re-add the conditional lockdown blocks, prepend the
guard. **Parity check:** diff the `pg_dump` of (baseline on a fresh database)
against (v2.3.1's 0001–0077 on a fresh database) — the diff must be empty
before the PR opens. The ongoing guarantee after that is the whole store and
server test suites running against baseline-built databases, plus the 0079/0081
shape tests.

## 4. The collapse step in `migrate`

`run_migrations()` (main.rs) becomes: connect → **collapse** → `Migrator::run`.

`run_migrations()` (main.rs) becomes one call to the new
`eros_engine_store::migrate(&pool)`: **collapse → `Migrator::run`**, with the
collapse under the same advisory lock sqlx's migrator takes (or an equivalent
guard), so two concurrent `migrate` runs cannot race it.

Collapse, one transaction, using the embedded migrator's own data
(`sqlx::migrate!` exposes each migration's `version` and `checksum`; note the
old `0000_schema.sql` means **every existing database already has a version-0
row** whose checksum differs from the baseline's — presence of version 0 says
nothing by itself):

- `_sqlx_migrations` missing → fresh database → do nothing (the migrator
  applies the baseline and everything after it).
- Row 0's checksum equals the embedded baseline checksum and no rows 1–77
  exist → already collapsed → do nothing.
- Version 77 applied → `DELETE … WHERE version BETWEEN 0 AND 77`,
  `INSERT (version 0, 'baseline', success, <embedded baseline checksum>,
  execution_time 0)`. Log one line with the collapsed count.
- Otherwise (history exists but never reached 0077) → abort: *"this database
  predates v2.3.1; upgrade to v2.3.1 and run its migrations first"*.

Checksums of rows 0–77 are deleted without being verified — they belong to
files that no longer exist; 0078+ rows keep sqlx's normal checksum
validation. The collapse is idempotent and crash-safe: it either commits
whole or leaves the old rows for the next run.

## 5. Tests

- **Fresh path:** every existing `#[sqlx::test(migrations = …)]` test in the
  workspace now builds its database via baseline + 0078…; the suites passing
  is the main regression net. The 0079/0081 shape tests keep working
  unchanged.
- **Upgrade path, end to end:** a `#[sqlx::test]` (no migrations — bare
  database) store test that fabricates a v2.3.1 database — executes the
  baseline body directly (it *is* the v2.3.1 schema), creates
  `_sqlx_migrations` and inserts rows 0–77 with dummy checksums (row 0
  standing in for the old `0000_schema.sql`) — then runs the real
  `eros_engine_store::migrate` path and asserts: rows 1–77 gone, row 0
  present with the embedded baseline checksum, 0078+ applied, a 0081 shape
  probe (the renamed audit table exists), and the ten
  `error_handling_config` seed rows present. This is the
  direct-from-v2.3.1 upgrade in miniature, built from files that still exist.
- **Collapse unit cases:** fresh (no-op), already-collapsed (no-op, second
  run), pre-v2.3.1 history (abort with the message), post-collapse re-run of
  `migrate` (idempotent).
- **Deleted with their subject matter** (my call, same precedent as the
  v2.4.1 backfill tests): the two `events.rs` tests that replay history
  around the 0072/0073 one-shot v1-event data migration
  (`the_backfill_copies_v1_events_and_leaves_them_in_place`,
  `the_v1_event_metadata_and_its_index_are_dropped`). They test a one-shot
  migration whose files this spec removes; the end-state shape probes stay.
- **Guard:** executing `0000_baseline.sql` against an already-migrated test
  database raises the guard's exception.

## 6. Documentation

- `docs/migrating/v2-5-0-migration-baseline.md` (English only):
  - v2.4.x → v2.5.0: nothing to do beyond the normal image swap + `migrate`;
    the collapse rewrites bookkeeping only.
  - **v2.3.1 → v2.5.0 directly (newly supported):** one `migrate` applies
    0078–0081 and the collapse. Everything the v2.4.0 and v2.4.1 guides say
    applies in one step — read both; in particular: **back up first** (the
    only rollback from this jump is a restore), export the seven retired
    profile columns beforehand if their frozen v2.3.x values matter, update
    anything that queries the audit table by its old name in the same deploy,
    and ship the new stage-1 `filter_prompt` with the image.
  - Older than v2.3.1: upgrade to v2.3.1 first; `migrate` refuses otherwise,
    with the same message.
  - sqlx-cli / raw `sqlx migrate run` is unsupported against existing
    databases; the baseline aborts rather than half-applying.
  - **Rollback:** to v2.4.x works only without running the old image's
    `migrate` — on a collapsed database it fails hard (checksum mismatch at
    version 0) before changing anything, and on Fly that means overriding the
    release command for the rollback deploy. `serve` never reads
    `_sqlx_migrations`, so the rolled-back image runs normally.
  - **Shared-database warning:** when several apps run against one database
    (a staging and a production app, say), the first v2.5.0 `migrate`
    collapses it for all of them; from that moment an older image can no
    longer deploy through a normal release command on any of the apps.
    Upgrade them together.
- `docs/deploying.md` + `.zh.md` and the store crate README: embedders call
  `eros_engine_store::migrate` instead of pointing `sqlx::migrate!` /
  `sqlx migrate run --source` at the directory; the engine's `migrate`
  subcommand is that same call.
- `docs/architecture.md` + `.zh.md`: migration-count mentions, if any.

## 7. Out of scope

- No schema change of any kind: the end state of every path is byte-identical
  to today's.
- No change to how `sqlx::test` fixtures work.
- No squash of 0078+ (they are the live upgrade path); a future squash can
  repeat this pattern with a higher floor.
- The v2.5.0 release flow itself (version bump, tag, crates) — standard, not
  part of this spec.

## 8. Left to the builder

- The exact collapse SQL and the public helper's signature/error type in
  `eros-engine-store`.
- Repointing docs that link to deleted migration files
  (`memory-layers*.md`, `life-events-needs*.md`, and any other
  `migrations/00NN_*.sql` references).
- The baseline's internal ordering (pg_dump's order is fine; grouping by
  table for readability is optional).
- Dollar-quoting tags inside the baseline (functions defined with `$$` must
  not collide with the guard block's tag).
- Test fixture placement and the dummy-checksum shape for the fabricated
  v2.3.1 history.
- Whether the collapse logs per-row or one summary line.
