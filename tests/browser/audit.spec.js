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

test('dense search, historical Key selection, cursor pages, and plugin-only navigation', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  await page.goto('/'); await ready(ui);
  await expect(ui.locator('tbody tr')).toHaveCount(50);
  await expect(ui.locator('thead th')).toHaveText(['记录时间', 'Client Key', '用户输入']);
  await expect(ui.locator('.detail-panel')).toHaveCount(0);
  await ui.getByRole('combobox', { name: 'Client Key', exact: true }).click();
  await ui.getByRole('option').filter({ hasText: 'ck_platform' }).click();
  await search(ui, '数据库');
  await expect(ui.locator('tbody tr').first().locator('mark')).toHaveText('数据库');
  for (const row of await ui.locator('tbody tr').all()) await expect(row.locator('.key-cell')).toContainText('ck_platform');
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
  await ui.locator('#date-from').fill('2026-10-09'); await ui.locator('#date-to').fill('2026-10-08');
  await ui.getByRole('button', { name: '检索', exact: true }).click();
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
  await expect(ui.locator('.metadata')).toContainText('模型、推理强度与 UA 取自宿主记录');
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
  await expect(ui.locator('.metadata')).toContainText('来源请求记录不可用');
  await expect(ui.locator('.input-card .body-text')).toContainText('请整理这次接口评审');
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

test('host theme propagation and mobile detail remain usable', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  await page.goto('/'); await ready(ui);
  await page.getByRole('button', { name: '切换深色主题' }).click();
  await expect(ui.locator('html')).toHaveAttribute('data-theme', 'dark');
  await page.setViewportSize({ width: 390, height: 844 });
  expect(await ui.locator('html').evaluate((root) => root.scrollWidth <= innerWidth)).toBe(true);
  await ui.locator('[data-audit="PA-0018"]').click();
  await expect(ui.locator('.input-card')).toBeVisible();
  await ui.locator('.metadata').scrollIntoViewIfNeeded();
  await expect(ui.locator('.metadata dd').first()).toBeVisible();
  expect(await ui.locator('.detail-scroll').evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(true);
  await ui.getByRole('button', { name: '关闭详情' }).click();
  await expect(ui.locator('.detail-panel')).toHaveCount(0);
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
  await expect(ui.locator('#date-from')).toHaveValue('2026-10-09');
  await expect(ui.locator('#date-to')).toHaveValue('2026-10-09');
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
  await expect(ui.locator('.metadata time')).toHaveText(rowTime);
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
