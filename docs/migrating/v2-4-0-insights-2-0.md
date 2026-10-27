# Migrating to v2.4.0: Insights 2.0, the fact ledger and the derived profile

**Applies to:** clients of `eros-engine` upgrading from `v2.3.1` to `v2.4.0`,
operators rolling it out, deployments that mirror the profile or the memories
through `/svc/v1/*`, and code that depends on the `eros-engine-store` or
`eros-engine-llm` crates.
**Design:** [`2026-10-26-insights-2-fact-ledger-design.md`](../superpowers/specs/2026-10-26-insights-2-fact-ledger-design.md)

The human profile is no longer rewritten by a second LLM call on every turn.
Extraction writes atomic facts into a ledger (`engine.human_facts`), and the
profile row is rebuilt from the ledger on a schedule. The new model is
described in [insights.md](../insights.md); this guide lists what an upgrade
has to change.

## Your `filter_prompt` must emit the object array

`[tasks.insight_extraction].filter_prompt` is deployment config, and the output
contract it must follow changed. The engine now reads one array of objects:

```json
{"facts":[{"content":"用户养了一只猫","category":"fact","domain":"lifestyle","evidence_type":"explicit_statement","temporality":"current","persistence":"ongoing","confidence":"high"}]}
```

A string inside `facts` is ignored and logged as one warning per run, and
`details` is not read. **A prompt still on the old shape (`facts` as strings
plus a parallel `details` array) writes nothing to the ledger**: every run ends
as `empty` and logs the warning, and the profile never updates. The engine does
not repair the old shape. Ship the new prompt with the new image.

The vocabularies are validated by the engine: a label outside its list folds to
`other` (`category`, `domain`) or `unknown` (the rest) and the fact is kept; the
six lists are in [insights.md](../insights.md#vocabularies). Three changes to
the shipped prompt, to copy into a customized one (a deployment on the shipped
prompt copies the whole `[tasks.insight_extraction]` block from
`examples/model_config.toml`):

```diff
-- domain: basic_profile / location_culture / career / education / technology / finance / family / friendship / romance / sexuality / health / lifestyle / hobby / politics / religion / personality / communication / other
+- domain: basic_profile / career / education / technology / finance / family / friendship / romance / sexuality / kink / health / mental_health / lifestyle / hobby / ideology / personality / communication / other。其中容易混淆的六个：
+  - sexuality：性取向（仅限用户自述）、性经历、性生活状态
+  - kink：性方面的偏好、角色、玩法、情境设定，以及性方面的界限
+  - health：身体状况、物质使用
+  - mental_health：用户自述的心理状态、诊断、治疗，以及持续性的痛苦
+  - ideology：政治、宗教与社会价值立场（性别角色、婚姻与家庭观、阶层等），可以是用户明说的，也可以是用户的反应所表现出的
+  - education：学历层次、学校、专业、在读或毕业状态（仅限用户自述）
```

```diff
+不提取：
+- 任何地理信息：居住地、当前所在地、城市、家乡、出生地、国籍、文化背景。也不得从用户所用的语言、方言或用词习惯去推断这些信息。
+- 第三方的姓名：用户提到的其他人只写与用户的关系（如「用户的前任」「用户的同事」），不写名字。
```

```diff
-严格输出 JSON，且只能是以下结构。facts 与 details 长度必须完全相同，facts[i] 必须与 details[i].content 完全相同；没有事实时输出 {"facts":[],"details":[]}。不得输出说明、Markdown、代码围栏或任何其他文本：
-{"facts":["用户事实1"],"details":[{"content":"用户事实1","category":"preference","domain":"hobby","evidence_type":"explicit_statement","temporality":"atemporal","persistence":"stable","confidence":"high"}]}
+严格输出 JSON，且只能是以下结构。facts 是对象数组，每个对象含 content 与上述六个元数据字段，取值只能来自词汇表；没有事实时输出 {"facts":[]}。不得输出说明、Markdown、代码围栏或任何其他文本：
+{"facts":[{"content":"用户养了一只猫","category":"fact","domain":"lifestyle","evidence_type":"explicit_statement","temporality":"current","persistence":"ongoing","confidence":"high"}]}
```

The domains `politics` and `religion` are still accepted at write time and map
to `ideology`; `location_culture` and any other unlisted value become `other`.
A prompt that keeps richer labels than the lists loses that detail to `other`
and `unknown`; this is not an error. The shipped prompt also adds per-domain
rules for `kink`, `ideology`, `mental_health` and `education`; copy them if the
deployment wants the split to behave the same way.

## Model config

**`[tasks.profile_rebuild]` is new.** It supplies `model`, `fallback`,
`temperature`, `max_tokens` and the sampling knobs of the scheduled rebuild.
The rebuild's prompt is engine-owned, so a `filter_prompt` in any shape,
blank included, refuses to boot, and so does a tier block. Without the block
the rebuild runs on `[tasks.insight_extraction]`'s parameters, not on the global
defaults. `max_tokens` bounds the whole profile JSON: a reply cut off before
the object closes does not parse and counts as a failed rebuild. The shipped
example sets the block.

**`[tasks.insight_structuring]` is ignored.** Nothing reads it, so it can be
deleted; leaving it does no harm and does not feed the rebuild. Two things
follow for anything keyed on the old task name:

- A `[[providers.<name>.body]]` rule whose `tasks` lists `insight_structuring`
  warns at boot (unknown task) and applies to nothing. Retarget it to
  `profile_rebuild`. A rule that lists only `insight_extraction` no longer
  covers the call that writes the profile; add `profile_rebuild` where it
  should.
- `engine.llm_generations` records the rebuild as task `profile_rebuild` with a
  `NULL` `session_id`; the old stage-2 calls stay under `insight_structuring`.
  A cost query that groups by task shows both names across the upgrade.

## `GET /comp/user/{user_id}/profile`

The seven retired fields are gone from the response: `city`, `location`,
`hometown`, `nationality`, `preferred_gender`, `age_min`, `age_max`. Five are
new: `education_level` (`secondary_or_below`, `associate`, `bachelor`,
`master`, `doctorate`, or `null`), `education_status` (`enrolled`, `graduated`,
or `null`), `kinks` (array), `ideology` and `mental_health`. This route
returns the sensitive three because it serves the user's own data. A client
with a strict schema or enum over the response updates both lists. Until a
user's first rebuild the new fields are `null` / `[]`.

The prompt bullets change with the fields. `城市`, `所在地`, `老家` and `国籍` are
no longer rendered, in either tier, from the deploy on. `学历` and `意识形态`
join both tiers, and `性癖` and `心理健康` join the Full tier
([insights.md](../insights.md#chat-prompt)).

## `GET /svc/v1/insights/changes`

The row loses the seven retired fields and gains `education_level` and
`education_status` (the same two code sets as above). **`kinks`, `ideology`
and `mental_health` are never exported**: a deployment that already has
`INSIGHTS_EXPORT_SECRET` set does not start leaking them on upgrade. A mirror
updates its row schema and any enum over the two education fields. The
cursor, the ordering and the secret are unchanged.

A row now moves through the feed only when a rebuild replaces the profile, not
on every turn. The one-time rebuild drain below moves each queued profile row
through the feed once, so a mirror re-reads every profile after the upgrade.

## New secrets and routes

`/svc/v1/*` was one sub-tree under one secret; it is now three, each mounted
only when its own secret is set (at least 32 bytes after trimming; shorter
refuses boot). An unset secret leaves its routes answering like any unknown
path.

| Secret | Header | Routes |
|--------|--------|--------|
| `INSIGHTS_EXPORT_SECRET` | `x-insights-export-secret` | `insights/changes`, `memories/changes`, and the new `GET /svc/v1/insights/deletions` |
| `FACTS_EXPORT_SECRET` | `x-facts-export-secret` | new: `GET /svc/v1/facts/changes`, `GET /svc/v1/facts/deletions` |
| `USER_ERASE_SECRET` | `x-user-erase-secret` | new: `POST /svc/v1/users/{user_id}/erase` |

**Any mirror of profiles or memories must consume `insights/deletions`.** An
erase deletes the profile and memory rows, and neither `insights/changes` nor
`memories/changes` can report a row that is gone. The facts mirror reads the
same stream at `facts/deletions` with its own secret. Rows are
`{user_id, erased_at}`, keyset-paged by `(erased_at, user_id)`; there is no
per-fact deletion. A repeat erase moves `erased_at` forward, which re-announces
the user.

**Erase before the caller deletes the user's sessions or persona instances.**
The per-relationship insight tables (`user_insights`, `character_insights`,
their snapshots and events) key on the instance alone, so they are reachable
only while the instance row exists. **Quiesce the user's traffic first:** an
extraction or rebuild already in flight can write after the erase commits, and
a repeat call cleans up. The call is idempotent and returns per-table counts of
rows deleted and rows blanked. What it removes, and what it blanks instead, is
tabulated in [insights.md](../insights.md#erasure). Chat history stays the
caller's own deletion.

## Environment

| Variable | Default | Effect |
|----------|---------|--------|
| `PROFILE_REBUILD_DISABLED` | unset | `1` or `true` stops the idle-flush sweeper. |
| `PROFILE_REBUILD_TICK_SECS` | `300` | Sweeper interval; `0` also stops it. |
| `PROFILE_REBUILD_IDLE_SECS` | `3600` | How long a user's last new fact sits before the sweeper rebuilds. |
| `PROFILE_REBUILD_BATCH` | `20` | Users rebuilt per tick. |

**With the sweeper off, only the count trigger fires**: a user is rebuilt when
the run that just wrote facts brings `pending_facts` to 20. Facts that never
reach 20 then stay out of the profile.

## Rollout

Run `migrate`, then serve the new image with the new `filter_prompt`. Run
`migrate` as the release step right before the switch: facts the old image
extracts after the backfill ran are not in the ledger, and stay only in the
audit table.

The release carries two migrations. `0079` creates `engine.human_facts`, `engine.human_profile_state`,
`engine.user_erasures` and `engine.fact_norm`, adds five columns to
`engine.human_insights` and widens `companion_insights_events.stage` to accept
`profile`. `0080` backfills, once:

1. **Events to ledger.** Every successful `facts` audit row that has a
   `details` array, for users that currently have a profile row. Users without
   a profile row are not resurrected from audit data and start their ledger at
   their next turn. `location_culture` facts are dropped, `politics` and
   `religion` become `ideology`, and runs from before the `details` format are
   skipped.
2. **Profile seeds.** Surviving profile values become `legacy_profile` ledger
   rows, one per scalar column and per array item, minus placeholders and the
   old schema's example strings.
3. **One rebuild queued per profile row**, at the count threshold with an idle
   timestamp a day old.

The migration prints its counts as a Postgres notice: `insights_2_backfill:
events=<n> seeds=<n> pending=<n>`. **`pending` is the number of LLM calls the
rebuild drain will make**, about one per profile row; the idle sweeper spends
them at `PROFILE_REBUILD_BATCH` users per `PROFILE_REBUILD_TICK_SECS`, and
until a user's turn comes their old profile row keeps serving injection. With
the sweeper off, each queued user is rebuilt at their next extraction run
instead. Size `[tasks.profile_rebuild]` first. A deployment that has turned
extraction off still has the queue: with no `[tasks.profile_rebuild]` block it
drains on the global default model, and `PROFILE_REBUILD_DISABLED=1` stops it.

**Sexuality and health reclassification is the operator's.** Backfilled facts
keep their old `sexuality` and `health` domains; the new `kink` and
`mental_health` domains apply to facts extracted from now on. The engine ships
no reclassifier. An operator who wants the old rows split runs an offline pass
that updates `engine.human_facts.domain` (it needs nothing beyond `UPDATE`
access); a relabel does not queue a rebuild, so the corrected domains are read
at each user's next rebuild.

**The retired columns stay populated.** `city`, `location`, `hometown`,
`nationality`, `preferred_gender`, `age_min` and `age_max` remain in the table
with their old values, unread and unwritten, and the daily snapshot still
carries them. An erase deletes the whole row. A deployment that needs location,
a gender preference or an age range for matching collects it from the user
directly.

**Audit consumers.** `companion_insights_events.stage` now takes `facts`,
`structured` and `profile`; `structured` rows are history. The `facts` payload
is `{"facts": [objects]}` with no `details`, exactly as the model emitted it.
A consumer that checks the stage set or reads `details` updates both.

## Rollback

Deploy the `v2.3.1` image without running its `migrate`. It reads the retired
columns, which this release leaves untouched, and does not see the new tables
or the `profile` stage. Restore the old `filter_prompt` with it: the old
engine reads string items in `facts`, so the new object-array prompt yields no
facts there. Facts extracted while rolled back go through the old pipeline into
the old columns and the audit table only, not into the ledger; the ledger and
the rebuild state stay as they were, and a later roll-forward continues from
them. The erase and facts routes do not exist on the old image.

## Crates

`eros-engine-store`: `human_insight::HumanInsightsRow` loses the seven retired
fields and gains `education_level`, `education_status`, `kinks`, `ideology` and
`mental_health` (breaks struct literals). `project_columns`,
`existing_as_extraction_json` and `HumanInsightRepo::apply_extraction` are
removed; `project_profile` and `HumanInsightRepo::replace_profile` replace
them, and `ProjectedColumns` is the new 19-field shape. New modules:
`human_fact`, `profile_state`, `erasure`.

`eros-engine-llm`: `ModelConfig::resolve_profile_rebuild` is new, and
`insight_structuring` is no longer a known task name.

## Coming in v2.4.1

The cleanup is a separate release, once `v2.4.0` has held:

- It drops the seven retired columns from `engine.human_insights`.
- It renames `engine.companion_insights_events` to `engine.human_insights_events`,
  the last `companion_insights` name; rows and ids are unchanged. SQL that reads
  the audit table by name moves to the new name.
- It deletes `docs/insights_legacy.md`.

Code that still reads the seven columns directly from the database stops
working at that release.
