#!/usr/bin/env node
// Converts local conversation logs into review fixtures, never collector evidence.
import { readdir, readFile, mkdir, writeFile } from 'node:fs/promises';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { isIP } from 'node:net';

const CONTEXT_MESSAGES = 12;
const CONTEXT_CHARACTERS = 32_000;
const excludedKinds = /^(?:agents_md\.|environments\.|skills\.|multi_agent\.subagent_notification$|user\.heartbeat$)/;
const points = (text) => Array.from(text);
const count = (message) => message.blocks.reduce((total, block) => total + (block.kind === 'text' ? points(block.text).length : 0), 0);
const pad = (number) => String(number).padStart(3, '0');
const escaped = (text) => text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
const uuidPattern = /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/gi;
const protocolIdPattern = /\b(?:call|turn|thread|session|request|req|response|resp|message|msg)[_-](?!sample_)[A-Za-z0-9_-]{8,}\b/g;
const emailPattern = /\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b/gi;
const ipPattern = /\b(?:\d{1,3}\.){3}\d{1,3}\b/g;
const domainPattern = /\b(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+[a-z]{2,63}\b/gi;
// Several language extensions are also real TLDs. Only fixed, public standard
// filenames are exempt; an ambiguous technical filename is anonymized too.
const standardFileNames = new Set(['README.md', 'AGENTS.md', 'SKILL.md', 'package.json', 'Cargo.toml', 'Cargo.lock', 'management.json', 'openapi.json']);
const isFileName = (value) => standardFileNames.has(value);
const protocolNames = new Set(['user.text', 'user.image', 'user.audio', 'user.file', 'user.video', 'user.answered_question', 'user.heartbeat', 'agents_md.instructions', 'environments.environment_context', 'skills.selected_skill_instructions', 'multi_agent.subagent_notification']);
const isProtocolName = (value) => protocolNames.has(value);
const localPathPattern = /(?<![\w.:/])(?:~?\/|[A-Z]:[\\/])[^\s<>"'`\]})，。；：]+/gi;
const ipv6Pattern = /(?<![\w:])(?:[0-9a-f]{0,4}:){2,7}[0-9a-f]{0,4}(?![\w:])/gi;
const credentialAssignmentPattern = /(?:\b[a-z0-9_-]*(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|database[_-]?url)[a-z0-9_-]*\b|密码|口令|密钥|令牌|凭据)[*`"']*\s*(?:[:：=]|\s+(?:is|was)\b)\s*("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`[^`]*`|[^\s"'`,;<>]+)/gi;
const secretPatterns = [
  /-----BEGIN (?:[A-Z ]*PRIVATE KEY|OPENSSH PRIVATE KEY)-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----/g,
  /\b(?:sk-[A-Za-z0-9_-]{12,}|(?:gh[pousr]_|github_pat_)[A-Za-z0-9_]{12,}|(?:AKIA|ASIA)[A-Z0-9]{16})\b/g,
  /\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b/g,
  /\b(?:Bearer|Basic)\s+[A-Za-z0-9_+/.=-]{8,}/gi,
  /\b(?:[a-z0-9]+[-_])+password[-_][a-z0-9_-]{4,}\b/gi,
  credentialAssignmentPattern,
  /\b(?:postgres(?:ql)?|mysql|redis):\/\/[^\s<>"'`]+/gi,
];

// The mapping is deliberately kept in memory. No reverse lookup is exported.
export function createRedactor() {
  const maps = new Map();
  const known = new Set();
  const labels = new Map();
  const replacements = {};
  const alias = (kind, original) => {
    known.add(original);
    if (!maps.has(kind)) maps.set(kind, new Map());
    const map = maps.get(kind);
    if (!map.has(original)) map.set(original, `${kind}-${pad(map.size + 1)}`);
    replacements[kind] = (replacements[kind] || 0) + 1;
    return map.get(original);
  };
  function register(metadata = {}) {
    const cwd = metadata.cwd;
    if (typeof cwd === 'string') {
      const user = /\/(?:home|Users)\/([^/]+)/.exec(cwd)?.[1];
      if (user && user.length >= 3) labels.set(user, alias('person', user));
      const parts = /\/(?:projects|ai_codings)\/(.+)/.exec(cwd)?.[1]?.split('/') || [];
      for (const part of [parts[0], parts.at(-1)]) {
        if (part && part.length >= 4 && !part.startsWith('.') && !labels.has(part)) labels.set(part, alias('project', part));
      }
      known.add(cwd);
    }
    for (const id of [metadata.id, metadata.session_id, metadata.parent_thread_id, metadata.forked_from_id]) {
      if (typeof id === 'string' && id.length >= 8 && !labels.has(id)) labels.set(id, alias('identity', id));
    }
    learn(metadata.git?.repository_url || '');
  }
  function learn(text) {
    credentialAssignmentPattern.lastIndex = 0;
    for (const match of String(text).matchAll(credentialAssignmentPattern)) {
      let value = match[1];
      if (/^["'`]/.test(value)) {
        try { value = JSON.parse(value); } catch { value = value.slice(1, -1).replace(/\\(["'`\\])/g, '$1'); }
      }
      if (value.length >= 4 && !labels.has(value)) labels.set(value, alias('credential', value));
    }
    for (const match of String(text).matchAll(/\/(?:home|Users)\/([^/\s<>"'`]+)\/[^\s<>"'`]+/g)) {
      const user = match[1];
      if (user.length >= 3 && !labels.has(user)) labels.set(user, alias('person', user));
    }
    for (const match of String(text).matchAll(/\/(?:projects|ai_codings)\/([^/\s<>"'`]+)/g)) {
      const project = match[1];
      if (project.length >= 4 && !labels.has(project)) labels.set(project, alias('project', project));
    }
    for (const match of String(text).matchAll(/(?:https?:\/\/[^/\s]+\/|git@[^:\s]+:)([\w.-]+)\/([\w.-]+)/g)) {
      for (const name of [match[1], match[2].replace(/\.git$/, '')]) {
        if (name.length >= 4 && !labels.has(name)) labels.set(name, alias('project', name));
      }
    }
  }
  function redact(original) {
    let text = String(original);
    // One pass prevents aliases from being rewritten by another learned label.
    // Names adjacent to Chinese prose or within hyphenated identifiers still
    // identify a project/person and must not bypass deidentification.
    const labelPattern = [...labels.keys()].sort((a, b) => b.length - a.length).map(escaped).join('|');
    if (labelPattern) {
      const byLowercase = new Map([...labels].map(([raw, replacement]) => [raw.toLowerCase(), replacement]));
      text = text.replace(new RegExp(labelPattern, 'gi'), (value) => byLowercase.get(value.toLowerCase()));
    }
    for (const pattern of secretPatterns) text = text.replace(pattern, (value) => `[${alias('credential', value)}]`);
    // Replace complete URLs first so authentication, query strings, repository
    // owners, and URL paths cannot survive independent domain replacement.
    text = text.replace(/\b(?:https?|ssh|git):\/\/[^\s<>"'`，。]+/gi, (value) => {
      const suffix = /[).,;]+$/.exec(value)?.[0] || '';
      const raw = suffix ? value.slice(0, -suffix.length) : value;
      let host = raw;
      try { host = new URL(raw).hostname.toLowerCase(); } catch { /* Malformed URLs are still anonymized. */ }
      return `https://${alias('host', host)}.example.invalid/resource${suffix}`;
    });
    text = text.replace(/\b[^\s@<>"'`]+@(?:[A-Za-z0-9-]+\.)+[A-Za-z]+:[^\s<>"'`，。]+/g,
      (value) => `git@${alias('host', value)}.example.invalid:repository.git`);
    text = text.replace(emailPattern, (value) => `${alias('contact', value)}@example.invalid`);
    text = text.replace(localPathPattern, (value) => `/workspace/${alias('path', value)}`);
    text = text.replace(uuidPattern, (value) => `[${alias('identity', value)}]`);
    text = text.replace(protocolIdPattern, (value) => `[${alias('identity', value)}]`);
    text = text.replace(ipPattern, (value) => `[${alias('address', value)}]`);
    text = text.replace(/\b\d{12}\b/g, (value) => `[${alias('account', value)}]`);
    text = text.replace(ipv6Pattern, (value) => isIP(value) === 6 ? `[${alias('address', value)}]` : value);
    text = text.replace(/\b(?:[0-9a-f]*[a-f][0-9a-f]*\d[0-9a-f]*|[0-9a-f]*\d[0-9a-f]*[a-f][0-9a-f]*)\b/gi,
      (value) => value.length >= 7 && value.length <= 64 ? `[${alias('revision', value)}]` : value);
    text = text.replace(domainPattern, (value) => isFileName(value) || /(?:^|\.)example\.invalid$/i.test(value) ? value : `${alias('host', value)}.example.invalid`);
    return text;
  }
  function verify(value) {
    // Provenance/stats are generated constants and aggregates. Ignore alias
    // spellings when checking learned names (a project may itself be "project").
    const inspected = value && typeof value === 'object' && Object.hasOwn(value, 'stats') ? Object.fromEntries(Object.entries(value).filter(([key]) => key !== 'stats')) : value;
    const strings = (value, field = '') => typeof value === 'string' ? field === 'inputKind' && isProtocolName(value) ? [] : [value] : value && typeof value === 'object' ? Object.entries(value).flatMap(([key, child]) => strings(child, key)) : [];
    const rawOutput = strings(inspected).join('\n');
    const output = rawOutput.replace(/(?:project|person|host|contact|credential|path|identity|address|revision|account)-\d{3,}/g, 'anonymous');
    const labelOutput = output.replace(/example\.invalid/gi, 'reserved-host');
    const failures = new Set();
    for (const pattern of [...secretPatterns, uuidPattern, protocolIdPattern, ipPattern]) {
      pattern.lastIndex = 0;
      if (pattern.test(output)) failures.add('sensitive-pattern');
    }
    domainPattern.lastIndex = 0;
    for (const match of output.matchAll(domainPattern)) if (!isFileName(match[0]) && !/(?:^|\.)example\.invalid$/i.test(match[0])) failures.add('original-domain');
    ipv6Pattern.lastIndex = 0;
    for (const match of output.matchAll(ipv6Pattern)) if (isIP(match[0]) === 6) failures.add('original-address');
    localPathPattern.lastIndex = 0;
    for (const match of rawOutput.matchAll(localPathPattern)) if (!/^\/workspace\/path-\d+$/.test(match[0])) failures.add('original-path');
    for (const raw of known) if (raw.length >= 8 && labelOutput.toLowerCase().includes(raw.toLowerCase())) failures.add('original-identifier');
    for (const raw of labels.keys()) {
      if (new RegExp(`(?<![\\p{L}\\p{N}_])${escaped(raw)}(?![\\p{L}\\p{N}_])`, 'iu').test(labelOutput)) failures.add('original-label');
    }
    if (failures.size) throw new Error(`Deidentification check failed (${[...failures].join(', ')}); no fixture was written.`);
    return true;
  }
  return { register, learn, redact, verify, summary: () => ({ ...replacements }) };
}

function isInternalSource(metadata) {
  return [metadata.source, metadata.thread_source].some((source) =>
    typeof source === 'object' && source !== null && Object.keys(source).some((name) => /subagent|guardian|memory|compact/i.test(name))
    || typeof source === 'string' && /^(?:subagent|guardian(?:_review)?|memory(?:_consolidation)?|compact(?:ion)?)$/i.test(source));
}
function inputBlocks(payload) {
  const content = Array.isArray(payload.content) ? payload.content : [];
  const kinds = payload.internal_chat_message_metadata_passthrough?.content_item_kinds;
  const classified = Array.isArray(kinds) && kinds.length === content.length && kinds.every((kind) => typeof kind === 'string');
  return content.flatMap((block, index) => {
    const inputKind = classified ? kinds[index] : null;
    if (inputKind && excludedKinds.test(inputKind)) return [];
    if (typeof block.text === 'string' && ['input_text', 'output_text', 'text'].includes(block.type)) return [{ kind: 'text', text: block.text, inputKind }];
    if (/image|audio|file|video/.test(block.type || '')) return [{ kind: 'nontext', mediaType: /image/.test(block.type) ? '图片' : /audio/.test(block.type) ? '音频' : /video/.test(block.type) ? '视频' : '文件', description: null, inputKind }];
    return [];
  });
}

// Only response_item messages are authoritative. event_msg mirrors messages
// and can contain tool/internal events, so its text is never imported.
export function parseSessionRows(rows) {
  const metadata = rows.find((row) => row.type === 'session_meta')?.payload || {};
  const messages = [];
  const ids = new Set();
  const turns = new Map();
  for (const row of rows) {
    if (row.type !== 'turn_context' || typeof row.payload?.turn_id !== 'string') continue;
    const value = { model: typeof row.payload.model === 'string' ? row.payload.model : null, reasoningEffort: typeof row.payload.effort === 'string' ? row.payload.effort : null };
    const previous = turns.get(row.payload.turn_id);
    turns.set(row.payload.turn_id, previous && JSON.stringify(previous) !== JSON.stringify(value) ? { model: null, reasoningEffort: null } : value);
  }
  for (const row of rows) {
    const payload = row.payload;
    if (row.type !== 'response_item' || payload?.type !== 'message' || !['user', 'assistant'].includes(payload.role)) continue;
    if (payload.role === 'assistant' && [payload.phase, payload.channel].some((value) => typeof value === 'string' && /^(?:analysis|reasoning|thinking)$/i.test(value))) continue;
    if (payload.id && ids.has(payload.id)) continue;
    if (payload.id) ids.add(payload.id);
    const blocks = payload.role === 'user' ? inputBlocks(payload) : (payload.content || []).flatMap((block) => typeof block.text === 'string' && ['output_text', 'text'].includes(block.type) ? [{ kind: 'text', text: block.text }] : []);
    if (!blocks.length) continue;
    const timestamp = typeof row.timestamp === 'string' && !Number.isNaN(Date.parse(row.timestamp)) ? new Date(row.timestamp).toISOString() : null;
    const turn = payload.internal_chat_message_metadata_passthrough?.turn_id;
    messages.push({ role: payload.role, blocks, phase: payload.role === 'assistant' && typeof payload.phase === 'string' && points(payload.phase).length <= 80 ? payload.phase : null, timestamp, turnId: typeof turn === 'string' ? turn : null, requestMetadata: { ...(turns.get(turn) || { model: null, reasoningEffort: null }) } });
  }
  return { metadata, messages, internal: isInternalSource(metadata) };
}

export function contextWindow(messages) {
  const window = messages.slice(-CONTEXT_MESSAGES).map((message) => ({ ...(message.id ? { id: message.id } : {}), role: message.role, blocks: message.blocks.map((block) => ({ ...block })), ...(message.role === 'assistant' ? { phase: message.phase } : {}) }));
  let total = window.reduce((sum, message) => sum + count(message), 0);
  while (total > CONTEXT_CHARACTERS && window.length > 1) {
    const oldestUser = window.findIndex((message) => message.role === 'user');
    const index = oldestUser < 0 ? 0 : oldestUser;
    total -= count(window[index]); window.splice(index, 1);
  }
  if (total > CONTEXT_CHARACTERS) {
    let budget = CONTEXT_CHARACTERS;
    const blocks = [];
    for (const block of [...window[0].blocks].reverse()) {
      if (block.kind === 'nontext') { blocks.unshift(block); continue; }
      const chars = points(block.text);
      blocks.unshift({ ...block, text: chars.slice(Math.max(0, chars.length - budget)).join('') });
      budget -= Math.min(budget, chars.length);
      if (!budget) break;
    }
    window[0].blocks = blocks; window[0].truncated = true;
  }
  return window;
}

export function buildSessionFixtures(sessions, { limit = 100, minAgentContext = 1, projectOnly = true, maxInputCharacters = Infinity, maxRecordCharacters = Infinity } = {}) {
  const redactor = createRedactor();
  const parsed = sessions.map((session) => Array.isArray(session) ? parseSessionRows(session) : session)
    .filter((session) => !session.internal && (!projectOnly || /\/(?:projects|ai_codings)\//.test(session.metadata.cwd || '')))
    .sort((a, b) => (b.metadata.timestamp || '').localeCompare(a.metadata.timestamp || ''));
  for (const session of parsed) redactor.register(session.metadata);
  for (const session of parsed) for (const message of session.messages) for (const block of message.blocks) if (block.kind === 'text') redactor.learn(block.text);
  const candidates = [];
  for (let sessionIndex = 0; sessionIndex < parsed.length; sessionIndex++) {
    const session = parsed[sessionIndex];
    const keyId = `ck_session_${pad(sessionIndex + 1)}`;
    const keyName = `会话样本 ${pad(sessionIndex + 1)}`;
    const turns = new Map();
    const safeMessages = session.messages.map((message, index) => ({ ...message, id: `msg_sample_${pad(sessionIndex + 1)}_${pad(index + 1)}`, blocks: message.blocks.map((block) => block.kind === 'text' ? { ...block, text: redactor.redact(block.text) } : { ...block }) }));
    for (let index = 0; index < safeMessages.length; index++) {
      const message = safeMessages[index];
      if (message.role !== 'user' || !message.timestamp) continue;
      const context = contextWindow(safeMessages.slice(0, index));
      if (context.filter((entry) => entry.role === 'assistant').length < minAgentContext) continue;
      if (count(message) > maxInputCharacters || count(message) + context.reduce((sum, entry) => sum + count(entry), 0) > maxRecordCharacters) continue;
      const number = pad(candidates.length + 1);
      if (message.turnId && !turns.has(message.turnId)) turns.set(message.turnId, `turn_sample_${pad(sessionIndex + 1)}_${pad(turns.size + 1)}`);
      candidates.push({
        audit: { id: `PA-SESSION-${number}`, messageId: message.id, firstObservedAt: message.timestamp, keyId, keyName,
          threadId: `thread_sample_${pad(sessionIndex + 1)}`, sessionId: `session_sample_${pad(sessionIndex + 1)}`,
          requestId: `req_sample_${number}`, turnId: message.turnId ? turns.get(message.turnId) : null,
          repositories: null, input: message.blocks, context },
        request: { id: `req_sample_${number}`, keyId, userAgent: null,
          model: message.requestMetadata.model ? redactor.redact(message.requestMetadata.model) : null,
          reasoningEffort: message.requestMetadata.reasoningEffort },
      });
    }
  }
  const selected = candidates.sort((a, b) => b.audit.firstObservedAt.localeCompare(a.audit.firstObservedAt) || b.audit.id.localeCompare(a.audit.id)).slice(0, limit);
  const audits = selected.map(({ audit }) => audit);
  const keyIds = new Set(audits.map((audit) => audit.keyId));
  const keys = parsed.map((_, index) => ({ id: `ck_session_${pad(index + 1)}`, names: [`会话样本 ${pad(index + 1)}`] })).filter((key) => keyIds.has(key.id));
  const stats = {
    source: 'deidentified-codex-session-sample',
    timestampSemantics: 'Session response_item timestamp mapped for mock review; not gateway first observation.',
    syntheticMetadata: ['keyId', 'keyName', 'threadId', 'sessionId', 'requestId'],
    repositorySemantics: 'Unavailable: local session cwd/git do not establish request-reported workspaces.',
    eligibleSessions: parsed.length, selectedSessions: keys.length, eligibleInputs: candidates.length, records: audits.length,
    contextMessages: audits.reduce((sum, audit) => sum + audit.context.length, 0),
    agentMessages: audits.reduce((sum, audit) => sum + audit.context.filter((message) => message.role === 'assistant').length, 0),
    commentaryMessages: audits.reduce((sum, audit) => sum + audit.context.filter((message) => message.phase === 'commentary').length, 0),
    finalAnswerMessages: audits.reduce((sum, audit) => sum + audit.context.filter((message) => message.phase === 'final_answer').length, 0),
    truncatedContextMessages: audits.reduce((sum, audit) => sum + audit.context.filter((message) => message.truncated).length, 0),
    uniqueUserMessages: new Set(audits.flatMap((audit) => [audit.messageId, ...audit.context.filter((message) => message.role === 'user').map((message) => message.id)])).size,
    contextWindow: { messages: CONTEXT_MESSAGES, unicodeCodePoints: CONTEXT_CHARACTERS },
    redactions: redactor.summary(),
  };
  const output = { keys, audits, modelRequests: selected.map(({ request }) => request), stats };
  redactor.verify(output);
  return output;
}

async function sessionFiles(root) {
  const files = [];
  for (const entry of await readdir(root, { withFileTypes: true })) {
    const path = join(root, entry.name);
    if (entry.isDirectory()) files.push(...await sessionFiles(path));
    else if (entry.isFile() && entry.name.endsWith('.jsonl')) files.push(path);
  }
  return files;
}

function options(args) {
  const result = { limit: 100, days: 2, minAgentContext: 1 };
  const names = { '--sessions': 'sessions', '--output': 'output', '--limit': 'limit', '--days': 'days', '--min-agent-context': 'minAgentContext', '--max-input-characters': 'maxInputCharacters', '--max-record-characters': 'maxRecordCharacters' };
  for (let index = 0; index < args.length; index++) {
    const name = names[args[index]];
    if (!name || args[index + 1] === undefined || args[index + 1].startsWith('--')) throw new Error('Expected --sessions directory --output file [--limit 100] [--days 2] [--min-agent-context 1].');
    result[name] = args[++index];
  }
  if (!result.sessions || !result.output) throw new Error('--sessions and --output are required.');
  for (const name of ['limit', 'days', 'minAgentContext', 'maxInputCharacters', 'maxRecordCharacters'].filter((name) => result[name] !== undefined)) {
    result[name] = Number(result[name]);
    if (!Number.isSafeInteger(result[name]) || result[name] < (name === 'minAgentContext' ? 0 : 1)) throw new Error(`Invalid ${name}.`);
  }
  return result;
}

export async function generateSessionFixtures(args) {
  const config = options(args);
  const root = resolve(config.sessions);
  const output = resolve(config.output);
  if (output === root || output.startsWith(`${root}/`)) throw new Error('Output must be outside the source sessions directory.');
  const files = (await sessionFiles(root)).sort((a, b) => basename(b).localeCompare(basename(a)));
  const dated = files.map((file) => ({ file, date: /(?:rollout-)?(\d{4}-\d{2}-\d{2})/.exec(basename(file))?.[1] })).filter((entry) => entry.date);
  const latest = dated[0]?.date;
  if (!latest) throw new Error('No dated session JSONL files found.');
  const cutoff = Date.parse(`${latest}T00:00:00Z`) - (config.days - 1) * 86_400_000;
  const sessions = [];
  let malformedLines = 0;
  for (const { file, date } of dated.filter((entry) => Date.parse(`${entry.date}T00:00:00Z`) >= cutoff)) {
    const rows = [];
    for (const line of (await readFile(file, 'utf8')).split('\n').filter(Boolean)) {
      try { rows.push(JSON.parse(line)); } catch { malformedLines++; }
    }
    sessions.push(parseSessionRows(rows));
  }
  const fixture = buildSessionFixtures(sessions, config);
  fixture.stats.malformedLines = malformedLines;
  fixture.stats.sampleDateRange = { from: new Date(cutoff).toISOString().slice(0, 10), to: latest };
  if (!fixture.audits.length) throw new Error('No qualifying human inputs with preceding Agent context found.');
  await mkdir(dirname(output), { recursive: true });
  await writeFile(output, `${JSON.stringify(fixture, null, 2)}\n`, { mode: 0o600 });
  return fixture.stats;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try { console.log(JSON.stringify(await generateSessionFixtures(process.argv.slice(2)), null, 2)); }
  catch (error) { console.error(error.code ? `Local fixture generation failed (${error.code}); no source content was logged.` : error.message); process.exitCode = 1; }
}
