# character_insights — the AI character's conversation-derived profile (experimental) — Design

- **Date:** 2026-08-15
- **Status:** Implemented
- **Type:** New subsystem — one new extraction chain, three new tables, one new
  read endpoint. Additive; no existing behaviour changes.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` 1.3.0
- **Mirrors:** [2026-08-11-companion-insights-teardown-design.md](2026-08-11-companion-insights-teardown-design.md)
  (the human-side chain this one is modelled on) and
  [2026-06-03-insight-events-and-audit-columns-design.md](2026-06-03-insight-events-and-audit-columns-design.md)
  (the audit-row contract).

## 1. Motivation

`human_insights` accumulates a profile of the real person from the conversation.
Nothing accumulates a profile of the **character**. The world system builds
something adjacent (`world_memories`, `world_worldviews`), but it is a heavy,
opt-in subsystem; a downstream product that never enables it has no way to know
what its characters have revealed.

The concrete downstream use is **"unlock character information as the story
progresses"**: the client shows the user what they have learned about her, and
that set grows as the relationship does. This spec builds the extraction
pipeline and the store, so the extraction quality can be measured on real
traffic before anything is built on top of it.

### Vocabulary — fixed by this spec

| Term | Means | In `chat_messages` |
|---|---|---|
| **human** | the real user | the `user` rows |
| **character** | the AI character | the `assistant` rows |
| **persona** | one *component* of a character (its authored genome) | — |

`persona_*` is therefore **not** the prefix for this feature: `persona_genomes`
/ `persona_instances` name the authored setup, not the character as it exists in
a relationship. `companion_*` is also unavailable — `companion_insights_events`
is live and holds **human**-side rows; that mismatch is exactly the naming
mistake this spec must not repeat.

## 2. What is in scope to extract — and what is deliberately not

The character has an authored genome; the human does not. That asymmetry is the
whole design:

> **The human has no genome, so everything must come from the conversation. The
> character has one, so only what the genome does *not* already contain is worth
> extracting.**

Any dimension whose source of truth is already written into
`persona_genomes.system_prompt` cannot be *extracted* — the extractor only ever
sees the turn text, so all it can produce is the model paraphrasing its own
backstory with embellishment. Persisting that produces **drift**, and the drift
then gets read back as fact. `appearance`, `background` and
`personality_traits` all have this disease and are **not** columns here. Read
the genome for those.

`occupation` is the deliberate exception and must not be "cleaned up" later as
redundant with the genome or with `world_memories`. Three *different* facts
coexist:

| Fact | Authority |
|---|---|
| the character's backstory job | `persona_genomes.system_prompt` |
| the job the world director assigned | `world_memories` |
| the job she actually holds in *this* relationship (the user handed her a job offer) | `character_insights.occupation` |

They are not three copies of one fact, so "one fact, one authority" is not
violated.

### The ten columns

| Column | Type | Meaning |
|---|---|---|
| `location` | `TEXT` | where she is right now — the office, the user's place, in transit |
| `occupation` | `TEXT` | the job she actually holds in this relationship (see above) |
| `current_situation` | `TEXT` | what is going on with her lately |
| `desires` | `TEXT` | what she wants, from the user and otherwise |
| `vulnerabilities` | `TEXT` | the soft spots she has let show |
| `habits` | `TEXT` | routines she has described |
| `personal_values` | `TEXT` | what she says she cares about |
| `likes` | `TEXT[]` | |
| `dislikes` | `TEXT[]` | |
| `relationships` | `TEXT[]` | people she has mentioned |

**`personal_values`, not `values`.** `VALUES` is a reserved word in Postgres; as
a column name it needs double-quoting in every hand-written statement, and this
codebase writes SQL by hand throughout.

The resulting table semantics are **"what she has revealed to you in this
relationship"**, not "what she is". That is also the better fit for
plot-gated unlocking: what gets unlocked is what the relationship bought.

## 3. Data model — migration `0047_character_insights.sql`

### 3.1 `engine.character_insights`

```sql
CREATE TABLE engine.character_insights (
    instance_id       UUID PRIMARY KEY
                      REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    location          TEXT,
    occupation        TEXT,
    current_situation TEXT,
    desires           TEXT,
    vulnerabilities   TEXT,
    habits            TEXT,
    personal_values   TEXT,
    likes             TEXT[] NOT NULL DEFAULT '{}',
    dislikes          TEXT[] NOT NULL DEFAULT '{}',
    relationships     TEXT[] NOT NULL DEFAULT '{}',
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

**Keyed on `instance_id`**, i.e. one row per (user × character) pair — a
`persona_instance` is `UNIQUE(genome_id, owner_uid)`, so the instance *is* the
relationship. Keying on `genome_id` instead would pool every user's roleplay
into one shared character profile: user A's invented detail would unlock for
user B, and characters' lines routinely carry the user's own content, so that
is a cross-user content leak, not just noise. `genome_id` is reachable by
joining `persona_instances` and is **not** stored here.

`ON DELETE CASCADE` mirrors `world_memories` (migration 0035): the profile is
meaningless without its relationship.

**No indexes.** `human_insights` carries GIN indexes on its array columns
because matching queries them by set overlap (`&&`); this table is only ever
read by primary key.

Supabase lockdown (`REVOKE` from `anon`/`authenticated` under `pg_roles`
existence guards, then policy-less `ENABLE ROW LEVEL SECURITY`) mirrors
migration 0013/0015 for all three tables below.

### 3.2 `engine.character_insights_events`

```sql
CREATE TABLE engine.character_insights_events (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    run_id        UUID NOT NULL,
    instance_id   UUID NOT NULL,
    session_id    UUID,
    message_id    UUID,
    stage         TEXT NOT NULL CHECK (stage IN ('extraction','structuring')),
    status        TEXT NOT NULL CHECK (status IN ('ok','empty','parse_error')),
    payload       JSONB,
    model         TEXT,
    usage         JSONB,
    generation_id TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_character_insights_events_instance_time
    ON engine.character_insights_events (instance_id, created_at DESC);
CREATE INDEX idx_character_insights_events_run
    ON engine.character_insights_events (run_id);
```

One row per OpenRouter call that returned a response; the two stages of a run
share a `run_id`. A call that never returned (transport error, timeout) writes
no row at all. Two deliberate omissions carried over from
`companion_insights_events`:

- **No FK on `instance_id`.** The audit trail is append-only and must survive
  the instance it describes.
- **No `owner_uid` column.** While the instance exists, `owner_uid` is derivable
  by joining `persona_instances`, and it never changes for a given instance.
  The missing FK has a consequence here, though: rows whose instance has since
  been deleted are no longer attributable, because the join has nothing to hit.
  That is acceptable while nothing reads these tables by owner; adding
  `owner_uid` is the fix if they ever need per-owner access or deletion.

**`stage` values name the config block**, not the human-side `'facts'` /
`'structured'`: seeing `stage='structuring'` tells you to go tune
`[tasks.character_insight_structuring]` with no lookup table in between (§5.4).

#### `payload` — what each row must carry

| stage | status | `payload` |
|---|---|---|
| `extraction` | `ok` / `empty` | `{"facts": [...], "details": [...]}` — the extractor's whole output |
| `structuring` | `ok` / `empty` | the model's JSON object, plus `_existing_keys` |
| either | `parse_error` | `{"raw": "<the unparseable reply, truncated to 2000 chars>"}` |

Two departures from the human-side chain, both aimed at the same blind spot —
**a fact the extractor mined and the structurer silently dropped**:

1. **`parse_error` records the raw reply** instead of `NULL`. The likeliest
   shape of "a fact got dropped" is not one field going missing, it is the
   structurer *refusing the whole turn*: the model emits refusal prose, nothing
   parses, and today the row says only "it failed". Keeping the text separates
   malformed-JSON from refusal from empty reply, and shows which content drew
   the refusal. This is not a new exposure surface — the `extraction` payload
   already stores facts mined verbatim from the conversation — and the table is
   RLS-locked to owner/`service_role`. Truncate at 2000 characters.

2. **`structuring` payload carries `_existing_keys`**: the names (never the
   values) of the columns that were already populated when the reverse-projected
   row was handed to the model, e.g. `["location", "likes", "desires"]`. The
   structurer's input is facts **plus** the existing profile; without knowing
   which fields arrived pre-filled, a fact that does not appear in the output is
   ambiguous between *dropped* and *judged already covered*. Key names alone
   settle it without storing the content twice.

Whether a given fact survived is otherwise answerable offline already: stage 1's
payload **is** stage 2's fact input, and both rows share a `run_id`.

Deliberately **not** added: asking the structurer to self-report which facts it
discarded (a model grading its own omissions, paid for in output tokens), and
recording the post-merge row (derivable from the payload and the deterministic
merge rules in §4.1).

### 3.3 `engine.character_insights_snapshot`

```sql
CREATE TABLE engine.character_insights_snapshot (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    instance_id UUID NOT NULL,
    snapshot    JSONB NOT NULL,
    captured_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX idx_character_insights_snapshot_instance_time
    ON engine.character_insights_snapshot (instance_id, captured_at DESC);
```

`snapshot` holds `to_jsonb(character_insights)` — the whole row, so each
snapshot is self-contained and later `ADD COLUMN`s need no snapshot migration.

**Written by `apply_extraction` itself, not by a sweeper.** There is no periodic
sweeper for this table in 1.3.0; the write path is the only writer, and it
appends one row per applied extraction (§4.1).

**Known cost, deferred:** this duplicates the full profile on every turn that
yields insights, which is precisely what the human side's periodic sweeper
exists to avoid. Acceptable at experiment volume; if it grows, the fix is a
retention policy or a change-detection guard, and neither is designed here.

This is the project's "open it up first, then ratchet the gate" posture, which
only works if the open step leaves a reading behind. The concrete queries that
would tell us it is time to tighten: `character_insights_events` grouped by
`status` (a rising `parse_error` or `empty` share means the chain is mostly
refusing or producing nothing, not that it's cheap and working); and
rows-per-day on `character_insights_snapshot` (the actual storage-growth
curve, to size a retention policy instead of guessing one).

## 4. Store — `crates/eros-engine-store/src/character_insight.rs`

One module holds both repos; they are born and retired together. Registered in
`eros-engine-store/src/lib.rs` next to `human_insight` and `insight`.

```rust
pub struct CharacterInsightsRow { /* instance_id + 10 columns + updated_at */ }
pub struct CharacterColumns     { /* the 10, owned, ready to bind */ }

pub fn project_columns(insights: &serde_json::Value) -> CharacterColumns;
pub fn existing_as_extraction_json(row: &CharacterInsightsRow) -> serde_json::Value;

pub struct CharacterInsightRepo<'a> { pub pool: &'a PgPool }
impl CharacterInsightRepo<'_> {
    pub async fn load(&self, instance_id: Uuid) -> Result<Option<CharacterInsightsRow>, sqlx::Error>;
    pub async fn apply_extraction(&self, instance_id: Uuid, insights: &serde_json::Value)
        -> Result<(), sqlx::Error>;
}

pub struct CharacterInsightEventInsert<'a> { /* mirrors InsightEventInsert, instance_id in place of user_id */ }
pub struct CharacterInsightEventRepo<'a> { pub pool: &'a PgPool }
impl CharacterInsightEventRepo<'_> {
    pub async fn record(&self, ev: CharacterInsightEventInsert<'_>) -> Result<(), sqlx::Error>;
}
```

`project_columns` reads all ten keys from the JSON **top level** — there is no
nested object here, so none of `human_insight.rs`'s `matching_preferences` /
`parse_age_range` machinery is carried over. Non-string array items are dropped
rather than erroring, matching `str_array`.

`existing_as_extraction_json` is the inverse, emitting only populated fields
(NULL scalars and empty arrays omitted), and feeds the stage-2 prompt.

### 4.1 `apply_extraction` — merge semantics and the snapshot write

Merge semantics are **identical** to `HumanInsightRepo::apply_extraction`:
extracted scalars overwrite, absent/null scalars keep the stored value, arrays
overwrite only when the extraction produced a non-empty array, and there is
deliberately **no *explicit* erase path**: absent and null scalars keep the
stored value. An empty-string scalar DOES overwrite — `COALESCE` only treats
`NULL` as "keep the old value", and `""` is non-NULL — matching the human
chain's behaviour exactly. One statement, no read-modify-write, so concurrent
extractions degrade to column-level last-write-wins rather than whole-row.

The snapshot append rides the same statement via a CTE, so it stays atomic with
the upsert and costs no extra round trip:

```sql
WITH upserted AS (
    INSERT INTO engine.character_insights (instance_id, location, ...)
    VALUES ($1, $2, ...)
    ON CONFLICT (instance_id) DO UPDATE SET
        location        = COALESCE(EXCLUDED.location, character_insights.location),
        ...
        likes           = CASE WHEN EXCLUDED.likes = '{}'
                               THEN character_insights.likes ELSE EXCLUDED.likes END,
        ...
        updated_at      = now()
    RETURNING *
)
INSERT INTO engine.character_insights_snapshot (instance_id, snapshot, captured_at)
SELECT instance_id, to_jsonb(upserted), now() FROM upserted;
```

## 5. Extraction chain

### 5.1 Shape

Two stages, each with **its own config block** — the one structural departure
from the human chain, which serves both stages from a single
`[tasks.insight_extraction]` section:

1. **extraction** — system message from
   `character_insight_extraction.filter_prompt`, user message from the existing
   `prompt::facts_user_message` (`用户: … / AI: …`). Parsed for `facts` (and the
   opaque `details` sibling), audited as `stage='extraction'`.
2. **structuring** — prompt built in `prompt.rs` from the stage-1 facts plus the
   reverse-projected existing row, output parsed as a JSON object matching the
   ten-column schema, audited as `stage='structuring'`, then applied.

Each call carries its own `task` name on the wire
(`character_insight_extraction` / `character_insight_structuring`), where the
human chain reports `insight_extraction` for both. That is most of the point of
splitting: OpenRouter accounting and `[[providers.*.body]]` rules can finally
tell the two apart, and `max_tokens` stops being one number covering two very
different outputs (the human block's `1200` is a combined budget).

`status` follows the same `ok` / `empty` / `parse_error` rules as the human side.

### 5.2 Wiring — `pipeline/post_process.rs`

`run()` gains a fourth future, `fut_character_insight`, joined concurrently with
the existing three:

```rust
let fut_character_insight = async {
    for m in &produced {
        if !user_msg.is_empty() && !m.full_text.is_empty() {
            extract_character_insights(
                &state, session_id, instance_id, m.message_id,
                &user_msg, &m.full_text, client_id.as_deref(),
            ).await;
        }
    }
};
tokio::join!(fut_insight, fut_memory, fut_affinity, fut_character_insight);
```

`instance_id` is already a `run()` parameter. The trigger condition is copied
verbatim from the human chain, including the per-produced-message loop.

Every failure is fail-open and warn-only: an audit insert, a load, or an apply
that fails must never break the turn.

### 5.3 The extractor never sees the genome

Stage 1 gets the turn text only. Feeding `system_prompt` in as an
"already-known, do not output" exclusion list was considered and rejected: it
hands the model the very text it is most likely to paraphrase back as an
extraction result, and costs a full system prompt of input tokens every turn.
The `genome`-exclusion rule lives in the prompt instruction instead — *extract
only concrete present-tense information actually said this turn; do not
summarise the character's settings.*

Stage 2 does receive the existing `character_insights` row (reverse-projected),
exactly as the human chain does. That is the character's own history, not the
genome.

### 5.4 Config — `crates/eros-engine-llm/src/model_config.rs`

```rust
/// Stage 1. Prompt-bearing, so the existing extract resolver applies.
pub fn resolve_character_insight_extract(&self) -> Option<ResolvedExtract> {
    self.resolve_extract("character_insight_extraction")
}

/// Stage-2 parameters: the dedicated block when present, else stage 1's.
/// NEVER falls through to global defaults — an absent stage-2 section means
/// "same model as stage 1", not "whatever FALLBACK_MODEL happens to be".
pub fn resolve_structuring(&self, stage2: &str, stage1: &str) -> ResolvedModel {
    let name = if self.tasks.contains_key(stage2) { stage2 } else { stage1 };
    self.resolve(name, None)
}
```

`resolve_structuring` exists because of a real trap: `resolve(task, None)` on an
**absent** task logs one `warn!` and falls through to `defaults.fallback_model` /
`FALLBACK_MODEL`. Calling it directly on the stage-2 name would therefore turn
"I only configured one block" into a silent model swap. With the explicit
fallback, one block reproduces today's behaviour exactly and two blocks tune the
stages independently — so downstream upgrades with zero config changes.

**Stage 2's block carries no `filter_prompt`.** Its prompt is built in
`prompt.rs` and is not configurable; the precedent is
`[tasks.affinity_evaluation]`, which the shipped example deliberately ships
without a prompt (and a test asserts it stays that way). Consequently:

- `KNOWN_CHAT_TASKS` gains **both** `"character_insight_extraction"` and
  `"character_insight_structuring"`.
- `validate_extraction_prompts` gains **only** `character_insight_extraction`:
  present-with-blank-`filter_prompt` refuses boot, absent means the feature is
  off. The stage-2 block has no prompt to validate.

**The stage-1 block is the entire on/off switch.** No new flag, no new env var:
`resolve_character_insight_extract()` returning `None` skips the whole chain
(both stages), reusing the mechanism already in place for `insight_extraction`.
A stage-2 block present without stage 1 is dead config that does nothing —
acceptable, since stage 2 cannot run without stage 1's facts.

### 5.5 Cost

Two extra OpenRouter calls per produced message per turn — insight-side spend
roughly doubles when enabled. `examples/model_config.toml` ships **both blocks**
enabled, with a comment stating that the feature is experimental and what it
costs, and with `max_tokens` set per stage rather than as one combined budget.
The example file is a template to copy, not anyone's running config, so
shipping it live is what gives the feature a baseline reading while the gate
stays in the operator's hands.

## 6. Read path

```
GET /comp/instance/{instance_id}/profile
```

- `PersonaRepo::load_instance_gate(instance_id)` supplies `owner_uid`; it must
  equal the JWT `sub`, else **403**. `None` (missing, or `status <> 'active'`)
  is **404**.
- Response is `CharacterProfileResponse`, a flat typed DTO over the ten columns,
  `#[derive(ToSchema)]` + `#[utoipa::path]`, registered on the `companion`
  router alongside `get_profile`.
- An instance with no row yet returns all-null fields and
  `updated_at: null` — the same convention as `GET /comp/user/{user_id}/profile`.

The endpoint exists because the alternative is downstream reading `engine.*`
directly, and the cross-repo rule grants exactly one such exemption (affinity),
which this is not.

## 7. Explicitly not in this release

- **No injection anywhere.** `character_insights` is not read by prompt
  building, the chat pipeline, voice, PDE, or the world system. Human-side
  insight injection is itself not yet stable, and character-side injection would
  feed the character's own past behaviour back into her next turn — a textbook
  echo loop. The table is written and read by the endpoint; that is all.
- **No snapshot sweeper**, and no retention policy (§3.3).
- **No coupling to the world system.** No mutual exclusion, no conditional skip
  when a world is enrolled: that would cost a lookup every turn to express
  something an operator can already say by removing the task block. The two
  overlap in subject matter and are independent in mechanism.
- **No change to `companion_insights_events`.** It is live, downstream and audit
  scripts read it, and renaming it to match the human/character vocabulary would
  be a breaking change bought for tidiness.

## 8. Testing

**Store (`sqlx::test`, real Postgres):**
- migration creates all three tables with every column; the reserved-word
  avoidance is proven by the plain unquoted `personal_values` in every query.
- `apply_extraction` incremental semantics: scalar overwrite, scalar absent
  keeps stored, array overwrite only when non-empty, no erase.
- `apply_extraction` appends exactly one snapshot row per call, and the snapshot
  contains the **post-merge** state.
- `ON DELETE CASCADE` removes the profile with its instance, while events and
  snapshots survive.
- `CharacterInsightEventRepo::record` round-trips `payload` / `usage` /
  `generation_id` so a bind-order swap fails here rather than at the database.

**Pure unit:**
- `project_columns` / `existing_as_extraction_json` round-trip; non-string array
  items dropped; unknown JSON keys ignored.
- stage-2 prompt renders all ten field names and carries the anti-attribution
  clause (the schema describes the **character**, never the human — the mirror
  of the human prompt's existing clause).
- `validate_extraction_prompts` fails boot on a present-but-blank
  `character_insight_extraction.filter_prompt`, and passes when absent.
- `resolve_character_insight_extract()` is `None` when the task is absent or its
  prompt is blank.
- `resolve_structuring` returns the **stage-1** model when the stage-2 block is
  absent, and the stage-2 model when present. The absent case must assert the
  stage-1 model id specifically — asserting "not empty" would pass on the
  global-default fall-through this method exists to prevent.
- `parse_error` truncates a >2000-char reply and stores it under `raw`;
  `_existing_keys` lists exactly the populated columns and no values.

**Server:**
- route returns 403 on owner mismatch, 404 on unknown/archived instance,
  all-null body for an instance with no row.
- OpenAPI snapshot regenerated.
- the shipped `examples/model_config.toml` parses and resolves the new task
  (the existing example-config test gains this assertion).

Pre-PR gate, all four: `cargo fmt --check`, `clippy`, `test`, OpenAPI
regeneration check.

## 9. Follow-up — porting the split back to the human chain

The two-block split (§5.4) and the audit additions (§3.2) are wanted on the
human chain too, where the standing complaint is that extraction comes out
**too thin** — and the readings needed to tune that are exactly what one merged
block and a `NULL`-on-`parse_error` payload hide.

**Deliberately a separate PR.** This spec is purely additive; the human chain is
live in production, so touching it carries regression surface that should not
ride along with a new feature.

Names here are already chosen so that port needs no renaming, and in particular
so that `insight_extraction` — which downstream configs set today — never has to
change:

| | stage 1 | stage 2 |
|---|---|---|
| character (this spec) | `character_insight_extraction` | `character_insight_structuring` |
| human (follow-up) | `insight_extraction` *(unchanged)* | `insight_structuring` |

`resolve_structuring(stage2, stage1)` is written parameterised over both names
for the same reason: the human port calls it, it is not copied.

`companion_insights_events.stage` keeps its existing `'facts'` / `'structured'`
values — they are live data. The port records the mapping to
`'extraction'` / `'structuring'` rather than rewriting history.

## 10. Version

Target `eros-engine` **1.3.0** — additive schema and a new endpoint, no breaking
change. Release mechanics (version strings, OpenAPI, README docker pin, signed
tag, GHCR) follow the standing release checklist and are not part of this spec.
