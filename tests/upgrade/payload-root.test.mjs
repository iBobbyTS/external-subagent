import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { verifyPayload } from '../../cli/install/payload.mjs';
import { preflightUpdate, updateInstallation } from '../../cli/install/update.mjs';
import { updateCommand } from '../../cli/commands/update.mjs';
import { registerCodexHome } from '../../cli/install/reconcile.mjs';

// Assemble a real candidate root: package manifest, native payload with
// Mach-O arm64 images and a release digest table, and a stable bin entry.
// Distinct versions produce distinct entry and payload bytes so two roots
// never share a digest.
function makeCandidate(version) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'candidate-'));
  fs.mkdirSync(path.join(root, 'npm/native/darwin-arm64'), { recursive: true });
  fs.mkdirSync(path.join(root, 'bin'), { recursive: true });
  const entryBytes = Buffer.from(`#!/usr/bin/env node\n// stable entry ${version}\n`);
  fs.writeFileSync(path.join(root, 'bin/external-subagent.mjs'), entryBytes, { mode: 0o755 });
  fs.writeFileSync(path.join(root, 'package.json'), JSON.stringify({ version }));
  const files = ['external-subagentd', 'external-subagent-mcp'].map((name) => {
    const bytes = Buffer.alloc(48);
    bytes.writeUInt32LE(0xfeedfacf); bytes.writeUInt32LE(0x0100000c, 4); bytes.write(`v${version}`, 8, 'utf8');
    fs.writeFileSync(path.join(root, 'npm/native/darwin-arm64', name), bytes, { mode: 0o755 });
    return { name, bytes: bytes.length, sha256: crypto.createHash('sha256').update(bytes).digest('hex') };
  });
  fs.writeFileSync(path.join(root, 'npm/native/darwin-arm64/payload.json'), JSON.stringify({ schema_version: 1, product: 'external-subagent', platform: 'darwin-arm64', version, files }));
  return {
    root,
    entry: path.join(fs.realpathSync(root), 'bin', 'external-subagent.mjs'),
    entryDigest: crypto.createHash('sha256').update(entryBytes).digest('hex'),
  };
}

const statePaths = (data) => ({ data, state: path.join(data, 'state.json') });
const upgradeOptions = (candidate, version) => ({
  version, candidateRoot: candidate.root, platform: 'darwin-arm64', availableVersions: ['1.0.0', '2.0.0'],
});

test('verifyPayload validates an independent candidate root', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'candidate-'));
  const dir = path.join(root, 'npm/native/darwin-arm64'); fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(root, 'package.json'), JSON.stringify({ version: '2.0.0' }));
  const files = ['external-subagentd', 'external-subagent-mcp'].map((name) => {
    const bytes = Buffer.alloc(32); bytes.writeUInt32LE(0xfeedfacf); bytes.writeUInt32LE(0x0100000c, 4);
    fs.writeFileSync(path.join(dir, name), bytes, { mode: 0o755 });
    return { name, bytes: bytes.length, sha256: crypto.createHash('sha256').update(bytes).digest('hex') };
  });
  fs.writeFileSync(path.join(dir, 'payload.json'), JSON.stringify({ schema_version: 1, product: 'external-subagent', platform: 'darwin-arm64', version: '2.0.0', files }));
  const result = verifyPayload({ root, platform: 'darwin-arm64' });
  assert.equal(result.version, '2.0.0');
  fs.rmSync(root, { recursive: true, force: true });
});

test('updateInstallation switches active entry, version, and digest between two candidate roots', () => {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'update-state-'));
  const p = statePaths(data);
  const a = makeCandidate('1.0.0'), b = makeCandidate('2.0.0');
  try {
    const first = updateInstallation(p, upgradeOptions(a, '1.0.0'));
    assert.equal(first.phase, 'active');
    assert.equal(first.active.root, a.root);
    assert.equal(first.active.version, '1.0.0');
    assert.equal(first.active.entry, a.entry);
    assert.equal(first.active.entry_sha256, a.entryDigest);
    const second = updateInstallation(p, upgradeOptions(b, '2.0.0'));
    assert.equal(second.phase, 'active');
    assert.equal(second.active.root, b.root);
    assert.equal(second.active.version, '2.0.0');
    assert.equal(second.active.entry, b.entry);
    assert.equal(second.active.entry_sha256, b.entryDigest);
    assert.notEqual(second.active.entry_sha256, first.active.entry_sha256);
    const state = JSON.parse(fs.readFileSync(p.state, 'utf8'));
    assert.equal(state.candidate, null);
    assert.equal(state.active.entry_sha256, b.entryDigest);
  } finally {
    for (const dir of [a.root, b.root, data]) fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('update rejects a non-executable candidate entry before publishing any state', () => {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'update-entry-'));
  const p = statePaths(data);
  const candidate = makeCandidate('1.0.0');
  try {
    fs.chmodSync(path.join(candidate.root, 'bin/external-subagent.mjs'), 0o644);
    assert.throws(() => updateInstallation(p, upgradeOptions(candidate, '1.0.0')), /candidate stable entry must be executable/);
    assert.equal(fs.existsSync(p.state), false, 'a rejected candidate must not publish state');
  } finally {
    fs.rmSync(candidate.root, { recursive: true, force: true });
    fs.rmSync(data, { recursive: true, force: true });
  }
});

test('bad candidate entry preserves the previously activated root', () => {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'update-prior-'));
  const p = statePaths(data);
  const a = makeCandidate('1.0.0'), bad = makeCandidate('2.0.0');
  try {
    updateInstallation(p, upgradeOptions(a, '1.0.0'));
    fs.chmodSync(path.join(bad.root, 'bin/external-subagent.mjs'), 0o644);
    assert.throws(() => updateInstallation(p, upgradeOptions(bad, '2.0.0')), /candidate stable entry must be executable/);
    const state = JSON.parse(fs.readFileSync(p.state, 'utf8'));
    assert.equal(state.phase, 'active');
    assert.equal(state.candidate, null);
    assert.equal(state.active.root, a.root);
    assert.equal(state.active.version, '1.0.0');
    assert.equal(state.active.entry_sha256, a.entryDigest);
  } finally {
    for (const dir of [a.root, bad.root, data]) fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('tampered payload digest is rejected before active changes', () => {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'update-digest-'));
  const p = statePaths(data);
  const a = makeCandidate('1.0.0'), bad = makeCandidate('2.0.0');
  try {
    updateInstallation(p, upgradeOptions(a, '1.0.0'));
    fs.appendFileSync(path.join(bad.root, 'npm/native/darwin-arm64/external-subagentd'), 'tampered');
    assert.throws(() => updateInstallation(p, upgradeOptions(bad, '2.0.0')), (error) => error.code === 'PAYLOAD_DIGEST_MISMATCH');
    const state = JSON.parse(fs.readFileSync(p.state, 'utf8'));
    assert.equal(state.phase, 'active');
    assert.equal(state.active.root, a.root);
    assert.equal(state.active.entry_sha256, a.entryDigest);
  } finally {
    for (const dir of [a.root, bad.root, data]) fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('invalid payload manifest is rejected before active changes', () => {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'update-manifest-'));
  const p = statePaths(data);
  const a = makeCandidate('1.0.0'), bad = makeCandidate('2.0.0');
  try {
    updateInstallation(p, upgradeOptions(a, '1.0.0'));
    const manifest = path.join(bad.root, 'npm/native/darwin-arm64/payload.json');
    const parsed = JSON.parse(fs.readFileSync(manifest, 'utf8'));
    fs.writeFileSync(manifest, JSON.stringify({ ...parsed, product: 'not-external-subagent' }));
    assert.throws(() => updateInstallation(p, upgradeOptions(bad, '2.0.0')), (error) => error.code === 'PAYLOAD_MANIFEST_INVALID');
    const state = JSON.parse(fs.readFileSync(p.state, 'utf8'));
    assert.equal(state.phase, 'active');
    assert.equal(state.active.root, a.root);
    assert.equal(state.active.entry_sha256, a.entryDigest);
  } finally {
    for (const dir of [a.root, bad.root, data]) fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('unavailable version is rejected before writing candidate or active state', () => {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'update-version-'));
  const p = statePaths(data);
  const a = makeCandidate('1.0.0'), b = makeCandidate('2.0.0');
  try {
    updateInstallation(p, upgradeOptions(a, '1.0.0'));
    const before = fs.readFileSync(p.state);
    assert.throws(() => updateInstallation(p, { ...upgradeOptions(b, '3.0.0') }), /PAYLOAD_VERSION_UNAVAILABLE|unavailable/);
    assert.deepEqual(fs.readFileSync(p.state), before, 'an unavailable version must not touch published state');
  } finally {
    for (const dir of [a.root, b.root, data]) fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('entry tampered during activation is rejected with failed evidence and prior active', () => {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'update-drift-'));
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'drift-home-'));
  const p = statePaths(data);
  const a = makeCandidate('1.0.0'), b = makeCandidate('2.0.0');
  try {
    updateInstallation(p, upgradeOptions(a, '1.0.0'));
    registerCodexHome(p, home);
    // A home reconcile hook swaps the entry after the candidate was verified:
    // activation must refuse to publish the mutated bytes as active.
    const tamper = () => {
      fs.writeFileSync(path.join(b.root, 'bin/external-subagent.mjs'), '#!/usr/bin/env node\n// tampered\n', { mode: 0o755 });
      return { digest: 'tampered' };
    };
    assert.throws(
      () => updateInstallation(p, { ...upgradeOptions(b, '2.0.0'), installer: tamper }),
      /PAYLOAD_ENTRY_CHANGED|changed during activation/,
    );
    const state = JSON.parse(fs.readFileSync(p.state, 'utf8'));
    assert.equal(state.phase, 'failed');
    assert.equal(state.active.root, a.root);
    assert.equal(state.active.version, '1.0.0');
    assert.equal(state.active.entry_sha256, a.entryDigest);
    assert.equal(state.candidate.root, b.root);
    assert.equal(state.error.code, 'PAYLOAD_ENTRY_CHANGED');
  } finally {
    for (const dir of [a.root, b.root, home, data]) fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('update command records retryable receipt evidence when the candidate drifts', async () => {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'drift-cmd-'));
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'drift-cmd-home-'));
  const p = statePaths(data);
  const a = makeCandidate('1.0.0'), b = makeCandidate('2.0.0');
  const rpc = async (_socket, command) => command === 'activate-ready'
    ? { ready_for_activation: true, activation_claim: 'drift-1' }
    : { ready_for_activation: true };
  try {
    updateInstallation(p, upgradeOptions(a, '1.0.0'));
    registerCodexHome(p, home);
    const tamper = () => {
      fs.writeFileSync(path.join(b.root, 'bin/external-subagent.mjs'), '#!/usr/bin/env node\n// tampered\n', { mode: 0o755 });
      return { digest: 'tampered' };
    };
    await assert.rejects(
      () => updateCommand(p, ['--version=2.0.0'], {
        callDaemon: rpc,
        // The candidate is genuine at preflight time; the tamper only lands
        // mid-activation through the installer hook below.
        preflightUpdate: (options) => preflightUpdate({ ...options, ...upgradeOptions(b, '2.0.0') }),
        updateInstallation: (paths, options) => updateInstallation(paths, { ...options, ...upgradeOptions(b, '2.0.0'), installer: tamper }),
      }),
      /changed during activation/,
    );
    const state = JSON.parse(fs.readFileSync(p.state, 'utf8'));
    assert.equal(state.phase, 'active');
    assert.equal(state.active.root, a.root);
    assert.equal(state.active.entry_sha256, a.entryDigest);
    const receipt = JSON.parse(fs.readFileSync(`${p.state}.activation.json`, 'utf8'));
    assert.equal(receipt.claim, 'drift-1');
    assert.equal(receipt.status, 'failed');
    assert.equal(receipt.retryable, true);
    assert.match(receipt.error, /changed during activation/);
    assert.equal(receipt.install_state.phase, 'failed');
    assert.equal(receipt.install_state.candidate.root, b.root);
    assert.equal(receipt.install_state.active.root, a.root);
  } finally {
    for (const dir of [a.root, b.root, home, data]) fs.rmSync(dir, { recursive: true, force: true });
  }
});
