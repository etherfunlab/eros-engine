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
- **World Stories（人生故事，v2，可选的第三层）**——每个角色拥有一段私有的、
  持续演化的人生（工作 / 感情——包括与用户的关系 / 日常生活），按
  `persona_instance` 每 8 小时模拟一轮，前置一道活跃度闸门。Stories 会喂给
  World Memories 导演（让剧本与每个角色自己的人生保持一致），并默认注入聊天
  prompt。

一切**默认关闭**，且分层设有独立开关——未配置的部署零成本、零查询、零
sweeper。

## 核心规则

用户是场外人。剧本描写的是角色↔角色的生活；角色可以自然地提及用户（通过回流
给导演的抽取版画像记忆——绝不是原始聊天记录），但导演绝不能编造用户做过的事
或说过的话。这条规则作为导演 payload 中固定、不可配置的部分随代码交付。

开启 World Stories 后，规则按层拆分：在 **stories 层**，用户是场上人——感情线
的推进要包含用户——但导演只能从聊天记录里*读取*用户的言行，绝不能编造；关系
定性以聊天记录为准（亲密度数值仅供参考）。World Memories 层维持原规则不变：
用户相关的事实只通过 story 事件间接回流。

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

用户在贴文下留言时，由贴文作者回复——按顺序过四道闸：

1. **活跃窗口**（`reply_window_secs`，默认 604800 / 7 天）：贴文的*最新一条*
   用户留言必须落在这个窗口内。新的用户留言会给贴文重新盖戳
   （`world_posts.last_user_comment_at`），所以哪怕是几个月前的旧贴，有人一
   留言就重新进入扫描；一直没人理的贴文串则自然落出窗口。这把扫描成本限定在
   近期活跃的贴文串上——是索引驱动的成本边界，不改变行为。
2. **防抖**（`debounce_secs`，默认 90）：*最新一条*用户留言必须已沉淀；连续
   多条用户留言合并成一次回应，回应能看到整条贴文串。
3. **每日上限**（`daily_cap`，默认每 owner 每 UTC 日 20 条）——在冷却之前
   检查，达到上限的 owner 绝不会白白烧掉冷却戳。到上限：静默跳过，信息流上
   不出现任何失败状态。
4. **单贴冷却**（`thread_cooldown_secs`，默认 600）——对贴文行的 CAS，同时
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

## World Stories（人生故事）

Stories 需要**全部条件**同时成立——且按构造骑在 World Memories 之上：

1. World Memories 本身已启用（`[tasks.world_director]` 已配置）——story 扫描
   是同一次世界 sweeper tick 的第二阶段，紧跟在 WM 导演扫描之后；若
   `world_director` 未配置，sweeper 根本不会启动，stories 也就不会跑。
2. 模型配置中存在 `[tasks.world_stories_director]` 且 `filter_prompt` 非空白。
3. 该 owner 的 `world_enrollments` 行上 `stories_enabled = true`（下游写入，
   与 `town_enabled` 同一套注册名单机制）。
4. 未设置 `WORLD_STORIES_DISABLED`。

### 故事回合

对每个 `persona_instance`，每 `interval_hours`（默认 8 小时）跑一轮，前置一道
**活跃度闸门**：只有在 `active_window_hours`（默认 72 小时）内被聊过的角色才
会推进。安静的角色人生会暂停，一旦聊天重新活跃就自动续上——不做补跑。

| 输入 | 来源 |
|------|------|
| 当前 UTC 时间 | 引擎——为相对时间表述提供锚点 |
| 角色 | 经由 instance 取 `persona_genomes`：名字、人格、backstory（canon） |
| 当前 insight + digest | `persona_story_insights` 行（`last_run_at IS NULL` ⇒ 首轮初始化） |
| 近期事件（12 条） | `persona_story_events`，按时间顺序——保连贯性、防重复 |
| 亲密度快照 | 最新会话的六轴 + bond + chemistry + relationship_label——仅供参考 |
| 聊天证据 | 该 (owner, instance) 最近 `context_days`（默认 7 天）内的消息，按轮数上限截断 |

| 输出 | 去向 |
|------|------|
| 全量替换的 insight（固定扁平 schema） | `persona_story_insights` 各栏位列，`insight_version` 递增 |
| digest（1-2 句） | `persona_story_insights.digest`，常驻注入 |
| events（category + content，每轮上限 6 条） | `persona_story_events`，并在同一回合/事务内原样嵌入 `persona_story_memories`（1:1） |

insight 字段列表是 `companion_insights` 的**固定扁平超集**（提前应用了
`human_insights` 的教训——从第一天起就是扁平类型列，没有不透明 JSONB 的阶段）：
既有的每个 companion 字段（改写为描述角色本身），再加四个 story 专属栏位——
`work_history`（工作经历）、`romance_history`（感情史）、`family_of_origin`
（与原生家庭的关系）、`user_relationship`（与用户的关系状态）。列表以引擎常量
的形式交付；运营方的 `filter_prompt` 只能控制各栏位内容的*丰富程度*，不能改
动字段列表本身。

持久化与 World Memories 相同：单个事务，`persona_story_events` 与
`persona_story_memories` 都按 `retention_days`（默认 30）清理；任何失败
（LLM、解析、嵌入、DB）都会释放认领，等下一次到期扫描重试。

### 聊天时注入

`[world_stories]` 块紧跟在 `[world_memories]` 之后注入：常驻 digest 加至多
3 条余弦相似度召回的片段——**复用本回合已经算好的查询嵌入**，不产生额外
Voyage 调用。该层一旦启用，注入**默认开启**；`WORLD_STORIES_PROMPT_DISABLED=true`
是隔离阀（与 `WORLD_PROMPT_DISABLED` 同一模式）：照常模拟人生，只是不再注入。

### 反哺 World Memories

当某个 owner 处于 stories-active 状态时，WM 导演每个角色的 payload 条目会
额外带上 `recent_life`——该 instance 自 WM 上次运行以来的 story 事件——并加
一条固定规则，要求 WM 剧本必须与之保持一致。没开 stories 的 owner，WM 行为
与今天完全字节一致。数据流严格单向：

```
World Stories（角色个人生活）  →  World Memories（角色↔角色关系图谱）  →  World Town（舞台）
```

story 回合从不回读 WM 种子或 `world_memories`。

### 相对时间表述约定

经历类 insight 栏位（`work_history`、`romance_history`、`family_of_origin`）
用**相对时间表述**（n年前/n个月前/n天前）和人生阶段（x岁时/上大学时）记录，
不用绝对日期。引擎在每一轮 payload 里都会传入当前 UTC 时间，让导演在每次全量
重写时刷新这些相对表述——手动改 prompt 或直接读存储行时要留意这条约定。

## 配置

环境变量（均可选；注释齐全的清单见 [`.env.example`](../.env.example)）：

| 变量 | 默认 | 作用 |
|------|------|------|
| `WORLD_DISABLED` | 关 | 总开关：不起 sweeper、不注入、每回合零查询 |
| `WORLD_PROMPT_DISABLED` | 关 | 照常模拟、停止注入（隔离阀） |
| `WORLD_TICK_SECS` | 300 | 导演 sweeper tick；`0` 关停世界 sweeper |
| `WORLD_TOWN_DISABLED` | 关 | 仅小镇：不生成贴文、不起小镇 sweeper；记忆照常运行 |
| `WORLD_STORIES_DISABLED` | 关 | 仅 stories：不跑故事回合、不注入 `[world_stories]`；记忆照常运行 |
| `WORLD_STORIES_PROMPT_DISABLED` | 关 | 照常模拟人生、停止注入（隔离阀） |

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
reply_window_secs = 604800    # 用户留言后的回复可响应窗口（7 天）

[tasks.world_stories_director]
model = "..."
filter_prompt = "..."       # 导演 system instruction——必填
interval_hours = 8          # 每 instance 回合节奏
retention_days = 30         # events + memories 保留天数
active_window_hours = 72    # 活跃度闸门：该窗口内聊过 ⇒ 人生才推进
context_days = 7            # 每轮喂给导演的聊天/亲密度证据窗口
# 部署者注意：经历类 insight 栏位用相对时间（n年前/上大学时）——见
# docs/superpowers/specs/2026-07-23-world-stories-design.md
```

启动行为：**section 存在但 `filter_prompt` 空白**会拒绝启动（宁可大声失败，
不要静默错配）。`WORLD_DISABLED` 会跳过全部四个 section 的校验；
`WORLD_TOWN_DISABLED` 跳过两个小镇 section 的校验，
`WORLD_STORIES_DISABLED` 跳过 `world_stories_director` 的校验——功能关着的
时候，暂存的或写坏的配置绝不会挡住启动。

## 数据模型

| 表 | 写入方 | 内容 |
|----|--------|------|
| `engine.world_enrollments` | 下游 | 注册行 + `town_enabled` + `stories_enabled` 开关 |
| `engine.world_states` | 引擎 | 种子、摘要、导演 + 评论轮调度状态 |
| `engine.world_memories` | 引擎 | 剧本片段 + `VECTOR(512)`，按日期保留 |
| `engine.world_posts` | 引擎 | 定时/已发布贴文、回复冷却戳 + 最新用户留言戳 |
| `engine.world_post_comments` | 引擎 + 用户路由 | 评论串；`author_instance_id IS NULL` = 用户本人 |
| `engine.persona_story_insights` | 引擎 | 常驻扁平人生画像（固定 schema）+ digest + 故事回合调度状态，每个 story-eligible instance 一行 |
| `engine.persona_story_events` | 引擎 | 只追加的人生进展日志，导演词汇的 `category` + `content`，按日期保留 |
| `engine.persona_story_memories` | 引擎 | 事件的 1:1 嵌入镜像（`event_id` 外键）+ `VECTOR(512)`，按日期保留 |

`stories_enabled`（migration 0038）与 `town_enabled` 写在同一张
`world_enrollments` 行上，下游写入，同一套开关约定。八张表都套用 0013 锁定
（对 Supabase 浏览器角色 REVOKE + 无策略 RLS）。取消注册（或关掉某个开关）
立即停止模拟与注入，但保留已积累的数据——重新注册会续上同一个世界/人生。

## 审计与成本

World Memories / Town 三个任务都以共享的世界哨兵用户
`11111111-1111-1111-1111-111111111112` 把 token 用量记为 tracing 字段；
`world_stories_director` 用自己的哨兵 `11111111-1111-1111-1111-111111111113`
（dreaming = `…111`、world = `…112`、stories = `…113`——按子系统分摊花费；见
[LLM / OpenRouter 审计](llm-audit.zh.md)）。稳态下每个注册 owner 每天的成本
上界是：1 次导演调用 + 至多 `24h/round_secs` 次评论轮（且只有有活动的那些）
+ 至多 `daily_cap` 条回复 + 每个**近期有聊天的**instance 至多
`24h/interval_hours` 次故事回合——没人碰的世界依然恰好只花一次导演调用。

## 当前限制

- `world_posts` / `world_post_comments` 的行永久保留——不做 retention（不同于
  `world_memories`），这是刻意的。回复响应扫描由活跃窗口（`reply_window_secs`）
  加一个 partial index 限定，成本与贴文总数无关；未来若要磁盘 retention 旋钮，
  也是独立机制，不与 sweeper 耦合。
- 无评论分页、点赞/表态、贴文配图、用户发贴、通知——见两份 spec 的
  out-of-scope 清单。

## 设计文档

决策历史与完整边界情况表：

- [`docs/superpowers/specs/2026-07-21-world-memories-design.md`](superpowers/specs/2026-07-21-world-memories-design.md)
- [`docs/superpowers/specs/2026-07-21-world-town-design.md`](superpowers/specs/2026-07-21-world-town-design.md)
- [`docs/superpowers/specs/2026-07-23-world-stories-design.md`](superpowers/specs/2026-07-23-world-stories-design.md)
