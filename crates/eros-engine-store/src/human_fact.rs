// SPDX-License-Identifier: AGPL-3.0-only
//! Ledger of distinct facts mined about a user (`engine.human_facts`): one row
//! per fact, counted again each time the extractor sees it. Labels use a closed
//! vocabulary validated here; the table keeps them as free text so a retired
//! value never blocks a write.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
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
    /// The fact's group: shared by the user's rows that state the same thing.
    pub group_id: Uuid,
}

/// How often a fact has come up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recurrence {
    Once,
    RepeatedInSession,
    RepeatedAcrossSessions,
}

impl Recurrence {
    /// The tier of a fact (or a group) seen `occurrences` times in
    /// `session_count` sessions.
    pub fn from_counts(occurrences: i32, session_count: i32) -> Recurrence {
        if session_count >= 2 {
            Recurrence::RepeatedAcrossSessions
        } else if occurrences >= 2 {
            Recurrence::RepeatedInSession
        } else {
            Recurrence::Once
        }
    }

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
        Recurrence::from_counts(self.occurrences, self.session_count)
    }
}

/// One fact group of a user: the rows sharing a `group_id`, representative
/// first. Every group attribute is derived from the rows here; none is
/// stored.
#[derive(Debug, Clone)]
pub struct FactGroup {
    pub group_id: Uuid,
    /// Non-empty. Ordered by occurrences (most first), then `first_seen_at`,
    /// then `id`, so `facts[0]` is the representative.
    pub facts: Vec<HumanFactRow>,
}

impl FactGroup {
    /// Every member shares it: placement never crosses the realm.
    pub fn roleplay(&self) -> bool {
        self.facts[0].evidence_type == "roleplay_expression"
    }

    /// Summed over members, so a run that emitted two members counts twice.
    pub fn occurrences(&self) -> i32 {
        self.facts.iter().map(|f| f.occurrences).sum()
    }

    /// Summed over members, so a session that saw two members counts twice.
    pub fn session_count(&self) -> i32 {
        self.facts.iter().map(|f| f.session_count).sum()
    }

    pub fn recurrence(&self) -> Recurrence {
        Recurrence::from_counts(self.occurrences(), self.session_count())
    }

    pub fn first_seen_at(&self) -> DateTime<Utc> {
        self.facts
            .iter()
            .map(|f| f.first_seen_at)
            .min()
            .expect("non-empty")
    }

    pub fn last_seen_at(&self) -> DateTime<Utc> {
        self.facts
            .iter()
            .map(|f| f.last_seen_at)
            .max()
            .expect("non-empty")
    }
}

/// Hand the user's profile to the next rebuild after a deletion (spec
/// 2026-11-10-row-deletion §5.2): the state row is upserted so it exists and
/// is locked, any in-flight rebuild loses its claim, and the profile is due.
async fn mark_profile_due(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO engine.human_profile_state \
            (user_id, pending_facts, last_fact_at, rebuild_due) \
         VALUES ($1, 1, now(), true) \
         ON CONFLICT (user_id) DO UPDATE SET \
            claimed_at = NULL, \
            pending_facts = human_profile_state.pending_facts + 1, \
            last_fact_at = now(), \
            rebuild_failures = 0, \
            rebuild_due = true",
    )
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// A ledger row the fact-group sweeper has not placed yet (`embedding IS NULL`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UnplacedFact {
    pub id: Uuid,
    pub user_id: Uuid,
    pub roleplay: bool,
    pub content: String,
}

/// What placing one row did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// It took the group of an earlier fact it restates.
    Joined,
    /// It restates no earlier fact closely enough and stays its own group.
    Alone,
    /// The row was gone or already placed; nothing was written.
    Skipped,
}

/// Serialise every change to one user's fact groups: placement and the group
/// delete take this lock first, inside their transaction.
pub(crate) async fn lock_user_groups(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('fact_groups:' || $1::text, 0))")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

const COLUMNS: &str = "id, user_id, content, category, domain, evidence_type, temporality, \
                       persistence, confidence, occurrences, session_count, \
                       first_seen_at, last_seen_at, group_id";

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
    /// An erased user (`engine.user_erasures`) gets no fact: the statement
    /// writes nothing and the count is 0.
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
             WHERE NOT EXISTS (SELECT 1 FROM engine.user_erasures WHERE user_id = $1) \
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

    /// Up to `limit` unplaced rows across all users, oldest first by
    /// `(first_seen_at, id)`: the fact-group sweeper's queue (spec
    /// 2026-11-25-fact-groups §5.2). Uses `idx_human_facts_unplaced`.
    pub async fn unplaced(&self, limit: i64) -> Result<Vec<UnplacedFact>, sqlx::Error> {
        sqlx::query_as::<_, UnplacedFact>(
            "SELECT id, user_id, roleplay, content FROM engine.human_facts \
             WHERE embedding IS NULL \
             ORDER BY first_seen_at, id \
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }

    /// Place one row with its vector (spec 2026-11-25-fact-groups §5.2), in
    /// its own transaction under the user's group lock. A group's anchor is
    /// its earliest placed member by `(first_seen_at, id)`; the row takes the
    /// group of the nearest anchor among the user's placed rows of the same
    /// realm when their cosine similarity is at least `threshold`, and keeps
    /// its own group otherwise. Comparing with anchors only keeps a group from
    /// growing by chains of near neighbours. The vector and the group are
    /// written together, and only while the row is still unplaced.
    pub async fn place(
        &self,
        fact: &UnplacedFact,
        embedding: &[f32],
        threshold: f64,
    ) -> Result<Placement, sqlx::Error> {
        let vector = crate::memory::format_vector(embedding);
        let mut tx = self.pool.begin().await?;
        lock_user_groups(&mut tx, fact.user_id).await?;
        let nearest: Option<(Uuid, f64)> = sqlx::query_as(
            "SELECT group_id, 1 - (embedding <=> $3::vector) \
             FROM (SELECT DISTINCT ON (group_id) group_id, embedding \
                   FROM engine.human_facts \
                   WHERE user_id = $1 AND roleplay = $2 AND embedding IS NOT NULL \
                   ORDER BY group_id, first_seen_at, id) AS anchors \
             ORDER BY embedding <=> $3::vector \
             LIMIT 1",
        )
        .bind(fact.user_id)
        .bind(fact.roleplay)
        .bind(&vector)
        .fetch_optional(&mut *tx)
        .await?;
        let joined = nearest
            .filter(|(_, similarity)| *similarity >= threshold)
            .map(|(group_id, _)| group_id);
        let written = sqlx::query(
            "UPDATE engine.human_facts \
             SET embedding = $2::vector, group_id = COALESCE($3, group_id) \
             WHERE id = $1 AND embedding IS NULL",
        )
        .bind(fact.id)
        .bind(&vector)
        .bind(joined)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(match (written, joined) {
            (0, _) => Placement::Skipped,
            (_, Some(_)) => Placement::Joined,
            (_, None) => Placement::Alone,
        })
    }

    /// The user's facts, newest first by `first_seen_at` (which never
    /// changes, so a walk through the pages neither repeats nor skips a row),
    /// strictly before the `before` keyset tuple when given.
    pub async fn list_owned(
        &self,
        user_id: Uuid,
        before: Option<(DateTime<Utc>, Uuid)>,
        limit: i64,
    ) -> Result<Vec<HumanFactRow>, sqlx::Error> {
        let (at, id) = before.unzip();
        sqlx::query_as::<_, HumanFactRow>(&format!(
            "SELECT {COLUMNS} FROM engine.human_facts \
             WHERE user_id = $1 AND ($2::timestamptz IS NULL OR (first_seen_at, id) < ($2, $3)) \
             ORDER BY first_seen_at DESC, id DESC \
             LIMIT $4"
        ))
        .bind(user_id)
        .bind(at)
        .bind(id)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }

    /// Delete one of the user's facts (spec 2026-11-10-row-deletion §3.2,
    /// §5.2). One transaction: the user's fact-group lock first, so a
    /// placement never joins a group through an anchor this delete removes
    /// mid-placement; then, in the lock order user erasure uses, the user's
    /// profile state row, upserted so it exists and is locked (any in-flight
    /// rebuild loses its claim; the profile is marked due), then the fact,
    /// then its tombstone. `false` when the user has no such fact; nothing is
    /// written then.
    pub async fn delete_owned(&self, user_id: Uuid, fact_id: Uuid) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        lock_user_groups(&mut tx, user_id).await?;
        mark_profile_due(&mut tx, user_id).await?;
        let deleted = sqlx::query("DELETE FROM engine.human_facts WHERE id = $1 AND user_id = $2")
            .bind(fact_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if deleted == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        crate::row_deletion::RowDeletionRepo::insert_in_tx(
            &mut tx,
            crate::row_deletion::KIND_FACT,
            fact_id,
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// The user's fact groups (spec 2026-11-25-fact-groups §5.4), newest
    /// first by the group's `first_seen_at` (its earliest member's), then
    /// `group_id` descending, strictly before the `before` keyset tuple
    /// `(group first_seen_at, group_id)` when given, at most `limit` groups.
    /// An unplaced row is a one-row group.
    ///
    /// One statement, so the page and its rows come from one snapshot: a
    /// placement or a delete landing mid-read can neither empty a group on
    /// the page (which would cut the page short and end a client's walk) nor
    /// move a group's sort key away from the cursor built from it.
    pub async fn list_groups_owned(
        &self,
        user_id: Uuid,
        before: Option<(DateTime<Utc>, Uuid)>,
        limit: i64,
    ) -> Result<Vec<FactGroup>, sqlx::Error> {
        let (at, id) = before.unzip();
        let rows = sqlx::query_as::<_, HumanFactRow>(&format!(
            "WITH page AS ( \
                 SELECT group_id AS page_group, min(first_seen_at) AS page_first_seen \
                 FROM engine.human_facts WHERE user_id = $1 \
                 GROUP BY group_id \
                 HAVING $2::timestamptz IS NULL \
                        OR (min(first_seen_at), group_id) < ($2, $3) \
                 ORDER BY page_first_seen DESC, page_group DESC \
                 LIMIT $4) \
             SELECT {COLUMNS} FROM page \
             JOIN engine.human_facts ON user_id = $1 AND group_id = page.page_group \
             ORDER BY page_first_seen DESC, page_group DESC, \
                      occurrences DESC, first_seen_at, id"
        ))
        .bind(user_id)
        .bind(at)
        .bind(id)
        .bind(limit)
        .fetch_all(self.pool)
        .await?;
        // Rows arrive group by group, in page order.
        let mut groups: Vec<FactGroup> = Vec::new();
        for row in rows {
            match groups.last_mut() {
                Some(g) if g.group_id == row.group_id => g.facts.push(row),
                _ => groups.push(FactGroup {
                    group_id: row.group_id,
                    facts: vec![row],
                }),
            }
        }
        Ok(groups)
    }

    /// Delete one of the user's fact groups as it stands (spec
    /// 2026-11-25-fact-groups §5.5). One transaction: the user's group lock
    /// first, so no row joins the group mid-delete; then the profile state,
    /// as the single-fact delete marks it; then the rows; then one tombstone
    /// per row. `false` when no fact of this user has that group id; nothing
    /// is written then.
    pub async fn delete_group_owned(
        &self,
        user_id: Uuid,
        group_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        lock_user_groups(&mut tx, user_id).await?;
        mark_profile_due(&mut tx, user_id).await?;
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "DELETE FROM engine.human_facts WHERE user_id = $1 AND group_id = $2 RETURNING id",
        )
        .bind(user_id)
        .bind(group_id)
        .fetch_all(&mut *tx)
        .await?;
        if ids.is_empty() {
            tx.rollback().await?;
            return Ok(false);
        }
        crate::row_deletion::RowDeletionRepo::insert_many_in_tx(
            &mut tx,
            crate::row_deletion::KIND_FACT,
            &ids,
        )
        .await?;
        tx.commit().await?;
        Ok(true)
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
            group_id: Uuid::new_v4(),
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
    async fn upsert_run_writes_nothing_for_an_erased_user(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let (erased, other) = (Uuid::new_v4(), Uuid::new_v4());
        crate::testutil::seed_erasure(&pool, erased).await;
        let facts = [fact("用户喜欢爬山", "explicit_statement", "stable")];
        let none = src(None, None, None);

        assert_eq!(repo.upsert_run(erased, &facts, &none).await.unwrap(), 0);
        assert!(repo.list_for_user(erased).await.unwrap().is_empty());

        assert_eq!(repo.upsert_run(other, &facts, &none).await.unwrap(), 1);
        assert_eq!(repo.list_for_user(other).await.unwrap().len(), 1);
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

    // ── The owner's list and delete (spec 2026-11-10-row-deletion) ──

    async fn seed_at(pool: &PgPool, user: Uuid, content: &str, first_seen: DateTime<Utc>) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.human_facts \
               (user_id, content, category, domain, evidence_type, temporality, \
                persistence, confidence, first_seen_at, last_seen_at) \
             VALUES ($1, $2, 'other', 'other', 'explicit_statement', 'current', 'stable', \
                     'high', $3, $3) RETURNING id",
        )
        .bind(user)
        .bind(content)
        .bind(first_seen)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn list_owned_pages_newest_first_by_first_seen(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let (user, other) = (Uuid::new_v4(), Uuid::new_v4());
        let t = |h| Utc.with_ymd_and_hms(2026, 11, 10, h, 0, 0).unwrap();
        let old = seed_at(&pool, user, "是工程师", t(1)).await;
        let mid = seed_at(&pool, user, "喜欢爬山", t(2)).await;
        let new = seed_at(&pool, user, "养了一只猫", t(3)).await;
        seed_at(&pool, other, "别人的事", t(4)).await;
        // A recurrence moves last_seen_at, not the list order.
        sqlx::query("UPDATE engine.human_facts SET last_seen_at = $2 WHERE id = $1")
            .bind(old)
            .bind(t(9))
            .execute(&pool)
            .await
            .unwrap();

        let page = repo.list_owned(user, None, 2).await.unwrap();
        assert_eq!(
            page.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![new, mid]
        );
        let last = page.last().unwrap();
        let rest = repo
            .list_owned(user, Some((last.first_seen_at, last.id)), 2)
            .await
            .unwrap();
        assert_eq!(rest.iter().map(|r| r.id).collect::<Vec<_>>(), vec![old]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn delete_owned_removes_tombstones_and_marks_the_profile_due(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let (user, other) = (Uuid::new_v4(), Uuid::new_v4());
        let t = Utc.with_ymd_and_hms(2026, 11, 10, 8, 0, 0).unwrap();
        let mine = seed_at(&pool, user, "是工程师", t).await;
        let theirs = seed_at(&pool, other, "是工程师", t).await;
        let states = crate::profile_state::ProfileStateRepo { pool: &pool };
        states.bump(user, 2).await.unwrap();
        let claimed = states
            .claim(user, std::time::Duration::from_secs(600))
            .await
            .unwrap()
            .expect("an in-flight rebuild");
        sqlx::query(
            "UPDATE engine.human_profile_state SET rebuild_failures = 2 WHERE user_id = $1",
        )
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();

        assert!(
            !repo.delete_owned(user, theirs).await.unwrap(),
            "not the owner's"
        );
        assert!(repo.delete_owned(user, mine).await.unwrap());
        assert!(
            !repo.delete_owned(user, mine).await.unwrap(),
            "already gone"
        );

        let left: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM engine.human_facts")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, vec![theirs]);
        let tombs: Vec<(String, Uuid)> =
            sqlx::query_as("SELECT kind, id FROM engine.row_deletions")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(tombs, vec![("fact".to_string(), mine)]);

        let s = states.load(user).await.unwrap().unwrap();
        assert_eq!(s.claimed_at, None, "the in-flight rebuild lost its claim");
        assert_eq!(s.pending_facts, 3);
        assert_eq!(s.rebuild_failures, 0);
        assert!(s.rebuild_due);
        states.finish_ok(user, claimed).await.unwrap();
        assert_eq!(
            states.load(user).await.unwrap().unwrap().pending_facts,
            3,
            "the old claim settles nothing"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn delete_owned_creates_a_due_state_row_and_a_miss_leaves_none(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let states = crate::profile_state::ProfileStateRepo { pool: &pool };

        assert!(!repo.delete_owned(user, Uuid::new_v4()).await.unwrap());
        assert!(
            states.load(user).await.unwrap().is_none(),
            "a miss rolls back"
        );

        let id = seed_at(&pool, user, "是工程师", Utc::now()).await;
        assert!(repo.delete_owned(user, id).await.unwrap());
        let s = states.load(user).await.unwrap().expect("created");
        assert_eq!((s.pending_facts, s.rebuild_due), (1, true));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_restated_fact_after_a_delete_is_a_new_row(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let none = src(None, None, None);
        repo.upsert_run(
            user,
            &[fact("用户喜欢爬山", "explicit_statement", "stable")],
            &none,
        )
        .await
        .unwrap();
        let first = repo.list_for_user(user).await.unwrap()[0].id;
        assert!(repo.delete_owned(user, first).await.unwrap());
        repo.upsert_run(
            user,
            &[fact("用户喜欢爬山", "explicit_statement", "stable")],
            &none,
        )
        .await
        .unwrap();
        let again = repo.list_for_user(user).await.unwrap();
        assert_eq!(again.len(), 1);
        assert_ne!(again[0].id, first);
        assert_eq!(again[0].occurrences, 1, "a fresh count");
    }

    // ── Fact groups (spec 2026-11-25-fact-groups) ──

    #[sqlx::test(migrations = "./migrations")]
    async fn a_new_row_is_its_own_unplaced_group(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        repo.upsert_run(
            user,
            &[
                fact("用户喜欢爬山", "explicit_statement", "stable"),
                fact("用户在深圳工作", "explicit_statement", "stable"),
            ],
            &src(None, None, None),
        )
        .await
        .unwrap();
        let rows = repo.list_for_user(user).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].group_id, rows[1].group_id, "each row starts alone");
        let unplaced: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.human_facts WHERE user_id = $1 AND embedding IS NULL",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(unplaced, 2);
    }

    const T: f64 = 0.96;

    /// A 512-dimension unit vector at `deg` degrees in the first plane: the
    /// cosine similarity of `unit(a)` and `unit(b)` is `cos(a − b)`.
    fn unit(deg: f64) -> Vec<f32> {
        let mut v = vec![0.0_f32; 512];
        v[0] = deg.to_radians().cos() as f32;
        v[1] = deg.to_radians().sin() as f32;
        v
    }

    fn h(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 25, hour, 0, 0).unwrap()
    }

    async fn seed_unplaced(
        pool: &PgPool,
        user: Uuid,
        content: &str,
        evidence_type: &str,
        first_seen: DateTime<Utc>,
    ) -> UnplacedFact {
        let id = sqlx::query_scalar(
            "INSERT INTO engine.human_facts \
               (user_id, content, category, domain, evidence_type, temporality, \
                persistence, confidence, first_seen_at, last_seen_at) \
             VALUES ($1, $2, 'other', 'other', $3, 'current', 'stable', 'high', $4, $4) \
             RETURNING id",
        )
        .bind(user)
        .bind(content)
        .bind(evidence_type)
        .bind(first_seen)
        .fetch_one(pool)
        .await
        .unwrap();
        UnplacedFact {
            id,
            user_id: user,
            roleplay: evidence_type == "roleplay_expression",
            content: content.into(),
        }
    }

    async fn group_of(pool: &PgPool, id: Uuid) -> Uuid {
        sqlx::query_scalar("SELECT group_id FROM engine.human_facts WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn joins_at_or_above_the_threshold(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let a = seed_unplaced(&pool, user, "用户喜欢爬山", "explicit_statement", h(1)).await;
        let b = seed_unplaced(&pool, user, "用户很喜欢爬山", "explicit_statement", h(2)).await;
        assert_eq!(
            repo.place(&a, &unit(0.0), T).await.unwrap(),
            Placement::Alone
        );
        assert_eq!(
            repo.place(&b, &unit(10.0), T).await.unwrap(),
            Placement::Joined
        );
        assert_eq!(group_of(&pool, b.id).await, group_of(&pool, a.id).await);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn stays_alone_below_the_threshold(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let a = seed_unplaced(&pool, user, "用户喜欢爬山", "explicit_statement", h(1)).await;
        let b = seed_unplaced(&pool, user, "用户喜欢露营", "explicit_statement", h(2)).await;
        let b_group = group_of(&pool, b.id).await;
        assert_eq!(
            repo.place(&a, &unit(0.0), T).await.unwrap(),
            Placement::Alone
        );
        assert_eq!(
            repo.place(&b, &unit(20.0), T).await.unwrap(),
            Placement::Alone
        );
        assert_ne!(group_of(&pool, b.id).await, group_of(&pool, a.id).await);
        assert_eq!(
            group_of(&pool, b.id).await,
            b_group,
            "alone keeps its own id"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn matches_anchors_only(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let a = seed_unplaced(&pool, user, "a", "explicit_statement", h(1)).await;
        let b = seed_unplaced(&pool, user, "b", "explicit_statement", h(2)).await;
        let c = seed_unplaced(&pool, user, "c", "explicit_statement", h(3)).await;
        repo.place(&a, &unit(0.0), T).await.unwrap();
        assert_eq!(
            repo.place(&b, &unit(12.0), T).await.unwrap(),
            Placement::Joined
        );
        // cos 12° = 0.978 to B, but cos 24° = 0.914 to the group's first fact.
        assert_eq!(
            repo.place(&c, &unit(24.0), T).await.unwrap(),
            Placement::Alone
        );
        assert_ne!(group_of(&pool, c.id).await, group_of(&pool, a.id).await);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn never_joins_across_roleplay(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let a = seed_unplaced(&pool, user, "用户是医生", "explicit_statement", h(1)).await;
        let b = seed_unplaced(&pool, user, "用户是医生", "roleplay_expression", h(2)).await;
        repo.place(&a, &unit(0.0), T).await.unwrap();
        assert_eq!(
            repo.place(&b, &unit(0.0), T).await.unwrap(),
            Placement::Alone
        );
        assert_ne!(group_of(&pool, b.id).await, group_of(&pool, a.id).await);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn never_joins_across_users(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let a = seed_unplaced(
            &pool,
            Uuid::new_v4(),
            "用户是医生",
            "explicit_statement",
            h(1),
        )
        .await;
        let b = seed_unplaced(
            &pool,
            Uuid::new_v4(),
            "用户是医生",
            "explicit_statement",
            h(2),
        )
        .await;
        repo.place(&a, &unit(0.0), T).await.unwrap();
        assert_eq!(
            repo.place(&b, &unit(0.0), T).await.unwrap(),
            Placement::Alone
        );
        assert_ne!(group_of(&pool, b.id).await, group_of(&pool, a.id).await);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unplaced_lists_oldest_first(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let late = seed_unplaced(&pool, user, "c", "explicit_statement", h(3)).await;
        let early = seed_unplaced(&pool, user, "a", "explicit_statement", h(1)).await;
        let mid = seed_unplaced(&pool, user, "b", "explicit_statement", h(2)).await;
        let placed = seed_unplaced(&pool, user, "z", "explicit_statement", h(0)).await;
        repo.place(&placed, &unit(90.0), T).await.unwrap();

        let all = repo.unplaced(10).await.unwrap();
        assert_eq!(
            all.iter().map(|f| f.id).collect::<Vec<_>>(),
            vec![early.id, mid.id, late.id]
        );
        assert_eq!(all[0].user_id, user);
        assert_eq!(all[0].content, "a");
        assert!(!all[0].roleplay);
        assert_eq!(repo.unplaced(2).await.unwrap().len(), 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn skips_a_row_deleted_or_already_placed(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let a = seed_unplaced(&pool, user, "a", "explicit_statement", h(1)).await;
        let b = seed_unplaced(&pool, user, "b", "explicit_statement", h(2)).await;
        let gone = seed_unplaced(&pool, user, "gone", "explicit_statement", h(3)).await;
        sqlx::query("DELETE FROM engine.human_facts WHERE id = $1")
            .bind(gone.id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            repo.place(&gone, &unit(0.0), T).await.unwrap(),
            Placement::Skipped
        );

        repo.place(&a, &unit(0.0), T).await.unwrap();
        let b_group = group_of(&pool, b.id).await;
        assert_eq!(
            repo.place(&b, &unit(45.0), T).await.unwrap(),
            Placement::Alone
        );
        // A second placement at a vector that would join A changes nothing.
        assert_eq!(
            repo.place(&b, &unit(0.0), T).await.unwrap(),
            Placement::Skipped
        );
        assert_eq!(group_of(&pool, b.id).await, b_group);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_recurrence_keeps_its_group(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let a = seed_unplaced(&pool, user, "用户喜欢爬山", "explicit_statement", h(1)).await;
        repo.place(&a, &unit(0.0), T).await.unwrap();
        let b_fact = [fact("用户很喜欢爬山", "explicit_statement", "stable")];
        repo.upsert_run(user, &b_fact, &src(None, None, None))
            .await
            .unwrap();
        let b = repo.unplaced(10).await.unwrap().remove(0);
        assert_eq!(
            repo.place(&b, &unit(10.0), T).await.unwrap(),
            Placement::Joined
        );

        repo.upsert_run(user, &b_fact, &src(None, None, None))
            .await
            .unwrap();
        let (occurrences, group, placed): (i32, Uuid, bool) = sqlx::query_as(
            "SELECT occurrences, group_id, embedding IS NOT NULL \
             FROM engine.human_facts WHERE id = $1",
        )
        .bind(b.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(occurrences, 2);
        assert_eq!(group, group_of(&pool, a.id).await);
        assert!(placed, "a recurrence keeps the vector");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_next_member_anchors_after_the_anchor_is_deleted(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let a = seed_unplaced(&pool, user, "a", "explicit_statement", h(1)).await;
        let b = seed_unplaced(&pool, user, "b", "explicit_statement", h(2)).await;
        let c = seed_unplaced(&pool, user, "c", "explicit_statement", h(3)).await;
        repo.place(&a, &unit(0.0), T).await.unwrap();
        assert_eq!(
            repo.place(&b, &unit(10.0), T).await.unwrap(),
            Placement::Joined
        );
        assert!(repo.delete_owned(user, a.id).await.unwrap());
        // cos 8° = 0.990 to B, now the group's first fact; cos 18° = 0.951 to A.
        assert_eq!(
            repo.place(&c, &unit(18.0), T).await.unwrap(),
            Placement::Joined
        );
        assert_eq!(group_of(&pool, c.id).await, group_of(&pool, b.id).await);
    }

    /// A single-fact delete serialises with placement on the user's group
    /// lock, so a placement never picks an anchor that is deleted before its
    /// own write lands.
    #[sqlx::test(migrations = "./migrations")]
    async fn delete_owned_waits_for_the_users_group_lock(pool: PgPool) {
        let user = Uuid::new_v4();
        let a = seed_unplaced(&pool, user, "a", "explicit_statement", h(1)).await;
        let mut holder = pool.begin().await.unwrap();
        lock_user_groups(&mut holder, user).await.unwrap();

        let p = pool.clone();
        let delete =
            tokio::spawn(async move { HumanFactRepo { pool: &p }.delete_owned(user, a.id).await });
        crate::testutil::wait_for_lock_wait(&pool).await;
        assert!(!delete.is_finished(), "the delete waits for the lock");
        holder.commit().await.unwrap();
        assert!(delete.await.unwrap().unwrap());
    }

    /// Put `ids` into one group and return its id.
    async fn group_up(pool: &PgPool, ids: &[Uuid]) -> Uuid {
        let group = Uuid::new_v4();
        sqlx::query("UPDATE engine.human_facts SET group_id = $2 WHERE id = ANY($1)")
            .bind(ids)
            .bind(group)
            .execute(pool)
            .await
            .unwrap();
        group
    }

    async fn set_counts(pool: &PgPool, id: Uuid, occurrences: i32, sessions: i32) {
        sqlx::query(
            "UPDATE engine.human_facts SET occurrences = $2, session_count = $3 WHERE id = $1",
        )
        .bind(id)
        .bind(occurrences)
        .bind(sessions)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn list_groups_derives_representative_and_counters(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        let a = seed_unplaced(&pool, user, "a", "explicit_statement", h(1)).await;
        let b = seed_unplaced(&pool, user, "b", "explicit_statement", h(2)).await;
        let c = seed_unplaced(&pool, user, "c", "explicit_statement", h(3)).await;
        let d = seed_unplaced(&pool, user, "d", "explicit_statement", h(4)).await;
        set_counts(&pool, b.id, 3, 2).await;
        set_counts(&pool, c.id, 3, 1).await;
        sqlx::query("UPDATE engine.human_facts SET last_seen_at = $2 WHERE id = $1")
            .bind(c.id)
            .bind(h(9))
            .execute(&pool)
            .await
            .unwrap();
        let group = group_up(&pool, &[a.id, b.id, c.id]).await;

        let groups = repo.list_groups_owned(user, None, 10).await.unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups[0].facts.iter().map(|f| f.id).collect::<Vec<_>>(),
            vec![d.id]
        );
        let g = &groups[1];
        assert_eq!(g.group_id, group);
        assert_eq!(
            g.facts.iter().map(|f| f.id).collect::<Vec<_>>(),
            vec![b.id, c.id, a.id],
            "most occurrences first, ties to the earliest"
        );
        assert_eq!(g.occurrences(), 7);
        assert_eq!(g.session_count(), 4);
        assert_eq!(g.recurrence(), Recurrence::RepeatedAcrossSessions);
        assert_eq!(g.first_seen_at(), h(1));
        assert_eq!(g.last_seen_at(), h(9));
        assert!(!g.roleplay());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn list_groups_shows_unplaced_rows_alone(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        seed_unplaced(&pool, user, "用户是医生", "roleplay_expression", h(1)).await;
        seed_unplaced(&pool, user, "用户是医生啊", "roleplay_expression", h(2)).await;
        let groups = repo.list_groups_owned(user, None, 10).await.unwrap();
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|g| g.facts.len() == 1 && g.roleplay()));
        assert_eq!(groups[0].recurrence(), Recurrence::Once);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn list_groups_pages_ties_by_group_id(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let user = Uuid::new_v4();
        for content in ["a", "b", "c"] {
            seed_unplaced(&pool, user, content, "explicit_statement", h(1)).await;
        }
        let first = repo.list_groups_owned(user, None, 2).await.unwrap();
        assert_eq!(first.len(), 2);
        let last = first.last().unwrap();
        let rest = repo
            .list_groups_owned(user, Some((last.first_seen_at(), last.group_id)), 2)
            .await
            .unwrap();
        assert_eq!(rest.len(), 1);
        let mut seen: Vec<Uuid> = first.iter().chain(&rest).map(|g| g.group_id).collect();
        assert!(seen.windows(2).all(|w| w[0] > w[1]), "group_id descending");
        seen.dedup();
        assert_eq!(seen.len(), 3, "each group once");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn list_groups_is_owner_scoped(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let (user, other) = (Uuid::new_v4(), Uuid::new_v4());
        let mine = seed_unplaced(&pool, user, "a", "explicit_statement", h(1)).await;
        let theirs = seed_unplaced(&pool, other, "b", "explicit_statement", h(2)).await;
        // Even a shared group id never shows another user's row.
        group_up(&pool, &[mine.id, theirs.id]).await;
        let groups = repo.list_groups_owned(user, None, 10).await.unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].facts.iter().map(|f| f.id).collect::<Vec<_>>(),
            vec![mine.id]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn delete_group_removes_members_tombstones_each_and_marks_due(pool: PgPool) {
        let repo = HumanFactRepo { pool: &pool };
        let (user, other) = (Uuid::new_v4(), Uuid::new_v4());
        let a = seed_unplaced(&pool, user, "a", "explicit_statement", h(1)).await;
        let b = seed_unplaced(&pool, user, "b", "explicit_statement", h(2)).await;
        let c = seed_unplaced(&pool, user, "c", "explicit_statement", h(3)).await;
        let kept = seed_unplaced(&pool, user, "d", "explicit_statement", h(4)).await;
        let group = group_up(&pool, &[a.id, b.id, c.id]).await;
        let states = crate::profile_state::ProfileStateRepo { pool: &pool };
        states.bump(user, 2).await.unwrap();
        states
            .claim(user, std::time::Duration::from_secs(600))
            .await
            .unwrap()
            .expect("an in-flight rebuild");

        assert!(
            !repo.delete_group_owned(other, group).await.unwrap(),
            "not the other user's group"
        );
        assert!(repo.delete_group_owned(user, group).await.unwrap());
        assert!(
            !repo.delete_group_owned(user, group).await.unwrap(),
            "already gone"
        );

        let left: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM engine.human_facts")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, vec![kept.id]);
        let mut tombs: Vec<(String, Uuid)> =
            sqlx::query_as("SELECT kind, id FROM engine.row_deletions ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        let mut expected: Vec<(String, Uuid)> = [a.id, b.id, c.id]
            .iter()
            .map(|id| ("fact".to_string(), *id))
            .collect();
        expected.sort_by_key(|(_, id)| *id);
        tombs.sort_by_key(|(_, id)| *id);
        assert_eq!(tombs, expected);

        let s = states.load(user).await.unwrap().unwrap();
        assert_eq!(s.claimed_at, None, "the in-flight rebuild lost its claim");
        assert_eq!(
            s.pending_facts, 3,
            "one deletion, whatever the group's size"
        );
        assert!(s.rebuild_due);
    }
}
