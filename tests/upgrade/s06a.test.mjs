import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { reconcileInstallation, updateInstallation } from '../../cli/install/update.mjs';
import { registerCodexHome, reconcileCodexHomes } from '../../cli/install/reconcile.mjs';
import { updateCommand } from '../../cli/commands/update.mjs';
import { packageVersion } from '../../cli/install/layout.mjs';

function paths(root) {
  return { data: path.join(root, 'data'), home: path.join(root, 'home'), state: path.join(root, 'data', 'install-state.json') };
}

test('upgrade writes versioned candidate and active pointer atomically', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-'));
  const p = paths(root);
  fs.mkdirSync(p.data, { recursive: true });
  const result = updateInstallation(p, { version: packageVersion(), dryRun: false });
  assert.equal(result.phase, 'active');
  const state = JSON.parse(fs.readFileSync(p.state, 'utf8'));
  assert.equal(state.schema_version, 2);
  assert.equal(state.active.version, packageVersion());
  assert.equal(state.candidate, null);
});

test('upgrade rejects unavailable payload versions before publishing active state', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-'));
  const p = paths(root);
  fs.mkdirSync(p.data, { recursive: true });
  assert.throws(() => updateInstallation(p, { version: '99.99.99' }), /PAYLOAD_VERSION_UNAVAILABLE|unavailable/);
  assert.equal(fs.existsSync(p.state), false);
});

test('reconcile rejects active cancellation unless --yes is explicit', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-'));
  const p = paths(root);
  fs.mkdirSync(p.data, { recursive: true });
  assert.throws(() => reconcileInstallation(p, { cancelActive: true }), /--yes/);
});

test('unregistered homes are never written and repeated update is idempotent', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-'));
  const p = paths(root); fs.mkdirSync(p.data, { recursive: true });
  const unregistered = path.join(root, 'unregistered'); fs.mkdirSync(unregistered);
  const first = updateInstallation(p, { version: packageVersion() });
  const second = updateInstallation(p, { version: packageVersion() });
  assert.deepEqual(second.active, first.active);
  assert.equal(fs.existsSync(path.join(unregistered, 'plugins')), false);
  registerCodexHome(p, path.join(root, 'registered'));
  assert.equal(fs.existsSync(unregistered), true);
});

test('registered home reconcile reports per-home partial results', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-')); const p = paths(root);
  fs.mkdirSync(p.data, { recursive: true }); const ok = path.join(root, 'ok'); const bad = path.join(root, 'bad');
  fs.mkdirSync(ok); fs.mkdirSync(bad); registerCodexHome(p, ok); registerCodexHome(p, bad);
  const result = reconcileCodexHomes(p, { installer: (_p, o) => { if (path.basename(o.codexHome) === 'bad') throw new Error('boom'); return { digest: 'd' }; } });
  assert.equal(result.all_updated, false); assert.deepEqual(result.homes.map((x) => x.status), ['updated', 'failed']);
});

test('update command requires yes for active cancellation', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-')); const p = paths(root);
  fs.mkdirSync(p.data, { recursive: true });
  await assert.rejects(() => updateCommand(p, ['--cancel-active']), /--yes/);
});

test('update activation calls daemon in order and runs updater once', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-')); const p = paths(root); fs.mkdirSync(p.data, { recursive: true });
  const calls = []; let updates = 0;
  const rpc = async (_s, command) => { calls.push(command); return command === 'activate-ready' ? { ready_for_activation: true, activation_claim: 'c1' } : { ready_for_activation: true }; };
  await updateCommand(p, ['--version=1'], { callDaemon: rpc, updateInstallation: () => { updates += 1; return { phase: 'active' }; } });
  assert.deepEqual(calls, ['drain', 'activate-ready']); assert.equal(updates, 1);
});

test('missing activation claim does not run updater', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-')); const p = paths(root); fs.mkdirSync(p.data, { recursive: true }); let updates = 0;
  const rpc = async () => ({ ready_for_activation: true, activation_claim: null });
  const result = await updateCommand(p, [], { callDaemon: rpc, updateInstallation: () => { updates += 1; } });
  assert.equal(result.update, 'not_activated'); assert.equal(updates, 0);
});

test('matching activation receipt is idempotent', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-')); const p = paths(root); fs.mkdirSync(p.data, { recursive: true }); let updates = 0;
  const rpc = async (_s, command) => command === 'activate-ready' ? { ready_for_activation: true, activation_claim: 'same' } : { ready_for_activation: true };
  const run = () => updateCommand(p, ['--version=1'], { callDaemon: rpc, updateInstallation: () => { updates += 1; return { phase: 'active' }; } });
  await run(); await run(); assert.equal(updates, 1);
});

test('failed update preserves a failed activation receipt', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-')); const p = paths(root); fs.mkdirSync(p.data, { recursive: true });
  const rpc = async (_s, command) => command === 'activate-ready' ? { ready_for_activation: true, activation_claim: 'bad' } : { ready_for_activation: true };
  await assert.rejects(() => updateCommand(p, ['--version=2'], { callDaemon: rpc, updateInstallation: () => { throw new Error('boom'); } }), /boom/);
  const receipt = JSON.parse(fs.readFileSync(`${p.state}.activation.json`, 'utf8'));
  assert.deepEqual({ claim: receipt.claim, version: receipt.version, status: receipt.status, error: receipt.error }, { claim: 'bad', version: '2', status: 'failed', error: 'boom' });
});

test('partial update is recorded as failed receipt and never reports success', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-')); const p = paths(root); fs.mkdirSync(p.data, { recursive: true });
  const rpc = async (_s, command) => command === 'activate-ready' ? { ready_for_activation: true, activation_claim: 'partial' } : { ready_for_activation: true };
  await assert.rejects(() => updateCommand(p, ['--version=1'], { callDaemon: rpc, updateInstallation: () => ({ phase: 'partial' }) }), /did not activate/);
  const receipt = JSON.parse(fs.readFileSync(`${p.state}.activation.json`, 'utf8'));
  assert.equal(receipt.status, 'failed');
});
