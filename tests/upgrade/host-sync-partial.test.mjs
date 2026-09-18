// AUD-003 oracle: the top-level update/reconcile verdict must reduce BOTH
// host integrations (Codex homes + ZCode binding) on every path — update,
// pure reconcile, and service-less.  Every test asserts the JOINED proof the
// finding requires: the PERSISTED activation receipt and the CALLER signal
// together.  A host binding failure after a healthy activation yields a
// retryable `partial` receipt plus a nonzero-exit CliError
// (ZCODE_SYNC_PARTIAL / CODEX_SYNC_PARTIAL) while the published active
// payload and the completed service activation are never rolled back; an
// absent binding or a fully healthy sync keeps the exact prior success
// receipt and resolves normally.
//
// Real owners drive everything host-shaped: the REAL reconcileZcodeBinding
// and reconcileCodexHomes, the REAL receipt writer, and the REAL updater
// wherever a payload candidate is involved (darwin-arm64, mirroring
// recovery.test.mjs's candidate stubs).  Only the daemon RPC and the service
// activation are seams, the established repo idiom for these paths.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { preflightUpdate, updateInstallation } from '../../cli/install/update.mjs';
import { updateCommand } from '../../cli/commands/update.mjs';
import { registerCodexHome } from '../../cli/install/reconcile.mjs';
import { installZcodePlugin } from '../../cli/install/zcode.mjs';
import { productPaths } from '../../cli/paths.mjs';
import { packageVersion } from '../../cli/install/layout.mjs';

const darwinArm64 = process.platform === 'darwin' && process.arch === 'arm64';

// Same real-candidate stub recovery.test.mjs uses: a payload manifest, a
// stable entry, and a daemon artifact the REAL verification owners accept.
function makeCandidate(version) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'candidate-'));
  fs.mkdirSync(path.join(root, 'npm/native/darwin-arm64'), { recursive: true });
  fs.mkdirSync(path.join(root, 'bin'), { recursive: true });
  fs.writeFileSync(path.join(root, 'bin/external-subagent.mjs'),
    Buffer.from(`#!/usr/bin/env node\n// stable entry ${version}\n`), { mode: 0o755 });
  fs.writeFileSync(path.join(root, 'package.json'), JSON.stringify({ version }));
  const files = ['external-subagentd', 'external-subagent-mcp'].map((name) => {
    const bytes = Buffer.alloc(48);
    bytes.writeUInt32LE(0xfeedfacf); bytes.writeUInt32LE(0x0100000c, 4); bytes.write(`v${version}`, 8, 'utf8');
    fs.writeFileSync(path.join(root, 'npm/native/darwin-arm64', name), bytes, { mode: 0o755 });
    return { name, bytes: bytes.length, sha256: crypto.createHash('sha256').update(bytes).digest('hex') };
  });
  fs.writeFileSync(path.join(root, 'npm/native/darwin-arm64/payload.json'),
    JSON.stringify({ schema_version: 1, product: 'external-subagent', platform: 'darwin-arm64', version, files }));
  return root;
}

const upgradeOptions = (candidateRoot, version) => ({
  version, candidateRoot, platform: 'darwin-arm64', availableVersions: ['1.0.0', '2.0.0', packageVersion()],
});

// Wrap the REAL preflight/updater pair with a forced candidate, the same
// seam recovery.test.mjs uses, so the coordinator and the updater can never
// disagree about the rules while the update itself runs for real.
const realUpdaterFor = (candidateRoot, version) => ({
  preflightUpdate: (options) => preflightUpdate({ ...options, ...upgradeOptions(candidateRoot, version) }),
  updateInstallation: (target, options) => updateInstallation(target, { ...options, ...upgradeOptions(candidateRoot, version) }),
});

const rpc = (claim) => async (_socket, command) => (
  command === 'activate-ready' ? { ready_for_activation: true, activation_claim: claim } : { ready_for_activation: true }
);

function stubUpdater(home, version = '0.1.0') {
  return {
    // Stub-updater tests waive the coordinator preflight: the ordering
    // oracle for the real pair lives in recovery.test.mjs.
    preflightUpdate: () => ({}),
    updateInstallation: async () => ({
      phase: 'active',
      active: {
        version,
        entry: path.join(home, 'entry'), entry_sha256: 'entry-sha',
        daemon_entry: path.join(home, 'daemon'), daemon_entry_sha256: 'daemon-sha',
      },
    }),
  };
}

const receiptOf = (paths) => JSON.parse(fs.readFileSync(`${paths.state}.activation.json`, 'utf8'));
const stateOf = (paths) => JSON.parse(fs.readFileSync(paths.state, 'utf8'));
const breakZcodeConfig = (paths) => fs.writeFileSync(paths.zcodeConfig, '{ not json');

function lockedHome(root) {
  const home = path.join(root, 'codex-locked');
  fs.mkdirSync(home, { recursive: true });
  fs.chmodSync(home, 0o500);
  return home;
}

// Scenario 1: a failed ZCode config after a HEALTHY service activation.  The
// joined proof: receipt `partial`/retryable with the zcode detail, caller
// ZCODE_SYNC_PARTIAL, and the verified activation — payload AND service —
// preserved rather than rolled back.
test('zcode config failure after healthy activation keeps the payload active and reports partial (AUD-003)', { skip: !darwinArm64 }, async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 's03-zcode-config-'));
  const paths = productPaths(home);
  const candidate = makeCandidate('2.0.0');
  const activations = [];
  try {
    installZcodePlugin(paths);
    breakZcodeConfig(paths);
    await assert.rejects(
      updateCommand(paths, ['--version=2.0.0'], {
        callDaemon: rpc('s03-config-1'),
        hasInstalledService: () => true,
        activateService: async (_target, serviceCandidate) => { activations.push(serviceCandidate); return { pid: 4242, service_generation: 3 }; },
        ...realUpdaterFor(candidate, '2.0.0'),
      }),
      (error) => error.code === 'ZCODE_SYNC_PARTIAL' && /ZCODE_CONFIG_INVALID/.test(error.message),
      'a failed zcode reconciliation must surface as a caller failure, not success',
    );

    const receipt = receiptOf(paths);
    assert.equal(receipt.status, 'partial');
    assert.equal(receipt.retryable, true);
    assert.equal(receipt.claim, 's03-config-1');
    assert.equal(receipt.version, '2.0.0');
    assert.equal(receipt.zcode.status, 'failed');
    assert.equal(receipt.zcode.error.code, 'ZCODE_CONFIG_INVALID');
    assert.equal(receipt.result.phase, 'active');

    // The healthy activation is never rolled back for a binding-only failure.
    const state = stateOf(paths);
    assert.equal(state.phase, 'active');
    assert.equal(state.active.version, '2.0.0');
    assert.equal(state.candidate, null);
    assert.equal(activations.length, 1, 'the service was switched exactly once and never rolled back');
    assert.equal(activations[0].version, '2.0.0');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(candidate, { recursive: true, force: true });
  }
});

// Scenario 2: the config is readable and the binding exists, but the refresh
// itself fails (a foreign-owned staged MCP binding conflicts).  Runs on every
// platform: the host outcome is real, the updater is the repo's stub idiom.
test('zcode refresh failure after healthy activation is partial, never success (AUD-003)', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 's03-zcode-refresh-'));
  const paths = productPaths(home);
  try {
    installZcodePlugin(paths);
    // A foreign MCP command in the managed staging: the next refresh must
    // refuse the tree (PLUGIN_STAGING_CONFLICT) instead of overwriting it.
    fs.writeFileSync(path.join(paths.zcodePlugin, '.mcp.json'), JSON.stringify({
      mcpServers: { external_subagent: { command: '/usr/local/bin/other-node', args: ['/tmp/foreign-bridge.mjs'], env: { ZCODE_AGENTD_SOCKET: paths.socket } } },
    }));
    const activations = [];
    await assert.rejects(
      updateCommand(paths, ['--version=0.1.0'], {
        callDaemon: rpc('s03-refresh-1'),
        hasInstalledService: () => true,
        activateService: async () => { activations.push(1); return { pid: 7, service_generation: 1 }; },
        ...stubUpdater(home),
      }),
      (error) => error.code === 'ZCODE_SYNC_PARTIAL' && /PLUGIN_STAGING_CONFLICT/.test(error.message),
    );
    const receipt = receiptOf(paths);
    assert.equal(receipt.status, 'partial');
    assert.equal(receipt.retryable, true);
    assert.equal(receipt.zcode.bound, true, 'the binding stays detected with its per-host failure detail');
    assert.equal(receipt.zcode.status, 'failed');
    assert.equal(receipt.zcode.error.code, 'PLUGIN_STAGING_CONFLICT');
    assert.equal(activations.length, 1, 'the healthy service activation is preserved');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// Scenario 3: both hosts implicated — a skipped Codex home plus a failed
// ZCode config.  One reducer, one receipt, one caller error, both details.
test('mixed host failures reduce to one partial receipt naming every host (AUD-003)', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 's03-mixed-'));
  const paths = productPaths(home);
  const locked = lockedHome(home);
  try {
    installZcodePlugin(paths);
    breakZcodeConfig(paths);
    registerCodexHome(paths, locked);
    const activations = [];
    await assert.rejects(
      updateCommand(paths, ['--version=0.1.0'], {
        callDaemon: rpc('s03-mixed-1'),
        hasInstalledService: () => true,
        activateService: async () => { activations.push(1); return { pid: 9, service_generation: 1 }; },
        ...stubUpdater(home),
      }),
      (error) => error.code === 'ZCODE_SYNC_PARTIAL'
        && error.message.includes(fs.realpathSync(locked)) && error.message.includes('skipped_not_writable')
        && error.message.includes('ZCODE_CONFIG_INVALID'),
      'the mixed error must name every host that failed',
    );
    const receipt = receiptOf(paths);
    assert.equal(receipt.status, 'partial');
    assert.equal(receipt.retryable, true);
    assert.equal(receipt.homes.all_updated, false);
    assert.equal(receipt.homes.homes[0].status, 'skipped_not_writable');
    assert.equal(receipt.homes.homes[0].home, fs.realpathSync(locked));
    assert.equal(receipt.zcode.status, 'failed');
    assert.equal(activations.length, 1, 'the healthy activation is preserved');
  } finally {
    fs.chmodSync(locked, 0o700);
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// Scenario 4: no binding at all.  An unbound host is never touched and never
// fails: the caller resolves and the receipt keeps the exact success shape.
test('no host binding stays a clean success with the unchanged receipt shape', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 's03-absent-'));
  const paths = productPaths(home);
  try {
    const result = await updateCommand(paths, ['--version=0.1.0'], {
      callDaemon: rpc('s03-absent-1'),
      hasInstalledService: () => true,
      activateService: async () => ({ pid: 11, service_generation: 1 }),
      ...stubUpdater(home),
    });
    assert.equal(result.phase, 'active');
    assert.deepEqual(result.zcode, { bound: false, status: 'absent' });
    assert.equal(fs.existsSync(paths.zcodeConfig), false, 'an unbound host config is never created');
    const receipt = receiptOf(paths);
    assert.equal(receipt.status, 'success');
    assert.deepEqual(Object.keys(receipt).sort(), ['claim', 'result', 'status', 'version'],
      'a fully healthy activation keeps the exact existing success receipt shape');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// Scenario 5: the service-less path — the same path the defect was observed
// on.  The payload activates for real; the zcode failure must not bury the
// run as success, and the freshly published active must survive.
test('a service-less update reports the failed zcode binding as partial and keeps the new active (AUD-003)', { skip: !darwinArm64 }, async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 's03-no-service-'));
  const paths = productPaths(home);
  const candidate = makeCandidate('2.0.0');
  try {
    installZcodePlugin(paths);
    breakZcodeConfig(paths);
    await assert.rejects(
      updateCommand(paths, ['--version=2.0.0'], {
        callDaemon: rpc('s03-nosvc-1'),
        hasInstalledService: () => false,
        ...realUpdaterFor(candidate, '2.0.0'),
      }),
      (error) => error.code === 'ZCODE_SYNC_PARTIAL' && /ZCODE_CONFIG_INVALID/.test(error.message),
    );
    const receipt = receiptOf(paths);
    assert.equal(receipt.status, 'partial');
    assert.equal(receipt.retryable, true);
    assert.equal(receipt.zcode.status, 'failed');
    const state = stateOf(paths);
    assert.equal(state.phase, 'active', 'the healthy payload is published, not rolled back');
    assert.equal(state.active.version, '2.0.0');
    assert.equal(state.candidate, null);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(candidate, { recursive: true, force: true });
  }
});

// Scenario 6: pure reconcile (`external-subagent reconcile` with no pending
// version drift).  This path never ran the old partial-homes reducer at all,
// so a failed host integration used to persist a success receipt.
test('pure reconcile reduces host failures truthfully instead of writing success (AUD-003)', { skip: !darwinArm64 }, async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 's03-pure-rec-'));
  const paths = productPaths(home);
  const candidate = makeCandidate(packageVersion());
  try {
    updateInstallation(paths, upgradeOptions(candidate, packageVersion()));
    installZcodePlugin(paths);
    breakZcodeConfig(paths);
    await assert.rejects(
      updateCommand(paths, ['reconcile'], { callDaemon: rpc('s03-rec-1') }),
      (error) => error.code === 'ZCODE_SYNC_PARTIAL' && /ZCODE_CONFIG_INVALID/.test(error.message),
    );
    const receipt = receiptOf(paths);
    assert.equal(receipt.status, 'partial');
    assert.equal(receipt.retryable, true);
    assert.equal(receipt.claim, 's03-rec-1');
    assert.equal(receipt.zcode.status, 'failed');
    assert.equal(stateOf(paths).active.version, packageVersion(), 'the published active is untouched');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(candidate, { recursive: true, force: true });
  }
});

// Scenario 6b: pure reconcile with a partial Codex home and a healthy (absent
// here) zcode binding — the codex-only half of the same latent pure-reconcile
// gap, keeping the existing CODEX_SYNC_PARTIAL signal.
test('pure reconcile reports a skipped codex home as partial, not success (AUD-003)', { skip: !darwinArm64 }, async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 's03-pure-homes-'));
  const paths = productPaths(home);
  const candidate = makeCandidate(packageVersion());
  const locked = lockedHome(home);
  try {
    updateInstallation(paths, upgradeOptions(candidate, packageVersion()));
    registerCodexHome(paths, locked);
    await assert.rejects(
      updateCommand(paths, ['reconcile'], { callDaemon: rpc('s03-rec-2') }),
      (error) => error.code === 'CODEX_SYNC_PARTIAL'
        && error.message.includes(fs.realpathSync(locked)) && error.message.includes('skipped_not_writable'),
    );
    const receipt = receiptOf(paths);
    assert.equal(receipt.status, 'partial');
    assert.equal(receipt.retryable, true);
    assert.equal(receipt.homes.all_updated, false);
    assert.equal(receipt.homes.homes[0].status, 'skipped_not_writable');
    assert.equal(receipt.zcode.status, 'absent', 'the healthy host keeps its per-host detail');
  } finally {
    fs.chmodSync(locked, 0o700);
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(candidate, { recursive: true, force: true });
  }
});

// Scenario 7: the retry arc.  A partial failure leaves a retryable receipt;
// once the host is repaired, the SAME public command succeeds, overwrites the
// partial receipt with a success one, and resolves for the caller.
test('a healthy retry after a partial failure clears the retryable receipt (AUD-003)', { skip: !darwinArm64 }, async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 's03-retry-'));
  const paths = productPaths(home);
  const candidate = makeCandidate(packageVersion());
  try {
    updateInstallation(paths, upgradeOptions(candidate, packageVersion()));
    installZcodePlugin(paths);
    breakZcodeConfig(paths);
    await assert.rejects(
      updateCommand(paths, ['reconcile'], { callDaemon: rpc('s03-retry-1') }),
      (error) => error.code === 'ZCODE_SYNC_PARTIAL',
    );
    assert.equal(receiptOf(paths).status, 'partial');

    // Repair the host: a valid config pointing at the staged binding again.
    fs.writeFileSync(paths.zcodeConfig, `${JSON.stringify({ plugins: { dirs: [path.resolve(paths.zcodePlugin)] } }, null, 2)}\n`);
    const result = await updateCommand(paths, ['reconcile'], { callDaemon: rpc('s03-retry-2') });
    assert.equal(result.phase, 'active');
    assert.equal(result.zcode.bound, true);
    assert.equal(result.zcode.status, 'updated');
    const receipt = receiptOf(paths);
    assert.equal(receipt.status, 'success', 'the successful retry overwrites the partial receipt');
    assert.equal(receipt.claim, 's03-retry-2');
    assert.deepEqual(Object.keys(receipt).sort(), ['claim', 'result', 'status', 'version']);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(candidate, { recursive: true, force: true });
  }
});
