# Migrating to v2.8.2: fact groups

**Applies to:**
- deployments (one migration that rewrites `engine.human_facts`, a new background sweeper, and a one-time embedding pass over the existing ledger);
- clients that show users what the engine learned about them;
- crate users who build `HumanFactRow` by hand.

Migration `0090` must run before the new image serves. Every field on the wire is additive: two routes, and `group_id` on the owner's fact list. The facts export, the profile rebuild and erasure are unchanged.
**Design:** [`2026-11-25-fact-groups-design.md`](../superpowers/specs/2026-11-25-fact-groups-design.md)

The other notes for this release are [her accepted bed and bath asks place their events](v2-8-2-life-choice-events.md), [Affinity 5.0, the relationship status and the cold war](v2-8-2-affinity-5-0.md), [the focused tone, four together actions and the user's gender in a picture](v2-8-2-focused-tone-together-actions-user-gender.md) and [her period on a 28-day cycle](v2-8-2-period-cycle.md).

The ledger keeps one row per wording, so a user who says the same thing in other words gets one row per wording, and a client that lists the facts shows that fact many times. Fact groups put those rows in one group, which the owner can list as one item and delete in one call. The rules are in:
- [fact groups](../insights.md#fact-groups) and [deleting one fact or memory](../insights.md#deleting-one-fact-or-memory);
- [the group list](../api-reference.md#get-v2compuseruser_idfact-groups) and [the group delete](../api-reference.md#delete-v2compuseruser_idfact-groupsgroup_id).

## What users will notice

- **Nothing, until a client offers the group routes.** With them, a fact said several ways is listed once, with its wordings behind it, and one delete removes all of them.
- **A group can hold facts that differ in a detail.** At the measured thresholds about one in nine pairs in a group does, so a client should show the facts behind a group before the user deletes it.

## Deployments

### Migration `0090` and deploy order

`0090` adds two columns to `engine.human_facts`: `group_id UUID NOT NULL DEFAULT gen_random_uuid()` and `embedding vector(512)`, and a partial index on the rows whose `embedding` is `NULL`. The volatile default rewrites the table and gives every existing row its own fresh `group_id`. The rewrite holds an `ACCESS EXCLUSIVE` lock on `engine.human_facts` while it runs: reads and writes of the ledger wait, and the `ALTER` itself first waits for any open transaction on the table. On a ledger of tens of thousands of rows it takes seconds.

1. **Run `migrate`.** The previous image names its columns in every read and write of `engine.human_facts`, so it keeps working against the new schema.
2. **Start the new image.** Machines can be swapped at any pace.

### The new sweeper, and the first pass over the ledger

`serve` starts a fact-group sweeper. Every `FACT_GROUPING_TICK_SECS` (default `60`) it embeds up to `FACT_GROUPING_BATCH` unplaced rows (default `500`) in one call to the write embedder and places each into the group of an earlier fact it restates; a full batch starts the next one at once.

After the migration every existing row is unplaced, so the first ticks place the whole ledger, oldest first. A ledger of 40,000 short rows takes about 80 embedding calls and roughly 0.6M tokens, done within minutes. Until a row is placed, it lists as a group of its own.

| Variable | Default | |
|----------|---------|---|
| `FACT_GROUPING_DISABLED` | unset | `1` or `true` keeps the sweeper from starting. Set it on an instance that should run no background work. |
| `FACT_GROUPING_TICK_SECS` | `60` | `0` also stops the sweeper. |
| `FACT_GROUPING_BATCH` | `500` | Clamped to 1..=1000. Set it at or under the per-request input cap of a custom embedding backend, or every batch fails. |
| `FACT_GROUPING_THRESHOLD` | `0.96` | The measured bar for voyage-4-lite, the engine's default write model. **A deployment that writes with voyage-4-large sets `0.94`.** |

Several instances with the sweeper on give the same groups, but each may embed the front of the queue; leave it on in as few instances as you like.

### Changing the write embedding model later

Vectors from another model are not comparable. After switching `[tasks.embedding]`'s write model, regroup from scratch:

```sql
UPDATE engine.human_facts SET embedding = NULL, group_id = gen_random_uuid();
```

The sweeper places every row again. Group ids change, so a client holding an old one gets `404`.

### Rollback

Roll back the image; the columns can stay, since the previous image ignores them. Its `migrate` refuses a database that has `0090` applied (an applied migration it does not know), so roll back without running the previous image's `migrate`. Rows the new image writes while the previous one is away keep defaults that the new image places when it returns.

## Clients

Two routes under the JWT tree, each checking that the path's `user_id` is the token's (`403` otherwise):

| Route | Result |
|---|---|
| `GET /v2/comp/user/{user_id}/fact-groups?limit=&before=&before_id=` | `{groups, next}`: one item per group, newest first by the group's earliest fact |
| `DELETE /v2/comp/user/{user_id}/fact-groups/{group_id}` | `204`, or `404` when no fact of this user has that group id |

1. **List groups instead of facts** where a user sees what the engine knows about them. Each group has `group_id`, `content` (the representative: the member seen most often, ties to the earliest), `roleplay`, `occurrences` and `session_count` (sums over the members), `recurrence`, `first_seen_at`, `last_seen_at`, and `facts`, every member in the facts list's shape with the representative first.
2. **Page with the group cursor.** Pass `next.first_seen_at` back as `before` and `next.group_id` as `before_id`; `limit` defaults to 100 (1..=500).
3. **Show the members before a group delete.** The delete removes every fact in the group as the call runs, including one placed into it after the list was read.
4. **Treat a `404` on a one-fact group as "moved".** A fact not placed yet is a group of its own; once placed it may join another group, and its old group id is gone. Reload the list to find it.
5. **Read `group_id` on the fact list if you keep it.** `GET /v2/comp/user/{user_id}/facts` gains `group_id` on every row; a client that deserializes strictly must accept it.

Each deleted fact gets its own `fact` tombstone on `GET /svc/v1/rows/deletions`, so mirrors need no change. The facts export does not carry `group_id`.

## Crate users

`eros_engine_store::human_fact::HumanFactRow` gains a public field, `group_id: Uuid`, so a struct literal of it needs the field. `HumanFactRepo` gains `unplaced`, `place`, `list_groups_owned` and `delete_group_owned`, and `delete_owned` now takes the user's fact-group lock before the profile-state row.
