# `user_insights`, the human-chain structuring split, and the v2 API convention — Design

- **Date:** 2026-08-22
- **Status:** Approved, not yet implemented
- **Type:** New subsystem (one extraction chain, three tables) + a change to a
  live chain + an API convention with two new endpoints
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` 1.6.0
- **Mirrors:** [2026-08-15-character-insights-design.md](2026-08-15-character-insights-design.md)
  — this spec is that one applied to the human side of the same relationship,
  plus the follow-up its §9 deliberately deferred.

## 1. What ships together, and why

Three changes, one release:

1. **`user_insights`** — a new extraction chain and three new tables, the
   symmetric counterpart to `character_insights`. Purely additive.
2. **The human chain gets the structuring split** — exactly the follow-up
   described in the character spec's §9, which that spec deliberately kept out
   of a purely-additive PR.
3. **A v2 API convention**, and the two read endpoints that are its first
   citizens.

They ship together because 1 cannot be read without 3, and 2 consumes the same
shared audit helpers that 1 lifts to the crate root (§2.4). Split across two
passes, those functions get moved twice and `extract_structured_insights` gets
rewritten twice.

### Cost

Insight-side calls go from **4 to 6 per produced message per turn** (human
extraction + human structuring, character extraction + character structuring,
user extraction + user structuring).

The marginal cost of those two extra calls is not what it would have been a
year ago: average token prices dropped once `deepseek-v4-flash-0731` landed,
and the insight chain already routes its fallbacks through that class of model.
Two more small-model calls per turn is not a material line item.

That is the project's "default-open, then ratchet the gate" posture, and it
only counts if the open step leaves a reading behind. The two queries that say
it is time to tighten:

- `engine.user_insights_events` grouped by `status` — a rising `parse_error` or
  `empty` share means the chain is refusing or producing nothing, not that it is
  cheap and working.
- rows-per-day on `engine.user_insights_snapshot` — the storage-growth curve,
  to size a retention policy instead of guessing one.

## 2. `user_insights`

### 2.1 Keyed on `instance_id`, not `user_id`

The table's semantics are **"what this user has revealed inside *this*
relationship"** — not "who this user is". The latter already has an authority:
`human_insights`, keyed on `user_id`, which feeds chat context injection and
user↔user matching and is **not touched by this spec**.

Those are two different facts about the same person, so storing both does not
violate one-fact-one-authority:

| Fact | Authority | Scope | Read by |
|---|---|---|---|
| who this user is | `engine.human_insights` | global, one row per user | prompt injection, matching |
| what this user revealed in this relationship | `engine.user_insights` | one row per `persona_instances.id` | the v2 read endpoint only |

Keying on `instance_id` also makes `user_insights` and `character_insights`
row-for-row symmetric: one instance, two rows, the two sides of one
relationship. The read endpoints in §5 are the same shape for the same reason.

A `user_id` key was considered and rejected: it would put `occupation`,
`location` and `likes` for the same human in two tables with no scope
distinction between them, and one of the two would be wrong before long.

### 2.2 The name collides with the vocabulary table, deliberately

The character spec's §1 fixes `human` = the real person and `character` = the
AI. `user_insights` therefore reads as a violation. It is a deliberate one: the
two tables are distinguished by **scope** (in-relationship vs global), not by
subject, and `human_insights` already owns the global slot under the correct
word. Renaming `human_insights` to free the name is a breaking change to a live
table bought for tidiness, which the character spec already declined for
`companion_insights_events` on the same reasoning.

The endpoint vocabulary in §4.4 pins `user` to the real person, so the wire
surface stays consistent with the table.

### 2.3 Tables — migration `0055_user_insights.sql`

A literal mirror of `0047_character_insights.sql`, `character` → `user`. All
three tables, all the same columns, the same omissions.

```sql
CREATE TABLE engine.user_insights (
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

- **`personal_values`, not `values`** — same reserved-word constraint, same
  reason: this codebase writes SQL by hand and a quoted column name would have
  to be quoted everywhere.
- **No indexes.** Read by primary key only. (`human_insights` carries GIN
  indexes because matching queries its arrays by `&&`; nothing queries this
  table that way.)
- **`ON DELETE CASCADE`** — the profile is meaningless without its relationship,
  and `DELETE /comp/instance/{instance_id}/sessions` already relies on this
  shape to give a genuinely cold restart.

```sql
CREATE TABLE engine.user_insights_events (
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

CREATE INDEX idx_user_insights_events_instance_time
    ON engine.user_insights_events (instance_id, created_at DESC);
CREATE INDEX idx_user_insights_events_run
    ON engine.user_insights_events (run_id);
```

One row per OpenRouter call that returned a response; the two stages of a run
share `run_id`; a call that never returned writes no row. No FK on
`instance_id` (the trail outlives the instance) and no `owner_uid` column
(derivable by joining `persona_instances` while the instance exists) — both
carried over with the same consequence noted in the character spec: rows whose
instance was deleted are no longer attributable.

`stage` values name the config block, matching the character chain.

```sql
CREATE TABLE engine.user_insights_snapshot (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    instance_id UUID NOT NULL,
    snapshot    JSONB NOT NULL,
    captured_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX idx_user_insights_snapshot_instance_time
    ON engine.user_insights_snapshot (instance_id, captured_at DESC);
```

`snapshot` holds `to_jsonb(user_insights)` — the whole row, so later
`ADD COLUMN`s need no snapshot migration. Written by `apply_extraction` itself;
there is no sweeper.

**Known cost, deferred — now doubled.** The character side already duplicates a
full profile row on every turn that yields insights; this adds a second one.
Acceptable at experiment volume, and §1 names the query that says when it is
not.

Supabase lockdown for all three tables: `REVOKE` from `anon` / `authenticated`
under `pg_roles` existence guards, then policy-less
`ENABLE ROW LEVEL SECURITY`.

### 2.4 Store — `crates/eros-engine-store/src/user_insight.rs`

A copy of `character_insight.rs` with the types renamed:

```rust
pub struct UserInsightsRow { /* instance_id + 10 columns + updated_at */ }
pub struct UserColumns     { /* the 10, owned, ready to bind */ }

pub fn project_columns(insights: &serde_json::Value) -> UserColumns;
pub fn existing_as_extraction_json(row: &UserInsightsRow) -> serde_json::Value;

pub struct UserInsightRepo<'a> { pub pool: &'a PgPool }
impl UserInsightRepo<'_> {
    pub async fn load(&self, instance_id: Uuid) -> Result<Option<UserInsightsRow>, sqlx::Error>;
    pub async fn apply_extraction(&self, instance_id: Uuid, insights: &serde_json::Value)
        -> Result<(), sqlx::Error>;
}

pub struct UserInsightEventInsert<'a> { /* run_id, instance_id, session_id, message_id, stage, status, payload, meta */ }
pub struct UserInsightEventRepo<'a> { pub pool: &'a PgPool }
impl UserInsightEventRepo<'_> {
    pub async fn record(&self, ev: UserInsightEventInsert<'_>) -> Result<(), sqlx::Error>;
}
```

Merge semantics in `apply_extraction` are identical to both existing chains:
extracted scalars overwrite, absent/null scalars keep the stored value (an
empty-string scalar **does** overwrite — `COALESCE` only treats `NULL` as
"keep"), arrays overwrite only when the extraction produced a non-empty array,
and there is no explicit erase path. One statement, no read-modify-write, so
concurrent extractions degrade to column-level last-write-wins. The snapshot
append rides the same statement via a CTE.

**No generic abstraction over the character and user modules.** Two
near-identical files is the correct amount of duplication here — three similar
copies beat one premature abstraction, and the two tables are free to diverge
(the character side may gain plot-gating columns the user side never wants).

**One shared-helper move.** `parse_error_payload`, `existing_keys` and
`RAW_PAYLOAD_MAX_CHARS` currently live in `character_insight.rs` and are pure
functions with nothing character-specific in them. All three chains now need
them, and the human chain importing from `character_insight` would be
misleading. Move them to `eros-engine-store/src/lib.rs`, next to
`OpenRouterCallMeta`, which is already the crate-root home for exactly this kind
of shared audit type. Update the two existing call sites; no behaviour change.

### 2.5 Extraction chain — `pipeline/post_process.rs`

`run()` gains a fifth future, joined with the existing four:

```rust
let fut_user_insight = async {
    for m in &produced {
        if !user_msg.is_empty() && !m.full_text.is_empty() {
            extract_user_insights(
                &state, session_id, instance_id, m.message_id,
                &user_msg, &m.full_text, client_id.as_deref(),
            ).await;
        }
    }
};
tokio::join!(fut_insight, fut_memory, fut_affinity, fut_character_insight, fut_user_insight);
```

`instance_id` is already a `run()` parameter. The trigger condition is copied
verbatim from the two existing chains, including the per-produced-message loop.

**Stage 1 — `user_insight_extraction`.** System message from the config block's
`filter_prompt`, user message from the existing `prompt::facts_user_message`
(`用户: … / AI: …`). Parsed for `facts` (and the opaque `details` sibling),
audited as `stage='extraction'`.

The prompt's job is the mirror of the character extractor's: mine only concrete
information the **user** actually stated this turn. It is a separate block from
`insight_extraction` precisely because the two want different things — the
human chain's stage 1 is tuned for `city` / `mbti_guess` / `interests`, while
this one is after `current_situation` / `desires` / `vulnerabilities` /
`habits`. Sharing one extraction and fanning out to two structurers was
considered and rejected: it saves one call, couples a new experiment to a chain
that is live in production, and hands stage 2 facts that were never mined for
its columns.

**Stage 2 — `user_insight_structuring`.** Prompt built in `prompt.rs` as
`extract_user_insights_prompt(facts, existing)` from the stage-1 facts plus the
reverse-projected existing row. Output parsed as a JSON object over the ten
columns, audited as `stage='structuring'`, then applied.

The anti-attribution clause is the **inverse** of the character prompt's: the
schema describes the *user*, never the character. Both prompts need one, and
they point in opposite directions.

Parameters come from `resolve_structuring("user_insight_structuring",
"user_insight_extraction")` — the existing method, which falls back to stage 1's
block rather than to global defaults. Nothing new is needed there.

`payload` follows the character contract exactly:

| stage | status | `payload` |
|---|---|---|
| `extraction` | `ok` / `empty` | `{"facts": [...], "details": [...]}` |
| `structuring` | `ok` / `empty` | the model's JSON object, plus `_existing_keys` |
| either | `parse_error` | `{"raw": "<reply, truncated to 2000 chars>"}` |

Every failure is fail-open and warn-only: an audit insert, a load, or an apply
that fails must never break the turn. The post-structuring `session_still_live`
re-check that the character chain performs before applying is carried over for
the same reason — two LLM calls is long enough for the archive endpoint to land
in between, and re-creating a row for an archived instance would undo it.

### 2.6 Config — `crates/eros-engine-llm/src/model_config.rs`

- `KNOWN_CHAT_TASKS` gains `"user_insight_extraction"`,
  `"user_insight_structuring"` and `"insight_structuring"` (the last from §3).
- `validate_extraction_prompts` gains `"user_insight_extraction"`:
  present-with-blank-`filter_prompt` refuses boot; absent means the feature is
  off.
- `validate_structuring_prompt_unset` currently hard-codes the single constant
  `"character_insight_structuring"`. It becomes a loop over three names —
  `character_insight_structuring`, `user_insight_structuring`,
  `insight_structuring` — each of which refuses boot if a `filter_prompt` is
  set, in any shape including blank. Structuring prompts are engine-owned
  because they must stay in lockstep with the columns they fill; a key that is
  read by nothing is exactly the dead config this gate exists to prevent.
- `resolve_user_insight_extract()` → `self.resolve_extract("user_insight_extraction")`,
  mirroring `resolve_character_insight_extract`.
- **The stage-1 block is the whole on/off switch.** No new flag, no new env var:
  `resolve_user_insight_extract()` returning `None` skips both stages. A stage-2
  block present without stage 1 is dead config that does nothing.
- `examples/model_config.toml` ships **both** blocks enabled, with a comment
  stating what the feature costs and that nothing reads it back. The example
  file is a template to copy, not anyone's running config, so shipping it live
  is what gives the feature a baseline reading while the gate stays with the
  operator.

## 3. The human chain gets the structuring split

This is the character spec's §9, executed. The standing complaint about the
human chain is that extraction comes out **too thin**, and the readings needed
to tune that are precisely what one merged config block and a `NULL`-on-parse-
error payload hide.

### 3.1 What changes

Three edits in `extract_structured_insights`, plus one config block. **No schema
change** — `companion_insights_events.payload` is already nullable `JSONB`.

1. **A dedicated `[tasks.insight_structuring]` block.** The call switches from
   `resolve(INSIGHT_TASK, None)` to
   `resolve_structuring("insight_structuring", "insight_extraction")`, and the
   wire `task` becomes `insight_structuring`.

   With the block absent, parameters resolve to stage 1's — byte-identical to
   today's behaviour. The one behaviour change that lands regardless is the wire
   task name, and that is the entire point of the split: OpenRouter accounting
   and `[[providers.*.body]]` rules can finally tell the two stages apart, and
   `max_tokens` stops being one number covering two very different outputs (the
   human block's `1200` is a combined budget).

2. **`parse_error` records the raw reply** via `parse_error_payload`, instead of
   `NULL`. The likeliest shape of "a fact got dropped" is not one field going
   missing, it is the structurer refusing the whole turn — refusal prose,
   nothing parses, and today the row says only that it failed. Keeping the text
   separates malformed-JSON from refusal from empty reply. Not a new exposure
   surface: the `facts` payload already stores facts mined verbatim from the
   conversation, and the table is RLS-locked to owner / `service_role`.

   `parse_error_payload`'s doc comment currently reads "The human chain stores
   NULL here" — it stops being true with this change and must be updated in the
   same edit that moves the function (§2.4).

3. **`_existing_keys` on the `structured` payload** — the names, never the
   values, of the columns already populated when the reverse-projected row was
   handed to the model. Without it, a fact absent from the output is ambiguous
   between *dropped* and *judged already covered*. The human chain already
   passes `existing` into the prompt, so this is one `insert` on the audited
   clone.

### 3.2 What deliberately does not change

**`companion_insights_events.stage` keeps `'facts'` / `'structured'`.** Aligning
it with the character chain's `'extraction'` / `'structuring'` would mean a
migration on a live table, a `CHECK` swap with a backfill window during which
old and new writers collide, and downstream audit scripts that read this table
by stage. The character spec already declined to rename this table for
tidiness; renaming its values is the same trade with the same answer.

Two stage vocabularies therefore coexist. That is an accepted, recorded cost,
not an oversight: `companion_insights_events` uses `facts` / `structured`, and
both `*_insights_events` tables use `extraction` / `structuring`.

Nothing else about the human chain moves: not `human_insights`' columns, not its
merge semantics, not prompt injection, not the matching path, not
`/comp/user/{user_id}/profile`.

## 4. The v2 API convention

The HTTP surface was built without one. It carries three spellings of a version
prefix, path segments that name a key type in some places and a resource in
others, one position (`/comp/chat/{uuid}/`) that holds two different entity
types depending on the leaf, and `profile` used for two different tables with
two different subjects.

**Operating principle, fixed by this spec: everything outside `/v2/` is frozen.**
No renames, no `deprecated` markers, no mirroring of v1 endpoints into v2. The
convention is not retroactive; it governs `/v2/` and nothing else.

**v1 is not technical debt.** It is a delivered, published contract that
consumers are calling right now, and it works. The list in §8 is a description
of what the frozen surface looks like, so that a reader who notices an
inconsistency knows it was seen and settled — it is not a backlog, not a
cleanup queue, and not a set of TODOs. Nothing in it is to be "tidied up" by a
later change, human or agent. The only correct action on a v1 endpoint is to
leave it alone.

### 4.1 Scope

- **Governs:** endpoints under `/v2/`.
- **Frozen, ungoverned:** `/comp/*`, `/world/*`, `/persona/*`, `/healthz`.
- **`/bff/*` — one rule fixed here, everything else deferred.** The BFF tree is
  the downstream aggregation surface and versions independently of the domain
  API. Fixed now: **its version segment sits after the tree — `/bff/v<N>/…`,
  never `/v<N>/bff/…`.** That position is settled and not reopenable.

  Nothing else about a future `/bff/v2` is decided here — not its path grammar,
  not its vocabulary, not whether it adopts §4.2 at all. That convention gets
  written when a `/bff/v2` is actually wanted, with the BFF's own consumers in
  the room. **Do not assume this document governs it.**

v1 and v2 forms of one resource may coexist; the v1 form stays frozen in place.

### 4.2 Path grammar

```
/v2/<tree>/<entity>/{<entity>_id}/<resource>[/<sub-resource>]   read
/v2/<tree>/<entity>/{<entity>_id}/<resource>/<action>           action
```

- **`<tree>` is a consumer surface, not a domain concept.** Three exist:
  `comp` (the companion domain API), `world`, `persona`. **Do not open a new
  top-level tree for a read endpoint** — `insight` is a resource inside `comp`,
  not a tree.
- **`<entity>` names the entity the id belongs to, and shares a word stem with
  the parameter**: `user/{user_id}`, `instance/{instance_id}`,
  `session/{session_id}`, `message/{message_id}`.
- **One position, one entity type.** The v1 shape where
  `/comp/chat/{session_id}/history` and `/comp/chat/{user_id}/sessions` share a
  path position is forbidden in v2.
- **`chat` is not an entity segment.** In v1 it is an alias for the session; in
  v2 a session is spelled `session`.

### 4.3 Methods and resources

| Shape | Rule |
|---|---|
| read | `GET` + noun. Singular = one object, plural = a collection |
| action | `POST .../<resource>/<action>`, `<action>` is a verb (`start`, `read`, `interrupt`, `compose`, `async`) |
| delete | `DELETE` + noun, never a verb leaf |

`POST` to a bare noun to mean an action is forbidden — the path has to say what
happened.

### 4.4 Vocabulary — one word, one thing

| Word | Means | Authority |
|---|---|---|
| `user` | the real person | `auth.users.id` |
| `instance` | one relationship (user × character) | `persona_instances.id` |
| `session` | one conversation | `chat_sessions.id` |
| `message` | one message | `chat_messages.id` |
| `insight` | a row in the `*_insights` family | `engine.*_insights` |
| `character` / `human` | the AI / the real person | 2026-08-15 spec §1 |

**`profile` is retired in v2.** In v1 it names both `human_insights`
(`/comp/user/{user_id}/profile`) and `character_insights`
(`/comp/instance/{instance_id}/profile`) — one word, two tables, two subjects,
and it is the direct cause of the confusion this convention exists to end. v2
uses `insight`.

### 4.5 What `/v2` means

`/v2` is a **path prefix marking the set of endpoints designed under this
convention**, not a claim that the API as a whole moved to 2.0. New endpoints go
under `/v2/`. v1 endpoints are not migrated or mirrored; a v2 twin appears only
when an endpoint's behaviour actually needs to change.

### 4.6 The one pre-existing v2 endpoint

`POST /v2/comp/chat/{session_id}/message/async` (shipped in 1.5.0) violates
§4.2: `chat` as an entity segment carrying a `session_id`.

It is renamed to the canonical spelling, with the old path kept as a documented
alias for one release:

```
POST /v2/comp/session/{session_id}/message/async     canonical
POST /v2/comp/chat/{session_id}/message/async        deprecated alias, removed in the release after 1.6.0
```

Implementation: a second thin handler annotated
`#[utoipa::path(post, path = "/v2/comp/chat/{session_id}/message/async", deprecated, ...)]`
that delegates to the real one, registered alongside it. A plain `.route()`
would work too but would leave the alias undocumented in the OpenAPI spec,
which is where downstream consumers look to learn it is going away.

A convention that ships with an exception on day one is a convention nobody
follows, and v2 currently holds exactly one endpoint — this is the cheapest this
rename will ever be.

## 5. The two new endpoints

```
GET /v2/comp/instance/{instance_id}/insight/character   → CharacterInsightResponse
GET /v2/comp/instance/{instance_id}/insight/user        → UserInsightResponse
```

`instance_id` is the real primary key of both tables, so there is no id
resolution, no new tree, and no reused word. A session-keyed form was considered
and dropped: both `StartChatResponse` and `SessionListEntry` already return
`instance_id`, so any client holding a `session_id` holds an `instance_id` too —
session keying would buy nothing and cost a `get_session` round trip plus a 404
branch for sessions with no instance.

Both handlers live in a new `crates/eros-engine-server/src/routes/insight.rs`,
merged into the authenticated sub-tree exactly like the other routers (merge,
not nest — the `#[utoipa::path]` annotations already carry the full prefix).

Auth and error mapping, identical for both, copied from `get_character_profile`:

- `PersonaRepo::load_instance_gate(instance_id)` supplies `owner_uid`.
- `owner_uid != jwt.sub` → **403**.
- `None` (unknown instance, or `status <> 'active'`) → **404**.
- An instance with no row yet → **200** with all-null fields and
  `updated_at: null`, matching the convention of the existing profile endpoints.

**DTOs.** Two new flat typed structs over the ten columns,
`#[derive(ToSchema)]`, named after the tables they carry:
`CharacterInsightResponse` and `UserInsightResponse`. The existing
`CharacterProfileResponse` is **not** reused and **not** renamed — it is the
response type of the frozen v1 endpoint and stays exactly as shipped. Two
structs with the same fields is the price of freezing v1; it is also what lets
the v2 pair diverge later without touching v1.

`/comp/instance/{instance_id}/profile` keeps working, unchanged and unmarked.

## 6. Explicitly not in this release

- **No injection anywhere.** `user_insights` is not read by prompt building, the
  chat pipeline, voice, PDE, or the world system. It is written by the chain and
  read by the endpoint; that is all. Injecting a profile of the user built from
  the user's own turns back into those turns is the same echo loop the character
  spec declined, from the other side.
- **No v2 endpoint for `human_insights`.** `/comp/user/{user_id}/profile` is
  frozen and adequate; §4.5 says v2 twins appear only when behaviour changes.
- **No snapshot sweeper and no retention policy** for either insights snapshot
  table. §1 names the query that says when that changes.
- **No change to `human_insights`** — not its columns, its merge semantics, its
  injection path, or its matching path.
- **No change to `companion_insights_events`' schema**, including its `stage`
  values (§3.2).
- **No migration of v1 endpoints into v2**, and no `deprecated` markers on them.
- **No generic abstraction** over the two `*_insight.rs` store modules (§2.4).

## 7. Testing

**Store (`sqlx::test`, real Postgres):**

- migration 0055 creates all three tables with every column; unquoted
  `personal_values` in every query proves the reserved-word avoidance.
- `apply_extraction` incremental semantics: scalar overwrite, absent scalar
  keeps stored, empty-string scalar overwrites, array overwrite only when
  non-empty, no erase path.
- `apply_extraction` appends exactly one snapshot row per call, containing the
  **post-merge** state.
- `ON DELETE CASCADE` removes the profile with its instance while events and
  snapshots survive.
- `UserInsightEventRepo::record` round-trips `payload` / `usage` /
  `generation_id`, so a bind-order swap fails here rather than at the database.

**Pure unit:**

- `project_columns` / `existing_as_extraction_json` round-trip; non-string array
  items dropped; unknown JSON keys ignored.
- the stage-2 prompt renders all ten field names and carries the
  anti-attribution clause pointing at the **user**.
- `validate_extraction_prompts` fails boot on a present-but-blank
  `user_insight_extraction.filter_prompt`, and passes when the section is absent.
- `validate_structuring_prompt_unset` fails boot for a `filter_prompt` on each
  of the three structuring blocks — three cases, not one, or the loop can
  regress to covering only the first.
- `resolve_user_insight_extract()` is `None` when the task is absent or its
  prompt is blank.
- `resolve_structuring("user_insight_structuring", "user_insight_extraction")`
  returns the **stage-1 model id specifically** when the stage-2 block is absent.
  Asserting "not empty" would pass on the global-default fall-through this
  method exists to prevent.
- human chain: `parse_error` payload carries a >2000-char reply truncated under
  `raw`; the `structured` payload carries `_existing_keys` listing exactly the
  populated columns and no values.
- the shipped `examples/model_config.toml` parses and resolves both new tasks
  (extend the existing example-config test).

**Server:**

- both v2 endpoints: 403 on owner mismatch, 404 on unknown or archived instance,
  all-null body for an instance with no row.
- the deprecated async alias and the canonical async path both reach the same
  handler and return the same status for the same request.
- OpenAPI snapshot regenerated; the alias appears marked `deprecated`.

**Pipeline (`sqlx::test` with a mocked OpenRouter, mirroring the existing
`insight_extraction_*` tests):**

- a run writes two `user_insights_events` rows sharing one `run_id`, and the
  structured result lands in `user_insights`.
- empty stage-1 facts writes one event and makes no stage-2 call.
- the human chain's stage-2 call now reports `task = "insight_structuring"` on
  the wire while its parameters are unchanged when the block is absent.

**Pre-PR gate, all four:** `cargo fmt --check`, `cargo clippy`, `cargo test`,
OpenAPI regeneration check.

## 8. Known deviations — frozen by design

**This is not a TODO list.** Every row below is a settled decision about a
shipped contract or a live table. They are written down so that a reader who
spots the inconsistency can see it was already examined, and stop — not so that
someone can work through them later. Treat a change to any of these as
out of scope for every future task unless a spec explicitly reopens it.

| Deviation | Settled because |
|---|---|
| `/comp/chat/{user_id}/sessions` shares a path position with `/comp/chat/{session_id}/*` | v1, delivered contract, frozen (§4.1) |
| `/bff/v1/…` puts the version after the tree, `/v2/…` before it | not a deviation — two trees, two deliberate version conventions. `/bff/v<N>/…` is fixed for the BFF tree; `/v<N>/…` is fixed for the domain API (§4.1) |
| `/persona/{instance_id}/…` keys a `persona` tree on an instance | v1, delivered contract, frozen |
| `/comp/*/profile` names two different tables | v1, delivered contract, frozen; v2 uses `insight` instead (§4.4) |
| `companion_insights_events` uses `facts` / `structured` while `*_insights_events` use `extraction` / `structuring` | live table with downstream audit readers; the rename buys tidiness and costs a backfill window (§3.2) |
| `user_insights` uses `user` where the vocabulary table says `human` | a scope distinction, not a subject error; `human_insights` owns the global slot under the correct word (§2.2) |
| `CharacterProfileResponse` and `CharacterInsightResponse` carry the same fields | v1's response type is frozen as shipped; the duplication is what lets the v2 pair diverge (§5) |
