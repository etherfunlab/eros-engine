# Migrating through the llm_generations consolidation (v1.6.1 → v1.7.2)

**Applies to:** existing deployments upgrading across any part of the
v1.6.1–v1.7.2 range — the four releases that moved per-generation `model` /
`usage` off eight child tables and into `engine.llm_generations` (store
migrations `0059`, `0060`, `0061`).
**Design:** [`docs/superpowers/specs/2026-08-24-llm-generations-audit-design.md`](../superpowers/specs/2026-08-24-llm-generations-audit-design.md) §7–§8.

**This chain is the one place in the engine's history where skipping a release
loses user writes.** Each migration in it is safe only against the code that
shipped one hop earlier, because `fly.toml` (and any deployment copying its
shape) runs `migrate` as a `release_command` — the schema changes **before**
traffic moves, so for the length of every rollout the *previous* build serves
against the *new* schema.

**Fresh installs are exempt.** On an empty database the migrations apply in
order, the backfill selects zero rows, the FKs validate trivially, and `0061`
drops columns nothing ever wrote — no old code is serving. The four-release
split is a rolling-upgrade requirement, not a database requirement. If you are
installing v1.7.2+ from scratch, stop reading; this guide is for deployments
with traffic.

---

## 1. The chain

| hop | release | contents | migration |
| --- | --- | --- | --- |
| A | v1.6.1 (or v1.6.2) | every LLM call site writes a parent row to `engine.llm_generations` | `0059` (creates the table) |
| B1 | v1.7.0 | backfill, eight indexes, eight **validated** foreign keys; code stops **writing** child `model` / `usage` | `0060` |
| B2-code | v1.7.1 | code stops **reading** them: `ChatMessage` loses the fields, replay joins `llm_generations` | none |
| B2-drop | v1.7.2 | `DROP` `model` / `usage` from the eight child tables | `0061` |

Each hop must be deployed **and serving** before the next hop's migration
runs. "Tagged" is not "serving", and neither is "the image is on the
machines" — `release_command` completes before traffic moves.

## 2. Preconditions, and how to verify each one

**Before deploying v1.7.0** (running `0060`): the serving build must be
Release A or later — a build that writes parent rows. Verify from the
database, not from tags:

```sql
SELECT count(*) > 0
FROM engine.llm_generations
WHERE created_at > now() - interval '1 hour';
```

A recent row is the only evidence the build *currently taking traffic* writes
parents. The caveat: this returns false on a freshly provisioned or idle
instance even when the build is right — for a quiet instance, confirm the
serving image version instead and accept that the query cannot help you.

**Before deploying v1.7.2** (running `0061`): every serving machine must be on
v1.7.1 or later — the build that neither writes nor reads the columns. The
evidence here is an *absence*, so no query can prove it. Check
`fly image show -a <app>` (or your platform's equivalent) reports v1.7.1+ on
**every** machine, with none mid-rollout.

**v1.7.1 itself has no precondition** beyond following v1.7.0: it carries no
migration and is valid on both sides of `0061` — being deployable on either
side is the reason the hop exists.

## 3. What each skip actually does

All three failure windows run from the moment the migration commits until
traffic has finished moving to the new build.

**≤ v1.6.0 → v1.7.2+ in one deploy** (one `release_command` applies
`0059`+`0060`+`0061` while a pre-A build serves):

1. The moment `0060` commits, its eight foreign keys are live. The serving
   build does not write parent rows, so every child INSERT violates a
   constraint — for `chat_messages`, that is a user's reply failing to
   persist.
2. After `0061`, the serving build's child INSERTs name `model` / `usage` —
   columns that no longer exist.
3. The same build hydrates `ChatMessage` via `SELECT *`, so every history
   read raises `ColumnNotFound` — every turn that loads context.

`0060` may instead **abort**, and that is the good outcome: its backfill and
orphan preflight take separate snapshots under READ COMMITTED, so a child row
the still-serving build commits between them is an orphan the preflight
raises on. The `release_command` fails and the old machines keep serving on
the old schema. But this is timing-dependent, not a version gate — a
low-traffic instance will likely sail through and then take 1–3.

**v1.6.x → v1.7.2+** (skipping B1 and B2-code): 1 does not apply — v1.6.x
already writes parents — but 2 and 3 both do. This is the likeliest mistake
in practice, because v1.6.x looks recent and skipping two hops looks like
saving two deploys.

**v1.7.0 → v1.7.2+** (skipping B2-code): only 3 applies, and it is the worst
of the three — not just replays but every turn that loads context fails for
the length of the rollout.

## 4. The rollback floor

Once `0061` has run, the oldest build that can serve is **v1.7.1**. v1.7.0
reads the dropped columns through `SELECT *` hydration; Release A and v1.6.2
additionally name them in every child INSERT. A deployer who skipped up and
hits trouble has no way back, only forward.

Before `0061`, the floor is v1.6.1: `0060`'s foreign keys require a build
that writes parent rows.

## 5. Two behaviors of `0060` worth knowing before you run it

- It sets `SET LOCAL lock_timeout = '3s'`. If a long-running transaction
  holds a lock it needs, the migration **fails fast** rather than holding
  `chat_messages` write-blocked behind the wait — a failed `release_command`
  leaves the old machines serving on the old schema. Re-run the deploy.
- `chat_messages.filter_model` survives `0061` on purpose. It is a
  discriminator that happens to be spelled like a model name (the regex arm
  writes a `<regex>` sentinel with no generation id at all), not a redundant
  copy of the parent's `model`.

## 6. Checklist

- [ ] Decide which hop you are on. `SELECT version FROM _sqlx_migrations
      ORDER BY version DESC LIMIT 1` tells you the schema; your serving image
      tells you the code. You need both.
- [ ] Walk the chain in order — v1.6.x, then v1.7.0, then v1.7.1, then
      v1.7.2 — deploying each hop and confirming it is **serving** before the
      next.
- [ ] Before v1.7.0: run the §2 one-query check (or verify the serving image
      on a quiet instance).
- [ ] Before v1.7.2: verify **every** machine serves v1.7.1+, none
      mid-rollout.
- [ ] If `0060` aborts on its orphan preflight or its `lock_timeout`: nothing
      is broken — the old build is still serving. Fix the precondition (or
      just re-run, for a lock trip) and deploy again.
- [ ] After v1.7.2: treat v1.7.1 as the rollback floor. Do not roll back
      further for any reason; roll forward instead.
