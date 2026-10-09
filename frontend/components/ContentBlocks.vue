<script setup>
import { computed, inject, onBeforeUnmount, ref, shallowRef, watch } from 'vue';
import { BaseButton } from '@codex-proxy/ui';
import { ArrowDown, Image, LoaderCircle } from '@lucide/vue';
import { appendContent } from '../api/content.js';
import { invalidResponse, isAborted } from '../api/client.js';
import HighlightedText from './HighlightedText.vue';

const props = defineProps({ auditId: { type: String, required: true }, content: { type: Object, required: true }, query: { type: String, default: '' } });
const client = inject('auditClient');
const data = shallowRef({ blocks: [], characterCount: 0, nextCursor: null });
const loading = ref(false);
const error = shallowRef(null);
const started = ref(false);
const seen = new Set();
let controller;
const remaining = computed(() => started.value && Boolean(data.value.nextCursor));
async function load(reset = false) {
  controller?.abort();
  const current = controller = new AbortController();
  if (reset) { data.value = { blocks: [], characterCount: 0, nextCursor: null }; started.value = false; seen.clear(); }
  loading.value = true; error.value = null;
  try {
    const page = await client.content(props.auditId, props.content.id, data.value.nextCursor, current.signal);
    if (current.signal.aborted) return;
    if (page.nextCursor && seen.has(page.nextCursor)) throw invalidResponse();
    data.value = appendContent(data.value.blocks, page, props.content);
    seen.add(page.nextCursor); started.value = true;
  } catch (failure) {
    if (!current.signal.aborted && !isAborted(failure)) error.value = failure;
  } finally { if (!current.signal.aborted) loading.value = false; }
}
watch(() => [props.auditId, props.content.id], () => load(true), { immediate: true });
onBeforeUnmount(() => controller?.abort());
</script>

<template>
  <div class="content-reader" :aria-busy="loading">
    <div v-for="block in data.blocks" :key="block.index" class="content-block">
      <div v-if="block.kind === 'nontext'" class="attachment"><Image :size="20" aria-hidden="true" /><div><strong>{{ block.mediaType }} · 非文本内容未采集</strong><p>{{ block.description }}</p></div></div>
      <p v-else class="body-text"><HighlightedText :text="block.text" :query="query" /></p>
    </div>
    <p v-if="loading && !started" class="loading-copy" role="status"><LoaderCircle :size="15" class="spinner" />正在读取正文…</p>
    <div v-if="error" class="inline-error" role="alert"><span>{{ error.message }}</span><BaseButton size="sm" variant="ghost" @click="load(error.code === 'cursor_expired' || error.code === 'invalid_cursor')">{{ ['cursor_expired', 'invalid_cursor'].includes(error.code) ? '重新读取' : '重试读取' }}</BaseButton></div>
    <div v-else-if="remaining || (started && content.characterCount > 1200)" class="body-progress">
      <span>已读取 {{ data.characterCount }} / {{ content.characterCount }} 字符</span>
      <BaseButton v-if="remaining" variant="ghost" size="sm" :loading="loading" @click="load()"><template #icon><ArrowDown :size="14" /></template>继续读取正文</BaseButton>
      <span v-else>正文已完整读取</span>
    </div>
    <p v-if="started && !content.blockCount" class="content-note">没有保存的正文内容。</p>
  </div>
</template>
