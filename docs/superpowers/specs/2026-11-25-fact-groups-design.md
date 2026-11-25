# Fact groups: paraphrases of one fact, listed and deleted together

Issue: #574. Release: v2.8.2.

## 1. Intent

`engine.human_facts` folds a new fact into an existing row only when the
normalised text is identical. The same thing said in other words is a new row,
and `GET /v2/comp/user/{user_id}/facts` returns every row, so a client that
shows users what the characters remember about them lists one fact many times
over for the people who talk most. On a production deployment the ledger holds
about 40k rows over about 760 users; the median user has a handful of facts,
the largest about 3k, and 40% of rows are `roleplay_expression`. The repeats
sit with the heaviest users, who are also the likeliest to open such a page.

This change groups the rows that state the same thing about the same user, so
the owner can see one item per group, with the rows behind it, and delete a
whole group in one call.

## 2. Goals, non-goals, anti-goals

Goals:
- Facts that state the same thing about the same user share a group. Real and
  roleplay facts never share one.
- The owner's per-user listing returns one item per group: a representative
  text, the group's counters, first and last seen, and the rows behind it.
- One call deletes a whole group. Each deleted row lands in
  `GET /svc/v1/rows/deletions` as a `fact` tombstone, so mirrors follow
  without change.
- Every row stays in the ledger. A group is a view over the rows; nothing is
  merged, rewritten or overwritten.
- The rows already in the ledger get grouped too.

Non-goals:
- The profile rebuild keeps reading rows, not groups. Feeding it one line per
  group would hide from it the details and negations that members differ in,
  and its 16,000-character budget already holds the whole ledger for nearly
  every user.
- The facts export (`GET /svc/v1/facts/changes`) does not carry the group id.
  A row is placed after it first appears in that stream, and placement does
  not move `last_seen_at`, so an exported id would be a placeholder for most
  rows and never be corrected.
- No model call decides membership. A model check on candidates can be added
  later as a stricter gate on top of this one (§4).
- No change to the dedup key, `engine.fact_norm`, or fact extraction.

Anti-goals (would make this a failure even if it works):
- Groups that chain distinct facts into one large cluster, so that a group
  delete wipes facts that do not restate the group's first fact.
- Any write that changes a member row's text, labels or counters.
- An embedding call on the chat turn's path.

## 3. Constraints

- Embeddings come from the engine's existing router; documents are embedded
  with the write backend (`EmbeddingRouter::embed_documents`, 512 dimensions,
  the same space as the memory tables). The engine's built-in default and the
  example configs write with voyage-4-lite; a deployment may split reads and
  writes and write with voyage-4-large. The router has no batch chunking, and
  an OpenAI-compatible backend may cap inputs per request below the default
  batch.
- Measured on a production deployment's ledger: on labelled near-duplicate
  pairs, voyage-4-large at 0.94 caught every pair that two independent judges
  both called a duplicate, while about one in nine of the pairs it joins
  differ in a detail. No bar separates those from true paraphrases: the two
  kinds overlap from 0.941 to 0.973. voyage-4-lite joins more distinct facts
  than large at every bar; lite at 0.96 is about where large is at 0.94. Below
  those bars, single linkage chains different facts into large clusters.
- `engine.*` rules: every id that points at a row of another table has a FK; a
  correlation id with no table behind it is not a reference. No new external
  identity columns.
- Deployments may run several engine instances against one database.

## 4. Approaches

- **A. Embeddings only (picked).** Each fact gets a vector; a background
  sweeper places it in the group of the nearest group's first fact when the
  cosine similarity reaches a threshold, else it stays its own group. The
  rows already in the ledger go through the same sweeper. Spend is embedding
  calls only. Wrong groups are possible at about the measured rate; the
  listing shows every member before a delete, and the stored vectors let the
  threshold be re-evaluated offline.
- **B. Embeddings plus a model check.** The same vectors find candidates, and
  a model confirms each with a yes/no, catching negations. Needs a new task
  config, prompt and audit stage, and grouping the existing rows becomes paid
  model calls run by an operator.
- **C. Groups from the profile rebuild.** The rebuild already reads the
  ledger, but only up to its character budget, so the heaviest users' tails
  would never be grouped. Dropped.

A was picked: it keeps the change small, a wrong group errs toward deleting
more of the owner's own facts (never another user's), and B stays available
as a later gate that raises the bar without removing anything.

## 5. Design

### 5.1 Schema — migration `0090_fact_groups.sql`

```sql
ALTER TABLE engine.human_facts
    ADD COLUMN group_id  UUID NOT NULL DEFAULT gen_random_uuid(),
    ADD COLUMN embedding vector(512);

CREATE INDEX idx_human_facts_unplaced
    ON engine.human_facts (first_seen_at, id) WHERE embedding IS NULL;
```

- `group_id` is an opaque correlation id with no table behind it, so it has no
  FK. Every row has one: a new row starts as its own one-row group. The store
  test `every_reference_column_in_engine_has_a_foreign_key` exempts
  `human_facts.group_id` by table and name, as it does `first_run_id` /
  `last_run_id`.
- `embedding` is `NULL` until the row is **placed** (§5.2). The vector and the
  placement decision are written in the same `UPDATE`, never apart, so
  `embedding IS NOT NULL` means "placed".
- No vector index: matching runs within one user's rows (a few thousand at
  most), where an exact scan is cheap. The existing
  `idx_human_facts_user_seen (user_id, …)` serves the per-user filter.
- The volatile default rewrites the table under an exclusive lock while the
  migration runs; at tens of thousands of rows that is seconds. Every existing
  row gets its own fresh `group_id` and stays unplaced.
- The previous image's `INSERT` does not name the new columns, so it keeps
  working against the new schema.

### 5.2 Placement — the fact-group sweeper

A new sweeper in `crates/eros-engine-server/src/pipeline/`, spawned from
`serve` like the profile-rebuild sweeper. Each tick:

1. Read up to `batch` unplaced rows, oldest first:
   `WHERE embedding IS NULL ORDER BY first_seen_at, id LIMIT $batch`, no lock.
2. Embed their `content` in one `embed_documents` call. On error, warn and end
   the tick; the rows stay unplaced and the next tick retries them.
3. Place the rows one at a time, in the batch's `(first_seen_at, id)` order,
   each in its own transaction:
   1. `pg_advisory_xact_lock(hashtextextended('fact_groups:' || user_id, 0))`,
      so two instances never place one user's rows at the same time (the
      pattern `events.rs` uses per instance).
   2. The **anchor** of a group is its earliest member by
      `(first_seen_at, id)` among placed rows. Among the user's placed rows
      with the same `roleplay`, find the anchor nearest to this row's vector
      (pgvector `<=>`, cosine distance).
   3. If `1 − distance ≥ threshold`, the row takes that anchor's `group_id`.
      Otherwise it keeps its own.
   4. `UPDATE … SET embedding = $v, group_id = $g WHERE id = $id AND
      embedding IS NULL`. Zero rows (deleted, erased, or placed by another
      instance) is a skip, not an error.

   A transaction holds at most one ledger row lock, so it cannot deadlock
   with `upsert_run` bumping several rows. Rows committed earlier in the batch
   are visible to later ones, so two paraphrases arriving in one batch group
   with each other.
4. Log the tick's counts (placed, joined a group, skipped).
5. A full batch starts the next one at once instead of waiting for the tick,
   so a backlog (the existing rows after the migration) drains in minutes.

The batch is the oldest unplaced rows across all users, so it holds every
unplaced row of a user that is older than that user's newest row in it.

Comparing only against anchors means a group never grows by chaining: every
member is within the threshold of the group's first fact. Taking rows oldest
first, with the per-user lock, means a row is placed after every earlier row
of the same user, so an anchor is always the group's earliest fact.

The sweeper only updates rows that exist. It never inserts, so it cannot bring
back a deleted fact or write for an erased user.

### 5.3 Configuration

Env vars, parsed like `PROFILE_REBUILD_*` (a pure parser with tests;
unparseable values keep the default):

| Var | Default | Meaning |
|-----|---------|---------|
| `FACT_GROUPING_DISABLED` | off | `1`/`true` keeps the sweeper from starting. Nothing is placed; the listing shows every row as its own group. |
| `FACT_GROUPING_TICK_SECS` | `60` | Sweep interval. `0` disables. |
| `FACT_GROUPING_BATCH` | `500` | Rows per tick and per embedding call, clamped to 1..=1000. |
| `FACT_GROUPING_THRESHOLD` | `0.96` | Cosine similarity a row needs to join a group. Values outside (0, 1] keep the default. |

One default for every model. `0.96` is the measured bar for voyage-4-lite,
the engine's default write model. On voyage-4-large it is stricter than
needed: it misses about one in twenty judged duplicates and joins about half
as many detail-differing pairs as `0.94`, the measured bar for large. A
deployment that writes with voyage-4-large can set `0.94`. Other models are
unmeasured.

### 5.4 Listing groups

`GET /v2/comp/user/{user_id}/fact-groups`, in the JWT tree, owner only (the
path's `user_id` must be the token's, else 403), next to the existing facts
list in `routes/user_rows.rs`.

Query: `limit` (groups per page, default 100, clamped 1..=500), `before` +
`before_id` (the previous page's `next`; half a cursor is 400).

Order: newest first by the group's `first_seen_at` (its earliest member's),
then `group_id` descending.

Response:

```json
{
  "groups": [
    {
      "group_id": "uuid",
      "content": "the representative member's text",
      "roleplay": false,
      "occurrences": 7,
      "session_count": 4,
      "recurrence": "repeated_across_sessions",
      "first_seen_at": "…",
      "last_seen_at": "…",
      "facts": [ /* OwnedFact, representative first */ ]
    }
  ],
  "next": { "first_seen_at": "…", "group_id": "uuid" }
}
```

- `next` is present iff the page is full, as in the other owner lists.
- **Representative**: the member with the most `occurrences`; ties go to the
  earliest `first_seen_at`, then the smaller `id`. Derived at read, never
  stored.
- `facts`: every member, in the representative's order (occurrences desc,
  first_seen_at asc, id asc), so `facts[0]` is the representative.
- Counters: `occurrences` and `session_count` are sums over members;
  `recurrence` is derived from the sums by the row rule; `first_seen_at` is
  the minimum, `last_seen_at` the maximum.
- `roleplay`: shared by every member (placement never crosses it).
- An unplaced row is a one-row group.

### 5.5 Deleting a group

`DELETE /v2/comp/user/{user_id}/fact-groups/{group_id}`, owner only. `204`;
`404` when no fact of this user has that `group_id` (a repeat included);
`403` for another user's path.

One transaction, in this lock order:
1. The user's fact-group advisory lock (§5.2), so no row joins the group while
   it is being deleted.
2. The user's `human_profile_state` upsert, exactly as the single-fact delete
   does it (claim cleared, `pending_facts + 1`, `last_fact_at = now`,
   `rebuild_failures = 0`, `rebuild_due = true`): one call is one deletion,
   whatever the group's size.
3. `DELETE FROM engine.human_facts WHERE user_id = $1 AND group_id = $2
   RETURNING id`. No rows: roll back, 404.
4. One `row_deletions` tombstone per deleted id, kind `fact`.

The delete removes the group as it stands when the call runs. A fact placed
into the group after the owner loaded the listing goes with it: the engine
judged it the same fact, and the error only ever removes more of the owner's
own facts. A one-row group whose fact has since joined another group no
longer exists, so its delete is a `404`; the client reloads the listing and
finds the fact in its new group.

### 5.6 The flat list

`OwnedFact` (the item of `GET …/facts` and of a group's `facts`) gains
`group_id`. Additive.

### 5.7 What does not change

- `upsert_run`: a new row gets a fresh `group_id` by default and no vector; a
  dedup hit keeps the row's group.
- The single-fact delete. When it removes a group's anchor, the next earliest
  member becomes the anchor.
- Erasure: rows go as before, their vectors with them.
- The profile rebuild, the facts export, the deletion streams, fact extraction.

### 5.8 Existing rows

After the migration every existing row is unplaced and alone; the sweeper
places them oldest first through the same path as new rows, batch after batch
without waiting for the tick (§5.2). For a ledger of about 40k short rows that
is about 80 embedding calls and roughly 0.6M embedding tokens. Until a row is
placed, the listing shows it as its own group.

### 5.9 Re-embedding after an embedding model change

Vectors from another model are in another space. To regroup from scratch:

```sql
UPDATE engine.human_facts SET embedding = NULL, group_id = gen_random_uuid();
```

Every row is unplaced and alone again, and the sweeper places them anew. Group
ids change, so a client holding an old one gets 404.

## 6. Deployment and rollback

- Run `migrate` before the new image serves (it selects the new columns). The
  previous image keeps working against the migrated schema, so instances can
  be swapped at any pace.
- The migration's table rewrite blocks ledger writes for its duration.
- An instance that should run no background work sets
  `FACT_GROUPING_DISABLED=1` with the other sweeper switches.
- Rollback: the previous image ignores the two columns; leave them. Upgrading
  again resumes placement where it stopped.
- Spend: embedding calls only, one per tick with work.

## 7. Known limits

- At the measured bars (0.96 on voyage-4-lite, 0.94 on voyage-4-large), about
  one in nine pairs of near-duplicates that land in one group differ in a
  detail, and no threshold separates them from true paraphrases. A group
  delete removes every member; the listing shows them first.
- A group delete also removes members placed after the owner loaded the
  listing, and a one-row group whose fact has joined another group returns
  `404` (§5.5).
- A restatement close to a later member but below the threshold to the group's
  first fact starts its own group (no chaining, by design).
- A new fact is its own group until the next tick.
- Group counters are sums: a run or session in which two members were both
  seen counts twice.
- Deleting a group's earliest member alone moves the group's place in the
  newest-first order; a page walk at that moment can skip or repeat that
  group.
- The export does not carry the group id.
- An embedding error holds back the whole batch until a later tick succeeds.
  A backend that caps inputs per request below `FACT_GROUPING_BATCH` fails
  every batch; set the batch at or under its cap.

## 8. Tests

- Store: placement with seeded vectors — joins at or above the threshold,
  stays alone below it, never across `roleplay`, never across users, matches
  anchors only (no chaining), groups two paraphrases within one batch, skips
  a row deleted or placed meanwhile.
- Store: group listing — representative and member order, summed counters,
  min/max timestamps, one-row groups for unplaced rows, keyset paging.
- Store: group delete — every member gone, one `fact` tombstone each, profile
  marked due, 404 on a repeat and on another user's group id.
- Sweeper against a mock embedding server: one call per batch; an error leaves
  the rows unplaced.
- Config parser: defaults, disable flags, clamps, invalid threshold.
- Routes: 200/204/400/403/404 and the response shapes; `group_id` on the flat
  list.
- OpenAPI snapshot regenerated.

## 9. Docs (separate PR after the code PR)

- `docs/insights.md` and `.zh.md`: the two new columns, a fact-groups section
  (placement, threshold, representative, counters), deleting a group, the
  config vars, the re-embedding operator note, and the known limits (replacing
  "the ledger keeps paraphrases as separate rows").
- `docs/api-reference.md` and `.zh.md`: the two routes and `group_id` on the
  flat list.
- Wherever the sweeper env vars are listed for deployers.
- `docs/migrating/v2-8-2-fact-groups.md`: migration, rewrite lock, backfill by
  the sweeper, the disable switch, rollback.

## 10. Left to the builder

The SQL shape of the anchor and listing queries, module and type names, how
tombstones are inserted in bulk, log fields, and test layout.

## 11. Decisions

Decided by the maintainer: approach A (embeddings only); ships in v2.8.2; a
group delete removes the group as it stands when the call runs (§5.5); one
default threshold, 0.96, for every model (§5.3).

Design calls in this document: matching against each group's first fact only;
one transaction per placed row; a full batch starting the next at once; an opaque per-row `group_id` with a fresh
default; placement by a background sweeper rather than on the write path, with
existing rows placed by the same sweeper; the per-user advisory lock shared by
placement and group delete; the representative rule; summed counters; one
`pending_facts` bump per group delete; `group_id` on the flat list but not in
the export; the profile rebuild left on rows; the route paths and the env var
names and defaults.
