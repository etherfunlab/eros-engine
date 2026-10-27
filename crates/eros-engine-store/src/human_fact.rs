// SPDX-License-Identifier: AGPL-3.0-only
//! Ledger of distinct facts mined about a user (`engine.human_facts`): one row
//! per fact, counted again each time the extractor sees it. Labels use a closed
//! vocabulary validated here; the table keeps them as free text so a retired
//! value never blocks a write.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

pub const DOMAINS: &[&str] = &[
    "basic_profile",
    "career",
    "education",
    "technology",
    "finance",
    "family",
    "friendship",
    "romance",
    "sexuality",
    "kink",
    "health",
    "mental_health",
    "lifestyle",
    "hobby",
    "ideology",
    "personality",
    "communication",
    "other",
];
pub const CATEGORIES: &[&str] = &[
    "fact",
    "preference",
    "aversion",
    "goal",
    "value",
    "habit",
    "skill",
    "event",
    "emotion",
    "relation",
    "communication_style",
    "roleplay_tendency",
    "constraint",
    "other",
];
pub const EVIDENCE_TYPES: &[&str] = &[
    "explicit_statement",
    "explicit_agreement",
    "observed_behavior",
    "roleplay_expression",
    "legacy_profile",
    "unknown",
];
pub const TEMPORALITIES: &[&str] = &["atemporal", "current", "past", "future", "unknown"];
pub const PERSISTENCES: &[&str] = &["stable", "ongoing", "transient", "one_off", "unknown"];
pub const CONFIDENCES: &[&str] = &["high", "medium", "low", "unknown"];

/// The listed value equal to `raw`, else `fallback`. Returns the list's own
/// `'static` copy so a label never borrows from the input.
fn pick(
    raw: Option<&str>,
    allowed: &'static [&'static str],
    fallback: &'static str,
) -> &'static str {
    raw.and_then(|r| allowed.iter().find(|a| **a == r).copied())
        .unwrap_or(fallback)
}

/// A domain from `DOMAINS`. Retired names fold into their successor
/// (`politics` and `religion` into `ideology`); `location_culture` and anything
/// unlisted become `other`.
pub fn fold_domain(raw: Option<&str>) -> &'static str {
    match raw {
        Some("politics" | "religion") => "ideology",
        other => pick(other, DOMAINS, "other"),
    }
}

/// One extracted fact with every label already inside the vocabulary.
#[derive(Debug, Clone, PartialEq)]
pub struct FactInput {
    pub content: String,
    pub category: String,
    pub domain: String,
    pub evidence_type: String,
    pub temporality: String,
    pub persistence: String,
    pub confidence: String,
}

impl FactInput {
    /// Parse one element of the extractor's fact array. `None` when it is not
    /// an object or its `content` is blank or a placeholder such as `未知` or an
    /// echoed `<…>` template slot; a missing or unlisted label folds to `other`
    /// (category, domain) or `unknown`, never rejects the fact.
    pub fn from_json(v: &serde_json::Value) -> Option<FactInput> {
        let obj = v.as_object()?;
        let content = obj.get("content")?.as_str()?.trim();
        if content.is_empty() || crate::human_insight::is_placeholder(content) {
            return None;
        }
        let label = |key: &str| obj.get(key).and_then(|x| x.as_str());
        Some(FactInput {
            content: content.to_string(),
            category: pick(label("category"), CATEGORIES, "other").to_string(),
            domain: fold_domain(label("domain")).to_string(),
            evidence_type: pick(label("evidence_type"), EVIDENCE_TYPES, "unknown").to_string(),
            temporality: pick(label("temporality"), TEMPORALITIES, "unknown").to_string(),
            persistence: pick(label("persistence"), PERSISTENCES, "unknown").to_string(),
            confidence: pick(label("confidence"), CONFIDENCES, "unknown").to_string(),
        })
    }
}

/// Where one extraction run saw its facts. Each id is `None` when the run has
/// no such anchor; a deleted session or message nulls the stored pointer.
#[derive(Debug, Clone)]
pub struct FactSource {
    pub session_id: Option<Uuid>,
    pub message_id: Option<Uuid>,
    pub run_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct HumanFactRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub content: String,
    pub category: String,
    pub domain: String,
    pub evidence_type: String,
    pub temporality: String,
    pub persistence: String,
    pub confidence: String,
    pub occurrences: i32,
    pub session_count: i32,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

/// How often a fact has come up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recurrence {
    Once,
    RepeatedInSession,
    RepeatedAcrossSessions,
}

impl Recurrence {
    pub fn as_str(&self) -> &'static str {
        match self {
            Recurrence::Once => "once",
            Recurrence::RepeatedInSession => "repeated_in_session",
            Recurrence::RepeatedAcrossSessions => "repeated_across_sessions",
        }
    }
}

impl HumanFactRow {
    pub fn recurrence(&self) -> Recurrence {
        if self.session_count >= 2 {
            Recurrence::RepeatedAcrossSessions
        } else if self.occurrences >= 2 {
            Recurrence::RepeatedInSession
        } else {
            Recurrence::Once
        }
    }
}

const COLUMNS: &str = "id, user_id, content, category, domain, evidence_type, temporality, \
                       persistence, confidence, occurrences, session_count, \
                       first_seen_at, last_seen_at";

pub struct HumanFactRepo<'a> {
    pub pool: &'a PgPool,
}

impl HumanFactRepo<'_> {
    /// Record one run's facts in a single statement and return how many rows it
    /// inserted or bumped. A fact already in the ledger (same user, same
    /// roleplay-ness, same normalised text) keeps its text, labels and first
    /// pointers; only its counters and `last_*` pointers move. `session_count`
    /// grows only when the run's session differs from the stored `last_session_id`.
    ///
    /// Facts within the batch that normalise to the same text count once, the
    /// first of them winning; a roleplay line and a real statement of the same
    /// words stay separate. The grouping uses the database's `fact_norm`, the
    /// same function the dedup index hashes, so the batch and the index cannot
    /// disagree. One statement means a failed run changes nothing, so a retry
    /// cannot count a fact twice.
    ///
    /// `first_seen_at` / `last_seen_at` are the database clock at the write, not
    /// the time the run started: the export keyset orders on `last_seen_at`, so a
    /// stamp older than the commit would let a consumer that already advanced
    /// past it skip the row for good. A recurrence never moves `last_seen_at`
    /// backwards.
    pub async fn upsert_run(
        &self,
        user_id: Uuid,
        facts: &[FactInput],
        src: &FactSource,
    ) -> Result<usize, sqlx::Error> {
        if facts.is_empty() {
            return Ok(0);
        }
        let contents: Vec<String> = facts.iter().map(|f| f.content.trim().to_string()).collect();
        let categories: Vec<String> = facts.iter().map(|f| f.category.clone()).collect();
        let domains: Vec<String> = facts.iter().map(|f| f.domain.clone()).collect();
        let evidence_types: Vec<String> = facts.iter().map(|f| f.evidence_type.clone()).collect();
        let temporalities: Vec<String> = facts.iter().map(|f| f.temporality.clone()).collect();
        let persistences: Vec<String> = facts.iter().map(|f| f.persistence.clone()).collect();
        let confidences: Vec<String> = facts.iter().map(|f| f.confidence.clone()).collect();

        let res = sqlx::query(
            "INSERT INTO engine.human_facts \
                (user_id, content, category, domain, evidence_type, temporality, \
                 persistence, confidence, first_seen_at, last_seen_at, \
                 first_session_id, first_message_id, first_run_id, \
                 last_session_id, last_message_id, last_run_id) \
             SELECT $1, content, category, domain, evidence_type, temporality, \
                    persistence, confidence, now(), now(), $9, $10, $11, $9, $10, $11 \
             FROM ( \
                 SELECT DISTINCT ON ((evidence_type = 'roleplay_expression'), \
                                     md5(engine.fact_norm(content))) \
                        content, category, domain, evidence_type, temporality, \
                        persistence, confidence \
                 FROM unnest($2::text[], $3::text[], $4::text[], $5::text[], \
                             $6::text[], $7::text[], $8::text[]) \
                      WITH ORDINALITY AS t(content, category, domain, evidence_type, \
                                           temporality, persistence, confidence, ord) \
                 ORDER BY (evidence_type = 'roleplay_expression'), \
                          md5(engine.fact_norm(content)), ord \
             ) AS batch \
             ON CONFLICT (user_id, roleplay, content_hash) DO UPDATE SET \
                occurrences     = human_facts.occurrences + 1, \
                session_count   = human_facts.session_count + CASE \
                    WHEN human_facts.last_session_id IS DISTINCT FROM EXCLUDED.last_session_id \
                    THEN 1 ELSE 0 END, \
                last_seen_at    = GREATEST(human_facts.last_seen_at, now()), \
                last_session_id = EXCLUDED.last_session_id, \
                last_message_id = EXCLUDED.last_message_id, \
                last_run_id     = EXCLUDED.last_run_id",
        )
        .bind(user_id)
        .bind(&contents)
        .bind(&categories)
        .bind(&domains)
        .bind(&evidence_types)
        .bind(&temporalities)
        .bind(&persistences)
        .bind(&confidences)
        .bind(src.session_id)
        .bind(src.message_id)
        .bind(src.run_id)
        .execute(self.pool)
        .await?;
        Ok(res.rows_affected() as usize)
    }

    pub async fn list_for_user(&self, user_id: Uuid) -> Result<Vec<HumanFactRow>, sqlx::Error> {
        sqlx::query_as::<_, HumanFactRow>(&format!(
            "SELECT {COLUMNS} FROM engine.human_facts WHERE user_id = $1 \
             ORDER BY occurrences DESC, last_seen_at DESC"
        ))
        .bind(user_id)
        .fetch_all(self.pool)
        .await
    }

    /// Rows changed strictly after the `(since, after_id)` keyset tuple, in
    /// `(last_seen_at, id)` order, at most `limit`. Backs the facts export
    /// stream; the tuple comparison keeps rows sharing one timestamp from being
    /// skipped or repeated across a page boundary. Uses `idx_human_facts_export`.
    pub async fn list_changes(
        &self,
        since: DateTime<Utc>,
        after_id: Uuid,
        limit: i64,
    ) -> Result<Vec<HumanFactRow>, sqlx::Error> {
        sqlx::query_as::<_, HumanFactRow>(&format!(
            "SELECT {COLUMNS} FROM engine.human_facts \
             WHERE (last_seen_at, id) > ($1, $2) \
             ORDER BY last_seen_at, id \
             LIMIT $3"
        ))
        .bind(since)
        .bind(after_id)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap()
    }

    fn fact(content: &str, evidence_type: &str, persistence: &str) -> FactInput {
        FactInput {
            content: content.into(),
            category: "preference".into(),
            domain: "hobby".into(),
            evidence_type: evidence_type.into(),
            temporality: "current".into(),
            persistence: persistence.into(),
            confidence: "high".into(),
        }
    }

    fn src(session_id: Option<Uuid>, message_id: Option<Uuid>, run_id: Option<Uuid>) -> FactSource {
        FactSource {
            session_id,
            message_id,
            run_id,
        }
    }

    /// A ledger stamp is the database clock at the write: close to now, and
    /// `first_seen_at` never after `last_seen_at`.
    fn assert_stamped_now(r: &HumanFactRow) {
        let now = Utc::now();
        assert!(r.first_seen_at <= r.last_seen_at, "{r:?}");
        for t in [r.first_seen_at, r.last_seen_at] {
            assert!(
                (now - t).num_seconds().abs() < 30,
                "{t} is not a write-time stamp (now {now})"
            );
        }
    }

    fn row(occurrences: i32, session_count: i32) -> HumanFactRow {
        HumanFactRow {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            content: "x".into(),
            category: "fact".into(),
            domain: "other".into(),
            evidence_type: "unknown".into(),
            temporality: "unknown".into(),
            persistence: "unknown".into(),
            confidence: "unknown".into(),
            occurrences,
            session_count,
            first_seen_at: at(0),
            last_seen_at: at(0),
        }
    }

    #[test]
    fn fact_input_folds_vocab() {
        let f = FactInput::from_json(&serde_json::json!({
            "content": "  用户关注大选  ",
            "category": "banana",
            "domain": "politics",
            "evidence_type": "explicit_statement",
            "temporality": "current",
            "persistence": "stable"
        }))
        .unwrap();
        assert_eq!(f.content, "用户关注大选");
        assert_eq!(f.category, "other");
        assert_eq!(f.domain, "ideology");
        assert_eq!(f.evidence_type, "explicit_statement");
        assert_eq!(f.temporality, "current");
        assert_eq!(f.persistence, "stable");
        assert_eq!(f.confidence, "unknown", "missing label folds to unknown");

        let g = FactInput::from_json(&serde_json::json!({
            "content": "x", "category": "habit", "domain": "location_culture",
            "evidence_type": "nope", "temporality": 3, "persistence": null,
            "confidence": "medium"
        }))
        .unwrap();
        assert_eq!(g.category, "habit");
        assert_eq!(g.domain, "other");
        assert_eq!(g.evidence_type, "unknown");
        assert_eq!(g.temporality, "unknown");
        assert_eq!(g.persistence, "unknown");
        assert_eq!(g.confidence, "medium");
    }

    #[test]
    fn fold_domain_maps_retired_and_unknown_values() {
        assert_eq!(fold_domain(Some("religion")), "ideology");
        assert_eq!(fold_domain(Some("politics")), "ideology");
        assert_eq!(fold_domain(Some("location_culture")), "other");
        assert_eq!(fold_domain(Some("career")), "career");
        assert_eq!(fold_domain(Some("banana")), "other");
        assert_eq!(fold_domain(None), "other");
    }

    #[test]
    fn fact_input_rejects_blank_content() {
        assert!(FactInput::from_json(&serde_json::json!({ "content": "  " })).is_none());
        assert!(FactInput::from_json(&serde_json::json!({ "category": "fact" })).is_none());
        assert!(FactInput::from_json(&serde_json::json!({ "content": 7 })).is_none());
        assert!(FactInput::from_json(&serde_json::json!("用户喜欢爬山")).is_none());
        assert!(FactInput::from_json(&serde_json::json!(["a"])).is_none());
        for slot in ["<用户事实>", "未知", "string"] {
            assert!(
                FactInput::from_json(&serde_json::json!({ "content": slot })).is_none(),
                "{slot:?} is a placeholder, not a fact"
            );
        }
    }

    #[test]
    fn recurrence_tiers() {
        assert_eq!(row(1, 1).recurrence(), Recurrence::Once);
        assert_eq!(row(3, 1).recurrence(), Recurrence::RepeatedInSession);
        assert_eq!(row(2, 2).recurrence(), Recurrence::RepeatedAcrossSessions);
        assert_eq!(Recurrence::Once.as_str(), "once");
        assert_eq!(
            Recurrence::RepeatedInSession.as_str(),
            "repeated_in_session"
        );
        assert_eq!(
            Recurrence::RepeatedAcrossSessions.as_str(),
            "repeated_across_sessions"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_merges_exact_normalised_duplicates_and_counts(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let s1 = crate::testutil::seed_chat_session(&pool, user).await;
        let s2 = crate::testutil::seed_chat_session(&pool, user).await;
        let m1 = crate::testutil::seed_chat_message(&pool, s1).await;
        let m2 = crate::testutil::seed_chat_message(&pool, s2).await;
        let (r1, r2) = (Uuid::new_v4(), Uuid::new_v4());

        let n = repo
            .upsert_run(
                user,
                &[fact("用户喜欢爬山。", "explicit_statement", "stable")],
                &src(Some(s1), Some(m1), Some(r1)),
            )
            .await
            .unwrap();
        assert_eq!(n, 1);
        let before = repo.list_for_user(user).await.unwrap().remove(0);
        assert_stamped_now(&before);
        assert_eq!(before.first_seen_at, before.last_seen_at);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let n = repo
            .upsert_run(
                user,
                &[fact("用户喜欢爬山", "explicit_statement", "transient")],
                &src(Some(s2), Some(m2), Some(r2)),
            )
            .await
            .unwrap();
        assert_eq!(n, 1);

        let rows = repo.list_for_user(user).await.unwrap();
        assert_eq!(rows.len(), 1, "punctuation-only variants share one row");
        let r = &rows[0];
        assert_eq!(r.content, "用户喜欢爬山。", "first occurrence's text wins");
        assert_eq!(r.persistence, "stable", "first occurrence's labels win");
        assert_eq!(r.occurrences, 2);
        assert_eq!(r.session_count, 2);
        assert_stamped_now(r);
        assert_eq!(
            r.first_seen_at, before.first_seen_at,
            "a recurrence keeps first_seen_at"
        );
        assert!(
            r.last_seen_at > before.last_seen_at,
            "a recurrence moves last_seen_at forward to the write"
        );
        assert_eq!(r.recurrence(), Recurrence::RepeatedAcrossSessions);

        #[allow(clippy::type_complexity)]
        let p: (
            Option<Uuid>,
            Option<Uuid>,
            Option<Uuid>,
            Option<Uuid>,
            Option<Uuid>,
            Option<Uuid>,
        ) = sqlx::query_as(
            "SELECT first_session_id, first_message_id, first_run_id, \
                    last_session_id, last_message_id, last_run_id \
             FROM engine.human_facts WHERE id = $1",
        )
        .bind(r.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            p,
            (Some(s1), Some(m1), Some(r1), Some(s2), Some(m2), Some(r2))
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_counts_a_repeat_in_the_same_session_once_per_session(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let s1 = crate::testutil::seed_chat_session(&pool, user).await;
        let f = [fact("用户养了一只猫", "explicit_statement", "stable")];
        repo.upsert_run(user, &f, &src(Some(s1), None, None))
            .await
            .unwrap();
        repo.upsert_run(user, &f, &src(Some(s1), None, None))
            .await
            .unwrap();
        let rows = repo.list_for_user(user).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].occurrences, rows[0].session_count), (2, 1));
        assert_eq!(rows[0].recurrence(), Recurrence::RepeatedInSession);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_stamps_at_write_time_and_never_moves_last_seen_backwards(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let f = [fact("用户养了一只猫", "explicit_statement", "stable")];
        repo.upsert_run(user, &f, &src(None, None, None))
            .await
            .unwrap();
        let first = repo.list_for_user(user).await.unwrap().remove(0);
        assert_stamped_now(&first);

        // A row stamped ahead of the database clock (a backfilled or skewed
        // value) keeps its later stamp when the fact recurs.
        let ahead = first.last_seen_at + chrono::Duration::days(1);
        sqlx::query("UPDATE engine.human_facts SET last_seen_at = $1 WHERE id = $2")
            .bind(ahead)
            .bind(first.id)
            .execute(&pool)
            .await
            .unwrap();
        repo.upsert_run(user, &f, &src(None, None, None))
            .await
            .unwrap();
        let again = repo.list_for_user(user).await.unwrap().remove(0);
        assert_eq!(again.occurrences, 2);
        assert_eq!(again.last_seen_at, ahead);
        assert_eq!(again.first_seen_at, first.first_seen_at);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_keeps_variants_apart(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let facts = [
            fact("用户喜欢爬山", "explicit_statement", "stable"),
            fact("用户不喜欢爬山", "explicit_statement", "stable"),
            fact("用户很喜欢爬山", "explicit_statement", "stable"),
        ];
        let n = repo
            .upsert_run(user, &facts, &src(None, None, None))
            .await
            .unwrap();
        assert_eq!(n, 3);
        assert_eq!(repo.list_for_user(user).await.unwrap().len(), 3);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_never_merges_roleplay_with_real(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        // Same run, then a later run: neither path may merge the two kinds.
        repo.upsert_run(
            user,
            &[
                fact("用户是一名医生", "roleplay_expression", "one_off"),
                fact("用户是一名医生", "explicit_statement", "stable"),
            ],
            &src(None, None, None),
        )
        .await
        .unwrap();
        repo.upsert_run(
            user,
            &[fact("用户是一名医生", "roleplay_expression", "one_off")],
            &src(None, None, None),
        )
        .await
        .unwrap();
        let rows = repo.list_for_user(user).await.unwrap();
        assert_eq!(rows.len(), 2);
        let role = rows
            .iter()
            .find(|r| r.evidence_type == "roleplay_expression")
            .unwrap();
        let real = rows
            .iter()
            .find(|r| r.evidence_type == "explicit_statement")
            .unwrap();
        assert_eq!((role.occurrences, real.occurrences), (2, 1));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_dedupes_within_one_run(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let n = repo
            .upsert_run(
                user,
                &[
                    fact("用户喜欢爬山", "explicit_statement", "stable"),
                    fact(" 用户喜欢爬山 ", "explicit_statement", "transient"),
                ],
                &src(None, None, None),
            )
            .await
            .unwrap();
        assert_eq!(n, 1);
        let rows = repo.list_for_user(user).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].occurrences, 1);
        assert_eq!(rows[0].persistence, "stable", "first of the run is kept");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_dedupes_punctuation_variants_within_one_run(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let n = repo
            .upsert_run(
                user,
                &[
                    fact("用户喜欢爬山。", "explicit_statement", "stable"),
                    fact("用户喜欢爬山", "explicit_statement", "transient"),
                ],
                &src(None, None, None),
            )
            .await
            .unwrap();
        assert_eq!(n, 1, "one fact seen once is one row written");
        let rows = repo.list_for_user(user).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].occurrences, 1);
        assert_eq!(rows[0].content, "用户喜欢爬山。", "first of the batch wins");
        assert_eq!(rows[0].persistence, "stable");
        assert_eq!(rows[0].recurrence(), Recurrence::Once);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_returns_rows_written_or_bumped(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        repo.upsert_run(
            user,
            &[fact("用户喜欢爬山", "explicit_statement", "stable")],
            &src(None, None, None),
        )
        .await
        .unwrap();
        let n = repo
            .upsert_run(
                user,
                &[
                    fact("用户喜欢爬山", "explicit_statement", "stable"),
                    fact("用户在深圳工作", "explicit_statement", "stable"),
                    fact("用户有一个妹妹", "explicit_statement", "stable"),
                ],
                &src(None, None, None),
            )
            .await
            .unwrap();
        assert_eq!(n, 3, "one bump plus two inserts");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn pointers_null_when_session_deleted(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let s = crate::testutil::seed_chat_session(&pool, user).await;
        let m = crate::testutil::seed_chat_message(&pool, s).await;
        repo.upsert_run(
            user,
            &[fact("用户喜欢爬山", "explicit_statement", "stable")],
            &src(Some(s), Some(m), Some(Uuid::new_v4())),
        )
        .await
        .unwrap();

        sqlx::query("DELETE FROM engine.chat_sessions WHERE id = $1")
            .bind(s)
            .execute(&pool)
            .await
            .unwrap();

        let (fs, fm, ls, lm): (Option<Uuid>, Option<Uuid>, Option<Uuid>, Option<Uuid>) =
            sqlx::query_as(
                "SELECT first_session_id, first_message_id, last_session_id, last_message_id \
                 FROM engine.human_facts WHERE user_id = $1",
            )
            .bind(user)
            .fetch_one(&pool)
            .await
            .expect("the row outlives its session");
        assert_eq!((fs, fm, ls, lm), (None, None, None, None));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn list_changes_keyset_pages_without_skip_or_repeat(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        // One statement, one database clock reading: all three rows tie on
        // last_seen_at.
        repo.upsert_run(
            user,
            &[
                fact("用户喜欢爬山", "explicit_statement", "stable"),
                fact("用户在深圳工作", "explicit_statement", "stable"),
                fact("用户有一个妹妹", "explicit_statement", "stable"),
            ],
            &src(None, None, None),
        )
        .await
        .unwrap();

        let mut expected: Vec<Uuid> = repo
            .list_for_user(user)
            .await
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        expected.sort();

        let mut walked = Vec::new();
        let mut cursor = (DateTime::<Utc>::UNIX_EPOCH, Uuid::nil());
        loop {
            let page = repo.list_changes(cursor.0, cursor.1, 2).await.unwrap();
            walked.extend(page.iter().map(|r| r.id));
            if page.len() < 2 {
                break;
            }
            let last = page.last().unwrap();
            cursor = (last.last_seen_at, last.id);
        }
        assert_eq!(
            walked, expected,
            "every row once, in (last_seen_at, id) order"
        );
    }
}
