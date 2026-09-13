// Dual-provider (DSH/ZCode) acceptance evidence for ONE installed
// external-subagent artifact.
//
// This file carries two explicitly separated layers:
//
//  1. Always-on contract checks (controlled, no live provider): the shared
//     config/registry surface must keep both upstreams addressable, and the
//     ZCode model-selection boundary must fail closed at the CLI config
//     layer with `model_selection_unsupported`.
//
//  2. An opt-in LIVE acceptance harness (real upstreams, real installed
//     artifact). It is skipped unless EXTERNAL_SUBAGENT_LIVE_PREFIX points
//     at an npm prefix that has the packed tarball installed (npm pack +
//     `npm install -g --prefix <prefix> external-subagent-<v>.tgz`).
//     The harness exercises, through the INSTALLED CLI and the INSTALLED
//     native daemon only: discovery/status for both providers, the
//     authenticated DSH hi probe, the DSH model catalog, one real DSH build
//     task spawn→wait→result→close, one real DSH strict-plan (plan-mode)
//     task, one real ZCode task spawn→wait→result→close, the ZCode explicit
//     model rejection, and a mid-run cancel with reaping.
//
// Provider authentication is bridged read-only: the harness builds isolated
// provider homes whose credential-bearing files (and the profiles/ tree)
// are SYMLINKS to the real user configuration. No secret is printed,
// copied, or read into the test; business workspaces, the store, and
// sockets live under a fresh temp root, and every bridged target is
// digest-checked unchanged in the finally block — even when a live step
// has already failed.
//
// Recorded live evidence from this harness pattern (2026-09-12, artifact
// external-subagent@0.1.0, tgz sha256 75b01ba6..., native daemon sha256
// 2f17c4a7..., zcode runtime 0.16.5, dsh 0.1.5-rc.1), split by boundary:
//
//   LIVE, asserted by the committed harness on every run with
//   EXTERNAL_SUBAGENT_LIVE_PREFIX: dsh+zcode discovery/status; dsh local
//   probe READY; authenticated dsh hi through the read-only bridge; dsh
//   model catalog (non-empty); dsh build task spawn→wait→result→close with
//   resources_reaped; dsh strict-plan task COMPLETED with an unmutated
//   workspace; zcode task COMPLETED with native model and reaped; zcode
//   model rejection `model_selection_unsupported` (prompt_count=0);
//   mid-run dsh cancel → CANCELLED/reaped/closed.
//
//   LIVE, recorded in that session only and NOT re-asserted by this file
//   (do not cite this harness as their ongoing proof): MCP facade
//   status/spawn/wait/close with a real dsh task; zcode pending permission
//   → respond allow → COMPLETED; restart no-replay (graceful SIGTERM →
//   CANCELLED/reaped, hard SIGKILL → RUNTIME_LOST/reaped).
//
//   CONTROLLED, always-on (layer 1 above, every `node --test` run): the
//   dual-provider config surface and the zcode model-selection fail-closed
//   boundary. Controlled skips below stay skips; they never stand in for
//   live evidence.
//
// Known bounded limitations (do not silently re-classify): an authenticated
// AND policy-verified ZCode hi probe requires the product hooks to be
// installed into the same user config.json that carries the provider
// credentials. With a read-only symlink bridge the probe reports
// `policy_unverified`; with a hooks-only config the probe passes the policy
// verifier but the zcode child exits with `model_config_missing`. Resolving
// both in one home requires an authorized hook installation into the real
// user config (product `init --install-hooks`), which the live harness must
// never perform. This harness therefore asserts no zcode hi state at all.
// Symmetrically, a DSH home whose credentials were NOT bridged (blank home)
// must fail the authenticated hi probe; such a failure is never recorded
// or reported as success here.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { agentsCommand } from '../../cli/commands/agents.mjs';
import { configCommand, parseConfigArgs } from '../../cli/commands/config.mjs';
import { CliError } from '../../cli/errors.mjs';

const REPO = fileURLToPath(new URL('../..', import.meta.url));

function writeDualConfig(file) {
  fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  fs.writeFileSync(file, JSON.stringify({
    schema_version: 1,
    revision: 1,
    default_agent: null,
    agents: {
      zcode: { enabled: true, spawn_supported: true, default_model: null },
      dsh: {
        enabled: true,
        spawn_supported: true,
        default_model: null,
        runtime_path: '/opt/homebrew/bin/dsh',
        home: '/tmp/controlled-dsh-home',
        profile: 'acp',
        version: '0.1.5-rc.1',
      },
    },
  }));
  return { config: file };
}

test('dual-provider config surface keeps DSH and ZCode addressable', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'double-provider-'));
  const paths = writeDualConfig(path.join(root, 'config.json'));
  const listed = await agentsCommand(paths, { operation: 'list' });
  const ids = listed.agents.map((agent) => agent.agent).sort();
  assert.deepEqual(ids, ['dsh', 'zcode']);
  for (const agent of listed.agents) {
    assert.equal(agent.enabled, true);
    assert.equal(agent.spawn_supported, true);
  }
});

test('zcode explicit model selection fails closed at the config boundary', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'double-provider-'));
  const paths = writeDualConfig(path.join(root, 'config.json'));
  const input = parseConfigArgs(['set', 'agents.zcode.default_model', 'glm-5.3']);
  assert.throws(() => configCommand(paths, input), (error) => {
    assert.ok(error instanceof CliError);
    assert.equal(error.code, 'model_selection_unsupported');
    return true;
  });
});

// ---------------------------------------------------------------------------
// Live acceptance harness (opt-in). Controlled skips below stay skips: they
// are not silent passes and never stand in for the live evidence.
// ---------------------------------------------------------------------------

const LIVE_PREFIX = process.env.EXTERNAL_SUBAGENT_LIVE_PREFIX || '';
const live = Boolean(LIVE_PREFIX);

function cleanDaemonEnv(home) {
  const env = { ...process.env, HOME: home };
  for (const key of ['DSH_RUNTIME_PATH', 'DSH_HOME', 'DSH_PROFILE', 'DSH_VERSION', 'ZCODE_RUNTIME_PATH', 'EXTERNAL_SUBAGENT_CONFIG', 'ZCODE_AGENTD_STORE', 'ZCODE_AGENTD_SOCKET']) {
    delete env[key];
  }
  return env;
}

function liveCli(prefix, socket, home) {
  return (args) => spawnSync(path.join(prefix, 'bin', 'external-subagent'), args, {
    encoding: 'utf8',
    timeout: 300_000,
    env: { ...cleanDaemonEnv(home), ZCODE_AGENTD_SOCKET: socket },
  });
}

function jsonOut(result) {
  assert.equal(result.status, 0, `cli failed: ${result.stderr || result.stdout}`);
  return JSON.parse(result.stdout);
}

function digest(file) {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

// Recursive digest for the one directory bridge (`profiles/`): hashes the
// sorted (relative path, content) pairs, so both in-place edits and new
// files written through the symlink surface as drift.
function treeDigest(dir) {
  const lines = [];
  const walk = (current, prefix) => {
    for (const entry of fs.readdirSync(current).sort()) {
      const child = path.join(current, entry);
      const rel = prefix ? `${prefix}/${entry}` : entry;
      if (fs.statSync(child).isDirectory()) walk(child, rel);
      else lines.push(`${rel} ${digest(child)}`);
    }
  };
  walk(dir, '');
  return `tree:${crypto.createHash('sha256').update(lines.join('\n')).digest('hex')}`;
}

function bridgeDigest(target) {
  return fs.statSync(target).isDirectory() ? treeDigest(target) : digest(target);
}

test('live dual-provider acceptance through one installed artifact', { skip: live ? false : 'set EXTERNAL_SUBAGENT_LIVE_PREFIX to an npm prefix with the tarball installed' }, async (t) => {
  const prefix = path.resolve(LIVE_PREFIX);
  const installedRoot = path.join(prefix, 'lib', 'node_modules', 'external-subagent');
  const daemonBinary = path.join(installedRoot, 'npm', 'native', 'darwin-arm64', 'external-subagentd');
  for (const target of [path.join(prefix, 'bin', 'external-subagent'), daemonBinary]) {
    fs.accessSync(target, fs.constants.X_OK);
  }
  const versionResult = spawnSync(path.join(prefix, 'bin', 'external-subagent'), ['version'], {
    encoding: 'utf8',
    timeout: 30_000,
    env: cleanDaemonEnv(os.homedir()),
  });
  assert.equal(versionResult.status, 0);
  const version = versionResult.stdout.trim();
  assert.match(version, /^\d+\.\d+\.\d+$/);

  const dshRuntime = process.env.EXTERNAL_SUBAGENT_LIVE_DSH_RUNTIME || '/opt/homebrew/bin/dsh';
  const zcodeRuntime = process.env.EXTERNAL_SUBAGENT_LIVE_ZCODE_RUNTIME
    || '/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs';
  const realDshHome = process.env.EXTERNAL_SUBAGENT_LIVE_DSH_BRIDGE || path.join(os.homedir(), '.dsh');
  const realZcodeConfig = process.env.EXTERNAL_SUBAGENT_LIVE_ZCODE_CONFIG_BRIDGE
    || path.join(os.homedir(), '.zcode', 'cli', 'config.json');

  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'es-live-accept-'));
  const home = path.join(root, 'home');
  const dshHome = path.join(root, 'dshhome');
  const wsDsh = path.join(root, 'ws-dsh');
  const wsZcode = path.join(root, 'ws-zcode');
  const daemonDir = path.join(root, 'daemon');
  for (const dir of [home, dshHome, wsDsh, wsZcode, daemonDir, path.join(home, '.zcode', 'cli')]) {
    fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  }
  // Read-only auth bridges: the credential files and the `profiles/`
  // directory stay in the real homes and are referenced through symlinks;
  // nothing is copied, printed, or read into the test. Every bridged
  // target — including the profiles tree, recursively — is digest-checked
  // unchanged in the `finally` below, so bridge drift fails the run even
  // when a live step has already failed.
  fs.symlinkSync(realZcodeConfig, path.join(home, '.zcode', 'cli', 'config.json'));
  for (const name of ['.credentials.yaml', 'settings.yaml', '.anonymous-user-id', 'profiles']) {
    fs.symlinkSync(path.join(realDshHome, name), path.join(dshHome, name));
  }
  const bridged = [
    ['zcode cli config.json', realZcodeConfig],
    ['dsh .credentials.yaml', path.join(realDshHome, '.credentials.yaml')],
    ['dsh settings.yaml', path.join(realDshHome, 'settings.yaml')],
    ['dsh .anonymous-user-id', path.join(realDshHome, '.anonymous-user-id')],
    ['dsh profiles/ (recursive tree)', path.join(realDshHome, 'profiles')],
  ];
  for (const [label, target] of bridged) {
    assert.ok(fs.existsSync(target), `live bridge target missing: ${label}`);
  }
  const before = new Map(bridged.map(([label, target]) => [label, bridgeDigest(target)]));

  fs.writeFileSync(path.join(daemonDir, 'config.json'), JSON.stringify({
    schema_version: 1,
    revision: 1,
    default_agent: null,
    agents: {
      zcode: { enabled: true, spawn_supported: true, default_model: null },
      dsh: {
        enabled: true,
        spawn_supported: true,
        default_model: null,
        runtime_path: dshRuntime,
        home: dshHome,
        profile: 'acp',
        version: '0.1.5-rc.1',
      },
    },
  }));

  const socket = path.join(daemonDir, 'daemon.sock');
  const daemon = spawn(daemonBinary, [
    '--database', path.join(daemonDir, 'state.sqlite'),
    '--socket', socket,
    '--agent-config', path.join(daemonDir, 'config.json'),
    '--runtime', zcodeRuntime,
  ], { stdio: ['ignore', 'pipe', 'pipe'], env: cleanDaemonEnv(home) });
  let daemonErr = '';
  daemon.stderr.on('data', (chunk) => { daemonErr += chunk.toString(); });
  const deadline = Date.now() + 20_000;
  while (!fs.existsSync(socket) && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  assert.ok(fs.existsSync(socket), `daemon socket never appeared: ${daemonErr}`);
  const cli = liveCli(prefix, socket, home);

  let settled = false;
  try {
    // Discovery/status through the installed CLI.
    const status = jsonOut(cli(['agents', 'status']));
    const byAgent = Object.fromEntries(status.agents.map((agent) => [agent.agent, agent]));
    assert.equal(byAgent.zcode.configured, true);
    assert.equal(byAgent.zcode.spawn_supported, true);
    assert.equal(byAgent.dsh.configured, true);
    assert.equal(byAgent.dsh.spawn_supported, true);
    assert.equal(byAgent.zcode.model_selection.supported, false);
    assert.equal(byAgent.dsh.model_selection.supported, true);

    const dshLocal = jsonOut(cli(['agents', 'probe', 'dsh', '--local'])).evidence;
    assert.equal(dshLocal.local.state, 'READY');
    assert.equal(dshLocal.local.version, '0.1.5-rc.1');

    // Authenticated DSH hi through the bridged credentials.
    const dshHi = jsonOut(cli(['agents', 'probe', 'dsh', '--hi', '--workspace', wsDsh])).evidence;
    assert.equal(dshHi.hi.state, 'READY', `dsh hi not READY: ${JSON.stringify(dshHi.hi)}`);

    const dshModels = jsonOut(cli(['agents', 'models', 'dsh', '--workspace', wsDsh]));
    assert.equal(dshModels.supported, true);
    assert.ok(dshModels.models.length > 0);

    const lifecycle = (agentId) => {
      const waited = jsonOut(cli(['wait', '--json', JSON.stringify({ agent_id: agentId, wait_time: 240 })])).result;
      assert.equal(waited.task.phase, 'TERMINAL');
      assert.equal(waited.task.outcome, 'COMPLETED');
      assert.equal(waited.task.resources_reaped, true);
      assert.equal(waited.result.final_text.length > 0, true);
      const closed = jsonOut(cli(['close', '--json', JSON.stringify({ agent_id: agentId })])).result;
      assert.equal(closed.task.closed, true);
    };

    // Real DSH build task (workspace-write composition).
    fs.writeFileSync(path.join(wsDsh, 'seed.txt'), 'seed');
    const dshBuild = jsonOut(cli(['spawn', '--agent', 'dsh', '--repository', wsDsh, '--permission-mode', 'build',
      '--prompt', 'Write the exact text LIVE_BUILD_OK into marker.txt in the current directory, then reply with just: DONE'])).result;
    lifecycle(dshBuild.agent_id);
    assert.equal(fs.readFileSync(path.join(wsDsh, 'marker.txt'), 'utf8'), 'LIVE_BUILD_OK');

    // Real DSH strict-plan task (read-only first-launch scope).
    const beforePlan = fs.readdirSync(wsDsh).sort().join(',');
    const dshPlan = jsonOut(cli(['spawn', '--agent', 'dsh', '--repository', wsDsh, '--permission-mode', 'plan',
      '--prompt', 'How many files are in the current directory? Reply with only the number.'])).result;
    lifecycle(dshPlan.agent_id);
    assert.equal(fs.readdirSync(wsDsh).sort().join(','), beforePlan, 'strict-plan must not mutate the workspace');

    // Real ZCode task with its native model.
    const zcodeTask = jsonOut(cli(['spawn', '--agent', 'zcode', '--repository', wsZcode, '--permission-mode', 'yolo',
      '--prompt', 'Reply with exactly the single word LIVE_ZCODE_OK and nothing else. Do not use any tools.'])).result;
    const zcodeWaited = jsonOut(cli(['wait', '--json', JSON.stringify({ agent_id: zcodeTask.agent_id, wait_time: 240 })])).result;
    assert.equal(zcodeWaited.task.phase, 'TERMINAL');
    assert.equal(zcodeWaited.task.outcome, 'COMPLETED');
    assert.equal(zcodeWaited.task.input_identity.model_source, 'native');
    assert.equal(zcodeWaited.result.final_text.includes('LIVE_ZCODE_OK'), true);
    const zcodeClosed = jsonOut(cli(['close', '--json', JSON.stringify({ agent_id: zcodeTask.agent_id })])).result;
    assert.equal(zcodeClosed.task.closed, true);

    // ZCode explicit model rejection (daemon-enforced boundary).
    const rejected = spawnSync(path.join(prefix, 'bin', 'external-subagent'), [
      'spawn', '--agent', 'zcode', '--repository', wsZcode, '--permission-mode', 'yolo',
      '--model', 'glm-5.3', '--prompt', 'hi',
    ], { encoding: 'utf8', timeout: 30_000, env: { ...cleanDaemonEnv(home), ZCODE_AGENTD_SOCKET: socket } });
    assert.equal(rejected.status, 1);
    const rejection = JSON.parse(rejected.stdout);
    assert.equal(rejection.error.code, 'model_selection_unsupported');
    assert.match(rejection.error.message, /prompt_count=0/);

    // Mid-run cancel with reaping on DSH. The poll loop must actually
    // observe RUNNING: cancelling a task that never left QUEUED would not
    // be mid-run evidence.
    const cancellable = jsonOut(cli(['spawn', '--agent', 'dsh', '--repository', wsDsh, '--permission-mode', 'build',
      '--prompt', 'Use the Bash tool to run: sleep 45 — then reply with just: SLEPT'])).result;
    let sawRunning = false;
    for (let attempt = 0; attempt < 30 && !sawRunning; attempt += 1) {
      const polled = jsonOut(cli(['wait', '--json', JSON.stringify({ agent_id: cancellable.agent_id, wait_time: 3 })])).result;
      if (polled.task.phase === 'RUNNING') sawRunning = true;
      else assert.notEqual(polled.task.phase, 'TERMINAL', 'cancel target reached terminal before RUNNING');
    }
    assert.equal(sawRunning, true, 'cancel target never observed RUNNING');
    const cancelled = jsonOut(cli(['cancel', '--json', JSON.stringify({ agent_id: cancellable.agent_id })])).result;
    assert.equal(cancelled.task.outcome, 'CANCELLED');
    assert.equal(cancelled.task.resources_reaped, true);
    const closedCancelled = jsonOut(cli(['close', '--json', JSON.stringify({ agent_id: cancellable.agent_id })])).result;
    assert.equal(closedCancelled.task.closed, true);

    // Bounded evidence output (states only — no secrets, no digests), one
    // line per live-asserted boundary plus the explicit not-asserted list,
    // mirroring the boundary map in the header comment.
    console.log([
      `live-evidence artifact=${version}`,
      'discovery: dsh+zcode configured & spawn_supported; model_selection: dsh=supported zcode=unsupported(fail-closed)',
      `dsh: local=${dshLocal.local.state} authenticated-hi=${dshHi.hi.state} models=${dshModels.models.length}`,
      'dsh tasks: build=COMPLETED/reaped strict-plan=COMPLETED/workspace-unmutated mid-run-cancel=CANCELLED/reaped',
      'zcode tasks: native-model=COMPLETED/reaped model-rejection=model_selection_unsupported(prompt_count=0)',
      'NOT asserted here (that-session live evidence only): MCP facade, pending/respond, restart no-replay',
      'NOT asserted here (bounded limitation): zcode authenticated hi (policy_unverified/model_config_missing split)',
    ].join('\n'));
    settled = true;
  } finally {
    daemon.kill('SIGTERM');
    await new Promise((resolve) => {
      if (daemon.exitCode !== null || daemon.signalCode !== null) return resolve();
      const hardKill = setTimeout(() => { try { daemon.kill('SIGKILL'); } catch {} resolve(); }, 5_000);
      daemon.once('exit', () => { clearTimeout(hardKill); resolve(); });
    });
    // Bridge integrity is enforced even when a live step above failed;
    // drift on a clean run fails the test, drift on an already-failed run
    // is reported without masking the original failure.
    const drift = bridged
      .filter(([label, target]) => bridgeDigest(target) !== before.get(label))
      .map(([label]) => label);
    if (drift.length > 0) {
      const message = `bridged real provider files were modified: ${drift.join(', ')}`;
      if (settled) throw new assert.AssertionError({ message });
      t.diagnostic(`BRIDGE DRIFT in addition to the failure above: ${message}`);
    }
    fs.rmSync(root, { recursive: true, force: true });
  }
});
