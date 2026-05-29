# v0.5.1: NFT-ownership stack teardown + filter_triggers reshape + companion_insights_snapshot

Three independent slices grouped under a single v0.5.1 release window. Drives:

1. Drop the entire NFT-ownership mirror (`wallet_links`, `persona_ownership`, `sync_cursors`, the `/s2s/wallets/*` + `/s2s/ownership/*` endpoints, and the `enforce_nft_ownership` gate). User→wallet binding is a downstream concern; engine no longer mirrors or gates on it.
2. Reshape `chat_messages.filter_triggers` JSONB from a mixed config-plus-observed format to predicate config-as-declared, so an auditor reading the row can see which TOML predicate fired without disambiguating `traits: []` vs `when="absent"`.
3. Add `engine.companion_insights_snapshot` time-series table + a cron-scheduled sweeper that writes one row per `companion_insights` user at every fire. Pure write-through history capture; no dedupe, no transformation, no LLM. Data egress for the private downstream worker discussed in `eros-engine-web#181`.

The slices are independent in code, schema, and review. They ship as three separate PRs squashed onto `dev`, and a single `chore(release): v0.5.1` PR squashed onto `main`.

---

## §1 — NFT-ownership stack teardown

### Motivation

`engine.wallet_links` mirrors `marketplace-svc`'s wallet↔user bindings, fed by `/s2s/wallets/upsert` and a self-heal `/s2s/wallets/since` pull. `engine.persona_ownership` mirrors NFT asset ownership. `OwnershipRepo::owns()` joins the two to enforce a per-request NFT-holding gate (`enforce_nft_ownership` in `routes/companion.rs:81`), called at `chat/start`, `chat/message`, and `chat-stream`. The gate's premise — engine knows who owns which NFT and who controls which wallet — pushes the entire wallet identity stack into OSS engine. That's the wrong boundary: engine should be chat + insights only; user→wallet identity is a downstream private concern. With the gate removed, the mirror tables, the sync pipeline, and the `/s2s/wallets|ownership` endpoints all become dead weight.

### Migration

New migration `0023_drop_nft_ownership_stack.sql` (number assumes this slice lands last per the recommended PR order in §"Release layout"):

```sql
-- SPDX-License-Identifier: AGPL-3.0-only
-- Spec: docs/superpowers/specs/2026-05-29-engine-cleanup-and-snapshot-design.md §1
--
-- DESTRUCTIVE. v0.5.1 BREAKING. Removes the entire NFT-ownership mirror.
-- engine no longer gates on wallet/asset ownership; user→wallet binding
-- is a downstream concern.

DROP TABLE engine.wallet_links;
DROP TABLE engine.persona_ownership;
DROP TABLE engine.sync_cursors;
ALTER TABLE engine.persona_genomes DROP COLUMN asset_id;
```

Notes:
- The 0013 supabase_lockdown REVOKE/RLS statements that referenced these tables become no-ops (the targets are gone). 0016 lockdown_sqlx_migrations is untouched.
- Migration is irreversible. Release note must carry a top-line BREAKING marker.
- The 0023 number assumes the recommended PR landing order (snapshot → filter → nft drop). If the order is reshuffled, this becomes whatever the next-unused number is at merge time.

### Files deleted (whole-file)

- `crates/eros-engine-store/src/wallets.rs`
- `crates/eros-engine-store/src/ownership.rs`
- `crates/eros-engine-store/src/sync_cursors.rs`
- `crates/eros-engine-store/src/pubkey.rs` (`validate_solana_pubkey` is used only by the s2s wallets/ownership handlers)
- `crates/eros-engine-server/src/pipeline/sync.rs` (whole file; both `tick_wallets` and `tick_ownership`)
- `crates/eros-engine-server/src/routes/s2s.rs` (whole file; all four handlers are wallets/ownership)
- `crates/eros-engine-server/src/auth/s2s.rs` (whole file; the HMAC `require_s2s` middleware and `build_outbound_signature` helper exist solely to authenticate the deleted s2s routes and the deleted self-heal pulls)

### Files edited

- `crates/eros-engine-store/src/lib.rs`: remove `pub mod` lines for `wallets`, `ownership`, `sync_cursors`, `pubkey`. Remove three migration tests: `wallet_links_schema_is_correct`, `persona_ownership_and_sync_cursors_schema`, `persona_genomes_gains_nullable_asset_id`.
- `crates/eros-engine-store/src/persona.rs`: remove `asset_id` field from `Genome` and `GenomeGate` structs. Remove `get_asset_id_for_genome`. Drop `asset_id` from the SELECT lists in `get_genome` and `get_genome_gate`. Update affected tests.
- `crates/eros-engine-server/src/pipeline/mod.rs`: remove `pub mod sync`.
- `crates/eros-engine-server/src/auth/mod.rs`: remove `pub mod s2s`.
- `crates/eros-engine-server/src/routes/mod.rs`: remove `pub mod s2s`, the `use auth::s2s::require_s2s` import, and the `s2s_routes` composition that layers `require_s2s` over `s2s::router()`. Update the module docstring near line 7 that lists "HMAC S2S: /s2s/*".
- `crates/eros-engine-server/src/state.rs`: remove `marketplace_s2s_secret` and `marketplace_s2s_secret_previous` fields from `AppState`.
- `crates/eros-engine-server/src/main.rs`: remove the `pipeline::sync::*` spawn (the self-heal pull loop), the `MARKETPLACE_SVC_S2S_SECRET` / `MARKETPLACE_SVC_S2S_SECRET_PREVIOUS` env reads (lines 284–290 area), and the corresponding `AppState` construction fields (lines 311–312).
- `crates/eros-engine-server/src/openapi.rs`: remove the `auth::s2s` references in the OpenAPI documentation strings (lines 16, 40 area).
- `crates/eros-engine-server/src/routes/companion.rs`: delete `enforce_nft_ownership` (line 81), delete its two call sites (lines 555, 577), delete the three NFT-gate tests near lines 1258, 1276, 1364, and delete the `marketplace_s2s_secret: None,` / `marketplace_s2s_secret_previous: None,` initializers in `test_state` (lines 914–915).
- `crates/eros-engine-server/src/routes/companion_stream.rs`: delete the gate call at line 257 and the `enforce_nft_ownership` import at line 26.

### Documentation

- `docs/api-reference.md` / `api-reference.zh.md`: remove the `/s2s/wallets/*` and `/s2s/ownership/*` sections.
- `docs/architecture.md` / `architecture.zh.md`: audit for wallet/ownership mentions; remove or rewrite.
- `docs/deploying.md` / `deploying.zh.md`: remove `MARKETPLACE_SVC_S2S_SECRET` / `MARKETPLACE_SVC_S2S_SECRET_PREVIOUS` from the env var reference, and any wallet/ownership mirror discussion.
- `README.md` / `README.zh.md`: audit for `MARKETPLACE_SVC_S2S_SECRET`, `wallet_links`, `persona_ownership`, `/s2s/wallets`, `/s2s/ownership` mentions and remove.

### Test plan

- Add a migration test verifying that after 0023, none of `engine.wallet_links`, `engine.persona_ownership`, `engine.sync_cursors` exist in `information_schema.tables`, and `engine.persona_genomes.asset_id` does not appear in `information_schema.columns`.
- The three deleted migration tests need no replacement — their assertions are subsumed by the table-absence check above.
- The three `enforce_nft_ownership` tests die with the function. No replacement.
- Existing chat-start / chat-message / chat-stream tests continue to pass (the gate becomes a no-op by being removed; legacy paths that previously skipped the gate via `asset_id IS NULL` now take the same path unconditionally).

### Blast radius / risk

- Breaking change for any caller of `/s2s/wallets/upsert`, `/s2s/wallets/since`, `/s2s/ownership/upsert`, `/s2s/ownership/since`. The only known caller is the user's private marketplace-svc, which will adapt downstream.
- Breaking change for any caller relying on the NFT-ownership gate's 403 behavior — none expected.
- `MARKETPLACE_SVC_S2S_SECRET` / `MARKETPLACE_SVC_S2S_SECRET_PREVIOUS` env vars become inert. Operators can drop them from their deployment config; the binary no longer reads them.
- Migration 0023 (per recommended order) is irreversible. The DROP statements destroy mirror data that downstream is now the authoritative source for, so no engine-side data loss is meaningful.

---

## §2 — `filter_triggers` reshape to config-as-declared

### Motivation

The current `filter_triggers` JSONB is a mix of source-config and observed values:

```json
{
  "random":  { "p": 0.30, "draw": 0.18 },
  "models":  "deepseek/deepseek-v4-flash",
  "traits":  ["nsfw_boost"]
}
```

Two problems make this hard to read:
- For a `traits` predicate configured as `{ any = ["nsfw_boost"], when = "absent" }`, a fire writes `traits: []` — readers see an empty array and have to know about the `absent`-mode convention to interpret it.
- The `models` field stores the single matched model id, not the configured allowlist — the auditor can't see what other models would have also tripped the filter.

Reshape the JSONB to echo the source `OutputFilterTrigger` predicate config verbatim, including only the predicates that fired. The auditor reads exactly what was configured.

### New shape

```json
// trigger.toml fragment:
//   traits = { any = ["nsfw_boost"], when = "absent" }
//   random = 0.30
// JSONB written when a fire passed both random and traits:
{
  "random":  0.30,
  "traits":  { "any": ["nsfw_boost"], "when": "absent" }
}
```

- `random`: scalar `f64` equal to the configured `p`. Not present if `trigger.random` is not configured.
- `models`: array of model ids equal to the configured allowlist. Not present if `trigger.models` is not configured.
- `traits`: `{ any: [...], when: "present"|"absent" }` struct echoing the configured `TraitPredicate`. Not present if `trigger.traits` is not configured.
- "Empty trigger fires unconditionally" (no `random`, no `models`, no `traits` configured): write SQL `NULL`. Reader disambiguates ran-vs-not-ran by `filter_model IS NOT NULL` — the `pre_filter_content` / `filter_model` / `f_client_msg_id` / `f_generation_id` quartet is the "filter ran" signal.

### Code changes

`crates/eros-engine-llm/src/model_config.rs`:

- Delete `TriggerHits` and `RandomHit` structs (lines 198–216).
- Introduce `FiredPredicates`:

```rust
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct FiredPredicates {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub random: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traits: Option<TraitPredicate>,
}

impl FiredPredicates {
    pub fn is_empty(&self) -> bool {
        self.random.is_none() && self.models.is_none() && self.traits.is_none()
    }
}
```

`TraitPredicate` already derives `Deserialize`; it must additionally derive `Serialize` (one-line change) so it can be re-emitted into the JSONB unchanged.

- Change `OutputFilterTrigger::should_filter`'s return type from `Option<TriggerHits>` to `Option<FiredPredicates>`. The decision logic (turn-level pass + models pass) is unchanged. On fire, build `FiredPredicates` by cloning each configured field of `self` directly — no observed-value reads:

```rust
pub fn should_filter(
    &self,
    model_id: &str,
    trait_tags: &[&str],
    random_draw: Option<f64>,
) -> Option<FiredPredicates> {
    if !self.turn_level_pass(random_draw, trait_tags) { return None; }
    if !self.models_pass(model_id) { return None; }
    Some(FiredPredicates {
        random: self.random,
        models: self.models.clone(),
        traits: self.traits.clone(),
    })
}
```

`crates/eros-engine-server/src/pipeline/stream.rs` (around line 466):

- The serialization call site stays `serde_json::to_value(&h)`. With the new `FiredPredicates`, an unconfigured-but-firing trigger serializes to `{}`. The "empty trigger fires unconditionally" case must instead write SQL NULL. Decide in the stream layer:

```rust
let filter_triggers = if h.is_empty() {
    serde_json::Value::Null
} else {
    serde_json::to_value(&h).expect("FiredPredicates Serialize is infallible")
};
```

- `FilterAudit::filter_triggers` field stays `serde_json::Value`; the bind in `crates/eros-engine-store/src/chat.rs` writes SQL NULL for `Value::Null` automatically.

### Migration

New migration `0022_filter_triggers_wipe_legacy_shape.sql`:

```sql
-- SPDX-License-Identifier: AGPL-3.0-only
-- Spec: docs/superpowers/specs/2026-05-29-engine-cleanup-and-snapshot-design.md §2
--
-- v0.5.1 ships a new shape for engine.chat_messages.filter_triggers. The
-- legacy shape (random as {p,draw}, models as single id, traits as observed
-- tag array) cannot be reconstructed back to source TOML because observed
-- values do not carry the `when` mode. Drop the legacy audit so readers
-- never see mixed shapes. The "filter ran" signal is preserved on the row
-- via filter_model NOT NULL; only the predicate detail is lost.
--
-- This runs at the v0.5.1 upgrade, before any new-shape row can exist, so
-- every non-null filter_triggers is legacy — wipe on that condition alone.

UPDATE engine.chat_messages
   SET filter_triggers = NULL
 WHERE filter_triggers IS NOT NULL;
```

### Documentation

- Update `docs/superpowers/specs/2026-05-26-tip-role-and-filter-audit-design.md` §4.2–4.4: replace the `TriggerHits` / shape block with a banner pointing at this spec's §2 ("Update (2026-05-29): the shape described here is superseded by `2026-05-29-engine-cleanup-and-snapshot-design.md` §2; legacy audit rows wiped by migration 0022.").
- `docs/prompt-traits.md` if it mentions the JSONB shape: update accordingly.

### Test plan

- `model_config.rs` tests: extend the existing `should_filter` coverage to assert the new return shape — pure `random` fire emits `{random: <p>}`, pure `models` fire emits `{models: [list]}`, pure `traits(present)` fire emits `{traits: {any, when}}`, pure `traits(absent)` fire emits the same `{traits: {any, when="absent"}}` (the "absent" pass still echoes the source predicate, not an empty array), combined fire emits multiple keys, empty-trigger fire returns `FiredPredicates::default()` whose `is_empty()` is true.
- `chat.rs` round-trip tests: update fixtures from `serde_json::json!({})` placeholder to representative new-shape payloads, including a `Value::Null` case asserting the bind writes SQL NULL.
- `pipeline/stream.rs` integration test (if present): a filtered-success turn with an empty trigger persists `filter_triggers IS NULL` and `filter_model IS NOT NULL` on the same row.

### Blast radius / risk

- Breaking shape change for any reader of `filter_triggers` JSONB. The user owns the only known reader. No public OpenAPI exposure (`filter_triggers` is operator-side audit, not returned in any DTO per `2026-05-26-tip-role-and-filter-audit-design.md` §6).
- Migration 0022 wipes legacy audit values. The "which predicates fired on this turn" detail is lost for rows written before 0022; the "filter ran at all" signal is preserved.

---

## §3 — `companion_insights_snapshot` + cron sweeper

### Motivation

`engine.companion_insights` stores one row per user (PK `user_id`) and `InsightRepo::merge` does a JSONB shallow-merge UPSERT. The row reflects only the latest merged state. The private downstream worker discussed in `eros-engine-web#181` (option 乙-1) needs a time-series record of how the JSONB and `training_level` evolved over time — for embedding-based dedupe, dreaming-full, drift analysis, and matching-profile maintenance.

Provide that egress as a pure write-through history table with a cron-scheduled sweeper. No LLM, no dedupe, no transformation. The downstream worker is responsible for whatever semantic processing it needs.

### Schema

New migration `0021_companion_insights_snapshot.sql` (number assumes this slice lands first per the recommended PR order in §"Release layout"):

```sql
-- SPDX-License-Identifier: AGPL-3.0-only
-- Spec: docs/superpowers/specs/2026-05-29-engine-cleanup-and-snapshot-design.md §3
--
-- Append-only history of engine.companion_insights. One row per user per
-- sweeper fire. captured_at is the fire instant (set by the sweeper), so
-- all rows from a single fire share a timestamp and group cleanly in
-- downstream time-series queries.

CREATE TABLE engine.companion_insights_snapshot (
    id              UUID             PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id         UUID             NOT NULL,
    insights        JSONB            NOT NULL,
    training_level  DOUBLE PRECISION NOT NULL,
    captured_at     TIMESTAMPTZ      NOT NULL    -- no DEFAULT; sweeper sets fire instant
);

CREATE INDEX idx_companion_insights_snapshot_user_time
    ON engine.companion_insights_snapshot (user_id, captured_at DESC);

-- Match the 0013 supabase_lockdown pattern: revoke role grants and enable RLS.
-- No policy → table is server-side only (sqlx-backed Rust caller).
REVOKE ALL ON engine.companion_insights_snapshot FROM anon, authenticated;
ALTER TABLE engine.companion_insights_snapshot ENABLE ROW LEVEL SECURITY;
```

Notes:
- `id` UUID PK: defensive against pathological double-fire (e.g. clock skew, scheduler glitch). `(user_id, captured_at)` is logically unique per fire but not enforced as a constraint.
- `captured_at` has no DEFAULT — the sweeper passes the fire-instant timestamp explicitly so every row in a fire shares the same value (timestamp alignment for downstream queries).

### Store layer

Add to `crates/eros-engine-store/src/insight.rs` (same file as `InsightRepo`, since this is a sibling operation on the same logical entity):

```rust
impl<'a> InsightRepo<'a> {
    /// Append one snapshot row per companion_insights record at the given
    /// instant. Single server-side INSERT … SELECT; no per-user roundtrip.
    /// Returns the number of rows written.
    pub async fn snapshot_all_users(
        &self,
        captured_at: chrono::DateTime<chrono::Utc>,
    ) -> sqlx::Result<usize> {
        let res = sqlx::query(
            "INSERT INTO engine.companion_insights_snapshot
                (user_id, insights, training_level, captured_at)
             SELECT user_id, insights, training_level, $1
               FROM engine.companion_insights",
        )
        .bind(captured_at)
        .execute(self.pool)
        .await?;
        Ok(res.rows_affected() as usize)
    }
}
```

### Pipeline module

New file `crates/eros-engine-server/src/pipeline/snapshot.rs`:

```rust
// SPDX-License-Identifier: AGPL-3.0-only
//! companion_insights_snapshot sweeper.
//!
//! On a cron schedule (default 23:00 SGT daily), inserts one row per
//! user with a companion_insights record into
//! engine.companion_insights_snapshot, preserving the JSONB and
//! training_level at that instant for downstream time-series consumers.
//! No LLM, no dedupe, no transformation.

use std::str::FromStr;

use chrono::Utc;
use cron::Schedule;

use eros_engine_store::insight::InsightRepo;

use crate::state::AppState;

pub async fn sweeper(state: AppState) {
    let cfg = &state.config.snapshot;
    if cfg.disabled {
        tracing::info!("snapshot sweeper disabled (SNAPSHOT_DISABLED=1)");
        return;
    }
    let schedule = match Schedule::from_str(&cfg.cron) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(cron = %cfg.cron, error = %e,
                "snapshot: invalid SNAPSHOT_CRON; sweeper disabled");
            return;
        }
    };
    tracing::info!(cron = %cfg.cron, tz = %cfg.tz, "snapshot sweeper starting");

    loop {
        let next = match schedule.upcoming(cfg.tz).next() {
            Some(n) => n.with_timezone(&Utc),
            None => {
                tracing::error!("snapshot cron yielded no upcoming fire; exiting");
                return;
            }
        };
        let delay = (next - Utc::now())
            .to_std()
            .unwrap_or(std::time::Duration::from_secs(1));
        tokio::time::sleep(delay).await;

        let fire_at = Utc::now();
        let repo = InsightRepo { pool: &state.pool };
        match repo.snapshot_all_users(fire_at).await {
            Ok(n) => tracing::info!(written = n, %fire_at, "snapshot: fire complete"),
            Err(e) => tracing::warn!(error = %e, "snapshot: fire failed; retrying next tick"),
        }
    }
}
```

`crates/eros-engine-server/src/pipeline/mod.rs`: add `pub mod snapshot;`.

### Configuration

Extend `AppState`'s config struct with a `SnapshotConfig`:

```rust
pub struct SnapshotConfig {
    pub disabled: bool,
    pub cron: String,                 // raw cron expression
    pub tz: chrono_tz::Tz,
}

impl SnapshotConfig {
    pub fn from_env() -> Self {
        let disabled = std::env::var("SNAPSHOT_DISABLED")
            .map(|v| v == "1")
            .unwrap_or(false);
        let cron = std::env::var("SNAPSHOT_CRON")
            .unwrap_or_else(|_| "0 0 23 * * *".to_string());  // every day 23:00
        let tz_raw = std::env::var("SNAPSHOT_TZ")
            .unwrap_or_else(|_| "Asia/Singapore".to_string());
        let tz = tz_raw.parse::<chrono_tz::Tz>().unwrap_or_else(|_| {
            tracing::warn!(tz = %tz_raw, "snapshot: invalid SNAPSHOT_TZ; using Asia/Singapore");
            chrono_tz::Asia::Singapore
        });
        Self { disabled, cron, tz }
    }
}
```

The cron crate's `Schedule::from_str` uses a 6-field format `sec min hr dom mon dow`. The default `"0 0 23 * * *"` means "second 0, minute 0, hour 23, every day".

### Cargo dependencies

Added to `crates/eros-engine-server/Cargo.toml`:
- `cron = "0.12"` (or latest stable; pure parser, no runtime)
- `chrono-tz = "0.10"` (or workspace-already-pinned version if present)

Verify whether `chrono-tz` is already in the dependency tree before adding; the workspace may already pull it via a transitive.

### Startup integration

`crates/eros-engine-server/src/main.rs`: spawn the sweeper alongside `dreaming::sweeper`:

```rust
tokio::spawn(pipeline::snapshot::sweeper(state.clone()));
```

### Non-goals (explicit)

- No picker-side dedupe. Every fire writes one row per `companion_insights` record, including JSONB that has not changed since the last snapshot. The downstream worker is responsible for semantic dedupe.
- No retention / TTL / cleaner. Operator-side concern.
- No admin endpoint to fire-on-demand. Downstream reads the table directly.
- No backfill from historical `companion_insights.updated_at` — observed merge timestamps don't carry the JSONB-at-that-time, so backfill would be lossy.

### Test plan

- Migration test: after 0021, `engine.companion_insights_snapshot` exists with the five columns at the expected types, the named index exists on `(user_id, captured_at DESC)`, and `relrowsecurity` is true.
- `InsightRepo::snapshot_all_users` `sqlx::test`: seed two users into `companion_insights` with distinct JSONB and training_level, call `snapshot_all_users(t)`, assert two rows present in the snapshot table with the same `captured_at = t` and matching content.
- Unit test on the cron schedule: `Schedule::from_str("0 0 23 * * *").upcoming(Asia/Singapore).next()` returns a timestamp whose `Asia/Singapore` projection is 23:00 of either today or tomorrow.
- Integration test for the sweeper loop is skipped — the loop body is a `sleep` on a real cron-derived instant. Time-warping in tests would require either `tokio::time::pause` (which the cron crate's reliance on `Utc::now()` does not respect) or refactoring the sweeper into a testable shape. Not worth it for a 30-line loop.

### Blast radius / risk

- New table only; no schema modifications to existing tables.
- A startup-time invalid `SNAPSHOT_CRON` disables the sweeper with an error log; chat path is unaffected.
- Default fire is 23:00 SGT (15:00 UTC), low-traffic for the operator's expected timezone. INSERT-SELECT is single-statement, server-side; even a 100k-user `companion_insights` is sub-second.

---

## Release layout

### PR sequence

Each slice ships as one independent PR, branched off `dev`, squashed back to `dev` per `feedback_branch_merge_types`.

| Order | Branch | Migration | Risk |
|---|---|---|---|
| 0 | `chore/open-dev-track-0.5.1` | none | trivial |
| 1 | `feat/companion-insights-snapshot` | 0021 | additive only |
| 2 | `feat/filter-triggers-config-as-declared` | 0022 | shape change + audit data wipe |
| 3 | `chore/drop-nft-ownership-stack` | 0023 | destructive (BREAKING) |

Rationale for ordering:
- PR 0 opens the dev track at `0.5.1-dev` before any feat PRs touch the version-pinned crates. Pattern matches `c4c0971`'s `chore(dev): open dev track at 0.4.3-dev (#48)` precedent.
- PR 1 is purely additive. Landing it first gives the downstream `eros-engine-web#181` worker the earliest start on consumption.
- PR 2 is non-destructive at the schema level (only JSONB content); landing before PR 3 keeps `dev`'s migration sequence monotonic.
- PR 3 is destructive and BREAKING; landing last gives the downstream the longest adaptation window before main promotion.

Migration filenames are assigned by merge order, not draft order — whoever lands first claims `0021`. The migration numbers used inside §1/§2/§3 above assume this recommended order; if the actual merge order differs, the numbers shift accordingly.

Each feat PR requires:
- Codex `/code-review` clean
- Local `cargo fmt`, `cargo clippy`, `cargo test`, `cargo run --bin gen-openapi` (per `feedback_rust_local_toolchain`)
- CI green
- User explicit `merge` approval per `feedback_dev_workflow` + `feedback_scope_vs_step`

### Main promotion

After all three feat PRs are on `dev`, a single `chore(release): v0.5.1` PR from `dev` to `main` carries:
- Workspace `version = "0.5.1"` (root `Cargo.toml`)
- `crates/eros-engine-server/src/openapi.rs` doesn't need a manual bump (it reads `env!("CARGO_PKG_VERSION")`).
- `README.md` / `README.zh.md` docker image tag references (if any reference `0.5.0` literally).
- Squash merge per `feedback_branch_merge_types`.

After main lands, a signed annotated tag `v0.5.1` (`git tag -s` per `feedback_git_sign`) is pushed to `main`, which triggers the GHCR build per `feedback_eros_engine_release`.

Then a `chore: back-merge main into dev` merge-commit (not squash) closes the release loop and reopens dev at `0.5.2-dev`, mirroring `45fd767`'s precedent.

### Release notes

A `RELEASES.md` entry (or equivalent) for v0.5.1 must carry top-line markers:

```
v0.5.1 (2026-XX-XX)

BREAKING:
- engine.wallet_links, engine.persona_ownership, engine.sync_cursors dropped.
- engine.persona_genomes.asset_id column dropped.
- /s2s/wallets/upsert, /s2s/wallets/since, /s2s/ownership/upsert,
  /s2s/ownership/since endpoints removed.
- NFT-ownership gate (enforce_nft_ownership) removed; chat-start, chat-message,
  and chat-stream no longer enforce wallet/asset ownership.
- engine.chat_messages.filter_triggers JSONB shape changed to
  predicate-config-as-declared; legacy rows wiped (filter_model retained
  as "filter ran" signal).

NEW:
- engine.companion_insights_snapshot table + cron-scheduled sweeper
  (SNAPSHOT_CRON / SNAPSHOT_TZ / SNAPSHOT_DISABLED env vars).
```

Per `feedback_dev_workflow`, the actual tag push and the timing of the release PR are explicit user decisions, not autonomous steps.

---

## Cross-cutting non-goals

- No reuse / refactoring beyond what each slice strictly requires. Per `CLAUDE.md`'s "no half-finished implementations" and "no backwards-compatibility hacks" guidance.
- No deploy concerns (`feedback_oss_scope_not_private_prod`). The OSS engine ships code; whoever runs it decides cron defaults, retention, monitoring.
- No telemetry / analytics on snapshot writes — backend stays chat-only per `feedback_engine_chat_only_scope`.
- No seed-personas changes (`feedback_seed_personas_boundary`); none of these slices touch persona seeding.

## Open items

None blocking. Filename and migration number assignments are tentative on PR merge order — the spec assumes the recommended order in §"Release layout".
