import { computed, onScopeDispose, reactive, ref, shallowRef } from 'vue';
import { invalidResponse, isAborted } from '../api/client.js';

export function useAudits(client, initial) {
  const query = reactive({ from: initial.from, to: initial.to, key: initial.key, q: initial.q, limit: String(initial.size) });
  const list = shallowRef({ kind: 'loading' });
  const detail = shallowRef({ kind: 'idle' });
  const keyDirectory = shallowRef({ kind: 'loading' });
  const selectedId = ref('');
  const pageIndex = ref(0);
  let cursors = [undefined];
  let listController, detailController, keysController;
  let lastPage = { cursor: undefined, index: 0 };
  const items = computed(() => list.value.kind === 'ready' ? list.value.data.items : []);
  const selectedIndex = computed(() => items.value.findIndex((item) => item.id === selectedId.value));
  const canPrevious = computed(() => list.value.kind === 'ready' && (selectedIndex.value > 0 || (selectedIndex.value === 0 && pageIndex.value > 0)));
  const canNext = computed(() => list.value.kind === 'ready' && selectedIndex.value >= 0 && (selectedIndex.value < items.value.length - 1 || Boolean(list.value.data.nextCursor)));

  async function loadPage(cursor, index) {
    listController?.abort();
    const controller = listController = new AbortController();
    lastPage = { cursor, index };
    list.value = { kind: 'loading' };
    try {
      const data = await client.list({ ...query, cursor }, controller.signal);
      if (controller.signal.aborted) return false;
      if (data.nextCursor && cursors.slice(0, index + 1).includes(data.nextCursor)) throw invalidResponse();
      cursors = [...cursors.slice(0, index), cursor];
      pageIndex.value = index;
      list.value = { kind: 'ready', data };
      return true;
    } catch (error) {
      if (!controller.signal.aborted && !isAborted(error)) list.value = { kind: 'error', error };
      return false;
    }
  }
  function close() {
    detailController?.abort(); selectedId.value = ''; detail.value = { kind: 'idle' };
  }
  async function select(id) {
    detailController?.abort();
    const controller = detailController = new AbortController();
    selectedId.value = id; detail.value = { kind: 'loading' };
    try {
      const data = await client.detail(id, controller.signal);
      if (!controller.signal.aborted) detail.value = { kind: 'ready', data };
    } catch (error) {
      if (!controller.signal.aborted && !isAborted(error)) detail.value = { kind: 'error', error };
    }
  }
  function search(filters, size = Number(query.limit), selected = '') {
    close(); cursors = [undefined]; pageIndex.value = 0;
    Object.assign(query, { from: filters.from, to: filters.to, key: filters.key, q: filters.q, limit: String(size) });
    const loading = loadPage(undefined, 0);
    if (selected) select(selected);
    return loading;
  }
  async function page(step, selectEdge = false) {
    if (list.value.kind !== 'ready') return;
    const index = pageIndex.value + step;
    if (index < 0 || (step > 0 && !list.value.data.nextCursor)) return;
    const cursor = step > 0 ? list.value.data.nextCursor : cursors[index];
    close();
    if (await loadPage(cursor, index) && selectEdge && items.value.length) select(step > 0 ? items.value[0].id : items.value.at(-1).id);
  }
  async function move(step) {
    if (list.value.kind !== 'ready') return;
    const index = selectedIndex.value;
    if (index < 0) { if (!selectedId.value && items.value.length) select(items.value[0].id); return; }
    const next = items.value[index + step];
    if (next) select(next.id);
    else await page(step, true);
  }
  async function loadKeys() {
    keysController?.abort();
    const controller = keysController = new AbortController();
    keyDirectory.value = { kind: 'loading' };
    try {
      const items = [], seen = new Set();
      let cursor;
      do {
        const data = await client.keys(cursor, controller.signal);
        if (controller.signal.aborted) return;
        for (const key of data.items) {
          if (items.length && key.id <= items.at(-1).id) throw invalidResponse();
          items.push(key);
        }
        cursor = data.nextCursor;
        if (cursor && seen.has(cursor)) throw invalidResponse();
        seen.add(cursor);
      } while (cursor);
      keyDirectory.value = { kind: 'ready', items };
    } catch (error) {
      if (!controller.signal.aborted && !isAborted(error)) keyDirectory.value = { kind: 'error', error };
    }
  }
  onScopeDispose(() => { listController?.abort(); detailController?.abort(); keysController?.abort(); });
  return { query, list, detail, keyDirectory, selectedId, selectedIndex, pageIndex, items, canPrevious, canNext,
    search, select, close, move, page, loadKeys, retryList: () => loadPage(lastPage.cursor, lastPage.index) };
}
