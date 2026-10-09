<script setup>
import { computed, inject, nextTick, onMounted, onUnmounted, reactive, ref } from 'vue';
import { BaseButton, BaseIconButton, BaseInput, BaseSelect, BaseTable, BaseToast, defineTableColumns, toast } from '@codex-proxy/ui';
import { ArrowDown, ArrowUp, ArrowDownWideNarrow, Copy, FileSearch, LoaderCircle, RefreshCw, Search, X } from '@lucide/vue';
import { PAGE_SIZES } from './audit.js';
import { useAudits } from './state/audits.js';
import AuditDetail from './components/AuditDetail.vue';
import HighlightedText from './components/HighlightedText.vue';

const props = defineProps({ viewContext: { type: Object, required: true } });
const initial = { from: props.viewContext.today, to: props.viewContext.today, key: '', q: '', size: 50 };
const store = useAudits(inject('auditClient'), initial);
const { query, list, detail: detailState, keyDirectory, selectedId, pageIndex, items, canPrevious, canNext } = store;
const filters = reactive({ from: initial.from, to: initial.to, key: initial.key, q: initial.q });
const filterError = ref('');
const detail = ref(null);
const recordsPanel = ref(null);
const viewport = ref(window.innerWidth);
const total = computed(() => list.value.kind === 'ready' ? list.value.data.total.toLocaleString() : '—');
const compactColumns = computed(() => (selectedId.value && viewport.value >= 1100 && viewport.value < 1600) || viewport.value < 900);
const columns = computed(() => defineTableColumns([
  { key: 'recordedAt', label: '记录时间', kind: 'datetime', size: '2xl', fixedWidth: true },
  { key: 'keyName', label: 'Client Key', kind: 'custom', size: compactColumns.value ? 'md' : '2xl', fixedWidth: true },
  { key: 'preview', label: '用户输入', kind: 'custom', size: compactColumns.value ? '3xl' : '4xl' },
]));
const keyOptions = computed(() => {
  const options = [{ value: '', label: '全部 Client Key' }, ...(keyDirectory.value.kind === 'ready' ? keyDirectory.value.items : []).map((key) => ({
    value: key.id, label: `${key.names.join(' / ') || '名称未知'}${key.historical ? '（历史）' : ''}`, description: key.id,
  }))];
  if (filters.key && !options.some((option) => option.value === filters.key)) options.push({ value: filters.key, label: filters.key });
  return options;
});
const sizeOptions = PAGE_SIZES.map((size) => ({ value: String(size), label: `${size} 条/页` }));
const restartRequired = computed(() => list.value.kind === 'error' && ['invalid_cursor', 'cursor_expired'].includes(list.value.error.code));
async function resetScroll() {
  await nextTick();
  recordsPanel.value?.querySelector('table')?.scrollIntoView({ block: 'start', inline: 'nearest' });
}
async function applyFilters() {
  if (filters.from && filters.to && filters.from > filters.to) { filterError.value = '起始日期不能晚于结束日期。'; return; }
  if (Array.from(filters.q.trim()).length > 200) { filterError.value = '关键词最多 200 个字符。'; return; }
  filterError.value = ''; filters.q = filters.q.trim();
  const loading = store.search(filters);
  await loading; resetScroll();
}
async function refreshList() { await store.search(query); resetScroll(); }
function resetFilters() { Object.assign(filters, { from: '', to: '', key: '', q: '' }); applyFilters(); }
async function selectRecord(id) {
  store.select(id);
  await nextTick(); detail.value?.focus({ preventScroll: true });
  recordsPanel.value?.querySelector(`[data-audit="${CSS.escape(id)}"]`)?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
}
async function closeDetail() {
  const previous = selectedId.value; store.close();
  await nextTick(); recordsPanel.value?.querySelector(`[data-audit="${CSS.escape(previous)}"]`)?.focus({ preventScroll: true });
}
async function moveRecord(step) {
  await store.move(step); await nextTick();
  detail.value?.focus({ preventScroll: true });
  recordsPanel.value?.querySelector(`[data-audit="${CSS.escape(selectedId.value)}"]`)?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
}
async function changePage(step) { await store.page(step); resetScroll(); }
async function changeSize(size) { await store.search(query, Number(size)); resetScroll(); }
function rowClick(event) {
  const button = event.target.closest('tbody tr')?.querySelector('[data-audit]');
  if (button && !event.target.closest('button')) selectRecord(button.dataset.audit);
}
async function copyReference() {
  const value = selectedId.value;
  const success = '审计 ID 已复制';
  try { await navigator.clipboard.writeText(value); toast.success(success); }
  catch {
    const field = document.createElement('textarea');
    field.value = value; field.style.position = 'fixed'; field.style.opacity = '0'; document.body.append(field); field.select();
    let copied = false;
    try { copied = document.execCommand('copy'); } catch { /* The visible record ID remains selectable. */ }
    field.remove(); detail.value?.focus({ preventScroll: true });
    toast.info(copied ? success : '请选中详情顶部的审计 ID 复制');
  }
}
function onKeydown(event) {
  if (event.defaultPrevented || event.ctrlKey || event.altKey || event.metaKey || event.isComposing) return;
  if (event.target.closest('input, select, textarea, [contenteditable="true"], [role="combobox"], [role="listbox"]')) return;
  if (event.key === '/') { event.preventDefault(); document.getElementById('query')?.focus(); }
  if (event.key === 'Escape' && selectedId.value) { event.preventDefault(); closeDetail(); }
  if (['ArrowDown', 'ArrowUp'].includes(event.key)) { event.preventDefault(); moveRecord(event.key === 'ArrowDown' ? 1 : -1); }
}
function resize() { viewport.value = window.innerWidth; }
store.search(initial, initial.size); store.loadKeys();
onMounted(() => {
  window.addEventListener('keydown', onKeydown);
  window.addEventListener('resize', resize);
});
onUnmounted(() => {
  window.removeEventListener('keydown', onKeydown); window.removeEventListener('resize', resize);
});
</script>

<template>
  <div class="audit-app">
    <header class="topbar">
      <div class="brand"><span class="brand-icon"><FileSearch :size="19" /></span><span>Codex Proxy</span><span class="brand-divider">/</span><h1>Prompt 审计</h1></div>
    </header>
    <main>
      <div class="filters" role="search" aria-label="筛选审计记录">
        <BaseInput id="query" v-model="filters.q" class="search-field" size="sm" type="search" aria-label="搜索用户输入正文" placeholder="搜索用户输入正文…" autocomplete="off" :maxlength="400" @keydown.enter.prevent="applyFilters">
          <template #prefix><Search :size="16" /></template><template #suffix><kbd>/</kbd></template>
        </BaseInput>
        <div class="date-range">
          <BaseInput id="date-from" v-model="filters.from" type="date" size="sm" aria-label="起始日期" @keydown.enter.prevent="applyFilters" /><span>—</span>
          <BaseInput id="date-to" v-model="filters.to" type="date" size="sm" aria-label="结束日期" @keydown.enter.prevent="applyFilters" />
        </div>
        <BaseSelect v-model="filters.key" class="key-filter" size="sm" :options="keyOptions" :loading="keyDirectory.kind === 'loading'" :disabled="keyDirectory.kind === 'loading'" filterable aria-label="Client Key" />
        <BaseButton variant="primary" size="sm" @click="applyFilters">检索</BaseButton>
        <BaseButton variant="ghost" size="sm" @click="resetFilters">重置</BaseButton>
        <p v-if="keyDirectory.kind === 'error'" class="filter-error" role="alert">{{ keyDirectory.error.message }} <BaseButton size="sm" variant="ghost" @click="store.loadKeys">重试 Key 列表</BaseButton></p>
        <p v-if="filterError" class="filter-error" role="alert">{{ filterError }}</p>
      </div>
      <div class="results-heading">
        <div class="results-summary"><h2>用户输入</h2><span id="result-count" class="count" role="status">{{ total }} 条</span><span v-if="query.q" class="active-filter">正文包含“{{ query.q }}”</span></div>
        <div class="table-options"><span class="timezone">系统时间 · {{ viewContext.timeZone }} · 最新在前</span><BaseIconButton label="刷新列表" size="sm" :disabled="list.kind === 'loading'" @click="refreshList"><RefreshCw :size="14" /></BaseIconButton></div>
      </div>
      <div class="workspace" :class="{ 'detail-open': selectedId }">
        <section ref="recordsPanel" class="records-panel" :class="{ 'compact-columns': compactColumns }" aria-label="用户输入列表">
          <div v-if="list.kind === 'error'" class="empty-state request-error" role="alert"><h2>暂时无法显示列表</h2><p>{{ list.error.message }}</p><BaseButton size="sm" @click="restartRequired ? refreshList() : store.retryList()">{{ restartRequired ? '重新检索' : '重试' }}</BaseButton></div>
          <BaseTable v-else class="audit-table" :loading="list.kind === 'loading'" :columns="columns" :rows="items" row-key="id" density="compact" :selected-row-keys="selectedId ? [selectedId] : []" :sort="{ key: 'recordedAt', direction: 'desc' }" show-header-when-empty @click="rowClick">
            <template #header-recordedAt><span class="time-heading" title="网关记录这条输入的时间；后续重复请求不更新时间。">记录时间<ArrowDownWideNarrow :size="13" /></span></template>
            <template #recordedAt="{ row }"><time :datetime="row.recordedAt" :title="viewContext.timeZone">{{ row.recordedAtDisplay }}</time></template>
            <template #keyName="{ row }"><span class="key-cell" :title="`${row.keyName || '名称未知'} · ${row.keyId}`"><span class="key-name">{{ row.keyName || '名称未知' }}</span><code class="key-id">{{ row.keyId }}</code></span></template>
            <template #preview="{ row }"><button class="record-preview" :data-audit="row.id" type="button" :aria-label="`审阅 ${row.id}：${row.preview.slice(0, 100)}`" :aria-expanded="row.id === selectedId" :title="row.preview.slice(0, 800)" @click="selectRecord(row.id)"><HighlightedText :text="row.preview" :query="query.q" /></button></template>
            <template #empty><div class="empty-state"><Search :size="24" /><h2>没有匹配的输入</h2><p>试试更换关键词，或放宽日期和 Key 筛选。</p><BaseButton size="sm" @click="resetFilters">清除筛选</BaseButton></div></template>
          </BaseTable>
          <footer class="cursor-pagination">
            <span>第 {{ pageIndex + 1 }} 页<span v-if="list.kind === 'ready'"> · 本页 {{ items.length }} 条</span></span>
            <div><BaseSelect :model-value="query.limit" :options="sizeOptions" size="sm" aria-label="每页条数" :disabled="list.kind === 'loading'" @update:model-value="changeSize" /><BaseButton size="sm" :disabled="list.kind !== 'ready' || pageIndex === 0" @click="changePage(-1)">上一页</BaseButton><BaseButton size="sm" :disabled="list.kind !== 'ready' || !list.data.nextCursor" @click="changePage(1)">下一页</BaseButton></div>
          </footer>
        </section>
        <section v-if="selectedId" ref="detail" class="detail-panel" aria-label="输入详情" tabindex="-1">
          <div class="detail-toolbar">
            <div class="detail-title"><h2>输入详情</h2><code>{{ selectedId }}</code></div>
            <div class="toolbar-actions">
              <BaseIconButton label="复制审计 ID" size="sm" @click="copyReference"><Copy :size="16" /></BaseIconButton>
              <BaseIconButton label="上一条输入" size="sm" :disabled="!canPrevious" @click="moveRecord(-1)"><ArrowUp :size="16" /></BaseIconButton>
              <BaseIconButton label="下一条输入" size="sm" :disabled="!canNext" @click="moveRecord(1)"><ArrowDown :size="16" /></BaseIconButton>
              <BaseIconButton label="关闭详情" size="sm" @click="closeDetail"><X :size="17" /></BaseIconButton>
            </div>
          </div>
          <AuditDetail v-if="detailState.kind === 'ready'" :key="detailState.data.id" :audit="detailState.data" :query="query.q" />
          <div v-else-if="detailState.kind === 'loading'" class="empty-state" role="status"><LoaderCircle :size="20" class="spinner" /><p>正在读取输入详情…</p></div>
          <div v-else-if="detailState.kind === 'error'" class="empty-state" role="alert"><h2>{{ detailState.error.code === 'not_found' ? '记录不可用' : '详情读取失败' }}</h2><p>{{ detailState.error.message }}</p><BaseButton size="sm" @click="store.select(selectedId)">重试详情</BaseButton></div>
        </section>
      </div>
      <footer class="page-footer"><span>每行一条用户输入 · 重复请求不另算发言</span><span><kbd>↑</kbd><kbd>↓</kbd> 连续审阅 <kbd>Esc</kbd> 关闭详情</span></footer>
    </main>
    <BaseToast />
  </div>
</template>
