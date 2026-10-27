// SPDX-License-Identifier: AGPL-3.0-only
//! Postgres + pgvector persistence layer.

pub mod affinity;
pub mod character_insight;
pub mod chat;
pub mod chat_queue;
pub mod decision;
pub mod erasure;
pub mod error_handling;
pub mod events;
pub mod generation;
pub mod group;
pub mod human_fact;
pub mod human_insight;
pub mod image_events;
pub mod insight;
pub mod life_choices;
pub mod memory;
pub mod persona;
pub mod pool;
pub mod profile_state;
pub mod session_archive;
pub mod story;
pub mod user_insight;
pub mod world;
pub mod world_town;

pub use sqlx::PgPool;

/// How many characters of an unparseable LLM reply are kept in a `parse_error`
/// audit row. Enough to tell a refusal from a truncated object; short enough
/// that a runaway reply cannot bloat the table.
pub const RAW_PAYLOAD_MAX_CHARS: usize = 2000;

/// Audit payload for a `parse_error` row: the reply itself, truncated. Without
/// it a whole-turn refusal and malformed JSON produce the same row — and
/// refusal is the likeliest way a mined fact goes missing. Truncation counts
/// characters, not bytes: byte slicing would panic mid-codepoint.
///
/// Shared by all three insight chains (human, character, user).
pub fn parse_error_payload(raw: &str) -> serde_json::Value {
    let truncated: String = raw.chars().take(RAW_PAYLOAD_MAX_CHARS).collect();
    serde_json::json!({ "raw": truncated })
}

/// Names — never values — of the columns that were already populated when a
/// structuring call was made. The structurer's input is facts PLUS the existing
/// profile, so without this a fact missing from the output is ambiguous between
/// "dropped" and "judged already covered".
pub fn existing_keys(existing: Option<&serde_json::Value>) -> Vec<String> {
    existing
        .and_then(|v| v.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

/// Test helpers shared across this crate's `#[cfg(test)]` modules.
#[cfg(test)]
pub(crate) mod testutil {
    use sqlx::PgPool;
    use uuid::Uuid;

    /// Persist a genome + active instance and return the instance id.
    /// Since 0040 `chat_sessions.instance_id`, `companion_affinity.instance_id`
    /// and `companion_memories.instance_id` carry real FKs, so a test that just
    /// needs "some valid instance" has to own one rather than fabricate a bare
    /// `Uuid::new_v4()`. The genome name is randomised so repeat calls never
    /// collide on `persona_instances UNIQUE(genome_id, owner_uid)`.
    pub(crate) async fn seed_persona_instance(pool: &PgPool, owner: Uuid) -> Uuid {
        let genome_id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ($1, 'you are a companion', '{}'::jsonb) RETURNING id",
        )
        .bind(format!("seed-{}", Uuid::new_v4()))
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id)
        .bind(owner)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Persist a BYOC instance (no genome, random key) and return its id.
    pub(crate) async fn seed_byoc_instance(pool: &PgPool, owner: Uuid) -> Uuid {
        crate::persona::PersonaRepo { pool }
            .create_byoc_instance(owner, &rand::random())
            .await
            .unwrap()
    }

    /// Persist a chat session (with its own instance) and return its id. Same
    /// reason as `seed_persona_instance`: since 0058 six event tables
    /// FK-reference `chat_sessions.id` — images, vision, the two companion_*
    /// and the two *_insights_events — so a fabricated `Uuid::new_v4()` session
    /// id no longer inserts.
    pub(crate) async fn seed_chat_session(pool: &PgPool, owner: Uuid) -> Uuid {
        let instance_id = seed_persona_instance(pool, owner).await;
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(owner)
        .bind(instance_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Persist a text session for an existing `(user_id, instance_id)` pair,
    /// archived or not, and return its id — for the `_if_live` writers, whose
    /// whole contract is what they do on each side of that flag.
    pub(crate) async fn seed_session(
        pool: &PgPool,
        user_id: Uuid,
        instance_id: Uuid,
        archived: bool,
    ) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, archived) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .bind(archived)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Persist one `role='user'` message in `session_id` and return its id, for
    /// the six event tables and `chat_messages.user_message_id` itself, all of
    /// which FK-reference `chat_messages.id` since 0058.
    pub(crate) async fn seed_chat_message(pool: &PgPool, session_id: Uuid) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.chat_messages (session_id, role, content) \
             VALUES ($1, 'user', 'seed') RETURNING id",
        )
        .bind(session_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// The `model` every `seed_generation` parent carries.
    pub(crate) const SEEDED_MODEL: &str = "seeded/model";

    /// The `usage` block every `seed_generation` parent carries.
    pub(crate) fn seeded_usage() -> serde_json::Value {
        serde_json::json!({ "total_tokens": 7 })
    }

    /// Persist one `engine.llm_generations` parent so a test may put `id` in a
    /// child row's `generation_id`. Since 0060 all eight of those columns carry
    /// a VALIDATED foreign key, so a fabricated id no longer inserts — same
    /// shape as `seed_chat_session` after 0058.
    ///
    /// The `task` is a literal `'test'` on purpose: the column is NOT NULL and
    /// carries no CHECK (0059 — the vocabulary is deployer-extensible config),
    /// and a test that needed a REAL task value would be testing 0060's
    /// backfill — spent: it ran in production, and its replay test retired
    /// in B2-code, ahead of 0061 dropping the columns it seeded and read.
    ///
    /// `model` and `usage` are non-NULL for a reason: since B1 the child rows
    /// no longer carry them, so tests that used to assert on a child column now
    /// assert through a join to here. Seeding NULLs would let those tests pass
    /// just as happily with the join deleted — `SEEDED_MODEL` / `seeded_usage`
    /// are what gives them something only a working join can return.
    ///
    /// `ON CONFLICT DO NOTHING` so a test may seed the same id twice — several
    /// exercise a replay or a double-write race on one generation.
    pub(crate) async fn seed_generation(pool: &PgPool, id: &str) {
        sqlx::query(
            "INSERT INTO engine.llm_generations (generation_id, task, model, usage) \
             VALUES ($1, 'test', $2, $3) ON CONFLICT (generation_id) DO NOTHING",
        )
        .bind(id)
        .bind(SEEDED_MODEL)
        .bind(seeded_usage())
        .execute(pool)
        .await
        .expect("seed llm_generations parent");
    }
}

#[cfg(test)]
mod audit_helper_tests {
    use super::{existing_keys, parse_error_payload, RAW_PAYLOAD_MAX_CHARS};

    #[test]
    fn parse_error_payload_truncates_by_chars_not_bytes() {
        // Multi-byte on purpose: byte slicing would panic mid-codepoint.
        let raw: String = "汉".repeat(RAW_PAYLOAD_MAX_CHARS + 10);
        let v = parse_error_payload(&raw);
        let kept = v["raw"].as_str().expect("raw is a string");
        assert_eq!(kept.chars().count(), RAW_PAYLOAD_MAX_CHARS);
    }

    #[test]
    fn parse_error_payload_keeps_short_replies_whole() {
        let v = parse_error_payload("I can't help with that.");
        assert_eq!(v["raw"], "I can't help with that.");
    }

    #[test]
    fn existing_keys_lists_names_and_never_values() {
        let existing = serde_json::json!({"location": "公司", "likes": ["下雨天的味道"]});
        let mut keys = existing_keys(Some(&existing));
        keys.sort();
        assert_eq!(keys, vec!["likes".to_string(), "location".to_string()]);
    }

    #[test]
    fn existing_keys_none_is_empty() {
        assert!(existing_keys(None).is_empty());
    }
}

#[cfg(test)]
mod migration_tests {
    use sqlx::PgPool;
    use uuid::Uuid;

    #[sqlx::test(migrations = "./migrations")]
    async fn human_insights_has_rls_enabled(pool: PgPool) {
        let enabled: bool = sqlx::query_scalar(
            "SELECT relrowsecurity FROM pg_class \
             WHERE oid = 'engine.human_insights'::regclass",
        )
        .fetch_one(&pool)
        .await
        .expect("query relrowsecurity for human_insights");
        assert!(enabled, "RLS must be enabled on engine.human_insights");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn sqlx_migrations_has_rls_enabled(pool: PgPool) {
        let enabled: bool = sqlx::query_scalar(
            "SELECT relrowsecurity FROM pg_class \
             WHERE oid = 'public._sqlx_migrations'::regclass",
        )
        .fetch_one(&pool)
        .await
        .expect("query relrowsecurity for _sqlx_migrations");
        assert!(enabled, "RLS must be enabled on public._sqlx_migrations");
    }

    /// 0040 closed the gap where the three original instance-scoped tables
    /// referenced `persona_instances` by convention only. Pins both the
    /// existence of each FK and its ON DELETE action — a plain `REFERENCES`
    /// (confdeltype 'a' = NO ACTION) would silently block instance deletion
    /// instead of wiping the instance's data, which is not what 0040 chose.
    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0040_instance_fks_exist_with_cascade(pool: PgPool) {
        for table in ["chat_sessions", "companion_affinity", "companion_memories"] {
            let deltype: Option<i8> = sqlx::query_scalar(
                "SELECT c.confdeltype::text::\"char\" FROM pg_constraint c \
                 JOIN unnest(c.conkey) k(attnum) ON true \
                 JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum \
                 WHERE c.contype = 'f' \
                   AND c.conrelid = ('engine.' || $1)::regclass \
                   AND c.confrelid = 'engine.persona_instances'::regclass \
                   AND a.attname = 'instance_id'",
            )
            .bind(table)
            .fetch_optional(&pool)
            .await
            .expect("query instance_id foreign key")
            .flatten();
            assert_eq!(
                deltype,
                Some(b'c' as i8),
                "engine.{table}.instance_id must carry an ON DELETE CASCADE FK to persona_instances"
            );
        }
    }

    /// 0058 closed the FK gap across `engine.*`. Pins each constraint's
    /// existence AND its ON DELETE action, because the action is the design:
    /// SET NULL ('n') keeps an audit row alive after what it points at is gone,
    /// where the default NO ACTION ('a') — what a bare `REFERENCES` produces —
    /// would block the delete instead.
    ///
    /// Attribution columns are absent by design, not oversight; see the
    /// blanket test below for which and why.
    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0058_reference_columns_carry_fks_with_the_right_action(pool: PgPool) {
        // (table, column, parent, expected confdeltype)
        let cases = [
            (
                "chat_images_events",
                "instance_id",
                "persona_instances",
                b'n',
            ),
            ("chat_images_events", "session_id", "chat_sessions", b'n'),
            ("chat_vision_events", "session_id", "chat_sessions", b'n'),
            ("chat_vision_events", "message_id", "chat_messages", b'n'),
            (
                "companion_insights_events",
                "session_id",
                "chat_sessions",
                b'n',
            ),
            (
                "companion_insights_events",
                "message_id",
                "chat_messages",
                b'n',
            ),
            (
                "companion_decision_events",
                "session_id",
                "chat_sessions",
                b'n',
            ),
            (
                "companion_decision_events",
                "message_id",
                "chat_messages",
                b'n',
            ),
            (
                "character_insights_events",
                "session_id",
                "chat_sessions",
                b'n',
            ),
            (
                "character_insights_events",
                "message_id",
                "chat_messages",
                b'n',
            ),
            ("user_insights_events", "session_id", "chat_sessions", b'n'),
            ("user_insights_events", "message_id", "chat_messages", b'n'),
            ("chat_messages", "user_message_id", "chat_messages", b'n'),
            (
                "chat_messages",
                "continues_from_message_id",
                "chat_messages",
                b'n',
            ),
        ];
        for (table, column, parent, want) in cases {
            let deltype: Option<i8> = sqlx::query_scalar(
                "SELECT c.confdeltype::text::\"char\" FROM pg_constraint c \
                 JOIN unnest(c.conkey) k(attnum) ON true \
                 JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum \
                 WHERE c.contype = 'f' \
                   AND c.conrelid = ('engine.' || $1)::regclass \
                   AND c.confrelid = ('engine.' || $3)::regclass \
                   AND a.attname = $2",
            )
            .bind(table)
            .bind(column)
            .bind(parent)
            .fetch_optional(&pool)
            .await
            .expect("query reference-column foreign key")
            .flatten();
            assert_eq!(
                deltype,
                Some(want as i8),
                "engine.{table}.{column} must carry an FK to {parent} with \
                 confdeltype '{}'",
                want as char
            );
        }
    }

    /// The rule the above encodes: every column in `engine.*` that names
    /// another table's row carries a foreign key. Fails when a new table or
    /// column ships without one, which is how the gap 0058 closed opened in the
    /// first place — table by table, each one copying the last.
    ///
    /// Attribution columns are the deliberate exclusion — the one column a row
    /// has that leads back to a person. An FK there either deletes the evidence
    /// or blanks the only handle anyone could find it by. That is every
    /// `user_id` / `owner_uid`, plus `instance_id` on the four insights tables
    /// that carry neither.
    ///
    /// `run_id` is excluded per table, not by name: on the four insight/decision
    /// event tables it is a correlation token tying the stages of one run
    /// together, with no `runs` table behind it. Excluding the NAME schema-wide
    /// would silently exempt a future `run_id` that really does reference
    /// something. `human_facts.first_run_id` / `last_run_id` are the same
    /// token, naming the extraction run that first and last emitted the fact.
    ///
    /// Covers `relkind` `'r'` and `'p'` — a partitioned table can carry an FK
    /// and must not slip through. Materialized views cannot, so they are out.
    #[sqlx::test(migrations = "./migrations")]
    async fn every_reference_column_in_engine_has_a_foreign_key(pool: PgPool) {
        let unlinked: Vec<(String, String)> = sqlx::query_as(
            "SELECT c.relname::text, a.attname::text \
             FROM pg_attribute a \
             JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'engine' AND c.relkind IN ('r', 'p') \
               AND a.attnum > 0 AND NOT a.attisdropped \
               AND a.atttypid = 'uuid'::regtype \
               AND a.attname <> 'id' \
               AND a.attname NOT IN ('user_id', 'owner_uid') \
               AND NOT (a.attname = 'instance_id' AND c.relname IN ( \
                     'character_insights_events', 'character_insights_snapshot', \
                     'user_insights_events', 'user_insights_snapshot')) \
               AND NOT (a.attname = 'run_id' AND c.relname IN ( \
                     'companion_insights_events', 'companion_decision_events', \
                     'character_insights_events', 'user_insights_events')) \
               AND NOT (a.attname IN ('first_run_id', 'last_run_id') \
                        AND c.relname = 'human_facts') \
               AND NOT EXISTS ( \
                     SELECT 1 FROM pg_constraint fk \
                     JOIN unnest(fk.conkey) k(attnum) ON k.attnum = a.attnum \
                     WHERE fk.contype = 'f' AND fk.conrelid = c.oid) \
             ORDER BY 1, 2",
        )
        .fetch_all(&pool)
        .await
        .expect("scan engine.* for unlinked uuid columns");
        assert!(
            unlinked.is_empty(),
            "every uuid column naming another table's row needs a foreign key \
             (see CLAUDE.md, 关联列与外键); unlinked: {unlinked:?}"
        );
    }

    /// The SET NULL FKs above are only expressible because 0058 dropped these
    /// two NOT NULLs. If either came back, deleting a session would error
    /// instead of blanking the pointer.
    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0058_vision_event_pointers_are_nullable(pool: PgPool) {
        for column in ["session_id", "message_id"] {
            let nullable: String = sqlx::query_scalar(
                "SELECT is_nullable FROM information_schema.columns \
                 WHERE table_schema = 'engine' AND table_name = 'chat_vision_events' \
                   AND column_name = $1",
            )
            .bind(column)
            .fetch_one(&pool)
            .await
            .expect("query is_nullable for chat_vision_events");
            assert_eq!(
                nullable, "YES",
                "chat_vision_events.{column} must be nullable for ON DELETE SET NULL"
            );
        }
    }

    /// 0060 makes `engine.llm_generations` the parent of every child
    /// `generation_id`. SET NULL ('n'), not the default NO ACTION ('a'): the
    /// model and usage a row records stay reconcilable against the provider's
    /// own log after the parent is gone.
    ///
    /// `chat_messages.f_generation_id` is absent on purpose (spec §7.2) — it is
    /// written by `mark_filtered`'s separate UPDATE path, and production holds
    /// exactly one non-null value in it.
    ///
    /// Note this cannot ride on `every_reference_column_in_engine_has_a_foreign_key`:
    /// that scan is `uuid`-typed only, and `generation_id` is TEXT (the
    /// provider's opaque handle, stored verbatim).
    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0060_generation_id_columns_carry_fks_to_llm_generations(pool: PgPool) {
        for table in [
            "chat_messages",
            "companion_affinity_events",
            "companion_insights_events",
            "companion_decision_events",
            "chat_images_events",
            "chat_vision_events",
            "character_insights_events",
            "user_insights_events",
        ] {
            let deltype: Option<i8> = sqlx::query_scalar(
                "SELECT c.confdeltype::text::\"char\" FROM pg_constraint c \
                 JOIN unnest(c.conkey) k(attnum) ON true \
                 JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum \
                 WHERE c.contype = 'f' \
                   AND c.conrelid = ('engine.' || $1)::regclass \
                   AND c.confrelid = 'engine.llm_generations'::regclass \
                   AND a.attname = 'generation_id'",
            )
            .bind(table)
            .fetch_optional(&pool)
            .await
            .expect("query generation_id foreign key")
            .flatten();
            assert_eq!(
                deltype,
                Some(b'n' as i8),
                "engine.{table}.generation_id must carry an ON DELETE SET NULL FK \
                 to llm_generations"
            );
        }

        // Validated, reversing 0058's blanket NOT VALID rule. The retrospective
        // guarantee is the whole point (spec §7.3); this catches a later hand
        // adding one NOT VALID for convenience.
        let unvalidated: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_constraint \
             WHERE contype = 'f' AND confrelid = 'engine.llm_generations'::regclass \
               AND NOT convalidated",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            unvalidated, 0,
            "every FK to llm_generations must be validated"
        );

        let f: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_constraint c \
             JOIN unnest(c.conkey) k(attnum) ON true \
             JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum \
             WHERE c.contype = 'f' AND c.conrelid = 'engine.chat_messages'::regclass \
               AND a.attname = 'f_generation_id'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(f, 0, "f_generation_id must stay unconstrained (spec §7.2)");

        // Every FK column needs its own index or the parent's ON DELETE
        // seq-scans the child. 0060 adds all eight; none existed before.
        for table in [
            "chat_messages",
            "companion_affinity_events",
            "companion_insights_events",
            "companion_decision_events",
            "chat_images_events",
            "chat_vision_events",
            "character_insights_events",
            "user_insights_events",
        ] {
            let indexed: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_index i \
                   JOIN pg_attribute a ON a.attrelid = i.indrelid \
                                      AND a.attnum = i.indkey[0] \
                  WHERE i.indrelid = ('engine.' || $1)::regclass \
                    AND a.attname = 'generation_id')",
            )
            .bind(table)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(
                indexed,
                "engine.{table}.generation_id must lead an index, or every \
                 llm_generations delete seq-scans it"
            );
        }
    }

    /// 0061 (B2) drops `model` and `usage` from the eight child tables — the
    /// same generation's model and usage now live in exactly one place,
    /// `engine.llm_generations`, reached by the join on `generation_id`.
    ///
    /// 0060's backfill replay test retired in B2-code, ahead of this DROP
    /// (its own comment said it would go with the columns it seeded): the
    /// backfill ran in production and cannot execute against the post-0061
    /// schema.
    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0061_child_model_and_usage_columns_are_gone(pool: PgPool) {
        for table in [
            "chat_messages",
            "companion_affinity_events",
            "companion_insights_events",
            "companion_decision_events",
            "chat_images_events",
            "chat_vision_events",
            "character_insights_events",
            "user_insights_events",
        ] {
            for column in ["model", "usage"] {
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
                     WHERE table_schema = 'engine' AND table_name = $1 \
                       AND column_name = $2)",
                )
                .bind(table)
                .bind(column)
                .fetch_one(&pool)
                .await
                .expect("query information_schema for a dropped column");
                assert!(
                    !exists,
                    "engine.{table}.{column} must be dropped by 0061; the only \
                     copy lives in llm_generations, reached by the join"
                );
            }
        }

        // The one place they still live.
        for column in ["model", "usage"] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
                 WHERE table_schema = 'engine' AND table_name = 'llm_generations' \
                   AND column_name = $1)",
            )
            .bind(column)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(exists, "llm_generations.{column} is the surviving copy");
        }

        // filter_model survives on purpose (spec §7.4): 2,024 rows hold the
        // regex arm's '<regex>' sentinel with no f_generation_id at all — a
        // discriminator that happens to be spelled like a model name, not a
        // redundant copy of anything in the parent table.
        let filter_model: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_schema = 'engine' AND table_name = 'chat_messages' \
               AND column_name = 'filter_model')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            filter_model,
            "filter_model is a discriminator, not a copy — it survives B2"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn chat_messages_has_no_extracted_facts_column(pool: PgPool) {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_schema = 'engine' AND table_name = 'chat_messages' \
               AND column_name = 'extracted_facts')",
        )
        .fetch_one(&pool)
        .await
        .expect("query for extracted_facts column");
        assert!(
            !exists,
            "extracted_facts column must be dropped (migration 0017)"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0020_seeds_ten_fallback_phrases(pool: PgPool) {
        let payload: serde_json::Value = sqlx::query_scalar(
            "SELECT payload FROM engine.error_handling_config \
             WHERE kind = 'chat_stream_failure_fallback_phrases'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let arr = payload.as_array().expect("payload is an array");
        assert_eq!(arr.len(), 10, "seed must carry exactly 10 phrases");
        for item in arr {
            assert!(item.is_string(), "each phrase must be a string: {item}");
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn pick_chat_stream_fallback_phrase_returns_seeded_phrase(pool: PgPool) {
        use crate::error_handling::ErrorHandlingRepo;
        let repo = ErrorHandlingRepo { pool: &pool };
        let phrase = repo.pick_chat_stream_fallback_phrase().await.unwrap();
        let phrase = phrase.expect("seeded phrase should be available");
        let seeded = [
            "huh?",
            "hm?",
            "...",
            "oh?",
            "mhm",
            "ok",
            "👀",
            "😅",
            "say again?",
            "wait what?",
        ];
        assert!(
            seeded.contains(&phrase.as_str()),
            "picked {phrase:?} not in seed"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn pick_chat_stream_fallback_phrase_returns_none_when_kind_missing(pool: PgPool) {
        use crate::error_handling::ErrorHandlingRepo;
        let repo = ErrorHandlingRepo { pool: &pool };
        // Clear the seeded row to simulate a fresh DB without the kind.
        sqlx::query(
            "DELETE FROM engine.error_handling_config \
             WHERE kind = 'chat_stream_failure_fallback_phrases'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let phrase = repo.pick_chat_stream_fallback_phrase().await.unwrap();
        assert!(phrase.is_none(), "expected None when config row absent");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn companion_insights_snapshot_schema(pool: PgPool) {
        // Table exists with the five expected columns at expected types.
        let cols: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT column_name, data_type, is_nullable \
             FROM information_schema.columns \
             WHERE table_schema = 'engine' \
               AND table_name   = 'companion_insights_snapshot' \
             ORDER BY ordinal_position",
        )
        .fetch_all(&pool)
        .await
        .expect("query columns for companion_insights_snapshot");
        let names: Vec<&str> = cols.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["id", "user_id", "insights", "training_level", "captured_at"],
            "column order/identity must match the migration"
        );
        // captured_at must be NOT NULL — sweeper sets it explicitly.
        let captured_at_null = cols
            .iter()
            .find(|(n, _, _)| n == "captured_at")
            .map(|(_, _, nullable)| nullable.as_str())
            .unwrap();
        assert_eq!(captured_at_null, "NO", "captured_at must be NOT NULL");

        // Index on (user_id, captured_at DESC) exists.
        let idx_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS ( \
                SELECT 1 FROM pg_indexes \
                 WHERE schemaname = 'engine' \
                   AND tablename  = 'companion_insights_snapshot' \
                   AND indexname  = 'idx_companion_insights_snapshot_user_time')",
        )
        .fetch_one(&pool)
        .await
        .expect("query pg_indexes");
        assert!(
            idx_exists,
            "idx_companion_insights_snapshot_user_time must be created by 0021"
        );

        // RLS enabled (no policy → server-side only access).
        let rls_enabled: bool = sqlx::query_scalar(
            "SELECT relrowsecurity FROM pg_class \
              WHERE oid = 'engine.companion_insights_snapshot'::regclass",
        )
        .fetch_one(&pool)
        .await
        .expect("query relrowsecurity");
        assert!(
            rls_enabled,
            "RLS must be enabled on companion_insights_snapshot"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn human_insights_snapshot_schema(pool: PgPool) {
        let cols: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT column_name, data_type, is_nullable \
             FROM information_schema.columns \
             WHERE table_schema = 'engine' \
               AND table_name   = 'human_insights_snapshot' \
             ORDER BY ordinal_position",
        )
        .fetch_all(&pool)
        .await
        .expect("query columns for human_insights_snapshot");
        let names: Vec<&str> = cols.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["id", "user_id", "snapshot", "captured_at"],
            "column order/identity must match migration 0042"
        );
        let captured_at_null = cols
            .iter()
            .find(|(n, _, _)| n == "captured_at")
            .map(|(_, _, nullable)| nullable.as_str())
            .unwrap();
        assert_eq!(captured_at_null, "NO", "captured_at must be NOT NULL");

        let idx_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS ( \
                SELECT 1 FROM pg_indexes \
                 WHERE schemaname = 'engine' \
                   AND tablename  = 'human_insights_snapshot' \
                   AND indexname  = 'idx_human_insights_snapshot_user_time')",
        )
        .fetch_one(&pool)
        .await
        .expect("query pg_indexes");
        assert!(idx_exists);

        let rls_enabled: bool = sqlx::query_scalar(
            "SELECT relrowsecurity FROM pg_class \
              WHERE oid = 'engine.human_insights_snapshot'::regclass",
        )
        .fetch_one(&pool)
        .await
        .expect("query relrowsecurity");
        assert!(rls_enabled);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0042_dropped_companion_insights_and_lead_score(pool: PgPool) {
        let ci_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
              WHERE table_schema = 'engine' AND table_name = 'companion_insights')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!ci_exists, "companion_insights must be dropped");

        let lead_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
              WHERE table_schema = 'engine' AND table_name = 'chat_sessions' \
                AND column_name = 'lead_score')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!lead_exists, "chat_sessions.lead_score must be dropped");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0023_drops_nft_ownership_stack(pool: PgPool) {
        // The three tables are gone.
        for tbl in ["wallet_links", "persona_ownership", "sync_cursors"] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
                 WHERE table_schema = 'engine' AND table_name = $1)",
            )
            .bind(tbl)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(!exists, "engine.{tbl} must be dropped by migration 0023");
        }
        // persona_genomes.asset_id is gone.
        let col_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_schema = 'engine' AND table_name = 'persona_genomes' \
               AND column_name = 'asset_id')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!col_exists, "persona_genomes.asset_id must be dropped");
        // persona_genomes itself survives (sanity).
        let pg_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_schema = 'engine' AND table_name = 'persona_genomes')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(pg_exists, "persona_genomes table must survive");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0024_strips_persona_genomes_to_chat_data(pool: PgPool) {
        for col in ["is_active", "avatar_url"] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
                 WHERE table_schema = 'engine' AND table_name = 'persona_genomes' \
                   AND column_name = $1)",
            )
            .bind(col)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(
                !exists,
                "persona_genomes.{col} must be dropped by migration 0024"
            );
        }
        // The chat-relevant columns survive.
        for col in ["name", "system_prompt", "tip_personality", "art_metadata"] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
                 WHERE table_schema = 'engine' AND table_name = 'persona_genomes' \
                   AND column_name = $1)",
            )
            .bind(col)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(exists, "persona_genomes.{col} must survive migration 0024");
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0035_world_tables_schema(pool: PgPool) {
        // All three tables exist with RLS enabled (0013-style lockdown).
        for tbl in ["world_enrollments", "world_states", "world_memories"] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
                 WHERE table_schema = 'engine' AND table_name = $1)",
            )
            .bind(tbl)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(exists, "engine.{tbl} must be created by migration 0035");
            let rls: bool = sqlx::query_scalar(&format!(
                "SELECT relrowsecurity FROM pg_class WHERE oid = 'engine.{tbl}'::regclass"
            ))
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(rls, "RLS must be enabled on engine.{tbl}");
        }

        // world_states column identity/order.
        let cols: Vec<String> = sqlx::query_scalar(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = 'engine' AND table_name = 'world_states' \
             ORDER BY ordinal_position",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            cols,
            vec![
                "owner_uid",
                "seed",
                "digests",
                "seed_version",
                "last_run_at",
                "claimed_at",
                "updated_at",
                "last_comment_round_at",
                "worldview_hash",
                "worldview_set_at"
            ],
        );

        // world_memories embedding is a 512-dim vector and the two indexes exist.
        let emb_type: String = sqlx::query_scalar(
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = 'engine.world_memories'::regclass AND attname = 'embedding'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(emb_type, "vector(512)");
        for idx in [
            "idx_world_memories_owner_instance",
            "idx_world_memories_embedding",
        ] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_indexes \
                 WHERE schemaname = 'engine' AND tablename = 'world_memories' \
                   AND indexname = $1)",
            )
            .bind(idx)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(exists, "{idx} must exist");
        }
    }

    #[sqlx::test]
    async fn migration_0036_world_town_schema(pool: PgPool) {
        // Both town tables exist with RLS enabled (0013-style lockdown).
        for tbl in ["world_posts", "world_post_comments"] {
            let rls: bool = sqlx::query_scalar(
                "SELECT relrowsecurity FROM pg_class \
                 WHERE oid = ('engine.' || $1)::regclass",
            )
            .bind(tbl)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(rls, "{tbl} must have RLS enabled");
        }

        // Column adds landed.
        let town_enabled_type: String = sqlx::query_scalar(
            "SELECT data_type FROM information_schema.columns \
             WHERE table_schema = 'engine' AND table_name = 'world_enrollments' \
               AND column_name = 'town_enabled'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(town_enabled_type, "boolean");
        let round_col: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.columns \
             WHERE table_schema = 'engine' AND table_name = 'world_states' \
               AND column_name = 'last_comment_round_at'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(round_col, 1);

        // Feed + due + thread indexes exist.
        for (tbl, idx) in [
            ("world_posts", "idx_world_posts_due"),
            ("world_posts", "idx_world_posts_feed"),
            ("world_post_comments", "idx_world_post_comments_thread"),
        ] {
            let n: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_indexes \
                 WHERE schemaname = 'engine' AND tablename = $1 AND indexname = $2",
            )
            .bind(tbl)
            .bind(idx)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(n, 1, "missing index {idx}");
        }

        // source/author coupling CHECK: user row (NULL, NULL) ok; persona row
        // with NULL source rejected.
        let owner = uuid::Uuid::new_v4();
        let genome: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('T','p','{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let inst: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) \
             VALUES ($1,$2) RETURNING id",
        )
        .bind(genome)
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        let post: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO engine.world_posts (owner_uid, instance_id, content, scheduled_at) \
             VALUES ($1,$2,'hello',now()) RETURNING id",
        )
        .bind(owner)
        .bind(inst)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.world_post_comments (post_id, author_instance_id, source, content) \
             VALUES ($1, NULL, NULL, 'user says hi')",
        )
        .bind(post)
        .execute(&pool)
        .await
        .expect("user comment row inserts");
        let err = sqlx::query(
            "INSERT INTO engine.world_post_comments (post_id, author_instance_id, source, content) \
             VALUES ($1, $2, NULL, 'bad')",
        )
        .bind(post)
        .bind(inst)
        .execute(&pool)
        .await;
        assert!(
            err.is_err(),
            "persona comment without source must violate CHECK"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0037_reply_window_schema(pool: PgPool) {
        // Nullable timestamptz column landed.
        let (data_type, is_nullable): (String, String) = sqlx::query_as(
            "SELECT data_type, is_nullable FROM information_schema.columns \
             WHERE table_schema = 'engine' AND table_name = 'world_posts' \
               AND column_name = 'last_user_comment_at'",
        )
        .fetch_one(&pool)
        .await
        .expect("last_user_comment_at column exists");
        assert_eq!(data_type, "timestamp with time zone");
        assert_eq!(is_nullable, "YES");

        // Partial reply index exists.
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_indexes \
             WHERE schemaname = 'engine' AND tablename = 'world_posts' \
               AND indexname = 'idx_world_posts_reply'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1, "missing idx_world_posts_reply");
    }

    /// One shape across five tables is the whole point — a fleet-wide error
    /// view must be a single UNION ALL.
    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0050_adds_both_jsonb_columns_to_every_target_table(pool: PgPool) {
        for table in [
            "engine.chat_messages",
            "engine.chat_vision_events",
            "engine.chat_images_events",
            "engine.companion_decision_events",
            "engine.companion_affinity_events",
        ] {
            let sql = "SELECT count(*) FROM information_schema.columns \
                 WHERE table_schema = split_part($1, '.', 1) \
                   AND table_name = split_part($1, '.', 2) \
                   AND column_name IN ('llm_attempts', 'gateway_errors') \
                   AND data_type = 'jsonb'";
            let n: i64 = sqlx::query_scalar(sql)
                .bind(table)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(n, 2, "{table} must have both jsonb columns");
        }
    }

    /// 0052. The column is the entire storage surface of session soft-delete:
    /// `false` is "visible", and an operator reviving a session sets it back
    /// with a plain UPDATE. Pinned here because a NULLable or defaultless
    /// column would make every pre-existing session invisible on deploy.
    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0052_chat_sessions_archived_column(pool: PgPool) {
        let (is_nullable, column_default): (String, Option<String>) = sqlx::query_as(
            "SELECT is_nullable, column_default FROM information_schema.columns \
             WHERE table_schema = 'engine' AND table_name = 'chat_sessions' \
               AND column_name = 'archived'",
        )
        .fetch_one(&pool)
        .await
        .expect("chat_sessions.archived must exist");

        assert_eq!(is_nullable, "NO", "archived must be NOT NULL");
        assert_eq!(
            column_default.as_deref(),
            Some("false"),
            "archived must default to false so existing sessions stay visible"
        );
    }

    async fn norm(pool: &PgPool, text: &str) -> String {
        sqlx::query_scalar("SELECT engine.fact_norm($1)")
            .bind(text)
            .fetch_one(pool)
            .await
            .expect("engine.fact_norm")
    }

    /// 0079. The ledger dedups on this function's output, so what it merges and
    /// what it keeps apart IS the dedup contract: surface noise (width, case,
    /// whitespace, punctuation) must collapse, while a negation or a hedge must
    /// stay its own fact.
    #[sqlx::test(migrations = "./migrations")]
    async fn fact_norm_merges_width_case_space_and_punctuation(pool: PgPool) {
        for (a, b) in [
            ("用户喜欢爬山。", "用户喜欢爬山"),
            ("ＡＢＣ", "abc"),
            ("用户 喜欢 爬山", "用户喜欢爬山"),
            ("用户喜欢爬山，周末常去", "用户喜欢爬山周末常去"),
        ] {
            assert_eq!(norm(&pool, a).await, norm(&pool, b).await, "{a} vs {b}");
        }
        assert_ne!(
            norm(&pool, "用户喜欢爬山").await,
            norm(&pool, "用户不喜欢爬山").await
        );
        assert_ne!(
            norm(&pool, "用户很喜欢爬山").await,
            norm(&pool, "用户喜欢爬山").await
        );
    }

    /// 0079. Pins the three new tables, the dedup key's shape (roleplay is part
    /// of it), the five widened `human_insights` columns and the third audit
    /// `stage`.
    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0079_shapes(pool: PgPool) {
        let user_id = uuid::Uuid::new_v4();
        let insert = |content: &'static str, evidence: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO engine.human_facts \
                       (user_id, content, category, domain, evidence_type, temporality, \
                        persistence, confidence, first_seen_at, last_seen_at) \
                     VALUES ($1, $2, 'other', 'other', $3, 'unknown', 'unknown', 'unknown', \
                             now(), now())",
                )
                .bind(user_id)
                .bind(content)
                .bind(evidence)
                .execute(&pool)
                .await
            }
        };

        insert("用户喜欢爬山。", "explicit_self_report")
            .await
            .expect("first insert");
        let dup = insert("用户喜欢爬山", "explicit_self_report").await;
        assert!(
            matches!(&dup, Err(sqlx::Error::Database(e)) if e.is_unique_violation()),
            "same normalized content, same user, same roleplay flag must collide: {dup:?}"
        );
        insert("用户喜欢爬山", "roleplay_expression")
            .await
            .expect("roleplay is its own dedup bucket");
        insert("用户不喜欢爬山", "explicit_self_report")
            .await
            .expect("a negation is its own fact");

        let (roleplay, hash_len): (bool, i32) = sqlx::query_as(
            "SELECT roleplay, length(content_hash) FROM engine.human_facts \
             WHERE content = '用户喜欢爬山' AND evidence_type = 'roleplay_expression'",
        )
        .fetch_one(&pool)
        .await
        .expect("generated columns");
        assert!(roleplay);
        assert_eq!(hash_len, 32, "content_hash is an md5 hex digest");

        // Pointers are SET NULL: deleting the session blanks them, the fact stays.
        let session_id = crate::testutil::seed_chat_session(&pool, user_id).await;
        let message_id = crate::testutil::seed_chat_message(&pool, session_id).await;
        sqlx::query(
            "INSERT INTO engine.human_facts \
               (user_id, content, category, domain, evidence_type, temporality, persistence, \
                confidence, first_seen_at, last_seen_at, first_session_id, first_message_id, \
                last_session_id, last_message_id) \
             VALUES ($1, 'x', 'other', 'other', 'explicit_self_report', 'unknown', 'unknown', \
                     'unknown', now(), now(), $2, $3, $2, $3)",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(message_id)
        .execute(&pool)
        .await
        .expect("insert with pointers");
        sqlx::query("DELETE FROM engine.chat_sessions WHERE id = $1")
            .bind(session_id)
            .execute(&pool)
            .await
            .expect("delete session");
        let (first_session, last_message): (Option<uuid::Uuid>, Option<uuid::Uuid>) =
            sqlx::query_as(
                "SELECT first_session_id, last_message_id FROM engine.human_facts \
                 WHERE content = 'x'",
            )
            .fetch_one(&pool)
            .await
            .expect("fact survives its session");
        assert_eq!((first_session, last_message), (None, None));

        // Every FK column of the ledger leads an index.
        for column in [
            "first_session_id",
            "first_message_id",
            "last_session_id",
            "last_message_id",
        ] {
            let indexed: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_index i \
                   JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = i.indkey[0] \
                  WHERE i.indrelid = 'engine.human_facts'::regclass AND a.attname = $1)",
            )
            .bind(column)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(indexed, "engine.human_facts.{column} must lead an index");
        }

        // The two state tables exist, are locked down, and take their defaults.
        sqlx::query("INSERT INTO engine.human_profile_state (user_id) VALUES ($1)")
            .bind(user_id)
            .execute(&pool)
            .await
            .expect("human_profile_state");
        let (pending, failures): (i32, i32) = sqlx::query_as(
            "SELECT pending_facts, rebuild_failures FROM engine.human_profile_state \
             WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((pending, failures), (0, 0));
        sqlx::query("INSERT INTO engine.user_erasures (user_id, erased_at) VALUES ($1, now())")
            .bind(user_id)
            .execute(&pool)
            .await
            .expect("user_erasures");
        for table in ["human_facts", "human_profile_state", "user_erasures"] {
            let rls: bool = sqlx::query_scalar(
                "SELECT relrowsecurity FROM pg_class WHERE oid = ('engine.' || $1)::regclass",
            )
            .bind(table)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(rls, "RLS must be enabled on engine.{table}");
        }

        // human_insights grew the five profile columns.
        let cols: Vec<String> = sqlx::query_scalar(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_schema = 'engine' AND table_name = 'human_insights' \
               AND column_name IN ('kinks', 'ideology', 'mental_health', \
                                   'education_level', 'education_status') \
             ORDER BY column_name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            cols,
            [
                "education_level",
                "education_status",
                "ideology",
                "kinks",
                "mental_health"
            ]
        );

        // The audit table accepts the third stage.
        let run_id = uuid::Uuid::new_v4();
        let session_id = crate::testutil::seed_chat_session(&pool, user_id).await;
        let message_id = crate::testutil::seed_chat_message(&pool, session_id).await;
        crate::testutil::seed_generation(&pool, "g").await;
        sqlx::query(
            "INSERT INTO engine.companion_insights_events \
               (run_id, user_id, session_id, message_id, stage, status, payload, generation_id) \
             VALUES ($1,$2,$3,$4,'profile','ok','{}'::jsonb,'g')",
        )
        .bind(run_id)
        .bind(user_id)
        .bind(session_id)
        .bind(message_id)
        .execute(&pool)
        .await
        .expect("stage 'profile' must be accepted");
        let bad = sqlx::query(
            "INSERT INTO engine.companion_insights_events \
               (run_id, user_id, stage, status) VALUES ($1,$2,'bogus','ok')",
        )
        .bind(run_id)
        .bind(user_id)
        .execute(&pool)
        .await;
        assert!(bad.is_err(), "an unknown stage must still be rejected");
    }

    // ─── 0080: backfill of the fact ledger ───────────────────────────────
    //
    // The migration has already run `engine.insights_2_backfill()` on the empty
    // test database, so each test seeds first and then calls it itself.

    /// One ledger row as the backfill tests read it back.
    #[derive(Debug, sqlx::FromRow)]
    struct LedgerRow {
        content: String,
        category: String,
        domain: String,
        evidence_type: String,
        temporality: String,
        persistence: String,
        confidence: String,
        occurrences: i32,
        session_count: i32,
        first_seen_at: chrono::DateTime<chrono::Utc>,
        last_seen_at: chrono::DateTime<chrono::Utc>,
        first_session_id: Option<Uuid>,
        first_message_id: Option<Uuid>,
        first_run_id: Option<Uuid>,
        last_session_id: Option<Uuid>,
        last_message_id: Option<Uuid>,
        last_run_id: Option<Uuid>,
    }

    /// A whole-second instant, so earliest / latest are unambiguous.
    fn at(secs: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_800_000_000 + secs, 0).expect("timestamp")
    }

    /// `(step, rows)` as the backfill reports them, in call order.
    async fn run_backfill(pool: &PgPool) -> Vec<(String, i64)> {
        sqlx::query_as("SELECT step, rows FROM engine.insights_2_backfill()")
            .fetch_all(pool)
            .await
            .expect("engine.insights_2_backfill")
    }

    fn counts(report: &[(String, i64)]) -> Vec<i64> {
        report.iter().map(|(_, rows)| *rows).collect()
    }

    async fn profile_row(pool: &PgPool, user_id: Uuid) {
        sqlx::query("INSERT INTO engine.human_insights (user_id) VALUES ($1)")
            .bind(user_id)
            .execute(pool)
            .await
            .expect("human_insights row");
    }

    /// A `stage = 'facts'`, `status = 'ok'` audit row created at `at(secs)`.
    async fn facts_event(
        pool: &PgPool,
        user_id: Uuid,
        run_id: Uuid,
        pointers: (Option<Uuid>, Option<Uuid>),
        secs: i64,
        payload: Option<serde_json::Value>,
    ) {
        sqlx::query(
            "INSERT INTO engine.companion_insights_events \
               (run_id, user_id, session_id, message_id, stage, status, payload, created_at) \
             VALUES ($1, $2, $3, $4, 'facts', 'ok', $5, $6)",
        )
        .bind(run_id)
        .bind(user_id)
        .bind(pointers.0)
        .bind(pointers.1)
        .bind(payload)
        .bind(at(secs))
        .execute(pool)
        .await
        .expect("companion_insights_events row");
    }

    async fn ledger(pool: &PgPool, user_id: Uuid) -> Vec<LedgerRow> {
        sqlx::query_as(
            "SELECT content, category, domain, evidence_type, temporality, persistence, \
                    confidence, occurrences, session_count, first_seen_at, last_seen_at, \
                    first_session_id, first_message_id, first_run_id, \
                    last_session_id, last_message_id, last_run_id \
             FROM engine.human_facts WHERE user_id = $1 ORDER BY content",
        )
        .bind(user_id)
        .fetch_all(pool)
        .await
        .expect("human_facts rows")
    }

    async fn ledger_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM engine.human_facts")
            .fetch_one(pool)
            .await
            .expect("count human_facts")
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn backfill_groups_events_into_ledger_rows(pool: PgPool) {
        let user = Uuid::new_v4();
        profile_row(&pool, user).await;
        let s1 = crate::testutil::seed_chat_session(&pool, user).await;
        let s2 = crate::testutil::seed_chat_session(&pool, user).await;
        let m1 = crate::testutil::seed_chat_message(&pool, s1).await;
        let m2 = crate::testutil::seed_chat_message(&pool, s2).await;
        let (run1, run2) = (Uuid::new_v4(), Uuid::new_v4());

        facts_event(
            &pool,
            user,
            run1,
            (Some(s1), Some(m1)),
            10,
            Some(serde_json::json!({
                "facts": ["a", "b", "c"],
                "details": [
                    {"content": "用户喜欢爬山。", "category": "preference", "domain": "hobby",
                     "evidence_type": "explicit_statement", "temporality": "current",
                     "persistence": "stable", "confidence": "high"},
                    {"content": "用户关心选举", "category": "value", "domain": "politics",
                     "evidence_type": "explicit_statement", "temporality": "current",
                     "persistence": "stable", "confidence": "medium"},
                    {"content": "用户住在海边", "category": "fact", "domain": "location_culture",
                     "evidence_type": "explicit_statement", "temporality": "current",
                     "persistence": "stable", "confidence": "high"}
                ]
            })),
        )
        .await;
        facts_event(
            &pool,
            user,
            run2,
            (Some(s2), Some(m2)),
            20,
            Some(serde_json::json!({
                "facts": ["a"],
                "details": [
                    {"content": "用户喜欢爬山", "category": "habit", "domain": "lifestyle",
                     "evidence_type": "observed_behavior", "temporality": "past",
                     "persistence": "transient", "confidence": "low"}
                ]
            })),
        )
        .await;

        let report = run_backfill(&pool).await;
        assert_eq!(
            report,
            [
                ("events".to_string(), 2),
                ("seeds".to_string(), 0),
                ("pending".to_string(), 1)
            ]
        );

        let rows = ledger(&pool, user).await;
        assert_eq!(rows.len(), 2, "climbing merges, politics stays: {rows:?}");
        assert!(
            rows.iter().all(|r| r.content != "用户住在海边"),
            "location_culture rows are dropped"
        );

        let ideology = rows.iter().find(|r| r.content == "用户关心选举").unwrap();
        assert_eq!(ideology.domain, "ideology");
        assert_eq!((ideology.occurrences, ideology.session_count), (1, 1));

        let climb = rows
            .iter()
            .find(|r| r.content.starts_with("用户喜欢爬山"))
            .unwrap();
        // Text and labels come from the earliest sighting.
        assert_eq!(climb.content, "用户喜欢爬山。");
        assert_eq!(
            (
                climb.category.as_str(),
                climb.domain.as_str(),
                climb.evidence_type.as_str(),
                climb.temporality.as_str(),
                climb.persistence.as_str(),
                climb.confidence.as_str()
            ),
            (
                "preference",
                "hobby",
                "explicit_statement",
                "current",
                "stable",
                "high"
            )
        );
        assert_eq!((climb.occurrences, climb.session_count), (2, 2));
        assert_eq!((climb.first_seen_at, climb.last_seen_at), (at(10), at(20)));
        assert_eq!(
            (
                climb.first_session_id,
                climb.first_message_id,
                climb.first_run_id
            ),
            (Some(s1), Some(m1), Some(run1))
        );
        assert_eq!(
            (
                climb.last_session_id,
                climb.last_message_id,
                climb.last_run_id
            ),
            (Some(s2), Some(m2), Some(run2))
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn backfill_skips_users_without_profile_row(pool: PgPool) {
        let user = Uuid::new_v4();
        facts_event(
            &pool,
            user,
            Uuid::new_v4(),
            (None, None),
            10,
            Some(serde_json::json!({"details": [{"content": "用户喜欢爬山"}]})),
        )
        .await;

        let report = run_backfill(&pool).await;
        assert_eq!(counts(&report), [0, 0, 0]);
        assert_eq!(ledger_count(&pool).await, 0);
        let states: i64 = sqlx::query_scalar("SELECT count(*) FROM engine.human_profile_state")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(states, 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn backfill_seeds_profile_values_and_filters_examples(pool: PgPool) {
        let user = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.human_insights \
               (user_id, occupation, interests, city, finance_status, mbti_guess, \
                life_rhythm, family, education, social_pattern, future_plans, updated_at) \
             VALUES ($1, '在深圳一家中厂做后端工程师，常年加班，最近想跳槽', \
                     ARRAY['周末常去爬山', '养了只橘猫', '周末打篮球', '<兴趣>'], 'x', '未知', \
                     'N/A', '   ', 'Unknown', '无', '「<待填>」', ' <待填> ', $2)",
        )
        .bind(user)
        .bind(at(5))
        .execute(&pool)
        .await
        .expect("profile row");

        let report = run_backfill(&pool).await;
        assert_eq!(counts(&report), [0, 1, 1]);

        let rows = ledger(&pool, user).await;
        assert_eq!(
            rows.len(),
            1,
            "only the non-example interest survives: {rows:?}"
        );
        let seed = &rows[0];
        assert_eq!(seed.content, "兴趣：周末打篮球");
        assert_eq!(
            (
                seed.category.as_str(),
                seed.domain.as_str(),
                seed.evidence_type.as_str(),
                seed.temporality.as_str(),
                seed.persistence.as_str(),
                seed.confidence.as_str()
            ),
            (
                "preference",
                "hobby",
                "legacy_profile",
                "unknown",
                "unknown",
                "unknown"
            )
        );
        assert_eq!((seed.occurrences, seed.session_count), (1, 1));
        assert_eq!((seed.first_seen_at, seed.last_seen_at), (at(5), at(5)));
        assert_eq!(
            [
                seed.first_session_id,
                seed.first_message_id,
                seed.first_run_id,
                seed.last_session_id,
                seed.last_message_id,
                seed.last_run_id
            ],
            [None; 6]
        );
    }

    /// Every seeded column keeps its label, domain and category; the five
    /// columns 2.0 added and the retired seven stay out.
    #[sqlx::test(migrations = "./migrations")]
    async fn backfill_seed_mapping_covers_every_column(pool: PgPool) {
        let user = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.human_insights \
               (user_id, occupation, mbti_guess, love_values, emotional_needs, life_rhythm, \
                interests, personality_traits, deal_breakers, education, family, \
                relationship_history, social_pattern, future_plans, finance_status, \
                kinks, ideology, mental_health, education_level, education_status, \
                city, location, hometown, nationality, preferred_gender, age_min, age_max) \
             VALUES ($1, 'v1', 'v2', 'v3', 'v4', 'v5', ARRAY['v6a', 'v6b'], ARRAY['v7'], \
                     ARRAY['v8'], 'v9', 'v10', 'v11', 'v12', 'v13', 'v14', \
                     ARRAY['k'], 'i', 'm', 'bachelor', 'graduated', \
                     'c', 'l', 'h', 'n', 'g', 20, 30)",
        )
        .bind(user)
        .execute(&pool)
        .await
        .expect("profile row");

        let report = run_backfill(&pool).await;
        assert_eq!(counts(&report), [0, 15, 1]);

        let mut rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT content, domain, category FROM engine.human_facts WHERE user_id = $1",
        )
        .bind(user)
        .fetch_all(&pool)
        .await
        .unwrap();
        rows.sort();
        let mut expected: Vec<(String, String, String)> = [
            ("职业：v1", "career", "fact"),
            ("MBTI：v2", "personality", "fact"),
            ("感情观：v3", "romance", "value"),
            ("情感需求：v4", "romance", "emotion"),
            ("作息：v5", "lifestyle", "habit"),
            ("兴趣：v6a", "hobby", "preference"),
            ("兴趣：v6b", "hobby", "preference"),
            ("性格特质：v7", "personality", "fact"),
            ("择偶雷区：v8", "romance", "constraint"),
            ("教育：v9", "education", "fact"),
            ("家庭：v10", "family", "fact"),
            ("感情经历：v11", "romance", "event"),
            ("社交模式：v12", "friendship", "habit"),
            ("未来计划：v13", "other", "goal"),
            ("经济状况：v14", "finance", "fact"),
        ]
        .into_iter()
        .map(|(c, d, k)| (c.to_string(), d.to_string(), k.to_string()))
        .collect();
        expected.sort();
        assert_eq!(rows, expected);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn backfill_sets_pending_for_every_profile_row(pool: PgPool) {
        let (a, b, kept) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        for user in [a, b, kept] {
            profile_row(&pool, user).await;
        }
        sqlx::query(
            "INSERT INTO engine.human_profile_state (user_id, pending_facts, last_fact_at) \
             VALUES ($1, 3, now())",
        )
        .bind(kept)
        .execute(&pool)
        .await
        .unwrap();

        let report = run_backfill(&pool).await;
        assert_eq!(counts(&report), [0, 0, 2]);

        for user in [a, b] {
            let (pending, old_enough): (i32, bool) = sqlx::query_as(
                "SELECT pending_facts, last_fact_at < now() - interval '1 hour' \
                 FROM engine.human_profile_state WHERE user_id = $1",
            )
            .bind(user)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(pending, 20);
            assert!(old_enough, "the idle sweeper must see the queue as due");
        }
        let kept_pending: i32 = sqlx::query_scalar(
            "SELECT pending_facts FROM engine.human_profile_state WHERE user_id = $1",
        )
        .bind(kept)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kept_pending, 3, "an existing counter is left alone");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn backfill_is_idempotent(pool: PgPool) {
        let user = Uuid::new_v4();
        sqlx::query("INSERT INTO engine.human_insights (user_id, family) VALUES ($1, '独居')")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        let s = crate::testutil::seed_chat_session(&pool, user).await;
        for (secs, content) in [(10, "用户喜欢爬山"), (20, "用户喜欢爬山。")] {
            facts_event(
                &pool,
                user,
                Uuid::new_v4(),
                (Some(s), None),
                secs,
                Some(serde_json::json!({"details": [{"content": content}]})),
            )
            .await;
        }

        let first = run_backfill(&pool).await;
        assert_eq!(counts(&first), [1, 1, 1]);
        let before = ledger(&pool, user).await;

        let second = run_backfill(&pool).await;
        assert_eq!(counts(&second), [0, 0, 0]);
        let after = ledger(&pool, user).await;
        assert_eq!(after.len(), 2);
        assert_eq!(
            before
                .iter()
                .map(|r| (r.occurrences, r.session_count))
                .collect::<Vec<_>>(),
            after
                .iter()
                .map(|r| (r.occurrences, r.session_count))
                .collect::<Vec<_>>(),
            "a replay must not bump counters"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn backfill_tolerates_malformed_payloads(pool: PgPool) {
        let user = Uuid::new_v4();
        profile_row(&pool, user).await;
        let payloads = [
            None,
            Some(serde_json::json!({"facts": []})),
            Some(serde_json::json!({"details": {"content": "对象不是数组"}})),
            Some(serde_json::json!({"details": "字符串"})),
            Some(serde_json::json!({"details": ["字符串元素", 7, null]})),
            Some(serde_json::json!({"details": [{}]})),
            Some(serde_json::json!({"details": [{"content": 5}]})),
            Some(serde_json::json!({"details": [{"content": "   "}]})),
            Some(serde_json::json!({"details": [{"content": "只有内容"}]})),
        ];
        for (i, payload) in payloads.into_iter().enumerate() {
            facts_event(&pool, user, Uuid::new_v4(), (None, None), i as i64, payload).await;
        }

        let report = run_backfill(&pool).await;
        assert_eq!(counts(&report), [1, 0, 1]);

        let rows = ledger(&pool, user).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        let row = &rows[0];
        assert_eq!(row.content, "只有内容");
        assert_eq!(
            (
                row.category.as_str(),
                row.domain.as_str(),
                row.evidence_type.as_str(),
                row.temporality.as_str(),
                row.persistence.as_str(),
                row.confidence.as_str()
            ),
            ("other", "other", "unknown", "unknown", "unknown", "unknown")
        );
        assert_eq!((row.occurrences, row.session_count), (1, 1));
    }

    /// The SQL fold and `FactInput::from_json` must agree label for label over
    /// the whole vocabulary: every listed value, an unlisted one, a non-string,
    /// a JSON null and a missing key, per label; plus the two retired domains.
    /// `location_culture` is left out because the backfill drops it where
    /// `from_json` folds it to `other`.
    #[sqlx::test(migrations = "./migrations")]
    async fn backfill_folds_labels_like_rust(pool: PgPool) {
        use crate::human_fact::{
            FactInput, CATEGORIES, CONFIDENCES, DOMAINS, EVIDENCE_TYPES, PERSISTENCES,
            TEMPORALITIES,
        };

        let user = Uuid::new_v4();
        profile_row(&pool, user).await;
        let base = |content: String| {
            serde_json::json!({
                "content": content, "category": "fact", "domain": "hobby",
                "evidence_type": "explicit_statement", "temporality": "current",
                "persistence": "stable", "confidence": "high"
            })
        };
        let mut details: Vec<serde_json::Value> = Vec::new();
        for (key, vocabulary) in [
            ("domain", DOMAINS),
            ("category", CATEGORIES),
            ("evidence_type", EVIDENCE_TYPES),
            ("temporality", TEMPORALITIES),
            ("persistence", PERSISTENCES),
            ("confidence", CONFIDENCES),
        ] {
            let mut candidates: Vec<serde_json::Value> =
                vocabulary.iter().map(|v| serde_json::json!(v)).collect();
            candidates.extend([
                serde_json::json!("bogus"),
                serde_json::json!(5),
                serde_json::Value::Null,
            ]);
            if key == "domain" {
                candidates.extend([serde_json::json!("politics"), serde_json::json!("religion")]);
            }
            for (i, candidate) in candidates.into_iter().enumerate() {
                let mut detail = base(format!("{key} 事实 {i}"));
                detail[key] = candidate;
                details.push(detail);
            }
            let mut detail = base(format!("{key} 事实 缺省"));
            detail.as_object_mut().unwrap().remove(key);
            details.push(detail);
        }
        facts_event(
            &pool,
            user,
            Uuid::new_v4(),
            (None, None),
            1,
            Some(serde_json::json!({ "details": details })),
        )
        .await;

        run_backfill(&pool).await;

        let rows = ledger(&pool, user).await;
        assert_eq!(rows.len(), details.len(), "every detail is its own fact");
        for detail in &details {
            let want = FactInput::from_json(detail).expect("valid fact");
            let got = rows
                .iter()
                .find(|r| r.content == want.content)
                .unwrap_or_else(|| panic!("no ledger row for {}", want.content));
            assert_eq!(
                (
                    got.category.as_str(),
                    got.domain.as_str(),
                    got.evidence_type.as_str(),
                    got.temporality.as_str(),
                    got.persistence.as_str(),
                    got.confidence.as_str()
                ),
                (
                    want.category.as_str(),
                    want.domain.as_str(),
                    want.evidence_type.as_str(),
                    want.temporality.as_str(),
                    want.persistence.as_str(),
                    want.confidence.as_str()
                ),
                "{detail}"
            );
        }
    }

    /// Trimming strips whitespace only. Edge letters survive (`tv`, `vlog`, a
    /// lone `v`), and tab, vertical tab and the wide spaces still go.
    #[sqlx::test(migrations = "./migrations")]
    async fn backfill_trims_whitespace_not_letters(pool: PgPool) {
        let user = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.human_insights (user_id, occupation, interests) VALUES ($1, $2, $3)",
        )
        .bind(user)
        .bind("\u{b}\u{3000}tv\u{a0}")
        .bind(vec!["\tvlog\n".to_string(), "\u{b}v\u{b}".to_string(), "\u{b}".to_string()])
        .execute(&pool)
        .await
        .expect("profile row");
        facts_event(
            &pool,
            user,
            Uuid::new_v4(),
            (None, None),
            1,
            Some(serde_json::json!({"details": [{"content": "\u{b}vlog\u{3000}"}]})),
        )
        .await;

        let report = run_backfill(&pool).await;
        assert_eq!(counts(&report), [1, 3, 1]);

        let mut contents: Vec<String> = ledger(&pool, user)
            .await
            .into_iter()
            .map(|r| r.content)
            .collect();
        contents.sort();
        let mut expected = ["vlog", "兴趣：vlog", "兴趣：v", "职业：tv"].map(String::from);
        expected.sort();
        assert_eq!(contents, expected);
    }

    /// 0058 left the audit pointers unvalidated, so old rows can name a
    /// session or message that no longer exists; the ledger's pointers are
    /// validated, so the backfill must null those and still count the session.
    #[sqlx::test(migrations = "./migrations")]
    async fn backfill_nulls_dangling_event_pointers(pool: PgPool) {
        for constraint in [
            "companion_insights_events_session_id_fkey",
            "companion_insights_events_message_id_fkey",
        ] {
            sqlx::query(&format!(
                "ALTER TABLE engine.companion_insights_events DROP CONSTRAINT {constraint}"
            ))
            .execute(&pool)
            .await
            .expect("drop the audit FK to fabricate a dangling row");
        }
        let user = Uuid::new_v4();
        profile_row(&pool, user).await;
        for secs in [10, 20] {
            facts_event(
                &pool,
                user,
                Uuid::new_v4(),
                (Some(Uuid::new_v4()), Some(Uuid::new_v4())),
                secs,
                Some(serde_json::json!({"details": [{"content": "用户喜欢爬山"}]})),
            )
            .await;
        }

        let report = run_backfill(&pool).await;
        assert_eq!(counts(&report), [1, 0, 1]);

        let rows = ledger(&pool, user).await;
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!((row.occurrences, row.session_count), (2, 2));
        assert_eq!(
            [
                row.first_session_id,
                row.first_message_id,
                row.last_session_id,
                row.last_message_id
            ],
            [None; 4]
        );
        assert!(row.first_run_id.is_some() && row.last_run_id.is_some());
        assert_ne!(row.first_run_id, row.last_run_id);
    }
}
