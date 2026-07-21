# 部署

[English](deploying.md) · [中文](deploying.zh.md)

兩條支持的路徑，按工作量排：

1. **Docker compose 自托管**——單機 VPS，自帶 Postgres+pgvector。
2. **作為庫嵌入**——`core + llm + store` 進你自己的服務，不要 HTTP 層。

## 兩種方式都需要的前置

- Postgres 16+，裝了 `pgvector` extension（≥ 0.7）。
- 一個 OpenRouter 賬號（`OPENROUTER_API_KEY`）。
- 一個 Voyage AI 賬號（`VOYAGE_API_KEY`）。
- 要麼 Supabase 項目（默認 JWT auth 用），要麼你自己的 JWT 簽發者（實現 `AuthValidator`）。

## 子命令

二進制文件有五種模式（按 `argv[1]` 分派）：

| 子命令 | 用途 |
|---|---|
| `serve`（默認） | 在 `BIND_ADDR` 上跑 HTTP 服務器 |
| `migrate` | 應用待處理的 sqlx migrations 然後退出 |
| `seed-personas <dir>` | 讀 `<dir>` 裡每個 `*.toml` 文件，upsert 為人格基因 |
| `backfill-human-insights` | 一次性把每行 `companion_insights` 投影进 `engine.human_insights`（幂等；仅手动执行） |
| `print-openapi` | 把 OpenAPI 规范打到 stdout 后退出（不连 DB、不读 env；CI 漂移检查用） |

`seed-personas` 是冪等的——再跑會 update 原有行（按 `name` 匹配），保持 UUID 跟 `persona_instances` 裡的 FK 引用穩定。

## 路徑 1：Docker compose 自托管

單機 VPS 部署，把 Postgres+pgvector 跟引擎一起放進同一個 compose stack：

```yaml
# docker/docker-compose.yml（草圖——按需要調端口、捲、env）
services:
  postgres:
    image: pgvector/pgvector:pg16
    environment:
      POSTGRES_PASSWORD: postgres
      POSTGRES_DB: eros_engine
    ports: ["5432:5432"]
    volumes:
      - eros_pg:/var/lib/postgresql/data

  engine:
    build:
      context: ..
      dockerfile: docker/Dockerfile
    depends_on: [postgres]
    environment:
      DATABASE_URL: postgres://postgres:postgres@postgres:5432/eros_engine
      OPENROUTER_API_KEY: ${OPENROUTER_API_KEY}
      VOYAGE_API_KEY: ${VOYAGE_API_KEY}
      SUPABASE_JWT_SECRET: ${SUPABASE_JWT_SECRET}
      EXPOSE_AFFINITY_DEBUG: "true"
    ports: ["8080:8080"]

volumes:
  eros_pg:
```

`docker compose -f docker/docker-compose.yml up` 跑起來。第一次 boot 會走 `migrate` 子命令入口跑遷移；之後重啟跳過已應用的遷移。

这份 compose 只接了旧版的 `SUPABASE_JWT_SECRET`，所以 `up` 之前要先导出一个非空值。空或未设置、又没有 JWKS 来源时，引擎会拒绝启动——这是刻意设计，让配错的部署直接报错，而不是默默拒掉每一个请求。若想改用非对称 JWKS 校验，在 `environment:` 块里加上 `SUPABASE_URL`（或 `SUPABASE_JWKS_URL`）。

前面放個真正的 Caddy / Traefik / Cloudflare 做 HTTPS 終止。

## 路徑 2：作為庫嵌入

如果你不需要 HTTP 層——比如你在這個基礎上搞另一個產品——直接跳過 `eros-engine-server`：

```toml
[dependencies]
eros-engine-core  = { git = "https://github.com/etherfunlab/eros-engine", branch = "main" }
eros-engine-llm   = { git = "https://github.com/etherfunlab/eros-engine", branch = "main" }
eros-engine-store = { git = "https://github.com/etherfunlab/eros-engine", branch = "main" }
```

然後構造 pool、倉儲、LLM 客戶端，寫你自己的分派層：

```rust
let pool = eros_engine_store::pool::build(&database_url).await?;
let openrouter = eros_engine_llm::openrouter::OpenRouterClient::new(or_key);
let voyage = eros_engine_llm::voyage::VoyageClient::new(voyage_key);

let affinity_repo = eros_engine_store::affinity::AffinityRepo { pool: &pool };
let mut affinity = affinity_repo
    .load_or_create(session_id, user_id, instance_id)
    .await?;

let signals = eros_engine_core::ghost::GhostSignals { … };
match eros_engine_core::ghost::decide(&affinity, signals) {
    GhostDecision::Reply  => { /* 跑 chat */ }
    GhostDecision::Ghost => { /* 保持沉默 */ }
}
```

遷移文件 `crates/eros-engine-store/migrations/` 隨 crate 發佈；用 `sqlx::migrate!()` 對你的 pool 跑就行，方式跟 `eros-engine-server` 一樣。

## 自帶 Auth

默認 JWT 驗證器是 Supabase HS256。換別的 IdP 就實現這個 trait：

```rust
use async_trait::async_trait;
use eros_engine_server::auth::{AuthError, AuthValidator};
use uuid::Uuid;

pub struct MyValidator { /* … */ }

#[async_trait]
impl AuthValidator for MyValidator {
    async fn validate(&self, bearer: &str) -> Result<Uuid, AuthError> {
        // 在這裡驗你的 token，返回 user_id
    }
}
```

然後把你的實例注入 `AppState.auth: Arc<dyn AuthValidator>`。中間件（`auth::middleware::require_auth`）對你提供的任何驗證器都通用。

## 自帶 Postgres

任何 sqlx Postgres 驅動兼容的都能用——Supabase、Neon、RDS、Crunchy Bridge、純自托管都行。硬要求：裝了 pgvector extension（`CREATE EXTENSION vector;`）。引擎自己建 schema（遷移 `0000_schema.sql` 裡的 `CREATE SCHEMA IF NOT EXISTS engine;`），跟數據庫裡其他東西可以乾淨共存。

如果跟另一個服務共用一個數據庫，引擎的表都在 `engine.*` 下、永不寫 `public.*`——零衝突。

### Supabase 部署——schema 暴露地雷

如果你的 Postgres 是 Supabase，**而且**把 `engine` 加進了項目的 Exposed Schemas 列表（Studio → Settings → API → Exposed schemas）——通常是為了讓同部署的 web 端能用 `@supabase/supabase-js` 讀 `engine.*`——那你可能同時把每張 `engine.*` 表都暴露給了可公開的 `anon` key，取決於 Studio Permissions 面板給了哪些角色什麼授權。

風險：拿到 publishable anon key 的人（這個 key 按設計就會出現在每個瀏覽器 bundle 裡）只要：

```bash
curl "https://<project>.supabase.co/rest/v1/chat_messages?select=*&limit=5" \
  -H "apikey: <publishable-anon-key>"
```

就能讀所有用戶的聊天記錄——如果 `anon` 曾經被授權對 `engine.chat_messages` 的 SELECT 的話。

遷移 `0013_supabase_lockdown.sql`（eros-engine 0.2+ 起內建）通過三步堵這個洞：

1. 對每張 `engine.*` 表執行 `REVOKE ALL FROM anon, authenticated`
2. 對 schema 本身執行 `REVOKE USAGE ON SCHEMA engine FROM anon, authenticated`
3. 對每張 `engine.*` 表執行 `ENABLE ROW LEVEL SECURITY`（無策略——縱深防禦；`postgres` 用戶和 `service_role` 都繞過 RLS，所以引擎本體和任何服務端的 Supabase client 都不受影響）

遷移外面包了 `IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon')`，所以非 Supabase 的 Postgres 部署（Neon、RDS、自托管）會靜默跳過 REVOKE，只繼承無害的 RLS enable。

**如果你是從 0.2 之前的版本升上來、又跑在 Supabase 上，跑一次 `eros-engine migrate` 應用它就行——這個遷移是冪等的。**

要獨立審計你的項目（與本遷移無關），以 `postgres` 角色執行：

```sql
-- engine.* 裡哪些表沒開 RLS？
SELECT relname FROM pg_class
 WHERE relnamespace = 'engine'::regnamespace
   AND relkind = 'r' AND NOT relrowsecurity;

-- engine.* 裡哪些表給 anon / authenticated 開了權限？
SELECT grantee, table_name, privilege_type
  FROM information_schema.role_table_grants
 WHERE table_schema = 'engine'
   AND grantee IN ('anon', 'authenticated');
```

應用遷移後，兩個查詢都應返回零行。

## 運維注意事項

### Prompt 日志（调试用，可选）

设置 `PROMPT_LOG_DIR` 后，引擎会把每一轮**主回复**组装好的完整 prompt 写成一个
可读文件（头部元数据 + 按 role 分块）。默认**关闭**，**仅供运营调试**（文件含原始
聊天内容），写入为后台 fire-and-forget，绝不阻塞或拖垮回复。把它指向你自己挂载的卷：

```yaml
# docker-compose: 挂载卷并设置 env
services:
  engine:
    environment:
      PROMPT_LOG_DIR: /data/prompt-logs
    volumes:
      - ./prompt-logs:/data/prompt-logs
```

```toml
# fly.io: 声明卷和 env（示例）
[mounts]
source = "prompt_logs"
destination = "/data/prompt-logs"

[env]
PROMPT_LOG_DIR = "/data/prompt-logs"
```

引擎不内置轮转或保留策略——卷由你自行管理。

### 世界系统（实验特性，可选）

[世界系统](world-system.zh.md)（World Memories 模拟 + World Town 动态）默认
完全关闭：模型配置里没有 `[tasks.world_director]` section 时，不会起任何
sweeper，每回合零查询。开启它是配置 + 数据层面的决定，不需要改部署：

1. 在模型配置中加入 `[tasks.world_*]` section（见
   [`examples/model_config.toml`](../examples/model_config.toml)）。
2. 通过 `service_role` / owner 连接往 `engine.world_enrollments` 插行来注册
   owner（引擎只读这张表）；对需要动态流的 owner 把 `town_enabled` 设为
   `true`。

运维开关，均可选：

| 变量 | 作用 |
|------|------|
| `WORLD_DISABLED=true` | 总关：不起 sweeper、不注入 prompt、零成本 |
| `WORLD_PROMPT_DISABLED=true` | 照常模拟积累，但不动聊天 prompt（灰度阀门） |
| `WORLD_TICK_SECS` | 导演 sweeper tick（默认 300；`0` 关停） |
| `WORLD_TOWN_DISABLED=true` | 仅小镇：不生成贴文、不起小镇 sweeper；记忆照常运行 |

成本形状：每个注册 owner 每 `interval_hours` 一次导演调用，外加（仅小镇）按
活动触发的每小时评论轮和按 owner 限额的回复。没人互动的世界恰好只花导演那一
次调用。细节、数据模型与启动校验规则见[世界系统](world-system.zh.md)。

- **健康探針：** `GET /healthz` 返 200，響應 `{ status: "ok", service, version, timestamp }`。把這個接到平台的健康檢查上。
- **OpenAPI / Scalar：** `GET /docs` 提供實時的 Scalar 參考。OpenAPI JSON 在 `/api-docs/openapi.json`。
- **Affinity debug：** `GET /comp/affinity/{session_id}` 受 `EXPOSE_AFFINITY_DEBUG=true` 控制。生產部署一般關掉；如果你的前端要實時畫好感度雷達圖，再打開。
- **日誌：** `RUST_LOG=info` 是默認。`RUST_LOG=debug,sqlx=warn` 看到除 SQLx 查詢噪音以外的一切。
- **成本：** OSS 部署默认 chat 使用一个快速廉价的模型、insight 抽取使用一个高质量抽取模型（当前默认值见 `examples/model_config.toml`）。一轮典型对话花费 ≪ $0.001 美元 token 成本，加上一个 Voyage embedding 调用（每个值得记住的事实约 $0.000003）。10000 轮对话花个位数美元。

## 源碼

- `docker/Dockerfile`——多階段構建（Rust 1.88 構建器 → debian:bookworm-slim 運行時）
- `docker/docker-compose.yml`——自托管 stack
- `crates/eros-engine-server/src/main.rs`——三個子命令
