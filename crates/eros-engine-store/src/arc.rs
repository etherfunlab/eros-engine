// SPDX-License-Identifier: AGPL-3.0-only
//! The rolling relationship arc (`engine.relationship_arcs`): one row per
//! persona instance holding how the relationship began, the arc so far, and
//! the arc it replaced.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ArcRow {
    pub instance_id: Uuid,
    /// How the relationship began; written once, with the first arc.
    pub opening: String,
    pub arc: String,
    /// The arc before the latest rewrite.
    pub previous_arc: Option<String>,
    /// The `ended_at` of the newest episode the arc has absorbed.
    pub folded_through: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct ArcRepo<'a> {
    pub pool: &'a PgPool,
}

impl ArcRepo<'_> {
    pub async fn load(&self, instance_id: Uuid) -> Result<Option<ArcRow>, sqlx::Error> {
        sqlx::query_as::<_, ArcRow>(
            "SELECT instance_id, opening, arc, previous_arc, folded_through, updated_at \
             FROM engine.relationship_arcs WHERE instance_id = $1",
        )
        .bind(instance_id)
        .fetch_optional(self.pool)
        .await
    }

    /// Store a rewritten arc; the one it replaces moves to `previous_arc`. The
    /// first write needs `opening`, which is then kept for good: later calls
    /// may omit it, and one that passes it again changes nothing.
    pub async fn replace(
        &self,
        instance_id: Uuid,
        opening: Option<&str>,
        arc: &str,
        folded_through: DateTime<Utc>,
        generation_id: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        match opening {
            Some(opening) => {
                sqlx::query(
                    "INSERT INTO engine.relationship_arcs \
                         (instance_id, opening, arc, folded_through, generation_id) \
                     VALUES ($1, $2, $3, $4, $5) \
                     ON CONFLICT (instance_id) DO UPDATE SET \
                        previous_arc   = relationship_arcs.arc, \
                        arc            = EXCLUDED.arc, \
                        folded_through = EXCLUDED.folded_through, \
                        generation_id  = EXCLUDED.generation_id, \
                        updated_at     = now()",
                )
                .bind(instance_id)
                .bind(opening)
                .bind(arc)
                .bind(folded_through)
                .bind(generation_id)
                .execute(self.pool)
                .await?;
            }
            None => {
                let rewritten = sqlx::query(
                    "UPDATE engine.relationship_arcs SET \
                        previous_arc   = arc, \
                        arc            = $2, \
                        folded_through = $3, \
                        generation_id  = $4, \
                        updated_at     = now() \
                     WHERE instance_id = $1",
                )
                .bind(instance_id)
                .bind(arc)
                .bind(folded_through)
                .bind(generation_id)
                .execute(self.pool)
                .await?
                .rows_affected();
                if rewritten == 0 {
                    return Err(sqlx::Error::Protocol(
                        "opening required on first arc".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::seed_persona_instance;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn arc_replace_keeps_opening_and_shifts_previous(pool: PgPool) {
        let repo = ArcRepo { pool: &pool };
        let instance = seed_persona_instance(&pool, Uuid::new_v4()).await;
        assert!(repo.load(instance).await.unwrap().is_none());

        repo.replace(
            instance,
            Some("how it began"),
            "arc one",
            at(1_000),
            Some("gen-1"),
        )
        .await
        .unwrap();
        let a = repo.load(instance).await.unwrap().expect("arc exists");
        assert_eq!(a.instance_id, instance);
        assert_eq!(a.opening, "how it began");
        assert_eq!(a.arc, "arc one");
        assert_eq!(a.previous_arc, None);
        assert_eq!(a.folded_through, at(1_000));
        let first_updated = a.updated_at;

        repo.replace(instance, Some("ignored"), "arc two", at(2_000), None)
            .await
            .unwrap();
        let a = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(a.opening, "how it began", "the stored opening is kept");
        assert_eq!(a.arc, "arc two");
        assert_eq!(a.previous_arc.as_deref(), Some("arc one"));
        assert_eq!(a.folded_through, at(2_000));
        assert!(a.updated_at >= first_updated);

        repo.replace(instance, None, "arc three", at(3_000), None)
            .await
            .unwrap();
        let a = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(a.opening, "how it began");
        assert_eq!(a.arc, "arc three");
        assert_eq!(
            a.previous_arc.as_deref(),
            Some("arc two"),
            "only the arc before the latest rewrite is kept"
        );

        let generation: Option<String> = sqlx::query_scalar(
            "SELECT generation_id FROM engine.relationship_arcs WHERE instance_id = $1",
        )
        .bind(instance)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(generation, None, "generation_id follows the latest rewrite");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn arc_first_write_needs_an_opening(pool: PgPool) {
        let repo = ArcRepo { pool: &pool };
        let instance = seed_persona_instance(&pool, Uuid::new_v4()).await;

        let err = repo
            .replace(instance, None, "arc", at(1_000), None)
            .await
            .expect_err("a first arc without an opening is a caller bug");
        assert!(matches!(err, sqlx::Error::Protocol(_)), "got {err:?}");
        assert!(repo.load(instance).await.unwrap().is_none());
    }
}
