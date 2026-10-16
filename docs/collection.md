# 采集行为

## 1. 采集对象与边界

本文定义 Prompt 审计“记录哪些最新输入、带上哪些上文、任务如何写入”。它是采集规则的独立维护入口；PG、插件接入和 UI 见[总体设计](design.md)。

一条审计记录描述：某个 Client Key 的请求中，网关观察到的最后一个输入项中的可采集用户消息，以及该请求携带的有限前序对话。目标是帮助审阅用户意图，例如将“请继续”与 Agent 此前的方案一起展示。

观察边界是网关 `request` 中间件，不是客户端输入框。客户端尚未发出的输入、到达该阶段之前就被拒绝的请求、客户端在发送前移除的历史，都不在插件可见范围内。页面将时间称为“记录时间”，其值为本次请求经过采集边界时的时间，存储字段沿用 `first_observed_at`。相同请求重传会分别记录，各自使用本次观察时间；该时间不等于客户端提交时间，也不等于异步写入数据库完成的时间。

最后一个输入项是可采集的用户消息时才提交审计任务，也允许保留来源尚不明确的 `user` 消息。请求用途与来源用于决定是否采集，判定后不将这些字段或分类快照落库。已识别的子代理请求整体排除。插件不保存完整请求转储、工具过程或推理轨迹。

## 2. 采集阶段

选择 `request` 中间件作为输入采集点，在调用 `next` 之前读取该阶段可见的原始输入和会话元数据，将审计任务提交到有界环形缓冲区后继续请求。提交不等待数据库写入或写入确认。宿主已完成入口认证及初步解析，可以取得 Client Key ID；具体 Provider/account 的执行尝试尚未开始。[HTTP 入口][http-entry]、[执行入口][execution]

插件仅以只读观察构建审计记录；过滤与正文片段选择均作用于审计副本，传给 `next` 的请求 headers、正文和执行设置保持原样。整条请求排除只决定是否提交审计任务，原请求仍调用一次 `next`。

| 阶段 | 与本设计的关系 |
| --- | --- |
| `http` / `websocket` 原始入口 | 更接近传输层，处于业务认证或消息解析之前，不作为首期审计主入口 |
| `request` 的入站部分 | 识别来源，检查最后一个输入项，构造并提交审计任务 |
| `attempt` | 可能因上游重试执行多次，不作为用户输入的采集单位 |
| `request` 的响应返回链及 `request_completed` | 不参与采集；后续失败、取消或未发送的请求也可能留下审计记录 |

宿主 WebSocket 在进入 request 中间件时仍可保留客户端 `response.create` 的增量正文，历史重放准备发生在后续链中。因此不能假设该阶段已经得到完整会话。`generate=false` 的主会话预热请求也可能携带用户输入，而正式生成时只发送增量；预热不是丢弃该输入的理由。[WebSocket 入口][ws-entry]、[Codex 增量请求][incremental]

审计插件安排在其他 request 输入改写插件之前；更外层 HTTP/WebSocket 插件仍可能已经改变正文，所以“原始输入”仅指本边界实际看到的内容。

## 3. 来源判定与子代理过滤

### 3.1 解析元数据

首先读取请求正文 `client_metadata["x-codex-turn-metadata"]`。它是一个包含 JSON 的**字符串**，应再次解析为结构化元数据。官方 Codex 将它定义为规范来源，flat `client_metadata` 字段与 HTTP headers 是兼容投影。[元数据合同][metadata]

解析时只取当次来源判定及已确定审计字段所需的部分。`thread_source`、`subagent_kind`、`request_kind`、`turn_trigger` 和兼容子代理标记只在本次采集判定中使用，不持久化请求用途或来源；不保存原始元数据 JSON。长期元数据限定为 `thread_id`、`session_id`、本条消息的 `turn_id`、逐块输入类型，以及下述仓库根路径与 remote URLs。

优先使用当前请求正文；规范对象不可用时，使用已知的 flat 字段及对应 headers。WebSocket 每个 `response.create` 的正文优先于握手 headers，不能让建立连接时的来源覆盖当前消息的来源。冲突不被静默解释成“确认主会话”，但不另存元数据来源标签。

仓库取规范对象的 `workspaces`：每个键是客户端报告的仓库根路径，每个值仅提取 `associated_remote_urls` 中的 remote 名称与 URL。Git 信息异步补充，早期请求可能没有这一字段；缺失就留空，不从输入正文、cwd 或其他请求猜测，也不补采 commit hash、`has_changes`。URL 入库前去除认证凭据，保留仓库定位语义，不访问远端。这份快照与记录的来源 request ID 关联，不能将当前仓库状态解释为每条历史输入编写时的状态。[工作区字段][workspace-fields]、[Git 补充][git-enrichment]

UA、模型和 reasoning effort 仅在管理员点击“加载请求信息”后由查询层读取宿主已有请求记录，采集器不保存副本，普通详情与正文读取也不查询这三项，见[总体设计 §4.3](design.md#43-逻辑数据模型)。其余不采集的请求元数据包括客户端时间、独立的客户端版本或 originator、安装标识、分叉谱系、上下文窗口标识、请求计时、工具目录与 MCP 归因、自定义元数据、执行权限和 service tier。`reasoning.summary` 是请求中的推理摘要配置，摘要正文属于模型输出；两者均不采集。

### 3.2 整条请求过滤

来源分类发生在生成审计任务之前，用于排除非用户发起的内部任务审计；用户请求携带生成的上下文，不因此成为内部任务：

采集器通过同一个 `ExclusionPolicy` 判定内建来源规则、自动提示规则与管理员配置的 UA 正则；通过请求级判定后才构建审计任务。请求级排除与 §4 的内容块过滤具有不同范围：消息中的通知块可被剔除，同一消息中的真实用户输入仍可采集。

| 来源证据 | 采集行为 |
| --- | --- |
| `subagent_kind`，或明确的 `thread_source=subagent` | 排除该请求 |
| `x-openai-subagent` 兼容标记 | 排除对应的子代理或内部任务请求，不只匹配 `collab_spawn` |
| 明确的内部任务来源，例如 guardian review、memory consolidation，或 `request_kind=compaction` / `memory`；Codex recap 使用的 `thread_source=system`、隐藏标题生成使用的 `thread_source=thread_title` | 不作为用户输入，也不补入主会话 Agent 上文 |
| `thread_source=user`，且没有子代理或内部任务证据 | 按主会话规则处理；这仍不代表其中每个 `user` 内容块都是人类输入 |
| 元数据不足或无法确认来源 | 按实际可见内容形成候选输入，不视为已确认的主会话来源 |

官方的子代理兼容标记包括 `review`、`compact`、`memory_consolidation`、`collab_spawn` 以及自定义值。`subagent_kind` 中的 `thread_spawn` 与兼容 header 中的 `collab_spawn` 不是同一个字符串枚举，不能直接用一张只含后者的白名单解释所有字段。[来源生成][subagent]、[线程来源类型][thread-source]

Codex 的 catch-up / recap 请求由临时 `system` 线程生成。客户端先将 recap prompt 渲染为字符串，再作为普通用户文本提交；这条路径不会保留 `recap.prompt` 分类，实际正文可标为 `role=user`、`user.text`，请求用途仍是 `request_kind=turn`。因此通过 `thread_source=system` 排除，不按提示词前缀匹配；用户引用相同提示词仍可采集。[Recap 提示词渲染][recap-render]、[Recap 请求][recap-request]、[临时请求输入][temporary-input]

自动标题及重命名建议使用隐藏的 `thread_title` 线程，同样提交普通用户文本，通过线程来源排除。[标题请求][title-request]

`turn_trigger` 描述整轮如何开始，不证明最后一个输入项由谁产生。自动目标推进、daemon 恢复或 heartbeat 开始的轮次仍可接收用户的新输入；不能仅凭这类 trigger 排除整条请求。恢复提示按下面的最后输入项分类判断，排队输入、实时语音输入和携带真实用户消息的预热仍按用户输入规则处理。[轮次来源保留][turn-trigger-lifetime]、[用户插入输入][steered-input]

`forked_from_thread_id` 和 `parent_thread_id` 本身不能替代明确来源分类，MVP 不保存这些谱系字段。主会话中的子代理通知是消息片段，不能因此过滤整条主会话请求。

过滤对象是子代理自身的输入和输出。主 Agent 已经向用户发送的解释或总结仍是有用上文，即使它提到了子代理工作；不尝试从该总结中逆向删除相关句子。

客户端不提供来源信息时，插件无法证明一条请求是否来自子代理。本设计允许保留这种未知来源记录，不宣称可以在缺少证据时实现绝对排除。来源字段是客户端协议事实，不是独立的人类身份认证。

UA 规则采用 [Rust regex 语法](https://docs.rs/regex/latest/regex/#syntax)，按子串搜索、默认区分大小写，支持 `^`、`$` 与 `(?i)` 等原生语法。任一规则匹配任一有效 User-Agent 值即排除该请求；重复 header 分别匹配。HTTP 使用入站 headers，WebSocket 的每条 `response.create` 使用该连接握手中的 UA。缺失、空值、非 UTF-8 或超过 8,192 字节的 UA 不参与匹配，仍按其他采集规则处理。UA 仅供当前判定使用，不写入审计事实。

规则最多 32 条，每条为非空白、最多 1,024 个 UTF-8 字节的原始正则字符串；保存时不修剪或改写源码。单条与合并后的 RegexSet 编译大小分别限制为 2 MiB；不支持的语法和超出资源限制的设置均在保存前拒绝。规则在加载和保存时编译，请求路径使用一次不可变策略快照。保存成功后的采集使用新策略，已排队任务保持原决定；后续未排除请求携带的历史文本仍可进入该请求的前序对话。持久化配置损坏或读取失败时暂停采集，管理页可通过显式保存修复。

## 4. 识别用户输入

对通过来源筛选的请求，先检查 Responses `input` 的最后一个原始输入项。只有它是可采集的 `user` 消息，才读取其中的内容块并构造一条审计任务。最后一项是 assistant、工具调用或工具结果时跳过整次采集，即使前面仍携带历史用户输入也不回头取它。字符串形式的 `input` 按一条用户文本处理。`role=user` 只是候选条件：Codex 也使用该角色承载 AGENTS.md、环境信息和运行期通知。

可用时，读取 `input[i].internal_chat_message_metadata_passthrough.content_item_kinds[j]`，与 `input[i].content[j]` 按位置对应。它是字符串数组，分类的是内容块而非整个请求。[消息类型][message-type]、[内容分类类型][content-kind]

| 内容 | 处理 |
| --- | --- |
| 用户输入类文本，包括 `user.answered_question` 等[交互问题回答][answered-question] | 按原文本和块顺序保留，形成候选输入 |
| 已标注的 [`agents_md.instructions`][agents-kind]、[`environments.environment_context`][environment-kind] | 作为客户端注入上下文排除，不当作本次用户输入 |
| 已标注的 [`multi_agent.subagent_notification`][notification-kind] | 排除该通知片段，不过滤同请求中的真实输入 |
| 已标注的 [`user.heartbeat`][heartbeat-kind] 等自动触发内容 | 区分为自动事件，不当作新的人类输入 |
| 最后输入项仅由 [`recap.prompt`][recap-kind] / [`daemon_recovery.internal_context`][daemon-recovery] 自动提示组成 | 跳过本次审计；同一消息含用户或未知分类的块时仍保留完整候选输入，前序对话中的这些提示也保留 |
| 请求携带的 skill、摘要、运行期通知、额外上下文和内部证据 | 不凭这些内容分类排除审计或剥离文本；它们的存在不证明请求由自动提示发起 |
| 未标注或尚未识别的 `user` 内容块 | 允许保留，不能认定为已确认的真人输入；后续在本文细化分类 |
| 工具调用、工具返回、reasoning、system / developer 指令 | 不形成审计输入，也不作为普通对话上文 |

不能将所有 `user.*` 分类都等同于真人发言；也不能仅靠文本以 `AGENTS.md` 或某个 XML 标签开头就删除内容，因为用户可能正在引用这些文本。分类信息缺失、与内容块数量不匹配或值未知时，保留为来源未确认的候选输入。[官方分类处理][context-classification]

Codex 可因功能设置移除 content kinds，也会在非 OpenAI provider 路径移除相关内部消息元数据。采集不能依赖这些分类永远存在，更不能因缺少分类而丢弃整个请求。[发送前处理][input-preparation]

Stop hook 的自动继续消息也可能使用 `role=user`，却没有内容分类或专用请求来源。插件不以 `<hook_prompt>` 等文本标记猜测来源，这类未标注内容仍可能形成候选输入。[Hook 继续消息][hook-continuation]

Realtime handoff 将用户语音和转录包装为普通用户文本，不保留专用内容分类。会话结束时的 transcript-tail flush 还加入生成的交接提示，但该路径只写入历史，不发起模型推理；这些混合文本可出现在后续请求的上文中。插件不按 `realtime` trigger 或文本标签排除整个包装，以保留其中的真实语音输入。[语音包装渲染][realtime-wrapper]、[结束时保存历史][realtime-tail-history]

客户端可在真实输入之后追加 [`skills.selected_skill_instructions`][skill-kind]。该分类表示随用户请求加载的 skill 正文，不表示一个独立的自动任务；即使它是最后一个原始输入项，也不因这项分类跳过审计。采集仍以最后一个原始输入项为准，不回头取前面的用户消息。[输入与注入顺序][skill-injection-order]

[`compaction.summary`][summary-kind]、[`generic.turn_aborted`][aborted-kind]、[`additional_content.<key>`][additional-context-kind]、memory / guardian 证据和兼容警告可作为用户请求携带的内容保留；独立的 compaction、memory 或 guardian 请求仍由 §3.2 的来源规则排除。不能把内容由客户端生成等同于请求由客户端自动发起。[Memory 证据][memory-evidence-kind]、[Guardian 指令][guardian-retained-kind]、[兼容警告][legacy-warning-kind]

[`goal.internal_context`][goal-steering] 是共享的[内部上下文分类][internal-context-kind]，既用于自动目标继续，也用于用户编辑目标之后的推进提示。仅凭它不能确认请求的发起者，因此保留这类候选输入，不按整个 `goal` trigger 排除。[用户目标变更与提示注入][goal-update-input]

`user.answered_question`、`user.goal` / `user.goal.omitted` 和 `shell.user_command` 来自明确的用户交互，继续保留。`images.preparation_error`、`images.unsupported` 和 `audio.unsupported` 是用户已提交媒体的替代说明，也继续保留，不将它们当作自动任务排除。[用户目标变更][user-goal-kind]、[用户 Shell 命令][shell-kind]、[媒体替代说明][media-placeholder-kind]

首期保存已保留输入块的原始分类字符串，详情以 `inputKinds` 按块顺序返回，不汇总为请求级类型或采集提示。移除注入块时同时移除它的分类；未知值原样保留，不强行归入已有枚举。分类缺失或原数组长度与原内容块数量不符时，每个保留块均记 `null`，不能将不确定的位置关系当作已知。非文本的媒体类型与这份客户端分类是两个不同字段。

最后一条用户消息内的多个可采集内容块组成一条审计输入；历史用户消息只可能参与前序上下文，不单独形成记录。正文保存观察到的文本，不由模型改写或总结；上下文窗口上限不用于截短本次输入。首期非文本内容只保留类型、位置及请求已有的可显示说明，不复制图片、音频或文件二进制，不主动下载附件，详情明确显示“非文本内容未采集”。

## 5. 来源标识与重复请求

### 5.1 标识的用途

| 标识 | 含义与用途 |
| --- | --- |
| 宿主 request ID | 定位承载本次审计观察的请求，并查询宿主已有请求元数据 |
| thread / session ID | 分别保留客户端报告的会话标识，不用于拼接其他请求的内容 |
| turn ID | 保存本条用户消息的 turn 标识，不作为记录唯一键 |

保存的 turn ID 只取最后一条用户消息的 `internal_chat_message_metadata_passthrough.turn_id`，没有就记 `null`；不使用当前请求的 turn ID 补值。消息的 `create_time` 不保存，页面继续仅使用网关记录时间。[消息级元数据][message-metadata]

优先使用规范元数据中的 `thread_id`，缺失时使用 `client_metadata.thread_id` 兼容投影。`session_id` 保留自身语义，不静默冒充 thread ID；`prompt_cache_key` 可被覆盖，不用作会话身份。[元数据兼容投影][metadata-flat]、[缓存键生成][cache-key]

### 5.2 重复请求分别记录

插件不进行输入去重、不保存消息指纹或历史匹配状态，也不依据 request、thread、session、turn 或消息 ID 复用审计记录。客户端重试同一请求时，只要最后一个输入项仍是可采集的用户消息，就提交新的审计任务。

Agent 工具循环携带早先的用户输入时，最后一个输入项通常是工具调用或工具结果；这种请求由第 4 节的最后一项检查跳过，无需通过历史去重判定。同一文本出现多条审计记录不证明用户实际重复发言，页面展示的是各次请求观察。

## 6. 前序对话的选择与关联

### 6.1 以本条输入的位置为界

上下文只能来自本次请求中、最后一条被采集用户消息之前的输入项。历史用户消息用于理解当前输入，不单独形成审计记录。

保留 Agent 的可读文本，包括解释、计划、`commentary` 与 `final_answer`；保留必要的较早用户消息以理解回答对象。忽略工具调用及结果、reasoning 和已识别的系统上下文片段。缺少 phase 的 assistant 文本仍可使用，不能因此只留下最终回答。

先取输入之前最近 12 条可读对话消息，再将总量限制为 32,000 个 Unicode 码点，当前被审计输入不计入。超过字符预算时，先舍弃窗口中最旧的前序用户消息，再舍弃最旧的 Agent 消息，直至满足预算或仅剩一条消息；最后一条仍超限时保留末尾并标注截断。结果按原会话顺序展示。当前输入不受这份上文预算截短；请求整体仍受第 7 节的采集大小上限限制。

上下文直接保存所选原文，不调用模型生成摘要。用户“请继续”对应的 Agent 方案即使较长，也应以可见原文或明确截断的原文呈现，避免将生成的解释冒充原始对话。

### 6.2 全量历史、增量和分叉

前序对话只来自本次请求携带的历史。插件不读取会话数据库、不跟踪模型响应，也不通过 `previous_response_id` 获取缺失上文。

| 情况 | 上下文处理 |
| --- | --- |
| 请求携带可用历史 | 从最后一条用户输入之前的可读消息截取窗口 |
| 请求只携带增量 | 仅保存本次输入之前实际携带的可读消息；没有上文时保存空上下文 |
| 客户端压缩上下文 | 使用本次可见历史，不恢复被客户端丢弃的原文 |
| 主会话分叉 | 仅使用该请求携带的历史，不取其他分支缓存 |

缺少 thread 或 session ID 不妨碍使用本次请求的上文；Key 或会话标识相同也不会继承其他请求的内容。

### 6.3 固定审计时的观察

审计上下文表示“本次请求被网关观察时携带的有限前序对话”。它随输入一起保存，输入之后产生的 Agent 输出不会修改旧快照。相同请求再次发送时分别形成记录，各自保留该次请求的上下文。

输入可能在客户端较早提交、稍后才送到网关；因此本设计不声称该快照精确还原客户端提交瞬间的全部画面。

## 7. 审计任务与异步写入

请求路径仅提取待保存的输入、有限上下文及元数据，然后以非阻塞方式提交审计任务。任务进入容量 **64** 的进程内环形缓冲区；满时直接覆盖最旧的待处理任务，保留新任务，不等待队列空位、worker、数据库或写入确认。请求路径没有数据库查询、Key 目录刷新或完成事件关联，也不等待互斥锁。

独立异步 worker 从环形缓冲区取出任务，每批最多 **32** 条，在一个数据库事务内批量保存输入及各自的固定上下文。worker 已取走的工作批次不受后续队列覆盖影响。单次批次写入最多等待 **2 秒**，数据库语句与锁分别限制为 1.5 秒和 1 秒；失败或超时的批次可以丢失，不自动重放。写入时机由 worker 决定，审计不提供入库时效保证。

Key 名称使用已有缓存，请求以 `try_lock` 取得可用快照；缓存锁忙时省略名称及名称观测时间，不等待刷新。目录仅在管理 `api/keys` 查询时按需分页刷新，最多每分钟尝试一次，整次刷新限 1 秒；失败或不完整时保留上一目录，不推断 Key 删除。

解析或写入失败不阻止模型请求继续处理。写入诊断只包含故障类别，不含正文和凭据。容量限制、队列覆盖、进程退出、数据库不可用或写入超时均可能造成审计丢失；没有磁盘缓冲、可靠投递恢复或请求成功记录保证。

启动必须在 4 秒内完成连接、审计迁移和游标签名密钥读取，否则进程退出，配置为 `failurePolicy: delegate` 的 middleware binding 继续原请求。正常 SDK 关闭或 SIGTERM／Ctrl+C 后尽力排空队列，插件最多等待 5 秒；宿主自身的 2 秒关闭宽限可能更早强制终止插件。强杀、进程崩溃和关机可能丢失所有尚未提交的队列项和工作批次；已提交的输入和固定上文保留。

请求正文超过 **16 MiB** 的插件采集上限时不建档，不截短本条输入。队列只持有已经提取的审计任务，worker 工作批次也有数量上限，不保存完整请求或响应副本。数据库存储边界见[总体设计 §4.3](design.md#43-逻辑数据模型)。

[http-entry]: https://github.com/zyycn/codex-proxy-rs/blob/447869860a0fb88f78109cdb700db0cb1eb5d8b5/backend/crates/gateway-api/src/openai/responses/http.rs#L72
[execution]: https://github.com/zyycn/codex-proxy-rs/blob/447869860a0fb88f78109cdb700db0cb1eb5d8b5/backend/crates/gateway-core/src/engine/execution/service.rs#L317
[ws-entry]: https://github.com/zyycn/codex-proxy-rs/blob/447869860a0fb88f78109cdb700db0cb1eb5d8b5/backend/crates/gateway-api/src/openai/responses/websocket/mod.rs#L325
[incremental]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/core/src/client.rs#L1994
[metadata]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/core/src/responses_metadata.rs#L225
[subagent]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/core/src/responses_metadata.rs#L458
[message-type]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/protocol/src/models.rs#L1021
[content-kind]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/protocol/src/models/item_metadata.rs#L4
[context-classification]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/core/src/context/contextual_user_message.rs#L51
[thread-source]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/protocol/src/protocol.rs#L2843
[answered-question]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/context-fragments/src/answered_question.rs#L24
[agents-kind]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/core/src/context/user_instructions.rs#L10
[environment-kind]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/core/src/context/world_state/environment.rs#L198
[notification-kind]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/core/src/context/subagent_notification.rs#L21
[heartbeat-kind]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/history/src/heartbeat.rs#L10
[metadata-flat]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/core/src/responses_metadata.rs#L325
[cache-key]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/core/src/client.rs#L575
[input-preparation]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/core/src/client.rs#L940
[workspace-fields]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/core/src/responses_metadata.rs#L187
[git-enrichment]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/core/src/turn_metadata.rs#L481
[message-metadata]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/protocol/src/models.rs#L960
[recap-request]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/tui/src/app/recap.rs#L226
[recap-render]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/tui/src/app/recap.rs#L57
[temporary-input]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/tui/src/temporary_structured_request.rs#L195
[recap-kind]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/context-fragments/src/recap_prompt.rs#L49
[title-request]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/tui/src/app/thread_title.rs#L103
[turn-trigger-lifetime]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/core/src/turn_metadata.rs#L371
[steered-input]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/core/src/session/turn_input.rs#L207
[skill-kind]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/ext/skills/src/fragments.rs#L77
[summary-kind]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/core/src/context/compaction_summary.rs#L17
[aborted-kind]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/core/src/context/turn_aborted.rs#L20
[internal-context-kind]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/core/src/context/internal_model_context.rs#L79
[goal-steering]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/ext/goal/src/steering.rs#L45
[goal-update-input]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/app-server/src/request_processors/thread_goal_processor.rs#L199
[daemon-recovery]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/app-server/src/request_processors/daemon_continuation.rs#L88
[additional-context-kind]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/context-fragments/src/additional_context.rs#L23
[memory-evidence-kind]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/core/src/context/memory.rs#L14
[guardian-retained-kind]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/core/src/context/guardian_retained_instructions.rs#L28
[legacy-warning-kind]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/core/src/context/legacy_apply_patch_exec_command_warning.rs#L8
[hook-continuation]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/protocol/src/items.rs#L675
[realtime-wrapper]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/core/src/realtime_conversation.rs#L1882
[realtime-tail-history]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/core/src/realtime_conversation/transcript_tail.rs#L21
[skill-injection-order]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/core/src/session/turn.rs#L396
[user-goal-kind]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/core/src/context/user_goal.rs#L15
[shell-kind]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/core/src/context/user_shell_command.rs#L30
[media-placeholder-kind]: https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/core/src/context/unsupported_media.rs#L8
