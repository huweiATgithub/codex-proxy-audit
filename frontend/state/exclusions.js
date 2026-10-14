import { computed, onScopeDispose, ref, shallowRef, watch } from 'vue';
import { isAborted } from '../api/client.js';

const samePatterns = (left, right) => left.length === right.length && left.every((pattern, index) => pattern === right[index]);

export function useExclusions(client) {
  const draft = ref([]);
  const saved = shallowRef(null);
  const loadState = shallowRef({ kind: 'idle' });
  const saveState = shallowRef({ kind: 'idle' });
  const revision = ref(0);
  const dirty = computed(() => saved.value === null ? revision.value > 0 : !samePatterns(draft.value, saved.value));
  let controller, disposed = false;
  watch(draft, () => {
    revision.value++;
    if (['error', 'saved'].includes(saveState.value.kind)) saveState.value = { kind: 'idle' };
  }, { deep: true, flush: 'sync' });

  async function load() {
    if (saveState.value.kind === 'saving') return;
    controller?.abort();
    const current = controller = new AbortController();
    const startedAt = revision.value;
    const preserveDraft = dirty.value;
    loadState.value = { kind: 'loading' };
    try {
      const result = await client.exclusions(current.signal);
      if (current.signal.aborted || disposed) return;
      saved.value = [...result.userAgentPatterns];
      if (!preserveDraft && revision.value === startedAt) draft.value = [...result.userAgentPatterns];
      loadState.value = { kind: 'ready' };
    } catch (error) {
      if (!current.signal.aborted && !disposed && !isAborted(error)) loadState.value = { kind: 'error', error };
    }
  }

  async function save() {
    if (saveState.value.kind === 'saving') return;
    controller?.abort();
    if (loadState.value.kind === 'loading') loadState.value = { kind: 'idle' };
    const patterns = [...draft.value];
    saveState.value = { kind: 'saving' };
    try {
      const result = await client.saveExclusions(patterns);
      if (disposed) return;
      saved.value = [...result.userAgentPatterns];
      loadState.value = { kind: 'ready' };
      saveState.value = { kind: 'saved' };
    } catch (error) {
      if (!disposed) saveState.value = { kind: 'error', error, patterns };
    }
  }

  function rowError(index) {
    const state = saveState.value;
    return state.kind === 'error' && state.error.patternIndex === index && samePatterns(draft.value, state.patterns) ? state.error.message : '';
  }
  onScopeDispose(() => { disposed = true; controller?.abort(); });
  return { draft, saved, loadState, saveState, dirty, load, save, rowError };
}
