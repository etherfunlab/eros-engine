# 記憶層

[English](memory-layers.md) · [中文](memory-layers.zh.md)

两层存着人格对你的印象。它们服务不同的回忆需求，分开查询。

## Profile vs Relationship

| 層 | `instance_id` | 存甚麼 | 生命周期 |
|---|---|---|---|
| **Profile** | `NULL` | 跨 session 的事實——任何人格跟用户熟起来之后都能知道的东西。 | 永久 |
| **Relationship** | `<uuid>` | 这个人格与这位用户之间的事：用户对它说过的话，以及[关系扫描](#关系记忆)从对话里提炼出的关系弧线、片段和纪事。 | 直到这段关系被归档 |

這個區分要緊，因為 **跨人格的角色穩定性** 跟 **單個關係內的親密度** 是不同的需求。如果你跟 Aria 說你對花生過敏，那是 profile 事實——Kenji 和用户不再是陌生人之后也能知道（见[亲密度门控](#亲密度门控)）。如果 Aria 提到她今晚在讀 Bishop，那是 relationship 記憶——Kenji 不應該假裝知道這事。

## 存儲

單表，兩層用 `instance_id` 區分：

```sql
CREATE TABLE engine.companion_memories (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id   UUID NOT NULL REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    user_id      UUID NOT NULL,
    instance_id  UUID,                         -- NULL = profile 層
    content      TEXT NOT NULL,
    embedding    VECTOR(512) NOT NULL,
    category     TEXT,                         -- fact|preference|event|emotion|relation；逐轮原文写入的行为 NULL
    metadata     JSONB,                        -- 每条记忆的不透明抽取载荷；逐轮原文写入的行为 NULL
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

兩個帶過濾條件的索引——一層一個——讓熱路徑檢索保持便宜：

```sql
CREATE INDEX idx_memories_user_profile
  ON engine.companion_memories(user_id)
  WHERE instance_id IS NULL;

CREATE INDEX idx_memories_session
  ON engine.companion_memories(session_id)
  WHERE instance_id IS NOT NULL;
```

关系扫描自己的产物存在另外几张以角色实例为键的表里，见[关系记忆](#关系记忆)。

## Embedding

默认 `voyage-4-lite` 走 Voyage 原生 API。512 维、多语言——schema 是 `VECTOR(512)`，非 Voyage 路由也会在请求里强制 `dimensions: 512`。

路由由 `[tasks.embedding]` 决定。`model` 带上 `@provider` 后缀就把调用发去别处：`@openrouter` 走内建的 OpenRouter embeddings 端点（URL 可用 `[providers].openrouter.embeddings` 覆盖），`@<name>` 走任何声明了 `embeddings` URL 的 `[providers]` 条目（密钥读 `<NAME>_API_KEY`）。另一种写法是配一对 `model_read`/`model_write`，把召回和入库模型拆开独立指定——两者必须成对出现、与 `model` 互斥，且只允许 voyage-4 系列及以上的 Voyage 模型：只有这个系列保证不同尺寸模型共享同一向量空间。

```rust
// crates/eros-engine-llm/src/embedding.rs — EmbeddingRouter
pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>, LlmError>;    // read 后端
pub async fn embed_document(&self, text: &str) -> Result<Vec<f32>, LlmError>; // write 后端
```

`embed_query` 服务每轮召回路径，`embed_document` 服务入库路径。在 Voyage 后端上，两者变成不同的 `input_type` 提示（`query` vs `document`）；OpenRouter 兼容协议没有这个提示，所以在那边这个区分只在 `model_read`/`model_write` 确实不同时才有意义。

密钥缺失时引擎依然**大声拒绝启动**——但 `VOYAGE_API_KEY` 这道 boot 闸只在解析出的路由包含 Voyage 后端时才触发；OpenRouter 和自定义路由改由各自的密钥把关（`OPENROUTER_API_KEY` / `<NAME>_API_KEY`）。閉源版的 eros-gateway 有個已知回歸：空 key 會悄悄關掉 embeddings；eros-engine 拒絕繼承這個坑。

## 檢索

走 pgvector 的 `<=>` 操作符做餘弦相似度，配 IVFFlat 索引：

```sql
CREATE INDEX idx_memories_embedding
  ON engine.companion_memories
  USING ivfflat (embedding vector_cosine_ops)
  WITH (lists = 100);
```

Profile 層查詢：

```sql
SELECT id, session_id, user_id, instance_id, content, category, metadata, created_at
FROM engine.companion_memories
WHERE user_id = $1 AND instance_id IS NULL
ORDER BY embedding <=> $2::vector
LIMIT $3;
```

Relationship 层查询改为过滤 `instance_id = $2`，并多一条 `content NOT LIKE '%\nAI：%'`（`$3`）排除旧版逐字转录行（issue #113），embedding 和 LIMIT 随之后移到 `$4`、`$5`。代码里任何地方都不计算相似度分数——结果直接按 `<=>` 余弦距离原值排序，检索就是 `LIMIT` 取 top-K；没有相似度阈值，最近的 K 行不管绝对距离多远都会返回。

`lists = 100` 是中小規模表（≲ 1M 行）的平衡默認值。語料更大就調高（經驗法則：`lists ≈ √rows`）。

## 甚麼會被 embed

两个互相独立的写入方，跑在不同的节奏上：

1. **逐轮原文写入**（`write_turn`，post-process）——在**每一个**实质轮次都跑：用户话语非空，且至少有一条非空的助手消息。它只把**用户话语**embed 进两个层，不存助手的回复文本——助手回复经记忆召回重新进入模型自己的 prompt 会形成反馈回路，让后续回复反复出现同一句话（issue #113）。关系层那一份带 `用户：` 前缀，召回出来后仍能认出是用户原话。这些行的 `category` 和 `metadata` 都是 NULL。
2. **关系扫描**（`[tasks.dreaming_scan]`，`pipeline::dreaming`）——按关系逐段读取游标之后的对话（见[关系记忆](#关系记忆)）。它的三样产出里只有画像记忆落在这张表：**profile 层的行**，带 `category ∈ {fact, preference, event, emotion, relation}`（模型自创的其它类别一律收敛成 `fact`），外加一份不透明的 `metadata`。片段和纪事写进各自的表。

`insight_extraction` 是**另一条**流水线，不往这张表写任何东西：它把关于用户的原子事实记进事实账本（`human_facts`），再由按计划的重建从账本派生出扁平的 `human_insights` 画像，这些都不在这里 embed（见 [insights.zh.md](insights.zh.md)）。另外两条实验性、按关系分开的链（`character_insight_*`、`user_insight_*`）各自 UPSERT 进自己的扁平表——`character_insights`（见 [api-reference.zh.md](api-reference.zh.md#get-v2compinstanceinstance_idinsightcharacter)）和 `user_insights`（见 [api-reference.zh.md](api-reference.zh.md#get-v2compinstanceinstance_idinsightuser)）；跟 `human_insights` 一样，这两张表都不会 embed 进这张表。`user_insights` 不会被注入任何 chat、voice 或 PDE prompt；`character_insights` 会——它的四个字段渲染 chat prompt 里的 `[character_state]` 块，整行填得有多满还决定注入的历史窗口大小（见[部署 → 運維注意事項](deploying.zh.md#運維注意事項)）。

所以 embedding 是每个实质轮次生成一次，不是「只有 LLM 挑出来的高光才生成」。

**群聊轮次**走同一个逐轮原文写入器，用户消息只 embed 一次。Profile 层写一行（`instance_id` 为 NULL）；关系层为每一位入座成员各写一行，不论它有没有开口，因为每位成员都在场。角色实例未激活的成员这一轮不入座。群聊来源的行 `session_id` 是群（话是在那里说的），内容带 `（群聊）用户：` 前缀，之后的 1:1 召回因此知道这句话出自哪里。这些行按各成员的 `instance_id` 归属，所以 `DELETE /comp/instance/{instance_id}/sessions` 会连同该关系的其余数据一并删除它们。详见 [group-chat.zh.md](group-chat.zh.md#进入-11-的内容)。

## 甚麼不被存

原始對話消息存在 `engine.chat_messages` 裡（完整逐字記錄、純文本）。它們 **不被** embed。記憶表存的是 *摘要* 跟 *事實*，不是完整消息日誌。想拿真實對話內容直接查 `chat_messages`——那才是「說了甚麼」的真相之源。

## 关系记忆

关系扫描（`[tasks.dreaming_scan]`，`pipeline::dreaming`）读取一段关系——一个角色实例和它的主人——的对话，除画像记忆之外再写三样产物。它们全部从对话里提炼，用户从不直接写入。

| 表 | 一行对应 | 存什么 |
|----|----------|--------|
| `relationship_episodes` | 扫过的一段对话 | 这段对话的摘要，最多 400 字，`started_at` / `ended_at` 取自它覆盖的第一行和最后一行。只追加。 |
| `relationship_arcs` | 一段关系 | `opening`（这段关系怎么开始的，最多 150 字，由第一次改写写下，此后不再改）、`arc`（到目前为止的故事，最多 900 字）和 `previous_arc`。 |
| `relationship_journal` | 一个时刻 | `occurred_on`（用户当地的日期）、`kind`、`realm`、一句话的 `summary`（最多 200 字），以及供语义召回的 embedding。 |

`relationship_scan_state` 存每段关系的游标、待扫轮数和认领，`relationship_scan_events` 为每次扫描或弧线改写记一行只含计数的记录（[LLM 审计](llm-audit.zh.md#关系扫描)）。这些表都没有 `user_id`：实例决定了归属。

### 什么时候扫描

- 每个 1:1 文字轮次都计入它所在关系的待扫轮数；语音轮次也计入，除非设置了 `DREAMING_VOICE_DISABLED`。群聊轮次不计。第一个被计入的轮次创建这段关系的扫描状态，游标就落在这一轮之前；更早的历史归[回填](deploying.zh.md#子命令)处理。
- 待扫轮数到 **20** 时，这一文字轮次的 post-process 在本轮其它写入之后就地扫描，读到本轮自己最新的那一行为止。语音轮次从不就地扫描。
- dreaming 清扫器每 `DREAMING_TICK_SECS`（默认 300）秒醒一次，按等待时间从长到短，扫描最多 10 段待扫轮数至少 **3**、最后一次被计入的轮次早于 `DREAMING_IDLE_SECS`（默认 3600）的关系。待扫轮数不到 3 的关系要等攒到 3 轮。
- 两条路径都先认领这段关系，所以不会有两个 worker 同时扫同一段关系。`DREAMING_DISABLED=1`（或 `DREAMING_TICK_SECS=0`）两条都停；没有 `[tasks.dreaming_scan]` 时两条都不跑。

### 一次扫描读什么、写什么

一次扫描最多读游标之后的 **60** 行：这对组合未归档的文字与语音 session 里的用户行和助手行，按时间排序，产品问答行一律不读。它们作为一份对话记录发给模型，每行一句，带 `[#n YYYY-MM-DD HH:MM]` 戳，按用户时区记时（取这段关系的文字轮次最近带来的有效时区；没有就用 UTC）。语音助手行先剥掉 TTS 音频标签；带动作的用户行显示动作标记，动作有了回应后再接上结果。一次调用返回画像记忆、一条片段摘要和若干纪事时刻；契约见[模型配置](model-config.zh.md#tasksdreaming_scan--关系扫描opt-in)。

画像记忆和纪事摘要在一次批量调用里 embed；随后先写片段，再写画像记忆，最后写纪事，游标移到读过的最后一行。超出 60 行上限的部分等下一次扫描。一段对话里没有可读的内容时，游标直接前移，不发调用。

调用失败、回复无法解析、embedding 调用失败，或写入之前的数据库错误，都会让游标原地不动，清扫器隔一个闲置周期再试。连续第三次失败时带一条警告跳过这一段；它的原话行仍在。扫描进行中落下的归档或抹除优先：这次扫描不再写入任何东西。

### 关系弧线

每写满第四个片段触发一次改写（`[tasks.arc_rewrite]`，prompt 由引擎持有），输入是开头、上一版弧线和还没折入的片段，各自带日期。prompt 要求 400~600 字，按时间顺序，先前的状态写成「曾经……后来……」，越早越简略，言行归到正确的一方，角色扮演写成角色扮演。下列改写会被引擎拒绝：无法解析；超过 900 字；上一版超过 300 字而新版不到它的 70%；第一次改写缺少开头，或开头超过 150 字。被拒绝或失败的改写让上一版弧线继续生效，到下一个第四个片段时再试，等待中的片段一并带上；连续三次之后，这些片段标记为已折入，不产生新弧线。没有 `[tasks.arc_rewrite]` 时片段一直不折入。

### 纪事

一部分行由引擎自己写入，不调用 LLM，`realm = real`：

| 时刻 | `kind` | 是否回归 |
|------|--------|----------|
| 这段关系的第一次聊天，在它的第一个轮次被计入时记下，日期取最早的一行聊天 | `first_meet` | 先按月，后按年 |
| 第一次达到某个 bond 或 chemistry 标签，与好感度写入同一个事务 | `tier_reached` | 否 |
| 被接受的 `give`、`gift` 或 `tip` 动作（打赏从不写金额） | `gift` | 否 |
| 第一次被接受的 `kiss` 动作 | `first_intimacy` | 否 |

这段关系已经记过的标签不会再记，哪怕之后的 session 再次达到。某条线下降时，它掉到之下的那些标签的 `tier_reached` 行若不满 24 小时就一并撤回，不论下降来自判官评估的轮次、被拒绝的动作，还是生活抉择。`hug`、`touch`、`lick` 和 `custom` 动作不记。

其余由扫描的模型写入：`confession`、`first_intimacy`、`conflict`、`reconciliation`、`promise`、`gift`、`shared_plan` 和 `user_shared_milestone`，日期取自它所出自的那份对话记录里的时间戳。`realm` 看发起权，与是否真实发生无关：角色主动，或用户邀约、角色同意，是 `real`；用户提供剧本、设定场景或直接描述这一刻、角色配合演出，是 `roleplay`。带到期日的 `promise` 或 `shared_plan` 会在那一天回归。只该有一行的地方由一个键保留先写入的那一行：一个 `first_meet`，每种 realm 一个 `first_intimacy`，每条消息一个 `gift`，每条线的每个标签一个 `tier_reached`。

引擎写入的行不带向量，清扫器在下一次醒来时为它们补上，每次最多 100 行、一次调用。在那之前，以及在清扫器不运行的部署上，它们不参与语义召回；纪念日行不需要向量。

### 进入 prompt 的样子

在 1:1 prompt 上（文字轮次、主动消息、改图回复），`[shared_memories]` 的关系部分变成：

```
[shared_memories]
{opening}
{arc}
- 2026-10-12：{片段摘要}
- 2026-10-20：{片段摘要}
- 2026-08-15（扮演）：{纪事摘要}
- 用户：…
```

- 弧线段落：开头和弧线，各占一行。
- 最多三个片段，取结束时间最新的三个，从旧到新排，日期取片段开始时间在请求时区里的日期（没有时区就用 UTC）。
- 最多两条纪事，按与本轮查询向量的距离由近到远，日期取 `occurred_on`；`roleplay` 的纪事标 `（扮演）`。
- 原话召回行，和以前一样。
- **预算：这一部分 1,200 字**；同一标题下的 `[world_memories]`、`[world_stories]` 和 `[character_state]` 不计在内。超出时依次删：片段从最旧的删起，再删原话行（从排名最低的删起），最后删纪事（从最远的删起）。弧线永不删。
- **没有新内容，就一切照旧。** 没有弧线、没有片段、也没有与本轮相近的纪事的关系，块和以前一字不差：原话行不设预算，一行都没有时显示占位语。

**纪念日行**放在 `[now]` 里生日行之后，每轮最多两行，初识的排在前面。它们读取这段关系会回归的 `real` 纪事行，并且和节日行一样需要请求带 `user_timezone`：

- `first_meet`，第 1–11 个月：只在每月的同一天（超出当月天数就取月末）当天出现，`今天是你们认识满 N 个月的日子。`
- `first_meet`，满 12 个月起：每年的这一天提前六天开始出现，`今天` / `明天` / `N 天后是你们认识满 Y 年的日子。`
- 到期的 `promise` 或 `shared_plan`：当天出现，`今天是你们约好的日子：{summary}。`

以上全部，纪念日行在内，都跟随 `memory_scope` 的关系层开关：`neutral_only`、`insights_only` 和 `none` 下都没有。[亲密度门控](#亲密度门控)不裁剪它们——弧线、片段和纪事是这段关系自己的记忆，在任何档位都会渲染。群聊席位一概没有。语音引导快照以 `[你们的故事]` 带上开头和弧线（[语音轮次](#语音轮次)）；语音的逐轮召回不检索纪事。

### 更早的历史

扫描从每段关系第一个被计入的轮次开始。`backfill-dreaming` 子命令负责读取更早的历史（[部署 → 子命令](deploying.zh.md#子命令)）；它是由运维手动执行、按调用计费的一步，`serve` 从不运行它。

## 检索与注入

每轮对话都会把记忆读回 prompt，由每次请求的 `memory_scope` 控制（取值与默认值
见 [api-reference.zh.md](api-reference.zh.md)；默认 `neutral_and_relationship`）。回复
handler（`pipeline::handlers`）从两个来源拼出画像 / 关系上下文块：

- **画像层** —— 由两个来源合并。基础画像 bullet 直接来自扁平的
  **`human_insights`** 表，即从用户事实账本派生出的画像（[insights.zh.md](insights.zh.md#读取方)）；
  `memory_scope` 决定是否带上私密字段（`full` / `insights_only`）还是只带
  中性子集（`neutral_*`）。与之并列，画像层的 `companion_memories` 行也会按相似度
  检索出来并按 `category` 分组注入，但仅当 scope 保留画像记忆时才会跑（`full` /
  `neutral_and_relationship`）——`relationship_only`、`neutral_only`、
  `insights_only`、`none` 都会整段跳过这一半，只剩下 `human_insights` bullet
  （如果该 scope 下还有 bullet 的话）。
- **关系层** —— `companion_memories` 行，按对当前轮的语义（embedding）相似度
  检索拉取，在 scope 保留关系记忆时纳入（`full` / `neutral_and_relationship` /
  `relationship_only`）。在 1:1 prompt 上，这段关系的弧线、最新的片段和最相近的
  纪事在同一个开关下与它们并列（[关系记忆 → 进入 prompt 的样子](#进入-prompt-的样子)）。

`memory_scope = none` 完全跳过记忆注入。**`memory_scope` 只管 prompt 注入，
不管写入。** 即使是 `none`，逐轮原文写入照样把这一轮 embed 并存下来，insight 抽取和
好感度评估也照常跑；目前没有任何 scope 取值能抑制写入。前端的
`/comp/user/{user_id}/profile` 端点返回类型化的 `human_insights` 行，作为已收集
内容的人类可读视图。

### 亲密度门控

`memory_scope` 是上限。在它之内，这段关系当前的亲密度决定有多少内容进 prompt：
一个跟用户还不熟的人格，不知道用户跟别的人格说过什么，也不会把用户说的话记得多清楚。
门控读取时间衰减后亲密度的 bond 和 chemistry 两条线的档位（1–5，见
[affinity-model.zh.md](affinity-model.zh.md)）；没有亲密度记录的组合两条线都按第 1 档算。
它只做减法——scope 排除的东西永远不会被加回来——并且作用在每一条注入路径上：
文字轮次、群聊里的每个席位（用该席位自己的亲密度）、语音，以及改图请求生成的文字回复。

- **基础画像私密字段** —— 每个字段只看一条线，并有这条线上的起始档；对照表在
  [insights.zh.md](insights.zh.md#聊天-prompt)。
- **画像层** —— K 跟着两条线里较高的那一档走：第 1 档完全不召回这一层，第 2 档取平常 K
  的一半（向上取整），第 3 档及以上取平常的 K。按类别分组的召回和未分类兜底都照此执行。
  此外，`metadata.domain` 对应某个私密字段的记忆行，还要达到那个字段的档位：

  | `metadata.domain` | 线 | 起始档 | 对应字段 |
  |-------------------|----|--------|----------|
  | `sexuality`、`kink` | chemistry | 4 | `kinks` |
  | `romance` | chemistry | 2 | `love_values` |
  | `family` | bond | 3 | `family` |
  | `finance` | bond | 4 | `finance_status` |
  | `health`、`mental_health` | bond | 4 | `mental_health` |

  domain 是 `dreaming_scan` prompt 给每条记忆写下的标签（见 [model-config.zh.md](model-config.zh.md)），
  匹配时去掉首尾空白、不分大小写。其他取值、非字符串或没有 domain 的行只受 K 的限制。
  未分类的行（原话行）没有 domain，所以也只受 K 限制。
  被扣下的行照样占它所在类别的检索名额：检索按平常的 K 只跑一次，门控再裁剪结果。
- **关系层** —— K 是平常的 K，但不超过两条线里较高的那一档（文字：第 1 / 2 / 3+ 档分别
  1 / 2 / 3 条；语音：1 / 2 条）。内容不过滤：这些是用户对这个人格说过的话。弧线、片段和
  纪事完全不受门控，在任何档位都会渲染。

### 语音轮次

语音端点（`POST /comp/voice/{session_id}/turn/stream`）读取同样的两层，但走
一条更精简的语音专属路径——请求体见
[api-reference.zh.md](api-reference.zh.md#post-compvoicesession_idturnstream)，
`[tasks.chat_voice]` 块见 [model-config.zh.md](model-config.zh.md)。

- **引导快照** —— 仅在 session 首轮组装一次，冻结进
  `chat_sessions.metadata.voice_bootstrap`，此后每轮原样重新注入（OpenRouter
  是无状态的，线路上不存在"只注入一次"这回事——之后的轮次也改不了它）。分五
  部分：默认中性档的 `human_insights` bullets（档位可由首轮的 `memory_scope`
  调整）；这段关系的开头和弧线，以 `[你们的故事]` 呈现（仅当已经有弧线、且首轮的
  `memory_scope` 保留关系记忆时）；上一通语音通话最后 8 条消息渲染出的纯文字记录；通话所读亲密度
  那条记录所属的文字 session 的余效（见
  [接口参考](api-reference.zh.md#get-v2compsessionsession_ideffects)）；以及
  通话开始时角色的需求。各部分
  各自独立降级；组装失败时标记位不落，下一轮重试。标记位本身不变，但每一轮都用当轮的档位
  让其中的基础画像 bullets 过一遍[亲密度门控](#亲密度门控)。
- **逐轮召回** —— 与聊天路径相同的 `companion_memories` 检索（受
  `memory_scope` 门控，经 `memory_hygiene` 做跨层去重），但 K 值小得多：
  分组画像 1/类别 + 原始兜底 2 + 关系记忆 2（聊天路径是 2/4/3），等这对组合的亲密度
  （并行读取，和文字端一样做时间衰减）返回后再由[亲密度门控](#亲密度门控)裁剪；整体包在
  300 ms 预算里——超时或检索出错只是丢掉这一轮的召回块，打一条 warn 日志，
  绝不是 error 帧。去掉空白和标点后不足 4 个字母数字字符的话语（嗯 / 好啊 /
  哈哈这类应和词）直接跳过召回，不发起 embedding 调用。部署方可以用
  `[tasks.chat_voice] recall = false`（默认 `true`）强制关闭，优先级高于
  请求里的 `memory_scope`。
- **经由关系扫描写入** —— 语音轮次进行中不写任何记忆：没有逐轮原文写入，没有
  insight 抽取，也没有好感度评估。它计入这段关系的[扫描](#关系记忆)，但从不就地
  扫描；关系闲置且攒够至少 3 个待扫轮次后由清扫器来扫，文字轮次的就地扫描也会读到
  范围内的语音行。扫描把通话和同一段关系的文字行放在一起读，从中写出画像记忆、片段
  和纪事——和文字完全一样。这些都是普通记忆，之后的语音通话和文字聊天都能召回。
  语音助手行里的 TTS 音频标签（开了 `tts_audio_tags` 时的 `[laughs]`、
  `[sighs]`）会先被剥掉，舞台提示不会变成记忆文本。`DREAMING_VOICE_DISABLED=1`
  让扫描不读语音 session，语音轮次也不再计入。游标按时间前移，所以开关打开期间
  被跳过的语音行，关掉开关后也不会再读。原话关系层的行从不由语音写入。
- **被打断的那轮留下什么** —— 被打断的轮次贡献的是客户端上报的「实际播出」
  文本，而不是完整生成内容，所以扫描读到的是用户**听到**的东西。在首个可听
  词之前就被切断的轮次只贡献用户那一行，因为根本没有写入 assistant 行。两种
  情况下通话记录都与这通电话本身一致——这正是记录 interrupt 的意义所在。

设计文档：
[2026-08-09-voice-memory-bootstrap-recall-design.md](superpowers/specs/2026-08-09-voice-memory-bootstrap-recall-design.md)
（引导快照 + 召回）与
[2026-08-09-voice-dreaming-ingestion-design.md](superpowers/specs/2026-08-09-voice-dreaming-ingestion-design.md)
（dreaming 读取语音行）。关系扫描：
[2026-10-28-memory-2-dreaming-plus-design.md](superpowers/specs/2026-10-28-memory-2-dreaming-plus-design.md)。

## 源碼

- `crates/eros-engine-store/src/memory.rs`——`MemoryRepo`（upsert + search，7 个 sqlx::test 集成测试）
- `crates/eros-engine-store/src/human_insight.rs`——扁平类型化的 `human_insights` 存储；画像重建的 `replace_profile` 写入，回复 handler 在注入时读取；事实账本在 `human_fact.rs`
- `crates/eros-engine-store/src/scan_state.rs`、`episode.rs`、`arc.rs`、`journal.rs`、`scan_event.rs`——关系扫描的表：游标与认领、片段与分段读取、弧线、纪事（含引擎自己写入的行）、事件行
- `crates/eros-engine-llm/src/voyage.rs`——Voyage 原生客户端
- `crates/eros-engine-llm/src/embedding.rs`——`EmbeddingRouter` + OpenRouter 兼容客户端
- `crates/eros-engine-server/src/pipeline/post_process.rs`——逐轮原文写入路径；把每个文字轮次计入所在关系的扫描
- `crates/eros-engine-server/src/pipeline/dreaming/`——关系扫描：触发与清扫器（`mod.rs`）、扫描与弧线改写（`scan.rs`）、回复解析与校验（`parse.rs`）、对话记录渲染（`render.rs`）
- `crates/eros-engine-server/src/relationship_context.rs`——为一轮加载弧线、片段和纪事，并渲染 `[shared_memories]` 的关系部分
- `crates/eros-engine-server/src/anniversary.rs`——`[now]` 里的纪念日行
- `crates/eros-engine-server/src/backfill.rs`——`backfill-dreaming` 子命令
- `crates/eros-engine-server/src/pipeline/handlers.rs`——检索 + prompt 注入
- `crates/eros-engine-server/src/pipeline/voice.rs`——语音轮次的引导快照 + 逐轮召回
- `crates/eros-engine-store/migrations/0000_baseline.sql`——表、索引与 `category` / `metadata` 列（由 baseline 之前的迁移链折叠而来）
- `crates/eros-engine-store/migrations/0082_relationship_memory.sql`——关系扫描的五张表
