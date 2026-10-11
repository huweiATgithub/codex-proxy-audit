import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createHash, randomBytes, randomUUID } from 'node:crypto';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, createWriteStream } from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { once } from 'node:events';
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
  console.error('Usage: node scripts/integration.js --host-binary /path/codex-proxy-rs --host-web /path/frontend/dist --package /path/plugin.tar.gz [--keep] [--port 4180] [--docker-config /path]');
  process.exit(2);
}
const temporary = mkdtempSync(join(tmpdir(), 'codex-audit-integration-'));
const suffix = randomBytes(5).toString('hex');
const pgName = options['postgres-container'] || `codex-audit-pg-${suffix}`;
const redisName = options['redis-container'] || `codex-audit-redis-${suffix}`;
const pgPort = Number(options['postgres-port'] || 25432), redisPort = Number(options['redis-port'] || 26379);
const hostPort = Number(options.port || 4180), base = `http://127.0.0.1:${hostPort}`;
const password = options['database-password'] || '0123456789abcdef0123456789abcdef0123456789abcdef';
const pgUser = options['database-user'] || 'cpr_test', pgDatabase = options['database-name'] || (options['postgres-container'] ? `cpr_audit_${suffix}` : 'cpr_audit_test');
const auditRole = `audit_${suffix}`, auditPassword = randomBytes(24).toString('hex');
const adminPassword = 'audit-integration-password-2026';
const dockerArguments = options['docker-config'] ? ['--config', options['docker-config']] : [];
let host, cookie, view, browser, upstream, cleaned = false;
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
async function requestModel(key, input, thread, { stream = false, extra = {}, purpose = {}, expectedStatus = 200 } = {}) {
  const response = await fetch(base + '/v1/responses', { method: 'POST', headers: { authorization: `Bearer ${key.plaintextKey}`, 'content-type': 'application/json', 'user-agent': 'codex-audit-integration/1.0' }, body: JSON.stringify({ model: 'gpt-5.4', store: false, stream, reasoning: { effort: 'low' }, input, client_metadata: metadata(thread, purpose), ...extra }), signal: AbortSignal.timeout(20000) });
  const text = await response.text(); assert.equal(response.status, expectedStatus, text);
  if (expectedStatus !== 200) return JSON.parse(text);
  if (!stream) return JSON.parse(text);
  const events = text.split('\n').filter((line) => line.startsWith('data: ') && line !== 'data: [DONE]').map((line) => JSON.parse(line.slice(6)));
  const complete = events.find((event) => event.type === 'response.completed'); assert.ok(complete, text); return complete.response;
}
async function listCount(q, count, key) {
  return until(async () => { const page = await management('audits', { q, ...(key ? { key: key.id } : {}) }); assert.equal(page.total, count); return page; }, `audit count ${q}=${count}`);
}
async function withAuditTableLock(action) {
  const transaction = spawn('docker', [...dockerArguments, 'exec', '-i', pgName, 'psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1', '-U', pgUser, '-d', pgDatabase], { stdio: ['pipe', 'pipe', 'pipe'] });
  const closed = new Promise((done) => transaction.once('close', done));
  let stderr = ''; transaction.stderr.on('data', (bytes) => { stderr += bytes; });
  transaction.stdin.on('error', () => {});
  try {
    await new Promise((done, reject) => {
      let output = '';
      const finish = (error) => { clearTimeout(timer); transaction.stdout.off('data', receive); transaction.off('error', failed); transaction.off('exit', stopped); error ? reject(error) : done(); };
      const receive = (bytes) => { output += bytes; if (output.includes('audit-lock-ready')) finish(); };
      const failed = (error) => finish(error), stopped = () => finish(new Error(`Audit lock transaction exited: ${stderr}`));
      const timer = setTimeout(() => finish(new Error('Audit lock transaction did not become ready')), 20000);
      transaction.stdout.on('data', receive); transaction.once('error', failed); transaction.once('exit', stopped);
      transaction.stdin.write("BEGIN; SET LOCAL statement_timeout = '15s'; SET LOCAL idle_in_transaction_session_timeout = '30s'; LOCK TABLE prompt_audit.audits IN ACCESS EXCLUSIVE MODE; SELECT 'audit-lock-ready';\n");
    });
    await action();
    assert.equal(transaction.exitCode, null, 'Audit table remains locked until the model request has completed');
    assert.equal(transaction.signalCode, null);
  } finally {
    transaction.stdin.end('ROLLBACK;\n\\q\n');
    const timer = setTimeout(() => transaction.kill('SIGKILL'), 3000);
    try { await closed; } finally { clearTimeout(timer); }
  }
}
async function textContent(auditId, descriptor) {
  let cursor, text = '', offset = 0;
  do {
    const page = await management('content', { id: auditId, contentId: descriptor.id, ...(cursor ? { cursor } : {}) });
    for (const block of page.blocks) if (block.kind === 'text') { assert.equal(block.offset, offset); offset += [...block.text].length; text += block.text; }
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
  // Existing container mode still refuses an existing audit schema: this is a
  // disposable integration database, never an in-place production installer.
  assert.equal(sql("SELECT count(*) FROM pg_namespace WHERE nspname = 'prompt_audit'"), '0', 'integration database must not already contain prompt_audit');
  upstream = await startUpstream();
  mkdirSync(join(temporary, 'deploy'));
  writeFileSync(join(temporary, 'deploy/config.yaml'), `schema_version: 1\nhost:\n  timezone: America/New_York\n  listen: {host: 0.0.0.0, port: ${hostPort}}\n  runtime_data_dir: ../runtime\n  logging:\n    level: info\n    stdout: true\n    file: {enabled: false, directory: ../logs, retention_days: 7, max_file_size_mb: 20}\nstore:\n  database: {url: 'postgres://${pgUser}@127.0.0.1:${pgPort}/${pgDatabase}', password: '${password}'}\n  redis: {url: 'redis://127.0.0.1:${redisPort}/', password: '${password}'}\nadmin:\n  default_username: admin@cpr.local\n  default_password: '${adminPassword}'\n  session_ttl_minutes: 10080\n  session_absolute_ttl_minutes: 43200\napi:\n  asset_directory: '${resolve(options['host-web'])}'\n  cors_allowed_origins: []\n  request_timeout_seconds: null\n  request_id_header: x-request-id\nopenai:\n  api: {base_url: '${upstream.url}/v1'}\n  stream_max_retries: 0\n`);
  await startHost(); report('real host bootstrap, PostgreSQL migrations and admin authentication');
  const provision = readFileSync(resolve('scripts/provision.sql'), 'utf8');
  docker(['exec', '-i', pgName, 'psql', '-X', '-v', 'ON_ERROR_STOP=1', '-v', `audit_role=${auditRole}`, '-v', `audit_password=${auditPassword}`, '-U', pgUser, '-d', pgDatabase], provision);
  await json('/api/admin/accounts/import', { provider: 'openai', data: { provider: 'openai', authentication_kind: 'api_key', name: 'Integration controlled upstream', base_url: `${upstream.url}/v1`, api_key: 'sk-integration-only', transport: 'http' } }, 201);
  const makeKey = (name) => json('/api/admin/client-keys/create', { name, groupIds: [], maxConcurrency: 0, requestsPerMinute: 0 }, 201);
  const key = await makeKey('Integration primary'), otherKey = await makeKey('Integration secondary');
  const archive = readFileSync(resolve(options.package));
  const sha = createHash('sha256').update(archive).digest('hex');
  const uploaded = await fetch(base + `/api/admin/plugins/artifacts/upload?sha256=${sha}`, { method: 'POST', headers: { cookie, 'content-type': 'application/gzip' }, body: archive });
  const install = await uploaded.json(); assert.equal(uploaded.status, 201, JSON.stringify(install));
  const configured = await json('/api/admin/plugins/instances', { creationId: randomUUID(), name: 'Prompt Audit integration', artifactSha256: install.data.artifact.metadata.sha256, enabled: true, configuration: {}, secrets: { databaseUrl: `postgres://${auditRole}:${auditPassword}@127.0.0.1:${pgPort}/${pgDatabase}` }, bindings: [{ contribution: install.data.artifact.metadata.contributes.middleware.id, stage: 'request', order: -1000000, failurePolicy: 'delegate' }] }, 201);
  instanceId = configured.id; await refreshView();
  const instance = (await json('/api/admin/plugins/instances')).find((item) => item.id === instanceId);
  assert.equal(instance.compatibilityWarning, null, 'plugin host range and capability versions match the actual host');
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
  await management('keys');
  const first = message('integration-input-1', 'INTEGRATION-first 请拟定数据库迁移方案。😀');
  const response1 = await requestModel(key, [first], 'thread-http');
  await listCount('INTEGRATION-first', 1);
  await requestModel(key, [first], 'thread-http', { stream: true }); await listCount('INTEGRATION-first', 2); report('HTTP JSON/SSE collection records repeated requests separately');
  const next = message('integration-input-2', 'INTEGRATION-continue 请继续。');
  const precedingAssistant = { id: 'integration-assistant-1', type: 'message', role: 'assistant', phase: 'final_answer', content: [{ type: 'output_text', text: 'Agent 方案：先检查数据库，再执行迁移。😀' }] };
  await requestModel(key, [first, precedingAssistant, next], 'thread-http', { stream: true });
  const continued = await listCount('INTEGRATION-continue', 1), continuation = await management('audit', { id: continued.items[0].id });
  assert.equal(continuation.turnId, 'same-turn'); assert.equal(continuation.keyName, 'Integration primary');
  assert.equal(continuation.repositories[0].remotes[0].url, 'https://example.invalid/repo.git');
  const assistant = continuation.context.find((item) => item.role === 'assistant'); assert.ok(assistant, 'the current request supplies its assistant context');
  assert.equal(await textContent(continuation.id, assistant.content), 'Agent 方案：先检查数据库，再执行迁移。😀');
  assert.ok(continuation.requestMetadata, 'host request metadata joins with source request and Key');
  assert.equal(continuation.requestMetadata.model, 'gpt-5.4'); assert.equal(continuation.requestMetadata.userAgent, 'codex-audit-integration/1.0'); report('current-request dialogue context and host metadata join');
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
  await requestModel(otherKey, [first], 'thread-http', { extra: { previous_response_id: response1.id }, expectedStatus: 403 });
  // The host checks continuation ownership after request middleware. A later
  // execution rejection does not undo an input that crossed the audit boundary.
  await listCount('INTEGRATION-first', 1, otherKey);
  await requestModel(otherKey, [first], 'thread-http');
  const other = await listCount('INTEGRATION-first', 2, otherKey); assert.equal((await management('audit', { id: other.items[0].id })).context.length, 0);
  await requestModel(key, [first], 'thread-isolated', { extra: { previous_response_id: response1.id } }); await listCount('INTEGRATION-first', 5); report('request-stage capture includes later rejection and keeps request-local context');
  await Promise.all(Array.from({ length: 6 }, () => requestModel(key, [message('integration-concurrent', 'INTEGRATION-concurrent request')], 'thread-concurrent'))); await listCount('INTEGRATION-concurrent', 6); report('concurrent repeated requests each produce one audit');
  // generate=false requires an upstream capable of WebSocket prewarming; the
  // HTTP-only test account intentionally cannot satisfy that transport policy.
  await json('/api/admin/accounts/import', { provider: 'openai', data: { provider: 'openai', authentication_kind: 'api_key', name: 'Integration WebSocket upstream', base_url: `${upstream.url}/v1`, api_key: 'sk-integration-websocket-only', transport: 'prefer_websocket' } }, 201);
  const ws = await connectWebSocket(base.replace('http:', 'ws:') + '/v1/responses', { authorization: `Bearer ${key.plaintextKey}`, 'user-agent': 'codex-audit-integration/ws' });
  const wsCall = async (input, extra = {}) => { ws.send({ type: 'response.create', model: 'gpt-5.4', input, client_metadata: metadata('thread-ws'), store: false, ...extra }); for (;;) { const event = await ws.receive(); assert.notEqual(event.type, 'error', JSON.stringify(event)); if (event.type === 'response.completed') return event.response; } };
  const warmup = await wsCall([message('integration-warmup', 'INTEGRATION-warmup input')], { generate: false }); await listCount('INTEGRATION-warmup', 1);
  const generated = await wsCall([], { previous_response_id: warmup.id });
  await wsCall([message('integration-ws-next', 'INTEGRATION-ws-next incremental')], { previous_response_id: generated.id });
  const wsItems = await listCount('INTEGRATION-ws-next', 1), wsDetail = await management('audit', { id: wsItems.items[0].id }); assert.equal(wsDetail.context.length, 0); ws.close(); report('real WebSocket warmup and incremental request-local collection');
  const long = 'INTEGRATION-unicode ' + '😀é汉'.repeat(900);
  await requestModel(key, [message('integration-unicode', long)], 'thread-unicode');
  const longItems = await listCount('INTEGRATION-unicode', 1), longDetail = await management('audit', { id: longItems.items[0].id }); assert.equal(await textContent(longDetail.id, longDetail.input), long); report('Unicode code point counts and contiguous lazy content segments');
  await withAuditTableLock(() => requestModel(key, [message('integration-blocked-db', 'INTEGRATION-blocked-db best effort')], 'thread-blocked-db'));
  report('model request completes while audit database writes are blocked');
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
  for (const item of dated.items) sql(`UPDATE prompt_audit.audits SET first_observed_at = '${item.preview.endsWith(' start') ? '2026-11-01T04:00:00Z' : '2026-11-02T05:00:00Z'}'::timestamptz WHERE id = '${item.id.replaceAll("'", "''")}'`);
  const day = await management('audits', { q: 'INTEGRATION-timezone', from: '2026-11-01', to: '2026-11-01', key: key.id });
  assert.equal(day.total, 1); assert.equal(day.items[0].recordedAtDisplay, '2026-11-01 00:00:00');
  report('saved plugin timezone inclusive dates across 25-hour DST day');
  for (const item of dated.items) sql(`UPDATE prompt_audit.audits SET first_observed_at = '${item.preview.endsWith(' start') ? '2026-03-08T05:00:00Z' : '2026-03-09T04:00:00Z'}'::timestamptz WHERE id = '${item.id.replaceAll("'", "''")}'`);
  const shortDay = await management('audits', { q: 'INTEGRATION-timezone', from: '2026-03-08', to: '2026-03-08', key: key.id });
  assert.equal(shortDay.total, 1); assert.equal(shortDay.timeZone, 'America/New_York');
  assert.equal(shortDay.items[0].recordedAtDisplay, '2026-03-08 00:00:00');
  report('23-hour DST day includes its start and excludes next-day start');
  const auditSnapshot = () => sql("SELECT md5(coalesce(jsonb_agg(to_jsonb(a) ORDER BY a.id)::text,'')) FROM prompt_audit.audits a") + sql("SELECT md5(coalesce(jsonb_agg(to_jsonb(c) ORDER BY c.id)::text,'')) FROM prompt_audit.contents c");
  const factsBefore = auditSnapshot(), secretBefore = sql("SELECT encode(value,'hex') FROM prompt_audit.settings WHERE name='query_cursor'");
  let inFlight;
  await withAuditTableLock(async () => {
    inFlight = management('audits', { q: 'INTEGRATION-continue' });
    await until(() => { assert.equal(sql(`SELECT count(*) FROM pg_stat_activity WHERE usename='${auditRole}' AND wait_event_type='Lock' AND query LIKE 'SELECT count(*) FROM prompt_audit.audits%'`), '1'); }, 'query reads timezone before waiting for audit data');
    await management('timeZone', { timeZone: 'Asia/Tokyo' });
  });
  const oldResponse = await inFlight;
  assert.equal(oldResponse.timeZone, 'America/New_York');
  assert.equal((await management('viewContext')).timeZone, 'Asia/Tokyo');
  await management('timeZone', { timeZone: 'America/New_York' });
  report('save completes during a blocked query and its response retains its original timezone');
  const contentFirst = await management('content', { id: longDetail.id, contentId: longDetail.input.id });
  assert.ok(contentFirst.nextCursor);
  // A large directory exercises the Key cursor without changing business credentials.
  sql("INSERT INTO prompt_audit.key_directory(id,present,observed_at) SELECT 'timezone-cursor-' || lpad(i::text,3,'0'),false,now() FROM generate_series(1,105) i");
  const keyFirst = await management('keys'); assert.ok(keyFirst.nextCursor);
  await management('timeZone', { timeZone: 'Asia/Kathmandu' });
  assert.equal((await management('audits', { q: 'integration-PAGINATION', cursor: cursorFirst.nextCursor }, 400)).error.code, 'invalid_cursor');
  assert.equal((await management('content', { id: longDetail.id, contentId: longDetail.input.id, cursor: contentFirst.nextCursor }, 400)).error.code, 'invalid_cursor');
  assert.equal((await management('keys', { cursor: keyFirst.nextCursor }, 400)).error.code, 'invalid_cursor');
  assert.equal((await management('audit', { id: longDetail.id })).timeZone, 'Asia/Kathmandu');
  assert.equal(auditSnapshot(), factsBefore);
  assert.equal(sql("SELECT encode(value,'hex') FROM prompt_audit.settings WHERE name='query_cursor'"), secretBefore);
  await management('timeZone', { timeZone: 'America/New_York' });
  await management('keys', { cursor: keyFirst.nextCursor });
  sql("DELETE FROM prompt_audit.key_directory WHERE id LIKE 'timezone-cursor-%'");
  report('timezone-bound list/content/Key cursors and unchanged facts, bodies and signing secret');
  const directory = await management('keys'); assert.ok(directory.items.some((item) => item.id === key.id && item.names.includes('Integration primary')));
  const all = await management('audits'); assert.equal(Number(sql('SELECT count(*) FROM prompt_audit.audits')), all.total); assert.ok(upstream.requests.length > 0); report('five contracted management GET routes and real persisted records');
  sql(`DELETE FROM public.model_requests WHERE id = '${continuation.requestId.replaceAll("'", "''")}'`);
  assert.equal((await management('audit', { id: continuation.id })).requestMetadata, null); assert.equal(await textContent(continuation.id, continuation.input), next.content[0].text); report('host request retention yields null metadata and preserves audit body');
  sql(`REVOKE SELECT (id, client_api_key_ref, user_agent, requested_model_id, reasoning_effort) ON public.model_request_observations FROM "${auditRole}"`);
  assert.equal((await management('audit', { id: continuation.id }, 500)).error.code, 'internal_error');
  sql(`GRANT SELECT (id, client_api_key_ref, user_agent, requested_model_id, reasoning_effort) ON public.model_request_observations TO "${auditRole}"`);
  report('host metadata SQL failure is an error rather than false missing record');
  await json('/api/admin/client-keys/update', { id: key.id, name: 'Integration renamed', groupIds: [], maxConcurrency: 0, requestsPerMinute: 0 });
  await json('/api/admin/client-keys/delete', { id: otherKey.id });
  await stopHost(); await startHost(); await refreshView();
  assert.equal((await management('viewContext')).timeZone, 'America/New_York');
  assert.equal(auditSnapshot(), factsBefore);
  assert.equal(sql("SELECT encode(value,'hex') FROM prompt_audit.settings WHERE name='query_cursor'"), secretBefore);
  await requestModel(key, [first], 'thread-http'); await listCount('INTEGRATION-first', 6); report('host/plugin restart preserves history and records a repeated request');
  const historical = await management('keys');
  const renamed = historical.items.find((item) => item.id === key.id), deleted = historical.items.find((item) => item.id === otherKey.id);
  assert.ok(renamed.names.includes('Integration primary') && renamed.names.includes('Integration renamed'));
  assert.equal(deleted.historical, true); assert.equal((await management('audit', { id: continuation.id })).keyName, 'Integration primary');
  report('host.keys.list snapshots retain renamed and deleted historical Key directory');
  browser = await chromium.launch({ headless: true, ...(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE } : {}) });
  const page = await browser.newPage({ viewport: { width: 1600, height: 1050 } });
  const errors = [], requested = []; page.on('pageerror', (error) => errors.push(error.message)); page.on('request', (request) => requested.push(request.url()));
  await page.context().addCookies([{ name: cookie.split('=')[0], value: cookie.slice(cookie.indexOf('=') + 1), url: base }]);
  await page.goto(`${base}/plugins/${instanceId}/${view.pages[0].id}`);
  const ui = page.frameLocator('iframe');
  await ui.locator('#result-count').waitFor({ timeout: 30000 });
  assert.deepEqual(await ui.locator('thead th').allTextContents(), ['记录时间', 'Client Key', '用户输入']);
  await ui.getByRole('searchbox').fill('INTEGRATION-continue'); await ui.getByRole('button', { name: '检索', exact: true }).click();
  await until(async () => { assert.equal(await ui.locator('#result-count').textContent(), '2 条'); }, 'real iframe body search');
  await ui.locator(`[data-audit="${continuation.id}"]`).click();
  await ui.locator('.input-card .body-text').waitFor(); assert.ok((await ui.locator('.input-card .body-text').textContent()).includes('INTEGRATION-continue'));
  await until(async () => { assert.ok((await ui.locator('.context-section').textContent()).includes('Agent 方案：先检查数据库')); }, 'real iframe reads assistant context supplied in the request');
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
    const testDatabase = `rust_query_${suffix}`;
    sql(`CREATE DATABASE "${testDatabase}" TEMPLATE "${pgDatabase}"`);
    const testEnvironment = { ...process.env, AUDIT_TEST_DATABASE_URL: `postgres://${pgUser}:${password}@127.0.0.1:${pgPort}/${testDatabase}` };
    const tested = spawnSync('cargo', ['test', '--manifest-path', 'backend/Cargo.toml', '--locked', '--test', 'storage', '--test', 'query', '--', '--ignored'], { env: testEnvironment, stdio: 'inherit' });
    assert.equal(tested.status, 0, 'isolated PostgreSQL Rust storage and host-migrated query tests');
    report('isolated PostgreSQL Rust storage and host-migrated query tests');
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
  for (const name of createdContainers.reverse()) { try { docker(['rm', '-f', name]); } catch {} }
}
try { await run(); } catch (error) { console.error(error); console.error(`Host log and evidence: ${temporary}`); process.exitCode = 1; } finally { await cleanup(); }
