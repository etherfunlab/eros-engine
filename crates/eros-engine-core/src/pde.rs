// SPDX-License-Identifier: AGPL-3.0-only
//! Persona Decision Engine — produces an ActionPlan per event.
//!
//! Strategy: deterministic rules first, LLM fallback only when rules cannot
//! decide. Ghost / energy mappings are all rule-based.

use crate::affinity::AffinityDeltas;
use crate::ghost::{self, GhostDecision, GhostSignals};
use crate::types::{ActionPlan, ActionType, DecisionInput, Event, ImageRef, ReplyStyle};

// Decision thresholds — tune here rather than at call sites.
const LONG_MSG_CHARS: usize = 30;
const SHORT_MSG_CHARS: usize = 3;
const STALE_HOURS: f64 = 24.0;

const INTRIGUE_LONG_BUMP: f64 = 0.02;
const PATIENCE_LONG_BUMP: f64 = 0.02;
const PATIENCE_SHORT_PENALTY: f64 = -0.02;
const PATIENCE_STALE_PENALTY: f64 = -0.05;
const TENSION_STALE_BUMP: f64 = 0.03;

const ENERGY_COST_REPLY: f64 = 0.05;
const ENERGY_COST_PROACTIVE: f64 = 0.10;
const ENERGY_COST_GHOST: f64 = 0.0;
const ENERGY_COST_APP_OPEN: f64 = 0.0;

const GHOST_DELTA_PATIENCE: f64 = -0.05;
const GHOST_DELTA_TENSION: f64 = 0.05;

/// Core decision function.
///
/// Phase 2: rules only. Phase 6 adds the LLM fallback path.
pub fn decide(input: &DecisionInput) -> ActionPlan {
    // 0. Tip on a user message — always reply, never ghost. Tone is driven by
    //    tip_personality (injected into the prompt downstream); the ReplyStyle
    //    here is only a baseline / fallback. Affinity deltas stay normal.
    if let Event::UserMessage {
        tips_amount_usd: Some(_),
        ..
    } = &input.event
    {
        let reply_style = match input.persona.genome.tip_personality.as_deref() {
            Some(_) => ReplyStyle::Neutral,
            None => ReplyStyle::Tsundere,
        };
        return ActionPlan {
            action_type: ActionType::ReplyText,
            reply_style,
            affinity_deltas: predict_reply_deltas(input),
            energy_cost: ENERGY_COST_REPLY,
            context_hints: vec![],
            image_prompt: None,
            image_ref: ImageRef::Face,
            aspect_ratio: None,
        };
    }

    // 1. Ghost judgement (via existing ghost module)
    let ghost_signals = GhostSignals {
        message_count: input.signals.message_count,
        hours_since_last_ghost: input.signals.hours_since_last_ghost,
    };
    if ghost::decide(&input.affinity, ghost_signals) == GhostDecision::Ghost {
        return ActionPlan {
            action_type: ActionType::Ghost,
            reply_style: ReplyStyle::Cold,
            affinity_deltas: ghost_affinity_deltas(),
            energy_cost: ENERGY_COST_GHOST,
            context_hints: vec![],
            image_prompt: None,
            image_ref: ImageRef::Face,
            aspect_ratio: None,
        };
    }

    // 2. Proactive trigger is passed through — Phase 6 defines full behaviour
    if matches!(input.event, Event::ProactiveTrigger) {
        return ActionPlan {
            action_type: ActionType::Proactive,
            reply_style: ReplyStyle::Neutral,
            affinity_deltas: AffinityDeltas::default(),
            energy_cost: ENERGY_COST_PROACTIVE,
            context_hints: vec![],
            image_prompt: None,
            image_ref: ImageRef::Face,
            aspect_ratio: None,
        };
    }

    // 3. AppOpen: user just opened the app — route to Proactive path with no cost.
    // Handler / post-process decide whether to actually send anything.
    if matches!(input.event, Event::AppOpen) {
        return ActionPlan {
            action_type: ActionType::Proactive,
            reply_style: ReplyStyle::Neutral,
            affinity_deltas: AffinityDeltas::default(),
            energy_cost: ENERGY_COST_APP_OPEN,
            context_hints: vec![],
            image_prompt: None,
            image_ref: ImageRef::Face,
            aspect_ratio: None,
        };
    }

    // 4. Regular reply
    ActionPlan {
        action_type: ActionType::ReplyText,
        reply_style: ReplyStyle::Neutral,
        affinity_deltas: predict_reply_deltas(input),
        energy_cost: ENERGY_COST_REPLY,
        context_hints: vec![],
        image_prompt: None,
        image_ref: ImageRef::Face,
        aspect_ratio: None,
    }
}

/// Build the `ActionPlan` for an LLM-chosen action, reusing the rule heuristic
/// and energy constants internally. Per action:
///   ReplyText / ReplyImage / ReplyTextImage → Neutral, predict_reply_deltas,
///                                              ENERGY_COST_REPLY, context_hints = hints
///   Ghost                                   → Cold, ghost_affinity_deltas,
///                                              ENERGY_COST_GHOST, hints discarded
///   Proactive                               → unreachable! (comes only from decide)
pub fn plan_for(
    input: &DecisionInput,
    action: ActionType,
    hints: Vec<String>,
    image_prompt: Option<String>,
    image_ref: ImageRef,
    aspect_ratio: Option<String>,
) -> ActionPlan {
    match action {
        ActionType::ReplyText | ActionType::ReplyImage | ActionType::ReplyTextImage => ActionPlan {
            action_type: action,
            reply_style: ReplyStyle::Neutral,
            affinity_deltas: predict_reply_deltas(input),
            energy_cost: ENERGY_COST_REPLY,
            context_hints: hints,
            image_prompt,
            image_ref,
            aspect_ratio,
        },
        ActionType::Ghost => ActionPlan {
            action_type: ActionType::Ghost,
            reply_style: ReplyStyle::Cold,
            affinity_deltas: ghost_affinity_deltas(),
            energy_cost: ENERGY_COST_GHOST,
            context_hints: vec![],
            image_prompt: None,
            image_ref: ImageRef::Face,
            aspect_ratio: None,
        },
        ActionType::Proactive => {
            unreachable!("plan_for is never called with Proactive; it comes only from pde::decide")
        }
    }
}

/// Predict affinity delta sign based on user message length / signals.
/// Conservative: small positive/negative heuristics only. Full evaluation
/// remains deterministic so no LLM JSON parsing is needed here.
fn predict_reply_deltas(input: &DecisionInput) -> AffinityDeltas {
    let mut d = AffinityDeltas::default();

    if let Event::UserMessage { content, .. } = &input.event {
        let chars = content.chars().count();
        // Long, thoughtful user message — small intrigue/patience bump
        if chars >= LONG_MSG_CHARS {
            d.intrigue += INTRIGUE_LONG_BUMP;
            d.patience += PATIENCE_LONG_BUMP;
        }
        // Very short/one-word — patience penalty
        if chars <= SHORT_MSG_CHARS {
            d.patience += PATIENCE_SHORT_PENALTY;
        }
    }

    // Time gap large — patience penalty + tension bump
    if input.signals.hours_since_last_message > STALE_HOURS {
        d.patience += PATIENCE_STALE_PENALTY;
        d.tension += TENSION_STALE_BUMP;
    }

    d
}

fn ghost_affinity_deltas() -> AffinityDeltas {
    AffinityDeltas {
        patience: GHOST_DELTA_PATIENCE,
        tension: GHOST_DELTA_TENSION,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::affinity::Affinity;
    use crate::persona::{CompanionPersona, PersonaGenome, PersonaInstance};
    use crate::types::ConversationSignals;
    use chrono::Utc;
    use uuid::Uuid;

    fn base_persona() -> CompanionPersona {
        let iid = Uuid::new_v4();
        let gid = Uuid::new_v4();
        let oid = Uuid::new_v4();
        CompanionPersona {
            instance_id: iid,
            genome: PersonaGenome {
                id: gid,
                name: "Mia".into(),
                system_prompt: "You are Mia.".into(),
                tip_personality: Some("normal".into()),
                art_metadata: serde_json::json!({}),
            },
            instance: PersonaInstance {
                id: iid,
                genome_id: gid,
                owner_uid: oid,
                status: "active".into(),
            },
        }
    }

    fn base_affinity() -> Affinity {
        let now = Utc::now();
        Affinity {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            instance_id: Uuid::new_v4(),
            warmth: 0.4,
            trust: 0.3,
            intrigue: 0.5,
            intimacy: 0.2,
            patience: 0.5,
            tension: 0.1,
            ghost_streak: 0,
            last_ghost_at: None,
            total_ghosts: 0,
            relationship_label: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn base_signals() -> ConversationSignals {
        ConversationSignals {
            message_count: 20,
            hours_since_last_message: 1.0,
            ghost_streak: 0,
            hours_since_last_ghost: Some(10.0),
        }
    }

    fn user_msg(content: &str) -> Event {
        Event::UserMessage {
            content: content.into(),
            message_id: Uuid::new_v4(),
            prompt_traits: Vec::new(),
            audit: None,
            tier: None,
            memory_scope: Default::default(),
            affinity_scope: Default::default(),
            tips_amount_usd: None,
        }
    }

    fn tip_msg(amount: f64) -> Event {
        Event::UserMessage {
            content: String::new(),
            message_id: Uuid::new_v4(),
            prompt_traits: Vec::new(),
            audit: None,
            tier: None,
            memory_scope: Default::default(),
            affinity_scope: Default::default(),
            tips_amount_usd: Some(amount),
        }
    }

    fn persona_with_tip(tip: Option<&str>) -> CompanionPersona {
        let mut p = base_persona();
        p.genome.tip_personality = tip.map(String::from);
        p
    }

    #[test]
    fn test_tip_forces_reply_even_when_ghost_signals_present() {
        // Same affinity that drives test_ghost_threshold_triggers_ghost_action.
        let mut affinity = base_affinity();
        affinity.intrigue = 0.05;
        affinity.patience = 0.05;
        affinity.tension = 0.5;
        let input = DecisionInput {
            event: tip_msg(20.0),
            affinity,
            persona: persona_with_tip(None),
            signals: base_signals(),
        };
        let plan = decide(&input);
        assert_eq!(
            plan.action_type,
            ActionType::ReplyText,
            "a tip must never be ghosted"
        );
    }

    #[test]
    fn test_tip_reply_style_neutral_when_personality_present() {
        let input = DecisionInput {
            event: tip_msg(20.0),
            affinity: base_affinity(),
            persona: persona_with_tip(Some("傲娇")),
            signals: base_signals(),
        };
        let plan = decide(&input);
        assert_eq!(plan.action_type, ActionType::ReplyText);
        assert_eq!(plan.reply_style, ReplyStyle::Neutral);
    }

    #[test]
    fn test_tip_reply_style_tsundere_when_personality_absent() {
        let input = DecisionInput {
            event: tip_msg(20.0),
            affinity: base_affinity(),
            persona: persona_with_tip(None),
            signals: base_signals(),
        };
        let plan = decide(&input);
        assert_eq!(plan.action_type, ActionType::ReplyText);
        assert_eq!(plan.reply_style, ReplyStyle::Tsundere);
    }

    #[test]
    fn test_ghost_threshold_triggers_ghost_action() {
        let mut affinity = base_affinity();
        affinity.intrigue = 0.05;
        affinity.patience = 0.05;
        affinity.tension = 0.5;

        let input = DecisionInput {
            event: user_msg("."),
            affinity,
            persona: base_persona(),
            signals: base_signals(),
        };
        let plan = decide(&input);
        assert_eq!(plan.action_type, ActionType::Ghost);
    }

    #[test]
    fn test_new_relationship_never_ghosts() {
        let mut affinity = base_affinity();
        affinity.intrigue = 0.05;
        affinity.patience = 0.05;
        affinity.tension = 0.5;

        let mut signals = base_signals();
        signals.message_count = 3; // within protection window

        let input = DecisionInput {
            event: user_msg("hello"),
            affinity,
            persona: base_persona(),
            signals,
        };
        let plan = decide(&input);
        assert_eq!(plan.action_type, ActionType::ReplyText);
    }

    #[test]
    fn test_long_message_predicts_positive_intrigue() {
        let input = DecisionInput {
            event: user_msg(&"deep content".repeat(10)),
            affinity: base_affinity(),
            persona: base_persona(),
            signals: base_signals(),
        };
        let plan = decide(&input);
        assert!(plan.affinity_deltas.intrigue > 0.0);
    }

    #[test]
    fn test_long_absence_penalises_patience() {
        let mut signals = base_signals();
        signals.hours_since_last_message = 48.0;

        let input = DecisionInput {
            event: user_msg("hey"),
            affinity: base_affinity(),
            persona: base_persona(),
            signals,
        };
        let plan = decide(&input);
        assert!(plan.affinity_deltas.patience < 0.0);
    }

    #[test]
    fn test_app_open_does_not_reply_and_has_zero_cost() {
        let input = DecisionInput {
            event: Event::AppOpen,
            affinity: base_affinity(),
            persona: base_persona(),
            signals: base_signals(),
        };
        let plan = decide(&input);
        assert_eq!(plan.action_type, ActionType::Proactive);
        assert_eq!(plan.energy_cost, 0.0);
    }

    #[test]
    fn test_short_msg_and_stale_both_apply_to_patience() {
        let mut signals = base_signals();
        signals.hours_since_last_message = 48.0;
        let input = DecisionInput {
            event: user_msg("k"),
            affinity: base_affinity(),
            persona: base_persona(),
            signals,
        };
        let plan = decide(&input);
        // short penalty (-0.02) + stale penalty (-0.05) = -0.07
        assert!((plan.affinity_deltas.patience - (-0.07)).abs() < 1e-9);
    }

    fn test_decision_input() -> DecisionInput {
        DecisionInput {
            event: user_msg("hello"),
            affinity: base_affinity(),
            persona: base_persona(),
            signals: base_signals(),
        }
    }

    #[test]
    fn plan_for_reply_text_is_neutral_with_hints() {
        let input = test_decision_input();
        let plan = plan_for(
            &input,
            ActionType::ReplyText,
            vec!["有点开心".into()],
            None,
            ImageRef::Face,
            None,
        );
        assert_eq!(plan.action_type, ActionType::ReplyText);
        assert_eq!(plan.reply_style, ReplyStyle::Neutral);
        assert_eq!(plan.context_hints, vec!["有点开心".to_string()]);
        assert_eq!(plan.energy_cost, ENERGY_COST_REPLY);
    }

    #[test]
    fn plan_for_ghost_is_cold_and_drops_hints() {
        let input = test_decision_input();
        let plan = plan_for(
            &input,
            ActionType::Ghost,
            vec!["想躲".into()],
            None,
            ImageRef::Face,
            None,
        );
        assert_eq!(plan.action_type, ActionType::Ghost);
        assert_eq!(plan.reply_style, ReplyStyle::Cold);
        assert!(plan.context_hints.is_empty());
        assert_eq!(plan.energy_cost, ENERGY_COST_GHOST);
    }

    #[test]
    fn plan_for_threads_image_prompt() {
        let input = test_decision_input();
        let plan = plan_for(
            &input,
            ActionType::ReplyTextImage,
            vec![],
            Some("a selfie in a cafe".to_string()),
            ImageRef::Face,
            None,
        );
        assert_eq!(plan.action_type, ActionType::ReplyTextImage);
        assert_eq!(plan.image_prompt.as_deref(), Some("a selfie in a cafe"));

        let ghost = plan_for(
            &input,
            ActionType::Ghost,
            vec![],
            Some("ignored".into()),
            ImageRef::Face,
            None,
        );
        assert_eq!(ghost.image_prompt, None, "ghost carries no image prompt");
    }

    #[test]
    fn plan_for_threads_image_ref_and_aspect() {
        let input = test_decision_input(); // reuse the helper the sibling plan_for tests use
        let plan = plan_for(
            &input,
            ActionType::ReplyImage,
            vec![],
            Some("a subject".into()),
            ImageRef::Previous,
            Some("9:16".into()),
        );
        assert_eq!(plan.image_ref, ImageRef::Previous);
        assert_eq!(plan.aspect_ratio.as_deref(), Some("9:16"));

        // ghost discards image fields → defaults
        let ghost = plan_for(
            &input,
            ActionType::Ghost,
            vec![],
            Some("ignored".into()),
            ImageRef::Previous,
            Some("9:16".into()),
        );
        assert_eq!(ghost.image_ref, ImageRef::Face);
        assert_eq!(ghost.aspect_ratio, None);
    }
}
