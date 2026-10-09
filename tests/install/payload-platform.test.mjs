import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { elfArch, machoArch, verifyPayload } from '../../cli/install/payload.mjs';
import {
  NATIVE_PLATFORMS, nativePayloadDir, nativePlatform, payloadManifestPath,
} from '../../cli/install/layout.mjs';
import { installPlan } from '../../cli/install/init.mjs';
import { mcpFacadePath } from '../../bin/external-subagent-mcp.mjs';

// Real image headers, built byte-for-byte so both verifiers run with no host
// dependency: a complete ELF64 little-endian x86-64 header and a Mach-O 64
// arm64 header.  Each supported platform gets exactly one accepted identity
// and the other platform's bytes must never satisfy it.
function elfHeader({ machine = 0x3e, elfClass = 2, data = 1, magic = [0x7f, 0x45, 0x4c, 0x46] } = {}) {
  const bytes = Buffer.alloc(64);
  Buffer.from(magic).copy(bytes, 0);
  bytes[4] = elfClass; // EI_CLASS: 2 = ELFCLASS64
  bytes[5] = data; // EI_DATA: 1 = ELFDATA2LSB
  bytes[6] = 1; // EI_VERSION
  bytes.writeUInt16LE(2, 16); // e_type = ET_EXEC
  bytes.writeUInt16LE(machine, 18); // e_machine
  bytes.writeUInt32LE(1, 20); // e_version
  return bytes;
}

function machoHeader({ cputype = 0x0100000c } = {}) {
  const bytes = Buffer.alloc(32);
  bytes.writeUInt32LE(0xfeedfacf, 0); // MH_MAGIC_64
  bytes.writeUInt32LE(cputype, 4); // cputype
  bytes.writeUInt32LE(0x0c, 8); // cpusubtype = ARM64_ALL
  bytes.writeUInt32LE(2, 12); // filetype = MH_EXECUTE
  return bytes;
}

const IMAGES = Object.freeze({
  'darwin-arm64': {
    valid: () => machoHeader(),
    wrongFormat: () => elfHeader(),
    wrongMachine: () => machoHeader({ cputype: 0x01000007 }), // x86_64
    truncated: (bytes) => bytes.subarray(0, 6), // below cputype
    arch: 'arm64',
  },
  'linux-x64': {
    valid: () => elfHeader(),
    wrongFormat: () => machoHeader(),
    wrongMachine: () => elfHeader({ machine: 0xb7 }), // EM_AARCH64
    truncated: (bytes) => bytes.subarray(0, 12), // below e_machine
    arch: 'x64',
  },
});

const sha256 = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');

// Assemble a candidate root exactly as the packager does: a package manifest,
// a per-platform native directory with executable images, and a payload.json
// digest table.  A record's sha256 is always derived from the bytes written,
// so the negative cases below isolate the one field under test.
function makeRoot(platform, options = {}) {
  const version = options.version ?? '0.4.1';
  const packageVersion = options.packageVersion ?? version;
  const dirPlatform = options.dirPlatform ?? platform;
  const mode = options.mode ?? 0o755;
  const files = options.files ?? ['external-subagentd', 'external-subagent-mcp'].map((name) => ({
    name, bytes: IMAGES[platform].valid(),
  }));
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'payload-platform-'));
  const dir = path.join(root, 'npm', 'native', dirPlatform);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(root, 'package.json'), JSON.stringify({ version: packageVersion }));
  const records = files.map(({ name, bytes, digest }) => {
    const target = path.join(dir, name);
    fs.writeFileSync(target, bytes);
    fs.chmodSync(target, mode);
    return { name, bytes: bytes.length, sha256: digest ?? sha256(bytes) };
  });
  const manifest = {
    schema_version: 1,
    product: 'external-subagent',
    platform: options.manifestPlatform ?? platform,
    version,
    files: records,
    ...options.manifest,
  };
  fs.writeFileSync(path.join(dir, 'payload.json'), `${JSON.stringify(manifest, null, 2)}\n`);
  return { root, dir, files: records };
}

function cleanup(...roots) {
  for (const root of roots) fs.rmSync(root, { recursive: true, force: true });
}

function errorCode(fn) {
  try { fn(); } catch (error) { return error.code; }
  return null;
}

test('nativePlatform maps the supported tuples and rejects the rest', () => {
  assert.deepEqual([...NATIVE_PLATFORMS], ['darwin-arm64', 'linux-x64']);
  assert.equal(nativePlatform('darwin', 'arm64'), 'darwin-arm64');
  assert.equal(nativePlatform('linux', 'x64'), 'linux-x64');
  for (const [platform, arch] of [['darwin', 'x64'], ['linux', 'arm64'], ['win32', 'x64'], ['freebsd', 'x64']]) {
    assert.equal(nativePlatform(platform, arch), null, `${platform}-${arch} must have no payload`);
  }
});

test('nativePayloadDir and payloadManifestPath resolve per supported platform', () => {
  for (const platform of NATIVE_PLATFORMS) {
    const dir = nativePayloadDir(platform);
    assert.ok(dir.endsWith(path.join('npm', 'native', platform)), dir);
    assert.equal(payloadManifestPath(platform), path.join(dir, 'payload.json'));
  }
  assert.equal(nativePayloadDir('linux-arm64'), null);
  assert.equal(payloadManifestPath('linux-arm64'), null);
});

test('elfArch accepts only 64-bit little-endian x86-64 and machoArch only arm64', () => {
  assert.equal(elfArch(elfHeader()), 'x64');
  assert.equal(elfArch(elfHeader({ machine: 0xb7 })), null);
  assert.equal(elfArch(elfHeader({ elfClass: 1 })), null); // 32-bit
  assert.equal(elfArch(elfHeader({ data: 2 })), null); // big-endian
  assert.equal(elfArch(elfHeader({ magic: [0x7f, 0x45, 0x4c, 0x00] })), null);
  assert.equal(elfArch(elfHeader().subarray(0, 12)), null); // truncated below e_machine
  assert.equal(machoArch(machoHeader()), 'arm64');
  assert.equal(machoArch(machoHeader({ cputype: 0x01000007 })), null);
  assert.equal(machoArch(machoHeader().subarray(0, 6)), null);
});

for (const platform of Object.keys(IMAGES)) {
  test(`${platform}: a well-formed payload verifies`, () => {
    const { root } = makeRoot(platform);
    try {
      const result = verifyPayload({ root, platform });
      assert.equal(result.status, 'verified');
      assert.equal(result.platform, platform);
      assert.deepEqual(result.files.map((file) => file.arch), [IMAGES[platform].arch, IMAGES[platform].arch]);
    } finally { cleanup(root); }
  });

  test(`${platform}: the other platform's image is rejected as unsupported`, () => {
    const { root } = makeRoot(platform, { files: [{ name: 'external-subagentd', bytes: IMAGES[platform].wrongFormat() }] });
    try {
      assert.equal(errorCode(() => verifyPayload({ root, platform })), 'PAYLOAD_ARCH_UNSUPPORTED');
    } finally { cleanup(root); }
  });

  test(`${platform}: a different machine of the same format is rejected`, () => {
    const { root } = makeRoot(platform, { files: [{ name: 'external-subagentd', bytes: IMAGES[platform].wrongMachine() }] });
    try {
      assert.equal(errorCode(() => verifyPayload({ root, platform })), 'PAYLOAD_ARCH_UNSUPPORTED');
    } finally { cleanup(root); }
  });

  test(`${platform}: a truncated image header is rejected`, () => {
    const truncated = IMAGES[platform].truncated(IMAGES[platform].valid());
    const { root } = makeRoot(platform, { files: [{ name: 'external-subagentd', bytes: truncated }] });
    try {
      assert.equal(errorCode(() => verifyPayload({ root, platform })), 'PAYLOAD_ARCH_UNSUPPORTED');
    } finally { cleanup(root); }
  });

  test(`${platform}: a non-755 payload file is rejected`, () => {
    const { root } = makeRoot(platform, { mode: 0o644 });
    try {
      assert.equal(errorCode(() => verifyPayload({ root, platform })), 'PAYLOAD_PERMISSIONS_INVALID');
    } finally { cleanup(root); }
  });

  test(`${platform}: a digest drift is rejected`, () => {
    const valid = IMAGES[platform].valid();
    const { root, dir } = makeRoot(platform, { files: [{ name: 'external-subagentd', bytes: valid }] });
    try {
      const bytes = Buffer.from(valid);
      bytes[20] ^= 0xff; // same length, different sha256
      fs.writeFileSync(path.join(dir, 'external-subagentd'), bytes);
      assert.equal(errorCode(() => verifyPayload({ root, platform })), 'PAYLOAD_DIGEST_MISMATCH');
    } finally { cleanup(root); }
  });

  test(`${platform}: a manifest/package version disagreement is rejected`, () => {
    const { root } = makeRoot(platform, { version: '9.9.9', packageVersion: '0.4.1' });
    try {
      assert.equal(errorCode(() => verifyPayload({ root, platform })), 'PAYLOAD_VERSION_MISMATCH');
    } finally { cleanup(root); }
  });
}

test('a manifest declaring the wrong platform is rejected', () => {
  const { root } = makeRoot('linux-x64', { manifestPlatform: 'darwin-arm64' });
  try {
    assert.equal(errorCode(() => verifyPayload({ root, platform: 'linux-x64' })), 'PAYLOAD_PLATFORM_MISMATCH');
  } finally { cleanup(root); }
});

test('an unsupported platform option has no payload to verify', () => {
  assert.equal(errorCode(() => verifyPayload({ platform: 'linux-arm64' })), 'UNSUPPORTED_PAYLOAD_PLATFORM');
});

test('the MCP facade shim resolves the payload directory for each platform', () => {
  assert.ok(
    mcpFacadePath('linux-x64', false).endsWith(path.join('npm', 'native', 'linux-x64', 'external-subagent-mcp')),
    'linux-x64 release payload',
  );
  assert.ok(
    mcpFacadePath('darwin-arm64', false).endsWith(path.join('npm', 'native', 'darwin-arm64', 'external-subagent-mcp')),
    'darwin-arm64 release payload',
  );
  assert.ok(
    mcpFacadePath('linux-x64', true).endsWith(path.join('npm', 'native-debug', 'linux-x64', 'external-subagent-debug-mcp')),
    'linux-x64 debug payload (debug shim re-entry)',
  );
  assert.ok(
    mcpFacadePath('darwin-arm64', true).endsWith(path.join('npm', 'native-debug', 'darwin-arm64', 'external-subagent-debug-mcp')),
    'darwin-arm64 debug payload (debug shim re-entry)',
  );
  const hostPlatform = nativePlatform() ?? `${process.platform}-${process.arch}`;
  assert.ok(
    mcpFacadePath().endsWith(path.join('npm', 'native', hostPlatform, 'external-subagent-mcp')),
    `host default resolves ${hostPlatform}`,
  );
});

test('init reports the derived per-platform manifest path', () => {
  const platform = nativePlatform();
  assert.ok(platform, 'the test host must be a supported payload platform');
  const plan = installPlan({ data: '/d', logs: '/l', config: '/c', launchAgent: '/a', state: '/s' });
  assert.equal(plan[0].id, 'verify-payload');
  assert.equal(plan[0].path, payloadManifestPath(platform));
  assert.ok(plan[0].path.endsWith(path.join('npm', 'native', platform, 'payload.json')), plan[0].path);
});
