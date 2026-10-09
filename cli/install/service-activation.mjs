import fs from 'node:fs';
import crypto from 'node:crypto';
import { atomicWrite } from '../fs-atomic.mjs';
import { CliError } from '../errors.mjs';
import { callDaemon } from '../rpc.mjs';
import { LAUNCH_AGENT_LABEL } from '../constants.mjs';
import { platform } from '../paths.mjs';
import { launchAgentPlist, launchctl } from './service-macos.mjs';
import { SYSTEMD_UNIT_NAME, execStartProgram, replaceExecStartProgram, systemctl, systemdUnit, unitServiceStatus } from './service-linux.mjs';

const xml = (s) => s.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
const unxml = (s) => s.replaceAll('&lt;', '<').replaceAll('&gt;', '>').replaceAll('&amp;', '&');
const program = /(<key>ProgramArguments<\/key>\s*<array>\s*<string>)([^<]+)(<\/string>)/;
const digest = (file) => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');

export function hasInstalledService(paths) {
  return Boolean(paths.launchAgent && fs.existsSync(paths.launchAgent));
}

// Platform dispatch (S03): the same activation health/rollback contract runs
// over launchd on macOS (verbatim) and the systemd user manager on Linux.
export async function activateService(paths, candidate, options = {}) {
  return platform() === 'darwin' ? activateLaunchdService(paths, candidate, options) : activateSystemdService(paths, candidate, options);
}

// The service definition is derived from the CURRENT config: activation
// regenerates the whole plist (environment, revision, runtime forwarding) so a
// reconfigured installation can never keep serving stale launchd environment.
// Only rollback restores the previous definition bytes — the exact known state
// the service ran before — with its executable repointed.
async function activateLaunchdService(paths, candidate, options = {}) {
  const control = options.launchctl || launchctl;
  const rpc = options.callDaemon || callDaemon;
  const target = `gui/${process.getuid()}/${LAUNCH_AGENT_LABEL}`;
  const oldPlist = fs.readFileSync(paths.launchAgent);
  const match = oldPlist.toString().match(program);
  if (!match) throw new CliError('SERVICE_DEFINITION_INVALID', 'installed service has no executable entry');
  const oldPath = unxml(match[2]);
  const servicePid = async () => {
    const result = await control(['print', target]);
    if (result.absent) return null;
    if (result.skipped) throw new CliError('SERVICE_NOT_VERIFIED', 'service control was skipped');
    const pid = Number(result.stdout?.match(/\bpid = (\d+)/)?.[1]);
    return Number.isInteger(pid) && pid > 0 ? pid : null;
  };
  const oldPid = await servicePid();
  const oldStatus = oldPid ? await rpc(paths.socket, 'status', {}) : null;
  // The running daemon's self-reported digest is the truth about which bytes
  // the service was executing (bound to the running executable at startup);
  // the file at oldPath may already carry the NEXT version after an npm
  // install replaced the package directory in place.
  const runningArtifact = oldStatus?.identity?.daemon?.artifact;
  const oldSha = (typeof runningArtifact?.sha256 === 'string' && runningArtifact.sha256) || digest(oldPath);
  if (oldPid) {
    const drain = await rpc(paths.socket, 'drain-status', {});
    if (!drain.ready_for_activation) throw new CliError('DAEMON_NOT_DRAINED', 'daemon still owns active or unreaped work');
    const claim = await rpc(paths.socket, 'activate-ready', {});
    if (!claim.activation_claim) throw new CliError('ACTIVATION_NOT_CLAIMED', 'daemon did not grant activation');
  }
  // npm may have replaced the old program path with the new payload: rolling
  // the service back must run the OLD bytes, from the original path when they
  // are still intact or from the retained payload copy the updater staged
  // for exactly this recovery.
  const rollbackExecutable = () => {
    if (fs.existsSync(oldPath) && digest(oldPath) === oldSha) return { path: oldPath, sha256: oldSha };
    const retained = options.rollbackPayload;
    if (retained && typeof retained.path === 'string' && typeof retained.sha256 === 'string'
      && fs.existsSync(retained.path) && digest(retained.path) === retained.sha256) {
      return { path: retained.path, sha256: retained.sha256 };
    }
    throw new CliError('ROLLBACK_PAYLOAD_UNAVAILABLE', 'previous payload bytes are unavailable; automatic rollback refused');
  };
  const healthy = async (expected, previousPid, previousGeneration) => {
    const deadline = Date.now() + (options.healthTimeoutMs ?? 10_000);
    let last;
    do {
      try {
        const pid = await servicePid();
        if (!pid || pid === previousPid) throw new Error('service has no new process');
        const status = await rpc(paths.socket, 'status', {});
        const identity = status.identity?.daemon;
        // Health verification uses the service generation and the running
        // artifact identity. The diagnostic-only mcp_version never gates
        // admission.
        if (!status.service_generation || status.service_generation === previousGeneration
          || identity?.artifact?.path !== expected.path || identity?.artifact?.sha256 !== expected.sha256
          || (expected.version && identity?.version !== expected.version)) throw new Error('running daemon identity does not match selected payload');
        if (await servicePid() !== pid) throw new Error('service process changed during health verification');
        return { pid, version: identity.version, artifact: identity.artifact, service_generation: status.service_generation };
      } catch (error) { last = error; }
      await new Promise((resolve) => setTimeout(resolve, 50));
    } while (Date.now() < deadline);
    throw new CliError('SERVICE_HEALTH_FAILED', last?.message || 'service did not become healthy');
  };
  const unload = async () => {
    const result = await control(['print', target]);
    if (result.absent) return;
    await control(['bootout', target]);
    const deadline = Date.now() + (options.unloadTimeoutMs ?? 10_000);
    do {
      const probe = await control(['print', target]);
      if (probe.absent) return;
      await new Promise((resolve) => setTimeout(resolve, 50));
    } while (Date.now() < deadline);
    throw new CliError('SERVICE_UNLOAD_TIMEOUT', 'launchd service remained registered after bootout');
  };
  const writeDefinition = (entryPath) => atomicWrite(paths.launchAgent, launchAgentPlist(paths, {
    daemonPath: entryPath,
    ...(options.zcodeRuntime !== undefined ? { zcodeRuntime: options.zcodeRuntime } : {}),
  }), 0o600);
  const restoreDefinition = (entryPath) => atomicWrite(paths.launchAgent, Buffer.from(oldPlist.toString().replace(program, (_, a, b, c) => `${a}${xml(entryPath)}${c}`)), 0o600);
  const rollbackService = async () => {
    await unload();
    const previous = rollbackExecutable();
    restoreDefinition(previous.path);
    await control(['bootstrap', `gui/${process.getuid()}`, paths.launchAgent]);
    return healthy({ path: previous.path, sha256: previous.sha256, version: oldStatus?.identity?.daemon?.version }, oldPid, oldStatus?.service_generation);
  };
  try {
    await unload();
    writeDefinition(candidate.path);
    await control(['bootstrap', `gui/${process.getuid()}`, paths.launchAgent]);
    const health = await healthy(candidate, oldPid, oldStatus?.service_generation);
      return {
      ...health,
      rollback: async () => {
        const restored = await rollbackService();
        if (digest(oldPath) !== oldSha) return { ...restored, restored_from: 'retained_payload' };
        return restored;
      },
    };
  } catch (error) {
    if (!oldPid) {
      try {
        await unload();
        atomicWrite(paths.launchAgent, oldPlist, 0o600);
        error.rollback = { attempted: true, restored: false, plist_restored: true, error: 'no previous service identity to verify' };
      } catch (rollbackError) {
        error.rollback = { attempted: true, restored: false, plist_restored: false, error: rollbackError.message };
      }
      throw error;
    }
    try {
      error.rollback = await rollbackService();
    } catch (rollbackError) { error.rollbackError = rollbackError.message; }
    throw error;
  }
}

// The systemd twin of the launchd activation above: same drain/claim prelude,
// same health verification (a NEW process running the selected artifact's
// exact bytes and self-reporting its identity), same rollback-to-retained-bytes
// contract.  unload = disable --now + the bounded inactive confirmation;
// load = daemon-reload + enable --now (the same bootstrap init uses).
async function activateSystemdService(paths, candidate, options = {}) {
  const control = options.systemctl || systemctl;
  const rpc = options.callDaemon || callDaemon;
  const oldUnit = fs.readFileSync(paths.launchAgent);
  const oldPath = execStartProgram(oldUnit);
  if (!oldPath) throw new CliError('SERVICE_DEFINITION_INVALID', 'installed service has no executable entry');
  const servicePid = async () => {
    const view = unitServiceStatus(control);
    if (view.skipped) throw new CliError('SERVICE_NOT_VERIFIED', 'service control was skipped');
    if (!view.registered) return null;
    return view.pid;
  };
  const oldPid = await servicePid();
  const oldStatus = oldPid ? await rpc(paths.socket, 'status', {}) : null;
  // The running daemon's self-reported digest is the truth about which bytes
  // the service was executing; the file at oldPath may already carry the NEXT
  // version after an npm install replaced the package directory in place.
  const runningArtifact = oldStatus?.identity?.daemon?.artifact;
  const oldSha = (typeof runningArtifact?.sha256 === 'string' && runningArtifact.sha256) || digest(oldPath);
  if (oldPid) {
    const drain = await rpc(paths.socket, 'drain-status', {});
    if (!drain.ready_for_activation) throw new CliError('DAEMON_NOT_DRAINED', 'daemon still owns active or unreaped work');
    const claim = await rpc(paths.socket, 'activate-ready', {});
    if (!claim.activation_claim) throw new CliError('ACTIVATION_NOT_CLAIMED', 'daemon did not grant activation');
  }
  // npm may have replaced the old program path with the new payload: rolling
  // the service back must run the OLD bytes, from the original path when they
  // are still intact or from the retained payload copy the updater staged
  // for exactly this recovery.
  const rollbackExecutable = () => {
    if (fs.existsSync(oldPath) && digest(oldPath) === oldSha) return { path: oldPath, sha256: oldSha };
    const retained = options.rollbackPayload;
    if (retained && typeof retained.path === 'string' && typeof retained.sha256 === 'string'
      && fs.existsSync(retained.path) && digest(retained.path) === retained.sha256) {
      return { path: retained.path, sha256: retained.sha256 };
    }
    throw new CliError('ROLLBACK_PAYLOAD_UNAVAILABLE', 'previous payload bytes are unavailable; automatic rollback refused');
  };
  const healthy = async (expected, previousPid, previousGeneration) => {
    const deadline = Date.now() + (options.healthTimeoutMs ?? 10_000);
    let last;
    do {
      try {
        const pid = await servicePid();
        if (!pid || pid === previousPid) throw new Error('service has no new process');
        const status = await rpc(paths.socket, 'status', {});
        const identity = status.identity?.daemon;
        // Health verification uses the service generation and the running
        // artifact identity. The diagnostic-only mcp_version never gates
        // admission.
        if (!status.service_generation || status.service_generation === previousGeneration
          || identity?.artifact?.path !== expected.path || identity?.artifact?.sha256 !== expected.sha256
          || (expected.version && identity?.version !== expected.version)) throw new Error('running daemon identity does not match selected payload');
        if (await servicePid() !== pid) throw new Error('service process changed during health verification');
        return { pid, version: identity.version, artifact: identity.artifact, service_generation: status.service_generation };
      } catch (error) { last = error; }
      await new Promise((resolve) => setTimeout(resolve, 50));
    } while (Date.now() < deadline);
    throw new CliError('SERVICE_HEALTH_FAILED', last?.message || 'service did not become healthy');
  };
  const unload = async () => {
    const view = unitServiceStatus(control);
    if (view.skipped || !view.registered) return;
    await control(['disable', '--now', SYSTEMD_UNIT_NAME]);
    const deadline = Date.now() + (options.unloadTimeoutMs ?? 10_000);
    do {
      const probe = unitServiceStatus(control);
      if (probe.skipped || !probe.registered || (probe.state === 'inactive' && probe.pid === null)) return;
      await new Promise((resolve) => setTimeout(resolve, 50));
    } while (Date.now() < deadline);
    throw new CliError('SERVICE_UNLOAD_TIMEOUT', 'systemd service remained active after disable --now');
  };
  const writeDefinition = (entryPath) => atomicWrite(paths.launchAgent, systemdUnit(paths, {
    daemonPath: entryPath,
    ...(options.zcodeRuntime !== undefined ? { zcodeRuntime: options.zcodeRuntime } : {}),
  }), 0o600);
  const restoreDefinition = (entryPath) => atomicWrite(paths.launchAgent, replaceExecStartProgram(oldUnit, entryPath), 0o600);
  const load = async () => {
    await control(['daemon-reload']);
    await control(['enable', '--now', SYSTEMD_UNIT_NAME]);
  };
  const rollbackService = async () => {
    await unload();
    const previous = rollbackExecutable();
    restoreDefinition(previous.path);
    await load();
    return healthy({ path: previous.path, sha256: previous.sha256, version: oldStatus?.identity?.daemon?.version }, oldPid, oldStatus?.service_generation);
  };
  try {
    await unload();
    writeDefinition(candidate.path);
    await load();
    const health = await healthy(candidate, oldPid, oldStatus?.service_generation);
    return {
      ...health,
      rollback: async () => {
        const restored = await rollbackService();
        if (digest(oldPath) !== oldSha) return { ...restored, restored_from: 'retained_payload' };
        return restored;
      },
    };
  } catch (error) {
    if (!oldPid) {
      try {
        await unload();
        atomicWrite(paths.launchAgent, oldUnit, 0o600);
        error.rollback = { attempted: true, restored: false, unit_restored: true, error: 'no previous service identity to verify' };
      } catch (rollbackError) {
        error.rollback = { attempted: true, restored: false, unit_restored: false, error: rollbackError.message };
      }
      throw error;
    }
    try {
      error.rollback = await rollbackService();
    } catch (rollbackError) { error.rollbackError = rollbackError.message; }
    throw error;
  }
}
