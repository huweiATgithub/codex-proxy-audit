import test from 'node:test';
import assert from 'node:assert/strict';
import { effectScope } from 'vue';
import { calendarDay, parseRange, shiftDay, shiftMonth, useDateRange } from '../frontend/state/date-range.js';

test('calendar dates cross leap years, year boundaries and DST days without elapsed-time arithmetic', () => {
  assert.equal(shiftDay('2024-03-01', -2), '2024-02-28');
  assert.equal(shiftDay('2026-01-01', -2), '2025-12-30');
  assert.equal(shiftDay('2026-11-02', -2), '2026-10-31');
  assert.equal(shiftMonth('0000-01', 1), '0000-02');
  assert.equal(shiftMonth('0000-01', -1), null);
  assert.equal(shiftMonth('9999-12', 1), null);
  for (const value of ['2026-02-29', '2026-04-31', '2026-2-01', 'not-a-date']) assert.equal(calendarDay(value), null);
  assert.ok(parseRange('2026-10-10', '2026-10-09').error);
  assert.deepEqual(parseRange('', '2026-10-09').value, { kind: 'custom', from: '', to: '2026-10-09' });
  assert.deepEqual(parseRange('', '').value, { kind: 'all', from: '', to: '' });
});

test('relative ranges refresh server dates and failures preserve the last resolved selection', async () => {
  const scope = effectScope();
  let current = { today: '2026-11-01', timeZone: 'America/New_York' }, failed = false;
  const range = scope.run(() => useDateRange({ viewContext: async () => {
    if (failed) throw new Error('unavailable');
    return current;
  } }, current));
  try {
    assert.deepEqual(await range.resolve({ kind: 'recent' }), { kind: 'recent', from: '2026-10-30', to: '2026-11-01' });
    const fixedPage = range.selection.value;
    current = { today: '2026-11-02', timeZone: 'Asia/Tokyo' };
    assert.deepEqual(await range.resolve(), { kind: 'recent', from: '2026-10-31', to: '2026-11-02' });
    assert.equal(fixedPage.to, '2026-11-01');
    assert.equal(range.context.value.timeZone, 'Asia/Tokyo');
    failed = true;
    assert.equal(await range.resolve({ kind: 'today' }), null);
    assert.equal(range.selection.value.kind, 'recent');
    assert.equal(range.state.value.kind, 'error');
    assert.deepEqual(await range.resolve({ kind: 'all' }), { kind: 'all', from: '', to: '' });
  } finally { scope.stop(); }
});

test('a slow relative selection cannot replace a newer selection or survive disposal', async () => {
  const scope = effectScope();
  let complete;
  const pending = new Promise((resolve) => { complete = resolve; });
  const range = scope.run(() => useDateRange({ viewContext: () => pending }, { today: '2026-10-09', timeZone: 'UTC' }));
  const old = range.resolve({ kind: 'recent' });
  await range.resolve({ kind: 'custom', from: '2026-10-01', to: '2026-10-02' });
  complete({ today: '2026-10-10', timeZone: 'UTC' });
  assert.equal(await old, null);
  assert.equal(range.selection.value.kind, 'custom');
  const disposed = range.resolve({ kind: 'today' });
  scope.stop();
  assert.equal(await disposed, null);
  assert.equal(range.selection.value.kind, 'custom');
});
