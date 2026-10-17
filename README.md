# Codex Proxy Audit

Codex Proxy RS 的 Prompt 审计插件：按 Client Key 记录经过网关的用户输入，以及理解这次输入所需的有限前序对话。例如，用户说“请继续”时，审计者可以同时看到 Agent 此前提出的计划。

采集排除能够识别的子代理请求，前序对话以 Agent 文本消息为重点，忽略工具调用、工具结果和推理内容。审计数据使用插件私有 SQLite 数据库，面向单宿主、单插件实例；管理员通过插件提供的审计页面查看记录。审计记录固定在记录时间满 3 天（72 小时）后过期，不提供保留期配置；插件启动后立即清理，随后每天检查，每批删除最多 1,000 条过期输入及其前序对话。正常运行时清理可延迟约一天，因此实际留存约 3～4 天。插件停用或数据库不可用期间清理暂停，恢复后继续；具体调度与磁盘空间语义见[总体设计 §4.1](docs/design.md#41-插件私有数据库)。

审计元数据保留 thread/session ID、消息级 turn ID、逐块输入类型及来源请求报告的仓库根路径、remote URLs。UA、请求模型与 reasoning effort 仅在管理员点击“加载请求信息”后，通过来源请求 ID 和 Key ID 查询宿主已有记录，不重复存入审计数据库；打开详情和读取正文不查询这三项。宿主请求记录清理后这三项不可用，输入和前序对话按审计自身的 72 小时保留期清理。详情优先展示用户输入和前序对话，“记录信息”在其后默认显示，Client Key 与其他元数据同级排列；请求信息独立显示加载、失败及重试状态，不影响正文阅读。外层表格保持记录时间、Client Key 与用户输入三列。具体取值和排除范围见采集行为，宿主查询接口与边界见总体设计 §4。

本仓库包含 [Rust 插件后端](backend/)、[前后端查询契约](contracts/openapi.json)和按契约读取数据的 [UI](frontend/)。后端在 request 阶段检查最后一个输入项，仅为可采集的用户消息提交审计任务；前序对话只取本次请求自带的内容。任务进入固定容量、满时覆盖最旧任务的环形缓冲区，由 worker 异步写入 SQLite，不等待写入确认。重复请求分别记录，后续失败或取消的请求也可能留下记录，尚未落库的任务可能丢失。后端自管数据库初始化、管理查询及共享时区与 UA 排除设置接口。UI 支持高密度表格、用户输入正文关键词与时间、Key 组合筛选、游标分页及按需读取的详情和前序对话。[Demo](demo/)默认提供 500 条虚构输入，也可加载脱敏 Session 样本，供独立审阅页面；模拟服务不承担真实后端验收。

管理员可打开审计页右上角的“设置”，在“排除规则”分区编辑“UA 排除规则”，按行填写 [Rust regex](https://docs.rs/regex/latest/regex/#syntax) 正则表达式并显式保存。任一规则匹配入站 User-Agent 时，本插件跳过该请求的审计记录和前序对话保存；原请求照常转发，headers 与正文保持原样。规则默认空列表，保存在插件私有数据库；保存空列表清除自定义规则，保存失败保留已生效规则。设置与既有来源过滤共同决定采集资格，适用于后续采集，已排队任务及已有记录保持各自原有决定。具体匹配语义与限制见[采集行为 §3.2](docs/collection.md#32-整条请求过滤)。

宿主兼容声明为 `~3.28.0`，接受稳定版 `>=3.28.0, <3.29.0`；SDK 与可复现构建基线固定到 upstream v3.28.0（`447869860a0fb88f78109cdb700db0cb1eb5d8b5`）。

Client Key 名称在列表、详情与筛选查询时直接从宿主当前 Key 列表取得，不写入审计记录或名称目录。改名后，旧记录也显示新名称；已删除的 Key 显示“名称未知”，输入与上下文仍按保留策略保存。名称查询失败会返回可重试错误。

审计时区由插件在私有数据库的 `settings` 表中保存为共享、非 secret 的 IANA `timeZone`，缺省为 `UTC`。管理员可在“设置”的“显示设置”分区填写时区，或点击“使用浏览器时区”填入浏览器检测结果；检测不可用时填入 `UTC`。只有点击“保存”才写入共享设置，所有管理员使用该值。每次查询读取一次已保存时区，并在整个响应内用于时间展示、服务端当天日期与日期边界。保存不刷新、取消或重试查询，不改变已选择的日期；列表和详情分别标明各自响应使用的时区。

插件通过 SDK 的 `HostClient::dispatch_http` 按需读取宿主请求详情，私有 SQLite 只负责保存审计数据，依据见[总体设计 §4](docs/design.md#4-插件私有-sqlite-的存储设计)。插件静态链接 SQLite 3.54.0；为使用尚未进入已发布 Rust 绑定的这一版本，SQLx 与 `libsqlite3-sys` 在 [Cargo.toml](backend/Cargo.toml) 中固定到兼容的上游提交，并由 `Cargo.lock` 锁定依赖。在仓库根目录准备宿主及插件产物：

```sh
git clone --branch v3.28.0 --depth 1 https://github.com/zyycn/codex-proxy-rs ../codex-proxy-audit-host
CPR_VERSION=3.28.0 cargo +1.97.0 build --manifest-path ../codex-proxy-audit-host/backend/Cargo.toml --locked -p codex-proxy-rs -p codex-proxy-plugin-cli
(cd ../codex-proxy-audit-host/frontend && corepack pnpm install --frozen-lockfile && corepack pnpm build)
npm ci
npm run plugin:build
CPR_PLUGIN=../codex-proxy-audit-host/backend/target/debug/cpr-plugin npm run plugin:package
```

正式安装包与校验文件从 [GitHub Releases](https://github.com/huweiATgithub/codex-proxy-audit/releases) 下载；本地构建输出 `dist/packages/huwei.prompt-audit-0.1.13-x86_64-unknown-linux-gnu.tar.gz`。宿主 CLI 生成资源声明、规范化能力和文件摘要，包内包含 MIT 许可证。同一版本的平台产物在宿主中不可变，修改后发布新的插件版本并使用实例的版本切换入口升级；升级保留原实例的 configuration 和 binding，审计事实继续保存在数据库中。正式包支持 Linux x86_64 GNU，要求 glibc 2.34 或更新版本；其他目标先通过 Cargo 的 `--target` 构建，再用 `AUDIT_TARGET`、`AUDIT_BINARY` 指定目标和二进制。`plugin.json` 只声明 middleware v4／management v1。宿主安装页面上传此包，接受受信任进程，configuration 保持 `{}`，secrets 保持为空。启用 request binding，order 必须小于所有会改写输入的 request 插件；建议 `-1000000`，failurePolicy 使用 `delegate`。管理页面与接口自动注册，不配置 management binding。

[CI](.github/workflows/ci.yml) 在 pull request 和 `main` 推送时自动执行契约检查、JavaScript 与浏览器测试、Rust fmt／Clippy／测试、正式插件打包及真实宿主验收（含 125 秒下游等待）。宿主版本从 `plugin.json` 的兼容声明读取，打包 CLI 使用 Cargo 中固定的 SDK 提交；验收前核对宿主 tag 与该提交一致，并校验下载的宿主包。构建检查 ELF 架构与 glibc 要求，安装包和验收结果保存在 Actions artifacts。

正式发布由 [Release](.github/workflows/release.yml) 自动完成：更新 `plugin.json`、`backend/Cargo.toml` 及 `backend/Cargo.lock` 中的插件版本，提交到 `main` 后，推送匹配的稳定版 tag（`vX.Y.Z`）。例如下一个版本为 `0.1.14` 时，执行 `git tag -a v0.1.14 -m "Release v0.1.14"` 和 `git push origin v0.1.14`。版本不一致会停止构建；全部 CI 检查通过后，工作流使用同次运行验收过的安装包，创建带自动生成说明的 GitHub Release，上传安装包、校验文件、MIT 许可证、验收结果与截图，再验证公开下载。工作流只需 GitHub 自动提供的 `GITHUB_TOKEN`；构建与测试只有读取权限，发布 job 才有 `contents: write`。失败后可在 Actions 中重跑；已发布的附件只校验，不覆盖。发布不安装或升级生产实例。

插件初始化时自动创建 `<宿主持久数据目录>/huwei.prompt-audit/audit.sqlite3` 及所需表。数据根目录由宿主设定的启动目录 `<宿主持久数据目录>/plugins/plugin-*` 向上两级定位，数据库位于固定子目录，重启和重新解包均复用同一文件。目录不可配置；安装无需准备数据库角色、连接串或额外数据库服务。该目录应随宿主持久数据区一起保留。

真实端到端验收使用 Docker、上述宿主二进制与前端、正式插件包，以及 Playwright Chromium。下面的命令为宿主创建独立 PG／Redis 和可控协议上游，插件自行创建 SQLite 数据库，完成认证、安装、HTTP／SSE／WebSocket 采集落库、管理接口与同一 UI 的真实页桥验证；输出证据目录并清理自己创建的服务。脚本自动重启宿主并重新解包插件，验证新的启动目录仍复用固定数据库，核对审计事实与正文、共享时区和游标签名密钥保留，并验证启动清理与按需加载请求信息。`--rust-tests` 在完成真实页面验证后，执行使用临时 SQLite 文件的 Rust storage 与 query 检查；`--host-commit` 将构建使用的 commit 写入验收证据。可选 `--next-wait-ms 125000` 让受控上游延迟响应头，验证等待宿主 `next` 超过 120 秒时模型请求仍未完成、审计查询继续可用，随后请求正常返回；默认跳过这项长等待检查。该测试请求的客户端总超时仍为指定等待时长加 30 秒，其他请求保持 20 秒上限。`--keep` 以当前前台进程保留宿主供远程审阅，监听 `0.0.0.0:4180`，打印页面地址及仅供测试的登录信息，按 Ctrl+C 停止并清理。已有服务占用默认端口时，可指定 `--postgres-port`、`--redis-port`、`--port`。

```sh
npx playwright install chromium
node scripts/integration.js \
  --host-binary ../codex-proxy-audit-host/backend/target/debug/codex-proxy-rs \
  --host-web ../codex-proxy-audit-host/frontend/dist \
  --package dist/packages/huwei.prompt-audit-0.1.13-x86_64-unknown-linux-gnu.tar.gz \
  --next-wait-ms 125000 \
  --keep
```

UI 使用 Vue 3 与官方组件库 [`@codex-proxy/ui` v0.3.2](https://github.com/zyycn/codex-proxy-ui/tree/v0.3.2)，参照 [#278](https://github.com/zyycn/codex-proxy-rs/issues/278) 中的插件示例接入组件、样式和主题。在 Node.js 24+ 环境下，于仓库根目录运行：

```sh
npm ci
npm run demo
```

远程浏览器访问 `http://<服务器 IP>:4173`；服务监听 `0.0.0.0` 的所有 IPv4 网卡，按 `Ctrl+C` 停止。时间范围控件将“今天”“最近三天”“全部”和“自定义”放在同一单选组中，快捷项点击即查询，自定义范围在一个日历面板内选定后应用。“最近三天”包括今天及前两个自然日，按插件时区和服务端日期计算；检索或刷新相对范围时重新取得当天日期，翻页沿用该次查询范围。页面默认筛选已保存审计时区的服务端当天；Demo 默认使用 `UTC`，并在启动时将示例整体平移到服务器当前时间；“重置”可查看全部示例。可搜索“数据库”或“请继续”，组合时间与 Key 缩小范围，再打开一条输入阅读上文。支持每页最多 50 / 100 / 200 条、前后翻页、单行预览、上下方向键连续审阅和 Esc 关闭详情；Demo 外层的主题按钮模拟宿主切换浅色与深色主题，插件本身沿用宿主主题。Thread ID 与 Session ID 分别保留在记录信息中，不提供会话归类入口。

`npm run build` 生成正式插件静态资源到 `dist/plugin/`，使用宿主页桥、宿主主题和经典脚本；生成的 `management.json` 供后端注册页面、资源、管理查询与共享设置入口。正式构建必须在宿主中打开，不会回退到模拟数据。UI 统一提供“复制审计 ID”，不提供记录链接或 URL 查询恢复。

`npm run demo` 先构建正式插件，再启动模拟宿主；模拟宿主读取上述注册信息，将同一份页面资源加载进隔离 iframe，通过 v2 管理页桥转发到模拟后端。没有独立的 Demo UI 构建。修改 UI 后重新运行此命令；`npm run preview` 直接加载已有构建。可用 `npm run demo -- --time-zone America/New_York` 指定 Demo 的初始审计时区，页面可显式保存其他时区。依赖版本和完整性由 `package-lock.json` 固定。

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
node tests/integration/update-review-host.js dist/packages/huwei.prompt-audit-0.1.13-x86_64-unknown-linux-gnu.tar.gz .local/seed-result.json
```

查询契约的语义见[设计文档 §5.3](docs/design.md#53-管理-api)，字段以 `contracts/audit.js` 为源；修改后运行 `npm run contract:export` 更新 OpenAPI。前端验证命令为 `npm run contract:check`、`npm test` 和 `npm run test:ui`。浏览器验证首次运行需执行 `npx playwright install chromium`，也可通过 `PLAYWRIGHT_CHROMIUM_EXECUTABLE` 指定已有 Chromium；测试自动使用 4174 端口，不占用供审阅的 4173。这组浏览器测试使用模拟服务；正式宿主验收使用上面的 `scripts/integration.js`。

Rust 检查与真实数据库测试命令如下。storage 与 query 测试自行创建和清理临时 SQLite 文件，随常规 `cargo test` 执行，无需外部数据库连接或管理权限。

```sh
cargo fmt --manifest-path backend/Cargo.toml --check
cargo clippy --manifest-path backend/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path backend/Cargo.toml --locked
```

| 文档 | 职责 |
| --- | --- |
| [总体设计](docs/design.md) | 已核实的插件合同、模块边界、Client Key 关联、私有 SQLite 存储、页面行为及查询契约语义 |
| [OpenAPI 契约](contracts/openapi.json) | 从共享 Schema 导出的请求参数与响应结构，供后端实现对接 |
| [采集行为](docs/collection.md) | 采集阶段、子代理过滤、最新用户输入识别、请求内上下文及异步写入边界 |

采集侧故障、队列与退出时的数据边界见[采集行为 §7](docs/collection.md#7-审计任务与异步写入)。页面读取错误与重试行为属于查询契约。
