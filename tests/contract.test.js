import test from 'node:test';
import assert from 'node:assert/strict';
import { createDemoService } from '../demo/service.js';
import { createAuditClient, AuditError } from '../frontend/api/client.js';
import { appendContent } from '../frontend/api/content.js';
import { audits } from '../demo/fixtures.js';
import { CONTENT_CHARACTERS, MAX_RESPONSE_BYTES, parseQuery } from '../contracts/audit.js';

const reply = (value, status = 200) => ({ status, contentType: 'application/json; charset=utf-8', body: new TextEncoder().encode(JSON.stringify(value)).buffer });
const bridge = (service) => ({ version: 2, async request({ method, path, query, body }) {
  assert.ok(['GET', 'PUT'].includes(method)); assert.ok(!path.startsWith('/')); assert.ok(!query.startsWith('?'));
  const result = service(path, new URLSearchParams(query), method, body);
  const response = reply(result.value, result.status);
  assert.ok(response.body.byteLength <= MAX_RESPONSE_BYTES);
  return response;
} });
const client = createAuditClient(bridge(createDemoService()));

// A backend can return fewer than the requested number without ending a page chain.
test('cursor traversal covers variable-size pages once, and binds every filter', async () => {
  const api = createAuditClient(bridge(createDemoService({ maxListRows: 7, timeZone: 'America/New_York' })));
  const query = { from: '2026-10-08', to: '2026-10-08', key: 'ck_platform', q: '数据库', limit: '50' };
  const first = await api.list(query);
  assert.equal(first.items.length, 7);
  assert.ok(first.nextCursor);
  assert.ok(first.items.every((item) => !('input' in item) && !('context' in item) && item.preview.includes('数据库')));
  const ids = first.items.map((item) => item.id);
  let cursor = first.nextCursor;
  while (cursor) { const page = await api.list({ ...query, cursor }); ids.push(...page.items.map((item) => item.id)); cursor = page.nextCursor; }
  assert.equal(ids.length, first.total);
  assert.equal(new Set(ids).size, ids.length);
  await assert.rejects(api.list({ ...query, q: 'CI', cursor: first.nextCursor }), (error) => error.code === 'invalid_cursor');
  await assert.rejects(api.list({ ...query, cursor: 'malformed' }), (error) => error.code === 'invalid_cursor');
});

test('search is literal, ASCII-insensitive, covers full input, and excludes context', async () => {
  assert.equal((await client.list({ q: '我会先核对现有实现与约束' })).total, 0);
  assert.equal((await client.list({ q: '[.*]' })).total, 0);
  const upper = await client.list({ q: 'API' }), lower = await client.list({ q: 'api' });
  assert.equal(upper.total, lower.total);
  const base = audits.find((audit) => audit.id === 'PA-0009');
  const extra = { ...base, id: 'tail-record', input: [{ kind: 'text', text: '前'.repeat(4000) + 'SQL 100% _ \\ literal' }] };
  const api = createAuditClient(bridge(createDemoService({ audits: [extra] })));
  const result = await api.list({ q: '100% _ \\' });
  assert.equal(result.total, 1); assert.match(result.items[0].preview, /100% _/); assert.ok(Array.from(result.items[0].preview).length <= 400);
});

test('local inclusive dates retain both DST repeated hours and sort by the instant', async () => {
  const source = audits[0];
  const records = [
    { ...source, id: 'before', firstObservedAt: '2026-11-01T03:59:59Z' },
    { ...source, id: 'first-hour', firstObservedAt: '2026-11-01T01:50:00-04:00' },
    { ...source, id: 'second-hour', firstObservedAt: '2026-11-01T01:10:00-05:00' },
    { ...source, id: 'end', firstObservedAt: '2026-11-02T04:59:59Z' },
    { ...source, id: 'after', firstObservedAt: '2026-11-02T05:00:00Z' },
  ];
  const api = createAuditClient(bridge(createDemoService({ audits: records, timeZone: 'America/New_York' })));
  assert.deepEqual((await api.list({ from: '2026-11-01', to: '2026-11-01' })).items.map((item) => item.id), ['end', 'second-hour', 'first-hour']);
  await assert.rejects(api.list({ from: '2026-11-02', to: '2026-11-01' }), (error) => error.code === 'invalid_query');
});

test('saved audit timezone owns default date, display text, date filtering and cursor scope', async () => {
  const records = [{ ...audits[0], firstObservedAt: '2026-10-08T18:42:18.000Z' }];
  for (const [timeZone, today, display] of [
    ['Asia/Shanghai', '2026-10-09', '2026-10-09 02:42:18'],
    ['Asia/Kathmandu', '2026-10-09', '2026-10-09 00:27:18'],
    ['America/New_York', '2026-10-08', '2026-10-08 14:42:18'],
    ['UTC', '2026-10-08', '2026-10-08 18:42:18'],
  ]) {
    const api = createAuditClient(bridge(createDemoService({ audits: records, timeZone, now: () => new Date('2026-10-08T19:00:00Z') })));
    assert.deepEqual(await api.viewContext(), { timeZone, today });
    const page = await api.list({ from: today, to: today });
    assert.equal(page.total, 1);
    assert.equal(page.items[0].recordedAtDisplay, display);
    assert.equal((await api.detail(records[0].id)).recordedAtDisplay, display);
    assert.equal(page.items[0].recordedAt, '2026-10-08T18:42:18.000Z');
    const otherDate = today === '2026-10-08' ? '2026-10-09' : '2026-10-08';
    assert.equal((await api.list({ from: otherDate, to: otherDate })).total, 0);
    await assert.rejects(api.list({ timeZone: 'UTC' }), (error) => error.code === 'invalid_query');
  }
  const shanghai = createAuditClient(bridge(createDemoService({ timeZone: 'Asia/Shanghai' })));
  const utc = createAuditClient(bridge(createDemoService({ timeZone: 'UTC' })));
  const first = await shanghai.list({});
  await assert.rejects(utc.list({ cursor: first.nextCursor }), (error) => error.code === 'invalid_cursor');
});

test('chunk traversal preserves code points, block order, and metadata independently', async () => {
  const original = [{ kind: 'text', text: '😀界'.repeat(1000) }, { kind: 'nontext', mediaType: '图片', description: '说明' }, { kind: 'text', text: '' }, { kind: 'text', text: 'END' }];
  const api = createAuditClient(bridge(createDemoService({ audits: [{ ...audits[0], input: original }] })));
  const detail = await api.detail(audits[0].id);
  assert.ok(!JSON.stringify(detail).includes('😀'));
  assert.equal(detail.threadId, 'thread_web_01'); assert.equal(detail.sessionId, 'session_demo_web_01');
  let result = { blocks: [], nextCursor: null }, requests = 0;
  do {
    const page = await api.content(detail.id, detail.input.id, result.nextCursor);
    assert.ok(page.blocks.reduce((count, block) => count + (block.kind === 'text' ? Array.from(block.text).length : 0), 0) <= CONTENT_CHARACTERS);
    result = appendContent(result.blocks, page, detail.input); requests++;
  } while (result.nextCursor);
  assert.equal(requests, 2);
  assert.deepEqual(result.blocks.map(({ kind, text, mediaType, description }) => kind === 'text' ? { kind, text } : { kind, mediaType, description }), original);
  assert.equal(result.characterCount, 2003);
  const first = await api.content(detail.id, detail.input.id);
  await assert.rejects(api.content(detail.id, detail.context[0].content.id, first.nextCursor), (error) => error.code === 'invalid_cursor');
  await assert.rejects(api.content(detail.id, 'missing'), (error) => error.code === 'not_found');
});

test('detail preserves saved metadata and only the explicit endpoint joins host facts by request and Key', async () => {
  const record = { ...audits[0], turnId: 'message-turn', input: [
    { kind: 'text', text: 'answer', inputKind: 'user.answered_question' },
    { kind: 'nontext', mediaType: '图片', inputKind: 'future.user_annotation' },
    { kind: 'text', text: 'unclassified' },
  ] };
  const host = { id: record.requestId, keyId: record.keyId, userAgent: 'client/1.0', model: 'requested-model', reasoningEffort: 'high' };
  const modelRequests = [{ ...host, id: 'another-request', model: 'wrong-request-model' }, host];
  const api = createAuditClient(bridge(createDemoService({ audits: [record], modelRequests })));
  const detail = await api.detail(record.id);
  assert.equal(detail.turnId, 'message-turn');
  assert.deepEqual(detail.inputKinds, ['user.answered_question', 'future.user_annotation', null]);
  assert.deepEqual(detail.repositories, record.repositories);
  assert.equal(Object.hasOwn(detail, 'requestMetadata'), false);
  assert.deepEqual(await api.requestMetadata(record.id), { auditId: record.id, requestMetadata: { userAgent: 'client/1.0', model: 'requested-model', reasoningEffort: 'high' } });
  host.keyId = 'another-key';
  await assert.rejects(api.requestMetadata(record.id), (error) => error.code === 'unavailable');
  assert.deepEqual((await api.detail(record.id)).input, detail.input);
  host.keyId = record.keyId;
  // Purging the matching host row leaves the audit and its content readable.
  modelRequests.splice(1, 1);
  const after = await api.detail(record.id);
  assert.equal(Object.hasOwn(after, 'requestMetadata'), false);
  assert.deepEqual(await api.requestMetadata(record.id), { auditId: record.id, requestMetadata: null });
  assert.deepEqual(after.input, detail.input); assert.deepEqual(after.repositories, detail.repositories);
  assert.equal((await api.content(record.id, detail.input.id)).blocks[0].text, 'answer');
  assert.equal((await api.list({})).total, 1);
  const missing = await createAuditClient(bridge(createDemoService({ audits: [{ ...record, turnId: null, repositories: null }] }))).detail(record.id);
  assert.equal(missing.turnId, null); assert.equal(missing.repositories, null);
  const bad = { ...detail, inputKinds: ['user.text'] };
  await assert.rejects(createAuditClient({ version: 2, request: async () => reply(bad) }).detail(record.id), (error) => error.code === 'invalid_response');
});

test('request metadata endpoint validates its audit identity, fields, and query independently', async () => {
  const id = audits[0].id;
  const metadata = { userAgent: 'client/1.0', model: 'requested-model', reasoningEffort: 'high' };
  for (const value of [
    { auditId: 'another-audit', requestMetadata: metadata },
    { auditId: id, requestMetadata: { ...metadata, userAgent: 42 } },
    { auditId: id, requestMetadata: { ...metadata, model: 'x'.repeat(257) } },
    { auditId: id },
  ]) await assert.rejects(createAuditClient({ version: 2, request: async () => reply(value) }).requestMetadata(id), (error) => error.code === 'invalid_response');
  assert.deepEqual(await createAuditClient({ version: 2, request: async () => reply({ auditId: id, requestMetadata: { userAgent: null, model: null, reasoningEffort: null } }) }).requestMetadata(id), { auditId: id, requestMetadata: { userAgent: null, model: null, reasoningEffort: null } });
  await assert.rejects(client.requestMetadata('missing-audit'), (error) => error.code === 'not_found');
  assert.throws(() => parseQuery('requestMetadata', new URLSearchParams('id=a&id=b')));
  assert.throws(() => parseQuery('requestMetadata', new URLSearchParams('id=a&extra=b')));
  const record = audits[0];
  const host = { id: record.requestId, keyId: record.keyId, userAgent: '😀'.repeat(8193), model: '界'.repeat(257), reasoningEffort: 'x'.repeat(81) };
  const api = createAuditClient(bridge(createDemoService({ audits: [record], modelRequests: [host] })));
  assert.deepEqual((await api.requestMetadata(id)).requestMetadata, { userAgent: null, model: null, reasoningEffort: null });
  host.userAgent = '😀'.repeat(8192); host.model = '界'.repeat(256); host.reasoningEffort = 'x'.repeat(80);
  assert.deepEqual((await api.requestMetadata(id)).requestMetadata, { userAgent: host.userAgent, model: host.model, reasoningEffort: host.reasoningEffort });
});

test('content rejects overlaps, gaps, changed totals, and a false end', () => {
  const descriptor = { id: 'input', blockCount: 1, characterCount: 4 };
  const partial = appendContent([], { blocks: [{ kind: 'text', index: 0, offset: 0, text: 'ab', totalCharacters: 4 }], nextCursor: 'next' }, descriptor);
  for (const segment of [
    { index: 0, offset: 1, text: 'cd', totalCharacters: 4 },
    { index: 1, offset: 0, text: 'cd', totalCharacters: 4 },
    { index: 0, offset: 2, text: 'cd', totalCharacters: 5 },
  ]) assert.throws(() => appendContent(partial.blocks, { blocks: [{ kind: 'text', ...segment }], nextCursor: null }, descriptor), AuditError);
  assert.throws(() => appendContent(partial.blocks, { blocks: [], nextCursor: null }, descriptor), AuditError);
  assert.equal(partial.blocks[0].text, 'ab');
  assert.throws(() => appendContent([], { blocks: [{ kind: 'text', index: 0, offset: 0, text: '', totalCharacters: 4 }], nextCursor: 'next' }, descriptor), AuditError);
  assert.throws(() => appendContent([], { blocks: [{ kind: 'text', index: 0, offset: 0, text: 'ab', totalCharacters: 5 }], nextCursor: 'next' }, descriptor), AuditError);
});

test('current host Key names resolve old audits and follow renames and deletions', async () => {
  const keys = [{ id: 'ck_platform', name: 'Current name' }];
  const service = createDemoService({ keys });
  const list = () => service('api/audits', new URLSearchParams({ key: 'ck_platform' })).value;
  const first = list().items[0];
  assert.equal(first.keyName, 'Current name');
  keys[0].name = 'Renamed';
  assert.ok(list().items.every((item) => item.keyName === 'Renamed'));
  assert.equal(service('api/audit', new URLSearchParams({ id: first.id })).value.keyName, 'Renamed');
  assert.deepEqual(service('api/keys', new URLSearchParams()).value.items, [{ id: 'ck_platform', name: 'Renamed' }]);
  keys.splice(0);
  assert.ok(list().items.every((item) => item.keyName === null));
  assert.equal(service('api/audit', new URLSearchParams({ id: first.id })).value.keyName, null);
  assert.deepEqual(service('api/keys', new URLSearchParams()).value.items, []);
  assert.ok(list().total > 0);
});

test('current Key pages preserve IDs and paginate within the fixed response cap', async () => {
  const keys = Array.from({ length: 250 }, (_, index) => ({ id: `key-${String(index).padStart(3, '0')}`, name: '\u0001'.repeat(512) }));
  const api = createAuditClient(bridge(createDemoService({ keys })));
  const found = []; let cursor;
  do { const page = await api.keys(cursor); found.push(...page.items); cursor = page.nextCursor; } while (cursor);
  assert.deepEqual(found, keys);
  assert.equal(createDemoService()('api/keys', new URLSearchParams('extra=1')).status, 400);
  assert.equal(createDemoService()('api/keys', new URLSearchParams('cursor=a&cursor=b')).status, 400);
});

test('bridge responses are parsed before use; malformed and unauthorized replies are distinct', async () => {
  const invalid = [
    { status: 200, contentType: 'text/html', body: new ArrayBuffer(0) },
    { status: 200, contentType: 'application/json', body: new TextEncoder().encode('bad json').buffer },
    reply({ items: [], total: 0, nextCursor: 'not-progressing' }),
    reply({ items: [], total: '0', nextCursor: null }),
    { status: 200, contentType: 'application/json', body: new ArrayBuffer(MAX_RESPONSE_BYTES + 1) },
  ];
  for (const response of invalid) await assert.rejects(createAuditClient({ version: 2, request: async () => response }).list({}), (error) => error.code === 'invalid_response');
  await assert.rejects(createAuditClient({ version: 2, request: async () => ({ status: 401 }) }).list({}), (error) => error.code === 'unauthorized');
  await assert.rejects(createAuditClient(undefined).list({}), (error) => error.code === 'host_missing');
});

test('shared request queue bounds concurrency and skips abandoned queued reads', async () => {
  let active = 0, maximum = 0, count = 0;
  const api = createAuditClient({ version: 2, async request() {
    count++; active++; maximum = Math.max(maximum, active);
    await new Promise((resolve) => setTimeout(resolve, 8)); active--;
    return reply({ timeZone: 'UTC', items: [], total: 0, nextCursor: null });
  } });
  const abort = new AbortController();
  const running = Array.from({ length: 10 }, (_, index) => api.list({}, index === 9 ? abort.signal : undefined));
  abort.abort();
  const results = await Promise.allSettled(running);
  assert.equal(maximum, 4); assert.equal(count, 9); assert.equal(results[9].reason.name, 'AbortError');
});

test('saving a shared UTC-default setting preserves data and binds all cursor families', async () => {
  const api = createAuditClient(bridge(createDemoService({ keyPageSize: 1 })));
  assert.equal((await api.viewContext()).timeZone, 'UTC');
  const list = await api.list({});
  const detail = await api.detail('PA-0009');
  const content = await api.content(detail.id, detail.input.id);
  const keys = await api.keys();
  assert.ok(list.nextCursor && content.nextCursor && keys.nextCursor);
  assert.deepEqual(await api.saveTimeZone('Asia/Kathmandu'), { timeZone: 'Asia/Kathmandu' });
  await assert.rejects(api.saveTimeZone('Invalid/Zone'), (error) => error.code === 'invalid_query');
  assert.equal((await api.viewContext()).timeZone, 'Asia/Kathmandu');
  assert.equal((await api.detail(detail.id)).timeZone, 'Asia/Kathmandu');
  assert.equal((await api.detail(detail.id)).recordedAt, detail.recordedAt);
  assert.deepEqual((await api.content(detail.id, detail.input.id)).blocks, content.blocks);
  assert.equal((await api.list({})).total, list.total);
  await assert.rejects(api.list({ cursor: list.nextCursor }), (error) => error.code === 'invalid_cursor');
  await assert.rejects(api.content(detail.id, detail.input.id, content.nextCursor), (error) => error.code === 'invalid_cursor');
  await assert.rejects(api.keys(keys.nextCursor), (error) => error.code === 'invalid_cursor');
});

test('settings saves complete while all read slots are occupied without refetching', async () => {
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  const calls = [];
  const api = createAuditClient({ version: 2, async request(input) {
    calls.push(input);
    if (input.method === 'PUT') {
      assert.equal(input.path, 'api/time-zone');
      assert.equal(input.query, '');
      assert.equal(input.contentType, 'application/json');
      return reply(JSON.parse(new TextDecoder().decode(input.body)));
    }
    await pending;
    return reply({ timeZone: 'UTC', items: [], total: 0, nextCursor: null });
  } });
  const reads = Array.from({ length: 4 }, () => api.list({ from: '2026-11-01', to: '2026-11-01' }));
  assert.deepEqual(await api.saveTimeZone('Asia/Tokyo'), { timeZone: 'Asia/Tokyo' });
  assert.equal(calls.length, 5);
  release();
  assert.ok((await Promise.all(reads)).every((value) => value.timeZone === 'UTC'));
  assert.equal(calls.length, 5);
});
