# Insights (legacy)

The current pipeline is described in [insights.md](insights.md).

Describes the insights pipeline as it shipped through v2.3.1. v2.4.0 replaces it; this document is removed after v2.4.1.

The insights pipeline builds a profile of the real user from conversation. After a chat turn, up to two LLM calls turn what the user said into the typed columns of one row in `engine.human_insights`. The row is read back into the chat prompt and the voice opener, served by a profile route and an export route, and snapshotted daily. Every LLM call leaves an audit row in `engine.companion_insights_events`.

The per-relationship chains `character_insights` and `user_insights` are separate pipelines with their own tables; see [llm-audit.md](llm-audit.md#character-and-user-insights-experimental).

## Storage

`engine.human_insights` holds one row per user, keyed on `user_id` (no foreign key). The profile is global to the user: facts come from every persona the user talks to, and no column records which persona heard what. Besides `user_id` and `updated_at` the row has 21 data columns, which are the whole output surface of the pipeline:

| Column | Type | Meaning |
|--------|------|---------|
| `city` | `TEXT` | Usual city of residence, as a concrete place (not just a province); residence length may be included. |
| `location` | `TEXT` | Where the user is now or recently (trip, temporary stay); filled only when clearly different from `city`. |
| `hometown` | `TEXT` | Hometown or place of origin; only when the user says so, never substituted by the current city. |
| `nationality` | `TEXT` | Nationality or regional identity. |
| `occupation` | `TEXT` | Job and work status with industry, level, company type or daily detail, not a bare job title. |
| `mbti_guess` | `TEXT` | MBTI type. A self-reported type is taken as given; otherwise a cautious guess, worded as one, from repeated behavior. |
| `love_values` | `TEXT` | Attitude toward and expectations of romantic relationships, one or two sentences. |
| `emotional_needs` | `TEXT` | What kind of emotional support the user needs, one or two sentences. |
| `life_rhythm` | `TEXT` | Sleep and daily routine as a concrete pattern. |
| `interests` | `TEXT[]` | Hobbies; each item a concrete short phrase with one real detail, not a bare label. |
| `personality_traits` | `TEXT[]` | Traits; each item a concrete phrase grounded in a situation, not a one-word label. |
| `preferred_gender` | `TEXT` | Preferred gender of a partner (`matching_preferences.preferred_gender`). |
| `age_min` | `INT` | Lower bound of the preferred partner age (`matching_preferences.age_range[0]`). |
| `age_max` | `INT` | Upper bound of the preferred partner age (`matching_preferences.age_range[1]`). |
| `deal_breakers` | `TEXT[]` | Things the user cannot accept, one concrete phrase each (`matching_preferences.deal_breakers`). |
| `education` | `TEXT` | Education background: level, school, major, studying or graduated. |
| `family` | `TEXT` | Family structure: marital and parental status, family members, relations; only when stated. |
| `relationship_history` | `TEXT` | Summary of past relationships: how the last one ended, how long single. |
| `social_pattern` | `TEXT` | Solitude or gathering tendency, online and offline habits, friend circle. |
| `future_plans` | `TEXT` | Near-term goals, life direction, things being planned. |
| `finance_status` | `TEXT` | Income clues: level, spending habits, financial pressure; only when stated, never inferred. |

Indexes: GIN on `interests` and `personality_traits` (set-overlap matching by a downstream service) and a btree on `(updated_at, user_id)` (the export keyset). Row-level security is enabled with no policy, and the `anon` and `authenticated` roles are revoked where they exist. Daily snapshots go to `engine.human_insights_snapshot`.

## The pipeline

Each run is two LLM calls. Stage 1 mines atomic facts from the turn; stage 2 files them into the schema.

```
user message + assistant message
   │
   ├─ stage 1  insight_extraction   system = filter_prompt (deployment-owned)
   │                                user   = the turn
   │   → {"facts": [...], "details": [...]}      audit row  stage='facts'
   │   (no facts: the run ends here)
   │
   ├─ stage 2  insight_structuring  user = facts + existing profile + schema
   │   → {"city": ..., "interests": [...], "matching_preferences": {...}}
   │                                              audit row  stage='structured'
   │
   └─ apply_extraction  → one UPSERT into engine.human_insights
```

### When it runs

The run is part of post-process, spawned after the reply and never blocking it. `memory_scope` gates prompt injection only; it does not gate this pipeline.

- **1:1 text turn** (`post_process::run`): one run for each produced assistant message with non-empty text, pairing the user's message with that message's text. A turn that produces several messages makes several independent runs, each with its own `run_id`. When the user's turn carries an action, the user text is prefixed with its action marker.
- **Group turn** (`post_process::run_group`): one run per turn, below.
- **Voice turns** never run it.
- A run is skipped when the session is already archived or the user text is blank.

### Stage 1: `insight_extraction`

`[tasks.insight_extraction]` supplies the model, fallback, temperature, `max_tokens` and sampling knobs. Its `filter_prompt` is the system message and is owned by the deployment; the engine has no built-in stage-1 prompt. The user message is built by `prompt::facts_user_message`:

```
用户: {user message}
AI:   {assistant message}
```

The output contract is a property of the prompt, not of the engine. The shipped `examples/model_config.toml` prompt asks for `{"facts": [string], "details": [object]}` with the two arrays the same length, `facts[i]` equal to `details[i].content`, at most 12 facts, and `{"facts":[],"details":[]}` when there is nothing new. It restricts facts to what the user said or showed (the AI's lines, persona and guesses are not user facts), records what the user expresses inside role-play with that framing kept (`evidence_type: roleplay_expression`), and treats fictional identities, places, bodies and plot events as not the user's reality unless the user says so. Each `details` item carries `content` plus six metadata fields:

| Field | Values in the example prompt |
|-------|------------------------------|
| `evidence_type` | `explicit_statement`, `explicit_agreement`, `observed_behavior`, `roleplay_expression` |
| `category` | `fact`, `preference`, `aversion`, `goal`, `value`, `habit`, `skill`, `event`, `emotion`, `relation`, `communication_style`, `roleplay_tendency`, `constraint` |
| `domain` | `basic_profile`, `location_culture`, `career`, `education`, `technology`, `finance`, `family`, `friendship`, `romance`, `sexuality`, `health`, `lifestyle`, `hobby`, `politics`, `religion`, `personality`, `communication`, `other` |
| `temporality` | `atemporal`, `current`, `past`, `future`, `unknown` |
| `persistence` | `stable`, `ongoing`, `transient`, `one_off`, `unknown` |
| `confidence` | `high`, `medium`, `low` |

The vocabularies belong to the prompt; the engine stores whatever the model emits. What the engine does with the reply:

- Parses it as JSON, the whole text first and then the first balanced `{…}` block.
- Reads `facts` and keeps its string items. Reads `details` as-is when it is an array; a missing or non-array `details` becomes `[]`.
- Writes both into the audit payload as `{"facts": [...], "details": [...]}`.
- Never validates `details` items, never zips them against `facts`, and never reads the metadata fields.
- Ends the run when there are no facts (audit status `empty`) or no parseable JSON (`parse_error`).

### Stage 2: `insight_structuring`

`[tasks.insight_structuring]` supplies parameters only. The prompt is engine-owned (`prompt::extract_structured_insights_prompt`), written in Chinese, and sent as a single user message that contains:

- the facts as a `- ` bullet list;
- the user's existing profile, rebuilt from the row by `existing_as_extraction_json` (the reverse of `project_columns`): populated fields only, empty arrays omitted, `preferred_gender`, `age_range` (only when both bounds are set) and `deal_breakers` re-nested under `matching_preferences`; `{}` for a user with no row;
- `COMPANION_INSIGHTS_SCHEMA`, the field list the columns above are taken from, introduced with the statement that every field describes the real user, never the AI companion;
- the instructions: fill only the fields the facts clearly support; when new facts make an existing field more complete or more accurate, return the full updated value instead of skipping it or repeating it unchanged; return a nested object whole, with the still-valid old sub-fields; invent no fields; output a single JSON object.

The reply is parsed as a JSON object, the whole text first and then the first balanced `{…}` block that parses to an object. `project_columns` maps it onto the 21 columns: `matching_preferences` is flattened into `preferred_gender`, `age_min`, `age_max` and `deal_breakers`, and keys outside the schema are ignored. The run ends without a write when the object is empty (audit status `empty`) or no object can be parsed (`parse_error`).

The example config gives stage 2 `max_tokens = 1200`, the same as stage 1: the prompt asks for complete updated values, so the output grows with how much of the profile is filled.

### Group chat

`run_group` calls the same two stages once per group turn, with these differences:

- The assistant side is every non-empty reply of the turn as `{speaker name}：{reply}` lines joined by newlines, so stage 1 sees all speakers in one `AI:` block.
- The audit rows carry the group session id and the id of the turn's last produced message. No `audit.user` is forwarded to the provider.
- Only the group session's liveness is checked on entry. The per-speaker archive check belongs to the `character_insights` and `user_insights` runs, not to this one.

See [group-chat.md](group-chat.md#what-reaches-11).

### Failure behavior

Every step fails open: a failure is logged and the turn is unaffected.

- A transport error or timeout on either call ends the run with no audit row.
- A failed read of the existing profile is logged and stage 2 runs with `{}` as the existing profile.
- A failed audit insert or a failed `apply_extraction` is logged only.
- A failed run is not retried or queued, and its facts are not revisited. (Within one call the model `fallback` chain still applies.)

## Write semantics

`HumanInsightRepo::apply_extraction` is one `INSERT … ON CONFLICT (user_id) DO UPDATE` statement. The first extraction for a user creates the row.

| Column kind | On conflict |
|-------------|-------------|
| Text scalars | `COALESCE(new, stored)`. A key that is absent, `null` or not a string keeps the stored value; any string, an empty string included, overwrites it. |
| Arrays (`interests`, `personality_traits`, `deal_breakers`) | Replaced by the new array when it is non-empty after non-string items are dropped; otherwise the stored array is kept. The new array replaces the old one as a whole; there is no merge. |
| `age_min` / `age_max` | `COALESCE` per bound. Both come from `matching_preferences.age_range` and only when it is a two-element array of integers in `i32` range; any other shape leaves both absent. |
| `updated_at` | Set to `now()` on every apply, including an apply that changes no value. An apply runs whenever stage 2 returns a non-empty object, even one with no key the schema knows; for a user without a row that inserts an all-`NULL` row. |

There is no erase path: `null`, an empty array and an absent key all leave a stored value in place. A stored value changes only when a later extraction supplies a replacement, or through SQL outside the engine.

The statement has no read-modify-write step, so two concurrent applies for the same user resolve per column (last write wins), not per row.

## Readers

### Chat prompt

The reply handler loads the row and renders it with `human_insights_to_bullets` under the label `基础画像`, as the first group of the `[user_profile]` section, ahead of the memory-category groups. How much is rendered follows the request's `memory_scope` through `InsightMode`:

| `memory_scope` | `InsightMode` |
|----------------|---------------|
| `full` | Full |
| `neutral_and_relationship` (default) | Neutral |
| `relationship_only` | Off |
| `neutral_only` | Neutral |
| `insights_only` | Full |
| `none` | Off |

Off reads nothing. The renderer emits a bullet only for a non-blank value:

| Tier | Columns (label) |
|------|-----------------|
| Neutral and Full | `city` (`城市`), `location` (`所在地`), `hometown` (`老家`), `nationality` (`国籍`), `occupation` (`职业`), `education` (`教育`), `mbti_guess` (`MBTI`), `life_rhythm` (`作息`), `social_pattern` (`社交模式`), `personality_traits` (`性格特质`), `future_plans` (`未来计划`) |
| Full only (the intimate fields) | `love_values` (`感情观`), `relationship_history` (`感情经历`), `interests` (`兴趣`), `emotional_needs` (`情感需求`), `family` (`家庭`), `finance_status` (`经济状况`) |
| Never rendered | `preferred_gender`, `age_min`, `age_max`, `deal_breakers` |

A bullet is `{label}：{value}`, with array values joined by `、`. The order is fixed: `city`, `location`, `hometown`, `nationality`, `occupation`, `education`, `mbti_guess`, then the Full-only fields in the order of the table above, then `life_rhythm`, `social_pattern`, `personality_traits`, `future_plans`. A failed read is logged and yields no bullets. See [memory-layers.md](memory-layers.md#retrieval-and-injection).

### Voice

On a voice session's first turn the bootstrap snapshot renders the same bullets with the same function, under `[关于他]`. The tier comes from that first turn's `memory_scope` (Neutral when omitted), and the snapshot is frozen into `chat_sessions.metadata.voice_bootstrap`, so later changes to the row do not reach that call. Unlike the chat path, a failed read is not swallowed: the snapshot is not frozen and the next turn retries. See [memory-layers.md](memory-layers.md#voice-turns).

### Profile route

`GET /comp/user/{user_id}/profile` returns the row as `ProfileResponse`: `user_id`, the 21 columns and `updated_at`. The path `user_id` must equal the JWT's user (403 otherwise). A user with no row gets 200 with `updated_at: null`, `null` scalars and `[]` arrays. See [api-reference.md](api-reference.md#get-compuseruser_idprofile).

### Export route

`GET /svc/v1/insights/changes` walks the live table for a downstream service that mirrors the profile.

- **Admission.** The header `x-insights-export-secret` is compared in constant time with `INSIGHTS_EXPORT_SECRET`. Missing or wrong is 403, and admission is decided before the query string is validated.
- **Mounting.** The `/svc/v1/*` sub-tree is mounted only when `INSIGHTS_EXPORT_SECRET` is set. Unset, the paths answer like any unknown path (never 403, never 200). The same secret also mounts `/svc/v1/memories/changes`, which is not part of this pipeline.
- **Query.** `since` (RFC 3339, default Unix epoch), `after_user` (default nil UUID), `limit` (default 500, clamped to 1..=1000).
- **Row shape.** `{"profiles": [...], "next": {...} | null}`. A profile has `user_id`, the 21 columns and `updated_at`.
- **Cursor.** Rows are returned in `(updated_at, user_id)` order for `(updated_at, user_id) > (since, after_user)`. `next` is `{updated_at, user_id}` of the last row and is present only when the page is full (`profiles` length equals `limit`); `null` means the walk is complete. Passing `since` alone includes rows at exactly that timestamp.

Every apply bumps `updated_at` (see [Write semantics](#write-semantics)), so a user reappears in the export after any run whose stage 2 returned a non-empty object, whether or not a value changed. Polling guidance is in [api-reference.md](api-reference.md#get-svcv1insightschanges).

### Daily snapshot

`pipeline::snapshot::sweeper` runs on a cron schedule (default 23:00 `Asia/Singapore`) and inserts one row per `human_insights` row into `engine.human_insights_snapshot`:

```sql
INSERT INTO engine.human_insights_snapshot (user_id, snapshot, captured_at)
SELECT hi.user_id, to_jsonb(hi), $1 FROM engine.human_insights hi
```

`snapshot` is the whole row as JSONB, and `captured_at` is the fire instant, shared by all rows of one fire. There is no LLM call, no dedupe and no filter: every fire writes every user, changed or not. A failed fire is logged and waits for the next scheduled one. The table is indexed on `(user_id, captured_at DESC)`.

## Audit

Each LLM call that returned a response writes one row to `engine.companion_insights_events`, append-only. The two rows of one run share `run_id`.

| Column | Meaning |
|--------|---------|
| `run_id` | Shared by the `facts` row and the `structured` row of one run. |
| `user_id` | Whose profile the run was for. No foreign key. |
| `session_id`, `message_id` | The session and the produced assistant message (the turn's last one, for a group). Foreign keys, `ON DELETE SET NULL`. |
| `stage` | `facts` for stage 1, `structured` for stage 2. |
| `status` | `ok`, `empty` or `parse_error`. |
| `payload` | See below. |
| `generation_id` | Join key into `engine.llm_generations`, `ON DELETE SET NULL`. `NULL` when the ledger write degraded. |

| `stage` | `status` | `payload` |
|---------|----------|-----------|
| `facts` | `ok` | `{"facts": [...], "details": [...]}` |
| `facts` | `empty` | The same shape with no facts; stage 2 does not run. |
| `facts` | `parse_error` | `NULL`. |
| `structured` | `ok` | The model's JSON object as returned (off-schema keys included), plus `_existing_keys`. |
| `structured` | `empty` | `{}` plus `_existing_keys`; nothing is applied. |
| `structured` | `parse_error` | `{"raw": "<first 2000 characters of the reply>"}`. |

`_existing_keys` lists the names, never the values, of the top-level keys of the existing profile sent to stage 2, so a fact missing from the output can be told apart: dropped, or judged already covered. The `stage` vocabulary is `facts` / `structured`, not the `extraction` / `structuring` of the sibling chains; see [llm-audit.md](llm-audit.md#character-and-user-insights-experimental).

Model and token usage are not on the event row. Stage 1 is recorded in `llm_generations` as task `insight_extraction`, stage 2 as `insight_structuring`:

```sql
SELECT e.run_id, e.stage, e.status, g.task, g.model, g.usage
FROM engine.companion_insights_events e
LEFT JOIN engine.llm_generations g ON g.generation_id = e.generation_id
WHERE e.user_id = $1
ORDER BY e.created_at DESC, e.stage;
```

See [llm-audit.md](llm-audit.md#llm-generations-ledger). Payloads hold extracted facts about the user verbatim.

## Configuration

| Setting | Effect |
|---------|--------|
| `[tasks.insight_extraction]` present | Stage 1 runs. `filter_prompt` is required and must be a plain string: blank refuses to boot, and a variant shape (array or table) refuses to boot. |
| `[tasks.insight_extraction]` absent | The whole pipeline is off: no calls, no audit rows, no writes. Existing rows are still served by the readers. |
| `[tasks.insight_structuring]` present | Supplies stage 2's `model`, `fallback`, `temperature`, `max_tokens` and sampling knobs. A `filter_prompt` in any shape, blank included, refuses to boot. |
| `[tasks.insight_structuring]` absent | Stage 2 runs on `[tasks.insight_extraction]`'s parameters (via `resolve_structuring`), not on the global defaults. |
| Tier blocks under either task | Refuse to boot; neither task resolves with a tier. |
| `[[providers.<name>.body]]` rules scoped with `tasks` | Match on the exact task name, so covering both stages means listing `insight_extraction` and `insight_structuring`. |

| Variable | Default | Effect |
|----------|---------|--------|
| `SNAPSHOT_DISABLED` | unset | Exactly `1` disables the snapshot sweeper. |
| `SNAPSHOT_CRON` | `0 0 23 * * *` | Six-field cron expression. An invalid expression logs an error and the sweeper does not start. |
| `SNAPSHOT_TZ` | `Asia/Singapore` | IANA zone the cron is evaluated in. An unparseable value falls back to the default. |
| `INSIGHTS_EXPORT_SECRET` | unset | Mounts `/svc/v1/*`. Blank counts as unset; after trimming, fewer than 32 bytes refuses to boot. |

See [model-config.md](model-config.md#enabling--disabling-extraction) and [deploying.md](deploying.md#operational-notes).

## Known limits

- Stage 2 receives the user's whole populated profile on every run and is asked to return the complete updated value of any field the new facts extend. Output size grows with the profile.
- Array columns are replaced as a whole. Items the model leaves out of a returned array are removed.
- Facts whose topic has no column (for example `health`, `sexuality`, `politics` or `religion` in the example vocabulary) are not written to `human_insights`. Their only copy is the `stage='facts'` audit payload.
- Facts are not carried from one run to the next. A fact that stage 2 does not place in a column during its own run is not revisited.
- `details` metadata is never parsed or used by the engine. The `facts[i]` / `details[i].content` correspondence is a prompt-level convention only.
- No code path in the engine prunes `engine.human_insights_snapshot` or `engine.companion_insights_events`, or deletes a `human_insights` row.

## Source

- `crates/eros-engine-server/src/pipeline/post_process.rs` — `extract_insights`, `extract_facts`, `extract_structured_insights`, `write_insight_event`; call sites in `run` and `run_group`
- `crates/eros-engine-server/src/prompt.rs` — `COMPANION_INSIGHTS_SCHEMA`, `facts_user_message`, `extract_structured_insights_prompt`
- `crates/eros-engine-store/src/human_insight.rs` — `project_columns`, `existing_as_extraction_json`, `apply_extraction`, `list_changes`, `snapshot_all_users`
- `crates/eros-engine-store/src/insight.rs` — `InsightEventRepo`
- `crates/eros-engine-server/src/pipeline/handlers.rs` — `human_insights_to_bullets`, `load_human_insight_bullets`
- `crates/eros-engine-server/src/pipeline/voice.rs` — bootstrap snapshot
- `crates/eros-engine-server/src/pipeline/snapshot.rs` — snapshot sweeper
- `crates/eros-engine-server/src/routes/svc.rs` — `GET /svc/v1/insights/changes`
- `crates/eros-engine-server/src/routes/companion.rs` — `GET /comp/user/{user_id}/profile`
- `crates/eros-engine-llm/src/model_config.rs` — `resolve_insight_extract`, `resolve_structuring`, `validate_extraction_prompts`, `validate_structuring_prompt_unset`
- `crates/eros-engine-store/migrations/0015_human_insights.sql`, `0025_insight_events_and_audit_cols.sql`, `0026_human_insights_geo.sql`, `0033_human_insights_profile_expansion.sql`, `0042_drop_companion_insights.sql`, `0069_human_insights_updated_idx.sql`
- `examples/model_config.toml` — `[tasks.insight_extraction]` and `[tasks.insight_structuring]`
