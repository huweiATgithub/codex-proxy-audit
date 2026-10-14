import test from 'node:test';
import assert from 'node:assert/strict';
import { effectScope, ref } from 'vue';
import { useAudits } from '../frontend/state/audits.js';
import { AuditError } from '../frontend/api/client.js';
import { useTimeZone } from '../frontend/state/time-zone.js';
import { useRequestMetadata } from '../frontend/state/request-metadata.js';
import { useExclusions } from '../frontend/state/exclusions.js';

const initial = { from: '', to: '', key: '', q: '', size: 50 };
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
const page = (ids, nextCursor = null) => ({ items: ids.map((id) => ({ id })), total: 3, nextCursor });
function setup(client) {
  const scope = effectScope();
  const store = scope.run(() => useAudits(client, initial));
  return { scope, store };
}

test('slow older searches and details cannot replace the current selection', async () => {
  const first = deferred(), second = deferred(), oldDetail = deferred(), newDetail = deferred();
  let calls = 0;
  const { scope, store } = setup({ list: () => (++calls === 1 ? first : second).promise, detail: (id) => (id === 'old' ? oldDetail : newDetail).promise });
  try {
    const one = store.search({ ...initial, q: 'old' });
    const two = store.search({ ...initial, q: 'new' });
    second.resolve(page(['new'])); await two;
    first.resolve(page(['old'])); await one;
    assert.equal(store.items.value[0].id, 'new');
    const old = store.select('old'), recent = store.select('new');
    newDetail.resolve({ id: 'new' }); await recent;
    oldDetail.resolve({ id: 'old' }); await old;
    assert.equal(store.detail.value.data.id, 'new');
  } finally { scope.stop(); }
});

test('previous/next crosses actual cursor pages and a page-size change resets the chain', async () => {
  const queries = [];
  const { scope, store } = setup({ list: async (query) => { queries.push(query); return query.cursor ? page(['third']) : page(['first', 'second'], 'cursor-two'); }, detail: async (id) => ({ id }) });
  try {
    await store.search(initial); await store.select('second'); await store.move(1);
    assert.equal(store.pageIndex.value, 1); assert.equal(store.selectedId.value, 'third');
    await store.move(-1);
    assert.equal(store.pageIndex.value, 0); assert.equal(store.selectedId.value, 'second');
    await store.search(store.query, 100);
    assert.equal(queries.at(-1).limit, '100'); assert.equal(queries.at(-1).cursor, undefined); assert.equal(store.selectedId.value, '');
  } finally { scope.stop(); }
});

test('failures retain a retry target and closing invalidates pending detail', async () => {
  const body = deferred(); let calls = 0;
  const { scope, store } = setup({ list: async () => { if (!calls++) throw new AuditError('unavailable', '重试', 503); return page(['one']); }, detail: () => body.promise });
  try {
    await store.search(initial); assert.equal(store.list.value.kind, 'error');
    await store.retryList(); assert.equal(store.list.value.kind, 'ready');
    const pending = store.select('one'); store.close(); body.resolve({ id: 'one' }); await pending;
    assert.equal(store.detail.value.kind, 'idle'); assert.equal(store.selectedId.value, '');
  } finally { scope.stop(); }
});

test('browser filling requires explicit save and unavailable detection fills UTC', async (t) => {
  const calls = [];
  const settings = useTimeZone({ saveTimeZone: async (timeZone) => { calls.push(timeZone); return { timeZone }; } }, 'UTC');
  t.mock.method(Intl, 'DateTimeFormat', () => ({ resolvedOptions: () => ({ timeZone: 'America/New_York' }) }));
  settings.fillBrowser();
  assert.equal(settings.draft.value, 'America/New_York');
  assert.equal(settings.saved.value, 'UTC');
  assert.deepEqual(calls, []);
  await settings.save();
  assert.deepEqual(calls, ['America/New_York']);
  assert.equal(settings.saved.value, 'America/New_York');
  Intl.DateTimeFormat.mock.mockImplementation(() => ({ resolvedOptions: () => ({}) }));
  settings.fillBrowser(); assert.equal(settings.draft.value, 'UTC');
  Intl.DateTimeFormat.mock.mockImplementation(() => { throw new Error('unavailable'); });
  settings.fillBrowser(); assert.equal(settings.draft.value, 'UTC');
  assert.equal(calls.length, 1);
});

test('draft edits and saving remain independent of pending audit results and filters', async () => {
  const response = deferred(), saved = deferred();
  let reads = 0, writes = 0;
  const client = { list: () => { reads++; return response.promise; }, saveTimeZone: () => { writes++; return saved.promise; } };
  const { scope, store } = setup(client);
  const settings = useTimeZone(client, 'UTC');
  try {
    const pending = store.search({ ...initial, from: '2026-11-01', to: '2026-11-01' });
    const before = { ...store.query };
    settings.draft.value = 'Asia/Tokyo';
    const saving = settings.save();
    assert.equal(store.list.value.kind, 'loading');
    saved.resolve({ timeZone: 'Asia/Tokyo' }); await saving;
    assert.deepEqual({ ...store.query }, before);
    assert.equal(store.list.value.kind, 'loading');
    response.resolve({ ...page(['old']), timeZone: 'UTC' }); await pending;
    assert.equal(store.list.value.data.timeZone, 'UTC');
    assert.equal(reads, 1); assert.equal(writes, 1);
    client.saveTimeZone = async () => { throw new AuditError('invalid_query', '无效时区', 400); };
    settings.draft.value = 'Invalid/Zone'; await settings.save();
    assert.equal(settings.state.value.kind, 'error');
    assert.equal(settings.saved.value, 'Asia/Tokyo');
    assert.equal(store.list.value.kind, 'ready');
  } finally { scope.stop(); }
});

test('request metadata waits for explicit loading and deduplicates pending and completed reads', async () => {
  const pending = deferred();
  const scope = effectScope();
  const calls = [];
  const metadata = { userAgent: 'client/1.0', model: 'model', reasoningEffort: 'high' };
  const store = scope.run(() => useRequestMetadata({ requestMetadata: (id, signal) => { calls.push({ id, signal }); return pending.promise; } }, () => 'audit-one'));
  try {
    assert.equal(store.state.value.kind, 'idle');
    assert.deepEqual(calls, []);
    const first = store.load();
    await store.load();
    assert.equal(calls.length, 1);
    assert.equal(store.state.value.kind, 'loading');
    pending.resolve({ requestMetadata: metadata }); await first;
    assert.deepEqual(store.state.value, { kind: 'ready', data: metadata });
    await store.load(); assert.equal(calls.length, 1);
  } finally { scope.stop(); }
});

test('request metadata retry, missing state, and late responses remain isolated to the open audit', async () => {
  const scope = effectScope();
  const auditId = ref('old');
  const old = deferred(), current = deferred();
  const calls = [];
  let failures = 1;
  const client = { requestMetadata: (id, signal) => {
    calls.push({ id, signal });
    if (id === 'old') return old.promise;
    if (failures--) return Promise.reject(new AuditError('unavailable', '暂时不可用', 503));
    return current.promise;
  } };
  const store = scope.run(() => useRequestMetadata(client, auditId));
  try {
    const pending = store.load();
    auditId.value = 'new';
    assert.equal(calls[0].signal.aborted, true);
    assert.equal(store.state.value.kind, 'idle');
    await store.load(); assert.equal(store.state.value.kind, 'error');
    const retry = store.load();
    current.resolve({ requestMetadata: null }); await retry;
    assert.equal(store.state.value.kind, 'missing');
    old.resolve({ requestMetadata: { userAgent: 'old', model: 'old-model', reasoningEffort: 'high' } }); await pending;
    assert.equal(store.state.value.kind, 'missing');
    await store.load(); assert.equal(calls.length, 3);
    auditId.value = 'old'; assert.equal(store.state.value.kind, 'idle');
    const reopening = store.load();
    await reopening; assert.equal(calls.length, 4);
  } finally { scope.stop(); }
});

test('closing request metadata aborts its pending read and ignores the late result', async () => {
  const scope = effectScope(), pending = deferred();
  let signal;
  const store = scope.run(() => useRequestMetadata({ requestMetadata: (_id, cancellation) => { signal = cancellation; return pending.promise; } }, () => 'audit-one'));
  const loading = store.load();
  scope.stop(); assert.equal(signal.aborted, true);
  pending.resolve({ requestMetadata: { userAgent: 'old', model: 'old-model', reasoningEffort: 'high' } });
  await loading;
  assert.equal(store.state.value.kind, 'loading');
});

function exclusionSetup(client) {
  const scope = effectScope();
  return { scope, settings: scope.run(() => useExclusions(client)) };
}

test('exclusion loading preserves edited drafts and can retry independently after failure', async () => {
  const loaded = deferred(); let reads = 0;
  const { scope, settings } = exclusionSetup({ exclusions: () => { if (!reads++) return Promise.reject(new AuditError('unavailable', 'Repair configuration', 503)); return loaded.promise; } });
  try {
    await settings.load();
    assert.equal(settings.saved.value, null);
    assert.equal(settings.loadState.value.kind, 'error');
    const retry = settings.load();
    settings.draft.value.push('(?i)new-draft');
    loaded.resolve({ userAgentPatterns: ['saved'] }); await retry;
    assert.deepEqual(settings.saved.value, ['saved']);
    assert.deepEqual(settings.draft.value, ['(?i)new-draft']);
    assert.equal(settings.dirty.value, true);
    assert.equal(settings.loadState.value.kind, 'ready');
  } finally { scope.stop(); }
});

test('retry preserves drafts edited before loading, including an explicit empty draft', async () => {
  for (const draft of [['(?i)before-retry'], []]) {
    let reads = 0;
    const { scope, settings } = exclusionSetup({ exclusions: async () => {
      if (!reads++) throw new AuditError('unavailable', 'Repair configuration', 503);
      return { userAgentPatterns: ['persisted'] };
    } });
    try {
      await settings.load();
      settings.draft.value = draft;
      assert.equal(settings.dirty.value, true);
      await settings.load();
      assert.deepEqual(settings.saved.value, ['persisted']);
      assert.deepEqual(settings.draft.value, draft);
      assert.equal(settings.dirty.value, true);
      assert.equal(settings.loadState.value.kind, 'ready');
    } finally { scope.stop(); }
  }
});

test('a pristine first exclusion load fills both saved settings and the editor', async () => {
  const { scope, settings } = exclusionSetup({ exclusions: async () => ({ userAgentPatterns: ['persisted'] }) });
  try {
    assert.equal(settings.dirty.value, false);
    await settings.load();
    assert.deepEqual(settings.saved.value, ['persisted']);
    assert.deepEqual(settings.draft.value, ['persisted']);
    assert.equal(settings.dirty.value, false);
  } finally { scope.stop(); }
});

test('explicit exclusion save repairs a failed load and retains row errors only for their submitted draft', async () => {
  const writes = [];
  const client = {
    exclusions: async () => { throw new AuditError('unavailable', 'Repair configuration', 503); },
    saveExclusions: async (patterns) => { writes.push(patterns); return { userAgentPatterns: patterns }; },
  };
  const { scope, settings } = exclusionSetup(client);
  try {
    await settings.load(); settings.draft.value = ['bot', '['];
    client.saveExclusions = async () => { throw new AuditError('invalid_exclusions', 'Unclosed character class', 400, 1); };
    await settings.save();
    assert.equal(settings.rowError(0), '');
    assert.equal(settings.rowError(1), 'Unclosed character class');
    assert.equal(settings.saved.value, null);
    settings.draft.value.splice(0, 1);
    assert.equal(settings.rowError(0), '');
    assert.equal(settings.saveState.value.kind, 'idle');
    client.saveExclusions = async (patterns) => { writes.push(patterns); return { userAgentPatterns: patterns }; };
    settings.draft.value = ['(?i)^bot/']; await settings.save();
    assert.deepEqual(settings.saved.value, ['(?i)^bot/']);
    assert.equal(settings.loadState.value.kind, 'ready');
    assert.equal(settings.dirty.value, false);
    settings.draft.value = [];
    assert.equal(settings.dirty.value, true);
    await settings.save();
    assert.deepEqual(settings.saved.value, []);
    assert.deepEqual(writes, [['(?i)^bot/'], []]);
  } finally { scope.stop(); }
});

test('exclusion saves invalidate older GETs and keep edits made while saving', async () => {
  const loaded = deferred(), saved = deferred();
  let signal, writes = 0;
  const { scope, settings } = exclusionSetup({ exclusions: (value) => { signal = value; return loaded.promise; }, saveExclusions: (patterns) => { writes++; assert.deepEqual(patterns, ['submitted']); return saved.promise; } });
  try {
    const loading = settings.load();
    settings.draft.value = ['submitted'];
    const saving = settings.save();
    assert.equal(signal.aborted, true);
    await settings.save();
    assert.equal(writes, 1);
    settings.draft.value[0] = 'next-draft';
    saved.resolve({ userAgentPatterns: ['submitted'] }); await saving;
    loaded.resolve({ userAgentPatterns: ['old'] }); await loading;
    assert.deepEqual(settings.saved.value, ['submitted']);
    assert.deepEqual(settings.draft.value, ['next-draft']);
    assert.equal(settings.dirty.value, true);
    assert.equal(settings.saveState.value.kind, 'saved');
    assert.equal(settings.loadState.value.kind, 'ready');
  } finally { scope.stop(); }
});

test('exclusion saves do not disturb audit filters, pending results, or timezone settings', async () => {
  const listed = deferred(), loaded = deferred(), saved = deferred();
  const calls = [];
  const client = {
    list: () => { calls.push('list'); return listed.promise; },
    exclusions: () => { calls.push('exclusions'); return loaded.promise; },
    saveExclusions: () => { calls.push('saveExclusions'); return saved.promise; },
    saveTimeZone: async (timeZone) => { calls.push('saveTimeZone'); return { timeZone }; },
  };
  const { scope, store } = setup(client);
  const settings = scope.run(() => useExclusions(client));
  const timeZone = useTimeZone(client, 'UTC');
  try {
    const pending = store.search({ ...initial, q: 'keep-filter' });
    const before = { ...store.query };
    const loading = settings.load();
    loaded.resolve({ userAgentPatterns: [] }); await loading;
    settings.draft.value.push('(?i)bot');
    const saving = settings.save();
    timeZone.draft.value = 'Asia/Tokyo'; await timeZone.save();
    saved.resolve({ userAgentPatterns: ['(?i)bot'] }); await saving;
    assert.deepEqual({ ...store.query }, before);
    assert.equal(store.list.value.kind, 'loading');
    listed.resolve({ ...page(['audit']), timeZone: 'UTC' }); await pending;
    assert.equal(store.list.value.data.timeZone, 'UTC');
    assert.equal(timeZone.saved.value, 'Asia/Tokyo');
    assert.deepEqual(calls, ['list', 'exclusions', 'saveExclusions', 'saveTimeZone']);
  } finally { scope.stop(); }
});
