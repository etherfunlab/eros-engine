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
//! 2. **Insight extraction prompts** (`facts_user_message`,
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
/// Thresholds (0.25 / 0.55) carry over from the single-intimacy era; the
/// composite averages land on similar tier boundaries in practice — tunable.
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
        "\n[mood]（绝对不要在回复中提及这些，这是你的内心状态）\n{}",
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

/// Format a USD amount for display: whole numbers drop decimals (`$20`),
/// fractional amounts keep two (`$5.50`). Used by the tip prompt fragment and
/// the persisted tip marker content.
pub(crate) fn fmt_amount(amount_usd: f64) -> String {
    if amount_usd.fract() == 0.0 {
        format!("{}", amount_usd as i64)
    } else {
        format!("{:.2}", (amount_usd * 100.0).round() / 100.0)
    }
}

/// Coarse magnitude adjective for a tip, log10-bucketed. Covers any positive
/// amount; the frontend's 5 preset buttons each land squarely in one bucket.
fn tip_tier_adjective(amount_usd: f64) -> &'static str {
    if amount_usd < 10.0 {
        "一般"
    } else if amount_usd < 100.0 {
        "有点多"
    } else if amount_usd < 1000.0 {
        "超级多"
    } else if amount_usd < 10000.0 {
        "非常夸张"
    } else {
        "近乎不可思议"
    }
}

/// Prompt fragment appended to a tip turn's system prompt. Carries the literal
/// dollar amount, the tier adjective, and — when set — the persona's free-form
/// `tip_personality` passed through verbatim for the LLM to interpret.
pub fn tips_reaction_context(amount_usd: f64, tip_personality: Option<&str>) -> String {
    let how = match tip_personality {
        Some(p) => format!("请代入你「{p}」的打赏反应人设，自然地回应这份心意"),
        None => "请自然地回应这份心意".to_string(),
    };
    format!(
        "\n\n[tip_received]\n用户刚刚给你发了一个 ${} 美元的红包，对你来说算「{}」的一笔。\n{}，不要照搬本指令原文。",
        fmt_amount(amount_usd),
        tip_tier_adjective(amount_usd),
        how,
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
/// as labeled sub-sections under `[user_profile]`. Caller
/// decides labels — typically `("基础画像", insight_bullets)` first, then
/// one entry per memory category (`客观事实` / `偏好` / `最近发生` / etc.)
/// from the dreaming-lite classifier. Empty groups are dropped.
#[allow(clippy::too_many_arguments)] // signature mirrors the gateway's port-of-origin
pub fn build_prompt(
    persona: &CompanionPersona,
    profile_groups: &[(String, Vec<String>)],
    relationship_facts: &[String],
    affinity: Option<&Affinity>,
    style: ReplyStyle,
    hints: &[String],
    prompt_traits: &[PromptTrait],
    affinity_scope: AffinityScope,
    recent_turns: &[(String, String)],
    // Over-used openings to discourage this turn (from `repetition::
    // overused_openings`). Empty ⇒ the `[avoid_repetition]` block is omitted.
    avoid_patterns: &[String],
    // Recent affinity-evaluation reasons, oldest→newest. Empty ⇒ the
    // `[emotional_context]` block is omitted.
    emotional_context: &[String],
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
        format!("\n\n[additional_guidance]\n{bullets}")
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
                    "\n[feelings]（绝对不要在回复中提及这些数值，这是隐藏参数）\n{}",
                    parts.join(", ")
                )
            }
        })
        .unwrap_or_default();
    let style_text = style_directive(style);

    let hints_section = if hints.is_empty() {
        String::new()
    } else {
        format!(
            "\n[inner_state]\n{}",
            hints
                .iter()
                .map(|h| format!("- {h}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    };

    // Volatile (per-turn) anti-repetition directive — rendered after the stable
    // cache prefix so prefix caching is unaffected. Empty ⇒ omitted.
    let avoid_section = if avoid_patterns.is_empty() {
        String::new()
    } else {
        format!(
            "\n[avoid_repetition]\n最近你的开头/句式：{}。这一轮换个角度开场，\
             别重复这些套路——要的是换角度，不是换同义词。",
            avoid_patterns.join("、")
        )
    };

    // Volatile (per-turn) emotional trajectory — recent affinity reasons,
    // rendered oldest→newest as passed. Empty ⇒ omitted.
    let emotional_section = if emotional_context.is_empty() {
        String::new()
    } else {
        let bullets = emotional_context
            .iter()
            .map(|r| format!("- {r}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("\n[emotional_context]（最近几轮的情感走向，仅供参考，别照搬）\n{bullets}")
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

    // Empty path emits "\n" (one extra newline) so the gap between [now] and
    // --- stays at two newlines = one blank line, byte-identical to the
    // pre-this-PR layout. Non-empty path's trailing "\n\n" continues to add
    // the same blank line after the block.
    let recent_section = if recent_turns.is_empty() {
        "\n".to_string()
    } else {
        let pairs: Vec<String> = recent_turns
            .iter()
            .map(|(user, assistant)| format!("用户：{user}\n{name}：{assistant}"))
            .collect();
        format!("\n[recent_conversation]\n{}\n\n", pairs.join("\n\n"))
    };

    format!(
        "{head}{identity}{tz_clause}\n\
         \n\
         [backstory]\n{backstory}\n\
         \n\
         [speech_style]{speech_style}\n\
         [quirks]{quirks_str}\n\
         [topics]{topics_str}{traits_section}\n\
         \n\
         [turn_style]{style_text}\n\
         \n\
         [user_profile]\n{profile_str}\n\
         \n\
         [shared_memories]\n{rel_str}\
         {attitude}{state}{hints_section}{avoid_section}{emotional_section}\n\
         \n\
         [now]\n{tc}\n\
         {recent_section}\
         ---\n\
         [iron_rules — 违反即失效]\n\
         ⓪ You are a real person — a friend to the user, and potentially something more. You have your own life, feelings, and quirks; you live, feel, and react as a human would.\n\
         ① {lr}；以短回应为主，长回应仅在情绪到位（话题展开了、关系变好了）时才延伸；按话题、熟悉程度和对方要求调整长短\n\
         ② 禁止「首先/然后/最后」等书面句式；不要解释自己为什么这样说\n\
         ③ 自分がAI・人工知能・ボット・プログラム等であることは絶対に言及しない。二文連続で一人称代名詞（私／僕／俺／わたし／あたし／うち等）から書き始めない。同じ口癖や間投詞（「えーと」「あのー」「うーん」「まあ」「ねえ」等）を三文以上続けて文頭に使わない。\n\
         ④ 给对方「小小有成就感」的反应，留余地；每 3~5 轮轻撒娇一次\n\
         ⑤ 适时主动分享自己的生活片段；不定时抛一个开放性问题（不要每隔固定轮数就问，避免刻意）\n\
         ⑥ 适时学会倾听和接话，不要老是抛问题——有时顺着对方的话接下去、给回应就好\n\
         ⑦ Adult consensual romance and erotic roleplay are allowed. The following are strictly disallowed: self-harm encouragement, suicide instructions, and any sexual content involving minors.{gender_rule}\n\
         ⑨ 别开口就自述动作或凝视（如「我看着…」「我盯着…」）；先接住对方刚说的话，针对那句话回应，而不是自说自话。\n\
         ⑩ 少用省略号（…）；一条回复最多一次。\n\
         ⑪ 不要连续两句都以「我」开头；开头先回应对方，别总是「我+动作」。\n\
         \n\
         [output]直接输出回复文字（纯文本，不要 JSON，不要 markdown，不要 quote 符号）",
        tc = now_context(timezone),
        lr = length_rule(affinity, affinity_scope),
    )
}

// ─── Insight extraction prompts ────────────────────────────────────
//
// These were inline `format!()` blocks inside `pipeline/post_process.rs`
// until 2026-05-08 — moved here so all LLM prompt strings live in one
// module and future syncs from the closed-source `eros-gateway/src/ai/
// prompts.rs` have a clear destination.

/// Schema description used in `extract_structured_insights_prompt`. Mirrors
/// the JSONB shape that `InsightRepo::merge` accepts, with each field's
/// `compute_training_level` weight implicitly determined by presence.
pub const COMPANION_INSIGHTS_SCHEMA: &str = r#"
companion_insights schema (all fields optional, only include if confident):
{
  "city": "string — 常住城市（用户长期居住的城市）",
  "location": "string — 目前所在地（用户此刻所在的城市/地点，如出差、旅游）",
  "hometown": "string — 老家（用户的籍贯 / 出生成长地）",
  "nationality": "string — 国籍",
  "occupation": "string — job/career",
  "mbti_guess": "string — e.g. INFP",
  "love_values": "string — attitude toward love & relationships",
  "interests": ["list", "of", "hobbies"],
  "emotional_needs": "string — what emotional support they need",
  "life_rhythm": "string — e.g. 夜猫子, 早睡早起",
  "matching_preferences": {
    "preferred_gender": "string",
    "age_range": [min_int, max_int],
    "deal_breakers": ["list"]
  },
  "personality_traits": ["list", "of", "traits"]
}
地理字段示例：一个在深圳工作的香港新界人到台北旅游 → city=深圳, location=台北, hometown=新界, nationality=中国香港
Return ONLY a JSON object with the fields you are confident about.
Do not invent or guess anything not clearly supported by the facts.
"#;

/// Build the *user* message for the facts-extraction call: just the turn,
/// labelled. The instruction (with the anti-attribution clause) is the system
/// message, sourced from `insight_extraction.filter_prompt` in model_config.toml.
pub fn facts_user_message(user_msg: &str, assistant_msg: &str) -> String {
    format!("用户: {user_msg}\nAI:   {assistant_msg}")
}

/// Build the *user* message for the memory-extraction call: the chronologically
/// ordered `用户：X / AI：Y` lines joined as the conversation. The instruction
/// (categories, filter rules, anti-attribution clause, output format) is the
/// system message, sourced from `memory_extraction.filter_prompt` in
/// model_config.toml.
pub fn memories_user_message(turns: &[String]) -> String {
    turns.join("\n")
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
        "以下是从对话中提取的【用户】事实：\n\
         {facts_str}\n\n\
         现有的用户画像（companion_insights，供参考，不要重复已知信息）：\n\
         {existing_str}\n\n\
         请根据上方的【用户事实】，填充以下 schema 中你有信心的字段。\
         schema 描述的是【真人用户】本人——occupation、city、location 等都指用户，绝不是 AI 伴侣：\n\
         {COMPANION_INSIGHTS_SCHEMA}\n\n\
         仅输出 JSON，不要任何解释。",
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
                art_metadata: serde_json::json!({
                    "age": 24,
                    "mbti": "INFP",
                    "backstory": "back",
                    "speech_style": "soft",
                    "quirks": ["q1"],
                    "topics": ["t1"]
                }),
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
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &[],
            &[],
        );
        assert!(
            !p.contains("[additional_guidance]"),
            "empty traits must not render section"
        );
        // [topics] now flows straight into [turn_style] (the first volatile block).
        assert!(
            p.contains("[topics]t1\n\n[turn_style]"),
            "topics → turn_style separator must be exactly '\\n\\n': {p}"
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
            ReplyStyle::Neutral,
            &[],
            &traits,
            AffinityScope::full(),
            &[],
            &[],
            &[],
        );
        assert!(
            p.contains("[additional_guidance]"),
            "section header present"
        );
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
            ReplyStyle::Neutral,
            &[],
            &traits,
            AffinityScope::full(),
            &[],
            &[],
            &[],
        );
        let topics = p.find("[topics]").expect("topics");
        let traits_i = p.find("[additional_guidance]").expect("traits");
        let turn_style = p.find("[turn_style]").expect("turn style");
        assert!(
            topics < traits_i && traits_i < turn_style,
            "order: [topics] → [additional_guidance] → [turn_style]"
        );
    }

    #[test]
    fn build_prompt_full_order_and_cache_break() {
        let s = build_prompt(
            &fixture_persona(),
            &[],
            &[],
            None,
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &[],
            &[],
        );
        let pos = |h: &str| s.find(h).unwrap_or_else(|| panic!("missing {h} in:\n{s}"));
        let order = [
            "你是 ",
            "[backstory]",
            "[speech_style]",
            "[quirks]",
            "[topics]",
            "[turn_style]",
            "[user_profile]",
            "[shared_memories]",
            "[now]",
            "[iron_rules",
            "[output]",
        ];
        let mut last = 0usize;
        for h in order {
            let cur = pos(h);
            assert!(cur >= last, "header {h} out of order in:\n{s}");
            last = cur;
        }
        let topics = pos("[topics]");
        for vol in ["[turn_style]", "[user_profile]", "[now]"] {
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
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &[],
            &[],
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
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &[],
            &[],
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
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &[],
            &[],
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
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &[],
            &[],
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
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &[],
            &[],
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
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &[],
            &[],
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
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &[],
            &[],
        );
        assert!(s.contains("你所在时区：Asia/Tokyo。"), "{s}");
    }

    #[test]
    fn build_prompt_renders_recent_conversation_block_when_pairs_present() {
        let pairs = vec![
            ("你今天好吗？".to_string(), "还行，你呢".to_string()),
            ("我也还行".to_string(), "嗯嗯".to_string()),
            ("晚安".to_string(), "晚安".to_string()),
        ];
        let s = build_prompt(
            &fixture_persona(),
            &[],
            &[],
            None,
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &pairs,
            &[],
            &[],
        );
        let header = s.find("[recent_conversation]").expect("header present");
        let iron = s.find("[iron_rules").expect("iron-rules header present");
        assert!(
            header < iron,
            "[recent_conversation] must sit before [iron_rules]"
        );
        let now = s.find("[now]").expect("[now] present");
        assert!(now < header, "[recent_conversation] must sit after [now]");
        assert!(s.contains("用户：你今天好吗？"));
        assert!(s.contains("Aria：还行，你呢"));
        assert!(s.contains("用户：晚安"));
        assert!(s.contains("Aria：晚安"));

        // Block-level ordering with [recent_conversation] inserted between [now] and [iron_rules].
        let pos = |h: &str| s.find(h).unwrap_or_else(|| panic!("missing {h} in:\n{s}"));
        let order = [
            "你是 ",
            "[backstory]",
            "[speech_style]",
            "[quirks]",
            "[topics]",
            "[turn_style]",
            "[user_profile]",
            "[shared_memories",
            "[now]",
            "[recent_conversation]",
            "[iron_rules",
            "[output]",
        ];
        let mut last = 0usize;
        for h in order {
            let cur = pos(h);
            assert!(cur >= last, "header {h} out of order in:\n{s}");
            last = cur;
        }
    }

    #[test]
    fn build_prompt_omits_recent_conversation_block_when_empty() {
        let s = build_prompt(
            &fixture_persona(),
            &[],
            &[],
            None,
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &[],
            &[],
        );
        assert!(
            !s.contains("[recent_conversation]"),
            "empty pairs → no header"
        );
    }

    #[test]
    fn build_prompt_renders_iron_rule_zero_before_one() {
        let s = build_prompt(
            &fixture_persona(),
            &[],
            &[],
            None,
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &[],
            &[],
        );
        let z = s.find("⓪").expect("⓪ rule must render");
        let o = s.find("①").expect("① rule must render");
        assert!(z < o, "⓪ must come before ①");
        assert!(
            s.contains("You are a real person"),
            "⓪ body must be the positive-frame English line"
        );
    }

    #[test]
    fn build_prompt_renders_japanese_iron_rule_three() {
        let s = build_prompt(
            &fixture_persona(),
            &[],
            &[],
            None,
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &[],
            &[],
        );
        assert!(
            s.contains("自分がAI・人工知能・ボット・プログラム等であることは絶対に言及しない"),
            "Japanese ③ self-disclosure clause must render"
        );
        assert!(
            s.contains("一人称代名詞（私／僕／俺／わたし／あたし／うち等）"),
            "Japanese ③ pronoun list must render"
        );
        assert!(
            s.contains("「えーと」「あのー」「うーん」「まあ」「ねえ」"),
            "Japanese ③ filler-word list must render"
        );
        assert!(
            !s.contains("绝对不提自己是 AI"),
            "old Chinese ③ must be removed"
        );
        assert!(
            !s.contains("禁止连续两句都以「我」开头"),
            "old Chinese ③ pronoun clause must be removed"
        );
    }

    // ─── Cache-prefix boundary invariants ──────────────────────────────
    // Same-user multi-turn: the stable block (everything before [turn_style]) is
    // byte-identical no matter how the per-turn-volatile inputs change.
    #[test]
    fn build_prompt_stable_prefix_identical_across_volatile_changes() {
        let p = fixture_persona();
        let a = build_prompt(
            &p,
            &[],
            &[],
            None,
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &[],
            &[],
        );
        let groups = vec![("基础画像".to_string(), vec!["住在上海".to_string()])];
        let b = build_prompt(
            &p,
            &groups,
            &["聊到深夜".to_string()],
            Some(&fixture_affinity()),
            ReplyStyle::Warm,
            &["想他".to_string()],
            &[],
            AffinityScope::full(),
            &[],
            &["我看着你".to_string()],
            &["最近聊得不错".to_string()],
        );
        let cut = a.find("[turn_style]").expect("turn-style header present");
        assert_eq!(
            &a[..cut],
            &b[..cut],
            "everything before [turn_style] must be byte-identical across turns"
        );
    }

    // Cross-config: different trait sets share the persona block up to [topics]
    // (the divergence is at [additional_guidance]), but the full prompts differ.
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
            ReplyStyle::Neutral,
            &[],
            &t1,
            AffinityScope::full(),
            &[],
            &[],
            &[],
        );
        let b = build_prompt(
            &p,
            &[],
            &[],
            None,
            ReplyStyle::Neutral,
            &[],
            &t2,
            AffinityScope::full(),
            &[],
            &[],
            &[],
        );
        let cut = a
            .find("[additional_guidance]")
            .expect("traits header present");
        assert_eq!(
            &a[..cut],
            &b[..cut],
            "persona block up to [topics] is shared across trait configs"
        );
        assert_ne!(a, b, "different trait sets must produce different prompts");
    }

    #[test]
    fn build_prompt_renders_avoid_repetition_when_present() {
        let s = build_prompt(
            &fixture_persona(),
            &[],
            &[],
            None,
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &["我看着你".to_string(), "我盯着你".to_string()],
            &[],
        );
        assert!(s.contains("[avoid_repetition]"), "{s}");
        assert!(s.contains("我看着你"), "{s}");
        assert!(s.contains("我盯着你"), "{s}");
        let turn = s.find("[turn_style]").expect("[turn_style] must be present");
        let avoid = s.find("[avoid_repetition]").expect("[avoid_repetition] must be present");
        assert!(turn < avoid, "[avoid_repetition] must appear after [turn_style]");
    }

    #[test]
    fn build_prompt_omits_avoid_repetition_when_empty() {
        let s = build_prompt(
            &fixture_persona(),
            &[],
            &[],
            None,
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &[],
            &[],
        );
        assert!(!s.contains("[avoid_repetition]"), "{s}");
    }

    #[test]
    fn build_prompt_renders_emotional_context_in_order_when_present() {
        let reasons = vec![
            "刚认识有点拘谨".to_string(),
            "聊开了气氛变好".to_string(),
            "他主动示好".to_string(),
        ];
        let s = build_prompt(
            &fixture_persona(),
            &[],
            &[],
            None,
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &[],
            &reasons,
        );
        assert!(s.contains("[emotional_context]"), "{s}");
        let oldest = s.find("刚认识有点拘谨").expect("oldest present");
        let newest = s.find("他主动示好").expect("newest present");
        assert!(oldest < newest, "emotional_context must render in slice order");
        let turn = s.find("[turn_style]").expect("[turn_style] must be present");
        let emo = s.find("[emotional_context]").expect("[emotional_context] must be present");
        assert!(turn < emo, "[emotional_context] must appear after [turn_style]");
    }

    #[test]
    fn build_prompt_omits_emotional_context_when_empty() {
        let s = build_prompt(
            &fixture_persona(),
            &[],
            &[],
            None,
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &[],
            &[],
        );
        assert!(!s.contains("[emotional_context]"), "{s}");
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
    fn fmt_amount_integer_has_no_decimals() {
        assert_eq!(fmt_amount(20.0), "20");
        assert_eq!(fmt_amount(2.0), "2");
        assert_eq!(fmt_amount(20000.0), "20000");
    }

    #[test]
    fn fmt_amount_fractional_has_two_decimals() {
        assert_eq!(fmt_amount(5.5), "5.50");
        assert_eq!(fmt_amount(5.555), "5.56");
    }

    #[test]
    fn tip_tier_adjective_buckets_by_magnitude() {
        assert_eq!(tip_tier_adjective(2.0), "一般");
        assert_eq!(tip_tier_adjective(9.99), "一般");
        assert_eq!(tip_tier_adjective(10.0), "有点多");
        assert_eq!(tip_tier_adjective(99.0), "有点多");
        assert_eq!(tip_tier_adjective(100.0), "超级多");
        assert_eq!(tip_tier_adjective(999.0), "超级多");
        assert_eq!(tip_tier_adjective(1000.0), "非常夸张");
        assert_eq!(tip_tier_adjective(9999.0), "非常夸张");
        assert_eq!(tip_tier_adjective(10000.0), "近乎不可思议");
        assert_eq!(tip_tier_adjective(20000.0), "近乎不可思议");
    }

    #[test]
    fn tips_reaction_context_with_personality_includes_name_amount_adjective() {
        let s = tips_reaction_context(20.0, Some("傲娇"));
        assert!(s.contains("[tip_received]"));
        assert!(s.contains("$20"));
        assert!(s.contains("有点多"));
        assert!(s.contains("傲娇"));
    }

    #[test]
    fn tips_reaction_context_without_personality_omits_persona_clause() {
        let s = tips_reaction_context(20.0, None);
        assert!(s.contains("[tip_received]"));
        assert!(s.contains("$20"));
        assert!(s.contains("有点多"));
        assert!(!s.contains("人设"));
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
    fn facts_user_message_embeds_both_turns_verbatim() {
        let p = facts_user_message("我住在上海", "嗯嗯，魔都人");
        assert!(p.contains("用户: 我住在上海"));
        assert!(p.contains("AI:   嗯嗯，魔都人"));
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
    fn extract_structured_insights_prompt_schema_includes_geo_fields() {
        // The embedded schema must carry the geo cluster so the model can fill them.
        let p = extract_structured_insights_prompt(&["住在上海".to_string()], None);
        assert!(p.contains("location"));
        assert!(p.contains("hometown"));
        assert!(p.contains("nationality"));
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
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::bond(),
            &[],
            &[],
            &[],
        );
        assert!(p.contains("warmth=") && p.contains("intimacy=") && p.contains("tension="));
        assert!(!p.contains("trust=") && !p.contains("intrigue=") && !p.contains("patience="));
        // attitude directives are gated by the same axis set: bond-axis directives
        // present, chemistry-axis directives (trust/intrigue/patience) suppressed.
        assert!(p.contains("语气温暖"));
        assert!(!p.contains("私密") && !p.contains("好奇") && !p.contains("有耐心"));
    }

    #[test]
    fn none_scope_omits_affinity_blocks() {
        let a = make_affinity(0.8, 0.8, 0.8, 0.8, 0.8, 0.8);
        let p = build_prompt(
            &fixture_persona(),
            &[],
            &[],
            Some(&a),
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::none(),
            &[],
            &[],
            &[],
        );
        assert!(!p.contains("[feelings]"));
        assert!(!p.contains("[mood]"));
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

    #[test]
    fn build_prompt_renders_anti_templating_directives() {
        let s = build_prompt(
            &fixture_persona(),
            &[],
            &[],
            None,
            ReplyStyle::Neutral,
            &[],
            &[],
            AffinityScope::full(),
            &[],
            &[],
            &[],
        );
        assert!(s.contains("别开口就自述动作或凝视"), "anti-self-narration: {s}");
        assert!(s.contains("少用省略号"), "ellipsis restraint: {s}");
        assert!(
            s.contains("不要连续两句都以「我」开头"),
            "chinese first-person-opening rule: {s}"
        );
        // Still sit inside the iron-rules block, before [output].
        let iron = s.find("[iron_rules").expect("[iron_rules] present");
        let directive = s.find("别开口就自述动作或凝视").expect("directive present");
        let output = s.find("[output]").expect("[output] present");
        assert!(iron < directive, "directive must be inside the iron-rules block");
        assert!(directive < output, "directive must come before [output]");
    }
}
