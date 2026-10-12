import { createHash } from 'node:crypto';
import { audits as fixtureAudits, keys as fixtureKeys, modelRequests as fixtureRequests } from './fixtures.js';
import { CONTENT_CHARACTERS, endpoints, MAX_RESPONSE_BYTES, parseQuery } from '../contracts/audit.js';

const encodeCursor = (value) => Buffer.from(JSON.stringify(value)).toString('base64url');
const bytes = (value) => Buffer.byteLength(JSON.stringify(value));
const fail = (code, status, message) => { throw Object.assign(new Error(message), { code, status }); };
const fold = (text) => text.replace(/[A-Z]/g, (letter) => letter.toLowerCase());
const boundedMetadata = (value, max) => typeof value === 'string' && Array.from(value).length <= max ? value : null;
const sourceText = (audit) => audit.input.filter((block) => block.kind === 'text').map((block) => block.text).join('\n');
function cursorValue(value, scope) {
  if (!value) return null;
  try {
    const parsed = JSON.parse(Buffer.from(value, 'base64url').toString('utf8'));
    if (parsed.scope !== scope || !Object.hasOwn(parsed, 'after')) throw new Error();
    return parsed.after;
  } catch { fail('invalid_cursor', 400, '分页位置与当前查询不匹配，请重新检索。'); }
}
function preview(audit, q) {
  const text = q ? sourceText(audit) : audit.input.map((block) => block.kind === 'text' ? block.text : `[${block.mediaType}：非文本内容未采集]`).join('\n');
  const chars = Array.from(text);
  const match = q ? fold(text).indexOf(fold(q)) : -1;
  const matchPoint = match < 0 ? 0 : Array.from(text.slice(0, match)).length;
  const start = Math.max(0, matchPoint - 35);
  return `${start ? '…' : ''}${chars.slice(start, start + 398).join('')}${chars.length > start + 398 ? '…' : ''}`;
}
function descriptor(id, blocks) {
  return { id, blockCount: blocks.length, characterCount: blocks.reduce((count, block) => count + (block.kind === 'text' ? Array.from(block.text).length : 0), 0) };
}

// Runs in the preview server only. The browser receives the same bounded wire
// responses as a plugin client; fixture bodies are never bundled into the UI.
export function createDemoService({ audits = fixtureAudits, keys = fixtureKeys, modelRequests = fixtureRequests, maxListRows = 200, keyPageSize = 100, timeZone = 'UTC', now = () => new Date() } = {}) {
  let dateFormatter = new Intl.DateTimeFormat('sv-SE', { timeZone });
  let timeFormatter = new Intl.DateTimeFormat('sv-SE', { timeZone, year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', second: '2-digit', hourCycle: 'h23' });
  const metadata = (audit) => ({ id: audit.id, recordedAt: audit.firstObservedAt, recordedAtDisplay: timeFormatter.format(new Date(audit.firstObservedAt)), keyId: audit.keyId, keyName: keys.find((key) => key.id === audit.keyId)?.name || null });
  const viewContext = () => ({ timeZone, today: dateFormatter.format(now()) });
  const records = audits.map((audit) => ({ ...audit, firstObservedAt: new Date(audit.firstObservedAt).toISOString() })).sort((a, b) => b.firstObservedAt.localeCompare(a.firstObservedAt) || (a.id < b.id ? 1 : a.id > b.id ? -1 : 0));
  function list(query) {
    const { cursor, ...filters } = query;
    const scope = `audits:${createHash('sha256').update(JSON.stringify({ timeZone, ...filters })).digest('base64url')}`;
    const after = cursorValue(cursor, scope);
    if (after !== null && (!Array.isArray(after) || after.length !== 2 || after.some((item) => typeof item !== 'string'))) fail('invalid_cursor', 400, '分页位置无效，请重新检索。');
    const matching = records.filter((audit) => {
      const date = dateFormatter.format(new Date(audit.firstObservedAt));
      return (!query.from || date >= query.from) && (!query.to || date <= query.to)
        && (!query.key || audit.keyId === query.key) && (!query.q || fold(sourceText(audit)).includes(fold(query.q)));
    });
    const remaining = matching.filter((audit) => !after || audit.firstObservedAt < after[0] || (audit.firstObservedAt === after[0] && audit.id < after[1]));
    const items = [];
    let last;
    for (const audit of remaining.slice(0, Math.min(Number(query.limit), maxListRows))) {
      const item = { ...metadata(audit), preview: preview(audit, query.q), contextCount: audit.context.length };
      if (bytes({ timeZone, items: [...items, item], total: matching.length, nextCursor: 'x'.repeat(2048) }) > MAX_RESPONSE_BYTES) break;
      items.push(item); last = audit;
    }
    if (!items.length && remaining.length) fail('internal_error', 500, '记录预览超过响应限制。');
    return { timeZone, items, total: matching.length, nextCursor: remaining.length > items.length ? encodeCursor({ scope, after: [last.firstObservedAt, last.id] }) : null };
  }
  function getAudit(id) {
    const audit = records.find((record) => record.id === id);
    if (!audit) fail('not_found', 404, '这条审计记录不存在或已被清理。');
    return audit;
  }
  function detail(query) {
    const audit = getAudit(query.id);
    return {
      timeZone, ...metadata(audit), threadId: audit.threadId ?? null, sessionId: audit.sessionId ?? null, requestId: audit.requestId ?? null,
      turnId: audit.turnId ?? null, inputKinds: audit.input.map((block) => block.inputKind ?? null),
      repositories: audit.repositories ?? null,
      input: descriptor('input', audit.input),
      context: audit.context.map((message, index) => ({ id: `message-${index}`, role: message.role, phase: message.phase ?? null, truncated: message.truncated ?? false, content: descriptor(`context-${index}`, message.blocks) })),
    };
  }
  function requestMetadata(query) {
    const audit = getAudit(query.id);
    const request = modelRequests.find((row) => row.id === audit.requestId);
    if (request && request.keyId !== audit.keyId) fail('unavailable', 503, '宿主请求信息暂时不可用，请重试。');
    return { auditId: audit.id, requestMetadata: request ? { userAgent: boundedMetadata(request.userAgent, 8192), model: boundedMetadata(request.model, 256), reasoningEffort: boundedMetadata(request.reasoningEffort, 80) } : null };
  }
  function content(query) {
    const audit = getAudit(query.id);
    const contextIndex = /^context-(\d+)$/.exec(query.contentId)?.[1];
    const source = query.contentId === 'input' ? audit.input : contextIndex === undefined ? null : audit.context[Number(contextIndex)]?.blocks;
    if (!source) fail('not_found', 404, '记录内容不存在或已被清理。');
    const scope = `content:${timeZone}:${query.id}:${query.contentId}`;
    const position = cursorValue(query.cursor, scope) ?? { index: 0, offset: 0 };
    if (!Number.isSafeInteger(position.index) || !Number.isSafeInteger(position.offset) || position.index < 0 || position.offset < 0
      || position.index > source.length || (position.index === source.length && position.offset !== 0)) fail('invalid_cursor', 400, '正文读取位置无效。');
    let { index, offset } = position;
    let budget = CONTENT_CHARACTERS;
    const blocks = [];
    while (index < source.length && blocks.length < 32) {
      const block = source[index];
      if (block.kind === 'nontext') {
        if (offset !== 0) fail('invalid_cursor', 400, '正文读取位置无效。');
        blocks.push({ kind: 'nontext', index, mediaType: block.mediaType, description: block.description ?? null });
        index++; continue;
      }
      const chars = Array.from(block.text);
      if (offset > chars.length) fail('invalid_cursor', 400, '正文读取位置无效。');
      if (budget === 0 && chars.length > offset) break;
      const text = chars.slice(offset, offset + budget).join('');
      blocks.push({ kind: 'text', index, offset, text, totalCharacters: chars.length });
      const read = Array.from(text).length;
      offset += read; budget -= read;
      if (offset === chars.length) { index++; offset = 0; } else break;
    }
    return { auditId: audit.id, contentId: query.contentId, blocks, nextCursor: index < source.length ? encodeCursor({ scope, after: { index, offset } }) : null };
  }
  function keyList(query) {
    const scope = `keys:${timeZone}`;
    const after = cursorValue(query.cursor, scope);
    if (after !== null && typeof after !== 'string') fail('invalid_cursor', 400, 'Key 分页位置无效。');
    const directory = [...keys].sort((a, b) => a.id < b.id ? -1 : a.id > b.id ? 1 : 0).filter((key) => after === null || key.id > after);
    const items = [];
    for (const key of directory.slice(0, Math.min(keyPageSize, 100))) {
      const item = { id: key.id, name: key.name || null };
      if (bytes({ items: [...items, item], nextCursor: 'x'.repeat(2048) }) > MAX_RESPONSE_BYTES) break;
      items.push(item);
    }
    if (!items.length && directory.length) fail('internal_error', 500, 'Key 列表项超过响应限制。');
    return { items, nextCursor: directory.length > items.length ? encodeCursor({ scope, after: items.at(-1).id }) : null };
  }
  return function handle(path, params, method = 'GET', body) {
    const endpoint = Object.entries(endpoints).find(([, entry]) => entry.path === path)?.[0];
    try {
      if (!endpoint) fail('not_found', 404, '接口不存在。');
      if (method !== (endpoints[endpoint].method || 'GET')) fail('invalid_query', 400, '请求方法无效。');
      let query;
      try { query = parseQuery(endpoint, params); } catch { fail('invalid_query', 400, '筛选条件无效，请检查日期、关键词和分页参数。'); }
      if (endpoint === 'timeZone') {
        let setting, nextDate, nextTime;
        try {
          setting = endpoints.timeZone.body.parse(JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(body)));
          if (!/^[A-Za-z][A-Za-z0-9._+/-]*$/.test(setting.timeZone)) throw new Error();
          nextDate = new Intl.DateTimeFormat('sv-SE', { timeZone: setting.timeZone });
          nextTime = new Intl.DateTimeFormat('sv-SE', { timeZone: setting.timeZone, year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', second: '2-digit', hourCycle: 'h23' });
        } catch { fail('invalid_query', 400, '审计时区无效，请使用 IANA 时区名称。'); }
        timeZone = setting.timeZone; dateFormatter = nextDate; timeFormatter = nextTime;
        return { status: 200, value: { timeZone } };
      }
      const value = { viewContext, audits: list, audit: detail, requestMetadata, content, keys: keyList }[endpoint](query);
      const parsed = endpoints[endpoint].response.parse(value);
      if (bytes(parsed) > MAX_RESPONSE_BYTES) fail('internal_error', 500, '响应超过大小限制。');
      return { status: 200, value: parsed };
    } catch (error) {
      return { status: error.status || 500, value: { error: { code: error.code || 'internal_error', message: error.status ? error.message : '模拟接口内部错误。' } } };
    }
  };
}
