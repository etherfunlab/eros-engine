# Migrating to v2.6.4: deleting one fact or memory, and the row-deletion stream

**Applies to:**
- clients that want to show users what the engine learned about them;
- services that mirror `GET /svc/v1/memories/changes` or `GET /svc/v1/facts/changes`;
- deployments (one migration, one behaviour change to the derived profile).

Every HTTP change is additive. Migration `0084` must run before the new image serves.
**Design:** [`2026-11-10-row-deletion-and-deletion-stream-design.md`](../superpowers/specs/2026-11-10-row-deletion-and-deletion-stream-design.md)

The owner can now list and delete their ledger facts and their extracted profile memories one row at a time. A mirror learns of each such deletion from a new stream, `GET /svc/v1/rows/deletions`. The rules are in:
- [the owner routes](../api-reference.md#get-v2compuseruser_idfacts);
- [the row-deletion stream](../api-reference.md#get-svcv1rowsdeletions);
- [deleting one fact or memory](../insights.md#deleting-one-fact-or-memory);
- [rebuild outcomes](../insights.md#outcomes).

## What users will notice

- **Nothing, until a client offers the new routes.** With them, a user can see the facts and memories the engine extracted and delete any one of them.
- **A deleted fact leaves the profile within one sweep.** The profile-rebuild sweeper rebuilds the user on its next tick (every five minutes by default). A deleted memory is no longer recalled at once.
- **Some profiles empty out.** See [the behaviour change](#behaviour-change-a-ledger-that-supports-no-profile-clears-it) below.

## Deployments

### Order

1. Run migration `0084_row_deletions`. It adds the table `engine.row_deletions` and the column `engine.human_profile_state.rebuild_due` (default `false`). Both are additive, so the running image keeps working.
2. Roll out the new image.

No new setting. The stream is mounted when `INSIGHTS_EXPORT_SECRET` or `FACTS_EXPORT_SECRET` is set, and either header opens it. The owner routes sit in the JWT tree, behind `CALLER_SECRET` when you set one.

### Rollback

Roll back the image; the migration can stay. The old image ignores `row_deletions` and `rebuild_due`. A user marked due but not yet rebuilt is still queued: a delete also bumps `pending_facts` and `last_fact_at`, so the old sweeper rebuilds that user after the idle window instead of on the next tick. Tombstones written before the rollback stay readable by the new image when it returns.

### Behaviour change: a ledger that supports no profile clears it

Before, three rebuild outcomes left the user's `human_insights` row as it was:
- the ledger is empty;
- the ledger holds only `transient` facts that never recurred across sessions;
- the model's reply projects to nothing writable.

Now each of them writes an empty profile over an existing row: every field `null` or `[]`, `updated_at` moved. A user with no row still gets none. This applies to every user, not only to users who deleted something.

On an existing deployment, a profile the ledger no longer supports is cleared on the user's next rebuild, which their next new facts bring. Such profiles exist where a profile row predates the fact ledger and the backfill found nothing to seed, or where a model filled fields before the transient rule existed. To count the profiles whose next rebuild will clear them without a model call:

```sql
SELECT count(*)
FROM engine.human_insights i
WHERE NOT EXISTS (
  SELECT 1 FROM engine.human_facts f
  WHERE f.user_id = i.user_id
    AND (f.persistence <> 'transient' OR f.session_count > 1)
);
```

A due rebuild after a delete that fails three times in a row also clears the profile, so a deleted fact does not stay in the profile because the model kept failing.

## Mirrors

`memories/changes` and `facts/changes` can now lose rows one at a time. Before, a row left only through a user's erasure (announced on `insights/deletions` / `facts/deletions`) or a hard-deleted session (announced nowhere).

1. **Poll `GET /svc/v1/rows/deletions`** with either secret you already hold. Each row is `{kind, id, deleted_at}`. `kind` is `memory` (a row of `memories/changes`) or `fact` (a row of `facts/changes`); skip any other `kind`. Page and resume as on the other streams, overlapping a few seconds.
2. **Drop the row with that `id`, and keep the id.** A changes page fetched just before a delete can arrive after its tombstone. Before inserting a row from a changes page, check that its id is not tombstoned. A deleted id never comes back; the same statement extracted again is a new row with a new id.
3. **Keep honoring the erasure stream.** `insights/deletions` and `facts/deletions` are unchanged: user-level, and they still mean "drop everything for this user".
4. **Profile mirrors need nothing new.** A cleared profile arrives on `insights/changes` as a row with empty fields and a newer `updated_at`.

## Clients

Four routes under the JWT tree, each checking that the path's `user_id` is the token's (`403` otherwise):

| Route | Result |
|---|---|
| `GET /v2/comp/user/{user_id}/facts?limit=&before=&before_id=` | `{facts, next}`, newest first by `first_seen_at` |
| `GET /v2/comp/user/{user_id}/memories?limit=&before=&before_id=` | `{memories, next}`, newest first by `created_at` |
| `DELETE /v2/comp/user/{user_id}/facts/{fact_id}` | `204`, or `404` for an id that is not this user's fact, a repeat included |
| `DELETE /v2/comp/user/{user_id}/memories/{memory_id}` | `204`, or `404` likewise |

`limit` defaults to 100 (1..=500). Pass `next` back as `before` and `before_id` together; one without the other is `400 bad_request`.

Tell users what a delete does not reach:
- The fact's wording stays in the engine's extraction audit and daily profile snapshots until the account is erased.
- A memory may already have shaped something written before the delete.
- The verbatim record of what they said is not listed and cannot be deleted one row at a time.
