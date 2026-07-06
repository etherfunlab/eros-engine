// SPDX-License-Identifier: AGPL-3.0-only
//! Voice channel — thin per-turn generator and prompt.
//!
//! Spec: docs/superpowers/specs/2026-07-07-voice-call-parts-design.md

use eros_engine_core::affinity::{Affinity, RelationshipLabel};
use eros_engine_core::persona::PersonaGenome;

/// Assemble the thin voice system prompt: persona + voice directive + one
/// optional relationship line. Deliberately excludes recall, memories, traits,
/// scopes, and every heavy block the text path's `build_prompt` composes.
pub fn build_voice_prompt(
    genome: &PersonaGenome,
    directive: &str,
    affinity: Option<&Affinity>,
) -> String {
    let mut s = String::with_capacity(genome.system_prompt.len() + directive.len() + 96);
    s.push_str(&genome.system_prompt);
    s.push_str("\n\n");
    s.push_str(directive);
    if let Some(line) = affinity.and_then(relationship_line) {
        s.push_str("\n\n");
        s.push_str(&line);
    }
    s
}

/// One short relationship-tone line from the cached `relationship_label`.
/// `None` (fresh affinity, no label yet) ⇒ no line.
fn relationship_line(affinity: &Affinity) -> Option<String> {
    let phrase = match &affinity.relationship_label {
        Some(RelationshipLabel::Stranger) => {
            "You two are still getting to know each other; keep it light."
        }
        Some(RelationshipLabel::Friend) => "You two are close friends; be warm and familiar.",
        Some(RelationshipLabel::Romantic) => "You share a romantic bond; be affectionate.",
        Some(RelationshipLabel::Frenemy) => "Your dynamic is playful and a little combative.",
        Some(RelationshipLabel::SlowBurn) => "There's a slow-building closeness between you.",
        None => return None,
    };
    Some(phrase.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn genome() -> PersonaGenome {
        PersonaGenome {
            id: Uuid::new_v4(),
            name: "Mia".into(),
            system_prompt: "You are Mia.".into(),
            tip_personality: None,
            art_metadata: serde_json::json!({}),
        }
    }

    fn affinity_with(label: Option<RelationshipLabel>) -> Affinity {
        let now = Utc::now();
        Affinity {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            instance_id: Uuid::new_v4(),
            warmth: 0.0,
            trust: 0.0,
            intrigue: 0.0,
            intimacy: 0.0,
            patience: 0.0,
            tension: 0.0,
            ghost_streak: 0,
            last_ghost_at: None,
            total_ghosts: 0,
            relationship_label: label,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn includes_persona_and_directive() {
        let p = build_voice_prompt(&genome(), "DIRECTIVE", None);
        assert!(p.contains("You are Mia."));
        assert!(p.contains("DIRECTIVE"));
        // No affinity ⇒ no relationship line.
        assert!(!p.contains("romantic"));
    }

    #[test]
    fn appends_relationship_line_when_labelled() {
        let a = affinity_with(Some(RelationshipLabel::Romantic));
        let p = build_voice_prompt(&genome(), "DIRECTIVE", Some(&a));
        assert!(p.contains("romantic bond"));
    }

    #[test]
    fn no_relationship_line_when_label_none() {
        let a = affinity_with(None);
        let p = build_voice_prompt(&genome(), "DIRECTIVE", Some(&a));
        assert_eq!(p, "You are Mia.\n\nDIRECTIVE");
    }
}
