import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { chromium, expect } from '@playwright/test';
import { sessionFixtureRequests } from '../../scripts/seed-session-fixtures.js';

// Read-only acceptance of sanitized session data in the actual host page. This
// uses its authentication, resource assembly and iframe bridge, never Demo APIs.
const seedPath = process.argv[2];
if (!seedPath) throw new Error('Usage: node tests/integration/verify-session-fixtures.js /path/seed-result.json [screenshot.png]');
const seed = JSON.parse(readFileSync(resolve(seedPath), 'utf8'));
const fixture = sessionFixtureRequests(JSON.parse(readFileSync('tests/fixtures/codex-sessions.json', 'utf8')));
const screenshot = resolve(process.argv[3] || '/tmp/codex-audit-session-fixtures-review.png');
const base = new URL(seed.pageUrl).origin;
const browser = await chromium.launch({ headless: true, ...(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE } : {}) });
try {
  const context = await browser.newContext({ viewport: { width: 1600, height: 1100 } });
  const login = await context.request.post(`${base}/api/auth/login`, { data: { mode: 'admin', password: process.env.AUDIT_SEED_ADMIN_PASSWORD || 'audit-integration-password-2026' } });
  assert.equal(login.status(), 200, 'Test host authentication');
  const page = await context.newPage(), errors = [], requests = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('request', (request) => requests.push(request.url()));
  await page.goto(seed.pageUrl);
  const ui = page.frameLocator('iframe');
  await expect(ui.locator('#result-count')).toBeVisible({ timeout: 30000 });
  await ui.getByRole('combobox', { name: 'Client Key', exact: true }).click();
  await ui.getByRole('option').filter({ hasText: seed.keyName }).click();
  await ui.getByRole('button', { name: '检索', exact: true }).click();
  await expect(ui.locator('#result-count')).toHaveText(`${seed.total} 条`);
  assert.deepEqual(await ui.locator('thead th').allTextContents(), ['记录时间', 'Client Key', '用户输入']);
  const directory = await context.request.get(`${base}/api/admin/plugins/extensions`);
  const views = (await directory.json()).data;
  const instanceId = new URL(seed.pageUrl).pathname.split('/')[2];
  const target = views.find((view) => view.target.instanceId === instanceId).target;
  const newestId = await ui.locator('[data-audit]').first().getAttribute('data-audit');
  const newestResponse = await context.request.get(`${base}/api/admin/plugins/extensions/${target.instanceId}/${target.artifactSha256}/${target.revision}/api/api/audit?${new URLSearchParams({ id: newestId })}`);
  assert.equal(newestResponse.status(), 200);
  assert.ok((await newestResponse.json()).context.some((message) => message.role === 'assistant'), 'Default newest fixture record has an Agent predecessor');
  await ui.locator(`[data-audit="${seed.verifiedAssistant.auditId}"]`).click();
  await expect(ui.locator('.input-card .body-text').first()).toBeVisible();
  const fixtureAgentTexts = fixture.requests.flatMap((request) => request.context.filter((message) => message.role === 'assistant').flatMap((message) => message.blocks.map((block) => block.text)));
  await expect(async () => {
    const visible = await ui.locator('.context-section .agent-message .body-text').allTextContents();
    assert.ok(visible.some((text) => text.length > 10 && fixtureAgentTexts.some((original) => original.startsWith(text))), 'Visible Agent text is an exact sanitized-session excerpt');
    assert.ok(!visible.some((text) => text === 'Agent 方案：先检查数据库，再执行迁移。😀'), 'Controlled upstream generic reply was not inserted into fixture history');
  }).toPass({ timeout: 15000 });
  await expect(ui.locator('.metadata')).toBeVisible();
  assert.equal(await page.locator('iframe').getAttribute('sandbox'), 'allow-scripts');
  assert.ok(requests.some((url) => url.includes('/api/api/audit-content')));
  assert.ok(!requests.some((url) => url.includes('__demo')));
  assert.deepEqual(errors, []);
  await page.screenshot({ path: screenshot, fullPage: true });
  console.log(JSON.stringify({ verified: true, keyName: seed.keyName, inputCount: seed.total, newestAuditId: newestId, reviewedAuditId: seed.verifiedAssistant.auditId, screenshot, pageUrl: seed.pageUrl }, null, 2));
} finally {
  await browser.close();
}
