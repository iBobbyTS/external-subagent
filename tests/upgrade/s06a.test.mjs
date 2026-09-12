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

test('stale install lock is recovered with evidence from dead pid', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-')); const p = paths(root);
  fs.mkdirSync(p.data, { recursive: true });
  fs.writeFileSync(path.join(p.data, 'install.lock'), JSON.stringify({ pid: 2147483647, started_at_ms: 1 }));
  const result = updateInstallation(p, { version: packageVersion() });
  assert.equal(result.phase, 'active');
  assert.equal(fs.existsSync(path.join(p.data, 'install.lock')), false);
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

test('cancel-active with yes is forwarded to drain and waits for reap readiness', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-')); const p = paths(root); fs.mkdirSync(p.data, { recursive: true });
  const calls = []; let statuses = 0;
  const rpc = async (_s, command, params) => {
    calls.push([command, params]);
    if (command === 'drain') return { ready_for_activation: false, cancelling: true };
    if (command === 'drain-status') { statuses += 1; return { ready_for_activation: statuses >= 2, tasks_reaped: statuses >= 2 }; }
    return { ready_for_activation: true, activation_claim: null };
  };
  await updateCommand(p, ['--cancel-active', '--yes'], { callDaemon: rpc });
  assert.deepEqual(calls.slice(0, 3), [['drain', { cancel_active: true }], ['drain-status', {}], ['drain-status', {}]]);
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

test('service activation failure restores prior state and records rollback evidence', async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'rollback-state-')); const state = path.join(dir, 'state.json');
  fs.writeFileSync(state, JSON.stringify({ phase: 'active', active: { version: '1.0.0' } }));
  const p = { data: dir, state, socket: path.join(dir, 'sock') };
  const rpc = async (_s, command) => command === 'activate-ready' ? { activation_claim: 'rollback-1', ready_for_activation: true } : { ready_for_activation: true };
  await assert.rejects(() => updateCommand(p, ['--version=2'], { callDaemon: rpc, updateInstallation: () => ({ phase: 'active', active: { version: '2.0.0', entry: path.join(dir, 'new-entry'), entry_sha256: 'new' } }), hasInstalledService: () => true, activateService: async () => { throw new Error('bootstrap failed'); } }), /bootstrap failed/);
  assert.equal(JSON.parse(fs.readFileSync(state)).active.version, '1.0.0');
  assert.equal(JSON.parse(fs.readFileSync(`${state}.activation.json`)).rollback.restored, true);
  fs.rmSync(dir, { recursive: true, force: true });
});
