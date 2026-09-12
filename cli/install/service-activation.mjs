import fs from 'node:fs';
import crypto from 'node:crypto';
import { atomicWrite } from '../fs-atomic.mjs';
import { CliError } from '../errors.mjs';
import { callDaemon, RPC_VERSION } from '../rpc.mjs';
import { LAUNCH_AGENT_LABEL } from '../constants.mjs';
import { launchctl } from './service-macos.mjs';

const xml = (s) => s.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
const unxml = (s) => s.replaceAll('&lt;', '<').replaceAll('&gt;', '>').replaceAll('&amp;', '&');
const program = /(<key>ProgramArguments<\/key>\s*<array>\s*<string>)([^<]+)(<\/string>)/;
const digest = (file) => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');

export function hasInstalledService(paths) {
  return Boolean(paths.launchAgent && fs.existsSync(paths.launchAgent));
}

// The service definition is the executable entry: change only its first argv,
// retaining the installed configuration/environment and rollback bytes exactly.
export async function activateService(paths, candidate, options = {}) {
  const control = options.launchctl || launchctl;
  const rpc = options.callDaemon || callDaemon;
  const target = `gui/${process.getuid()}/${LAUNCH_AGENT_LABEL}`;
  const oldPlist = fs.readFileSync(paths.launchAgent);
  const match = oldPlist.toString().match(program);
  if (!match) throw new CliError('SERVICE_DEFINITION_INVALID', 'installed service has no executable entry');
  const oldPath = unxml(match[2]);
  const oldHash = digest(oldPath);
  const servicePid = async () => {
    const result = await control(['print', target]);
    if (result.absent) return null;
    if (result.skipped) throw new CliError('SERVICE_NOT_VERIFIED', 'service control was skipped');
    const pid = Number(result.stdout?.match(/\bpid = (\d+)/)?.[1]);
    return Number.isInteger(pid) && pid > 0 ? pid : null;
  };
  const oldPid = await servicePid();
  const oldStatus = oldPid ? await rpc(paths.socket, 'status', {}) : null;
  if (oldPid) {
    const drain = await rpc(paths.socket, 'drain-status', {});
    if (!drain.ready_for_activation) throw new CliError('DAEMON_NOT_DRAINED', 'daemon still owns active or unreaped work');
    const claim = await rpc(paths.socket, 'activate-ready', {});
    if (!claim.activation_claim) throw new CliError('ACTIVATION_NOT_CLAIMED', 'daemon did not grant activation');
  }
  const healthy = async (expected, previousPid, previousGeneration) => {
    const deadline = Date.now() + (options.healthTimeoutMs ?? 10_000);
    let last;
    do {
      try {
        const pid = await servicePid();
        if (!pid || pid === previousPid) throw new Error('service has no new process');
        const status = await rpc(paths.socket, 'status', {});
        const identity = status.identity?.daemon;
        if (status.protocol_version !== RPC_VERSION || !status.service_generation || status.service_generation === previousGeneration
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
    if (!result.absent) await control(['bootout', target]);
  };
  try {
    await unload();
    atomicWrite(paths.launchAgent, Buffer.from(oldPlist.toString().replace(program, (_, a, b, c) => `${a}${xml(candidate.path)}${c}`)), 0o600);
    await control(['bootstrap', `gui/${process.getuid()}`, paths.launchAgent]);
    const health = await healthy(candidate, oldPid, oldStatus?.service_generation);
      return {
      ...health,
      rollback: async () => {
        await unload();
        if (digest(oldPath) !== oldHash) throw new Error('previous payload changed; automatic rollback refused');
        atomicWrite(paths.launchAgent, oldPlist, 0o600);
        await control(['bootstrap', `gui/${process.getuid()}`, paths.launchAgent]);
        return healthy({ path: oldPath, sha256: oldHash, version: oldStatus?.identity?.daemon?.version }, oldPid, oldStatus?.service_generation);
      },
    };
  } catch (error) {
    if (!oldPid) {
      try {
        await unload();
        atomicWrite(paths.launchAgent, oldPlist, 0o600);
        error.rollback = { attempted: true, restored: false, error: 'no previous service identity to verify' };
      } catch (rollbackError) {
        error.rollback = { attempted: true, restored: false, error: rollbackError.message };
      }
      throw error;
    }
    try {
      await unload();
      if (digest(oldPath) !== oldHash) throw new Error('previous payload changed; automatic rollback refused');
      atomicWrite(paths.launchAgent, oldPlist, 0o600);
      await control(['bootstrap', `gui/${process.getuid()}`, paths.launchAgent]);
      error.rollback = await healthy({ path: oldPath, sha256: oldHash, version: oldStatus?.identity?.daemon?.version }, oldPid, oldStatus?.service_generation);
    } catch (rollbackError) { error.rollbackError = rollbackError.message; }
    throw error;
  }
}
