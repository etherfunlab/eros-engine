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
        if let Some(id) = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM engine.persona_genomes WHERE name = $1",
        )
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
}
