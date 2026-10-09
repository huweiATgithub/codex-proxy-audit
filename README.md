# Codex Proxy Audit

Codex Proxy RS 的 Prompt 审计插件设计：按 Client Key 记录经过网关的用户输入，以及理解这次输入所需的有限前序对话。例如，用户说“请继续”时，审计者可以同时看到 Agent 此前提出的计划。

采集排除能够识别的子代理请求，前序对话以 Agent 文本消息为重点，忽略工具调用、工具结果和推理内容。审计数据使用与宿主相同的 PostgreSQL 数据库、独立 schema；管理员通过插件提供的审计页面查看记录。

审计元数据保留 thread/session ID、消息级 turn ID、逐块输入类型及来源请求报告的仓库根路径、remote URLs。UA、请求模型与 reasoning effort 通过来源请求 ID 和 Key ID 关联宿主已有记录，不重复存入审计 schema；宿主请求记录清理后这三项不可用，输入和前序对话仍保留。详情优先展示用户输入和前序对话，“记录信息”在其后默认显示，Client Key 与其他元数据同级排列；外层表格保持记录时间、Client Key 与用户输入三列。具体取值和排除范围见采集行为，宿主只读关联的权限与边界见总体设计 §4。

本仓库包含开发设计、[前后端查询契约](contracts/openapi.json)和按契约读取数据的 [UI](frontend/)。UI 支持高密度表格、用户输入正文关键词与时间、Key 组合筛选、游标分页及按需读取的详情和前序对话。采集、PG 存储和插件后端尚未实现；[Demo](demo/)通过本地模拟服务提供 500 条虚构输入，不接入真实网关或管理员认证。

UI 使用 Vue 3 与官方组件库 [`@codex-proxy/ui` v0.3.2](https://github.com/zyycn/codex-proxy-ui/tree/v0.3.2)，参照 [#278](https://github.com/zyycn/codex-proxy-rs/issues/278) 中的插件示例接入组件、样式和主题。在 Node.js 24+ 环境下，于仓库根目录运行：

```sh
npm ci
npm run demo
```

远程浏览器访问 `http://<服务器 IP>:4173`；服务监听 `0.0.0.0` 的所有 IPv4 网卡，按 `Ctrl+C` 停止。页面默认筛选宿主当天；Demo 按宿主默认的 `Asia/Shanghai` 模拟系统时间，并在启动时将示例整体平移到服务器当前时间；“重置”可查看全部示例。可搜索“数据库”或“请继续”，组合时间与 Key 缩小范围，再打开一条输入阅读上文。支持每页最多 50 / 100 / 200 条、前后翻页、单行预览、上下方向键连续审阅和 Esc 关闭详情；Demo 外层的主题按钮模拟宿主切换浅色与深色主题，插件本身沿用宿主主题。Thread ID 与 Session ID 分别保留在记录信息中，不提供会话归类入口。

`npm run build` 生成正式插件静态资源到 `dist/plugin/`，使用宿主页桥、宿主主题和经典脚本；生成的 `management.json` 供后端注册页面、资源及五个查询入口。正式构建必须在宿主中打开，不会回退到模拟数据。UI 统一提供“复制审计 ID”，不提供记录链接或 URL 查询恢复。

`npm run demo` 先构建正式插件，再启动模拟宿主；模拟宿主读取上述注册信息，将同一份页面资源加载进隔离 iframe，通过 v2 管理页桥转发到模拟后端。没有独立的 Demo UI 构建。修改 UI 后重新运行此命令；`npm run preview` 直接加载已有构建。可用 `npm run demo -- --time-zone America/New_York` 模拟其他宿主部署时区，浏览器时区不影响展示和筛选。依赖版本和完整性由 `package-lock.json` 固定。

查询契约的语义见[设计文档 §5.3](docs/design.md#53-管理-api)，字段以 `contracts/audit.js` 为源；修改后运行 `npm run contract:export` 更新 OpenAPI。验证命令为 `npm run contract:check`、`npm test` 和 `npm run test:ui`。浏览器验证首次运行需执行 `npx playwright install chromium`，也可通过 `PLAYWRIGHT_CHROMIUM_EXECUTABLE` 指定已有 Chromium；测试自动使用 4174 端口，不占用供审阅的 4173。

| 文档 | 职责 |
| --- | --- |
| [总体设计](docs/design.md) | 已核实的插件合同、模块边界、Client Key 关联、PG 存储、页面行为及查询契约语义 |
| [OpenAPI 契约](contracts/openapi.json) | 从共享 Schema 导出的请求参数与响应结构，供后端实现对接 |
| [采集行为](docs/collection.md) | 采集阶段、子代理过滤、用户输入识别、去重和上下文关联；作为持续完善采集规则的独立文档 |

采集侧故障处理、执行调度和验收方案由后端 implementer 定义；页面读取错误与重试行为属于查询契约。
