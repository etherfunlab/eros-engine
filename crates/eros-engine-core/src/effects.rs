// SPDX-License-Identifier: AGPL-3.0-only
//! What an accepted `give` keeps doing to the persona over the turns after it
//! (spec 2026-10-12-given-item-lingering-effects-design.md §4).

use serde::{Deserialize, Serialize};

use crate::types::{GivenItem, UserAction};

/// Turns an effect lasts at most.
pub const MAX_STEPS: usize = 5;

/// Levels by step for quantities 1–5 (§4.1). One row per quantity.
const CIGARETTE: [&[u8]; MAX_STEPS] = [&[3], &[3, 1], &[3, 2, 1], &[3, 2, 2, 1], &[3, 3, 2, 2, 1]];
const ALCOHOL: [&[u8]; MAX_STEPS] = [&[1], &[1, 3], &[1, 2, 3], &[1, 2, 2, 3], &[1, 2, 2, 3, 3]];
const MEDICINE: [&[u8]; MAX_STEPS] = [&[3], &[3, 3], &[1, 3, 1], &[1, 3, 3, 1], &[1, 2, 3, 2, 1]];
/// A quantity above 5: the only rows that reach level 4.
const CIGARETTE_HEAVY: &[u8] = &[4, 3, 3, 2, 1];
const ALCOHOL_HEAVY: &[u8] = &[1, 2, 3, 4, 4];
const MEDICINE_HEAVY: &[u8] = &[1, 3, 4, 2, 1];

/// Where a step sits on its row (§4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectPhase {
    Rising,
    Peak,
    Fading,
}

/// One accepted give still at work on this turn. The serde shape is the
/// `body_state` frame's element and is persisted in a voice call's snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveEffect {
    pub item: GivenItem,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub quantity: u8,
    /// 1–4.
    pub level: u8,
    pub phase: EffectPhase,
    /// This turn's step k, 1-based.
    pub step: u8,
    /// How many turns the effect lasts, `min(quantity, 5)`.
    pub steps: u8,
}

/// The curve row for an item and a quantity (§4.1). Empty for quantity 0.
pub fn curve(item: GivenItem, quantity: u8) -> &'static [u8] {
    let (rows, heavy) = match item {
        GivenItem::Cigarette => (&CIGARETTE, CIGARETTE_HEAVY),
        GivenItem::Alcohol => (&ALCOHOL, ALCOHOL_HEAVY),
        GivenItem::Medicine => (&MEDICINE, MEDICINE_HEAVY),
    };
    match quantity {
        0 => &[],
        1..=5 => rows[usize::from(quantity) - 1],
        _ => heavy,
    }
}

/// The direction of step index `i` (0-based) on `row`: at the row's maximum
/// it is the peak, before the first maximum it rises, after it it fades.
fn phase(row: &[u8], i: usize) -> EffectPhase {
    let peak = row.iter().copied().max().unwrap_or(0);
    let first = row.iter().position(|&l| l == peak).unwrap_or(0);
    if row[i] == peak {
        EffectPhase::Peak
    } else if i < first {
        EffectPhase::Rising
    } else {
        EffectPhase::Fading
    }
}

/// `prior` holds the turns before the current one, newest first: the action
/// each turn carried if it was accepted, else `None`. Returns the active
/// effects, newest first (§3).
pub fn active_effects(prior: &[Option<UserAction>]) -> Vec<ActiveEffect> {
    prior
        .iter()
        .enumerate()
        .filter_map(|(i, action)| {
            let UserAction::Give {
                item,
                name,
                quantity,
            } = action.as_ref()?
            else {
                return None;
            };
            let row = curve(*item, *quantity);
            let level = *row.get(i)?;
            Some(ActiveEffect {
                item: *item,
                name: name.clone(),
                quantity: *quantity,
                level,
                phase: phase(row, i),
                step: (i + 1) as u8,
                steps: row.len() as u8,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use EffectPhase::{Fading, Peak, Rising};

    fn give(item: GivenItem, quantity: u8) -> Option<UserAction> {
        Some(UserAction::Give {
            item,
            name: None,
            quantity,
        })
    }

    /// The spec's §4.1 rows for quantities 1–5 and above 5, with the §4.2
    /// direction of every step.
    #[allow(clippy::type_complexity)]
    fn table() -> [(GivenItem, [(&'static [u8], &'static [EffectPhase]); 6]); 3] {
        [
            (
                GivenItem::Cigarette,
                [
                    (&[3], &[Peak]),
                    (&[3, 1], &[Peak, Fading]),
                    (&[3, 2, 1], &[Peak, Fading, Fading]),
                    (&[3, 2, 2, 1], &[Peak, Fading, Fading, Fading]),
                    (&[3, 3, 2, 2, 1], &[Peak, Peak, Fading, Fading, Fading]),
                    (&[4, 3, 3, 2, 1], &[Peak, Fading, Fading, Fading, Fading]),
                ],
            ),
            (
                GivenItem::Alcohol,
                [
                    (&[1], &[Peak]),
                    (&[1, 3], &[Rising, Peak]),
                    (&[1, 2, 3], &[Rising, Rising, Peak]),
                    (&[1, 2, 2, 3], &[Rising, Rising, Rising, Peak]),
                    (&[1, 2, 2, 3, 3], &[Rising, Rising, Rising, Peak, Peak]),
                    (&[1, 2, 3, 4, 4], &[Rising, Rising, Rising, Peak, Peak]),
                ],
            ),
            (
                GivenItem::Medicine,
                [
                    (&[3], &[Peak]),
                    (&[3, 3], &[Peak, Peak]),
                    (&[1, 3, 1], &[Rising, Peak, Fading]),
                    (&[1, 3, 3, 1], &[Rising, Peak, Peak, Fading]),
                    (&[1, 2, 3, 2, 1], &[Rising, Rising, Peak, Fading, Fading]),
                    (&[1, 3, 4, 2, 1], &[Rising, Rising, Peak, Fading, Fading]),
                ],
            ),
        ]
    }

    #[test]
    fn curve_rows_match_the_spec_table() {
        for (item, rows) in table() {
            for q in 1..=5u8 {
                assert_eq!(curve(item, q), rows[usize::from(q) - 1].0, "{item:?} ×{q}");
            }
            for q in [6u8, 7, 99] {
                assert_eq!(curve(item, q), rows[5].0, "{item:?} ×{q}");
            }
            assert!(curve(item, 0).is_empty(), "{item:?} ×0");
        }
    }

    /// Every step of every row through the public fold: the give sits k turns
    /// back and comes out on step k with that cell's level and direction.
    #[test]
    fn every_step_of_every_row() {
        for (item, rows) in table() {
            for (qi, (levels, phases)) in rows.iter().enumerate() {
                let quantity = if qi == 5 { 6 } else { qi as u8 + 1 };
                for k in 1..=levels.len() {
                    let mut prior = vec![None; k - 1];
                    prior.push(give(item, quantity));
                    let got = active_effects(&prior);
                    assert_eq!(got.len(), 1, "{item:?} ×{quantity} k={k}");
                    let e = &got[0];
                    assert_eq!(e.level, levels[k - 1], "{item:?} ×{quantity} k={k}");
                    assert_eq!(e.phase, phases[k - 1], "{item:?} ×{quantity} k={k}");
                    assert_eq!(usize::from(e.step), k);
                    assert_eq!(usize::from(e.steps), levels.len());
                }
            }
        }
    }

    #[test]
    fn an_effect_ends_after_its_last_step() {
        let mut prior = vec![None, None];
        prior.push(give(GivenItem::Alcohol, 3));
        assert_eq!(active_effects(&prior).len(), 1, "k = N is active");
        prior.insert(0, None);
        assert!(active_effects(&prior).is_empty(), "k = N + 1 is not");
    }

    #[test]
    fn a_quantity_above_five_lasts_five_turns() {
        let mut prior = vec![None; 4];
        prior.push(give(GivenItem::Medicine, 12));
        let e = &active_effects(&prior)[0];
        assert_eq!((e.level, e.phase, e.step, e.steps), (1, Fading, 5, 5));
        prior.insert(0, None);
        assert!(active_effects(&prior).is_empty());
    }

    #[test]
    fn non_give_entries_and_quantity_zero_are_skipped() {
        let prior = vec![
            Some(UserAction::Kiss),
            give(GivenItem::Cigarette, 0),
            None,
            give(GivenItem::Cigarette, 5),
        ];
        let got = active_effects(&prior);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].step, got[0].level), (4, 2));
    }

    #[test]
    fn several_effects_come_newest_first_and_keep_the_name() {
        let prior = vec![
            give(GivenItem::Cigarette, 1),
            Some(UserAction::Give {
                item: GivenItem::Alcohol,
                name: Some("威士忌".into()),
                quantity: 3,
            }),
        ];
        let got = active_effects(&prior);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].item, got[0].step), (GivenItem::Cigarette, 1));
        assert_eq!((got[1].item, got[1].step), (GivenItem::Alcohol, 2));
        assert_eq!(got[1].name.as_deref(), Some("威士忌"));
    }

    #[test]
    fn active_effect_serde_shape() {
        let e = ActiveEffect {
            item: GivenItem::Alcohol,
            name: Some("威士忌".into()),
            quantity: 3,
            level: 2,
            phase: Rising,
            step: 2,
            steps: 3,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"item": "alcohol", "name": "威士忌", "quantity": 3,
                               "level": 2, "phase": "rising", "step": 2, "steps": 3})
        );
        assert_eq!(serde_json::from_value::<ActiveEffect>(v).unwrap(), e);
        let unnamed = ActiveEffect { name: None, ..e };
        assert!(serde_json::to_value(&unnamed)
            .unwrap()
            .get("name")
            .is_none());
    }
}
