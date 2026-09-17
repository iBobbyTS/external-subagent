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
// has already failed. The daemon and the temp root are reaped by that same
// guaranteed finally — even when the daemon socket never appears.
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
import { subagentsCommand } from '../../cli/commands/agents.mjs';
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
  const listed = await subagentsCommand(paths, { operation: 'list' });
  const ids = listed.subagents.map((agent) => agent.subagent).sort();
  assert.deepEqual(ids, ['codex', 'dsh', 'zcode']);
  assert.equal(listed.subagents.find((entry) => entry.subagent === 'codex').enabled, false);
  for (const agent of listed.subagents.filter((entry) => entry.subagent !== 'codex')) {
    assert.equal(agent.enabled, true);
    assert.equal(agent.spawn_supported, true);
  }
});

test('zcode explicit model selection fails closed at the config boundary', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'double-provider-'));
  const paths = writeDualConfig(path.join(root, 'config.json'));
  const input = parseConfigArgs(['set', 'subagents.zcode.default_model', 'glm-5.3']);
  assert.throws(() => configCommand(paths, input), (error) => {
    assert.ok(error instanceof CliError);
    assert.equal(error.code, 'model_selection_unsupported');
    return true;
  });
});

// Controlled oracle for the bridge-integrity digest itself: the finally-block
// re-check is only as good as treeDigest's ability to see drift through the
// symlink bridge, so the link semantics stay pinned by always-on tests.
test('treeDigest sees symlink target content drift, keeps dangling links, and bounds cycles', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'double-provider-digest-'));
  try {
    const tree = path.join(root, 'profiles');
    const inner = path.join(tree, 'settings');
    fs.mkdirSync(inner, { recursive: true });
    fs.writeFileSync(path.join(inner, 'creds.txt'), 'one');
    fs.writeFileSync(path.join(tree, 'plain.txt'), 'plain');
    fs.symlinkSync(path.join(inner, 'creds.txt'), path.join(tree, 'cred-link'));
    fs.symlinkSync(inner, path.join(tree, 'dir-link'));
    fs.symlinkSync(path.join(root, 'gone', 'target'), path.join(tree, 'dangling-link'));

    const before = treeDigest(tree);
    assert.match(before, /^tree:[0-9a-f]{64}$/u);

    // In-place edit of a file reached only through a resolving symlink.
    fs.writeFileSync(path.join(inner, 'creds.txt'), 'two');
    const afterContent = treeDigest(tree);
    assert.notEqual(afterContent, before, 'target content edits behind a symlink must surface');

    // Retargeting a resolving symlink (same content elsewhere).
    fs.writeFileSync(path.join(tree, 'plain.txt'), 'two');
    fs.rmSync(path.join(tree, 'cred-link'));
    fs.symlinkSync(path.join(tree, 'plain.txt'), path.join(tree, 'cred-link'));
    const afterRetarget = treeDigest(tree);
    assert.notEqual(afterRetarget, afterContent, 'link retargeting must surface');

    // A dangling link resolves into a real (empty) directory.
    fs.mkdirSync(path.join(root, 'gone', 'target'), { recursive: true });
    const afterResolving = treeDigest(tree);
    assert.notEqual(afterResolving, afterRetarget, 'a dangling link becoming resolved must surface');

    // Valid targets OUTSIDE the traversed root, reachable only through their
    // links: the walk never visits them directly, so only symlink resolution
    // can surface their content.
    const outside = path.join(root, 'outside');
    fs.mkdirSync(outside, { recursive: true });
    fs.writeFileSync(path.join(outside, 'token.txt'), 'first');
    fs.symlinkSync(path.join(outside, 'token.txt'), path.join(tree, 'outside-file-link'));
    const outsideDir = path.join(root, 'outside-dir');
    fs.mkdirSync(outsideDir, { recursive: true });
    fs.writeFileSync(path.join(outsideDir, 'note.txt'), 'one');
    fs.symlinkSync(outsideDir, path.join(tree, 'outside-dir-link'));
    const beforeOutside = treeDigest(tree);
    fs.writeFileSync(path.join(outside, 'token.txt'), 'second');
    assert.notEqual(treeDigest(tree), beforeOutside, 'an out-of-root file target reachable only via its link must surface');
    const afterOutsideFile = treeDigest(tree);
    fs.writeFileSync(path.join(outsideDir, 'note.txt'), 'two');
    assert.notEqual(treeDigest(tree), afterOutsideFile, 'content inside an out-of-root linked directory must surface');

    // A symlink loop and a diamond terminate, and content inside the looped
    // subtree is still covered by the digest.
    const loopBase = path.join(root, 'loop');
    const loopA = path.join(loopBase, 'a');
    fs.mkdirSync(loopA, { recursive: true });
    fs.writeFileSync(path.join(loopA, 'marker.txt'), 'first');
    fs.symlinkSync(loopA, path.join(loopA, 'self'));
    fs.symlinkSync(loopA, path.join(loopBase, 'diamond'));
    const cycled = treeDigest(loopBase);
    fs.writeFileSync(path.join(loopA, 'marker.txt'), 'second');
    assert.notEqual(treeDigest(loopBase), cycled, 'content inside a cycled subtree is still hashed');
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
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
// sorted (relative path, content) pairs, so in-place edits and new files
// written through the symlink surface as drift. Symlink entries hash the
// linked CONTENT when the link resolves — including targets that live
// OUTSIDE the traversed root and are reachable only through the link (an
// in-place edit of a target file behind the bridge must surface) — and keep
// their target path in the line
// (retargeting still changes the digest); dangling symlinks stay supported —
// the real DSH profiles tree contains dangling `node_modules` dependency
// symlinks, which statSync would refuse. Resolution is cycle-bounded: a
// directory real path already accounted for (a symlink loop or a diamond)
// becomes one stable `seen:` line instead of a second traversal, and the
// walk depth carries a hard backstop, so the digest always terminates.
export function treeDigest(dir, { depthLimit = 256 } = {}) {
  const lines = [];
  const seen = new Set([fs.realpathSync(dir)]);
  const walk = (current, prefix, depth) => {
    for (const entry of fs.readdirSync(current).sort()) {
      const child = path.join(current, entry);
      const rel = prefix ? `${prefix}/${entry}` : entry;
      const stat = fs.lstatSync(child);
      if (stat.isSymbolicLink()) {
        const target = fs.readlinkSync(child);
        let resolved = null;
        try { resolved = fs.statSync(child); } catch { /* dangling: hashed by target path only */ }
        if (resolved === null) lines.push(`${rel} dangling:${target}`);
        else if (resolved.isDirectory()) {
          const real = fs.realpathSync(child);
          if (seen.has(real)) lines.push(`${rel} seen:${target}`);
          else if (depth >= depthLimit) lines.push(`${rel} depth:${target}`);
          else { seen.add(real); lines.push(`${rel} linkdir:${target}`); walk(child, rel, depth + 1); }
        } else lines.push(`${rel} linkfile:${target}:${digest(child)}`);
      } else if (stat.isDirectory()) {
        const real = fs.realpathSync(child);
        if (seen.has(real)) lines.push(`${rel} seen:.`);
        else if (depth >= depthLimit) lines.push(`${rel} depth:.`);
        else { seen.add(real); walk(child, rel, depth + 1); }
      } else lines.push(`${rel} file:${digest(child)}`);
    }
  };
  walk(dir, '', 0);
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

  // The entire live resource lifecycle — temp root, read-only bridges,
  // daemon spawn, socket wait — lives inside one try/finally so the
  // finally reaps the daemon and removes the temp root even when the
  // daemon socket never appears or a live step fails early.
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'es-live-accept-'));
  let daemon = null;
  let daemonErr = '';
  let settled = false;
  let bridged = [];
  let before = new Map();
  try {
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
    bridged = [
      ['zcode cli config.json', realZcodeConfig],
      ['dsh .credentials.yaml', path.join(realDshHome, '.credentials.yaml')],
      ['dsh settings.yaml', path.join(realDshHome, 'settings.yaml')],
      ['dsh .anonymous-user-id', path.join(realDshHome, '.anonymous-user-id')],
      ['dsh profiles/ (recursive tree)', path.join(realDshHome, 'profiles')],
    ];
    for (const [label, target] of bridged) {
      assert.ok(fs.existsSync(target), `live bridge target missing: ${label}`);
    }
    before = new Map(bridged.map(([label, target]) => [label, bridgeDigest(target)]));

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
    daemon = spawn(daemonBinary, [
      '--database', path.join(daemonDir, 'state.sqlite'),
      '--socket', socket,
      '--agent-config', path.join(daemonDir, 'config.json'),
      '--runtime', zcodeRuntime,
    ], { stdio: ['ignore', 'pipe', 'pipe'], env: cleanDaemonEnv(home) });
    // An unhandled 'error' event would crash the runner and bypass the
    // finally, so spawn failures are recorded into the failure message.
    daemon.on('error', (error) => { daemonErr += error.message; });
    daemon.stderr.on('data', (chunk) => { daemonErr += chunk.toString(); });
    const deadline = Date.now() + 20_000;
    while (!fs.existsSync(socket) && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    assert.ok(fs.existsSync(socket), `daemon socket never appeared: ${daemonErr}`);
    const cli = liveCli(prefix, socket, home);

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

    // ZCode explicit model rejection (daemon-enforced boundary). The CLI
    // shim writes its error JSON to stderr and exits 1.
    const rejected = spawnSync(path.join(prefix, 'bin', 'external-subagent'), [
      'spawn', '--agent', 'zcode', '--repository', wsZcode, '--permission-mode', 'yolo',
      '--model', 'glm-5.3', '--prompt', 'hi',
    ], { encoding: 'utf8', timeout: 30_000, env: { ...cleanDaemonEnv(home), ZCODE_AGENTD_SOCKET: socket } });
    assert.equal(rejected.status, 1);
    const rejection = JSON.parse(rejected.stderr);
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
    // Reap the daemon even when the socket never appeared or a live step
    // already failed: SIGTERM, wait for exit, SIGKILL fallback.
    if (daemon && daemon.pid) {
      try { daemon.kill('SIGTERM'); } catch {}
      await new Promise((resolve) => {
        if (daemon.exitCode !== null || daemon.signalCode !== null) return resolve();
        const hardKill = setTimeout(() => { try { daemon.kill('SIGKILL'); } catch {} resolve(); }, 5_000);
        daemon.once('exit', () => { clearTimeout(hardKill); resolve(); });
      });
    }
    // Bridge integrity is enforced even when a live step above failed, and
    // only against targets whose baseline was captured. On a clean run
    // drift fails the test — but only after the cleanup below, so rmSync is
    // never skipped; on an already-failed run drift is reported without
    // masking the original failure. A vanished target counts as drift; a
    // missing baseline never throws here.
    let driftFailure = null;
    const drift = bridged
      .filter(([label, target]) => (fs.existsSync(target)
        ? before.has(label) && bridgeDigest(target) !== before.get(label)
        : before.has(label)))
      .map(([label]) => label);
    if (drift.length > 0) {
      const message = `bridged real provider files were modified: ${drift.join(', ')}`;
      if (settled) driftFailure = new assert.AssertionError({ message });
      else t.diagnostic(`BRIDGE DRIFT in addition to the failure above: ${message}`);
    }
    fs.rmSync(root, { recursive: true, force: true });
    if (driftFailure) throw driftFailure;
  }
});
