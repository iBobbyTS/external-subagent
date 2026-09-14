import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { backupData, cleanupLegacy, purge, restoreData, uninstall } from '../../cli/maintenance.mjs';
import { uninstall as uninstallProduct } from '../../cli/commands/maintenance.mjs';
import { loadCodexHomes, registerCodexHome } from '../../cli/install/reconcile.mjs';
import { bootstrapService, bootoutService, serviceRegistrationStatus } from '../../cli/install/service-macos.mjs';
import { runInit } from '../../cli/install/init.mjs';
import { CliError } from '../../cli/errors.mjs';
import { productPaths } from '../../cli/paths.mjs';

test('backup verifies bytes and restore replaces product data', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-data-'));
  const paths = productPaths(home);
  fs.mkdirSync(paths.data, { recursive: true });
  fs.writeFileSync(path.join(paths.data, 'state.bin'), Buffer.from([0, 1, 2, 255]));
  const backup = path.join(home, 'backup');
  assert.equal(backupData(backup, paths).files, 1);
  fs.writeFileSync(path.join(paths.data, 'state.bin'), 'changed');
  assert.equal(restoreData(backup, paths).files, 1);
  assert.deepEqual(fs.readFileSync(path.join(paths.data, 'state.bin')), Buffer.from([0, 1, 2, 255]));
});

test('restore detects corrupted backup before replacing data', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-corrupt-'));
  const paths = productPaths(home);
  fs.mkdirSync(paths.data, { recursive: true });
  fs.writeFileSync(path.join(paths.data, 'state'), 'original');
  const backup = path.join(home, 'backup');
  backupData(backup, paths);
  fs.writeFileSync(path.join(backup, 'data', 'state'), 'corrupt');
  fs.writeFileSync(path.join(paths.data, 'state'), 'current');
  assert.throws(() => restoreData(backup, paths), (error) => error.code === 'BACKUP_CORRUPT');
  assert.equal(fs.readFileSync(path.join(paths.data, 'state'), 'utf8'), 'current');
});

test('backup rejects a destination inside product data before creating files', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-nested-backup-'));
  const paths = productPaths(home);
  fs.mkdirSync(paths.data, { recursive: true });
  fs.writeFileSync(path.join(paths.data, 'state'), 'original');
  const destination = path.join(paths.data, 'backup');

  assert.throws(() => backupData(destination, paths), (error) => error.code === 'BACKUP_DESTINATION_IN_DATA');
  assert.equal(fs.existsSync(destination), false);
  assert.equal(fs.readFileSync(path.join(paths.data, 'state'), 'utf8'), 'original');
});

test('uninstall retains data, while purge is an explicit separate operation', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-retain-'));
  const paths = productPaths(home);
  fs.mkdirSync(paths.data, { recursive: true });
  fs.mkdirSync(path.dirname(paths.launchAgent), { recursive: true });
  fs.writeFileSync(paths.launchAgent, 'plist');
  const result = uninstall(paths, { launchctl: recordingLaunchctl().control });
  assert.equal(result.data_retained, true);
  assert.equal(result.service_stopped, true);
  assert.equal(result.service_already_stopped, true, 'an unregistered service is not an uninstall error');
  assert.equal(fs.existsSync(paths.data), true);
  assert.equal(fs.existsSync(paths.launchAgent), false);
  purge(paths);
  assert.equal(fs.existsSync(paths.data), false);
});

test('uninstall boots out the loaded ES service before removing its definition', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-unload-'));
  const paths = productPaths(home);
  fs.mkdirSync(paths.data, { recursive: true });
  fs.mkdirSync(path.dirname(paths.launchAgent), { recursive: true });
  fs.writeFileSync(paths.launchAgent, 'plist');
  const launchctl = recordingLaunchctl({ loaded: true });
  const result = uninstall(paths, { launchctl: launchctl.control });
  assert.ok(launchctl.calls.some((call) => call.startsWith('bootout gui/')), 'uninstall must boot out the service it owns');
  assert.equal(launchctl.state.loaded, false);
  assert.equal(result.service_stopped, true);
  assert.equal(result.service_already_stopped, false);
  assert.equal(result.removed_launch_agent, true);
  assert.equal(fs.existsSync(paths.data), true, 'uninstall never purges retained data');
  fs.rmSync(home, { recursive: true, force: true });
});

test('uninstall reports a bootout it cannot complete instead of removing the definition', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-unload-stuck-'));
  const paths = productPaths(home);
  fs.mkdirSync(path.dirname(paths.launchAgent), { recursive: true });
  fs.writeFileSync(paths.launchAgent, 'plist');
  // A job that stays registered after bootout: the command must fail loudly
  // and leave the definition in place rather than strand a running service.
  const stuck = (args) => {
    if (args[0] === 'print') return { action: 'print', status: 0, stdout: 'state = running\npid = 999\n' };
    if (args[0] === 'bootout') return { action: 'bootout', status: 0 };
    throw new Error(`unexpected launchctl call: ${args.join(' ')}`);
  };
  assert.throws(() => uninstall(paths, { launchctl: stuck, unloadTimeoutMs: 150 }), (error) => error.code === 'SERVICE_UNLOAD_TIMEOUT');
  assert.equal(fs.existsSync(paths.launchAgent), true);
  fs.rmSync(home, { recursive: true, force: true });
});

test('product uninstall keeps every registry claim when the bootout cannot complete', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-uninstall-stuck-'));
  const paths = productPaths(home);
  fs.mkdirSync(paths.data, { recursive: true });
  fs.mkdirSync(path.dirname(paths.launchAgent), { recursive: true });
  fs.writeFileSync(paths.launchAgent, 'plist');
  const codexHome = path.join(home, 'codex-claimed');
  fs.mkdirSync(codexHome, { recursive: true, mode: 0o700 });
  fs.writeFileSync(path.join(codexHome, 'binding.json'), 'managed binding');
  registerCodexHome(paths, codexHome, { version: '0.1.0', digest: 'deadbeef', status: 'claimed' });
  // A job that stays registered after bootout: the command must fail before
  // any claim is released, leaving the registry and the bound homes exactly
  // as they were so the uninstall can simply be retried.
  const stuck = (args) => {
    if (args[0] === 'print') return { action: 'print', status: 0, stdout: 'state = running\npid = 999\n' };
    if (args[0] === 'bootout') return { action: 'bootout', status: 0 };
    throw new Error(`unexpected launchctl call: ${args.join(' ')}`);
  };
  try {
    assert.throws(
      () => uninstallProduct(paths, { launchctl: stuck, unloadTimeoutMs: 150 }),
      (error) => error.code === 'SERVICE_UNLOAD_TIMEOUT',
    );
    const registry = loadCodexHomes(paths).registry;
    assert.deepEqual(registry.homes.map((entry) => entry.home), [fs.realpathSync(codexHome)],
      'a failed service removal must not release any claim');
    assert.equal(registry.homes[0].digest, 'deadbeef', 'per-home registry state survives the failed uninstall verbatim');
    assert.equal(fs.readFileSync(path.join(codexHome, 'binding.json'), 'utf8'), 'managed binding',
      'the bound home is left untouched for the retry');
    assert.equal(fs.existsSync(paths.launchAgent), true, 'the service definition stays in place');

    // The intermediate state is retry-safe: the same uninstall completes once
    // launchd gives the job up, and only then are the claims released.
    const launchctl = recordingLaunchctl({ loaded: true });
    const retried = uninstallProduct(paths, { launchctl: launchctl.control });
    assert.equal(retried.codex_homes_unregistered, 1);
    assert.deepEqual(loadCodexHomes(paths).registry.homes, []);
    assert.equal(fs.existsSync(paths.launchAgent), false);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('product uninstall removes the service first, then releases every claim while retaining data', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-uninstall-claims-'));
  const paths = productPaths(home);
  fs.mkdirSync(paths.data, { recursive: true });
  fs.writeFileSync(path.join(paths.data, 'install-state.json'), 'state');
  fs.mkdirSync(path.dirname(paths.launchAgent), { recursive: true });
  fs.writeFileSync(paths.launchAgent, 'plist');
  const claimed = [];
  for (const name of ['codex-a', 'codex-b']) {
    const codexHome = path.join(home, name);
    fs.mkdirSync(codexHome, { recursive: true, mode: 0o700 });
    fs.writeFileSync(path.join(codexHome, 'binding.json'), `binding ${name}`);
    registerCodexHome(paths, codexHome, { version: '0.1.0', status: 'claimed' });
    claimed.push(codexHome);
  }
  const launchctl = recordingLaunchctl({ loaded: true });
  const result = uninstallProduct(paths, { launchctl: launchctl.control });
  assert.ok(launchctl.calls.some((call) => call.startsWith('bootout gui/')), 'uninstall must boot out the service it owns');
  assert.equal(result.service_stopped, true);
  assert.equal(result.service_already_stopped, false);
  assert.equal(result.removed_launch_agent, true);
  assert.equal(result.codex_homes_unregistered, 2, 'every claimed home is released');
  assert.equal(result.data_retained, true);
  assert.equal(fs.existsSync(paths.data), true, 'uninstall never purges retained data');
  assert.equal(fs.existsSync(paths.launchAgent), false);
  assert.deepEqual(loadCodexHomes(paths).registry.homes, [], 'no home remains claimed');
  for (const codexHome of claimed) {
    assert.equal(fs.readFileSync(path.join(codexHome, 'binding.json'), 'utf8'), `binding ${path.basename(codexHome)}`,
      'claim release is registry-only; bound home contents are left to their owners');
  }
  fs.rmSync(home, { recursive: true, force: true });
});

test('legacy cleanup removes only enumerated old paths and creates no alias or migration', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-legacy-'));
  const old = path.join(home, '.local', 'bin', 'zcode-reviewd');
  fs.mkdirSync(path.dirname(old), { recursive: true });
  fs.writeFileSync(old, 'old');
  const result = cleanupLegacy(home);
  assert.equal(result.migration, false);
  assert.deepEqual(result.aliases_created, []);
  assert.equal(fs.existsSync(old), false);
});

// launchd answers a bootstrap over an already-loaded label with
// "Bootstrap failed: 5: Input/output error" (observed live in the user GUI
// domain); a repeat start/init must stay idempotent instead of failing.
function servicePaths() {
  return productPaths(fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-boot-')));
}

test('bootstrap reports already-loaded instead of surfacing the launchd EIO', () => {
  const paths = servicePaths();
  const calls = [];
  const control = (args) => {
    calls.push(args.join(' '));
    if (args[0] === 'print') return { action: 'print', status: 0, stdout: 'state = running\n\tpid = 4242\n' };
    throw new CliError('DAEMON_CONTROL_FAILED', 'Bootstrap failed: 5: Input/output error');
  };
  const started = bootstrapService(paths, 501, { launchctl: control });
  assert.deepEqual(calls, ['print gui/501/com.external-subagent.daemon'],
    'an already-loaded label must never reach bootstrap');
  assert.equal(started.already_loaded, true);
  assert.equal(started.state, 'running');
  assert.equal(started.pid, 4242);

  const status = serviceRegistrationStatus(501, { launchctl: control });
  assert.equal(status.registered, true);
  assert.equal(status.pid, 4242);
});

test('bootstrap resolves a lost race through the registration lookup and still fails real errors', () => {
  const paths = servicePaths();
  // First print: absent. bootstrap: EIO (a racing loader won). Second print: loaded.
  let prints = 0;
  const raced = (args) => {
    if (args[0] !== 'print') throw new CliError('DAEMON_CONTROL_FAILED', 'Bootstrap failed: 5: Input/output error');
    prints += 1;
    return prints === 1 ? { action: 'print', absent: true } : { action: 'print', status: 0, stdout: 'pid = 777\n' };
  };
  const resolved = bootstrapService(paths, 501, { launchctl: raced });
  assert.equal(resolved.already_loaded, true);
  assert.equal(resolved.pid, 777);

  // Both prints absent and bootstrap still failing is a genuine control failure.
  const absent = (args) => (args[0] === 'print' ? { action: 'print', absent: true } : (() => { throw new CliError('DAEMON_CONTROL_FAILED', 'Bootstrap failed: 5: Input/output error'); })());
  assert.throws(() => bootstrapService(paths, 501, { launchctl: absent }), (error) => error.code === 'DAEMON_CONTROL_FAILED');
  assert.equal(serviceRegistrationStatus(501, { launchctl: absent }).registered, false);
});

test('service status lookup stays neutralized under the launchd test seam', () => {
  const previous = process.env.EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL;
  process.env.EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL = '1';
  try {
    const status = serviceRegistrationStatus();
    assert.equal(status.registered, null);
    assert.equal(status.query, 'skipped');
    const started = bootstrapService(servicePaths());
    assert.equal(started.skipped, true);
  } finally {
    if (previous === undefined) delete process.env.EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL;
    else process.env.EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL = previous;
  }
});

// Stateful launchctl double: print/bootstrap/bootout track one loaded label,
// so the init rollback oracles below observe exactly what launchd would see.
function recordingLaunchctl({ loaded = false } = {}) {
  const calls = [];
  const state = { loaded };
  return {
    calls,
    state,
    control(args) {
      calls.push(args.join(' '));
      if (args[0] === 'print') {
        return state.loaded
          ? { action: 'print', status: 0, stdout: 'state = running\npid = 999\n' }
          : { action: 'print', absent: true };
      }
      if (args[0] === 'bootstrap') { state.loaded = true; return { action: 'bootstrap', status: 0 }; }
      if (args[0] === 'bootout') { state.loaded = false; return { action: 'bootout', status: 0 }; }
      throw new Error(`unexpected launchctl call: ${args.join(' ')}`);
    },
  };
}

function initFixture({ failStep } = {}) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-service-'));
  const paths = productPaths(home);
  // The fake codex CLI materializes the plugin cache (staged manifest +
  // .mcp.json under plugins/cache/<marketplace>/<plugin>/<version>) like the
  // real CLI, so the init runs below get past installPlugin's read-back cache
  // verification before their injected claim-codex-home failure.
  const fakeCodex = path.join(home, 'codex-fake.mjs');
  fs.writeFileSync(fakeCodex, `#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
const args = process.argv.slice(2);
const rootsFile = path.join(${JSON.stringify(home)}, 'marketplace-roots.json');
const text = (value) => { process.stdout.write(JSON.stringify(value, null, 2) + '\\n'); };
const loadRoots = () => { try { return JSON.parse(fs.readFileSync(rootsFile, 'utf8')); } catch { return {}; } };
if (args[0] === 'plugin' && args[1] === 'add' && args.includes('--help')) { process.stdout.write('usage\\n'); process.exit(0); }
if (args[0] === 'plugin' && args[1] === 'marketplace' && args[2] === 'add') {
  const roots = loadRoots();
  roots[process.env.CODEX_HOME] = args[3];
  fs.writeFileSync(rootsFile, JSON.stringify(roots));
  text({ marketplaceName: 'personal' });
  process.exit(0);
}
if (args[0] === 'plugin' && args[1] === 'add') {
  const name = args[2]; const marketplace = args[args.indexOf('--marketplace') + 1];
  const root = loadRoots()[process.env.CODEX_HOME];
  if (!root) { process.stderr.write('no marketplace registered for this CODEX_HOME\\n'); process.exit(1); }
  const doc = JSON.parse(fs.readFileSync(path.join(root, '.agents', 'plugins', 'marketplace.json'), 'utf8'));
  const staging = path.resolve(root, doc.plugins.find((plugin) => plugin.name === name).source.path);
  const version = JSON.parse(fs.readFileSync(path.join(staging, '.codex-plugin', 'plugin.json'), 'utf8')).version;
  const cache = path.join(process.env.CODEX_HOME || '', 'plugins', 'cache', marketplace, name, String(version));
  fs.rmSync(cache, { recursive: true, force: true });
  fs.mkdirSync(path.dirname(cache), { recursive: true });
  fs.cpSync(staging, cache, { recursive: true });
  text({ pluginId: name + '@' + marketplace, name, marketplaceName: marketplace, version, installedPath: cache });
  process.exit(0);
}
process.stderr.write('unexpected codex invocation: ' + JSON.stringify(args) + '\\n');
process.exit(1);
`);
  fs.chmodSync(fakeCodex, 0o755);
  const run = (overrides = {}) => runInit({
    paths,
    skipRuntimeProbe: true,
    skipPayloadProbe: true,
    codexCli: fakeCodex,
    codexHome: path.join(home, 'codex-home'),
    ...(failStep ? { _failStep: failStep } : {}),
    ...overrides,
  });
  return { home, paths, run };
}

test('a failed init boots back out the service that init itself loaded', () => {
  const { paths, run } = initFixture({ failStep: 'claim-codex-home' });
  const launchctl = recordingLaunchctl();
  try {
    assert.throws(() => run({ launchctl: launchctl.control }), /injected failure at claim-codex-home/u);
    assert.ok(launchctl.calls.some((call) => call.startsWith('bootout gui/')), 'rollback must undo the bootstrap init performed');
    assert.equal(launchctl.state.loaded, false);
    assert.equal(fs.existsSync(paths.launchAgent), false, 'the LaunchAgent still rolls back with the service');
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('a failed init never boots out a service that was already loaded before it', () => {
  const { paths, run } = initFixture({ failStep: 'claim-codex-home' });
  const launchctl = recordingLaunchctl({ loaded: true });
  try {
    assert.throws(() => run({ launchctl: launchctl.control }), /injected failure at claim-codex-home/u);
    assert.equal(launchctl.calls.some((call) => call.startsWith('bootstrap ')), false, 'an already-loaded label is not bootstrapped again');
    assert.equal(launchctl.calls.some((call) => call.startsWith('bootout')), false, 'rollback must not touch a service init did not load');
    assert.equal(launchctl.state.loaded, true);
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('stop confirms launchd removal before returning and stays idempotent', () => {
  const paths = servicePaths();
  try {
    // Loaded job: bootout unloads, and the stop only succeeds once the
    // registration probe observes the job gone (a bootstrap issued before
    // launchd finishes the removal can be swept by it — observed live).
    const loaded = recordingLaunchctl({ loaded: true });
    const stopped = bootoutService(paths, process.getuid(), { launchctl: loaded.control });
    assert.equal(stopped.removed, true);
    assert.equal(stopped.already_stopped, undefined);
    assert.ok(loaded.calls.filter((call) => call.startsWith('print gui/')).length >= 2, 'removal must be confirmed by a follow-up probe');
    // Stopped job: an idempotent re-stop reports already_stopped without
    // issuing another bootout.
    const unloaded = recordingLaunchctl({ loaded: false });
    const again = bootoutService(paths, process.getuid(), { launchctl: unloaded.control });
    assert.equal(again.already_stopped, true);
    assert.equal(unloaded.calls.some((call) => call.startsWith('bootout')), false);
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('a stop whose job stays registered fails bounded instead of pretending removal', () => {
  const paths = servicePaths();
  try {
    const stuck = (args) => {
      if (args[0] === 'print') return { action: 'print', status: 0, stdout: 'state = running\npid = 999\n' };
      if (args[0] === 'bootout') return { action: 'bootout', status: 0 };
      throw new Error(`unexpected launchctl call: ${args.join(' ')}`);
    };
    assert.throws(
      () => bootoutService(paths, process.getuid(), { launchctl: stuck, unloadTimeoutMs: 150 }),
      (error) => error.code === 'SERVICE_UNLOAD_TIMEOUT',
    );
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('a stop racing an external removal treats the gone job as stopped', () => {
  const paths = servicePaths();
  try {
    // The registration probe sees the job, but by the time bootout runs a
    // concurrent removal won; launchd answers an error, and the settle probe
    // confirms the job is gone — a repeated stop must stay idempotent.
    let seen = false;
    const racing = (args) => {
      if (args[0] === 'print') {
        if (!seen) { seen = true; return { action: 'print', status: 0, stdout: 'state = running\npid = 999\n' }; }
        return { action: 'print', absent: true };
      }
      if (args[0] === 'bootout') throw new Error('Boot-out failed: 5: Input/output error');
      throw new Error(`unexpected launchctl call: ${args.join(' ')}`);
    };
    const result = bootoutService(paths, process.getuid(), { launchctl: racing });
    assert.equal(result.already_stopped, true);
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});
