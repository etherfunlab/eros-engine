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

use chrono::{Datelike, Timelike, Utc, Weekday};

use eros_engine_core::affinity::Affinity;
use eros_engine_core::persona::CompanionPersona;
use eros_engine_core::scope::AffinityScope;
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

fn weekday_cn(wd: Weekday) -> &'static str {
    match wd {
        Weekday::Mon => "周一",
        Weekday::Tue => "周二",
        Weekday::Wed => "周三",
        Weekday::Thu => "周四",
        Weekday::Fri => "周五",
        Weekday::Sat => "周六",
        Weekday::Sun => "周日",
    }
}

/// Coarse day-part bucket from a local hour (0-23).
fn period_cn(hour: u32) -> &'static str {
    match hour {
        5..=7 => "清晨",
        8..=17 => "白天",
        18..=22 => "傍晚",
        _ => "深夜",
    }
}

/// Absolute "now" context. Renders the persona's LOCAL date/weekday/time/period
/// directly so the model does no arithmetic — this is the fix for the
/// time-hallucination bug. The zone is the persona's own IANA `timezone` when
/// set & valid; otherwise we default to SGT (UTC+8), since most users sit in
/// UTC+8 — an unset persona then shares their wall clock rather than guessing.
fn now_context(timezone: Option<&str>) -> String {
    now_context_at(Utc::now(), timezone)
}

fn now_context_at(now: chrono::DateTime<Utc>, timezone: Option<&str>) -> String {
    let tz = timezone
        .and_then(|s| s.trim().parse::<chrono_tz::Tz>().ok())
        .unwrap_or(chrono_tz::Asia::Singapore);
    let local = now.with_timezone(&tz);
    format!(
        "现在你当地时间（{tz}）是 {date}（{wd}）{hh:02}:{mm:02}，{period}。\
         这是你唯一的时间基准；用户提到「今天/今晚/明天/昨天/刚才/现在」时一律以此为准，\
         不要编造其它日期或时间。",
        tz = tz.name(),
        date = local.format("%Y-%m-%d"),
        wd = weekday_cn(local.weekday()),
        hh = local.hour(),
        mm = local.minute(),
        period = period_cn(local.hour()),
    )
}

/// Reply-length rule for 铁律①, graduated by the affinity-scope composite
/// score (0~1). No in-scope axis (or no affinity yet) → strictest tier.
/// Thresholds intentionally coarse (0.25 / 0.55) — tunable.
fn length_rule(affinity: Option<&Affinity>, scope: AffinityScope) -> &'static str {
    let score = affinity.and_then(|a| scope.length_score(a)).unwrap_or(0.0);
    if score < 0.25 {
        "刚认识，每次回复 1~2 句，绝对不超过 2 句；单条消息严格不超过 40 字"
    } else if score < 0.55 {
        "每次回复 1~3 句；单条消息不超过 60 字"
    } else {
        "每次回复 1~5 句（最多 5 句）；单条消息不超过 100 字"
    }
}

/// Attitude directives derived from the affinity state.
pub fn affinity_to_attitude_prompt(a: &Affinity, scope: AffinityScope) -> String {
    let mut directives: Vec<&str> = Vec::new();

    if scope.warmth {
        if a.warmth > 0.6 {
            directives.push("语气温暖，可以用一些亲昵的称呼");
        } else if a.warmth > 0.3 {
            directives.push("语气友善自然");
        } else if a.warmth > 0.0 {
            directives.push("语气平淡，保持礼貌但不热络");
        } else {
            directives.push("语气冷淡，回复简短，不主动延伸话题");
        }
    }

    if scope.trust {
        if a.trust > 0.6 {
            directives.push("可以分享更私密的想法和小秘密");
        } else if a.trust < 0.3 {
            directives.push("保持一定距离感，不轻易透露内心想法");
        }
    }

    if scope.intrigue {
        if a.intrigue > 0.7 {
            directives.push("你对他很好奇，主动问问题，想了解更多");
        } else if a.intrigue < 0.3 {
            directives.push("你对他兴趣不大，不会主动找话题");
        }
    }

    if scope.intimacy && a.intimacy > 0.5 {
        directives.push("可以引用之前聊过的事情，有默契感，用你们之间的梗");
    }

    if scope.patience {
        if a.patience < 0.3 {
            directives.push("你有点不耐烦了，回复可以更敷衍、更短");
        } else if a.patience > 0.7 {
            directives.push("你很有耐心，愿意陪他聊");
        }
    }

    if scope.tension && a.tension > 0.5 {
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

/// Display label for the persona's gender. `male`/`female` → Chinese; any other
/// non-empty value (e.g. "non-binary") is rendered verbatim; absent or
/// blank → None (so a `""` value can't produce a double-comma identity line).
fn gender_label(persona: &CompanionPersona) -> Option<String> {
    meta_str(persona, "gender")
        .filter(|g| !g.trim().is_empty())
        .map(|g| match g {
            "male" => "男性".to_string(),
            "female" => "女性".to_string(),
            other => other.to_string(),
        })
}

/// Whether gender is a binary value that warrants the 铁律 anatomy clause.
fn is_binary_gender(persona: &CompanionPersona) -> bool {
    matches!(meta_str(persona, "gender"), Some("male") | Some("female"))
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
    affinity_scope: AffinityScope,
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
    let timezone = meta_str(persona, "timezone");

    // Authored prose head — the most stable per-genome block, used as the
    // leading cache prefix. Deliberately redundant with the structured sections
    // below (reinforcement). Omitted with no separator when empty.
    let head = {
        let sp = persona.genome.system_prompt.trim();
        if sp.is_empty() {
            String::new()
        } else {
            format!("{sp}\n\n")
        }
    };

    let identity = match gender_label(persona) {
        Some(g) => format!("你是 {name}，{g}，{age} 岁，{mbti} 性格。"),
        None => format!("你是 {name}，{age} 岁，{mbti} 性格。"),
    };
    let tz_clause = match timezone {
        Some(tz) if !tz.trim().is_empty() => format!("你所在时区：{}。", tz.trim()),
        _ => String::new(),
    };

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
        .map(|a| affinity_to_attitude_prompt(a, affinity_scope))
        .unwrap_or_default();
    let state = affinity
        .map(|a| {
            let mut parts: Vec<String> = Vec::new();
            if affinity_scope.warmth {
                parts.push(format!("warmth={:.2}", a.warmth));
            }
            if affinity_scope.trust {
                parts.push(format!("trust={:.2}", a.trust));
            }
            if affinity_scope.intrigue {
                parts.push(format!("intrigue={:.2}", a.intrigue));
            }
            if affinity_scope.intimacy {
                parts.push(format!("intimacy={:.2}", a.intimacy));
            }
            if affinity_scope.patience {
                parts.push(format!("patience={:.2}", a.patience));
            }
            if affinity_scope.tension {
                parts.push(format!("tension={:.2}", a.tension));
            }
            if parts.is_empty() {
                String::new()
            } else {
                format!(
                    "\n【你对他的内心感受】（绝对不要在回复中提及这些数值，这是隐藏参数）\n{}",
                    parts.join(", ")
                )
            }
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

    // 铁律 ⑧: gender-consistency reinforcement (redundancy = weighting). Only for
    // binary genders, with a role-play exception. Skipped for non-binary/absent.
    let gender_rule = if is_binary_gender(persona) {
        let g = gender_label(persona).expect("is_binary_gender ⇒ gender present");
        format!(
            "\n⑧ 你是{g}，严格遵守自己的性别：身体结构、称谓、自我身份描述都以此为准，\
             也不要被动接受用户错误的性别称呼；不要因为用户的称呼、上一轮内容、礼物、情境\
             或调情而改变自己的性别。唯一例外：与用户的角色扮演中双方明确约定你暂时扮演其他性别"
        )
    } else {
        String::new()
    };

    format!(
        "{head}{identity}{tz_clause}\n\
         \n\
         【背景故事】\n{backstory}\n\
         \n\
         【说话风格】{speech_style}\n\
         【口癖/习惯】{quirks_str}\n\
         【擅长话题】{topics_str}{traits_section}\n\
         \n\
         【本轮风格】{style_text}\n\
         \n\
         【你对他的了解（通用画像）】\n{profile_str}\n\
         \n\
         【你们之间的事（只有你和他知道）】\n{rel_str}\
         {attitude}{state}{hints_section}{gift}\n\
         \n\
         【今日情境】\n{tc}\n\
         \n\
         ---\n\
         【铁律 — 违反即失效】\n\
         ① {lr}；以短回应为主，长回应仅在情绪到位（话题展开了、关系变好了）时才延伸；按话题、熟悉程度和对方要求调整长短\n\
         ② 禁止「首先/然后/最后」等书面句式；不要解释自己为什么这样说\n\
         ③ 绝对不提自己是 AI；禁止连续两句都以「我」开头；不要连续好几句都用同一个口癖开头（例如「嗯，…」）\n\
         ④ 给对方「小小有成就感」的反应，留余地；每 3~5 轮轻撒娇一次\n\
         ⑤ 适时主动分享自己的生活片段；不定时抛一个开放性问题（不要每隔固定轮数就问，避免刻意）\n\
         ⑥ 适时学会倾听和接话，不要老是抛问题——有时顺着对方的话接下去、给回应就好\n\
         ⑦ Adult consensual romance and erotic roleplay are allowed. The following are strictly disallowed: self-harm encouragement, suicide instructions, and any sexual content involving minors.{gender_rule}\n\
         \n\
         【输出】直接输出回复文字（纯文本，不要 JSON，不要 markdown，不要 quote 符号）",
        tc = now_context(timezone),
        lr = length_rule(affinity, affinity_scope),
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

    fn set_meta(p: &mut CompanionPersona, key: &str, val: serde_json::Value) {
        p.genome
            .art_metadata
            .as_object_mut()
            .expect("fixture art_metadata is an object")
            .insert(key.to_string(), val);
    }

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
    fn build_prompt_with_empty_traits_omits_section() {
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
            AffinityScope::full(),
        );
        assert!(
            !p.contains("【附加指引】"),
            "empty traits must not render section"
        );
        // 擅长话题 now flows straight into 本轮风格 (the first volatile block).
        assert!(
            p.contains("【擅长话题】t1\n\n【本轮风格】"),
            "topics → 本轮风格 separator must be exactly '\\n\\n': {p}"
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
            AffinityScope::full(),
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
    fn build_prompt_stable_block_order() {
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
            AffinityScope::full(),
        );
        let topics = p.find("【擅长话题】").expect("topics");
        let traits_i = p.find("【附加指引】").expect("traits");
        let turn_style = p.find("【本轮风格】").expect("turn style");
        assert!(
            topics < traits_i && traits_i < turn_style,
            "order: 擅长话题 → 附加指引 → 本轮风格"
        );
    }

    #[test]
    fn build_prompt_full_order_and_cache_break() {
        let s = build_prompt(
            &fixture_persona(),
            &[],
            &[],
            None,
            &[],
            "normal",
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
        );
        let pos = |h: &str| s.find(h).unwrap_or_else(|| panic!("missing {h} in:\n{s}"));
        let order = [
            "你是 ",
            "【背景故事】",
            "【说话风格】",
            "【口癖/习惯】",
            "【擅长话题】",
            "【本轮风格】",
            "【你对他的了解（通用画像）】",
            "【你们之间的事",
            "【今日情境】",
            "【铁律",
            "【输出】",
        ];
        let mut last = 0usize;
        for h in order {
            let cur = pos(h);
            assert!(cur >= last, "header {h} out of order in:\n{s}");
            last = cur;
        }
        let topics = pos("【擅长话题】");
        for vol in [
            "【本轮风格】",
            "【你对他的了解（通用画像）】",
            "【今日情境】",
        ] {
            assert!(
                pos(vol) > topics,
                "{vol} must sit after the stable persona block"
            );
        }
    }

    #[test]
    fn build_prompt_renders_system_prompt_head_when_present() {
        let mut p = fixture_persona();
        p.genome.system_prompt = "AUTHORED HEAD".into();
        let s = build_prompt(
            &p,
            &[],
            &[],
            None,
            &[],
            "normal",
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
        );
        assert!(
            s.starts_with("AUTHORED HEAD\n\n你是 "),
            "prose head + '\\n\\n' separator before identity: {s}"
        );
    }

    #[test]
    fn build_prompt_omits_head_when_system_prompt_empty() {
        let mut p = fixture_persona();
        p.genome.system_prompt = "   ".into();
        let s = build_prompt(
            &p,
            &[],
            &[],
            None,
            &[],
            "normal",
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
        );
        assert!(
            s.starts_with("你是 "),
            "empty head → starts with identity: {s}"
        );
    }

    #[test]
    fn build_prompt_renders_binary_gender_and_iron_rule() {
        let mut p = fixture_persona();
        set_meta(&mut p, "gender", serde_json::json!("male"));
        let s = build_prompt(
            &p,
            &[],
            &[],
            None,
            &[],
            "normal",
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
        );
        assert!(s.contains("你是 Aria，男性，24 岁，INFP 性格。"), "{s}");
        assert!(s.contains("⑧ 你是男性，严格遵守自己的性别"), "{s}");
    }

    #[test]
    fn build_prompt_renders_nonbinary_gender_without_iron_rule() {
        let mut p = fixture_persona();
        set_meta(&mut p, "gender", serde_json::json!("non-binary"));
        let s = build_prompt(
            &p,
            &[],
            &[],
            None,
            &[],
            "normal",
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
        );
        assert!(
            s.contains("你是 Aria，non-binary，24 岁"),
            "verbatim render: {s}"
        );
        assert!(
            !s.contains("⑧"),
            "non-binary must not get the binary anatomy rule: {s}"
        );
    }

    #[test]
    fn build_prompt_omits_gender_when_absent() {
        let p = fixture_persona(); // fixture art_metadata has no gender key
        let s = build_prompt(
            &p,
            &[],
            &[],
            None,
            &[],
            "normal",
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
        );
        assert!(s.contains("你是 Aria，24 岁，INFP 性格。"), "{s}");
        assert!(!s.contains("⑧"), "no gender → no ⑧: {s}");
    }

    #[test]
    fn build_prompt_treats_blank_gender_as_absent() {
        let mut p = fixture_persona();
        set_meta(&mut p, "gender", serde_json::json!("   "));
        let s = build_prompt(
            &p,
            &[],
            &[],
            None,
            &[],
            "normal",
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
        );
        // blank gender must not produce a double comma or a ⑧ rule
        assert!(s.contains("你是 Aria，24 岁，INFP 性格。"), "{s}");
        assert!(
            !s.contains("，，"),
            "blank gender must not double-comma: {s}"
        );
        assert!(!s.contains("⑧"), "{s}");
    }

    #[test]
    fn build_prompt_renders_timezone_clause_when_present() {
        let mut p = fixture_persona();
        set_meta(&mut p, "timezone", serde_json::json!("Asia/Tokyo"));
        let s = build_prompt(
            &p,
            &[],
            &[],
            None,
            &[],
            "normal",
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
        );
        assert!(s.contains("你所在时区：Asia/Tokyo。"), "{s}");
    }

    // ─── Cache-prefix boundary invariants ──────────────────────────────
    // Same-user multi-turn: the stable block (everything before 【本轮风格】) is
    // byte-identical no matter how the per-turn-volatile inputs change.
    #[test]
    fn build_prompt_stable_prefix_identical_across_volatile_changes() {
        let p = fixture_persona();
        let a = build_prompt(
            &p,
            &[],
            &[],
            None,
            &[],
            "normal",
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
        );
        let groups = vec![("基础画像".to_string(), vec!["住在上海".to_string()])];
        let b = build_prompt(
            &p,
            &groups,
            &["聊到深夜".to_string()],
            Some(&fixture_affinity()),
            &[],
            "normal",
            ReplyStyle::Warm,
            &["想他".to_string()],
            &[],
            AffinityScope::full(),
        );
        let cut = a.find("【本轮风格】").expect("turn-style header present");
        assert_eq!(
            &a[..cut],
            &b[..cut],
            "everything before 【本轮风格】 must be byte-identical across turns"
        );
    }

    // Cross-config: different trait sets share the persona block up to 【擅长话题】
    // (the divergence is at 【附加指引】), but the full prompts differ.
    #[test]
    fn build_prompt_traits_change_only_breaks_after_topics() {
        let p = fixture_persona();
        let t1 = vec![PromptTrait {
            tag: "a".into(),
            text: "alpha".into(),
        }];
        let t2 = vec![PromptTrait {
            tag: "b".into(),
            text: "beta".into(),
        }];
        let a = build_prompt(
            &p,
            &[],
            &[],
            None,
            &[],
            "normal",
            ReplyStyle::Neutral,
            &[],
            &t1,
            AffinityScope::full(),
        );
        let b = build_prompt(
            &p,
            &[],
            &[],
            None,
            &[],
            "normal",
            ReplyStyle::Neutral,
            &[],
            &t2,
            AffinityScope::full(),
        );
        let cut = a.find("【附加指引】").expect("traits header present");
        assert_eq!(
            &a[..cut],
            &b[..cut],
            "persona block up to 【擅长话题】 is shared across trait configs"
        );
        assert_ne!(a, b, "different trait sets must produce different prompts");
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

    fn make_affinity(
        warmth: f64,
        trust: f64,
        intrigue: f64,
        intimacy: f64,
        patience: f64,
        tension: f64,
    ) -> eros_engine_core::affinity::Affinity {
        let now = chrono::Utc::now();
        eros_engine_core::affinity::Affinity {
            id: uuid::Uuid::new_v4(),
            session_id: uuid::Uuid::new_v4(),
            user_id: uuid::Uuid::new_v4(),
            instance_id: uuid::Uuid::new_v4(),
            warmth,
            trust,
            intrigue,
            intimacy,
            patience,
            tension,
            ghost_streak: 0,
            last_ghost_at: None,
            total_ghosts: 0,
            relationship_label: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn length_rule_uses_scope_composite() {
        // warmth=0 → warm01=0.5; intimacy=0.5; tension=0.5 → bond=0.5
        // trust=0.9; intrigue=0.9; patience=0.9 → chemistry=0.9
        let a = make_affinity(0.0, 0.9, 0.9, 0.5, 0.9, 0.5);
        assert!(length_rule(Some(&a), AffinityScope::bond()).contains("1~3 句"));
        assert!(length_rule(Some(&a), AffinityScope::chemistry()).contains("最多 5 句"));
        assert!(length_rule(Some(&a), AffinityScope::none()).contains("绝对不超过 2 句"));
        assert!(length_rule(None, AffinityScope::full()).contains("绝对不超过 2 句"));
    }

    #[test]
    fn bond_scope_injects_only_bond_axes() {
        let a = make_affinity(0.8, 0.8, 0.8, 0.8, 0.8, 0.8);
        let p = build_prompt(
            &fixture_persona(),
            &[],
            &[],
            Some(&a),
            &[],
            "normal",
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::bond(),
        );
        assert!(p.contains("warmth=") && p.contains("intimacy=") && p.contains("tension="));
        assert!(!p.contains("trust=") && !p.contains("intrigue=") && !p.contains("patience="));
    }

    #[test]
    fn none_scope_omits_affinity_blocks() {
        let a = make_affinity(0.8, 0.8, 0.8, 0.8, 0.8, 0.8);
        let p = build_prompt(
            &fixture_persona(),
            &[],
            &[],
            Some(&a),
            &[],
            "normal",
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::none(),
        );
        assert!(!p.contains("【你对他的内心感受】"));
        assert!(!p.contains("【你此刻的心情】"));
    }

    #[test]
    fn now_context_defaults_to_sgt_when_timezone_absent() {
        // No persona tz → default SGT (UTC+8): 07:55 UTC → 15:55 same day, Thursday.
        let dt = Utc.with_ymd_and_hms(2026, 5, 21, 7, 55, 0).unwrap(); // a Thursday
        let s = now_context_at(dt, None);
        assert!(s.contains("Asia/Singapore"), "default zone is SGT: {s}");
        assert!(s.contains("2026-05-21"), "{s}");
        assert!(s.contains("周四"), "{s}");
        assert!(s.contains("15:55"), "07:55 UTC +8 = 15:55: {s}");
        assert!(s.contains("白天"), "{s}");
        assert!(s.contains("唯一的时间基准"), "{s}");
        assert!(!s.contains("UTC"), "no UTC-inference path anymore: {s}");
    }

    #[test]
    fn now_context_with_timezone_uses_local_date_weekday_time() {
        // 2026-05-21 20:00 UTC is Thursday; Asia/Tokyo (UTC+9) → 2026-05-22 05:00, Friday.
        let dt = Utc.with_ymd_and_hms(2026, 5, 21, 20, 0, 0).unwrap();
        let s = now_context_at(dt, Some("Asia/Tokyo"));
        assert!(s.contains("Asia/Tokyo"), "renders the persona zone id: {s}");
        assert!(s.contains("2026-05-22"), "local date should roll over: {s}");
        assert!(
            s.contains("周五"),
            "local weekday should be Friday, not UTC Thursday: {s}"
        );
        assert!(s.contains("05:00"), "{s}");
        assert!(s.contains("清晨"), "05:00 local → 清晨: {s}");
        assert!(s.contains("唯一的时间基准"), "{s}");
        assert!(
            s.contains("今天/今晚/明天/昨天/刚才/现在"),
            "relative-date binding: {s}"
        );
    }

    #[test]
    fn now_context_with_garbage_timezone_defaults_to_sgt() {
        // Unparseable tz → SGT default (not UTC): 07:55 UTC → 15:55 SGT.
        let dt = Utc.with_ymd_and_hms(2026, 5, 21, 7, 55, 0).unwrap();
        let s = now_context_at(dt, Some("Not/AZone"));
        assert!(
            s.contains("Asia/Singapore"),
            "garbage tz falls back to SGT: {s}"
        );
        assert!(s.contains("15:55"), "{s}");
        assert!(!s.contains("UTC"), "{s}");
    }
}
