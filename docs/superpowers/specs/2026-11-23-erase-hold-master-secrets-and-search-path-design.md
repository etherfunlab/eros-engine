# An erase that holds, two master secrets, and pinned search paths — Design

- **Date:** 2026-11-23
- **Status:** Approved
- **Type:** Engine change. One migration (two `ALTER FUNCTION`). Two new
  environment variables. No wire change: no new route, header or field.
- **Owner:** enriquephl (sole dev)
- **Issues:** #545, #555
- **Builds on:** `2026-10-26-insights-2-fact-ledger-design.md` (the fact
  ledger, the erasure tombstone), `2026-08-18-session-soft-delete-design.md`
  (`lock_live_session`), `2026-10-27-caller-secret-gate-design.md`,
  `2026-10-08-byok-chat-design.md`, `2026-10-16-byoc-design.md`,
  `2026-11-20-together-actions-state-in-picture-and-genome-routes-design.md`
  (the genome-write sub-tree).

## 1. Intent

Three independent parts, shipped one after another.

- **A. Pinned search paths (#545).** Two engine functions run with the
  caller's `search_path`, which a hosted database's linter flags.
- **B. Master secrets.** An operator sets one variable instead of seven, and
  can hand out a key that does everything except erase a user.
- **C. An erase that holds (#555).** A deployment that erases a user and keeps
  the chat history gets the erased data back: the backfill re-reads the
  history, a write in flight lands after the erase, a new turn starts the
  counters again. After this change the erasure tombstone means the user is
  gone, and the engine derives nothing more about them.

## 2. Goals, non-goals, anti-goals

**Goals.**

- A: both functions carry `search_path = ''`; their results do not change.
- B: `SUPER_SECRET` is accepted wherever any feature secret is, and turns on
  every secret-gated feature by itself. `ADMIN_SECRET` does the same for
  everything except the user-erase sub-tree.
- C: once a user is in `engine.user_erasures`, no path writes a row that an
  erase would delete, whenever it runs: seconds later or weeks later.

**Non-goals.**

- B: a new header; several comma-separated master keys; replacing
  `SUPABASE_JWT_SECRET`, which verifies tokens and admits nobody.
- C: refusing an erased user's requests (a turn still gets its reply);
  an endpoint that undoes an erasure; erasing chat sessions or messages;
  gating the world and story chains, which hang off `world_enrollments`, a
  table the deployment writes and the erase deletes.

**Anti-goals.**

- B: a master key switching the caller-secret gate on. That gate restricts;
  a deployment that sets a master key must not lock out its own clients.
- B: `ADMIN_SECRET` reaching the erase route by any path, including being set
  to the same value as `SUPER_SECRET`.
- C: a database round trip added to the request path; a second stored copy of
  "this user is erased" beside the tombstone.

## 3. Approaches

**A.** One way: a new migration with two `ALTER FUNCTION … SET search_path`.
Editing `0000_baseline.sql` or `0079` is out: applied migrations are frozen.

**B.**

1. *Resolve at boot into each feature's key list* (chosen). Every gated
   feature already asks "does the sent value equal one of my keys"; the
   caller-secret gate holds a list today. Boot builds each feature's list
   from its own secret plus the masters that apply. One rule, in one place.
2. *A check for the masters at each admission site.* Seven sites each learn
   the rule; the erase exception is repeated by hand.
3. *A dedicated `x-super-secret` header.* A caller would send a different
   header per key type, so a master key would not be a drop-in for a feature
   secret.

**C.**

1. *The tombstone gates the deriving writes* (chosen). A write for an erased
   user is dropped, by a predicate inside the statement that already runs.
2. *The erase archives the user's sessions.* Reuses the archive guard, but
   the erase starts changing sessions it documents as the caller's, a new
   session derives again, and the fact ledger checks no session.
3. *A line at `erased_at`.* Nothing said before it is derived again; a user
   who keeps chatting starts fresh. Every write path would need its source
   message's time.
4. *Documentation only.* The fact ledger stays open whatever the caller does.

## 4. Part A — search paths

Migration `0087_function_search_path.sql`:

```sql
ALTER FUNCTION engine.touch_world_worldviews() SET search_path = '';
ALTER FUNCTION engine.fact_norm(text) SET search_path = '';
```

Both bodies call only `pg_catalog` functions, which resolve under an empty
path. `fact_norm` stays `IMMUTABLE STRICT`; the generated column
`human_facts.content_hash` is not rewritten. A `SET` clause stops the planner
inlining `fact_norm`, which costs a function call per fact in the batch dedup
of `HumanFactRepo::upsert_run`: a handful of rows per run.

Proof: a store test reads `pg_proc.proconfig` for both functions; the
existing `fact_norm` tests cover its results.

## 5. Part B — master secrets

| Variable | Stands in for | Turns on by itself |
|---|---|---|
| `SUPER_SECRET` | every feature secret | every feature in the table below |
| `ADMIN_SECRET` | every feature secret except `USER_ERASE_SECRET` | the same, except user erase |

| Feature secret | Header | Off means |
|---|---|---|
| `CALLER_SECRET` | `x-caller-secret` | the gate is open |
| `BYOK_CALLER_SECRET` | `x-byok-caller-secret` | BYOK refused |
| `BYOC_CALLER_SECRET` | `x-byoc-caller-secret` | BYOC refused |
| `INSIGHTS_EXPORT_SECRET` | `x-insights-export-secret` | routes unmounted |
| `FACTS_EXPORT_SECRET` | `x-facts-export-secret` | routes unmounted |
| `USER_ERASE_SECRET` | `x-user-erase-secret` | route unmounted |
| `GENOME_WRITE_SECRET` | `x-genome-write-secret` | routes unmounted |

Rules:

- A master key is sent in the feature's own header, in place of its secret.
- A feature is on when its own secret is set or a master that covers it is.
  A sub-tree mounts on the same condition.
- **The caller-secret gate is the exception.** It is on only when
  `CALLER_SECRET` is set. While it is on, a master key passes it.
- Each master is one value of at least 32 bytes. Blank means unset. A shorter
  value refuses boot, naming the variable. `ADMIN_SECRET` equal to
  `SUPER_SECRET` or to `USER_ERASE_SECRET` refuses boot.
- BYOC signs its init ticket with the key the caller presented, so a ticket
  is redeemed with the same key that minted it.
- Neither value reaches a log line through `ServerConfig`'s `Debug`.
- A feature secret is still never compared with another feature's header.

OpenAPI: the security scheme descriptions say a master key is accepted. No
path, header or schema changes.

## 6. Part C — an erase that holds

### 6.1 The rule

A user with a row in `engine.user_erasures` is gone. Every write that an
erase would delete is dropped for that user. The gate is a `NOT EXISTS` on
the tombstone inside the writing statement, or in the guard the write already
runs; no statement is added to a turn.

Undoing an erasure is an operator deleting the tombstone row. The engine
offers no route for it.

### 6.2 The gates

| Gate | Where | Covers |
|---|---|---|
| A session of an erased user is not live | `session_archive::lock_live_session` | profile and relationship memories (`upsert_if_live`), `user_insights`, `character_insights`, creating an affinity row, opening and settling an activity |
| No fact for an erased user | `HumanFactRepo::upsert_run` | `human_facts` |
| No profile state for an erased user | `ProfileStateRepo::bump` | `human_profile_state`, and through it the profile rebuild and `human_insights` |
| No scan state for an erased user's relationship | `ScanStateRepo::bump`, `ScanStateRepo::ensure_for_backfill` | `relationship_scan_state`, and through it episodes, arcs and the journal, whose writes already need the state row |
| The backfill does not see an erased user's history | `BACKFILL_ROWS` | `backfill-dreaming`: instance selection and the window |

The scan-state gates reach the owner through `persona_instances.owner_uid`.
`ProfileStateRepo::bump` and `ScanStateRepo::bump` return `None` when they
wrote nothing; their callers stop there, as they do for an archived session.

What a turn of an erased user does: it is answered. Its post-processing
stops at its entry check, which reads the same predicate as the live-session
guard in place of the archive-only read it makes today, so no extraction or
embedding call is spent. No fact, memory or insight is written and no
affinity moves. The snapshot sweeper
has nothing to copy. Not gated: the turn's own audit rows
(`companion_decision_events`, the image and vision events). They record the
new turn, whose message the caller keeps anyway, and a repeat erase blanks
them.

### 6.3 The erase itself

`ErasureRepo::erase_user` runs two transactions:

1. Insert the tombstone and commit. From here every new write sees it.
2. Lock the user's `chat_sessions` rows `FOR NO KEY UPDATE`, then the deletes
   and blanks as today, then move `erased_at` forward. That lock mode
   conflicts with the guard's `FOR SHARE` and leaves message inserts alone.

The lock waits for a guarded write that passed its check before step 1
committed, so the deletes after it take what that write left. A failure in
step 2 leaves the tombstone in place: writes stay blocked, the caller sees an
error and repeats the call, which is idempotent as before. A mirror that
reads the tombstone between the two steps drops its copy early and is told
again when `erased_at` moves.

Not closed: a fact insert is one statement outside any session lock. To
survive, it would have to start before step 1 commits and still be running
when step 2 reaches `human_facts`. A repeat erase removes such a row.

### 6.4 Proof

- Each gate: a store test that the write changes nothing for an erased user
  and still writes for another user.
- Backfill: a relationship with history and no scan state is listed before
  the erase and not after; `ensure_for_backfill` creates nothing.
- Erase: a repeat call still reports 0 everywhere; a tombstone present
  without step 2 (the failure case) is completed by a repeat.
- One pipeline test: a turn's post-processing for an erased user leaves every
  derived table empty.

## 7. Documentation

Separate PR, after the code. English and Chinese where both exist.

- Erase: the route's doc comment, `docs/api-reference`, `docs/insights`
  § Erasure, `docs/deploying`. The "stop the user's traffic first, call
  again" advice goes; in its place, what the tombstone now means, that it is
  permanent, and that the audit rows of later turns need a repeat call.
- Master secrets: `.env.example`, `docs/deploying`, the secret table in
  `docs/api-reference`, any README that lists the feature secrets.
- A second v2.8.1 migrating guide: a deployment that used the erase as a
  reset now gets a user about whom nothing is derived; the two master
  variables are optional.

## 8. Delivery

Three code PRs into `dev`, in this order, then the docs PR:

1. `fix:` search paths. Fixes #545.
2. `feat:` `SUPER_SECRET` and `ADMIN_SECRET`.
3. `fix:` the erase holds. Fixes #555.

## 9. Left to the builder

- B: the type that holds a feature's resolved keys, and whether the existing
  per-feature config structs keep their `secret` field or carry the list.
- C: the exact SQL of each predicate; whether `lock_live_session` checks the
  tombstone in its locking statement or in a second one.
- Test names and placement.
