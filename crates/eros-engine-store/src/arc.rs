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

    /// Store a rewritten arc while the relationship lives (its scan state row
    /// exists): an archive or erasure that lands during the rewrite leaves
    /// nothing to write onto, and `false` comes back. The check takes
    /// `FOR KEY SHARE` on the state row, so a delete of it that has not
    /// committed holds the write until it does. The arc it replaces
    /// moves to `previous_arc`. The first write needs `opening`, which is then
    /// kept for good: later calls may omit it, and one that passes it again
    /// changes nothing.
    pub async fn replace_if_live(
        &self,
        instance_id: Uuid,
        opening: Option<&str>,
        arc: &str,
        folded_through: DateTime<Utc>,
        generation_id: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let written = match opening {
            Some(opening) => sqlx::query(
                "INSERT INTO engine.relationship_arcs \
                         (instance_id, opening, arc, folded_through, generation_id) \
                     SELECT $1::uuid, $2::text, $3::text, $4::timestamptz, $5::text \
                     WHERE EXISTS (SELECT 1 FROM engine.relationship_scan_state \
                                   WHERE instance_id = $1 FOR KEY SHARE) \
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
            .await?
            .rows_affected(),
            None => {
                let rewritten = sqlx::query(
                    "UPDATE engine.relationship_arcs SET \
                        previous_arc   = arc, \
                        arc            = $2, \
                        folded_through = $3, \
                        generation_id  = $4, \
                        updated_at     = now() \
                     WHERE instance_id = $1 \
                       AND EXISTS (SELECT 1 FROM engine.relationship_scan_state \
                                   WHERE instance_id = $1 FOR KEY SHARE)",
                )
                .bind(instance_id)
                .bind(arc)
                .bind(folded_through)
                .bind(generation_id)
                .execute(self.pool)
                .await?
                .rows_affected();
                if rewritten == 0 && self.live(instance_id).await? {
                    return Err(sqlx::Error::Protocol(
                        "opening required on first arc".into(),
                    ));
                }
                rewritten
            }
        };
        Ok(written > 0)
    }

    async fn live(&self, instance_id: Uuid) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM engine.relationship_scan_state \
             WHERE instance_id = $1 FOR KEY SHARE)",
        )
        .bind(instance_id)
        .fetch_one(self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan_state::ScanStateRepo;
    use crate::testutil::seed_persona_instance;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    /// A persona instance whose relationship is live: it has a scan state row.
    async fn live_instance(pool: &PgPool) -> Uuid {
        let instance = seed_persona_instance(pool, Uuid::new_v4()).await;
        ScanStateRepo { pool }
            .bump(instance, at(500), None)
            .await
            .unwrap();
        instance
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn arc_replace_keeps_opening_and_shifts_previous(pool: PgPool) {
        let repo = ArcRepo { pool: &pool };
        let instance = live_instance(&pool).await;
        assert!(repo.load(instance).await.unwrap().is_none());

        assert!(repo
            .replace_if_live(
                instance,
                Some("how it began"),
                "arc one",
                at(1_000),
                Some("gen-1"),
            )
            .await
            .unwrap());
        let a = repo.load(instance).await.unwrap().expect("arc exists");
        assert_eq!(a.instance_id, instance);
        assert_eq!(a.opening, "how it began");
        assert_eq!(a.arc, "arc one");
        assert_eq!(a.previous_arc, None);
        assert_eq!(a.folded_through, at(1_000));
        let first_updated = a.updated_at;

        assert!(repo
            .replace_if_live(instance, Some("ignored"), "arc two", at(2_000), None)
            .await
            .unwrap());
        let a = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(a.opening, "how it began", "the stored opening is kept");
        assert_eq!(a.arc, "arc two");
        assert_eq!(a.previous_arc.as_deref(), Some("arc one"));
        assert_eq!(a.folded_through, at(2_000));
        assert!(a.updated_at >= first_updated);

        assert!(repo
            .replace_if_live(instance, None, "arc three", at(3_000), None)
            .await
            .unwrap());
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
        let instance = live_instance(&pool).await;

        let err = repo
            .replace_if_live(instance, None, "arc", at(1_000), None)
            .await
            .expect_err("a first arc without an opening is a caller bug");
        assert!(matches!(err, sqlx::Error::Protocol(_)), "got {err:?}");
        assert!(repo.load(instance).await.unwrap().is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn replace_if_live_writes_nothing_without_a_state_row(pool: PgPool) {
        let repo = ArcRepo { pool: &pool };
        let gone = seed_persona_instance(&pool, Uuid::new_v4()).await;
        assert!(!repo
            .replace_if_live(gone, Some("how it began"), "arc", at(1_000), None)
            .await
            .unwrap());
        assert!(
            !repo
                .replace_if_live(gone, None, "arc", at(1_000), None)
                .await
                .unwrap(),
            "no relationship, no opening needed: nothing is written"
        );
        assert!(repo.load(gone).await.unwrap().is_none());

        // An arc whose state row went away after it was written (an archive
        // landing mid-rewrite) is not rewritten either.
        let instance = live_instance(&pool).await;
        assert!(repo
            .replace_if_live(instance, Some("how it began"), "arc one", at(1_000), None)
            .await
            .unwrap());
        sqlx::query("DELETE FROM engine.relationship_scan_state WHERE instance_id = $1")
            .bind(instance)
            .execute(&pool)
            .await
            .unwrap();
        for opening in [None, Some("how it began")] {
            assert!(!repo
                .replace_if_live(instance, opening, "arc two", at(2_000), None)
                .await
                .unwrap());
        }
        let a = repo.load(instance).await.unwrap().unwrap();
        assert_eq!((a.arc.as_str(), a.folded_through), ("arc one", at(1_000)));
    }

    /// An archive or erasure that has deleted the state row but not committed
    /// holds the rewrite until it commits; the rewrite then writes nothing.
    #[sqlx::test(migrations = "./migrations")]
    async fn replace_if_live_waits_for_an_uncommitted_state_delete(pool: PgPool) {
        let instance = live_instance(&pool).await;
        let mut erase = pool.begin().await.unwrap();
        sqlx::query("DELETE FROM engine.relationship_scan_state WHERE instance_id = $1")
            .bind(instance)
            .execute(&mut *erase)
            .await
            .unwrap();

        let write = tokio::spawn({
            let pool = pool.clone();
            async move {
                ArcRepo { pool: &pool }
                    .replace_if_live(instance, Some("how it began"), "arc", at(1_000), None)
                    .await
            }
        });
        crate::testutil::wait_for_lock_wait(&pool).await;
        assert!(!write.is_finished(), "the rewrite waits for the delete");
        erase.commit().await.unwrap();

        assert!(!write.await.unwrap().unwrap());
        assert!(ArcRepo { pool: &pool }
            .load(instance)
            .await
            .unwrap()
            .is_none());
    }
}
