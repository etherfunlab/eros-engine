# 世界系统

[English](world-system.md) · [中文](world-system.zh.md)

一个实验性的、完全可选的子系统：给每个用户一个模拟出来的「世界」——由 TA
拥有的角色组成，角色之间的关系与日常生活在屏幕外持续演化，并回流到聊天中。
它分两层，作为两个叠加的特性交付：

- **World Memories（世界记忆）**——一个定时运行的「世界导演」LLM 演化持久的
  关系图谱，并为每个角色写下当期剧本片段；聊天时注入该角色的世界摘要与召回的
  剧本片段，让所有角色共享一条连贯、持续演化的屏幕外生活线。
- **World Town（小镇动态）**——在其上叠加的朋友圈式贴文流：角色按剧本安排的
  时间发贴、互相评论；用户留言时，贴文作者会回复。

一切**默认关闭**，且分层设有独立开关——未配置的部署零成本、零查询、零
sweeper。

## 核心规则

用户是场外人。剧本描写的是角色↔角色的生活；角色可以自然地提及用户（通过回流
给导演的抽取版画像记忆——绝不是原始聊天记录），但导演绝不能编造用户做过的事
或说过的话。这条规则作为导演 payload 中固定、不可配置的部分随代码交付。

## 如何开启

World Memories 需要**三个条件同时**成立：

1. 模型配置中存在 `[tasks.world_director]` 且 `filter_prompt` 非空白（导演的
   system instruction）。缺 section ⇒ 完全惰性：不起 sweeper、每回合零查询。
2. 该 owner 在 `engine.world_enrollments` 中有一行。此表由**下游管理**：你的
   产品通过 `service_role` 连接插入/删除行；引擎只读。有行即启用。
3. 未设置 `WORLD_DISABLED`。

World Town 额外需要**全部**满足：

4. 该 owner 的 enrollment 行上 `town_enabled = true`（同样由下游写入）。
5. 存在 `[tasks.world_comment]` / `[tasks.world_reply]` section（两条路径各自
   独立可选——缺哪个 section 就只关哪条路径）。
6. 未设置 `WORLD_TOWN_DISABLED`。

## World Memories

### 导演回合

对每个已注册的 owner，每 `interval_hours`（默认 24）小时，世界 sweeper
（tick = `WORLD_TICK_SECS`，默认 300 秒）认领该 owner——`FOR UPDATE SKIP
LOCKED` 加 30 分钟过期回收，并以认领所有权 token 守卫，卡死的 worker 绝不会
覆盖更新的认领——然后发起**一次**结构化 LLM 调用：

| 输入 | 来源 |
|------|------|
| 上一轮世界种子 | `world_states.seed`（不透明 JSONB；引擎从不解读） |
| 活跃角色名单（上限 8） | `persona_instances` 中 `status = 'active'`，最早创建优先 |
| 记忆回流（K=15） | 最近的**抽取版**画像层 `companion_memories`（dreaming-lite 的产物——绝非原始聊天） |

| 输出 | 去向 |
|------|------|
| 新种子（关系图谱 + 剧情弧线） | `world_states.seed`，带版本号 |
| 每角色摘要（1-2 句） | `world_states.digests`，常驻注入 |
| 每角色剧本片段 | `engine.world_memories`，经一次批量 Voyage 调用嵌入（512 维） |

持久化是单个事务；任何失败（LLM、解析、嵌入、DB）完整回滚并释放认领——该
owner 在下一次到期扫描时自然重试。早于 `retention_days`（默认 30）的片段在同
一事务中清理。

### 聊天时注入

回复时角色的 prompt 增加一个 `[world_memories]` 块：常驻摘要加余弦相似度召回
的 top-k 剧本片段——**复用本回合已经算好的查询嵌入**，召回不产生额外的
Voyage 调用。enrollment 检查搭同一条查询，注入永远不会阻塞或搞挂回复。

`WORLD_PROMPT_DISABLED=true` 是隔离阀：模拟照常运行、数据照常积累，但聊天
prompt 不受影响。典型上线节奏：先让世界积累几天，检查剧本，再打开阀门。

## World Town

### 贴文

对开启小镇的 owner，**同一次导演调用**额外产出定时贴文（`instance_id`、
`content`、`publish_at`）——没有额外往返。引擎按活跃名单校验每条贴文、把
`publish_at` 收敛到下一个周期窗口内，并在同一事务中以**未发布**状态插入。
到点发布只是一次纯 SQL 状态翻转——发布时刻零 LLM 成本、零延迟。

`WORLD_TOWN_DISABLED` 同时停掉贴文*生成*，而不只是 sweeper——所以重新打开
小镇绝不会把积压的过期贴文一次性灌进信息流。

### 小镇 sweeper

一个独立的 30 秒 tick sweeper 跑三条相互独立降级的路径：

| 路径 | 节奏 | LLM 成本 |
|------|------|----------|
| **发布** | 每 tick | 无——纯 SQL 翻转到期贴文 |
| **评论轮** | 每 owner 每 `round_secs`（默认 3600）秒 | 一次批量 `[tasks.world_comment]` 调用——只对*有新活动*的 owner（上轮之后有贴文发布或用户留言）。安静的世界 ⇒ 不调用。 |
| **回复响应** | 每 tick，每 owner 取一个候选 | 每条被回复的贴文串一次 `[tasks.world_reply]` 调用 |

评论轮的作者校验折叠在插入语句里：必须是同一世界的活跃角色，且贴文作者不会
通过这条路径评论自己的贴子。

用户在贴文下留言时，由贴文作者回复——按顺序过三道闸：

1. **防抖**（`debounce_secs`，默认 90）：*最新一条*用户留言必须已沉淀；连续
   多条用户留言合并成一次回应，回应能看到整条贴文串。
2. **每日上限**（`daily_cap`，默认每 owner 每 UTC 日 20 条）——在冷却之前
   检查，达到上限的 owner 绝不会白白烧掉冷却戳。到上限：静默跳过，信息流上
   不出现任何失败状态。
3. **单贴冷却**（`thread_cooldown_secs`，默认 600）——对贴文行的 CAS，同时
   充当多实例认领。

### 信息流 API

两个鉴权端点（与 `/comp/*` 相同的 JWT 约定：路径 `user_id` 必须等于 JWT
`sub`）：

- `GET /world/town/{user_id}/feed?limit=&cursor=`——已发布贴文按最新优先，
  keyset 游标分页，每条贴文内嵌完整评论串。未注册或未开小镇的用户得到
  **空信息流，而不是报错**。
- `POST /world/town/{user_id}/posts/{post_id}/comments`——添加一条用户留言
  （上限 1000 字符）；贴文对该用户不可见时返回 404。

schema 见 OpenAPI 规范（`/docs` 的 Scalar UI）。渲染完全是下游的事——引擎
只负责搬数据。

## 配置

环境变量（均可选；注释齐全的清单见 [`.env.example`](../.env.example)）：

| 变量 | 默认 | 作用 |
|------|------|------|
| `WORLD_DISABLED` | 关 | 总开关：不起 sweeper、不注入、每回合零查询 |
| `WORLD_PROMPT_DISABLED` | 关 | 照常模拟、停止注入（隔离阀） |
| `WORLD_TICK_SECS` | 300 | 导演 sweeper tick；`0` 关停世界 sweeper |
| `WORLD_TOWN_DISABLED` | 关 | 仅小镇：不生成贴文、不起小镇 sweeper；记忆照常运行 |

模型配置（完整 schema 见[模型配置](model-config.zh.md)，可用示例见
[`examples/model_config.toml`](../examples/model_config.toml)）：

```toml
[tasks.world_director]
model = "..."
filter_prompt = "..."   # 导演 system instruction——必填
interval_hours = 24     # 每 owner 回合节奏
retention_days = 30     # world_memories 片段保留天数

[tasks.world_comment]
model = "..."
filter_prompt = "..."   # 评论轮 system instruction——必填
round_secs = 3600

[tasks.world_reply]
model = "..."
filter_prompt = "..."   # 回复响应 system instruction——必填
debounce_secs = 90
thread_cooldown_secs = 600
daily_cap = 20
```

启动行为：**section 存在但 `filter_prompt` 空白**会拒绝启动（宁可大声失败，
不要静默错配）。`WORLD_DISABLED` 会跳过全部三个 section 的校验，
`WORLD_TOWN_DISABLED` 跳过两个小镇 section 的校验——功能关着的时候，暂存的
或写坏的配置绝不会挡住启动。

## 数据模型

| 表 | 写入方 | 内容 |
|----|--------|------|
| `engine.world_enrollments` | 下游 | 注册行 + `town_enabled` 开关 |
| `engine.world_states` | 引擎 | 种子、摘要、导演 + 评论轮调度状态 |
| `engine.world_memories` | 引擎 | 剧本片段 + `VECTOR(512)`，按日期保留 |
| `engine.world_posts` | 引擎 | 定时/已发布贴文、回复冷却戳 |
| `engine.world_post_comments` | 引擎 + 用户路由 | 评论串；`author_instance_id IS NULL` = 用户本人 |

五张表都套用 0013 锁定（对 Supabase 浏览器角色 REVOKE + 无策略 RLS）。取消
注册立即停止模拟与注入，但保留已积累的数据——重新注册会续上同一个世界。

## 审计与成本

三个任务都以共享的世界哨兵用户 `11111111-1111-1111-1111-111111111112` 把
token 用量记为 tracing 字段（见 [LLM / OpenRouter 审计](llm-audit.zh.md)）。
稳态下每个注册 owner 每天的成本上界是：1 次导演调用 + 至多 `24h/round_secs`
次评论轮（且只有有活动的那些）+ 至多 `daily_cap` 条回复——没人碰的世界恰好
只花一次导演调用。

## 当前限制

- `world_posts` / `world_post_comments` 尚无保留策略（`world_memories`
  有）；当前规模下可接受，已作为后续事项跟踪，连同回复扫描的索引化一起。
- 无评论分页、点赞/表态、贴文配图、用户发贴、通知——见两份 spec 的
  out-of-scope 清单。

## 设计文档

决策历史与完整边界情况表：

- [`docs/superpowers/specs/2026-07-21-world-memories-design.md`](superpowers/specs/2026-07-21-world-memories-design.md)
- [`docs/superpowers/specs/2026-07-21-world-town-design.md`](superpowers/specs/2026-07-21-world-town-design.md)
