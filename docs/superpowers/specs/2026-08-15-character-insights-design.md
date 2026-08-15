# character_insights — the AI character's conversation-derived profile (experimental) — Design

- **Date:** 2026-08-15
- **Status:** Draft
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
    stage         TEXT NOT NULL CHECK (stage IN ('facts','structured')),
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
share a `run_id`. Same contract as `companion_insights_events`, including its
two deliberate omissions:

- **No FK on `instance_id`.** The audit trail is append-only and must survive
  the instance it describes.
- **No `owner_uid` column.** Derivable by joining `persona_instances`, whose
  `owner_uid` never changes for a given instance.

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
deliberately **no erase path**. One statement, no read-modify-write, so
concurrent extractions degrade to column-level last-write-wins rather than
whole-row.

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

Two stages, mirroring `insight_extraction` exactly, both served by **one**
config block:

1. **facts** — system message from `character_insight_extraction.filter_prompt`,
   user message from the existing `prompt::facts_user_message` (`用户: … / AI: …`).
   Parsed for `facts` (and the opaque `details` sibling), audited as
   `stage='facts'`.
2. **structured** — prompt built in `prompt.rs` from the stage-1 facts plus the
   reverse-projected existing row, output parsed as a JSON object matching the
   ten-column schema, audited as `stage='structured'`, then applied.

Both calls carry `task: Some("character_insight_extraction")`. `status` is
`ok` / `empty` / `parse_error` on the same rules as the human side; a call that
never returned (transport error / timeout) writes no row at all.

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
pub fn resolve_character_insight_extract(&self) -> Option<ResolvedExtract> {
    self.resolve_extract("character_insight_extraction")
}
```

- `KNOWN_CHAT_TASKS` gains `"character_insight_extraction"`.
- `validate_extraction_prompts` gains it too: a section that is **present** with
  a blank `filter_prompt` refuses boot, an **absent** section means the feature
  is simply off. Same failure mode as the other two, same treatment.

**The task block is the entire on/off switch.** No new flag, no new env var:
`resolve_character_insight_extract()` returning `None` skips the whole chain,
reusing the mechanism already in place for `insight_extraction`.

### 5.5 Cost

Two extra OpenRouter calls per produced message per turn — insight-side spend
roughly doubles when enabled. `examples/model_config.toml` ships the block
**enabled**, with a comment stating that it is experimental and what it costs.
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

**Server:**
- route returns 403 on owner mismatch, 404 on unknown/archived instance,
  all-null body for an instance with no row.
- OpenAPI snapshot regenerated.
- the shipped `examples/model_config.toml` parses and resolves the new task
  (the existing example-config test gains this assertion).

Pre-PR gate, all four: `cargo fmt --check`, `clippy`, `test`, OpenAPI
regeneration check.

## 9. Version

Target `eros-engine` **1.3.0** — additive schema and a new endpoint, no breaking
change. Release mechanics (version strings, OpenAPI, README docker pin, signed
tag, GHCR) follow the standing release checklist and are not part of this spec.
