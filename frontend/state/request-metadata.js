import { onScopeDispose, shallowRef, toValue, watch } from 'vue';
import { isAborted } from '../api/client.js';

export function useRequestMetadata(client, auditId) {
  const state = shallowRef({ kind: 'idle' });
  let controller;
  watch(() => toValue(auditId), () => {
    controller?.abort();
    state.value = { kind: 'idle' };
  }, { flush: 'sync' });
  async function load() {
    if (['loading', 'ready', 'missing'].includes(state.value.kind)) return;
    const current = controller = new AbortController();
    state.value = { kind: 'loading' };
    try {
      const result = await client.requestMetadata(toValue(auditId), current.signal);
      if (current.signal.aborted) return;
      state.value = result.requestMetadata === null ? { kind: 'missing' } : { kind: 'ready', data: result.requestMetadata };
    } catch (error) {
      if (!current.signal.aborted && !isAborted(error)) state.value = { kind: 'error', error };
    }
  }
  onScopeDispose(() => controller?.abort());
  return { state, load };
}
