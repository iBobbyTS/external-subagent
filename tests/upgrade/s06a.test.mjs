import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { reconcileInstallation, updateInstallation } from '../../cli/install/update.mjs';
import { registerCodexHome, reconcileCodexHomes } from '../../cli/install/reconcile.mjs';
import { updateCommand } from '../../cli/commands/update.mjs';
import { packageRoot, packageVersion } from '../../cli/install/layout.mjs';
import { runInit } from '../../cli/install/init.mjs';

const darwinArm64 = process.platform === 'darwin' && process.arch === 'arm64';

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

test('malformed install lock is recovered by updateInstallation', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-')); const p = paths(root);
  fs.mkdirSync(p.data, { recursive: true });
  fs.writeFileSync(path.join(p.data, 'install.lock'), 'not-a-lock-document');
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
  // Stub-updater tests waive the coordinator preflight: the simulated
  // installer brings its own validation, and the preflight-before-drain
  // ordering oracle lives in recovery.test.mjs.
  await updateCommand(p, ['--version=1'], { callDaemon: rpc, preflightUpdate: () => ({}), updateInstallation: () => { updates += 1; return { phase: 'active' }; } });
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
  const run = () => updateCommand(p, ['--version=1'], { callDaemon: rpc, preflightUpdate: () => ({}), updateInstallation: () => { updates += 1; return { phase: 'active' }; } });
  await run(); await run(); assert.equal(updates, 1);
});

test('failed update preserves a failed activation receipt', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-')); const p = paths(root); fs.mkdirSync(p.data, { recursive: true });
  const rpc = async (_s, command) => command === 'activate-ready' ? { ready_for_activation: true, activation_claim: 'bad' } : { ready_for_activation: true };
  await assert.rejects(() => updateCommand(p, ['--version=2'], { callDaemon: rpc, preflightUpdate: () => ({}), updateInstallation: () => { throw new Error('boom'); } }), /boom/);
  const receipt = JSON.parse(fs.readFileSync(`${p.state}.activation.json`, 'utf8'));
  assert.deepEqual({ claim: receipt.claim, version: receipt.version, status: receipt.status, error: receipt.error }, { claim: 'bad', version: '2', status: 'failed', error: 'boom' });
});

test('partial update is recorded as failed receipt and never reports success', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-')); const p = paths(root); fs.mkdirSync(p.data, { recursive: true });
  const rpc = async (_s, command) => command === 'activate-ready' ? { ready_for_activation: true, activation_claim: 'partial' } : { ready_for_activation: true };
  await assert.rejects(() => updateCommand(p, ['--version=1'], { callDaemon: rpc, preflightUpdate: () => ({}), updateInstallation: () => ({ phase: 'partial' }) }), /did not activate/);
  const receipt = JSON.parse(fs.readFileSync(`${p.state}.activation.json`, 'utf8'));
  assert.equal(receipt.status, 'failed');
  assert.equal(receipt.retryable, true);
});

test('update result without an active phase is never recorded as success', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-')); const p = paths(root); fs.mkdirSync(p.data, { recursive: true });
  const rpc = async (_s, command) => command === 'activate-ready' ? { ready_for_activation: true, activation_claim: 'phaseless' } : { ready_for_activation: true };
  await assert.rejects(() => updateCommand(p, ['--version=1'], { callDaemon: rpc, preflightUpdate: () => ({}), updateInstallation: async () => ({}) }), /did not activate payload \(phase=none\)/);
  const receipt = JSON.parse(fs.readFileSync(`${p.state}.activation.json`, 'utf8'));
  assert.equal(receipt.status, 'failed');
  assert.equal(receipt.retryable, true);
});

test('failed activation receipt stays retryable for the same claim', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-')); const p = paths(root); fs.mkdirSync(p.data, { recursive: true });
  let attempts = 0;
  const rpc = async (_s, command) => command === 'activate-ready' ? { ready_for_activation: true, activation_claim: 'retry-1' } : { ready_for_activation: true };
  const updater = async () => {
    attempts += 1;
    if (attempts === 1) throw new Error('boom');
    return { phase: 'active', active: { version: '1.0.0' } };
  };
  await assert.rejects(() => updateCommand(p, ['--version=1.0.0'], { callDaemon: rpc, preflightUpdate: () => ({}), updateInstallation: updater }), /boom/);
  const failed = JSON.parse(fs.readFileSync(`${p.state}.activation.json`, 'utf8'));
  assert.equal(failed.claim, 'retry-1');
  assert.equal(failed.version, '1.0.0');
  assert.equal(failed.status, 'failed');
  assert.equal(failed.retryable, true);
  const result = await updateCommand(p, ['--version=1.0.0'], { callDaemon: rpc, preflightUpdate: () => ({}), updateInstallation: updater });
  assert.equal(result.phase, 'active');
  assert.equal(attempts, 2);
  const receipt = JSON.parse(fs.readFileSync(`${p.state}.activation.json`, 'utf8'));
  assert.equal(receipt.claim, 'retry-1');
  assert.equal(receipt.status, 'success');
});

test('update command recovers a stale install lock through the real updater', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-')); const p = paths(root); fs.mkdirSync(p.data, { recursive: true });
  fs.writeFileSync(path.join(p.data, 'install.lock'), 'not-a-lock-document');
  const rpc = async (_s, command) => command === 'activate-ready' ? { ready_for_activation: true, activation_claim: 'lock-1' } : { ready_for_activation: true };
  const result = await updateCommand(p, [], { callDaemon: rpc });
  assert.equal(result.phase, 'active');
  assert.equal(fs.existsSync(path.join(p.data, 'install.lock')), false);
  const receipt = JSON.parse(fs.readFileSync(`${p.state}.activation.json`, 'utf8'));
  assert.equal(receipt.status, 'success');
});

test('service activation failure restores prior state and records rollback evidence', async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'rollback-state-')); const state = path.join(dir, 'state.json');
  fs.writeFileSync(state, JSON.stringify({ phase: 'active', active: { version: '1.0.0' } }));
  const p = { data: dir, state, socket: path.join(dir, 'sock') };
  const rpc = async (_s, command) => command === 'activate-ready' ? { activation_claim: 'rollback-1', ready_for_activation: true } : { ready_for_activation: true };
  await assert.rejects(() => updateCommand(p, ['--version=2'], { callDaemon: rpc, preflightUpdate: () => ({}), updateInstallation: () => ({ phase: 'active', active: { version: '2.0.0', entry: path.join(dir, 'new-entry'), entry_sha256: 'new', daemon_entry: path.join(dir, 'new-daemon'), daemon_entry_sha256: 'new-daemon-sha' } }), hasInstalledService: () => true, activateService: async () => { throw new Error('bootstrap failed'); } }), /bootstrap failed/);
  assert.equal(JSON.parse(fs.readFileSync(state)).active.version, '1.0.0');
  assert.equal(JSON.parse(fs.readFileSync(`${state}.activation.json`)).rollback.restored, true);
  fs.rmSync(dir, { recursive: true, force: true });
});

test('service activation failure removes a newly created active state', async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'rollback-fresh-'));
  const p = { data: dir, state: path.join(dir, 'state.json'), socket: path.join(dir, 'sock') };
  const rpc = async (_s, command) => command === 'activate-ready' ? { activation_claim: 'fresh-rollback', ready_for_activation: true } : { ready_for_activation: true };
  await assert.rejects(() => updateCommand(p, ['--version=2'], {
    callDaemon: rpc,
    preflightUpdate: () => ({}),
    updateInstallation: () => ({ phase: 'active', active: { version: '2.0.0', entry: path.join(dir, 'entry'), entry_sha256: 'x', daemon_entry: path.join(dir, 'daemon'), daemon_entry_sha256: 'y' } }),
    hasInstalledService: () => true,
    activateService: async () => { throw new Error('bootstrap failed'); },
  }), /bootstrap failed/);
  assert.equal(fs.existsSync(p.state), false);
  fs.rmSync(dir, { recursive: true, force: true });
});

test('default update derives a verified candidate root from the installed package', { skip: !darwinArm64 }, () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-derived-'));
  const p = paths(root);
  fs.mkdirSync(p.data, { recursive: true });
  try {
    const result = updateInstallation(p, {});
    assert.equal(result.phase, 'active');
    // Expected values are recomputed from the staged package tree itself: the
    // active pointer must describe the package's own manifest and entry bytes,
    // never an ambient version string.
    const manifest = JSON.parse(fs.readFileSync(path.join(packageRoot(), 'npm', 'native', 'darwin-arm64', 'payload.json'), 'utf8'));
    const entry = path.join(fs.realpathSync(packageRoot()), 'bin', 'external-subagent.mjs');
    const entryDigest = crypto.createHash('sha256').update(fs.readFileSync(entry)).digest('hex');
    assert.equal(result.active.root, packageRoot());
    assert.ok(!path.resolve(result.active.root).startsWith(path.resolve(p.data)), 'candidate root must be independent of install state');
    assert.equal(result.active.version, manifest.version);
    assert.equal(result.active.entry, entry);
    assert.equal(result.active.entry_sha256, entryDigest);
    assert.deepEqual(result.active.payload.map((file) => file.name), manifest.files.map((file) => file.name));
    const state = JSON.parse(fs.readFileSync(p.state, 'utf8'));
    assert.equal(state.candidate, null);
    assert.equal(state.active.root, packageRoot());
    assert.equal(state.active.entry_sha256, entryDigest);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('update command runs the updater once and activates the service from the verified active payload', { skip: !darwinArm64 }, async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-service-wire-'));
  const p = paths(root);
  fs.mkdirSync(p.data, { recursive: true });
  const events = [];
  let activated = null;
  const rpc = async (_socket, command) => command === 'activate-ready' ? { ready_for_activation: true, activation_claim: 'svc-wire-1' } : { ready_for_activation: true };
  try {
    const result = await updateCommand(p, [], {
      callDaemon: rpc,
      updateInstallation: (target, options) => {
        events.push('update');
        assert.equal(options.candidateRoot, undefined, 'updateCommand must let the updater derive the candidate root');
        return updateInstallation(target, options);
      },
      hasInstalledService: () => true,
      activateService: async (_target, candidate) => { events.push('activate'); activated = candidate; return { pid: 4242, service_generation: 7 }; },
    });
    assert.equal(result.phase, 'active');
    assert.deepEqual(events, ['update', 'activate']);
    assert.equal(activated.path, result.active.daemon_entry, 'service activation receives the verified daemon artifact');
    assert.equal(activated.sha256, result.active.daemon_entry_sha256);
    assert.equal(activated.version, result.active.version);
    assert.notEqual(activated.path, result.active.entry, 'activation must not target the npm bin shim');
    const receipt = JSON.parse(fs.readFileSync(`${p.state}.activation.json`, 'utf8'));
    assert.equal(receipt.status, 'success');
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('unverifiable default candidate never overwrites the published active', { skip: !darwinArm64 }, () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-unverifiable-'));
  const p = paths(root);
  fs.mkdirSync(p.data, { recursive: true });
  try {
    updateInstallation(p, {});
    const before = fs.readFileSync(p.state);
    assert.throws(() => updateInstallation(p, { platform: 'linux-x64' }), (error) => error.code === 'PAYLOAD_MANIFEST_MISSING');
    assert.deepEqual(fs.readFileSync(p.state), before, 'a rejected default candidate must leave the prior active untouched');
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('service activation is skipped when the default candidate fails verification', { skip: !darwinArm64 }, async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-svc-fail-'));
  const p = paths(root);
  fs.mkdirSync(p.data, { recursive: true });
  const activations = [];
  const rpc = async (_socket, command) => command === 'activate-ready' ? { ready_for_activation: true, activation_claim: 'svc-fail-1' } : { ready_for_activation: true };
  try {
    updateInstallation(p, {});
    await assert.rejects(() => updateCommand(p, [], {
      callDaemon: rpc,
      updateInstallation: (target, options) => updateInstallation(target, { ...options, platform: 'linux-x64' }),
      hasInstalledService: () => true,
      activateService: async (...args) => { activations.push(args); return {}; },
    }), (error) => error.code === 'PAYLOAD_MANIFEST_MISSING');
    assert.equal(activations.length, 0);
    const state = JSON.parse(fs.readFileSync(p.state, 'utf8'));
    assert.equal(state.phase, 'active');
    assert.equal(state.active.root, packageRoot());
    const receipt = JSON.parse(fs.readFileSync(`${p.state}.activation.json`, 'utf8'));
    assert.equal(receipt.status, 'failed');
    assert.equal(receipt.retryable, true);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('an init without a verified payload stays journal-only and never publishes activation state', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-stage-only-'));
  const p = fullPaths(root);
  fs.mkdirSync(p.home, { recursive: true });
  try {
    const result = runInit({ paths: p, skipRuntimeProbe: true, skipPayloadProbe: true, skipServiceStart: true, skipCodexPlugin: true });
    assert.equal(result.installed, true);
    const state = JSON.parse(fs.readFileSync(p.state, 'utf8'));
    assert.equal(state.schema_version, 1, 'a payload-unverified init writes its resume journal, never the activation state');
    assert.equal(state.active, undefined);
    assert.equal(state.candidate, undefined);
    assert.equal(fs.existsSync(`${p.state}.activation.json`), false, 'init never claims an activation receipt');
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

function fullPaths(root) {
  return {
    data: path.join(root, 'data'), logs: path.join(root, 'logs'), home: path.join(root, 'home'),
    state: path.join(root, 'data', 'install-state.json'), config: path.join(root, 'config', 'product.json'),
    launchAgent: path.join(root, 'LaunchAgents', 'com.external-subagent.daemon.plist'),
    socket: path.join(root, 'data', 'daemon.sock'), database: path.join(root, 'data', 'daemon.db'),
    zcodeConfig: path.join(root, 'zcode', 'config.toml'), hookProvenance: path.join(root, 'zcode', 'hooks.json'),
  };
}

// B-3: the standard public sequence is `npm A -> init A -> use A -> npm B`.
// The baseline oracle: a SUCCESSFUL init that verified its payload publishes
// the active identity and retained bytes itself, through the existing update
// owner — no hidden extra "A update" may be a prerequisite for the first
// upgrade.  Stage-only belongs to the npm install, never to a verified init.
test('a verified-payload init publishes the active/retention baseline through the update owner', { skip: !darwinArm64 }, () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-init-baseline-'));
  const p = fullPaths(root);
  fs.mkdirSync(p.home, { recursive: true });
  try {
    const result = runInit({ paths: p, skipRuntimeProbe: true, skipServiceStart: true, skipCodexPlugin: true });
    assert.equal(result.installed, true);
    assert.equal(result.payload.status, 'verified');
    assert.ok(result.completed.includes('publish-active-payload'), 'init reports the baseline publication step');
    assert.equal(result.baseline.version, packageVersion());

    const state = JSON.parse(fs.readFileSync(p.state, 'utf8'));
    assert.equal(state.schema_version, 2, 'a successful verified init publishes the activation state');
    assert.equal(state.phase, 'active');
    assert.equal(state.candidate, null);
    assert.equal(state.active.version, packageVersion());
    // Expected values are recomputed from the package tree itself, never an
    // ambient version string.
    const manifest = JSON.parse(fs.readFileSync(path.join(packageRoot(), 'npm', 'native', 'darwin-arm64', 'payload.json'), 'utf8'));
    const daemonSha = manifest.files.find((file) => file.name === 'external-subagentd').sha256;
    assert.equal(state.active.daemon_entry, path.join(fs.realpathSync(packageRoot()), 'npm', 'native', 'darwin-arm64', 'external-subagentd'));
    assert.equal(state.active.daemon_entry_sha256, daemonSha, 'the published daemon digest is the verified payload digest');

    const retained = path.join(p.data, 'payload-store', packageVersion(), 'external-subagentd');
    assert.ok(fs.existsSync(retained), 'the verified daemon bytes are retained outside the package tree');
    assert.equal(crypto.createHash('sha256').update(fs.readFileSync(retained)).digest('hex'), daemonSha, 'the retained bytes match the verified digest');
    assert.equal(fs.existsSync(`${p.state}.activation.json`), false, 'init never claims an activation receipt');
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

// The baseline publication is the last init step and shares the same
// all-or-nothing rollback: a retention failure must leave no journal, no
// config, and no LaunchAgent behind.
test('a failed baseline publication rolls the whole init back', { skip: !darwinArm64 }, () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-init-baseline-fail-'));
  const p = fullPaths(root);
  fs.mkdirSync(p.home, { recursive: true });
  fs.mkdirSync(p.data, { recursive: true });
  // Block the retained-payload store so the publication fails after every
  // earlier init step has completed and been journaled.
  fs.writeFileSync(path.join(p.data, 'payload-store'), 'not a directory');
  try {
    assert.throws(
      () => runInit({ paths: p, skipRuntimeProbe: true, skipServiceStart: true, skipCodexPlugin: true }),
      (error) => error.code === 'ENOTDIR' || /ENOTDIR/.test(error.message),
    );
    assert.equal(fs.existsSync(p.state), false, 'rollback removes the state the failed init wrote');
    assert.equal(fs.existsSync(p.config), false, 'product config rolls back');
    assert.equal(fs.existsSync(p.launchAgent), false, 'the LaunchAgent rolls back');
    assert.equal(fs.existsSync(`${p.state}.activation.json`), false, 'a failed init never claims an activation receipt');
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
