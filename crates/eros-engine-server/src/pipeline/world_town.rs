// SPDX-License-Identifier: AGPL-3.0-only
//! World Town sweeper (town spec §3): per 30s tick — publish due posts
//! (pure SQL), run hourly CAS-claimed comment rounds (one batched
//! `[tasks.world_comment]` call per owner with activity), and answer user
//! comments via the debounced/cooldown/capped `[tasks.world_reply]` path.
//! Every LLM/parse failure warns and moves on: the publish flip is
//! idempotent, a lost comment round catches up next hour, and a lost reply
//! retries after its cooldown.

use serde::Deserialize;
use uuid::Uuid;

use eros_engine_llm::model_config::{ResolvedWorldComment, ResolvedWorldReply};
use eros_engine_llm::openrouter::{ChatMessage, ChatRequest};
use eros_engine_store::world::WorldRepo;
use eros_engine_store::world_town::{FeedComment, FeedPost, ReplyCandidate, WorldTownRepo};

use super::world::WORLD_AUDIT_USER;
use crate::state::AppState;

/// Deliberately faster than the director's WORLD_TICK_SECS (spec §3):
/// publish precision + reply debounce need finer granularity; every town
/// path is a cheap indexed scan.
const TOWN_TICK: std::time::Duration = std::time::Duration::from_secs(30);
/// Posts (with threads) fed to one comment round, newest first.
const TOWN_ROUND_POSTS_CAP: i64 = 10;
/// Reply candidates processed per tick.
const TOWN_REPLY_BATCH: i64 = 10;
/// Comments accepted from one comment round (defensive cap, mirrors
/// WORLD_FRAGMENTS_PER_PERSONA_CAP's role).
const TOWN_ROUND_COMMENTS_CAP: usize = 12;

const WORLD_COMMENT_RULES: &str = "规则：\
1) 只使用给出的 post_id 和 instance_id；发帖者不评论自己的贴子。\
2) 评论要符合 seed 里的角色关系；贴子下有用户（user）留言时，角色可以自然回应它。\
3) 输出 0 到 N 条评论；没有值得评论的就输出空数组。每条评论一两句话，口语化。";

#[derive(Debug, Deserialize)]
struct CommentRoundOutput {
    comments: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RoundComment {
    post_id: Uuid,
    author_instance_id: Uuid,
    content: String,
}

/// Run forever; spawn once at boot. Inert when the world subsystem is off,
/// WORLD_TOWN_DISABLED is set, or the director is unconfigured (posts can
/// only exist downstream of the director — town spec §3).
pub async fn sweeper(state: AppState) {
    if state.config.world.disabled {
        tracing::info!("world_town sweeper disabled (WORLD_DISABLED)");
        return;
    }
    if state.config.world.town_disabled {
        tracing::info!("world_town sweeper disabled (WORLD_TOWN_DISABLED)");
        return;
    }
    if state.model_config.resolve_world_director().is_none() {
        tracing::info!("world_director not configured — world_town sweeper inert");
        return;
    }
    let comment = state.model_config.resolve_world_comment();
    let reply = state.model_config.resolve_world_reply();
    tracing::info!(
        comment_round = comment.is_some(),
        reply_responder = reply.is_some(),
        "world_town sweeper starting"
    );
    let mut tick = tokio::time::interval(TOWN_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        run_tick(&state, comment.as_ref(), reply.as_ref()).await;
    }
}

/// One tick: publish, comment rounds, replies. Each path degrades
/// independently — a failure in one never blocks the others.
async fn run_tick(
    state: &AppState,
    comment: Option<&ResolvedWorldComment>,
    reply: Option<&ResolvedWorldReply>,
) {
    let repo = WorldTownRepo { pool: &state.pool };
    match repo.publish_due().await {
        Ok(0) => {}
        Ok(n) => tracing::info!(published = n, "world_town: posts published"),
        Err(e) => tracing::warn!("world_town: publish scan failed: {e}"),
    }
    if let Some(resolved) = comment {
        let round = std::time::Duration::from_secs(resolved.round_secs);
        match repo.list_round_candidates(round).await {
            Ok(owners) => {
                for owner in owners {
                    if let Err(e) = run_comment_round(state, resolved, owner).await {
                        tracing::warn!(%owner, "world_town: comment round failed: {e}");
                    }
                }
            }
            Err(e) => tracing::warn!("world_town: round candidate scan failed: {e}"),
        }
    }
    if let Some(resolved) = reply {
        let debounce = std::time::Duration::from_secs(resolved.debounce_secs);
        match repo.list_reply_candidates(debounce, TOWN_REPLY_BATCH).await {
            Ok(cands) => {
                for cand in cands {
                    if let Err(e) = run_reply(state, resolved, &cand).await {
                        tracing::warn!(post = %cand.post_id, "world_town: reply failed: {e}");
                    }
                }
            }
            Err(e) => tracing::warn!("world_town: reply candidate scan failed: {e}"),
        }
    }
}

/// One owner's comment round (spec §3.2). The CAS stamps the round BEFORE
/// the activity check: a no-activity owner costs one cheap query per cadence
/// and no LLM call. Call/parse failure after the CAS loses this round —
/// accepted; threads catch up next hour.
async fn run_comment_round(
    state: &AppState,
    resolved: &ResolvedWorldComment,
    owner: Uuid,
) -> Result<(), String> {
    let repo = WorldTownRepo { pool: &state.pool };
    let round = std::time::Duration::from_secs(resolved.round_secs);
    let Some(prev) = repo
        .claim_comment_round(owner, round)
        .await
        .map_err(|e| format!("round CAS failed: {e}"))?
    else {
        return Ok(()); // contended or no longer due
    };
    if !repo
        .has_town_activity_since(owner, prev)
        .await
        .map_err(|e| format!("activity check failed: {e}"))?
    {
        return Ok(()); // quiet world — no call, zero cost
    }

    let world_repo = WorldRepo { pool: &state.pool };
    let seed = world_repo
        .load_seed(owner)
        .await
        .map_err(|e| format!("seed load failed: {e}"))?
        .unwrap_or_else(|| serde_json::json!({}));
    let roster = world_repo
        .list_active_roster(owner, i64::from(u8::MAX))
        .await
        .map_err(|e| format!("roster load failed: {e}"))?;
    if roster.is_empty() {
        return Ok(());
    }
    let posts = repo
        .feed_page(owner, TOWN_ROUND_POSTS_CAP, None)
        .await
        .map_err(|e| format!("posts load failed: {e}"))?;
    if posts.is_empty() {
        return Ok(());
    }
    let post_ids: Vec<Uuid> = posts.iter().map(|p| p.post_id).collect();
    let comments = repo
        .list_comments_for_posts(&post_ids)
        .await
        .map_err(|e| format!("threads load failed: {e}"))?;

    let payload = comment_round_payload(&seed, &roster, &posts, &comments);
    let req = ChatRequest {
        model: resolved.model.clone(),
        fallback_model: resolved.fallback_model.clone(),
        messages: vec![
            ChatMessage {
                role: "system".into(),
                content: resolved.comment_prompt.clone(),
            },
            ChatMessage {
                role: "user".into(),
                content: payload,
            },
        ],
        temperature: resolved.temperature as f32,
        max_tokens: resolved.max_tokens,
        user: Some(WORLD_AUDIT_USER.into()),
        reasoning: resolved.reasoning.clone(),
        response_format: resolved
            .structured_output
            .then(world_comment_response_format),
        ..Default::default()
    };
    let raw = state
        .openrouter
        .execute(req)
        .await
        .map_err(|e| format!("world_comment LLM call failed: {e}"))?;
    super::log_openrouter_usage("world_comment", None, &raw);

    let output = parse_comment_output(&raw.reply)
        .ok_or_else(|| "world_comment output did not parse".to_string())?;
    let mut inserted = 0usize;
    for entry in output.comments.into_iter().take(TOWN_ROUND_COMMENTS_CAP) {
        let Ok(c) = serde_json::from_value::<RoundComment>(entry) else {
            tracing::warn!(%owner, "world_town: malformed round comment dropped");
            continue;
        };
        let content = c.content.trim();
        if content.is_empty() {
            continue;
        }
        match repo
            .insert_round_comment(owner, c.post_id, c.author_instance_id, content)
            .await
        {
            Ok(true) => inserted += 1,
            Ok(false) => {
                tracing::warn!(%owner, post = %c.post_id, author = %c.author_instance_id,
                    "world_town: invalid round comment dropped");
            }
            Err(e) => return Err(format!("round comment insert failed: {e}")),
        }
    }
    if inserted > 0 {
        tracing::info!(%owner, inserted, "world_town: comment round persisted");
    }
    Ok(())
}

/// One reply-responder pass for a candidate post (spec §3.3). Gate order:
/// daily cap BEFORE the cooldown CAS so hitting the cap never burns a
/// cooldown stamp. Failures after the CAS wait out the cooldown — accepted.
async fn run_reply(
    state: &AppState,
    resolved: &ResolvedWorldReply,
    cand: &ReplyCandidate,
) -> Result<(), String> {
    let repo = WorldTownRepo { pool: &state.pool };
    let spent = repo
        .count_replies_today(cand.owner_uid)
        .await
        .map_err(|e| format!("daily cap count failed: {e}"))?;
    if spent >= i64::from(resolved.daily_cap) {
        return Ok(()); // silent skip (spec: no failure state on the feed)
    }
    let cooldown = std::time::Duration::from_secs(resolved.thread_cooldown_secs);
    if !repo
        .claim_reply_cooldown(cand.post_id, cooldown)
        .await
        .map_err(|e| format!("cooldown CAS failed: {e}"))?
    {
        return Ok(()); // cooling down or another instance owns it
    }

    let world_repo = WorldRepo { pool: &state.pool };
    let seed = world_repo
        .load_seed(cand.owner_uid)
        .await
        .map_err(|e| format!("seed load failed: {e}"))?
        .unwrap_or_else(|| serde_json::json!({}));
    let Some(post) = repo
        .get_post(cand.post_id)
        .await
        .map_err(|e| format!("post load failed: {e}"))?
    else {
        return Ok(());
    };
    let thread = repo
        .list_comments_for_posts(&[cand.post_id])
        .await
        .map_err(|e| format!("thread load failed: {e}"))?;

    let req = ChatRequest {
        model: resolved.model.clone(),
        fallback_model: resolved.fallback_model.clone(),
        messages: vec![
            ChatMessage {
                role: "system".into(),
                content: resolved.reply_prompt.clone(),
            },
            ChatMessage {
                role: "user".into(),
                content: reply_payload(&seed, &post, &thread),
            },
        ],
        temperature: resolved.temperature as f32,
        max_tokens: resolved.max_tokens,
        user: Some(WORLD_AUDIT_USER.into()),
        reasoning: resolved.reasoning.clone(),
        ..Default::default()
    };
    let raw = state
        .openrouter
        .execute(req)
        .await
        .map_err(|e| format!("world_reply LLM call failed: {e}"))?;
    super::log_openrouter_usage("world_reply", None, &raw);

    let content = raw.reply.trim();
    if content.is_empty() {
        return Err("world_reply returned empty content".into());
    }
    repo.insert_reply_comment(cand.post_id, cand.author_instance_id, content)
        .await
        .map_err(|e| format!("reply insert failed: {e}"))
}

/// Round payload: seed + roster (valid author ids) + posts with threads.
fn comment_round_payload(
    seed: &serde_json::Value,
    roster: &[eros_engine_store::world::RosterEntry],
    posts: &[FeedPost],
    comments: &[FeedComment],
) -> String {
    let personas: Vec<serde_json::Value> = roster
        .iter()
        .map(|r| {
            serde_json::json!({
                "instance_id": r.instance_id,
                "name": r.name,
                "personality": r.tip_personality,
            })
        })
        .collect();
    let posts_json: Vec<serde_json::Value> = posts
        .iter()
        .map(|p| {
            let thread: Vec<serde_json::Value> = comments
                .iter()
                .filter(|c| c.post_id == p.post_id)
                .map(|c| {
                    serde_json::json!({
                        "author": c.author_name.clone().unwrap_or_else(|| "user".into()),
                        "content": c.content,
                    })
                })
                .collect();
            serde_json::json!({
                "post_id": p.post_id,
                "author_instance_id": p.instance_id,
                "author": p.author_name,
                "content": p.content,
                "comments": thread,
            })
        })
        .collect();
    let data = serde_json::json!({
        "seed": seed,
        "personas": personas,
        "posts": posts_json,
    });
    format!(
        "为这个世界的贴文串生成新一轮评论。\n\n{}\n\n{WORLD_COMMENT_RULES}",
        serde_json::to_string_pretty(&data).unwrap_or_default()
    )
}

/// Reply payload: the responder speaks as the post's author.
fn reply_payload(seed: &serde_json::Value, post: &FeedPost, thread: &[FeedComment]) -> String {
    let thread_json: Vec<serde_json::Value> = thread
        .iter()
        .map(|c| {
            serde_json::json!({
                "author": c.author_name.clone().unwrap_or_else(|| "user".into()),
                "content": c.content,
            })
        })
        .collect();
    let data = serde_json::json!({
        "seed": seed,
        "post": { "author": post.author_name, "content": post.content },
        "thread": thread_json,
    });
    format!(
        "你是贴文作者「{}」。用户（author 为 user 的留言）在你的贴文下留了言，\
         请以作者身份自然回应最近的用户留言。只输出评论正文，一两句话，口语化。\n\n{}",
        post.author_name,
        serde_json::to_string_pretty(&data).unwrap_or_default()
    )
}

/// OpenRouter `response_format` for the comment round. Non-strict, same
/// rationale as the director's.
fn world_comment_response_format() -> serde_json::Value {
    serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "world_comment_round",
            "strict": false,
            "schema": {
                "type": "object",
                "required": ["comments"],
                "properties": {
                    "comments": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["post_id", "author_instance_id", "content"],
                            "properties": {
                                "post_id": { "type": "string" },
                                "author_instance_id": { "type": "string" },
                                "content": { "type": "string" }
                            }
                        }
                    }
                }
            }
        }
    })
}

/// Lenient parse, same two-step as the director's.
fn parse_comment_output(raw: &str) -> Option<CommentRoundOutput> {
    if let Ok(v) = serde_json::from_str::<CommentRoundOutput>(raw) {
        return Some(v);
    }
    let block = super::find_json_block(raw)?;
    serde_json::from_str::<CommentRoundOutput>(block).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn town_test_state(
        pool: sqlx::PgPool,
        mock_uri: &str,
        config_toml: &str,
    ) -> crate::state::AppState {
        let mut state = crate::routes::companion::test_state(pool);
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(config_toml).unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                Default::default(),
                format!("{mock_uri}/api/v1/chat/completions"),
            ),
        );
        state
    }

    async fn seed_town_world(pool: &sqlx::PgPool) -> (Uuid, Uuid, Uuid) {
        let owner = Uuid::new_v4();
        let g1: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('Aria','p','{}'::jsonb) RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let author: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) \
             VALUES ($1,$2) RETURNING id",
        )
        .bind(g1)
        .bind(owner)
        .fetch_one(pool)
        .await
        .unwrap();
        let g2: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('Rin','p','{}'::jsonb) RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let commenter: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) \
             VALUES ($1,$2) RETURNING id",
        )
        .bind(g2)
        .bind(owner)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.world_enrollments (owner_uid, town_enabled) VALUES ($1, true)",
        )
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.world_states (owner_uid, seed, digests) \
             VALUES ($1, '{\"arc\":\"a\"}'::jsonb, '{}'::jsonb)",
        )
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
        (owner, author, commenter)
    }

    const COMMENT_TOML: &str = "[tasks.world_comment]\nmodel=\"w/c\"\nfilter_prompt=\"comment\"\n";
    const REPLY_TOML: &str = "[tasks.world_reply]\nmodel=\"w/r\"\nfilter_prompt=\"reply\"\n";

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn comment_round_inserts_valid_and_drops_invalid(pool: sqlx::PgPool) {
        let (owner, author, commenter) = seed_town_world(&pool).await;
        let repo = WorldTownRepo { pool: &pool };
        let post: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.world_posts \
                 (owner_uid, instance_id, content, scheduled_at, published_at) \
             VALUES ($1,$2,'开店了',now(),now()) RETURNING id",
        )
        .bind(owner)
        .bind(author)
        .fetch_one(&pool)
        .await
        .unwrap();

        let reply = serde_json::json!({
            "comments": [
                { "post_id": post, "author_instance_id": commenter, "content": "恭喜！" },
                // self-reply: dropped by the INSERT validation
                { "post_id": post, "author_instance_id": author, "content": "谢谢自己" },
                // unknown post: dropped
                { "post_id": Uuid::new_v4(), "author_instance_id": commenter, "content": "x" }
            ]
        });
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "gen-town", "model": "w/c",
                "choices": [{"message": {"content": reply.to_string()}}],
            })))
            .expect(1)
            .mount(&mock)
            .await;
        let state = town_test_state(pool.clone(), &mock.uri(), COMMENT_TOML);
        let resolved = state.model_config.resolve_world_comment().unwrap();

        run_comment_round(&state, &resolved, owner)
            .await
            .expect("round ok");

        let rows: Vec<(Option<Uuid>, String)> = sqlx::query_as(
            "SELECT author_instance_id, content FROM engine.world_post_comments \
             WHERE post_id = $1 AND source = 'round'",
        )
        .bind(post)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "only the valid comment landed");
        assert_eq!(rows[0].0, Some(commenter));
        assert_eq!(rows[0].1, "恭喜！");

        // Round is stamped: immediately re-running claims nothing and makes
        // no second LLM call (mock .expect(1) enforces it on drop).
        run_comment_round(&state, &resolved, owner)
            .await
            .expect("noop ok");
        let _ = repo;
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn reply_responder_answers_and_respects_daily_cap(pool: sqlx::PgPool) {
        let (owner, author, _commenter) = seed_town_world(&pool).await;
        let repo = WorldTownRepo { pool: &pool };
        let post: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.world_posts \
                 (owner_uid, instance_id, content, scheduled_at, published_at) \
             VALUES ($1,$2,'新拉花',now(),now()) RETURNING id",
        )
        .bind(owner)
        .bind(author)
        .fetch_one(&pool)
        .await
        .unwrap();
        repo.insert_user_comment(owner, post, "好看！")
            .await
            .unwrap()
            .unwrap();
        sqlx::query(
            "UPDATE engine.world_post_comments SET created_at = now() - interval '3 minutes' \
             WHERE post_id = $1",
        )
        .bind(post)
        .execute(&pool)
        .await
        .unwrap();

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "gen-reply", "model": "w/r",
                "choices": [{"message": {"content": "谢谢，下次做给你看～"}}],
            })))
            .expect(1)
            .mount(&mock)
            .await;
        let state = town_test_state(pool.clone(), &mock.uri(), REPLY_TOML);
        let resolved = state.model_config.resolve_world_reply().unwrap();

        let cand = ReplyCandidate {
            post_id: post,
            owner_uid: owner,
            author_instance_id: author,
        };
        run_reply(&state, &resolved, &cand).await.expect("reply ok");
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.world_post_comments \
             WHERE post_id = $1 AND source = 'reply'",
        )
        .bind(post)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1);

        // At cap with a cooldown-fresh post: silent skip due to daily cap,
        // no LLM call (expect(1) still holds), and the fresh post's last_reply_at
        // remains NULL—proving the cap gate ran BEFORE the cooldown CAS.
        // If gate order were swapped (cooldown checked first), this post would
        // get a cooldown stamp despite being capped, making last_reply_at non-NULL.
        let post2: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.world_posts \
                 (owner_uid, instance_id, content, scheduled_at, published_at) \
             VALUES ($1,$2,'月亮圆圆',now(),now()) RETURNING id",
        )
        .bind(owner)
        .bind(author)
        .fetch_one(&pool)
        .await
        .unwrap();
        let cand2 = ReplyCandidate {
            post_id: post2,
            owner_uid: owner,
            author_instance_id: author,
        };
        let mut capped = resolved.clone();
        capped.daily_cap = 1;
        run_reply(&state, &capped, &cand2)
            .await
            .expect("cap skip ok");

        // Verify the second post's cooldown was not burned (spec §3.3 gate 2).
        let last_reply_at: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT last_reply_at FROM engine.world_posts WHERE id = $1")
                .bind(post2)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            last_reply_at, None,
            "cap skip should not burn cooldown on fresh post"
        );
    }

    #[test]
    fn parse_comment_output_lenient() {
        let clean = r#"{"comments":[]}"#;
        assert!(parse_comment_output(clean).is_some());
        let fenced = format!("好的：\n```json\n{clean}\n```");
        assert!(parse_comment_output(&fenced).is_some());
        assert!(parse_comment_output("nope").is_none());
    }
}
