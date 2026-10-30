# Migrating to v2.6.0: memory and the relationship scan

**Applies to:** operators upgrading from `v2.5.2` to `v2.6.0`, deployments
that run their own `[tasks.memory_extraction]` prompt, deployments that erase
users or mirror memories through `/svc/v1/*`, and code that depends on the
`eros-engine-store` or `eros-engine-llm` crates.
**Design:** [`2026-10-28-memory-2-dreaming-plus-design.md`](../superpowers/specs/2026-10-28-memory-2-dreaming-plus-design.md)

The per-session sweeper of [memory (legacy)](../memory-layers_legacy.md) is
gone. In its place each relationship (one
persona instance) has a scan cursor, and one call per stretch of conversation
returns three things: the profile memories the sweeper used to write, a dated
**episode** summary, and dated **journal** moments. Every fourth episode is
folded into a running **relationship arc**. The arc, the latest episodes and
the journal moments nearest the turn join `[shared_memories]`, and a
relationship's anniversaries render under `[now]`. The model is described in
[Memory layers → Relationship memory](../memory-layers.md#relationship-memory);
this guide lists what an upgrade has to change.

## `[tasks.memory_extraction]` is retired

The block is ignored. A config that still carries it boots and logs:

```
[tasks.memory_extraction] is retired and ignored; configure [tasks.dreaming_scan] instead
```

A deployment that upgrades with only the old block runs with the scan off: no
categorised profile memories, no episodes, no model-written journal rows, no
arc (turns are still counted, and the engine's own journal rows and the
anniversary lines still work). Ship `[tasks.dreaming_scan]` with the new
image.

A `[[providers.<name>.body]]` rule that lists `memory_extraction` now warns at
boot (unknown task) and applies to nothing; retarget it to `dreaming_scan`, and
to `arc_rewrite` where it should cover the arc too. `engine.llm_generations`
records the scan as task `dreaming_scan` and the arc rewrite as `arc_rewrite`;
rows from before the upgrade keep `memory_extraction`, so a cost query grouped
by task shows both across the upgrade.

## `[tasks.dreaming_scan]`: the three-output contract

The gate is the old block's: a present section needs a non-blank
`filter_prompt` (the server refuses to boot otherwise), and an absent section
turns the scan off. The prompt is deployment config; the output contract is
the engine's. The full reference prompt is the `[tasks.dreaming_scan]` block in
[`examples/model_config.toml`](../../examples/model_config.toml) (the same
block is [`examples/model_config.d/memory.toml`](../../examples/model_config.d/memory.toml)).
A deployment on the shipped `memory_extraction` prompt copies the whole block.
A customised prompt keeps its memory rules as the `memories` part and adds the
other two.

The engine sends the stretch as one user message, one line per chat row:

```
[#12 2026-10-01 21:04] 用户：…
[#13 2026-10-01 21:05] AI：…
```

Stamps are in the user's timezone, the last valid `user_timezone` a text turn
of this relationship carried (UTC when none has). The `#n` is what a journal
item's `source` cites. A user row that carried an action shows its marker and,
once the persona answered it, the outcome sentence.

The reply is one JSON object:

```json
{"memories": [{"content": "...", "category": "fact", "domain": "career"}],
 "episode": "...",
 "journal": [{"kind": "confession", "realm": "real", "occurred_at": "2026-10-01",
              "summary": "...", "source": "#12", "due": null}]}
```

- **`memories`** keeps the old contract: objects with `content` and
  `category` (`fact`, `preference`, `event`, `emotion`, `relation`; anything
  else becomes `fact`), every other key stored verbatim in
  `companion_memories.metadata`, where `domain` drives the
  [affinity gate](../memory-layers.md#affinity-gate). They are written to the
  profile layer under the instance's owner.
- **`episode`** is one string (an object with a `summary` key is read the
  same). Blank or missing writes no episode; the rest of the reply is still
  used. It is stored on one line and cut to 400 characters.
- **`journal`** is an array of moments. An item missing `kind`,
  `occurred_at` or `summary` is dropped.

| Field | Engine handling |
|-------|-----------------|
| `kind` | One of `confession`, `first_intimacy`, `conflict`, `reconciliation`, `promise`, `gift`, `shared_plan`, `user_shared_milestone`; any other value drops the item. `first_meet` and `tier_reached` are written by the engine only. |
| `realm` | `real` or `roleplay`; a missing or other value is stored as `roleplay`. |
| `occurred_at` | `YYYY-MM-DD`, and it must be the date of a stamp in this transcript; otherwise the item is dropped. |
| `summary` | One sentence; stored on one line and cut to 200 characters. Blank drops the item. |
| `source` | `#n` of the line the moment rests on. A value that names no line keeps the item without a message pointer. |
| `due` | `YYYY-MM-DD`, read for `promise` and `shared_plan` only. A due date makes the moment come back as an anniversary line on that day. |

`realm` follows initiative, never physical reality: `real` when the character
initiates, or the user invites and the character agrees; `roleplay` when the
user supplies the script, sets the scene or narrates the moment and the
character performs it, and whenever the model is unsure. The shipped prompt
states the rule in these terms; keep them in a customised one. Only `real`
rows become anniversary lines; a recalled `roleplay` row is marked `（扮演）`.

Gifts and kisses offered through the action mechanism are journaled by the
engine at the turn. The shipped prompt tells the model to leave them out, and
the dedup keys hold either way: a `gift` whose `source` resolves to a message
is unique per message, and `first_intimacy` is unique per relationship and
realm.

The shipped block sets `max_tokens = 1024` and a two-model `fallback`; see
[The fallback and the journal](#the-fallback-and-the-journal) below.

## `[tasks.arc_rewrite]`

New, parameters only: `model`, `fallback`, `temperature`, `max_tokens` and the
sampling knobs. The prompt is engine-owned, so a `filter_prompt` in any shape,
blank included, refuses to boot. The engine rejects an arc over 900
characters and keeps the previous one, so `max_tokens` must sit well above
that: a tight limit cuts the JSON off, which counts as a parse failure. The
shipped block sets `max_tokens = 1500`.

Without the block, arc rewrites are skipped with one warning
(`arc_rewrite is not configured: episodes stay unfolded`), and scans,
episodes and journal rows run as usual. **Configure it together with
`dreaming_scan`.** Episodes written while it is absent stay unfolded, and the
first rewrite after the block is added folds all of them in one call.

## Environment

| Variable | Default | What changed |
|----------|---------|--------------|
| `DREAMING_IDLE_SECS` | `3600` (was `1800`) | How long a relationship's last counted turn must sit before the sweeper scans it. A value set explicitly keeps applying, now per relationship rather than per session. |
| `DREAMING_DISABLED` | unset | `1` or `true` now also stops the inline scan at 20 pending turns, not only the sweeper. Turns are still counted and the engine's own journal rows are still written. |
| `DREAMING_VOICE_DISABLED` | unset | `1` or `true` leaves voice sessions out of the scan's transcript and stops voice turns from counting toward the scan. The cursor moves by time, so voice rows passed over while it is set are not read after it is cleared. |
| `DREAMING_TICK_SECS` | `300` | Unchanged. `0` stops both the sweeper and the inline scan, like `DREAMING_DISABLED`. |
| `DREAMING_CLAIM_STALE_SECS` | `600` | Unchanged; it now ages a relationship's scan claim. Keep it above one scan call plus one embedding call; a backfill segment also holds its claim through an arc rewrite. |

With the sweeper stopped, the journal rows the engine writes itself get no
vector: the sweeper's tick is what embeds them. They still drive the
anniversary lines but stay out of semantic recall.

## When the scan runs

- **Counting.** Every 1:1 text turn counts toward its relationship. A voice
  turn counts too but never scans inline. Group turns do not count.
- **At 20 pending turns** a text turn's post-processing scans the
  relationship inline, after its other post-turn work.
- **Idle flush.** The sweeper scans relationships with at least 3 pending turns
  whose last counted turn is `DREAMING_IDLE_SECS` old, up to 10 per tick. A
  relationship that never reaches 3 pending turns is not scanned; its rows are
  read by the next scan that reaches it.
- **One scan** reads at most 60 rows after the cursor, across the pair's
  non-archived text and voice sessions. The rest waits for the next claim.
- **Failures.** A failed call, an unparsable reply, a failed embedding call, or
  a database error while reading the owner or writing the episode leaves the
  cursor where it was; the sweeper retries after one idle period (a turn that
  still finds 20 pending retries inline). The third consecutive failure moves
  the cursor past the segment with a warning: the scan loses that stretch,
  the raw rows stay.

Cost shape: about one scan call per 20 user turns of an active relationship,
plus one per idle flush (the retired sweeper made one call per session); one
arc rewrite per four episodes; one embedding call per scan that writes
memories or journal rows, plus one per sweeper tick that finds unembedded
engine journal rows.

## Migration `0082` and deploy order

`0082` creates five tables: `relationship_scan_state`,
`relationship_episodes`, `relationship_arcs`, `relationship_journal` and
`relationship_scan_events`, with RLS on and no access for `anon` /
`authenticated`. It fills nothing.

**Run `migrate` before the new image serves.** The new image inserts journal
rows inside the affinity write's transaction (a tier first reached) and the
action-response write's transaction (an accepted gift or kiss). Against a
database without the table those inserts fail and take the affinity update
and the recorded response down with them.

**A database that applied a pre-release build of `0082` must be rebuilt.**
The migration was edited before release, and `migrate` refuses a version whose
checksum changed. On such a database, drop the five tables, delete the
`version = 82` row from `_sqlx_migrations`, and run `migrate` again (or
recreate the database). A database that never ran an unreleased build is not
affected.

Live scanning starts at the upgrade. A relationship's scan state is created
on its first turn after it, with the cursor just before that turn; older
history is read only by the backfill below. The same first turn journals the
relationship's first meeting, dated by its earliest chat row, so an existing
relationship gets its real date and can have an anniversary line from then
on. Tier, gift and kiss moments are journaled from the upgrade on.

## `backfill-dreaming`

```
eros-engine backfill-dreaming [--dry-run] [--min-rows N] [--limit N] [--instance UUID]
```

The operator's pass over history before the live cursor, run against the same
environment, database and model config as `serve` (Docker:
`docker compose run --rm engine backfill-dreaming --dry-run`). It is never
scheduled and `serve` never starts it; it does not read `DREAMING_DISABLED`.

| Flag | Default | Meaning |
|------|---------|---------|
| `--dry-run` | off | Print the plan and exit. Reads the database and the model config; needs no API key and makes no call. |
| `--min-rows N` | `20`, or `1` with `--instance` | Skip relationships with fewer rows left to backfill. |
| `--limit N` | all | At most `N` relationships, most rows first. |
| `--instance UUID` | all | This relationship only. |

Without `[tasks.dreaming_scan]` the command prints
`backfill-dreaming: [tasks.dreaming_scan] is not configured; nothing to scan with`
and exits 2 before it builds any client, dry run included. An unknown flag
also exits 2, with the usage line.

**Cost model.** The plan is one line per relationship (`instance_id`, `rows`,
`segments`) and a total: `N relationships, R rows: C scan calls, about A arc
rewrites`. A segment is at most 60 rows and ends after a reply when more rows
follow; **every segment is one scan call**. Arc rewrites are about the
segments divided by four, counted per relationship together with the episodes
already waiting for one. A segment that writes memories or journal rows also
makes one embedding call. Run `--dry-run` first and size the run with
`--limit` or `--instance`.

**What a run does.**

- Walks each relationship oldest first, through the normal scan, with
  `kind = 'backfill'` on its event rows, and prints
  `segments: X ok, Y empty, Z failed; relationships skipped: S` at the end.
- Writes episodes, journal rows and arc rewrites for the whole window. Profile
  memories are written only from segments holding a row newer than its
  session's `classified_at`, or from a session never stamped: the retired
  sweeper already extracted the rest. A segment that straddles a stamp writes
  its memories, so a few may be extracted twice.
- Reads rows up to the cursor the relationship's live scanning started from.
  It never moves the live cursor, the pending count or the failure streak; its
  own progress is `backfilled_through_at`.
- **Resumable.** An interrupted run restarts from each relationship's mark. A
  failed segment stops that relationship, and the next run starts with it.
- Runs beside `serve`. It claims a relationship one segment at a time, so a
  live scan waits at most one segment; a relationship a live scan holds is
  skipped and counted under `relationships skipped`, for the next run.
- Opens a scan state for a relationship that has not chatted since the
  upgrade, with the cursor at its newest row. Its first meeting is journaled
  by its next live turn, in the user's timezone.

Run it soon after the upgrade. The arc folds episodes as they are written, so
a relationship whose arc already folded live episodes gets its history folded
in afterwards, as later material.

## The fallback and the journal

The shipped `dreaming_scan` block falls back to two other models. Measured on
a production deployment, one candidate model produced no journal entries at
all, so a scan a fallback served may write memories and an episode with no
journal rows. The engine records this instead of preventing it: each scan's
row in `engine.relationship_scan_events` carries the hop that served it in
`model` and `served_fallback = true`, and a fallback-served scan logs at warn
with its journal count (see
[LLM audit → Relationship scanner](../llm-audit.md#relationship-scanner)).

A deployment that wants a dependable journal sets `fallback = []` on the task.
A primary outage then fails the scan instead: one idle period of backoff, and
the third consecutive failure skips the segment.

## Archive, erasure and export

- **`DELETE /comp/instance/{instance_id}/sessions`** also deletes the
  instance's rows in `relationship_scan_state`, `relationship_episodes`,
  `relationship_arcs` and `relationship_journal`. `relationship_scan_events`
  stays: counts and ids only. A scan in flight writes nothing further. A
  relationship restarted on the same instance starts a new scan state and a
  new first meeting, dated by its first non-archived row.
- **`POST /svc/v1/users/{user_id}/erase`** deletes the same four tables
  through the user's persona instances and leaves `relationship_scan_events`
  alone. The `deleted` map gains four keys; a client that checks the key set
  adds them. Call it before deleting the user's persona instances, as for the
  per-relationship insight tables. Tables in
  [Insights → Erasure](../insights.md#erasure).
- **`GET /svc/v1/memories/changes`** is unchanged. It exports the
  profile-layer categorised rows, which the scan's `memories` now write.
  Episodes, arcs and journal rows are never exported.

## What users will notice

- **`[shared_memories]`** changes shape once a relationship has an arc, an
  episode or a journal moment near the turn: the arc paragraph, then up to
  three dated episodes, then up to two dated journal moments, then the raw
  recalled lines, within 1,200 characters. Until then the block is exactly as
  before. The first episode comes with the relationship's first scan (20
  turns, or an idle hour after at least 3); the arc after four episodes.
- **Anniversary lines** under `[now]`: the monthly first-meeting date for the
  first eleven months, then the yearly one up to six days ahead, and a
  promise or plan on its due date. They need `user_timezone` on the turn and
  a `memory_scope` that keeps relationship memory.
- **Every 1:1 prompt** carries this memory: text turns, proactive messages
  (`/open` greetings, events, life choices) and image-edit replies. Group
  seats do not.
- **Voice.** A call's first-turn snapshot gains a `[你们的故事]` block, the
  relationship's opening and arc, when there is an arc and that turn's
  `memory_scope` keeps relationship memory. Voice turns count toward the scan,
  and the scan reads calls with the text rows of the same relationship.

## Rollback

Deploy the `v2.5.2` image without running its `migrate`: its migrator does not
know `0082` and refuses a database that has it.

- The five tables are invisible to it and stay as they are.
- The new image never stamps `chat_sessions.classified_at`, so the old sweeper
  sweeps each session it finds unstamped once, under its per-session rule.
  Profile memories from sessions the scan already read may be written a
  second time.
- Keep `[tasks.memory_extraction]` in the config while a rollback is possible.
  The new image ignores it with the warning above, the old sweeper needs it,
  and the old image ignores the two new blocks.
- The old image's archive and erase do not reach the four tables. Before
  upgrading again, delete the four tables' rows of every instance archived
  while rolled back:

  ```sql
  DELETE FROM engine.relationship_scan_state WHERE instance_id = $1;
  DELETE FROM engine.relationship_episodes   WHERE instance_id = $1;
  DELETE FROM engine.relationship_arcs       WHERE instance_id = $1;
  DELETE FROM engine.relationship_journal    WHERE instance_id = $1;
  ```

  For a user erased while rolled back, call the erase route again once the
  new image serves; it is idempotent and reaches the four tables.

## Crates

`eros-engine-store`: new modules `scan_state`, `episode`, `arc`, `journal` and
`scan_event`. `AffinityRepo::persist_with_event` gains a trailing
`journal_tz: Option<chrono_tz::Tz>`, and `ChatRepo::set_action_response`
gains `instance_id: Uuid` and `journal_tz: Option<chrono_tz::Tz>` (both break
call sites). `SessionArchiveRepo::archive_relationship` and
`ErasureRepo::erase_user` cover the four new tables. The crate now depends on
`chrono-tz`.

`eros-engine-llm`: `ModelConfig::resolve_memory_extract` is replaced by
`resolve_dreaming_scan`; `resolve_arc_rewrite`, `DREAMING_SCAN_TASK` and
`ARC_REWRITE_TASK` are new, and `memory_extraction` is no longer a known task
name.

`eros-engine-core`: `BondLabel` and `ChemistryLabel` gain `zh()` and `rank()`.

## Coming in a later release

`chat_sessions.classified_at` and `classification_claimed_at` are no longer
written. The backfill is their last reader, and an image older than `v2.6.0`
needs them. A later release drops both, once `v2.6.0` has held; the rollback
above ends with that release.
