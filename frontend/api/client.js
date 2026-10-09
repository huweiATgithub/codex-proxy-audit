import { CONTENT_CHARACTERS, endpoints, errorResponse, MAX_QUERY_BYTES, MAX_RESPONSE_BYTES, parseQuery } from '../../contracts/audit.js';

export class AuditError extends Error {
  constructor(code, message, status = 0) { super(message); this.name = 'AuditError'; this.code = code; this.status = status; }
}
export const invalidResponse = () => new AuditError('invalid_response', '接口响应不符合审计契约，请检查插件版本。');
const aborted = () => new DOMException('Aborted', 'AbortError');
export const isAborted = (error) => error?.name === 'AbortError';

// One queue covers all readers. Abandoned in-flight bridge calls retain their slot
// until settlement because the host does not support cancellation for management.
export function createAuditClient(host) {
  let active = 0;
  const queue = [];
  function drain() {
    while (active < 4 && queue.length) {
      const job = queue.shift();
      if (job.signal?.aborted) { job.reject(aborted()); continue; }
      active++;
      job.run().then(job.resolve, job.reject).finally(() => { active--; drain(); });
    }
  }
  function request(endpoint, values, signal) {
    return new Promise((resolve, reject) => {
      queue.push({ signal, resolve, reject, run: async () => {
        if (!host || host.version !== 2 || typeof host.request !== 'function') throw new AuditError('host_missing', '请从 Codex Proxy 的插件管理页面打开审计。');
        const params = new URLSearchParams(Object.entries(values).filter(([, value]) => value !== undefined && value !== null && value !== '').map(([key, value]) => [key, String(value)]));
        let parsedQuery;
        try { parsedQuery = parseQuery(endpoint, params); } catch { throw new AuditError('invalid_query', '筛选条件无效，请检查日期和关键词。', 400); }
        const query = new URLSearchParams(parsedQuery).toString();
        if (new TextEncoder().encode(query).byteLength > MAX_QUERY_BYTES) throw new AuditError('invalid_query', '查询条件过长，请缩小筛选范围。', 400);
        let reply;
        try { reply = await host.request({ method: 'GET', path: endpoints[endpoint].path, query }); }
        catch { throw signal?.aborted ? aborted() : new AuditError('transport', '暂时无法读取审计数据，请重试。'); }
        if (signal?.aborted) throw aborted();
        if (reply?.status === 401) throw new AuditError('unauthorized', '登录已失效，请在宿主重新登录。', 401);
        if (reply?.status === 403) throw new AuditError('forbidden', '当前账号没有审计访问权限。', 403);
        if (!reply || typeof reply.contentType !== 'string' || reply.contentType.split(';')[0].trim().toLowerCase() !== 'application/json'
          || !(reply.body instanceof ArrayBuffer) || reply.body.byteLength > MAX_RESPONSE_BYTES) throw invalidResponse();
        let value;
        try { value = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(reply.body)); } catch { throw invalidResponse(); }
        if (reply.status !== 200) {
          const parsed = errorResponse.safeParse(value);
          if (!parsed.success) throw invalidResponse();
          const expected = { invalid_query: 400, invalid_cursor: 400, not_found: 404, cursor_expired: 410, unavailable: 503, internal_error: 500 };
          if (expected[parsed.data.error.code] !== reply.status) throw invalidResponse();
          throw new AuditError(parsed.data.error.code, parsed.data.error.message, reply.status);
        }
        const parsed = endpoints[endpoint].response.safeParse(value);
        if (!parsed.success) throw invalidResponse();
        const result = parsed.data;
        if (result.nextCursor && result.nextCursor === parsedQuery.cursor) throw invalidResponse();
        if (result.items && (new Set(result.items.map((item) => item.id)).size !== result.items.length || (!result.items.length && result.nextCursor))) throw invalidResponse();
        if (endpoint === 'audits' && (result.items.length > Number(parsedQuery.limit) || result.total < result.items.length)) throw invalidResponse();
        if (endpoint === 'audit' && (result.id !== parsedQuery.id || new Set([result.input.id, ...result.context.map((message) => message.content.id)]).size !== result.context.length + 1 || new Set(result.context.map((message) => message.id)).size !== result.context.length)) throw invalidResponse();
        if (endpoint === 'content' && (result.auditId !== parsedQuery.id || result.contentId !== parsedQuery.contentId || (!result.blocks.length && result.nextCursor) || result.blocks.reduce((total, block) => total + (block.kind === 'text' ? Array.from(block.text).length : 0), 0) > CONTENT_CHARACTERS)) throw invalidResponse();
        return result;
      } });
      drain();
    });
  }
  return {
    viewContext: (signal) => request('viewContext', {}, signal),
    list: (query, signal) => request('audits', query, signal),
    detail: (id, signal) => request('audit', { id }, signal),
    content: (id, contentId, cursor, signal) => request('content', { id, contentId, cursor }, signal),
    keys: (cursor, signal) => request('keys', { cursor }, signal),
  };
}
