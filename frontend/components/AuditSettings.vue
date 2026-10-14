<script setup>
import { inject, watch } from 'vue';
import { BaseButton, BaseInput, BaseModal } from '@codex-proxy/ui';
import { MAX_EXCLUSION_PATTERNS, MAX_EXCLUSION_PATTERN_BYTES } from '../../contracts/audit.js';
import { useExclusions } from '../state/exclusions.js';
import { useTimeZone } from '../state/time-zone.js';

const props = defineProps({ timeZone: { type: String, required: true } });
const emit = defineEmits(['time-zone-saved']);
const open = defineModel({ type: Boolean, default: false });
const client = inject('auditClient');
const { draft: timeZoneDraft, saved: savedTimeZone, state: timeZoneState, fillBrowser, save: saveTimeZone } = useTimeZone(client, props.timeZone);
const { draft, saved, loadState, saveState, dirty, load, save, rowError } = useExclusions(client);
watch(savedTimeZone, (timeZone) => emit('time-zone-saved', timeZone));
load();
</script>

<template>
  <BaseModal v-model="open" title="设置" size="lg" :draggable="false">
    <div class="audit-settings">
      <section class="settings-section" aria-labelledby="display-settings-heading">
        <h3 id="display-settings-heading">显示设置</h3>
        <p class="settings-description">审计时区用于记录时间展示和日期筛选，保存后在下次查询时生效。</p>
        <div class="timezone-settings" aria-label="审计时区设置">
          <label for="time-zone">审计时区</label>
          <div class="timezone-controls">
            <BaseInput id="time-zone" class="timezone-input" v-model="timeZoneDraft" size="sm" aria-label="审计时区" placeholder="UTC" autocomplete="off" :maxlength="128" />
            <BaseButton size="sm" variant="secondary" @click="fillBrowser">使用浏览器时区</BaseButton>
            <BaseButton size="sm" :disabled="timeZoneState.kind === 'saving'" @click="saveTimeZone">{{ timeZoneState.kind === 'saving' ? '保存中…' : '保存' }}</BaseButton>
          </div>
          <span class="timezone-saved" role="status">已保存：{{ savedTimeZone }}<template v-if="timeZoneState.kind === 'saved'"> · 保存成功</template></span>
          <p v-if="timeZoneState.kind === 'error'" class="filter-error" role="alert">{{ timeZoneState.error.message }}</p>
        </div>
      </section>
      <section class="settings-section" aria-labelledby="exclusion-settings-heading">
        <h3 id="exclusion-settings-heading">排除规则</h3>
        <p class="settings-description">匹配任意排除规则的请求将不记录。已有记录仍可查询，请求照常转发。</p>
        <div class="exclusion-settings">
          <div class="exclusion-heading">
            <h4>UA 排除规则</h4>
            <span class="exclusion-status" role="status">
              <template v-if="saved !== null">已保存：{{ saved.length }} 条</template>
              <template v-else>{{ loadState.kind === 'loading' ? '正在加载…' : '尚未加载' }}</template>
              <template v-if="dirty"> · 有未保存修改</template>
              <template v-if="loadState.kind === 'error'"> · 加载失败</template>
              <template v-if="saveState.kind === 'saved'"> · 保存成功</template>
              <template v-if="saveState.kind === 'error'"> · 保存失败</template>
            </span>
          </div>
          <div class="exclusion-editor" aria-label="UA 排除规则设置">
            <p class="exclusion-help">使用 <a href="https://docs.rs/regex/latest/regex/#syntax" target="_blank" rel="noopener noreferrer">Rust regex 语法</a>：默认区分大小写，在任意位置匹配；可用 <code>^</code>、<code>$</code> 和 <code>(?i)</code>。最多 {{ MAX_EXCLUSION_PATTERNS }} 条，每条不超过 {{ MAX_EXCLUSION_PATTERN_BYTES }} 个 UTF-8 字节。</p>
            <p v-if="loadState.kind === 'error'" class="inline-error" role="alert">{{ loadState.error.message }} <BaseButton size="sm" variant="ghost" :disabled="saveState.kind === 'saving'" @click="load">重试加载排除规则</BaseButton><span>也可直接编辑并保存以替换配置。</span></p>
            <p v-if="loadState.kind === 'loading'" class="exclusion-help" role="status">正在加载已保存规则…</p>
            <div class="exclusion-patterns">
              <div v-for="(_pattern, index) in draft" :key="index" class="exclusion-row">
                <div class="exclusion-row-controls">
                  <label :for="`ua-pattern-${index}`">{{ index + 1 }}</label>
                  <BaseInput :id="`ua-pattern-${index}`" v-model="draft[index]" size="sm" :aria-label="`UA 正则 ${index + 1}`" :aria-invalid="Boolean(rowError(index))" :aria-describedby="rowError(index) ? `ua-pattern-error-${index}` : undefined" placeholder="例如：(?i)^internal-bot/" autocomplete="off" spellcheck="false" />
                  <BaseButton size="sm" variant="ghost" :aria-label="`移除规则 ${index + 1}`" @click="draft.splice(index, 1)">移除</BaseButton>
                </div>
                <p v-if="rowError(index)" :id="`ua-pattern-error-${index}`" class="exclusion-row-error">{{ rowError(index) }}</p>
              </div>
            </div>
            <p v-if="!draft.length" class="exclusion-empty">尚未添加 UA 排除规则。<span v-if="saved?.length">保存空列表会清除已保存规则。</span></p>
            <div class="exclusion-actions">
              <BaseButton size="sm" variant="secondary" :disabled="draft.length >= MAX_EXCLUSION_PATTERNS" @click="draft.push('')">添加规则</BaseButton>
              <BaseButton size="sm" :disabled="saveState.kind === 'saving'" @click="save">{{ saveState.kind === 'saving' ? '保存中…' : '保存排除规则' }}</BaseButton>
            </div>
            <p v-if="saveState.kind === 'error'" class="inline-error" role="alert">{{ saveState.error.message }}</p>
          </div>
        </div>
      </section>
    </div>
  </BaseModal>
</template>
