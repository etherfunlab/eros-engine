# Migrating to v2.4.1: the Insights 2.0 cleanup

**Applies to:** operators and database consumers upgrading from `v2.4.0` to
`v2.4.1`, and code that depends on the `eros-engine-store` crate. Nothing on
the HTTP surface changes.
**Design:** [`2026-10-26-insights-2-fact-ledger-design.md`](../superpowers/specs/2026-10-26-insights-2-fact-ledger-design.md) (§10)
**Previous guide for this subsystem:** [`v2-4-0-insights-2-0.md`](v2-4-0-insights-2-0.md)

`v2.4.0` left the columns the fact ledger replaced, and the old audit table
name, in place so that a rollback to `v2.3.x` kept working. This release
removes them.

## Upgrade path: `v2.4.0` first, then `v2.4.1`

**`v2.4.1` is only supported on a deployment that is already running
`v2.4.0`.** A deployment on `v2.3.x` or earlier upgrades in two steps:

1. Upgrade to `v2.4.0` by following
   [`v2-4-0-insights-2-0.md`](v2-4-0-insights-2-0.md) (the `filter_prompt`
   contract, the model-config block and the export shapes change there). Run its
   `migrate`, serve it, and let it run until you have no reason to roll back to
   `v2.3.x`. Its idle sweeper drains the one-time profile rebuild queue in the
   meantime.
2. Only then upgrade to `v2.4.1`.

Do not jump from `v2.3.x` straight to `v2.4.1`. The migrations would apply in
order, but the deployment would skip the only release that can still roll back
to `v2.3.x`. `v2.4.0` is the step where the new extraction contract, the
rebuilt profiles and the new routes are checked against production traffic while
the old columns are still there to fall back to. After `0081`, they are not.

## What is dropped and renamed

- **Seven columns leave `engine.human_insights`:** `city`, `location`,
  `hometown`, `nationality`, `preferred_gender`, `age_min` and `age_max`. They
  held `v2.3.x` values that nothing has read or written since `v2.4.0`. None of
  them had an index or a constraint.
- **`engine.companion_insights_events` becomes `engine.human_insights_events`.**
  Rows and ids are unchanged, and the table's indexes and constraints are
  renamed with it (`idx_companion_insights_events_*` to
  `idx_human_insights_events_*`, `companion_insights_events_*` constraints to
  `human_insights_events_*`).
- **`engine.insights_2_backfill()` is dropped.** The one-shot backfill from `0080`
  has already run wherever it was going to.
- **Unchanged:** `engine.persona_story_insights`, which has its own `city`,
  `hometown` and similar columns for the world system, and the old keys inside
  the existing `engine.human_insights_snapshot` rows. History keeps what it
  recorded.

The erase endpoint reports the renamed table under its new key:
`blanked.human_insights_events` replaces `blanked.companion_insights_events`.

## Who has to act

- **Anything that SELECTs one of the seven columns from `engine.human_insights`**
  (a matching job, a dashboard, a read replica, an export script) fails once
  `0081` has run. The engine stopped writing them in `v2.4.0`, so what such a
  consumer reads is frozen `v2.3.x` data; collect location, gender preference
  and age range from the user directly.
- **Anything that queries the audit table by its old name** (cost dashboards,
  audit scripts, analytics views, foreign data wrappers, read replicas) moves to
  `engine.human_insights_events`. A view that selects from the old name keeps
  working through the rename, but a script that names the table in text does
  not. Grep the deployment for `companion_insights_events`.
- **A consumer of the erase response** that reads
  `blanked.companion_insights_events` reads `blanked.human_insights_events`.
- **Code that depends on `eros-engine-store`** and names the table in its own
  SQL moves to the new name; the Rust types (`InsightEventRepo` and the rest)
  keep theirs.

## Rollout

Run `migrate` as the release step right before switching images. The statements
are metadata changes: the column drops and renames take a brief
`ACCESS EXCLUSIVE` lock and rewrite nothing.

The old image keeps running against the new schema for the length of a rolling
deploy. Chat, the profile and the ledger are unaffected. Its audit inserts name
the old table and fail until it is replaced; they are fail-open, so the cost is a
warning in the log and a missing audit row for those few turns. Its erase
endpoint does not survive the window (see below).

## Rollback

- **To `v2.4.0`:** works for chat, the ledger and the profile, since `v2.4.0`
  reads only the columns that remain. Two things break, because that image
  names the old table: the stage-1 and rebuild audit inserts fail (fail-open: a
  warning per call and no audit row), and `POST /svc/v1/users/{user_id}/erase`
  fails outright, as its transaction touches the old name. **Do not roll back
  while erase traffic is expected.**
- **To `v2.3.x`:** no longer possible. That image reads the dropped columns.
- **Never run the old image's `migrate`.** It does not know `0081`, and a
  database that has applied it is ahead of the image.

The migration cannot be reversed by a later one without losing the dropped
values; restore from a backup taken before `migrate` if the columns' contents
matter.
