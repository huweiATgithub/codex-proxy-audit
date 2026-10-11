import { ref, shallowRef } from 'vue';

export function useTimeZone(client, initial) {
  const draft = ref(initial);
  const saved = ref(initial);
  const state = shallowRef({ kind: 'idle' });
  function fillBrowser() {
    try { draft.value = Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC'; }
    catch { draft.value = 'UTC'; }
  }
  async function save() {
    if (state.value.kind === 'saving') return;
    state.value = { kind: 'saving' };
    try {
      const result = await client.saveTimeZone(draft.value);
      saved.value = result.timeZone;
      state.value = { kind: 'saved' };
    } catch (error) { state.value = { kind: 'error', error }; }
  }
  return { draft, saved, state, fillBrowser, save };
}
