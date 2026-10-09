import { createServer } from 'node:http';
import { readFileSync } from 'node:fs';
import { parseArgs } from 'node:util';
import { resolveTheme, DEFAULT_THEME_COLOR, DEFAULT_CUSTOM_THEME_COLOR } from '@codex-proxy/ui/theme';
import { createDemoService } from './service.js';
import { audits, keys, modelRequests } from './fixtures.js';

const { values } = parseArgs({ options: {
  host: { type: 'string', default: '0.0.0.0' }, port: { type: 'string', default: '4173' },
  'time-zone': { type: 'string', default: 'Asia/Shanghai' },
  fixtures: { type: 'string' },
} });
const port = Number(values.port);
if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('Invalid port');
const now = process.env.AUDIT_DEMO_NOW ? () => new Date(process.env.AUDIT_DEMO_NOW) : () => new Date();
const fixtures = values.fixtures ? JSON.parse(readFileSync(values.fixtures, 'utf8')) : { audits, keys, modelRequests };
if (!['audits', 'keys', 'modelRequests'].every((name) => Array.isArray(fixtures[name])) || !fixtures.audits.length) {
  throw new Error('Fixture file must contain audits, keys, and modelRequests arrays with at least one audit');
}
// Move fixtures together so the newest input is available on the server's current day.
const shift = now().getTime() - Math.max(...fixtures.audits.map((audit) => Date.parse(audit.firstObservedAt)));
const handle = createDemoService({ ...fixtures, timeZone: values['time-zone'], now, audits: fixtures.audits.map((audit) => ({
  ...audit, firstObservedAt: new Date(Date.parse(audit.firstObservedAt) + shift).toISOString(),
})) });
const directory = new URL('../dist/plugin/', import.meta.url);
const registration = JSON.parse(readFileSync(new URL('management.json', directory), 'utf8'));
const types = { html: 'text/html', js: 'application/javascript', css: 'text/css', svg: 'image/svg+xml' };
const resources = Object.fromEntries(registration.resources.map(({ path }) => {
  if (!/^[\w.-]+$/.test(path)) throw new Error('Unexpected registered resource path');
  return [path, { contentType: types[path.split('.').at(-1)], body: readFileSync(new URL(path, directory)).toString('base64') }];
}));
const themes = Object.fromEntries(['light', 'dark'].map((name) => [name, {
  name, tokens: resolveTheme(name, DEFAULT_THEME_COLOR, DEFAULT_CUSTOM_THEME_COLOR).tokens,
}]));
const assets = new Map([
  ['/', ['text/html', readFileSync(new URL('./index.html', import.meta.url))]],
  ['/shell.js', ['application/javascript', readFileSync(new URL('./shell.js', import.meta.url))]],
]);
const server = createServer(async (request, response) => {
  const url = new URL(request.url, 'http://demo.invalid');
  const send = (status, type, body) => {
    response.writeHead(status, { 'Content-Type': type + '; charset=utf-8', 'Cache-Control': 'no-store' });
    response.end(body);
  };
  if (request.method !== 'GET') return send(405, 'text/plain', 'GET required');
  if (url.pathname.startsWith('/__demo/')) {
    const result = handle(url.pathname.slice('/__demo/'.length), url.searchParams);
    await new Promise((resolve) => setTimeout(resolve, 120));
    return send(result.status, 'application/json', JSON.stringify(result.value));
  }
  if (url.pathname === '/plugin.json') return send(200, 'application/json', JSON.stringify({ registration, resources, themes }));
  const asset = assets.get(url.pathname);
  if (asset) return send(200, ...asset);
  send(404, 'text/plain', 'Not found');
});
server.listen(port, values.host, () => {
  console.log(`Prompt Audit demo: http://${values.host}:${port}/ (host.timezone: ${values['time-zone']})`);
  console.log('Loading dist/plugin through the simulated management host. Ctrl+C stops the server.');
});
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => server.close(() => process.exit(0)));
