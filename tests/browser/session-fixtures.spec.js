import { test, expect } from '@playwright/test';
import { spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const fixturePath = fileURLToPath(new URL('../fixtures/codex-sessions.json', import.meta.url));
const fixture = JSON.parse(readFileSync(fixturePath, 'utf8'));
const text = (blocks) => blocks.filter((block) => block.kind === 'text').map((block) => block.text).join('\n');
let server;

test.beforeAll(async () => {
  server = spawn(process.execPath, ['demo/server.js', '--host', '127.0.0.1', '--port', '4175', '--fixtures', fixturePath], {
    env: { ...process.env, AUDIT_DEMO_NOW: '2026-10-09T04:00:00.000Z' }, stdio: ['ignore', 'pipe', 'pipe'],
  });
  await new Promise((resolve, reject) => {
    let stderr = '';
    server.stderr.on('data', (data) => { stderr += data; });
    server.once('error', reject);
    server.once('exit', (code) => reject(new Error(`Session fixture server exited (${code}): ${stderr}`)));
    server.stdout.on('data', (data) => { if (data.toString().includes('Prompt Audit demo:')) resolve(); });
  });
});
test.afterAll(async () => {
  if (server && server.exitCode === null) {
    const exited = new Promise((resolve) => server.once('exit', resolve));
    server.kill('SIGTERM');
    await exited;
  }
});

test('sanitized session fixture renders actual Agent text through the existing lazy detail flow', async ({ page }) => {
  const selected = fixture.audits.find((audit) => audit.input.length === 1 && audit.input[0].kind === 'text'
    && Array.from(text(audit.input)).length <= 1200
    && audit.context.slice(-3).some((message) => message.role === 'assistant' && message.phase === 'final_answer'
      && message.blocks.length === 1 && Array.from(text(message.blocks)).length > 0 && Array.from(text(message.blocks)).length <= 1200));
  expect(selected).toBeTruthy();
  const agent = selected.context.slice(-3).find((message) => message.role === 'assistant' && message.phase === 'final_answer'
    && message.blocks.length === 1 && Array.from(text(message.blocks)).length > 0 && Array.from(text(message.blocks)).length <= 1200);
  const requested = [];
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('request', (request) => { if (request.url().includes('/__demo/')) requested.push(new URL(request.url())); });
  const ui = page.frameLocator('iframe');
  await page.goto('http://127.0.0.1:4175/');
  await expect(ui.locator('#result-count')).toHaveText(/\d+ 条/);
  expect(requested.some((url) => url.pathname.endsWith('audit-content'))).toBe(false);
  await ui.getByRole('button', { name: '重置', exact: true }).click();
  await expect(ui.locator('#result-count')).toHaveText(`${fixture.audits.length} 条`);
  await ui.locator(`[data-audit="${selected.id}"]`).click();
  await expect(ui.locator('.input-card .body-text')).toContainText(text(selected.input));
  await expect(ui.locator('.context-section .message-phase').filter({ hasText: '最终答复' }).first()).toBeVisible();
  await expect.poll(async () => await ui.locator('.context-section .body-text').allTextContents()).toContain(text(agent.blocks));
  await expect(ui.locator('.metadata')).toBeVisible();
  expect(errors).toEqual([]);
});
