import { execFileSync } from 'node:child_process';
import { appendFileSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import semver from 'semver';

function isStableVersion(value) {
  return typeof value === 'string' && semver.valid(value) === value && semver.prerelease(value) === null && !value.includes('+');
}

function parseHostRange(value) {
  if (typeof value !== 'string' || /[\r\n]/.test(value)) return null;
  const comparators = value.split(',').map((part) => {
    // Rust accepts one comparator per comma segment, without npm OR or hyphen ranges.
    const match = /^(>=|<=|>|<|=|\^|~)? *([^\s|<>=~^]+)$/.exec(part.trim());
    if (!match || match[2].startsWith('v')) return null;
    const [, operator, version] = match;
    const wildcard = version.split('.').some((part) => ['*', 'x', 'X'].includes(part));
    // A bare Rust version means caret; a bare wildcard retains wildcard semantics.
    return `${operator ?? (wildcard ? '' : '^')}${version}`;
  });
  return comparators.includes(null) ? null : semver.validRange(comparators.join(' '));
}

export function parseReleaseMetadata({ manifest, metadata, lockfile, compatibility, tag }) {
  const version = manifest.version;
  if (!isStableVersion(version)) throw new Error('plugin.json version must be a stable X.Y.Z version.');
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
  const sdkSource = sdk.length === 1 ? sdk[0].source : undefined;
  const sdkRevision = typeof sdkSource === 'string' ? sdkSource.match(/^git\+https:\/\/github\.com\/zyycn\/codex-proxy-rs\?rev=([a-f0-9]{40})$/)?.[1] : undefined;
  if (!sdkRevision || sdkSource !== `git+https://github.com/zyycn/codex-proxy-rs?rev=${sdkRevision}`) throw new Error('gateway-plugin-sdk must pin zyycn/codex-proxy-rs to a full Git revision.');
  const hostRange = manifest.engines?.['codex-proxy-rs'];
  const parsedRange = parseHostRange(hostRange);
  if (parsedRange === null) throw new Error('plugin.json host compatibility must be a valid Rust semver range.');
  const minimumHost = semver.minVersion(parsedRange)?.version;
  if (!isStableVersion(minimumHost) || minimumHost === '0.0.0' || !semver.satisfies(minimumHost, parsedRange)) throw new Error('plugin.json host compatibility must have a minimum stable Host version.');
  if (!Array.isArray(compatibility?.hosts) || compatibility.hosts.length === 0) throw new Error('host-compatibility.json must list verified Hosts.');
  const seenVersions = new Set();
  const hosts = compatibility.hosts.map((host) => {
    if (!isStableVersion(host?.version)) throw new Error('Verified Host versions must be stable X.Y.Z versions.');
    if (seenVersions.has(host.version)) throw new Error('Verified Host versions must be unique.');
    seenVersions.add(host.version);
    if (typeof host.revision !== 'string' || host.revision.length !== 40 || !/^[a-f0-9]+$/.test(host.revision)) throw new Error('Verified Hosts must pin a full Git revision.');
    if (typeof host.openExternal !== 'boolean') throw new Error('Verified Hosts must declare the expected openExternal support.');
    if (!semver.satisfies(host.version, parsedRange)) throw new Error(`Verified Host ${host.version} is outside plugin.json host compatibility.`);
    return { version: host.version, revision: host.revision, openExternal: host.openExternal };
  });
  if (!seenVersions.has(minimumHost)) throw new Error(`Verified Hosts must cover the minimum Host version ${minimumHost}.`);
  if (!seenVersions.has(compatibility.primaryHost)) throw new Error('host-compatibility.json primaryHost must match a verified Host version.');

  return {
    version,
    'host-range': hostRange,
    'sdk-revision': sdkRevision,
    'primary-host': compatibility.primaryHost,
    'host-matrix': JSON.stringify({ include: hosts }),
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
      compatibility: JSON.parse(readFileSync('host-compatibility.json', 'utf8')),
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
