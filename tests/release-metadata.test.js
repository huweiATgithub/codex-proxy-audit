import test from 'node:test';
import assert from 'node:assert/strict';
import { parseReleaseMetadata } from '../scripts/release-metadata.js';

const revision = 'e0a3dc20dd907071fc78f81cfba9a58f00a27521';
function fixture() {
  return {
    manifest: { publisher: 'huwei', name: 'prompt-audit', version: '0.1.18', engines: { 'codex-proxy-rs': '>=3.41.2, <4.0.0' } },
    metadata: { packages: [{ name: 'codex-proxy-audit', source: null, version: '0.1.18', dependencies: [{ name: 'gateway-plugin-sdk', source: `git+https://github.com/zyycn/codex-proxy-rs?rev=${revision}` }] }] },
    lockfile: 'version = 4\n\n[[package]]\nname = "codex-proxy-audit"\nversion = "0.1.18"\ndependencies = [\n "gateway-plugin-sdk",\n]\n\n[[package]]\nname = "gateway-plugin-sdk"\nversion = "0.1.0"\nsource = "git+https://github.com/zyycn/codex-proxy-rs"\n',
    compatibility: {
      primaryHost: '3.42.1',
      hosts: [
        { version: '3.41.2', revision: 'ba3baddbda7293d1d76ea066dd5e42298618c15a', openExternal: false },
        { version: '3.42.1', revision, openExternal: true },
      ],
    },
  };
}

test('release metadata keeps compatibility, SDK pin and verified Hosts independent', () => {
  const input = fixture();
  const expected = {
    version: '0.1.18',
    'host-range': '>=3.41.2, <4.0.0',
    'sdk-revision': revision,
    'primary-host': '3.42.1',
    'host-matrix': JSON.stringify({ include: input.compatibility.hosts }),
    archive: 'huwei.prompt-audit-0.1.18-x86_64-unknown-linux-gnu.tar.gz',
  };
  assert.deepEqual(parseReleaseMetadata(input), expected);
  assert.deepEqual(parseReleaseMetadata({ ...input, tag: 'v0.1.18' }), expected);

  // A separately pinned SDK and a different screenshot Host do not change the tested revisions.
  input.metadata.packages[0].dependencies[0].source = `git+https://github.com/zyycn/codex-proxy-rs?rev=${'a'.repeat(40)}`;
  input.compatibility.primaryHost = '3.41.2';
  const changed = parseReleaseMetadata(input);
  assert.equal(changed['sdk-revision'], 'a'.repeat(40));
  assert.equal(changed['primary-host'], '3.41.2');
  assert.equal(changed['host-matrix'], expected['host-matrix']);
  assert.equal(changed['host-range'], expected['host-range']);
});

test('Host eligibility follows Rust comparator conjunctions and stable-version bounds', () => {
  for (const range of ['^3.41.2', '3.41.2', '>=3.41.2', '>= 3.41.2', '>=3.41.2, <4.0.0', '>3.41.1, <=3.42.1']) {
    const input = fixture();
    input.manifest.engines['codex-proxy-rs'] = range;
    assert.equal(parseReleaseMetadata(input)['host-range'], range);
  }
  for (const version of ['3.41.1', '4.0.0', '3.42.1-beta.1']) {
    const input = fixture();
    input.compatibility.hosts.push({ version, revision: 'b'.repeat(40), openExternal: true });
    assert.throws(() => parseReleaseMetadata(input), /outside|stable X\.Y\.Z/);
  }
  for (const range of ['~3.41.2', '=3.41.2']) {
    const narrow = fixture();
    narrow.manifest.engines['codex-proxy-rs'] = range;
    assert.throws(() => parseReleaseMetadata(narrow), /3\.42\.1 is outside/);
  }
});

test('Host ranges reject npm-only syntax that Rust VersionReq cannot parse', () => {
  for (const hostRange of ['3.41.2 - 3.42.1', '>=3.41.2 || >=3.42.1', '>=3.41.2||>=3.42.1', '>=3.41.2 <4.0.0', '^v3.41.2']) {
    const input = fixture();
    input.manifest.engines['codex-proxy-rs'] = hostRange;
    assert.throws(() => parseReleaseMetadata(input), /valid Rust semver range/);
  }
});

test('verified Hosts cover the minimum compatible version and the selected primary Host', () => {
  const input = fixture();
  input.compatibility.hosts.shift();
  assert.throws(() => parseReleaseMetadata(input), /cover the minimum Host version 3\.41\.2/);
  const primary = fixture();
  primary.compatibility.primaryHost = '3.43.0';
  assert.throws(() => parseReleaseMetadata(primary), /primaryHost must match/);
  for (const compatibility of [undefined, {}, { hosts: [] }, { hosts: null }]) {
    assert.throws(() => parseReleaseMetadata({ ...fixture(), compatibility }), /must list verified Hosts/);
  }
});

test('release metadata rejects invalid or unbounded-below Host ranges', () => {
  for (const hostRange of ['not-a-range', '', '*', '<4.0.0', '>=3.41.2,, <4.0.0', '>=3.41.2,', '>=3.41.2-beta.1 <4.0.0', '>=3.41.2\n<4.0.0', null]) {
    const input = fixture();
    input.manifest.engines['codex-proxy-rs'] = hostRange;
    assert.throws(() => parseReleaseMetadata(input), /host compatibility must/);
  }
});

test('verified Host entries require unique stable versions, pinned revisions and capability expectations', () => {
  const duplicate = fixture();
  duplicate.compatibility.hosts.push({ ...duplicate.compatibility.hosts[0] });
  assert.throws(() => parseReleaseMetadata(duplicate), /must be unique/);
  for (const version of ['v3.41.2', '3.41.2+build.1', '3.41.2-beta.1', '3.041.2', '3.41.2\n', null]) {
    const input = fixture();
    input.compatibility.hosts[0].version = version;
    assert.throws(() => parseReleaseMetadata(input), /stable X\.Y\.Z/);
  }
  for (const revision of [null, 'ba3baddb', 'a'.repeat(40) + '\n', 'g'.repeat(40)]) {
    const input = fixture();
    input.compatibility.hosts[0].revision = revision;
    assert.throws(() => parseReleaseMetadata(input), /full Git revision/);
  }
  for (const openExternal of [undefined, 'false', 0, null]) {
    const input = fixture();
    input.compatibility.hosts[0].openExternal = openExternal;
    assert.throws(() => parseReleaseMetadata(input), /expected openExternal support/);
  }
});

test('release metadata rejects stale Cargo manifest and lockfile versions', () => {
  const input = fixture();
  input.metadata.packages[0].version = '0.1.17';
  assert.throws(() => parseReleaseMetadata(input), /Cargo\.toml version/);
  input.metadata.packages[0].version = '0.1.18';
  input.lockfile = input.lockfile.replace('version = "0.1.18"', 'version = "0.1.17"');
  assert.throws(() => parseReleaseMetadata(input), /Cargo\.lock root package version/);
});

test('release metadata requires a unique local root package in Cargo metadata and lockfile', () => {
  for (const packages of [[], [{ ...fixture().metadata.packages[0], source: 'registry+https://github.com/rust-lang/crates.io-index' }], [...fixture().metadata.packages, ...fixture().metadata.packages]]) {
    assert.throws(() => parseReleaseMetadata({ ...fixture(), metadata: { packages } }), /Cargo\.toml version/);
  }
  const input = fixture();
  for (const lockfile of ['', input.lockfile.replace('name = "codex-proxy-audit"', 'name = "another-package"'), input.lockfile + '\n[[package]]\nname = "codex-proxy-audit"\nversion = "0.1.18"\n']) {
    assert.throws(() => parseReleaseMetadata({ ...input, lockfile }), /Cargo\.lock root package version/);
  }
});

test('release metadata rejects prereleases, malformed versions and tags for other versions', () => {
  for (const version of ['0.1.18-beta.1', '0.1.18+build.1', 'v0.1.18', '0.1', '00.1.18', '0.01.18', '0.1.018', '0.1.18\n', 18]) {
    const input = fixture(); input.manifest.version = version;
    assert.throws(() => parseReleaseMetadata(input), /stable X\.Y\.Z/);
  }
  for (const tag of ['v0.1.17', '0.1.18', 'v0.1.18-beta.1', 'v0.1.18\n', '']) {
    assert.throws(() => parseReleaseMetadata({ ...fixture(), tag }), /Release tag must be/);
  }
});

test('release metadata requires one fully pinned upstream SDK source', () => {
  for (const source of [null, 'registry+https://github.com/rust-lang/crates.io-index', 'git+https://github.com/zyycn/codex-proxy-rs?branch=main', `git+https://github.com/other/codex-proxy-rs?rev=${revision}`, 'git+https://github.com/zyycn/codex-proxy-rs?rev=e0a3dc20', `git+https://github.com/zyycn/codex-proxy-rs?rev=${revision}&branch=main`, `git+https://github.com/zyycn/codex-proxy-rs?rev=${revision}\n`]) {
    const input = fixture(); input.metadata.packages[0].dependencies[0].source = source;
    assert.throws(() => parseReleaseMetadata(input), /full Git revision/);
  }
  for (const dependencies of [[], [...fixture().metadata.packages[0].dependencies, ...fixture().metadata.packages[0].dependencies]]) {
    const input = fixture(); input.metadata.packages[0].dependencies = dependencies;
    assert.throws(() => parseReleaseMetadata(input), /full Git revision/);
  }
});
