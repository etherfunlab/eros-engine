# eros-engine — World Town reply-scan activity window (issue #176)

Follow-up to the World Town design (`2026-07-21-world-town-design.md`).
Closes the deliberate v1 gap tracked in that spec's §1.3: the reply-responder
scan (`WorldTownRepo::list_reply_candidates`) walks **all published posts of
all town-enabled owners** every 30s tick and degrades linearly with total
post count forever, because no index bounds it and the town tables never
shrink.

This design bounds the scan with an **activity window** keyed on recent user
comments — a pure performance fix with **zero user-visible behavior change**.
It deliberately does **not** adopt retention/deletion (issue #176 item 2).

---

## 0. Decisions (settled during brainstorm)

- **Window keys off last-user-comment recency, not post age.** A post is a
  reply candidate only while it has had a user comment inside the window. A
  user commenting on a months-old post refreshes the stamp → the thread is
  eligible again, exactly as today. A post that simply goes quiet drops out of
  the scan. This makes it a performance fix, not a product change — spec §6's
  "user comments on an old post: nothing special" stays true.
- **Mechanism: a `last_user_comment_at` stamp on `world_posts`** + a partial
  index over it. Chosen over a partial index on the comments table: the scan
  stays post-centric (smallest query change), and one index directly expresses
  "recent user activity."
- **Old rows are kept forever. No retention, no deletion.** The window bounds
  the *scan cost*; it does not delete content. Consistent with the town spec's
  §6 principle that unenroll / `town_enabled = false` keeps rows.
- **Retention is deferred and decoupled.** If a disk-management retention knob
  is ever wanted, it is a **separate mechanism** and MUST NOT be coupled to the
  town sweeper. This window design owns none of it.
- **The feed endpoint is unchanged.** It is already keyset-paginated and
  index-driven (`idx_world_posts_feed (owner_uid, published_at DESC)`, owner
  constant); it was never the unbounded path. No default feed time-window
  (YAGNI — it would create a "can't scroll to old posts" product problem for
  no scaling benefit).
- **Scope is the reply scan only.** Every other town read is already bounded:
  the feed (above), the comment round's post load (`feed_page(owner, 10)`),
  `list_round_candidates` (one row per owner in `world_states`), and
  `has_town_activity_since` (owner-constant). None are touched.

---

## 1. Data model (migration 0037)

```sql
ALTER TABLE engine.world_posts ADD COLUMN last_user_comment_at TIMESTAMPTZ;

-- Only posts that ever received a user comment enter the index; the vast
-- majority of posts (no user comment) never do, so the index is inherently
-- small and is the sole driving access path for the reply scan.
CREATE INDEX idx_world_posts_reply ON engine.world_posts (last_user_comment_at)
    WHERE last_user_comment_at IS NOT NULL;

-- One-time backfill for posts that already have user comments.
UPDATE engine.world_posts p SET last_user_comment_at = (
    SELECT max(c.created_at) FROM engine.world_post_comments c
    WHERE c.post_id = p.id AND c.author_instance_id IS NULL);
```

`last_user_comment_at` is a denormalized "latest user-comment time" — redundant
with `world_post_comments` by design, so the reply scan is a single indexed
range on `world_posts` instead of a full scan + per-post LATERAL probe.

## 2. Write path (the stamp)

`WorldTownRepo::insert_user_comment` (store `world_town.rs`) already inserts the
user comment as a single `INSERT ... SELECT ... RETURNING` guarded by the
visibility join. The stamp is folded into that **same statement** via a CTE so
insert-and-stamp are atomic — a torn write (comment in, stamp unset) would drop
the post from the reply scan and lose the reply:

```sql
WITH ins AS (
    INSERT INTO engine.world_post_comments (post_id, author_instance_id, source, content)
    SELECT p.id, NULL, NULL, $content
    FROM engine.world_posts p JOIN engine.world_enrollments we
      ON we.owner_uid = p.owner_uid AND we.town_enabled
    WHERE p.id = $post_id AND p.owner_uid = $owner AND p.published_at IS NOT NULL
    RETURNING id, post_id, author_instance_id, content, created_at
),
upd AS (
    UPDATE engine.world_posts SET last_user_comment_at = now()
    WHERE id = (SELECT post_id FROM ins)
)
SELECT id AS comment_id, post_id, author_instance_id,
       NULL::text AS author_name, content, created_at FROM ins
```

Not-visible ⇒ `ins` is empty ⇒ `upd` stamps nothing ⇒ outer `SELECT` returns no
rows ⇒ `None` ⇒ caller 404s, exactly as today. A user comment is always the
latest, so the stamp is an unconditional `now()` (no `max` / `GREATEST`).
Persona comments — both `source = 'round'` and `source = 'reply'` — never touch
the stamp: it means *user* activity only. User comments are human-frequency, so
the extra write is negligible.

## 3. Reply-scan rewrite (`list_reply_candidates`)

The candidate predicate becomes (per post `p`):

```
p.last_user_comment_at > now() - $window          -- activity window (index-driven)
AND p.last_user_comment_at <= now() - $debounce   -- debounce (replaces the LATERAL max)
AND (p.last_reply_at IS NULL
     OR p.last_reply_at < now() - $cooldown)       -- thread cooldown (unchanged)
AND NOT EXISTS (                                   -- "no persona comment after the
    SELECT 1 FROM engine.world_post_comments a     --  latest user comment" (unchanged)
    WHERE a.post_id = p.id
      AND a.author_instance_id IS NOT NULL
      AND a.created_at > p.last_user_comment_at)
```

Preserved from the current query: the `world_enrollments.town_enabled` and
active-author joins, `DISTINCT ON (p.owner_uid)` (at most one candidate per
owner per tick — fairness under cap pressure), and `ORDER BY
last_user_comment_at ASC` (oldest unanswered first), `LIMIT $batch`.

Two deliberate points:

- **The LATERAL `max(created_at)` probe is dropped.** `last_user_comment_at`
  *is* that value, so the debounce check is a plain column comparison and the
  driving scan is the `idx_world_posts_reply` range.
- **The `NOT EXISTS` persona-after-user check is kept, not replaced by
  `last_reply_at`.** `last_reply_at` tracks only `source = 'reply'` (the
  cooldown CAS), not `source = 'round'`. Using it to gate "already answered"
  would let a *round* comment landing after a user comment still trigger a
  reply → a duplicate response. Correctness requires the subquery. It is now
  cheap: it runs only on the handful of windowed candidates and rides
  `idx_world_post_comments_thread`.

Net planner shape: driving range scan on `idx_world_posts_reply` (bounded by
the window), a cheap `NOT EXISTS` per surviving row. Cost is a function of
"posts with a user comment in the last `$window`", independent of total
published-post count — satisfying issue #176's acceptance.

## 4. Config knob (`[tasks.world_reply]`)

New task-specific field `reply_window_secs`, matching the existing
`debounce_secs` / `thread_cooldown_secs` naming:

```toml
[tasks.world_reply]
model = "..."
filter_prompt = "..."
debounce_secs = 90
thread_cooldown_secs = 600
daily_cap = 20
reply_window_secs = 604800   # NEW: reply-eligibility window after a user comment (default 7d)
```

- **Default 604800 (7 days).** The only post the window can permanently drop is
  one whose user comment never got answered *and* then stayed silent past the
  window — which happens only when `daily_cap` / cooldown deferred the reply.
  `daily_cap` resets each UTC day, so a capped post waits at most ~1 day for
  fresh budget; 7 days leaves comfortable margin while pinning the scan to
  "posts with a user comment in the last week."
- **Floored above the debounce window.** A `reply_window_secs <= debounce_secs`
  would make the eligible range (`> now()-window AND <= now()-debounce`) empty;
  the resolver clamps it to strictly exceed the resolved `debounce_secs`.
- Added to `ResolvedWorldReply` and `resolve_world_reply()` in
  `model_config.rs`, alongside the existing three.

## 5. Spec updates (to `2026-07-21-world-town-design.md`)

Land together with the code so the town spec matches the tree at merge:

- **§1.3** — retitle from "Retention (v1: none — deliberate)" to the activity-
  window mechanism. State the reply scan is bounded by `last_user_comment_at`
  window + `idx_world_posts_reply`, that rows are still kept forever (no
  retention adopted), and that any future retention is a separate,
  sweeper-decoupled mechanism.
- **§3.3** — add the window gate and the `last_user_comment_at` stamp to the
  reply-responder description; note the dropped LATERAL and retained
  `NOT EXISTS` reasoning.
- **§3.4** — add `reply_window_secs` to the `[tasks.world_reply]` block.
- **§6** — keep "user comments on an old post: nothing special"; add a line
  that the stamp is what preserves it (a fresh user comment re-enters the
  window).

## 6. Testing

- **Migration:** backfill sets `last_user_comment_at` from existing user
  comments; posts with no user comment stay NULL (out of the index).
- **Write path:** `insert_user_comment` stamps `last_user_comment_at`;
  round/reply inserts do not.
- **Scan (sqlx):**
  - windowed-in: user comment inside window, past debounce, no persona after →
    candidate.
  - windowed-out: user comment older than `reply_window_secs` → *not* a
    candidate (the new bound).
  - regression guard: a `source = 'round'` comment landing after the user
    comment suppresses the reply (proves `NOT EXISTS` wasn't replaced by
    `last_reply_at`).
  - existing debounce-boundary and cooldown coverage still holds.
- **model_config:** `reply_window_secs` default (604800) + override + the
  clamp-above-debounce floor.
- **Schema drift:** extend the drift test for `world_posts.last_user_comment_at`
  and `idx_world_posts_reply`.

## 7. Out of scope

- Retention / deletion of any town table (deferred; if ever added, a separate
  mechanism not coupled to the sweeper).
- Any change to the feed endpoint, the comment round, or the publish path.
- A `last_persona_comment_at` second stamp to fully eliminate the `NOT EXISTS`
  (possible future micro-optimization; unnecessary while the candidate set is
  window-bounded).
