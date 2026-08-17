# Migrating to Affinity 4.1

**Applies to:** clients of `eros-engine` upgrading to the release that carries
store migration `0049` (the version number is set at release time; match on the
migration, not on a number quoted here).
**Design:** [`docs/superpowers/specs/2026-08-17-affinity-41-design.md`](../superpowers/specs/2026-08-17-affinity-41-design.md)

Affinity 4.1 changes no scoring math. It changes where the relationship tier
lives, what the audit trail records, and how a client is supposed to read an
absolute affinity value.

**If you do nothing, two things break:** any endpoint call behind
`EXPOSE_AFFINITY_DEBUG` starts returning 404, and any SQL that selects
`engine.companion_affinity.relationship_label` fails outright.

> **Read §5 before you deploy anything.** This upgrade takes **three deploys**
> in a fixed order — one on your side before the engine, the engine, then one on
> your side after. Getting the order wrong is an outage in either direction.

---

## 1. Removed

| Removed | Replacement |
| --- | --- |
| `GET /comp/affinity/{session_id}` | `GET /bff/v1/comp/affinity/{session_id}` (§2) |
| `GET /comp/affinity/{session_id}/event` | No API replacement — query `engine.companion_affinity_events` directly (§4) |
| `EXPOSE_AFFINITY_DEBUG` env var | Nothing. Remove it from your deployment config; the engine ignores it. |

Both routes were registered only when `EXPOSE_AFFINITY_DEBUG=true`. They are now
deleted rather than un-gated: the first is superseded by a supported BFF
endpoint, and the second was a debugging affordance whose data is better reached
with a direct query. The engine now has no env-gated routes at all.

## 2. New: absolute affinity endpoint

```
GET /bff/v1/comp/affinity/{session_id}
Authorization: Bearer <jwt>
```

```jsonc
{
  "session_id": "…",
  "affinity": {
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

`affinity` is `null` when the session has no affinity row yet. The row is
created on the session's first turn, so a session that has just been started
legitimately returns `null` — render an empty relationship, not an error.

**Status codes:** 401 without a valid bearer, 403 if the session belongs to
another user, 404 if the session does not exist. Same contract as
`GET /bff/v1/comp/affinity/{session_id}/event`.

### Why this is not the same as reading the table

`warmth` and `patience` are **derived** as of Affinity 4.0. Their authoritative
value is recomputed at read time from the stored judge level, the counterpart
line score, and the time elapsed since the last turn. The columns hold a
write-time cache. The line axes `intrigue` and `tension` drift with elapsed time
on the same schedule.

This endpoint applies both refreshes before responding. A privileged direct
`SELECT` does not, and returns a relationship that reads systematically warmer
than it is — the longer the user has been away, the larger the error.

**Call this endpoint on chat mount and on any re-sync.** Do not select the axis
columns for display.

## 3. Stop copying the tier thresholds

The tier boundaries (`0.15 / 0.35 / 0.62 / 0.90` over a 0–1 line score) are
owned by `eros_engine_core::affinity::tier_index`. If your client carries its
own copy to name tiers, delete it — in **deploy 3**, once the replacements below
exist (§5).

Three ways to get the tier, all authoritative:

- `bond_tier` / `chem_tier` and `bond_label` / `chemistry_label` from §2.
- `state_after.bond_tier` / `state_after.chem_tier` from the event endpoint
  (§4) — an authoritative absolute value on every turn.
- The new `bond_tier` / `chem_tier` columns on `engine.companion_affinity`, for
  SQL that cannot call the API — list views, aggregate queries. These are the
  engine's computed result, written on every turn. Being write-time values they
  carry the §2 caveat: `intrigue` and `tension` drift with elapsed time, so
  after a long absence a stored tier can sit one step above where the endpoint
  would put it. Acceptable for a list; call §2 wherever the number is the point.

The tier ladder is extensible by formula: a future release can add a tier
without changing the shape of the table or the endpoints. Do not encode "there
are exactly five tiers" as a range check.

## 4. Event endpoint: `state_after`

`GET /bff/v1/comp/affinity/{session_id}/event` keeps its shape — `after` /
`wait` long-poll parameters, `effective_deltas`, `effective_deltas_computed`
(`{bond, chemistry}`), `label_changes` — and gains one field:

```jsonc
{
  "session_id": "…",
  "event": {
    "event_id": "…",
    "event_type": "message",
    "effective_deltas": { "warmth": 0.012, "trust": 0.031, "…": 0 },
    "effective_deltas_computed": { "bond": 0.0155, "chemistry": 0.0 },
    "label_changes": { "bond": { "from": "friend", "to": "close_friend" } },
    "state_after": {
      "warmth": 0.3106, "trust": 0.4402, "intrigue": 0.4024,
      "intimacy": 0.1901, "patience": 0.2740, "tension": 0.1703,
      "bond": 0.4213, "chemistry": 0.1802,
      "bond_tier": 3, "chem_tier": 2,
      "warmth_grade": 2, "patience_grade": 2,
      "ghost_streak": 0, "total_ghosts": 2,
      "updated_at": "2026-08-17T14:02:11.412Z"
    },
    "created_at": "2026-08-17T14:02:11Z"
  }
}
```

`state_after` is omitted on events written before the upgrade.

**This removes the need to accumulate.** Previously a client had to hold a
running total and add `effective_deltas_computed` each turn, with a periodic
re-sync to correct drift. Now: take `state_after` as the new absolute value, and
use the deltas only for animating the change.

`state_after` is a **write-time** snapshot. It agrees with §2 immediately after
a turn and diverges as time passes. Use `state_after` for the turn you just
received; use §2 whenever you are re-establishing state after a gap.

**`label_changes` and `state_after` can disagree, and `state_after` is the one
to believe.** `label_changes` reports only what *this turn's delta* moved: it is
measured after absence decay has already been applied, so a tier the user lost
purely by staying away shows up as a changed `state_after.bond_tier` with no
corresponding entry in `label_changes`. Drive your displayed tier from
`state_after`; use `label_changes` for "a thing just happened" animation, and
expect it to be silent on decay-driven moves.

`effective_deltas_computed` keeps its name despite it being a poor one — it is a
published field and renaming it would cost you work for no benefit.

## 5. Schema changes — **ordering matters**

### `engine.companion_affinity`

| Change | Action |
| --- | --- |
| `relationship_label` **dropped** | **Stop selecting it before the engine upgrades.** See below. |
| `bond_tier SMALLINT NOT NULL` added | Engine-computed tier, 1-based. Safe to read. |
| `chem_tier SMALLINT NOT NULL` added | Same, for the chemistry line. |

`relationship_label` was the legacy five-name label (`stranger` / `friend` /
`slow_burn` / `romantic` / `frenemy`). The engine stopped consuming it in 4.0 —
it was derived on read from the two line scores, never from the column. Replace
it with the two tiers, which say strictly more.

> **This is the ordering constraint, and it needs three deploys, not two.**
> `DROP COLUMN` takes effect the moment the engine's migration runs, and every
> SQL statement still selecting that column fails immediately, for every user,
> with no gradual rollout. But `bond_tier` / `chem_tier` are *created* by that
> same migration — so you cannot switch to them in the same breath as you drop
> the old column.
>
> **Deploy 1 (yours, before the engine).** Stop selecting `relationship_label`
> and stop rendering it. Do **not** reach for `bond_tier` / `chem_tier` or the
> new endpoint yet — neither exists until the engine ships. Keep whatever local
> tier derivation you already have; it runs off `bond` / `chemistry`, which are
> already there. This deploy only removes a reader.
>
> **Deploy 2 (the engine).** Migration 0049 adds the tier columns and the event
> state columns, and drops `relationship_label`.
>
> **Deploy 3 (yours, after the engine).** Now adopt `bond_tier` / `chem_tier`,
> switch to `GET /bff/v1/comp/affinity/{session_id}`, consume `state_after`,
> and delete your local copy of the thresholds.
>
> Doing deploy 1's work in deploy 3's order takes down whatever surface reads
> that column; doing deploy 3's work in deploy 1's order takes down whatever
> surface reads the columns that do not exist yet. Both directions are outages.

### `engine.companion_affinity_events`

| Change | Action |
| --- | --- |
| `state_before JSONB` added | Optional. NULL on pre-upgrade rows. |
| `state_after JSONB` added | Optional. NULL on pre-upgrade rows. Also served by the event endpoint (§4). |

Both hold the same shape as `state_after` in §4. They make each event row
self-contained: `state_after − state_before` equals `effective_deltas`, and the
gap between one row's `state_after` and the next row's `state_before` is exactly
the absence effect applied at the start of that turn (line-axis drift plus the
endpoint refresh) — which previously appeared nowhere in the log and made the
trail impossible to replay.

Nothing is backfilled. Rows written before the upgrade keep NULL in both
columns; the values are not recoverable and inventing them would defeat the
purpose.

## 6. Checklist

Grouped by deploy. The grouping is the point — see §5.

**Deploy 1, before the engine ships:**

- [ ] Remove every `relationship_label` reference from SQL, views and RPCs.
- [ ] Stop rendering the legacy label. Keep your existing tier derivation off
      `bond` / `chemistry` for now; it still works and its replacement does not
      exist yet.
- [ ] Remove `EXPOSE_AFFINITY_DEBUG` from deployment config (harmless either
      way — the engine ignores it once upgraded).

**Deploy 2: the engine.** Migration 0049 runs here.

**Deploy 3, after the engine is live:**

- [ ] Replace `GET /comp/affinity/{sid}` calls with
      `GET /bff/v1/comp/affinity/{sid}`; handle `affinity: null`.
- [ ] Replace any privileged direct read of the affinity axes for display with
      that endpoint — it is the only path that refreshes the derived values.
- [ ] Switch to `bond_tier` / `chem_tier` (or the label keys) and delete the
      local copy of the tier thresholds.
- [ ] Consume `state_after` from the event endpoint instead of accumulating
      deltas.
- [ ] Replace `GET /comp/affinity/{sid}/event` usage with a direct query against
      `engine.companion_affinity_events`.
