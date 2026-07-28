# eros-engine — World Worldview (per-owner custom worldview; World System v2)

**Status**: design, pending implementation plan
**Target release**: `0.9.x` dev track. **Migration: 0039** (one new table + two `world_states` columns).
**Scope**: let each user supply a custom worldview background (sci-fi, modern,
ancient, …) as a single free-text document that every World System director
round must honor — the WM director writes scripts inside it, the Stories
director simulates persona lives inside it, and Town comments/replies stay in
voice. The engine ships **no default worldview**: providing one (its own
default text, or forcing the user to write one) is entirely the downstream's
job, and an enrolled owner without a worldview gets **no** World System LLM
activity at all.

Companion specs: `2026-07-21-world-memories-design.md` (v1 base),
`2026-07-21-world-town-design.md`, `2026-07-23-world-stories-design.md`.
Worldview is a new **input** to all three layers; it adds no fourth layer and
no new LLM task.

---

## 0. Motivation & research

Today the world runs worldview-less: the WM director improvises a background
from persona `art_metadata` alone, so every deployment gets an implicit
"whatever the model assumes" setting, and users cannot ask for 古代 or 科幻.

Survey of the AI-tavern ecosystem (SillyTavern World Info, NovelAI Lorebook,
Character Card V2/V3 `character_book`, Agnai, Backyard AI, KoboldAI Lite)
shows two lineages:

- **Lineage A** — a single `scenario` / world-description text injected
  unconditionally.
- **Lineage B** — a lorebook: many keyed entries, keyword-scanned against the
  recent chat window, injected on demand under a token budget (interchange
  standard: CCv2 `character_book`).

Lineage B's machinery exists to protect **per-turn chat prompts** from large
static lore. Our primary consumer is different: the directors are background
sweeper calls (24 h / 8 h / hourly cadence) with no token pressure, and they
need the worldview **wholesale**, not keyword-sliced. Chat already receives
the worldview indirectly through the script fragments the director writes.
So this design is deliberately Lineage A — one text, whole, into every
director payload — with the data model shaped so a Lineage B entries table
can be added later without migration breakage (lorebook A = constant-only
degenerate form of B).

Sources: docs.sillytavern.app World Info; malfoyslastname/character-card-spec-v2;
kwaroran/character-card-spec-v3; backyard.ai lorebook docs; docs.novelai.net
lorebook; agnai memory docs.

---

## 1. Decisions (settled during brainstorm)

- **Single free text, lorebook-reserved.** One `content` text (1..=10 000
  chars) per owner. Future lorebook = a sibling `world_worldview_entries`
  table; nothing in this design would migrate.
- **Downstream-writes-table, engine-reads-only** — the `world_enrollments`
  pattern, verbatim: `service_role` writes, engine only SELECTs, zero HTTP
  surface, 0013-style lockdown. Upload UX, default-worldview policy, and
  content moderation are downstream concerns.
- **Missing worldview = hard prerequisite, skip loudly.** An enrolled owner
  without a worldview is excluded from **every** World System LLM task (WM
  rounds, Stories scans, Town comment/reply rounds). The sweeper logs an
  aggregate `warn!` per tick while any such owner exists. No LLM call is ever
  made worldview-less. Rationale: matches the engine's
  present-but-blank-refuses-to-boot convention for config, and avoids
  producing scripts that contradict a worldview set five minutes later.
- **Worldview change = world reset.** The engine hashes the content it used
  (SHA-256, hex) into `world_states.worldview_hash` each round. Hash mismatch
  (including `NULL` → first hash) makes the next round an **init-style
  reset**: fresh seed, no `previous_seed`, no stale `recent_life`, and a
  same-transaction purge of old-worldview data (see §2 for the exact
  inventory). Consequence, accepted: **existing worlds reset once** after the
  upgrade backfill, because their `worldview_hash` is `NULL`.
- **Reset keeps the Town feed as history.** `world_posts` +
  `world_post_comments` (including user comments) survive resets as a
  read-only past; what is purged is `world_memories` fragments and all three
  Stories tables. To keep eras separate, new AI activity (comment rounds,
  replies) never targets posts published before the current worldview era
  (§5).
- **Worldview feeds WM + Stories + Town, not chat.** All four director-side
  payloads gain a `worldview` field. The chat prompt is untouched — no new
  block, byte-identical guarantees preserved; worldview reaches chat only
  through fragments, as today.
- **Change takes effect within one tick.** A changed worldview makes the
  owner immediately due for a WM round (claim condition: interval elapsed OR
  `worldview.updated_at > last_run_at`), so a new worldview lands in
  ≤ `WORLD_TICK_SECS` (default 300 s), not up to 24 h.
- **Zero new config surface.** No env vars, no model-config fields, no HTTP
  routes, no `openapi.json` drift. `WORLD_DISABLED` / `WORLD_TOWN_DISABLED` /
  `WORLD_STORIES_DISABLED` and the filter-prompt boot gates are unchanged.

---

## 2. Data model (migration 0039)

```sql
-- Downstream-managed worldview table. The engine only ever SELECTs it
-- (over the service_role/owner connection); downstream INSERTs/UPDATEs/
-- DELETEs rows. The engine ships no default worldview: an enrolled owner
-- with no row here (or a blank content) receives no World System LLM
-- activity until downstream provides one.
CREATE TABLE engine.world_worldviews (
    owner_uid  UUID PRIMARY KEY,
    content    TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT worldview_content_len
        CHECK (char_length(content) BETWEEN 1 AND 10000)
);

-- 0013-style lockdown: REVOKE ALL from anon/authenticated; RLS enabled
-- with no policies (service_role bypasses).

ALTER TABLE engine.world_states
    ADD COLUMN worldview_hash   TEXT,         -- SHA-256 hex of content used last round
    ADD COLUMN worldview_set_at TIMESTAMPTZ;  -- start of the current worldview era
```

Both new `world_states` columns are engine-owned and engine-written; `NULL`
means "pre-worldview world" and forces a reset on first sight of a worldview.

**Reset inventory** (executed inside the `persist_round` transaction of the
reset round, scoped to the owner):

| Purged | Kept |
|---|---|
| `world_memories` (old-worldview fragments must stop injecting into chat) | `world_enrollments`, `world_worldviews` |
| `persona_story_insights`, `persona_story_events`, `persona_story_memories` | **published** `world_posts` + their `world_post_comments` (historical feed) |
| scheduled-but-unpublished `world_posts` (stale-era pipeline, §5) | everything outside the World System (`companion_memories`, affinity, …) |
| `world_states.seed` / `digests` (replaced by the init round's output) | |

Atomicity: purge + new seed/digests/fragments + `worldview_hash` +
`worldview_set_at` land in one transaction. Any failure (LLM, parse, embed,
DB, lost claim) rolls back the whole round — the old world stays intact and
the next tick retries. This is the existing `persist_round` failure contract,
extended with the purge statements.

Validation: the DDL `CHECK` blocks oversize/empty at the write site. The
engine additionally trims on read and treats blank-after-trim as missing
(defense in depth). No control-character policing: the content enters JSON
payloads via serde, so serialization is injection-safe by construction
(unlike `prompt_traits`, which renders into prompt bullets and does police).

---

## 3. Sweeper behavior

**Claim gating.** `claim_due` joins `world_worldviews` and considers an owner
due when `worldview present AND (interval elapsed OR ww.updated_at >
ws.last_run_at)`. Owners without a worldview are excluded in SQL — no claim
churn, no per-owner log spam. No hashing in SQL (would need pgcrypto): the
timestamp only decides *dueness*; the engine hashes the fetched content
inside the claimed round, and that comparison against `worldview_hash` is
what decides *reset vs normal*. A content-identical touch (downstream
rewrites the same text) therefore costs exactly one extra **normal** round —
`mark_ran` bumps `last_run_at`, so it converges immediately, and no reset
fires.

**Aggregate warn.** Each world tick additionally counts enrolled owners with
no (or blank) worldview and, if `> 0`, logs one
`warn!("world: {n} enrolled owner(s) have no worldview; skipping")`. Loud,
non-spammy per owner, self-heals the tick after downstream backfills.

**Reset round assembly.** When the claimed round sees a hash mismatch:

- header: the existing **init** variant（「初始化这个世界…」）, even though a
  previous seed exists;
- payload: no `previous_seed`, no `recent_life` (old-world evidence);
  `recent_user_memories` **is** kept — user-profile facts are real-world,
  worldview-independent;
- persist: reset inventory (§2) + fresh seed/digests/fragments + new
  `worldview_hash` + `worldview_set_at = now()`.

A normal (hash-matching) round changes nothing relative to today except the
added `worldview` payload field.

**Stories & Town gating.** The Stories scan (phase 2 of the world tick) and
the Town sweeper's comment-round and reply-candidate queries all join
`world_worldviews` and skip worldview-less owners. Ordering note: on a reset
tick, the WM round purges Stories data first (phase 1 transaction), then the
phase-2 Stories scan re-derives insights already under the new worldview.

**Concurrency.** The hash stored by `persist_round` is the hash of the
content actually used to assemble the payload. If downstream updates the
worldview mid-round, a naive commit would land output generated from the old
content while stamping `last_run_at = now()` — which lands *after* the new
row's `updated_at`, so the touch-dueness condition (`ww.updated_at >
ws.last_run_at`) would never fire and the change would sit unprocessed for up
to a full `interval_hours`. `persist_round` closes this instead of tolerating
it: before doing any purge/insert work, it re-checks — `FOR SHARE`, inside the
same transaction — that the worldview row's `updated_at` still matches the
value read at round start. A mismatch (content changed, or the row vanished)
aborts the round (`RowNotFound`, no commit) without touching `last_run_at`, so
the owner re-claims on the very next tick and the retried round picks up the
fresh content. `FOR SHARE` also closes the race window itself: it blocks a
concurrent downstream `UPDATE` on that row from committing until the round's
transaction ends, so there is no gap between the check and the commit for a
change to sneak through. Lost-claim semantics unchanged (`RowNotFound`, no
commit).

---

## 4. Payload & rules changes (four LLM tasks, chat untouched)

- **WM director** — `director_user_payload` gains a top-level
  `"worldview": "<content>"` field (serialization order is serde-determined —
  the workspace's `serde_json` has no `preserve_order` feature, so keys
  serialize alphabetically here; the framing intent is carried by the
  always-on worldview rule below, not by field position).
  `WORLD_DIRECTOR_RULES` gains one always-on rule:
  「一切设定（时代、科技、地点、职业、事件）必须符合 worldview 描述的世界观，
  不得引入与其冲突的元素。」 The conditional rule constants
  (`WORLD_TOWN_POST_RULES`, `WORLD_STORIES_WM_RULES`) are renumbered to
  follow it.
- **Stories director** — payload gains `worldview`;
  `STORY_DIRECTOR_RULES` gains one rule: 生活推演（职业、居所、日常事件）必须
  发生在 worldview 设定的世界内. The 25-field
  `PERSONA_STORY_INSIGHTS_SCHEMA` is **unchanged** — the field list is the
  engine's contract; worldview steers content, never the list.
- **Town comment & reply** — both payloads gain `worldview`;
  `WORLD_COMMENT_RULES` and the reply header each gain one line: 评论/回复的
  语气与内容须符合 worldview（不得出现与世界观冲突的时代元素）.
- **Chat prompt** — zero change. `[world_memories]` / `[world_stories]`
  blocks, ordering, and empty-context byte-identical guarantees are all
  untouched.

---

## 5. Town history isolation

Because resets keep the feed, a mixed-era feed exists by design. What must
not happen is **new** AI activity on old-era posts (a sci-fi persona
commenting on an 古代 post). Rule: comment rounds and reply candidacy only
consider posts with `published_at >= world_states.worldview_set_at`. The
feed GET serves full history unchanged. One asymmetry: posts scheduled by a
pre-reset director round but **not yet published** carry old-worldview
content that would otherwise surface *after* the reset, so the reset purge
deletes unpublished rows while keeping published ones. Published = history;
unpublished = pipeline, and the pipeline must not leak a stale era. The same
race exists at smaller scale within a single round: a reset can commit while
a comment/reply LLM call for an in-era post is still in flight. To close it,
`insert_round_comment` and `insert_reply_comment` both revalidate the era at
write time (not just at candidate-scan time), so a reset committing mid-flight
cannot land AI activity on a post that is now pre-era.

---

## 6. Upgrade path

1. Deploy migration 0039 + new binary. From the first tick, every enrolled
   owner lacking a worldview is skipped (aggregate warn fires). **World
   System activity pauses deployment-wide until backfill** — this is the
   designed force-function, not an accident.
2. Downstream backfills `world_worldviews` (its own default text, or
   user-collected input) — one INSERT per owner.
3. Next tick: each backfilled owner is immediately due (hash `NULL` ≠ new
   hash) → one-time init-style reset per owner, staggered only by claim
   concurrency. Old published Town posts remain as history; fragments and
   Stories data re-derive under the new worldview.

Downstream contract addition (docs): `world_worldviews` joins
`world_enrollments` in the "downstream-managed tables" list. Flipping
`town_enabled` / `stories_enabled` flags keeps its existing semantics
(stop simulation, keep data) — unrelated to worldview.

---

## 7. Testing

- **Store**: `claim_due` excludes missing/blank-worldview owners;
  `ww.updated_at > ws.last_run_at` makes an owner due ahead of interval;
  engine-side hash mismatch (incl. `NULL`) marks the round as reset while a
  content-identical touch yields a normal round; `persist_round` reset
  path purges exactly the §2 inventory + stamps hash/set_at atomically;
  induced failure rolls back purge and stamps together (old world intact);
  unpublished-post purge vs published-post retention; aggregate count query.
- **Pipeline**: all four payloads carry `worldview`; reset round uses the
  init header and omits `previous_seed`/`recent_life` while keeping
  `recent_user_memories`; rule-constant renumbering locked by snapshot
  tests.
- **Town**: comment/reply candidate queries exclude posts with
  `published_at < worldview_set_at`.
- **Chat**: existing byte-identical prompt suite runs unchanged (no chat
  edits in this design — the suite is the proof).

---

## 8. Docs

- `docs/world-system.md` + `docs/world-system.zh.md` (new sections in 简体中文
  per current convention): a "Worldview" section — data contract, missing =
  skip + warn, change = reset (feed kept), and the explicit statement:
  **the engine ships no default worldview; downstream must provide one**
  (its own default, or by forcing user input).
- `docs/deploying.md`: add `world_worldviews` to the downstream-written
  tables.

---

## 9. Out of scope

- Lorebook entries (keys / constant / budget / recursion / CCv2 import) —
  reserved via the sibling-table shape, not built.
- Worldview in the chat system prompt — chat inherits it via fragments only.
- Any engine-owned upload/read HTTP API for worldviews.
- Per-persona or per-session worldviews — the worldview is per-owner, same
  granularity as the world itself.
- Fixing the pre-existing `docs/model-config.md` gap (world task sections
  undocumented there) — noted, separate chore.
