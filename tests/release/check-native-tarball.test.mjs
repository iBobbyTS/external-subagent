// Release pack gate regression (S06): drives the real
// scripts/release/check-native-tarball.mjs --staged in throwaway fixture trees
// that look exactly like the merged publish tree (package.json + both platform
// payload directories).  The gate must accept the dual-platform staging and
// reject any tree that is missing a platform or whose manifest declares the
// wrong platform, so a single-platform tarball can never reach a publish.  The
// binary facades reuse the real Mach-O arm64 / ELF x86-64 headers from
// tests/install/payload-platform.test.mjs, so the gate's own image probes run.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

const repoRoot = path.resolve(import.meta.dirname, '../..');
const checker = path.join(repoRoot, 'scripts', 'release', 'check-native-tarball.mjs');
const VERSION = '0.4.1';
const RELEASE_PLATFORMS = ['darwin-arm64', 'linux-x64'];
const BINARIES = ['external-subagentd', 'external-subagent-mcp'];

function machoHeader() {
  const bytes = Buffer.alloc(32);
  bytes.writeUInt32LE(0xfeedfacf, 0); // MH_MAGIC_64
  bytes.writeUInt32LE(0x0100000c, 4); // CPU_TYPE_ARM64
  bytes.writeUInt32LE(0x0c, 8); // cpusubtype
  bytes.writeUInt32LE(2, 12); // MH_EXECUTE
  return bytes;
}

function elfHeader() {
  const bytes = Buffer.alloc(64);
  Buffer.from([0x7f, 0x45, 0x4c, 0x46]).copy(bytes, 0);
  bytes[4] = 2; // ELFCLASS64
  bytes[5] = 1; // ELFDATA2LSB
  bytes[6] = 1; // EI_VERSION
  bytes.writeUInt16LE(2, 16); // ET_EXEC
  bytes.writeUInt16LE(0x3e, 18); // EM_X86_64
  bytes.writeUInt32LE(1, 20); // e_version
  return bytes;
}

const IMAGES = Object.freeze({ 'darwin-arm64': machoHeader, 'linux-x64': elfHeader });

// A merged publish tree: package.json plus the requested platform payload
// directories.  Each manifest carries the identity fields the gate checks and
// its binaries carry the matching image header at release mode.
function makeTree({ platforms = RELEASE_PLATFORMS, manifestPlatform = {}, version = VERSION, manifestVersion = version } = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'check-native-tarball-'));
  fs.writeFileSync(path.join(root, 'package.json'), JSON.stringify({ name: 'external-subagent', version }));
  for (const platform of platforms) {
    const dir = path.join(root, 'npm', 'native', platform);
    fs.mkdirSync(dir, { recursive: true });
    for (const name of BINARIES) fs.writeFileSync(path.join(dir, name), IMAGES[platform](), { mode: 0o755 });
    fs.writeFileSync(path.join(dir, 'payload.json'), `${JSON.stringify({
      schema_version: 1,
      product: 'external-subagent',
      platform: manifestPlatform[platform] ?? platform,
      version: manifestVersion,
      files: BINARIES.map((name) => ({ name, bytes: 0, sha256: '0'.repeat(64) })),
    }, null, 2)}\n`);
  }
  return root;
}

function runStaged(root) {
  const env = { ...process.env };
  delete env.npm_lifecycle_event;
  delete env.npm_config_dry_run;
  return spawnSync(process.execPath, [checker, '--staged'], { cwd: root, encoding: 'utf8', env });
}

function cleanup(root) {
  fs.rmSync(root, { recursive: true, force: true });
}

test('dual-platform staged payloads pass the publish gate', () => {
  const root = makeTree();
  try {
    const result = runStaged(root);
    assert.equal(result.status, 0, `expected the gate to pass, stderr: ${result.stderr}`);
    assert.match(result.stdout, /staged release payload checks passed/u);
  } finally {
    cleanup(root);
  }
});

test('a tree missing the darwin-arm64 payload is rejected', () => {
  const root = makeTree({ platforms: ['linux-x64'] });
  try {
    const result = runStaged(root);
    assert.equal(result.status, 1, 'a tarball without darwin-arm64 must be rejected');
    assert.match(result.stderr, /staged darwin-arm64 payload manifest is unavailable/u);
  } finally {
    cleanup(root);
  }
});

test('a tree missing the linux-x64 payload is rejected', () => {
  const root = makeTree({ platforms: ['darwin-arm64'] });
  try {
    const result = runStaged(root);
    assert.equal(result.status, 1, 'a tarball without linux-x64 must be rejected');
    assert.match(result.stderr, /staged linux-x64 payload manifest is unavailable/u);
  } finally {
    cleanup(root);
  }
});

test('a manifest declaring the wrong platform is rejected', () => {
  const root = makeTree({ manifestPlatform: { 'darwin-arm64': 'linux-x64' } });
  try {
    const result = runStaged(root);
    assert.equal(result.status, 1, 'a manifest whose platform disagrees with its directory must be rejected');
    assert.match(result.stderr, /staged darwin-arm64 payload manifest declares linux-x64/u);
  } finally {
    cleanup(root);
  }
});

test('a binary carrying the other platform image is rejected', () => {
  const root = makeTree();
  try {
    fs.writeFileSync(path.join(root, 'npm', 'native', 'linux-x64', 'external-subagentd'), machoHeader(), { mode: 0o755 });
    const result = runStaged(root);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /linux-x64\/external-subagentd is not a ELF x86-64 executable/u);
  } finally {
    cleanup(root);
  }
});

test('a staged payload at the wrong version is rejected', () => {
  const root = makeTree({ manifestVersion: '9.9.9' });
  try {
    const result = runStaged(root);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /does not match package version/u);
  } finally {
    cleanup(root);
  }
});
