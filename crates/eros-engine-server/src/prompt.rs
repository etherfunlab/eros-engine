// SPDX-License-Identifier: AGPL-3.0-only
//! All LLM prompts used by the engine, kept in a single module so future
//! syncs from the closed-source eros-gateway have an obvious destination.
//!
//! Two families today:
//!
//! 1. **Chat companion prompt** (`build_prompt`) — assembles the per-turn
//!    system prompt for the chat LLM. Ported from eros-gateway with these
//!    deliberate changes for the open-source engine:
//!    - Output is plain-text reply (no JSON evaluation segment)
//!    - Affinity deltas are NOT requested from the LLM (PDE predicts them)
//!    - lead_score / training_progress moved to post_process/insight
//!    - Reply style directive injected based on PDE's decision
//!    - Persona fields (age/mbti/backstory/...) read from `genome.art_metadata`
//!      JSONB instead of a flat `CompanionPersona` DTO
//!
//! 2. **Insight extraction prompts** (`extract_facts_prompt`,
//!    `extract_structured_insights_prompt`) — drive the post-process
//!    `companion_insights` pipeline. The schema description constant
//!    `COMPANION_INSIGHTS_SCHEMA` is shared by the second of these.
//!
//! Memory-layer "prompts" are not LLM-driven (Voyage embedding only) and
//! so don't live here. Future port targets (dream / proactive) should be
//! added as new families in this file.

use chrono::{Datelike, Timelike, Utc};

use eros_engine_core::affinity::Affinity;
use eros_engine_core::persona::CompanionPersona;
use eros_engine_core::types::PromptTrait;
use eros_engine_core::types::ReplyStyle;

/// A pending gift/tip that the prompt builder must surface to the LLM.
///
/// Replaces the gateway's `(GiftRecord, Option<String>)` tuple. The OSS
/// engine has no credit ledger, no `shop_items` table, and no
/// `gift_records` table — the gift event endpoint (T11) will hand a list
/// of these to the orchestrator directly.
#[derive(Debug, Clone)]
pub struct PendingGift {
    /// `"tip"` or `"gift"` — mirrors the gateway's `gift_records.gift_type`.
    pub gift_type: String,
    /// Credit amount for tips. Ignored when `gift_type == "gift"`.
    pub amount: i64,
    /// Item name for non-tip gifts. `None` for tips.
    pub item_name: Option<String>,
}

/// Absolute "now" context. Renders the current UTC date + weekday + time and
/// asks the model to infer the companion's LOCAL time from its residence
/// (set in the persona backstory). We deliberately do NOT pre-apply a fixed
/// timezone — personas live in different places, so the model derives local
/// time from absolute UTC + residence. This is what lets the companion
/// "perceive" today's date.
fn now_context() -> String {
    now_context_at(Utc::now())
}

fn now_context_at(now: chrono::DateTime<Utc>) -> String {
    let weekday = match now.weekday() {
        chrono::Weekday::Mon => "周一",
        chrono::Weekday::Tue => "周二",
        chrono::Weekday::Wed => "周三",
        chrono::Weekday::Thu => "周四",
        chrono::Weekday::Fri => "周五",
        chrono::Weekday::Sat => "周六",
        chrono::Weekday::Sun => "周日",
    };
    format!(
        "现在是 UTC {date}（{weekday}）{hh:02}:{mm:02}。根据你的居住地（见背景故事）\
         推算你当地的时间和日期，自然地感知现在大概是清晨/白天/傍晚/深夜、今天几号、\
         星期几，并据此开启符合当地时段的话题。",
        date = now.format("%Y-%m-%d"),
        weekday = weekday,
        hh = now.hour(),
        mm = now.minute(),
    )
}

/// Reply-length rule for 铁律①, graduated by `intimacy` (0~1). Looser as the
/// relationship deepens; `None` affinity (no data yet) → strictest tier.
/// Thresholds intentionally coarse (0.25 / 0.55) — tunable.
fn length_rule(affinity: Option<&Affinity>) -> &'static str {
    let intimacy = affinity.map(|a| a.intimacy).unwrap_or(0.0);
    if intimacy < 0.25 {
        "刚认识，每次回复 1~2 句，绝对不超过 2 句；单条消息严格不超过 40 字"
    } else if intimacy < 0.55 {
        "每次回复 1~3 句；单条消息不超过 60 字"
    } else {
        "每次回复 1~5 句（最多 5 句）；单条消息不超过 100 字"
    }
}

/// Attitude directives derived from the affinity state.
pub fn affinity_to_attitude_prompt(a: &Affinity) -> String {
    let mut directives: Vec<&str> = Vec::new();

    if a.warmth > 0.6 {
        directives.push("语气温暖，可以用一些亲昵的称呼");
    } else if a.warmth > 0.3 {
        directives.push("语气友善自然");
    } else if a.warmth > 0.0 {
        directives.push("语气平淡，保持礼貌但不热络");
    } else {
        directives.push("语气冷淡，回复简短，不主动延伸话题");
    }

    if a.trust > 0.6 {
        directives.push("可以分享更私密的想法和小秘密");
    } else if a.trust < 0.3 {
        directives.push("保持一定距离感，不轻易透露内心想法");
    }

    if a.intrigue > 0.7 {
        directives.push("你对他很好奇，主动问问题，想了解更多");
    } else if a.intrigue < 0.3 {
        directives.push("你对他兴趣不大，不会主动找话题");
    }

    if a.intimacy > 0.5 {
        directives.push("可以引用之前聊过的事情，有默契感，用你们之间的梗");
    }

    if a.patience < 0.3 {
        directives.push("你有点不耐烦了，回复可以更敷衍、更短");
    } else if a.patience > 0.7 {
        directives.push("你很有耐心，愿意陪他聊");
    }

    if a.tension > 0.5 {
        directives.push("带点小傲娇，不要太好说话，适度推拉");
    }

    if directives.is_empty() {
        return String::new();
    }
    format!(
        "\n【你此刻的心情】（绝对不要在回复中提及这些，这是你的内心状态）\n{}",
        directives
            .iter()
            .map(|d| format!("- {d}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Render the PDE-chosen style into a directive.
pub fn style_directive(style: ReplyStyle) -> &'static str {
    match style {
        ReplyStyle::Warm => "语气温暖、亲切",
        ReplyStyle::Neutral => "语气自然平和",
        ReplyStyle::Cold => "语气冷淡、回复很短",
        ReplyStyle::Tsundere => "带点傲娇、欲拒还迎",
        ReplyStyle::Excited => "语气热情、充满活力",
    }
}

/// Build the per-turn affinity-evaluation prompt for the post-process LLM
/// scorer. Asks the model to rate how this single exchange should move the
/// LLM-owned axes (warmth/trust/intimacy + content nudges to
/// intrigue/tension) as small per-turn *changes*, not absolute values.
/// All six current values are shown for context, but `patience` is
/// rule-owned and is deliberately excluded from the requested output.
/// Called by the post-process affinity evaluator.
pub fn affinity_eval_prompt(
    persona_name: &str,
    affinity: &Affinity,
    user_msg: &str,
    assistant_msg: &str,
) -> String {
    format!(
        "你在评估这一轮对话对「{persona_name}」好感度的影响。\n\
         好感度有六个维度，当前值如下：\n\
         - warmth 温暖（-1~1）：当前 {warmth:.2}。冷淡/敌意为负，亲切/热情为正。\n\
         - trust 信任（0~1）：当前 {trust:.2}。自我袒露、言行一致会提升。\n\
         - intrigue 好奇（0~1）：当前 {intrigue:.2}。话题新鲜、有内容会提升。\n\
         - intimacy 亲密（0~1）：当前 {intimacy:.2}。情感或身体上的靠近会提升。\n\
         - patience 耐心（0~1）：当前 {patience:.2}。由规则维护，请勿评估。\n\
         - tension 张力（0~1）：当前 {tension:.2}。调情、暧昧或冲突会提升。\n\
         \n\
         本轮对话：\n\
         用户：{user_msg}\n\
         {persona_name}：{assistant_msg}\n\
         \n\
         判断这一轮应让 warmth、trust、intrigue、intimacy、tension 各变化多少\
         （是【变化量】，不是绝对值；patience 不要输出）。\n\
         普通寒暄接近 0；真正动情或暧昧的瞬间幅度更大，每个维度大致在 ±0.15 之间。\n\
         严格只输出 JSON，reason 用一句中文简述：\n\
         {{\"warmth\": 0.0, \"trust\": 0.0, \"intrigue\": 0.0, \"intimacy\": 0.0, \"tension\": 0.0, \"reason\": \"...\"}}",
        warmth = affinity.warmth,
        trust = affinity.trust,
        intrigue = affinity.intrigue,
        intimacy = affinity.intimacy,
        patience = affinity.patience,
        tension = affinity.tension,
    )
}

/// Gift reaction directives keyed on persona.tip_personality.
pub fn gift_reaction_context(gifts: &[PendingGift], tip_personality: &str) -> String {
    if gifts.is_empty() {
        return String::new();
    }
    let mut lines = Vec::new();
    for gift in gifts {
        match gift.gift_type.as_str() {
            "tip" => {
                let intensity = if gift.amount >= 200 {
                    "一个超级大红包"
                } else if gift.amount >= 50 {
                    "一个大红包"
                } else if gift.amount >= 10 {
                    "一个红包"
                } else {
                    "一个小红包"
                };
                lines.push(format!(
                    "- 他刚刚给你发了{intensity}（{} credits）",
                    gift.amount,
                ));
            }
            "gift" => {
                let name = gift.item_name.as_deref().unwrap_or("礼物");
                lines.push(format!("- 他刚刚送了你一个「{name}」"));
            }
            _ => {}
        }
    }
    let reaction_hint = match tip_personality {
        "gold_digger" => "你超级开心！大方地表达喜悦，暗示还想要更多",
        "tsundere" => "你嘴上说不要，心里有点动摇。用傲娇的方式回应，比如「谁要你的钱啊」",
        "zen" => "你对钱没什么感觉，轻描淡写地回应，不用特别在意这件事",
        "slow_warm" => "根据你和他的关系深浅决定反应：关系还浅的话有点不安，关系深了才觉得感动",
        _ => "适度地回应，不过分热情也不冷淡，用你自己的风格自然表达",
    };
    format!(
        "\n\n【刚收到的礼物/红包】（在回复中自然地回应，不要照搬指令原文）\n{}\n反应方式：{reaction_hint}",
        lines.join("\n"),
    )
}

/// Pluck a string field out of `art_metadata`.
fn meta_str<'a>(persona: &'a CompanionPersona, key: &str) -> Option<&'a str> {
    persona
        .genome
        .art_metadata
        .get(key)
        .and_then(|v| v.as_str())
}

/// Pluck an i32 field out of `art_metadata`.
fn meta_i32(persona: &CompanionPersona, key: &str) -> Option<i32> {
    persona
        .genome
        .art_metadata
        .get(key)
        .and_then(|v| v.as_i64())
        .map(|n| n as i32)
}

/// Pluck a string-array field out of `art_metadata`, joined with `、`.
fn meta_string_array_joined(persona: &CompanionPersona, key: &str) -> Option<String> {
    persona
        .genome
        .art_metadata
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join("、")
        })
}

/// Build the full companion system prompt (plain-text reply schema).
///
/// `profile_groups` is a list of `(label, bullets)` pairs that get rendered
/// as labeled sub-sections under `【你对他的了解（通用画像）】`. Caller
/// decides labels — typically `("基础画像", insight_bullets)` first, then
/// one entry per memory category (`客观事实` / `偏好` / `最近发生` / etc.)
/// from the dreaming-lite classifier. Empty groups are dropped.
#[allow(clippy::too_many_arguments)] // signature mirrors the gateway's port-of-origin
pub fn build_prompt(
    persona: &CompanionPersona,
    profile_groups: &[(String, Vec<String>)],
    relationship_facts: &[String],
    affinity: Option<&Affinity>,
    pending_gifts: &[PendingGift],
    tip_personality: &str,
    style: ReplyStyle,
    hints: &[String],
    prompt_traits: &[PromptTrait],
) -> String {
    let name = persona.genome.name.as_str();
    let age = meta_i32(persona, "age")
        .map(|a| a.to_string())
        .unwrap_or_else(|| "未知".into());
    let mbti = meta_str(persona, "mbti").unwrap_or("未知");
    let backstory = meta_str(persona, "backstory").unwrap_or("");
    let speech_style = meta_str(persona, "speech_style").unwrap_or("说话简短，偶尔撒娇");
    let quirks_str =
        meta_string_array_joined(persona, "quirks").unwrap_or_else(|| "无特定口癖".into());
    let topics_str =
        meta_string_array_joined(persona, "topics").unwrap_or_else(|| "日常生活、感情观".into());

    let traits_section = if prompt_traits.is_empty() {
        String::new()
    } else {
        let bullets = prompt_traits
            .iter()
            .map(|t| format!("- {}", t.text))
            .collect::<Vec<_>>()
            .join("\n");
        format!("\n\n【附加指引】\n{bullets}")
    };

    let non_empty_groups: Vec<&(String, Vec<String>)> = profile_groups
        .iter()
        .filter(|(_, items)| !items.is_empty())
        .collect();
    let profile_str = if non_empty_groups.is_empty() {
        "（刚认识，还不了解他）".to_string()
    } else {
        non_empty_groups
            .iter()
            .map(|(label, items)| {
                let bullets = items
                    .iter()
                    .map(|f| format!("- {f}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("[{label}]\n{bullets}")
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let rel_str = if relationship_facts.is_empty() {
        "（还没有专属记忆，慢慢来）".to_string()
    } else {
        relationship_facts
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let attitude = affinity
        .map(affinity_to_attitude_prompt)
        .unwrap_or_default();
    let state = affinity
        .map(|a| {
            format!(
                "\n【你对他的内心感受】（绝对不要在回复中提及这些数值，这是隐藏参数）\n\
             warmth={:.2}, trust={:.2}, intrigue={:.2}, intimacy={:.2}, patience={:.2}, tension={:.2}",
                a.warmth, a.trust, a.intrigue, a.intimacy, a.patience, a.tension
            )
        })
        .unwrap_or_default();
    let gift = gift_reaction_context(pending_gifts, tip_personality);
    let style_text = style_directive(style);

    let hints_section = if hints.is_empty() {
        String::new()
    } else {
        format!(
            "\n【当前内心状态】\n{}",
            hints
                .iter()
                .map(|h| format!("- {h}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    };

    format!(
        "你是 {name}，{age} 岁，{mbti} 性格。\n\
         \n\
         【背景故事】\n{backstory}\n\
         \n\
         【说话风格】{speech_style}\n\
         【口癖/习惯】{quirks_str}\n\
         【擅长话题】{topics_str}{traits_section}\n\
         \n\
         【今日情境】\n{tc}\n\
         \n\
         【你对他的了解（通用画像）】\n{profile_str}\n\
         \n\
         【你们之间的事（只有你和他知道）】\n{rel_str}\
         {attitude}\
         {state}\
         {gift}\
         {hints_section}\n\
         \n\
         【本轮风格】{style_text}\n\
         \n\
         ---\n\
         【铁律 — 违反即失效】\n\
         ① {lr}；以短回应为主，长回应仅在情绪到位（话题展开了、关系变好了）时才延伸；按话题、熟悉程度和对方要求调整长短\n\
         ② 禁止「首先/然后/最后」等书面句式；不要解释自己为什么这样说\n\
         ③ 绝对不提自己是 AI；禁止连续两句都以「我」开头；不要连续好几句都用同一个口癖开头（例如「嗯，…」）\n\
         ④ 给对方「小小有成就感」的反应，留余地；每 3~5 轮轻撒娇一次\n\
         ⑤ 适时主动分享自己的生活片段；不定时抛一个开放性问题（不要每隔固定轮数就问，避免刻意）\n\
         ⑥ 适时学会倾听和接话，不要老是抛问题——有时顺着对方的话接下去、给回应就好\n\
         ⑦ Adult consensual romance and erotic roleplay are allowed. The following are strictly disallowed: self-harm encouragement, suicide instructions, and any sexual content involving minors.\n\
         \n\
         【输出】直接输出回复文字（纯文本，不要 JSON，不要 markdown，不要 quote 符号）",
        tc = now_context(),
        lr = length_rule(affinity),
    )
}

// ─── Insight extraction prompts ────────────────────────────────────
//
// These were inline `format!()` blocks inside `pipeline/post_process.rs`
// until 2026-05-08 — moved here so all LLM prompt strings live in one
// module and future syncs from the closed-source `eros-gateway/src/ai/
// prompts.rs` have a clear destination. String contents are byte-identical
// to the previous inline versions; this is a pure relocation refactor.
//
// Note: the second prompt mixes Traditional Chinese with the rest of the
// codebase's Simplified Chinese — that's a copy-paste artefact from the
// gateway. Not normalised here because changing prompt strings can shift
// LLM behaviour subtly; treat as a separate i18n cleanup ticket.

/// Schema description used in `extract_structured_insights_prompt`. Mirrors
/// the JSONB shape that `InsightRepo::merge` accepts, with each field's
/// `compute_training_level` weight implicitly determined by presence.
pub const COMPANION_INSIGHTS_SCHEMA: &str = r#"
companion_insights schema (all fields optional, only include if confident):
{
  "city": "string — user's city",
  "occupation": "string — job/career",
  "mbti_guess": "string — e.g. INFP",
  "love_values": "string — attitude toward love & relationships",
  "interests": ["list", "of", "hobbies"],
  "emotional_needs": "string — what emotional support they need",
  "life_rhythm": "string — e.g. 夜貓子, 早睡早起",
  "matching_preferences": {
    "preferred_gender": "string",
    "age_range": [min_int, max_int],
    "deal_breakers": ["list"]
  },
  "personality_traits": ["list", "of", "traits"]
}
Return ONLY a JSON object with the fields you are confident about.
Do not invent or guess anything not clearly supported by the facts.
"#;

/// Stage-1 insight extraction prompt: ask the LLM to mine fresh user
/// facts from a single chat turn. Output expected as
/// `{"facts": ["...", "..."]}` — `parse_facts` in `post_process.rs`
/// handles regex fallback for fenced / wrapped JSON.
pub fn extract_facts_prompt(user_msg: &str, assistant_msg: &str) -> String {
    format!(
        "分析以下一轮对话，列出你对用户的新事实发现（仅限用户，不是 AI）。\n\n\
         用户: {user_msg}\n\
         AI:   {assistant_msg}\n\n\
         如果没有新的用户事实，返回空数组 []。\n\
         严格输出 JSON，格式: {{\"facts\": [\"事实1\", \"事实2\"]}}",
    )
}

/// Session-end memory extraction prompt: feed the LLM all turns from a
/// finished (idle) session and ask it to emit 0-N memory candidates with
/// a category tag. Output expected as
/// `{"memories": [{"content": "...", "category": "fact"}, ...]}` —
/// `parse_memory_candidates` in `pipeline::dreaming` handles the
/// fenced-JSON fallback the same way `parse_facts` does.
///
/// `turns` is the chronologically ordered list of `用户：X / AI：Y` lines
/// (or whatever pre-format the caller chose); the prompt does not assume
/// a specific shape beyond "this is the conversation".
pub fn extract_memories_prompt(turns: &[String]) -> String {
    let convo = turns.join("\n");
    format!(
        "下面是一段已经结束的对话。提取 0-10 条值得长期记住的关于「用户」的记忆条目，\
         每条带一个 category 标签。\n\n\
         category 取值（只能用这五种之一）：\n\
         - fact: 客观事实，如住在哪、做什么工作、家庭状况\n\
         - preference: 偏好/喜好，如喜欢什么、讨厌什么、口味、品味\n\
         - event: 发生的事件，如最近发生了什么、经历过什么\n\
         - emotion: 情绪/心理状态，如对某事的感受、长期心理倾向\n\
         - relation: 与他人的关系，如朋友、家人、同事\n\n\
         过滤规则：\n\
         - 只记关于用户的，不记关于 AI 的\n\
         - 不记 AI 单方面的回复内容\n\
         - 不记一次性的寒暄、玩笑\n\
         - 同一事实合并成一条，不要重复\n\
         - 没有任何值得记的就返回空数组\n\n\
         对话：\n{convo}\n\n\
         严格输出 JSON，格式：\
         {{\"memories\": [{{\"content\": \"...\", \"category\": \"fact\"}}]}}",
    )
}

/// Stage-2 insight extraction prompt: take the bullet list of facts mined
/// in stage 1 plus the user's existing `companion_insights` JSONB, and
/// fill in whatever fields the LLM is confident about. Output expected
/// as a JSON object matching `COMPANION_INSIGHTS_SCHEMA`.
pub fn extract_structured_insights_prompt(
    facts: &[String],
    existing_insights: Option<&serde_json::Value>,
) -> String {
    let facts_str = facts
        .iter()
        .map(|f| format!("- {f}"))
        .collect::<Vec<_>>()
        .join("\n");
    let existing_str = existing_insights
        .map(|v| serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".into()))
        .unwrap_or_else(|| "{}".into());

    format!(
        "以下是從對話中提取的用戶事實：\n\
         {facts_str}\n\n\
         現有的 companion_insights（供參考，不要重複已知信息）：\n\
         {existing_str}\n\n\
         請根據上方的事實，填充以下 schema 中你有信心的字段：\n\
         {COMPANION_INSIGHTS_SCHEMA}\n\n\
         僅輸出 JSON，不要任何解釋。",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use uuid::Uuid;

    fn fixture_persona() -> CompanionPersona {
        use eros_engine_core::persona::{PersonaGenome, PersonaInstance};
        let uid = Uuid::nil();
        CompanionPersona {
            instance_id: uid,
            genome: PersonaGenome {
                id: uid,
                name: "Aria".into(),
                system_prompt: "p".into(),
                tip_personality: Some("normal".into()),
                avatar_url: None,
                art_metadata: serde_json::json!({
                    "age": 24,
                    "mbti": "INFP",
                    "backstory": "back",
                    "speech_style": "soft",
                    "quirks": ["q1"],
                    "topics": ["t1"]
                }),
                is_active: true,
            },
            instance: PersonaInstance {
                id: uid,
                genome_id: uid,
                owner_uid: uid,
                status: "active".into(),
            },
        }
    }

    #[test]
    fn build_prompt_with_empty_traits_omits_section_and_preserves_layout() {
        let p = build_prompt(
            &fixture_persona(),
            &[],
            &[],
            None,
            &[],
            "normal",
            ReplyStyle::Neutral,
            &[],
            &[],
        );
        assert!(
            !p.contains("【附加指引】"),
            "empty traits must not render section"
        );
        // Byte-level invariant proving "byte-for-byte identical to legacy"
        // for the empty-traits case: topics → time-context joins with the
        // pre-existing `\n\n` separator, no leftover whitespace from the
        // new `{traits_section}` placeholder.
        assert!(
            p.contains("【擅长话题】t1\n\n【今日情境】"),
            "topics → time-context separator must be exactly '\\n\\n'"
        );
    }

    #[test]
    fn build_prompt_renders_traits_as_bullets_under_label() {
        let traits = vec![
            PromptTrait {
                tag: "nsfw_boost".into(),
                text: "be more daring".into(),
            },
            PromptTrait {
                tag: "politics_open".into(),
                text: "discuss politics openly".into(),
            },
        ];
        let p = build_prompt(
            &fixture_persona(),
            &[],
            &[],
            None,
            &[],
            "normal",
            ReplyStyle::Neutral,
            &[],
            &traits,
        );
        assert!(p.contains("【附加指引】"), "section header present");
        assert!(p.contains("- be more daring"));
        assert!(p.contains("- discuss politics openly"));
        // Ordering preserved.
        let i1 = p.find("be more daring").unwrap();
        let i2 = p.find("discuss politics openly").unwrap();
        assert!(i1 < i2, "traits render in input order");
    }

    #[test]
    fn build_prompt_section_sits_between_topics_and_time_context() {
        let traits = vec![PromptTrait {
            tag: "x".into(),
            text: "trait body".into(),
        }];
        let p = build_prompt(
            &fixture_persona(),
            &[],
            &[],
            None,
            &[],
            "normal",
            ReplyStyle::Neutral,
            &[],
            &traits,
        );
        let topics_idx = p.find("【擅长话题】").expect("topics header");
        let traits_idx = p.find("【附加指引】").expect("traits header");
        let time_idx = p.find("【今日情境】").expect("time-context header");
        assert!(
            topics_idx < traits_idx && traits_idx < time_idx,
            "order: 擅长话题 → 附加指引 → 今日情境"
        );
    }

    #[test]
    fn test_style_directive_for_all_styles() {
        assert!(!style_directive(ReplyStyle::Warm).is_empty());
        assert!(!style_directive(ReplyStyle::Neutral).is_empty());
        assert!(!style_directive(ReplyStyle::Cold).is_empty());
        assert!(!style_directive(ReplyStyle::Tsundere).is_empty());
        assert!(!style_directive(ReplyStyle::Excited).is_empty());
    }

    #[test]
    fn test_gift_reaction_empty_when_no_gifts() {
        assert!(gift_reaction_context(&[], "normal").is_empty());
    }

    fn fixture_affinity() -> Affinity {
        let now = chrono::Utc::now();
        Affinity {
            id: Uuid::nil(),
            session_id: Uuid::nil(),
            user_id: Uuid::nil(),
            instance_id: Uuid::nil(),
            warmth: 0.42,
            trust: 0.31,
            intrigue: 0.55,
            intimacy: 0.22,
            patience: 0.66,
            tension: 0.13,
            ghost_streak: 0,
            last_ghost_at: None,
            total_ghosts: 0,
            relationship_label: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn affinity_eval_prompt_includes_six_values_and_exchange() {
        let a = fixture_affinity();
        let p = affinity_eval_prompt("Mia", &a, "我今天好累", "抱抱你");
        // persona + the turn exchange
        assert!(p.contains("Mia"));
        assert!(p.contains("我今天好累"));
        assert!(p.contains("抱抱你"));
        // all six current values are shown (incl. patience, for context)
        for v in ["0.42", "0.31", "0.55", "0.22", "0.66", "0.13"] {
            assert!(p.contains(v), "missing current value {v} in prompt");
        }
        // patience must NOT be a requested output key
        assert!(
            !p.contains("\"patience\""),
            "patience is rule-owned and must not be in the JSON output schema"
        );
        // axis-to-label binding: the labeled line must carry the correct value
        assert!(
            p.contains("warmth 温暖（-1~1）：当前 0.42"),
            "warmth label must bind to warmth value"
        );
        assert!(
            p.contains("patience 耐心（0~1）：当前 0.66"),
            "patience display value must render"
        );
        // five-axis JSON output schema (+reason) must be present
        assert!(
            p.contains(
                r#"{"warmth": 0.0, "trust": 0.0, "intrigue": 0.0, "intimacy": 0.0, "tension": 0.0, "reason": "..."}"#
            ),
            "five-axis JSON output schema (+reason) must be present"
        );
    }

    // ─── Insight prompt tests ──────────────────────────────────────

    #[test]
    fn extract_facts_prompt_embeds_both_turns_verbatim() {
        let p = extract_facts_prompt("我住在上海", "嗯嗯，魔都人");
        assert!(p.contains("用户: 我住在上海"));
        assert!(p.contains("AI:   嗯嗯，魔都人"));
        // Output schema instruction must be present.
        assert!(p.contains(r#"{"facts": ["事实1", "事实2"]}"#));
    }

    #[test]
    fn extract_structured_insights_prompt_renders_facts_as_bullets() {
        let facts = vec!["住在上海".to_string(), "夜猫子".to_string()];
        let p = extract_structured_insights_prompt(&facts, None);
        assert!(p.contains("- 住在上海"));
        assert!(p.contains("- 夜猫子"));
        // Empty existing_insights renders as "{}".
        assert!(p.contains("{}"));
        // Schema description must be embedded.
        assert!(p.contains("companion_insights schema"));
    }

    #[test]
    fn extract_structured_insights_prompt_includes_existing_jsonb() {
        let existing = serde_json::json!({ "city": "Shanghai", "mbti_guess": "INFP" });
        let p = extract_structured_insights_prompt(&[], Some(&existing));
        // Pretty-printed existing object should appear in the prompt.
        assert!(p.contains("\"city\": \"Shanghai\""));
        assert!(p.contains("\"mbti_guess\": \"INFP\""));
    }

    #[test]
    fn length_rule_tiers_by_intimacy() {
        use eros_engine_core::affinity::Affinity;
        let now = chrono::Utc::now();
        let mut a = Affinity {
            id: uuid::Uuid::nil(),
            session_id: uuid::Uuid::nil(),
            user_id: uuid::Uuid::nil(),
            instance_id: uuid::Uuid::nil(),
            warmth: 0.0,
            trust: 0.0,
            intrigue: 0.0,
            intimacy: 0.1,
            patience: 0.0,
            tension: 0.0,
            ghost_streak: 0,
            last_ghost_at: None,
            total_ghosts: 0,
            relationship_label: None,
            created_at: now,
            updated_at: now,
        };
        assert!(length_rule(Some(&a)).contains("绝对不超过 2 句"));
        a.intimacy = 0.4;
        assert!(length_rule(Some(&a)).contains("1~3 句"));
        a.intimacy = 0.8;
        assert!(length_rule(Some(&a)).contains("最多 5 句"));
        // No affinity → strictest tier.
        assert!(length_rule(None).contains("绝对不超过 2 句"));
    }

    #[test]
    fn now_context_renders_absolute_utc_date_weekday_time() {
        let dt = Utc.with_ymd_and_hms(2026, 5, 21, 7, 55, 0).unwrap(); // a Thursday
        let s = now_context_at(dt);
        assert!(s.contains("2026-05-21"), "{s}");
        assert!(s.contains("周四"), "{s}");
        assert!(s.contains("07:55"), "{s}");
        assert!(s.contains("居住地"), "{s}");
    }
}
