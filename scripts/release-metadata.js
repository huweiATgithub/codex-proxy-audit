import { execFileSync } from 'node:child_process';
import { appendFileSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const stableVersion = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;

export function parseReleaseMetadata({ manifest, metadata, lockfile, tag }) {
  const version = manifest.version;
  if (typeof version !== 'string' || !stableVersion.test(version)) throw new Error('plugin.json version must be a stable X.Y.Z version.');
  if (manifest.publisher !== 'huwei' || manifest.name !== 'prompt-audit') throw new Error('Release packaging expects plugin huwei.prompt-audit.');
  if (tag !== undefined && tag !== `v${version}`) throw new Error(`Release tag must be v${version}.`);

  const packages = metadata.packages.filter((entry) => entry.name === 'codex-proxy-audit' && entry.source === null);
  if (packages.length !== 1 || packages[0].version !== version) throw new Error('backend/Cargo.toml version must match plugin.json.');
  const backend = packages[0];

  // Cargo --no-deps skips lockfile resolution, so compare the generated root entry explicitly.
  const lockedPackages = lockfile.split(/^\[\[package\]\]\s*$/m).slice(1);
  const roots = lockedPackages.filter((entry) => /^name = "codex-proxy-audit"\s*$/m.test(entry) && !/^source = /m.test(entry));
  const lockedVersion = roots.length === 1 ? roots[0].match(/^version = "([^"]+)"\s*$/m)?.[1] : undefined;
  if (lockedVersion !== version) throw new Error('backend/Cargo.lock root package version must match plugin.json.');

  const sdk = backend.dependencies.filter((dependency) => dependency.name === 'gateway-plugin-sdk');
  const hostRevision = sdk.length === 1 ? sdk[0].source?.match(/^git\+https:\/\/github\.com\/zyycn\/codex-proxy-rs\?rev=([a-f0-9]{40})$/)?.[1] : undefined;
  if (!hostRevision) throw new Error('gateway-plugin-sdk must pin zyycn/codex-proxy-rs to a full Git revision.');
  const hostRange = manifest.engines?.['codex-proxy-rs'];
  const hostVersion = typeof hostRange === 'string' && hostRange.startsWith('~') ? hostRange.slice(1) : '';
  if (!stableVersion.test(hostVersion)) throw new Error('plugin.json host compatibility must use ~X.Y.Z.');

  return {
    version,
    'host-version': hostVersion,
    'host-revision': hostRevision,
    archive: `huwei.prompt-audit-${version}-x86_64-unknown-linux-gnu.tar.gz`,
  };
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const args = process.argv.slice(2);
    if (args.length !== 0 && (args.length !== 2 || args[0] !== '--tag')) throw new Error('Usage: node scripts/release-metadata.js [--tag vX.Y.Z]');
    const metadata = JSON.parse(execFileSync('cargo', ['metadata', '--no-deps', '--locked', '--manifest-path', 'backend/Cargo.toml', '--format-version', '1'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'] }));
    const release = parseReleaseMetadata({
      manifest: JSON.parse(readFileSync('plugin.json', 'utf8')),
      metadata,
      lockfile: readFileSync('backend/Cargo.lock', 'utf8'),
      tag: args[1],
    });
    const output = Object.entries(release).map(([name, value]) => `${name}=${value}\n`).join('');
    if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, output);
    process.stdout.write(output);
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
