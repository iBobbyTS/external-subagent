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

function run(home, args, extraEnv = {}) {
  return spawnSync(process.execPath, [cli, ...args], {
    encoding: 'utf8',
    env: {
      ...process.env,
      HOME: home,
      // Windows product paths read %LOCALAPPDATA% from the environment; keep
      // every product write inside this fixture home.
      LOCALAPPDATA: path.join(home, 'AppData', 'Local'),
      EXTERNAL_SUBAGENT_TEST_PLATFORM: 'win32',
      ...extraEnv,
    },
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

test('every business command passes the win32 platform gate and reaches its structured failure', () => {
  // The win32 path stage opens the gate: no command may answer UNSUPPORTED_PLATFORM anymore.
  // Daemon-backed commands answer with a structured connection error; purge
  // stops at its own confirmation guard. SCM lifecycle commands are covered by
  // the injectable sc.exe seam in the dispatch test below, never by this test
  // (that keeps an elevated CI runner from mutating the real SCM database).
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-win-commands-'));
  const repository = path.join(home, 'repo');
  fs.mkdirSync(repository, { recursive: true });
  const agentId = 10_000_000;

  const rejected = (args, code, env = {}) => {
    const result = run(home, args, env);
    assert.equal(result.status, 1, `${args.join(' ')}: ${result.stderr}`);
    const error = JSON.parse(result.stderr).error;
    assert.notEqual(error.code, 'UNSUPPORTED_PLATFORM', `${args[0]} must pass the platform gate`);
    assert.equal(error.code, code, `${args.join(' ')}: ${error.message}`);
  };

  // Side-effect-free commands must run their real win32 path.
  let result = run(home, ['init', '--dry-run']);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(JSON.parse(result.stdout).plan[0].id, 'verify-payload');

  result = run(home, ['status']);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(JSON.parse(result.stdout).daemon_error.code, 'SOCKET_UNAVAILABLE');

  result = run(home, ['diagnose']);
  assert.equal(result.status, 0, result.stderr);

  result = run(home, ['backup', '--output', path.join(home, 'backup')]);
  assert.equal(result.status, 0, result.stderr);
  result = run(home, ['restore', '--input', path.join(home, 'backup')]);
  assert.equal(result.status, 0, result.stderr);

  // purge passes the platform gate and fails on its own confirmation guard
  // before any service or data mutation.
  rejected(['purge'], 'CONFIRMATION_REQUIRED');

  // SCM-backed commands (start/stop/uninstall/purge) are exercised through the
  // injectable `sc.exe` seam in the dispatch test below and in the RPC decoupling stage
  // service-windows suite: running them here would mutate a real SCM database
  // on an elevated CI runner. The platform gate itself is shared and covered
  // by every command above and below.

  // Daemon-backed commands answer with the structured connection error.
  const daemonCommands = [
    ['create', '--json', JSON.stringify({ repository, prompt: 'no daemon' })],
    ['wait', '--json', JSON.stringify({ agent_id: agentId, wait_time: 0 })],
    ['list', '--json', JSON.stringify({ repository })],
    ['send', '--json', JSON.stringify({ agent_id: agentId, mode: 'queue', content: 'no daemon' })],
    ['respond', '--json', JSON.stringify({ agent_id: agentId, request_id: 'req-1', decision: 'allow' })],
    ['cancel', '--json', JSON.stringify({ agent_id: agentId })],
    ['result', '--json', JSON.stringify({ agent_id: agentId })],
    ['close', '--json', JSON.stringify({ agent_id: agentId })],
  ];
  for (const args of daemonCommands) rejected(args, 'SOCKET_UNAVAILABLE');

  // The probe command layer reaches the daemon wire method (agent-probe)
  // instead of being rejected before connecting.
  rejected(['subagents', 'probe', 'codex', '--hi'], 'SOCKET_UNAVAILABLE');
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

test('win32 service dispatch routes to the real SCM backend through the injectable sc.exe seam', async () => {
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

  const daemonPath = process.execPath; // a real, space-bearing Windows executable
  const state = { registered: false, running: false, pid: 0 };
  const calls = [];
  const query = () => (!state.registered
    ? { status: 1060, stdout: '' }
    : {
      status: 0,
      stdout: [
        'SERVICE_NAME: external-subagentd',
        `        STATE              : ${state.running ? 4 : 1}  ${state.running ? 'RUNNING' : 'STOPPED'}`,
        `        PID                : ${state.pid}`,
        '',
      ].join('\r\n'),
    });
  const sc = (args) => {
    calls.push(args);
    switch (args[0]) {
      case 'queryex': return query();
      case 'create': state.registered = true; state.binPath = args[3]; return { status: 0, stdout: '' };
      case 'start': state.running = true; state.pid += 1; return { status: 0, stdout: '' };
      case 'stop': state.running = false; return { status: 0, stdout: '' };
      case 'delete': state.registered = false; state.running = false; return { status: 0, stdout: '' };
      case 'qc': return state.registered
        ? { status: 0, stdout: `        BINARY_PATH_NAME   : ${state.binPath}\r\n` }
        : { status: 1060, stdout: '' };
      default: throw new Error(`unexpected sc.exe call: ${args.join(' ')}`);
    }
  };
  const options = { sc, daemonPath, ...controlSpies };
  const paths = productPaths('/home/winuser', { EXTERNAL_SUBAGENT_TEST_PLATFORM: 'win32' });

  try {
    // 1. installServiceDefinition renders the SCM definition without errors.
    const definition = installServiceDefinition(paths, options);
    assert.equal(definition.backend, 'scm');
    assert.equal(definition.service, 'external-subagentd');
    assert.match(definition.bin_path, /--database "[^"]*" --socket "[^"]*"/u);
    assert.equal(calls.length, 0, 'rendering the definition does not mutate SCM');

    // 2. bootstrapService creates and starts the real SCM definition.
    const boot = bootstrapService(paths, undefined, options);
    assert.equal(boot.action, 'bootstrap');
    assert.equal(boot.created, true);
    assert.equal(boot.started, true);
    const create = calls.find((call) => call[0] === 'create');
    assert.ok(create, 'sc.exe create was issued');
    assert.deepEqual(create.slice(0, 3), ['create', 'external-subagentd', 'binPath=']);
    assert.equal(create[4], 'start=');
    assert.equal(create[5], 'auto');

    // 3. Read-only status parses the SCM registration.
    const status = serviceRegistrationStatus(undefined, options);
    assert.equal(status.query, 'available');
    assert.equal(status.registered, true);
    assert.equal(status.state, 'running');

    // 4. bootoutService stops and deletes it.
    const stopped = bootoutService(paths, undefined, { ...options, unloadTimeoutMs: 1000 });
    assert.equal(stopped.removed, true);

    // 5. activateService reaches the real SCM activation contract: with no
    //    registration left it reports SERVICE_NOT_INSTALLED, never
    //    UNSUPPORTED_PLATFORM_SERVICE.
    await assert.rejects(
      async () => activateService(paths, { path: 'dummy', sha256: 'dummy' }, options),
      (error) => error.code === 'SERVICE_NOT_INSTALLED',
    );

    // Verification: none of getuid, launchctl, or systemctl was ever called.
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
