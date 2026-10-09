<script setup>
import { inject, onUnmounted, shallowRef } from 'vue';
import { BaseButton } from '@codex-proxy/ui';
import App from './App.vue';

const client = inject('auditClient');
const context = shallowRef({ kind: 'loading' });
let controller;
async function load() {
  controller?.abort();
  const request = controller = new AbortController();
  context.value = { kind: 'loading' };
  try {
    const data = await client.viewContext(request.signal);
    if (!request.signal.aborted) context.value = { kind: 'ready', data };
  } catch (error) {
    if (!request.signal.aborted) context.value = { kind: 'error', error };
  }
}
load();
onUnmounted(() => controller?.abort());
</script>

<template>
  <App v-if="context.kind === 'ready'" :view-context="context.data" />
  <div v-else-if="context.kind === 'error'" class="empty-state" role="alert">
    <h2>暂时无法打开审计</h2><p>{{ context.error.message }}</p><BaseButton size="sm" @click="load">重试</BaseButton>
  </div>
  <div v-else class="empty-state" role="status">正在读取审计设置…</div>
</template>
