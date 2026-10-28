# Migrating to v2.5.2: outing and indulgence needs, persona habits

**Applies to:**
- clients that read the events route or the `needs` object;
- deployments that run the LLM judge with their own judge prompt;
- persona authors.

There is no migration and no configuration key. Every HTTP change is additive.
**Design:** [`2026-10-28-outing-indulgence-needs-and-habits-design.md`](../superpowers/specs/2026-10-28-outing-indulgence-needs-and-habits-design.md)

The persona gains two needs, `outing` and `indulgence`, and three optional persona keys, `smoking`, `alcoholic` and `drug`. Two `mid` life-event kinds also arrive: `want_to_go_out` and `want_to_go_home`. The rules are in [Life events and needs](../life-events-needs.md).

## What users will notice

- **Two more needs in the reply.** A persona can now want to go out or stay in (只想待在屋里 / 想出门走走). When `indulgence` is high she wants to let loose. With no habit key `true` the reply reads 很想放纵一下; otherwise it names one thing she uses.
- **Two new incidents.** A draw can now come up `want_to_go_out` (想出门) or `want_to_go_home` (想回家). Both are told and she keeps replying.
- **Nothing changes for gives** until a persona sets a habit key to `false`. Personas without the keys behave as before.

## What to change

- **Event kinds.** A caller that sends no `kinds`, or a `{"disallow": […]}` list, can now receive `want_to_go_out` and `want_to_go_home`. A client that maps `kind` to labels, icons or a closed enum must add both. A caller that sends an allow-list draws them only once it lists them.
- **`needs` shape.** `needs` on the `body_state` stream frame and on `GET /v2/comp/session/{session_id}/effects` now carries `outing` and `indulgence`, each `low` / `mid` / `high`. A client that deserializes the object strictly must accept the two keys. A client that displays needs may show them.
- **Your judge prompt.** If `[tasks.pde_decision].filter_prompt` is your own, add the two needs. Copy the two lines under `"needs"` in the reference prompt in [`examples/model_config.toml`](../../examples/model_config.toml).
  - **With structured output on**, the schema already requires all six keys, but the judge rates the two new ones blind until the prompt describes them.
  - **Without structured output**, a verdict that leaves them out keeps their previous levels.
  - Either way, the `[需求]` line now always carries `外出=…`, and carries `享受=高` only when indulgence is high. The reference prompt tells the judge that a missing 享受 means "not high".

## What to check

- **Persona authors.** `art_metadata.smoking`, `alcoholic` and `drug` are optional booleans. Leaving a key out, or setting it `true`, keeps today's behavior on gives.
  - **An explicit `false`.** A `give` of that item (`cigarette`, `alcohol`, `herb`) is then refused two times in three, with or without the judge.
  - **More keys `true`.** The more keys are `true`, the faster her urge to let loose comes back between ratings.
  - The keys never reach the judge's persona brief. A persona whose habits should colour her talk says so in her persona text.
- **BYOC cards.** The keys are read from the card on a turn. Outside a turn (`GET …/effects`, a replay) a BYOC persona's `indulgence` drifts as for a persona with no habit.

## Rollback

Swap back to the `v2.5.1` image.
- Ratings written by v2.5.2 carry two extra keys in `metadata.needs`, and frozen voice snapshots do too. v2.5.1 ignores them.
- Event rows of the two new kinds are skipped on read by a build that does not know them.

Nothing needs undoing.
