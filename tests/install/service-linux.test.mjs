// S03 systemd user-service oracles (seam-based; the real-user-manager smoke
// is recorded separately in the section evidence).  The backend under test is
// the REAL cli/install/service-linux.mjs plus the platform dispatch in
// service-macos.mjs; only `systemctl` is a double (the structural twin of the
// launchctl doubles in tests/cli/maintenance.test.mjs) or neutralized through
// the EXTERNAL_SUBAGENT_TEST_NO_SYSTEMCTL seam.  The oracles mirror the launchd
// ones one-to-one: idempotence (already loaded / already stopped), the lost
// bootstrap race, the bounded inactive confirmation with its SERVICE_UNLOAD_TIMEOUT
// refusal, the stop racing an external removal, the unit's one-to-one
// parameterization (arguments, environment forwarding, fixed PATH,
// Restart=always, 0600), the no-user-session failure with its linger hint,
// and the activation health/rollback contract over a faithful systemctl
// stand-in that spawns the unit's real ExecStart.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawn } from 'node:child_process';
import { CliError } from '../../cli/errors.mjs';
import { productPaths, platform } from '../../cli/paths.mjs';
import { SYSTEMD_FIXED_PATH, pathReport, systemdServicePath, which } from '../../cli/install/path.mjs';
import { runInit } from '../../cli/install/init.mjs';
import { activateService } from '../../cli/install/service-activation.mjs';
import {
  SYSTEMD_UNIT_NAME, SYSTEMCTL_PATH, bootoutServiceSystemd, bootstrapServiceSystemd, execStartProgram,
  hasUserSystemdSession, installServiceUnit, systemctl, systemdServiceRegistrationStatus, systemdUnit,
  replaceExecStartProgram,
} from '../../cli/install/service-linux.mjs';
import { bootstrapService, bootoutService, installServiceDefinition, serviceRegistrationStatus } from '../../cli/install/service-macos.mjs';

// The Linux layout is pinned through the documented platform seam so every
// assertion below runs on any host; only the dispatch-level tests (runInit,
// activation) are additionally gated to a real Linux host.
const LINUX = { EXTERNAL_SUBAGENT_TEST_PLATFORM: 'linux' };
const linuxHost = platform() === 'linux';
const sha256 = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');
const digest = (file) => sha256(fs.readFileSync(file));

function servicePaths(prefix = 'external-subagent-systemd-') {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  return { home, paths: productPaths(home, LINUX) };
}

// Stateful systemctl double: `show` reports one tracked unit, enable/disable
// transition it, so the oracles observe exactly what the user manager would.
function recordingSystemd({ loaded = false, active = false, pid = 999 } = {}) {
  const calls = [];
  const state = { loadState: loaded ? 'loaded' : 'not-found', activeState: active ? 'active' : 'inactive', subState: active ? 'running' : 'dead', pid: active ? pid : 0 };
  return {
    calls,
    state,
    control(args) {
      calls.push(args.join(' '));
      if (args[0] === 'show') {
        if (state.loadState === 'not-found') return { action: 'show', status: 0, stdout: 'LoadState=not-found\nActiveState=inactive\nSubState=dead\nMainPID=0\n' };
        return { action: 'show', status: 0, stdout: `LoadState=${state.loadState}\nActiveState=${state.activeState}\nSubState=${state.subState}\nMainPID=${state.pid}\n` };
      }
      if (args[0] === 'daemon-reload') return { action: 'daemon-reload', status: 0 };
      if (args[0] === 'enable') {
        state.loadState = 'loaded'; state.activeState = 'active'; state.subState = 'running'; state.pid = pid;
        return { action: 'enable', status: 0 };
      }
      if (args[0] === 'disable') {
        state.activeState = 'inactive'; state.subState = 'dead'; state.pid = 0;
        return { action: 'disable', status: 0 };
      }
      throw new Error(`unexpected systemctl call: ${args.join(' ')}`);
    },
  };
}

// A config the forwarding oracles below can reason about.
function writeAgentConfig(paths, revision = 5) {
  fs.mkdirSync(path.dirname(paths.config), { recursive: true });
  fs.writeFileSync(paths.config, JSON.stringify({
    schema_version: 2,
    revision,
    default_subagent: 'dsh',
    subagents: {
      dsh: { enabled: true, spawn_supported: true, runtime_path: '/opt/dsh/acp', home: '/var/lib/dsh', profile: 'acp', version: '0.1.5' },
      codex: { enabled: false, spawn_supported: false, runtime_path: '/opt/codex-runtime', home: '/home/t/codex-home' },
      agy: { enabled: false, spawn_supported: false, runtime_path: '/opt/agy-runtime' },
    },
  }));
}

test('the systemd unit mirrors the plist parameterization one-to-one', () => {
  const { home, paths } = servicePaths();
  try {
    writeAgentConfig(paths, 5);
    const unit = systemdUnit(paths, { daemonPath: path.join(home, 'payload', 'external-subagentd'), zcodeRuntime: '/definitely/absent/zcode.cjs' }).toString('utf8');
    // Arguments, in the plist's order, with the diagnostic log under XDG state.
    assert.match(unit, new RegExp(`ExecStart="${path.join(home, 'payload', 'external-subagentd')}" "--database" "${paths.database}" "--socket" "${paths.socket}" "--diagnostic-log" "${path.join(paths.logs, 'daemon-error.log')}"`));
    assert.doesNotMatch(unit, /--runtime/u, 'an absent pinned ZCode runtime is never forwarded');
    // Environment forwarding, one-to-one with the plist keys.
    assert.match(unit, new RegExp(`^Environment="PATH=${systemdServicePath().replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}"$`, 'm'));
    assert.match(unit, /^Environment="EXTERNAL_SUBAGENT_CONFIG_REVISION=5"$/m);
    assert.match(unit, /^Environment="DSH_RUNTIME_PATH=\/opt\/dsh\/acp"$/m);
    assert.match(unit, /^Environment="DSH_HOME=\/var\/lib\/dsh"$/m);
    assert.match(unit, /^Environment="DSH_PROFILE=acp"$/m);
    assert.match(unit, /^Environment="DSH_VERSION=0\.1\.5"$/m);
    assert.match(unit, /^Environment="CODEX_RUNTIME_PATH=\/opt\/codex-runtime"$/m);
    assert.match(unit, /^Environment="CODEX_HOME=\/home\/t\/codex-home"$/m);
    assert.match(unit, /^Environment="AGY_RUNTIME_PATH=\/opt\/agy-runtime"$/m);
    // launchd-equivalent lifecycle: KeepAlive -> Restart=always, RunAtLoad ->
    // WantedBy=default.target.
    assert.match(unit, /^Restart=always$/m);
    assert.match(unit, /^WantedBy=default\.target$/m);
    // The fixed PATH is the platform's own, never the homebrew set.
    assert.doesNotMatch(unit, /homebrew/u);

    // A present pinned ZCode runtime is forwarded as the --runtime argument.
    const runtime = path.join(home, 'zcode.cjs');
    fs.writeFileSync(runtime, 'export {};\n');
    const withRuntime = systemdUnit(paths, { daemonPath: path.join(home, 'payload', 'external-subagentd'), zcodeRuntime: runtime }).toString('utf8');
    assert.match(withRuntime, new RegExp(`"--runtime" "${runtime}"`));
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('unit generation escapes spaces, quotes, dollars, and percents in paths', () => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-escape-'));
  try {
    const odd = path.join(base, '安 装目录 $x %h');
    fs.mkdirSync(odd, { recursive: true });
    const paths = {
      config: path.join(base, 'config.json'),
      database: path.join(odd, 'db.sqlite3'),
      socket: path.join(odd, 'daemon.sock'),
      logs: odd,
    };
    const unit = systemdUnit(paths, { daemonPath: path.join(odd, 'external-subagentd'), zcodeRuntime: '/definitely/absent/zcode.cjs' }).toString('utf8');
    assert.match(unit, /ExecStart="[^"]*安 装目录 \$\$x %%h[^"]*external-subagentd" "--database" "[^"]*安 装目录 \$\$x %%h\/db\.sqlite3"/u);
    // The round-trip parser recovers the literal path exactly.
    assert.equal(execStartProgram(Buffer.from(unit, 'utf8')), path.join(odd, 'external-subagentd'));
    assert.equal(
      execStartProgram(replaceExecStartProgram(Buffer.from(unit, 'utf8'), path.join(odd, 'retained', 'external-subagentd'))),
      path.join(odd, 'retained', 'external-subagentd'),
      'a rollback restore repoints the executable and keeps everything else',
    );
  } finally {
    fs.rmSync(base, { recursive: true, force: true });
  }
});

test('environment values keep dollars literal and escape percent-specifiers, quotes, and backslashes', () => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-env-escape-'));
  try {
    // Hostile-but-legal config values exercising every character whose
    // meaning differs between the two systemd grammars.
    const paths = {
      config: path.join(base, 'config.json'),
      database: '/tmp/unused/db.sqlite3',
      socket: '/tmp/unused/daemon.sock',
      logs: '/tmp/unused-logs',
    };
    fs.writeFileSync(paths.config, JSON.stringify({
      schema_version: 2,
      revision: 7,
      default_subagent: 'dsh',
      subagents: {
        dsh: {
          enabled: true, spawn_supported: true,
          runtime_path: '/opt/dsh/$BIN %h',
          home: '/dsh/home with $HOME and %h',
          profile: 'he said "hi" \\ twice',
          version: 'v$1.2%h',
        },
        codex: { enabled: false, spawn_supported: false, runtime_path: null, home: null },
        agy: { enabled: false, spawn_supported: false, runtime_path: null },
      },
    }));
    const lines = systemdUnit(paths, { daemonPath: '/p/external-subagentd', zcodeRuntime: '/absent' })
      .toString('utf8').split('\n');
    const environmentOf = (name) => {
      const line = lines.find((entry) => entry.startsWith(`Environment="${name}=`));
      assert.ok(line, `the unit must forward ${name}`);
      return line;
    };
    // Environment= performs NO variable expansion (a literal '$' has no
    // special meaning, verified against the running user manager), so dollars
    // pass through verbatim — never doubled like ExecStart arguments.
    assert.equal(environmentOf('DSH_RUNTIME_PATH'), 'Environment="DSH_RUNTIME_PATH=/opt/dsh/$BIN %%h"');
    assert.equal(environmentOf('DSH_HOME'), 'Environment="DSH_HOME=/dsh/home with $HOME and %%h"');
    // %-specifiers DO expand in Environment values: %%h is the literal "%h".
    assert.equal(environmentOf('DSH_PROFILE'), 'Environment="DSH_PROFILE=he said \\"hi\\" \\\\ twice"');
    assert.equal(environmentOf('DSH_VERSION'), 'Environment="DSH_VERSION=v$1.2%%h"');
    // The revision value itself is a plain number, untouched.
    assert.equal(environmentOf('EXTERNAL_SUBAGENT_CONFIG_REVISION'), 'Environment="EXTERNAL_SUBAGENT_CONFIG_REVISION=7"');
    // The two grammars side by side: the same '$' stays single in an
    // Environment value and doubles in an ExecStart argument.
    assert.match(lines.find((line) => line.startsWith('ExecStart=')), /"--database" "\/tmp\/unused\/db\.sqlite3"/u);
  } finally {
    fs.rmSync(base, { recursive: true, force: true });
  }
});

test('newline-bearing values are refused loudly instead of injecting unit directives', () => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-newline-'));
  try {
    const paths = {
      config: path.join(base, 'config.json'),
      database: '/tmp/unused/db.sqlite3',
      socket: '/tmp/unused/daemon.sock',
      logs: '/tmp/unused-logs',
    };
    // The reviewer's injection payload: a profile whose continuation lines
    // are real directives that would override the product-fixed Restart.
    fs.writeFileSync(paths.config, JSON.stringify({
      schema_version: 2, revision: 1, default_subagent: 'dsh',
      subagents: {
        dsh: { enabled: true, spawn_supported: true, runtime_path: '/opt/dsh/acp', home: '/dsh/home', profile: 'acp\nRestart=no\n#', version: '0.1.5' },
        codex: { enabled: false, spawn_supported: false, runtime_path: null, home: null },
        agy: { enabled: false, spawn_supported: false, runtime_path: null },
      },
    }));
    assert.throws(
      () => systemdUnit(paths, { daemonPath: '/p/external-subagentd', zcodeRuntime: '/absent' }),
      (error) => error.code === 'SERVICE_DEFINITION_INVALID'
        && error.message.includes('DSH_PROFILE')
        && error.message.includes('newline')
        && error.message.includes('refused rather than written corrupted'),
    );
    // The installer surfaces the same refusal and never writes the unit.
    const unitPath = path.join(base, '.config', 'systemd', 'user', SYSTEMD_UNIT_NAME);
    assert.throws(() => installServiceUnit({ ...paths, launchAgent: unitPath }), (error) => error.code === 'SERVICE_DEFINITION_INVALID');
    assert.equal(fs.existsSync(unitPath), false, 'a refused definition writes no file');
    assert.equal(fs.existsSync(path.dirname(unitPath)), false, 'not even the unit directory appears');

    // A carriage return is the same injection vector.
    fs.writeFileSync(paths.config, JSON.stringify({
      schema_version: 2, revision: 1, default_subagent: null,
      subagents: {
        dsh: { enabled: false, spawn_supported: false, runtime_path: null, home: null, profile: null, version: null },
        codex: { enabled: false, spawn_supported: false, runtime_path: '/opt/codex\rruntime', home: null },
        agy: { enabled: false, spawn_supported: false, runtime_path: null },
      },
    }));
    assert.throws(
      () => systemdUnit(paths, { daemonPath: '/p/external-subagentd', zcodeRuntime: '/absent' }),
      (error) => error.code === 'SERVICE_DEFINITION_INVALID' && error.message.includes('CODEX_RUNTIME_PATH'),
    );

    // The ExecStart argument path refuses newlines the same way (a newline in
    // the pinned daemon or data path cannot be represented either).
    fs.writeFileSync(paths.config, JSON.stringify({
      schema_version: 2, revision: 1, default_subagent: null, subagents: {},
    }));
    assert.throws(
      () => systemdUnit(paths, { daemonPath: '/p/external-\nsubagentd', zcodeRuntime: '/absent' }),
      (error) => error.code === 'SERVICE_DEFINITION_INVALID' && error.message.includes('ExecStart argument'),
    );
  } finally {
    fs.rmSync(base, { recursive: true, force: true });
  }
});

test('an absent config forwards the default revision, never a null one', () => {
  const { home, paths } = servicePaths();
  try {
    // readConfig falls back to the schema default (revision 0), the exact
    // value the plist generator forwards for a config-less install.
    const unit = systemdUnit(paths, { daemonPath: '/p/external-subagentd', zcodeRuntime: '/absent' }).toString('utf8');
    assert.match(unit, /^Environment="EXTERNAL_SUBAGENT_CONFIG_REVISION=0"$/m);
    assert.match(unit, /^Environment="PATH=/m, 'the fixed PATH is forwarded unconditionally');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('installServiceUnit writes the 0600 unit into the XDG config tree', () => {
  const { home, paths } = servicePaths();
  try {
    writeAgentConfig(paths);
    const result = installServiceUnit(paths);
    assert.equal(result.unit, SYSTEMD_UNIT_NAME);
    assert.equal(result.path, paths.launchAgent);
    assert.equal(paths.launchAgent, path.join(home, '.config', 'systemd', 'user', SYSTEMD_UNIT_NAME));
    const stat = fs.statSync(paths.launchAgent);
    assert.equal(stat.mode & 0o777, 0o600, 'the unit file is private to the user');
    assert.match(fs.readFileSync(paths.launchAgent, 'utf8'), /^Restart=always$/m);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('bootstrap reports already-loaded with the real state and never enables a live unit', () => {
  const { home, paths } = servicePaths();
  try {
    const control = recordingSystemd({ loaded: true, active: true, pid: 4242 });
    const started = bootstrapServiceSystemd(paths, process.getuid(), { systemctl: control.control });
    assert.deepEqual(control.calls, [`show ${SYSTEMD_UNIT_NAME}`], 'a live unit must never reach enable');
    assert.equal(started.already_loaded, true);
    assert.equal(started.state, 'active');
    assert.equal(started.pid, 4242);
    const status = systemdServiceRegistrationStatus({ systemctl: control.control });
    assert.equal(status.registered, true);
    assert.equal(status.pid, 4242);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('bootstrap reloads the manager and enables the unit, and resolves a lost race through the probe', () => {
  const { home, paths } = servicePaths();
  try {
    const control = recordingSystemd();
    const started = bootstrapServiceSystemd(paths, process.getuid(), { systemctl: control.control });
    assert.deepEqual(control.calls, [`show ${SYSTEMD_UNIT_NAME}`, 'daemon-reload', `enable --now ${SYSTEMD_UNIT_NAME}`]);
    assert.equal(started.action, 'enable');
    assert.equal(control.state.activeState, 'active');

    // A racing starter wins between the probe and the enable: the settle probe
    // must read it as already loaded instead of surfacing the phantom failure.
    let shows = 0;
    const raced = (args) => {
      if (args[0] === 'show') {
        shows += 1;
        return shows === 1
          ? { action: 'show', status: 0, stdout: 'LoadState=not-found\nActiveState=inactive\nMainPID=0\n' }
          : { action: 'show', status: 0, stdout: 'LoadState=loaded\nActiveState=active\nSubState=running\nMainPID=777\n' };
      }
      if (args[0] === 'daemon-reload') return { action: 'daemon-reload', status: 0 };
      throw new CliError('DAEMON_CONTROL_FAILED', 'Job for external-subagent.service failed');
    };
    const resolved = bootstrapServiceSystemd(paths, process.getuid(), { systemctl: raced });
    assert.equal(resolved.already_loaded, true);
    assert.equal(resolved.pid, 777);

    // Both probes absent and enable still failing is a genuine control failure.
    const failing = (args) => {
      if (args[0] === 'show') return { action: 'show', status: 0, stdout: 'LoadState=not-found\nActiveState=inactive\nMainPID=0\n' };
      if (args[0] === 'daemon-reload') return { action: 'daemon-reload', status: 0 };
      throw new CliError('DAEMON_CONTROL_FAILED', 'Job for external-subagent.service failed');
    };
    assert.throws(() => bootstrapServiceSystemd(paths, process.getuid(), { systemctl: failing }), (error) => error.code === 'DAEMON_CONTROL_FAILED');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('stop confirms the unit is inactive before returning and stays idempotent', () => {
  const { home, paths } = servicePaths();
  try {
    const running = recordingSystemd({ loaded: true, active: true });
    const stopped = bootoutServiceSystemd(paths, process.getuid(), { systemctl: running.control });
    assert.equal(stopped.removed, true);
    assert.equal(stopped.already_stopped, undefined);
    assert.ok(running.calls.filter((call) => call === `show ${SYSTEMD_UNIT_NAME}`).length >= 2, 'removal must be confirmed by a follow-up probe');
    assert.ok(running.calls.includes(`disable --now ${SYSTEMD_UNIT_NAME}`));

    // A unit the manager never loaded: idempotent re-stop, no disable call.
    const absent = recordingSystemd();
    const again = bootoutServiceSystemd(paths, process.getuid(), { systemctl: absent.control });
    assert.equal(again.already_stopped, true);
    assert.equal(absent.calls.some((call) => call.startsWith('disable')), false);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('a stop whose unit stays active fails bounded instead of pretending removal', () => {
  const { home, paths } = servicePaths();
  try {
    const stuck = (args) => {
      if (args[0] === 'show') return { action: 'show', status: 0, stdout: 'LoadState=loaded\nActiveState=active\nSubState=running\nMainPID=999\n' };
      if (args[0] === 'disable') return { action: 'disable', status: 0 };
      throw new Error(`unexpected systemctl call: ${args.join(' ')}`);
    };
    assert.throws(
      () => bootoutServiceSystemd(paths, process.getuid(), { systemctl: stuck, unloadTimeoutMs: 150 }),
      (error) => error.code === 'SERVICE_UNLOAD_TIMEOUT',
    );
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('a stop racing an external removal treats the gone unit as stopped', () => {
  const { home, paths } = servicePaths();
  try {
    let seen = false;
    const racing = (args) => {
      if (args[0] === 'show') {
        if (!seen) { seen = true; return { action: 'show', status: 0, stdout: 'LoadState=loaded\nActiveState=active\nSubState=running\nMainPID=999\n' }; }
        return { action: 'show', status: 0, stdout: 'LoadState=not-found\nActiveState=inactive\nMainPID=0\n' };
      }
      if (args[0] === 'disable') throw new CliError('DAEMON_CONTROL_FAILED', 'Unit external-subagent.service not loaded.');
      throw new Error(`unexpected systemctl call: ${args.join(' ')}`);
    };
    const result = bootoutServiceSystemd(paths, process.getuid(), { systemctl: racing });
    assert.equal(result.already_stopped, true);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('the status view reports a failed unit as failed, never healthy', () => {
  const failed = (args) => {
    assert.equal(args[0], 'show');
    return { action: 'show', status: 0, stdout: 'LoadState=loaded\nActiveState=failed\nSubState=dead\nMainPID=0\n' };
  };
  const status = systemdServiceRegistrationStatus({ systemctl: failed });
  assert.equal(status.registered, true);
  assert.equal(status.state, 'failed', 'a crashed unit surfaces its real state');
  assert.equal(status.pid, null);
});

test('the systemd backend stays neutralized under its test seam', () => {
  const previous = process.env.EXTERNAL_SUBAGENT_TEST_NO_SYSTEMCTL;
  process.env.EXTERNAL_SUBAGENT_TEST_NO_SYSTEMCTL = '1';
  try {
    const status = systemdServiceRegistrationStatus();
    assert.equal(status.registered, null);
    assert.equal(status.query, 'skipped');
    const { paths } = servicePaths();
    const started = bootstrapServiceSystemd(paths, process.getuid());
    assert.equal(started.skipped, true);
    const stopped = bootoutServiceSystemd(paths, process.getuid());
    assert.equal(stopped.already_stopped, true, 'a neutralized stop reads as not-registered, like the launchd seam');
  } finally {
    if (previous === undefined) delete process.env.EXTERNAL_SUBAGENT_TEST_NO_SYSTEMCTL;
    else process.env.EXTERNAL_SUBAGENT_TEST_NO_SYSTEMCTL = previous;
  }
});

test('a missing user session fails control with the linger hint and degrades the read-only view', () => {
  const saved = { XDG_RUNTIME_DIR: process.env.XDG_RUNTIME_DIR, DBUS_SESSION_BUS_ADDRESS: process.env.DBUS_SESSION_BUS_ADDRESS, EXTERNAL_SUBAGENT_TEST_NO_SYSTEMCTL: process.env.EXTERNAL_SUBAGENT_TEST_NO_SYSTEMCTL };
  delete process.env.XDG_RUNTIME_DIR;
  delete process.env.DBUS_SESSION_BUS_ADDRESS;
  delete process.env.EXTERNAL_SUBAGENT_TEST_NO_SYSTEMCTL;
  try {
    assert.equal(hasUserSystemdSession(), false);
    // Deterministic: the pre-check fails before any process is spawned.
    assert.throws(() => systemctl(['show', SYSTEMD_UNIT_NAME]), (error) => {
      assert.equal(error.code, 'NO_USER_SYSTEMD_SESSION');
      assert.match(error.message, /XDG_RUNTIME_DIR/u);
      assert.match(error.message, /loginctl enable-linger/u);
      assert.match(error.message, /never enables linger/u);
      return true;
    });
    // The read-only status view degrades into an explicit unavailable answer
    // instead of failing the whole command.
    const view = systemdServiceRegistrationStatus();
    assert.equal(view.query, 'unavailable');
    assert.equal(view.registered, null);
    assert.match(view.reason, /enable-linger/u);
  } finally {
    for (const [key, value] of Object.entries(saved)) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
  }
});

test('the Linux fixed PATH resolves systemctl and stays platform-trueful', () => {
  assert.equal(SYSTEMD_FIXED_PATH.includes('/opt/homebrew'), false);
  // The service PATH is the fixed system set plus the interpreter directory of
  // the node that rendered the unit, so a Node-launcher Codex runtime resolves
  // `node` from the unit environment (the bounded S04 fix).
  assert.equal(systemdServicePath(), `${SYSTEMD_FIXED_PATH}:${path.dirname(process.execPath)}`);
  assert.equal(systemdServicePath().startsWith(`${SYSTEMD_FIXED_PATH}:`), true);
  // The fixed set must actually find systemctl, the tool the service itself
  // depends on (verified on a real Linux host).
  if (linuxHost) {
    assert.equal(fs.existsSync(SYSTEMCTL_PATH), true, 'the pinned systemctl path must exist on a supported Linux host');
    assert.equal(which('systemctl', { PATH: SYSTEMD_FIXED_PATH }), SYSTEMCTL_PATH);
    assert.equal(which('systemctl', { PATH: systemdServicePath() }), SYSTEMCTL_PATH);
  }
  const report = pathReport({ env: { ...LINUX, PATH: '' } });
  assert.equal(report.systemd.path, systemdServicePath());
  assert.equal(report.systemd.path.startsWith(`${SYSTEMD_FIXED_PATH}:`), true);
  assert.equal(report.launchd, undefined, 'a Linux report never claims the launchd key');
  const darwinReport = pathReport({ env: { EXTERNAL_SUBAGENT_TEST_PLATFORM: 'darwin', PATH: '' } });
  assert.equal(darwinReport.launchd.path, '/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin');
  assert.equal(darwinReport.systemd, undefined);
});

test('the platform dispatch routes init, start, stop, status, and install to the systemd backend', { skip: !linuxHost }, () => {
  const previous = process.env.EXTERNAL_SUBAGENT_TEST_PLATFORM;
  process.env.EXTERNAL_SUBAGENT_TEST_PLATFORM = 'linux';
  try {
    const { home, paths } = servicePaths();
    const control = recordingSystemd();
    try {
      writeAgentConfig(paths);
      // The payload verifies from the repo's staged linux-x64 tree, so the
      // injected failure lands at the final baseline-publication step — the
      // same oracle shape as the launchd init-rollback tests. The failed init
      // rolls its own service work back: the enable happened, the rollback
      // disabled it again, the unit file is gone, and the pre-existing config
      // is restored byte-for-byte rather than deleted.
      const configBefore = fs.readFileSync(paths.config);
      assert.throws(
        () => runInit({ paths, systemctl: control.control, _failStep: 'publish-active-payload' }),
        /injected failure at publish-active-payload/u,
      );
      assert.ok(control.calls.includes(`enable --now ${SYSTEMD_UNIT_NAME}`), 'init enables and starts the unit');
      assert.ok(control.calls.includes(`disable --now ${SYSTEMD_UNIT_NAME}`), 'rollback disables the service init enabled');
      assert.equal(fs.existsSync(paths.launchAgent), false, 'the unit rolls back with the service');
      assert.deepEqual(fs.readFileSync(paths.config), configBefore, 'a prior config survives the failed init byte-for-byte');
    } finally {
      fs.rmSync(home, { recursive: true, force: true });
    }

    const dispatchHome = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-systemd-dispatch-'));
    try {
      const dispatchPaths = productPaths(dispatchHome, LINUX);
      const live = recordingSystemd({ loaded: true, active: true, pid: 31337 });
      assert.equal(serviceRegistrationStatus(process.getuid(), { systemctl: live.control }).pid, 31337);
      assert.equal(bootstrapService(dispatchPaths, process.getuid(), { systemctl: live.control }).already_loaded, true);
      const stopping = recordingSystemd({ loaded: true, active: true });
      assert.equal(bootoutService(dispatchPaths, process.getuid(), { systemctl: stopping.control }).removed, true);
      assert.equal(installServiceDefinition(dispatchPaths).unit, SYSTEMD_UNIT_NAME);
      assert.ok(fs.existsSync(path.join(dispatchHome, '.config', 'systemd', 'user', SYSTEMD_UNIT_NAME)));
    } finally {
      fs.rmSync(dispatchHome, { recursive: true, force: true });
    }
  } finally {
    if (previous === undefined) delete process.env.EXTERNAL_SUBAGENT_TEST_PLATFORM;
    else process.env.EXTERNAL_SUBAGENT_TEST_PLATFORM = previous;
  }
});

// A faithful systemctl stand-in (the only systemctl-shaped thing tests may
// run): show/daemon-reload/enable/disable against the REAL unit file,
// spawning the unit's exact ExecStart like the user manager would.  The daemon
// "binary" is a stub shell process recording its own argv0; the simulated RPC
// reports the identity captured at spawn time, exactly like the real daemon
// binds its artifact hash to the running executable at startup.
function faithfulSystemd(dir, unitPath) {
  const stateFile = path.join(dir, 'systemctl-state.json');
  const write = (doc) => fs.writeFileSync(stateFile, JSON.stringify(doc));
  const read = () => { try { return JSON.parse(fs.readFileSync(stateFile, 'utf8')); } catch { return { pid: null, program: null, sha: null }; } };
  const alive = (pid) => { try { process.kill(pid, 0); return true; } catch { return false; } };
  const spawnCount = { value: 0 };
  const unquote = (text) => {
    let out = '';
    for (let index = 0; index < text.length; index += 1) {
      if (text[index] === '$' && text[index + 1] === '$') { out += '$'; index += 1; continue; }
      if (text[index] === '%' && text[index + 1] === '%') { out += '%'; index += 1; continue; }
      out += text[index];
    }
    return out;
  };
  const execArgv = () => {
    const line = fs.readFileSync(unitPath, 'utf8').match(/^ExecStart=(.*)$/m)[1];
    const argv = [];
    let index = 0;
    while (index < line.length) {
      while (index < line.length && /\s/.test(line[index])) index += 1;
      if (index >= line.length) break;
      if (line[index] === '"') {
        index += 1;
        let arg = '';
        while (index < line.length && line[index] !== '"') {
          if (line[index] === '\\' && index + 1 < line.length) { arg += line[index + 1]; index += 2; continue; }
          arg += line[index];
          index += 1;
        }
        index += 1;
        argv.push(unquote(arg));
      } else {
        const end = line.indexOf(' ', index);
        const token = end === -1 ? line.slice(index) : line.slice(index, end);
        argv.push(token);
        index = end === -1 ? line.length : end;
      }
    }
    return argv;
  };
  const control = (args) => {
    if (args[0] === 'show') {
      const state = read();
      if (!state.pid || !alive(state.pid)) return { action: 'show', status: 0, stdout: 'LoadState=not-found\nActiveState=inactive\nSubState=dead\nMainPID=0\n' };
      return { action: 'show', status: 0, stdout: `LoadState=loaded\nActiveState=active\nSubState=running\nMainPID=${state.pid}\n` };
    }
    if (args[0] === 'daemon-reload') return { action: 'daemon-reload', status: 0 };
    if (args[0] === 'disable') {
      const state = read();
      if (state.pid && alive(state.pid)) {
        process.kill(state.pid, 'SIGTERM');
        const deadline = Date.now() + 10_000;
        while (alive(state.pid) && Date.now() < deadline) Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 50);
      }
      write({ pid: null, program: null, sha: null });
      return { action: 'disable', status: 0 };
    }
    if (args[0] === 'enable') {
      const argv = execArgv();
      const child = spawn(argv[0], argv.slice(1), { stdio: 'ignore' });
      child.unref();
      spawnCount.value += 1;
      write({ pid: child.pid, program: argv[0], sha: digest(argv[0]) });
      return { action: 'enable', status: 0 };
    }
    throw new Error(`unexpected systemctl call: ${args.join(' ')}`);
  };
  const versionFor = (program) => (program.includes('retained-1.0.0') || program.includes('pkg-a') ? '1.0.0' : '2.0.0');
  let generation = 0;
  const callDaemon = async (_socket, command) => {
    assert.ok(command === 'status' || command === 'drain-status' || command === 'activate-ready', `unexpected rpc ${command}`);
    if (command === 'drain-status') return { is_draining: true, ready_for_activation: true };
    if (command === 'activate-ready') return { ready_for_activation: true, activation_claim: 'sim-claim' };
    // Real RPC latency: a process that dies at startup must be observed dead
    // within the same health-verification iteration that spawns it.
    await new Promise((resolve) => setTimeout(resolve, 25));
    const state = read();
    generation += 1;
    return {
      mcp_version: '0.1.0',
      service_generation: `sim-${generation}`,
      identity: { daemon: { version: versionFor(state.program), artifact: { path: state.program, sha256: state.sha } } },
    };
  };
  return { control, callDaemon, read, alive, spawnCount, unitPath };
}

// Mirrors the darwin rollback oracle (tests/upgrade/recovery.test.mjs): npm
// overwrote the old program path, the new payload can never health-verify,
// and the rollback must restore a live OLD service from the retained bytes.
test('a failed activation restores the old systemd service from retained bytes after npm overwrote the old path', { skip: !linuxHost }, async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'systemd-rollback-'));
  try {
    const marker = path.join(dir, 'spawned-argv0.log');
    const pkgA = path.join(dir, 'pkg-a', 'external-subagentd');
    const retainedA = path.join(dir, 'retained-1.0.0', 'external-subagentd');
    const pkgB = path.join(dir, 'pkg-b', 'external-subagentd');
    for (const target of [pkgA, retainedA, pkgB]) fs.mkdirSync(path.dirname(target), { recursive: true });
    const oldBytes = Buffer.from(`#!/bin/sh\necho "$0" >> ${JSON.stringify(marker)}\nexec sleep 300\n`);
    fs.writeFileSync(pkgA, oldBytes, { mode: 0o755 });
    fs.writeFileSync(retainedA, oldBytes, { mode: 0o755 }); // retained copy: same vA bytes
    fs.writeFileSync(pkgB, Buffer.from('#!/bin/sh\nexit 9\n'), { mode: 0o755 }); // new daemon dies instantly
    const shaA = sha256(oldBytes);
    const shaB = digest(pkgB);
    const unitPath = path.join(dir, 'external-subagent.service');
    fs.writeFileSync(unitPath, systemdUnit(
      { config: path.join(dir, 'absent-config.json'), database: '/tmp/unused.db', socket: path.join(dir, 'unused.sock'), logs: dir },
      { daemonPath: pkgA, zcodeRuntime: '/definitely/absent/zcode.cjs' },
    ), { mode: 0o600 });
    assert.equal(execStartProgram(fs.readFileSync(unitPath)), pkgA);
    const paths = { launchAgent: unitPath, config: path.join(dir, 'absent-config.json'), socket: path.join(dir, 'unused.sock'), database: '/tmp/unused.db', logs: dir };
    const service = faithfulSystemd(dir, unitPath);

    // The vA service is running when npm replaces the package directory:
    // the old program path now holds the vB bytes.
    service.control(['enable', '--now', SYSTEMD_UNIT_NAME]);
    const runningPid = service.read().pid;
    assert.ok(runningPid && service.alive(runningPid));
    fs.writeFileSync(pkgA, fs.readFileSync(pkgB), { mode: 0o755 });
    assert.equal(digest(pkgA), shaB, 'the old program path now carries the new payload bytes');

    // The new payload self-reports 2.0.0 while the selected candidate claims
    // 2.0.1 and its process dies at startup; the failure is injected only
    // through the simulated daemon identity, activateService itself is real.
    let activationError = null;
    try {
      await activateService(paths, { path: pkgB, sha256: shaB, version: '2.0.1' }, {
        systemctl: service.control,
        callDaemon: service.callDaemon,
        zcodeRuntime: '/definitely/absent/zcode.cjs',
        healthTimeoutMs: 1_500,
        rollbackPayload: { path: retainedA, sha256: shaA },
      });
      assert.fail('activation of a broken payload must fail');
    } catch (error) {
      assert.equal(error.code, 'SERVICE_HEALTH_FAILED');
      activationError = error;
    }
    const restored = service.read();
    assert.ok(restored.pid && service.alive(restored.pid), 'rollback left a live service');
    assert.equal(restored.program, retainedA, 'the restored service runs the retained vA artifact');
    assert.equal(activationError.rollback.artifact.path, retainedA);
    assert.equal(activationError.rollback.version, '1.0.0');
    assert.equal(digest(pkgA), shaB, 'rollback never rewrote the npm-managed directory');
    assert.equal(execStartProgram(fs.readFileSync(unitPath)), retainedA, 'the unit is restored to the retained executable');
    // The retained stub records its own argv0 once it starts executing;
    // process startup on a loaded machine can take a few seconds, so the
    // proof polls instead of racing the interpreter.
    const deadlineMarker = Date.now() + 15_000;
    let markerLines = [];
    while (Date.now() < deadlineMarker) {
      markerLines = fs.existsSync(marker) ? fs.readFileSync(marker, 'utf8').trim().split('\n').filter(Boolean) : [];
      if (markerLines.includes(retainedA)) break;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    assert.ok(markerLines.includes(retainedA), 'the running stub proves the retained bytes were execed');
    if (restored.pid && service.alive(restored.pid)) process.kill(restored.pid, 'SIGTERM');
    assert.ok(service.spawnCount.value >= 2, 'activation attempted the new payload and rolled back');
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

// Mirrors the darwin environment-regeneration oracle: the unit is derived from
// the CURRENT config, so a stale environment never survives an activation.
test('a successful activation regenerates the systemd unit environment from the current config', { skip: !linuxHost }, async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'systemd-env-regen-'));
  try {
    const oldProgram = path.join(dir, 'pkg-old', 'external-subagentd');
    const candidate = path.join(dir, 'pkg-c', 'external-subagentd');
    for (const target of [oldProgram, candidate]) fs.mkdirSync(path.dirname(target), { recursive: true });
    const stub = () => Buffer.from('#!/bin/sh\nexec sleep 300\n');
    fs.writeFileSync(oldProgram, stub(), { mode: 0o755 });
    fs.writeFileSync(candidate, stub(), { mode: 0o755 });

    const config = {
      schema_version: 2,
      revision: 22,
      default_subagent: null,
      subagents: {
        dsh: { home: '/fresh/dsh-home' },
        codex: { home: '/fresh/codex-home', runtime_path: '/opt/codex-runtime' },
      },
    };
    const configPath = path.join(dir, 'config.json');
    fs.writeFileSync(configPath, JSON.stringify(config));
    const unitPath = path.join(dir, 'external-subagent.service');
    fs.writeFileSync(unitPath, [
      '[Unit]', 'Description=stale', '', '[Service]', 'Type=simple',
      `ExecStart="${oldProgram}" "--database" "/tmp/unused.db"`,
      'Environment="PATH=/usr/bin:/bin"',
      'Environment="EXTERNAL_SUBAGENT_CONFIG_REVISION=21"',
      'Environment="CODEX_HOME=/home/gone/.codex-multi-2"',
      'Environment="USER_ADDED_KEY=hand-edit-that-regeneration-must-drop"',
      'Restart=always', '', '[Install]', 'WantedBy=default.target', '',
    ].join('\n'), { mode: 0o600 });
    const paths = { launchAgent: unitPath, config: configPath, socket: path.join(dir, 'unused.sock'), database: '/tmp/unused.db', logs: dir };
    const service = faithfulSystemd(dir, unitPath);

    const activated = await activateService(paths, { path: candidate, sha256: digest(candidate), version: '2.0.0' }, {
      systemctl: service.control,
      callDaemon: service.callDaemon,
      zcodeRuntime: '/definitely/absent/zcode.cjs',
    });
    assert.ok(activated.service_generation, 'the candidate health-verified');
    const text = fs.readFileSync(unitPath, 'utf8');
    assert.match(text, new RegExp(`ExecStart="${candidate}"`));
    assert.match(text, /Environment="EXTERNAL_SUBAGENT_CONFIG_REVISION=22"/);
    assert.match(text, /Environment="CODEX_HOME=\/fresh\/codex-home"/);
    assert.match(text, /Environment="CODEX_RUNTIME_PATH=\/opt\/codex-runtime"/);
    assert.match(text, /Environment="DSH_HOME=\/fresh\/dsh-home"/);
    assert.match(text, new RegExp(`Environment="PATH=${systemdServicePath().replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}"`));
    assert.doesNotMatch(text, /codex-multi-2/);
    assert.doesNotMatch(text, /USER_ADDED_KEY/);
    assert.doesNotMatch(text, /="21"/);
    assert.doesNotMatch(text, new RegExp(oldProgram.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));
    const running = service.read();
    if (running.pid && service.alive(running.pid)) process.kill(running.pid, 'SIGTERM');
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
