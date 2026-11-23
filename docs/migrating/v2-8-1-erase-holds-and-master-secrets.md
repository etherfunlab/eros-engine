# Migrating to v2.8.1: an erase that holds, two master secrets, pinned search paths

**Applies to:**
- deployments that call `POST /svc/v1/users/{user_id}/erase`, above all those that keep the user's chat history afterwards;
- deployments that used the erase to reset a user who then kept chatting;
- operators who manage the engine's secrets (two optional variables);
- deployments (one migration, `0087`, no data change);
- crate users who call `ProfileStateRepo::bump` or `ScanStateRepo::bump`.

No HTTP change: no new route, header or field. Migration `0087` has no ordering constraint with the new image.
**Design:** [`2026-11-23-erase-hold-master-secrets-and-search-path-design.md`](../superpowers/specs/2026-11-23-erase-hold-master-secrets-and-search-path-design.md)

The rules are in [Insights → Erasure](../insights.md#erasure), [the erase route](../api-reference.md#post-svcv1usersuser_iderase) and [Deploying](../deploying.md#operational-notes).

## What changed

### An erased user stays erased

Until v2.8.0 the tombstone in `engine.user_erasures` fed the deletion streams and nothing else. A deployment that erased a user and kept their chat history got the erased data back: `backfill-dreaming` re-read the history, a write in flight landed after the erase, and a later turn opened the counters again.

From v2.8.1 the tombstone means the user is gone:

| Was | Is |
|---|---|
| `backfill-dreaming` re-scanned an erased user's kept history | it leaves that history unread |
| a later turn wrote facts, memories and insights again | the turn is answered and derives nothing; no extraction or embedding call is made |
| "stop the user's traffic, then call again" was the only protection | the engine drops the writes itself; a repeat call is still harmless |
| an erasure could be followed by normal use | the erasure is permanent |

What to check:

1. **If you erase on account deletion:** nothing to change. You no longer need to archive the user's sessions to keep the backfill away from them.
2. **If you erased a user as a reset and let them keep chatting:** that user now gets replies and nothing else — no facts, profile, memories, insights or affinity. To readmit one, delete their row from `engine.user_erasures`. Every user erased before the upgrade is affected from the moment the new image starts.
3. **A turn sent after the erase** still writes its own audit rows (`companion_decision_events`, the image and vision events). Call the erase again to blank them.
4. **Call the erase before deleting the user's sessions or persona instances**, as before.
5. **A write in flight at the instant of the erase** can still land: one whose check began just before the tombstone committed. Stopping the user's traffic first avoids it, and a repeat call removes such a row.

### Two master secrets

Both are optional; a deployment that sets neither behaves as before.

| Variable | Stands in for | Turns on by itself |
|---|---|---|
| `SUPER_SECRET` | every feature secret | BYOK, BYOC and all four `/svc/v1` sub-trees |
| `ADMIN_SECRET` | every feature secret except `USER_ERASE_SECRET` | the same, except user erase |

- A master key is sent in the feature's own header, in place of its secret.
- Setting a master turns features **on** that were off: with `ADMIN_SECRET` alone, BYOK and BYOC are served and the export and genome-write sub-trees are mounted. Set one only where that is wanted.
- The caller-secret gate stays governed by `CALLER_SECRET`. A master key does not turn it on and passes it once it is on.
- Each is one value of at least 32 bytes. `ADMIN_SECRET` equal to `SUPER_SECRET` or to `USER_ERASE_SECRET` refuses boot.
- BYOC signs its init ticket with the key the caller presented: create the instance with the same key that minted the ticket.

### Migration `0087`

`0087_function_search_path` sets `search_path = ''` on `engine.touch_world_worldviews()` and `engine.fact_norm(text)`, which a hosted database's linter flags when they run with the caller's path. It rewrites no row and changes no result: both functions call only `pg_catalog` functions.

## Rolling back

Roll back the image; the migration can stay. On the `v2.8.0` image:
- the tombstone is ignored again, so an erased user's later turns derive data and `backfill-dreaming` reads their kept history;
- `SUPER_SECRET` and `ADMIN_SECRET` are not read: a feature that only a master turned on is off again, and a caller sending a master key is refused.

Deploy the older image without running its `migrate`: its migrator does not know `0087` and refuses a database that has it.

## Crate users

- `ProfileStateRepo::bump` returns `Result<Option<i32>, _>` and `ScanStateRepo::bump` returns `Result<Option<Bumped>, _>`. `None` means the user is erased and nothing was written.
- `session_archive::lock_live_session` reads a session of an erased user as not live. `session_archive::is_live` gives the same answer without the lock.
- `ErasureRepo::erase_user` runs two transactions: the tombstone, then the deletes.
