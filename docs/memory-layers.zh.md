# 记忆分层

[English](memory-layers.md) · [中文](memory-layers.zh.md)

角色记得两类东西：它对用户了解到了什么，以及它和用户之间发生过什么。前者是**画像层**，用户聊过的任何角色都能用上；后者是**关系层**，按角色实例分开保存：用户对这个角色说过的话，以及引擎从对话里整理出来的这段关系的故事——一条持续改写的弧线、带日期的片段，和一本会在纪念日回到眼前的纪事。

本页讲这两层怎么写入、怎么存储、怎么召回、怎么进入 prompt。v2.6.0 之前的模型见[记忆（legacy）](memory-layers_legacy.zh.md)；升级步骤见 [v2.6.0 迁移指南](migrating/v2-6-0-memory.md)。

## 整体结构

四个写入方，各有各的节奏：

| 写入方 | 什么时候跑 | 写什么 |
|--------|------------|--------|
| [逐轮原文写入](#逐轮原文写入) | 每个实质轮次，在 post-process 里 | 用户这一句，embed 后写进 `companion_memories` 的两层 |
| [Dreaming](#dreaming)（`[tasks.dreaming_scan]`） | 按关系，待扫轮数到 20 或关系闲置之后 | 画像记忆、一个片段、若干纪事 |
| [弧线改写](#关系弧线)（`[tasks.arc_rewrite]`） | 每第四个片段 | 这段关系的弧线 |
| [引擎自己](#纪事) | 时刻发生的那一轮，不调用 LLM | 初识、首次达到某档、礼物和第一次亲吻的纪事行 |

进入 1:1 prompt 的部分：

| prompt 部分 | 来源 |
|-------------|------|
| 基础画像 bullets | `human_insights`，由 [insights](insights.zh.md) 流水线派生的画像（不属于本页的表） |
| `[shared_memories]` | 按相似度召回、按类别分组的画像记忆；这段关系召回的原话行；它的弧线、最近的片段和最相近的纪事 |
| `[now]` 下的纪念日行 | 纪事里会回归的行 |

有多少能进 prompt，由请求的 `memory_scope` 决定，在它之内再由这段关系的亲密度决定（[亲密度门控](#亲密度门控)）。

## 画像层与关系层

| 层 | 归属 | 存什么 | 生命周期 |
|----|------|--------|----------|
| **画像** | 用户（`instance_id` 为 NULL） | 关于用户的事实，任何角色和用户熟起来之后都能知道 | 永久，直到用户被抹除 |
| **关系** | 角色实例 | 用户对这个角色说过的话，加上这段关系的弧线、片段和纪事 | 直到这段关系被归档 |

分两层，是因为**跨角色的稳定**和**一段关系之内的亲密**是两回事。你跟 Aria 说你对花生过敏，那是画像事实：Kenji 和用户不再是陌生人之后也能知道。Aria 提到她今晚在读 Bishop，那属于这段关系：Kenji 不该假装知道。

## 存储

**`companion_memories`** 存两层可召回的行，用 `instance_id` 区分：

```sql
CREATE TABLE engine.companion_memories (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id   UUID NOT NULL REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    user_id      UUID NOT NULL,
    instance_id  UUID,                         -- NULL = 画像层
    content      TEXT NOT NULL,
    embedding    VECTOR(512) NOT NULL,
    category     TEXT,                         -- fact|preference|event|emotion|relation；原文行为 NULL
    metadata     JSONB,                        -- 扫描给每条记忆的不透明载荷；原文行为 NULL
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

两个带过滤条件的索引，一层一个，让热路径保持便宜：

```sql
CREATE INDEX idx_memories_user_profile
  ON engine.companion_memories(user_id)
  WHERE instance_id IS NULL;

CREATE INDEX idx_memories_session
  ON engine.companion_memories(session_id)
  WHERE instance_id IS NOT NULL;
```

**这段关系的故事**存在自己的几张表里，按角色实例归属。它们都没有 `user_id`：实例决定了归属。

| 表 | 一行对应 | 存什么 |
|----|----------|--------|
| `relationship_episodes` | 扫过的一段对话 | 最多 400 字的摘要，`started_at` / `ended_at` 取自它覆盖的第一行和最后一行，以及弧线是否已经把它折入。只追加。 |
| `relationship_arcs` | 一段关系 | `opening`（这段关系怎么开始的，最多 150 字，只写一次）、`arc`（到目前为止的故事，最多 900 字）和 `previous_arc`。 |
| `relationship_journal` | 一个时刻 | `occurred_on`（用户当地的日期）、`kind`、`realm`（`real` 或 `roleplay`）、最多 200 字的一句话 `summary`、可选的 `due_on`，以及供语义召回的 embedding。 |
| `relationship_scan_state` | 一段关系 | 扫描的账：游标、待扫轮数、认领、失败计数、用户最近一次已知的时区。 |
| `relationship_scan_events` | 一次扫描或弧线改写 | 只有计数和 id，给运维看（[LLM 审计](llm-audit.zh.md#关系扫描)）。关系归档后保留。 |

## Embedding

默认 `voyage-4-lite` 走 Voyage 原生 API：512 维、多语言。schema 是 `VECTOR(512)`，非 Voyage 路由会在请求里强制 `dimensions: 512`。

路由由 `[tasks.embedding]` 决定。`model` 带上 `@provider` 后缀就把调用发去别处：`@openrouter` 走内建的 OpenRouter embeddings 端点（URL 可用 `[providers].openrouter.embeddings` 覆盖），`@<name>` 走任何声明了 `embeddings` URL 的 `[providers]` 条目（密钥读 `<NAME>_API_KEY`）。另一种写法是配一对 `model_read`/`model_write`，把召回和入库模型拆开指定。两者必须成对出现、与 `model` 互斥，且只允许 voyage-4 系列及以上的 Voyage 模型：只有这个系列保证不同尺寸的模型共享同一个向量空间。

```rust
// crates/eros-engine-llm/src/embedding.rs — EmbeddingRouter
pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>, LlmError>;    // read 后端
pub async fn embed_document(&self, text: &str) -> Result<Vec<f32>, LlmError>; // write 后端
```

`embed_query` 服务每轮召回，`embed_document` 服务所有入库路径。在 Voyage 后端上两者是不同的 `input_type` 提示（`query` 与 `document`）；OpenRouter 兼容协议没有这个提示，所以那边只有 `model_read` 和 `model_write` 真的不同时，这个区分才有意义。

缺密钥会让启动失败。`VOYAGE_API_KEY` 的检查只在解析出的路由包含 Voyage 后端时触发；OpenRouter 和自定义路由检查各自的密钥（`OPENROUTER_API_KEY` / `<NAME>_API_KEY`）。

## 逐轮原文写入

`write_turn` 在 post-process 里对**每一个**实质轮次运行（用户话语非空，且至少有一条非空的助手消息）。它只把**用户这一句** embed 进两层，`category` 和 `metadata` 都是 NULL。关系层那一份带 `用户：` 前缀，召回出来仍能认出是用户的原话。助手的回复从不 embed：它经召回回到 prompt 里，会把回复往重复同一句话的方向拉。

**群聊轮次**走同一个写入方，用户消息只 embed 一次。画像层写一行；关系层为每一位入座成员各写一行，不论它有没有开口，因为每位成员都在场。角色实例未激活的成员这一轮不入座。群聊来源的行 `session_id` 是群，内容带 `（群聊）用户：` 前缀，之后的 1:1 召回因此知道这句话是在哪里说的。详见 [group-chat.zh.md](group-chat.zh.md#进入-11-的内容)。

**不 embed 的东西。** `engine.chat_messages` 以纯文本保存完整的对话记录，从不 embed；它是「说过什么」的真相来源。insight 各条流水线只写自己的表，不往这里写：`insight_extraction` 维护事实账本（`human_facts`），按计划的重建从账本派生出 `human_insights`（[insights.zh.md](insights.zh.md)）；实验性的按关系链 UPSERT `character_insights` 和 `user_insights`（见 [api-reference.zh.md](api-reference.zh.md#get-v2compinstanceinstance_idinsightcharacter)）。其中只有 `human_insights`（基础画像 bullets）和 `character_insights` 的四个字段（`[character_state]` 块，见[部署 → 運維注意事項](deploying.zh.md#運維注意事項)）会进入 prompt。

## 关系记忆

Dreaming 读取一段关系——一个角色实例和它的主人——的对话，把这段关系的故事整理成片段、弧线和纪事；途中发现的画像记忆也一并写入。这里的一切都从对话中提炼，用户从不直接写入，也没有端点对外暴露。

### Dreaming

Dreaming 就是关系扫描，`pipeline::dreaming`。它为每段关系保留一个游标，从游标往后读。

- **计数。** 每个 1:1 文字轮次给它所在的关系加一个待扫轮数；语音轮次也算，除非设置了 `DREAMING_VOICE_DISABLED`。群聊轮次不算。一段关系第一个被计入的轮次创建它的扫描状态，游标就落在这一轮之前；更早的历史由[回填](#更早的历史)读取。
- **待扫轮数到 20** 时，这一文字轮次的 post-process 在本轮其它写入之后就地扫描，读到本轮自己最新的那一行为止。语音轮次从不就地扫描。
- **闲置清扫。** dreaming 清扫器每 `DREAMING_TICK_SECS`（默认 300）秒醒一次，按等待时间从长到短，扫描最多 10 段待扫轮数至少 **3**、最后一次被计入的轮次早于 `DREAMING_IDLE_SECS`（默认 3600）的关系。不到 3 轮的关系要等攒到 3 轮；它的行会被最终轮到它的那次扫描读到。
- **一次一个。** 两条路径扫描前都先认领这段关系，所以不会有两个 worker 同时扫同一段关系。
- **开关。** 没有 `[tasks.dreaming_scan]` 时什么都不扫。`DREAMING_DISABLED=1`（或 `DREAMING_TICK_SECS=0`）同时停掉清扫器和就地扫描。两种情况下轮次照样计数，引擎自己的纪事行也照样写。

一次扫描最多读游标之后的 **60** 行：这对组合未归档的文字与语音 session 里的用户行和助手行，按时间排序，产品问答行一律不读。它们作为一份对话记录发给模型，每行一句，带 `[#n YYYY-MM-DD HH:MM]` 戳，按用户时区记时（取这段关系的文字轮次最近带来的有效时区；没有就用 UTC）。语音助手行先剥掉 TTS 音频标签。带动作的用户行显示动作标记，角色回应之后再接上结果。

一次 `dreaming_scan` 调用返回全部三样产物：画像记忆、一条片段摘要和若干纪事时刻。回复的契约见[模型配置](model-config.zh.md#tasksdreaming_scan--关系扫描opt-in)。画像记忆和纪事摘要在一次批量调用里 embed；随后先写片段，再写画像记忆（写进画像层，归到实例主人名下），最后写纪事，游标移到读过的最后一行。超出 60 行上限的部分等下一次扫描。一段对话里没有可读的内容时，游标直接前移，不发调用。

调用失败、回复无法解析、embedding 调用失败，或写入之前的数据库错误，都会让游标原地不动，清扫器隔一个闲置周期再试。连续第三次失败时带一条警告跳过这一段；它的原话仍留在 `chat_messages` 里。扫描进行中落下的归档或抹除优先：这次扫描不再写入任何东西。

### 关系弧线

弧线是这段关系到目前为止的故事，随片段累积不断改写。每写满第四个片段触发一次改写（`[tasks.arc_rewrite]`，prompt 由引擎持有），输入是开头、上一版弧线和还没折入的片段，各自带日期。prompt 要求 400~600 字，按时间顺序：先前的状态写成「曾经……后来……」，越早的事越简略，言行归到做出它的一方，角色扮演写成角色扮演。第一次改写同时写下**开头**——这段关系是怎么开始的——此后不再改写。

下列改写会被引擎拒绝：无法解析；超过 900 字；上一版超过 300 字而新版不到它的 70%；第一次改写缺少开头，或开头超过 150 字。被拒绝或失败的改写让上一版弧线继续生效，到下一个第四个片段时再试，等待中的片段一并带上；连续三次之后，这些片段标记为已折入，不产生新弧线。没有 `[tasks.arc_rewrite]` 时片段一直不折入，配上之后的第一次改写把它们一次全部折入。

### 纪事

纪事记下这段关系里的时刻，一个时刻一行，带日期。每一行都有 `realm`：角色和用户之间真正发生的是 `real`，两人演的一场戏是 `roleplay`。`realm` 看**发起权**，与是否在物理上发生无关：角色主动，或用户邀约、角色同意，是 `real`；用户提供剧本、设定场景或直接描述这一刻、角色配合演出，是 `roleplay`。

一部分行由引擎自己写入，不调用 LLM，`realm = real`：

| 时刻 | `kind` | 是否回归 |
|------|--------|----------|
| 这段关系的第一次聊天，在它的第一个轮次被计入时记下，日期取最早的一行聊天 | `first_meet` | 先按月，后按年 |
| 第一次达到某个 bond 或 chemistry 标签，与好感度写入同一个事务 | `tier_reached` | 否 |
| 被接受的 `give`、`gift` 或 `tip` 动作（打赏从不写金额） | `gift` | 否 |
| 第一次被接受的 `kiss` 动作 | `first_intimacy` | 否 |

这段关系已经记过的标签不会再记，哪怕之后的 session 再次达到。某条线下降时，它掉到之下的那些标签的 `tier_reached` 行若不满 24 小时就一并撤回，不论下降来自判官评估的轮次、被拒绝的动作，还是生活抉择。`hug`、`touch`、`lick` 和 `custom` 动作不记。

其余由扫描的模型写入：`confession`、`first_intimacy`、`conflict`、`reconciliation`、`promise`、`gift`、`shared_plan` 和 `user_shared_milestone`，日期取自它所出自的那份对话记录里的时间戳。带到期日的 `promise` 或 `shared_plan` 会在那一天回归。只该有一行的地方由一个键保留先写入的那一行：一个 `first_meet`，每种 realm 一个 `first_intimacy`，每条消息一个 `gift`，每条线的每个标签一个 `tier_reached`。

引擎自己写的行不带向量，dreaming 清扫器在下一次醒来时为它们补上，每次最多 100 行、一次调用。在那之前，以及在清扫器不运行的部署上，它们不参与语义召回；纪念日行不需要向量。

### 更早的历史

实时扫描从每段关系第一个被计入的轮次开始。`backfill-dreaming` 子命令读取在那之前的历史（[部署 → 子命令](deploying.zh.md#子命令)）：它是由运维手动执行、按调用计费的一步，`serve` 从不运行它。

## 检索与注入

每轮对话都会把记忆读回 prompt，由请求的 `memory_scope` 控制（取值见 [api-reference.zh.md](api-reference.zh.md)；默认 `neutral_and_relationship`）。回复 handler（`pipeline::handlers`）从这些来源拼出上下文：

- **画像层。** 基础画像 bullets 来自 `human_insights`（[insights.zh.md](insights.zh.md#读取方)）；`memory_scope` 决定是否带上私密字段（`full` / `insights_only`）还是只带中性子集（`neutral_*`）。与之并列，画像层的 `companion_memories` 行按相似度召回、按 `category` 分组，但仅当 scope 保留画像记忆时才会跑（`full` / `neutral_and_relationship`）。`relationship_only`、`neutral_only`、`insights_only` 和 `none` 跳过这一半，只剩 scope 仍允许的 bullets。
- **关系层。** 按与当前轮的相似度召回的关系层 `companion_memories` 行；在 1:1 prompt 上再加这段关系的弧线、最近的片段、最相近的纪事和纪念日行。这些都要求 scope 保留关系记忆（`full` / `neutral_and_relationship` / `relationship_only`）。

`memory_scope = none` 完全跳过记忆注入。**`memory_scope` 只管 prompt，不管写入：**即使是 `none`，逐轮原文写入照样存下这一轮，这一轮照样计入 dreaming，insight 抽取和好感度评估也照常跑。`/comp/user/{user_id}/profile` 端点返回 `human_insights` 行，作为已收集内容的可读视图。

### 召回

相似度检索用 pgvector 的 `<=>` 余弦距离，配 IVFFlat 索引：

```sql
CREATE INDEX idx_memories_embedding
  ON engine.companion_memories
  USING ivfflat (embedding vector_cosine_ops)
  WITH (lists = 100);
```

画像层查询：

```sql
SELECT id, session_id, user_id, instance_id, content, category, metadata, created_at
FROM engine.companion_memories
WHERE user_id = $1 AND instance_id IS NULL
ORDER BY embedding <=> $2::vector
LIMIT $3;
```

关系层查询改为按 `instance_id` 过滤，并排除内容里含 `\nAI：` 的行——那是早期写入方留下的逐字对话记录。不计算分数，也没有阈值：按余弦距离原值排序，最近的 K 行不管多远都会返回。纪事用同样的方式检索，用本轮的查询向量，走它自己的 IVFFlat 索引。

`lists = 100` 适合中小规模的表（≲ 1M 行）；更大的表调高（经验法则：`lists ≈ √rows`）。

### `[shared_memories]`

在 1:1 prompt 上（文字轮次、主动消息、改图回复），`[shared_memories]` 的关系部分是：

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
- 最多两条纪事，与本轮最相近的在前，日期取 `occurred_on`；`roleplay` 的纪事标 `（扮演）`。
- 召回的原话行。
- **预算：这一部分 1,200 字。** 同一标题下的 `[world_memories]`、`[world_stories]` 和 `[character_state]` 不计在内。超出时依次删：片段从最旧的删起，再删原话行（从排名最低的删起），最后删纪事（从最远的删起）。弧线永不删。
- 没有弧线、没有片段、也没有与本轮相近的纪事的关系，只有原话行，不设预算；一行都没有时显示占位语。

群聊席位有召回的原话行，但没有弧线、片段和纪事。

### 纪念日行

纪念日行放在 `[now]` 里生日行之后，每轮最多两行，初识的排在前面。它们读取这段关系会回归的 `real` 纪事行，并且和节日行一样需要请求带 `user_timezone`：

- `first_meet`，第 1–11 个月：只在每月的同一天（超出当月天数就取月末）当天出现：`今天是你们认识满 N 个月的日子。`
- `first_meet`，满 12 个月起：每年的这一天，提前六天开始出现：`今天` / `明天` / `N 天后是你们认识满 Y 年的日子。`
- 到期的 `promise` 或 `shared_plan`，当天出现：`今天是你们约好的日子：{summary}。`

### 亲密度门控

`memory_scope` 是上限。在它之内，这段关系当前的亲密度决定有多少内容进 prompt：一个跟用户还不熟的角色，不知道用户跟别的角色说过什么，也不会把用户说的话记得多清楚。门控读取时间衰减后亲密度的 bond 和 chemistry 两条线的档位（1–5，见 [affinity-model.zh.md](affinity-model.zh.md)）；没有亲密度记录的组合两条线都按第 1 档算。它只做减法——scope 排除的东西永远不会被加回来——并且作用在每一条注入路径上：文字轮次、群聊里的每个席位（用该席位自己的亲密度）、语音，以及改图请求生成的文字回复。

- **基础画像私密字段。** 每个字段只看一条线，并有这条线上的起始档；对照表在 [insights.zh.md](insights.zh.md#聊天-prompt)。
- **画像层。** K 跟着两条线里较高的那一档走：第 1 档完全不召回这一层，第 2 档取平常 K 的一半（向上取整），第 3 档及以上取平常的 K。按类别分组的召回和未分类兜底都照此执行。此外，`metadata.domain` 对应某个私密字段的记忆行，还要达到那个字段的档位：

  | `metadata.domain` | 线 | 起始档 | 对应字段 |
  |-------------------|----|--------|----------|
  | `sexuality`、`kink` | chemistry | 4 | `kinks` |
  | `romance` | chemistry | 2 | `love_values` |
  | `family` | bond | 3 | `family` |
  | `finance` | bond | 4 | `finance_status` |
  | `health`、`mental_health` | bond | 4 | `mental_health` |

  domain 是 `dreaming_scan` prompt 给每条记忆写下的标签（见 [model-config.zh.md](model-config.zh.md)），匹配时去掉首尾空白、不分大小写。其他取值、非字符串或没有 domain 的行只受 K 的限制；原文行没有 domain，所以也只受 K 限制。被扣下的行照样占它所在类别的检索名额：检索按平常的 K 只跑一次，门控再裁剪结果。
- **关系层。** 原话行的 K 是平常的 K，但不超过两条线里较高的那一档（文字：第 1 / 2 / 3+ 档分别 1 / 2 / 3 条；语音：1 / 2 条）。内容不过滤：这些是用户对这个角色说过的话。弧线、片段、纪事和纪念日行完全不受门控，在任何档位都会渲染。

### 语音轮次

语音端点（`POST /comp/voice/{session_id}/turn/stream`）读取同样的几层，但走一条更精简的路径——请求体见 [api-reference.zh.md](api-reference.zh.md#post-compvoicesession_idturnstream)，`[tasks.chat_voice]` 块见 [model-config.zh.md](model-config.zh.md)。

- **引导快照。** 只在 session 首轮组装一次，冻结进 `chat_sessions.metadata.voice_bootstrap`，此后每轮原样重新注入。分五部分：默认中性档的 `human_insights` bullets（档位可由首轮的 `memory_scope` 调整）；这段关系的开头和弧线，以 `[你们的故事]` 呈现，前提是它已有弧线、且首轮的 `memory_scope` 保留关系记忆；上一通语音通话最后 8 条消息组成的纯文字记录；通话读取关系线的那个文字 session 的余效（见[接口参考](api-reference.zh.md#get-v2compsessionsession_ideffects)）；以及通话开始时角色的需求。各部分各自独立降级，组装失败时标记不落，下一轮重试。快照本身不变，但每一轮都用当轮的档位让其中的基础画像 bullets 过一遍[亲密度门控](#亲密度门控)。
- **逐轮召回。** 与文字路径相同的 `companion_memories` 检索（受 `memory_scope` 门控，经 `memory_hygiene` 做跨层去重），但 K 小得多：分组画像每类 1 条、原始兜底 2 条、关系 2 条（文字路径是 2 / 4 / 3），等这对组合的亲密度（并行读取，和文字一样做时间衰减）返回后再由[亲密度门控](#亲密度门控)裁剪。整体包在 300 ms 预算里：超时或检索出错只丢掉这一轮的召回块，打一条 warn 日志，绝不是 error 帧。去掉空白和标点后不足 4 个字母数字字符的话语（嗯 / 好啊 / 哈哈）跳过召回，也不发 embedding 调用。语音的逐轮召回不检索纪事。`[tasks.chat_voice] recall = false`（默认 `true`）关闭召回，优先级高于请求的 `memory_scope`。
- **写入都经由 dreaming。** 语音轮次自己不写任何记忆：没有原文行，没有 insight 抽取，没有好感度评估。它计入所在关系的 [dreaming](#dreaming)，但从不就地扫描；关系闲置且待扫轮数至少 3 之后由清扫器扫描，文字轮次的就地扫描也会读到范围内的语音行。扫描把通话和这段关系的文字行放在一起读，像对文字一样写出画像记忆、片段和纪事；之后的通话和文字聊天都能召回它们。`DREAMING_VOICE_DISABLED=1` 把语音 session 排除在扫描之外，语音轮次也不再计数。游标按时间前进，所以开关打开期间被越过的语音行，关掉开关后也不会再读。
- **被打断的那轮留下什么。** 被打断的轮次存下的是客户端上报的「实际播出」文本，而不是完整生成内容，所以 dreaming 读到的是用户**听到**的东西。在第一个可听的词之前就被切断的轮次只存用户那一行，因为根本没有写入助手行。

## 归档与抹除

- **`DELETE /comp/instance/{instance_id}/sessions`** 归档这段关系：它的 session 标为已归档，关系层的 `companion_memories` 行，以及它在 `relationship_scan_state`、`relationship_episodes`、`relationship_arcs`、`relationship_journal` 里的行都被删除。画像记忆保留。同一个实例上重新开始的关系会有新的扫描状态和新的初识。
- **`POST /svc/v1/users/{user_id}/erase`** 删除用户在两层里的记忆，以及其角色实例在四张关系表里的行（[Insights → 抹除](insights.zh.md#抹除)）。
- `relationship_scan_events` 两种情况下都保留：它只有计数和 id。

## 设计文档

- [2026-10-28-memory-2-dreaming-plus-design.md](superpowers/specs/2026-10-28-memory-2-dreaming-plus-design.md)——dreaming、弧线、纪事及它们在 prompt 里的位置
- [2026-08-09-voice-memory-bootstrap-recall-design.md](superpowers/specs/2026-08-09-voice-memory-bootstrap-recall-design.md)——语音引导快照与逐轮召回
- [2026-08-09-voice-dreaming-ingestion-design.md](superpowers/specs/2026-08-09-voice-dreaming-ingestion-design.md)——语音行进入 dreaming

## 源码

- `crates/eros-engine-store/src/memory.rs`——`MemoryRepo`：`companion_memories` 的写入与检索
- `crates/eros-engine-store/src/scan_state.rs`、`episode.rs`、`arc.rs`、`journal.rs`、`scan_event.rs`——关系各表：游标与认领、片段与分段读取、弧线、纪事（含引擎自己写的行）、事件行
- `crates/eros-engine-store/src/human_insight.rs`——基础画像 bullets 背后的 `human_insights` 存储
- `crates/eros-engine-llm/src/voyage.rs`——Voyage 原生客户端
- `crates/eros-engine-llm/src/embedding.rs`——`EmbeddingRouter` + OpenRouter 兼容客户端
- `crates/eros-engine-server/src/pipeline/post_process.rs`——逐轮原文写入；把每个文字轮次计入 dreaming
- `crates/eros-engine-server/src/pipeline/dreaming/`——dreaming：触发与清扫器（`mod.rs`）、扫描与弧线改写（`scan.rs`）、回复解析与守卫（`parse.rs`）、对话记录（`render.rs`）
- `crates/eros-engine-server/src/relationship_context.rs`——为一轮加载弧线、片段和纪事，渲染 `[shared_memories]` 的关系部分
- `crates/eros-engine-server/src/anniversary.rs`——纪念日行
- `crates/eros-engine-server/src/backfill.rs`——`backfill-dreaming` 子命令
- `crates/eros-engine-server/src/pipeline/handlers.rs`——召回与 prompt 注入
- `crates/eros-engine-server/src/pipeline/voice.rs`——语音引导快照与逐轮召回
- `crates/eros-engine-store/migrations/0000_baseline.sql`——`companion_memories` 及其索引
- `crates/eros-engine-store/migrations/0082_relationship_memory.sql`——五张关系表
