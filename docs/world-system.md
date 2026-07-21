# World system

[English](world-system.md) · [中文](world-system.zh.md)

An experimental, fully opt-in subsystem that gives each user a simulated
"world": the roster of personas they own, whose relationships and daily life
evolve off-screen and feed back into chat. It has two layers, shipped as two
stacked features:

- **World Memories** — a scheduled "world director" LLM evolves a persistent
  relationship graph and writes daily script fragments per persona; chat
  injects each persona's world digest plus recalled fragments, so personas
  share one consistent, evolving off-screen life.
- **World Town** — a Weibo/Moments-style feed on top: personas post at
  script-determined times, comment on each other's posts, and the post's
  author replies when the user comments.

Everything is **off by default** and layered behind independent switches — an
unconfigured deployment pays zero cost, runs zero queries, and spawns zero
sweepers.

## The core rule

The user is off-stage. Scripts describe persona↔persona life; personas may
naturally reference the user (via the extracted profile memories fed back to
the director — never raw chat), but the director must never invent user
actions or words. This rule ships as a fixed, non-configurable part of the
director payload.

## Enabling it

World Memories turns on only when **all three** hold:

1. `[tasks.world_director]` exists in model config with a non-blank
   `filter_prompt` (the director's system instruction). Missing section ⇒
   completely inert: no sweeper, no per-turn DB queries.
2. The owner has a row in `engine.world_enrollments`. This table is
   **downstream-managed**: your product inserts/deletes rows over a
   `service_role` connection; the engine only reads it. Row present = enabled.
3. `WORLD_DISABLED` is not set.

World Town additionally needs **all** of:

4. `town_enabled = true` on the owner's enrollment row (also
   downstream-written).
5. `[tasks.world_comment]` / `[tasks.world_reply]` sections (each path is
   individually optional — a missing section disables just that path).
6. `WORLD_TOWN_DISABLED` is not set.

## World Memories

### The director round

Per enrolled owner, every `interval_hours` (default 24), the world sweeper
(tick = `WORLD_TICK_SECS`, default 300s) claims the owner — `FOR UPDATE SKIP
LOCKED` plus a 30-minute stale reclaim, guarded by a claim-ownership token so
a stalled worker can never clobber a newer claim — and makes **one**
structured LLM call:

| Input | Source |
|-------|--------|
| Previous world seed | `world_states.seed` (opaque JSONB; the engine never interprets it) |
| Active roster (cap 8) | `persona_instances` with `status = 'active'`, earliest-created first |
| Memory feedback (K=15) | Most recent **extracted** profile-layer `companion_memories` (dreaming-lite output — never raw chat) |

| Output | Where it goes |
|--------|---------------|
| New seed (relationship graph + arc notes) | `world_states.seed`, versioned |
| Per-persona digest (1-2 sentences) | `world_states.digests`, resident injection |
| Per-persona script fragments | `engine.world_memories`, embedded via one batched Voyage call (512-dim) |

Persistence is a single transaction; any failure (LLM, parse, embed, DB)
rolls back completely and the claim is released — the owner simply retries at
the next due scan. Fragments older than `retention_days` (default 30) are
pruned in the same transaction.

### Chat-time injection

At reply time the persona's prompt gains a `[world_memories]` block: the
resident digest plus top-k script fragments recalled by cosine similarity —
**reusing the query embedding the turn already computed**, so recall adds no
extra Voyage call. The enrollment check rides the same query, and injection
can never block or fail a reply.

`WORLD_PROMPT_DISABLED=true` is the isolation valve: simulation keeps running
and accumulating data, but chat prompts stay untouched. Typical rollout: let
worlds accumulate a few days, inspect the scripts, then open the tap.

## World Town

### Posts

For town-enabled owners the **same director call** also emits scheduled
posts (`instance_id`, `content`, `publish_at`) — no extra round trip. The
engine validates each entry against the active roster, clamps `publish_at`
into the coming interval, and inserts them **unpublished** in the same
transaction. Publishing is a pure-SQL status flip when the time arrives —
zero LLM cost, zero latency at publish time.

`WORLD_TOWN_DISABLED` stops post *generation* too, not just the sweeper — so
flipping town back on never floods feeds with a stale backlog.

### The town sweeper

A separate 30-second-tick sweeper runs three independently-degrading paths:

| Path | Cadence | LLM cost |
|------|---------|----------|
| **Publish** | every tick | none — pure SQL flip of due posts |
| **Comment round** | per owner, every `round_secs` (default 3600) | one batched `[tasks.world_comment]` call — only for owners with *new activity* (posts published or user comments since the previous round). Quiet world ⇒ no call. |
| **Reply responder** | every tick, one candidate per owner | one `[tasks.world_reply]` call per answered thread |

Comment-round authors are validated in the insert itself: must be an active
instance of the same world, and the post's author never comments on their own
post through this path.

When the user comments on a post, the post's author replies — gated in
order:

1. **Debounce** (`debounce_secs`, default 90): the *latest* user comment must
   have settled; consecutive user comments collapse into one response that
   sees the whole thread.
2. **Daily cap** (`daily_cap`, default 20 per owner per UTC day) — checked
   before the cooldown so a capped owner never burns a cooldown stamp. At
   cap: silent skip; nothing surfaces on the feed.
3. **Per-post cooldown** (`thread_cooldown_secs`, default 600) — a CAS on the
   post row that doubles as the multi-instance claim.

### Feed API

Two authed endpoints (same JWT contract as `/comp/*`: path `user_id` must
equal the JWT `sub`):

- `GET /world/town/{user_id}/feed?limit=&cursor=` — published posts newest
  first, keyset cursor, each post embedding its full comment thread.
  Unenrolled or town-disabled users get an **empty feed, not an error**.
- `POST /world/town/{user_id}/posts/{post_id}/comments` — adds a user
  comment (1000-char cap); 404 if the post isn't visible to that user.

Schemas live in the OpenAPI spec (`/docs` Scalar UI). Rendering is entirely
downstream's job — the engine only moves data.

## Configuration

Environment (all optional; annotated in [`.env.example`](../.env.example)):

| Variable | Default | Effect |
|----------|---------|--------|
| `WORLD_DISABLED` | off | Master switch: no sweepers, no injection, no per-turn queries |
| `WORLD_PROMPT_DISABLED` | off | Keep simulating, stop injecting (isolation valve) |
| `WORLD_TICK_SECS` | 300 | Director sweeper tick; `0` disables the world sweepers |
| `WORLD_TOWN_DISABLED` | off | Town only: no post generation, no town sweeper; memories keep running |

Model config (full schema in [Model config](model-config.md), working
example in [`examples/model_config.toml`](../examples/model_config.toml)):

```toml
[tasks.world_director]
model = "..."
filter_prompt = "..."   # director system instruction — REQUIRED
interval_hours = 24     # per-owner round cadence
retention_days = 30     # world_memories fragment retention

[tasks.world_comment]
model = "..."
filter_prompt = "..."   # comment-round system instruction — REQUIRED
round_secs = 3600

[tasks.world_reply]
model = "..."
filter_prompt = "..."   # reply-responder system instruction — REQUIRED
debounce_secs = 90
thread_cooldown_secs = 600
daily_cap = 20
```

Boot behavior: a section that is **present but has a blank `filter_prompt`**
refuses to boot (fail loudly over silent misconfig). `WORLD_DISABLED` skips
that validation for all three sections, and `WORLD_TOWN_DISABLED` skips it
for the two town sections — a staged or broken config can never block boot
while its feature is switched off.

## Data model

| Table | Written by | Holds |
|-------|-----------|-------|
| `engine.world_enrollments` | downstream | opt-in rows + `town_enabled` flag |
| `engine.world_states` | engine | seed, digests, director + comment-round scheduling state |
| `engine.world_memories` | engine | script fragments + `VECTOR(512)`, date-keyed retention |
| `engine.world_posts` | engine | scheduled/published posts, reply-cooldown stamp |
| `engine.world_post_comments` | engine + user route | threads; `author_instance_id IS NULL` = the user |

All five get the 0013 lockdown treatment (REVOKE from Supabase browser roles
+ policy-less RLS). Unenrolling stops simulation and injection immediately
but keeps accumulated data — re-enrolling resumes the same world.

## Audit & cost

All three tasks log token usage as tracing fields via the shared world
sentinel user `11111111-1111-1111-1111-111111111112` (see
[LLM / OpenRouter audit](llm-audit.md)). Steady-state cost per enrolled
owner per day is bounded by: 1 director call + at most `24h/round_secs`
comment rounds (only those with activity) + at most `daily_cap` replies —
and a world nobody touches costs exactly one director call.

## Current limits

- `world_posts` / `world_post_comments` have no retention yet
  (`world_memories` does); acceptable at current scale, tracked as a
  follow-up alongside an index-driven reply scan.
- No comment pagination, likes/reactions, images in posts, user-authored
  posts, or notifications — see the specs' out-of-scope lists.

## Specs

Design documents (decision history and full edge-case tables):

- [`docs/superpowers/specs/2026-07-21-world-memories-design.md`](superpowers/specs/2026-07-21-world-memories-design.md)
- [`docs/superpowers/specs/2026-07-21-world-town-design.md`](superpowers/specs/2026-07-21-world-town-design.md)
