import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { backupData, purge, restoreData, uninstall } from '../../cli/maintenance.mjs';
import { uninstall as uninstallProduct } from '../../cli/commands/maintenance.mjs';
import { loadCodexHomes, registerCodexHome } from '../../cli/install/reconcile.mjs';
import { SYSTEMD_UNIT_NAME } from '../../cli/install/service-linux.mjs';
import { bootstrapService, bootoutService, launchdServiceRegistrationStatus } from '../../cli/install/service-macos.mjs';
import { runInit } from '../../cli/install/init.mjs';
import { CliError } from '../../cli/errors.mjs';
import { productPaths, platform } from '../../cli/paths.mjs';

// Windows product data lives under %LOCALAPPDATA%; pin it inside each fixture
// home so no test reads or writes the real product tree.
const win32PosixBackend = process.platform === 'win32'
  ? 'win32 uses the SCM service backend; launchd/systemd definition fixtures are POSIX-only'
  : false;

function fixturePaths(home) {
  const env = process.platform === 'win32'
    ? { ...process.env, LOCALAPPDATA: path.join(home, 'AppData', 'Local') }
    : process.env;
  return productPaths(home, env);
}

// The service-removal confirmation behaviour is owned per backend: launchd
// bootout/removal on macOS, systemctl --user disable --now plus the bounded
// inactive confirmation on Linux (its seam twins live in
// tests/install/service-linux.test.mjs). The backend-specific tests below are
// host-gated to their backend; path/layout and data-retention assertions stay
// host-neutral.
const macos = platform() === 'darwin';
const linux = platform() === 'linux';
const removedServiceDefinition = macos ? 'removed_launch_agent' : 'removed_service_definition';

// The seams each backend reads: uninstall/init accept both injectable
// controls and the platform dispatch picks its own.
function bothSeams(launchctlControl, systemctlControl) {
  return { launchctl: launchctlControl, systemctl: systemctlControl };
}

// Stateful systemctl double mirroring recordingLaunchctl: show/enable/disable
// track one unit, so the uninstall oracles below observe exactly what the
// user manager would.
function recordingSystemd({ loaded = false, active = false } = {}) {
  const calls = [];
  const state = { loaded, active };
  return {
    calls,
    state,
    control(args) {
      calls.push(args.join(' '));
      if (args[0] === 'show') {
        if (!state.loaded) return { action: 'show', status: 0, stdout: 'LoadState=not-found\nActiveState=inactive\nSubState=dead\nMainPID=0\n' };
        return {
          action: 'show', status: 0,
          stdout: `LoadState=loaded\nActiveState=${state.active ? 'active' : 'inactive'}\nSubState=${state.active ? 'running' : 'dead'}\nMainPID=${state.active ? 999 : 0}\n`,
        };
      }
      if (args[0] === 'daemon-reload') return { action: 'daemon-reload', status: 0 };
      if (args[0] === 'enable') { state.loaded = true; state.active = true; return { action: 'enable', status: 0 }; }
      if (args[0] === 'disable') { state.active = false; state.loaded = false; return { action: 'disable', status: 0 }; }
      throw new Error(`unexpected systemctl call: ${args.join(' ')}`);
    },
  };
}

test('backup verifies bytes and restore replaces product data', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-data-'));
  const paths = fixturePaths(home);
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
  const paths = fixturePaths(home);
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
  const paths = fixturePaths(home);
  fs.mkdirSync(paths.data, { recursive: true });
  fs.writeFileSync(path.join(paths.data, 'state'), 'original');
  const destination = path.join(paths.data, 'backup');

  assert.throws(() => backupData(destination, paths), (error) => error.code === 'BACKUP_DESTINATION_IN_DATA');
  assert.equal(fs.existsSync(destination), false);
  assert.equal(fs.readFileSync(path.join(paths.data, 'state'), 'utf8'), 'original');
});

test('uninstall retains data, while purge is an explicit separate operation', { skip: win32PosixBackend }, () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-retain-'));
  const paths = fixturePaths(home);
  fs.mkdirSync(paths.data, { recursive: true });
  fs.mkdirSync(path.dirname(paths.launchAgent), { recursive: true });
  fs.writeFileSync(paths.launchAgent, 'plist');
  const launchctl = recordingLaunchctl();
  const systemd = recordingSystemd();
  const result = uninstall(paths, bothSeams(launchctl.control, systemd.control));
  assert.equal(result.data_retained, true);
  assert.equal(result.service_stopped, true);
  assert.equal(result.service_already_stopped, true, 'an unregistered service is not an uninstall error');
  assert.equal(result[removedServiceDefinition], true, 'the service definition is removed');
  assert.equal(fs.existsSync(paths.data), true);
  assert.equal(fs.existsSync(paths.launchAgent), false);
  purge(paths);
  assert.equal(fs.existsSync(paths.data), false);
});

test('uninstall boots out the loaded ES service before removing its definition', { skip: !macos }, () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-unload-'));
  const paths = fixturePaths(home);
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

// The systemd twin of the three launchd uninstall oracles above.
test('uninstall disables the loaded ES unit before removing its definition', { skip: !linux }, () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-unload-linux-'));
  const paths = fixturePaths(home);
  fs.mkdirSync(paths.data, { recursive: true });
  fs.mkdirSync(path.dirname(paths.launchAgent), { recursive: true });
  fs.writeFileSync(paths.launchAgent, '[Unit]\n');
  const systemd = recordingSystemd({ loaded: true, active: true });
  const result = uninstall(paths, { systemctl: systemd.control });
  assert.ok(systemd.calls.includes(`disable --now ${SYSTEMD_UNIT_NAME}`), 'uninstall must stop and disable the unit it owns');
  assert.equal(systemd.state.active, false);
  assert.equal(result.service_stopped, true);
  assert.equal(result.service_already_stopped, false);
  assert.equal(result.removed_service_definition, true);
  assert.equal(fs.existsSync(paths.data), true, 'uninstall never purges retained data');
  fs.rmSync(home, { recursive: true, force: true });
});

test('uninstall reports a stop it cannot complete instead of removing the definition', { skip: !linux }, () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-unload-stuck-linux-'));
  const paths = fixturePaths(home);
  fs.mkdirSync(path.dirname(paths.launchAgent), { recursive: true });
  fs.writeFileSync(paths.launchAgent, '[Unit]\n');
  // A unit that stays active after disable --now: the command must fail loudly
  // and leave the definition in place rather than strand a running service.
  const stuck = (args) => {
    if (args[0] === 'show') return { action: 'show', status: 0, stdout: 'LoadState=loaded\nActiveState=active\nSubState=running\nMainPID=999\n' };
    if (args[0] === 'disable') return { action: 'disable', status: 0 };
    throw new Error(`unexpected systemctl call: ${args.join(' ')}`);
  };
  assert.throws(() => uninstall(paths, { systemctl: stuck, unloadTimeoutMs: 150 }), (error) => error.code === 'SERVICE_UNLOAD_TIMEOUT');
  assert.equal(fs.existsSync(paths.launchAgent), true);
  fs.rmSync(home, { recursive: true, force: true });
});

test('product uninstall keeps every registry claim when the systemd stop cannot complete', { skip: !linux }, () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-uninstall-stuck-linux-'));
  const paths = fixturePaths(home);
  fs.mkdirSync(paths.data, { recursive: true });
  fs.mkdirSync(path.dirname(paths.launchAgent), { recursive: true });
  fs.writeFileSync(paths.launchAgent, '[Unit]\n');
  const codexHome = path.join(home, 'codex-claimed');
  fs.mkdirSync(codexHome, { recursive: true, mode: 0o700 });
  fs.writeFileSync(path.join(codexHome, 'binding.json'), 'managed binding');
  registerCodexHome(paths, codexHome, { version: '0.1.0', digest: 'deadbeef', status: 'claimed' });
  const stuck = (args) => {
    if (args[0] === 'show') return { action: 'show', status: 0, stdout: 'LoadState=loaded\nActiveState=active\nSubState=running\nMainPID=999\n' };
    if (args[0] === 'disable') return { action: 'disable', status: 0 };
    throw new Error(`unexpected systemctl call: ${args.join(' ')}`);
  };
  try {
    assert.throws(
      () => uninstallProduct(paths, { systemctl: stuck, unloadTimeoutMs: 150 }),
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
    // the user manager gives the unit up, and only then are the claims released.
    const systemd = recordingSystemd({ loaded: true, active: true });
    const retried = uninstallProduct(paths, { systemctl: systemd.control });
    assert.equal(retried.codex_homes_unregistered, 1);
    assert.deepEqual(loadCodexHomes(paths).registry.homes, []);
    assert.equal(fs.existsSync(paths.launchAgent), false);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('uninstall reports a bootout it cannot complete instead of removing the definition', { skip: !macos }, () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-unload-stuck-'));
  const paths = fixturePaths(home);
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

test('product uninstall keeps every registry claim when the bootout cannot complete', { skip: !macos }, () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-uninstall-stuck-'));
  const paths = fixturePaths(home);
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

test('product uninstall removes the service first, then releases every claim while retaining data', { skip: win32PosixBackend }, () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-uninstall-claims-'));
  const paths = fixturePaths(home);
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
  const systemd = recordingSystemd({ loaded: true, active: true });
  const result = uninstallProduct(paths, bothSeams(launchctl.control, systemd.control));
  assert.equal(result.service_stopped, true);
  if (macos) {
    assert.ok(launchctl.calls.some((call) => call.startsWith('bootout gui/')), 'uninstall must boot out the service it owns');
    assert.equal(result.service_already_stopped, false);
  } else {
    assert.ok(systemd.calls.includes(`disable --now ${SYSTEMD_UNIT_NAME}`), 'uninstall must stop and disable the unit it owns');
    assert.equal(systemd.state.active, false);
    assert.equal(result.service_already_stopped, false);
  }
  assert.equal(result[removedServiceDefinition], true);
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

// Retained installations may still carry managed hooks written by the retired
// installer. The inverse cleanup is bounded: provenance-recorded exact paths
// and the legacy `<any plugin root>/hooks/<managed script>` shapes disappear,
// while every foreign entry, unknown hooks key, and unrelated top-level key
// survives. The provenance record and the verifier staged beside it are
// deleted afterwards.
test('product uninstall removes only recognized managed hooks and deletes the staged verifier', { skip: win32PosixBackend }, () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-hook-cleanup-'));
  const paths = fixturePaths(home);
  fs.mkdirSync(paths.data, { recursive: true });
  fs.mkdirSync(path.dirname(paths.launchAgent), { recursive: true });
  fs.writeFileSync(paths.launchAgent, 'plist');

  const packageRoot = path.join(home, 'node_modules', 'external-subagent', 'plugins', 'codex', 'external-subagent');
  const stagedRoot = path.join(home, 'staged');
  const guardExact = path.join(stagedRoot, 'hooks', 'guard-bash.mjs');
  const auditExact = path.join(stagedRoot, 'hooks', 'audit-bash-result.mjs');
  const fileExact = path.join(stagedRoot, 'hooks', 'check-agent-files.mjs');
  const managed = (matcher, script) => ({ matcher, hooks: [{ type: 'process', command: process.execPath, args: [script], timeoutMs: 5000 }] });
  const foreign = { matcher: 'Bash', hooks: [{ type: 'process', command: 'existing', args: ['/usr/local/bin/foreign-hook'] }], id: 'foreign-entry' };
  const config = {
    theme: 'dark',
    hooks: {
      enabled: true,
      custom: { keep: true },
      events: {
        PreToolUse: [
          foreign,
          managed('Bash', guardExact),
          managed('^(Read|Grep|Glob|Write|Edit|Delete|Move)$', path.join(packageRoot, 'hooks', 'check-agent-files.mjs')),
        ],
        PostToolUse: [managed('Bash', path.join(packageRoot, 'hooks', 'audit-bash-result.mjs')), foreign],
        PostToolUseFailure: [managed('Bash', auditExact)],
        OtherEvent: [foreign],
      },
    },
  };
  fs.mkdirSync(path.dirname(paths.zcodeConfig), { recursive: true });
  fs.writeFileSync(paths.zcodeConfig, JSON.stringify(config));
  fs.writeFileSync(paths.hookProvenance, JSON.stringify({
    effective_guard_wrapper_path: guardExact,
    effective_audit_wrapper_path: auditExact,
    effective_file_wrapper_path: fileExact,
  }));
  const verifier = path.join(path.dirname(paths.hookProvenance), 'external-subagent-policy-verifier');
  fs.writeFileSync(verifier, '#!/usr/bin/env node\n');

  try {
    const result = uninstallProduct(paths, bothSeams(recordingLaunchctl().control, recordingSystemd().control));
    assert.equal(result.zcode_hooks.entries_removed, 4, 'exact and package-root recognized entries are removed');
    assert.equal(result.zcode_hooks.config_updated, true);
    assert.equal(result.zcode_hooks.provenance_removed, true);
    assert.equal(result.zcode_hooks.verifier_removed, true);
    assert.equal(result.zcode_hooks.error, undefined);

    const after = JSON.parse(fs.readFileSync(paths.zcodeConfig, 'utf8'));
    assert.deepEqual(after.hooks.events.PreToolUse, [foreign], 'foreign entries survive with every key');
    assert.deepEqual(after.hooks.events.PostToolUse, [foreign]);
    assert.deepEqual(after.hooks.events.PostToolUseFailure, []);
    assert.deepEqual(after.hooks.events.OtherEvent, [foreign], 'unmanaged events are untouched');
    assert.equal(after.theme, 'dark', 'unknown top-level keys survive');
    assert.deepEqual(after.hooks.custom, { keep: true }, 'unknown hooks keys survive');
    assert.equal(after.hooks.enabled, true, 'the enabled flag is not a managed-hook entry');
    assert.equal(fs.existsSync(paths.hookProvenance), false);
    assert.equal(fs.existsSync(verifier), false);

    // The cleanup is idempotent: a retry finds nothing recognized and writes
    // nothing.
    const again = uninstallProduct(paths, bothSeams(recordingLaunchctl().control, recordingSystemd().control));
    assert.equal(again.zcode_hooks.entries_removed, 0);
    assert.equal(again.zcode_hooks.config_updated, false);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// The retired installer wrote the exact lowercase script names and compared
// resolved paths exactly. Recognition must not case-fold, or a foreign
// `hooks/GUARD-BASH.mjs` on a case-sensitive filesystem would be removed.
test('managed hook recognition is case-sensitive and preserves foreign case variants', { skip: win32PosixBackend }, () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-hook-case-'));
  const paths = fixturePaths(home);
  fs.mkdirSync(paths.data, { recursive: true });
  fs.mkdirSync(path.dirname(paths.launchAgent), { recursive: true });
  fs.writeFileSync(paths.launchAgent, 'plist');
  const pluginRoot = path.join(home, 'node_modules', 'external-subagent', 'plugins', 'codex', 'external-subagent');
  const foreign = {
    matcher: 'Bash',
    hooks: [{ type: 'process', command: process.execPath, args: [path.join(pluginRoot, 'hooks', 'GUARD-BASH.mjs')], timeoutMs: 5000 }],
    id: 'foreign-case-variant',
  };
  const config = { hooks: { events: { PreToolUse: [foreign] } } };
  fs.mkdirSync(path.dirname(paths.zcodeConfig), { recursive: true });
  fs.writeFileSync(paths.zcodeConfig, JSON.stringify(config));
  // No provenance record: only the legacy shape could classify this entry.
  try {
    const result = uninstallProduct(paths, bothSeams(recordingLaunchctl().control, recordingSystemd().control));
    assert.equal(result.zcode_hooks.entries_removed, 0, 'case-variant names are not managed');
    assert.equal(result.zcode_hooks.config_updated, false);
    assert.deepEqual(JSON.parse(fs.readFileSync(paths.zcodeConfig, 'utf8')).hooks.events.PreToolUse, [foreign]);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// Every reference a retry needs must survive a config-phase failure.
// The entry below is recognized only through its provenance exact path (its
// matcher deliberately fails the legacy fallback), so deleting the provenance
// after a failed write would make it permanently unremovable.
test('a failed config write retains provenance and the staged verifier for retry', { skip: win32PosixBackend }, (t) => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-hook-retry-'));
  const paths = fixturePaths(home);
  fs.mkdirSync(paths.data, { recursive: true });
  fs.mkdirSync(path.dirname(paths.launchAgent), { recursive: true });
  fs.writeFileSync(paths.launchAgent, 'plist');
  const exact = path.join(home, 'custom-plugin-root', 'guard-bash.mjs');
  const entry = {
    matcher: '^Bash$',
    hooks: [{ type: 'process', command: process.execPath, args: [exact], timeoutMs: 5000 }],
  };
  const configDir = path.dirname(paths.zcodeConfig);
  fs.mkdirSync(configDir, { recursive: true });
  fs.writeFileSync(paths.zcodeConfig, JSON.stringify({ hooks: { events: { PreToolUse: [entry] } } }));
  fs.writeFileSync(paths.hookProvenance, JSON.stringify({ effective_guard_wrapper_path: exact }));
  const verifier = path.join(path.dirname(paths.hookProvenance), 'external-subagent-policy-verifier');
  fs.writeFileSync(verifier, '#!/usr/bin/env node\n');

  try {
    fs.chmodSync(configDir, 0o500);
    try {
      const probe = path.join(configDir, '.writable-probe');
      fs.writeFileSync(probe, 'probe');
      fs.unlinkSync(probe);
      t.skip('config directory remains writable (running as root)');
      return;
    } catch (error) {
      if (error?.code !== 'EACCES' && error?.code !== 'EPERM') throw error;
    }

    const failed = uninstallProduct(paths, bothSeams(recordingLaunchctl().control, recordingSystemd().control));
    assert.notEqual(failed.zcode_hooks.error, undefined, 'the failed write is reported');
    assert.equal(failed.zcode_hooks.entries_removed, 0);
    assert.equal(failed.zcode_hooks.provenance_removed, false, 'provenance survives a failed config phase');
    assert.equal(failed.zcode_hooks.verifier_removed, false, 'staged verifier survives a failed config phase');
    assert.equal(fs.existsSync(paths.hookProvenance), true);
    assert.equal(fs.existsSync(verifier), true);

    // Allow the write and retry: the retained exact path still classifies the
    // entry even though the legacy matcher fallback would not.
    fs.chmodSync(configDir, 0o700);
    const retried = uninstallProduct(paths, bothSeams(recordingLaunchctl().control, recordingSystemd().control));
    assert.equal(retried.zcode_hooks.entries_removed, 1);
    assert.equal(retried.zcode_hooks.config_updated, true);
    assert.equal(retried.zcode_hooks.provenance_removed, true);
    assert.equal(retried.zcode_hooks.verifier_removed, true);
    assert.deepEqual(JSON.parse(fs.readFileSync(paths.zcodeConfig, 'utf8')).hooks.events.PreToolUse, []);
    assert.equal(fs.existsSync(paths.hookProvenance), false);
    assert.equal(fs.existsSync(verifier), false);
  } finally {
    try { fs.chmodSync(configDir, 0o700); } catch {}
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// The legacy fallback is bound to the events and the single script position
// the retired installer wrote. A same-named script under the wrong event, or
// a script-looking path that is only a data argument, is foreign state.
test('legacy hook recognition is event-bound and uses the script position only', { skip: win32PosixBackend }, () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-hook-shape-'));
  const paths = fixturePaths(home);
  fs.mkdirSync(paths.data, { recursive: true });
  fs.mkdirSync(path.dirname(paths.launchAgent), { recursive: true });
  fs.writeFileSync(paths.launchAgent, 'plist');
  const pluginRoot = path.join(home, 'node_modules', 'external-subagent', 'plugins', 'codex', 'external-subagent');
  const guard = path.join(pluginRoot, 'hooks', 'guard-bash.mjs');
  const check = path.join(pluginRoot, 'hooks', 'check-agent-files.mjs');
  const audit = path.join(pluginRoot, 'hooks', 'audit-bash-result.mjs');
  const hook = (matcher, args) => ({ matcher, hooks: [{ type: 'process', command: process.execPath, args, timeoutMs: 5000 }] });
  const dataArgument = { matcher: 'Bash', hooks: [{ type: 'process', command: '/usr/bin/printf', args: ['%s', guard], timeoutMs: 5000 }] };
  const wrongEvent = hook('Bash', [guard]);
  const config = {
    hooks: {
      events: {
        PreToolUse: [hook('Bash', [guard]), dataArgument, hook('^(Read|Grep|Glob|Write|Edit|Delete|Move)$', [check])],
        PostToolUse: [wrongEvent, hook('Bash', [audit])],
        PostToolUseFailure: [hook('Bash', [audit])],
      },
    },
  };
  fs.mkdirSync(path.dirname(paths.zcodeConfig), { recursive: true });
  fs.writeFileSync(paths.zcodeConfig, JSON.stringify(config));
  try {
    const result = uninstallProduct(paths, bothSeams(recordingLaunchctl().control, recordingSystemd().control));
    assert.equal(result.zcode_hooks.entries_removed, 4, 'canonical entries under their own events are removed');
    const after = JSON.parse(fs.readFileSync(paths.zcodeConfig, 'utf8'));
    assert.deepEqual(after.hooks.events.PreToolUse, [dataArgument],
      'a script-looking path after the script position is data, not a managed hook');
    assert.deepEqual(after.hooks.events.PostToolUse, [wrongEvent],
      'guard-bash.mjs is never managed under PostToolUse');
    assert.deepEqual(after.hooks.events.PostToolUseFailure, []);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// A real provenance read error is not an absent record: exact-path evidence
// is the only way to recognize an entry whose matcher differs from the legacy
// fallback, so the record and verifier must survive until a retry can read it.
test('a provenance read failure retains the record and staged verifier for retry', { skip: win32PosixBackend }, (t) => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-provenance-retry-'));
  const paths = fixturePaths(home);
  fs.mkdirSync(paths.data, { recursive: true });
  fs.mkdirSync(path.dirname(paths.launchAgent), { recursive: true });
  fs.writeFileSync(paths.launchAgent, 'plist');
  const exact = path.join(home, 'custom-plugin-root', 'guard-bash.mjs');
  const entry = {
    matcher: '^Bash$',
    hooks: [{ type: 'process', command: process.execPath, args: [exact], timeoutMs: 5000 }],
  };
  fs.mkdirSync(path.dirname(paths.zcodeConfig), { recursive: true });
  fs.writeFileSync(paths.zcodeConfig, JSON.stringify({ hooks: { events: { PreToolUse: [entry] } } }));
  fs.writeFileSync(paths.hookProvenance, JSON.stringify({ effective_guard_wrapper_path: exact }));
  const verifier = path.join(path.dirname(paths.hookProvenance), 'external-subagent-policy-verifier');
  fs.writeFileSync(verifier, '#!/usr/bin/env node\n');

  try {
    fs.chmodSync(paths.hookProvenance, 0o000);
    try {
      fs.readFileSync(paths.hookProvenance);
      t.skip('provenance file remains readable (running as root)');
      return;
    } catch (error) {
      if (error?.code !== 'EACCES' && error?.code !== 'EPERM') throw error;
    }

    const failed = uninstallProduct(paths, bothSeams(recordingLaunchctl().control, recordingSystemd().control));
    assert.notEqual(failed.zcode_hooks.error, undefined, 'the provenance read failure is reported');
    assert.equal(failed.zcode_hooks.entries_removed, 0);
    assert.equal(failed.zcode_hooks.provenance_removed, false, 'the unreadable provenance is retained');
    assert.equal(failed.zcode_hooks.verifier_removed, false, 'the staged verifier is retained');
    assert.equal(fs.existsSync(paths.hookProvenance), true);
    assert.equal(fs.existsSync(verifier), true);
    assert.deepEqual(JSON.parse(fs.readFileSync(paths.zcodeConfig, 'utf8')).hooks.events.PreToolUse, [entry]);

    fs.chmodSync(paths.hookProvenance, 0o600);
    const retried = uninstallProduct(paths, bothSeams(recordingLaunchctl().control, recordingSystemd().control));
    assert.equal(retried.zcode_hooks.entries_removed, 1);
    assert.equal(retried.zcode_hooks.config_updated, true);
    assert.equal(retried.zcode_hooks.provenance_removed, true);
    assert.equal(retried.zcode_hooks.verifier_removed, true);
    assert.deepEqual(JSON.parse(fs.readFileSync(paths.zcodeConfig, 'utf8')).hooks.events.PreToolUse, []);
    assert.equal(fs.existsSync(paths.hookProvenance), false);
    assert.equal(fs.existsSync(verifier), false);
  } finally {
    try { fs.chmodSync(paths.hookProvenance, 0o600); } catch {}
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// The provenance exact-path rule is the installer's written shape: the
// recorded wrapper must be the process script position (`args[0]`), not a
// data argument that merely contains the recorded path.
test('provenance exact-path recognition uses the script position only', { skip: win32PosixBackend }, () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-hook-exact-shape-'));
  const paths = fixturePaths(home);
  fs.mkdirSync(paths.data, { recursive: true });
  fs.mkdirSync(path.dirname(paths.launchAgent), { recursive: true });
  fs.writeFileSync(paths.launchAgent, 'plist');
  const exact = path.join(home, 'staged-plugin', 'hooks', 'guard-bash.mjs');
  const canonical = {
    matcher: 'Bash',
    hooks: [{ type: 'process', command: process.execPath, args: [exact], timeoutMs: 5000 }],
  };
  const dataArgument = {
    matcher: 'Bash',
    hooks: [{ type: 'process', command: '/usr/bin/printf', args: ['%s', exact], timeoutMs: 5000 }],
  };
  fs.mkdirSync(path.dirname(paths.zcodeConfig), { recursive: true });
  fs.writeFileSync(paths.zcodeConfig, JSON.stringify({ hooks: { events: { PreToolUse: [canonical, dataArgument] } } }));
  fs.writeFileSync(paths.hookProvenance, JSON.stringify({ effective_guard_wrapper_path: exact }));
  const verifier = path.join(path.dirname(paths.hookProvenance), 'external-subagent-policy-verifier');
  fs.writeFileSync(verifier, '#!/usr/bin/env node\n');
  try {
    const result = uninstallProduct(paths, bothSeams(recordingLaunchctl().control, recordingSystemd().control));
    assert.equal(result.zcode_hooks.entries_removed, 1, 'only the args[0] exact-path entry is managed');
    assert.equal(result.zcode_hooks.provenance_removed, true);
    assert.equal(result.zcode_hooks.verifier_removed, true);
    assert.deepEqual(JSON.parse(fs.readFileSync(paths.zcodeConfig, 'utf8')).hooks.events.PreToolUse, [dataArgument],
      'the recorded path passed as data is preserved');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// launchd answers a bootstrap over an already-loaded label with
// "Bootstrap failed: 5: Input/output error" (observed live in the user GUI
// domain); a repeat start/init must stay idempotent instead of failing.
function servicePaths() {
  return productPaths(fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-boot-')));
}

test('bootstrap reports already-loaded instead of surfacing the launchd EIO', { skip: !macos }, () => {
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

  const status = launchdServiceRegistrationStatus(501, { launchctl: control });
  assert.equal(status.registered, true);
  assert.equal(status.pid, 4242);
});

test('bootstrap resolves a lost race through the registration lookup and still fails real errors', { skip: !macos }, () => {
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
  assert.equal(launchdServiceRegistrationStatus(501, { launchctl: absent }).registered, false);
});

test('service status lookup stays neutralized under the launchd test seam', { skip: !macos }, () => {
  const previous = process.env.EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL;
  process.env.EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL = '1';
  try {
    const status = launchdServiceRegistrationStatus();
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

// The active backend double for the init fixtures: the recording launchctl on
// macOS, the recording systemctl on Linux, each with the seam key the
// dispatched backend reads.
function recordingBackend({ loaded = false } = {}) {
  if (macos) {
    const launchctl = recordingLaunchctl({ loaded });
    return { seams: () => ({ launchctl: launchctl.control }), calls: launchctl.calls, state: launchctl.state };
  }
  const systemd = recordingSystemd({ loaded, active: loaded });
  return { seams: () => ({ systemctl: systemd.control }), calls: systemd.calls, state: systemd.state };
}

function initFixture({ failStep = 'publish-active-payload' } = {}) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-service-'));
  const paths = fixturePaths(home);
  // AUD-005/D1: init installs the standalone service only, so the injected
  // post-bootstrap failure lives at the final baseline-publication step (the
  // payload verifies from the repo's staged native tree); no host binding and
  // no codex CLI fake is involved anymore.
  const run = (overrides = {}) => runInit({
    paths,
    _failStep: failStep,
    ...overrides,
  });
  return { home, paths, run };
}

test('a failed init boots back out the service that init itself loaded', { skip: win32PosixBackend }, () => {
  const { paths, run } = initFixture();
  const backend = recordingBackend();
  try {
    assert.throws(() => run(backend.seams()), /injected failure at publish-active-payload/u);
    if (macos) {
      assert.ok(backend.calls.some((call) => call.startsWith('bootout gui/')), 'rollback must undo the bootstrap init performed');
      assert.equal(backend.state.loaded, false);
    } else {
      assert.ok(backend.calls.includes(`disable --now ${SYSTEMD_UNIT_NAME}`), 'rollback must undo the enable init performed');
      assert.equal(backend.state.active, false);
    }
    assert.equal(fs.existsSync(paths.launchAgent), false, 'the service definition still rolls back with the service');
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('a failed init never boots out a service that was already loaded before it', { skip: win32PosixBackend }, () => {
  const { paths, run } = initFixture();
  const backend = recordingBackend({ loaded: true });
  try {
    assert.throws(() => run(backend.seams()), /injected failure at publish-active-payload/u);
    if (macos) {
      assert.equal(backend.calls.some((call) => call.startsWith('bootstrap ')), false, 'an already-loaded label is not bootstrapped again');
      assert.equal(backend.calls.some((call) => call.startsWith('bootout')), false, 'rollback must not touch a service init did not load');
      assert.equal(backend.state.loaded, true);
    } else {
      assert.equal(backend.calls.some((call) => call.startsWith('enable')), false, 'an already-live unit is not enabled again');
      assert.equal(backend.calls.some((call) => call.startsWith('disable')), false, 'rollback must not touch a service init did not load');
      assert.equal(backend.state.active, true);
    }
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('stop confirms launchd removal before returning and stays idempotent', { skip: !macos }, () => {
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

test('a stop whose job stays registered fails bounded instead of pretending removal', { skip: !macos }, () => {
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

test('a stop racing an external removal treats the gone job as stopped', { skip: !macos }, () => {
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
