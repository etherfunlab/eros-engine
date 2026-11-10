# Deleting one profile memory or one ledger fact, and a deletion stream — Design

- **Date:** 2026-11-10
- **Status:** Approved
- **Type:** Engine change. One migration (`0084`: a new table, one new column
  on `engine.human_profile_state`). Additive API: four owner routes under
  `/v2/comp/user/{user_id}/…` and one service route under `/svc/v1`. One
  behaviour change for every user: a ledger that supports no profile now
  clears the derived profile (§5.4).
- **Owner:** enriquephl (sole dev)
- **Issue:** #504
- **Builds on:** `2026-10-26-insights-2-fact-ledger-design.md` (the ledger,
  the profile rebuild, the export streams, user erasure; it listed per-fact
  deletion as out of scope) and the memory layers
  (`docs/memory-layers.md`).

## 1. Why

A user's data can be erased as a whole (`POST /svc/v1/users/{user_id}/erase`),
but not one row at a time. The fact ledger has no per-fact deletion, a
profile-layer memory only goes with an erase, and nothing lets the owner even
list those rows. A downstream mirror of `GET /svc/v1/memories/changes` and
`GET /svc/v1/facts/changes` therefore has no way to learn that a single row
is gone.

This change gives the owner a list and a delete for both kinds of row, and
gives mirrors one stream of row-level deletions.

## 2. Goals, non-goals, anti-goals

**Goals**

- The owner can list their ledger facts and their extracted profile
  memories, and delete any one of them.
- A deleted fact drops out of the derived profile, including when it was the
  last one, and including when what is left projects to nothing.
- A mirror learns of every such deletion from one keyset-paged stream of
  `{kind, id, deleted_at}`.
- A deleted row never reappears in a change stream under its old id. The
  same statement extracted again later is a new row with a new id (this
  already holds: neither table dedupes against deleted rows).

**Non-goals**

- The text a fact left elsewhere. A fact's wording also sits in
  `human_insights_events.payload` (the extraction audit) and in the daily
  `human_insights_snapshot` rows. Deleting a fact does not touch them; only
  user erasure does.
- What was already written from a memory. The world director reads recent
  extracted memories into its prompt; a world seed, fragment or post written
  before a delete can still restate the deleted memory.
- The raw-turn profile row (`instance_id IS NULL`, `category IS NULL`) that
  holds the user's utterance verbatim. It is not exported, not listed and not
  deletable here; it can still be recalled.
- Relationship-layer rows (episodes, arcs, journal, raw lines): no list, no
  delete, no entry in the stream.
- Deletions that already happen without a trace: a profile memory that
  cascades away with a hard-deleted `chat_sessions` row is still not
  reported.
- A service-secret variant of the delete routes.

**Anti-goals**

- A profile that keeps stating a deleted fact because a rebuild that had
  read the ledger before the delete finished after it (§5.2).
- A new column that points at an external identity. The tombstone carries
  no `user_id`.

## 3. Owner routes

All four sit in the JWT tree beside `GET /comp/user/{user_id}/profile`, with
its rule: the path's `user_id` must be the token's, else `403 forbidden`.

### 3.1 Lists

```
GET /v2/comp/user/{user_id}/facts?limit=&before=&before_id=
GET /v2/comp/user/{user_id}/memories?limit=&before=&before_id=
```

Newest first, by a timestamp that never changes for a row, so a walk through
the pages neither repeats nor skips a row that exists for the whole walk.
`limit` defaults to 100, clamped to 1..=500. `before` (RFC 3339) and
`before_id` (uuid) come together and are the `next` of the previous page;
absent, the list starts at the newest row. One without the other is
`400 invalid_payload`.

- **Facts:** every `engine.human_facts` row of the user, the rows
  `facts/changes` exports, with that route's fields minus `user_id`: `id`,
  `content`, `category`, `domain`, `evidence_type`, `temporality`,
  `persistence`, `confidence`, `occurrences`, `session_count`, `recurrence`,
  `first_seen_at`, `last_seen_at`. Ordered by `(first_seen_at, id)`
  descending. Response `{ "facts": [...], "next": {"first_seen_at","id"} | null }`.
- **Memories:** every `engine.companion_memories` row of the user with
  `instance_id IS NULL AND category IS NOT NULL`, the rows `memories/changes`
  exports, with that route's fields minus `user_id`: `id`, `content`,
  `category`, `metadata`, `created_at`. Ordered by `(created_at, id)`
  descending. Response `{ "memories": [...], "next": {"created_at","id"} | null }`.

`next` is non-null exactly when the page is full, as on the export streams.

### 3.2 Deletes

```
DELETE /v2/comp/user/{user_id}/facts/{fact_id}
DELETE /v2/comp/user/{user_id}/memories/{memory_id}
```

`204` when the row was deleted. `404 not_found` when no such row belongs to
the user: an unknown id, another user's row, a row already deleted, or (for
memories) a row that is not an extracted profile memory. A repeat of a
successful delete is therefore `404`.

One transaction deletes the row and writes its tombstone (§4). For a fact
the same transaction also hands the profile to the next rebuild (§5). A
`404` rolls the whole transaction back.

## 4. Tombstones and the deletion stream

### 4.1 Table (migration `0084`)

```sql
CREATE TABLE engine.row_deletions (
    kind       TEXT        NOT NULL CHECK (kind IN ('memory', 'fact')),
    id         UUID        NOT NULL,
    deleted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, id)
);
CREATE INDEX idx_row_deletions_export ON engine.row_deletions (deleted_at, id);
```

Plus the REVOKE and row-level-security block every `engine.*` table carries
(as in `0079`). `id` is the id the deleted row had; the row is gone, so there
is nothing to reference and no foreign key. No `user_id`: the row holds ids
only, and `engine.*` gains no new column pointing at an external identity.
User erasure leaves this table alone for the same reason: it has no user to
select by and nothing of the user's in it.

### 4.2 Stream

```
GET /svc/v1/rows/deletions?since=<rfc3339>&after_id=<uuid>&limit=<n>
```

Shaped like the existing change streams: `since` defaults to the epoch,
`after_id` to the nil uuid, `limit` to 500 (1..=1000); rows ordered by
`(deleted_at, id)` ascending and strictly after the cursor; `next` is the
last row's `{deleted_at, id}` when the page is full, else `null`.

```json
{ "deletions": [ { "kind": "fact", "id": "…", "deleted_at": "…" } ],
  "next": { "deleted_at": "…", "id": "…" } }
```

`kind` is `memory` (a row `memories/changes` exported) or `fact` (a row
`facts/changes` exported). A mirror skips a `kind` it does not know.
`deleted_at` is the deleting transaction's start, so a late commit can land
behind a cursor; the overlap the docs already advise for the other streams
applies here too.

The existing `insights/deletions` and `facts/deletions` routes stay what they
are, the user-erasure stream; this one has its own name so neither changes
meaning.

**Admission.** The rows carry no content, so either read secret opens the
route: `x-insights-export-secret` matching `INSIGHTS_EXPORT_SECRET`, or
`x-facts-export-secret` matching `FACTS_EXPORT_SECRET`. The route is mounted
when at least one of the two is configured, and a header is only ever
compared with its own secret. As everywhere under `/svc/v1`, admission comes
before any query error, and a refusal is `403`.

The two export routes' descriptions (`memories/changes`: "carries no
deletions"; `facts/changes`: rows leave only through an erase) are rewritten
to point at this stream; they feed the OpenAPI snapshot.

## 5. The profile after a fact is deleted

### 5.1 The next sweeper tick rebuilds it

A delete does not call the model. It marks the user's profile due, and the
existing sweeper rebuilds it on its next tick (every 5 minutes by default),
ahead of the users who are merely idle. Several deletes in one tick cost one
rebuild.

Migration `0084` adds the mark:

```sql
ALTER TABLE engine.human_profile_state
    ADD COLUMN rebuild_due BOOLEAN NOT NULL DEFAULT false;
```

- The sweeper's `claim_due` takes a user that has pending facts and is
  either idle (as today) or `rebuild_due`, due users first.
- A successful settle clears `rebuild_due`, as it zeroes `pending_facts`.
  The failure that reaches `MAX_REBUILD_FAILURES` clears it too, with the
  count it drops.
- On a deployment whose sweeper is off, the mark waits for the next
  count-triggered rebuild.

### 5.2 An in-flight rebuild must not have the last word

A rebuild that read the ledger before the delete must neither write its
profile nor settle the state row afterwards. Today a claim only proves that
*someone* holds the user: `replace_profile_if_claimed` checks
`claimed_at IS NOT NULL`, and `finish_ok` / `finish_failed` update the row
unconditionally. So a rebuild that outlived its claim can write over, and
settle away, work that belongs to a newer claim.

The claim becomes an ownership token, as the world system's already is:

- `claim`, `claim_due` and `refresh_claim` return the `claimed_at` they
  wrote; `refresh_claim` takes the token it refreshes and returns the new
  one.
- `replace_profile_if_claimed` (and the cleared write of §5.4), `finish_ok`
  and `finish_failed` take the token and act only where
  `claimed_at = token`. A rebuild whose token no longer matches writes
  nothing and settles nothing.

The fact-delete transaction then works on the user's state row **first**,
before it touches the ledger (the lock order user erasure uses):

```
INSERT the state row (user_id) ON CONFLICT DO UPDATE SET
    claimed_at = NULL,              -- any in-flight rebuild loses its token
    pending_facts = pending_facts + 1,
    last_fact_at = now(),
    rebuild_failures = 0,
    rebuild_due = true
```

An upsert, because a user with facts is not guaranteed a state row (the
ledger write and the counter bump are separate statements), and it locks the
row whether or not it existed. Then the fact is deleted (`404` if none: the
whole transaction rolls back, leaving no state row behind), then the
tombstone is written.

### 5.3 Memories

Deleting a profile memory needs nothing more: recall reads the table and no
derived row is built from it (the world director's past output is a
non-goal).

### 5.4 When the ledger supports no profile

Today three rebuild outcomes leave the existing `human_insights` row
untouched: an empty ledger, a ledger of nothing but transient facts that
never recurred across sessions (no model call), and a parsed reply whose
projection is empty (after the write-time guards: a roleplay-only ledger
keeps only `kinks`, a ledger with no health fact drops `mental_health`). In
all three the same ledger always gives the same answer, so after a delete
the old row would keep stating the deleted fact for good.

On those three outcomes, when the user has a `human_insights` row, the
rebuild now **writes an empty profile over it**: every profile field null or
an empty array, `updated_at` advanced, through the token-guarded write. The
row stays, so `GET /svc/v1/insights/changes` carries the cleared profile to
a mirror; a deleted row would simply stop appearing. A user with no row gets
none. The outcome is still `Empty`, and its audit row as today.

Outcomes that are not a statement about the ledger keep the old row: the
model call failed, or its reply did not parse.

This is a rule about the ledger, not about deletion: it also applies to
users who never deleted anything. A production deployment has profiles
whose ledger is already empty or transient-only (left by the migration
backfill, or written by a model before the transient short-circuit existed);
each is cleared on that user's next rebuild. The release notes say so.

A model glitch that returns `{}` for a rich ledger now clears that user's
profile until the next rebuild, which the next new fact brings.

## 6. How it is built

The shape, not the code:

- **Store:** list and delete functions on `HumanFactRepo` and `MemoryRepo`,
  each delete taking the user id and returning whether a row went; the fact
  delete is one function that runs the whole transaction of §5.2. A small
  repo for `row_deletions` (insert inside a transaction, keyset list).
  `ProfileStateRepo` and `HumanInsightRepo` gain the token parameters of
  §5.2 and the `rebuild_due` handling of §5.1; `HumanInsightRepo` gains the
  cleared write of §5.4, which writes only over an existing row.
- **Rebuild:** `rebuild_user` carries the token from its caller (the count
  trigger in `post_process.rs`, the sweeper's batch) through the write and
  the settle; the three outcomes of §5.4 clear the profile.
- **Routes:** the four owner routes in the JWT tree, declared with
  `#[utoipa::path]` and `ToSchema` DTOs first; the stream route in
  `routes/svc.rs` beside the others, with its two-secret admission and its
  own mount rule.
- **Schema test:** a `migration_0084` shape test asserts the new table, its
  RLS, and the new column.
- **OpenAPI:** the snapshot is regenerated.

Left to the builder: file placement of the owner routes, DTO names, whether
the two lists share a paging helper, and test layout. Tests cover:

- each list's order, paging, the `400` for half a cursor, and the ownership
  check;
- each delete's `204`, its `404` cases, that it only removes the caller's
  exported kind of row, and the tombstone written in the same transaction;
- a fact delete for a user with no state row creating one, due;
- the stream's paging, both secrets, each secret alone, and its absence when
  neither is set;
- a deleted fact gone from `facts/changes` and from the next rebuilt
  profile;
- the sweeper taking a due user before the idle window;
- a rebuild that read the ledger before a delete and finishes after it,
  with another claim taken in between: it writes nothing and settles
  nothing, and the newer claim's rebuild writes the profile without the
  fact;
- the three clearing outcomes leaving an empty row with a newer
  `updated_at`, and a user with no row still having none;
- a failed model call keeping the old row;
- a restated fact coming back under a new id.

## 7. Docs and release

- Docs go in a separate PR after the code PR, English and Chinese:
  `docs/api-reference.md` (the owner routes; the service section, where "No
  deletions in this feed" and "There is no per-fact deletion" stop being
  true, with the overlap advice for the new stream and a line that a mirror
  should keep tombstoned ids, since a changes page fetched before a delete
  can arrive after its tombstone), `docs/insights.md` (exports and
  deletions, the rebuild outcomes of §5.4, known limits: what a fact's
  deletion does not reach), `docs/memory-layers.md` (which profile rows can
  be deleted, and that the raw-turn row is not one of them).
- A short migrating guide for mirrors (English only, as the other migrating
  guides): the two changes streams can now lose rows, and the new stream is
  how a mirror learns of it. It also states the §5.4 behaviour change.
- The release carries a migration; the deploy order is the usual one
  (migrate, then the image).

## 8. Decisions

Made by the owner:

- The list and delete routes are owner routes in the JWT tree.
- Deleting a fact deletes the row and rebuilds the profile; the extraction
  audit and the profile snapshots are left to user erasure.
- Only the exported kind of memory row is listed and deletable; the
  raw-turn row and the relationship layer are for a later change.
- An emptied ledger clears the profile; so does a ledger whose projection is
  empty. The rule applies to every user, including existing profiles the
  ledger no longer supports.
- After a delete the sweeper's next tick rebuilds the profile; the delete
  does not call the model.

Made in this design:

- The cleared profile is an empty row with a new `updated_at`, not a
  deleted row, so mirrors see it (§5.4).
- The claim becomes an ownership token (§5.2).
- One stream, `/svc/v1/rows/deletions`, opened by either read secret
  (§4.2).
- The tombstone has no `user_id` and user erasure does not touch it (§4.1).
- A delete resets the rebuild failure streak (§5.2).
- Lists are newest first by an immutable timestamp; a repeated delete is
  `404` (§3).
