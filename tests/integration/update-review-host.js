import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { endpoints, MAX_RESPONSE_BYTES } from '../../contracts/audit.js';
import { appendContent } from '../../frontend/api/content.js';
import { matchFixtureTargets, sessionFixtureRequests } from '../../scripts/seed-session-fixtures.js';

// Update the existing isolated review instance, then read its existing data.
// This never creates a database, Key, instance, or model request.
const archivePath = process.argv[2], seedPath = process.argv[3];
if (!archivePath || !seedPath || process.argv.length > 6) throw new Error('Usage: node tests/integration/update-review-host.js plugin.tar.gz seed-result.json [base-url] [instance-id]');
const seed = JSON.parse(readFileSync(resolve(seedPath), 'utf8'));
const seedPage = new URL(seed.pageUrl), seedInstance = /^\/plugins\/([^/]+)\/prompt-audit\/?$/.exec(seedPage.pathname);
assert.ok(seedInstance, 'Seed result identifies a Prompt Audit page');
const hostUrl = new URL(process.argv[4] || seedPage.origin);
assert.ok(['http:', 'https:'].includes(hostUrl.protocol) && !hostUrl.username && !hostUrl.password && hostUrl.pathname === '/' && !hostUrl.search && !hostUrl.hash, 'Host URL must be an HTTP origin');
const base = hostUrl.origin, instanceId = process.argv[5] || decodeURIComponent(seedInstance[1]);
const digest = (bytes) => createHash('sha256').update(bytes).digest('hex');
const archive = readFileSync(resolve(archivePath)), artifactSha256 = digest(archive);
const fixtureBytes = readFileSync('tests/fixtures/codex-sessions.json');
const fixture = sessionFixtureRequests(JSON.parse(fixtureBytes));
const login = await fetch(`${base}/api/auth/login`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ mode: 'admin', password: process.env.AUDIT_SEED_ADMIN_PASSWORD || 'audit-integration-password-2026' }), signal: AbortSignal.timeout(15000) });
assert.equal(login.status, 200, 'Host admin authentication');
const cookie = login.headers.get('set-cookie')?.split(';')[0];
assert.ok(cookie, 'Host admin session');
async function call(path, body, expected = 200) {
  const response = await fetch(base + path, { method: body === undefined ? 'GET' : 'POST', headers: { cookie, ...(body === undefined ? {} : { 'content-type': 'application/json' }) }, body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(120000) });
  const bytes = new Uint8Array(await response.arrayBuffer());
  if (path.includes('/api/api/')) assert.ok(bytes.length <= MAX_RESPONSE_BYTES, 'Management response byte budget');
  const value = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes));
  assert.equal(response.status, expected, `Host request ${path.split('?')[0]}`);
  if (value.code !== undefined) { assert.equal(value.code, 200); return value.data; }
  return value;
}
async function published() {
  const views = await call('/api/admin/plugins/extensions');
  const view = views.find((item) => item.target.instanceId === instanceId);
  assert.ok(view, 'Existing published management instance');
  return view;
}
let view = await published();
const managementPrefix = () => `/api/admin/plugins/extensions/${instanceId}/${view.target.artifactSha256}/${view.target.revision}`;
const management = async (name, query = {}) => endpoints[name].response.parse(await call(`${managementPrefix()}/api/${endpoints[name].path}?${new URLSearchParams(query)}`));
const before = (await call('/api/admin/plugins/instances')).find((instance) => instance.id === instanceId);
assert.ok(before?.running && before.enabled, 'Existing instance is running');
const beforeTotal = (await management('audits')).total;
const beforeKeys = await management('keys');
if (before.artifactSha256 !== artifactSha256) {
  const uploaded = await fetch(`${base}/api/admin/plugins/artifacts/upload?sha256=${artifactSha256}`, { method: 'POST', headers: { cookie, 'content-type': 'application/gzip' }, body: archive, signal: AbortSignal.timeout(120000) });
  const installed = await uploaded.json();
  assert.equal(uploaded.status, 201, 'Install updated artifact');
  assert.equal(installed.data.artifact.metadata.sha256, artifactSha256);
  const plan = await call(`/api/admin/plugins/instances/version-plan?${new URLSearchParams({ id: instanceId, artifactSha256 })}`);
  assert.equal(plan.instanceRevision, before.revision);
  assert.deepEqual(plan.configuration, before.configuration, 'Version plan preserves configuration');
  assert.deepEqual(plan.secretFields, before.secretFields, 'Version plan preserves secret names');
  assert.deepEqual(plan.bindings, before.bindings, 'Version plan preserves request binding');
  await call('/api/admin/plugins/instances/switch-version', { id: instanceId, target: { artifactSha256, expectedRevision: plan.instanceRevision } });
}
let after;
const deadline = Date.now() + 30000;
while (Date.now() < deadline) {
  after = (await call('/api/admin/plugins/instances')).find((instance) => instance.id === instanceId);
  view = await published();
  if (after.runtime.status === 'running' && after.runtime.actualArtifactSha256 === artifactSha256 && view.target.artifactSha256 === artifactSha256 && view.target.revision === after.revision) break;
  await new Promise((done) => setTimeout(done, 200));
}
assert.equal(after.runtime.status, 'running');
assert.equal(after.runtime.actualArtifactSha256, artifactSha256);
assert.equal(view.target.artifactSha256, artifactSha256);
assert.equal(view.target.revision, after.revision);
for (const field of ['id', 'name', 'enabled', 'configuration', 'secretFields', 'bindings']) assert.deepEqual(after[field], before[field], `Updated instance preserves ${field}`);
assert.equal((await management('audits')).total, beforeTotal, 'All existing audit rows survive update');
assert.deepEqual(await management('keys'), beforeKeys, 'Historical Key directory survives update');
const time = await management('viewContext');
new Intl.DateTimeFormat('en', { timeZone: time.timeZone });
const denied = await fetch(`${base}${managementPrefix()}/api/api/view-context`, { signal: AbortSignal.timeout(15000) });
assert.equal(denied.status, 401, 'Management API retains host authentication');
const resourceHashes = {};
for (const resource of view.resources) {
  const response = await fetch(`${base}${managementPrefix()}/resources/${resource.path}`, { headers: { cookie }, signal: AbortSignal.timeout(15000) });
  assert.equal(response.status, 200, `Current resource ${resource.path}`);
  const actual = new Uint8Array(await response.arrayBuffer()), expected = readFileSync(resolve('dist/plugin', resource.path));
  assert.equal(digest(actual), digest(expected), `Served ${resource.path} equals current dist`);
  resourceHashes[resource.path] = digest(actual);
}
const first = await management('audits', { key: seed.keyId }), rows = [...first.items];
assert.equal(first.total, seed.total);
let cursor = first.nextCursor;
while (cursor) { const page = await management('audits', { key: seed.keyId, cursor }); rows.push(...page.items); cursor = page.nextCursor; }
assert.equal(rows.length, seed.total);
const selectedIds = seed.verifiedAuditIds ? new Set(seed.verifiedAuditIds) : null;
const selectedRows = selectedIds ? rows.filter((row) => selectedIds.has(row.id)) : rows;
if (selectedIds) assert.equal(selectedRows.length, fixture.expectedInputCount, 'The seeded audit batch survives update');
async function content(detail, descriptor) {
  let state = { blocks: [], characterCount: 0, nextCursor: undefined };
  do { const page = await management('content', { id: detail.id, contentId: descriptor.id, ...(state.nextCursor ? { cursor: state.nextCursor } : {}) }); state = appendContent(state.blocks, page, descriptor); } while (state.nextCursor);
  return state.blocks.map(({ kind, text }) => ({ kind, text }));
}
const observations = [];
for (const row of selectedRows) {
  const detail = await management('audit', { id: row.id }), input = await content(detail, detail.input), context = [];
  for (const message of detail.context) { assert.equal(message.truncated, false); context.push({ role: message.role, phase: message.phase, blocks: await content(detail, message.content) }); }
  observations.push({ detail, input, context });
}
const matches = matchFixtureTargets(fixture.requests, observations);
console.log(JSON.stringify({ verified: true, updated: before.artifactSha256 !== artifactSha256, instanceId, previousArtifactSha256: before.artifactSha256, artifactSha256, previousRevision: before.revision, revision: after.revision, allAuditCount: beforeTotal, fixtureInputCount: first.total, verifiedTargetCount: matches.length, fixtureSha256: digest(fixtureBytes), resourceHashes, preserved: ['configuration', 'secretFields', 'bindings', 'audits', 'historicalKeys'], managementGetRoutesVerified: Object.keys(endpoints).filter((name) => !endpoints[name].method || endpoints[name].method === 'GET'), pageUrl: `${base}/plugins/${instanceId}/prompt-audit` }, null, 2));
