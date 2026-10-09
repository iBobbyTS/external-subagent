// S05 Linux-first oracles for the remaining subagents (zcode/dsh/agy) and the
// zcode runtime discovery (cli/constants.mjs).  The fail-closed negatives come
// FIRST: without a runtime, or before enable, every Linux admission must
// refuse exactly the way the macOS contract does, and no configuration may be
// written by a failed enable.  The discovery oracles pin the precedence the
// product promises — explicit ZCODE_RUNTIME_PATH, then the platform's
// conventional installation (the ZCode server runtime under ~/.zcode/server,
// OBSERVED 2026-10-09), then the packaged macOS pin reported honestly as
// absent — plus the systemd unit/runtime-observation forwarding that consumes
// it.  The real daemon behind the negatives is the debug payload
// (target/debug/external-subagentd) under a throwaway HOME with a PATH that
// holds none of the subagent runtimes.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { ZCODE_RUNTIME, zcodeRuntimePath } from '../../cli/constants.mjs';
import { productPaths } from '../../cli/paths.mjs';
import { runtimeObservations } from '../../cli/install/service-macos.mjs';
import { systemdUnit } from '../../cli/install/service-linux.mjs';
import { runInit } from '../../cli/install/init.mjs';
import { subagentsCommand, parseSubagentsArgs } from '../../cli/commands/agents.mjs';
import { callDaemon } from '../../cli/rpc.mjs';

const LINUX_SEAM = { EXTERNAL_SUBAGENT_TEST_PLATFORM: 'linux' };

function fixtureHome(prefix = 'external-subagent-s05-') {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

// The zcode server runtime location the Linux deployment owns, relative to a
// product home (the official zcode-agent launcher resolves the same default).
function serverRuntime(home) {
  return path.join(home, '.zcode', 'server', 'agents', 'glm', 'zcode.cjs');
}

test('zcode runtime discovery: explicit configuration, then the platform convention, then the packaged pin', () => {
  const home = fixtureHome();
  try {
    // Explicit configuration wins on every platform, reported verbatim
    // (presence stays the caller's existsSync observation).
    assert.equal(zcodeRuntimePath(home, { ...LINUX_SEAM, ZCODE_RUNTIME_PATH: '/explicit/zcode.cjs' }), '/explicit/zcode.cjs');
    assert.equal(zcodeRuntimePath(home, { EXTERNAL_SUBAGENT_TEST_PLATFORM: 'darwin', ZCODE_RUNTIME_PATH: '/explicit/zcode.cjs' }), '/explicit/zcode.cjs');
    // darwin keeps the frozen app-bundle bytes.
    assert.equal(zcodeRuntimePath(home, { EXTERNAL_SUBAGENT_TEST_PLATFORM: 'darwin' }), ZCODE_RUNTIME);
    // Linux without a conventional installation reports the packaged pin —
    // unavailable, never invented.
    assert.equal(zcodeRuntimePath(home, LINUX_SEAM), ZCODE_RUNTIME);
    // A present conventional installation is discovered under the product home.
    fs.mkdirSync(path.dirname(serverRuntime(home)), { recursive: true });
    fs.writeFileSync(serverRuntime(home), 'export {};\n');
    assert.equal(zcodeRuntimePath(home, LINUX_SEAM), serverRuntime(home));
    // The deployment's ZCODE_SERVER_RUNTIME_ROOT is deliberately not
    // consulted: the conventional location follows the product home only.
    assert.equal(zcodeRuntimePath(home, { ...LINUX_SEAM, ZCODE_SERVER_RUNTIME_ROOT: '/elsewhere' }), serverRuntime(home));
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('the systemd unit and the runtime report forward the discovered Linux runtime, or nothing', () => {
  const home = fixtureHome();
  try {
    const paths = productPaths(home, LINUX_SEAM);
    fs.mkdirSync(path.dirname(paths.config), { recursive: true });
    fs.writeFileSync(paths.config, JSON.stringify({ schema_version: 2, revision: 1, subagents: {} }));
    // The seam selects the linux arm of the discovery while this file may run
    // on any host; restored immediately so later tests see the real platform.
    const previous = process.env.EXTERNAL_SUBAGENT_TEST_PLATFORM;
    process.env.EXTERNAL_SUBAGENT_TEST_PLATFORM = 'linux';
    try {
      // No conventional installation: no --runtime argument is forwarded and
      // the report carries the absent packaged pin.
      const absent = systemdUnit(paths, { daemonPath: path.join(home, 'external-subagentd') }).toString('utf8');
      assert.doesNotMatch(absent, /--runtime/u, 'an absent conventional runtime is never forwarded');
      assert.equal(runtimeObservations(paths).zcode.path, ZCODE_RUNTIME);
      assert.equal(runtimeObservations(paths).zcode.present, false);

      // A conventional installation under the product home is forwarded as
      // the daemon --runtime argument and reported present.
      fs.mkdirSync(path.dirname(serverRuntime(home)), { recursive: true });
      fs.writeFileSync(serverRuntime(home), 'export {};\n');
      const present = systemdUnit(paths, { daemonPath: path.join(home, 'external-subagentd') }).toString('utf8');
      const escaped = serverRuntime(home).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
      assert.match(present, new RegExp(`^ExecStart=.*"--runtime" "${escaped}"`, 'm'));
      assert.equal(runtimeObservations(paths).zcode.path, serverRuntime(home));
      assert.equal(runtimeObservations(paths).zcode.present, true);
    } finally {
      if (previous === undefined) delete process.env.EXTERNAL_SUBAGENT_TEST_PLATFORM;
      else process.env.EXTERNAL_SUBAGENT_TEST_PLATFORM = previous;
    }
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// A real daemon with NO zcode runtime argument, NO subagent runtime
// environment, and a PATH that holds none of the executables — the Linux
// fail-closed baseline every enable/spawn refusal below is measured against.
async function bareDaemon(home) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-s05-daemon-'));
  const socket = path.join(root, 'daemon.sock');
  const env = { ...process.env, HOME: home, PATH: '/usr/bin:/bin' };
  for (const key of ['EXTERNAL_SUBAGENT_CONFIG', 'EXTERNAL_SUBAGENT_CONFIG_REVISION', 'DSH_RUNTIME_PATH', 'DSH_HOME', 'DSH_PROFILE', 'DSH_VERSION', 'CODEX_RUNTIME_PATH', 'CODEX_HOME', 'AGY_RUNTIME_PATH', 'ZCODE_RUNTIME_PATH']) delete env[key];
  const child = spawn(path.resolve('target/debug/external-subagentd'), [
    '--agent-config', productPaths(home, env).config,
    '--database', path.join(root, 'daemon.sqlite'),
    '--socket', socket,
    '--diagnostic-log', path.join(root, 'daemon-error.log'),
  ], { env, stdio: ['ignore', 'ignore', 'pipe'] });
  let stderr = '';
  child.stderr.on('data', (chunk) => { stderr += chunk; });
  const exited = new Promise((resolve) => { child.once('exit', resolve); });
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline && !fs.existsSync(socket)) {
    assert.equal(child.exitCode, null, `daemon exited early: ${stderr}`);
    await new Promise((resolve) => setTimeout(resolve, 40));
  }
  assert.ok(fs.existsSync(socket), `daemon socket never appeared: ${stderr}`);
  for (;;) {
    try { await callDaemon(socket, 'status', {}); break; } catch { await new Promise((resolve) => setTimeout(resolve, 40)); }
  }
  return {
    socket,
    stop: async () => {
      if (child.exitCode !== null || child.signalCode !== null) return;
      child.kill('SIGTERM');
      await Promise.race([exited, new Promise((resolve) => setTimeout(resolve, 3000))]);
      if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
      await exited;
      fs.rmSync(root, { recursive: true, force: true });
    },
  };
}

test('Linux fail-closed: without runtimes no subagent enables and no config is written; spawn before enable refuses', { timeout: 60000 }, async (t) => {
  const home = fixtureHome();
  const paths = productPaths(home);
  runInit({ paths, skipPayloadProbe: true, skipServiceStart: true });
  const daemon = await bareDaemon(home);
  t.after(() => daemon.stop());
  const options = { socket: daemon.socket, callDaemon };

  // The zcode adapter first: no --runtime was given, so the local probe is
  // unavailable and enable refuses without writing the config.
  await assert.rejects(() => subagentsCommand(paths, parseSubagentsArgs(['enable', 'zcode']), options), (error) => {
    assert.equal(error.code, 'agent_probe_failed');
    assert.match(error.message, /Cannot enable zcode/u);
    return true;
  });
  // dsh and agy: nothing on the daemon PATH, no *_RUNTIME_PATH exported.
  for (const agent of ['dsh', 'agy']) {
    await assert.rejects(() => subagentsCommand(paths, parseSubagentsArgs(['enable', agent]), options), (error) => {
      assert.equal(error.code, 'agent_probe_failed');
      assert.match(error.message, new RegExp(`Cannot enable ${agent}`, 'u'));
      return true;
    });
  }
  // A failed enable persists nothing: every subagent stays disabled.
  const config = JSON.parse(fs.readFileSync(paths.config, 'utf8'));
  for (const agent of ['zcode', 'dsh', 'codex', 'agy']) {
    assert.equal(config.subagents[agent].enabled, false, `${agent} must stay disabled`);
    assert.equal(config.subagents[agent].spawn_supported, false);
  }
  // The refusal is admission-level: spawn before enable creates no task.
  await assert.rejects(() => callDaemon(daemon.socket, 'spawn', {
    subagent: 'zcode', repository: home, prompt: 'must not run', permission_mode: 'plan',
  }), (error) => error.code === 'agent_disabled');
  const listed = await callDaemon(daemon.socket, 'list', { repository: home });
  assert.equal(listed.tasks.length, 0);
});
