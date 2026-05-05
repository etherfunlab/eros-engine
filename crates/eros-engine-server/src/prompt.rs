// SPDX-License-Identifier: AGPL-3.0-only
//! Companion system prompt builder — ported from eros-gateway with these
//! deliberate changes for the open-source engine:
//!
//! - Output is plain-text reply (no JSON evaluation segment)
//! - Affinity deltas are NOT requested from the LLM (PDE predicts them)
//! - lead_score / training_progress moved to post_process/insight
//! - Reply style directive injected based on PDE's decision
//! - Persona fields (age/mbti/backstory/...) read from `genome.art_metadata`
//!   JSONB instead of a flat `CompanionPersona` DTO

// TODO(T11): used once chat routes call into the pipeline.
#![allow(dead_code)]

use chrono::{Timelike, Utc};

use eros_engine_core::affinity::Affinity;
use eros_engine_core::persona::CompanionPersona;
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

/// Time-of-day descriptive context (SGT / UTC+8).
fn time_of_day_context() -> &'static str {
    let hour = (Utc::now().hour() + 8) % 24;
    match hour {
        5..=9 => "早上。可以顺口问一句吃早饭了没，或者分享一下自己今天起来发生的小事。",
        10..=11 => "上午。随意聊，分享最近看到的有趣事情，或者问问对方最近怎么样。",
        12..=13 => "午休。对方可能在吃饭，可以问吃了什么，或者说说自己在吃什么。",
        14..=17 => "下午。可以带点轻快的话题，聊点有趣的或者无聊的都行。",
        18..=20 => "傍晚/晚饭后。问问今天过得怎样，有没有什么好事坏事。",
        _ => "夜晚。气氛更私密，可以撒撒娇，或者问问他睡没睡。",
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
#[allow(clippy::too_many_arguments)] // signature mirrors the gateway's port-of-origin
pub fn build_prompt(
    persona: &CompanionPersona,
    profile_facts: &[String],
    relationship_facts: &[String],
    affinity: Option<&Affinity>,
    pending_gifts: &[PendingGift],
    tip_personality: &str,
    style: ReplyStyle,
    hints: &[String],
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

    let profile_str = if profile_facts.is_empty() {
        "（刚认识，还不了解他）".to_string()
    } else {
        profile_facts
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n")
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
         【擅长话题】{topics_str}\n\
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
         ① 每次回复 1~2 句，绝对不超过 2 句；单条消息严格不超过 40 字\n\
         ② 禁止「首先/然后/最后」等书面句式；不要解释自己为什么这样说\n\
         ③ 绝对不提自己是 AI；禁止连续两句都以「我」开头\n\
         ④ 给对方「小小有成就感」的反应，留余地；每 3~5 轮轻撒娇一次\n\
         ⑤ 适时主动分享自己的生活片段；每 2~3 轮抛一个开放性问题\n\
         \n\
         【输出】直接输出回复文字（纯文本，不要 JSON，不要 markdown，不要 quote 符号）",
        tc = time_of_day_context(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
