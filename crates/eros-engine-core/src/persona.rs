// SPDX-License-Identifier: AGPL-3.0-only
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::types::ItemKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaGenome {
    pub id: Uuid,
    pub name: String,
    pub system_prompt: String,
    pub tip_personality: Option<String>,
    pub art_metadata: Value, // JSONB: gender/age/mbti/backstory/speech_style/quirks/topics/model
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaInstance {
    pub id: Uuid,
    pub genome_id: Uuid,
    pub owner_uid: Uuid,
    pub status: String,
}

/// One entry of the persona's item lists as the prompt renders them (spec
/// 2026-10-12-persona-items-and-gift-design.md §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonaItem {
    pub kind: ItemKind,
    pub name: String,
    /// Some instance row with this kind and name came from a gift.
    pub gifted: bool,
}

/// The genome's preset lists: `art_metadata.possessions` (items) and
/// `art_metadata.wardrobe` (clothing), in genome order. Non-string and blank
/// entries are skipped and repeats collapse; the engine validates nothing
/// else (spec §3.3).
pub fn preset_items(art_metadata: &Value) -> Vec<(ItemKind, String)> {
    let mut out: Vec<(ItemKind, String)> = Vec::new();
    for (kind, key) in [
        (ItemKind::Item, "possessions"),
        (ItemKind::Clothing, "wardrobe"),
    ] {
        let names = art_metadata
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty());
        for name in names {
            if !out.iter().any(|(k, n)| *k == kind && n == name) {
                out.push((kind, name.to_string()));
            }
        }
    }
    out
}

/// The lists the prompt renders: the genome presets, then the instance's own
/// names in the order given (newest first, as `load_companion` reads them),
/// skipping a name already in the same list. A preset the user also gave is
/// marked `gifted`.
pub fn merge_items(art_metadata: &Value, instance: Vec<PersonaItem>) -> Vec<PersonaItem> {
    let mut out: Vec<PersonaItem> = preset_items(art_metadata)
        .into_iter()
        .map(|(kind, name)| PersonaItem {
            kind,
            name,
            gifted: false,
        })
        .collect();
    for item in instance {
        match out
            .iter_mut()
            .find(|p| p.kind == item.kind && p.name == item.name)
        {
            Some(p) => p.gifted |= item.gifted,
            None => out.push(item),
        }
    }
    out
}

/// Joined view used by the engine pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompanionPersona {
    pub instance_id: Uuid,
    pub genome: PersonaGenome,
    pub instance: PersonaInstance,
    /// The persona's items and wardrobe, merged by [`merge_items`]. Empty
    /// wherever the persona was not read through `load_companion`.
    #[serde(default)]
    pub items: Vec<PersonaItem>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ItemKind;

    fn it(kind: ItemKind, name: &str, gifted: bool) -> PersonaItem {
        PersonaItem {
            kind,
            name: name.into(),
            gifted,
        }
    }

    #[test]
    fn presets_skip_blank_and_non_string_entries_and_collapse_repeats() {
        let art = serde_json::json!({
            "possessions": ["吉他", "  ", 3, "吉他", " 相机 "],
            "wardrobe": ["白色开衫"],
        });
        assert_eq!(
            preset_items(&art),
            vec![
                (ItemKind::Item, "吉他".to_string()),
                (ItemKind::Item, "相机".to_string()),
                (ItemKind::Clothing, "白色开衫".to_string()),
            ]
        );
        assert!(preset_items(&serde_json::json!({})).is_empty());
        assert!(preset_items(&serde_json::json!({"wardrobe": "not an array"})).is_empty());
    }

    #[test]
    fn merge_puts_presets_first_and_marks_a_given_preset() {
        let art = serde_json::json!({"possessions": ["吉他"], "wardrobe": ["白色开衫"]});
        let instance = vec![
            it(ItemKind::Clothing, "白色开衫", true),
            it(ItemKind::Clothing, "红色旗袍", true),
            it(ItemKind::Item, "摩托车", false),
        ];
        assert_eq!(
            merge_items(&art, instance),
            vec![
                it(ItemKind::Item, "吉他", false),
                it(ItemKind::Clothing, "白色开衫", true),
                it(ItemKind::Clothing, "红色旗袍", true),
                it(ItemKind::Item, "摩托车", false),
            ]
        );
    }

    #[test]
    fn the_same_name_in_both_lists_stays_two_entries() {
        let art = serde_json::json!({"possessions": ["围巾"]});
        let merged = merge_items(&art, vec![it(ItemKind::Clothing, "围巾", true)]);
        assert_eq!(merged.len(), 2, "{merged:?}");
    }

    #[test]
    fn a_persona_without_items_deserializes_with_an_empty_list() {
        let v = serde_json::json!({
            "instance_id": uuid::Uuid::nil(),
            "genome": {"id": uuid::Uuid::nil(), "name": "A", "system_prompt": "p",
                       "tip_personality": null, "art_metadata": {}},
            "instance": {"id": uuid::Uuid::nil(), "genome_id": uuid::Uuid::nil(),
                         "owner_uid": uuid::Uuid::nil(), "status": "active"},
        });
        let p: CompanionPersona = serde_json::from_value(v).unwrap();
        assert!(p.items.is_empty());
    }
}
