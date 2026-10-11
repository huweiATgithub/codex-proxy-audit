// Fictional observations for design review. No gateway or user data is loaded.
export const keys = [
  { id: 'ck_platform', name: '平台工程' },
  { id: 'ck_product', name: '产品设计' },
  { id: 'ck_research', name: '研究环境' },
];

const text = (value) => ({ kind: 'text', text: value, inputKind: 'user.text' });
const user = (value) => ({ role: 'user', blocks: [text(value)] });
const agent = (value, phase = 'final_answer', extra = {}) => ({ role: 'assistant', blocks: [text(value)], phase, ...extra });

function record(id, time, keyId, threadId, input, context, extra = {}) {
  return {
    id: `PA-${String(id).padStart(4, '0')}`,
    firstObservedAt: `${time}-04:00`,
    keyId,
    threadId,
    sessionId: null,
    requestId: `req_demo_${String(id).padStart(4, '0')}`,
    turnId: threadId ? `turn_demo_${id}` : null,
    repositories: threadId ? [{ rootPath: '/workspace/prompt-audit', remotes: [
      { name: 'origin', url: 'git@example.com:demo/prompt-audit.git' },
      { name: 'upstream', url: 'https://example.com/upstream/prompt-audit.git' },
    ] }] : null,
    input: [text(input)],
    context,
    ...extra,
  };
}

const initialRequest = '我们需要给项目增加一个审计页面。请先检查现有方案，提出实施计划，暂时不要修改代码。';
const plan = '我建议把审阅过程分成三个步骤：\n\n1. 找到输入：按正文关键词、时间和 Client Key 筛选，列表只显示必要的预览。\n2. 理解意图：突出本次用户输入，在下方保留此前 Agent 提出的计划和必要的用户消息。\n3. 连续审阅：保留列表位置，支持逐条阅读和返回列表。\n\n先制作可交互的页面原型，用实际阅读体验确定布局；确认后再接入审计查询接口。';
const firstContext = [user(initialRequest), agent('我会先梳理“找到记录 → 阅读输入 → 理解上文”这条审阅路径。', 'commentary'), agent(plan)];
const afterFirst = [...firstContext, user('请继续。'), agent('第一版原型已完成：左侧为输入列表，右侧为本次输入和前序对话。下一步检查正文搜索与时间、Key 的组合筛选，再验证分页和长正文阅读。', 'final_answer')];
const longInput = Array.from({ length: 16 }, (_, i) => `${i + 1}. 审阅要求：本次输入需要完整保留。列表可以使用有限预览，但详情中的长文本应当按原有顺序分段读取。页面必须区分“尚未加载的正文”和“采集时已经截断的前序上下文”；继续读取只能取得已保存的内容，不能恢复没有采集的部分。请保留原文中的换行、编号和引号，不生成模型摘要。`).join('\n\n');

const examples = [
  record(18, '2026-10-08T14:42:18', 'ck_platform', 'thread_web_01', '请继续。', afterFirst, { sessionId: 'session_demo_web_01' }),
  record(17, '2026-10-08T14:28:06', 'ck_product', 'thread_design_02', '这个方向可以。把信息密度再降一点，优先让我看到用户说了什么。', [user('请帮我调整审计页面的信息层级。'), agent('建议保留输入列表与阅读区域，将关联 ID 收进“记录信息”。正文区域优先显示用户输入和前序对话。')]),
  record(16, '2026-10-08T14:15:42', 'ck_research', null, '按刚才讨论的方案执行。', []),
  record(15, '2026-10-08T13:56:11', 'ck_platform', 'thread_migration_03', '先检查迁移脚本，不要执行生产环境变更。', [user('我们需要审查数据库迁移方案。'), agent('我会检查迁移顺序、约束变化和回滚条件，列出需要确认的问题。')]),
  record(14, '2026-10-08T13:32:09', 'ck_product', 'thread_image_04', '参考这张截图调整布局，保留左右分栏。', [agent('请提供你希望参考的页面，我会先分析信息组织。')], { input: [text('参考这张截图调整布局，保留左右分栏。'), { kind: 'nontext', mediaType: '图片', description: '用户附加的界面截图', inputKind: 'user.image' }] }),
  record(13, '2026-10-08T12:48:37', 'ck_platform', 'thread_long_05', '可以，按这个拆分继续实现。', [user('请分析页面需要哪些模块。'), agent('……\n\n保留的方案末尾：\n\n四、上下文阅读\n按原顺序保留用户与 Agent 文本，用角色标识区分。较早消息因窗口限制舍弃时，需要解释缺失原因。\n\n五、记录信息\n提供稳定 Key ID、当前名称、记录时间与来源请求 ID。\n\n六、实现顺序\n先验证页面交互，再接入管理 API。', 'final_answer', { truncated: true })]),
  record(12, '2026-10-08T12:20:03', 'ck_legacy', 'thread_legacy_06', '请整理这次接口评审中还没有解决的问题。', [agent('接口评审还剩两个问题：长正文如何分段，以及历史 Key 如何在筛选中保留。')]),
  record(11, '2026-10-07T11:54:29', 'ck_platform', 'thread_web_01', initialRequest, []),
  record(10, '2026-10-08T11:32:14', 'ck_research', 'thread_unknown_07', '请检查这个输入是否被正确识别。', [agent('当前客户端没有提供消息内容分类，审计时需要保留这个限制。', undefined)], { input: [{ kind: 'text', text: '请检查这个输入是否被正确识别。' }], turnId: null, repositories: [] }),
  record(9, '2026-10-08T10:48:52', 'ck_platform', 'thread_spec_08', longInput, [user('我想明确长文本的阅读方式。'), agent('请列出需要保留的要求。我会在列表预览和详情正文之间建立明确的边界。')]),
  record(8, '2026-10-08T10:15:10', 'ck_product', 'thread_branch_09', '在这个分支里，我们先只看窄屏阅读体验。', [user('页面需要兼顾大屏与窄屏。'), agent('大屏可以并排展示列表和详情；窄屏建议进入单独的详情视图，并提供返回列表的入口。')]),
  record(7, '2026-10-08T09:35:21', 'ck_platform', 'thread_safe_10', '请把下面的示例当作普通文本展示：\n\n<script>alert("audit")</script>\n<img src=x onerror=alert(1)>\n\n不要执行这些 HTML 标签。', [agent('我会按原文保留示例中的标签与换行。')]),
  record(6, '2026-10-08T09:15:12', 'ck_platform', 'thread_web_01', '请继续。', firstContext),
  record(5, '2026-10-07T16:45:39', 'ck_platform', 'thread_names_11', '请确认改名后，旧的审计记录还能通过同一个 Key 找到。', [agent('筛选与关联使用稳定 Key ID。查询时使用宿主当前名称，改名后旧记录也显示新名称。')]),
  record(4, '2026-10-07T15:08:44', 'ck_product', 'thread_design_02', '保留原文，不要自动生成对话摘要。', [agent('为了方便审阅，可以在列表显示输入预览，在详情保留原始对话。')]),
  record(3, '2026-10-06T14:10:30', 'ck_research', 'thread_gap_12', '按上面第二种方案处理。', [agent('我会保留能够观察到的部分。', 'commentary')]),
  record(2, '2026-10-06T10:03:00', 'ck_legacy', 'thread_legacy_06', '先列出接口清单，暂时不要实现。', []),
  record(1, '2026-10-05T09:12:05', 'ck_platform', 'thread_start_13', '我们开始设计 Prompt 审计页面。', []),
];

// A deterministic fictional workload for scanning and paging, not gateway throughput.
const prompts = [
  '检查数据库迁移脚本的顺序，列出可能影响现有记录的约束变更。',
  '修复 CI 中的构建失败，先定位触发失败的最小改动。',
  '把 API 超时的复现步骤整理出来，保留请求 ID 和错误信息。',
  '请评审这个 PR，重点检查权限边界和错误处理。',
  '给审计列表增加关键词搜索，匹配处需要高亮。',
  '继续完成已经确认的修改，并运行相关测试。',
  '比较两种缓存策略，说明失效条件与并发情况下的行为。',
  '数据库查询越来越慢，先分析索引和执行计划。',
  '这段错误提示不够清楚，请说明用户可以怎么处理。',
  '检查生产部署流程中的回滚步骤，暂时不要执行部署。',
  '重构重复的配置解析逻辑，保留现有命令行参数。',
  '请确认日志里没有明文凭据，再提交修复。',
  '把表格行高压缩一些，让同屏可以审阅更多输入。',
  '这次只改文档，说明 WebSocket 重连后如何继续请求。',
  '给分页接口补上稳定排序，时间相同时使用 ID 区分。',
  '分析这条用户输入需要哪些前序上下文，不要生成摘要。',
  '核对历史 Key 改名后是否还能检索旧记录。',
  '先查看最近一次测试失败的日志，再决定是否修改代码。',
  '检查 PostgreSQL 连接池耗尽的原因，给出复现方案。',
  '把说明改成中文，保留代码示例中的字段名称。',
  '处理空结果页面，不要把没有记录显示成加载失败。',
  '确认用户连续两次说“请继续”时会保留两个输入。',
  '审查文件下载接口，确认错误状态码没有被重写。',
  '保留这段 HTML 示例的原文：<button onclick="alert(1)">确认</button>。',
];
const workload = Array.from({ length: 482 }, (_, index) => {
  const key = keys[index % keys.length];
  const seconds = 14 * 3600 + 41 * 60 - index * 37;
  const time = [Math.floor(seconds / 3600), Math.floor(seconds / 60) % 60, seconds % 60].map((value) => String(value).padStart(2, '0')).join(':');
  const prompt = prompts[index % prompts.length];
  const threadId = index % 13 === 4 ? null : `thread_demo_${index}`;
  const context = index % 19 === 7 ? [] : [agent('我会先核对现有实现与约束，再给出需要修改的内容和验证结果。', 'commentary')];
  return record(1001 + index, `2026-10-08T${time}`, key.id, threadId, prompt, context);
});

export const audits = [...examples, ...workload].sort((a, b) => b.firstObservedAt.localeCompare(a.firstObservedAt) || b.id.localeCompare(a.id));

// Separate host-owned request rows. Historical audit bodies outlive these rows.
export const modelRequests = audits.filter((audit) => audit.keyId !== 'ck_legacy').map((audit) => ({
  id: audit.requestId, keyId: audit.keyId,
  userAgent: 'codex_cli_rs/0.160.1 (Linux; x86_64) xterm-256color',
  model: 'demo-model', reasoningEffort: 'high',
}));
