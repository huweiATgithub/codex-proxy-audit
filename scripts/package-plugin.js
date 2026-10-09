import { cpSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

// The host CLI owns manifest normalization, platform validation and digests.
const source = resolve('dist/package-source');
mkdirSync(source, { recursive: true });
const registration = JSON.parse(readFileSync('dist/plugin/management.json', 'utf8'));
const manifest = JSON.parse(readFileSync('plugin.json', 'utf8'));
const mime = { html: 'text/html; charset=utf-8', js: 'application/javascript', css: 'text/css', svg: 'image/svg+xml' };
for (const { path } of registration.resources) {
  const contentType = mime[path.split('.').at(-1)];
  if (!contentType) throw new Error(`Unknown resource content type: ${path}`);
  manifest.resources[path] = contentType;
  cpSync(resolve('dist/plugin', path), resolve(source, path));
}
manifest.resources.LICENSE = 'text/plain; charset=utf-8';
cpSync(resolve('LICENSE'), resolve(source, 'LICENSE'));
writeFileSync(resolve(source, 'plugin.json'), JSON.stringify(manifest, null, 2) + '\n');
const target = process.env.AUDIT_TARGET || 'x86_64-unknown-linux-gnu';
const binary = resolve(process.env.AUDIT_BINARY || 'backend/target/release/codex-proxy-audit');
const cli = process.env.CPR_PLUGIN || 'cpr-plugin';
const result = spawnSync(cli, ['package', '--manifest', resolve(source, 'plugin.json'), '--binary', binary, '--target', target, '--output-dir', resolve('dist/packages')], { stdio: 'inherit' });
if (result.error) throw result.error;
if (result.status !== 0) process.exit(result.status || 1);
