// SPDX-License-Identifier: AGPL-3.0-only
//! World Memories director sweeper (spec §2).
//!
//! Per tick: backfill state rows for new enrollments, claim due owners
//! (SKIP LOCKED, dreaming-style), and run one structured LLM round per
//! claimed owner: previous seed + active roster + extracted memory feedback
//! → new seed + per-persona digests + script fragments. Persistence is a
//! single transaction; ANY failure releases the claim and the owner retries
//! at its next due scan. No retry queue, no partial writes.

use serde::Deserialize;
use uuid::Uuid;

use eros_engine_llm::model_config::ResolvedWorldDirector;
use eros_engine_llm::openrouter::{ChatMessage, ChatRequest};
use eros_engine_store::world::{FragmentInsert, PostInsert, RosterEntry, WorldRepo};

use crate::state::AppState;

const WORLD_TASK: &str = "world_director";
/// Sentinel OpenRouter `user` for world-subsystem background calls. Distinct
/// from dreaming's `...111` so OpenRouter spend is attributable per subsystem
/// (spec §2.6). Not a real auth UUID; cannot collide with a real user.
pub(crate) const WORLD_AUDIT_USER: &str = "11111111-1111-1111-1111-111111111112";
/// Max owners claimed per tick.
const WORLD_PICK_BATCH: i64 = 5;
/// Claim considered crashed after this (spec §2.2).
const WORLD_CLAIM_STALE: std::time::Duration = std::time::Duration::from_secs(1800);
/// Roster cap per world (spec §2.3): earliest-created wins, warn on truncation.
const WORLD_ROSTER_CAP: usize = 8;
/// Memory-feedback rows per round (spec §2.3).
const WORLD_FEEDBACK_K: i64 = 15;
/// Defensive cap on fragments accepted per persona per round.
const WORLD_FRAGMENTS_PER_PERSONA_CAP: usize = 6;

/// Fixed engine-owned rules appended to every director payload. The
/// operator-owned filter_prompt carries tone/genre; these are the floor.
const WORLD_DIRECTOR_RULES: &str = "规则：\
1) 用户是场外人：可以被角色们自然提及，但绝不能编造用户做过的事或说过的话。\
2) seed 描述角色之间的关系图与剧情弧线，供下一轮延续。\
3) 每个角色输出 digest（该角色视角的世界近况摘要，1-2 句）和 script_fragments\
（当期发生的具体事件片段，每条一句、自成一体、适合单独召回）。\
4) 只使用给出的 instance_id。";

/// Appended to WORLD_DIRECTOR_RULES only for town-enabled owners.
const WORLD_TOWN_POST_RULES: &str = "\
5) posts：为部分角色生成朋友圈式贴文（不是每个角色都要发；没有合适内容就输出空数组）。\
每条含 instance_id、content（贴文正文，第一人称）、publish_at（ISO-8601 时间戳，\
安排在未来一个周期内的自然时刻）。";

#[derive(Debug, Deserialize)]
struct DirectorOutput {
    seed: serde_json::Value,
    personas: Vec<DirectorPersona>,
    #[serde(default)]
    posts: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct DirectorPersona {
    instance_id: Uuid,
    digest: String,
    #[serde(default)]
    script_fragments: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DirectorPost {
    instance_id: Uuid,
    content: String,
    publish_at: String,
}

/// Run forever; spawn once at boot. Inert when WORLD_DISABLED is set or
/// `[tasks.world_director]` is absent/blank.
pub async fn sweeper(state: AppState) {
    if state.config.world.disabled {
        tracing::info!("world sweeper disabled (WORLD_DISABLED)");
        return;
    }
    let Some(resolved) = state.model_config.resolve_world_director() else {
        tracing::info!("world_director not configured — world sweeper inert");
        return;
    };
    let tick_interval = state.config.world.tick;
    if tick_interval.is_zero() {
        tracing::info!("world sweeper disabled (WORLD_TICK_SECS=0)");
        return;
    }
    tracing::info!(
        ?tick_interval,
        interval_hours = resolved.interval_hours,
        retention_days = resolved.retention_days,
        "world sweeper starting"
    );
    let mut tick = tokio::time::interval(tick_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        match run_round(&state, &resolved).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(processed = n, "world: director rounds completed"),
            Err(e) => tracing::warn!("world: round scan failed: {e}"),
        }
    }
}

/// One tick: backfill + claim + per-owner rounds. Per-owner failures release
/// that owner's claim and continue with the rest.
async fn run_round(
    state: &AppState,
    resolved: &ResolvedWorldDirector,
) -> Result<usize, sqlx::Error> {
    let repo = WorldRepo { pool: &state.pool };
    repo.ensure_states_for_enrollments().await?;
    let interval = std::time::Duration::from_secs(u64::from(resolved.interval_hours) * 3600);
    let owners = repo
        .claim_due(interval, WORLD_CLAIM_STALE, WORLD_PICK_BATCH)
        .await?;
    let mut count = 0;
    for (owner, token) in owners {
        match direct_world(state, resolved, owner, token).await {
            Ok(()) => count += 1,
            Err(e) => {
                tracing::warn!(%owner, "world: director round failed: {e}");
                if let Err(re) = repo.release_claim(owner, token).await {
                    tracing::warn!(%owner, "world: release_claim failed: {re}");
                }
            }
        }
    }
    Ok(count)
}

/// One owner's round (spec §2.3–§2.4). Any Err ⇒ caller releases the claim;
/// nothing has been written (persist_round is transactional). `token` is the
/// ownership timestamp from `claim_due`, threaded through to every write so a
/// round that outlives WORLD_CLAIM_STALE can't clobber a newer claim.
async fn direct_world(
    state: &AppState,
    resolved: &ResolvedWorldDirector,
    owner: Uuid,
    token: chrono::DateTime<chrono::Utc>,
) -> Result<(), String> {
    let repo = WorldRepo { pool: &state.pool };

    let mut roster = repo
        .list_active_roster(owner, (WORLD_ROSTER_CAP + 1) as i64)
        .await
        .map_err(|e| format!("roster load failed: {e}"))?;
    if roster.is_empty() {
        // Nothing to simulate; stamp the run so the owner isn't re-claimed
        // every tick until the interval passes.
        return repo
            .mark_ran(owner, token)
            .await
            .map_err(|e| format!("mark_ran (empty roster) failed: {e}"));
    }
    if roster.len() > WORLD_ROSTER_CAP {
        tracing::warn!(%owner, cap = WORLD_ROSTER_CAP, "world: roster truncated");
        roster.truncate(WORLD_ROSTER_CAP);
    }
    let town = !state.config.world.town_disabled
        && repo
            .town_enabled(owner)
            .await
            .map_err(|e| format!("town_enabled load failed: {e}"))?;

    let seed = repo
        .load_seed(owner)
        .await
        .map_err(|e| format!("seed load failed: {e}"))?
        .unwrap_or_else(|| serde_json::json!({}));
    let memories = repo
        .recent_extracted_memories(owner, WORLD_FEEDBACK_K)
        .await
        .map_err(|e| format!("memory feedback load failed: {e}"))?;

    let payload = director_user_payload(&seed, &roster, &memories, town);
    let req = ChatRequest {
        model: resolved.model.clone(),
        fallback_model: resolved.fallback_model.clone(),
        messages: vec![
            ChatMessage {
                role: "system".into(),
                content: resolved.director_prompt.clone(),
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
            .then(|| world_director_response_format(town)),
        ..Default::default()
    };
    let raw = state
        .openrouter
        .execute(req)
        .await
        .map_err(|e| format!("world_director LLM call failed: {e}"))?;
    super::log_openrouter_usage(WORLD_TASK, None, &raw);

    let output = parse_director_output(&raw.reply)
        .ok_or_else(|| "world_director output did not parse".to_string())?;

    // Keep only personas that exist in THIS roster; cap fragments per persona.
    let roster_ids: std::collections::HashSet<Uuid> =
        roster.iter().map(|r| r.instance_id).collect();
    let mut digests = serde_json::Map::new();
    let mut fragments: Vec<(Uuid, String)> = Vec::new();
    for p in output.personas {
        if !roster_ids.contains(&p.instance_id) {
            tracing::warn!(%owner, instance = %p.instance_id, "world: unknown instance dropped");
            continue;
        }
        if !p.digest.trim().is_empty() {
            digests.insert(
                p.instance_id.to_string(),
                serde_json::Value::String(p.digest.trim().to_string()),
            );
        }
        let mut frags: Vec<String> = p
            .script_fragments
            .into_iter()
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty())
            .collect();
        if frags.len() > WORLD_FRAGMENTS_PER_PERSONA_CAP {
            tracing::warn!(%owner, instance = %p.instance_id, "world: fragments truncated");
            frags.truncate(WORLD_FRAGMENTS_PER_PERSONA_CAP);
        }
        fragments.extend(frags.into_iter().map(|f| (p.instance_id, f)));
    }

    // Batch-embed all fragments in one Voyage call (order-preserving).
    let texts: Vec<&str> = fragments.iter().map(|(_, f)| f.as_str()).collect();
    let embeddings = state
        .voyage
        .embed_documents(&texts)
        .await
        .map_err(|e| format!("voyage embed_documents failed: {e}"))?;
    let inserts: Vec<FragmentInsert> = fragments
        .into_iter()
        .zip(embeddings)
        .map(|((instance_id, content), embedding)| FragmentInsert {
            instance_id,
            content,
            embedding,
        })
        .collect();

    let posts = if town {
        validate_director_posts(
            &output.posts,
            &roster_ids,
            chrono::Utc::now(),
            resolved.interval_hours,
            owner,
        )
    } else {
        Vec::new()
    };

    let script_date = chrono::Utc::now().date_naive();
    repo.persist_round(
        owner,
        &output.seed,
        &serde_json::Value::Object(digests),
        &inserts,
        &posts,
        script_date,
        resolved.retention_days,
        token,
    )
    .await
    .map_err(|e| format!("persist_round failed: {e}"))
}

/// Assemble the director's user message: framing header + structured JSON of
/// previous seed / roster / memory feedback + the fixed rules. `town` appends
/// `WORLD_TOWN_POST_RULES` (the posts rule) for town-enabled owners only —
/// memories-only owners see no mention of posts.
fn director_user_payload(
    seed: &serde_json::Value,
    roster: &[RosterEntry],
    memories: &[String],
    town: bool,
) -> String {
    let is_init = seed.as_object().map(|o| o.is_empty()).unwrap_or(false);
    let personas: Vec<serde_json::Value> = roster
        .iter()
        .map(|r| {
            serde_json::json!({
                "instance_id": r.instance_id,
                "name": r.name,
                "personality": r.tip_personality,
                "profile": r.art_metadata,
            })
        })
        .collect();
    let data = serde_json::json!({
        "previous_seed": if is_init { serde_json::Value::Null } else { seed.clone() },
        "personas": personas,
        "recent_user_memories": memories,
    });
    let header = if is_init {
        "初始化这个世界：根据下列角色设定推演他们之间的初始关系（seed），并生成当期剧本。"
    } else {
        "延续这个世界：在 previous_seed 的基础上推演关系发展，并生成当期剧本。"
    };
    let rules = if town {
        format!("{WORLD_DIRECTOR_RULES}{WORLD_TOWN_POST_RULES}")
    } else {
        WORLD_DIRECTOR_RULES.to_string()
    };
    format!(
        "{header}\n\n{}\n\n{rules}",
        serde_json::to_string_pretty(&data).unwrap_or_default()
    )
}

/// OpenRouter `response_format` for the director round. `strict` is false —
/// `seed` is deliberately free-form (the engine stores it opaquely). `town`
/// attaches the `posts` array (schema + required) only for town-enabled
/// owners — memories-only owners get no posts key in the schema at all.
fn world_director_response_format(town: bool) -> serde_json::Value {
    let mut v = serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "world_director_round",
            "strict": false,
            "schema": {
                "type": "object",
                "required": ["seed", "personas"],
                "properties": {
                    "seed": { "type": "object" },
                    "personas": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["instance_id", "digest", "script_fragments"],
                            "properties": {
                                "instance_id": { "type": "string" },
                                "digest": { "type": "string" },
                                "script_fragments": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                }
                            }
                        }
                    }
                }
            }
        }
    });
    if town {
        v["json_schema"]["schema"]["properties"]["posts"] = serde_json::json!({
            "type": "array",
            "items": {
                "type": "object",
                "required": ["instance_id", "content", "publish_at"],
                "properties": {
                    "instance_id": { "type": "string" },
                    "content": { "type": "string" },
                    "publish_at": { "type": "string" }
                }
            }
        });
        v["json_schema"]["schema"]["required"] = serde_json::json!(["seed", "personas", "posts"]);
    }
    v
}

/// Lenient parse: direct JSON first, then the shared balanced-brace block
/// extractor (`super::find_json_block`, in pipeline/mod.rs) for models that
/// wrap JSON in prose/fences.
fn parse_director_output(raw: &str) -> Option<DirectorOutput> {
    if let Ok(v) = serde_json::from_str::<DirectorOutput>(raw) {
        return Some(v);
    }
    let block = super::find_json_block(raw)?;
    serde_json::from_str::<DirectorOutput>(block).ok()
}

/// Validate + clamp the director's raw `posts` entries (spec town §2): keep
/// only roster instances with non-blank content and a parseable ISO-8601
/// publish_at, clamped into `[now, now + horizon_hours]`. Malformed entries
/// are dropped with a warn, mirroring unknown-persona handling.
fn validate_director_posts(
    raw: &[serde_json::Value],
    roster_ids: &std::collections::HashSet<Uuid>,
    now: chrono::DateTime<chrono::Utc>,
    horizon_hours: u32,
    owner: Uuid,
) -> Vec<PostInsert> {
    let max_at = now + chrono::Duration::hours(i64::from(horizon_hours));
    let mut out = Vec::new();
    for entry in raw {
        let Ok(p) = serde_json::from_value::<DirectorPost>(entry.clone()) else {
            tracing::warn!(%owner, "world: malformed post entry dropped");
            continue;
        };
        if !roster_ids.contains(&p.instance_id) {
            tracing::warn!(%owner, instance = %p.instance_id, "world: post for unknown instance dropped");
            continue;
        }
        let content = p.content.trim().to_string();
        if content.is_empty() {
            tracing::warn!(%owner, "world: blank post content dropped");
            continue;
        }
        let Ok(at) = chrono::DateTime::parse_from_rfc3339(&p.publish_at) else {
            tracing::warn!(%owner, publish_at = %p.publish_at, "world: unparseable publish_at dropped");
            continue;
        };
        let scheduled_at = at.with_timezone(&chrono::Utc).clamp(now, max_at);
        out.push(PostInsert {
            instance_id: p.instance_id,
            content,
            scheduled_at,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_audit_user_is_distinct_from_dreaming() {
        assert_eq!(WORLD_AUDIT_USER, "11111111-1111-1111-1111-111111111112");
        assert!(WORLD_AUDIT_USER.ends_with('2'));
    }

    #[test]
    fn parse_director_output_handles_clean_and_fenced_json() {
        let id = Uuid::new_v4();
        let clean = format!(
            r#"{{"seed":{{"arc":"x"}},"personas":[{{"instance_id":"{id}","digest":"d","script_fragments":["f1","f2"]}}]}}"#
        );
        let out = parse_director_output(&clean).expect("clean parses");
        assert_eq!(out.personas.len(), 1);
        assert_eq!(out.personas[0].script_fragments, vec!["f1", "f2"]);
        assert!(
            out.posts.is_empty(),
            "clean sample has no posts key — #[serde(default)] must still parse"
        );

        let fenced = format!("好的：\n```json\n{clean}\n```");
        assert!(parse_director_output(&fenced).is_some(), "fenced parses");

        assert!(parse_director_output("no json at all").is_none());
        assert!(
            parse_director_output(r#"{"personas": []}"#).is_none(),
            "missing seed ⇒ None"
        );
    }

    #[test]
    fn director_payload_flags_init_vs_continuation() {
        let roster = vec![RosterEntry {
            instance_id: Uuid::new_v4(),
            name: "Aria".into(),
            tip_personality: Some("温柔".into()),
            art_metadata: serde_json::json!({"backstory": "咖啡店店主"}),
        }];
        let init = director_user_payload(&serde_json::json!({}), &roster, &[], false);
        assert!(init.contains("初始化这个世界"));
        assert!(init.contains("\"previous_seed\": null"));
        assert!(init.contains("Aria"));
        assert!(init.contains("用户是场外人"), "fixed rules always present");

        let cont = director_user_payload(
            &serde_json::json!({"arc": "opening"}),
            &roster,
            &["用户喜欢旅行".into()],
            false,
        );
        assert!(cont.contains("延续这个世界"));
        assert!(cont.contains("\"arc\": \"opening\""));
        assert!(cont.contains("用户喜欢旅行"));
    }

    #[test]
    fn world_director_response_format_shape() {
        let v = world_director_response_format(false);
        assert_eq!(v["type"], "json_schema");
        assert_eq!(v["json_schema"]["strict"], false);
        let required = v["json_schema"]["schema"]["required"].as_array().unwrap();
        assert!(required.iter().any(|r| r == "seed"));
        assert!(required.iter().any(|r| r == "personas"));
    }

    #[test]
    fn director_posts_validated_clamped_and_dropped() {
        let inst = Uuid::new_v4();
        let stranger = Uuid::new_v4();
        let roster_ids: std::collections::HashSet<Uuid> = [inst].into_iter().collect();
        let now = chrono::Utc::now();
        let horizon_hours = 24u32;
        let raw = serde_json::json!([
            // valid, inside window
            {"instance_id": inst, "content": "去了海边", "publish_at": (now + chrono::Duration::hours(2)).to_rfc3339()},
            // beyond window ⇒ clamped to now + horizon
            {"instance_id": inst, "content": "远期计划", "publish_at": (now + chrono::Duration::hours(100)).to_rfc3339()},
            // past ⇒ clamped up to now
            {"instance_id": inst, "content": "旧闻", "publish_at": (now - chrono::Duration::hours(5)).to_rfc3339()},
            // unknown instance ⇒ dropped
            {"instance_id": stranger, "content": "x", "publish_at": now.to_rfc3339()},
            // malformed timestamp ⇒ dropped
            {"instance_id": inst, "content": "y", "publish_at": "not-a-date"},
            // blank content ⇒ dropped
            {"instance_id": inst, "content": "  ", "publish_at": now.to_rfc3339()},
        ]);
        let posts = validate_director_posts(
            raw.as_array().unwrap(),
            &roster_ids,
            now,
            horizon_hours,
            Uuid::new_v4(),
        );
        assert_eq!(posts.len(), 3);
        let max_at = now + chrono::Duration::hours(i64::from(horizon_hours));
        assert!(posts
            .iter()
            .all(|p| p.scheduled_at >= now && p.scheduled_at <= max_at));
        assert_eq!(posts[2].scheduled_at, now, "past publish_at clamps to now");
    }

    #[test]
    fn world_director_response_format_town_arm() {
        let base = world_director_response_format(false);
        assert!(base["json_schema"]["schema"]["properties"]["posts"].is_null());
        let town = world_director_response_format(true);
        assert_eq!(
            town["json_schema"]["schema"]["properties"]["posts"]["type"],
            "array"
        );
        let required = town["json_schema"]["schema"]["required"]
            .as_array()
            .unwrap();
        assert!(required.iter().any(|r| r == "posts"));
    }

    #[test]
    fn director_payload_mentions_posts_only_for_town() {
        let roster = vec![RosterEntry {
            instance_id: Uuid::new_v4(),
            name: "Aria".into(),
            tip_personality: None,
            art_metadata: serde_json::json!({}),
        }];
        let plain = director_user_payload(&serde_json::json!({}), &roster, &[], false);
        assert!(!plain.contains("posts"), "no posts rule for memories-only");
        let town = director_user_payload(&serde_json::json!({}), &roster, &[], true);
        assert!(
            town.contains("posts"),
            "town payload carries the posts rule"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn direct_world_persists_seed_and_digests_without_fragments(pool: sqlx::PgPool) {
        use wiremock::matchers::{method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let owner = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('W','p','{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) \
             VALUES ($1,$2) RETURNING id",
        )
        .bind(genome_id)
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO engine.world_enrollments (owner_uid) VALUES ($1)")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();

        let reply = serde_json::json!({
            "seed": {"arc": "第一幕"},
            "personas": [{
                "instance_id": instance_id,
                "digest": "W 在筹备开店",
                "script_fragments": []
            }]
        });
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "gen-world", "model": "w/m",
                "choices": [{"message": {"content": reply.to_string()}}],
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.world_director]\nmodel=\"w/m\"\nfilter_prompt=\"direct\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                Default::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        let resolved = state.model_config.resolve_world_director().unwrap();

        let repo = eros_engine_store::world::WorldRepo { pool: &pool };
        repo.ensure_states_for_enrollments().await.unwrap();
        // Claim the owner so claimed_at is actually SET before direct_world
        // runs persist_round — otherwise the `assert!(claimed.is_none())`
        // below is vacuous (never-claimed rows are already NULL).
        let claimed = repo
            .claim_due(
                std::time::Duration::from_secs(24 * 3600),
                std::time::Duration::from_secs(1800),
                5,
            )
            .await
            .unwrap();
        let (_o, token) = claimed[0];

        direct_world(&state, &resolved, owner, token)
            .await
            .expect("round ok");

        let (seed, digests, version, claimed): (
            serde_json::Value,
            serde_json::Value,
            i32,
            Option<chrono::DateTime<chrono::Utc>>,
        ) = sqlx::query_as(
            "SELECT seed, digests, seed_version, claimed_at \
             FROM engine.world_states WHERE owner_uid = $1",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(seed["arc"], "第一幕");
        assert_eq!(digests[instance_id.to_string()], "W 在筹备开店");
        assert_eq!(version, 2);
        assert!(claimed.is_none());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn direct_world_parse_failure_writes_nothing(pool: sqlx::PgPool) {
        use wiremock::matchers::{method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let owner = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('X','p','{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1,$2)")
            .bind(genome_id)
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO engine.world_enrollments (owner_uid) VALUES ($1)")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "gen-bad", "model": "w/m",
                "choices": [{"message": {"content": "这不是 JSON"}}],
            })))
            .mount(&mock)
            .await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.world_director]\nmodel=\"w/m\"\nfilter_prompt=\"direct\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                Default::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        let resolved = state.model_config.resolve_world_director().unwrap();
        let repo = eros_engine_store::world::WorldRepo { pool: &pool };
        repo.ensure_states_for_enrollments().await.unwrap();
        let claimed = repo
            .claim_due(
                std::time::Duration::from_secs(24 * 3600),
                std::time::Duration::from_secs(1800),
                5,
            )
            .await
            .unwrap();
        let (_o, token) = claimed[0];

        let err = direct_world(&state, &resolved, owner, token)
            .await
            .unwrap_err();
        assert!(err.contains("did not parse"));

        // Nothing persisted: version still 1, seed still {}, no memories.
        let (seed, version): (serde_json::Value, i32) = sqlx::query_as(
            "SELECT seed, seed_version FROM engine.world_states WHERE owner_uid = $1",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(seed, serde_json::json!({}));
        assert_eq!(version, 1);
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.world_memories WHERE owner_uid = $1")
                .bind(owner)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn direct_world_skips_posts_when_town_disabled(pool: sqlx::PgPool) {
        use wiremock::matchers::{method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let owner = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('W','p','{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) \
             VALUES ($1,$2) RETURNING id",
        )
        .bind(genome_id)
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        // Owner is town-enrolled at the DB level — the kill-switch must win
        // over this, not just gate on it.
        sqlx::query(
            "INSERT INTO engine.world_enrollments (owner_uid, town_enabled) VALUES ($1, true)",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();

        let reply = serde_json::json!({
            "seed": {"arc": "第一幕"},
            "personas": [{
                "instance_id": instance_id,
                "digest": "W 在筹备开店",
                "script_fragments": []
            }],
            "posts": [{
                "instance_id": instance_id,
                "content": "开业啦",
                "publish_at": chrono::Utc::now().to_rfc3339()
            }]
        });
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "gen-world", "model": "w/m",
                "choices": [{"message": {"content": reply.to_string()}}],
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.config.world.town_disabled = true;
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.world_director]\nmodel=\"w/m\"\nfilter_prompt=\"direct\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                Default::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        let resolved = state.model_config.resolve_world_director().unwrap();

        let repo = eros_engine_store::world::WorldRepo { pool: &pool };
        repo.ensure_states_for_enrollments().await.unwrap();
        let claimed = repo
            .claim_due(
                std::time::Duration::from_secs(24 * 3600),
                std::time::Duration::from_secs(1800),
                5,
            )
            .await
            .unwrap();
        let (_o, token) = claimed[0];

        direct_world(&state, &resolved, owner, token)
            .await
            .expect("round ok");

        let posts: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.world_posts WHERE owner_uid = $1")
                .bind(owner)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            posts, 0,
            "town_disabled must suppress posts even when town_enabled=true"
        );

        let (seed, digests): (serde_json::Value, serde_json::Value) =
            sqlx::query_as("SELECT seed, digests FROM engine.world_states WHERE owner_uid = $1")
                .bind(owner)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(seed["arc"], "第一幕");
        assert_eq!(digests[instance_id.to_string()], "W 在筹备开店");
    }
}
