import { onScopeDispose, shallowRef } from 'vue';
import { isAborted } from '../api/client.js';

// UTC is only a representation for arithmetic on calendar dates supplied by the server.
export function calendarDay(value) {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(value)) return null;
  const day = new Date(`${value}T12:00:00Z`);
  return !Number.isNaN(day.getTime()) && day.toISOString().slice(0, 10) === value ? day : null;
}
export function shiftDay(value, days) {
  const day = calendarDay(value);
  day.setUTCDate(day.getUTCDate() + days);
  const result = day.toISOString().slice(0, 10);
  return calendarDay(result) ? result : null;
}
export function shiftMonth(value, months) {
  const day = calendarDay(`${value}-01`);
  day.setUTCMonth(day.getUTCMonth() + months);
  const result = day.toISOString().slice(0, 10);
  return calendarDay(result) ? result.slice(0, 7) : null;
}
export function parseRange(from, to) {
  if ((from && !calendarDay(from)) || (to && !calendarDay(to))) return { error: '请输入有效日期，格式为 YYYY-MM-DD。' };
  if (from && to && from > to) return { error: '起始日期不能晚于结束日期。' };
  return { value: { kind: from || to ? 'custom' : 'all', from, to } };
}
export function rangeLabel(range, compact = false) {
  const format = compact ? (value) => value.slice(5).replace('-', '/') : (value) => value;
  if (!range.from && !range.to) return '全部现存记录';
  if (range.from === range.to) return format(range.from);
  if (!range.from) return `截至 ${format(range.to)}`;
  if (!range.to) return `${format(range.from)} 起`;
  return `${format(range.from)} — ${format(range.to)}`;
}

export function useDateRange(client, initial) {
  const context = shallowRef(initial);
  const selection = shallowRef({ kind: 'today', from: initial.today, to: initial.today });
  const state = shallowRef({ kind: 'idle' });
  let controller;
  async function resolve(next = selection.value) {
    controller?.abort();
    const request = controller = new AbortController();
    state.value = { kind: 'loading' };
    try {
      let range = next;
      if (next.kind === 'today' || next.kind === 'recent') {
        const current = await client.viewContext(request.signal);
        if (request.signal.aborted) return null;
        context.value = current;
        range = { kind: next.kind, from: next.kind === 'today' ? current.today : shiftDay(current.today, -2), to: current.today };
      } else if (next.kind === 'all') range = { kind: 'all', from: '', to: '' };
      selection.value = range;
      state.value = { kind: 'idle' };
      return range;
    } catch (error) {
      if (!request.signal.aborted && !isAborted(error)) state.value = { kind: 'error', error };
      return null;
    }
  }
  onScopeDispose(() => controller?.abort());
  return { context, selection, state, resolve };
}
