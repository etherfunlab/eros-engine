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
                pg.art_metadata     AS art_metadata \
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
                art_metadata: r.art_metadata,
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
                pg.name      AS genome_name \
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
}
