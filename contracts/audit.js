import { z } from 'zod';

export const PAGE_SIZES = [50, 100, 200];
export const MAX_RESPONSE_BYTES = 512 * 1024;
export const MAX_QUERY_BYTES = 8192;
export const CONTENT_CHARACTERS = 1200;
export const MAX_EXCLUSION_PATTERNS = 32;
export const MAX_EXCLUSION_PATTERN_BYTES = 1024;
const id = z.string().regex(/^[\s\S]{1,256}$/u);
const cursor = z.string().regex(/^[\s\S]{1,2048}$/u);
const count = z.number().int().nonnegative().max(Number.MAX_SAFE_INTEGER);
const boundedText = (max) => z.string().regex(new RegExp(`^[\\s\\S]{0,${max}}$`, 'u'));
const date = z.iso.date();
const timestamp = z.iso.datetime({ precision: 3 });
const displayTime = z.string().regex(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/).describe('Server-generated local time text; render verbatim, never parse as an instant.');
const timeZone = z.string().min(1).max(128).describe('Shared audit IANA timezone, default UTC.');
export const timeZoneSetting = z.object({ timeZone }).strict();
const userAgentPattern = z.string().max(MAX_EXCLUSION_PATTERN_BYTES).regex(/\P{White_Space}/u)
  .refine((pattern) => new TextEncoder().encode(pattern).byteLength <= MAX_EXCLUSION_PATTERN_BYTES, { message: 'Pattern exceeds UTF-8 byte limit.' })
  .describe('Nonblank Rust regex source, at most 1024 UTF-8 bytes. Preserved verbatim; syntax is validated by the Rust backend.');
export const exclusionSetting = z.object({
  userAgentPatterns: z.array(userAgentPattern).max(MAX_EXCLUSION_PATTERNS).describe('Any matching pattern excludes future audit storage. An empty list clears custom exclusions.'),
}).strict();
const keyName = boundedText(512).nullable().describe('Current host Client Key name; null when the Key is missing or its name cannot be represented.');
const pageQuery = { cursor: cursor.optional() };

export const auditQuery = z.object({
  from: date.optional().describe('Inclusive date in the saved audit timezone; absent means unbounded.'),
  to: date.optional().describe('Inclusive date in the saved audit timezone; absent means unbounded.'),
  key: id.optional().describe('Stable Client Key ID, including historical keys.'),
  q: boundedText(200).optional().describe('Trimmed literal substring of input text only; ASCII case insensitive.'),
  limit: z.enum(['50', '100', '200']).default('50').describe('Target maximum; the byte budget may return fewer rows.'),
  ...pageQuery,
}).strict();
export const detailQuery = z.object({ id }).strict();
export const contentQuery = z.object({ id, contentId: id, ...pageQuery }).strict();
export const keysQuery = z.object({ ...pageQuery }).strict();
export const viewContext = z.object({
  timeZone,
  today: date.describe('Current date from the server clock in the saved audit timezone; used for the initial filter.'),
});

const contentDescriptor = z.object({
  id,
  characterCount: count.describe('Unicode code points in stored text blocks; excludes nontext descriptions.'),
  blockCount: count,
});
export const auditSummary = z.object({
  id, recordedAt: timestamp, recordedAtDisplay: displayTime, keyId: id, keyName,
  preview: boundedText(400).describe('Original input excerpt, centered on a match when q is present. Never HTML.'),
  contextCount: count,
});
export const auditPage = z.object({
  timeZone, items: z.array(auditSummary).max(200), total: count,
  nextCursor: cursor.nullable(),
});
export const auditDetail = z.object({
  timeZone,
  id, recordedAt: timestamp, recordedAtDisplay: displayTime, keyId: id, keyName,
  threadId: id.nullable(), sessionId: id.nullable(), requestId: id.nullable(),
  turnId: id.nullable().describe('The input message turn_id; never inferred from the enclosing request turn.'),
  inputKinds: z.array(boundedText(256).nullable()).describe('Client classifications aligned with saved input blocks; null for each unavailable classification.'),
  repositories: z.array(z.object({
    rootPath: boundedText(4096),
    remotes: z.array(z.object({ name: boundedText(256), url: boundedText(8192) })).nullable(),
  })).nullable().describe('Repository snapshot observed on requestId, not necessarily when a historical input was written. Null means unavailable; [] means reported empty.'),
  input: contentDescriptor,
  context: z.array(z.object({
    id, role: z.enum(['user', 'assistant']), phase: boundedText(80).nullable(),
    truncated: z.boolean().describe('Only a suffix of the original message was collected.'),
    content: contentDescriptor,
  })),
}).refine((audit) => audit.inputKinds.length === audit.input.blockCount, { message: 'Input classifications must align with saved blocks.' });
export const requestMetadata = z.object({
  auditId: id,
  requestMetadata: z.object({
    userAgent: boundedText(8192).nullable(),
    model: boundedText(256).nullable().describe('Host requestedModel, not the upstream resolved model.'),
    reasoningEffort: boundedText(80).nullable(),
  }).nullable().describe('Loaded explicitly through the host request detail API matching the saved requestId and keyId; not duplicated in audit storage. Null when no matching host record exists.'),
});
const textSegment = z.object({
  kind: z.literal('text'), index: count, offset: count,
  text: boundedText(CONTENT_CHARACTERS), totalCharacters: count,
});
const nontextSegment = z.object({
  kind: z.literal('nontext'), index: count,
  mediaType: boundedText(80), description: boundedText(512).nullable(),
});
export const contentPage = z.object({
  auditId: id, contentId: id,
  blocks: z.array(z.discriminatedUnion('kind', [textSegment, nontextSegment])).max(32),
  nextCursor: cursor.nullable(),
});
export const keyPage = z.object({
  items: z.array(z.object({ id, name: keyName })).max(100),
  nextCursor: cursor.nullable(),
});
export const errorResponse = z.object({ error: z.object({
  code: z.enum(['invalid_query', 'invalid_exclusions', 'invalid_cursor', 'not_found', 'cursor_expired', 'unavailable', 'internal_error']),
  message: boundedText(300),
  patternIndex: count.optional().describe('Zero-based row of an invalid pattern; only supplied with invalid_exclusions, when available.'),
}).refine((error) => error.patternIndex === undefined || error.code === 'invalid_exclusions', { message: 'Pattern index is only valid for invalid_exclusions.' }) });
export const endpoints = {
  timeZone: { method: 'PUT', path: 'api/time-zone', query: z.object({}).strict(), body: timeZoneSetting, response: timeZoneSetting, summary: 'Save the shared audit timezone explicitly; does not refresh audit queries.' },
  exclusions: { path: 'api/exclusions', query: z.object({}).strict(), response: exclusionSetting, summary: 'Get plugin-owned User-Agent exclusions for future audit storage.' },
  saveExclusions: { method: 'PUT', path: 'api/exclusions', query: z.object({}).strict(), body: exclusionSetting, response: exclusionSetting, summary: 'Validate Rust regexes and replace the saved exclusion list; does not refresh audit queries.' },
  viewContext: { path: 'api/view-context', query: z.object({}).strict(), response: viewContext, summary: 'Get the saved audit timezone and server-local date for initial page filters.' },
  audits: { path: 'api/audits', query: auditQuery, response: auditPage, summary: 'List input excerpts using a stable descending cursor.' },
  audit: { path: 'api/audit', query: detailQuery, response: auditDetail, summary: 'Get saved audit metadata and ordered content descriptors; no full bodies or host request metadata.' },
  requestMetadata: { path: 'api/audit-request-metadata', query: detailQuery, response: requestMetadata, summary: 'Explicitly load host request metadata for one saved audit; no content bodies.' },
  content: { path: 'api/audit-content', query: contentQuery, response: contentPage, summary: 'Read ordered stored content in bounded chunks.' },
  keys: { path: 'api/keys', query: keysQuery, response: keyPage, summary: 'List current host Client Keys with their current names, ordered by ID.' },
};

export function parseQuery(endpoint, params) {
  if (new TextEncoder().encode(params.toString()).byteLength > MAX_QUERY_BYTES) throw new Error('Query exceeds byte budget');
  if ([...params.keys()].some((key) => params.getAll(key).length !== 1)) throw new Error('Duplicate query parameter');
  const query = endpoints[endpoint].query.parse(Object.fromEntries(params));
  if (endpoint === 'audits' && query.from && query.to && query.from > query.to) throw new Error('Invalid date range');
  if (query.q !== undefined && query.q !== query.q.trim()) throw new Error('Query must be trimmed');
  return query;
}
