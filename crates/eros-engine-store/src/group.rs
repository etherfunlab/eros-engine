// SPDX-License-Identifier: AGPL-3.0-only

//! Group chat sessions and their members
//! (docs/superpowers/specs/2026-10-12-group-chat-design.md §3).
//!
//! A group is a `chat_sessions` row with `channel = 'group'`. Its members are
//! the caller's own 1:1 text sessions. Every write that changes membership
//! runs under `FOR UPDATE` on the group row, which is what holds the cap.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use eros_engine_core::group::MAX_GROUP_MEMBERS;

pub struct GroupRepo<'a> {
    pub pool: &'a PgPool,
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct GroupMember {
    pub member_session_id: Uuid,
    pub instance_id: Uuid,
    pub joined_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub id: Uuid,
    pub user_id: Uuid,
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
    pub members: Vec<GroupMember>,
}

/// Why a session cannot join a group. `NotFound` covers "not yours" too, so a
/// caller never learns another user's session exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberRefusal {
    NotFound,
    WrongChannel,
    Archived,
    InstanceAlreadyMember,
    GroupFull,
}

#[derive(sqlx::FromRow)]
struct GroupRow {
    id: Uuid,
    user_id: Uuid,
    title: Option<String>,
    created_at: DateTime<Utc>,
    last_active_at: DateTime<Utc>,
}

const MEMBERS_SQL: &str = "SELECT gm.member_session_id, s.instance_id, gm.joined_at \
     FROM engine.chat_session_members gm \
     JOIN engine.chat_sessions s ON s.id = gm.member_session_id \
     WHERE gm.group_session_id = $1 AND NOT s.archived AND s.instance_id IS NOT NULL \
     ORDER BY gm.joined_at, gm.member_session_id";

impl<'a> GroupRepo<'a> {
    pub async fn create(
        &self,
        user_id: Uuid,
        title: Option<&str>,
        members: &[Uuid],
    ) -> Result<Result<Group, MemberRefusal>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, channel, title) \
             VALUES ($1, 'group', $2) RETURNING id",
        )
        .bind(user_id)
        .bind(title)
        .fetch_one(&mut *tx)
        .await?;
        for &m in members {
            if let Err(r) = admit(&mut tx, id, user_id, m).await? {
                // Dropping `tx` rolls the group row back with the refusal.
                return Ok(Err(r));
            }
        }
        tx.commit().await?;
        Ok(Ok(self
            .get(id, user_id)
            .await?
            .expect("the group just committed")))
    }

    pub async fn get(&self, group_id: Uuid, user_id: Uuid) -> Result<Option<Group>, sqlx::Error> {
        let row = sqlx::query_as::<_, GroupRow>(
            "SELECT id, user_id, title, created_at, last_active_at FROM engine.chat_sessions \
             WHERE id = $1 AND user_id = $2 AND channel = 'group' AND NOT archived",
        )
        .bind(group_id)
        .bind(user_id)
        .fetch_optional(self.pool)
        .await?;
        match row {
            Some(r) => Ok(Some(self.with_members(r).await?)),
            None => Ok(None),
        }
    }

    pub async fn list(&self, user_id: Uuid) -> Result<Vec<Group>, sqlx::Error> {
        let rows = sqlx::query_as::<_, GroupRow>(
            "SELECT id, user_id, title, created_at, last_active_at FROM engine.chat_sessions \
             WHERE user_id = $1 AND channel = 'group' AND NOT archived \
             ORDER BY last_active_at DESC",
        )
        .bind(user_id)
        .fetch_all(self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(self.with_members(r).await?);
        }
        Ok(out)
    }

    pub async fn rename(
        &self,
        group_id: Uuid,
        user_id: Uuid,
        title: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE engine.chat_sessions SET title = $3 \
             WHERE id = $1 AND user_id = $2 AND channel = 'group' AND NOT archived",
        )
        .bind(group_id)
        .bind(user_id)
        .bind(title)
        .execute(self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    pub async fn add_member(
        &self,
        group_id: Uuid,
        user_id: Uuid,
        member: Uuid,
    ) -> Result<Result<(), MemberRefusal>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let locked: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM engine.chat_sessions \
             WHERE id = $1 AND user_id = $2 AND channel = 'group' AND NOT archived \
             FOR UPDATE",
        )
        .bind(group_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if locked.is_none() {
            return Ok(Err(MemberRefusal::NotFound));
        }
        let res = admit(&mut tx, group_id, user_id, member).await?;
        if res.is_ok() {
            tx.commit().await?;
        }
        Ok(res)
    }

    pub async fn remove_member(
        &self,
        group_id: Uuid,
        user_id: Uuid,
        member: Uuid,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "DELETE FROM engine.chat_session_members gm USING engine.chat_sessions g \
             WHERE gm.group_session_id = $1 AND gm.member_session_id = $3 \
               AND g.id = gm.group_session_id AND g.user_id = $2",
        )
        .bind(group_id)
        .bind(user_id)
        .bind(member)
        .execute(self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    pub async fn archive(&self, group_id: Uuid, user_id: Uuid) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE engine.chat_sessions SET archived = true \
             WHERE id = $1 AND user_id = $2 AND channel = 'group' AND NOT archived",
        )
        .bind(group_id)
        .bind(user_id)
        .execute(self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Members whose 1:1 session is still live, in join order.
    pub async fn active_members(&self, group_id: Uuid) -> Result<Vec<GroupMember>, sqlx::Error> {
        sqlx::query_as::<_, GroupMember>(MEMBERS_SQL)
            .bind(group_id)
            .fetch_all(self.pool)
            .await
    }

    /// Persona display names for these sessions (archived or not). Missing
    /// sessions are simply absent from the map.
    pub async fn speaker_names(
        &self,
        session_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, String>, sqlx::Error> {
        let rows: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT s.id, g.name FROM engine.chat_sessions s \
             JOIN engine.persona_instances pi ON pi.id = s.instance_id \
             JOIN engine.persona_genomes g ON g.id = pi.genome_id \
             WHERE s.id = ANY($1)",
        )
        .bind(session_ids)
        .fetch_all(self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    async fn with_members(&self, r: GroupRow) -> Result<Group, sqlx::Error> {
        Ok(Group {
            members: self.active_members(r.id).await?,
            id: r.id,
            user_id: r.user_id,
            title: r.title,
            created_at: r.created_at,
            last_active_at: r.last_active_at,
        })
    }
}

/// The member checks and the insert, inside the caller's transaction (which
/// holds the group row). Order: existence/ownership, channel, archived,
/// instance already seated, cap.
async fn admit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group_id: Uuid,
    user_id: Uuid,
    member: Uuid,
) -> Result<Result<(), MemberRefusal>, sqlx::Error> {
    let row: Option<(Uuid, String, bool, Option<Uuid>)> = sqlx::query_as(
        "SELECT user_id, channel, archived, instance_id FROM engine.chat_sessions WHERE id = $1 FOR SHARE",
    )
    .bind(member)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((owner, channel, archived, instance_id)) = row else {
        return Ok(Err(MemberRefusal::NotFound));
    };
    if owner != user_id {
        return Ok(Err(MemberRefusal::NotFound));
    }
    if channel != "text" {
        return Ok(Err(MemberRefusal::WrongChannel));
    }
    if archived {
        return Ok(Err(MemberRefusal::Archived));
    }
    let Some(instance_id) = instance_id else {
        return Ok(Err(MemberRefusal::NotFound));
    };
    let seated: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM engine.chat_session_members gm \
                        JOIN engine.chat_sessions s ON s.id = gm.member_session_id \
                        WHERE gm.group_session_id = $1 AND s.instance_id = $2 AND NOT s.archived)",
    )
    .bind(group_id)
    .bind(instance_id)
    .fetch_one(&mut **tx)
    .await?;
    if seated {
        return Ok(Err(MemberRefusal::InstanceAlreadyMember));
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM engine.chat_session_members gm \
         JOIN engine.chat_sessions s ON s.id = gm.member_session_id \
         WHERE gm.group_session_id = $1 AND NOT s.archived",
    )
    .bind(group_id)
    .fetch_one(&mut **tx)
    .await?;
    if count >= MAX_GROUP_MEMBERS as i64 {
        return Ok(Err(MemberRefusal::GroupFull));
    }
    // clock_timestamp(), not the column's now() default: now() is the
    // transaction's start, so every member `create` adds would share one
    // joined_at and lose their argument order.
    sqlx::query(
        "INSERT INTO engine.chat_session_members (group_session_id, member_session_id, joined_at) \
         VALUES ($1, $2, clock_timestamp())",
    )
    .bind(group_id)
    .bind(member)
    .execute(&mut **tx)
    .await?;
    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ChatRepo;
    use crate::testutil::seed_persona_instance;

    async fn text_session(pool: &PgPool, user_id: Uuid) -> Uuid {
        let instance_id = seed_persona_instance(pool, user_id).await;
        ChatRepo { pool }
            .create_session(user_id, instance_id)
            .await
            .unwrap()
            .id
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn create_get_list_rename_archive(pool: PgPool) {
        let repo = GroupRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let a = text_session(&pool, user_id).await;
        let b = text_session(&pool, user_id).await;
        let g = repo
            .create(user_id, Some("周末爬山"), &[a, b])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(g.title.as_deref(), Some("周末爬山"));
        assert_eq!(
            g.members
                .iter()
                .map(|m| m.member_session_id)
                .collect::<Vec<_>>(),
            vec![a, b]
        );

        assert!(
            repo.get(g.id, Uuid::new_v4()).await.unwrap().is_none(),
            "not the owner"
        );
        assert!(repo.rename(g.id, user_id, None).await.unwrap());
        assert_eq!(repo.get(g.id, user_id).await.unwrap().unwrap().title, None);
        assert_eq!(repo.list(user_id).await.unwrap().len(), 1);

        assert!(repo.archive(g.id, user_id).await.unwrap());
        assert!(repo.get(g.id, user_id).await.unwrap().is_none());
        assert!(repo.list(user_id).await.unwrap().is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn add_member_refusals(pool: PgPool) {
        let repo = GroupRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let a = text_session(&pool, user_id).await;
        let g = repo.create(user_id, None, &[a]).await.unwrap().unwrap();

        let foreign = text_session(&pool, Uuid::new_v4()).await;
        assert_eq!(
            repo.add_member(g.id, user_id, foreign).await.unwrap(),
            Err(MemberRefusal::NotFound)
        );

        let instance_id = seed_persona_instance(&pool, user_id).await;
        let voice = ChatRepo { pool: &pool }
            .create_session_with_metadata(user_id, instance_id, serde_json::json!({}), "voice")
            .await
            .unwrap()
            .id;
        assert_eq!(
            repo.add_member(g.id, user_id, voice).await.unwrap(),
            Err(MemberRefusal::WrongChannel)
        );

        let archived = text_session(&pool, user_id).await;
        sqlx::query("UPDATE engine.chat_sessions SET archived = true WHERE id = $1")
            .bind(archived)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            repo.add_member(g.id, user_id, archived).await.unwrap(),
            Err(MemberRefusal::Archived)
        );

        // Same instance as `a`, different session (e.g. a re-created one).
        let a_instance: Uuid =
            sqlx::query_scalar("SELECT instance_id FROM engine.chat_sessions WHERE id = $1")
                .bind(a)
                .fetch_one(&pool)
                .await
                .unwrap();
        let twin = ChatRepo { pool: &pool }
            .create_session(user_id, a_instance)
            .await
            .unwrap()
            .id;
        assert_eq!(
            repo.add_member(g.id, user_id, twin).await.unwrap(),
            Err(MemberRefusal::InstanceAlreadyMember)
        );
        assert_eq!(
            repo.add_member(g.id, user_id, a).await.unwrap(),
            Err(MemberRefusal::InstanceAlreadyMember),
            "re-adding a present member"
        );

        let b = text_session(&pool, user_id).await;
        let c = text_session(&pool, user_id).await;
        let d = text_session(&pool, user_id).await;
        assert_eq!(repo.add_member(g.id, user_id, b).await.unwrap(), Ok(()));
        assert_eq!(repo.add_member(g.id, user_id, c).await.unwrap(), Ok(()));
        assert_eq!(
            repo.add_member(g.id, user_id, d).await.unwrap(),
            Err(MemberRefusal::GroupFull)
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn create_refuses_a_fourth_member_and_rolls_back(pool: PgPool) {
        let repo = GroupRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let mut ids = Vec::new();
        for _ in 0..4 {
            ids.push(text_session(&pool, user_id).await);
        }
        assert_eq!(
            repo.create(user_id, None, &ids).await.unwrap().err(),
            Some(MemberRefusal::GroupFull)
        );
        let groups: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_sessions WHERE user_id = $1 AND channel = 'group'",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(groups, 0, "the group row rolls back with the refusal");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn concurrent_adds_respect_the_cap(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let a = text_session(&pool, user_id).await;
        let b = text_session(&pool, user_id).await;
        let g = GroupRepo { pool: &pool }
            .create(user_id, None, &[a, b])
            .await
            .unwrap()
            .unwrap();
        let c = text_session(&pool, user_id).await;
        let d = text_session(&pool, user_id).await;
        let (p1, p2) = (pool.clone(), pool.clone());
        let (r1, r2) = tokio::join!(
            async move {
                GroupRepo { pool: &p1 }
                    .add_member(g.id, user_id, c)
                    .await
                    .unwrap()
            },
            async move {
                GroupRepo { pool: &p2 }
                    .add_member(g.id, user_id, d)
                    .await
                    .unwrap()
            },
        );
        let oks = [r1, r2].iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            oks, 1,
            "exactly one of two racing adds fills the last seat: {r1:?} {r2:?}"
        );
        assert!([r1, r2].contains(&Err(MemberRefusal::GroupFull)));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn active_members_skips_archived_sessions(pool: PgPool) {
        let repo = GroupRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let a = text_session(&pool, user_id).await;
        let b = text_session(&pool, user_id).await;
        let g = repo.create(user_id, None, &[a, b]).await.unwrap().unwrap();
        // An operator flip that leaves the membership row in place.
        sqlx::query("UPDATE engine.chat_sessions SET archived = true WHERE id = $1")
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();
        let active = repo.active_members(g.id).await.unwrap();
        assert_eq!(
            active
                .iter()
                .map(|m| m.member_session_id)
                .collect::<Vec<_>>(),
            vec![b]
        );
        assert!(repo.remove_member(g.id, user_id, b).await.unwrap());
        assert!(repo.active_members(g.id).await.unwrap().is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn speaker_names_resolve_through_the_genome(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let a = text_session(&pool, user_id).await;
        let names = GroupRepo { pool: &pool }
            .speaker_names(&[a, Uuid::new_v4()])
            .await
            .unwrap();
        assert_eq!(names.len(), 1);
        assert!(
            names[&a].starts_with("seed-"),
            "seed_persona_instance names genomes seed-<uuid>"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn archived_seat_does_not_block_the_same_persona(pool: PgPool) {
        let repo = GroupRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let a = text_session(&pool, user_id).await;
        let g = repo.create(user_id, None, &[a]).await.unwrap().unwrap();
        sqlx::query("UPDATE engine.chat_sessions SET archived = true WHERE id = $1")
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();
        let a_instance: Uuid =
            sqlx::query_scalar("SELECT instance_id FROM engine.chat_sessions WHERE id = $1")
                .bind(a)
                .fetch_one(&pool)
                .await
                .unwrap();
        let fresh = ChatRepo { pool: &pool }
            .create_session(user_id, a_instance)
            .await
            .unwrap()
            .id;
        assert_eq!(repo.add_member(g.id, user_id, fresh).await.unwrap(), Ok(()));
        let active = repo.active_members(g.id).await.unwrap();
        assert_eq!(
            active
                .iter()
                .map(|m| m.member_session_id)
                .collect::<Vec<_>>(),
            vec![fresh]
        );
    }
}
