# Affinity 4.1 — tiers in the row, replayable events, a public value endpoint — Design

- **Date:** 2026-08-17
- **Status:** Approved
- **Type:** Engine change — one schema migration (two tier columns, two event
  audit columns, one legacy column dropped), one new BFF endpoint, removal of
  the env-gated debug router. Scoring math unchanged.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` 1.4.0 proposed — the change is breaking (two routes
  and one column removed), so it is not a patch. The release number and its
  timing are the owner's call, not this document's.
- **Amends:** [2026-08-16-affinity-40-design.md](2026-08-16-affinity-40-design.md)
  (storage and read surface only — the 4.0 write pipeline, judge contract and
  endpoint derivation are untouched). Retires the debug surface introduced in
  [2026-05-20-affinity-event-delta-design.md](2026-05-20-affinity-event-delta-design.md)
  §3 and the gating decision in
  [2026-05-20-history-latency-cuts-design.md](2026-05-20-history-latency-cuts-design.md)
  §3.5.

## 1. Motivation

A downstream client cannot get an absolute affinity reading out of this engine
through a supported route. That single gap causes everything below.

**The only absolute-value endpoint is gated and labelled debug.**
`GET /comp/affinity/{sid}` returns the full snapshot, but it is registered only
when `EXPOSE_AFFINITY_DEBUG=true`. The BFF surface a client is supposed to build
on — `GET /bff/v1/comp/affinity/{sid}/event` — carries **deltas only**: per-axis
`effective_deltas`, per-line `effective_deltas_computed`, and a tier transition
in `label_changes`. There is no starting point anywhere in it.

**So clients read the table directly and re-derive.** With no endpoint to call,
a client reads `engine.companion_affinity` through a privileged connection to
get the scores, then applies **its own copy of the tier thresholds** to name the
tier — because the thresholds live only in `eros_engine_core::affinity`
(`TIER1_HI`…`TIER4_HI`), and nothing in the database or the API exposes the
resulting tier. Two copies of a rule that decides what the user sees. They will
drift.

**The direct read is also wrong after an absence.** As of 4.0 `warmth` and
`patience` are derived endpoints: the authoritative value is
`refresh_endpoints()` evaluated at read time against the elapsed gap, and the
stored column is a write-time cache. `apply_time_decay()` (the intrigue/tension
drift) is likewise applied on the write path only. A reader that selects the
columns gets a snapshot from the last turn, systematically warm.

**The event log cannot be replayed.** Two holes. Events carry no absolute state
at all, so reconstruction means summing deltas from the beginning of time — and
that sum does not converge on the current row, because `apply_time_decay()`
runs *before* the baseline snapshot inside `persist_with_event`, so the drift it
applies never appears in `effective_deltas`. It is invisible in the audit trail.
(Separately: `event_type = 'time_decay'` is permitted by the CHECK and filtered
by both readers, but nothing in the workspace ever writes it. Dead value.)

## 2. Design principles applied

1. Every legacy env-gated absolute-value endpoint is deleted, not un-gated.
2. `bond_tier` / `chem_tier` are computed by the engine and stored; the column
   is the engine's authoritative result projected for SQL consumers.
3. The event audit table carries enough state to replay a relationship.
4. The BFF event endpoint carries per-line deltas.
5. The new absolute-value endpoint lives on BFF, consistent with the event
   endpoint it sits beside.

## 3. Schema — migration `0049_affinity_tiers_and_event_state.sql`

### 3.1 Tier columns

```sql
ALTER TABLE engine.companion_affinity
    ADD COLUMN bond_tier SMALLINT NOT NULL DEFAULT 1,
    ADD COLUMN chem_tier SMALLINT NOT NULL DEFAULT 1;
```

Plain columns, written by the engine — **not** generated columns.

PostgreSQL forbids a generation expression from referencing another generated
column (`ERROR: cannot use generated column "bond" in column generation
expression`), and `bond` / `chemistry` are already generated. A generated tier
would therefore have to inline the line formula a second time inside its own
expression, putting the thresholds in SQL as well as in Rust — two authorities
for the rule that decides what the user sees, which is the defect this design
exists to remove. It would also inherit PostgreSQL's rule that changing a
generation expression's function body does not recompute existing rows.

Writing from Rust keeps `tier_index()` the single authority. `DEFAULT 1` is
correct rather than arbitrary: a fresh row has every line axis at 0, so both
lines score 0, which is tier 1.

**No CHECK constraint.** The value is produced by engine code from a value the
engine already holds; a range check would be a defensive assertion against our
own writer. Its absence is also what makes the tier ladder extensible by
formula alone: adding a sixth tier is a change to `tier_index()` and a backfill,
with no change to the table's shape. (Contrast `warmth_grade` / `patience_grade`
in 0048, which do carry CHECKs — those originate in an LLM response, where
validation earns its keep.)

Backfill, one-shot, using the 4.1 thresholds inline; no helper function is
created, so nothing outlives the migration:

```sql
UPDATE engine.companion_affinity SET
    bond_tier = CASE WHEN bond      < 0.15 THEN 1 WHEN bond      < 0.35 THEN 2
                     WHEN bond      < 0.62 THEN 3 WHEN bond      < 0.90 THEN 4
                     ELSE 5 END,
    chem_tier = CASE WHEN chemistry < 0.15 THEN 1 WHEN chemistry < 0.35 THEN 2
                     WHEN chemistry < 0.62 THEN 3 WHEN chemistry < 0.90 THEN 4
                     ELSE 5 END;
```

### 3.2 Event state snapshots

```sql
ALTER TABLE engine.companion_affinity_events
    ADD COLUMN state_before JSONB,
    ADD COLUMN state_after  JSONB;
```

Same shape in both:

```jsonc
{ "warmth": 0.31, "trust": 0.44, "intrigue": 0.40, "intimacy": 0.19,
  "patience": 0.27, "tension": 0.17,
  "bond": 0.42, "chemistry": 0.18,
  "bond_tier": 3, "chem_tier": 2,
  "warmth_grade": 2, "patience_grade": 2,
  "ghost_streak": 0, "total_ghosts": 2,
  "updated_at": "2026-08-17T14:02:11.412Z" }
```

Each row becomes self-contained, and that closes both replay holes:

- `state_after − state_before == effective_deltas`, so a row validates itself.
- The gap between the **previous** row's `state_after` and **this** row's
  `state_before` is exactly the absence effect applied at the head of
  `persist_with_event` — `apply_time_decay()` on the line axes plus
  `refresh_endpoints()` at the old judge levels and the new decay factor.
  Previously unrecorded; now explicit and attributable without inference.

Storing `bond` / `chemistry` / the two tiers inside the snapshot duplicates
values derivable from the axes, and that is deliberate. An audit row is an
immutable point-in-time record: it must say what the tier *was under the
thresholds in force at the time*, which a later re-derivation cannot reproduce
once the thresholds move. Live state remains single-sourced — the authority for
"where is this relationship now" is `companion_affinity` and nothing else.

`record_ghost` writes both columns too (no axis moves, so `before == after`);
leaving them NULL there would put a hole back in the chain. It snapshots the
row it just wrote (`UPDATE … RETURNING *`), **never the caller's `Affinity`** —
stream callers decay and refresh their copy in memory at turn start without
writing those axes back, so recording that copy would put values in the audit
row that never existed in the table and make the relationship appear to rebound
on the following turn. Pre-migration rows stay NULL and are not backfilled — the
values are not recoverable, and fabricating them would defeat the point of the
columns.

`updated_at` and the ghost counters are in the snapshot for a reason that only
shows up on the ghost path. A ghost moves no axis, so without them its two
snapshots would be byte-identical and would read as "nothing happened" — while
the operation has in fact reset the decay clock, silently forgiving however much
absence had accrued. An audit row that asserts no change across a real change is
worse than one that says nothing. `updated_at` is taken from the row (`RETURNING`)
rather than from `apply_deltas`' Rust-side `now()`, since it is the baseline a
replay measures the next gap from.

The clock reset itself is left alone: `record_ghost` still does not materialise
the elapsed decay, so a ghosted turn forgives the absence. That is 4.0 behaviour
and changing it is a scoring decision, not a storage one — but it is now visible
in the log instead of hidden.

`record_ghost` also becomes row-locked (`SELECT … FOR UPDATE`, then `UPDATE …
RETURNING *`, in one transaction) and increments the counters in SQL rather than
from the caller's in-memory value, which closes a lost-update race between two
concurrent ghosts. It writes the persisted row back to the caller on the way out,
the same contract `persist_with_event` already had — a caller left holding
decayed axes and a stale `updated_at` is how a later `persist_with_event` on the
same struct would double-count decay.

### 3.3 Legacy column dropped

```sql
ALTER TABLE engine.companion_affinity DROP COLUMN relationship_label;
```

The engine already does not read it. `AffinitySnapshot` derives its
`relationship_label` field from `legacy_relationship_label()` at read time
rather than from the column, and `pipeline/voice.rs` documents explicitly that
the relationship line must come from the bond/chemistry tiers and "never from
the cached `relationship_label`". Only the write in `persist_with_event` keeps
it alive. See §8 for the deployment ordering this drop requires.

## 4. `eros-engine-core`

- `tier_index(score: f64) -> u8` becomes `pub` — it is now the sole authority
  for a value other layers store and render.
- `Affinity::bond_tier() -> u8`, `Affinity::chem_tier() -> u8`.
- `BondLabel::from_tier(u8)`, `BondLabel::from_score(f64)`, and the same pair on
  `ChemistryLabel`, so a caller holding only a score can name the tier without
  reaching for the thresholds.
- **Removed:** `RelationshipLabel`, `Affinity::relationship_label`,
  `Affinity::legacy_relationship_label()`. No survivors, no deprecation shim —
  the enum's last consumer disappears in this change, and a type kept only for
  parse compatibility with a column that no longer exists is a tombstone.

## 5. `eros-engine-store`

- `AffinityRow`: drop `relationship_label`. **Do not add the tier columns.**
  `load()` uses `SELECT *`, and sqlx's derived `FromRow` ignores columns absent
  from the struct, so this is a removal only. Engine code always holds the score
  and derives the tier through §4; reading back its own projection would be a
  round-trip for nothing and would expose it to rows written by an older engine.
  The tier columns exist for SQL consumers that cannot call Rust.
- `label_to_str` / `label_from_str`: removed with the enum. `to_domain` stays —
  it never touched the label beyond mapping that one field.
- `persist_with_event`: drop the label computation and the
  `relationship_label = $10` bind; add `bond_tier` / `chem_tier` to the UPDATE;
  build `state_before` from `before_affinity` and `state_after` from `current`
  and bind both on the event INSERT.
- `record_ghost`: bind `state_before` / `state_after` from the row (identical).
- `AffinityEventRow`: add `state_before` / `state_after`; add both to the
  `SELECT` lists in `list_events` and `latest_turn_event`.
- `StoryRepo::affinity_snapshot`: `SELECT ca.relationship_label` is removed. The
  query already selects `ca.bond` and `ca.chemistry`; `StoryAffinity` drops
  `relationship_label` and gains nothing, with `pipeline/story.rs` naming the
  two tiers through `BondLabel::from_score` / `ChemistryLabel::from_score`.

The World Stories prompt slot changes from

```jsonc
"relationship_label": "friend"
```

to

```jsonc
"bond_label": "close_friend", "chemistry_label": "flirtation"
```

Two named tiers carry more than one of four legacy names, they match what the
user is shown, and they are discrete absolute labels rather than a continuous
reading — which is what an LLM can actually use.

## 6. `eros-engine-server`

### 6.1 Removals

`routes/debug.rs` is deleted whole: both handlers, the `#[deprecated]`
`AffinityDebugResponse` alias, and the module's tests. With it goes the entire
`EXPOSE_AFFINITY_DEBUG` chain — `.env.example`, `Config::expose_affinity_debug`
and its parse in `state.rs`, both `debug::router(...)` merges in `routes/mod.rs`,
the `expose_affinity_debug` parameter of `router_for_openapi`, the
`debug_affinity` field on the startup log line and the comment above the OpenAPI
dump in `main.rs`, and the field in the `companion.rs` test state. The engine is
left with no env-gated routes.

`AffinitySnapshot` (`routes/dto.rs`) survives the deletion of its only current
caller and becomes the new endpoint's body: `relationship_label` is removed,
`bond_tier` / `chem_tier` are added.

### 6.2 New endpoint

`GET /bff/v1/comp/affinity/{session_id}` in `routes/bff/affinity.rs`, beside the
event endpoint — same module, same `bff-companion` tag, same auth, ungated.

```jsonc
{
  "session_id": "…",
  "affinity": null | {
    "bond": 0.4213, "chemistry": 0.1802,
    "bond_tier": 3,  "chem_tier": 2,
    "bond_label": "close_friend", "chemistry_label": "flirtation",
    "warmth": 0.3106, "trust": 0.4402, "intrigue": 0.4024,
    "intimacy": 0.1901, "patience": 0.2740, "tension": 0.1703,
    "ghost_streak": 0, "total_ghosts": 2,
    "updated_at": "2026-08-17T14:02:11Z"
  }
}
```

`affinity` is `null` when the session has no affinity row yet — the row is
created on the first turn by `load_or_create`, so a freshly started session
legitimately has none.

The handler runs `apply_time_decay()` then `refresh_endpoints()` before
serialising. **That is the entire reason this endpoint exists** rather than
letting clients read the table: it returns the value as of *now*, not as of the
last write.

Both the tier number and the label key are returned. The number drives level
indicators, the key is an i18n lookup. A client needs neither a threshold table
nor an ordered tier array.

### 6.3 Event endpoint gains the post-turn state

`BffAffinityDelta` gains `state_after` (omitted when NULL, i.e. pre-migration
rows). A client that has the absolute value from §6.2 at mount then receives an
authoritative absolute value with every turn, and never accumulates deltas at
all. `effective_deltas` and `effective_deltas_computed` are unchanged and remain
the right input for per-turn animation.

The two absolutes differ and the difference is load-bearing: `state_after` is a
**write-time** snapshot, `GET /bff/v1/comp/affinity/{sid}` is **refreshed at
read**. Immediately after a turn they agree. After an absence only the endpoint
is correct.

`effective_deltas_computed` keeps its awkward name. It is a published field; the
rename would cost downstream work and buy nothing.

## 7. Authorisation

Two layers, identical to the event endpoint:

1. **JWT** — the whole `/bff/*` subtree sits under the `require_auth` layer
   applied to the merged `comp` router in `routes/mod.rs`, yielding
   `AuthUser(user_id)`.
2. **Ownership** — the handler's first statement is
   `require_session_for_user(&state, session_id, user_id).await?`: unknown
   session → 404, session owned by someone else → **403**.

This is deliberately *not* the deleted handler's scheme, which loaded the
affinity row and compared `a.user_id`, reporting 404 when no row existed. Using
the shared helper keeps both BFF affinity endpoints on one status-code contract.

No redundant `AND user_id = $1` filter is applied to the affinity row.
`companion_affinity.session_id` is UNIQUE and session ownership is already
established, so the extra predicate would only assert a schema invariant against
ourselves.

**Known trade-off:** distinct 403 and 404 responses form an existence oracle for
session IDs. This is accepted. Session IDs are v4 UUIDs — 122 bits of entropy,
not enumerable — and the alternative (collapsing both into a null body) would
put the two neighbouring BFF affinity endpoints on different error contracts,
which is the more likely source of a real bug.

## 8. Deployment ordering — mandatory

`DROP COLUMN relationship_label` breaks every reader of that column the instant
the migration runs. Downstream SQL that selects it (list views, aggregate
queries, privileged direct reads) fails immediately and for every user, with no
gradual rollout available.

The replacements land in the *same* migration, so this cannot be done in two
deploys — a downstream change that drops the old column and adopts the new ones
at once is broken whichever side it ships on. Three deploys:

1. **Downstream, before the engine.** Stop selecting `relationship_label` and
   stop rendering it. Nothing new is adopted here: `bond_tier` / `chem_tier` and
   the value endpoint do not exist yet. Whatever local tier derivation the
   client already runs off `bond` / `chemistry` keeps working and stays for now.
2. **The engine.** Migration 0049 runs: tier columns and event state columns
   added, `relationship_label` dropped.
3. **Downstream, after the engine.** Adopt `bond_tier` / `chem_tier`, switch to
   `GET /bff/v1/comp/affinity/{sid}`, consume `state_after`, and delete the
   local threshold copy.

Both inversions are outages: doing step 3's work in step 1 selects columns that
do not exist yet; doing step 1's work in step 3 leaves a reader on a dropped
column.

`docs/migrating/affinity-4-0-v1-3-1.md` is the downstream-facing instruction set.

## 9. Testing

- **Threshold agreement.** Persist turns landing on `0.0`, `0.149999`, `0.15`,
  `0.349999`, `0.35`, `0.619999`, `0.62`, `0.899999`, `0.90`, `1.0` and assert
  the stored `bond_tier` equals `Affinity::bond_tier()` at each, with the two
  lines on different scores so a bind-order mix-up cannot pass. This pins the
  post-delta capture and the i16 round-trip; it does NOT prove single-sourcing,
  since both sides call the same `tier_index`.
- **Backfill agreement.** The migration's inlined `CASE` is the one place the
  thresholds genuinely exist twice. Run that expression in Postgres over the
  same boundary set and demand it equal `tier_index`. This is the test that
  fails when a threshold moves in Rust alone.
- **Absence gap + ghost provenance.** Backdate the row, decay a copy in memory
  the way the stream path does, and assert `record_ghost` recorded the row's
  values and not the copy's. This is the regression guard for the trap in §3.2.
- **Replay.** For a multi-turn session: every event satisfies
  `state_after − state_before == effective_deltas` per axis; adjacent pairs
  written with no elapsed gap satisfy
  `previous.state_after == current.state_before`, and where a gap exists the
  difference reproduces from `context.decay_factor` and the elapsed interval
  through `apply_time_decay` + `refresh_endpoints`. A ghost event has
  `state_before == state_after`.
- **Value endpoint.** `affinity: null` on a session with no row; a row whose
  `updated_at` is backdated returns a `warmth` strictly below the stored column
  (proving the read-time refresh); 401 without a bearer; 403 on another user's
  session; 404 on an unknown session.
- **Event endpoint.** `state_after` present on new rows, omitted on rows written
  with NULL.
- **Removal.** `/comp/affinity/{sid}` and `/comp/affinity/{sid}/event` both 404.
- Tests referencing `Config::expose_affinity_debug` are deleted, not adapted.
- OpenAPI snapshot regenerated.

## 10. Breaking changes to the published crates

`eros-engine-core` and `eros-engine-store` are published libraries, and this
change breaks their source API in three ways. All are deliberate; the release is
breaking regardless (§ header).

- `RelationshipLabel`, `Affinity::relationship_label` and
  `Affinity::legacy_relationship_label()` are removed outright. No deprecation
  shim: the enum's last consumer disappears here, and a type kept only to parse a
  column that no longer exists is a tombstone.
- `AffinityRow::relationship_label` and `StoryAffinity::relationship_label` are
  removed — both mirror dropped columns.
- `AffinityEventRow` gains `state_before` / `state_after`. Adding public fields
  breaks any downstream literal construction of the struct. It is populated by
  sqlx in every in-workspace use, but the break is real and is listed here rather
  than discovered at upgrade time.

## 11. Non-goals

- **The 4.0 scoring math is untouched.** No threshold, unit, decay rate or
  penalty parameter moves in this change. Tier labels churn only where the
  backfill corrects a row that was already mislabelled by a downstream copy of
  the thresholds.
- **`event_type = 'time_decay'` stays in the CHECK and stays unwritten.**
  Removing an unused enumerand from a constraint is churn with no reader.
- **No `label_changes` shape change.** It carries tier keys; with `state_after`
  present the client has the numeric tier without it.
- **No client-side caching or long-poll on the value endpoint.** The event
  endpoint's long-poll already covers the live case; the value endpoint is for
  mount and re-sync.
