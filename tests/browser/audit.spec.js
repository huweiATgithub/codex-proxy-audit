import { test, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';

const errors = new WeakMap();
test.beforeEach(async ({ page }) => {
  const failures = [];
  errors.set(page, failures);
  page.on('pageerror', (error) => failures.push(error.message));
  page.on('dialog', async (dialog) => { failures.push(dialog.message()); await dialog.dismiss(); });
});
test.afterEach(async ({ page }) => { expect(errors.get(page)).toEqual([]); });
const ready = async (page, count = '494 条') => expect(page.locator('#result-count')).toHaveText(count);
const search = async (page, text) => { await page.getByRole('searchbox').fill(text); await page.getByRole('button', { name: '检索', exact: true }).click(); };
const serviceError = (code, message) => ({ error: { code, message } });
const rgb = (hex) => `rgb(${[1, 3, 5].map((offset) => Number.parseInt(hex.slice(offset, offset + 2), 16)).join(', ')})`;

test('dense search, current Key selection, cursor pages, and plugin-only navigation', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  await page.goto('/'); await ready(ui);
  await expect(ui.locator('tbody tr')).toHaveCount(50);
  await expect(ui.locator('thead th')).toHaveText(['记录时间', 'Client Key', '用户输入']);
  await expect(ui.locator('.detail-panel')).toHaveCount(0);
  await ui.getByRole('combobox', { name: 'Client Key', exact: true }).click();
  await ui.getByRole('option').filter({ hasText: 'ck_platform' }).click();
  await search(ui, '数据库');
  await expect(ui.locator('tbody tr').first().locator('mark')).toHaveText('数据库');
  for (const row of await ui.locator('tbody tr').all()) {
    await expect(row.locator('.key-cell')).toHaveText('平台工程');
    await expect(row.locator('.key-cell')).toHaveAttribute('title', '平台工程');
  }
  await ui.locator('[data-audit]').first().click();
  await expect(ui.locator('.input-card .body-text')).toContainText('数据库');
  await expect(ui.getByRole('button', { name: '复制审计 ID' })).toBeVisible();
  await expect(page).toHaveURL('http://127.0.0.1:4174/');
  await page.reload(); await ready(ui);
  await expect(ui.locator('.detail-panel')).toHaveCount(0);
  await expect(ui.getByRole('searchbox')).toHaveValue('');
  await ui.getByRole('button', { name: '重置', exact: true }).click(); await ready(ui, '500 条');
  await ui.getByRole('button', { name: '下一页', exact: true }).click();
  await expect(ui.locator('.cursor-pagination')).toContainText('第 2 页');
  expect((await ui.locator('tbody tr').first().boundingBox()).y).toBeGreaterThanOrEqual((await ui.locator('thead').boundingBox()).y + (await ui.locator('thead').boundingBox()).height - 1);
  await ui.getByRole('button', { name: '上一页', exact: true }).click();
  await expect(ui.locator('.cursor-pagination')).toContainText('第 1 页');
  await ui.locator('[data-audit]').nth(49).click();
  await page.keyboard.press('ArrowDown');
  await expect(ui.locator('.cursor-pagination')).toContainText('第 2 页');
  await expect(ui.locator('.input-card')).toBeVisible();
  await page.keyboard.press('Escape'); await expect(ui.locator('.detail-panel')).toHaveCount(0);
  await ui.getByRole('combobox', { name: '每页条数' }).click();
  await ui.getByRole('option', { name: '100 条/页', exact: true }).click();
  await expect(ui.locator('tbody tr')).toHaveCount(100);
  await ui.getByRole('button', { name: '自定义', exact: true }).click();
  await ui.locator('#date-from').fill('2026-10-09'); await ui.locator('#date-to').fill('2026-10-08');
  await expect(ui.getByRole('button', { name: '应用', exact: true })).toBeDisabled();
  await expect(ui.getByRole('alert')).toContainText('起始日期不能晚于');
});

test('details fetch bodies lazily, preserve literal HTML, and read long input in chunks', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  const requested = [];
  page.on('request', (request) => { if (request.url().includes('/__demo/')) requested.push(new URL(request.url())); });
  await page.goto('/'); await ready(ui);
  expect(requested.some((url) => url.pathname.endsWith('audit-content'))).toBe(false);
  await ui.locator('[data-audit="PA-0018"]').click();
  await expect(ui.locator('.input-card .body-text')).toHaveText('请继续。');
  await expect(ui.locator('.detail-scroll > :first-child')).toHaveClass('input-card');
  await expect(ui.locator('.metadata dd').first()).toHaveText('平台工程 · ck_platform');
  await expect(ui.locator('.metadata dd').first()).toBeVisible();
  await expect(ui.locator('.context-section .body-text')).toHaveCount(3);
  expect(requested.some((url) => url.searchParams.get('contentId') === 'context-0')).toBe(false);
  await ui.getByRole('button', { name: '展开更早的 2 条消息' }).click();
  await expect(ui.locator('.context-section .body-text')).toHaveCount(5);
  await expect(ui.locator('.metadata')).toContainText('thread_web_01');
  await expect(ui.locator('.metadata')).toContainText('session_demo_web_01');
  await expect(ui.locator('.metadata')).toContainText('turn_demo_18');
  await expect(ui.locator('.metadata')).toContainText('块 1 · user.text');
  await expect(ui.locator('.metadata')).toContainText('/workspace/prompt-audit');
  await expect(ui.locator('.metadata')).toContainText('origin · git@example.com:demo/prompt-audit.git');
  await expect(ui.locator('.metadata')).toContainText('upstream · https://example.com/upstream/prompt-audit.git');
  expect(requested.some((url) => url.pathname.endsWith('audit-request-metadata'))).toBe(false);
  await expect(ui.locator('.request-metadata dt')).toHaveCount(0);
  await ui.getByRole('button', { name: '加载请求信息', exact: true }).click();
  await expect(ui.locator('.metadata')).toContainText('请求信息取自宿主记录');
  await expect(ui.locator('.metadata')).toContainText('demo-model');
  await expect(ui.locator('.metadata')).toContainText('codex_cli_rs/0.160.1');
  await search(ui, '审阅要求');
  await ui.locator('[data-audit="PA-0009"]').click();
  await expect(ui.locator('.input-card .body-progress')).toContainText('已读取 1200');
  await ui.getByRole('button', { name: '继续读取正文', exact: true }).click();
  await expect(ui.locator('.input-card .body-progress')).toContainText('正文已完整读取');
  await search(ui, '<script>');
  await ui.locator('[data-audit="PA-0007"]').click();
  await expect(ui.locator('.input-card .body-text')).toContainText('<script>alert');
  await expect(ui.locator('.input-card script, .input-card img')).toHaveCount(0);
  await expect(ui.locator('.input-card mark')).toHaveText('<script>');
  await search(ui, '参考这张截图');
  await ui.locator('[data-audit="PA-0014"]').click();
  await expect(ui.locator('.attachment')).toContainText('非文本内容未采集');
  await expect(ui.locator('.metadata')).toContainText('块 2 · user.image');
  await search(ui, '请整理这次接口评审');
  await ui.locator('[data-audit="PA-0012"]').click();
  await expect(ui.locator('.metadata')).not.toContainText('来源请求记录不可用');
  await ui.getByRole('button', { name: '加载请求信息', exact: true }).click();
  await expect(ui.locator('.metadata')).toContainText('来源请求记录不可用');
  await expect(ui.locator('.request-metadata button')).toHaveCount(0);
  await expect(ui.locator('.input-card .body-text')).toContainText('请整理这次接口评审');
});

test('request metadata loads once per open detail and prevents duplicate pending calls', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  const calls = [];
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  await page.route('**/__demo/api/audit-request-metadata?**', async (route) => {
    calls.push(new URL(route.request().url()).searchParams.get('id'));
    if (calls.length === 1) await gate;
    await route.continue();
  });
  await page.goto('/'); await ready(ui);
  await ui.locator('[data-audit="PA-0018"]').click();
  await expect(ui.locator('.input-card .body-text')).toHaveText('请继续。');
  await expect(ui.locator('.context-section .body-text')).toHaveCount(3);
  await expect(ui.locator('.request-metadata dt')).toHaveCount(0);
  expect(calls).toEqual([]);
  try {
    await ui.getByRole('button', { name: '加载请求信息', exact: true }).click();
    const loading = ui.getByRole('button', { name: '正在加载请求信息…', exact: true });
    await expect(loading).toBeDisabled();
    await loading.evaluate((button) => { button.click(); button.click(); });
    await expect(ui.locator('.request-metadata')).toHaveAttribute('aria-busy', 'true');
    expect(calls).toEqual(['PA-0018']);
  } finally { release(); }
  await expect(ui.locator('.request-metadata dd')).toHaveText(['demo-model', 'high', '演示上游账号', 'codex_cli_rs/0.160.1 (Linux; x86_64) xterm-256color']);
  await expect(ui.locator('.request-metadata button')).toHaveCount(0);
  await ui.getByRole('button', { name: '关闭详情' }).click();
  await ui.locator('[data-audit="PA-0018"]').click();
  await expect(ui.locator('.input-card .body-text')).toHaveText('请继续。');
  await expect(ui.getByRole('button', { name: '加载请求信息', exact: true })).toBeVisible();
  await expect(ui.locator('.request-metadata dt')).toHaveCount(0);
  expect(calls).toEqual(['PA-0018']);
  await ui.getByRole('button', { name: '加载请求信息', exact: true }).click();
  await expect(ui.locator('.request-metadata dd').first()).toHaveText('demo-model');
  expect(calls).toEqual(['PA-0018', 'PA-0018']);
});

for (const scenario of [
  { name: 'different models', model: 'requested-model', upstreamModel: 'upstream-model', accountName: '上游账号 A', different: true },
  { name: 'matching models', model: 'requested-model', upstreamModel: 'requested-model', accountName: '上游账号 A', different: false },
  { name: 'unknown upstream model and account', model: 'requested-model', upstreamModel: null, accountName: null, different: false },
  { name: 'unknown requested model', model: null, upstreamModel: 'upstream-model', accountName: '上游账号 A', different: false },
]) {
  test(`request metadata compares models and displays the upstream account: ${scenario.name}`, async ({ page }, testInfo) => {
    const ui = page.frameLocator('iframe');
    await page.route('**/__demo/api/audit-request-metadata?**', (route) => route.fulfill({ json: {
      auditId: new URL(route.request().url()).searchParams.get('id'),
      requestMetadata: { model: scenario.model, upstreamModel: scenario.upstreamModel, accountName: scenario.accountName, reasoningEffort: 'high', userAgent: 'test-agent' },
    } }));
    await page.goto('/'); await ready(ui);
    await ui.locator('[data-audit="PA-0018"]').click();
    await expect(ui.locator('.request-metadata dt')).toHaveCount(0);
    await ui.getByRole('button', { name: '加载请求信息', exact: true }).click();
    await expect(ui.locator('.request-metadata .metadata-field:has(dt:text-is("请求模型")) dd')).toHaveText(scenario.model || '未取得');
    await expect(ui.locator('.request-metadata .metadata-field:has(dt:text-is("上游账号")) dd')).toHaveText(scenario.accountName || '未取得');
    await expect(ui.getByText('（与请求模型不同）', { exact: true })).toHaveCount(scenario.different ? 1 : 0);
    await expect(ui.locator('.request-metadata dt').getByText('上游模型', { exact: true })).toHaveCount(scenario.different ? 1 : 0);
    if (scenario.different) {
      await expect(ui.locator('.request-metadata .metadata-field:has(dt:text-is("上游模型")) dd')).toHaveText('upstream-model （与请求模型不同）');
      await ui.locator('.request-metadata').scrollIntoViewIfNeeded();
      await ui.locator('.detail-panel').screenshot({ path: testInfo.outputPath('request-metadata-light.png') });
      await page.getByRole('button', { name: '切换深色主题' }).click();
      await ui.locator('.detail-panel').screenshot({ path: testInfo.outputPath('request-metadata-dark.png') });
      await page.setViewportSize({ width: 390, height: 844 });
      await ui.locator('.request-metadata').scrollIntoViewIfNeeded();
      expect(await ui.locator('.detail-scroll').evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(true);
      await ui.locator('.detail-panel').screenshot({ path: testInfo.outputPath('request-metadata-mobile.png') });
    }
  });
}

test('request metadata errors retry inline while input and context stay usable', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  let calls = 0;
  await page.route('**/__demo/api/audit-request-metadata?**', async (route) => {
    if (++calls === 1) return route.fulfill({ status: 503, json: serviceError('unavailable', '宿主请求信息暂时不可用') });
    return route.continue();
  });
  await page.goto('/'); await ready(ui);
  await ui.locator('[data-audit="PA-0018"]').click();
  await expect(ui.locator('.input-card .body-text')).toHaveText('请继续。');
  await ui.getByRole('button', { name: '加载请求信息', exact: true }).click();
  await expect(ui.locator('.request-metadata [role="alert"]')).toContainText('宿主请求信息暂时不可用');
  await expect(ui.locator('.input-card .body-text')).toHaveText('请继续。');
  await expect(ui.locator('.context-section .body-text')).toHaveCount(3);
  await ui.getByRole('button', { name: '展开更早的 2 条消息' }).click();
  await expect(ui.locator('.context-section .body-text')).toHaveCount(5);
  await ui.getByRole('button', { name: '重试请求信息', exact: true }).click();
  await expect(ui.locator('.request-metadata [role="alert"]')).toHaveCount(0);
  await expect(ui.locator('.request-metadata dd').first()).toHaveText('demo-model');
  expect(calls).toBe(2);
});

test('late request metadata from a previous detail never appears in the current detail', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  const calls = [];
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  await page.route('**/__demo/api/audit-request-metadata?**', async (route) => {
    const id = new URL(route.request().url()).searchParams.get('id');
    calls.push(id);
    if (id === 'PA-0018') {
      await gate;
      return route.fulfill({ json: { auditId: id, requestMetadata: { model: 'old-model', upstreamModel: 'old-upstream', accountName: 'old-account', reasoningEffort: 'low', userAgent: 'old-agent' } } });
    }
    return route.fulfill({ json: { auditId: id, requestMetadata: { model: 'current-model', upstreamModel: 'current-model', accountName: 'current-account', reasoningEffort: 'high', userAgent: 'current-agent' } } });
  });
  await page.goto('/'); await ready(ui);
  await ui.locator('[data-audit="PA-0018"]').click();
  await expect(ui.locator('.input-card .body-text')).toHaveText('请继续。');
  try {
    await ui.getByRole('button', { name: '加载请求信息', exact: true }).click();
    await expect(ui.getByRole('button', { name: '正在加载请求信息…', exact: true })).toBeDisabled();
    await search(ui, '审阅要求');
    await ui.locator('[data-audit="PA-0009"]').click();
    await expect(ui.locator('.input-card .body-text')).toContainText('审阅要求');
    await expect(ui.getByRole('button', { name: '加载请求信息', exact: true })).toBeVisible();
    expect(calls).toEqual(['PA-0018']);
    await ui.getByRole('button', { name: '加载请求信息', exact: true }).click();
    await expect(ui.locator('.request-metadata dd')).toHaveText(['current-model', 'high', 'current-account', 'current-agent']);
    const complete = page.waitForResponse((response) => response.url().includes('/audit-request-metadata?') && new URL(response.url()).searchParams.get('id') === 'PA-0018');
    release(); await complete;
    await expect(ui.locator('.request-metadata dd')).toHaveText(['current-model', 'high', 'current-account', 'current-agent']);
    await expect(ui.locator('.metadata')).not.toContainText('old-model');
    await expect(ui.locator('.metadata')).not.toContainText('old-upstream');
    await expect(ui.locator('.metadata')).not.toContainText('old-account');
    expect(calls).toEqual(['PA-0018', 'PA-0009']);
  } finally { release(); }
});

test('list, detail and chunk failures stay distinct from empty data and can retry', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  let failList = true, failDetail = true, failChunk = true;
  await page.route('**/__demo/api/**', async (route) => {
    const url = new URL(route.request().url());
    let failure = false;
    if (url.pathname.endsWith('/audits') && failList) { failList = false; failure = true; }
    else if (url.pathname.endsWith('/audit') && failDetail) { failDetail = false; failure = true; }
    else if (url.pathname.endsWith('/audit-content') && url.searchParams.get('contentId') === 'input' && failChunk) { failChunk = false; failure = true; }
    if (failure) return route.fulfill({ status: 503, json: serviceError('unavailable', '暂时不可用') });
    return route.continue();
  });
  await page.goto('/');
  await expect(ui.getByRole('alert')).toContainText('暂时不可用');
  await expect(ui.getByText('没有匹配的输入', { exact: true })).toHaveCount(0);
  await ui.getByRole('button', { name: '重试', exact: true }).click(); await ready(ui);
  await ui.locator('[data-audit="PA-0018"]').click();
  await expect(ui.locator('.detail-panel')).toContainText('详情读取失败');
  await ui.getByRole('button', { name: '重试详情' }).click();
  await expect(ui.locator('.input-card [role="alert"]')).toContainText('暂时不可用');
  await ui.getByRole('button', { name: '重试读取' }).click();
  await expect(ui.locator('.input-card .body-text')).toHaveText('请继续。');
  await page.route('**/__demo/api/audits?**', async (route) => {
    if (new URL(route.request().url()).searchParams.has('cursor')) return route.fulfill({ status: 410, json: serviceError('cursor_expired', '分页位置已失效') });
    return route.continue();
  });
  await ui.getByRole('button', { name: '关闭详情' }).click();
  await ui.getByRole('button', { name: '下一页', exact: true }).click();
  await ui.getByRole('button', { name: '重新检索', exact: true }).click(); await ready(ui);
  await page.route('**/__demo/api/audit?**', (route) => route.fulfill({ status: 404, json: serviceError('not_found', '记录已被清理') }));
  await ui.locator('[data-audit="PA-0018"]').click();
  await expect(ui.locator('.detail-panel')).toContainText('记录不可用');
});

test('a slow old query never replaces newer results', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  await page.route('**/__demo/api/audits?**', async (route) => {
    if (new URL(route.request().url()).searchParams.get('q') === '数据库') await gate;
    await route.continue();
  });
  await page.goto('/'); await ready(ui);
  const pending = page.waitForRequest((request) => new URL(request.url()).searchParams.get('q') === '数据库');
  await search(ui, '数据库'); await pending;
  await search(ui, 'CI');
  await expect(ui.locator('tbody tr').first()).toContainText('CI');
  const complete = page.waitForResponse((response) => new URL(response.url()).searchParams.get('q') === '数据库');
  release(); await complete;
  await expect(ui.locator('tbody tr').first()).toContainText('CI');
  await expect(ui.locator('.active-filter')).toHaveText('正文包含“CI”');
});

test('settings sits beside refresh without duplicating the host title and stays accessible on narrow screens', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  await page.goto('/'); await ready(ui);
  await expect(ui.getByRole('heading', { level: 1 })).toHaveCount(0);
  await expect(ui.getByRole('heading', { name: 'Prompt 审计', exact: true })).toHaveCount(0);
  await expect(ui.getByText('Codex Proxy', { exact: true })).toHaveCount(0);
  const settings = ui.getByRole('button', { name: '设置', exact: true });
  const refresh = ui.getByRole('button', { name: '刷新列表', exact: true });
  const dialog = ui.getByRole('dialog', { name: '设置', exact: true });
  for (const viewport of [{ width: 1440, height: 900 }, { width: 390, height: 844 }]) {
    await page.setViewportSize(viewport);
    await expect(settings).toBeInViewport();
    await expect(refresh).toBeInViewport();
    const gear = await settings.boundingBox(), reload = await refresh.boundingBox();
    const gap = reload.x - (gear.x + gear.width);
    expect(gap).toBeGreaterThanOrEqual(0);
    expect(gap).toBeLessThanOrEqual(12);
    expect(Math.abs(gear.y + gear.height / 2 - reload.y - reload.height / 2)).toBeLessThanOrEqual(2);
    expect(Math.abs(gear.width - reload.width)).toBeLessThanOrEqual(2);
    await settings.focus();
    await settings.press('Enter');
    await expect(dialog).toBeVisible();
    await expect(dialog.getByRole('heading', { name: '显示设置', exact: true })).toBeVisible();
    await dialog.getByRole('button', { name: '关闭', exact: true }).click();
    await expect(settings).toBeFocused();
    await refresh.focus();
    const refreshed = page.waitForResponse((response) => new URL(response.url()).pathname === '/__demo/api/audits');
    await refresh.press('Enter'); await refreshed; await ready(ui);
  }
});

test('host theme colors reach the page, table and settings modal while mobile layouts remain usable', async ({ page, request }) => {
  const ui = page.frameLocator('iframe');
  const { themes } = await (await request.get('/plugin.json')).json();
  const dialog = ui.getByRole('dialog', { name: '设置', exact: true });
  const checkColors = async (theme) => {
    const tokens = themes[theme].tokens;
    await expect(ui.locator('html')).toHaveCSS('color', rgb(tokens['--cp-color-text']));
    await expect(ui.locator('html')).toHaveCSS('background-color', rgb(tokens['--cp-color-bg-layout']));
    await expect(ui.locator('body')).toHaveCSS('background-color', rgb(tokens['--cp-color-bg-layout']));
    await expect(ui.locator('.records-panel')).toHaveCSS('background-color', rgb(tokens['--cp-color-bg-container']));
    await expect(ui.locator('table')).toHaveCSS('color', rgb(tokens['--cp-color-text']));
    if (await dialog.count()) {
      await expect(dialog).toBeVisible();
      await expect(dialog).toHaveCSS('background-color', rgb(tokens['--cp-modal-bg']));
      await expect(dialog).toHaveCSS('color', rgb(tokens['--cp-color-text']));
    }
  };
  await page.goto('/'); await ready(ui);
  await checkColors('light');
  await ui.getByRole('button', { name: '设置', exact: true }).click();
  await expect(dialog).toBeVisible();
  await checkColors('light');
  await page.getByRole('button', { name: '切换深色主题' }).click();
  await checkColors('dark');
  await page.getByRole('button', { name: '切换浅色主题' }).click();
  await checkColors('light');
  await dialog.getByRole('button', { name: '关闭', exact: true }).click();
  await page.setViewportSize({ width: 390, height: 844 });
  expect(await ui.locator('html').evaluate((root) => root.scrollWidth <= innerWidth)).toBe(true);
  await ui.locator('[data-audit="PA-0018"]').click();
  await expect(ui.locator('.input-card')).toBeVisible();
  await ui.locator('.metadata').scrollIntoViewIfNeeded();
  await expect(ui.locator('.metadata dd').first()).toBeVisible();
  expect(await ui.locator('.detail-scroll').evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(true);
  await ui.getByRole('button', { name: '关闭详情' }).click();
  await expect(ui.locator('.detail-panel')).toHaveCount(0);
  await ui.getByRole('button', { name: '设置', exact: true }).click();
  await expect(dialog).toBeVisible();
  await checkColors('light');
  const bounds = await dialog.boundingBox();
  const viewport = await ui.locator('html').evaluate(() => ({ width: innerWidth, height: innerHeight }));
  expect(bounds.width).toBeGreaterThan(250);
  expect(bounds.height).toBeGreaterThan(250);
  expect(bounds.x).toBeGreaterThanOrEqual(0);
  expect(bounds.y).toBeGreaterThanOrEqual(0);
  expect(bounds.x + bounds.width).toBeLessThanOrEqual(viewport.width);
  expect(bounds.y + bounds.height).toBeLessThanOrEqual(viewport.height);
});

test('the Demo follows the Host default theme prefix when a page omits its declaration', async ({ page, request }) => {
  const { themes } = await (await request.get('/plugin.json')).json();
  await page.route('**/plugin.json', async (route) => {
    const bundle = await (await route.fetch()).json();
    const entry = bundle.registration.pages[0].entry;
    const html = Buffer.from(bundle.resources[entry].body, 'base64').toString('utf8')
      .replace(/<meta\b[^>]*\bname=["']codex-proxy-ui-theme-prefix["'][^>]*>/gi, '');
    bundle.resources[entry].body = Buffer.from(html).toString('base64');
    await route.fulfill({ json: bundle });
  });
  const ui = page.frameLocator('iframe');
  await page.goto('/'); await ready(ui);
  await page.getByRole('button', { name: '切换深色主题' }).click();
  await expect.poll(() => ui.locator('html').evaluate((root) => root.style.getPropertyValue('--z-color-text'))).toBe(themes.dark.tokens['--cp-color-text']);
  expect(await ui.locator('html').evaluate((root) => root.style.getPropertyValue('--cp-color-text'))).toBe('');
});

test('demo loads exact production resources in isolation and uses server time facts', async ({ page, request }) => {
  await page.clock.setFixedTime(new Date('2035-01-01T00:00:00Z'));
  const bundle = await (await request.get('/plugin.json')).json();
  const registration = JSON.parse(readFileSync(new URL('../../dist/plugin/management.json', import.meta.url), 'utf8'));
  expect(bundle.registration).toEqual(registration);
  for (const { path } of registration.resources) {
    expect(Buffer.from(bundle.resources[path].body, 'base64')).toEqual(readFileSync(new URL('../../dist/plugin/' + path, import.meta.url)));
  }
  const ui = page.frameLocator('iframe');
  const requests = [];
  page.on('request', (entry) => { if (entry.url().includes('/__demo/')) requests.push(entry); });
  await page.goto('/?q=ignored&audit=PA-0009&theme=dark'); await ready(ui);
  await expect(page.locator('iframe')).toHaveAttribute('sandbox', 'allow-scripts');
  await expect(ui.locator('meta[http-equiv="Content-Security-Policy"]')).toHaveAttribute('content', /connect-src 'none'/);
  await expect(ui.locator('html')).toHaveAttribute('data-theme', 'light');
  await expect(ui.getByRole('searchbox')).toHaveValue('');
  await ui.getByRole('button', { name: '自定义', exact: true }).click();
  await expect(ui.locator('#date-from')).toHaveValue('2026-10-09');
  await expect(ui.locator('#date-to')).toHaveValue('2026-10-09');
  await ui.getByRole('dialog', { name: '自定义时间范围', exact: true }).getByRole('button', { name: '取消', exact: true }).click();
  await expect(ui.locator('.timezone')).toContainText('Asia/Shanghai');
  await expect(ui.locator('tbody time').first()).toHaveText('2026-10-09 12:00:00');
  await expect(ui.locator('.detail-panel')).toHaveCount(0);
  await expect(ui.getByText('两行预览', { exact: true })).toHaveCount(0);
  await expect(ui.getByRole('button', { name: '复制链接' })).toHaveCount(0);
  await expect(ui.getByRole('button', { name: '切换深色主题' })).toHaveCount(0);
  await ui.getByRole('searchbox').fill('数据库');
  await ui.getByRole('searchbox').press('Enter');
  await expect(ui.locator('tbody tr').first().locator('mark')).toHaveText('数据库');
  await ui.locator('[data-audit]').first().click();
  await expect(ui.getByRole('button', { name: '复制审计 ID' })).toBeVisible();
  const rowTime = await ui.locator('tbody time').first().innerText();
  await expect(ui.locator('.metadata time')).toHaveText(`${rowTime} · Asia/Shanghai`);
  expect(requests.every((entry) => entry.frame() === page.mainFrame() && entry.method() === 'GET' && !new URL(entry.url()).searchParams.has('timeZone'))).toBe(true);
});

test('missing host time facts stop initialization, with explicit retry and no local fallback', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  let fail = true;
  const paths = [];
  page.on('request', (request) => { if (request.url().includes('/__demo/')) paths.push(new URL(request.url()).pathname); });
  await page.route('**/__demo/api/view-context?**', (route) => fail
    ? route.fulfill({ status: 503, json: serviceError('unavailable', '无法读取宿主时间设置') }) : route.continue());
  await page.goto('/');
  await expect(ui.getByRole('alert')).toContainText('无法读取宿主时间设置');
  expect(paths).toEqual(['/__demo/api/view-context']);
  await expect(ui.locator('#date-from')).toHaveCount(0);
  fail = false;
  await ui.getByRole('button', { name: '重试', exact: true }).click(); await ready(ui);
});

test('explicit save stays independent of pending queries and labels retained responses', async ({ page, request }) => {
  await request.put('/__demo/api/time-zone', { data: { timeZone: 'UTC' } });
  try {
    await page.context().addInitScript(() => {
      const original = Intl.DateTimeFormat;
      Intl.DateTimeFormat = function (locale, options) {
        const formatter = new original(locale, options);
        if (!locale && !options) formatter.resolvedOptions = () => ({ timeZone: 'Asia/Tokyo' });
        return formatter;
      };
    });
    const ui = page.frameLocator('iframe');
    const calls = [];
    page.on('request', (entry) => { if (entry.url().includes('/__demo/')) calls.push(entry); });
    await page.goto('/');
    await expect(ui.locator('tbody tr')).toHaveCount(50);
    await ui.locator('[data-audit="PA-0018"]').click();
    await expect(ui.locator('.metadata time')).toContainText('UTC');
    const dates = await ui.locator('.range-caption').textContent();
    const oldTime = await ui.locator('tbody time').first().innerText();
    const reads = calls.filter((entry) => entry.method() === 'GET').length;
    await ui.getByRole('button', { name: '设置', exact: true }).click();
    await expect(ui.locator('#time-zone')).toHaveValue('UTC');
    await ui.getByRole('button', { name: '使用浏览器时区', exact: true }).click();
    await expect(ui.locator('#time-zone')).toHaveValue('Asia/Tokyo');
    expect(calls.some((entry) => entry.method() === 'PUT')).toBe(false);
    expect((await (await request.get('/__demo/api/view-context')).json()).timeZone).toBe('UTC');
    await ui.getByRole('button', { name: '保存', exact: true }).click();
    await expect(ui.locator('.timezone-saved')).toContainText('Asia/Tokyo');
    expect(calls.filter((entry) => entry.method() === 'GET')).toHaveLength(reads);
    await expect(ui.locator('.timezone')).toContainText('UTC');
    await expect(ui.locator('tbody time').first()).toHaveText(oldTime);
    await expect(ui.locator('.metadata time')).toContainText('UTC');
    expect(await ui.locator('.range-caption').textContent()).toEqual(dates);
    await ui.locator('#time-zone').fill('Invalid/Zone');
    await ui.getByRole('button', { name: '保存', exact: true }).click();
    await expect(ui.locator('.timezone-settings [role="alert"]')).toContainText('时区无效');
    await expect(ui.locator('.timezone-saved')).toContainText('Asia/Tokyo');
    await ui.locator('#time-zone').fill('America/New_York');
    let release, captured;
    const snapshot = new Promise((resolve) => { captured = resolve; });
    const gate = new Promise((resolve) => { release = resolve; });
    await page.route('**/__demo/api/audits?**', async (route) => {
      const response = await route.fetch();
      captured();
      await gate;
      await route.fulfill({ response });
    });
    const started = page.waitForRequest((entry) => entry.url().includes('/__demo/api/audits'));
    await ui.getByRole('dialog', { name: '设置', exact: true }).getByRole('button', { name: '关闭', exact: true }).click();
    await ui.getByRole('button', { name: '刷新列表' }).click(); await started; await snapshot;
    const before = calls.filter((entry) => entry.method() === 'GET').length;
    await ui.getByRole('button', { name: '设置', exact: true }).click();
    await ui.getByRole('button', { name: '保存', exact: true }).click();
    await expect(ui.locator('.timezone-saved')).toContainText('America/New_York');
    expect(calls.filter((entry) => entry.method() === 'GET')).toHaveLength(before);
    release();
    await expect(ui.locator('.timezone')).toContainText('Asia/Tokyo');
    expect(await ui.locator('.range-caption').textContent()).toEqual(dates);
  } finally { await request.put('/__demo/api/time-zone', { data: { timeZone: 'Asia/Shanghai' } }); }
});

test('unavailable browser detection fills UTC without saving', async ({ page, request }) => {
  await page.addInitScript(() => {
    const original = Intl.DateTimeFormat;
    Intl.DateTimeFormat = function (locale, options) {
      const formatter = new original(locale, options);
      if (!locale && !options) formatter.resolvedOptions = () => ({});
      return formatter;
    };
  });
  const ui = page.frameLocator('iframe');
  await page.goto('/'); await ready(ui);
  await ui.getByRole('button', { name: '设置', exact: true }).click();
  await ui.getByRole('button', { name: '使用浏览器时区', exact: true }).click();
  await expect(ui.locator('#time-zone')).toHaveValue('UTC');
  expect((await (await request.get('/__demo/api/view-context')).json()).timeZone).toBe('Asia/Shanghai');
});
