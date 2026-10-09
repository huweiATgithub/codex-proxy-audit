# 采集行为

## 1. 采集对象与边界

本文定义 Prompt 审计“记录什么、如何认定是同一次输入、带上哪些上文”。它是采集规则的独立维护入口；PG、插件接入和 UI 见[总体设计](design.md)。

一条审计记录描述：某个 Client Key 对应的会话中，网关观察到的一条用户输入，以及当时可取得、用于理解该输入的有限前序对话。目标是帮助审阅用户意图，例如将“请继续”与 Agent 此前的方案一起展示。

观察边界是网关 `request` 中间件，不是客户端输入框。客户端尚未发出的输入、到达该阶段之前就被拒绝的请求、客户端在发送前移除的历史，都不在插件可见范围内。页面将时间称为“记录时间”，其值为该输入首次经过本采集边界并形成审计记录的时间（`first_observed_at`）。同一输入随之后的模型请求重复出现时，复用已有记录且不更新时间。例如一条输入在 10:00 被记录，10:01 随后续请求重传，记录时间仍为 10:00。该时间不等于客户端提交时间，也不等于异步写入数据库完成的时间。

首期保留可识别的用户输入，也允许保留来源尚不明确的 `user` 消息；只保存输入身份与去重所需依据，MVP 不提供采集提示展示或筛选。请求用途与来源用于决定是否采集，判定后不将这些字段或分类快照落库。已识别的子代理请求整体排除。插件不保存完整请求转储、工具过程或推理轨迹。

## 2. 采集阶段

选择 `request` 中间件作为输入采集点，在调用 `next` 之前读取该阶段可见的原始输入和会话元数据。宿主已完成入口认证及初步解析，可以取得 Client Key ID；具体 Provider/account 的执行尝试尚未开始。[HTTP 入口][http-entry]、[执行入口][execution]

| 阶段 | 与本设计的关系 |
| --- | --- |
| `http` / `websocket` 原始入口 | 更接近传输层，处于业务认证或消息解析之前，不作为首期审计主入口 |
| `request` 的入站部分 | 识别来源、用户输入及输入前的历史，构造审计记录 |
| `attempt` | 可能因上游重试执行多次，不作为用户输入的采集单位 |
| `request` 的响应返回链 | 观察主会话 Agent 文本及协议响应 ID，为后续用户输入准备上文 |

宿主 WebSocket 在进入 request 中间件时仍可保留客户端 `response.create` 的增量正文，历史重放准备发生在后续链中。因此不能假设该阶段已经得到完整会话。`generate=false` 的主会话预热请求也可能携带用户输入，而正式生成时只发送增量；预热不是丢弃该输入的理由。[WebSocket 入口][ws-entry]、[Codex 增量请求][incremental]

审计插件安排在其他 request 输入改写插件之前；更外层 HTTP/WebSocket 插件仍可能已经改变正文，所以“原始输入”仅指本边界实际看到的内容。

## 3. 来源判定与子代理过滤

### 3.1 解析元数据

首先读取请求正文 `client_metadata["x-codex-turn-metadata"]`。它是一个包含 JSON 的**字符串**，应再次解析为结构化元数据。官方 Codex 将它定义为规范来源，flat `client_metadata` 字段与 HTTP headers 是兼容投影。[元数据合同][metadata]

解析时只取当次来源判定及已确定审计字段所需的部分。`thread_source`、`subagent_kind`、`request_kind`、`turn_trigger` 和兼容子代理标记只在本次采集判定中使用，不持久化请求用途或来源；不保存原始元数据 JSON。长期元数据限定为 `thread_id`、`session_id`、本条消息的 `turn_id`、逐块输入类型，以及下述仓库根路径与 remote URLs。

优先使用当前请求正文；规范对象不可用时，使用已知的 flat 字段及对应 headers。WebSocket 每个 `response.create` 的正文优先于握手 headers，不能让建立连接时的来源覆盖当前消息的来源。冲突不被静默解释成“确认主会话”，但不另存元数据来源标签。

仓库取规范对象的 `workspaces`：每个键是客户端报告的仓库根路径，每个值仅提取 `associated_remote_urls` 中的 remote 名称与 URL。Git 信息异步补充，早期请求可能没有这一字段；缺失就留空，不从输入正文、cwd 或其他请求猜测，也不补采 commit hash、`has_changes`。URL 入库前去除认证凭据，保留仓库定位语义，不访问远端。这份快照与记录的来源 request ID 关联，不能将当前仓库状态解释为每条历史输入编写时的状态。[工作区字段][workspace-fields]、[Git 补充][git-enrichment]

UA、模型和 reasoning effort 由查询层关联宿主已有请求记录，采集器不再保存副本，见[总体设计 §4.3](design.md#43-逻辑数据模型)。其余不采集的请求元数据包括客户端时间、独立的客户端版本或 originator、安装标识、分叉谱系、上下文窗口标识、请求计时、工具目录与 MCP 归因、自定义元数据、执行权限和 service tier。`reasoning.summary` 是请求中的推理摘要配置，摘要正文属于模型输出；两者均不采集。

### 3.2 整条请求过滤

来源分类发生在生成审计内容及注册正文采集之前：

| 来源证据 | 采集行为 |
| --- | --- |
| `subagent_kind`，或明确的 `thread_source=subagent` | 排除该请求的输入和响应 |
| `x-openai-subagent` 兼容标记 | 排除对应的子代理或内部任务请求，不只匹配 `collab_spawn` |
| 明确的内部任务来源，例如 guardian review、memory consolidation，或 `request_kind=compaction` / `memory` | 不作为用户输入，也不补入主会话 Agent 上文 |
| `thread_source=user`，且没有子代理或内部任务证据 | 按主会话规则处理；这仍不代表其中每个 `user` 内容块都是人类输入 |
| 元数据不足或无法确认来源 | 按实际可见内容形成候选输入，不视为已确认的主会话来源 |

官方的子代理兼容标记包括 `review`、`compact`、`memory_consolidation`、`collab_spawn` 以及自定义值。`subagent_kind` 中的 `thread_spawn` 与兼容 header 中的 `collab_spawn` 不是同一个字符串枚举，不能直接用一张只含后者的白名单解释所有字段。[来源生成][subagent]、[线程来源类型][thread-source]

`forked_from_thread_id` 和 `parent_thread_id` 本身不能替代明确来源分类，MVP 不保存这些谱系字段。主会话中的子代理通知是消息片段，不能因此过滤整条主会话请求。

过滤对象是子代理自身的输入和输出。主 Agent 已经向用户发送的解释或总结仍是有用上文，即使它提到了子代理工作；不尝试从该总结中逆向删除相关句子。

客户端不提供来源信息时，插件无法证明一条请求是否来自子代理。本设计允许保留这种未知来源记录，不宣称可以在缺少证据时实现绝对排除。来源字段是客户端协议事实，不是独立的人类身份认证。

## 4. 识别用户输入

对通过来源筛选的请求，按顺序读取 Responses `input` 中的消息及内容块。`role=user` 只是候选条件：Codex 也使用该角色承载 AGENTS.md、环境信息和运行期通知。

可用时，读取 `input[i].internal_chat_message_metadata_passthrough.content_item_kinds[j]`，与 `input[i].content[j]` 按位置对应。它是字符串数组，分类的是内容块而非整个请求。[消息类型][message-type]、[内容分类类型][content-kind]

| 内容 | 处理 |
| --- | --- |
| 用户输入类文本，包括 `user.answered_question` 等[交互问题回答][answered-question] | 按原文本和块顺序保留，形成候选输入 |
| 已标注的 [`agents_md.instructions`][agents-kind]、[`environments.environment_context`][environment-kind] | 作为客户端注入上下文排除，不当作本次用户输入 |
| 已标注的 [`multi_agent.subagent_notification`][notification-kind] | 排除该通知片段，不过滤同请求中的真实输入 |
| 已标注的 [`user.heartbeat`][heartbeat-kind] 等自动触发内容 | 区分为自动事件，不当作新的人类输入 |
| 未标注或尚未识别的 `user` 内容块 | 允许保留，不能认定为已确认的真人输入；后续在本文细化分类 |
| 工具调用、工具返回、reasoning、system / developer 指令 | 不形成审计输入，也不作为普通对话上文 |

不能将所有 `user.*` 分类都等同于真人发言；也不能仅靠文本以 `AGENTS.md` 或某个 XML 标签开头就删除内容，因为用户可能正在引用这些文本。分类信息缺失、与内容块数量不匹配或值未知时，保留为来源未确认的候选输入。[官方分类处理][context-classification]

Codex 可因功能设置移除 content kinds，也会在非 OpenAI provider 路径移除相关内部消息元数据。采集不能依赖这些分类永远存在，更不能因缺少分类而丢弃整个请求。[发送前处理][input-preparation]

首期保存已保留输入块的原始分类字符串，详情以 `inputKinds` 按块顺序返回，不汇总为请求级类型或采集提示。移除注入块时同时移除它的分类；未知值原样保留，不强行归入已有枚举。分类缺失或原数组长度与原内容块数量不符时，每个保留块均记 `null`，不能将不确定的位置关系当作已知。非文本的媒体类型与这份客户端分类是两个不同字段。

一条消息内的多个用户内容块组成一条审计输入，多条用户消息分别形成记录。正文保存观察到的文本，不由模型改写或总结；上下文窗口上限不用于截短本次输入。首期非文本内容只保留类型、位置及请求已有的可显示说明，不复制图片、音频或文件二进制，不主动下载附件，详情明确显示“非文本内容未采集”。

## 5. 输入身份与重复观察

### 5.1 区分四种标识

| 标识 | 含义与用途 |
| --- | --- |
| 宿主 request ID | 一次模型请求的来源定位；不等于一次用户发言 |
| thread / session ID | 在 Client Key 范围内关联同一会话；不同会话和 Key 的内容不混合 |
| turn ID | 保存本条用户消息的 turn 标识；一次 turn 内可以追加多条用户消息 |
| 稳定消息 ID，或已建立的有序历史位置 | 识别同一条输入的重复传输 |

Codex 允许向 active turn 追加输入并返回同一个 turn ID，因此不能使用 `(key, thread, turn)` 作为一条审计输入的唯一键。消息 ID 本身也是可选的，发送前还会清除部分 ID。[追加输入][steering]、[发送前处理][input-preparation]

保存的 turn ID 只取 `input[i].internal_chat_message_metadata_passthrough.turn_id`，没有就记 `null`；不取当前请求的 turn ID 为历史消息补值。同一请求里的历史消息可能属于不同 turn。消息的 `create_time` 不保存，页面继续仅使用网关记录时间。[消息级元数据][message-metadata]

优先使用规范元数据中的 `thread_id`，缺失时使用 `client_metadata.thread_id` 兼容投影。`session_id` 保留自身语义，不静默冒充 thread ID；`prompt_cache_key` 可被覆盖，不用作会话身份。没有可关联的会话身份时，只使用本次请求内的前序消息，不凭 Key 相同就拼接其他请求。[元数据兼容投影][metadata-flat]、[缓存键生成][cache-key]

### 5.2 识别重复与新输入

按下列顺序确定输入身份：

1. 有稳定消息 ID 时，在 Key 与会话范围内匹配已观察消息
2. 否则，将本次有序消息与已知会话历史或明确的前驱响应对齐，利用位置和相同内容的出现次数辨认重传及新增后缀
3. 对齐证据不足时，保留可能的新输入并标注依据不确定，不以全文哈希直接消除它

文本摘要可以辅助有序匹配，但相同文本不是同一输入。例如用户连续两次说“请继续”，应保留两次；而同一句输入随 Agent 工具循环反复传输，不应每次都产生新审计记录。

首次观察一个已有会话时，请求可能携带多轮历史。明确处于历史中的用户消息标为历史输入，不伪装为刚刚提交；不能确认新旧的候选标为不确定。已有记录再次出现时复用身份，不用后来的请求覆盖当初固定的上下文。

每条记录保存采集策略版本、输入身份依据以及消息在来源请求中的必要位置，使规则逐步完善后仍能解释旧记录。缺少稳定 ID 和完整历史时，本设计不承诺精确一次的用户事件识别。

## 6. 前序对话的选择与关联

### 6.1 以本条输入的位置为界

上下文只能来自该输入之前的消息。一个请求包含多条新用户消息时，分别按各自位置构造上下文；不能把整个请求末尾的消息共同附给每一条输入。

保留 Agent 的可读文本，包括解释、计划、`commentary` 与 `final_answer`；保留必要的较早用户消息以理解回答对象。忽略工具调用及结果、reasoning 和已识别的系统上下文片段。缺少 phase 的 assistant 文本仍可使用，不能因此只留下最终回答。

初始建议先取输入之前最近 12 条可读对话消息，再将总量限制为 32,000 个 Unicode 码点，当前被审计输入不计入。超过字符预算时，先舍弃窗口中最旧的前序用户消息，再舍弃最旧的 Agent 消息，直至满足预算或仅剩一条消息；最后一条仍超限时保留末尾并标注截断。结果按原会话顺序展示。窗口数值属于可调整的采集策略，随策略版本记录。

上下文直接保存所选原文，不调用模型生成摘要。用户“请继续”对应的 Agent 方案即使较长，也应以可见原文或明确截断的原文呈现，避免将生成的解释冒充原始对话。

### 6.2 全量历史、增量和分叉

本次请求携带的前序历史是直接证据。请求只携带增量时，通过它的 `previous_response_id` 查找同 Key、同会话的前驱响应，从对应的有限窗口接续；不能使用“这个会话最近收到的响应”代替明确前驱。并发输出的到达顺序不等于会话顺序。这些协议 ID 只属于有限会话状态，不扩展为长期审计元数据。

| 情况 | 上下文处理 |
| --- | --- |
| 请求携带可用历史 | 从本次输入之前的可读消息截取窗口，并对齐已知消息，避免重复拼接 |
| 增量请求有已知前驱响应 | 使用该响应对应的会话窗口，加上本次输入前的增量消息 |
| 前驱不可用或会话首次出现 | 仅保留实际可见的上文，记录上下文缺口 |
| 客户端压缩上下文 | 依据可见历史更新有限窗口；不存窗口标识，不把 compact 生成的总结当作新的人类输入，也不假装能恢复被客户端丢弃的原文 |
| 主会话分叉 | 仅使用请求携带的历史理解新分支输入；明确属于历史的消息不伪装为刚刚提交，不保存分叉谱系或跨分支取缓存 |

没有可用前驱时只保留本次可见历史，不跨 Key 或跨 thread 继承缓存。上文缺口和按窗口截断仍作为内容可用性事实保留，不记录请求来源或分叉谱系。

### 6.3 固定审计时的观察

审计上下文表示“该输入首次被网关观察时可取得的前序对话”。同一输入后续重传不改变它；输入之后产生的 Agent 输出只参与后续输入的上下文构造。这样，查看旧输入不会因 Agent 后来继续工作而看到不断变化的上文。

输入可能在客户端较早提交、稍后才送到网关；因此本设计不声称该快照精确还原客户端提交瞬间的全部画面。

## 7. 响应观察与后续关联

对通过第 3 节来源筛选的请求，在 request 的返回链解析 JSON / SSE / WebSocket 响应，按 response、输出项与内容块的位置合并 assistant 文本，不另存请求来源标记。增量事件与最终完整消息属于同一个输出，不能重复追加。仅保存第 6 节需要的文本窗口及关联事实。

响应关联使用协议正文中的 ID：完整 JSON 的 `id`，或 SSE / WebSocket 响应事件的 `response.id`。宿主原生 WebSocket 路径在进入该观察边界前已完成编码，之后不会再替换成功响应 ID，宿主自身也按这个 ID 匹配 `previous_response_id`。[WebSocket 响应编码及续接][ws-response]

后续出站插件必须保留这个 ID。WebSocket 出站插件及 HTTP 外层插件仍可能改写内容，当前 request 合同不另外提供一个“经过所有插件之后的客户端响应 ID”。首期不支持这种额外的 ID 改写组合。[WebSocket 出站链][ws-outgoing]

宿主 request ID、Provider upstream request ID 以及 SDK 的不透明 `response` 句柄都不是协议响应 ID，不能作为 `previous_response_id` 的关联键。[SDK 响应句柄][response-handle]

SDK `inspect_frames` 的观察闭包是同步 `FnMut`，且随下游 Credit 拉取正文；只能观察实际被消费的帧，不能据此承诺取得完整输出或独立的 EOF 通知。观察适配层负责提取文本与协议事件，持久化执行方式由 implementer 决定，不能直接在该闭包里假设可 `await` 数据库写入。后续请求若携带更完整的历史，可以为新的审计输入提供上下文，但不改写旧快照。[帧观察合同][inspect-frames]

[http-entry]: https://github.com/zyycn/codex-proxy-rs/blob/f174320e2ac8987578146d4684e69ad196db0286/backend/crates/gateway-api/src/openai/responses/http.rs#L72
[execution]: https://github.com/zyycn/codex-proxy-rs/blob/f174320e2ac8987578146d4684e69ad196db0286/backend/crates/gateway-core/src/engine/execution.rs#L962
[ws-entry]: https://github.com/zyycn/codex-proxy-rs/blob/f174320e2ac8987578146d4684e69ad196db0286/backend/crates/gateway-api/src/openai/responses/websocket/mod.rs#L218
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
[steering]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/core/src/session/turn_input.rs#L641
[ws-response]: https://github.com/zyycn/codex-proxy-rs/blob/f174320e2ac8987578146d4684e69ad196db0286/backend/crates/gateway-api/src/openai/responses/websocket/forward.rs#L475
[ws-outgoing]: https://github.com/zyycn/codex-proxy-rs/blob/f174320e2ac8987578146d4684e69ad196db0286/backend/crates/gateway-api/src/openai/responses/websocket/connection/mod.rs#L730
[response-handle]: https://github.com/zyycn/codex-proxy-rs/blob/f174320e2ac8987578146d4684e69ad196db0286/backend/crates/gateway-plugin/sdk/src/call/middleware/mod.rs#L148
[inspect-frames]: https://github.com/zyycn/codex-proxy-rs/blob/f174320e2ac8987578146d4684e69ad196db0286/backend/crates/gateway-plugin/sdk/src/client/middleware/body.rs#L160
[workspace-fields]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/core/src/responses_metadata.rs#L187
[git-enrichment]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/core/src/turn_metadata.rs#L481
[message-metadata]: https://github.com/openai/codex/blob/d27764b82f7118f674371e6d6e76271d9d606edb/codex-rs/protocol/src/models.rs#L960
