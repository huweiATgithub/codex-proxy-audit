# Codex Proxy Audit

Codex Proxy RS 的 Prompt 审计插件：按 Client Key 记录经过网关的用户输入，以及理解这次输入所需的有限前序对话。例如，用户说“请继续”时，审计者可以同时看到 Agent 此前提出的计划。

采集排除能够识别的子代理请求，前序对话以 Agent 文本消息为重点，忽略工具调用、工具结果和推理内容。审计数据使用与宿主相同的 PostgreSQL 数据库、独立 schema；管理员通过插件提供的审计页面查看记录。

审计元数据保留 thread/session ID、消息级 turn ID、逐块输入类型及来源请求报告的仓库根路径、remote URLs。UA、请求模型与 reasoning effort 通过来源请求 ID 和 Key ID 关联宿主已有记录，不重复存入审计 schema；宿主请求记录清理后这三项不可用，输入和前序对话仍保留。详情优先展示用户输入和前序对话，“记录信息”在其后默认显示，Client Key 与其他元数据同级排列；外层表格保持记录时间、Client Key 与用户输入三列。具体取值和排除范围见采集行为，宿主只读关联的权限与边界见总体设计 §4。

本仓库包含 [Rust 插件后端](backend/)、[前后端查询契约](contracts/openapi.json)和按契约读取数据的 [UI](frontend/)。后端在 request 阶段检查最后一个输入项，仅为可采集的用户消息提交审计任务；前序对话只取本次请求自带的内容。任务进入固定容量、满时覆盖最旧任务的环形缓冲区，由 worker 异步写入 PostgreSQL，不等待写入确认。重复请求分别记录，后续失败或取消的请求也可能留下记录，尚未落库的任务可能丢失。后端自管数据库迁移与五个管理 GET 接口。UI 支持高密度表格、用户输入正文关键词与时间、Key 组合筛选、游标分页及按需读取的详情和前序对话。[Demo](demo/)默认提供 500 条虚构输入，也可加载脱敏 Session 样本，供独立审阅页面；模拟服务不承担真实后端验收。

宿主兼容声明为 `~3.23.0`，接受稳定版 `>=3.23.0, <3.24.0`，包括 v3.23.1；SDK 与可复现构建基线固定到 upstream v3.23.0（`389dc63633a3b24c5b40b0002901371820bc6998`）。宿主需要应用[只读部署上下文补丁](host-patches/v3.23.0-deployment-context.patch)，补丁也适用于 v3.23.1。补丁只提供有效 `host.timezone` 与管理响应预算；插件没有时区配置或浏览器时区回退。宿主通过 `model_request_observations` 视图投影请求元数据，插件角色仅读取指定五列，详情字段不变，依据见[总体设计 §4](docs/design.md#4-共用-postgresql-的存储设计)。在仓库根目录准备宿主及插件产物：

```sh
git clone --branch v3.23.0 --depth 1 https://github.com/zyycn/codex-proxy-rs ../codex-proxy-audit-host
git -C ../codex-proxy-audit-host apply "$PWD/host-patches/v3.23.0-deployment-context.patch"
CPR_VERSION=3.23.0 cargo +1.97.0 build --manifest-path ../codex-proxy-audit-host/backend/Cargo.toml --locked -p codex-proxy-rs -p codex-proxy-plugin-cli
(cd ../codex-proxy-audit-host/frontend && corepack pnpm install --frozen-lockfile && corepack pnpm build)
npm ci
npm run plugin:build
CPR_PLUGIN=../codex-proxy-audit-host/backend/target/debug/cpr-plugin npm run plugin:package
```

正式安装包与校验文件从 [GitHub Releases](https://github.com/huweiATgithub/codex-proxy-audit/releases) 下载；本地构建输出 `dist/packages/huwei.prompt-audit-0.1.5-x86_64-unknown-linux-gnu.tar.gz`。宿主 CLI 生成资源声明、规范化能力和文件摘要，包内包含 MIT 许可证。同一版本的平台产物在宿主中不可变，修改后发布新的插件版本并使用实例的版本切换入口升级；升级保留原实例的配置、secrets 和 binding，审计事实继续保存在数据库中。正式包支持 Linux x86_64 GNU，要求 glibc 2.34 或更新版本；其他目标先通过 Cargo 的 `--target` 构建，再用 `AUDIT_TARGET`、`AUDIT_BINARY` 指定目标和二进制。`plugin.json` 只声明 middleware v4／management v1。宿主安装页面上传此包，接受受信任进程，配置 secrets 中的 `databaseUrl`；普通 configuration 保持 `{}`。启用 request binding，order 必须小于所有会改写输入的 request 插件；建议 `-1000000`，failurePolicy 使用 `delegate`。管理页面与接口自动注册，不配置 management binding。

安装前，用数据库所有者连接宿主所在的同一 database，为插件创建独立角色；连接变量仅供这条准备命令使用，插件从握手取得连接串。`scripts/provision.sql` 不写入密码，建立 `prompt_audit` schema 并限制宿主视图的列权限。插件首次启动自动迁移审计 schema，迁移表独立于宿主；整库恢复后须重新准备角色和授权。

```sh
psql "$HOST_DATABASE_URL" -v audit_role=prompt_audit -v audit_password="$AUDIT_DATABASE_PASSWORD" -f scripts/provision.sql
```

真实端到端验收使用 Docker、上述宿主二进制与前端、正式插件包，以及 Playwright Chromium。下面的命令创建独立 PG／Redis 和可控协议上游，完成认证、安装、HTTP／SSE／WebSocket 采集落库、管理接口与同一 UI 的真实页桥验证；输出证据目录并清理自己创建的服务。`--keep` 以当前前台进程保留宿主供远程审阅，监听 `0.0.0.0:4180`，打印页面地址及仅供测试的登录信息，按 Ctrl+C 停止并清理。已有服务占用默认端口时，可指定 `--postgres-port`、`--redis-port`、`--port`。

```sh
npx playwright install chromium
node scripts/integration.js \
  --host-binary ../codex-proxy-audit-host/backend/target/debug/codex-proxy-rs \
  --host-web ../codex-proxy-audit-host/frontend/dist \
  --package dist/packages/huwei.prompt-audit-0.1.5-x86_64-unknown-linux-gnu.tar.gz \
  --keep
```

UI 使用 Vue 3 与官方组件库 [`@codex-proxy/ui` v0.3.2](https://github.com/zyycn/codex-proxy-ui/tree/v0.3.2)，参照 [#278](https://github.com/zyycn/codex-proxy-rs/issues/278) 中的插件示例接入组件、样式和主题。在 Node.js 24+ 环境下，于仓库根目录运行：

```sh
npm ci
npm run demo
```

远程浏览器访问 `http://<服务器 IP>:4173`；服务监听 `0.0.0.0` 的所有 IPv4 网卡，按 `Ctrl+C` 停止。页面默认筛选宿主当天；Demo 按宿主默认的 `Asia/Shanghai` 模拟系统时间，并在启动时将示例整体平移到服务器当前时间；“重置”可查看全部示例。可搜索“数据库”或“请继续”，组合时间与 Key 缩小范围，再打开一条输入阅读上文。支持每页最多 50 / 100 / 200 条、前后翻页、单行预览、上下方向键连续审阅和 Esc 关闭详情；Demo 外层的主题按钮模拟宿主切换浅色与深色主题，插件本身沿用宿主主题。Thread ID 与 Session ID 分别保留在记录信息中，不提供会话归类入口。

`npm run build` 生成正式插件静态资源到 `dist/plugin/`，使用宿主页桥、宿主主题和经典脚本；生成的 `management.json` 供后端注册页面、资源及五个查询入口。正式构建必须在宿主中打开，不会回退到模拟数据。UI 统一提供“复制审计 ID”，不提供记录链接或 URL 查询恢复。

`npm run demo` 先构建正式插件，再启动模拟宿主；模拟宿主读取上述注册信息，将同一份页面资源加载进隔离 iframe，通过 v2 管理页桥转发到模拟后端。没有独立的 Demo UI 构建。修改 UI 后重新运行此命令；`npm run preview` 直接加载已有构建。可用 `npm run demo -- --time-zone America/New_York` 模拟其他宿主部署时区，浏览器时区不影响展示和筛选。依赖版本和完整性由 `package-lock.json` 固定。

[脱敏 Session 固定样本](tests/fixtures/codex-sessions.json)来自本地 Codex 的真实多轮用户／Agent 对话，保留 `commentary`、`final_answer` 与逐条输入的前序快照；原始日志、工具输出和推理内容不入样本。路径、项目名、地址、标识符及凭据统一替换，Key 与 thread/session/message 身份采用合成值。日志时间仅用于 mock 展示，不代表网关记录时间。`npm test` 与 `npm run test:ui` 均验证这份固定样本；可用下面的命令打开同一 UI，或从本地日志重新生成样本到忽略提交的 `.local/`：

```sh
npm run demo -- --fixtures tests/fixtures/codex-sessions.json
node scripts/generate-session-fixtures.js --sessions "$HOME/.codex/sessions" --output .local/session-fixtures.json --limit 40 --days 2 --min-agent-context 1 --max-input-characters 2400 --max-record-characters 12000
npm run preview -- --fixtures .local/session-fixtures.json
```

要把固定样本载入已启动的隔离验收宿主，运行下面的命令。脚本登录测试管理员，创建或复用名为 `Codex Sessions · 脱敏样本` 的专用 Client Key，把用户输入及明确的 Agent 历史提交到 `/v1/responses`，再用管理接口核验落库正文与上下文；相同样本重复执行会分别记录。页面可按这个 Key 筛选。默认密码仅适用于上面的测试宿主，其他测试密码通过 `AUDIT_SEED_ADMIN_PASSWORD` 传入；无需数据库连接串。

```sh
mkdir -p .local
node scripts/seed-session-fixtures.js --base-url http://127.0.0.1:4180 --fixture tests/fixtures/codex-sessions.json > .local/seed-result.json
node tests/integration/verify-session-fixtures.js .local/seed-result.json
node tests/integration/update-review-host.js dist/packages/huwei.prompt-audit-0.1.5-x86_64-unknown-linux-gnu.tar.gz .local/seed-result.json
```

查询契约的语义见[设计文档 §5.3](docs/design.md#53-管理-api)，字段以 `contracts/audit.js` 为源；修改后运行 `npm run contract:export` 更新 OpenAPI。前端验证命令为 `npm run contract:check`、`npm test` 和 `npm run test:ui`。浏览器验证首次运行需执行 `npx playwright install chromium`，也可通过 `PLAYWRIGHT_CHROMIUM_EXECUTABLE` 指定已有 Chromium；测试自动使用 4174 端口，不占用供审阅的 4173。这组浏览器测试使用模拟服务；正式宿主验收使用上面的 `scripts/integration.js`。

Rust 检查与真实数据库测试命令如下。常规 `cargo test` 不执行带 `ignore` 的 PG 测试；显式运行时，`AUDIT_TEST_DATABASE_URL` 必须指向隔离测试库的管理角色，拥有创建测试 database／role 的权限。storage 测试自行创建及删除测试库；query 测试需要先在目标库执行 v3.23.0 的宿主迁移，不用于生产库。

```sh
cargo fmt --manifest-path backend/Cargo.toml --check
cargo clippy --manifest-path backend/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path backend/Cargo.toml --locked
AUDIT_TEST_DATABASE_URL="$ISOLATED_TEST_DATABASE_URL" cargo test --manifest-path backend/Cargo.toml --test storage -- --ignored
AUDIT_TEST_DATABASE_URL="$ISOLATED_HOST_TEST_DATABASE_URL" cargo test --manifest-path backend/Cargo.toml --test query -- --ignored
```

| 文档 | 职责 |
| --- | --- |
| [总体设计](docs/design.md) | 已核实的插件合同、模块边界、Client Key 关联、PG 存储、页面行为及查询契约语义 |
| [OpenAPI 契约](contracts/openapi.json) | 从共享 Schema 导出的请求参数与响应结构，供后端实现对接 |
| [采集行为](docs/collection.md) | 采集阶段、子代理过滤、最新用户输入识别、请求内上下文及异步写入边界 |

采集侧故障、队列与退出时的数据边界见[采集行为 §7](docs/collection.md#7-审计任务与异步写入)。页面读取错误与重试行为属于查询契约。
