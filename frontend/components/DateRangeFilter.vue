<script setup>
import { computed, nextTick, ref, watch } from 'vue';
import { BaseButton, BaseIconButton, BasePopover } from '@codex-proxy/ui';
import { CalendarDays, ChevronDown, ChevronLeft, ChevronRight, X } from '@lucide/vue';
import { calendarDay, parseRange, rangeLabel, shiftDay, shiftMonth } from '../state/date-range.js';

const props = defineProps({ range: { type: Object, required: true }, today: { type: String, required: true }, timeZone: { type: String, required: true }, loading: Boolean });
const emit = defineEmits(['apply']);
const open = defineModel('open', { type: Boolean, default: false });
const draftFrom = ref('');
const draftTo = ref('');
const choosingEnd = ref(false);
const hoverDay = ref('');
const month = ref(props.today.slice(0, 7));
const focusedDay = ref(props.today);
const trigger = ref(null);
const panel = ref(null);
const weekdays = ['一', '二', '三', '四', '五', '六', '日'];
const active = computed(() => props.range.kind);
const caption = computed(() => rangeLabel(props.range, props.range.from?.slice(0, 4) === props.range.to?.slice(0, 4)));
const fullCaption = computed(() => `${rangeLabel(props.range)} · ${props.timeZone}`);
const months = computed(() => [month.value, shiftMonth(month.value, 1)].filter(Boolean).map((value) => {
  const first = calendarDay(`${value}-01`);
  const last = new Date(first);
  last.setUTCMonth(last.getUTCMonth() + 1, 0);
  const leading = (first.getUTCDay() + 6) % 7;
  return {
    label: `${first.getUTCFullYear()} 年 ${first.getUTCMonth() + 1} 月`,
    days: Array.from({ length: 42 }, (_, index) => {
      const day = index - leading + 1;
      return day > 0 && day <= last.getUTCDate() ? { day, value: `${value}-${String(day).padStart(2, '0')}` } : null;
    }),
  };
}));
function moveMonth(step) {
  const next = shiftMonth(month.value, step);
  if (next) { month.value = next; focusedDay.value = `${next}-01`; }
}
const preview = computed(() => {
  const end = choosingEnd.value && hoverDay.value ? hoverDay.value : draftTo.value;
  return draftFrom.value && end && draftFrom.value > end ? { from: end, to: draftFrom.value } : { from: draftFrom.value, to: end };
});
function pickDay(day) {
  focusedDay.value = day;
  if (!choosingEnd.value) {
    draftFrom.value = day; draftTo.value = day;
    choosingEnd.value = true;
  } else {
    [draftFrom.value, draftTo.value] = [draftFrom.value, day].sort();
    choosingEnd.value = false;
  }
  hoverDay.value = '';
}
async function moveFocus(event, day) {
  const offsets = { ArrowLeft: -1, ArrowRight: 1, ArrowUp: -7, ArrowDown: 7 };
  if (!(event.key in offsets)) return;
  event.preventDefault();
  const next = shiftDay(day, offsets[event.key]);
  if (!next) return;
  const visibleMonths = window.innerWidth <= 620 ? [month.value] : [month.value, shiftMonth(month.value, 1)];
  if (!visibleMonths.includes(next.slice(0, 7))) month.value = next.slice(0, 7);
  focusedDay.value = next;
  await nextTick(); panel.value?.querySelector(`[data-day="${next}"]`)?.focus();
}
const parsed = computed(() => parseRange(draftFrom.value, draftTo.value));
function selectPreset(kind) { open.value = false; emit('apply', { kind }); }
function applyCustom() {
  if (!parsed.value.value) return;
  open.value = false;
  emit('apply', parsed.value.value);
}
watch(open, async (value) => {
  if (!value) {
    const restoreFocus = panel.value?.contains(document.activeElement);
    await nextTick(); if (restoreFocus) trigger.value?.focus();
    return;
  }
  draftFrom.value = props.range.from; draftTo.value = props.range.to;
  focusedDay.value = props.range.from || props.range.to || props.today;
  month.value = focusedDay.value.slice(0, 7);
  choosingEnd.value = false; hoverDay.value = '';
  await nextTick(); panel.value?.querySelector('input')?.focus();
});
</script>

<template>
  <div class="range-control" role="group" aria-label="时间范围">
    <span class="range-label"><CalendarDays :size="14" />时间</span>
    <div class="range-options" aria-label="选择时间范围">
      <button type="button" :class="{ selected: active === 'today' }" :aria-pressed="active === 'today'" @click="selectPreset('today')">今天</button>
      <button type="button" :class="{ selected: active === 'recent' }" :aria-pressed="active === 'recent'" @click="selectPreset('recent')">最近三天</button>
      <button type="button" :class="{ selected: active === 'all' }" :aria-pressed="active === 'all'" @click="selectPreset('all')">全部</button>
      <BasePopover v-model="open" placement="bottom-end" :offset="8">
        <template #trigger>
          <button type="button" ref="trigger" class="custom-range-trigger" :class="{ selected: active === 'custom' }" :aria-pressed="active === 'custom'" aria-haspopup="dialog" :aria-expanded="open">自定义<ChevronDown :size="12" /></button>
        </template>
        <section ref="panel" class="range-popover" role="dialog" aria-label="自定义时间范围" @keydown.stop @keydown.esc.prevent="open = false">
          <header class="range-popover-heading"><h2>自定义时间范围</h2><BaseIconButton label="关闭日期选择" size="sm" @click="open = false"><X :size="15" /></BaseIconButton></header>
          <div class="range-date-fields">
            <label><span>开始日期</span><input id="date-from" v-model="draftFrom" :aria-invalid="Boolean(parsed.error)" aria-label="开始日期" placeholder="不限" autocomplete="off" @input="choosingEnd = false" /></label>
            <span>—</span>
            <label><span>结束日期</span><input id="date-to" v-model="draftTo" :aria-invalid="Boolean(parsed.error)" aria-label="结束日期" placeholder="不限" autocomplete="off" @input="choosingEnd = false" /></label>
          </div>
          <div class="range-month-navigation">
            <BaseIconButton label="上个月" size="sm" :disabled="!shiftMonth(month, -1)" @click="moveMonth(-1)"><ChevronLeft :size="15" /></BaseIconButton>
            <div class="range-month-titles"><span v-for="(item, index) in months" :key="item.label" :class="{ 'range-second-month': index === 1 }">{{ item.label }}</span></div>
            <BaseIconButton label="下个月" size="sm" :disabled="!shiftMonth(month, 1)" @click="moveMonth(1)"><ChevronRight :size="15" /></BaseIconButton>
          </div>
          <div class="range-calendars" @mouseleave="hoverDay = ''">
            <div v-for="(item, monthIndex) in months" :key="item.label" class="range-calendar" :class="{ 'range-second-month': monthIndex === 1 }">
              <span v-for="weekday in weekdays" :key="weekday" class="range-weekday">{{ weekday }}</span>
              <template v-for="(day, index) in item.days" :key="index">
                <button v-if="day" type="button" :data-day="day.value" :aria-label="day.value" :aria-current="day.value === props.today ? 'date' : undefined" :tabindex="day.value === focusedDay ? 0 : -1" @keydown="moveFocus($event, day.value)" @focus="focusedDay = day.value" :class="{ 'range-endpoint': day.value === preview.from || day.value === preview.to, 'range-between': preview.from && preview.to && day.value > preview.from && day.value < preview.to, 'range-today': day.value === props.today }" @click="pickDay(day.value)" @mouseenter="hoverDay = day.value">{{ day.day }}</button>
                <span v-else class="range-blank"></span>
              </template>
            </div>
          </div>
          <p v-if="parsed.error" class="filter-error" role="alert">{{ parsed.error }}</p>
          <p class="range-calendar-hint">{{ choosingEnd ? '选择结束日期，或直接应用单日。' : '选择开始和结束日期。' }}</p>
          <footer class="range-popover-footer">
            <span>审计时区 · {{ props.timeZone }}</span>
            <div><BaseButton variant="ghost" size="sm" @click="open = false">取消</BaseButton><BaseButton variant="primary" size="sm" :disabled="!parsed.value" @click="applyCustom">应用</BaseButton></div>
          </footer>
        </section>
      </BasePopover>
    </div>
    <span class="range-caption" :title="fullCaption" aria-live="polite">{{ props.loading ? '正在读取日期…' : caption }}</span>
  </div>
</template>

<style>
.range-control { display: flex; align-items: center; flex-shrink: 0; gap: 8px; height: 32px; padding: 3px 9px; border: 1px solid var(--cp-color-border-secondary); border-radius: 8px; background: var(--cp-color-bg-container); }
.range-label { display: flex; align-items: center; gap: 5px; font-size: 11px; color: var(--cp-color-text-secondary); white-space: nowrap; }
.range-options { display: flex; align-items: center; gap: 2px; }
.range-options > button, .range-options .custom-range-trigger { display: inline-flex; align-items: center; justify-content: center; gap: 4px; border: 0; border-radius: 5px; padding: 0 8px; height: 24px; background: transparent; color: var(--cp-color-text-secondary); font-size: 12px; white-space: nowrap; }
.range-options button:hover { background: var(--cp-color-fill-quaternary); }
.range-options button.selected { background: var(--cp-color-primary-container); color: var(--cp-color-primary-text); font-weight: 650; }
.range-caption { border-left: 1px solid var(--cp-color-border-secondary); padding-left: 10px; font-size: 11px; color: var(--cp-color-text-tertiary); white-space: nowrap; font-variant-numeric: tabular-nums; }
.range-popover { width: 540px; max-width: calc(100vw - 24px); max-height: calc(100dvh - 24px); overflow-y: auto; padding: 16px; }
.range-popover-heading { display: flex; align-items: center; justify-content: space-between; margin-bottom: 14px; }
.range-popover-heading h2 { font-size: 13px; font-weight: 650; }
.range-date-fields { display: flex; align-items: center; gap: 12px; padding: 8px 12px; margin-bottom: 14px; border: 1px solid var(--cp-color-border-secondary); border-radius: 8px; }
.range-date-fields > span { color: var(--cp-color-text-tertiary); }
.range-date-fields label { flex: 1; min-width: 0; display: flex; align-items: center; gap: 10px; }
.range-date-fields label span { font-size: 10px; color: var(--cp-color-text-tertiary); white-space: nowrap; }
.range-date-fields input { width: 100%; min-width: 0; border: 0; background: transparent; color: var(--cp-color-text); font-size: 12px; outline: none; font-variant-numeric: tabular-nums; }
.range-date-fields input:focus { color: var(--cp-color-primary-text); }
.range-month-navigation { display: flex; align-items: center; gap: 8px; margin-bottom: 8px; }
.range-month-titles { flex: 1; display: grid; grid-template-columns: repeat(2, 1fr); gap: 24px; text-align: center; font-size: 12px; font-weight: 600; }
.range-calendars { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 24px; }
.range-calendar { display: grid; grid-template-columns: repeat(7, minmax(0, 1fr)); row-gap: 3px; }
.range-weekday { display: grid; place-items: center; height: 24px; color: var(--cp-color-text-tertiary); font-size: 10px; }
.range-calendar button, .range-blank { height: 30px; min-width: 0; }
.range-calendar button { position: relative; padding: 0; border: 0; border-radius: 5px; background: transparent; color: var(--cp-color-text); font-size: 12px; font-variant-numeric: tabular-nums; }
.range-calendar button:hover { background: var(--cp-color-fill-quaternary); }
.range-calendar .range-between { border-radius: 0; background: var(--cp-color-primary-container); color: var(--cp-color-primary-text); }
.range-calendar .range-endpoint { background: var(--cp-color-primary); color: var(--cp-color-bg-container); font-weight: 650; }
.range-calendar .range-today::after { content: ''; position: absolute; bottom: 3px; left: calc(50% - 1.5px); width: 3px; height: 3px; border-radius: 50%; background: currentColor; }
.range-calendar-hint { margin-top: 10px; color: var(--cp-color-text-tertiary); font-size: 11px; }
.range-popover-footer { display: flex; flex-wrap: wrap; gap: 10px; align-items: center; justify-content: space-between; margin-top: 14px; padding-top: 12px; border-top: 1px solid var(--cp-color-border-secondary); }
.range-popover-footer > span { font-size: 10px; color: var(--cp-color-text-tertiary); }
.range-popover-footer > div { display: flex; gap: 6px; }
@media (max-width: 620px) {
  .range-control { flex: 1 1 100%; display: grid; grid-template-columns: auto 1fr; height: auto; padding: 8px; gap: 8px; }
  .range-options { justify-content: space-between; }
  .range-options > button, .range-options .custom-range-trigger { padding: 0 6px; }
  .range-caption { grid-column: 1 / -1; padding: 7px 0 0; border-left: 0; border-top: 1px solid var(--cp-color-border-secondary); text-align: right; }
  .range-popover { width: calc(100vw - 24px); padding: 14px; }
  .range-calendars, .range-month-titles { grid-template-columns: minmax(0, 1fr); }
  .range-second-month { display: none; }
  .range-date-fields { gap: 8px; padding: 10px; }
  .range-date-fields label { flex-direction: column; align-items: flex-start; gap: 5px; }
  .range-calendar button, .range-blank { height: 34px; }
}
</style>
