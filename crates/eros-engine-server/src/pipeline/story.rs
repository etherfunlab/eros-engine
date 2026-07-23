// SPDX-License-Identifier: AGPL-3.0-only
//! World Stories director (spec: docs/superpowers/specs/2026-07-23-world-stories-design.md §3).
//!
//! Per claimed instance: one structured LLM round — persona canon + current
//! insight + recent events + affinity snapshot + 7d chat evidence → full
//! insight replacement + digest + life events (1:1 embedded as recall
//! memories). Runs as the second phase of the world sweeper tick.

use serde::Deserialize;
use uuid::Uuid;

use eros_engine_store::story::{StoryAffinity, StoryInsight, StoryInsightRow, StoryPersona};

// NOTE(task-7 scaffold): `eros_engine_llm::model_config::ResolvedWorldStories`,
// `eros_engine_llm::openrouter::{ChatMessage, ChatRequest}`,
// `eros_engine_store::story::{StoryEventInsert, StoryRepo}`, and
// `crate::state::AppState` are the runner's (Task 8) dependencies. They are
// intentionally not imported here — this module is the pure layer only — and
// will be added back when the runner (`sweeper`/`direct_story`) lands.

// NOTE(task-7 scaffold): only the pure layer lands in this task — the
// runner (Task 8: `sweeper`/`direct_story`) is the real consumer of most of
// what follows. In this crate's binary target (no lib — see Cargo.toml),
// items reachable only from `#[cfg(test)]` still read as dead code in the
// non-test build, so each is individually `#[allow(dead_code)]`d until Task
// 8 wires it in; none of the allows suppress anything the unit tests below
// don't already exercise.
#[allow(dead_code)]
const STORY_TASK: &str = "world_stories_director";
/// Sentinel OpenRouter `user` for story-subsystem calls. dreaming=…111,
/// world=…112 — stories continue the sequence for per-subsystem attribution.
#[allow(dead_code)]
pub(crate) const STORY_AUDIT_USER: &str = "11111111-1111-1111-1111-111111111113";
#[allow(dead_code)]
const STORY_PICK_BATCH: i64 = 8;
#[allow(dead_code)]
const STORY_CLAIM_STALE: std::time::Duration = std::time::Duration::from_secs(1800);
/// Defensive cap on events accepted per round.
#[allow(dead_code)]
const STORY_EVENTS_CAP: usize = 6;
/// Recent events fed back for continuity + repetition guard.
#[allow(dead_code)]
const STORY_RECENT_EVENTS: i64 = 12;
/// Max chat turns fed as evidence (inside the context_days window).
#[allow(dead_code)]
const STORY_CHAT_TURNS_CAP: u8 = 60;

/// Fixed engine-owned rules (spec §3.3). The operator filter_prompt carries
/// tone / genre / category vocabulary / per-field richness; these are the floor.
#[allow(dead_code)]
const STORY_DIRECTOR_RULES: &str = "规则：\
1) 用户在场：感情线应当包含用户，但用户的言行只能取自聊天记录，绝不编造用户做过的事或说过的话。\
2) 关系定性以聊天记录为准（例：用户明确告白且角色答应，才能视为情侣）；亲密度数值仅供参考。\
3) insight 是人生底座：只输出固定 schema 中的栏位，不要新增/改名。首轮先把 backstory 烤入再丰富；\
backstory 是 canon，不可与之冲突。每轮输出更新后的完整 insight（全量替换）。\
4) 经历类内容用相对时间（n年前/n个月前/n天前）和人生阶段（x岁时、上大学时）记录；\
每轮根据 current_time 刷新相对时间表述。\
5) events：当期发生的具体生活事件（工作进展、感情进展、生活进展等，类目见系统指示），\
每条一句、自成一体、适合单独召回；避免与近期事件重复。\
6) digest：1-2 句该角色当前人生近况。";

/// Persona-side life-base schema: superset of COMPANION_INSIGHTS_SCHEMA
/// (prompt.rs), reworded for the persona, flat (matching preferences
/// pre-flattened). The field LIST is the engine's contract — the operator
/// filter_prompt may steer richness, never the list. Kept in lockstep with
/// `STORY_INSIGHT_FIELDS` + the 0038 DDL (tested).
#[allow(dead_code)]
pub const PERSONA_STORY_INSIGHTS_SCHEMA: &str = r#"
persona_story_insights schema（角色人生底座；所有字段可选；只输出下列字段，不要新增/编造字段名）：
{
  "city": "string — 角色常住城市，写出具体地点与生活痕迹",
  "location": "string — 此刻/近期所在地（出差、旅行），仅当明显不同于常住城市才填",
  "hometown": "string — 老家 / 出生成长地",
  "nationality": "string — 国籍/地区身份",
  "occupation": "string — 当前职业与工作状态，写出行业/日常细节",
  "mbti_guess": "string — MBTI（角色自认或由性格推断，带推测措辞）",
  "love_values": "string — 对爱情/亲密关系的态度与期待，一两句具体总结",
  "emotional_needs": "string — 角色需要什么样的情感支持",
  "life_rhythm": "string — 作息与生活节奏的具体模式",
  "education": "string — 教育背景：学历/学校/专业",
  "family": "string — 家庭结构：成员、婚育状况概况",
  "relationship_history": "string — 感情经历概况：过往恋情、单身多久等，一两句总结",
  "social_pattern": "string — 社交模式：独处/聚会倾向、朋友圈子状态",
  "future_plans": "string — 近期目标、人生方向、正在筹划的事",
  "finance_status": "string — 收入水平/消费习惯/经济压力",
  "interests": ["array of strings — 兴趣爱好，每项 4~12 个汉字的具体短语"],
  "personality_traits": ["array of strings — 性格特质，每项带依据/情境"],
  "preferred_gender": "string — 感情上偏好的对象性别",
  "age_min": 0,
  "age_max": 0,
  "deal_breakers": ["array of strings — 感情里无法接受的点"],
  "work_history": "string — 工作经历：历任工作与转折，用相对时间/人生阶段锚定（如「3年前」「上大学时」）",
  "romance_history": "string — 感情史：过往恋情如何开始与结束，用相对时间锚定",
  "family_of_origin": "string — 与原生家庭的关系：现状与渊源，不只写结构",
  "user_relationship": "string — 与用户的当前关系状态；必须以聊天记录为据（规则 2），不可臆测升级"
}
"#;

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // fields read by Task 8's persist_round call; tests only check a subset
struct StoryOutput {
    insight: StoryInsight,
    digest: String,
    #[serde(default)]
    events: Vec<StoryEventOut>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // consumed by Task 8's cap_events / persist_round wiring
struct StoryEventOut {
    category: String,
    content: String,
}

/// Assemble the story director's user message.
#[allow(dead_code)]
fn story_user_payload(
    now: chrono::DateTime<chrono::Utc>,
    persona: &StoryPersona,
    row: &StoryInsightRow,
    recent_events: &[(String, String)],
    affinity: Option<&StoryAffinity>,
    chat_pairs: &[(String, String)],
) -> String {
    let is_init = row.last_run_at.is_none();
    let header = if is_init {
        "初始化这个角色的人生：先把 backstory 烤入 insight 底座，再补足其余栏位的丰富度，并生成当期 events。"
    } else {
        "延续这个角色的人生：在现有 insight 与近期 events 的基础上推进生活，输出更新后的完整 insight 与当期 events。"
    };
    let affinity_json = affinity.map(|a| {
        serde_json::json!({
            "warmth": a.warmth, "trust": a.trust, "intrigue": a.intrigue,
            "intimacy": a.intimacy, "patience": a.patience, "tension": a.tension,
            "bond": a.bond, "chemistry": a.chemistry,
            "relationship_label": a.relationship_label,
        })
    });
    let chat: Vec<serde_json::Value> = chat_pairs
        .iter()
        .map(|(u, a)| serde_json::json!({"用户": u, "角色": a}))
        .collect();
    let data = serde_json::json!({
        "current_time": now.to_rfc3339(),
        "persona": {
            "name": persona.name,
            "personality": persona.tip_personality,
            "profile": persona.art_metadata,
        },
        "current_insight": if is_init { serde_json::Value::Null } else {
            serde_json::to_value(&row.insight).unwrap_or_default()
        },
        "current_digest": if is_init { serde_json::Value::Null } else {
            serde_json::Value::String(row.digest.clone())
        },
        "recent_events": recent_events
            .iter()
            .map(|(cat, c)| serde_json::json!({"category": cat, "content": c}))
            .collect::<Vec<_>>(),
        "affinity_reference": affinity_json,
        "recent_chat": chat,
    });
    format!(
        "{header}\n\n{}\n\n{PERSONA_STORY_INSIGHTS_SCHEMA}\n{STORY_DIRECTOR_RULES}",
        serde_json::to_string_pretty(&data).unwrap_or_default()
    )
}

/// OpenRouter response_format for the story round. strict=false — `insight`
/// keys are validated engine-side by the typed deserialize.
#[allow(dead_code)]
fn story_response_format() -> serde_json::Value {
    serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "world_stories_round",
            "strict": false,
            "schema": {
                "type": "object",
                "required": ["insight", "digest", "events"],
                "properties": {
                    "insight": { "type": "object" },
                    "digest": { "type": "string" },
                    "events": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["category", "content"],
                            "properties": {
                                "category": { "type": "string" },
                                "content": { "type": "string" }
                            }
                        }
                    }
                }
            }
        }
    })
}

/// Lenient parse: direct JSON, then the shared balanced-brace extractor.
/// Warns (once per round) on unknown insight keys — the fixed field list is
/// the contract; unknown keys are dropped by the typed deserialize.
#[allow(dead_code)]
fn parse_story_output(raw: &str) -> Option<StoryOutput> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .ok()
        .or_else(|| super::find_json_block(raw).and_then(|b| serde_json::from_str(b).ok()))?;
    if let Some(obj) = value.get("insight").and_then(|v| v.as_object()) {
        let unknown: Vec<&str> = obj
            .keys()
            .map(String::as_str)
            .filter(|k| !eros_engine_store::story::STORY_INSIGHT_FIELDS.contains(k))
            .collect();
        if !unknown.is_empty() {
            tracing::warn!(?unknown, "story: unknown insight keys dropped");
        }
    }
    serde_json::from_value::<StoryOutput>(value).ok()
}

/// Trim, drop blanks, cap at STORY_EVENTS_CAP (warn on truncation).
#[allow(dead_code)]
fn cap_events(events: Vec<StoryEventOut>, instance: Uuid) -> Vec<StoryEventOut> {
    let mut out: Vec<StoryEventOut> = events
        .into_iter()
        .map(|e| StoryEventOut {
            category: e.category.trim().to_string(),
            content: e.content.trim().to_string(),
        })
        .filter(|e| !e.content.is_empty())
        .collect();
    if out.len() > STORY_EVENTS_CAP {
        tracing::warn!(%instance, cap = STORY_EVENTS_CAP, "story: events truncated");
        out.truncate(STORY_EVENTS_CAP);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use eros_engine_store::story::{
        StoryInsight, StoryInsightRow, StoryPersona, STORY_INSIGHT_FIELDS,
    };

    fn fixture_persona() -> StoryPersona {
        StoryPersona {
            name: "Aria".into(),
            tip_personality: Some("温柔".into()),
            art_metadata: serde_json::json!({"backstory": "咖啡店店主"}),
        }
    }

    fn fixture_row(run_before: bool) -> StoryInsightRow {
        StoryInsightRow {
            insight: StoryInsight::default(),
            digest: if run_before {
                "近况".into()
            } else {
                String::new()
            },
            insight_version: 1,
            last_run_at: run_before.then(chrono::Utc::now),
        }
    }

    #[test]
    fn schema_constant_covers_every_column() {
        for f in STORY_INSIGHT_FIELDS {
            assert!(
                PERSONA_STORY_INSIGHTS_SCHEMA.contains(&format!("\"{f}\"")),
                "schema constant must describe {f}"
            );
        }
    }

    #[test]
    fn payload_flags_init_vs_continuation_and_carries_rules() {
        let init = story_user_payload(
            chrono::Utc::now(),
            &fixture_persona(),
            &fixture_row(false),
            &[],
            None,
            &[],
        );
        assert!(init.contains("初始化这个角色的人生"));
        assert!(init.contains("咖啡店店主"), "backstory in payload");
        assert!(init.contains("用户在场"), "rule 1");
        assert!(init.contains("关系定性以聊天记录为准"), "rule 2");
        assert!(init.contains("相对时间"), "rule 4");
        assert!(init.contains("current_time"), "clock present");

        let cont = story_user_payload(
            chrono::Utc::now(),
            &fixture_persona(),
            &fixture_row(true),
            &[("work".into(), "上周修好了咖啡机".into())],
            None,
            &[("最近好吗".into(), "在忙开店".into())],
        );
        assert!(cont.contains("延续这个角色的人生"));
        assert!(cont.contains("上周修好了咖啡机"));
        assert!(cont.contains("最近好吗"));
    }

    #[test]
    fn story_response_format_shape() {
        let v = story_response_format();
        assert_eq!(v["type"], "json_schema");
        assert_eq!(v["json_schema"]["strict"], false);
        let required = v["json_schema"]["schema"]["required"].as_array().unwrap();
        for k in ["insight", "digest", "events"] {
            assert!(required.iter().any(|r| r == k));
        }
    }

    #[test]
    fn parse_story_output_clean_fenced_and_garbage() {
        let clean = r#"{"insight":{"occupation":"店主"},"digest":"d","events":[{"category":"work","content":"c"}]}"#;
        let out = parse_story_output(clean).expect("clean parses");
        assert_eq!(out.insight.occupation.as_deref(), Some("店主"));
        assert_eq!(out.events.len(), 1);
        let fenced = format!("好的：\n```json\n{clean}\n```");
        assert!(parse_story_output(&fenced).is_some());
        assert!(parse_story_output("nope").is_none());
        assert!(
            parse_story_output(r#"{"digest":"d"}"#).is_none(),
            "missing insight ⇒ None"
        );
    }

    #[test]
    fn events_truncated_at_cap() {
        let events: Vec<String> = (0..10).map(|i| format!("e{i}")).collect();
        let json = serde_json::json!({
            "insight": {},
            "digest": "d",
            "events": events.iter().map(|c| serde_json::json!({"category":"life","content":c})).collect::<Vec<_>>()
        });
        let out = parse_story_output(&json.to_string()).unwrap();
        let capped = cap_events(out.events, uuid::Uuid::nil());
        assert_eq!(capped.len(), STORY_EVENTS_CAP);
        assert_eq!(capped[0].content, "e0");
    }

    #[test]
    fn story_audit_user_is_distinct() {
        assert_eq!(STORY_AUDIT_USER, "11111111-1111-1111-1111-111111111113");
        assert_ne!(STORY_AUDIT_USER, crate::pipeline::world::WORLD_AUDIT_USER);
    }
}
