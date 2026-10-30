// SPDX-License-Identifier: AGPL-3.0-only
//! User-level erasure: `erase_user` removes what the engine derived about one
//! user and records a tombstone (`engine.user_erasures`, one row per erased
//! user), which is the deletion stream downstream mirrors read.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ErasureRow {
    pub user_id: Uuid,
    pub erased_at: DateTime<Utc>,
}

pub struct ErasureRepo<'a> {
    pub pool: &'a PgPool,
}

/// What one `erase_user` call did, by bare table name. Every table the erasure
/// covers is present, with 0 when it had nothing to do.
#[derive(Debug, Clone)]
pub struct EraseReport {
    pub erased_at: DateTime<Utc>,
    /// Rows removed.
    pub deleted: BTreeMap<&'static str, i64>,
    /// Rows kept but emptied of content.
    pub blanked: BTreeMap<&'static str, i64>,
}

const BY_USER: &str = "user_id = $1";
const BY_OWNER: &str = "owner_uid = $1";
/// The insight chains key on the instance alone, so they are reachable only
/// while the instance row exists; the engine archives instances rather than
/// deleting them, and a caller that deletes one first orphans its chain.
const BY_INSTANCE: &str = "instance_id IN (SELECT id FROM engine.persona_instances \
                           WHERE owner_uid = $1)";

/// Derived rows: conversation-derived content that has no life apart from the
/// user. Children precede their parents so a cascade never removes rows the
/// report should have counted.
const DERIVED: &[(&str, &str)] = &[
    // The state row is the lock anchor that serializes this erase against an
    // in-flight profile write (`HumanInsightRepo::replace_profile_if_claimed`
    // writes under a `FOR UPDATE` on it). It must be deleted first: the delete
    // waits for a write that holds the lock, and the `human_insights` delete
    // below then removes the row that write committed.
    ("human_profile_state", BY_USER),
    ("human_facts", BY_USER),
    ("human_insights", BY_USER),
    ("human_insights_snapshot", BY_USER),
    // Frozen history of the retired profile table; no writer remains, but rows
    // written before it was retired are still attributed to their user.
    ("companion_insights_snapshot", BY_USER),
    ("companion_memories", BY_USER),
    ("world_memories", BY_OWNER),
    ("world_states", BY_OWNER),
    ("world_enrollments", BY_OWNER),
    // Comments go with their post.
    ("world_posts", BY_OWNER),
    ("persona_story_insights", BY_OWNER),
    // Memories cascade from their event.
    ("persona_story_memories", BY_OWNER),
    ("persona_story_events", BY_OWNER),
    ("user_insights", BY_INSTANCE),
    ("user_insights_snapshot", BY_INSTANCE),
    // The persona's side of the same relationship, extracted from the same
    // conversation.
    ("character_insights", BY_INSTANCE),
    ("character_insights_snapshot", BY_INSTANCE),
    // The relationship scanner's products. The state row goes first among
    // them: every claim-guarded scanner write (episode, journal, arc) reads it
    // `FOR KEY SHARE`, so this delete waits for a write in flight and the
    // deletes below remove what it wrote, while a write that reaches the row
    // after the delete waits for this transaction and then writes nothing.
    // `relationship_scan_events` holds counts and ids only and is left alone.
    ("relationship_scan_state", BY_INSTANCE),
    ("relationship_episodes", BY_INSTANCE),
    ("relationship_arcs", BY_INSTANCE),
    ("relationship_journal", BY_INSTANCE),
];

/// An audit or state row that survives an erasure with its content cleared.
struct Blank {
    table: &'static str,
    /// The `SET` list that clears the content columns.
    clear: &'static str,
    scope: &'static str,
    /// True while the row still carries content. Restricting the update to such
    /// rows is what makes a repeat call report 0.
    carries_content: &'static str,
}

/// Audit rows keep their ids and timestamps, which cost reconciliation and
/// tracing refer to, and lose every column that can hold text the user
/// wrote or the model derived from it. `companion_affinity` is the one state
/// table here: its numbers are the relationship, not the person, and only the
/// generated summary is text.
const BLANKS: &[Blank] = &[
    Blank {
        table: "human_insights_events",
        clear: "payload = NULL",
        scope: BY_USER,
        carries_content: "payload IS NOT NULL",
    },
    Blank {
        table: "companion_decision_events",
        clear: "payload = NULL",
        scope: BY_USER,
        carries_content: "payload IS NOT NULL",
    },
    Blank {
        table: "chat_images_events",
        clear: "inputs = '{}'::jsonb, subject = NULL, caption = NULL, composed_prompt = NULL, \
                last_failure = NULL",
        scope: BY_USER,
        carries_content: "inputs <> '{}'::jsonb OR subject IS NOT NULL OR caption IS NOT NULL \
                          OR composed_prompt IS NOT NULL OR last_failure IS NOT NULL",
    },
    Blank {
        table: "chat_vision_events",
        clear: "vision = NULL, image_url = '', last_failure = NULL",
        scope: BY_USER,
        carries_content: "vision IS NOT NULL OR image_url <> '' OR last_failure IS NOT NULL",
    },
    Blank {
        table: "user_insights_events",
        clear: "payload = NULL",
        scope: BY_INSTANCE,
        carries_content: "payload IS NOT NULL",
    },
    Blank {
        table: "character_insights_events",
        clear: "payload = NULL",
        scope: BY_INSTANCE,
        carries_content: "payload IS NOT NULL",
    },
    Blank {
        table: "companion_affinity",
        clear: "feeling_clause = NULL, feeling_clause_at = NULL",
        scope: BY_USER,
        carries_content: "feeling_clause IS NOT NULL OR feeling_clause_at IS NOT NULL",
    },
    Blank {
        table: "companion_affinity_events",
        clear: "context = '{}'::jsonb",
        scope: "affinity_id IN (SELECT id FROM engine.companion_affinity WHERE user_id = $1)",
        carries_content: "context <> '{}'::jsonb",
    },
];

impl ErasureRepo<'_> {
    /// Remove everything the engine derived about `user_id` and record the
    /// tombstone, in one transaction: deletes, then blanks, then the tombstone.
    ///
    /// Idempotent. The tombstone is an upsert that moves `erased_at` forward on
    /// every call, so a repeat re-announces the erasure on the deletion streams
    /// for a mirror that missed the first; nothing else is left to touch, so the
    /// repeat's counts are all 0.
    pub async fn erase_user(&self, user_id: Uuid) -> Result<EraseReport, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        let mut deleted = BTreeMap::new();
        for (table, scope) in DERIVED {
            let res = sqlx::query(&format!("DELETE FROM engine.{table} WHERE {scope}"))
                .bind(user_id)
                .execute(&mut *tx)
                .await?;
            deleted.insert(*table, res.rows_affected() as i64);
        }

        let mut blanked = BTreeMap::new();
        for b in BLANKS {
            let res = sqlx::query(&format!(
                "UPDATE engine.{} SET {} WHERE ({}) AND ({})",
                b.table, b.clear, b.scope, b.carries_content
            ))
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
            blanked.insert(b.table, res.rows_affected() as i64);
        }

        let erased_at: DateTime<Utc> = sqlx::query_scalar(
            "INSERT INTO engine.user_erasures (user_id, erased_at) VALUES ($1, now()) \
             ON CONFLICT (user_id) DO UPDATE SET erased_at = EXCLUDED.erased_at \
             RETURNING erased_at",
        )
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(EraseReport {
            erased_at,
            deleted,
            blanked,
        })
    }

    /// Tombstones strictly after the `(since, after_user)` keyset tuple, in
    /// `(erased_at, user_id)` order, at most `limit`. The tuple comparison keeps
    /// rows sharing one timestamp from being skipped or repeated across a page
    /// boundary. Uses `idx_user_erasures_erased_at`.
    pub async fn list_changes(
        &self,
        since: DateTime<Utc>,
        after_user: Uuid,
        limit: i64,
    ) -> Result<Vec<ErasureRow>, sqlx::Error> {
        sqlx::query_as::<_, ErasureRow>(
            "SELECT user_id, erased_at FROM engine.user_erasures \
             WHERE (erased_at, user_id) > ($1, $2) \
             ORDER BY erased_at, user_id \
             LIMIT $3",
        )
        .bind(since)
        .bind(after_user)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::human_fact::{FactInput, FactSource, HumanFactRepo};
    use crate::human_insight::{HumanInsightRepo, ProjectedColumns};
    use crate::memory::{MemoryLayer, MemoryRepo};
    use crate::profile_state::ProfileStateRepo;
    use crate::testutil::{seed_chat_message, seed_chat_session};
    use chrono::TimeZone;

    /// Derived tables an erasure empties, each with the predicate that scopes a
    /// row to user `$1`. Written out here independently of the implementation.
    const ERASED_TABLES: &[(&str, &str)] = &[
        ("human_facts", "user_id = $1"),
        ("human_insights", "user_id = $1"),
        ("human_insights_snapshot", "user_id = $1"),
        ("companion_insights_snapshot", "user_id = $1"),
        ("human_profile_state", "user_id = $1"),
        ("companion_memories", "user_id = $1"),
        ("world_enrollments", "owner_uid = $1"),
        ("world_states", "owner_uid = $1"),
        ("world_memories", "owner_uid = $1"),
        ("world_posts", "owner_uid = $1"),
        ("persona_story_insights", "owner_uid = $1"),
        ("persona_story_events", "owner_uid = $1"),
        ("persona_story_memories", "owner_uid = $1"),
        (
            "user_insights",
            "instance_id IN (SELECT id FROM engine.persona_instances WHERE owner_uid = $1)",
        ),
        (
            "user_insights_snapshot",
            "instance_id IN (SELECT id FROM engine.persona_instances WHERE owner_uid = $1)",
        ),
        (
            "character_insights",
            "instance_id IN (SELECT id FROM engine.persona_instances WHERE owner_uid = $1)",
        ),
        (
            "character_insights_snapshot",
            "instance_id IN (SELECT id FROM engine.persona_instances WHERE owner_uid = $1)",
        ),
        (
            "relationship_scan_state",
            "instance_id IN (SELECT id FROM engine.persona_instances WHERE owner_uid = $1)",
        ),
        (
            "relationship_episodes",
            "instance_id IN (SELECT id FROM engine.persona_instances WHERE owner_uid = $1)",
        ),
        (
            "relationship_arcs",
            "instance_id IN (SELECT id FROM engine.persona_instances WHERE owner_uid = $1)",
        ),
        (
            "relationship_journal",
            "instance_id IN (SELECT id FROM engine.persona_instances WHERE owner_uid = $1)",
        ),
    ];

    /// Audit tables an erasure keeps and blanks: (table, scope to user `$1`,
    /// predicate that is true while the row still carries content).
    const KEPT_TABLES: &[(&str, &str, &str)] = &[
        (
            "human_insights_events",
            "user_id = $1",
            "payload IS NOT NULL",
        ),
        (
            "companion_decision_events",
            "user_id = $1",
            "payload IS NOT NULL",
        ),
        (
            "chat_images_events",
            "user_id = $1",
            "inputs <> '{}'::jsonb OR subject IS NOT NULL OR caption IS NOT NULL \
             OR composed_prompt IS NOT NULL OR last_failure IS NOT NULL",
        ),
        (
            "chat_vision_events",
            "user_id = $1",
            "vision IS NOT NULL OR image_url <> '' OR last_failure IS NOT NULL",
        ),
        (
            "user_insights_events",
            "instance_id IN (SELECT id FROM engine.persona_instances WHERE owner_uid = $1)",
            "payload IS NOT NULL",
        ),
        (
            "character_insights_events",
            "instance_id IN (SELECT id FROM engine.persona_instances WHERE owner_uid = $1)",
            "payload IS NOT NULL",
        ),
        (
            "companion_affinity",
            "user_id = $1",
            "feeling_clause IS NOT NULL OR feeling_clause_at IS NOT NULL",
        ),
        (
            "companion_affinity_events",
            "affinity_id IN (SELECT id FROM engine.companion_affinity WHERE user_id = $1)",
            "context <> '{}'::jsonb",
        ),
    ];

    /// Binds a seed statement's four ids; every statement ends in `FROM p`.
    const IDS: &str = "(SELECT $1::uuid AS u, $2::uuid AS i, $3::uuid AS s, $4::uuid AS m) p";

    /// Rows that need no repo: one per table, each carrying content.
    const SEEDS: &[&str] = &[
        "INSERT INTO engine.human_insights_snapshot (user_id, snapshot, captured_at) \
         SELECT p.u, '{\"interests\":[\"jazz\"]}', now()",
        "INSERT INTO engine.companion_insights_snapshot (user_id, insights, training_level, captured_at) \
         SELECT p.u, '{\"interests\":[\"jazz\"]}', 0.5, now()",
        "INSERT INTO engine.world_enrollments (owner_uid) SELECT p.u",
        "INSERT INTO engine.world_states (owner_uid, seed, digests) \
         SELECT p.u, '{\"a\":1}', '{\"b\":2}'",
        "INSERT INTO engine.world_memories (owner_uid, instance_id, content, embedding, script_date) \
         SELECT p.u, p.i, 'script', array_fill(0::real, ARRAY[512])::vector(512), current_date",
        "INSERT INTO engine.world_posts (owner_uid, instance_id, content, scheduled_at) \
         SELECT p.u, p.i, 'post', now()",
        "INSERT INTO engine.world_post_comments (post_id, content) \
         SELECT (SELECT id FROM engine.world_posts WHERE instance_id = p.i), 'comment'",
        "INSERT INTO engine.persona_story_insights (instance_id, owner_uid, city) \
         SELECT p.i, p.u, 'Lisbon'",
        "INSERT INTO engine.persona_story_events (owner_uid, instance_id, category, content, story_date) \
         SELECT p.u, p.i, 'work', 'story', current_date",
        "INSERT INTO engine.persona_story_memories \
             (owner_uid, instance_id, event_id, content, embedding, story_date) \
         SELECT p.u, p.i, (SELECT id FROM engine.persona_story_events WHERE instance_id = p.i), \
                'story', array_fill(0::real, ARRAY[512])::vector(512), current_date",
        "INSERT INTO engine.user_insights (instance_id, location) SELECT p.i, 'Lisbon'",
        "INSERT INTO engine.character_insights (instance_id, location) SELECT p.i, 'Porto'",
        "INSERT INTO engine.character_insights_snapshot (instance_id, snapshot, captured_at) \
         SELECT p.i, '{\"location\":\"Porto\"}', now()",
        "INSERT INTO engine.user_insights_snapshot (instance_id, snapshot, captured_at) \
         SELECT p.i, '{\"location\":\"Lisbon\"}', now()",
        "INSERT INTO engine.user_insights_events (run_id, instance_id, stage, status, payload) \
         SELECT gen_random_uuid(), p.i, 'extraction', 'ok', '{\"raw\":\"x\"}'",
        "INSERT INTO engine.character_insights_events (run_id, instance_id, stage, status, payload) \
         SELECT gen_random_uuid(), p.i, 'extraction', 'ok', '{\"raw\":\"x\"}'",
        "INSERT INTO engine.human_insights_events (run_id, user_id, stage, status, payload) \
         SELECT gen_random_uuid(), p.u, 'facts', 'ok', '{\"facts\":[\"x\"]}'",
        "INSERT INTO engine.companion_decision_events (run_id, user_id, status, payload) \
         SELECT gen_random_uuid(), p.u, 'ok', '{\"action\":\"reply\"}'",
        "INSERT INTO engine.chat_images_events \
             (source, user_id, session_id, status, inputs, subject, caption, composed_prompt, \
              last_failure, attempts) \
         SELECT 'compose_endpoint', p.u, p.s, 'ok', '{\"slot\":\"x\"}', 'subject', 'caption', \
                'prompt', 'upstream_error', 1",
        "INSERT INTO engine.chat_vision_events \
             (user_id, session_id, message_id, status, image_url, vision, last_failure, attempts) \
         SELECT p.u, p.s, p.m, 'ok', 'https://example.test/a.png', '{\"seen\":\"x\"}', \
                'upstream_error', 1",
        // Rows whose only content is the failure text.
        "INSERT INTO engine.chat_images_events \
             (source, user_id, session_id, status, inputs, last_failure, attempts) \
         SELECT 'compose_endpoint', p.u, p.s, 'exhausted', '{}', 'gateway_error', 1",
        "INSERT INTO engine.chat_vision_events \
             (user_id, session_id, message_id, status, image_url, last_failure, attempts) \
         SELECT p.u, p.s, p.m, 'exhausted', '', 'gateway_error', 1",
        "INSERT INTO engine.companion_affinity \
             (session_id, user_id, instance_id, feeling_clause, feeling_clause_at) \
         SELECT p.s, p.u, p.i, 'warm', now()",
        "INSERT INTO engine.companion_affinity_events (affinity_id, event_type, context) \
         SELECT (SELECT id FROM engine.companion_affinity WHERE session_id = p.s), 'message', \
                '{\"said\":\"x\"}'",
        "INSERT INTO engine.relationship_scan_state \
             (instance_id, scanned_through_at, live_from_at) \
         SELECT p.i, '2026-10-01T00:00:00Z', '2026-10-01T00:00:00Z'",
        "INSERT INTO engine.relationship_episodes (instance_id, summary, started_at, ended_at) \
         SELECT p.i, 'a stretch', '2026-10-01T00:00:00Z', '2026-10-02T00:00:00Z'",
        "INSERT INTO engine.relationship_arcs (instance_id, opening, arc, folded_through) \
         SELECT p.i, 'they met', 'they grew close', '2026-10-02T00:00:00Z'",
        "INSERT INTO engine.relationship_journal \
             (instance_id, occurred_on, kind, realm, summary, source, recurrence, embedding) \
         SELECT p.i, '2026-10-01', 'first_date', 'real', 'a dinner', 'llm', 'none', NULL",
        // Counts and ids only: an erasure leaves these rows alone.
        "INSERT INTO engine.relationship_scan_events (instance_id, kind, status) \
         SELECT p.i, 'scan', 'ok'",
    ];

    /// A profile write that holds the state row's lock when the erase starts is
    /// not lost to it: the erase waits, then deletes the row the write
    /// committed.
    #[sqlx::test(migrations = "./migrations")]
    async fn erase_waits_for_an_in_flight_profile_write_and_removes_what_it_wrote(pool: PgPool) {
        let user = Uuid::new_v4();
        let state = ProfileStateRepo { pool: &pool };
        state.bump(user, 1).await.unwrap();
        assert!(state
            .claim(user, std::time::Duration::from_secs(600))
            .await
            .unwrap());

        // The rebuild's side: the claimed row locked, the profile not yet committed.
        let mut write = pool.begin().await.unwrap();
        sqlx::query("SELECT user_id FROM engine.human_profile_state WHERE user_id = $1 FOR UPDATE")
            .bind(user)
            .fetch_one(&mut *write)
            .await
            .unwrap();

        let erase = tokio::spawn({
            let pool = pool.clone();
            async move { ErasureRepo { pool: &pool }.erase_user(user).await }
        });
        crate::testutil::wait_for_lock_wait(&pool).await;
        assert!(!erase.is_finished(), "the erase waits for the write");

        sqlx::query("INSERT INTO engine.human_insights (user_id, interests) VALUES ($1, '{jazz}')")
            .bind(user)
            .execute(&mut *write)
            .await
            .unwrap();
        write.commit().await.unwrap();

        let report = erase.await.unwrap().unwrap();
        assert_eq!(report.deleted["human_insights"], 1);
        assert_eq!(report.deleted["human_profile_state"], 1);
        assert!(HumanInsightRepo { pool: &pool }
            .load(user)
            .await
            .unwrap()
            .is_none());
    }

    /// One row of everything for `user`: the repo-written tables through their
    /// repos, the rest through `SEEDS`.
    async fn seed_footprint(pool: &PgPool, user: Uuid) {
        let session = seed_chat_session(pool, user).await;
        let message = seed_chat_message(pool, session).await;
        let instance: Uuid =
            sqlx::query_scalar("SELECT instance_id FROM engine.chat_sessions WHERE id = $1")
                .bind(session)
                .fetch_one(pool)
                .await
                .unwrap();

        HumanFactRepo { pool }
            .upsert_run(
                user,
                &[FactInput {
                    content: "likes jazz".into(),
                    category: "preference".into(),
                    domain: "hobby".into(),
                    evidence_type: "explicit_statement".into(),
                    temporality: "current".into(),
                    persistence: "stable".into(),
                    confidence: "high".into(),
                }],
                &FactSource {
                    session_id: Some(session),
                    message_id: Some(message),
                    run_id: Some(Uuid::new_v4()),
                },
            )
            .await
            .unwrap();
        HumanInsightRepo { pool }
            .replace_profile(
                user,
                &ProjectedColumns {
                    interests: vec!["jazz".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        ProfileStateRepo { pool }.bump(user, 1).await.unwrap();
        let memories = MemoryRepo { pool };
        let embedding = vec![0.0f32; 512];
        for (layer, owner_instance) in [
            (MemoryLayer::Profile, None),
            (MemoryLayer::Relationship, Some(instance)),
        ] {
            memories
                .upsert_if_live(
                    session,
                    layer,
                    session,
                    user,
                    owner_instance,
                    "remembered",
                    &embedding,
                    None,
                    None,
                )
                .await
                .unwrap()
                .expect("session is live");
        }

        for seed in SEEDS {
            sqlx::query(&format!("{seed} FROM {IDS}"))
                .bind(user)
                .bind(instance)
                .bind(session)
                .bind(message)
                .execute(pool)
                .await
                .unwrap_or_else(|e| panic!("seed failed: {e}\n{seed}"));
        }
    }

    async fn count(pool: &PgPool, table: &str, predicate: &str, user: Uuid) -> i64 {
        sqlx::query_scalar(&format!(
            "SELECT count(*) FROM engine.{table} WHERE {predicate}"
        ))
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Per table: rows in scope, and for audit tables the rows still carrying
    /// content.
    async fn footprint(pool: &PgPool, user: Uuid) -> BTreeMap<String, (i64, i64)> {
        let mut out = BTreeMap::new();
        for (table, scope) in ERASED_TABLES {
            out.insert(
                table.to_string(),
                (count(pool, table, scope, user).await, 0),
            );
        }
        for (table, scope, content) in KEPT_TABLES {
            out.insert(
                table.to_string(),
                (
                    count(pool, table, scope, user).await,
                    count(pool, table, &format!("({scope}) AND ({content})"), user).await,
                ),
            );
        }
        out
    }

    /// Scan-event rows of the user's instances; the table is in neither list.
    async fn scan_events(pool: &PgPool, user: Uuid) -> i64 {
        count(
            pool,
            "relationship_scan_events",
            "instance_id IN (SELECT id FROM engine.persona_instances WHERE owner_uid = $1)",
            user,
        )
        .await
    }

    async fn tombstones(pool: &PgPool, user: Uuid) -> Vec<DateTime<Utc>> {
        sqlx::query_scalar("SELECT erased_at FROM engine.user_erasures WHERE user_id = $1")
            .bind(user)
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn erase_user_empties_every_table_blanks_payloads_and_tombstones(pool: PgPool) {
        let (gone, kept) = (Uuid::new_v4(), Uuid::new_v4());
        seed_footprint(&pool, gone).await;
        seed_footprint(&pool, kept).await;
        let before = footprint(&pool, gone).await;
        let kept_before = footprint(&pool, kept).await;
        for (table, (rows, with_content)) in &before {
            assert!(*rows >= 1, "{table} was seeded");
            if KEPT_TABLES.iter().any(|(t, _, _)| t == table) {
                assert_eq!(rows, with_content, "{table} was seeded with content");
            }
        }

        let repo = ErasureRepo { pool: &pool };
        let report = repo.erase_user(gone).await.unwrap();

        // The report names every table, with the row count it touched.
        assert_eq!(report.deleted.len(), ERASED_TABLES.len());
        assert_eq!(report.blanked.len(), KEPT_TABLES.len());
        for (table, _) in ERASED_TABLES {
            assert_eq!(report.deleted[table], before[*table].0, "deleted {table}");
        }
        for (table, _, _) in KEPT_TABLES {
            assert_eq!(report.blanked[table], before[*table].0, "blanked {table}");
        }

        // Derived rows are gone; audit rows keep their ids and lose their content.
        let after = footprint(&pool, gone).await;
        for (table, _) in ERASED_TABLES {
            assert_eq!(after[*table].0, 0, "{table} emptied");
        }
        for (table, _, _) in KEPT_TABLES {
            assert_eq!(after[*table], (before[*table].0, 0), "{table} blanked");
        }
        // A post's comments go with it; only the other user's remain.
        let comments: i64 = sqlx::query_scalar("SELECT count(*) FROM engine.world_post_comments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(comments, 1);

        // The other user is untouched.
        assert_eq!(footprint(&pool, kept).await, kept_before);

        // Scan events hold counts and ids only: neither deleted nor blanked.
        assert_eq!(scan_events(&pool, gone).await, 1);
        assert_eq!(scan_events(&pool, kept).await, 1);

        // One tombstone, stamped with the report's time.
        assert_eq!(tombstones(&pool, gone).await, vec![report.erased_at]);
        assert!(tombstones(&pool, kept).await.is_empty());

        // A second call finds nothing, keeps one tombstone, and moves it forward.
        let again = repo.erase_user(gone).await.unwrap();
        assert!(
            again.deleted.values().all(|n| *n == 0),
            "{:?}",
            again.deleted
        );
        assert!(
            again.blanked.values().all(|n| *n == 0),
            "{:?}",
            again.blanked
        );
        assert_eq!(again.deleted.len(), ERASED_TABLES.len());
        assert_eq!(again.blanked.len(), KEPT_TABLES.len());
        assert!(again.erased_at > report.erased_at);
        assert_eq!(tombstones(&pool, gone).await, vec![again.erased_at]);
        assert_eq!(footprint(&pool, kept).await, kept_before);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn erase_user_with_nothing_to_erase_still_tombstones(pool: PgPool) {
        let stranger = Uuid::new_v4();
        let report = ErasureRepo { pool: &pool }
            .erase_user(stranger)
            .await
            .unwrap();
        assert!(report.deleted.values().all(|n| *n == 0));
        assert!(report.blanked.values().all(|n| *n == 0));
        assert_eq!(tombstones(&pool, stranger).await, vec![report.erased_at]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn list_changes_keyset_pages_without_skip_or_repeat(pool: PgPool) {
        let repo = ErasureRepo { pool: &pool };
        let t0 = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap();
        // Three tombstones tie on t0, one is later.
        let mut expected: Vec<(DateTime<Utc>, Uuid)> = Vec::new();
        for at in [t0, t0, t0, t1] {
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO engine.user_erasures (user_id, erased_at) VALUES ($1, $2)")
                .bind(id)
                .bind(at)
                .execute(&pool)
                .await
                .unwrap();
            expected.push((at, id));
        }
        expected.sort();

        let mut walked = Vec::new();
        let mut cursor = (DateTime::<Utc>::UNIX_EPOCH, Uuid::nil());
        loop {
            let page = repo.list_changes(cursor.0, cursor.1, 2).await.unwrap();
            walked.extend(page.iter().map(|r| (r.erased_at, r.user_id)));
            if page.len() < 2 {
                break;
            }
            let last = page.last().unwrap();
            cursor = (last.erased_at, last.user_id);
        }
        assert_eq!(
            walked, expected,
            "every row once, in (erased_at, user_id) order"
        );
    }
}
