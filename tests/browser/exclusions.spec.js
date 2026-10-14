import { test, expect } from '@playwright/test';

const failures = new WeakMap();
test.beforeEach(async ({ page }) => {
  const errors = [];
  failures.set(page, errors);
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('dialog', async (dialog) => { errors.push(dialog.message()); await dialog.dismiss(); });
});
test.afterEach(async ({ page }) => { expect(failures.get(page)).toEqual([]); });

const ready = async (ui) => expect(ui.locator('#result-count')).toHaveText('494 条');
const editor = (ui) => ui.locator('.exclusion-settings');
const openEditor = async (ui) => ui.getByRole('button', { name: '设置', exact: true }).click();
const patterns = (userAgentPatterns) => ({ userAgentPatterns });
const serviceError = (code, message, extra = {}) => ({ error: { code, message, ...extra } });
const auditReads = (page) => {
  const calls = [];
  page.on('request', (request) => {
    if (new URL(request.url()).pathname === '/__demo/api/audits') calls.push(request);
  });
  return calls;
};

test('settings starts with no UA rules and keeps drafts and record selection when reopened', async ({ page, request }) => {
  expect(await (await request.get('/__demo/api/exclusions')).json()).toEqual(patterns([]));
  const ui = page.frameLocator('iframe');
  const writes = [];
  page.on('request', (entry) => { if (entry.method() === 'PUT') writes.push(entry); });
  await page.goto('/'); await ready(ui);
  const trigger = ui.getByRole('button', { name: '设置', exact: true });
  const dialog = ui.getByRole('dialog', { name: '设置', exact: true });
  await expect(dialog).toHaveCount(0);
  await expect(ui.locator('#time-zone')).toHaveCount(0);
  const firstRecord = ui.locator('[data-audit]').first();
  await firstRecord.click();
  await openEditor(ui);
  await expect(dialog.getByRole('heading', { name: '显示设置', exact: true })).toBeVisible();
  const exclusions = dialog.getByRole('region', { name: '排除规则', exact: true });
  await expect(exclusions.getByRole('heading', { name: 'UA 排除规则', exact: true })).toBeVisible();
  await expect(exclusions).toContainText('匹配任意排除规则的请求将不记录。已有记录仍可查询，请求照常转发。');
  await expect(editor(ui).locator('.exclusion-status')).toContainText('已保存：0 条');
  await expect(editor(ui).locator('.exclusion-row')).toHaveCount(0);
  await expect(editor(ui).locator('.exclusion-empty')).toHaveText('尚未添加 UA 排除规则。');
  await ui.locator('#time-zone').fill('Asia/Tokyo');
  await ui.getByRole('button', { name: '添加规则', exact: true }).click();
  await expect(ui.getByRole('textbox', { name: 'UA 正则 1', exact: true })).toHaveValue('');
  await ui.getByRole('textbox', { name: 'UA 正则 1', exact: true }).fill('^unsaved/');
  const close = dialog.getByRole('button', { name: '关闭', exact: true });
  await close.focus();
  await close.press('ArrowDown');
  await expect(firstRecord).toHaveAttribute('aria-expanded', 'true');
  await close.press('/');
  await expect(close).toBeFocused();
  await close.press('Escape');
  await expect(dialog).toHaveCount(0);
  await expect(trigger).toBeFocused();
  await expect(firstRecord).toHaveAttribute('aria-expanded', 'true');
  await openEditor(ui);
  await expect(ui.locator('#time-zone')).toHaveValue('Asia/Tokyo');
  await expect(ui.getByRole('textbox', { name: 'UA 正则 1', exact: true })).toHaveValue('^unsaved/');
  await expect(editor(ui).locator('.exclusion-status')).toContainText('有未保存修改');
  expect(writes).toEqual([]);
});

test('UA exclusions save explicitly, clear with an empty list, and leave audit queries independent', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  const reads = auditReads(page);
  const writes = [];
  let saved = ['^existing/'];
  await page.route('**/__demo/api/exclusions?**', async (route) => {
    if (route.request().method() === 'PUT') {
      saved = route.request().postDataJSON().userAgentPatterns;
      writes.push(saved);
    }
    await route.fulfill({ json: patterns(saved) });
  });
  await page.goto('/'); await ready(ui);
  const before = reads.length;
  await openEditor(ui);
  await expect(editor(ui).locator('.exclusion-status')).toContainText('已保存：1 条');
  await ui.getByRole('textbox', { name: 'UA 正则 1', exact: true }).fill('^monitor/');
  await ui.getByRole('button', { name: '添加规则', exact: true }).click();
  await ui.getByRole('textbox', { name: 'UA 正则 2', exact: true }).fill('(?i)^internal-bot/');
  await expect(editor(ui).locator('.exclusion-status')).toContainText('有未保存修改');
  expect(writes).toEqual([]);
  expect(reads).toHaveLength(before);
  await ui.getByRole('button', { name: '保存排除规则', exact: true }).click();
  await expect(editor(ui).locator('.exclusion-status')).toContainText('已保存：2 条');
  await expect(editor(ui).locator('.exclusion-status')).not.toContainText('有未保存修改');
  expect(writes).toEqual([['^monitor/', '(?i)^internal-bot/']]);
  expect(reads).toHaveLength(before);
  await ui.getByRole('button', { name: '移除规则 2', exact: true }).click();
  await ui.getByRole('button', { name: '移除规则 1', exact: true }).click();
  await ui.getByRole('button', { name: '保存排除规则', exact: true }).click();
  await expect(editor(ui).locator('.exclusion-status')).toContainText('已保存：0 条');
  await expect(editor(ui).locator('.exclusion-status')).not.toContainText('有未保存修改');
  expect(writes).toEqual([['^monitor/', '(?i)^internal-bot/'], []]);
  expect(reads).toHaveLength(before);
});

test('Rust regex syntax reaches the backend verbatim and invalid patterns point to their row', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  const writes = [];
  let saved = [];
  await page.route('**/__demo/api/exclusions?**', async (route) => {
    if (route.request().method() === 'PUT') {
      const proposed = route.request().postDataJSON().userAgentPatterns;
      writes.push(proposed);
      if (proposed[1] === '[') return route.fulfill({ status: 400, json: serviceError('invalid_exclusions', '正则表达式无效：未闭合的字符类。', { patternIndex: 1 }) });
      saved = proposed;
    }
    return route.fulfill({ json: patterns(saved) });
  });
  await page.goto('/'); await ready(ui);
  await openEditor(ui);
  await expect(editor(ui).locator('.exclusion-status')).toContainText('已保存：0 条');
  await ui.getByRole('button', { name: '添加规则', exact: true }).click();
  await ui.getByRole('textbox', { name: 'UA 正则 1', exact: true }).fill('(?i)^monitor/');
  await ui.getByRole('button', { name: '添加规则', exact: true }).click();
  await ui.getByRole('textbox', { name: 'UA 正则 2', exact: true }).fill('[');
  await ui.getByRole('button', { name: '保存排除规则', exact: true }).click();
  await expect(editor(ui).locator('.exclusion-row').nth(1)).toContainText('正则表达式无效：未闭合的字符类。');
  await expect(editor(ui).locator('.exclusion-row').first()).not.toContainText('正则表达式无效');
  await expect(editor(ui).locator('.exclusion-status')).toContainText('已保存：0 条');
  await expect(ui.getByRole('textbox', { name: 'UA 正则 1', exact: true })).toHaveValue('(?i)^monitor/');
  await expect(ui.getByRole('textbox', { name: 'UA 正则 2', exact: true })).toHaveValue('[');
  expect(writes).toEqual([['(?i)^monitor/', '[']]);
  await ui.getByRole('textbox', { name: 'UA 正则 2', exact: true }).fill('^backup/');
  await ui.getByRole('button', { name: '保存排除规则', exact: true }).click();
  await expect(editor(ui).locator('.exclusion-status')).toContainText('已保存：2 条');
  await expect(editor(ui).locator('[role="alert"]')).toHaveCount(0);
  expect(writes).toEqual([['(?i)^monitor/', '['], ['(?i)^monitor/', '^backup/']]);
});

test('unavailable exclusion settings do not block audits and can be repaired by saving', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  const reads = auditReads(page);
  const writes = [];
  await page.route('**/__demo/api/exclusions?**', async (route) => {
    if (route.request().method() === 'GET') return route.fulfill({ status: 503, json: serviceError('unavailable', '排除设置暂时不可用。') });
    const proposed = route.request().postDataJSON().userAgentPatterns;
    writes.push(proposed);
    return route.fulfill({ json: patterns(proposed) });
  });
  await page.goto('/'); await ready(ui);
  await openEditor(ui);
  await expect(editor(ui).getByRole('alert')).toContainText('排除设置暂时不可用。');
  await expect(ui.getByRole('button', { name: '重试加载排除规则', exact: true })).toBeVisible();
  const before = reads.length;
  await ui.getByRole('button', { name: '添加规则', exact: true }).click();
  await ui.getByRole('textbox', { name: 'UA 正则 1', exact: true }).fill('^repair/');
  await ui.getByRole('button', { name: '保存排除规则', exact: true }).click();
  await expect(editor(ui).locator('.exclusion-status')).toContainText('已保存：1 条');
  await expect(editor(ui).getByRole('alert')).toHaveCount(0);
  expect(writes).toEqual([['^repair/']]);
  expect(reads).toHaveLength(before);
  await ready(ui);
});

test('a delayed exclusions load preserves a draft edited before it returns', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  await page.route('**/__demo/api/exclusions?**', async (route) => {
    await gate;
    return route.fulfill({ json: patterns(['^persisted/']) });
  });
  await page.goto('/'); await ready(ui);
  await openEditor(ui);
  try {
    await ui.getByRole('button', { name: '添加规则', exact: true }).click();
    await ui.getByRole('textbox', { name: 'UA 正则 1', exact: true }).fill('^draft/');
    const complete = page.waitForResponse((response) => new URL(response.url()).pathname === '/__demo/api/exclusions');
    release(); await complete;
    await expect(editor(ui).locator('.exclusion-status')).toContainText('已保存：1 条');
    await expect(ui.getByRole('textbox', { name: 'UA 正则 1', exact: true })).toHaveValue('^draft/');
    await expect(editor(ui).locator('.exclusion-status')).toContainText('有未保存修改');
  } finally { release(); }
});

test('retrying a failed exclusion load keeps draft edits made before retry', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  let reads = 0;
  await page.route('**/__demo/api/exclusions?**', async (route) => {
    if (!reads++) return route.fulfill({ status: 503, json: serviceError('unavailable', '排除设置暂时不可用。') });
    return route.fulfill({ json: patterns(['^persisted/']) });
  });
  await page.goto('/'); await ready(ui);
  await openEditor(ui);
  await expect(editor(ui).getByRole('alert')).toContainText('排除设置暂时不可用。');
  await ui.getByRole('button', { name: '添加规则', exact: true }).click();
  await ui.getByRole('textbox', { name: 'UA 正则 1', exact: true }).fill('^draft-before-retry/');
  await ui.getByRole('button', { name: '重试加载排除规则', exact: true }).click();
  await expect(editor(ui).locator('.exclusion-status')).toContainText('已保存：1 条');
  await expect(editor(ui).getByRole('alert')).toHaveCount(0);
  await expect(ui.getByRole('textbox', { name: 'UA 正则 1', exact: true })).toHaveValue('^draft-before-retry/');
  await expect(editor(ui).locator('.exclusion-status')).toContainText('有未保存修改');
  expect(reads).toBe(2);
});

test('a delayed exclusions load cannot overwrite a completed save or a later draft', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  const writes = [];
  await page.route('**/__demo/api/exclusions?**', async (route) => {
    if (route.request().method() === 'GET') {
      await gate;
      return route.fulfill({ json: patterns(['^stale/', '^old/']) });
    }
    const proposed = route.request().postDataJSON().userAgentPatterns;
    writes.push(proposed);
    return route.fulfill({ json: patterns(proposed) });
  });
  await page.goto('/'); await ready(ui);
  await openEditor(ui);
  try {
    await ui.getByRole('button', { name: '添加规则', exact: true }).click();
    await ui.getByRole('textbox', { name: 'UA 正则 1', exact: true }).fill('^saved/');
    await ui.getByRole('button', { name: '保存排除规则', exact: true }).click();
    await expect(editor(ui).locator('.exclusion-status')).toContainText('已保存：1 条');
    await ui.getByRole('textbox', { name: 'UA 正则 1', exact: true }).fill('^new-draft/');
    const complete = page.waitForResponse((response) => response.request().method() === 'GET' && new URL(response.url()).pathname === '/__demo/api/exclusions');
    release(); await complete;
    await expect(editor(ui).locator('.exclusion-status')).toContainText('已保存：1 条');
    await expect(editor(ui).locator('.exclusion-row')).toHaveCount(1);
    await expect(ui.getByRole('textbox', { name: 'UA 正则 1', exact: true })).toHaveValue('^new-draft/');
    await expect(editor(ui).locator('.exclusion-status')).toContainText('有未保存修改');
    expect(writes).toEqual([['^saved/']]);
  } finally { release(); }
});

test('edits made while saving remain as an unsaved draft for the next save', async ({ page }) => {
  const ui = page.frameLocator('iframe');
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  const writes = [];
  await page.route('**/__demo/api/exclusions?**', async (route) => {
    if (route.request().method() === 'GET') return route.fulfill({ json: patterns(['^original/']) });
    const proposed = route.request().postDataJSON().userAgentPatterns;
    writes.push(proposed);
    if (writes.length === 1) await gate;
    return route.fulfill({ json: patterns(proposed) });
  });
  await page.goto('/'); await ready(ui);
  await openEditor(ui);
  await expect(editor(ui).locator('.exclusion-status')).toContainText('已保存：1 条');
  try {
    await ui.getByRole('textbox', { name: 'UA 正则 1', exact: true }).fill('^submitted/');
    await ui.getByRole('button', { name: '保存排除规则', exact: true }).click();
    await expect(ui.getByRole('button', { name: '保存中…', exact: true })).toBeDisabled();
    await ui.getByRole('textbox', { name: 'UA 正则 1', exact: true }).fill('^next/');
    release();
    await expect(ui.getByRole('button', { name: '保存排除规则', exact: true })).toBeEnabled();
    await expect(ui.getByRole('textbox', { name: 'UA 正则 1', exact: true })).toHaveValue('^next/');
    await expect(editor(ui).locator('.exclusion-status')).toContainText('有未保存修改');
    expect(writes).toEqual([['^submitted/']]);
    await ui.getByRole('button', { name: '保存排除规则', exact: true }).click();
    await expect(editor(ui).locator('.exclusion-status')).not.toContainText('有未保存修改');
    expect(writes).toEqual([['^submitted/'], ['^next/']]);
  } finally { release(); }
});
