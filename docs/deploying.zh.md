# 部署

[English](deploying.md) · [中文](deploying.zh.md)

三條支持的路徑，按工作量排：

1. **Fly.io**——倉庫裏的 `fly.toml` 直接配好了。如果之前用過 Fly，大約 10 分鐘搞定。
2. **Docker compose 自托管**——單機 VPS，自帶 Postgres+pgvector。
3. **作為庫嵌入**——`core + llm + store` 進你自己的服務，不要 HTTP 層。

## 三種方式都需要的前置

- Postgres 16+，裝了 `pgvector` extension（≥ 0.7）。
- 一個 OpenRouter 賬號（`OPENROUTER_API_KEY`）。
- 一個 Voyage AI 賬號（`VOYAGE_API_KEY`）。
- 要麼 Supabase 項目（默認 JWT auth 用），要麼你自己的 JWT 簽發者（實現 `AuthValidator`）。

## 路徑 1：Fly.io

倉庫根目錄的 `fly.toml` 是一份可以直接用的生產配置。app 名 `eros-engine`、region `nrt`、`shared-cpu-1x` 512MB、scale-to-zero。自定義域名走 Fly certs。部署前把 app 名換成自己的、region 挑一個合適的。

```bash
# 1. 建 app
flyctl apps create eros-engine --org personal

# 2. 寫入密鑰（換成你的真實值）
flyctl secrets set --app eros-engine \
  DATABASE_URL='postgres://…@…supabase.co:5432/postgres' \
  OPENROUTER_API_KEY='sk-or-…' \
  VOYAGE_API_KEY='pa-…' \
  SUPABASE_URL='https://…supabase.co' \
  SUPABASE_JWT_SECRET='…' \
  SUPABASE_SERVICE_ROLE_KEY='eyJ…'

# 或者從 .env 文件導入：
#   grep -E '^(DATABASE_URL|OPENROUTER_API_KEY|VOYAGE_API_KEY|SUPABASE_URL|SUPABASE_JWT_SECRET|SUPABASE_SERVICE_ROLE_KEY)=' .env \
#     | flyctl secrets import -a eros-engine

# 3. 首次部署（Fly 的遠端構建器；空緩存約 5-10 分鐘）
flyctl deploy --remote-only -a eros-engine

# 4. 自定義域名
flyctl certs create your-domain.example.com -a eros-engine
# 把 flyctl 打印的 A + AAAA 記錄加到你的 DNS 提供商。
flyctl certs check your-domain.example.com -a eros-engine

# 5. （可選）Seed 人格
flyctl ssh console -a eros-engine \
  -C "/usr/local/bin/eros-engine seed-personas /etc/eros-engine/personas"
```

`fly.toml` 裡的 `release_command = "migrate"` 會在每次 deploy 切流量之前自動跑 sqlx migrations——你不需要手動跑遷移。

### 子命令

二進制文件有三種模式（按 `argv[1]` 分派）：

| 子命令 | 用途 |
|---|---|
| `serve`（默認） | 在 `BIND_ADDR` 上跑 HTTP 服務器 |
| `migrate` | 應用待處理的 sqlx migrations 然後退出 |
| `seed-personas <dir>` | 讀 `<dir>` 裡每個 `*.toml` 文件，upsert 為人格基因 |

`seed-personas` 是冪等的——再跑會 update 原有行（按 `name` 匹配），保持 UUID 跟 `persona_instances` 裡的 FK 引用穩定。

## 路徑 2：Docker compose 自托管

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

前面放個真正的 Caddy / Traefik / Cloudflare 做 HTTPS 終止。

## 路徑 3：作為庫嵌入

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

- **健康探針：** `GET /healthz` 返 200，響應 `{ status: "ok", service, version, timestamp }`。把這個接到平台的健康檢查上。
- **OpenAPI / Scalar：** `GET /docs` 提供實時的 Scalar 參考。OpenAPI JSON 在 `/api-docs/openapi.json`。
- **Affinity debug：** `GET /comp/affinity/{session_id}` 受 `EXPOSE_AFFINITY_DEBUG=true` 控制。生產部署一般關掉；如果你的前端要實時畫好感度雷達圖，再打開。
- **日誌：** `RUST_LOG=info` 是默認。`RUST_LOG=debug,sqlx=warn` 看到除 SQLx 查詢噪音以外的一切。
- **成本：** OSS 部署默認 chat 用 grok-4-fast（便宜）、insight 抽取用 grok-4-mini。一輪典型對話花費 ≪ $0.001 美元 token 成本，加上一個 Voyage embedding 調用（每個值得記住的事實大約 $0.000003）。10000 輪對話花個位數美元。

## 源碼

- `fly.toml`——可直接套用的 Fly.io 生產配置
- `docker/Dockerfile`——多階段構建（Rust 1.88 構建器 → debian:bookworm-slim 運行時）
- `docker/docker-compose.yml`——自托管 stack
- `crates/eros-engine-server/src/main.rs`——三個子命令
