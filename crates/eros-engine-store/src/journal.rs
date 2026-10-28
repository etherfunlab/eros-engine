// SPDX-License-Identifier: AGPL-3.0-only
//! The relationship journal (`engine.relationship_journal`): dated moments of
//! one relationship, written by the engine's own mechanisms (first meeting, a
//! tier first reached, an accepted gift or kiss) and by the scanner's LLM.

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use eros_engine_core::affinity::{Affinity, AffinityLine, BondLabel, ChemistryLabel};
use eros_engine_core::types::UserAction;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgExecutor, PgPool};
use uuid::Uuid;

use crate::memory::format_vector;

/// One journal row, every column but the embedding.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct JournalRow {
    pub id: Uuid,
    pub instance_id: Uuid,
    pub occurred_on: NaiveDate,
    pub kind: String,
    pub realm: String,
    pub summary: String,
    pub source: String,
    pub message_id: Option<Uuid>,
    pub affinity_event_id: Option<Uuid>,
    pub dedup_key: Option<String>,
    pub due_on: Option<NaiveDate>,
    pub recurrence: String,
    pub created_at: DateTime<Utc>,
}

const COLUMNS: &str = "id, instance_id, occurred_on, kind, realm, summary, source, message_id, \
                       affinity_event_id, dedup_key, due_on, recurrence, created_at";

#[derive(Debug, Clone)]
pub struct JournalInsert {
    pub instance_id: Uuid,
    pub occurred_on: NaiveDate,
    pub kind: String,
    /// `real` or `roleplay`.
    pub realm: String,
    pub summary: String,
    /// `llm` or `engine`.
    pub source: String,
    pub message_id: Option<Uuid>,
    pub affinity_event_id: Option<Uuid>,
    /// `None` marks a repeatable kind; a key is unique per relationship.
    pub dedup_key: Option<String>,
    pub due_on: Option<NaiveDate>,
    /// `none`, `monthly_then_yearly` or `on_due`.
    pub recurrence: String,
    pub embedding: Option<Vec<f32>>,
}

/// The calendar date of `at` in `tz`, UTC when there is none.
pub fn local_date(at: DateTime<Utc>, tz: Option<Tz>) -> NaiveDate {
    match tz {
        Some(tz) => at.with_timezone(&tz).date_naive(),
        None => at.date_naive(),
    }
}

/// The one INSERT behind every journal write. A row whose `dedup_key` is taken
/// is dropped (`None`): whichever writer lands first wins. With `claimed_only`
/// the row is written only while the relationship's scan claim is live.
async fn insert_row<'e>(
    executor: impl PgExecutor<'e>,
    j: &JournalInsert,
    claimed_only: bool,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO engine.relationship_journal \
             (instance_id, occurred_on, kind, realm, summary, source, message_id, \
              affinity_event_id, dedup_key, due_on, recurrence, embedding) \
         SELECT $1::uuid, $2::date, $3::text, $4::text, $5::text, $6::text, $7::uuid, \
                $8::uuid, $9::text, $10::date, $11::text, $12::vector \
         WHERE NOT $13::bool \
            OR EXISTS (SELECT 1 FROM engine.relationship_scan_state \
                       WHERE instance_id = $1 AND claimed_at IS NOT NULL) \
         ON CONFLICT (instance_id, dedup_key) WHERE dedup_key IS NOT NULL DO NOTHING \
         RETURNING id",
    )
    .bind(j.instance_id)
    .bind(j.occurred_on)
    .bind(&j.kind)
    .bind(&j.realm)
    .bind(&j.summary)
    .bind(&j.source)
    .bind(j.message_id)
    .bind(j.affinity_event_id)
    .bind(j.dedup_key.as_deref())
    .bind(j.due_on)
    .bind(&j.recurrence)
    .bind(j.embedding.as_deref().map(format_vector))
    .bind(claimed_only)
    .fetch_optional(executor)
    .await
}

/// Insert a journal row on any executor, so a caller can write it inside its
/// own transaction. `None` when the `dedup_key` was already taken.
pub async fn insert_journal<'e>(
    executor: impl PgExecutor<'e>,
    j: &JournalInsert,
) -> Result<Option<Uuid>, sqlx::Error> {
    insert_row(executor, j, false).await
}

/// A mechanism row: the engine wrote it, in the real world, with no embedding.
fn engine_row(
    instance_id: Uuid,
    on: NaiveDate,
    kind: &str,
    summary: String,
    dedup_key: String,
    recurrence: &str,
) -> JournalInsert {
    JournalInsert {
        instance_id,
        occurred_on: on,
        kind: kind.to_string(),
        realm: "real".to_string(),
        summary,
        source: "engine".to_string(),
        message_id: None,
        affinity_event_id: None,
        dedup_key: Some(dedup_key),
        due_on: None,
        recurrence: recurrence.to_string(),
        embedding: None,
    }
}

/// The relationship's first chat, on `on`.
pub fn first_meet(instance_id: Uuid, on: NaiveDate) -> JournalInsert {
    engine_row(
        instance_id,
        on,
        "first_meet",
        "你们第一次聊天".to_string(),
        "first_meet".to_string(),
        "monthly_then_yearly",
    )
}

/// `line` first reached the label `label_key` (`label_zh` in words), at the
/// affinity event `event_id`.
pub fn tier_reached(
    instance_id: Uuid,
    line: AffinityLine,
    label_key: &str,
    label_zh: &str,
    event_id: Uuid,
    on: NaiveDate,
) -> JournalInsert {
    JournalInsert {
        affinity_event_id: Some(event_id),
        ..engine_row(
            instance_id,
            on,
            "tier_reached",
            format!("你们的关系到了「{label_zh}」"),
            format!("{}:{label_key}", line.as_str()),
            "none",
        )
    }
}

/// An accepted gift, give or tip. A tip never carries its amount; a named item
/// is quoted by name.
pub fn gift(
    instance_id: Uuid,
    message_id: Uuid,
    on: NaiveDate,
    item_name: Option<&str>,
    is_tip: bool,
) -> JournalInsert {
    let summary = match (is_tip, item_name) {
        (true, _) => "用户给了你打赏".to_string(),
        (false, Some(name)) => format!("用户送了你{name}"),
        (false, None) => "用户送了你一份礼物".to_string(),
    };
    JournalInsert {
        message_id: Some(message_id),
        ..engine_row(
            instance_id,
            on,
            "gift",
            summary,
            format!("gift:{message_id}"),
            "none",
        )
    }
}

/// The first kiss the persona accepted.
pub fn first_kiss(instance_id: Uuid, message_id: Uuid, on: NaiveDate) -> JournalInsert {
    JournalInsert {
        message_id: Some(message_id),
        ..engine_row(
            instance_id,
            on,
            "first_intimacy",
            "用户亲了你，你接受了".to_string(),
            "first_intimacy:real".to_string(),
            "none",
        )
    }
}

/// The moment an accepted action leaves in the journal, read from the row's
/// stored `metadata.action` (which also holds the decided `response`, no part
/// of the action). A hug, touch, lick or custom action leaves none, and an
/// action that does not parse reads as no action.
pub(crate) fn accepted_moment(
    action: serde_json::Value,
    instance_id: Uuid,
    message_id: Uuid,
    on: NaiveDate,
) -> Option<JournalInsert> {
    fn named(name: Option<&str>) -> Option<&str> {
        name.filter(|n| !n.is_empty())
    }
    let mut obj = match action {
        serde_json::Value::Object(obj) => obj,
        _ => return None,
    };
    obj.remove("response");
    match serde_json::from_value(serde_json::Value::Object(obj)).ok()? {
        UserAction::Give { name, .. } => Some(gift(
            instance_id,
            message_id,
            on,
            named(name.as_deref()),
            false,
        )),
        UserAction::Gift { name, .. } => {
            Some(gift(instance_id, message_id, on, named(Some(&name)), false))
        }
        UserAction::Tip { .. } => Some(gift(instance_id, message_id, on, None, true)),
        UserAction::Kiss => Some(first_kiss(instance_id, message_id, on)),
        _ => None,
    }
}

/// `line`'s label on `a`: (rank, key, Chinese name).
fn rung(line: AffinityLine, a: &Affinity) -> (u8, &'static str, &'static str) {
    match line {
        AffinityLine::Bond => {
            let l = a.bond_label();
            (l.rank(), l.as_key(), l.zh())
        }
        AffinityLine::Chemistry => {
            let l = a.chemistry_label();
            (l.rank(), l.as_key(), l.zh())
        }
    }
}

/// Keys of `line`'s labels ranked above `rank`.
fn keys_above(line: AffinityLine, rank: u8) -> Vec<&'static str> {
    (1..=5u8)
        .filter_map(|tier| {
            let (r, key) = match line {
                AffinityLine::Bond => {
                    let l = BondLabel::from_tier(tier);
                    (l.rank(), l.as_key())
                }
                AffinityLine::Chemistry => {
                    let l = ChemistryLabel::from_tier(tier);
                    (l.rank(), l.as_key())
                }
            };
            (r > rank).then_some(key)
        })
        .collect()
}

/// Journal the label moves between `before` and `after`, on the transaction
/// that wrote `event_id`. A label first reached becomes a `tier_reached` row
/// (a label reached before keeps its row: the key is taken). A line that falls
/// takes back the rows of the labels it fell below, if they are under 24 hours
/// old.
pub(crate) async fn record_label_moves(
    conn: &mut PgConnection,
    before: &Affinity,
    after: &Affinity,
    event_id: Uuid,
    on: NaiveDate,
) -> Result<(), sqlx::Error> {
    for line in [AffinityLine::Bond, AffinityLine::Chemistry] {
        let (was, _, _) = rung(line, before);
        let (now, key, zh) = rung(line, after);
        if now > was {
            insert_journal(
                &mut *conn,
                &tier_reached(after.instance_id, line, key, zh, event_id, on),
            )
            .await?;
        } else if now < was {
            let undone: Vec<String> = keys_above(line, now)
                .into_iter()
                .map(|k| format!("{}:{k}", line.as_str()))
                .collect();
            sqlx::query(
                "DELETE FROM engine.relationship_journal \
                 WHERE instance_id = $1 AND kind = 'tier_reached' \
                   AND dedup_key = ANY($2) \
                   AND created_at > now() - interval '24 hours'",
            )
            .bind(after.instance_id)
            .bind(&undone)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

pub struct JournalRepo<'a> {
    pub pool: &'a PgPool,
}

impl JournalRepo<'_> {
    /// Store a row. `None` when its `dedup_key` was already taken.
    pub async fn insert(&self, j: &JournalInsert) -> Result<Option<Uuid>, sqlx::Error> {
        insert_journal(self.pool, j).await
    }

    /// `insert`, but only while the relationship's scan claim is live: the
    /// scanner's rows come through here, so a worker that lost its claim
    /// writes nothing.
    pub async fn insert_if_claimed(&self, j: &JournalInsert) -> Result<Option<Uuid>, sqlx::Error> {
        insert_row(self.pool, j, true).await
    }

    /// The real-world rows that come back around (anniversaries, due dates),
    /// oldest first.
    pub async fn recurring(&self, instance_id: Uuid) -> Result<Vec<JournalRow>, sqlx::Error> {
        sqlx::query_as::<_, JournalRow>(&format!(
            "SELECT {COLUMNS} FROM engine.relationship_journal \
             WHERE instance_id = $1 AND recurrence <> 'none' AND realm = 'real' \
             ORDER BY occurred_on, id"
        ))
        .bind(instance_id)
        .fetch_all(self.pool)
        .await
    }

    /// The `k` embedded rows nearest to `query`, nearest first.
    pub async fn search(
        &self,
        instance_id: Uuid,
        query: &[f32],
        k: i32,
    ) -> Result<Vec<JournalRow>, sqlx::Error> {
        sqlx::query_as::<_, JournalRow>(&format!(
            "SELECT {COLUMNS} FROM engine.relationship_journal \
             WHERE instance_id = $1 AND embedding IS NOT NULL \
             ORDER BY embedding <=> $2::vector \
             LIMIT $3"
        ))
        .bind(instance_id)
        .bind(format_vector(query))
        .bind(i64::from(k))
        .fetch_all(self.pool)
        .await
    }

    /// Up to `limit` rows still waiting for a vector, oldest first, across all
    /// relationships.
    pub async fn pending_embeddings(&self, limit: i64) -> Result<Vec<(Uuid, String)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT id, summary FROM engine.relationship_journal \
             WHERE embedding IS NULL \
             ORDER BY created_at, id \
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }

    /// Store the vectors of `rows`, all or none.
    pub async fn set_embeddings(&self, rows: &[(Uuid, Vec<f32>)]) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        for (id, embedding) in rows {
            sqlx::query(
                "UPDATE engine.relationship_journal SET embedding = $2::vector WHERE id = $1",
            )
            .bind(id)
            .bind(format_vector(embedding))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await
    }

    /// The date of the relationship's earliest chat row, in `tz` (UTC when
    /// `None`), across its live text and voice sessions.
    pub async fn first_meet_date(
        &self,
        instance_id: Uuid,
        tz: Option<Tz>,
    ) -> Result<Option<NaiveDate>, sqlx::Error> {
        let first: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT min(m.sent_at) FROM engine.chat_messages m \
             JOIN engine.chat_sessions s ON s.id = m.session_id \
             WHERE s.instance_id = $1 AND NOT s.archived \
               AND s.channel IN ('text', 'voice') \
               AND m.role IN ('user', 'assistant')",
        )
        .bind(instance_id)
        .fetch_one(self.pool)
        .await?;
        Ok(first.map(|at| local_date(at, tz)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan_state::ScanStateRepo;
    use crate::testutil::{seed_persona_instance, seed_session};
    use std::time::Duration;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    async fn seed_relationship(pool: &PgPool) -> (Uuid, Uuid) {
        let owner = Uuid::new_v4();
        (owner, seed_persona_instance(pool, owner).await)
    }

    async fn seed_msg(pool: &PgPool, session: Uuid, role: &str, sent_at: DateTime<Utc>) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
             VALUES ($1, $2, 'hi', $3) RETURNING id",
        )
        .bind(session)
        .bind(role)
        .bind(sent_at)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// A real message in a fresh live session of the relationship.
    async fn seed_message_of(pool: &PgPool, owner: Uuid, instance: Uuid) -> Uuid {
        let session = seed_session(pool, owner, instance, false).await;
        seed_msg(pool, session, "user", at(1_000)).await
    }

    /// A scanner-style row: no dedup key, no pointer, no vector.
    fn llm_row(instance: Uuid, summary: &str, realm: &str, recurrence: &str) -> JournalInsert {
        JournalInsert {
            instance_id: instance,
            occurred_on: day(2026, 3, 1),
            kind: "promise".to_string(),
            realm: realm.to_string(),
            summary: summary.to_string(),
            source: "llm".to_string(),
            message_id: None,
            affinity_event_id: None,
            dedup_key: None,
            due_on: None,
            recurrence: recurrence.to_string(),
            embedding: None,
        }
    }

    /// A unit vector along axis `i` of the 512-dim space.
    fn axis(i: usize) -> Vec<f32> {
        let mut v = vec![0.0; 512];
        v[i] = 1.0;
        v
    }

    async fn count(pool: &PgPool, instance: Uuid) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM engine.relationship_journal WHERE instance_id = $1",
        )
        .bind(instance)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn summaries(rows: &[JournalRow]) -> Vec<&str> {
        rows.iter().map(|r| r.summary.as_str()).collect()
    }

    #[test]
    fn mechanism_rows_carry_their_wording_and_keys() {
        let i = Uuid::new_v4();
        let m = Uuid::new_v4();
        let e = Uuid::new_v4();
        let on = day(2026, 3, 1);

        let r = first_meet(i, on);
        assert_eq!(
            (r.kind.as_str(), r.summary.as_str(), r.dedup_key.as_deref()),
            ("first_meet", "你们第一次聊天", Some("first_meet"))
        );
        assert_eq!(r.recurrence, "monthly_then_yearly");

        let r = tier_reached(i, AffinityLine::Bond, "friend", "朋友", e, on);
        assert_eq!(
            (r.kind.as_str(), r.summary.as_str(), r.dedup_key.as_deref()),
            (
                "tier_reached",
                "你们的关系到了「朋友」",
                Some("bond:friend")
            )
        );
        assert_eq!(r.affinity_event_id, Some(e));
        assert_eq!(r.recurrence, "none");
        let r = tier_reached(i, AffinityLine::Chemistry, "crush", "迷恋", e, on);
        assert_eq!(r.dedup_key.as_deref(), Some("chemistry:crush"));

        let r = gift(i, m, on, Some("红色旗袍"), false);
        assert_eq!(
            (r.kind.as_str(), r.summary.as_str(), r.dedup_key),
            ("gift", "用户送了你红色旗袍", Some(format!("gift:{m}")))
        );
        assert_eq!(r.message_id, Some(m));
        assert_eq!(gift(i, m, on, None, false).summary, "用户送了你一份礼物");
        assert_eq!(
            gift(i, m, on, Some("威士忌"), true).summary,
            "用户给了你打赏",
            "a tip names neither item nor amount"
        );

        let r = first_kiss(i, m, on);
        assert_eq!(
            (r.kind.as_str(), r.summary.as_str(), r.dedup_key.as_deref()),
            (
                "first_intimacy",
                "用户亲了你，你接受了",
                Some("first_intimacy:real")
            )
        );
        assert_eq!(r.message_id, Some(m));

        for r in [
            first_meet(i, on),
            tier_reached(i, AffinityLine::Bond, "friend", "朋友", e, on),
            gift(i, m, on, None, false),
            first_kiss(i, m, on),
        ] {
            assert_eq!(
                (r.instance_id, r.occurred_on, r.realm.as_str()),
                (i, on, "real")
            );
            assert_eq!(r.source, "engine");
            assert!(r.embedding.is_none() && r.due_on.is_none());
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn dedup_key_conflict_keeps_first_row(pool: PgPool) {
        let repo = JournalRepo { pool: &pool };
        let (owner, instance) = seed_relationship(&pool).await;
        let first_msg = seed_message_of(&pool, owner, instance).await;
        let second_msg = seed_message_of(&pool, owner, instance).await;

        let id = repo
            .insert(&first_kiss(instance, first_msg, day(2026, 3, 1)))
            .await
            .unwrap()
            .expect("the first kiss is written");
        assert_eq!(
            repo.insert(&first_kiss(instance, second_msg, day(2026, 4, 9)))
                .await
                .unwrap(),
            None,
            "the key is taken"
        );
        assert_eq!(count(&pool, instance).await, 1);
        let kept: (Uuid, NaiveDate, Option<Uuid>) = sqlx::query_as(
            "SELECT id, occurred_on, message_id FROM engine.relationship_journal \
             WHERE instance_id = $1",
        )
        .bind(instance)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kept, (id, day(2026, 3, 1), Some(first_msg)));

        // The key is per relationship: another one takes it too.
        let (_, other) = seed_relationship(&pool).await;
        assert!(repo
            .insert(&first_kiss(other, first_msg, day(2026, 5, 1)))
            .await
            .unwrap()
            .is_some());

        // No key, no conflict.
        for _ in 0..2 {
            assert!(repo
                .insert(&llm_row(instance, "promised a trip", "real", "none"))
                .await
                .unwrap()
                .is_some());
        }
        assert_eq!(count(&pool, instance).await, 3);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn insert_if_claimed_requires_a_claim(pool: PgPool) {
        let repo = JournalRepo { pool: &pool };
        let scan = ScanStateRepo { pool: &pool };
        let (_, instance) = seed_relationship(&pool).await;
        let row = llm_row(instance, "moved to Osaka", "real", "none");

        assert_eq!(
            repo.insert_if_claimed(&row).await.unwrap(),
            None,
            "no scan state, no row"
        );
        scan.bump(instance, at(1_000), None).await.unwrap();
        assert_eq!(
            repo.insert_if_claimed(&row).await.unwrap(),
            None,
            "an unclaimed relationship takes no scanner row"
        );
        assert_eq!(count(&pool, instance).await, 0);
        assert!(
            repo.insert(&row).await.unwrap().is_some(),
            "the mechanism path needs no claim"
        );

        assert!(scan
            .claim(instance, Duration::from_secs(600))
            .await
            .unwrap());
        let keyed = JournalInsert {
            dedup_key: Some("k".to_string()),
            ..row.clone()
        };
        assert!(repo.insert_if_claimed(&keyed).await.unwrap().is_some());
        assert_eq!(
            repo.insert_if_claimed(&keyed).await.unwrap(),
            None,
            "a claimed write still honours the dedup key"
        );
        assert_eq!(count(&pool, instance).await, 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recurring_excludes_roleplay_and_none(pool: PgPool) {
        let repo = JournalRepo { pool: &pool };
        let (_, instance) = seed_relationship(&pool).await;
        let (_, other) = seed_relationship(&pool).await;

        let due = JournalInsert {
            kind: "promise".to_string(),
            occurred_on: day(2026, 2, 1),
            due_on: Some(day(2026, 9, 14)),
            ..llm_row(instance, "meet at the lake", "real", "on_due")
        };
        repo.insert(&first_meet(instance, day(2026, 1, 5)))
            .await
            .unwrap();
        repo.insert(&due).await.unwrap();
        repo.insert(&llm_row(
            instance,
            "roleplay wedding",
            "roleplay",
            "monthly_then_yearly",
        ))
        .await
        .unwrap();
        repo.insert(&llm_row(instance, "one-off", "real", "none"))
            .await
            .unwrap();
        repo.insert(&first_meet(other, day(2025, 1, 1)))
            .await
            .unwrap();

        let rows = repo.recurring(instance).await.unwrap();
        assert_eq!(summaries(&rows), ["你们第一次聊天", "meet at the lake"]);

        let first = &rows[0];
        assert_eq!(first.instance_id, instance);
        assert_eq!(first.occurred_on, day(2026, 1, 5));
        assert_eq!(
            (first.kind.as_str(), first.realm.as_str()),
            ("first_meet", "real")
        );
        assert_eq!(first.source, "engine");
        assert_eq!(first.dedup_key.as_deref(), Some("first_meet"));
        assert_eq!(first.recurrence, "monthly_then_yearly");
        assert_eq!(
            (first.message_id, first.affinity_event_id, first.due_on),
            (None, None, None)
        );
        let promise = &rows[1];
        assert_eq!(promise.due_on, Some(day(2026, 9, 14)));
        assert_eq!(promise.source, "llm");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn search_ignores_rows_without_embedding(pool: PgPool) {
        let repo = JournalRepo { pool: &pool };
        let (_, instance) = seed_relationship(&pool).await;
        let (_, other) = seed_relationship(&pool).await;

        let with = |summary: &str, who: Uuid, v: Option<Vec<f32>>| JournalInsert {
            embedding: v,
            ..llm_row(who, summary, "real", "none")
        };
        repo.insert(&with("far", instance, Some(axis(1))))
            .await
            .unwrap();
        repo.insert(&with("near", instance, Some(axis(0))))
            .await
            .unwrap();
        repo.insert(&with("unembedded", instance, None))
            .await
            .unwrap();
        repo.insert(&with("foreign", other, Some(axis(0))))
            .await
            .unwrap();

        let hits = repo.search(instance, &axis(0), 10).await.unwrap();
        assert_eq!(summaries(&hits), ["near", "far"]);
        let top = repo.search(instance, &axis(0), 1).await.unwrap();
        assert_eq!(summaries(&top), ["near"]);
        assert!(repo.search(instance, &axis(0), 0).await.unwrap().is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn pending_embeddings_then_set(pool: PgPool) {
        let repo = JournalRepo { pool: &pool };
        let (_, instance) = seed_relationship(&pool).await;
        let (_, other) = seed_relationship(&pool).await;

        let a = repo
            .insert(&llm_row(instance, "a", "real", "none"))
            .await
            .unwrap()
            .unwrap();
        let b = repo
            .insert(&llm_row(other, "b", "real", "none"))
            .await
            .unwrap()
            .unwrap();
        repo.insert(&JournalInsert {
            embedding: Some(axis(5)),
            ..llm_row(instance, "already embedded", "real", "none")
        })
        .await
        .unwrap();

        let pending = repo.pending_embeddings(10).await.unwrap();
        assert_eq!(
            pending,
            vec![(a, "a".to_string()), (b, "b".to_string())],
            "oldest first, across relationships, embedded rows absent"
        );
        assert_eq!(repo.pending_embeddings(1).await.unwrap().len(), 1);

        repo.set_embeddings(&[]).await.unwrap();
        assert_eq!(repo.pending_embeddings(10).await.unwrap().len(), 2);

        repo.set_embeddings(&[(a, axis(0)), (b, axis(1))])
            .await
            .unwrap();
        assert!(repo.pending_embeddings(10).await.unwrap().is_empty());
        let hits = repo.search(instance, &axis(0), 10).await.unwrap();
        assert_eq!(summaries(&hits), ["a", "already embedded"]);
        let hits = repo.search(other, &axis(1), 10).await.unwrap();
        assert_eq!(summaries(&hits), ["b"]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn first_meet_date_uses_earliest_non_archived_row_in_tz(pool: PgPool) {
        use chrono_tz::America::Los_Angeles;
        use chrono_tz::Asia::Tokyo;
        let repo = JournalRepo { pool: &pool };
        let (owner, instance) = seed_relationship(&pool).await;
        let (other_owner, other) = seed_relationship(&pool).await;

        assert_eq!(repo.first_meet_date(instance, None).await.unwrap(), None);

        let live = seed_session(&pool, owner, instance, false).await;
        let archived = seed_session(&pool, owner, instance, true).await;
        let foreign = seed_session(&pool, other_owner, other, false).await;
        // 2026-03-01T17:00Z is already the 2nd in Tokyo.
        let first = DateTime::parse_from_rfc3339("2026-03-01T17:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        seed_msg(&pool, live, "assistant", first + chrono::Duration::days(3)).await;
        seed_msg(&pool, live, "user", first).await;
        seed_msg(
            &pool,
            live,
            "system_error",
            first - chrono::Duration::days(20),
        )
        .await;
        seed_msg(&pool, archived, "user", first - chrono::Duration::days(30)).await;
        seed_msg(&pool, foreign, "user", first - chrono::Duration::days(40)).await;

        assert_eq!(
            repo.first_meet_date(instance, Some(Tokyo)).await.unwrap(),
            Some(day(2026, 3, 2))
        );
        assert_eq!(
            repo.first_meet_date(instance, None).await.unwrap(),
            Some(day(2026, 3, 1))
        );
        assert_eq!(
            repo.first_meet_date(instance, Some(Los_Angeles))
                .await
                .unwrap(),
            Some(day(2026, 3, 1))
        );
    }
}
