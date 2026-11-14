// SPDX-License-Identifier: AGPL-3.0-only
//! Shared response DTOs used by more than one routing subtree.
//!
//! Currently holds:
//!   * `AffinitySnapshot` — point-in-time view of the affinity vector,
//!     returned by `GET /bff/v1/comp/affinity/{sid}`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use eros_engine_core::affinity::Affinity;
use eros_engine_store::activity::ActivityRecord;

/// Point-in-time projection of a session's `Affinity`.
///
/// Serialise this only from an `Affinity` that has had `apply_time_decay()`
/// and `refresh_endpoints()` applied: `warmth` and `patience` are derived as
/// of 4.0 and their stored columns are a write-time cache, so a snapshot taken
/// straight off the row reads systematically warm.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct AffinitySnapshot {
    pub warmth: f64,
    pub trust: f64,
    pub intrigue: f64,
    pub intimacy: f64,
    pub patience: f64,
    pub tension: f64,
    pub ghost_streak: i32,
    pub total_ghosts: i32,
    pub updated_at: String,
    /// Friendship line score, 0..1 — the real stored composite, no display
    /// curve (affinity 3.0 dropped the `bar()` projection; nonlinearity now
    /// lives in the write-side tier decay, so the raw value already reads
    /// fast-early/slow-late).
    pub bond: f64,
    /// Romance line score, 0..1 — real stored composite, no display curve.
    pub chemistry: f64,
    /// Friendship tier, 1..=5. Returned alongside the key so a client needs
    /// neither the thresholds nor an ordered tier array.
    pub bond_tier: u8,
    /// Romance tier, 1..=5.
    pub chem_tier: u8,
    /// Friendship tier key (`acquaintance`/`friend`/`close_friend`/`confidant`/`soulmate`).
    pub bond_label: String,
    /// Romance tier key (`spark`/`flirtation`/`crush`/`lover`/`beloved`).
    pub chemistry_label: String,
}

impl From<Affinity> for AffinitySnapshot {
    fn from(a: Affinity) -> Self {
        Self {
            warmth: a.warmth,
            trust: a.trust,
            intrigue: a.intrigue,
            intimacy: a.intimacy,
            patience: a.patience,
            tension: a.tension,
            ghost_streak: a.ghost_streak,
            total_ghosts: a.total_ghosts,
            updated_at: a.updated_at.to_rfc3339(),
            bond: a.bond_score(),
            chemistry: a.chemistry_score(),
            bond_tier: a.bond_tier(),
            chem_tier: a.chem_tier(),
            bond_label: a.bond_label().as_key().to_string(),
            chemistry_label: a.chemistry_label().as_key().to_string(),
        }
    }
}

/// One turn's exact per-turn bond/chemistry line delta, computed at persist time
/// from the floored before/after scores and read from the stored event column.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BondChemistryDeltas {
    pub bond: f64,
    pub chemistry: f64,
}

/// One line's tier transition (serialised keys). Read-side mirror of
/// `eros_engine_core::affinity::LabelTransition`.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LabelTransitionDto {
    pub from: String,
    pub to: String,
}

/// Per-turn tier transition across the two lines, read from the stored
/// `companion_affinity_events.label_changes` JSONB.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TurnLabelChangesDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bond: Option<LabelTransitionDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chemistry: Option<LabelTransitionDto>,
}

/// A date (spec 2026-11-12-activity-sub-session-design.md §4.6, §10): the
/// sub-session it is played in and its span, `null` once its event row is gone.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ActivityView {
    pub id: Uuid,
    pub sub_session_id: Uuid,
    /// `short` (1 hour) | `mid` (4 hours).
    pub duration: String,
    pub place: String,
    pub content: String,
    /// `ongoing` | `ended` | `settled`, as of the response.
    pub status: String,
    pub started_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
}

impl ActivityView {
    pub fn from_record(r: &ActivityRecord, now: DateTime<Utc>) -> Self {
        Self {
            id: r.id,
            sub_session_id: r.sub_session_id,
            duration: r.duration.clone(),
            place: r.place.clone(),
            content: r.content.clone(),
            status: r.status_at(now).as_str().to_string(),
            started_at: r.started_at,
            ends_at: r.ends_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eros_engine_core::affinity::Affinity;
    use uuid::Uuid;

    fn affinity(warmth: f64, trust: f64, intrigue: f64, intimacy: f64, tension: f64) -> Affinity {
        let now = chrono::Utc::now();
        Affinity {
            id: Uuid::nil(),
            session_id: Uuid::nil(),
            user_id: Uuid::nil(),
            instance_id: Uuid::nil(),
            warmth,
            trust,
            intrigue,
            intimacy,
            patience: 0.5,
            tension,
            warmth_grade: 2,
            patience_grade: 2,
            ghost_streak: 0,
            last_ghost_at: None,
            total_ghosts: 0,
            feeling_clause: None,
            feeling_clause_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn snapshot_exposes_raw_line_scores_and_labels() {
        // bond = (0.6+0.6)/2 = 0.6 → tier3 close_friend ; chemistry = 0 → spark
        let snap = AffinitySnapshot::from(affinity(0.0, 0.6, 0.6, 0.0, 0.0));
        assert_eq!(snap.bond_label, "close_friend");
        assert_eq!(snap.chemistry_label, "spark");
        // The real composite, not a projection.
        assert!((snap.bond - 0.6).abs() < 1e-9);
        assert!((snap.chemistry).abs() < 1e-9);
    }

    #[test]
    fn snapshot_tier_number_and_key_agree() {
        // Both are emitted so a client needs no threshold table; they must never
        // disagree — same `tier_index` behind each.
        let snap = AffinitySnapshot::from(affinity(0.0, 0.6, 0.6, 0.0, 0.0));
        assert_eq!(snap.bond_tier, 3);
        assert_eq!(snap.bond_label, "close_friend");
        assert_eq!(snap.chem_tier, 1);
        assert_eq!(snap.chemistry_label, "spark");
    }

    #[test]
    fn snapshot_of_fresh_row_is_bottom_tier() {
        let snap = AffinitySnapshot::from(affinity(0.1, 0.0, 0.0, 0.0, 0.0));
        assert_eq!(snap.bond_tier, 1);
        assert_eq!(snap.chem_tier, 1);
        assert_eq!(snap.bond_label, "acquaintance");
        assert_eq!(snap.chemistry_label, "spark");
    }
}

/// Frames as an SSE response, the way the chat stream sends them: one
/// `data:` line of JSON per frame, a `ping` comment every 15 seconds.
pub(crate) fn sse_response(
    frames: impl futures_util::Stream<Item = crate::pipeline::stream::ProtocolFrame> + Send + 'static,
) -> axum::response::Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use axum::response::IntoResponse as _;
    use futures_util::StreamExt as _;
    Sse::new(frames.map(|frame| {
        let json =
            serde_json::to_string(&frame).expect("ProtocolFrame serialization is infallible");
        Ok::<_, std::convert::Infallible>(Event::default().data(json))
    }))
    .keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("ping"),
    )
    .into_response()
}

/// Whether a request asks for the SSE response of a route that can answer
/// either way: `stream: true` in its body, or `text/event-stream` in
/// `Accept`. A range with `q=0` and `*/*` do not ask for it.
pub(crate) fn wants_stream(headers: &axum::http::HeaderMap, body_flag: bool) -> bool {
    body_flag
        || headers
            .get_all(axum::http::header::ACCEPT)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|range| {
                let mut parts = range.split(';').map(str::trim);
                let is_sse = parts
                    .next()
                    .is_some_and(|t| t.eq_ignore_ascii_case("text/event-stream"));
                let refused = parts.any(|p| {
                    p.split_once('=').is_some_and(|(k, v)| {
                        k.trim().eq_ignore_ascii_case("q")
                            && v.trim().parse::<f32>().is_ok_and(|q| q == 0.0)
                    })
                });
                is_sse && !refused
            })
}

#[cfg(test)]
mod stream_negotiation_tests {
    use super::wants_stream;
    use axum::http::{header::ACCEPT, HeaderMap, HeaderValue};

    fn accept(values: &[&'static str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(ACCEPT, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn wants_stream_reads_body_flag_or_accept() {
        assert!(!wants_stream(&HeaderMap::new(), false));
        assert!(wants_stream(&HeaderMap::new(), true));
        assert!(wants_stream(&accept(&["text/event-stream"]), false));
        assert!(wants_stream(&accept(&["TEXT/Event-Stream"]), false));
        assert!(wants_stream(
            &accept(&["application/json, text/event-stream;q=0.9"]),
            false
        ));
        assert!(wants_stream(
            &accept(&["application/json", "text/event-stream"]),
            false
        ));
        assert!(!wants_stream(&accept(&["text/event-stream;q=0"]), false));
        assert!(!wants_stream(&accept(&["text/event-stream; q=0.0"]), false));
        assert!(!wants_stream(&accept(&["*/*"]), false));
        assert!(!wants_stream(&accept(&["application/json"]), false));
        // The two are OR-ed: a body `false` does not veto the header.
        assert!(wants_stream(&accept(&["text/event-stream"]), false));
    }
}
