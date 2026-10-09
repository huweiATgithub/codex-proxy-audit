import test from 'node:test';
import assert from 'node:assert/strict';
import { effectScope } from 'vue';
import { useAudits } from '../frontend/state/audits.js';
import { AuditError } from '../frontend/api/client.js';

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
