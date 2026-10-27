# 洞察

[English](insights.md) · [中文](insights.zh.md)

洞察流水线从对话里建立引擎对真人用户的了解，分两层。**事实账本**（`engine.human_facts`）记录抽取调用从每一轮挖出的原子事实，每条不同的事实一行，附复现计数器。**画像**（`engine.human_insights`，每个用户一行）按计划从账本派生，不是逐轮更新；聊天 prompt、语音开场、画像端点和画像导出都读它。每次 LLM 调用都在 `engine.companion_insights_events` 留一行审计。用户的数据可以一次调用抹除，导出会把抹除传达给下游镜像。

按关系分开的 `character_insights` 与 `user_insights` 两条链是另外的流水线，各有自己的表，见 [llm-audit.zh.md](llm-audit.zh.md#角色画像与用户画像实验特性)。从 v2.3.1 或更早版本升级的部署，可在 [insights_legacy.md](insights_legacy.md)（仅英文）找到此前的两次调用流水线，升级步骤见 [migrating/v2-4-0-insights-2-0.md](migrating/v2-4-0-insights-2-0.md)。

```
用户消息 + 助手消息
   │
   ├─ insight_extraction   system = filter_prompt（部署自有）
   │   → {"facts": [{content, category, domain, …}]}      审计行  stage='facts'
   │
   ├─ 账本写入             engine.human_facts   （精确匹配去重，复现计数）
   │   └─ pending_facts += N      engine.human_profile_state
   │
   └─ pending_facts ≥ 20，或闲置满闲置时长（清扫器；默认一小时）
        └─ profile_rebuild   用户消息 = 整个账本 + 引擎自有的 schema
            → 整行替换 engine.human_insights         审计行  stage='profile'
```

## 事实账本

`engine.human_facts` 每个用户的每条不同事实占一行。账本只记录，不裁决：「用户单身」和之后的「用户有伴侣」是两行，各带自己的时间戳。判断当前什么为真，是派生画像和下游消费方的事。

| 列 | 含义 |
|----|------|
| `id` | 行 id。 |
| `user_id` | 谁的事实。无外键。 |
| `content` | 事实的一句话表述，保持首次出现时的写法。 |
| `category`、`domain`、`evidence_type`、`temporality`、`persistence`、`confidence` | 首次出现的六个标签；复现不会重新打标签。词表见[下文](#词表)。 |
| `occurrences` | 输出过这条事实的抽取运行次数。 |
| `session_count` | 见过它的 session 数（计数规则见下）。 |
| `first_seen_at`、`last_seen_at` | 首次与最近一次出现的时间。 |
| `first_session_id`、`first_message_id`、`last_session_id`、`last_message_id` | 首次与最近一次出现的来源。外键，`ON DELETE SET NULL`。 |
| `first_run_id`、`last_run_id` | 抽取运行的关联 id（即审计行的 `run_id`）。不是引用。 |
| `roleplay`、`content_hash` | 生成列：`evidence_type = 'roleplay_expression'`，以及 `md5(engine.fact_norm(content))`。 |

**去重。** 用户、证据类别和归一化后的文本三者相同才算同一条事实：在 `(user_id, roleplay, content_hash)` 上建唯一索引。`engine.fact_norm` 做 NFKC、转小写、去掉空白与标点（ASCII 和常见的 CJK 符号）。去重只做精确匹配，不做 embedding 或改写合并，所以否定、含糊、带条件或换了说法的变体各占一行。`roleplay_expression` 事实绝不与其他证据类型合并。

**一次出现 = 一次输出过该事实的抽取运行。** 1:1 一轮产出多条助手消息，每条消息各跑一次；群聊每轮跑一次。同一次运行的数组里的重复项只算一次，先出现的那条胜出。

**命中去重时**，该行保留原文本与标签，`occurrences` 加一；`last_seen_at` 与 `last_*` 指针移到新的运行上；只有新运行的 session 与已存的 `last_session_id` 不同时，`session_count` 才加一。这个近似在事实于两个 session 之间交替出现时会多计，但能精确判断「是否在两个及以上 session 出现过」，而档位只需要这一点。

**复现档位**由计数器派生，导出时作为 `recurrence`：

| 档位 | 条件 |
|------|------|
| `once` | `occurrences = 1` |
| `repeated_in_session` | `occurrences > 1` 且 `session_count = 1` |
| `repeated_across_sessions` | `session_count > 1` |

档位与 `persistence` 标签并用，不取代它：`persistence` 能区分「此刻在地铁上」和长期特质，计数器做不到。

## 事实抽取

`[tasks.insight_extraction]` 提供模型、fallback、temperature、`max_tokens` 和采样参数。它的 `filter_prompt` 是 system 消息，归部署所有；引擎没有内置的抽取 prompt。用户消息由 `prompt::facts_user_message` 构造：

```
用户: {user message}
AI:   {assistant message}
```

### 何时运行

它属于 post-process，在回复之后 spawn，从不阻塞回复。`memory_scope` 只管 prompt 注入，不管抽取。

- **1:1 文本轮**（`post_process::run`）：每条非空文本的助手消息各跑一次，把用户消息与该条消息的文本配对。用户这一轮带 action 时，用户文本前会加上 action 标记。
- **群聊轮**（`post_process::run_group`）：每轮跑一次。助手一侧是所有非空回复，写成 `{speaker name}：{reply}` 的行。见 [group-chat.zh.md](group-chat.zh.md#进入-11-的内容)。
- **语音轮**从不运行。
- session 已归档或用户文本为空白时，这次运行被跳过。

### 输出契约

回复是一个 JSON 对象，其中 `facts` 是对象数组：`content` 加六个标签。自带的 `examples/model_config.toml` prompt 给出了确切形状，并要求至多 12 条、没有新内容时输出 `{"facts":[]}`、只写关于真人用户的事实（不写 AI 的台词或人设）、角色扮演表达标为 `roleplay_expression`、不写任何地理信息和第三方姓名。

引擎对回复做的处理：

- 按 JSON 解析，先解析整段文本，再解析第一个平衡的 `{…}` 块。解析不出 JSON，这次运行结束（审计状态 `parse_error`）。
- 把 `facts` 当数组读，保留是对象、且 `content` 既非空白也非占位串（`string`、`null`、`unknown`、`n/a`、`未知`、`无`，或 `<…>` / `「<…>」` 形状的槽位）的项。其余任何项都被忽略，只要忽略了任何一项，这次运行就记录一条带数量的警告。仍停留在字符串数组契约的 prompt 因此不会往账本写任何东西，并且每次运行都告警。
- 把标签收敛进下面的词表；缺失或不在词表里的标签从不拒绝一条事实。
- 没有可用事实时结束这次运行（审计状态 `empty`）。
- 用一条语句把可用事实写入账本，所以写入失败不改变任何东西，重试也不会把同一条事实计两次。

### 词表

词表归引擎所有，每个标签都对照它校验。`content` 会 trim；`content` 为空白或占位串的项被跳过。

| 字段 | 取值 | 兜底 |
|------|------|------|
| `evidence_type` | `explicit_statement`、`explicit_agreement`、`observed_behavior`、`roleplay_expression`、`legacy_profile`、`unknown` | `unknown` |
| `category` | `fact`、`preference`、`aversion`、`goal`、`value`、`habit`、`skill`、`event`、`emotion`、`relation`、`communication_style`、`roleplay_tendency`、`constraint`、`other` | `other` |
| `domain` | `basic_profile`、`career`、`education`、`technology`、`finance`、`family`、`friendship`、`romance`、`sexuality`、`kink`、`health`、`mental_health`、`lifestyle`、`hobby`、`ideology`、`personality`、`communication`、`other` | `other` |
| `temporality` | `atemporal`、`current`、`past`、`future`、`unknown` | `unknown` |
| `persistence` | `stable`、`ongoing`、`transient`、`one_off`、`unknown` | `unknown` |
| `confidence` | `high`、`medium`、`low`、`unknown` | `unknown` |

`domain` 里出现的 `politics` 和 `religion` 映射到 `ideology`；其他不在词表里的值（包括 `location_culture`）都变成 `other`。`legacy_profile` 标记升级时由 2.0 之前的画像值播种出的事实，示例 prompt 不会提供它。prompt 用了比这些列表更丰富的词表的部署，会把多出来的粒度损失成 `other` 和 `unknown`；这是设计如此，不是错误。

## 派生画像

`engine.human_insights` 每个用户一行，以 `user_id` 为键（无外键）。画像对用户是全局的：事实来自用户聊过的每一个角色，没有任何一列记录哪个角色听到了什么。只有重建会写它，别的都不写。

| 列 | 类型 | 含义 |
|----|------|------|
| `occupation` | `TEXT` | 职业与工作状态，含行业、职级或日常细节。 |
| `mbti_guess` | `TEXT` | 自报的类型原样填；否则是谨慎的推测，措辞上带推测语气，依据是账本里反复出现的模式。 |
| `love_values` | `TEXT` | 对恋爱关系的态度与期待；至多三条命题。 |
| `emotional_needs` | `TEXT` | 需要什么样的情感支持；至多三条命题。 |
| `life_rhythm` | `TEXT` | 作息与日常节奏的具体模式。 |
| `interests` | `TEXT[]` | 兴趣爱好；每项是带实际细节的具体短语。 |
| `personality_traits` | `TEXT[]` | 性格特质；每项是有情境依据的具体短语。 |
| `deal_breakers` | `TEXT[]` | 现实生活中无法接受的点；对 AI 说话风格的不满、剧情里的即时反应不算。 |
| `education` | `TEXT` | 学校、专业、在读或已毕业。 |
| `education_level` | `TEXT` | `secondary_or_below`、`associate`、`bachelor`、`master`、`doctorate` 之一。 |
| `education_status` | `TEXT` | `enrolled` 或 `graduated`。 |
| `family` | `TEXT` | 家庭结构：婚育状况、成员、关系。 |
| `relationship_history` | `TEXT` | 过往感情：上一段怎么结束、单身多久。 |
| `social_pattern` | `TEXT` | 独处或聚会的倾向、线上线下的习惯。 |
| `future_plans` | `TEXT` | 近期目标与人生方向。 |
| `finance_status` | `TEXT` | 收入线索与消费习惯；仅在用户说过时才写。 |
| `kinks` | `TEXT[]` | 具体的玩法与角色设定；来自角色扮演的项在短语里写明。 |
| `ideology` | `TEXT` | 政治、宗教与社会价值立场；依据行为推断时措辞留有余地，绝不依据用户是谁。 |
| `mental_health` | `TEXT` | 仅限用户自述，不做诊断。 |

共 19 个字段（`education_level` 与 `education_status` 算两个），外加 `user_id` 和 `updated_at`。未知即缺省：空字段是 `NULL`（或 `[]`），从不存占位值。`city`、`location`、`hometown`、`nationality`、`preferred_gender`、`age_min`、`age_max` 已退役：引擎不再写、不再渲染、不再导出它们，这几列留在表里无人读取，直到后续版本删掉。需要位置、性别偏好或年龄范围来做匹配的部署，应直接向用户收集。

索引：`interests` 和 `personality_traits` 上的 GIN，以及 `(updated_at, user_id)` 上的 btree（导出的 keyset）。启用行级安全且不设 policy，`anon` 与 `authenticated` 角色（存在时）被撤销权限。

### 状态与触发

`engine.human_profile_state` 为每个有事实的用户保留一行：

| 列 | 含义 |
|----|------|
| `pending_facts` | 自上次落定的重建以来写入或累加的账本事实数。 |
| `last_fact_at` | 计数器上次变动的时间（或失败上次把它往后推的时间）。 |
| `last_rebuilt_at` | 上次重建落定的时间。 |
| `rebuild_failures` | 连续失败的重建次数。 |
| `claimed_at` | 重建进行中时设置。 |

每次抽取运行写入或累加了 N ≥ 1 条账本行之后，`pending_facts` 增加 N，`last_fact_at` 设为当前时间。重建有两种启动方式：

- **计数。** 一次运行之后 `pending_facts` 达到 20，该运行的 post-process 任务就地重建。它本来就在回复路径之外。
- **闲置清扫。** 清扫器每隔 `PROFILE_REBUILD_TICK_SECS` 醒来，认领至多 `PROFILE_REBUILD_BATCH` 个 `pending_facts > 0` 且 `last_fact_at` 早于 `PROFILE_REBUILD_IDLE_SECS` 的用户，等得最久的优先，然后逐个重建。清扫器关闭时，只有计数触发。

重建先认领状态行，所以重叠的轮次与清扫器不会把同一个用户重建两次。超过 600 秒的认领被视为已死亡的 worker 留下的，可以被再次认领；清扫器在每个用户重建前刷新其认领，所以一个长批次不会把自己批尾的认领拖到可被再次认领。

### 重建读什么

只读该用户的整个账本，别的什么都不读：**绝不读上一版画像。** 改写旧画像会让输出膨胀、被截断，而账本已经包含画像所概括的一切。prompt（引擎自有，`prompt.rs`）是一条 user 消息，内含渲染后的账本和 `PROFILE_REBUILD_SCHEMA`。

账本渲染成 `## <domain>` 分组，每条事实一行。行按 `occurrences` 降序、再按 `last_seen_at` 降序排名，依次放入，直到整段文本（含分组标题）达到 **16,000 个字符**。第一条放不下的事实结束遍历，所以读到的永远是账本按排名的前缀；其余的留在账本里，只是不进这一次重建。分组按各自第一条事实被放入的顺序出现。每行包含事实、证据类型、持久性（`unknown` 时省略）、事实出现不止一次时的复现档位及计数，以及最近出现的日期：

```
## hobby
- 爱爬山（证据: explicit_statement；持续性: stable；复现: repeated_across_sessions，×3，2 个会话；最近 2026-10-21）
```

标签按原样的代码打印，因为 prompt 的规则里引用的就是这些代码。

### 重建写什么

模型返回一个带有上述字段的 JSON 对象（schema 里只有 `<…>` 占位，没有可能被抄回去的示例值）。prompt 带的规则：

- 只填账本清楚支持的字段，其余省略。
- `transient` 事实不进任何字段，除非它跨 session 反复出现。
- `roleplay_expression` 事实只能进 `kinks`，并在短语里标明来自角色扮演；绝不进入现实生活的字段。
- `mental_health` 只记用户自述；`ideology` 可以依据事实里表现出的行为，措辞留有余地。
- `education_level` 是对象 `{"level", "status"}`；没有可写的内容时输出 `{}`。

写入之前，引擎把对象投影到各列，丢掉不携带信息的内容：标量必须是 JSON 字符串，数组只保留其中的字符串项，空白、`string`、`null`、`unknown`、`N/A`（不分大小写）、`未知`、`无`，或没填的 `<…>` / `「<…>」` 槽位，都被丢弃。`education_level.level` 与 `.status` 各自只在是上表中的代码时才保留。类型不对的值和未知的键被忽略。

写入是一条语句**整体替换** 19 个字段：重建里省略的字段变空，`updated_at` 随之移动，这正是该行出现在 `GET /svc/v1/insights/changes` 里的原因。七个已退役的列不被触碰。

### 结果

| 结果 | 何时 | 画像行 | 计数器 |
|------|------|--------|--------|
| `ok` | 对象至少投影出一个字段，且写入成功。 | 被替换。 | `pending_facts = 0`，`last_rebuilt_at = now`，`rebuild_failures = 0`。 |
| `empty` | 用户没有账本行（不发起调用），或回复是没有可写内容的 JSON 对象（包括 `{}`）。 | 不动。 | 与 `ok` 一样落定：这批事实作为残余被接受，用户的下一批事实重新武装触发器。 |
| `failed` | 账本读取失败、调用没有返回回复、回复不是 JSON 对象，或写入失败。 | 不动；旧行继续服务。 | 保留 `pending_facts`，并设 `last_fact_at = now`，于是闲置清扫再等一个完整的闲置时长（`PROFILE_REBUILD_IDLE_SECS`，默认一小时）；`rebuild_failures` 加一。第三次连续失败则改为把 `pending_facts` 与 `rebuild_failures` 清零。 |

所以账本无法重建的用户，每一波活动至多花三次调用，绝不会每个 tick 一次。无论哪种结果，认领都会释放。

## 读取方

### 聊天 prompt

回复 handler 读取该行，用 `human_insights_to_bullets` 渲染，标签为 `基础画像`，作为 `[user_profile]` 段的第一组，排在记忆类别分组之前。渲染多少取决于请求的 `memory_scope`，经 `InsightMode` 映射：

| `memory_scope` | `InsightMode` |
|----------------|---------------|
| `full` | Full |
| `neutral_and_relationship`（默认） | Neutral |
| `relationship_only` | Off |
| `neutral_only` | Neutral |
| `insights_only` | Full |
| `none` | Off |

Off 什么都不读。渲染器只对非空白的值输出 bullet：

| 档位 | 字段（标签） |
|------|--------------|
| Neutral 与 Full | `occupation`（`职业`）、`education`（`教育`）、`education_level` 加 `education_status`（`学历`，如 `学历：本科·在读`）、`mbti_guess`（`MBTI`）、`ideology`（`意识形态`）、`life_rhythm`（`作息`）、`social_pattern`（`社交模式`）、`personality_traits`（`性格特质`）、`future_plans`（`未来计划`） |
| 仅 Full（私密字段） | `love_values`（`感情观`）、`relationship_history`（`感情经历`）、`interests`（`兴趣`）、`emotional_needs`（`情感需求`）、`family`（`家庭`）、`finance_status`（`经济状况`）、`kinks`（`性癖`）、`mental_health`（`心理健康`） |
| 从不渲染 | `deal_breakers` |

一条 bullet 是 `{label}：{value}`，数组值用 `、` 连接。顺序固定：`职业`、`教育`、`学历`、`MBTI`、`意识形态`，然后是仅 Full 的字段（按上表顺序），再是 `作息`、`社交模式`、`性格特质`、`未来计划`。读取失败只记日志，不产生 bullet。见 [memory-layers.zh.md](memory-layers.zh.md#检索与注入)。

### 语音

语音 session 首轮，引导快照用同一个函数渲染同样的 bullet，放在 `[关于他]` 下。档位取自首轮的 `memory_scope`（省略时为 Neutral），快照冻结进 `chat_sessions.metadata.voice_bootstrap`，所以之后该行的变化不会进入这通通话。读取失败不会被吞掉：快照不冻结，下一轮重试。见 [memory-layers.zh.md](memory-layers.zh.md#语音轮次)。

### 画像端点

`GET /comp/user/{user_id}/profile` 把该行作为 `ProfileResponse` 返回：`user_id`、19 个字段（全部，包括三个敏感字段，因为这是用户自己的数据）和 `updated_at`。路径里的 `user_id` 必须等于 JWT 的用户（否则 403）。没有该行的用户得到 200，`updated_at: null`，标量为 `null`，数组为 `[]`。见 [api-reference.zh.md](api-reference.zh.md#get-compuseruser_idprofile)。

### 每日快照

`pipeline::snapshot::sweeper` 按 cron 计划运行（默认 `Asia/Singapore` 的 23:00），为每个 `human_insights` 行向 `engine.human_insights_snapshot` 插入一行，内容是整行的 JSONB，同一次触发的所有行共用一个 `captured_at`。没有 LLM 调用、不去重、不过滤。由 `SNAPSHOT_DISABLED`、`SNAPSHOT_CRON`、`SNAPSHOT_TZ` 配置；见 [deploying.zh.md](deploying.zh.md#運維注意事項)。

## 导出与删除

`/svc/v1/*` 下的三棵子树，每棵只在自己的 secret 设置后才挂载，所以消费方只持有它所读取的流对应的凭据。准入用常量时间比较请求头与 secret；缺失或错误是 403，且先于查询串校验。secret 未设置的子树，响应与任何未知路径一致（既不是 403，也不是 200）。secret 会 trim，空白视为未设置，少于 32 字节则拒绝启动。

| Secret | 请求头 | 路由 |
|--------|--------|------|
| `INSIGHTS_EXPORT_SECRET` | `x-insights-export-secret` | `GET /svc/v1/insights/changes`、`GET /svc/v1/memories/changes`、`GET /svc/v1/insights/deletions` |
| `FACTS_EXPORT_SECRET` | `x-facts-export-secret` | `GET /svc/v1/facts/changes`、`GET /svc/v1/facts/deletions` |
| `USER_ERASE_SECRET` | `x-user-erase-secret` | `POST /svc/v1/users/{user_id}/erase` |

所有流都用 keyset 分页：`since`、一个平局 id 和 `limit`（默认 500，夹在 1..=1000）；`next` 仅在页满时出现，`null` 表示遍历结束。请求与响应形状及轮询建议见 [api-reference.zh.md](api-reference.zh.md#服務端點)。

- **画像导出。** `GET /svc/v1/insights/changes` 按 `(updated_at, user_id)` 遍历 `human_insights`。一行包含 `user_id`、`updated_at` 和除 `kinks`、`ideology`、`mental_health` 之外的每个画像字段：**这三个字段从不导出。**
- **事实导出。** `GET /svc/v1/facts/changes` 按 `(last_seen_at, id)` 遍历账本。一行包含 `id`、`user_id`、`content`、六个标签、`occurrences`、`session_count`、`recurrence`、`first_seen_at` 和 `last_seen_at`；session、消息和 run 的指针留在引擎内部。一次复现会移动 `last_seen_at`，所以每当计数器变化，该行就会在流里再出现一次。`first_seen_at` 与 `last_seen_at` 由写账本的那条语句自己打上（`now()`，复现时 `last_seen_at` 只进不退），时间戳只比提交早一条短语句，所以续接的轮询方重叠几秒即可。
- **删除。** `GET /svc/v1/insights/deletions` 与 `GET /svc/v1/facts/deletions` 是同一条流，每个 secret 一条路由：来自 `engine.user_erasures` 的用户级墓碑 `{user_id, erased_at}`，按 `(erased_at, user_id)` keyset 分页。没有逐条事实的删除。**镜像画像、记忆或事实的消费方必须遵守这条流**：抹除会删掉它所镜像的行，而任何 changes 流都无法报告一行已经不存在。
- **记忆导出。** `GET /svc/v1/memories/changes` 不变，自己不带删除信息；它的镜像通过 `insights/deletions` 得知哪些用户被抹除。

## 抹除

`POST /svc/v1/users/{user_id}/erase` 只在设置了 `USER_ERASE_SECRET` 时挂载，删除 `engine.*` 里引擎关于该用户派生出的一切。它与导出 secret 分开，这样即使部署从不启用任何导出，抹除也能用。一次调用在一个事务里完成：先删除，再清空，最后写墓碑。

**删除**（行被移除）：

| 表 | 范围 |
|----|------|
| `human_profile_state`（最先删：它是把抹除与进行中的重建写入串行化的那把锁）、`human_facts`、`human_insights`、`human_insights_snapshot`、`companion_insights_snapshot` | `user_id` |
| `companion_memories` | `user_id`（画像层与关系层） |
| `world_memories`、`world_states`、`world_enrollments`、`world_posts`（评论随帖子一起走）、`persona_story_insights`、`persona_story_memories`、`persona_story_events` | `owner_uid` |
| `user_insights`、`user_insights_snapshot`、`character_insights`、`character_insights_snapshot` | 该用户的角色实例（`persona_instances.owner_uid`） |

**清空**（行保留 id 和时间戳，内容清除）：

| 表 | 清除的内容 |
|----|-----------|
| `companion_insights_events` | `payload` |
| `companion_decision_events` | `payload` |
| `chat_images_events` | `inputs`、`subject`、`caption`、`composed_prompt`、`last_failure` |
| `chat_vision_events` | `vision`、`image_url`、`last_failure` |
| `user_insights_events`、`character_insights_events` | `payload`（通过该用户的角色实例限定范围） |
| `companion_affinity` | `feeling_clause`、`feeling_clause_at`；数值是关系，不是人 |
| `companion_affinity_events` | `context` |

**写入：** 向 `engine.user_erasures (user_id, erased_at)` upsert 一行，它为两条删除流供数据。

响应是 `{user_id, erased_at, deleted: {table: n}, blanked: {table: n}}`；上面每张表都会列出，没有可做的就是 0。`blanked` 只统计仍然带有内容的行。

- **幂等。** 重复调用什么都找不到，各处报 0，并把 `erased_at` 往前移，让错过第一次的镜像在删除流上再次收到通知。
- **顺序。** 在删除该用户的 session 或角色实例**之前**调用。几条 insight 链只以实例为键，所以只有实例行还在时才够得着它们。
- **先静默流量。** 已经在进行中的第一段抽取可能在抹除提交之后写入事实；调用前先停掉该用户的流量，若仍有漏网，再调用一次清理。进行中的重建写不回画像：它在状态行的锁下写入，而抹除最先拿这把锁，之后它会发现用户已不在。
- **不触碰。** 聊天历史（`chat_messages`、session、实例）属于调用方自己的删除流程。

之后新增的、带用户归属的表，会在新增它的那次改动里加入抹除。

## 审计

每次返回了响应的 LLM 调用都向 `engine.companion_insights_events` 写一行，只追加。

| 列 | 含义 |
|----|------|
| `run_id` | 每次抽取运行一个，每次重建新开一个。它同时是该运行触及的账本行的 `first_run_id` / `last_run_id`。 |
| `user_id` | 这次运行是为谁的画像。无外键。 |
| `session_id`、`message_id` | session 与所产出的助手消息（群聊取该轮最后一条）。`profile` 行上为 `NULL`：重建不是一个轮次。外键，`ON DELETE SET NULL`。 |
| `stage` | `facts` 或 `profile`。 |
| `status` | `ok`、`empty` 或 `parse_error`。 |
| `payload` | 见下。 |
| `generation_id` | 连到 `engine.llm_generations` 的 join key，`ON DELETE SET NULL`。账本写入降级时为 `NULL`。 |

| `stage` | `status` | `payload` |
|---------|----------|-----------|
| `facts` | `ok` | `{"facts": [...]}`：数组原样保留模型输出的内容，包括引擎忽略的项。 |
| `facts` | `empty` | 同样形状，但没有可用事实。 |
| `facts` | `parse_error` | `NULL`。 |
| `profile` | `ok` | 模型返回的 JSON 对象原样保留，包括 schema 之外的键。 |
| `profile` | `empty` | 模型的 JSON 对象，其中没有可写内容。 |
| `profile` | `parse_error` | `{"raw": "<回复的前 2000 个字符>"}`。 |

`stage = 'structured'` 的行（以及形如 `{"facts": [...], "details": [...]}` 的 `facts` payload）是此前流水线留下的历史，现在没有任何东西再写它们。传输错误不写行；账本为空的用户触发重建时，不发起调用，也不写行。

模型和 token 用量不在事件行上。抽取调用在 `llm_generations` 里记为任务 `insight_extraction`，重建记为 `profile_rebuild`，其 `session_id` 为 `NULL`：

```sql
SELECT e.run_id, e.stage, e.status, g.task, g.model, g.usage
FROM engine.companion_insights_events e
LEFT JOIN engine.llm_generations g ON g.generation_id = e.generation_id
WHERE e.user_id = $1
ORDER BY e.created_at DESC, e.stage;
```

见 [llm-audit.zh.md](llm-audit.zh.md#llm-生成记录表)。payload 原样保存关于用户的抽取事实和画像值；抹除会把它们清空。

## 配置

| 设置 | 效果 |
|------|------|
| 存在 `[tasks.insight_extraction]` | 抽取运行。`filter_prompt` 必填且必须是普通字符串：空白拒绝启动，变体形状（数组或表）也拒绝启动。 |
| 没有 `[tasks.insight_extraction]` | 抽取关闭：无调用、无审计行、不写账本。已有的行仍由读取方提供，已排队的重建仍会运行。 |
| 存在 `[tasks.profile_rebuild]` | 提供重建的 `model`、`fallback`、`temperature`、`max_tokens` 与采样参数。任何形状的 `filter_prompt`（包括空白）都拒绝启动：prompt 归引擎所有。 |
| 没有 `[tasks.profile_rebuild]` | 重建用 `[tasks.insight_extraction]` 的参数运行，不用全局默认值。两者都没有时落到全局默认值。 |
| 存在 `[tasks.insight_structuring]` | 被忽略。 |
| `[tasks.profile_rebuild]` 下的 tier 块 | 拒绝启动。 |
| 用 `tasks` 限定范围的 `[[providers.<name>.body]]` 规则 | 按任务名精确匹配：抽取是 `insight_extraction`，重建是 `profile_rebuild`。 |

| 变量 | 默认 | 效果 |
|------|------|------|
| `PROFILE_REBUILD_DISABLED` | 未设置 | `1` 或 `true` 停掉闲置清扫器。计数触发仍会重建。 |
| `PROFILE_REBUILD_TICK_SECS` | `300` | 清扫间隔。`0` 同样停掉清扫器；无法解析的值保持默认。 |
| `PROFILE_REBUILD_IDLE_SECS` | `3600` | 用户最后一条新事实要放多久，清扫器才重建。 |
| `PROFILE_REBUILD_BATCH` | `20` | 每个 tick 重建的用户数，至少为 1。 |
| `INSIGHTS_EXPORT_SECRET`、`FACTS_EXPORT_SECRET`、`USER_ERASE_SECRET` | 未设置 | 挂载各自的子树，见[上文](#导出与删除)。 |
| `SNAPSHOT_DISABLED`、`SNAPSHOT_CRON`、`SNAPSHOT_TZ` | | 每日快照。 |

计数阈值（20）和 600 秒的认领过期时间是常量，不是设置。见 [model-config.zh.md](model-config.zh.md#任务名) 与 [deploying.zh.md](deploying.zh.md#運維注意事項)。

## 运维须知

- **升级会回填账本，并为每个画像行排一次重建。** 回填迁移以 Postgres `NOTICE` 报告计数，形如 `insights_2_backfill: events=<n> seeds=<n> pending=<n>`。`events` 是由审计事件建出的账本行数；`seeds` 是由当前画像值播种的行数（证据类型 `legacy_profile`）；`pending` 是排队等重建的用户数。只有当前有画像行的用户会被处理，所以没有画像行的部署什么都不回填。
- **重建排空的成本是每个排队用户一次 LLM 调用**，所以部署之后预计有 `pending` 次调用，由闲置清扫器按每 `PROFILE_REBUILD_TICK_SECS` 处理 `PROFILE_REBUILD_BATCH` 个用户的节奏排空。在用户首次重建之前，他原来的画像行继续服务注入。清扫器关闭时，每个排队用户会在他下一次抽取运行时被重建，因为队列是按计数阈值播下的。请在部署前为 `[tasks.profile_rebuild]` 定好规格：抽取已关闭且没有 `[tasks.profile_rebuild]` 块时，排空会跑在全局默认模型上。`PROFILE_REBUILD_DISABLED=1` 可以阻止排空运行，而抽取关闭时再没有别的东西会触发重建。
- **领域重分类归部署负责。** 回填的行保留旧的 `sexuality` 与 `health` 领域；拆出 `kink` 与 `mental_health` 只作用于新事实。引擎不带重分类器：想把旧行拆开的运维方，自己跑一遍离线流程去更新 `engine.human_facts.domain`，除 `UPDATE` 权限外不需要别的。重新打标签不会排队重建；更正后的领域会在该用户下一次重建时被读到。要手动排一次，把该用户 `human_profile_state` 行的 `pending_facts` 设为大于 0，并把 `last_fact_at` 设成早于闲置时长。
- **镜像画像、记忆或事实的部署必须消费删除流**，并且在删除用户的 session 之前先调用抹除。

## 已知限制

- 账本把改写后的说法保留为不同的行。计数以抽取运行为单位，而一轮产出多条助手消息时，会针对同一条用户消息按每条消息各跑一次抽取，所以一句话可能被计多次、读成 `repeated_in_session`。
- `session_count` 在事实于两个 session 之间交替出现时可能多计。
- 一条事实始终保留首次出现时的标签，即使之后的出现会给它打别的标签。
- 画像的新鲜度等于它上一次重建：最后一条新事实之后，最长可滞后一个闲置时长。
- 账本超过 16,000 字符的预算时，每次重建只读它排名靠前的前缀。
- 重建返回了没有可写内容的结果时，这批事实作为残余被接受，不会重试。
- 没有逐条事实的删除；抹除以用户为单位。
- 没有任何代码路径清理 `engine.human_insights_snapshot` 或 `engine.companion_insights_events`。

## 源码

- `crates/eros-engine-store/src/human_fact.rs` —— 词表、`FactInput`、`HumanFactRepo::upsert_run`、复现档位
- `crates/eros-engine-store/src/profile_state.rs` —— `REBUILD_K`、`MAX_REBUILD_FAILURES`、`bump`、`claim`、`claim_due`、`finish_ok`、`finish_failed`
- `crates/eros-engine-store/src/human_insight.rs` —— 19 个字段、`project_profile`、`is_placeholder`、`replace_profile`、`list_changes`
- `crates/eros-engine-store/src/erasure.rs` —— `erase_user`、删除与清空的表清单、删除流
- `crates/eros-engine-store/src/insight.rs` —— `InsightEventRepo`
- `crates/eros-engine-server/src/pipeline/post_process.rs` —— `extract_insights`、`extract_facts`、`parse_fact_objects`、`record_facts`
- `crates/eros-engine-server/src/pipeline/profile_rebuild.rs` —— `render_ledger`、`rebuild_user`、清扫器、`LEDGER_BUDGET_CHARS`
- `crates/eros-engine-server/src/prompt.rs` —— `facts_user_message`、`PROFILE_REBUILD_SCHEMA`、`profile_rebuild_prompt`
- `crates/eros-engine-server/src/pipeline/handlers.rs` —— `human_insights_to_bullets`
- `crates/eros-engine-server/src/pipeline/voice.rs` —— 引导快照
- `crates/eros-engine-server/src/pipeline/snapshot.rs` —— 快照清扫器
- `crates/eros-engine-server/src/routes/svc.rs` —— 三棵 `/svc/v1/*` 子树；`routes/mod.rs` 按各自的 secret 挂载
- `crates/eros-engine-server/src/routes/companion.rs` —— `GET /comp/user/{user_id}/profile`
- `crates/eros-engine-llm/src/model_config.rs` —— `resolve_insight_extract`、`resolve_profile_rebuild`、`validate_structuring_prompt_unset`
- `crates/eros-engine-store/migrations/0079_human_facts.sql`、`0080_human_facts_backfill.sql`
- `examples/model_config.toml` —— `[tasks.insight_extraction]` 与 `[tasks.profile_rebuild]`
- 设计：[2026-10-26-insights-2-fact-ledger-design.md](superpowers/specs/2026-10-26-insights-2-fact-ledger-design.md)
