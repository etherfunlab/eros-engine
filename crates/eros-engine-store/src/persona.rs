// SPDX-License-Identifier: AGPL-3.0-only
//! Persona genome + instance persistence.

use eros_engine_core::persona::{CompanionPersona, PersonaGenome, PersonaInstance};
use sqlx::PgPool;
use uuid::Uuid;

/// Row mirror of `persona_genomes` for sqlx.
#[derive(Debug, Clone, sqlx::FromRow)]
struct GenomeRow {
    id: Uuid,
    name: String,
    system_prompt: String,
    tip_personality: Option<String>,
    avatar_url: Option<String>,
    art_metadata: serde_json::Value,
    is_active: bool,
}

/// Narrow gate-fields view of a genome for the chat-start path: the three
/// fields `resolve_or_create_session` needs — `name` (response),
/// `is_active` (400 check), `asset_id` (NFT gate) — in one row. Folds the
/// former `get_genome` + `get_asset_id_for_genome` reads.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct GenomeGate {
    pub name: String,
    pub is_active: bool,
    pub asset_id: Option<String>,
}

/// Gate-fields view for the explicit-`instance_id` chat-start path: owner
/// (403 check), genome name (response), asset_id (NFT gate), in one JOIN.
/// Filters `status='active'` like `load_companion`, so an archived instance
/// yields `None`. Folds the former `load_companion` + `get_asset_id_for_genome`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct InstanceGate {
    pub instance_id: Uuid,
    pub genome_id: Uuid,
    pub owner_uid: Uuid,
    pub genome_name: String,
    pub asset_id: Option<String>,
}

impl From<GenomeRow> for PersonaGenome {
    fn from(r: GenomeRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            system_prompt: r.system_prompt,
            tip_personality: r.tip_personality,
            avatar_url: r.avatar_url,
            art_metadata: r.art_metadata,
            is_active: r.is_active,
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

pub struct PersonaRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> PersonaRepo<'a> {
    /// Active platform genomes, ordered by name.
    pub async fn list_active(&self) -> Result<Vec<PersonaGenome>, sqlx::Error> {
        let rows = sqlx::query_as::<_, GenomeRow>(
            "SELECT id, name, system_prompt, tip_personality, avatar_url, \
                    art_metadata, is_active \
             FROM engine.persona_genomes \
             WHERE is_active = true \
             ORDER BY name",
        )
        .fetch_all(self.pool)
        .await?;
        Ok(rows.into_iter().map(PersonaGenome::from).collect())
    }

    pub async fn get_genome(&self, genome_id: Uuid) -> Result<Option<PersonaGenome>, sqlx::Error> {
        let row = sqlx::query_as::<_, GenomeRow>(
            "SELECT id, name, system_prompt, tip_personality, avatar_url, \
                    art_metadata, is_active \
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
            "SELECT name, is_active, asset_id \
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
            avatar_url: Option<String>,
            art_metadata: serde_json::Value,
            is_active: bool,
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
                pg.avatar_url       AS avatar_url, \
                pg.art_metadata     AS art_metadata, \
                pg.is_active        AS is_active \
             FROM engine.persona_instances pi \
             JOIN engine.persona_genomes pg ON pg.id = pi.genome_id \
             WHERE pi.id = $1 AND pi.status = 'active'",
        )
        .bind(instance_id)
        .fetch_optional(self.pool)
        .await?;

        Ok(row.map(|r| CompanionPersona {
            instance_id: r.instance_id,
            genome: PersonaGenome {
                id: r.g_id,
                name: r.name,
                system_prompt: r.system_prompt,
                tip_personality: r.tip_personality,
                avatar_url: r.avatar_url,
                art_metadata: r.art_metadata,
                is_active: r.is_active,
            },
            instance: PersonaInstance {
                id: r.instance_id,
                genome_id: r.genome_id,
                owner_uid: r.owner_uid,
                status: r.status,
            },
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
                pg.name      AS genome_name, \
                pg.asset_id  AS asset_id \
             FROM engine.persona_instances pi \
             JOIN engine.persona_genomes pg ON pg.id = pi.genome_id \
             WHERE pi.id = $1 AND pi.status = 'active'",
        )
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
        avatar_url: Option<&str>,
        art_metadata: serde_json::Value,
        is_active: bool,
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
                    avatar_url = $4, \
                    art_metadata = $5, \
                    is_active = $6 \
                 WHERE id = $1",
            )
            .bind(id)
            .bind(system_prompt)
            .bind(tip_personality)
            .bind(avatar_url)
            .bind(art_metadata)
            .bind(is_active)
            .execute(self.pool)
            .await?;
            return Ok((id, false));
        }
        let id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.persona_genomes \
                (name, system_prompt, tip_personality, avatar_url, art_metadata, is_active) \
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
        )
        .bind(name)
        .bind(system_prompt)
        .bind(tip_personality)
        .bind(avatar_url)
        .bind(art_metadata)
        .bind(is_active)
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
    /// Caller MUST run the NFT-ownership gate before this write.
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

    /// Returns the `asset_id` for an NFT-backed genome, or `None` for legacy
    /// seed-persona rows where the column is NULL. Used by the chat-start
    /// and per-message gates to decide whether to invoke the NFT ownership
    /// check.
    pub async fn get_asset_id_for_genome(
        &self,
        genome_id: uuid::Uuid,
    ) -> sqlx::Result<Option<String>> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT asset_id FROM engine.persona_genomes WHERE id = $1")
                .bind(genome_id)
                .fetch_optional(self.pool)
                .await?;
        Ok(row.and_then(|(opt,)| opt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn insert_genome(
        pool: &PgPool,
        name: &str,
        is_active: bool,
        art: serde_json::Value,
    ) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata, is_active) \
             VALUES ($1, $2, $3, $4) RETURNING id",
        )
        .bind(name)
        .bind("you are a companion")
        .bind(art)
        .bind(is_active)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn list_active_filters_by_is_active(pool: PgPool) {
        let repo = PersonaRepo { pool: &pool };
        let _g_active1 = insert_genome(&pool, "Aria", true, serde_json::json!({})).await;
        let _g_inactive = insert_genome(&pool, "Boris", false, serde_json::json!({})).await;
        let _g_active2 = insert_genome(&pool, "Cara", true, serde_json::json!({})).await;

        let active = repo.list_active().await.unwrap();
        assert_eq!(active.len(), 2);
        // ordered by name
        assert_eq!(active[0].name, "Aria");
        assert_eq!(active[1].name, "Cara");
        assert!(active.iter().all(|g| g.is_active));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn get_genome_returns_full_record(pool: PgPool) {
        let repo = PersonaRepo { pool: &pool };
        let id = insert_genome(&pool, "Nova", true, serde_json::json!({ "mbti": "INFP" })).await;

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
        let genome_id = insert_genome(&pool, "Echo", true, serde_json::json!({ "age": 27 })).await;

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
    async fn get_genome_gate_returns_name_active_and_asset_id(pool: PgPool) {
        let repo = PersonaRepo { pool: &pool };

        // legacy genome → asset_id NULL
        let legacy = insert_genome(&pool, "Legacy", true, serde_json::json!({})).await;
        let g = repo.get_genome_gate(legacy).await.unwrap().unwrap();
        assert_eq!(g.name, "Legacy");
        assert!(g.is_active);
        assert_eq!(g.asset_id, None);

        // NFT genome, inactive → asset_id Some, is_active false
        let nft = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.persona_genomes \
                (name, system_prompt, art_metadata, is_active, asset_id) \
             VALUES ('Nft', 'p', '{}'::jsonb, false, 'asset-xyz') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let g2 = repo.get_genome_gate(nft).await.unwrap().unwrap();
        assert_eq!(g2.name, "Nft");
        assert!(!g2.is_active);
        assert_eq!(g2.asset_id.as_deref(), Some("asset-xyz"));

        // missing → None
        assert!(repo.get_genome_gate(Uuid::new_v4()).await.unwrap().is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn find_active_instance_skips_archived_and_missing(pool: PgPool) {
        let repo = PersonaRepo { pool: &pool };
        let owner = Uuid::new_v4();
        let genome_id = insert_genome(&pool, "Echo", true, serde_json::json!({})).await;

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
        let genome_id = insert_genome(&pool, "Echo", true, serde_json::json!({})).await;
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
                (name, system_prompt, art_metadata, is_active, asset_id) \
             VALUES ('Nova', 'p', '{}'::jsonb, true, 'asset-1') RETURNING id",
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
        assert_eq!(gate.asset_id.as_deref(), Some("asset-1"));

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
        let genome_id = insert_genome(&pool, "Echo", true, serde_json::json!({})).await;

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
        assert_eq!(again, iid, "must reactivate existing row, not create a new one");

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
}

#[cfg(test)]
mod ownership_lookup_tests {
    use super::*;
    use sqlx::PgPool;

    #[sqlx::test(migrations = "./migrations")]
    async fn returns_none_for_legacy_genome(pool: PgPool) {
        let repo = PersonaRepo { pool: &pool };
        let id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.persona_genomes
                (id, name, system_prompt, art_metadata, is_active)
             VALUES ($1, 'Legacy', 'p', '{}'::jsonb, true)",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(repo.get_asset_id_for_genome(id).await.unwrap(), None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn returns_some_for_nft_genome(pool: PgPool) {
        let repo = PersonaRepo { pool: &pool };
        let id = uuid::Uuid::new_v4();
        let asset = "11111111111111111111111111111111";
        sqlx::query(
            "INSERT INTO engine.persona_genomes
                (id, name, system_prompt, art_metadata, is_active, asset_id)
             VALUES ($1, 'Nft', 'p', '{}'::jsonb, true, $2)",
        )
        .bind(id)
        .bind(asset)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(
            repo.get_asset_id_for_genome(id).await.unwrap().as_deref(),
            Some(asset)
        );
    }
}
