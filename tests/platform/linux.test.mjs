import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { ZCODE_RUNTIME } from '../../cli/constants.mjs';
import { productPaths, profilesDir } from '../../cli/paths.mjs';

// Linux mirrors the macOS platform suite: the same dry-run oracle, the XDG
// layout contract (D-S02), and the zero-write failure path of an
// uninitialized install.  EXTERNAL_SUBAGENT_TEST_PLATFORM stays the process
// platform override seam, so every assertion here is host-independent.
const cli = path.resolve('bin/external-subagent.mjs');
const LINUX = Object.freeze({ EXTERNAL_SUBAGENT_TEST_PLATFORM: 'linux' });

function run(home, args, extraEnv = {}) {
  return spawnSync(process.execPath, [cli, ...args], {
    encoding: 'utf8',
    env: { HOME: home, ...LINUX, ...extraEnv },
  });
}

test('Linux help and version work without creating anything', () => {
  for (const args of [['--help'], ['version']]) {
    const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-linux-basic-'));
    const result = run(home, args);
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(fs.readdirSync(home), []);
  }
});

test('Linux dry-run remains PATH-independent, host-neutral, and side-effect free', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-linux-'));
  const result = run(home, ['init', '--dry-run'], { PATH: '' });
  assert.equal(result.status, 0, result.stderr);
  const { plan } = JSON.parse(result.stdout);
  assert.equal(plan[0].id, 'verify-payload');
  // AUD-005/D1: the standalone init plan probes no fixed runtime, installs no
  // host plugin, and claims no codex home — the only absolute runtime path it
  // may mention is the payload it ships itself.
  assert.equal(plan.some((step) => step.path === ZCODE_RUNTIME), false, 'no step probes the fixed ZCode runtime');
  assert.equal(plan.some((step) => ['probe-runtime', 'install-codex-plugin', 'claim-codex-home'].includes(step.id)), false, 'the retired implicit host steps stay out of the plan');
  assert.deepEqual(fs.readdirSync(home), []);
});

test('Linux XDG layout follows the specification and keeps the macOS tree apart', () => {
  const home = '/tmp/es-linux-home';
  const layout = productPaths(home, LINUX);
  const data = path.join(home, '.local', 'share', 'external-subagent');
  assert.equal(layout.data, data);
  assert.equal(layout.config, path.join(data, 'config.json'));
  assert.equal(layout.state, path.join(data, 'install-state.json'));
  assert.equal(layout.hookProvenance, path.join(data, 'zcode-agent-hook-provenance.json'));
  assert.equal(layout.database, path.join(data, 'external-subagent.sqlite3'));
  assert.equal(layout.socket, path.join(data, 'external-subagent.sock'));
  assert.equal(layout.profiles, path.join(data, 'profiles'));
  assert.equal(layout.zcodePlugin, path.join(data, 'zcode-plugin', 'external-subagent'));
  assert.equal(layout.logs, path.join(home, '.local', 'state', 'external-subagent'));
  // The systemd user unit directory is provided for the S03 service backend.
  assert.equal(layout.launchAgent, path.join(home, '.config', 'systemd', 'user', 'external-subagent.service'));

  // Absolute XDG variables win; empty and relative values fall back per spec.
  const xdg = productPaths(home, { ...LINUX, XDG_DATA_HOME: '/data', XDG_STATE_HOME: '/state', XDG_CONFIG_HOME: '/conf' });
  assert.equal(xdg.data, path.join('/data', 'external-subagent'));
  assert.equal(xdg.logs, path.join('/state', 'external-subagent'));
  assert.equal(xdg.launchAgent, path.join('/conf', 'systemd', 'user', 'external-subagent.service'));
  const fallback = productPaths(home, { ...LINUX, XDG_DATA_HOME: 'relative/data', XDG_STATE_HOME: '' });
  assert.equal(fallback.data, data);
  assert.equal(fallback.logs, path.join(home, '.local', 'state', 'external-subagent'));

  // macOS keeps the frozen byte paths and never shares the XDG tree; the
  // zcode config path is identical on both platforms.
  const darwin = productPaths(home, { EXTERNAL_SUBAGENT_TEST_PLATFORM: 'darwin' });
  assert.equal(darwin.data, path.join(home, 'Library', 'Application Support', 'external-subagent'));
  assert.equal(darwin.logs, path.join(home, 'Library', 'Logs', 'external-subagent'));
  assert.equal(darwin.launchAgent, path.join(home, 'Library', 'LaunchAgents', 'com.external-subagent.daemon.plist'));
  assert.equal(darwin.zcodeConfig, layout.zcodeConfig);
  assert.equal(layout.zcodeConfig, path.join(home, '.zcode', 'cli', 'config.json'));
});

test('CLI profiles resolve the XDG data directory the daemon also falls back to', () => {
  const home = '/tmp/es-linux-home';
  const expected = path.join(home, '.local', 'share', 'external-subagent', 'profiles');
  assert.equal(profilesDir(LINUX, home), expected);
  // An exported config path still wins over the data fallback (env sibling).
  assert.equal(profilesDir({ ...LINUX, EXTERNAL_SUBAGENT_CONFIG: '/etc/es/agents.json' }, home), '/etc/es/profiles');
});

test('Linux profile list reports the XDG directory without writing HOME', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-linux-profiles-'));
  const result = run(home, ['profile', 'list']);
  assert.equal(result.status, 0, result.stderr);
  const parsed = JSON.parse(result.stdout);
  assert.equal(parsed.directory, path.join(home, '.local', 'share', 'external-subagent', 'profiles'));
  assert.deepEqual(parsed.profiles, []);
  assert.deepEqual(fs.readdirSync(home), [], 'profile list must not create the profiles directory');
});

test('Linux status reports the real systemd service view without probing launchd', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-linux-status-'));
  // No launchd test seam: the real Linux path must not invoke /bin/launchctl.
  // The systemd view is the REAL `systemctl --user show` probe (S03): an
  // uninitialized install reports the unit as absent, and a host without a
  // reachable user manager degrades to an explicit unavailable view with the
  // linger hint instead of failing the read-only command.
  const result = run(home, ['status']);
  assert.equal(result.status, 0, result.stderr);
  const parsed = JSON.parse(result.stdout);
  assert.equal(parsed.ok, true);
  assert.equal(parsed.service.skipped, undefined, 'the transitional skipped view is gone');
  if (parsed.service.query === 'unavailable') {
    assert.equal(parsed.service.registered, null);
    assert.match(parsed.service.reason, /enable-linger/u, 'a missing user session carries the linger hint');
  } else {
    assert.equal(parsed.service.registered, false, 'an uninitialized install has no loaded unit');
  }
  assert.equal(parsed.service_definition, false, 'an uninitialized install has no service definition');
  assert.equal(parsed.daemon_status, null);
  assert.ok(parsed.daemon_error, 'the missing daemon is reported as a daemon_error');
  assert.deepEqual(fs.readdirSync(home), [], 'status must not write HOME state');
});

test('the darwin platform seam still reports the launchd service view', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-darwin-status-'));
  const result = run(home, ['status'], { EXTERNAL_SUBAGENT_TEST_PLATFORM: 'darwin', EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL: '1' });
  assert.equal(result.status, 0, result.stderr);
  const parsed = JSON.parse(result.stdout);
  // darwin keeps the launchd field and probing path; the neutralized seam
  // yields a skipped query rather than a fake registered/absent answer.
  assert.equal(parsed.launch_agent, false);
  assert.equal(Object.hasOwn(parsed, 'service_definition'), false);
  assert.equal(parsed.service.query, 'skipped');
  assert.equal(parsed.service.registered, null);
});

test('uninitialized Linux business commands fail without writing HOME', () => {
  const invocations = [
    ['spawn', '--json', JSON.stringify({ repository: '/tmp/es-linux-repo', prompt: 'no daemon' })],
    ['list', '--json', JSON.stringify({ repository: '/tmp/es-linux-repo' })],
    ['wait', '--json', JSON.stringify({ agent_id: 1, wait_time: 1 })],
  ];
  for (const args of invocations) {
    const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-linux-noinit-'));
    const result = run(home, args);
    // The command passes the platform gate; whatever failure follows (a missing
    // daemon) is not an UNSUPPORTED_PLATFORM rejection.
    if (result.status !== 0) {
      assert.notEqual(JSON.parse(result.stderr).error.code, 'UNSUPPORTED_PLATFORM', `${args[0]} must pass the platform gate`);
    }
    assert.deepEqual(fs.readdirSync(home), [], `${args[0]} must not write HOME state`);
  }
});
