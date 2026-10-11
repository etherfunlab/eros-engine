// SPDX-License-Identifier: AGPL-3.0-only
//! Persona genome + instance persistence.

use chrono::{DateTime, Utc};
use eros_engine_core::persona::{
    merge_items, CompanionPersona, PersonaGenome, PersonaInstance, PersonaItem,
};
use eros_engine_core::types::ItemKind;
use sqlx::PgPool;
use uuid::Uuid;

/// Row mirror of `persona_genomes` for sqlx.
#[derive(Debug, Clone, sqlx::FromRow)]
struct GenomeRow {
    id: Uuid,
    name: String,
    system_prompt: String,
    tip_personality: Option<String>,
    art_metadata: serde_json::Value,
}

/// Narrow gate-field view of a genome for the chat-start path: just
/// `name` (used in the response). The engine no longer gates on
/// availability, so existence of the row is the only check.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct GenomeGate {
    pub name: String,
}

/// Gate-fields view for the explicit-`instance_id` chat-start path: owner
/// (403 check) and genome name (response) in one JOIN.
/// Filters `status='active'` like `load_companion`, so an archived instance
/// yields `None`. Folds the former `load_companion` read.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct InstanceGate {
    pub instance_id: Uuid,
    pub genome_id: Uuid,
    pub owner_uid: Uuid,
    pub genome_name: String,
}

impl From<GenomeRow> for PersonaGenome {
    fn from(r: GenomeRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            system_prompt: r.system_prompt,
            tip_personality: r.tip_personality,
            art_metadata: r.art_metadata,
        }
    }
}

/// Row mirror of `persona_instances` for sqlx.
#[derive(Debug, Clone, sqlx::FromRow)]
struct InstanceRow {
    id: Uuid,
    genome_id: Uuid,
    owner_uid: Uuid,
    status: String,
}

impl From<InstanceRow> for PersonaInstance {
    fn from(r: InstanceRow) -> Self {
        Self {
            id: r.id,
            genome_id: r.genome_id,
            owner_uid: r.owner_uid,
            status: r.status,
        }
    }
}

/// At most this many of an instance's own names per list reach the prompt,
/// newest first (spec 2026-10-12-persona-items-and-gift-design.md §5.1).
/// Genome presets come on top. The items route is uncapped.
pub const PROMPT_ITEMS_PER_KIND: i64 = 20;

/// One row of `engine.persona_instance_items`.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct InstanceItemRow {
    pub id: Uuid,
    pub kind: String,
    pub name: String,
    pub source: String,
    pub user_message_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

pub struct PersonaRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> PersonaRepo<'a> {
    pub async fn get_genome(&self, genome_id: Uuid) -> Result<Option<PersonaGenome>, sqlx::Error> {
        let row = sqlx::query_as::<_, GenomeRow>(
            "SELECT id, name, system_prompt, tip_personality, \
                    art_metadata \
             FROM engine.persona_genomes \
             WHERE id = $1",
        )
        .bind(genome_id)
        .fetch_optional(self.pool)
        .await?;
        Ok(row.map(PersonaGenome::from))
    }

    /// One-row gate read for the genome chat-start path. See [`GenomeGate`].
    pub async fn get_genome_gate(
        &self,
        genome_id: Uuid,
    ) -> Result<Option<GenomeGate>, sqlx::Error> {
        sqlx::query_as::<_, GenomeGate>(
            "SELECT name \
             FROM engine.persona_genomes WHERE id = $1",
        )
        .bind(genome_id)
        .fetch_optional(self.pool)
        .await
    }

    /// Joined genome+instance view used by the chat pipeline.
    pub async fn load_companion(
        &self,
        instance_id: Uuid,
    ) -> Result<Option<CompanionPersona>, sqlx::Error> {
        #[derive(sqlx::FromRow)]
        struct Joined {
            // instance side
            instance_id: Uuid,
            genome_id: Uuid,
            owner_uid: Uuid,
            status: String,
            // genome side
            g_id: Uuid,
            name: String,
            system_prompt: String,
            tip_personality: Option<String>,
            art_metadata: serde_json::Value,
            items: serde_json::Value,
        }

        let row = sqlx::query_as::<_, Joined>(
            "SELECT \
                pi.id          AS instance_id, \
                pi.genome_id   AS genome_id, \
                pi.owner_uid   AS owner_uid, \
                pi.status      AS status, \
                pg.id          AS g_id, \
                pg.name        AS name, \
                pg.system_prompt    AS system_prompt, \
                pg.tip_personality  AS tip_personality, \
                pg.art_metadata     AS art_metadata, \
                COALESCE(( \
                    SELECT json_agg(json_build_object( \
                               'kind', t.kind, 'name', t.name, 'gifted', t.gifted) \
                           ORDER BY t.kind, t.last_at DESC, t.name) \
                    FROM ( \
                        SELECT kind, name, \
                               bool_or(source = 'gift') AS gifted, \
                               max(created_at) AS last_at, \
                               row_number() OVER ( \
                                   PARTITION BY kind ORDER BY max(created_at) DESC, name) AS rn \
                        FROM engine.persona_instance_items \
                        WHERE instance_id = pi.id \
                        GROUP BY kind, name \
                    ) t \
                    WHERE t.rn <= $2 \
                ), '[]'::json) AS items \
             FROM engine.persona_instances pi \
             JOIN engine.persona_genomes pg ON pg.id = pi.genome_id \
             WHERE pi.id = $1 AND pi.status = 'active'",
        )
        .bind(instance_id)
        .bind(PROMPT_ITEMS_PER_KIND)
        .fetch_optional(self.pool)
        .await?;

        Ok(row.map(|r| {
            let rows: Vec<PersonaItem> = serde_json::from_value(r.items).unwrap_or_default();
            let items = merge_items(&r.art_metadata, rows);
            CompanionPersona {
                instance_id: r.instance_id,
                genome: PersonaGenome {
                    id: r.g_id,
                    name: r.name,
                    system_prompt: r.system_prompt,
                    tip_personality: r.tip_personality,
                    art_metadata: r.art_metadata,
                },
                instance: PersonaInstance {
                    id: r.instance_id,
                    genome_id: r.genome_id,
                    owner_uid: r.owner_uid,
                    status: r.status,
                },
                items,
            }
        }))
    }

    /// One-JOIN gate read for the explicit-`instance_id` path. See [`InstanceGate`].
    pub async fn load_instance_gate(
        &self,
        instance_id: Uuid,
    ) -> Result<Option<InstanceGate>, sqlx::Error> {
        sqlx::query_as::<_, InstanceGate>(
            "SELECT \
                pi.id        AS instance_id, \
                pi.genome_id AS genome_id, \
                pi.owner_uid AS owner_uid, \
                pg.name      AS genome_name \
             FROM engine.persona_instances pi \
             JOIN engine.persona_genomes pg ON pg.id = pi.genome_id \
             WHERE pi.id = $1 AND pi.status = 'active'",
        )
        .bind(instance_id)
        .fetch_optional(self.pool)
        .await
    }

    /// The instance's owner, whatever its `status`. Deliberately unlike
    /// `load_instance_gate`, which filters `status = 'active'`: a client may
    /// archive the conversations of a relationship it has already marked
    /// dormant, and that call must not 404 on a technicality.
    pub async fn instance_owner(&self, instance_id: Uuid) -> Result<Option<Uuid>, sqlx::Error> {
        sqlx::query_scalar("SELECT owner_uid FROM engine.persona_instances WHERE id = $1")
            .bind(instance_id)
            .fetch_optional(self.pool)
            .await
    }

    /// Upsert a persona genome by `name`. New row → INSERT, returns
    /// `(uuid, true)`. Existing row → UPDATE in place (id stable, content
    /// refreshed), returns `(uuid, false)`. Used by `seed-personas` so
    /// editing a TOML file and re-running picks up the changes without
    /// dropping foreign-key references from `persona_instances`.
    pub async fn upsert_genome(
        &self,
        name: &str,
        system_prompt: &str,
        tip_personality: Option<&str>,
        art_metadata: serde_json::Value,
    ) -> Result<(Uuid, bool), sqlx::Error> {
        if let Some(id) =
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM engine.persona_genomes WHERE name = $1")
                .bind(name)
                .fetch_optional(self.pool)
                .await?
        {
            sqlx::query(
                "UPDATE engine.persona_genomes SET \
                    system_prompt = $2, \
                    tip_personality = $3, \
                    art_metadata = $4 \
                 WHERE id = $1",
            )
            .bind(id)
            .bind(system_prompt)
            .bind(tip_personality)
            .bind(art_metadata)
            .execute(self.pool)
            .await?;
            return Ok((id, false));
        }
        let id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.persona_genomes \
                (name, system_prompt, tip_personality, art_metadata) \
             VALUES ($1, $2, $3, $4) RETURNING id",
        )
        .bind(name)
        .bind(system_prompt)
        .bind(tip_personality)
        .bind(art_metadata)
        .fetch_one(self.pool)
        .await?;
        Ok((id, true))
    }

    /// Create a new persona instance for `(genome_id, owner_uid)`.
    pub async fn create_instance(
        &self,
        genome_id: Uuid,
        owner_uid: Uuid,
    ) -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id)
        .bind(owner_uid)
        .fetch_one(self.pool)
        .await
    }

    /// The id of the user's *active* instance for `genome_id`, if any.
    /// Lifted from the inline lookup in `resolve_or_create_session` so it
    /// can run inside `tokio::try_join!`.
    pub async fn find_active_instance(
        &self,
        genome_id: Uuid,
        owner_uid: Uuid,
    ) -> Result<Option<Uuid>, sqlx::Error> {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM engine.persona_instances \
             WHERE genome_id = $1 AND owner_uid = $2 AND status = 'active'",
        )
        .bind(genome_id)
        .bind(owner_uid)
        .fetch_optional(self.pool)
        .await
    }

    /// Ensure an *active* instance exists for `(genome_id, owner_uid)` and
    /// return its id. Creates a new row, or reactivates the existing
    /// (possibly archived) one. `persona_instances` is
    /// `UNIQUE(genome_id, owner_uid)`, so a plain INSERT 500s on an archived
    /// row (issue #37); the `ON CONFLICT DO UPDATE` flips it back to active.
    pub async fn ensure_active_instance(
        &self,
        genome_id: Uuid,
        owner_uid: Uuid,
    ) -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) \
             VALUES ($1, $2) \
             ON CONFLICT (genome_id, owner_uid) DO UPDATE SET status = 'active' \
             RETURNING id",
        )
        .bind(genome_id)
        .bind(owner_uid)
        .fetch_one(self.pool)
        .await
    }

    /// Every item row of the instance, newest first. Presets are the
    /// genome's and are not rows.
    pub async fn list_items(&self, instance_id: Uuid) -> Result<Vec<InstanceItemRow>, sqlx::Error> {
        sqlx::query_as::<_, InstanceItemRow>(
            "SELECT id, kind, name, source, user_message_id, created_at \
             FROM engine.persona_instance_items \
             WHERE instance_id = $1 \
             ORDER BY created_at DESC, id",
        )
        .bind(instance_id)
        .fetch_all(self.pool)
        .await
    }

    /// A caller-written item (spec §7). The name is validated by the route.
    pub async fn insert_item(
        &self,
        instance_id: Uuid,
        kind: ItemKind,
        name: &str,
    ) -> Result<InstanceItemRow, sqlx::Error> {
        sqlx::query_as::<_, InstanceItemRow>(
            "INSERT INTO engine.persona_instance_items (instance_id, kind, name, source) \
             VALUES ($1, $2, $3, 'caller') \
             RETURNING id, kind, name, source, user_message_id, created_at",
        )
        .bind(instance_id)
        .bind(kind.as_str())
        .bind(name)
        .fetch_one(self.pool)
        .await
    }

    /// Delete one of the instance's rows, gift or caller. `false` when the id
    /// is not a row of this instance.
    pub async fn delete_item(&self, instance_id: Uuid, item_id: Uuid) -> Result<bool, sqlx::Error> {
        let r = sqlx::query(
            "DELETE FROM engine.persona_instance_items WHERE id = $1 AND instance_id = $2",
        )
        .bind(item_id)
        .bind(instance_id)
        .execute(self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn insert_genome(pool: &PgPool, name: &str, art: serde_json::Value) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(name)
        .bind("you are a companion")
        .bind(art)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn get_genome_returns_full_record(pool: PgPool) {
        let repo = PersonaRepo { pool: &pool };
        let id = insert_genome(&pool, "Nova", serde_json::json!({ "mbti": "INFP" })).await;

        let g = repo.get_genome(id).await.unwrap().unwrap();
        assert_eq!(g.name, "Nova");
        assert_eq!(g.art_metadata["mbti"], "INFP");

        let missing = repo.get_genome(Uuid::new_v4()).await.unwrap();
        assert!(missing.is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn create_instance_and_load_companion(pool: PgPool) {
        let repo = PersonaRepo { pool: &pool };
        let owner = Uuid::new_v4();
        let genome_id = insert_genome(&pool, "Echo", serde_json::json!({ "age": 27 })).await;

        let instance_id = repo.create_instance(genome_id, owner).await.unwrap();

        let companion = repo
            .load_companion(instance_id)
            .await
            .unwrap()
            .expect("companion must load");
        assert_eq!(companion.instance_id, instance_id);
        assert_eq!(companion.genome.id, genome_id);
        assert_eq!(companion.genome.name, "Echo");
        assert_eq!(companion.instance.owner_uid, owner);
        assert_eq!(companion.instance.status, "active");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn get_genome_gate_returns_name(pool: PgPool) {
        let repo = PersonaRepo { pool: &pool };

        let id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('Legacy', 'sp', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let g = repo.get_genome_gate(id).await.unwrap().unwrap();
        assert_eq!(g.name, "Legacy");

        // missing → None
        assert!(repo
            .get_genome_gate(Uuid::new_v4())
            .await
            .unwrap()
            .is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn find_active_instance_skips_archived_and_missing(pool: PgPool) {
        let repo = PersonaRepo { pool: &pool };
        let owner = Uuid::new_v4();
        let genome_id = insert_genome(&pool, "Echo", serde_json::json!({})).await;

        // none yet
        assert!(repo
            .find_active_instance(genome_id, owner)
            .await
            .unwrap()
            .is_none());

        // active → found
        let iid = repo.create_instance(genome_id, owner).await.unwrap();
        assert_eq!(
            repo.find_active_instance(genome_id, owner).await.unwrap(),
            Some(iid)
        );

        // archived → skipped
        sqlx::query("UPDATE engine.persona_instances SET status = 'archived' WHERE id = $1")
            .bind(iid)
            .execute(&pool)
            .await
            .unwrap();
        assert!(repo
            .find_active_instance(genome_id, owner)
            .await
            .unwrap()
            .is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn load_companion_skips_non_active_instances(pool: PgPool) {
        let repo = PersonaRepo { pool: &pool };
        let owner = Uuid::new_v4();
        let genome_id = insert_genome(&pool, "Echo", serde_json::json!({})).await;
        let instance_id = repo.create_instance(genome_id, owner).await.unwrap();

        sqlx::query("UPDATE engine.persona_instances SET status = 'archived' WHERE id = $1")
            .bind(instance_id)
            .execute(&pool)
            .await
            .unwrap();

        let companion = repo.load_companion(instance_id).await.unwrap();
        assert!(companion.is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn load_instance_gate_returns_fields_and_filters_active(pool: PgPool) {
        let repo = PersonaRepo { pool: &pool };
        let owner = Uuid::new_v4();
        let genome_id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.persona_genomes \
                (name, system_prompt, art_metadata) \
             VALUES ('Nova', 'p', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let iid = repo.create_instance(genome_id, owner).await.unwrap();

        let gate = repo
            .load_instance_gate(iid)
            .await
            .unwrap()
            .expect("active instance gate");
        assert_eq!(gate.instance_id, iid);
        assert_eq!(gate.genome_id, genome_id);
        assert_eq!(gate.owner_uid, owner);
        assert_eq!(gate.genome_name, "Nova");

        // archived → None (mirrors load_companion's active filter)
        sqlx::query("UPDATE engine.persona_instances SET status = 'archived' WHERE id = $1")
            .bind(iid)
            .execute(&pool)
            .await
            .unwrap();
        assert!(repo.load_instance_gate(iid).await.unwrap().is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn ensure_active_instance_creates_then_reactivates(pool: PgPool) {
        // #37: a plain INSERT would 500 on the unconditional
        // UNIQUE(genome_id, owner_uid) when an archived row exists. The
        // upsert must reactivate the SAME row instead.
        let repo = PersonaRepo { pool: &pool };
        let owner = Uuid::new_v4();
        let genome_id = insert_genome(&pool, "Echo", serde_json::json!({})).await;

        // first call creates
        let iid = repo.ensure_active_instance(genome_id, owner).await.unwrap();

        // archive it
        sqlx::query("UPDATE engine.persona_instances SET status = 'archived' WHERE id = $1")
            .bind(iid)
            .execute(&pool)
            .await
            .unwrap();

        // second call reactivates the same row (no unique violation)
        let again = repo.ensure_active_instance(genome_id, owner).await.unwrap();
        assert_eq!(
            again, iid,
            "must reactivate existing row, not create a new one"
        );

        let status: String =
            sqlx::query_scalar("SELECT status FROM engine.persona_instances WHERE id = $1")
                .bind(iid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "active");

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM engine.persona_instances \
             WHERE genome_id = $1 AND owner_uid = $2",
        )
        .bind(genome_id)
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "exactly one instance per (genome, owner)");
    }

    async fn add_item(
        pool: &PgPool,
        instance_id: Uuid,
        kind: &str,
        name: &str,
        source: &str,
        minutes_ago: i32,
    ) {
        sqlx::query(
            "INSERT INTO engine.persona_instance_items (instance_id, kind, name, source, created_at) \
             VALUES ($1, $2, $3, $4, now() - make_interval(mins => $5))",
        )
        .bind(instance_id)
        .bind(kind)
        .bind(name)
        .bind(source)
        .bind(minutes_ago)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn load_companion_merges_presets_and_instance_items(pool: PgPool) {
        use eros_engine_core::persona::PersonaItem;
        use eros_engine_core::types::ItemKind;
        let repo = PersonaRepo { pool: &pool };
        let genome_id = insert_genome(
            &pool,
            "Wardrobe",
            serde_json::json!({"possessions": ["吉他"], "wardrobe": ["白色开衫"]}),
        )
        .await;
        let instance_id = repo
            .create_instance(genome_id, Uuid::new_v4())
            .await
            .unwrap();
        add_item(&pool, instance_id, "clothing", "红色旗袍", "gift", 3).await;
        add_item(&pool, instance_id, "clothing", "白色开衫", "gift", 2).await;
        add_item(&pool, instance_id, "item", "摩托车", "caller", 1).await;
        add_item(&pool, instance_id, "item", "摩托车", "gift", 5).await;

        let p = repo.load_companion(instance_id).await.unwrap().unwrap();
        let item = |kind, name: &str, gifted| PersonaItem {
            kind,
            name: name.into(),
            gifted,
        };
        assert_eq!(
            p.items,
            vec![
                item(ItemKind::Item, "吉他", false),
                item(ItemKind::Clothing, "白色开衫", true),
                item(ItemKind::Clothing, "红色旗袍", true),
                item(ItemKind::Item, "摩托车", true),
            ]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn load_companion_caps_each_list_at_the_newest_twenty_names(pool: PgPool) {
        use eros_engine_core::types::ItemKind;
        let repo = PersonaRepo { pool: &pool };
        let genome_id = insert_genome(&pool, "Hoarder", serde_json::json!({})).await;
        let instance_id = repo
            .create_instance(genome_id, Uuid::new_v4())
            .await
            .unwrap();
        for i in 0..21 {
            add_item(&pool, instance_id, "item", &format!("物品{i}"), "caller", i).await;
        }
        add_item(&pool, instance_id, "clothing", "外套", "caller", 100).await;

        let p = repo.load_companion(instance_id).await.unwrap().unwrap();
        let items: Vec<&str> = p
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::Item)
            .map(|i| i.name.as_str())
            .collect();
        assert_eq!(items.len(), 20);
        assert_eq!(items[0], "物品0", "newest first");
        assert!(!items.contains(&"物品20"), "the oldest falls off");
        assert!(
            p.items.iter().any(|i| i.name == "外套"),
            "the cap is per list"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn item_rows_list_insert_and_delete(pool: PgPool) {
        use eros_engine_core::types::ItemKind;
        let repo = PersonaRepo { pool: &pool };
        let genome_id = insert_genome(&pool, "Rows", serde_json::json!({})).await;
        let instance_id = repo
            .create_instance(genome_id, Uuid::new_v4())
            .await
            .unwrap();
        let other = repo
            .create_instance(genome_id, Uuid::new_v4())
            .await
            .unwrap();

        let older = repo
            .insert_item(instance_id, ItemKind::Clothing, "白色开衫")
            .await
            .unwrap();
        assert_eq!(older.kind, "clothing");
        assert_eq!(older.source, "caller");
        assert_eq!(older.user_message_id, None);
        sqlx::query(
            "UPDATE engine.persona_instance_items SET created_at = now() - interval '1 hour' \
             WHERE id = $1",
        )
        .bind(older.id)
        .execute(&pool)
        .await
        .unwrap();
        let newer = repo
            .insert_item(instance_id, ItemKind::Item, "摩托车")
            .await
            .unwrap();

        let rows = repo.list_items(instance_id).await.unwrap();
        assert_eq!(
            rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![newer.id, older.id],
            "newest first"
        );
        assert!(
            !repo.delete_item(other, newer.id).await.unwrap(),
            "another instance's id deletes nothing"
        );
        assert!(repo.delete_item(instance_id, newer.id).await.unwrap());
        assert_eq!(repo.list_items(instance_id).await.unwrap().len(), 1);
    }
}
