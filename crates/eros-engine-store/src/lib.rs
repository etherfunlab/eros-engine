// SPDX-License-Identifier: AGPL-3.0-only
//! Postgres + pgvector persistence layer.

pub mod affinity;
pub mod character_insight;
pub mod chat;
pub mod chat_queue;
pub mod decision;
pub mod error_handling;
pub mod generation;
pub mod human_insight;
pub mod image_events;
pub mod insight;
pub mod memory;
pub mod persona;
pub mod pool;
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

    /// Persist one `engine.llm_generations` parent so a test may put `id` in a
    /// child row's `generation_id`. Since 0060 all eight of those columns carry
    /// a VALIDATED foreign key, so a fabricated id no longer inserts — same
    /// shape as `seed_chat_session` after 0058.
    ///
    /// The `task` is a literal `'test'` on purpose: the column is NOT NULL and
    /// carries no CHECK (0059 — the vocabulary is deployer-extensible config),
    /// and a test that needed a REAL task value would be testing the backfill,
    /// which `migration_0060_backfill_assigns_the_right_task_and_session` owns.
    ///
    /// `ON CONFLICT DO NOTHING` so a test may seed the same id twice — several
    /// exercise a replay or a double-write race on one generation.
    pub(crate) async fn seed_generation(pool: &PgPool, id: &str) {
        sqlx::query(
            "INSERT INTO engine.llm_generations (generation_id, task) \
             VALUES ($1, 'test') ON CONFLICT (generation_id) DO NOTHING",
        )
        .bind(id)
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
    /// something.
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

    /// 0060's backfill is fifteen task literals across four different
    /// discriminator columns, and **every existing test is blind to it**:
    /// `sqlx::test` runs migrations against an empty database, so the backfill
    /// selects nothing and a wrong `CASE` branch passes CI untouched. It would
    /// then surface as a failed `release_command` in production — or worse, as
    /// silently mislabelled rows if the orphan preflight happens to pass.
    ///
    /// So this test replays it: drop the constraints, plant one orphan child
    /// row per arm, run the migration's own backfill text, and assert what came
    /// out. The SQL is extracted from the migration file rather than copied,
    /// because a copy that drifts passes while the migration is wrong.
    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0060_backfill_assigns_the_right_task_and_session(pool: PgPool) {
        use crate::testutil;
        use uuid::Uuid;

        let user = Uuid::new_v4();
        let instance = testutil::seed_persona_instance(&pool, user).await;

        let session: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) \
             RETURNING id",
        )
        .bind(user)
        .bind(instance)
        .fetch_one(&pool)
        .await
        .unwrap();

        // The eight FKs 0060 just added, plus the session pointer on
        // companion_insights_events — dropped so this test can plant the
        // orphans and the dangling session id that only exist in the
        // pre-0060 world the backfill was written for.
        //
        // The seeded child rows below deliberately still write `model` and
        // `usage`, which B1's code no longer does. That is not a leftover:
        // those columns are exactly what the backfill READS, and they hold
        // pre-B1 data until 0061 drops them. When B2 lands, this test's
        // seeding goes with them and the backfill's model/usage arms become
        // unreachable — which is fine, because by then it has already run.
        for stmt in [
            "ALTER TABLE engine.chat_messages DROP CONSTRAINT chat_messages_generation_id_fkey",
            "ALTER TABLE engine.companion_affinity_events DROP CONSTRAINT companion_affinity_events_generation_id_fkey",
            "ALTER TABLE engine.companion_insights_events DROP CONSTRAINT companion_insights_events_generation_id_fkey",
            "ALTER TABLE engine.companion_decision_events DROP CONSTRAINT companion_decision_events_generation_id_fkey",
            "ALTER TABLE engine.chat_images_events DROP CONSTRAINT chat_images_events_generation_id_fkey",
            "ALTER TABLE engine.chat_vision_events DROP CONSTRAINT chat_vision_events_generation_id_fkey",
            "ALTER TABLE engine.character_insights_events DROP CONSTRAINT character_insights_events_generation_id_fkey",
            "ALTER TABLE engine.user_insights_events DROP CONSTRAINT user_insights_events_generation_id_fkey",
            "ALTER TABLE engine.companion_insights_events DROP CONSTRAINT companion_insights_events_session_id_fkey",
        ] {
            sqlx::query(stmt).execute(&pool).await.expect(stmt);
        }

        // ── chat_messages: three channels plus the output filter's own id.
        for (gen, channel) in [
            ("bf-companion", None),
            ("bf-voice", Some("voice")),
            ("bf-productqa", Some("product_qa")),
        ] {
            sqlx::query(
                "INSERT INTO engine.chat_messages \
                   (session_id, role, content, channel, model, usage, generation_id) \
                 VALUES ($1, 'assistant', 'x', $2, 'm/1', '{\"total_tokens\":1}'::jsonb, $3)",
            )
            .bind(session)
            .bind(channel)
            .bind(gen)
            .execute(&pool)
            .await
            .unwrap();
        }
        // The filter rides on an existing message. filter_model is its model.
        let user_msg: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages \
               (session_id, role, content, filter_model, f_generation_id) \
             VALUES ($1, 'user', 'hi', 'filter/m', 'bf-filter') RETURNING id",
        )
        .bind(session)
        .fetch_one(&pool)
        .await
        .unwrap();

        // ── companion_affinity_events: no session_id column at all. Its only
        // route to one is user_message_id -> chat_messages.session_id, which is
        // exactly what this row pins.
        let affinity_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.companion_affinity (session_id, user_id, instance_id) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(session)
        .bind(user)
        .bind(instance)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.companion_affinity_events \
               (affinity_id, event_type, model, usage, generation_id, user_message_id) \
             VALUES ($1, 'message', 'aff/m', '{}'::jsonb, 'bf-affinity', $2)",
        )
        .bind(affinity_id)
        .bind(user_msg)
        .execute(&pool)
        .await
        .unwrap();

        // ── companion_insights_events: both stages. The 'structured' row
        // deliberately carries a DANGLING session id -- 0058 left 26 of those
        // in production and refused to backfill them. If the LEFT JOIN in the
        // migration is ever dropped, this row copies a dead uuid into
        // llm_generations.session_id, whose own FK is validated, and the
        // migration aborts inside release_command.
        for (gen, stage, sess) in [
            ("bf-facts", "facts", Some(session)),
            ("bf-structured", "structured", Some(Uuid::new_v4())),
        ] {
            sqlx::query(
                "INSERT INTO engine.companion_insights_events \
                   (run_id, user_id, session_id, stage, status, model, usage, generation_id) \
                 VALUES ($1, $2, $3, $4, 'ok', 'ins/m', '{}'::jsonb, $5)",
            )
            .bind(Uuid::new_v4())
            .bind(user)
            .bind(sess)
            .bind(stage)
            .bind(gen)
            .execute(&pool)
            .await
            .unwrap();
        }

        sqlx::query(
            "INSERT INTO engine.companion_decision_events \
               (run_id, user_id, session_id, status, model, usage, generation_id) \
             VALUES ($1, $2, $3, 'ok', 'pde/m', '{}'::jsonb, 'bf-decision')",
        )
        .bind(Uuid::new_v4())
        .bind(user)
        .bind(session)
        .execute(&pool)
        .await
        .unwrap();

        // ── chat_images_events: both sources. 'image_edit' has zero rows in
        // production today, which is exactly why its CASE branch needs a test.
        for (gen, source) in [
            ("bf-image", "chat_reply_text_image"),
            ("bf-imageedit", "image_edit"),
        ] {
            sqlx::query(
                "INSERT INTO engine.chat_images_events \
                   (source, user_id, session_id, status, inputs, attempts, \
                    model, usage, generation_id) \
                 VALUES ($1, $2, $3, 'ok', '{}'::jsonb, 1, 'img/m', '{}'::jsonb, $4)",
            )
            .bind(source)
            .bind(user)
            .bind(session)
            .bind(gen)
            .execute(&pool)
            .await
            .unwrap();
        }

        sqlx::query(
            "INSERT INTO engine.chat_vision_events \
               (user_id, session_id, status, image_url, attempts, model, usage, generation_id) \
             VALUES ($1, $2, 'ok', 'https://x/i.png', 1, 'vis/m', '{}'::jsonb, 'bf-vision')",
        )
        .bind(user)
        .bind(session)
        .execute(&pool)
        .await
        .unwrap();

        for (table, gen, stage) in [
            ("character_insights_events", "bf-ch-extract", "extraction"),
            ("character_insights_events", "bf-ch-struct", "structuring"),
            ("user_insights_events", "bf-us-extract", "extraction"),
            ("user_insights_events", "bf-us-struct", "structuring"),
        ] {
            sqlx::query(&format!(
                "INSERT INTO engine.{table} \
                   (run_id, instance_id, session_id, stage, status, model, usage, generation_id) \
                 VALUES ($1, $2, $3, $4, 'ok', 'ins/m', '{{}}'::jsonb, $5)"
            ))
            .bind(Uuid::new_v4())
            .bind(instance)
            .bind(session)
            .bind(stage)
            .bind(gen)
            .execute(&pool)
            .await
            .unwrap();
        }

        // ── Run the migration's own backfill, not a copy of it.
        let migration = include_str!("../migrations/0060_llm_generations_backfill_fks.sql");
        let backfill = migration
            .split("-- ─── 2. Orphan preflight")
            .next()
            .expect("migration 0060 must keep its numbered section markers");
        assert_eq!(
            backfill
                .matches("INSERT INTO engine.llm_generations")
                .count(),
            9,
            "expected 9 backfill arms before the preflight marker; the section \
             markers or the arm count changed and this test is no longer \
             covering what it claims to"
        );
        sqlx::raw_sql(backfill).execute(&pool).await.unwrap();

        for (gen, want_task) in [
            ("bf-companion", "chat_companion"),
            ("bf-voice", "chat_voice"),
            ("bf-productqa", "chat_product_qa"),
            ("bf-filter", "chat_output_filter"),
            ("bf-affinity", "affinity_evaluation"),
            ("bf-facts", "insight_extraction"),
            ("bf-structured", "insight_structuring"),
            ("bf-decision", "pde_decision"),
            ("bf-image", "chat_image_prompt_compose"),
            ("bf-imageedit", "chat_image_edit_compose"),
            ("bf-vision", "chat_vision"),
            ("bf-ch-extract", "character_insight_extraction"),
            ("bf-ch-struct", "character_insight_structuring"),
            ("bf-us-extract", "user_insight_extraction"),
            ("bf-us-struct", "user_insight_structuring"),
        ] {
            let task: Option<String> = sqlx::query_scalar(
                "SELECT task FROM engine.llm_generations WHERE generation_id = $1",
            )
            .bind(gen)
            .fetch_optional(&pool)
            .await
            .unwrap();
            assert_eq!(
                task.as_deref(),
                Some(want_task),
                "backfill produced the wrong task for {gen}"
            );
        }

        // The affinity row reached its session the only way it can.
        let aff_session: Option<Uuid> = sqlx::query_scalar(
            "SELECT session_id FROM engine.llm_generations WHERE generation_id = 'bf-affinity'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            aff_session,
            Some(session),
            "companion_affinity_events must reach its session through \
             user_message_id -> chat_messages.session_id"
        );

        // The dangling one landed NULL instead of aborting the migration.
        let dangling: Option<Uuid> = sqlx::query_scalar(
            "SELECT session_id FROM engine.llm_generations WHERE generation_id = 'bf-structured'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            dangling, None,
            "a dangling session id must land NULL; copying it across would \
             violate llm_generations' own validated FK and abort the deploy"
        );

        // Model and usage carried over -- this is what B2 will read through the
        // join once the child columns are gone.
        let (model, usage): (Option<String>, Option<serde_json::Value>) = sqlx::query_as(
            "SELECT model, usage FROM engine.llm_generations WHERE generation_id = 'bf-companion'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(model.as_deref(), Some("m/1"));
        assert_eq!(usage, Some(serde_json::json!({"total_tokens": 1})));

        // filter_model became the filter generation's model.
        let filter_model: Option<String> = sqlx::query_scalar(
            "SELECT model FROM engine.llm_generations WHERE generation_id = 'bf-filter'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(filter_model.as_deref(), Some("filter/m"));
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
}
