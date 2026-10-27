# Insights 2.0: the fact ledger and the derived profile — Design

- **Date:** 2026-10-26
- **Status:** Approved
- **Type:** Engine change: one new fact table plus two small state tables, a
  rewritten human-insight pipeline (stage 2 replaced by a scheduled profile
  rebuild), profile column additions and retirements, two new service
  endpoints behind their own secrets, a backfill migration, and a breaking
  change to `GET /svc/v1/insights/changes`. Ships with a migration guide.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — the human chain only (`insight_extraction` →
  `engine.human_insights`). The character and user insight chains, and
  `memory_extraction`, are untouched as pipelines; erasure (§8) reaches
  their rows and payloads.
- **Builds on:** `2026-06-03-insight-events-and-audit-columns-design.md`
  (the events table), `2026-06-03-extraction-geo-and-config-prompts-design.md`
  (geo columns, now retired), the 2026-07-15 insight-memory enrichment
  (the six profile slots). The two-stage pipeline those specs built remains
  stage 1 + a writer; stage 2 as a per-turn LLM call is retired here.

## 1. Motivation

Today every chat turn runs two LLM calls for the human profile: stage 1
(`insight_extraction`, prompt owned by deployment config) mines atomic user
facts with per-fact metadata; stage 2 (`insight_structuring`, prompt owned by
the engine) squeezes them into the 21 fixed columns of
`engine.human_insights`, rewriting the whole profile each time
(`apply_extraction`: non-null scalars overwrite, non-empty arrays replace
wholesale, no erase path). The per-fact metadata is never parsed; facts
survive only inside `engine.companion_insights_events` payloads.

Measured on a production deployment:

- **About two-thirds of stage-1 facts have no profile column to land in**
  (sexuality, communication and other domains dominate) and exist only in the
  audit table. Some users had facts extracted for months and still no profile
  row, because stage 2 kept returning an empty object.
- **Stage 2 is a per-turn full rewrite.** Over 99% of the keys it emits are
  re-emissions of values already stored; whole-array replacement loses more
  items between snapshots than it adds; and every truncation observed (the
  p99 of stage-2 output sits at `max_tokens`) occurred on turns whose
  existing-profile JSON was large — rewriting the old profile is the
  truncation root cause, and no model swap fixes it.
- **The free-text columns are overloaded.** Multi-clause paragraphs
  accumulate in `love_values` / `emotional_needs` and are rewritten each
  turn, occasionally shrinking.
- **Geography is noise.** Users rarely state where they live; roleplay
  settings contaminate what they do state; and schema example values were
  found copied verbatim into stored profiles. The matching fields fare no
  better: stored `age_range` values are mostly trivial defaults (0–0,
  18–99), and `preferred_gender` carries placeholders and "the AI" as a
  gender.

2.0 makes the atomic facts a first-class table (the **fact ledger**) and
turns the profile into a **derived summary** rebuilt from the ledger on a
schedule, not per turn. Geography and the LLM-guessed matching fields are
removed: a deployment that needs location, gender preference or an age range
for matching should collect them from the user directly, not infer them from
chat.

## 2. Decisions

1. **The ledger is the source of truth for conversation-derived user
   facts; the profile is a derived summary.** The ledger records and never
   adjudicates: "user is single" and a later "user has a partner" are two
   rows, each with its own timestamps. Current-state judgment belongs to the
   derived profile and to downstream consumers.
2. **Write-time dedup is exact-match only**, within one user and one
   evidence class (`roleplay_expression` never merges with anything else),
   on text normalized by NFKC, lower-casing, and stripping whitespace and
   punctuation. No embedding merge on the write path: labelling measured on
   production facts put the detail-losing merge rate at roughly one in nine
   near-duplicate pairs at the best cosine threshold, with no threshold
   separating good merges from bad, for a ledger only ~14% smaller.
   Paraphrased duplicates stay as separate rows; the derived profile, which
   reads the ledger grouped by domain, folds them in prose.
3. **`persistence` stays a stage-1 label.** Measured, ~98% of ledger rows
   occur exactly once and under 1% recur across sessions, so an
   engine-computed tier from occurrence counts would put nearly everything
   in one bucket, and the label's `stable`/`transient` split is exactly what
   keeps "user is on the subway right now" out of durable profile fields.
   The engine stores raw recurrence columns (occurrences, sessions, first/
   last seen) and derives one coarse tier — `once` / `repeated_in_session` /
   `repeated_across_sessions` — used alongside the label, never instead of
   it.
4. **The profile rebuilds when a user has accumulated K=20 new-or-recurring
   facts, or when a non-zero counter has sat idle for 1 hour** (sweeper
   flush). Simulated on production traffic this cuts profile-writing LLM
   calls by ~86% with a p99 freshness of ~1.6 hours and no fact stranded
   forever.
5. **A rebuild reads the ledger alone — never the previous profile** — cut
   to a 16,000-character input budget (grouped by domain, ranked by
   occurrence count then recency). Output replaces the row: a field the
   rebuild omits becomes empty. A rebuild whose output fails to parse or is
   empty writes nothing and leaves the old row standing.
6. **Seven profile columns are retired:** `city`, `location`, `hometown`,
   `nationality`, `preferred_gender`, `age_min`, `age_max`. 2.0 stops
   writing, rendering and exporting them; a later release drops the columns
   (two-step removal, so a rollback to the previous image still finds every
   column it reads). `deal_breakers` stays, constrained to real-life
   dealbreakers by the rebuild prompt.
7. **Four profile fields are added:** `kinks` (TEXT[], roleplay-sourced
   items marked as such inside the phrase), `ideology` (TEXT, hedged wording
   when inferred from behaviour), `mental_health` (TEXT, user self-report
   only), `education_level` + `education_status` (engine-validated enums;
   the free-text `education` column stays for school/major detail).
   Injection grouping: `kinks` and `mental_health` render in Full mode only;
   `ideology` and `education_level` render in both Full and Neutral.
   **None of the three sensitive fields (`kinks`, `ideology`,
   `mental_health`) is ever added to `GET /svc/v1/insights/changes`** — a
   deployment that has its secret set would otherwise start leaking them on
   upgrade.
8. **Ledger export and erasure are separately gated.** The facts export
   mounts only under its own secret (default: unmounted), with a deletions
   stream so erasures propagate downstream. The erase endpoint mounts under
   its own secret and removes everything the engine has derived about a
   user; for erased users, erasure outranks the audit-rows-outlive-their-
   subject rule — event rows stay, their payloads are blanked.
9. **The stage-1 parse contract becomes a single object array.** Today the
   engine reads only the `facts` string array and ignores `details`; 2.0
   parses one array of fact objects and retires the string array. The
   deployment's `filter_prompt` must change in the same release (config
   ships with the image, so the switch is atomic per deployment).
10. **History is kept.** Facts already sitting in
    `companion_insights_events` payloads are backfilled into the ledger, and
    current profile column values are seeded into it as legacy-evidence
    facts, so the switchover loses nothing that predates it.
11. **`mbti_guess` keeps its current rule** (self-reported type verbatim;
    otherwise a hedged guess only from patterns recurring in the ledger).
    The stage-1 ban on diagnosing personality types governs fact
    extraction; the profile's `_guess` field is a summary-level inference
    and says so in its wording.

## 3. The fact ledger

### 3.1 Table

One row per distinct fact per user (`engine.human_facts`; all names are the
builder's to adjust):

```sql
CREATE TABLE engine.human_facts (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id          UUID NOT NULL,          -- attribution column: no FK
    content          TEXT NOT NULL,
    content_norm     TEXT NOT NULL,          -- engine.fact_norm(content)
    category         TEXT NOT NULL,          -- 'other' when out of vocab
    domain           TEXT NOT NULL,          -- 'other' when out of vocab
    evidence_type    TEXT NOT NULL,
    temporality      TEXT NOT NULL,          -- 'unknown' when absent
    persistence      TEXT NOT NULL,          -- 'unknown' when absent
    confidence       TEXT NOT NULL,          -- 'unknown' when absent
    occurrences      INT  NOT NULL DEFAULT 1,
    session_count    INT  NOT NULL DEFAULT 1,
    first_seen_at    TIMESTAMPTZ NOT NULL,
    last_seen_at     TIMESTAMPTZ NOT NULL,
    first_session_id UUID REFERENCES engine.chat_sessions(id) ON DELETE SET NULL,
    first_message_id UUID REFERENCES engine.chat_messages(id) ON DELETE SET NULL,
    first_run_id     UUID,                   -- correlation id, not a reference
    last_session_id  UUID REFERENCES engine.chat_sessions(id) ON DELETE SET NULL,
    last_message_id  UUID REFERENCES engine.chat_messages(id) ON DELETE SET NULL,
    last_run_id      UUID
);
```

- **Normalization lives in SQL once.** A migration creates an immutable
  `engine.fact_norm(TEXT)` (NFKC via `normalize()`, lower-case, strip
  whitespace and punctuation); the write path, the dedup index and the
  backfill all call it, so Rust never re-implements it. `content_norm` is a
  generated column or written through the function — builder's choice.
- **Dedup key:** unique index on
  `(user_id, (evidence_type = 'roleplay_expression'), md5(content_norm))`.
  Exact-only dedup makes the hard constraints automatic: a negated, hedged
  or conditional variant normalizes to different text and stays its own row.
- **Attribution and FKs** follow the house rules: `user_id` has no FK
  (erasure is the endpoint's job, §8); session/message pointers are nullable
  with `ON DELETE SET NULL`; `run_id`s are correlation ids, not references.
  Every FK column gets an index. Supabase lockdown block as on every
  `engine.*` table.
- **Recurrence semantics.** On a dedup hit: `occurrences + 1`, `last_*`
  pointers and `last_seen_at` refreshed; `session_count + 1` only when the
  arriving session differs from the stored `last_session_id` (an
  approximation that can overcount alternation but detects
  "ever seen in ≥2 sessions" exactly, which is all the tier needs). The
  first occurrence's six metadata labels are kept; a recurrence does not
  relabel. The derived tier is: `once` (occurrences = 1),
  `repeated_in_session` (occurrences > 1, session_count = 1),
  `repeated_across_sessions` (session_count > 1).
- **One occurrence = one stage-1 run** that emitted the fact (group chat
  runs the extraction once per turn, so this holds there too). Duplicates
  within a single run's array count once.

### 3.2 Write path

`extract_insights` becomes: stage 1 → validate → ledger upsert → counter
bump (§4.2). The per-turn stage-2 LLM call and `apply_extraction` are
removed from the turn path, and everything that existed only for them is
deleted, not stranded: `COMPANION_INSIGHTS_SCHEMA`,
`extract_structured_insights_prompt`, `existing_as_extraction_json`, the
`insight_structuring` task constant (an `[tasks.insight_structuring]`
block still present in a deployment's config is ignored). The stage-1 audit row (`stage = 'facts'`) is
written exactly as today, payload now the object array as emitted.

Validation is engine-side Rust against hardcoded canonical vocabularies
(the lists the in-repo example extraction prompt documents, with the §5
changes applied): out-of-vocab `category`/`domain` fold to `other`,
out-of-vocab or missing `evidence_type`/`temporality`/`persistence`/
`confidence` fold to `unknown`, empty `content` is skipped. Legacy domains
arriving at write time (`politics`, `religion`) map to `ideology`;
`location_culture` folds to `other` like any other retired value. A
deployment that customizes the prompt's vocabulary beyond the canonical
lists loses granularity to `other`, documented, not an error.

## 4. The derived profile

### 4.1 State

`engine.human_profile_state` (`user_id` PK — attribution column, no FK;
`pending_facts INT NOT NULL DEFAULT 0`, `last_fact_at TIMESTAMPTZ`,
`last_rebuilt_at TIMESTAMPTZ`). After each run that wrote or bumped N ≥ 1
ledger facts: `pending_facts += N`, `last_fact_at = now()`.

### 4.2 Trigger

- **K-trigger:** when `pending_facts` reaches 20 after a run, that
  post-process task rebuilds inline (it already runs off the reply path).
- **Idle flush:** a sweeper (same shape as the snapshot sweeper, own
  disable env) scans every few minutes for `pending_facts > 0 AND
  last_fact_at < now() - 1 hour` and rebuilds those users, paced in small
  batches.
- **Outcome semantics.** An `ok` rebuild writes the row, zeroes
  `pending_facts`, sets `last_rebuilt_at`. An `empty` rebuild (valid JSON,
  nothing to write — including a refusal-shaped `{}`) also zeroes the
  counter: that batch is accepted as a residual, and the user's next facts
  re-arm the trigger. A failure (`parse_error`, transport error) keeps the
  counter, sets `last_fact_at = now()` so the idle flush waits another hour
  (a one-hour backoff with no new mechanism), and increments a
  `rebuild_failures` column; at three consecutive failures the counter is
  zeroed with a warning — one user with an unrebuildable ledger costs at
  most three calls per burst of activity, never a call per tick forever.
  `ok`/`empty` reset `rebuild_failures`. Concurrent rebuilds for one user
  are prevented by claiming the state row (builder's mechanism).

### 4.3 What a rebuild reads

The user's whole ledger rendered to text, grouped under `## <domain>`
headers, rows ranked by occurrence count then `last_seen_at`, cut to
16,000 characters (the budget counts everything the model reads —
metadata and headers included; at this budget ~98% of production users fit
whole). Each row is one line: the fact text; evidence type; persistence
(unless `unknown`); the recurrence tier with counts when seen more than
once; last-seen date.

### 4.4 Prompt and output

The rebuild prompt is engine-owned (`prompt.rs`), a new sibling of the
retired stage-2 prompt; the model/parameters live in a new model-config
task block (name the builder's, e.g. `[tasks.profile_rebuild]`). Rules the
prompt must carry:

- Format described with angle-bracket placeholders only — **no example
  values that could be copied back**; the engine additionally drops any
  output value equal to a placeholder, a bare type name (`string`), or a
  known placeholder word (「未知」 and kin) before writing.
- `love_values` and `emotional_needs`: at most three propositions each.
- `deal_breakers`: real-life dealbreakers only — complaints about the AI's
  style or in-scene reactions don't qualify.
- `kinks`: concrete play/roles, items sourced from roleplay marked as such
  inside the phrase; roleplay facts are barred from all real-life fields.
- `ideology`: may rest on behaviour shown in the facts (hedged wording),
  never on who the user is.
- `mental_health`: user self-report only, no diagnosis.
- `mbti_guess`: per decision 11.
- `transient` facts stay out of durable fields unless they recur across
  sessions.
- The flattened output schema has no `matching_preferences` nesting:
  `deal_breakers` moves to the top level.

`education_level` is emitted as `{level, status}` and stored as two
columns, both engine-validated enums (`secondary_or_below` / `associate` /
`bachelor` / `master` / `doctorate`; `enrolled` / `graduated`); anything
else is dropped. Unknown = absent, not a stored value.

### 4.5 Write

Full-row replace of the 2.0 fields; the seven retired columns are left
untouched (rollback safety, §10). `updated_at` bumps, which is what moves
the row through `GET /svc/v1/insights/changes`. Every rebuild writes one
audit row in `companion_insights_events` with a new `stage = 'profile'`
(CHECK extended by migration; `session_id`/`message_id` NULL — a rebuild
is not a turn), payload = the written JSON, status `ok`/`empty`/
`parse_error`.

## 5. Stage-1 contract and vocabulary

The engine owns the parse contract and the canonical vocab constants; the
prompt text stays deployment config. Changes the in-repo example prompt and
docs must reflect:

- **Output:** one `facts` array of objects
  (`content` + the six metadata fields). String items are ignored with a
  warning — a deployment still running the old prompt gets zero ledger
  writes and a loud log line, not silent junk.
- **Domains:** remove `location_culture`, `politics`, `religion`; add
  `kink` (split from `sexuality`), `mental_health` (split from `health`),
  `ideology` (absorbing politics/religion plus social-values stances,
  evidence may be `observed_behavior`).
- **Not extracted:** geography of any kind (and no inferring it from
  language), third-party names (relationship only).
- The stereotyping iron rule (no inference from age, gender, occupation,
  nationality, interests, or language used) and the no-diagnosis iron rule
  stay.

## 6. Prompt injection

`human_insights_to_bullets` (shared by chat and the voice opener):

- Remove the 城市 / 所在地 / 老家 / 国籍 lines in both modes.
- Add: `education_level`(+status) and `ideology` in both modes; `kinks`
  and `mental_health` in Full mode only (joining the intimate set).
- Matching-only columns stay unrendered (`deal_breakers`; the retired
  gender/age columns are gone from the row type).

`GET /comp/user/{user_id}/profile` (the JWT user reading their own
profile) gains the new fields — including the sensitive three; it is the
user's own data — and loses the retired seven.

## 7. Exports

- **`GET /svc/v1/insights/changes`** (existing secret, unchanged
  mounting): row shape loses the seven retired columns, gains
  `education_level` + `education_status`, and never carries `kinks` /
  `ideology` / `mental_health`. Breaking for consumers; migration guide
  covers it.
- **`GET /svc/v1/insights/deletions`** (existing secret): the same
  user-level tombstone stream as the facts one below. An erase deletes the
  profile row, which never surfaces in a `changes` feed, so a consumer
  mirroring profiles or memories needs this to drop the user. One table,
  two routes, one per secret.
- **Facts export, own secret** (new env, own header, min length and
  constant-time comparison as the existing one; sub-tree unmounted when
  unset — mounting is per-secret, not all-or-nothing):
  - `GET /svc/v1/facts/changes` — keyset pages over
    `(last_seen_at, id)`, row = `id`, `user_id`, `content`, the six
    metadata labels, `occurrences`, `session_count`, recurrence tier,
    `first_seen_at`, `last_seen_at`. Source pointers (session/message ids)
    stay internal.
  - `GET /svc/v1/facts/deletions` — keyset pages over
    `engine.user_erasures` (`user_id`, `erased_at`): user-level tombstones
    (2.0 has no per-fact deletion). A consumer that mirrors facts must
    honor these.
- The memories export is untouched.

## 8. Erasure

`POST /svc/v1/users/{user_id}/erase`, mounted only under its own secret
(erasure must work on deployments that never enable any export). One call,
idempotent, removes **everything in `engine.*` the engine derived about
this user** — the rule, not a list: every derived row attributed to the
`user_id` is deleted, and every audit/event row attributed to it keeps its
id and timestamps but loses its payload and free-text columns. The
builder enumerates by scanning the schema for attribution columns; the
known set today:

- **Deleted:** `human_facts`, `human_insights`, `human_insights_snapshot`,
  `human_profile_state`, `companion_memories` (profile and relationship
  layers), and `user_insights` rows owned by the user. Rows that already
  cascade from the caller's own session/instance deletion are still
  covered here, for callers that erase before (or instead of) deleting.
- **Blanked, rows kept:** payloads of `companion_insights_events`,
  `user_insights_events`, `character_insights_events`,
  `companion_decision_events`, the image/vision event tables; the text
  columns of the affinity event rows (feeling clauses). The trail of ids
  and timestamps stays; the words do not.
- **Written:** upsert into `engine.user_erasures (user_id PK, erased_at)`,
  which feeds both deletion streams (§7).

Chat history itself (`chat_messages`, sessions, instances) is the caller's
own deletion flow, not this endpoint's. The response reports per-table
counts. Tables added later that carry a `user_id` join the erase in the
same change that adds them — the test in §12 that asserts the known set is
where that shows up.

## 9. Backfill and switchover

One schema migration (§3/§4 tables, profile column adds, events CHECK),
then one backfill migration, idempotent and deployment-neutral (a fresh
deployment backfills nothing):

1. **Events → ledger.** Every `companion_insights_events` row with
   `stage = 'facts'`, `status = 'ok'` and a `details` array in its payload,
   for users that currently have a `human_insights` row. The filter exists
   because users already cleaned up by the deployment must not be
   resurrected from audit data, and the engine has no other liveness
   marker. Its cost is accepted: users whose stage 2 never produced a row
   (a handful on the measured deployment, a couple dozen facts) keep their
   facts in the audit table only, and start their ledger from their next
   turn. Grouped per
   `(user, evidence class, fact_norm)` with counters, first/last
   timestamps and pointers from the event rows. Domain mapping: `politics`
   / `religion` → `ideology`; `location_culture` rows are dropped; all
   others carry over. Runs predating the `details` format are skipped.
2. **Profile seeds.** Current values of the surviving profile columns
   become ledger rows with a dedicated evidence type (e.g.
   `legacy_profile`), array columns one row per item, after filtering out
   values byte-equal to the old schema's example strings, placeholder
   strings, and the retired columns entirely.
3. **Sexuality/health reclassification** (splitting out `kink` /
   `mental_health` on old rows) is a paid offline pass run by the
   deployment operator after upgrade; the ledger needs nothing beyond
   UPDATE access, and until it runs those rows keep their old domains.
4. **One full rebuild per user:** the migration sets `pending_facts` high
   enough on every user with a profile row; the idle-flush sweeper drains
   the queue at its own pace after deploy. Until a user's first rebuild,
   the old profile row keeps serving injection unchanged.

## 10. Rollback and versioning

- The seven retired columns stay in place this release, so rolling back to
  the previous image restores the old pipeline against intact tables; the
  old image must not run `migrate` (as previous migration guides state).
  New tables and the `profile` stage value are invisible to it.
- **v2.4.1 carries the destructive cleanup** once 2.4.0 has held in
  production: DROP of the seven retired columns, and RENAME of
  `engine.companion_insights_events` to `engine.human_insights_events` —
  the last `companion_insights` name left once stage 2 is gone (rows and
  ids unchanged). A 2.4.1 → 2.4.0 rollback loses audit writes on the old
  name (the insert is fail-open and only warns), nothing else; the 2.4.1
  guide states it alongside the usual "do not run the old image's
  `migrate`".
- Version: **v2.4.0**. Migration guide must
  cover: the `filter_prompt` contract change (object array; new/removed
  domains), the new model-config task block, the changed
  `/svc/v1/insights/changes` shape, the two new secrets/endpoints, the
  sweeper env, and the operator notes (reclassification pass; the
  one-time rebuild drain and its LLM cost — call count ≈ profile rows,
  reported by the migration).

## 11. Out of scope

- `memory_extraction`, the memory layers, and the character and user
  insight chains as pipelines (erasure, §8, is the one thing that touches
  their data).
- Per-fact user deletion, and any embedding/paraphrase merging on the
  ledger (only safe once per-occurrence text is kept — a future
  occurrence sub-table, not built now).
- Matching, geography, and any engine knowledge of who consumes the
  exports.
- Model and parameter choice for the rebuild task (deployment config).
- Dropping the retired columns and renaming the events table (v2.4.1).

## 12. Testing

- **Store, ledger:** upsert merges exact-normalized duplicates and keeps
  first labels; negation/degree/condition variants stay separate rows;
  roleplay never merges with non-roleplay; session_count and tier
  transitions; out-of-vocab folding; legacy domain mapping; run-internal
  duplicates count once; SET NULL on session/message delete.
- **Store, profile:** full-replace clears omitted fields and leaves
  retired columns untouched; placeholder/type-name/enum rejection;
  empty/parse_error writes nothing.
- **Trigger:** K boundary, idle eligibility, counter reset on success and
  retention on failure, single rebuild under concurrency.
- **Render:** domain grouping, ranking, budget cut, metadata line shape.
- **Server:** bullets per mode (lines removed/added); profile route shape;
  facts changes keyset across page boundaries; deletions stream; erase
  endpoint empties every table in §8, blanks payloads, upserts the
  tombstone, and is idempotent; each secret mounts only its own sub-tree;
  OpenAPI assertions for the new and changed paths.
- **Norm function:** pinned sample pairs (CJK + Latin, punctuation,
  width variants).
- **Backfill:** unit tests on the mapping; the full pass validated by a
  dry run against a copy of production data, operator-side.

## 13. Documentation

The 1.0 system is renamed **insights (legacy)** wherever it stays
mentioned. There is no standalone insights doc today — the pipeline is
described across memory-layers, api-reference, llm-audit, model-config and
architecture — so:

- New `docs/insights.md` + `.zh.md`: the canonical insights doc — fact
  ledger, derived profile, triggers, exports, erasure. Scattered mentions
  elsewhere link here.
- New `docs/insights_legacy.md`: the 1.0 two-stage pipeline collected in
  one place for deployments migrating from 1.x; **deleted after v2.4.1**
  (git history keeps it). English only.
- `docs/api-reference.md` + `.zh.md`: the changed insights export, the
  facts export, deletions, erase.
- `docs/llm-audit.md` + `.zh.md`: the `profile` stage, stage-2 retirement.
- `docs/model-config.md` + `.zh.md`: the rebuild task block; the stage-1
  output contract.
- `docs/deploying.md` + `.zh.md`: the new secrets and sweeper env.
- The in-repo example extraction prompt/config: §5 changes.
- `docs/migrating/` guide per §10.

## 14. Left to the builder

- All table/column/endpoint/env/task names sketched above.
- The state-row claiming mechanism for concurrent rebuilds, sweeper tick
  and batch size, and the exact "set pending high" switchover value.
- The norm function's precise punctuation set; md5 vs direct index on
  `content_norm`.
- Render line wording and date format; prompt wording (rules fixed by §4.4).
- Whether `human_profile_state` is its own table or columns elsewhere —
  provided `human_insights` rows can't exist without it being trackable
  for users who have facts but no profile yet.
- DTO/module placement, test fixtures and placement.
