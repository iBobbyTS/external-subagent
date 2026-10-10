// Payload extraction-mode normalization (S06 repair).  `npm install -g` lays
// the packaged native binaries down with the tarball's recorded mode masked by
// the installer's umask, so a host with umask 0002 extracts an archived 0755
// binary as 0775.  verifyPayload must keep rejecting that (a group/world
// writable native binary is an install defect), so instead the install paths
// repair the extracted mode before verification: normalizePayloadMode and
// normalizePayloadFiles restore 0755 without touching the bytes, and the
// strict gate stays `=== 0o755`.  These tests drive the exact normalize ->
// verify sequence init and update use, with the same small real-image fixtures
// payload-platform.test.mjs established (Mach-O arm64 / ELF x86-64 headers).
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import {
  normalizePayloadFiles, normalizePayloadMode, verifyPayload,
} from '../../cli/install/payload.mjs';
import { preflightUpdate } from '../../cli/install/update.mjs';

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

function machoHeader() {
  const bytes = Buffer.alloc(32);
  bytes.writeUInt32LE(0xfeedfacf, 0); // MH_MAGIC_64
  bytes.writeUInt32LE(0x0100000c, 4); // CPU_TYPE_ARM64
  bytes.writeUInt32LE(0x0c, 8); // cpusubtype
  bytes.writeUInt32LE(2, 12); // MH_EXECUTE
  return bytes;
}

const IMAGES = Object.freeze({
  'darwin-arm64': machoHeader,
  'linux-x64': elfHeader,
});
const BINARIES = ['external-subagentd', 'external-subagent-mcp'];

const sha256 = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');

// Assemble a candidate root shaped exactly like a packed package: package.json,
// a per-platform native directory with real image bytes, and a payload.json
// digest table derived from those bytes.  `mode` is applied to every binary so
// the tests can produce the 0775 umask extraction under study.
function makeRoot(platform, { mode = 0o755, version = '0.4.1' } = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'payload-mode-'));
  const dir = path.join(root, 'npm', 'native', platform);
  fs.mkdirSync(dir, { recursive: true });
  fs.mkdirSync(path.join(root, 'bin'), { recursive: true });
  fs.writeFileSync(path.join(root, 'package.json'), JSON.stringify({ version }));
  fs.writeFileSync(path.join(root, 'bin', 'external-subagent.mjs'), Buffer.from('#!/usr/bin/env node\n'), { mode: 0o755 });
  const files = BINARIES.map((name) => {
    const bytes = IMAGES[platform]();
    const target = path.join(dir, name);
    fs.writeFileSync(target, bytes);
    fs.chmodSync(target, mode);
    return { name, bytes: bytes.length, sha256: sha256(bytes) };
  });
  fs.writeFileSync(path.join(dir, 'payload.json'), `${JSON.stringify({
    schema_version: 1, product: 'external-subagent', platform, version, files,
  }, null, 2)}\n`);
  return { root, dir };
}

function cleanup(...roots) {
  for (const root of roots) fs.rmSync(root, { recursive: true, force: true });
}

function mode(target) {
  return fs.statSync(target).mode & 0o777;
}

function errorCode(fn) {
  try { fn(); } catch (error) { return error.code; }
  return null;
}

test('normalizePayloadMode repairs a non-755 regular file and is a no-op at 755', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'payload-mode-unit-'));
  try {
    const umaskExtract = path.join(dir, 'binary');
    // Explicit chmod, not writeFileSync's mode option: the option is masked by
    // the runner umask (0002 keeps 0775, 0022 lands 0755), and the 0775
    // extraction this test studies must not depend on the host's umask.
    fs.writeFileSync(umaskExtract, 'x');
    fs.chmodSync(umaskExtract, 0o775);
    assert.equal(normalizePayloadMode(umaskExtract), true, 'a 0775 extraction is repaired');
    assert.equal(mode(umaskExtract), 0o755, 'the repaired file is exactly 0755');
    assert.equal(normalizePayloadMode(umaskExtract), false, 'a 0755 file is a no-op');
    assert.equal(mode(umaskExtract), 0o755);

    const missing = path.join(dir, 'absent');
    assert.equal(normalizePayloadMode(missing), false, 'a missing file is never created or chmodded');
    const symlink = path.join(dir, 'link');
    fs.symlinkSync(umaskExtract, symlink);
    assert.equal(normalizePayloadMode(symlink), false, 'a symlink is never followed and chmodded');
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('a 0775 linux-x64 payload is repaired before the strict verifier reads it', () => {
  const { root, dir } = makeRoot('linux-x64', { mode: 0o775 });
  try {
    assert.equal(errorCode(() => verifyPayload({ root, platform: 'linux-x64' })), 'PAYLOAD_PERMISSIONS_INVALID',
      'the strict gate still rejects an un-normalized 0775 payload (not loosened)');

    const normalized = normalizePayloadFiles({ root, platform: 'linux-x64' });
    assert.deepEqual(normalized.map((entry) => entry.name).sort(), [...BINARIES].sort(),
      'both payload binaries are reported as repaired');
    for (const entry of normalized) assert.equal(entry.from, '775', `the prior mode is reported for ${entry.name}`);
    for (const name of BINARIES) assert.equal(mode(path.join(dir, name)), 0o755, `${name} ends at 0755`);

    const result = verifyPayload({ root, platform: 'linux-x64' });
    assert.equal(result.status, 'verified', 'the strict verifier accepts the repaired payload');
  } finally {
    cleanup(root);
  }
});

test('darwin-arm64 seam: a correctly-permissioned payload normalizes as a no-op', () => {
  const { root, dir } = makeRoot('darwin-arm64', { mode: 0o755 });
  try {
    const before = BINARIES.map((name) => mode(path.join(dir, name)));
    assert.deepEqual(normalizePayloadFiles({ root, platform: 'darwin-arm64' }), [],
      'no repair is reported when the extracted mode already matches the release mode');
    assert.deepEqual(BINARIES.map((name) => mode(path.join(dir, name))), before, 'no mode is touched');
    assert.equal(verifyPayload({ root, platform: 'darwin-arm64' }).status, 'verified');
  } finally {
    cleanup(root);
  }
});

test('darwin-arm64 seam: the same repair applies to a masked 0775 extraction', () => {
  const { root, dir } = makeRoot('darwin-arm64', { mode: 0o775 });
  try {
    const normalized = normalizePayloadFiles({ root, platform: 'darwin-arm64' });
    assert.equal(normalized.length, BINARIES.length);
    for (const name of BINARIES) assert.equal(mode(path.join(dir, name)), 0o755);
    assert.equal(verifyPayload({ root, platform: 'darwin-arm64' }).status, 'verified');
  } finally {
    cleanup(root);
  }
});

test('a non-payload regular file is repaired too, but a missing manifest is left to the verifier', () => {
  const { root, dir } = makeRoot('linux-x64', { mode: 0o775 });
  try {
    fs.chmodSync(path.join(dir, 'payload.json'), 0o775);
    const normalized = normalizePayloadFiles({ root, platform: 'linux-x64' });
    assert.deepEqual(normalized.map((entry) => entry.name).sort(), [...BINARIES].sort(),
      'only the manifest-named binaries are normalized, never payload.json itself');
    assert.equal(mode(path.join(dir, 'payload.json')), 0o775, 'the manifest is untouched');

    const empty = fs.mkdtempSync(path.join(os.tmpdir(), 'payload-mode-empty-'));
    try {
      assert.deepEqual(normalizePayloadFiles({ root: empty, platform: 'linux-x64' }), [],
        'an absent manifest yields no repairs; verifyPayload owns the rejection');
      assert.equal(errorCode(() => verifyPayload({ root: empty, platform: 'linux-x64' })), 'PAYLOAD_MANIFEST_MISSING');
    } finally {
      fs.rmSync(empty, { recursive: true, force: true });
    }
  } finally {
    cleanup(root);
  }
});

test('update preflight repairs a umask-masked candidate before it is verified', () => {
  const { root } = makeRoot('darwin-arm64', { mode: 0o775, version: '2.0.0' });
  try {
    const preflight = preflightUpdate({
      candidateRoot: root, platform: 'darwin-arm64', version: '2.0.0', availableVersions: ['2.0.0'],
    });
    assert.equal(preflight.version, '2.0.0');
    assert.equal(preflight.payload.status, 'verified');
    assert.equal(mode(path.join(root, 'npm', 'native', 'darwin-arm64', 'external-subagentd')), 0o755);
  } finally {
    cleanup(root);
  }
});
