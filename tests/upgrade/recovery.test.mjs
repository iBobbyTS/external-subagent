// R2 recovery oracle: the failure half of the active-task vA→vB upgrade.
//
// Controlled, always-on (no live upstream, no real launchd): real npm-free
// candidate roots and the REAL owner modules drive the failure paths the live
// drain oracle cannot inject safely.
//
//   1. Activation must retain the verified payload bytes beyond the npm
//      package directory, so a later npm replacement of that directory never
//      leaves active/rollback pointing at bytes it can no longer read.
//   2. A failed cross-version service activation must restore the OLD service
//      from those retained bytes when npm has already overwritten the old
//      program path — the real activateService under a faithful launchctl
//      seam that spawns real stub processes.
//   3. Corrupted install state must keep its evidence on disk instead of
//      being silently treated as an empty state and overwritten.
//   4. The npm auto-coordination entry detects package/active drift
//      read-only, stays stage-only before init, and never fires provider
//      probes.
//   5. An ignore-scripts style npm update (no daemon running) stays
//      coordinatable through the public CLI update/reconcile commands, which
//      hand service activation the retained rollback payload.
//   6. Registered Codex homes keep their real content and their disabled
//      plugin state across reconcile; reconcile never writes enablement.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { updateInstallation } from '../../cli/install/update.mjs';
import { updateCommand } from '../../cli/commands/update.mjs';
import { activateService } from '../../cli/install/service-activation.mjs';
import { npmUpdateCoordination, reconcileCodexHomes, registerCodexHome } from '../../cli/install/reconcile.mjs';
import { packageVersion } from '../../cli/install/layout.mjs';
import { RPC_VERSION } from '../../cli/rpc.mjs';

const darwinArm64 = process.platform === 'darwin' && process.arch === 'arm64';
const sha256 = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');
const digest = (file) => sha256(fs.readFileSync(file));

function makeCandidate(version) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'candidate-'));
  fs.mkdirSync(path.join(root, 'npm/native/darwin-arm64'), { recursive: true });
  fs.mkdirSync(path.join(root, 'bin'), { recursive: true });
  const entryBytes = Buffer.from(`#!/usr/bin/env node\n// stable entry ${version}\n`);
  fs.writeFileSync(path.join(root, 'bin/external-subagent.mjs'), entryBytes, { mode: 0o755 });
  fs.writeFileSync(path.join(root, 'package.json'), JSON.stringify({ version }));
  const files = ['external-subagentd', 'external-subagent-mcp'].map((name) => {
    const bytes = Buffer.alloc(48);
    bytes.writeUInt32LE(0xfeedfacf); bytes.writeUInt32LE(0x0100000c, 4); bytes.write(`v${version}`, 8, 'utf8');
    fs.writeFileSync(path.join(root, 'npm/native/darwin-arm64', name), bytes, { mode: 0o755 });
    return { name, bytes: bytes.length, sha256: crypto.createHash('sha256').update(bytes).digest('hex') };
  });
  fs.writeFileSync(path.join(root, 'npm/native/darwin-arm64/payload.json'), JSON.stringify({ schema_version: 1, product: 'external-subagent', platform: 'darwin-arm64', version, files }));
  return { root, entryBytes };
}

const statePaths = (data) => ({ data, state: path.join(data, 'state.json') });
const upgradeOptions = (candidate, version) => ({
  version, candidateRoot: candidate.root, platform: 'darwin-arm64', availableVersions: ['1.0.0', '2.0.0', packageVersion()],
});

const retainedRoot = (data, version) => path.join(data, 'payload-store', version);

test('activation retains immutable payload bytes beyond the npm package directory', () => {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'retain-state-'));
  const p = statePaths(data);
  const a = makeCandidate('1.0.0'), b = makeCandidate('2.0.0');
  try {
    const first = updateInstallation(p, upgradeOptions(a, '1.0.0'));
    assert.equal(first.phase, 'active');
    // npm replacing the package removes the old directory's bytes entirely:
    // the retained store must be the surviving copy of the vA identity.
    fs.rmSync(a.root, { recursive: true, force: true });
    const second = updateInstallation(p, upgradeOptions(b, '2.0.0'));
    assert.equal(second.phase, 'active');

    const storeA = retainedRoot(data, '1.0.0');
    const daemonA = path.join(storeA, 'external-subagentd');
    const entryA = path.join(storeA, 'external-subagent.mjs');
    assert.ok(fs.existsSync(daemonA), 'the retired vA daemon bytes survive the npm replacement');
    assert.equal(digest(daemonA), first.active.daemon_entry_sha256, 'retained vA daemon digest matches the published active identity');
    assert.equal(digest(entryA), first.active.entry_sha256, 'retained vA entry digest matches the published active identity');
    assert.equal(fs.statSync(daemonA).mode & 0o111, 0o111, 'retained daemon bytes stay executable');
    const manifestA = JSON.parse(fs.readFileSync(path.join(storeA, 'payload.json'), 'utf8'));
    assert.equal(manifestA.version, '1.0.0', 'retained vA payload manifest keeps its release identity');

    const storeB = retainedRoot(data, second.active.version);
    assert.equal(second.active.retained.daemon_entry, path.join(storeB, 'external-subagentd'));
    assert.equal(digest(second.active.retained.daemon_entry), second.active.daemon_entry_sha256);
    assert.equal(second.active.retained.entry, path.join(storeB, 'external-subagent.mjs'));
    assert.equal(digest(second.active.retained.entry), second.active.entry_sha256);
    assert.notEqual(second.active.retained.daemon_entry, second.active.daemon_entry, 'retained copies live outside the npm candidate directory');
  } finally {
    for (const dir of [b.root, data]) fs.rmSync(dir, { recursive: true, force: true });
  }
});

// A faithful launchd stand-in (the only launchctl-shaped thing tests may run):
// print/bootout/bootstrap against the REAL plist file, spawning the plist's
// exact ProgramArguments like launchd would.  The daemon "binary" is a stub
// shell process that records its own argv0 and sleeps; the simulated RPC
// reports the identity captured at spawn time, exactly like the real daemon
// binds its artifact hash to the running executable at startup.
async function faithfulService(dir, plist, marker) {
  const stateFile = path.join(dir, 'launchctl-state.json');
  const write = (doc) => fs.writeFileSync(stateFile, JSON.stringify(doc));
  const read = () => { try { return JSON.parse(fs.readFileSync(stateFile, 'utf8')); } catch { return { pid: null, program: null, sha: null }; } };
  const alive = (pid) => { try { process.kill(pid, 0); return true; } catch { return false; } };
  const unxml = (s) => s.replaceAll('&lt;', '<').replaceAll('&gt;', '>').replaceAll('&amp;', '&');
  const spawnCount = { value: 0 };
  const launchctl = async (argv) => {
    if (argv[0] === 'print') {
      const state = read();
      if (!state.pid || !alive(state.pid)) return { action: 'print', absent: true };
      return { action: 'print', status: 0, stdout: `\tpid = ${state.pid}\n` };
    }
    if (argv[0] === 'bootout') {
      const state = read();
      if (state.pid && alive(state.pid)) {
        process.kill(state.pid, 'SIGTERM');
        const deadline = Date.now() + 10_000;
        while (alive(state.pid) && Date.now() < deadline) await new Promise((resolve) => setTimeout(resolve, 50));
      }
      write({ pid: null, program: null, sha: null });
      return { action: 'bootout', status: 0 };
    }
    if (argv[0] === 'bootstrap') {
      const text = fs.readFileSync(argv[2], 'utf8');
      const programArgv = [...text.match(/<key>ProgramArguments<\/key>\s*<array>([\s\S]*?)<\/array>/)[1].matchAll(/<string>([^<]*)<\/string>/g)].map((m) => unxml(m[1]));
      const { spawn } = await import('node:child_process');
      const child = spawn(programArgv[0], programArgv.slice(1), { stdio: 'ignore', detached: false });
      child.unref();
      spawnCount.value += 1;
      write({ pid: child.pid, program: programArgv[0], sha: digest(programArgv[0]) });
      return { action: 'bootstrap', status: 0 };
    }
    throw new Error(`unexpected launchctl args: ${JSON.stringify(argv)}`);
  };
  const versionFor = (program) => (program.includes('retained-1.0.0') || program.includes('pkg-a') ? '1.0.0' : '2.0.0');
  let generation = 0;
  const callDaemon = async (_socket, command) => {
    assert.equal(command === 'status' || command === 'drain-status' || command === 'activate-ready', true, `unexpected rpc ${command}`);
    if (command === 'drain-status') return { is_draining: true, ready_for_activation: true };
    if (command === 'activate-ready') return { ready_for_activation: true, activation_claim: 'sim-claim' };
    // Real RPC latency: a process that dies at startup must be observed dead
    // within the same health-verification iteration that spawns it.
    await new Promise((resolve) => setTimeout(resolve, 25));
    const state = read();
    generation += 1;
    return {
      protocol_version: RPC_VERSION,
      service_generation: `sim-${generation}`,
      identity: { daemon: { version: versionFor(state.program), artifact: { path: state.program, sha256: state.sha } } },
    };
  };
  return { launchctl, callDaemon, read, alive, marker, plist, spawnCount };
}

test('service unload polls through a transient launchd registration', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'unload-poll-'));
  const launchAgent = path.join(root, 'agent.plist');
  fs.writeFileSync(launchAgent, '<plist><key>ProgramArguments</key><array><string>/bin/true</string></array></plist>');
  const paths = { launchAgent, socket: path.join(root, 'sock') };
  let prints = 0;
  const control = async (argv) => {
    if (argv[0] === 'print') return prints++ === 0 ? { status: 0, stdout: 'pid = 1' } : { absent: true };
    if (argv[0] === 'bootout') return { status: 0 };
    if (argv[0] === 'bootstrap') return { status: 0 };
    throw new Error('unexpected launchctl');
  };
  const rpc = async () => ({ });
  await assert.rejects(() => activateService(paths, { path: launchAgent, sha256: digest(launchAgent) }, { launchctl: control, callDaemon: rpc, healthTimeoutMs: 1 }), /service has no new process/);
  assert.ok(prints >= 2);
});

function stubPlist(program) {
  const xml = (s) => s.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
  return Buffer.from(`<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0"><dict>\n<key>Label</key><string>com.external-subagent.daemon</string>\n<key>ProgramArguments</key><array><string>${xml(program)}</string><string>--database</string><string>/tmp/unused.db</string></array>\n<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>\n</dict></plist>\n`);
}

test('a failed cross-version activation restores the old service from retained bytes after npm overwrote the old path', { skip: !darwinArm64 }, async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'rollback-retained-'));
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
    const plist = path.join(dir, 'agent.plist');
    fs.writeFileSync(plist, stubPlist(pkgA), { mode: 0o600 });
    const p = { data: dir, state: path.join(dir, 'state.json'), socket: path.join(dir, 'absent.sock'), launchAgent: plist };
    const service = await faithfulService(dir, plist, marker);

    // The vA service is running when npm replaces the package directory:
    // the old program path now holds the vB bytes.
    await service.launchctl(['bootstrap', `gui/${process.getuid()}`, plist]);
    const runningPid = service.read().pid;
    assert.ok(runningPid && service.alive(runningPid));
    fs.writeFileSync(pkgA, fs.readFileSync(pkgB), { mode: 0o755 }); // npm in-place overwrite
    assert.equal(digest(pkgA), shaB, 'the old program path now carries the new payload bytes');

    // The new payload can never health-verify: it self-reports version 2.0.0
    // while the selected candidate claims 2.0.1 — a genuinely broken release —
    // and its process dies at startup besides.  The failure is injected only
    // through the simulated daemon identity; activateService itself is real.
    let activationError = null;
    try {
      await activateService(p, { path: pkgB, sha256: shaB, version: '2.0.1' }, {
        launchctl: service.launchctl,
        callDaemon: service.callDaemon,
        healthTimeoutMs: 1_500,
        rollbackPayload: { path: retainedA, sha256: shaA },
      });
      assert.fail('activation of a broken payload must fail');
    } catch (error) {
      assert.equal(error.code, 'SERVICE_HEALTH_FAILED');
      activationError = error;
    }
    await (async () => {
      // The rollback must have restored a live OLD service running the
      // retained vA bytes — not the overwritten package path.
      const restored = service.read();
      assert.ok(restored.pid && service.alive(restored.pid), 'rollback left a live service');
      assert.equal(restored.program, retainedA, 'the restored service runs the retained vA artifact');
      assert.equal(restored.sha, shaA);
      assert.equal(activationError.rollback.artifact.path, retainedA);
      assert.equal(activationError.rollback.version, '1.0.0');
      assert.equal(digest(pkgA), shaB, 'rollback never rewrote the npm-managed directory');
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
      process.kill(restored.pid, 'SIGTERM');
    })();
    assert.ok(service.spawnCount.value >= 3, 'activation attempted the new payload and rolled back to the retained one');
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('corrupted install state keeps its evidence instead of being treated as empty', () => {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'corrupt-state-'));
  const p = statePaths(data);
  const a = makeCandidate('1.0.0');
  try {
    const evidence = Buffer.from('{"schema_version":2,"active":{"version":"9.9.9"},"phase":"active", NOT-A-VALID-DOCUMENT');
    fs.writeFileSync(p.state, evidence);
    const result = updateInstallation(p, upgradeOptions(a, '1.0.0'));
    assert.equal(result.phase, 'active');
    const backups = fs.readdirSync(data).filter((name) => name.startsWith('state.json.corrupt-'));
    assert.equal(backups.length, 1, 'the corrupted install state must be preserved on disk');
    assert.deepEqual(fs.readFileSync(path.join(data, backups[0])), evidence, 'the preserved evidence keeps the original bytes');
    assert.equal(result.recovery.code, 'INSTALL_STATE_CORRUPT');
    assert.equal(result.recovery.backup, path.join(data, backups[0]));
    const state = JSON.parse(fs.readFileSync(p.state, 'utf8'));
    assert.equal(state.phase, 'active');
    assert.equal(state.active.version, '1.0.0');
  } finally {
    fs.rmSync(data, { recursive: true, force: true });
    fs.rmSync(a.root, { recursive: true, force: true });
  }
});

test('npm auto-coordination detects drift read-only and stays stage-only before init', () => {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'coord-entry-'));
  const p = statePaths(data);
  const a = makeCandidate('1.0.0');
  try {
    const fresh = npmUpdateCoordination(p);
    assert.equal(fresh.initialized, false, 'a never-initialized product is stage-only');
    assert.equal(fresh.stage_only, true);
    assert.equal(fresh.update_pending, false);
    assert.equal(fresh.package_version, packageVersion());

    updateInstallation(p, upgradeOptions(a, '1.0.0'));
    const before = fs.readFileSync(p.state);
    const drifted = npmUpdateCoordination(p);
    assert.equal(drifted.initialized, true);
    assert.equal(drifted.stage_only, false);
    assert.equal(drifted.active_version, '1.0.0');
    assert.equal(drifted.update_pending, packageVersion() !== '1.0.0');
    assert.equal(drifted.coordination, packageVersion() !== '1.0.0' ? 'update' : 'reaffirm');
    // Detection is read-only: no daemon RPC, no codex invocation, no probe,
    // and not a single byte of product state changes.
    assert.deepEqual(fs.readFileSync(p.state), before);

    const current = updateInstallation(p, upgradeOptions(makeCandidate(packageVersion()), packageVersion()));
    assert.equal(current.phase, 'active');
    const reaffirm = npmUpdateCoordination(p);
    assert.equal(reaffirm.coordination, 'reaffirm');
    assert.equal(reaffirm.update_pending, false);
  } finally {
    fs.rmSync(data, { recursive: true, force: true });
    fs.rmSync(a.root, { recursive: true, force: true });
  }
});

test('an ignore-scripts npm update stays coordinatable through the CLI with no daemon running', { skip: !darwinArm64 }, async () => {
  for (const args of [[], ['reconcile']]) {
    const data = fs.mkdtempSync(path.join(os.tmpdir(), 'offline-update-'));
    const p = { data, state: path.join(data, 'state.json'), socket: path.join(data, 'absent.sock'), launchAgent: path.join(data, 'agent.plist') };
    try {
      const prior = updateInstallation(p, upgradeOptions(makeCandidate('1.0.0'), '1.0.0'));
      fs.writeFileSync(p.launchAgent, stubPlist('/tmp/never-started'), { mode: 0o600 });

      const activations = [];
      const result = await updateCommand(p, args, {
        hasInstalledService: () => true,
        activateService: async (_paths, candidate, options) => {
          activations.push({ candidate, options });
          return { pid: 4242, service_generation: 'offline-gen', artifact: { path: candidate.path, sha256: candidate.sha256 } };
        },
      });
      assert.equal(result.phase, 'active', `offline ${args[0] || 'update'} must still coordinate the published payload`);
      assert.equal(result.active.version, packageVersion(), 'the drifted package version becomes the new active');
      assert.equal(activations.length, 1, 'service activation runs exactly once');
      assert.equal(activations[0].options.rollbackPayload.path, prior.active.retained.daemon_entry,
        'activation receives the retained previous payload for rollback');
      assert.equal(activations[0].options.rollbackPayload.sha256, prior.active.retained.daemon_entry_sha256);
      const receipt = JSON.parse(fs.readFileSync(`${p.state}.activation.json`, 'utf8'));
      assert.equal(receipt.status, 'success');
      assert.match(receipt.claim, /^offline-/, 'an offline coordination carries a distinct receipt claim');
    } finally {
      fs.rmSync(data, { recursive: true, force: true });
    }
  }
});

test('reconcile re-affirmation never fires provider probes through the update surface', async () => {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'reaffirm-'));
  const p = { data, state: path.join(data, 'state.json'), socket: path.join(data, 'absent.sock') };
  try {
    updateInstallation(p, upgradeOptions(makeCandidate(packageVersion()), packageVersion()));
    const commands = [];
    const rpc = async (_socket, command) => {
      commands.push(command);
      return command === 'activate-ready' ? { ready_for_activation: true, activation_claim: 'reaffirm-1' } : { ready_for_activation: true };
    };
    await updateCommand(p, ['reconcile'], { callDaemon: rpc });
    assert.deepEqual(commands, ['drain', 'activate-ready']);
    assert.equal(commands.includes('agent-probe'), false, 'read-only coordination must never fire a paid provider probe');
  } finally {
    fs.rmSync(data, { recursive: true, force: true });
  }
});

// R2 bounded repair oracle A: a doomed candidate must never take the working
// daemon offline. The coordinator's pre-drain preflight is the SAME pure
// validation the locked updater owns (no second rule set), so a bad version
// or a bad manifest returns before any daemon RPC, any state write, or any
// receipt.
test('a bad requested version is rejected before the working daemon is drained', async () => {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'preflight-version-'));
  const p = statePaths(data);
  const daemon = { draining: false };
  const commands = [];
  const rpc = async (_socket, command) => {
    commands.push(command);
    if (command === 'drain') { daemon.draining = true; return { ready_for_activation: true }; }
    if (command === 'activate-ready') return { ready_for_activation: true, activation_claim: 'never-claimed' };
    if (command === 'drain-abort') { daemon.draining = false; return { is_draining: false }; }
    throw new Error(`unexpected rpc ${command}`);
  };
  try {
    await assert.rejects(() => updateCommand(p, ['--version=9.9.99'], { callDaemon: rpc }),
      (error) => error.code === 'PAYLOAD_VERSION_UNAVAILABLE');
    assert.deepEqual(commands, [], 'candidate verification must precede every daemon RPC');
    assert.equal(daemon.draining, false, 'the working daemon was never put into draining');
    assert.equal(fs.existsSync(`${p.state}.activation.json`), false, 'a pre-drain rejection writes no receipt');
    assert.equal(fs.existsSync(p.state), false, 'a pre-drain rejection publishes no install state');
  } finally {
    fs.rmSync(data, { recursive: true, force: true });
  }
});

test('an unverifiable candidate manifest is rejected before any daemon RPC through the same validation owner', async () => {
  const { preflightUpdate } = await import('../../cli/install/update.mjs');
  assert.equal(typeof preflightUpdate, 'function', 'the pure preflight must be exported by the install validation owner');
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'preflight-manifest-'));
  const p = statePaths(data);
  const a = makeCandidate('1.0.0');
  const bad = makeCandidate('2.0.0');
  // A genuinely broken release: the payload manifest is not even valid JSON.
  fs.writeFileSync(path.join(bad.root, 'npm/native/darwin-arm64/payload.json'), '{ NOT-A-VALID-DOCUMENT');
  const commands = [];
  const rpc = async (_socket, command) => {
    commands.push(command);
    if (command === 'drain') return { ready_for_activation: true };
    if (command === 'activate-ready') return { ready_for_activation: true, activation_claim: 'manifest-1' };
    if (command === 'drain-abort') return { is_draining: false };
    throw new Error(`unexpected rpc ${command}`);
  };
  try {
    updateInstallation(p, upgradeOptions(a, '1.0.0'));
    const before = fs.readFileSync(p.state);
    // The seam pair wraps the REAL owners with the same forced candidate, so
    // the preflight and the updater can never disagree about the rules.
    const forced = (options) => ({ ...options, ...upgradeOptions(bad, '2.0.0') });
    await assert.rejects(() => updateCommand(p, ['--version=2.0.0'], {
      callDaemon: rpc,
      preflightUpdate: (options) => preflightUpdate(forced(options)),
      updateInstallation: (target, options) => updateInstallation(target, forced(options)),
    }), (error) => error.code === 'PAYLOAD_MANIFEST_INVALID');
    assert.deepEqual(commands, [], 'a bad manifest must return before the drain RPC');
    assert.deepEqual(fs.readFileSync(p.state), before, 'the published active is untouched by the pre-drain rejection');
    assert.equal(fs.existsSync(`${p.state}.activation.json`), false, 'the pre-drain rejection writes no receipt');
  } finally {
    for (const dir of [a.root, bad.root, data]) fs.rmSync(dir, { recursive: true, force: true });
  }
});

// R2 bounded repair oracle B: a failed update that already drained a live
// daemon must reopen its admission through the bounded abort-drain
// management RPC, keep every completed/reaped fact, and leave a retryable
// receipt with the abort evidence.
test('a failed post-claim update aborts the daemon drain and keeps the receipt retryable', async () => {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'abort-recover-'));
  const p = statePaths(data);
  const commands = [];
  const rpc = async (_socket, command) => {
    commands.push(command);
    if (command === 'drain') return { ready_for_activation: true };
    if (command === 'activate-ready') return { ready_for_activation: true, activation_claim: 'abort-1' };
    if (command === 'drain-abort') return { is_draining: false, ready_for_activation: false };
    throw new Error(`unexpected rpc ${command}`);
  };
  try {
    await assert.rejects(() => updateCommand(p, ['--version=2.0.0'], {
      callDaemon: rpc,
      // Stub-updater test: the preflight is waived; its ordering oracle is
      // the two tests above.
      preflightUpdate: () => ({}),
      updateInstallation: async () => { throw new Error('boom'); },
    }), /boom/);
    assert.deepEqual(commands, ['drain', 'activate-ready', 'drain-abort'],
      'a failed, not-yet-activated update must end with the bounded drain abort');
    const receipt = JSON.parse(fs.readFileSync(`${p.state}.activation.json`, 'utf8'));
    assert.equal(receipt.claim, 'abort-1');
    assert.equal(receipt.status, 'failed');
    assert.equal(receipt.retryable, true);
    assert.equal(receipt.drain_aborted, true);
  } finally {
    fs.rmSync(data, { recursive: true, force: true });
  }
});

test('a legacy daemon that cannot abort keeps its evidence without masking the update failure', async () => {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'abort-legacy-'));
  const p = statePaths(data);
  const commands = [];
  const rpc = async (_socket, command) => {
    commands.push(command);
    if (command === 'drain') return { ready_for_activation: true };
    if (command === 'activate-ready') return { ready_for_activation: true, activation_claim: 'legacy-1' };
    if (command === 'drain-abort') {
      // Exactly what the REAL callDaemon produces for a released daemon
      // whose is_known table predates daemon_abort_drain: the raw wire
      // frame { code: 'unknown_method', message: 'unknown RPC method' }
      // (RpcErrorCode serializes snake_case) passes through as the CliError
      // code verbatim — rpc.mjs preserves daemon error codes.
      const error = new Error('unknown RPC method');
      error.code = 'unknown_method';
      error.daemonResponded = true;
      throw error;
    }
    throw new Error(`unexpected rpc ${command}`);
  };
  try {
    await assert.rejects(() => updateCommand(p, ['--version=2.0.0'], {
      callDaemon: rpc,
      preflightUpdate: () => ({}),
      updateInstallation: async () => { throw new Error('boom'); },
    }), /boom/);
    assert.deepEqual(commands, ['drain', 'activate-ready', 'drain-abort']);
    const receipt = JSON.parse(fs.readFileSync(`${p.state}.activation.json`, 'utf8'));
    assert.equal(receipt.status, 'failed');
    assert.equal(receipt.error, 'boom', 'the receipt reason stays the original update failure, not the abort failure');
    assert.equal(receipt.retryable, true, 'a daemon that stayed draining keeps the failure retryable');
    assert.equal(receipt.drain_aborted, false);
    assert.equal(receipt.drain_abort_error.code, 'UNKNOWN_METHOD',
      'the raw snake_case wire code is canonicalized into the CLI error vocabulary');
    assert.equal(receipt.daemon_may_be_draining, true,
      'the receipt states explicitly that the legacy daemon may still be draining');
  } finally {
    fs.rmSync(data, { recursive: true, force: true });
  }
});

test('reconcile preserves registered home content and the disabled plugin state', { skip: !darwinArm64 }, () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'home-preserve-'));
  const p = { ...statePaths(path.join(root, 'data')), home: root, socket: path.join(root, 'data', 's.sock') };
  fs.mkdirSync(p.data, { recursive: true });
  const home = path.join(root, 'codex-home');
  fs.mkdirSync(home, { recursive: true, mode: 0o700 });
  const userNotes = path.join(home, 'user-notes.md');
  fs.writeFileSync(userNotes, 'user-owned content that reconcile must never touch\n');
  const configToml = path.join(home, 'config.toml');
  const disabledBinding = [
    '# user-owned codex configuration',
    '[mcp_servers.external_subagent]',
    'command = "/somewhere/external-subagent-mcp"',
    'enabled = false',
    '',
    '[mcp_servers.user_own_server]',
    'command = "uvx"',
    'enabled = true',
    '',
  ].join('\n');
  fs.writeFileSync(configToml, disabledBinding);
  try {
    registerCodexHome(p, home);
    const shim = path.join(root, 'bin', 'codex');
    fs.mkdirSync(path.dirname(shim), { recursive: true });
    fs.writeFileSync(shim, [
      '#!/usr/bin/env node',
      'import path from "node:path";',
      'const args = process.argv.slice(2);',
      'const text = (value) => { process.stdout.write(JSON.stringify(value, null, 2) + "\\n"); };',
      'if (args[0] === "plugin" && args[1] === "add" && args.includes("--help")) process.exit(0);',
      'if (args[0] === "plugin" && args[1] === "marketplace" && args[2] === "add") { text({ marketplaceName: "personal", installedRoot: args[3], alreadyAdded: false }); process.exit(0); }',
      'if (args[0] === "plugin" && args[1] === "add") { text({ pluginId: "external-subagent@personal", installedPath: path.join(process.env.CODEX_HOME || "", "plugins", "cache", "personal", "external-subagent", "0.1.0") }); process.exit(0); }',
      'process.stderr.write("unexpected codex invocation: " + JSON.stringify(args) + "\\n");',
      'process.exit(1);',
    ].join('\n'), { mode: 0o755 });

    const result = reconcileCodexHomes(p, { codexCli: shim });
    assert.equal(result.homes[0].status, 'updated');
    assert.equal(result.all_updated, true);
    assert.deepEqual(fs.readFileSync(configToml).toString(), disabledBinding,
      'reconcile must preserve the user disabled binding byte-for-byte and never force enablement');
    assert.deepEqual(fs.readFileSync(userNotes).toString(), 'user-owned content that reconcile must never touch\n');
    const staging = path.join(p.home, 'plugins', 'external-subagent', '.codex-plugin', 'plugin.json');
    assert.ok(fs.existsSync(staging), 'the managed staging tree was refreshed');
    const registry = JSON.parse(fs.readFileSync(path.join(p.data, 'codex-homes.json'), 'utf8'));
    assert.equal(registry.homes[0].last_status, 'updated');
    assert.ok(registry.homes[0].last_sync_ms, 'the registry records the successful refresh');
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
