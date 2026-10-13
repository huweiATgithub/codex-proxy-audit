import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { mkdtempSync, mkdirSync, readFileSync, readdirSync, writeFileSync, createWriteStream } from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { once } from 'node:events';
import { DatabaseSync } from 'node:sqlite';
import { chromium } from '@playwright/test';
import { endpoints, errorResponse, MAX_RESPONSE_BYTES } from '../contracts/audit.js';
import { startUpstream, connectWebSocket } from '../tests/integration/upstream.js';

// This harness creates its own database/services and installs the actual plugin
// archive into an actual host. The existing Demo is never started or imported.
const options = Object.fromEntries(process.argv.slice(2).reduce((pairs, token, index, args) => {
  if (token.startsWith('--')) pairs.push([token.slice(2), args[index + 1]?.startsWith('--') || !args[index + 1] ? true : args[index + 1]]);
  return pairs;
}, []));
if (!options['host-binary'] || !options['host-web'] || !options.package) {
  console.error('Usage: node scripts/integration.js --host-binary /path/codex-proxy-rs --host-web /path/frontend/dist --package /path/plugin.tar.gz [--keep] [--port 4180] [--docker-config /path] [--next-wait-ms 125000]');
  process.exit(2);
}
const nextWaitMs = options['next-wait-ms'] === undefined ? 0 : Number(options['next-wait-ms']);
if (options['next-wait-ms'] !== undefined) assert.ok(Number.isInteger(nextWaitMs) && nextWaitMs >= 125000 && nextWaitMs <= 2147453647, '--next-wait-ms must be an integer from 125000 to 2147453647');
const nextWaitInput = 'INTEGRATION-next-wait downstream completes beyond the SDK execution budget';
const temporary = mkdtempSync(join(tmpdir(), 'codex-audit-integration-'));
const suffix = randomBytes(5).toString('hex');
const pgName = options['postgres-container'] || `codex-audit-pg-${suffix}`;
const redisName = options['redis-container'] || `codex-audit-redis-${suffix}`;
const pgPort = Number(options['postgres-port'] || 25432), redisPort = Number(options['redis-port'] || 26379);
const hostPort = Number(options.port || 4180), base = `http://127.0.0.1:${hostPort}`;
const password = options['database-password'] || '0123456789abcdef0123456789abcdef0123456789abcdef';
const pgUser = options['database-user'] || 'cpr_test', pgDatabase = options['database-name'] || (options['postgres-container'] ? `cpr_audit_${suffix}` : 'cpr_audit_test');
const auditPath = join(temporary, 'runtime/huwei.prompt-audit/audit.sqlite3');
const adminPassword = 'audit-integration-password-2026';
const dockerArguments = options['docker-config'] ? ['--config', options['docker-config']] : [];
let host, cookie, view, browser, upstream, auditDatabase, cleaned = false;
const createdContainers = [];
const reports = [];
function command(binary, args, input) {
  const result = spawnSync(binary, args, { input, encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${binary} failed (${result.status}): ${result.stderr || result.stdout}`);
  return result.stdout.trim();
}
const docker = (args, input) => command('docker', [...dockerArguments, ...args], input);
const sql = (statement) => docker(['exec', '-i', pgName, 'psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1', '-U', pgUser, '-d', pgDatabase], statement);
const auditRows = (statement, ...parameters) => auditDatabase.prepare(statement).all(...parameters);
const auditValue = (statement, ...parameters) => Object.values(auditDatabase.prepare(statement).get(...parameters))[0];
const auditExecute = (statement, ...parameters) => auditDatabase.prepare(statement).run(...parameters);
const auditSnapshot = () => createHash('sha256').update(JSON.stringify({ audits: auditRows('SELECT * FROM audits ORDER BY id'), contents: auditRows('SELECT * FROM contents ORDER BY id') })).digest('hex');
const signingSecret = () => Buffer.from(auditValue("SELECT value FROM settings WHERE name='query_cursor'")).toString('hex');
const delay = (milliseconds) => new Promise((done) => setTimeout(done, milliseconds));
async function until(action, label, milliseconds = 30000) {
  const deadline = Date.now() + milliseconds; let last;
  while (Date.now() < deadline) { try { return await action(); } catch (error) { last = error; await delay(200); } }
  throw new Error(`${label}: ${last?.message || 'timed out'}`);
}
function report(label) { reports.push(label); console.log(`PASS ${label}`); }
async function json(path, body, expected = 200) {
  const response = await fetch(base + path, { method: body === undefined ? 'GET' : 'POST', headers: { ...(cookie ? { cookie } : {}), ...(body === undefined ? {} : { 'content-type': 'application/json' }) }, body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(15000) });
  const value = await response.json();
  assert.equal(response.status, expected, `${path}: ${JSON.stringify(value)}`);
  if (value.code !== undefined) { assert.equal(value.code, 200, `${path}: ${JSON.stringify(value)}`); return value.data; }
  return value;
}
async function uploadPackage(path) {
  const archive = readFileSync(resolve(path));
  const sha = createHash('sha256').update(archive).digest('hex');
  const response = await fetch(base + `/api/admin/plugins/artifacts/upload?sha256=${sha}`, { method: 'POST', headers: { cookie, 'content-type': 'application/gzip' }, body: archive });
  const installed = await response.json(); assert.equal(response.status, 201, JSON.stringify(installed));
  return installed;
}
async function startHost() {
  const log = createWriteStream(join(temporary, 'host.log'), { flags: 'a' });
  const environment = { ...process.env };
  for (const name of ['CPR_DATABASE_URL', 'CPR_REDIS_URL', 'CPR_DATABASE_PASSWORD', 'CPR_REDIS_PASSWORD', 'CPR_SERVER_HOST', 'CPR_SERVER_PORT', 'CPR_WEB_DIST_DIR']) delete environment[name];
  host = spawn(resolve(options['host-binary']), ['serve'], { cwd: temporary, env: environment, stdio: ['ignore', 'pipe', 'pipe'] });
  host.stdout.pipe(log); host.stderr.pipe(log);
  await until(async () => { assert.equal(host.exitCode, null, `host exited; inspect ${temporary}/host.log`); await json('/api/auth/status'); }, 'host startup');
  const login = await fetch(base + '/api/auth/login', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ mode: 'admin', password: adminPassword }) });
  assert.equal(login.status, 200, await login.text());
  cookie = login.headers.get('set-cookie')?.split(';')[0]; assert.ok(cookie, 'host login issued a cookie');
}
async function stopHost() {
  if (!host || host.exitCode !== null) return;
  const exited = once(host, 'exit'); host.kill('SIGTERM');
  const timer = setTimeout(() => host.kill('SIGKILL'), 10000);
  try { await exited; } finally { clearTimeout(timer); }
}
async function management(endpoint, values = {}, expected = 200) {
  const target = view.target;
  const path = `/api/admin/plugins/extensions/${target.instanceId}/${target.artifactSha256}/${target.revision}/api/${endpoints[endpoint].path}?${new URLSearchParams(endpoints[endpoint].body ? {} : values)}`;
  const definition = endpoints[endpoint];
  const response = await fetch(base + path, { method: definition.method || 'GET', headers: { cookie, ...(definition.body ? { 'content-type': 'application/json' } : {}) }, ...(definition.body ? { body: JSON.stringify(values) } : {}), signal: AbortSignal.timeout(15000) });
  const bytes = new Uint8Array(await response.arrayBuffer());
  assert.ok(bytes.length <= MAX_RESPONSE_BYTES, `response budget: ${endpoint}`);
  const value = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes));
  assert.equal(response.status, expected, `${endpoint}: ${JSON.stringify(value)}`);
  return (expected === 200 ? endpoints[endpoint].response : errorResponse).parse(value);
}
const metadata = (thread, extra = {}) => ({ 'x-codex-turn-metadata': JSON.stringify({ thread_id: thread, session_id: `session-${thread}`, thread_source: 'user', workspaces: { '/workspace/integration': { associated_remote_urls: { origin: 'https://test-user:test-password@example.invalid/repo.git' } } }, ...extra }) });
const message = (id, text, turn = 'same-turn', kinds = ['user.text']) => ({ id, type: 'message', role: 'user', content: [{ type: 'input_text', text }], internal_chat_message_metadata_passthrough: { turn_id: turn, content_item_kinds: kinds } });
async function requestModel(key, input, thread, { stream = false, extra = {}, purpose = {}, expectedStatus = 200, timeoutMs = 20000 } = {}) {
  const response = await fetch(base + '/v1/responses', { method: 'POST', headers: { authorization: `Bearer ${key.plaintextKey}`, 'content-type': 'application/json', 'user-agent': 'codex-audit-integration/1.0' }, body: JSON.stringify({ model: 'gpt-5.4', store: false, stream, reasoning: { effort: 'low' }, input, client_metadata: metadata(thread, purpose), ...extra }), signal: AbortSignal.timeout(timeoutMs) });
  const text = await response.text(); assert.equal(response.status, expectedStatus, text);
  if (expectedStatus !== 200) return JSON.parse(text);
  if (!stream) return JSON.parse(text);
  const events = text.split('\n').filter((line) => line.startsWith('data: ') && line !== 'data: [DONE]').map((line) => JSON.parse(line.slice(6)));
  const complete = events.find((event) => event.type === 'response.completed'); assert.ok(complete, text); return complete.response;
}
async function listCount(q, count, key) {
  return until(async () => { const page = await management('audits', { q, ...(key ? { key: key.id } : {}) }); assert.equal(page.total, count); return page; }, `audit count ${q}=${count}`);
}
async function withAuditWriteLock(action) {
  auditDatabase.exec('BEGIN IMMEDIATE');
  try {
    await action();
  } finally {
    auditDatabase.exec('ROLLBACK');
  }
}
async function textContent(auditId, descriptor) {
  let cursor, text = ''; const offsets = new Map();
  do {
    const page = await management('content', { id: auditId, contentId: descriptor.id, ...(cursor ? { cursor } : {}) });
    for (const block of page.blocks) if (block.kind === 'text') { assert.equal(block.offset, offsets.get(block.index) ?? 0); offsets.set(block.index, block.offset + [...block.text].length); text += block.text; }
    cursor = page.nextCursor;
  } while (cursor);
  assert.equal([...text].length, descriptor.characterCount); return text;
}
async function refreshView() {
  view = await until(async () => { const views = await json('/api/admin/plugins/extensions'); const found = views.find((item) => item.target.instanceId === instanceId); assert.ok(found, 'enabled audit management view'); return found; }, 'plugin management registration');
}
let instanceId;
async function run() {
  if (!options['postgres-container']) {
    docker(['run', '-d', '--name', pgName, '-e', `POSTGRES_USER=${pgUser}`, '-e', `POSTGRES_PASSWORD=${password}`, '-e', `POSTGRES_DB=${pgDatabase}`, '-p', `127.0.0.1:${pgPort}:5432`, 'postgres:17-alpine']); createdContainers.push(pgName);
  }
  if (!options['redis-container']) {
    docker(['run', '-d', '--name', redisName, '-p', `127.0.0.1:${redisPort}:6379`, 'redis:7-alpine', 'redis-server', '--requirepass', password]); createdContainers.push(redisName);
  }
  if (options['postgres-container'] && !options['database-name']) {
    docker(['exec', '-i', pgName, 'psql', '-X', '-v', 'ON_ERROR_STOP=1', '-U', pgUser, '-d', 'postgres'], `CREATE DATABASE "${pgDatabase}"`);
  }
  await until(() => { assert.equal(sql('SELECT 1'), '1'); }, 'PostgreSQL startup');
  upstream = await startUpstream(0, { responseDelayMs: (body) => body.input?.some((item) => item.content?.some((part) => part.text === nextWaitInput)) ? nextWaitMs : 0 });
  mkdirSync(join(temporary, 'deploy'));
  writeFileSync(join(temporary, 'deploy/config.yaml'), `schema_version: 1\nhost:\n  timezone: America/New_York\n  listen: {host: 0.0.0.0, port: ${hostPort}}\n  runtime_data_dir: ../runtime\n  logging:\n    level: info\n    stdout: true\n    file: {enabled: false, directory: ../logs, retention_days: 7, max_file_size_mb: 20}\nstore:\n  database: {url: 'postgres://${pgUser}@127.0.0.1:${pgPort}/${pgDatabase}', password: '${password}'}\n  redis: {url: 'redis://127.0.0.1:${redisPort}/', password: '${password}'}\nadmin:\n  default_username: admin@cpr.local\n  default_password: '${adminPassword}'\n  session_ttl_minutes: 10080\n  session_absolute_ttl_minutes: 43200\napi:\n  asset_directory: '${resolve(options['host-web'])}'\n  cors_allowed_origins: []\n  request_timeout_seconds: null\n  request_id_header: x-request-id\nopenai:\n  api: {base_url: '${upstream.url}/v1'}\n  stream_max_retries: 0\n`);
  await startHost(); report('real host bootstrap, PostgreSQL migrations and admin authentication');
  await json('/api/admin/accounts/import', { provider: 'openai', data: { provider: 'openai', authentication_kind: 'api_key', name: 'Integration controlled upstream', base_url: `${upstream.url}/v1`, api_key: 'sk-integration-only', transport: 'http' } }, 201);
  const makeKey = (name) => json('/api/admin/client-keys/create', { name, groupIds: [], maxConcurrency: 0, requestsPerMinute: 0 }, 201);
  const key = await makeKey('Integration primary'), otherKey = await makeKey('Integration secondary');
  const sha = createHash('sha256').update(readFileSync(resolve(options.package))).digest('hex');
  const install = await uploadPackage(options.package);
  assert.equal(install.data.configurationRequired, false, 'plugin installation requires no configuration');
  instanceId = install.data.defaultInstanceId;
  assert.ok(instanceId, 'host installation creates the default plugin instance');
  const defaultInstance = (await json('/api/admin/plugins/instances')).find((item) => item.id === instanceId);
  assert.ok(defaultInstance?.enabled, 'host enables the plugin with its empty default configuration');
  assert.deepEqual(defaultInstance.configuration, {});
  assert.deepEqual(defaultInstance.secretFields, []);
  await json('/api/admin/plugins/instances/update', { id: instanceId, instance: { expectedRevision: defaultInstance.revision, name: 'Prompt Audit integration', artifactSha256: install.data.artifact.metadata.sha256, enabled: true, configuration: {}, secrets: {}, bindings: [{ contribution: install.data.artifact.metadata.contributes.middleware.id, stage: 'request', order: -1000000, failurePolicy: 'delegate' }] } });
  await refreshView();
  const instance = (await json('/api/admin/plugins/instances')).find((item) => item.id === instanceId);
  assert.equal(instance.compatibilityWarning, null, 'plugin host range and capability versions match the actual host');
  auditDatabase = new DatabaseSync(auditPath, { timeout: 3000 });
  auditDatabase.exec('PRAGMA foreign_keys = ON');
  assert.equal(auditValue('PRAGMA user_version'), 1);
  assert.equal(auditValue('PRAGMA journal_mode'), 'wal');
  assert.deepEqual(instance.configuration, {});
  assert.equal(sql("SELECT count(*) FROM pg_namespace WHERE nspname = 'prompt_audit'"), '0', 'plugin creates no PostgreSQL audit schema');
  report('plugin creates its private SQLite database without database configuration');
  const context = await management('viewContext'); assert.equal(context.timeZone, 'UTC');
  assert.equal(context.today, new Date().toISOString().slice(0, 10));
  report('unmodified host installs plugin with UTC default independent of host timezone');
  const timeZonePath = `/api/admin/plugins/extensions/${view.target.instanceId}/${view.target.artifactSha256}/${view.target.revision}/api/api/time-zone`;
  for (const headers of [{}, { authorization: `Bearer ${key.plaintextKey}` }]) {
    const denied = await fetch(base + timeZonePath, { method: 'PUT', headers: { ...headers, 'content-type': 'application/json' }, body: JSON.stringify({ timeZone: 'Asia/Tokyo' }) });
    assert.ok([401, 403].includes(denied.status), 'timezone save requires administrator session');
  }
  assert.equal((await management('timeZone', { timeZone: 'America/New_York' })).timeZone, 'America/New_York');
  for (const setting of [{ timeZone: 'Invalid/Zone' }, { timeZone: 'UTC', extra: true }, { timeZone: null }, {}]) {
    assert.equal((await management('timeZone', setting, 400)).error.code, 'invalid_query');
    assert.equal((await management('viewContext')).timeZone, 'America/New_York');
  }
  report('administrator-only strict timezone save preserves setting on invalid input');
  const first = message('integration-input-1', 'INTEGRATION-first 请拟定数据库迁移方案。😀');
  const response1 = await requestModel(key, [first], 'thread-http');
  assert.ok(auditRows('PRAGMA table_info(audits)').every((column) => !['key_name', 'key_name_observed_at'].includes(column.name)));
  assert.equal(auditValue("SELECT count(*) FROM sqlite_schema WHERE type='table' AND name IN ('key_directory','key_names')"), 0);
  const cold = await listCount('INTEGRATION-first', 1);
  assert.equal(cold.items[0].keyName, 'Integration primary');
  assert.equal((await management('audit', { id: cold.items[0].id })).keyName, 'Integration primary');
  await requestModel(otherKey, [message('integration-deleted-key', 'INTEGRATION-deleted-key retained record')], 'thread-deleted-key');
  const deletedAudit = (await listCount('INTEGRATION-deleted-key', 1)).items[0];
  report('cold capture resolves current names without a directory cache');
  await requestModel(key, [first], 'thread-http', { stream: true }); await listCount('INTEGRATION-first', 2); report('HTTP JSON/SSE collection records repeated requests separately');
  const next = message('integration-input-2', 'INTEGRATION-continue 请继续。');
  const precedingAssistant = { id: 'integration-assistant-1', type: 'message', role: 'assistant', phase: 'final_answer', content: [{ type: 'output_text', text: 'Agent 方案：先检查数据库，再执行迁移。😀' }] };
  await requestModel(key, [first, precedingAssistant, next], 'thread-http', { stream: true });
  const continued = await listCount('INTEGRATION-continue', 1), continuation = await management('audit', { id: continued.items[0].id });
  assert.equal(continuation.turnId, 'same-turn'); assert.equal(continuation.keyName, 'Integration primary');
  assert.equal(continuation.repositories[0].remotes[0].url, 'https://example.invalid/repo.git');
  const assistant = continuation.context.find((item) => item.role === 'assistant'); assert.ok(assistant, 'the current request supplies its assistant context');
  assert.equal(await textContent(continuation.id, assistant.content), 'Agent 方案：先检查数据库，再执行迁移。😀');
  assert.ok(!Object.hasOwn(continuation, 'requestMetadata'), 'ordinary audit detail contains no host request metadata');
  const additional = await management('requestMetadata', { id: continuation.id });
  assert.equal(additional.auditId, continuation.id);
  assert.deepEqual(additional.requestMetadata, { model: 'gpt-5.4', userAgent: 'codex-audit-integration/1.0', reasoningEffort: 'low' });
  assert.equal((await management('requestMetadata', { id: 'missing-audit' }, 404)).error.code, 'not_found');
  assert.equal((await management('requestMetadata', { id: continuation.id, extra: 'invalid' }, 400)).error.code, 'invalid_query');
  report('ordinary detail reads input and context; explicit metadata uses authenticated host dispatch without SQL grants');
  await requestModel(key, [message('integration-delta', 'INTEGRATION-delta no reconstructed history')], 'thread-http', { extra: { previous_response_id: response1.id } });
  const delta = await listCount('INTEGRATION-delta', 1);
  assert.equal((await management('audit', { id: delta.items[0].id })).context.length, 0); report('incremental input keeps only the context present in its request');
  const duplicateText = message('integration-input-3', next.content[0].text);
  await requestModel(key, [first, next, duplicateText], 'thread-http'); await listCount('INTEGRATION-continue', 2); report('only the newest user input produces a record');
  const noId = message(undefined, 'INTEGRATION-position repeated');
  await requestModel(key, [noId], 'thread-position'); await requestModel(key, [noId], 'thread-position'); await listCount('INTEGRATION-position', 2);
  await requestModel(key, [noId, noId], 'thread-position'); await listCount('INTEGRATION-position', 3); report('requests without message identities are also recorded separately');
  const multi = [message('integration-multi-1', 'INTEGRATION-multi early'), { id: 'integration-between', type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'Visible between two inputs' }] }, message('integration-multi-2', 'INTEGRATION-multi later')];
  await requestModel(key, multi, 'thread-multi'); const multiples = await listCount('INTEGRATION-multi', 1);
  const later = multiples.items[0]; assert.ok(later.preview.includes('later'));
  await listCount('INTEGRATION-multi early', 0);
  const laterDetail = await management('audit', { id: later.id }); assert.equal(laterDetail.context.length, 2);
  await requestModel(key, multi, 'thread-multi'); await listCount('INTEGRATION-multi', 2);
  assert.equal((await management('audit', { id: later.id })).context.length, 2); report('one newest input per request with fixed preceding context');
  const carriedUser = message('integration-tail-user', 'INTEGRATION-tail carried user must not be collected');
  await requestModel(key, [carriedUser, { type: 'function_call', call_id: 'integration-tool', name: 'inspect', arguments: '{}' }, { type: 'function_call_output', call_id: 'integration-tool', output: 'done' }], 'thread-tool');
  await requestModel(key, [carriedUser, precedingAssistant], 'thread-tool');
  await requestModel(key, [message('integration-filter', 'INTEGRATION-filter should not exist')], 'thread-filter', { purpose: { subagent_kind: 'thread_spawn' } });
  await requestModel(key, [message('integration-auto', 'INTEGRATION-auto should not exist', 'turn', ['user.heartbeat'])], 'thread-filter');
  await requestModel(key, [{ type: 'message', role: 'user', content: [{ type: 'input_text', text: 'INTEGRATION-unknown preserved' }] }], 'thread-unknown'); await listCount('INTEGRATION-unknown', 1);
  await listCount('INTEGRATION-tail', 0); report('tool and assistant tails skip carried historical user inputs');
  await listCount('INTEGRATION-filter', 0); await listCount('INTEGRATION-auto', 0); report('subagent/automatic filtering and conservative unknown classification');
  const recapText = 'Write a brief catch-up for a user returning to this task. Return JSON with summary and nullable next_action.\n\nConversation:\nUser: INTEGRATION-recap';
  await requestModel(key, [message('integration-recap-system', recapText)], 'thread-recap-system', { purpose: { thread_source: 'system', request_kind: 'turn' } });
  await requestModel(key, [message('integration-recap-user', recapText)], 'thread-recap-user');
  const quotedRecap = await listCount('INTEGRATION-recap', 1);
  assert.equal((await management('audit', { id: quotedRecap.items[0].id })).threadId, 'thread-recap-user');
  report('system recap request completes without an audit while identical genuine user text is preserved');
  const titleText = 'Generate a concise, single-line task title.\n\nUser prompt:\nINTEGRATION-title';
  await requestModel(key, [message('integration-title-generated', titleText)], 'thread-title-generated', { purpose: { thread_source: 'thread_title', request_kind: 'turn' } });
  await requestModel(key, [message('integration-title-user', titleText)], 'thread-title-user');
  const quotedTitle = await listCount('INTEGRATION-title', 1);
  assert.equal((await management('audit', { id: quotedTitle.items[0].id })).threadId, 'thread-title-user');
  report('hidden title generation is excluded while identical genuine user text is preserved');
  await requestModel(key, [message('integration-skill-input', 'INTEGRATION-skill-tail earlier user'), message('integration-skill-injection', 'INTEGRATION-skill-tail injected instructions', 'same-turn', ['skills.selected_skill_instructions'])], 'thread-skill-tail');
  const skillTail = await listCount('INTEGRATION-skill-tail', 1), skillDetail = await management('audit', { id: skillTail.items[0].id });
  assert.equal(await textContent(skillDetail.id, skillDetail.input), 'INTEGRATION-skill-tail injected instructions');
  assert.deepEqual(skillDetail.inputKinds, ['skills.selected_skill_instructions']); assert.equal(skillDetail.context.length, 1);
  report('skill instructions carried after user input do not suppress the audit');
  await requestModel(key, [message('integration-daemon-prompt', 'INTEGRATION-prompt-only daemon continuation', 'same-turn', ['daemon_recovery.internal_context'])], 'thread-daemon-prompt', { purpose: { turn_trigger: 'daemon_recovery' } });
  await requestModel(key, [message('integration-recap-prompt', 'INTEGRATION-prompt-only classified recap', 'same-turn', ['recap.prompt'])], 'thread-recap-prompt');
  await requestModel(key, [message('integration-goal-steering', 'INTEGRATION-goal-steering after user goal edit', 'same-turn', ['goal.internal_context'])], 'thread-goal-steering', { purpose: { turn_trigger: 'goal' } });
  await listCount('INTEGRATION-goal-steering', 1);
  const classifiedInput = message('integration-classified-input', 'INTEGRATION-classified genuine user');
  classifiedInput.content.unshift({ type: 'input_text', text: 'INTEGRATION-runtime-generated recovery context' });
  classifiedInput.internal_chat_message_metadata_passthrough.content_item_kinds.unshift('daemon_recovery.internal_context');
  await requestModel(key, [message('integration-summary', 'INTEGRATION-runtime-generated summary', 'same-turn', ['compaction.summary']), message('integration-context', 'INTEGRATION-runtime-generated context', 'same-turn', ['additional_content.selected_file']), classifiedInput], 'thread-classified', { purpose: { turn_trigger: 'goal' } });
  const classified = await listCount('INTEGRATION-classified', 1), classifiedDetail = await management('audit', { id: classified.items[0].id });
  assert.equal(await textContent(classifiedDetail.id, classifiedDetail.input), 'INTEGRATION-runtime-generated recovery contextINTEGRATION-classified genuine user');
  assert.deepEqual(classifiedDetail.inputKinds, ['daemon_recovery.internal_context', 'user.text']); assert.equal(classifiedDetail.context.length, 2);
  assert.equal(await textContent(classifiedDetail.id, classifiedDetail.context[0].content), 'INTEGRATION-runtime-generated summary');
  assert.equal(await textContent(classifiedDetail.id, classifiedDetail.context[1].content), 'INTEGRATION-runtime-generated context');
  await listCount('INTEGRATION-prompt-only', 0);
  report('automatic initiating prompts skip audits while carried content and ambiguous goal steering remain');
  await requestModel(otherKey, [first], 'thread-http', { extra: { previous_response_id: response1.id }, expectedStatus: 403 });
  // The host checks continuation ownership after request middleware. A later
  // execution rejection does not undo an input that crossed the audit boundary.
  await listCount('INTEGRATION-first', 1, otherKey);
  await requestModel(otherKey, [first], 'thread-http');
  const other = await listCount('INTEGRATION-first', 2, otherKey); assert.equal((await management('audit', { id: other.items[0].id })).context.length, 0);
  await requestModel(key, [first], 'thread-isolated', { extra: { previous_response_id: response1.id } }); await listCount('INTEGRATION-first', 5); report('request-stage capture includes later rejection and keeps request-local context');
  await Promise.all(Array.from({ length: 6 }, () => requestModel(key, [message('integration-concurrent', 'INTEGRATION-concurrent request')], 'thread-concurrent'))); await listCount('INTEGRATION-concurrent', 6); report('concurrent repeated requests each produce one audit');
  if (nextWaitMs) {
    let settled = false;
    const completion = requestModel(key, [message('integration-next-wait', nextWaitInput)], 'thread-next-wait', { timeoutMs: nextWaitMs + 30000 })
      .then((value) => ({ value }), (error) => ({ error })).finally(() => { settled = true; });
    await until(() => { assert.ok(upstream.requests.some((body) => body.input?.some((item) => item.content?.some((part) => part.text === nextWaitInput)))); }, 'long-wait request reaches the real upstream');
    const arrivedAt = Date.now();
    const waitingAudit = (await listCount('INTEGRATION-next-wait', 1)).items[0];
    assert.equal(auditValue('SELECT count(*) FROM audits WHERE id=?', waitingAudit.id), 1);
    assert.equal(settled, false, 'audit is persisted before the downstream response');
    assert.equal((await management('requestMetadata', { id: waitingAudit.id })).requestMetadata.model, 'gpt-5.4');
    assert.equal(settled, false, 'host detail metadata is available for an in-progress request');
    console.log(`Waiting ${nextWaitMs}ms for the real downstream response; audit is already persisted.`);
    // Keep five seconds between the old budget boundary and the controlled
    // response so management calls can be exercised while next is still waiting.
    await delay(Math.max(0, 120100 - (Date.now() - arrivedAt)));
    assert.equal(settled, false, 'model request remains pending beyond the former 120-second SDK budget');
    const waitingDetail = await management('audit', { id: waitingAudit.id });
    assert.equal(await textContent(waitingDetail.id, waitingDetail.input), nextWaitInput);
    assert.ok((await management('keys')).items.some((item) => item.id === key.id));
    const outcome = await completion;
    if (outcome.error) throw outcome.error;
    assert.equal(outcome.value.status, 'completed');
    assert.ok(Date.now() - arrivedAt >= 120000, 'the downstream wait crosses the former SDK budget');
    assert.equal((await management('audit', { id: waitingAudit.id })).id, waitingAudit.id);
    report(`real downstream next wait of ${nextWaitMs}ms completes with persisted audit and responsive management`);
  }
  // generate=false requires an upstream capable of WebSocket prewarming; the
  // HTTP-only test account intentionally cannot satisfy that transport policy.
  await json('/api/admin/accounts/import', { provider: 'openai', data: { provider: 'openai', authentication_kind: 'api_key', name: 'Integration WebSocket upstream', base_url: `${upstream.url}/v1`, api_key: 'sk-integration-websocket-only', transport: 'prefer_websocket' } }, 201);
  const ws = await connectWebSocket(base.replace('http:', 'ws:') + '/v1/responses', { authorization: `Bearer ${key.plaintextKey}`, 'user-agent': 'codex-audit-integration/ws' });
  const wsCall = async (input, extra = {}) => { ws.send({ type: 'response.create', model: 'gpt-5.4', input, client_metadata: metadata('thread-ws'), store: false, ...extra }); for (;;) { const event = await ws.receive(); assert.notEqual(event.type, 'error', JSON.stringify(event)); if (event.type === 'response.completed') return event.response; } };
  const warmup = await wsCall([message('integration-warmup', 'INTEGRATION-warmup input')], { generate: false }); await listCount('INTEGRATION-warmup', 1);
  const generated = await wsCall([], { previous_response_id: warmup.id });
  await wsCall([message('integration-ws-next', 'INTEGRATION-ws-next incremental')], { previous_response_id: generated.id });
  const wsItems = await listCount('INTEGRATION-ws-next', 1), wsDetail = await management('audit', { id: wsItems.items[0].id }); assert.equal(wsDetail.context.length, 0); report('real WebSocket warmup and incremental request-local collection');
  await wsCall([message('integration-ws-recap-system', recapText + '-ws')], { client_metadata: metadata('thread-ws-recap-system', { thread_source: 'system', request_kind: 'turn' }) });
  await wsCall([message('integration-ws-recap-user', recapText + '-ws')]);
  const wsQuotedRecap = await listCount('INTEGRATION-recap-ws', 1);
  assert.equal((await management('audit', { id: wsQuotedRecap.items[0].id })).threadId, 'thread-ws'); ws.close();
  report('WebSocket recap filtering uses the current response.create source and preserves genuine user input');
  const long = 'INTEGRATION-unicode ' + '😀é汉'.repeat(900);
  await requestModel(key, [message('integration-unicode', long)], 'thread-unicode');
  const longItems = await listCount('INTEGRATION-unicode', 1), longDetail = await management('audit', { id: longItems.items[0].id }); assert.equal(await textContent(longDetail.id, longDetail.input), long); report('Unicode code point counts and contiguous lazy content segments');
  await withAuditWriteLock(async () => {
    await requestModel(key, [message('integration-blocked-db', 'INTEGRATION-blocked-db best effort')], 'thread-blocked-db');
    assert.equal((await management('audit', { id: continuation.id })).id, continuation.id);
  });
  report('model request completes while audit database writes are blocked');
  report('SQLite WAL keeps existing audit records readable during a write transaction');
  for (let index = 0; index < 52; index++) await requestModel(key, [message(`integration-page-${index}`, `INTEGRATION-pagination ${index}`)], 'thread-pagination');
  const cursorFirst = await listCount('integration-PAGINATION', 52);
  assert.equal(cursorFirst.items.length, 50); assert.ok(cursorFirst.nextCursor);
  await requestModel(key, [message('integration-page-late', 'INTEGRATION-pagination late insertion')], 'thread-pagination-late');
  await listCount('integration-PAGINATION', 53);
  const cursorNext = await management('audits', { q: 'integration-PAGINATION', cursor: cursorFirst.nextCursor });
  // The reviewed contract fixes position, while total remains the current
  // complete filtered count; it deliberately promises no cross-page snapshot.
  assert.equal(cursorNext.total, 53); assert.equal(cursorNext.items.length, 2); assert.equal(cursorNext.nextCursor, null);
  assert.equal(new Set([...cursorFirst.items, ...cursorNext.items].map((item) => item.id)).size, 52);
  assert.equal((await management('audits', { q: 'different', cursor: cursorFirst.nextCursor }, 400)).error.code, 'invalid_cursor');
  report('ASCII literal body search, stable cursor position and accurate live total');
  await requestModel(key, [message('integration-date-start', 'INTEGRATION-timezone start')], 'thread-dates');
  await requestModel(key, [message('integration-date-end', 'INTEGRATION-timezone end')], 'thread-dates');
  const dated = await listCount('INTEGRATION-timezone', 2);
  // Keep DST fixtures ahead of the retention window, including across restarts.
  const dstYear = new Date().getUTCFullYear() + 1;
  const sunday = (month, earliest, offset = 0) => {
    const date = new Date(Date.UTC(dstYear, month, earliest));
    date.setUTCDate(earliest + (7 - date.getUTCDay()) % 7 + offset);
    return date.toISOString().slice(0, 10);
  };
  const longDate = sunday(10, 1), longEnd = sunday(10, 1, 1), shortDate = sunday(2, 8), shortEnd = sunday(2, 8, 1);
  for (const item of dated.items) auditExecute('UPDATE audits SET first_observed_at=? WHERE id=?', Date.parse(item.preview.endsWith(' start') ? `${longDate}T04:00:00Z` : `${longEnd}T05:00:00Z`), item.id);
  const day = await management('audits', { q: 'INTEGRATION-timezone', from: longDate, to: longDate, key: key.id });
  assert.equal(day.total, 1); assert.equal(day.items[0].recordedAtDisplay, `${longDate} 00:00:00`);
  report('saved plugin timezone inclusive dates across 25-hour DST day');
  for (const item of dated.items) auditExecute('UPDATE audits SET first_observed_at=? WHERE id=?', Date.parse(item.preview.endsWith(' start') ? `${shortDate}T05:00:00Z` : `${shortEnd}T04:00:00Z`), item.id);
  const shortDay = await management('audits', { q: 'INTEGRATION-timezone', from: shortDate, to: shortDate, key: key.id });
  assert.equal(shortDay.total, 1); assert.equal(shortDay.timeZone, 'America/New_York');
  assert.equal(shortDay.items[0].recordedAtDisplay, `${shortDate} 00:00:00`);
  report('23-hour DST day includes its start and excludes next-day start');
  const factsBefore = auditSnapshot(), secretBefore = signingSecret();
  const [concurrentResponse] = await Promise.all([
    management('audits', { q: 'INTEGRATION-continue' }),
    management('timeZone', { timeZone: 'Asia/Tokyo' }),
  ]);
  assert.ok(['America/New_York', 'Asia/Tokyo'].includes(concurrentResponse.timeZone));
  const rendered = new Intl.DateTimeFormat('sv-SE', { timeZone: concurrentResponse.timeZone, year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', second: '2-digit', hourCycle: 'h23' });
  for (const item of concurrentResponse.items) assert.equal(item.recordedAtDisplay, rendered.format(new Date(item.recordedAt)));
  assert.equal((await management('viewContext')).timeZone, 'Asia/Tokyo');
  await management('timeZone', { timeZone: 'America/New_York' });
  report('concurrent query and timezone save retain a consistent timezone for each response');
  const contentFirst = await management('content', { id: longDetail.id, contentId: longDetail.input.id });
  assert.ok(contentFirst.nextCursor);
  // Real host Keys exercise host pagination and the plugin Key cursor.
  const cursorKeys = [];
  for (let index = 0; index < 105; index += 10) {
    cursorKeys.push(...await Promise.all(Array.from({ length: Math.min(10, 105 - index) }, (_, offset) => makeKey(`Timezone cursor ${index + offset}`))));
  }
  const keyFirst = await management('keys'); assert.ok(keyFirst.nextCursor);
  await management('timeZone', { timeZone: 'Asia/Kathmandu' });
  assert.equal((await management('audits', { q: 'integration-PAGINATION', cursor: cursorFirst.nextCursor }, 400)).error.code, 'invalid_cursor');
  assert.equal((await management('content', { id: longDetail.id, contentId: longDetail.input.id, cursor: contentFirst.nextCursor }, 400)).error.code, 'invalid_cursor');
  assert.equal((await management('keys', { cursor: keyFirst.nextCursor }, 400)).error.code, 'invalid_cursor');
  assert.equal((await management('audit', { id: longDetail.id })).timeZone, 'Asia/Kathmandu');
  assert.equal(auditSnapshot(), factsBefore);
  assert.equal(signingSecret(), secretBefore);
  await management('timeZone', { timeZone: 'America/New_York' });
  await management('keys', { cursor: keyFirst.nextCursor });
  for (let index = 0; index < cursorKeys.length; index += 10) await Promise.all(cursorKeys.slice(index, index + 10).map((key) => json('/api/admin/client-keys/delete', { id: key.id })));
  report('timezone-bound list/content/Key cursors and unchanged facts, bodies and signing secret');
  const directory = await management('keys'); assert.ok(directory.items.some((item) => item.id === key.id && item.name === 'Integration primary'));
  const all = await management('audits'); assert.equal(auditValue('SELECT count(*) FROM audits'), all.total); assert.ok(upstream.requests.length > 0); report('six contracted management GET routes and real persisted records');
  const foreignRequest = (await management('audit', { id: deletedAudit.id })).requestId;
  auditExecute('UPDATE audits SET request_id=? WHERE id=?', foreignRequest, continuation.id);
  assert.equal((await management('requestMetadata', { id: continuation.id }, 503)).error.code, 'unavailable');
  assert.equal((await management('audit', { id: continuation.id })).id, continuation.id);
  assert.equal(await textContent(continuation.id, continuation.input), next.content[0].text);
  auditExecute('UPDATE audits SET request_id=? WHERE id=?', continuation.requestId, continuation.id);
  report('host Key mismatch is an independent metadata error and leaves audit content readable');
  sql(`DELETE FROM public.model_requests WHERE id = '${continuation.requestId.replaceAll("'", "''")}'`);
  assert.equal((await management('requestMetadata', { id: continuation.id })).requestMetadata, null); assert.equal(await textContent(continuation.id, continuation.input), next.content[0].text); report('host request retention yields null optional metadata and preserves audit body');
  await json('/api/admin/client-keys/update', { id: key.id, name: 'Integration renamed', groupIds: [], maxConcurrency: 0, requestsPerMinute: 0 });
  assert.equal((await management('audit', { id: continuation.id })).keyName, 'Integration renamed');
  assert.ok((await management('audits', { key: key.id, q: 'INTEGRATION-continue' })).items.every((item) => item.keyName === 'Integration renamed'));
  await json('/api/admin/client-keys/delete', { id: otherKey.id });
  assert.equal((await management('audit', { id: deletedAudit.id })).keyName, null);
  assert.equal((await management('requestMetadata', { id: deletedAudit.id })).requestMetadata.model, 'gpt-5.4');
  assert.equal((await management('audits', { key: otherKey.id })).items[0].keyName, null);
  assert.ok(!(await management('keys')).items.some((item) => item.id === otherKey.id));
  report('live renames update old rows immediately and deleted Keys retain only their audit IDs');
  await requestModel(key, [{ role: 'assistant', phase: 'commentary', content: [{ type: 'output_text', text: 'Prior context for retention' }] }, message('integration-expired', 'INTEGRATION-retention expired')], 'thread-retention');
  const expired = (await listCount('INTEGRATION-retention', 1)).items[0];
  assert.equal(auditValue('SELECT count(*) FROM contents WHERE audit_id=?', expired.id), 2);
  const packageDirectoriesBefore = readdirSync(join(temporary, 'runtime/plugins')).filter((name) => name.startsWith('plugin-'));
  assert.ok(packageDirectoriesBefore.length > 0);
  await stopHost();
  auditExecute('UPDATE audits SET first_observed_at=? WHERE id=?', Date.now() - 96 * 60 * 60 * 1000, expired.id);
  await startHost(); await refreshView();
  const packageDirectoriesAfter = readdirSync(join(temporary, 'runtime/plugins')).filter((name) => name.startsWith('plugin-'));
  assert.ok(packageDirectoriesAfter.some((name) => !packageDirectoriesBefore.includes(name)), 'restart extracts the plugin into a new temporary directory');
  await until(async () => { assert.equal((await management('audit', { id: expired.id }, 404)).error.code, 'not_found'); }, 'startup retention removes expired audit without a model request');
  assert.equal(auditValue('SELECT count(*) FROM contents WHERE audit_id=?', expired.id), 0);
  report('startup retention removes expired input and context while retaining recent audits');
  assert.equal((await management('viewContext')).timeZone, 'America/New_York');
  assert.equal(auditSnapshot(), factsBefore);
  assert.equal(signingSecret(), secretBefore);
  await requestModel(key, [first], 'thread-http'); await listCount('INTEGRATION-first', 6); report('host/plugin restart and package re-extraction preserve private SQLite history and settings');
  const currentKeys = await management('keys');
  assert.equal(currentKeys.items.find((item) => item.id === key.id).name, 'Integration renamed');
  assert.ok(!currentKeys.items.some((item) => item.id === otherKey.id));
  assert.equal((await management('audit', { id: continuation.id })).keyName, 'Integration renamed');
  assert.equal((await management('audit', { id: deletedAudit.id })).keyName, null);
  report('restart uses current host names without persisting names or a directory');
  browser = await chromium.launch({ headless: true, ...(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE } : {}) });
  const page = await browser.newPage({ viewport: { width: 1600, height: 1050 } });
  const errors = [], requested = []; page.on('pageerror', (error) => errors.push(error.message)); page.on('request', (request) => requested.push(request.url()));
  await page.context().addCookies([{ name: cookie.split('=')[0], value: cookie.slice(cookie.indexOf('=') + 1), url: base }]);
  await page.goto(`${base}/plugins/${instanceId}/${view.pages[0].id}`);
  const ui = page.frameLocator('iframe');
  await ui.locator('#result-count').waitFor({ timeout: 30000 });
  assert.deepEqual(await ui.locator('thead th').allTextContents(), ['记录时间', 'Client Key', '用户输入']);
  await ui.getByRole('combobox', { name: 'Client Key', exact: true }).click();
  await ui.getByRole('option').filter({ hasText: 'Integration renamed' }).click();
  await ui.getByRole('searchbox').fill('INTEGRATION-continue'); await ui.getByRole('button', { name: '检索', exact: true }).click();
  await until(async () => { assert.equal(await ui.locator('#result-count').textContent(), '2 条'); }, 'real iframe body search');
  assert.deepEqual(await ui.locator('tbody .key-cell').allTextContents(), ['Integration renamed', 'Integration renamed']);
  await ui.locator(`[data-audit="${continuation.id}"]`).click();
  await ui.locator('.input-card .body-text').waitFor(); assert.ok((await ui.locator('.input-card .body-text').textContent()).includes('INTEGRATION-continue'));
  await until(async () => { assert.ok((await ui.locator('.context-section').textContent()).includes('Agent 方案：先检查数据库')); }, 'real iframe reads assistant context supplied in the request');
  const metadataRequests = () => requested.filter((url) => url.includes('/api/api/audit-request-metadata'));
  assert.equal(metadataRequests().length, 0, 'opening real UI detail does not load host metadata');
  await ui.getByRole('button', { name: '加载请求信息', exact: true }).click();
  await until(async () => { assert.ok((await ui.locator('.metadata').textContent()).includes('来源请求记录不可用')); }, 'real UI displays missing optional host record');
  assert.equal(metadataRequests().length, 1, 'one explicit click makes one metadata request');
  await ui.locator(`[data-audit]:not([data-audit="${continuation.id}"])`).first().click();
  await ui.locator('.input-card .body-text').waitFor();
  assert.equal(metadataRequests().length, 1, 'navigating to another record starts with metadata unloaded');
  await ui.getByRole('button', { name: '加载请求信息', exact: true }).click();
  await until(async () => { assert.ok((await ui.locator('.metadata').textContent()).includes('gpt-5.4')); }, 'real UI explicitly loads host request metadata');
  assert.equal(metadataRequests().length, 2);
  report('real host UI loads optional metadata only on clicks and resets it when switching records');
  assert.equal(await page.locator('iframe').getAttribute('sandbox'), 'allow-scripts');
  assert.equal(await page.locator('iframe').getAttribute('src'), null, 'host owns srcdoc assembly');
  assert.ok(requested.some((url) => url.includes('/api/api/audit-content')), 'actual host page bridge called backend content endpoint');
  assert.ok(!requested.some((url) => url.includes('__demo')), 'Demo service never used'); assert.deepEqual(errors, []);
  const dates = [await ui.locator('#date-from').inputValue(), await ui.locator('#date-to').inputValue()];
  const rowTime = await ui.locator('tbody time').first().textContent();
  const beforeSaveRequests = requested.filter((url) => /\/api\/api\/(audits|audit|audit-content|keys|view-context)(?:\?|$)/.test(url)).length;
  await ui.locator('#time-zone').fill('Asia/Kathmandu');
  await ui.getByRole('button', { name: '保存', exact: true }).click();
  await until(async () => { assert.ok((await ui.locator('.timezone-saved').textContent()).includes('Asia/Kathmandu')); }, 'real iframe saves timezone through PUT bridge');
  assert.equal(requested.filter((url) => /\/api\/api\/(audits|audit|audit-content|keys|view-context)(?:\?|$)/.test(url)).length, beforeSaveRequests);
  assert.deepEqual([await ui.locator('#date-from').inputValue(), await ui.locator('#date-to').inputValue()], dates);
  assert.equal(await ui.locator('tbody time').first().textContent(), rowTime);
  assert.ok((await ui.locator('.timezone').textContent()).includes('America/New_York'));
  assert.ok((await ui.locator('.metadata time').textContent()).includes('America/New_York'));
  await page.screenshot({ path: join(temporary, 'real-host-independent-save.png'), fullPage: true });
  await ui.getByRole('button', { name: '刷新列表' }).click();
  await until(async () => { assert.ok((await ui.locator('.timezone').textContent()).includes('Asia/Kathmandu')); }, 'manual refresh uses saved timezone');
  await ui.locator(`[data-audit="${continuation.id}"]`).click();
  await until(async () => { assert.ok((await ui.locator('.metadata time').textContent()).includes('Asia/Kathmandu')); }, 'detail labels its own response timezone');
  await until(async () => { assert.ok((await ui.locator('.input-card .body-text').textContent()).includes('INTEGRATION-continue')); }, 'refreshed detail finishes reading input');
  await until(async () => { assert.ok((await ui.locator('.context-section').textContent()).includes('Agent 方案：先检查数据库')); }, 'refreshed detail finishes reading context');
  assert.deepEqual(errors, []);
  report('authenticated page saves independently, retains date filters and labels each response');
  await page.screenshot({ path: join(temporary, 'real-host-ui.png'), fullPage: true }); await browser.close(); browser = null;
  report('existing built UI through real authenticated host bridge and sandbox iframe');
  if (options['rust-tests']) {
    await stopHost();
    const tested = spawnSync('cargo', ['test', '--manifest-path', 'backend/Cargo.toml', '--locked', '--test', 'storage', '--test', 'query'], { stdio: 'inherit' });
    assert.equal(tested.status, 0, 'isolated SQLite Rust storage and audit query tests');
    report('isolated SQLite Rust storage and audit query tests');
    if (options.keep) await startHost();
  }
  writeFileSync(join(temporary, 'result.json'), JSON.stringify({ hostUrl: `http://0.0.0.0:${hostPort}`, pageUrl: `${base}/plugins/${instanceId}/${view.pages[0].id}`, reports, screenshot: join(temporary, 'real-host-ui.png'), independentSaveScreenshot: join(temporary, 'real-host-independent-save.png'), hostCommit: options['host-commit'] || null, hostBinarySha256: createHash('sha256').update(readFileSync(resolve(options['host-binary']))).digest('hex'), packageSha256: sha }, null, 2));
  console.log(`Evidence: ${temporary}`);
  if (options.keep) {
    console.log(`Remote review: http://<this-machine>:${hostPort}/plugins/${instanceId}/${view.pages[0].id}`);
    console.log(`Test admin: admin@cpr.local / ${adminPassword}`);
    console.log('Foreground host remains available; Ctrl+C stops this harness and its owned services.');
    await new Promise((done) => process.once('SIGINT', done));
  }
}
async function cleanup() {
  if (cleaned) return; cleaned = true;
  await browser?.close(); await stopHost(); await upstream?.close();
  auditDatabase?.close();
  for (const name of createdContainers.reverse()) { try { docker(['rm', '-f', name]); } catch {} }
}
try { await run(); } catch (error) { console.error(error); console.error(`Host log and evidence: ${temporary}`); process.exitCode = 1; } finally { await cleanup(); }
