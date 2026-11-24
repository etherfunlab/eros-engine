// SPDX-License-Identifier: AGPL-3.0-only
//! Relationship status, the composed relation, and the rules that move them
//! (spec 2026-11-23-affinity-50-design.md). Pure functions over the two
//! lines and the judge's patience levels; storage and the turn pipeline live
//! in the store and server crates.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::affinity::{Affinity, AffinityLine};

/// The settled conclusion about the pair, independent of the two lines. The
/// wire keys are the snake_case variant names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipStatus {
    #[default]
    None,
    Dating,
    Partner,
    Peer,
    Fwb,
    Situationship,
    Engaged,
    Ex,
}

impl RelationshipStatus {
    /// Every status, in wire order.
    pub const ALL: [Self; 8] = [
        Self::None,
        Self::Dating,
        Self::Partner,
        Self::Peer,
        Self::Fwb,
        Self::Situationship,
        Self::Engaged,
        Self::Ex,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Dating => "dating",
            Self::Partner => "partner",
            Self::Peer => "peer",
            Self::Fwb => "fwb",
            Self::Situationship => "situationship",
            Self::Engaged => "engaged",
            Self::Ex => "ex",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }

    /// Whether a break-up can end this status.
    pub fn is_breakable(self) -> bool {
        matches!(
            self,
            Self::Dating | Self::Partner | Self::Fwb | Self::Situationship | Self::Engaged
        )
    }

    /// Gender-neutral Chinese wording, read by the judge line for every
    /// status but `none` and by the prompt's status line for every status
    /// but `dating` and `engaged`, which take gendered nouns.
    pub fn zh(self) -> &'static str {
        match self {
            Self::None => "未确定",
            Self::Dating => "伴侣",
            Self::Partner => "伙伴",
            Self::Peer => "同事",
            Self::Fwb => "伙伴，也是情人",
            Self::Situationship => "同事，私下在交往",
            Self::Engaged => "已订婚",
            Self::Ex => "前任",
        }
    }
}

/// A session's status plus its cold-war flag. `cold_war_since` is the one
/// carrier of the flag: `Some` means a cold war is on, since that instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Relationship {
    pub status: RelationshipStatus,
    pub cold_war_since: Option<DateTime<Utc>>,
}

impl Relationship {
    pub fn cold_war(&self) -> bool {
        self.cold_war_since.is_some()
    }
}

/// The four asks that can change the status. Her three (`Confess`,
/// `PartnerUp`, `BreakUp`) and the user's `Propose` / `BreakUp` share one
/// transition table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusAsk {
    Confess,
    PartnerUp,
    Propose,
    BreakUp,
}

impl StatusAsk {
    /// Whether this ask can be made from `status` at all.
    pub fn asked_from(self, status: RelationshipStatus) -> bool {
        use RelationshipStatus as S;
        match self {
            Self::Confess => matches!(status, S::None | S::Ex | S::Partner | S::Peer),
            Self::PartnerUp => matches!(status, S::None | S::Ex | S::Peer),
            Self::Propose => matches!(status, S::Dating | S::Fwb | S::Situationship),
            Self::BreakUp => status.is_breakable(),
        }
    }
}

/// The relationship after `ask` is answered, or `None` when the ask cannot be
/// made from `from.status`. A refused `BreakUp` starts a cold war at `now`
/// unless one is already under way; an accepted one ends it.
pub fn settle(
    ask: StatusAsk,
    from: Relationship,
    accepted: bool,
    now: DateTime<Utc>,
) -> Option<Relationship> {
    use RelationshipStatus as S;
    if !ask.asked_from(from.status) {
        return None;
    }
    let status = match (ask, accepted) {
        (StatusAsk::Confess, true) => match from.status {
            S::Partner => S::Fwb,
            S::Peer => S::Situationship,
            _ => S::Dating,
        },
        (StatusAsk::PartnerUp, true) => S::Partner,
        (StatusAsk::PartnerUp, false) => S::Peer,
        (StatusAsk::Propose, true) => S::Engaged,
        (StatusAsk::BreakUp, true) => {
            return Some(Relationship {
                status: S::Ex,
                cold_war_since: None,
            });
        }
        (StatusAsk::BreakUp, false) => {
            return Some(Relationship {
                status: from.status,
                cold_war_since: from.cold_war_since.or(Some(now)),
            });
        }
        (StatusAsk::Confess | StatusAsk::Propose, false) => from.status,
    };
    Some(Relationship { status, ..from })
}

/// What the pair's two tiers add up to, in words that name no status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposedRelation {
    pub key: &'static str,
    pub zh: &'static str,
}

/// Rows are bond tiers 1..=5, columns chemistry tiers 1..=5. The first cell
/// is empty: two strangers have no composed relation.
const COMPOSED: [[(&str, &str); 5]; 5] = [
    [
        ("", ""),
        ("intriguing_stranger", "初识心动对象"),
        ("heady_stranger", "让你上头的陌生人"),
        ("fling", "露水情人"),
        ("love_at_first_sight", "一见倾心的人"),
    ],
    [
        ("casual_friend", "普通朋友"),
        ("budding_flame", "暧昧对象"),
        ("friend_and_crush", "迷恋对象"),
        ("passionate_flame", "热恋对象"),
        ("ardent_beloved", "热烈爱人"),
    ],
    [
        ("trusted_friend", "亲近好友"),
        ("more_than_friends", "友达以上"),
        ("almost_lovers", "恋人未满"),
        ("tender_lover", "相依恋人"),
        ("inseparable", "难分难舍的人"),
    ],
    [
        ("kindred_spirit", "知音"),
        ("knowing_confidant", "心照不宣的知己"),
        ("smitten_confidant", "暗生情愫的知己"),
        ("heart_lover", "知心爱人"),
        ("beloved", "挚爱"),
    ],
    [
        ("lifelong_kindred_spirit", "莫逆之交"),
        ("stirred_kindred", "动了心的灵魂之交"),
        ("platonic_love", "柏拉图式的爱人"),
        ("destined_lover", "灵犀恋人"),
        ("destined", "命中注定的人"),
    ],
];

/// The composed relation for a pair of tiers (each clamped to 1..=5);
/// `None` only at `(1, 1)`.
pub fn composed_relation(bond_tier: u8, chem_tier: u8) -> Option<ComposedRelation> {
    let (b, c) = (bond_tier.clamp(1, 5), chem_tier.clamp(1, 5));
    if (b, c) == (1, 1) {
        return None;
    }
    let (key, zh) = COMPOSED[b as usize - 1][c as usize - 1];
    Some(ComposedRelation { key, zh })
}

/// The line with the higher tier leads; on equal tiers the higher score does,
/// and an exact tie is bond's.
pub fn leading_line(a: &Affinity) -> AffinityLine {
    let (bond, chem) = (a.bond_tier(), a.chem_tier());
    if chem > bond || (chem == bond && a.chemistry_score() > a.bond_score()) {
        AffinityLine::Chemistry
    } else {
        AffinityLine::Bond
    }
}

/// The judge's lowest patience level (`patience_grade` runs 1..=3).
pub const LOWEST_PATIENCE: i16 = 1;

/// Streak length that starts and ends a cold war and grounds a her-side
/// break-up.
const STREAK: usize = 3;
/// The newest judged turns the patience rules read, the widest of their
/// windows: a cold war also starts on five lowest levels among this many.
pub const COLD_WAR_WINDOW: usize = 7;
const RECENT_LOWEST: usize = 5;

fn newest_streak_all(levels: &[i16], pred: impl Fn(i16) -> bool) -> bool {
    levels.len() >= STREAK && levels[..STREAK].iter().all(|&l| pred(l))
}

/// A cold war starts: the newest three levels are all the lowest, or at least
/// five of the newest seven are. `levels` are the judge's patience levels
/// since the streak anchor, newest first.
pub fn cold_war_starts(levels: &[i16]) -> bool {
    newest_streak_all(levels, |l| l == LOWEST_PATIENCE)
        || levels
            .iter()
            .take(COLD_WAR_WINDOW)
            .filter(|&&l| l == LOWEST_PATIENCE)
            .count()
            >= RECENT_LOWEST
}

/// A cold war ends: the newest three levels are all above the lowest.
pub fn cold_war_ends(levels: &[i16]) -> bool {
    newest_streak_all(levels, |l| l > LOWEST_PATIENCE)
}

/// She asks to break up: the newest three levels are all the lowest.
pub fn wants_break_up(levels: &[i16]) -> bool {
    newest_streak_all(levels, |l| l == LOWEST_PATIENCE)
}

/// Whether her `sleep` ask means sex rather than falling asleep side by side:
/// a couple's status always does, a bare partner's only at chemistry tier 4+.
pub fn sleep_ask_is_sex(status: RelationshipStatus, chem_tier: u8) -> bool {
    use RelationshipStatus as S;
    match status {
        S::Dating | S::Engaged | S::Fwb | S::Situationship => true,
        S::Partner => chem_tier >= 4,
        S::None | S::Peer | S::Ex => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::affinity::{Affinity, AffinityLine};
    use chrono::Utc;

    #[test]
    fn status_keys_round_trip_and_breakable_set() {
        let keys = [
            "none",
            "dating",
            "partner",
            "peer",
            "fwb",
            "situationship",
            "engaged",
            "ex",
        ];
        for (s, k) in RelationshipStatus::ALL.into_iter().zip(keys) {
            assert_eq!(s.as_str(), k);
            assert_eq!(RelationshipStatus::parse(k), Some(s));
            assert_eq!(serde_json::to_value(s).unwrap(), serde_json::json!(k));
        }
        assert_eq!(RelationshipStatus::parse("married"), Option::None);
        use RelationshipStatus::*;
        let breakable: Vec<_> = RelationshipStatus::ALL
            .into_iter()
            .filter(|s| s.is_breakable())
            .collect();
        assert_eq!(breakable, [Dating, Partner, Fwb, Situationship, Engaged]);
    }

    #[test]
    fn settle_follows_the_transition_table() {
        use RelationshipStatus::*;
        let now = Utc::now();
        let at = |status| Relationship {
            status,
            cold_war_since: Option::None,
        };
        let to = |ask, from, ok| settle(ask, at(from), ok, now).map(|r| r.status);
        // confess
        for (from, won) in [
            (None, Dating),
            (Ex, Dating),
            (Partner, Fwb),
            (Peer, Situationship),
        ] {
            assert_eq!(to(StatusAsk::Confess, from, true), Some(won), "{from:?}");
            assert_eq!(to(StatusAsk::Confess, from, false), Some(from), "{from:?}");
        }
        for from in [Dating, Fwb, Situationship, Engaged] {
            assert_eq!(to(StatusAsk::Confess, from, true), Option::None, "{from:?}");
        }
        // partner_up
        for from in [None, Ex, Peer] {
            assert_eq!(to(StatusAsk::PartnerUp, from, true), Some(Partner));
            assert_eq!(to(StatusAsk::PartnerUp, from, false), Some(Peer));
        }
        for from in [Partner, Dating, Fwb, Situationship, Engaged] {
            assert_eq!(
                to(StatusAsk::PartnerUp, from, true),
                Option::None,
                "{from:?}"
            );
        }
        // propose
        for from in [Dating, Fwb, Situationship] {
            assert_eq!(to(StatusAsk::Propose, from, true), Some(Engaged));
            assert_eq!(to(StatusAsk::Propose, from, false), Some(from));
        }
        for from in [None, Partner, Peer, Engaged, Ex] {
            assert_eq!(to(StatusAsk::Propose, from, true), Option::None, "{from:?}");
        }
        // break_up
        for from in [Dating, Partner, Fwb, Situationship, Engaged] {
            let accepted = settle(
                StatusAsk::BreakUp,
                Relationship {
                    status: from,
                    cold_war_since: Some(now),
                },
                true,
                now,
            )
            .unwrap();
            assert_eq!(
                accepted,
                Relationship {
                    status: Ex,
                    cold_war_since: Option::None
                },
                "an accept ends the cold war"
            );
            let refused = settle(StatusAsk::BreakUp, at(from), false, now).unwrap();
            assert_eq!(
                refused,
                Relationship {
                    status: from,
                    cold_war_since: Some(now)
                }
            );
        }
        let earlier = now - chrono::Duration::hours(5);
        let kept = settle(
            StatusAsk::BreakUp,
            Relationship {
                status: Dating,
                cold_war_since: Some(earlier),
            },
            false,
            now,
        )
        .unwrap();
        assert_eq!(
            kept.cold_war_since,
            Some(earlier),
            "a cold war under way keeps its start"
        );
        for from in [None, Peer, Ex] {
            assert_eq!(to(StatusAsk::BreakUp, from, true), Option::None, "{from:?}");
        }
    }

    #[test]
    fn composed_relation_names_every_cell_but_the_first() {
        assert_eq!(composed_relation(1, 1), None);
        let mut keys = std::collections::HashSet::new();
        for b in 1..=5 {
            for c in 1..=5 {
                if (b, c) == (1, 1) {
                    continue;
                }
                let r = composed_relation(b, c).unwrap();
                assert!(keys.insert(r.key), "duplicate key {}", r.key);
                for word in ["伴侣", "伙伴", "同事", "前任", "未婚"] {
                    assert!(!r.zh.contains(word), "{} uses a status word", r.zh);
                }
            }
        }
        assert_eq!(keys.len(), 24);
        assert_eq!(
            composed_relation(3, 2).unwrap(),
            ComposedRelation {
                key: "more_than_friends",
                zh: "友达以上"
            }
        );
        assert_eq!(
            composed_relation(4, 4).unwrap(),
            ComposedRelation {
                key: "heart_lover",
                zh: "知心爱人"
            }
        );
        assert_eq!(
            composed_relation(5, 5).unwrap(),
            ComposedRelation {
                key: "destined",
                zh: "命中注定的人"
            }
        );
        assert_eq!(composed_relation(2, 1).unwrap().zh, "普通朋友");
    }

    #[test]
    fn the_higher_tier_leads_and_a_tie_goes_to_the_higher_score() {
        // bond (trust+intrigue)/2, chemistry (intimacy+tension)/2
        let a = |trust: f64, intimacy: f64| {
            let mut x = Affinity::unrecorded();
            x.trust = trust;
            x.intrigue = trust;
            x.intimacy = intimacy;
            x.tension = intimacy;
            x
        };
        assert_eq!(leading_line(&a(0.40, 0.70)), AffinityLine::Chemistry); // 3 vs 4
        assert_eq!(leading_line(&a(0.70, 0.40)), AffinityLine::Bond);
        assert_eq!(leading_line(&a(0.40, 0.45)), AffinityLine::Chemistry); // both tier 3
        assert_eq!(leading_line(&a(0.45, 0.40)), AffinityLine::Bond);
        assert_eq!(
            leading_line(&a(0.40, 0.40)),
            AffinityLine::Bond,
            "an exact tie is bond's"
        );
    }

    #[test]
    fn patience_streaks() {
        // newest first
        assert!(cold_war_starts(&[1, 1, 1]));
        assert!(!cold_war_starts(&[1, 1]));
        assert!(!cold_war_starts(&[1, 1, 2, 1]));
        assert!(cold_war_starts(&[1, 2, 1, 1, 2, 1, 1]), "five of seven");
        assert!(!cold_war_starts(&[1, 2, 1, 1, 2, 1, 2]), "four of seven");
        assert!(
            !cold_war_starts(&[2, 1, 1, 1, 1, 2, 2, 1]),
            "only the newest seven count"
        );
        assert!(cold_war_ends(&[2, 3, 2]));
        assert!(!cold_war_ends(&[2, 3]));
        assert!(!cold_war_ends(&[2, 1, 2]));
        assert!(wants_break_up(&[1, 1, 1, 3]));
        assert!(!wants_break_up(&[1, 1]));
    }

    #[test]
    fn her_sleep_ask_is_sex_for_couples_and_close_partners() {
        use RelationshipStatus::*;
        for s in [Dating, Engaged, Fwb, Situationship] {
            assert!(sleep_ask_is_sex(s, 3), "{s:?}");
        }
        assert!(!sleep_ask_is_sex(Partner, 3));
        assert!(sleep_ask_is_sex(Partner, 4));
        for s in [None, Peer, Ex] {
            assert!(!sleep_ask_is_sex(s, 5), "{s:?}");
        }
    }
}
