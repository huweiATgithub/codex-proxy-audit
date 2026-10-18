import test from 'node:test';
import assert from 'node:assert/strict';
import { parseReleaseMetadata } from '../scripts/release-metadata.js';

const revision = '447869860a0fb88f78109cdb700db0cb1eb5d8b5';
function fixture() {
  return {
    manifest: { publisher: 'huwei', name: 'prompt-audit', version: '0.1.12', engines: { 'codex-proxy-rs': '~3.28.0' } },
    metadata: { packages: [{ name: 'codex-proxy-audit', source: null, version: '0.1.12', dependencies: [{ name: 'gateway-plugin-sdk', source: `git+https://github.com/zyycn/codex-proxy-rs?rev=${revision}` }] }] },
    lockfile: 'version = 4\n\n[[package]]\nname = "codex-proxy-audit"\nversion = "0.1.12"\ndependencies = [\n "gateway-plugin-sdk",\n]\n\n[[package]]\nname = "gateway-plugin-sdk"\nversion = "0.1.0"\nsource = "git+https://github.com/zyycn/codex-proxy-rs"\n',
  };
}

test('release metadata derives the artifact and host baseline from matching version sources', () => {
  const input = fixture();
  const expected = { version: '0.1.12', 'host-version': '3.28.0', 'host-revision': revision, archive: 'huwei.prompt-audit-0.1.12-x86_64-unknown-linux-gnu.tar.gz' };
  assert.deepEqual(parseReleaseMetadata(input), expected);
  assert.deepEqual(parseReleaseMetadata({ ...input, tag: 'v0.1.12' }), expected);
  input.manifest.engines['codex-proxy-rs'] = '~3.29.1';
  input.metadata.packages[0].dependencies[0].source = `git+https://github.com/zyycn/codex-proxy-rs?rev=${'a'.repeat(40)}`;
  assert.equal(parseReleaseMetadata(input)['host-version'], '3.29.1');
  assert.equal(parseReleaseMetadata(input)['host-revision'], 'a'.repeat(40));
});

test('release metadata rejects stale Cargo manifest and lockfile versions', () => {
  const input = fixture();
  input.metadata.packages[0].version = '0.1.11';
  assert.throws(() => parseReleaseMetadata(input), /Cargo\.toml version/);
  input.metadata.packages[0].version = '0.1.12';
  input.lockfile = input.lockfile.replace('version = "0.1.12"', 'version = "0.1.11"');
  assert.throws(() => parseReleaseMetadata(input), /Cargo\.lock root package version/);
});

test('release metadata requires a unique local root package in Cargo metadata and lockfile', () => {
  for (const packages of [[], [{ ...fixture().metadata.packages[0], source: 'registry+https://github.com/rust-lang/crates.io-index' }], [...fixture().metadata.packages, ...fixture().metadata.packages]]) {
    assert.throws(() => parseReleaseMetadata({ ...fixture(), metadata: { packages } }), /Cargo\.toml version/);
  }
  const input = fixture();
  for (const lockfile of ['', input.lockfile.replace('name = "codex-proxy-audit"', 'name = "another-package"'), input.lockfile + '\n[[package]]\nname = "codex-proxy-audit"\nversion = "0.1.12"\n']) {
    assert.throws(() => parseReleaseMetadata({ ...input, lockfile }), /Cargo\.lock root package version/);
  }
});

test('release metadata rejects prereleases, malformed versions and tags for other versions', () => {
  for (const version of ['0.1.12-beta.1', '0.1.12+build.1', 'v0.1.12', '0.1', '00.1.12', '0.01.12', '0.1.012', '0.1.12\n', 12]) {
    const input = fixture(); input.manifest.version = version;
    assert.throws(() => parseReleaseMetadata(input), /stable X\.Y\.Z/);
  }
  for (const tag of ['v0.1.11', '0.1.12', 'v0.1.12-beta.1', 'v0.1.12\n', '']) {
    assert.throws(() => parseReleaseMetadata({ ...fixture(), tag }), /Release tag must be/);
  }
});

test('release metadata rejects unpinned or foreign SDK sources and broad host ranges', () => {
  for (const source of [null, 'registry+https://github.com/rust-lang/crates.io-index', 'git+https://github.com/zyycn/codex-proxy-rs?branch=main', `git+https://github.com/other/codex-proxy-rs?rev=${revision}`, 'git+https://github.com/zyycn/codex-proxy-rs?rev=44786986', `git+https://github.com/zyycn/codex-proxy-rs?rev=${revision}&branch=main`]) {
    const input = fixture(); input.metadata.packages[0].dependencies[0].source = source;
    assert.throws(() => parseReleaseMetadata(input), /full Git revision/);
  }
  for (const hostRange of ['^3.28.0', '>=3.28.0', '~3.28', '~3.28.0-beta.1', '~3.028.0', null]) {
    const input = fixture(); input.manifest.engines['codex-proxy-rs'] = hostRange;
    assert.throws(() => parseReleaseMetadata(input), /host compatibility must use/);
  }
});
