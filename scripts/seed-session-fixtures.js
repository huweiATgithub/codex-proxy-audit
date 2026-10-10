import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { z } from 'zod';
import { endpoints, MAX_RESPONSE_BYTES } from '../contracts/audit.js';
import { appendContent } from '../frontend/api/content.js';

const textBlock = z.object({ kind: z.literal('text'), text: z.string(), inputKind: z.string().nullable().optional() });
const fixtureMessage = z.object({ id: z.string().min(1).max(256), role: z.enum(['user', 'assistant']), phase: z.string().max(80).nullable().optional(), turnId: z.string().max(256).nullable().optional(), blocks: z.array(textBlock).min(1) }).refine((message) => !(message.role === 'assistant' && message.phase === 'analysis'), 'Session fixtures must exclude assistant reasoning');
const fixtureAudit = z.object({ id: z.string().min(1), messageId: z.string().min(1).max(256), firstObservedAt: z.iso.datetime({ offset: true }).optional(), threadId: z.string().min(1).max(256), sessionId: z.string().max(256).nullable(), turnId: z.string().max(256).nullable(), input: z.array(textBlock).min(1), context: z.array(fixtureMessage) });
const fixtureDocument = z.object({ audits: z.array(fixtureAudit).min(1) });
const DEFAULT_KEY_NAME = 'Codex Sessions · 脱敏样本';

// Only the checked-in sanitized fixture is consumed. Each request contributes
// one newest input and supplies its own Agent context. Replaying fixtures adds
// another batch of audits, without using the controlled upstream's test reply.
export function sessionFixtureRequests(document) {
  const { audits } = fixtureDocument.parse(document);
  const knownTurns = new Map(audits.map((audit) => [`${audit.threadId}\0${audit.messageId}`, audit.turnId]));
  // Fixture timestamps establish source ordering only. The plugin records the
  // new gateway observation time, and no client timestamp is sent or stored.
  const ordered = audits.every((audit) => audit.firstObservedAt) ? [...audits].sort((left, right) => Date.parse(left.firstObservedAt) - Date.parse(right.firstObservedAt) || left.messageId.localeCompare(right.messageId)) : audits;
  const requests = ordered.map((audit) => {
    const messages = [...audit.context, { id: audit.messageId, role: 'user', turnId: audit.turnId, blocks: audit.input }];
    const input = messages.map((message) => {
      const result = {
        id: message.id, type: 'message', role: message.role,
        ...(message.phase ? { phase: message.phase } : {}),
        content: message.blocks.map((block) => ({ type: message.role === 'user' ? 'input_text' : 'output_text', text: block.text })),
      };
      if (message.role === 'user') {
        const turnId = message.turnId ?? knownTurns.get(`${audit.threadId}\0${message.id}`);
        result.internal_chat_message_metadata_passthrough = {
          ...(turnId ? { turn_id: turnId } : {}),
          ...(message.blocks.every((block) => typeof block.inputKind === 'string') ? { content_item_kinds: message.blocks.map((block) => block.inputKind) } : {}),
        };
      }
      return result;
    });
    return {
      auditId: audit.id,
      threadId: audit.threadId,
      messageId: audit.messageId,
      sessionId: audit.sessionId,
      turnId: audit.turnId,
      input: audit.input,
      context: audit.context,
      body: {
        model: 'gpt-5.4', store: false, stream: false, input,
        client_metadata: { 'x-codex-turn-metadata': JSON.stringify({ thread_source: 'user', thread_id: audit.threadId, session_id: audit.sessionId }) },
      },
    };
  });
  return { requests, expectedInputCount: requests.length };
}

const bodySignature = (blocks) => JSON.stringify(blocks.map(({ kind, text }) => ({ kind, text })));
const contextSignature = (messages) => JSON.stringify(messages.map(({ role, phase, blocks }) => ({ role, phase: phase ?? null, blocks: blocks.map(({ kind, text }) => ({ kind, text })) })));

// Turn IDs and identical input text are deliberately insufficient for this
// check. Every target must map to a distinct audit with its full ordered context.
export function matchFixtureTargets(requests, observations) {
  const used = new Set();
  return requests.map((request) => {
    const observation = observations.find(({ detail, input, context }) => !used.has(detail.id)
      && detail.threadId === request.threadId && detail.sessionId === request.sessionId && detail.turnId === request.turnId
      && bodySignature(input) === bodySignature(request.input)
      && contextSignature(context) === contextSignature(request.context));
    if (!observation) throw new Error(`No exact saved input/context matches sanitized fixture target ${request.auditId}`);
    used.add(observation.detail.id);
    return { request, ...observation };
  });
}

export async function seedSessionFixtures({ baseUrl = 'http://127.0.0.1:4180', fixturePath = 'tests/fixtures/codex-sessions.json', adminPassword = process.env.AUDIT_SEED_ADMIN_PASSWORD || 'audit-integration-password-2026', keyName = DEFAULT_KEY_NAME, instanceId } = {}) {
  const base = new URL(baseUrl);
  if (!['http:', 'https:'].includes(base.protocol) || base.username || base.password || base.search || base.hash) throw new Error('Invalid test host URL');
  const { requests, expectedInputCount } = sessionFixtureRequests(JSON.parse(readFileSync(resolve(fixturePath), 'utf8')));
  let cookie;
  const call = async (path, { body, token } = {}) => {
    const response = await fetch(new URL(path, base), { method: body === undefined ? 'GET' : 'POST', headers: { ...(cookie ? { cookie } : {}), ...(body === undefined ? {} : { 'content-type': 'application/json' }), ...(token ? { authorization: `Bearer ${token}`, 'user-agent': 'codex-audit-sanitized-fixture/1.0' } : {}) }, body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(30000) });
    const bytes = new Uint8Array(await response.arrayBuffer());
    if (path.includes('/api/api/') && bytes.length > MAX_RESPONSE_BYTES) throw new Error('Audit response exceeds contract budget');
    const value = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes));
    if (!response.ok || (value.code !== undefined && value.code !== 200)) throw new Error(`Test host request failed: ${response.status} ${path.split('?')[0]}`);
    return value.code === undefined ? value : value.data;
  };
  const login = await fetch(new URL('/api/auth/login', base), { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ mode: 'admin', password: adminPassword }), signal: AbortSignal.timeout(15000) });
  if (!login.ok) throw new Error(`Test host login failed: ${login.status}`);
  cookie = login.headers.get('set-cookie')?.split(';')[0];
  if (!cookie) throw new Error('Test host did not issue an admin session');
  const views = await call('/api/admin/plugins/extensions');
  const candidates = views.filter((view) => view.pages.some((page) => page.id === 'prompt-audit') && (!instanceId || view.target.instanceId === instanceId));
  if (candidates.length !== 1) throw new Error('Select exactly one running Prompt Audit instance with --instance-id');
  const view = candidates[0];
  const management = async (endpoint, values = {}) => {
    const { instanceId, artifactSha256, revision } = view.target;
    const value = await call(`/api/admin/plugins/extensions/${instanceId}/${artifactSha256}/${revision}/api/${endpoints[endpoint].path}?${new URLSearchParams(values)}`);
    return endpoints[endpoint].response.parse(value);
  };
  let key, cursor;
  do {
    const page = await call(`/api/admin/client-keys?${new URLSearchParams({ limit: '100', search: keyName, ...(cursor ? { cursor } : {}) })}`);
    for (const candidate of page.items.filter((item) => item.name === keyName)) {
      if (key) throw new Error('More than one dedicated fixture Key has the configured name');
      key = candidate;
    }
    cursor = page.nextCursor;
  } while (cursor);
  if (key) {
    if (!key.enabled) throw new Error('The dedicated fixture Key is disabled');
    key = { ...key, ...await call(`/api/admin/client-keys/reveal?${new URLSearchParams({ id: key.id })}`) };
  } else {
    key = await call('/api/admin/client-keys/create', { body: { name: keyName, groupIds: [], maxConcurrency: 0, requestsPerMinute: 0 } });
  }
  const waitUntil = async (action, label, timeout = 75000) => {
    const deadline = Date.now() + timeout; let last;
    do { try { return await action(); } catch (error) { last = error; await new Promise((done) => setTimeout(done, 250)); } } while (Date.now() < deadline);
    throw new Error(`${label}: ${last?.message || 'timed out'}`);
  };
  // Wait for the host.keys.list cache before capturing name snapshots; this
  // never updates a previously saved input's original Key-name observation.
  await waitUntil(async () => {
    let cursor;
    do { const page = await management('keys', cursor ? { cursor } : {}); if (page.items.some((item) => item.id === key.id && item.names.includes(keyName))) return; cursor = page.nextCursor; } while (cursor);
    throw new Error('Dedicated Key snapshot not refreshed yet');
  }, 'Fixture Key snapshot');
  const before = await management('audits', { key: key.id });
  const previousIds = new Set(before.items.map((row) => row.id));
  cursor = before.nextCursor;
  while (cursor) { const page = await management('audits', { key: key.id, cursor }); for (const row of page.items) previousIds.add(row.id); cursor = page.nextCursor; }
  for (const request of requests) await call('/v1/responses', { body: request.body, token: key.plaintextKey });
  const firstPage = await waitUntil(async () => { const page = await management('audits', { key: key.id }); assert.equal(page.total, before.total + expectedInputCount); return page; }, 'Persisted fixture input count');
  const rows = [...firstPage.items]; cursor = firstPage.nextCursor;
  while (cursor) { const page = await management('audits', { key: key.id, cursor }); rows.push(...page.items); cursor = page.nextCursor; }
  const addedRows = rows.filter((row) => !previousIds.has(row.id));
  assert.equal(new Set(addedRows.map((row) => row.id)).size, expectedInputCount);
  const details = [];
  for (const row of addedRows) details.push(await management('audit', { id: row.id }));
  const multiTurn = details.filter((detail) => detail.context.some((message) => message.role === 'assistant'));
  if (!multiTurn.length) throw new Error('No saved multi-turn fixture input has an assistant predecessor');
  const readContent = async (detail, descriptor) => {
    let state = { blocks: [], characterCount: 0, nextCursor: undefined };
    do { const page = await management('content', { id: detail.id, contentId: descriptor.id, ...(state.nextCursor ? { cursor: state.nextCursor } : {}) }); state = appendContent(state.blocks, page, descriptor); } while (state.nextCursor);
    return state.blocks.map(({ kind, text }) => ({ kind, text }));
  };
  const observations = [];
  for (const detail of details) {
    const input = await readContent(detail, detail.input), context = [];
    for (const message of detail.context) {
      assert.equal(message.truncated, false, 'Fixed sanitized fixture contexts fit the collection window');
      context.push({ role: message.role, phase: message.phase, blocks: await readContent(detail, message.content) });
    }
    observations.push({ detail, input, context });
  }
  const matched = matchFixtureTargets(requests, observations);
  const sample = matched.findLast(({ detail }) => detail.context.some((message) => message.role === 'assistant'));
  const assistant = sample.detail.context.findLast((message) => message.role === 'assistant');
  const verifiedAssistant = { auditId: sample.detail.id, phase: assistant.phase, characterCount: assistant.content.characterCount, threadId: sample.detail.threadId };
  const pageUrl = new URL(`/plugins/${view.target.instanceId}/prompt-audit`, base).href;
  const result = { keyId: key.id, keyName, fixtureRequests: requests.length, verifiedTargetCount: matched.length, verifiedAuditIds: matched.map(({ detail }) => detail.id), expectedInputCount, previousInputCount: before.total, addedInputCount: firstPage.total - before.total, total: firstPage.total, inputsWithAssistantContext: multiTurn.length, verifiedAssistant, pageUrl };
  console.log(JSON.stringify(result, null, 2));
  return result;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const args = process.argv.slice(2), options = {};
  const names = { '--base-url': 'baseUrl', '--fixture': 'fixturePath', '--key-name': 'keyName', '--instance-id': 'instanceId' };
  for (let index = 0; index < args.length; index += 2) {
    if (!names[args[index]] || !args[index + 1]) throw new Error('Usage: node scripts/seed-session-fixtures.js [--base-url http://127.0.0.1:4180] [--fixture tests/fixtures/codex-sessions.json] [--key-name name] [--instance-id id]');
    options[names[args[index]]] = args[index + 1];
  }
  await seedSessionFixtures(options);
}
