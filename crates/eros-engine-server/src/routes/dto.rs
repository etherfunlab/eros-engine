// SPDX-License-Identifier: AGPL-3.0-only
//! Shared response DTOs used by more than one routing subtree.
//!
//! Currently holds:
//!   * `AffinitySnapshot` — point-in-time view of the 6-axis affinity
//!     vector, used by both `/comp/affinity/{sid}` (debug) and
//!     `/bff/v1/comp/chat/start` (Plan C).

use serde::Serialize;

use eros_engine_core::affinity::{Affinity, RelationshipLabel};

/// Point-in-time projection of a session's `Affinity`. Same field set
/// as the historical `AffinityDebugResponse`; renamed because it now
/// flows through non-debug surfaces too.
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
    pub relationship_label: Option<String>,
    pub updated_at: String,
}

fn label_to_str(l: RelationshipLabel) -> String {
    match l {
        RelationshipLabel::Stranger => "stranger",
        RelationshipLabel::Romantic => "romantic",
        RelationshipLabel::Friend => "friend",
        RelationshipLabel::Frenemy => "frenemy",
        RelationshipLabel::SlowBurn => "slow_burn",
    }
    .to_string()
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
            relationship_label: a.relationship_label.map(label_to_str),
            updated_at: a.updated_at.to_rfc3339(),
        }
    }
}
