import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

const cli = path.resolve('bin/external-subagent.mjs');

// Legacy brand literals are synthesized at runtime so this anti-residue guard
// carries no legacy literal in its own source.
const legacyShort = ['z', 'a', 's'].join('');
const legacyCliName = ['zcode', 'as', 'subagent'].join('-');

function run(home, args) {
  return spawnSync(process.execPath, [cli, ...args], {
    encoding: 'utf8',
    env: { ...process.env, HOME: home, EXTERNAL_SUBAGENT_TEST_PLATFORM: 'win32' },
  });
}

test('Windows help and version work without creating anything', () => {
  for (const args of [['--help'], ['version']]) {
    const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-win-basic-'));
    const result = run(home, args);
    assert.equal(result.status, 0, result.stderr);
    assert.equal(fs.readdirSync(home).length, 0);
  }
});

test('help identifies the public CLI as external-subagent', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-win-help-'));
  const result = run(home, ['--help']);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /^external-subagent \S+\n\nUsage: external-subagent <command> \[options\]/u);
  assert.doesNotMatch(result.stdout, new RegExp(`Usage: ${legacyCliName}|Usage: ${legacyShort}`, 'u'));
  assert.deepEqual(fs.readdirSync(home), []);
});

test('every business command is rejected structurally before filesystem side effects', () => {
  const commands = ['init', 'status', 'diagnose', 'backup', 'restore', 'start', 'stop', 'uninstall', 'purge', 'create', 'wait', 'list', 'send', 'respond', 'cancel', 'result', 'close'];
  for (const command of commands) {
    const home = fs.mkdtempSync(path.join(os.tmpdir(), `external-subagent-win-${command}-`));
    const result = run(home, [command]);
    assert.equal(result.status, 1, `${command}: ${result.stderr}`);
    assert.equal(JSON.parse(result.stderr).error.code, 'UNSUPPORTED_PLATFORM');
    assert.deepEqual(fs.readdirSync(home), []);
  }
});

test('Windows productPaths derives all 12 frozen fields and honors LOCALAPPDATA injection and fallback', async () => {
  const { productPaths } = await import('../../cli/paths.mjs');
  const home = '/users/testuser';

  // Fallback when LOCALAPPDATA is absent, empty, or relative
  for (const emptyOrRelative of [undefined, '', 'relative/path']) {
    const env = {
      EXTERNAL_SUBAGENT_TEST_PLATFORM: 'win32',
      ...(emptyOrRelative !== undefined ? { LOCALAPPDATA: emptyOrRelative } : {}),
    };
    const paths = productPaths(home, env);

    const expectedData = path.join(home, 'AppData', 'Local', 'external-subagent');
    assert.equal(paths.home, home);
    assert.equal(paths.data, expectedData);
    assert.equal(paths.config, path.join(expectedData, 'config.json'));
    assert.equal(paths.state, path.join(expectedData, 'install-state.json'));
    assert.equal(paths.hookProvenance, path.join(expectedData, 'zcode-agent-hook-provenance.json'));
    assert.equal(paths.database, path.join(expectedData, 'external-subagent.sqlite3'));
    assert.equal(paths.socket, '\\\\.\\pipe\\external-subagent-daemon');
    assert.equal(paths.socket.slice(0, 9), '\\\\.\\pipe\\');
    assert.equal(paths.logs, path.join(home, 'AppData', 'Local', 'external-subagent', 'logs'));
    assert.equal(paths.launchAgent, null);
    assert.equal(paths.zcodeConfig, path.join(home, '.zcode', 'cli', 'config.json'));
    assert.equal(paths.zcodePlugin, path.join(expectedData, 'zcode-plugin', 'external-subagent'));
    assert.equal(paths.profiles, path.join(expectedData, 'profiles'));
  }

  // Injected LOCALAPPDATA (non-empty absolute path)
  const injectedLocal = '/custom/local/appdata';
  const injectedEnv = {
    EXTERNAL_SUBAGENT_TEST_PLATFORM: 'win32',
    LOCALAPPDATA: injectedLocal,
  };
  const injectedPaths = productPaths(home, injectedEnv);
  const expectedInjectedData = path.join(injectedLocal, 'external-subagent');
  assert.equal(injectedPaths.home, home);
  assert.equal(injectedPaths.data, expectedInjectedData);
  assert.equal(injectedPaths.config, path.join(expectedInjectedData, 'config.json'));
  assert.equal(injectedPaths.state, path.join(expectedInjectedData, 'install-state.json'));
  assert.equal(injectedPaths.hookProvenance, path.join(expectedInjectedData, 'zcode-agent-hook-provenance.json'));
  assert.equal(injectedPaths.database, path.join(expectedInjectedData, 'external-subagent.sqlite3'));
  assert.equal(injectedPaths.socket, '\\\\.\\pipe\\external-subagent-daemon');
  assert.equal(injectedPaths.logs, path.join(injectedLocal, 'external-subagent', 'logs'));
  assert.equal(injectedPaths.launchAgent, null);
  assert.equal(injectedPaths.zcodeConfig, path.join(home, '.zcode', 'cli', 'config.json'));
  assert.equal(injectedPaths.zcodePlugin, path.join(expectedInjectedData, 'zcode-plugin', 'external-subagent'));
  assert.equal(injectedPaths.profiles, path.join(expectedInjectedData, 'profiles'));
});

test('win32 service dispatch seam fails closed without invoking getuid, launchctl, or systemctl', async () => {
  const {
    bootstrapService,
    bootoutService,
    installServiceDefinition,
    serviceRegistrationStatus,
  } = await import('../../cli/install/service-macos.mjs');
  const { activateService } = await import('../../cli/install/service-activation.mjs');
  const { productPaths } = await import('../../cli/paths.mjs');

  const previousPlatform = process.env.EXTERNAL_SUBAGENT_TEST_PLATFORM;
  const originalGetuid = process.getuid;
  process.env.EXTERNAL_SUBAGENT_TEST_PLATFORM = 'win32';

  let getuidCalled = false;
  process.getuid = () => {
    getuidCalled = true;
    throw new Error('process.getuid must not be called on win32');
  };

  let launchctlCalled = false;
  let systemctlCalled = false;
  const controlSpies = {
    launchctl: () => { launchctlCalled = true; throw new Error('launchctl must not be called'); },
    systemctl: () => { systemctlCalled = true; throw new Error('systemctl must not be called'); },
  };

  const paths = productPaths('/home/winuser', { EXTERNAL_SUBAGENT_TEST_PLATFORM: 'win32' });

  try {
    // 1. bootstrapService rejects with UNSUPPORTED_PLATFORM_SERVICE
    assert.throws(
      () => bootstrapService(paths, undefined, controlSpies),
      (error) => {
        assert.equal(error.code, 'UNSUPPORTED_PLATFORM_SERVICE');
        assert.match(error.message, /^windows:.*pending real-machine implementation/u);
        return true;
      },
    );

    // Also assert with default parameters (no uid or options passed) to verify lazy uid
    assert.throws(
      () => bootstrapService(paths),
      (error) => {
        assert.equal(error.code, 'UNSUPPORTED_PLATFORM_SERVICE');
        assert.match(error.message, /^windows:.*pending real-machine implementation/u);
        return true;
      },
    );

    // 2. bootoutService rejects with UNSUPPORTED_PLATFORM_SERVICE
    assert.throws(
      () => bootoutService(paths, undefined, controlSpies),
      (error) => {
        assert.equal(error.code, 'UNSUPPORTED_PLATFORM_SERVICE');
        assert.match(error.message, /^windows:.*pending real-machine implementation/u);
        return true;
      },
    );

    assert.throws(
      () => bootoutService(paths),
      (error) => {
        assert.equal(error.code, 'UNSUPPORTED_PLATFORM_SERVICE');
        assert.match(error.message, /^windows:.*pending real-machine implementation/u);
        return true;
      },
    );

    // 3. installServiceDefinition rejects with UNSUPPORTED_PLATFORM_SERVICE
    assert.throws(
      () => installServiceDefinition(paths, controlSpies),
      (error) => {
        assert.equal(error.code, 'UNSUPPORTED_PLATFORM_SERVICE');
        assert.match(error.message, /^windows:.*pending real-machine implementation/u);
        return true;
      },
    );

    // 4. activateService rejects with UNSUPPORTED_PLATFORM_SERVICE
    await assert.rejects(
      async () => activateService(paths, { path: 'dummy' }, controlSpies),
      (error) => {
        assert.equal(error.code, 'UNSUPPORTED_PLATFORM_SERVICE');
        assert.match(error.message, /^windows:.*pending real-machine implementation/u);
        return true;
      },
    );

    // 5. serviceRegistrationStatus returns the unavailable view isomorphic to service-linux.mjs
    const status = serviceRegistrationStatus(undefined, controlSpies);
    assert.deepEqual(status, {
      query: 'unavailable',
      registered: null,
      reason: 'windows: service integration pending real-machine implementation',
    });

    const statusDefault = serviceRegistrationStatus();
    assert.deepEqual(statusDefault, {
      query: 'unavailable',
      registered: null,
      reason: 'windows: service integration pending real-machine implementation',
    });

    // Verification: none of getuid, launchctl, or systemctl was ever called
    assert.equal(getuidCalled, false, 'process.getuid was unexpectedly called');
    assert.equal(launchctlCalled, false, 'launchctl was unexpectedly called');
    assert.equal(systemctlCalled, false, 'systemctl was unexpectedly called');
  } finally {
    if (previousPlatform === undefined) {
      delete process.env.EXTERNAL_SUBAGENT_TEST_PLATFORM;
    } else {
      process.env.EXTERNAL_SUBAGENT_TEST_PLATFORM = previousPlatform;
    }
    process.getuid = originalGetuid;
  }
});
