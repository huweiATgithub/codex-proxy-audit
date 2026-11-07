import { test, expect } from '@playwright/test';

const syntaxUrl = 'https://docs.rs/regex/latest/regex/#syntax';
const failures = new WeakMap();
test.beforeEach(async ({ page }) => {
  const errors = [];
  failures.set(page, errors);
  page.on('pageerror', (error) => errors.push(error.message));
});
test.afterEach(async ({ page }) => { expect(failures.get(page)).toEqual([]); });

async function openSettings(page) {
  const ui = page.frameLocator('iframe');
  await page.goto('/');
  await expect(ui.locator('#result-count')).toBeVisible();
  await ui.getByRole('button', { name: '设置', exact: true }).click();
  await ui.getByRole('button', { name: '添加规则', exact: true }).click();
  await ui.getByRole('textbox', { name: 'UA 正则 1', exact: true }).fill('^draft/help/');
  return ui;
}

function trackDataRequests(page) {
  const reads = [], writes = [];
  page.on('request', (request) => {
    if (new URL(request.url()).pathname === '/__demo/api/audits') reads.push(request);
    if (request.method() === 'PUT') writes.push(request);
  });
  return { reads, writes };
}

test('regex help uses host confirmation for cancel, copy and open without changing the draft or audit query', async ({ page, context }) => {
  await page.addInitScript(() => {
    if (window.parent === window) Object.defineProperty(navigator, 'clipboard', { value: { writeText: (text) => {
      if (window.deferClipboard) return new Promise((_, reject) => { window.rejectClipboard = () => reject(new Error('Clipboard unavailable')); });
      window.copiedLink = text;
      return Promise.resolve();
    } } });
  });
  await context.route('https://docs.rs/regex/latest/regex/**', (route) => route.fulfill({ contentType: 'text/html', body: '<title>Regex syntax fixture</title>' }));
  const { reads, writes } = trackDataRequests(page);
  const ui = await openSettings(page);
  const count = reads.length;
  const help = ui.getByRole('link', { name: 'Rust regex 语法', exact: true });
  const confirmation = page.getByRole('dialog', { name: '打开外部链接', exact: true });
  await help.click();
  await expect(confirmation).toBeVisible();
  await expect(confirmation).toContainText('docs.rs');
  await page.screenshot({ path: '/tmp/codex-audit-open-external-demo.png', fullPage: true });
  await confirmation.getByRole('button', { name: '取消', exact: true }).click();
  await expect(confirmation).not.toBeVisible();
  await help.click();
  await confirmation.getByRole('button', { name: '复制链接', exact: true }).click();
  await expect.poll(() => page.evaluate(() => window.copiedLink)).toBe(syntaxUrl);
  await expect(confirmation).toBeVisible();
  const opened = page.waitForEvent('popup');
  await confirmation.getByRole('button', { name: '打开链接', exact: true }).click();
  const popup = await opened;
  await expect(popup).toHaveURL(syntaxUrl);
  expect(await popup.evaluate(() => window.opener)).toBeNull();
  await popup.close();
  await expect(confirmation).not.toBeVisible();
  await help.click();
  await page.evaluate(() => { window.deferClipboard = true; });
  await confirmation.getByRole('button', { name: '复制链接', exact: true }).click();
  await confirmation.getByRole('button', { name: '取消', exact: true }).click();
  await help.click();
  await page.evaluate(() => window.rejectClipboard());
  await expect(confirmation.locator('#external-host')).toHaveText('docs.rs');
  await confirmation.getByRole('button', { name: '取消', exact: true }).click();
  await expect(confirmation).not.toBeVisible();
  await expect(ui.getByRole('textbox', { name: 'UA 正则 1', exact: true })).toHaveValue('^draft/help/');
  await expect(ui.locator('.exclusion-status')).toContainText('有未保存修改');
  await expect(ui.locator('.exclusion-editor > [role="alert"]')).toHaveCount(0);
  expect(reads).toHaveLength(count);
  expect(writes).toEqual([]);
});

for (const capability of ['rejected', 'missing']) {
  test(`regex help preserves the draft and exposes a selectable address when the host capability is ${capability}`, async ({ page }) => {
    if (capability === 'rejected') await page.setViewportSize({ width: 390, height: 844 });
    await page.addInitScript((mode) => {
      if (mode === 'rejected' && window.parent === window) {
        window.addEventListener('message', (event) => {
          if (event.data?.target !== 'codex-proxy-plugin-management' || event.data.type !== 'open-external') return;
          event.stopImmediatePropagation();
          event.source.postMessage({ ...event.data, type: 'result', ok: false, error: 'Host rejected external link' }, '*');
        });
      }
      if (mode === 'missing' && window.parent !== window) {
        // Install an older v2 bridge fixture before its immutable property is published.
        const define = Object.defineProperty;
        Object.defineProperty = (target, key, descriptor) => {
          if (target === window && key === 'codexProxyPlugin') {
            const api = { ...descriptor.value };
            delete api.openExternal;
            Object.defineProperty = define;
            return define(target, key, { ...descriptor, value: Object.freeze(api) });
          }
          return define(target, key, descriptor);
        };
      }
    }, capability);
    const { reads, writes } = trackDataRequests(page);
    const ui = await openSettings(page);
    const count = reads.length;
    await ui.getByRole('link', { name: 'Rust regex 语法', exact: true }).click();
    const error = ui.locator('.exclusion-editor > [role="alert"]');
    await expect(error).toContainText(capability === 'missing' ? '当前宿主不支持打开外部链接' : '无法请求打开链接');
    await expect(error.locator('code')).toHaveText(syntaxUrl);
    const address = await error.locator('code').boundingBox();
    expect(address.x).toBeGreaterThanOrEqual(0);
    expect(address.x + address.width).toBeLessThanOrEqual(page.viewportSize().width);
    await expect(page.getByRole('dialog', { name: '打开外部链接', exact: true })).not.toBeVisible();
    await expect(ui.getByRole('textbox', { name: 'UA 正则 1', exact: true })).toHaveValue('^draft/help/');
    expect(reads).toHaveLength(count);
    expect(writes).toEqual([]);
    if (capability === 'rejected') await page.screenshot({ path: '/tmp/codex-audit-open-external-rejected.png', fullPage: true });
  });
}

test('the Demo bridge requires a click, validates HTTPS addresses and accepts only one pending confirmation', async ({ page }) => {
  const ui = await openSettings(page);
  const plugin = page.frames().find((frame) => frame.parentFrame());
  await page.getByRole('button', { name: '切换深色主题', exact: true }).click();
  expect(await plugin.evaluate(() => window.codexProxyPlugin.openExternal('https://example.invalid/').then(() => 'accepted', (error) => error.message))).toBe('请在插件页面中点击链接或按钮');
  await plugin.evaluate(() => {
    const probe = document.createElement('button');
    probe.id = 'external-probe'; probe.textContent = 'External bridge probe';
    probe.onclick = () => {
      window.externalResults = [];
      for (const url of window.probeUrls) window.codexProxyPlugin.openExternal(url).then(() => window.externalResults.push('accepted'), (error) => window.externalResults.push(error.message));
    };
    document.querySelector('.exclusion-editor').append(probe);
  });
  for (const url of ['http://example.invalid/', '/relative', 'https://user:password@example.invalid/']) {
    await plugin.evaluate((url) => { window.probeUrls = [url]; }, url);
    await ui.locator('#external-probe').click();
    await expect.poll(() => plugin.evaluate(() => window.externalResults)).toEqual(['外部链接地址无效']);
  }
  await plugin.evaluate(() => { window.probeUrls = ['https://example.invalid:8443/path', 'https://example.invalid/second']; });
  await ui.locator('#external-probe').click();
  await expect.poll(() => plugin.evaluate(() => window.externalResults)).toEqual(['accepted', '已有链接等待确认']);
  const confirmation = page.getByRole('dialog', { name: '打开外部链接', exact: true });
  await expect(confirmation).toContainText('example.invalid:8443');
  await expect(confirmation).toBeVisible();
  await confirmation.getByRole('button', { name: '取消', exact: true }).click();
});
