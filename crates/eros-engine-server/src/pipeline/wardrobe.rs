// SPDX-License-Identifier: AGPL-3.0-only
//! The wardrobe steward (`[tasks.pde_wardrobe]`): a task beside the judge
//! whose one answer is what she is wearing in the picture about to be drawn.
//! [`dress`] asks it and returns that line for the composer's `[此刻穿着]`;
//! [`carried_clothing`] reads a steward-dressed picture back for the reply
//! prompts of the [`WINDOW_MINUTES`] after it.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use eros_engine_core::persona::CompanionPersona;
use eros_engine_core::types::ItemKind;
use eros_engine_llm::failure::AttemptFailure;
use eros_engine_llm::model_config::{ResolvedWardrobe, WARDROBE_TASK};
use eros_engine_store::image_events::{ImageComposeEventRepo, LastWorn};

use super::stream::{filter_timeout_failure, sanitize_inner_state, FILTER_TIMEOUT};
use crate::prompt::{item_list, meta_str, persona_local_time, NowContext};
use crate::state::AppState;

/// How long after a picture its clothing counts as what she last wore: the
/// steward's `[上次穿着]`, and the outfit later reply prompts carry.
pub(crate) const WINDOW_MINUTES: i32 = 45;

/// The body the payload writes for a section that has nothing.
const NONE: &str = "（无）";

/// The turn a picture is drawn in: its `[now]` context, and the user message
/// it answers (`None` for a turn that answers none, such as an event
/// message). The last-picture read is bounded above by that message's
/// `sent_at`, else by `now.now`.
#[derive(Debug, Clone, Default)]
pub(crate) struct PictureTurn {
    pub now: NowContext,
    pub driving_message: Option<Uuid>,
}

/// What [`dress`] concluded: the line for `[此刻穿着]` (`None` when the chain
/// gave no answer), the `inputs.wardrobe` audit object, and the failed
/// attempts under the task name `pde_wardrobe`.
pub(crate) struct Dressed {
    pub clothing: Option<String>,
    pub audit: serde_json::Value,
    pub failures: Vec<AttemptFailure>,
}

/// The clothing of her last picture inside [`WINDOW_MINUTES`] before the
/// turn's moment. A failed read is warned and reads as none.
pub(crate) async fn last_worn(
    pool: &sqlx::PgPool,
    instance_id: Uuid,
    driving_message: Option<Uuid>,
    now: DateTime<Utc>,
) -> Option<LastWorn> {
    let repo = ImageComposeEventRepo { pool };
    match repo
        .last_worn(instance_id, driving_message, now, WINDOW_MINUTES)
        .await
    {
        Ok(worn) => worn,
        Err(e) => {
            tracing::warn!(instance = %instance_id, error = %e, "wardrobe: last-worn read failed");
            None
        }
    }
}

/// The steward's user message: each section as the composer's payload writes
/// its own (`[label]`, a newline, the body, a blank line), `（无）` for an
/// empty body. `[此刻状态]`, `[最近场景]` and `[对方最新消息]` are the
/// composer's own sections, verbatim; `[此刻状态]` is absent when the
/// composer's is. Pure.
pub(crate) fn steward_payload(
    persona: &CompanionPersona,
    worn: Option<&LastWorn>,
    current_state: Option<&str>,
    now: &NowContext,
    recent_scene: &str,
    latest_user_msg: &str,
) -> String {
    let state = present(current_state)
        .map(|s| format!("[此刻状态]\n{s}\n\n"))
        .unwrap_or_default();
    format!(
        "[人物外观]\n{appearance}\n\n[穿搭设定]\n{brief}\n\n[衣柜]\n{items}\n\n\
         [上次穿着]\n{last_worn}\n\n{state}[此刻时间]\n{now}\n\n\
         [最近场景]\n{scene}\n\n[对方最新消息]\n{latest}",
        appearance = present(meta_str(persona, "appearance")).unwrap_or(NONE),
        brief = brief(persona).unwrap_or(NONE),
        items = items(persona).as_deref().unwrap_or(NONE),
        last_worn = worn.map(worn_line).as_deref().unwrap_or(NONE),
        now = persona_local_time(persona, now),
        scene = verbatim(recent_scene),
        latest = verbatim(latest_user_msg),
    )
}

/// The `inputs.wardrobe` audit object: `status`, `generation_id` when an
/// attempt answered, and the payload sections the picture's row does not
/// already hold, each as sent, the empty string where the payload wrote
/// `（无）`. Pure.
fn steward_audit(
    persona: &CompanionPersona,
    worn: Option<&LastWorn>,
    now: &NowContext,
    status: &str,
    generation_id: Option<&str>,
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "status": status,
        "brief": brief(persona).unwrap_or(""),
        "items": items(persona).unwrap_or_default(),
        "last_worn": worn.map(worn_line).unwrap_or_default(),
        "now": persona_local_time(persona, now),
    });
    if let Some(id) = generation_id {
        v["generation_id"] = id.into();
    }
    v
}

/// `[穿搭设定]`: the card's `art_metadata.clothing`; a value of the wrong
/// shape reads as absent.
fn brief(persona: &CompanionPersona) -> Option<&str> {
    present(meta_str(persona, "clothing"))
}

/// `[衣柜]`: her clothing items, as `[你的衣柜]` lists them.
fn items(persona: &CompanionPersona) -> Option<String> {
    item_list(persona, ItemKind::Clothing)
}

/// `[上次穿着]`: 「{clothing}（{n} 分钟前）」.
fn worn_line(w: &LastWorn) -> String {
    format!("{}（{} 分钟前）", w.clothing, w.minutes)
}

/// Ask the steward what she is wearing. One attempt per model down
/// `[model] + fallback`, each under `FILTER_TIMEOUT`: a transport error or a
/// timeout is recorded as a failure and moves on, and so does an answer that
/// is empty once sanitized. Each answered attempt is recorded in
/// `llm_generations`. Never fails: a chain without an answer is
/// `clothing: None` with the audit status `exhausted`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dress(
    state: &AppState,
    c: &ResolvedWardrobe,
    persona: &CompanionPersona,
    session_id: Uuid,
    turn: &PictureTurn,
    current_state: Option<&str>,
    recent_scene: &str,
    latest_user_msg: &str,
) -> Dressed {
    use eros_engine_llm::openrouter::{ChatMessage, ChatRequest};
    let worn = last_worn(
        &state.pool,
        persona.instance.id,
        turn.driving_message,
        turn.now.now,
    )
    .await;
    let payload = steward_payload(
        persona,
        worn.as_ref(),
        current_state,
        &turn.now,
        recent_scene,
        latest_user_msg,
    );
    let audit = |status, generation_id: Option<&str>| {
        steward_audit(persona, worn.as_ref(), &turn.now, status, generation_id)
    };
    let mut failures = Vec::new();
    // The generation id of the last attempt that answered.
    let mut generation_id: Option<String> = None;
    for model_id in std::iter::once(&c.model).chain(&c.fallback_model) {
        let req = ChatRequest {
            model: model_id.clone(),
            fallback_model: vec![],
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: c.steward_prompt.clone(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: payload.clone(),
                },
            ],
            temperature: c.temperature as f32,
            max_tokens: c.max_tokens,
            sampling: c.sampling,
            task: Some(WARDROBE_TASK.into()),
            ..Default::default()
        };
        let resp = match tokio::time::timeout(FILTER_TIMEOUT, state.openrouter.execute(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::warn!(model = %model_id, error = %e, "wardrobe: model error; next");
                failures.push(AttemptFailure::from_llm_error(WARDROBE_TASK, model_id, &e));
                continue;
            }
            Err(_) => {
                tracing::warn!(model = %model_id, "wardrobe: timeout; next");
                failures.push(filter_timeout_failure(WARDROBE_TASK, model_id));
                continue;
            }
        };
        generation_id = super::record_generation(
            &state.pool,
            super::GenerationRecord {
                task: WARDROBE_TASK,
                session_id: Some(session_id),
                generation_id: resp.generation_id.as_deref(),
                model: resp.model.as_deref(),
                usage: resp.usage.as_ref(),
            },
        )
        .await;
        let clothing = sanitize_inner_state(&resp.reply);
        if clothing.is_empty() {
            tracing::warn!(model = %model_id, "wardrobe: empty answer; next");
            continue;
        }
        return Dressed {
            clothing: Some(clothing),
            audit: audit("ok", generation_id.as_deref()),
            failures,
        };
    }
    Dressed {
        clothing: None,
        audit: audit("exhausted", generation_id.as_deref()),
        failures,
    }
}

/// The steward's outfit a reply prompt carries: the clothing of her last
/// picture inside [`WINDOW_MINUTES`], when the steward dressed that picture.
/// `None` with the steward off, past the window, or when the newest picture
/// in it was dressed by anything else.
pub(crate) async fn carried_clothing(
    state: &AppState,
    instance_id: Uuid,
    driving_message: Option<Uuid>,
    now: DateTime<Utc>,
) -> Option<String> {
    if !state.model_config.wardrobe_enabled() {
        return None;
    }
    last_worn(&state.pool, instance_id, driving_message, now)
        .await
        .filter(|w| w.source.as_deref() == Some("steward"))
        .map(|w| w.clothing)
}

/// A trimmed, non-blank string, else `None`.
fn present(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// The composer's rule for `[最近场景]` and `[对方最新消息]`: the text as
/// given, `（无）` when blank.
fn verbatim(s: &str) -> &str {
    if s.trim().is_empty() {
        NONE
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eros_engine_core::persona::{PersonaGenome, PersonaInstance, PersonaItem};

    /// A persona whose `art_metadata` is `art`.
    fn test_persona_with_art(art: serde_json::Value) -> CompanionPersona {
        let iid = Uuid::new_v4();
        let gid = Uuid::new_v4();
        CompanionPersona {
            instance_id: iid,
            genome: PersonaGenome {
                id: Some(gid),
                name: "TestPersona".into(),
                system_prompt: "You are TestPersona.".into(),
                tip_personality: None,
                art_metadata: art,
            },
            instance: PersonaInstance {
                id: iid,
                genome_id: Some(gid),
                owner_uid: Uuid::new_v4(),
                status: "active".into(),
            },
            items: vec![],
        }
    }

    /// A persona whose `art_metadata` holds these string keys.
    fn test_persona_with_meta(pairs: &[(&str, &str)]) -> CompanionPersona {
        test_persona_with_art(serde_json::Value::Object(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
                .collect(),
        ))
    }

    fn taipei_evening() -> NowContext {
        NowContext {
            now: "2026-11-28T13:05:00Z".parse().unwrap(),
            ..Default::default()
        }
    }

    #[test]
    fn payload_has_every_section_in_order() {
        let p = test_persona_with_meta(&[
            ("appearance", "auburn hair"),
            ("clothing", "平时穿白衬衫配百褶裙"),
            ("timezone", "Asia/Taipei"),
        ]);
        let worn = LastWorn {
            clothing: "米色开衫".into(),
            source: Some("steward".into()),
            minutes: 10,
        };
        let s = steward_payload(
            &p,
            Some(&worn),
            Some("短途旅游；在坐火车"),
            &taipei_evening(),
            "SCENE",
            "LATEST",
        );
        assert_eq!(
            s,
            "[人物外观]\nauburn hair\n\n[穿搭设定]\n平时穿白衬衫配百褶裙\n\n[衣柜]\n（无）\n\n\
             [上次穿着]\n米色开衫（10 分钟前）\n\n[此刻状态]\n短途旅游；在坐火车\n\n\
             [此刻时间]\n2026-11-28（周六）21:05，傍晚\n\n[最近场景]\nSCENE\n\n[对方最新消息]\nLATEST"
        );
    }

    #[test]
    fn payload_writes_none_for_empty_sections_and_drops_the_state() {
        let p = test_persona_with_meta(&[]);
        let s = steward_payload(&p, None, None, &NowContext::default(), "  ", "");
        for label in [
            "人物外观",
            "穿搭设定",
            "衣柜",
            "上次穿着",
            "最近场景",
            "对方最新消息",
        ] {
            assert!(s.contains(&format!("[{label}]\n（无）")), "{label}: {s}");
        }
        assert!(!s.contains("[此刻状态]"));
    }

    #[test]
    fn payload_reads_a_wrong_shaped_brief_as_absent() {
        let p = test_persona_with_art(serde_json::json!({ "clothing": ["a", "b"] }));
        let s = steward_payload(&p, None, None, &NowContext::default(), "S", "L");
        assert!(s.contains("[穿搭设定]\n（无）"));
    }

    #[test]
    fn payload_lists_her_clothing_items() {
        let mut p = test_persona_with_meta(&[]);
        p.items = vec![
            PersonaItem {
                kind: ItemKind::Clothing,
                name: "红色旗袍".into(),
                gifted: true,
            },
            PersonaItem {
                kind: ItemKind::Item,
                name: "吉他".into(),
                gifted: false,
            },
        ];
        let s = steward_payload(&p, None, None, &NowContext::default(), "S", "L");
        assert!(s.contains("[衣柜]\n「红色旗袍」（对方送的）\n\n"), "{s}");
    }

    #[test]
    fn audit_holds_the_sections_as_sent() {
        let mut p =
            test_persona_with_meta(&[("clothing", " 只穿裙子 "), ("timezone", "Asia/Taipei")]);
        p.items = vec![PersonaItem {
            kind: ItemKind::Clothing,
            name: "白色开衫".into(),
            gifted: false,
        }];
        let worn = LastWorn {
            clothing: "毛衣".into(),
            source: None,
            minutes: 10,
        };
        let now = taipei_evening();
        let a = steward_audit(&p, Some(&worn), &now, "ok", Some("gen_1"));
        assert_eq!(
            a,
            serde_json::json!({
                "status": "ok",
                "generation_id": "gen_1",
                "brief": "只穿裙子",
                "items": "「白色开衫」",
                "last_worn": "毛衣（10 分钟前）",
                "now": "2026-11-28（周六）21:05，傍晚",
            })
        );
        let s = steward_payload(&p, Some(&worn), None, &now, "S", "L");
        for (label, key) in [
            ("穿搭设定", "brief"),
            ("衣柜", "items"),
            ("上次穿着", "last_worn"),
            ("此刻时间", "now"),
        ] {
            let body = a[key].as_str().unwrap();
            assert!(
                s.contains(&format!("[{label}]\n{body}\n\n")),
                "{label}: {s}"
            );
        }
    }

    #[test]
    fn audit_writes_the_empty_string_for_none_and_no_generation_id_when_none_answered() {
        let p = test_persona_with_meta(&[]);
        let a = steward_audit(&p, None, &taipei_evening(), "exhausted", None);
        assert_eq!(a["status"], "exhausted");
        assert!(a.get("generation_id").is_none(), "{a}");
        for key in ["brief", "items", "last_worn"] {
            assert_eq!(a[key], "", "{key}: {a}");
        }
    }
}
