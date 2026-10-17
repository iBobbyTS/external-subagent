import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { DAEMON_BIN_NAME, NATIVE_DIR_NAME } from '../constants.mjs';
import { CliError } from '../errors.mjs';
import { sha256 } from '../fs-atomic.mjs';
import { productPaths } from '../paths.mjs';
import { callDaemon } from '../rpc.mjs';
import { packageRoot } from './layout.mjs';

// The Windows service backend for the product daemon: a single SCM service
// registered through `sc.exe`, the structural twin of the launchd plist
// (service-macos.mjs) and the systemd user unit (service-linux.mjs).  The
// service command line pins the verified daemon executable and the product
// database/socket arguments; the SCM registration itself is the service
// definition (Windows has no file-based unit, so paths.launchAgent is null).
//
// Every `sc.exe` invocation is checkable: a non-zero exit is projected into a
// structured CliError (exit 5 = access denied, the expected non-elevated
// outcome on a normal user shell), never swallowed.  The injectable seam is
// `options.sc`, the twin of `options.launchctl`/`options.systemctl`, so the
// lifecycle contract is exercisable without touching a real SCM database.
export const WINDOWS_SERVICE_NAME = DAEMON_BIN_NAME;
// Pin the tool path exactly like the launchd/systemd twins pin their own
// binaries (/bin/launchctl, /usr/bin/systemctl).  SystemRoot is the Windows
// installation root; a missing/relative value falls back to the conventional
// C:\Windows.  Never resolve `sc.exe` through PATH (or the current directory):
// a CWD-shadowing executable must not be able to intercept service control.
function systemRoot(env = process.env) {
  const configured = env.SystemRoot;
  return typeof configured === 'string' && configured !== '' && path.win32.isAbsolute(configured)
    ? configured
    : 'C:\\Windows';
}

export const SC_EXE_PATH = path.win32.join(systemRoot(), 'System32', 'sc.exe');

const ERROR_ACCESS_DENIED = 5;
const ERROR_SERVICE_DOES_NOT_EXIST = 1060;
const ERROR_SERVICE_NOT_ACTIVE = 1062;
const ERROR_SERVICE_ALREADY_RUNNING = 1056;
const ERROR_SERVICE_EXISTS = 1073;
const ERROR_SERVICE_MARKED_FOR_DELETE = 1072;

// SCM service states (SERVICE_STATUS.dwCurrentState) that mean the service
// owns (or is acquiring) a live process right now.
const LIVE_STATES = new Set([2 /* START_PENDING */, 4 /* RUNNING */, 5 /* CONTINUE_PENDING */]);
const STOPPED_STATE = 1;

export function scExe(args) {
  const result = spawnSync(SC_EXE_PATH, args, { encoding: 'utf8', windowsHide: true });
  return {
    action: args[0],
    status: result.status,
    stdout: result.stdout,
    stderr: result.stderr,
    error: result.error,
  };
}

function controlFor(options) {
  return options.sc || scExe;
}

function runSc(control, args) {
  let result;
  try {
    result = control(args);
  } catch (error) {
    if (error instanceof CliError) throw error;
    throw new CliError('DAEMON_CONTROL_FAILED', `sc.exe ${args[0]} is unavailable: ${error.message}`);
  }
  if (result?.skipped) return result;
  if (!result || typeof result !== 'object') {
    throw new CliError('DAEMON_CONTROL_FAILED', `sc.exe ${args[0]} returned an invalid control result`);
  }
  if (result.error) {
    throw new CliError('DAEMON_CONTROL_FAILED', `sc.exe ${args[0]} is unavailable: ${result.error.message || result.error}`);
  }
  if (typeof result.status !== 'number') {
    throw new CliError('DAEMON_CONTROL_FAILED', `sc.exe ${args[0]} returned an invalid control result`);
  }
  return result;
}

// sc.exe emits localized text on some hosts; the ASCII projection keeps an
// actionable English fragment and never leaks mojibake into the error message.
function asciiDetail(result) {
  const text = `${result?.stderr || ''}${result?.stdout || ''}`
    .split(/\r?\n/u)
    .map((line) => line.replace(/[^\x20-\x7e]/gu, '').trim())
    .filter(Boolean)
    .join(' ');
  return text.length > 0 ? `: ${text.slice(0, 400)}` : '';
}

function scFailure(args, result) {
  if (result.status === ERROR_ACCESS_DENIED) {
    return new CliError(
      'SERVICE_ACCESS_DENIED',
      `sc.exe ${args[0]} was denied access (exit code 5)${asciiDetail(result)}; registering, starting, stopping, or deleting a Windows service requires an elevated (Administrator) session`,
    );
  }
  return new CliError('DAEMON_CONTROL_FAILED', `sc.exe ${args[0]} failed with exit code ${result.status}${asciiDetail(result)}`);
}

function parseServiceQuery(stdout) {
  const text = String(stdout || '');
  const state = text.match(/STATE\s*:\s*(\d+)\s+([A-Z_]+)/iu);
  const pid = text.match(/\bPID\s*:\s*(\d+)/iu);
  const stateCode = state ? Number(state[1]) : null;
  const processId = pid ? Number(pid[1]) : 0;
  return {
    state: state ? state[2].toLowerCase() : null,
    state_code: Number.isInteger(stateCode) ? stateCode : null,
    pid: Number.isInteger(processId) && processId > 0 ? processId : null,
  };
}

function queryService(control) {
  const args = ['queryex', WINDOWS_SERVICE_NAME];
  const result = runSc(control, args);
  if (result.skipped) return { skipped: true, reason: result.reason || 'sc.exe neutralized by test seam' };
  if (result.status === ERROR_SERVICE_DOES_NOT_EXIST) return { registered: false };
  if (result.status === ERROR_ACCESS_DENIED) {
    return { unavailable: true, reason: scFailure(args, result).message };
  }
  if (result.status !== 0) throw scFailure(args, result);
  return { registered: true, ...parseServiceQuery(result.stdout) };
}

function isLive(view) {
  return view.registered === true && LIVE_STATES.has(view.state_code);
}

function quoteServiceArgument(value, label) {
  if (typeof value !== 'string' || value.length === 0) {
    throw new CliError('SERVICE_DEFINITION_INVALID', `${label} must be a non-empty string`);
  }
  if (/["\r\n]/u.test(value)) {
    throw new CliError('SERVICE_DEFINITION_INVALID', `${label} cannot contain a quote, carriage return, or line feed; the Windows service command line cannot represent it`);
  }
  return `"${value}"`;
}

// The verified executable lives in the packaged payload once win32 support is
// activated; a development checkout falls back to the host cargo output so the
// library-level lifecycle is exercisable on a real machine before activation.
function resolveDaemonPath(options = {}) {
  if (typeof options.daemonPath === 'string' && options.daemonPath !== '') return options.daemonPath;
  const tuple = `win32-${process.arch}`;
  const packaged = path.join(packageRoot(), 'npm', NATIVE_DIR_NAME, tuple, `${DAEMON_BIN_NAME}.exe`);
  if (fs.existsSync(packaged)) return packaged;
  const development = path.join(packageRoot(), 'target', 'debug', `${DAEMON_BIN_NAME}.exe`);
  if (fs.existsSync(development)) return development;
  return packaged;
}

function resolvedPaths(paths) {
  if (paths && typeof paths.database === 'string' && typeof paths.socket === 'string') return paths;
  return productPaths();
}

// The service command line SCM stores in BINARY_PATH_NAME.  The executable and
// every argument value are quoted; a path containing a quote or newline is
// refused instead of producing an ambiguous service definition.
export function windowsServiceBinPath(paths, options = {}) {
  const daemon = resolveDaemonPath(options);
  return [
    quoteServiceArgument(daemon, 'daemon executable'),
    '--database',
    quoteServiceArgument(paths.database, '--database path'),
    '--socket',
    quoteServiceArgument(paths.socket, '--socket name'),
  ].join(' ');
}

// Read-only registration view for `status`.  A missing service is registered
// false; an access-denied probe degrades to an explicit unavailable view (the
// same shape the systemd backend uses without a user session) instead of
// failing the whole read-only command.
export function windowsServiceRegistrationStatus(options = {}) {
  const view = queryService(controlFor(options));
  if (view.skipped) return { query: 'skipped', registered: null, reason: view.reason };
  if (view.unavailable) return { query: 'unavailable', registered: null, reason: view.reason };
  if (!view.registered) return { query: 'available', registered: false };
  return {
    query: 'available',
    registered: true,
    service: WINDOWS_SERVICE_NAME,
    state: view.state,
    pid: view.pid,
  };
}

// Windows has no file-based service definition: the SCM registration created
// by bootstrap IS the definition.  This step therefore renders and validates
// the exact command line that bootstrap will register, so init can fail before
// touching the SCM when a path cannot be represented.
export function installServiceDefinitionWindows(paths, options = {}) {
  const resolved = resolvedPaths(paths);
  return {
    installed: true,
    backend: 'scm',
    service: WINDOWS_SERVICE_NAME,
    bin_path: windowsServiceBinPath(resolved, options),
    start_type: 'auto',
  };
}

function startService(control, existing, extra = {}) {
  if (isLive(existing)) {
    return {
      action: 'bootstrap', service: WINDOWS_SERVICE_NAME, already_loaded: true,
      state: existing.state ?? null, pid: existing.pid ?? null, ...extra,
    };
  }
  const start = runSc(control, ['start', WINDOWS_SERVICE_NAME]);
  if (start.status === ERROR_SERVICE_ALREADY_RUNNING) {
    return { action: 'bootstrap', service: WINDOWS_SERVICE_NAME, already_loaded: true, ...extra };
  }
  if (start.status !== 0) {
    // A definition this call just created must not survive a failed start:
    // init rollback cannot otherwise discover a half-created service.
    if (extra.created) {
      try { runSc(control, ['delete', WINDOWS_SERVICE_NAME]); } catch { /* best-effort self-clean */ }
    }
    throw scFailure(['start', WINDOWS_SERVICE_NAME], start);
  }
  const settled = queryService(control);
  return {
    action: 'bootstrap',
    service: WINDOWS_SERVICE_NAME,
    created: Boolean(extra.created),
    started: true,
    state: settled.skipped || !settled.registered ? null : settled.state,
    pid: settled.skipped || !settled.registered ? null : settled.pid,
  };
}

export function bootstrapServiceWindows(paths, options = {}) {
  const control = controlFor(options);
  const resolved = resolvedPaths(paths);
  const existing = queryService(control);
  if (existing.skipped) {
    return { action: 'bootstrap', service: WINDOWS_SERVICE_NAME, skipped: true, reason: existing.reason };
  }
  if (existing.unavailable) throw new CliError('SERVICE_ACCESS_DENIED', existing.reason);
  if (existing.registered) return startService(control, existing);

  const daemon = resolveDaemonPath(options);
  if (!fs.existsSync(daemon)) {
    throw new CliError('PAYLOAD_FILE_MISSING', `the daemon executable is missing: ${daemon}`);
  }
  const binPath = windowsServiceBinPath(resolved, { ...options, daemonPath: daemon });
  const create = runSc(control, ['create', WINDOWS_SERVICE_NAME, 'binPath=', binPath, 'start=', 'auto']);
  if (create.status === ERROR_SERVICE_EXISTS) {
    const settled = queryService(control);
    if (settled.skipped) {
      return { action: 'bootstrap', service: WINDOWS_SERVICE_NAME, skipped: true, reason: settled.reason };
    }
    if (settled.unavailable) throw new CliError('SERVICE_ACCESS_DENIED', settled.reason);
    if (settled.registered) return startService(control, settled);
    throw scFailure(['create', WINDOWS_SERVICE_NAME], create);
  }
  if (create.status !== 0) throw scFailure(['create', WINDOWS_SERVICE_NAME], create);
  return startService(control, { registered: true, state_code: null }, { created: true });
}

function waitForStopped(control, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const probe = queryService(control);
    if (probe.skipped || !probe.registered || probe.state_code === STOPPED_STATE) return;
    if (Date.now() >= deadline) {
      throw new CliError('SERVICE_UNLOAD_TIMEOUT', 'Windows service remained active after sc.exe stop');
    }
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 50);
  }
}

export function bootoutServiceWindows(paths, options = {}) {
  const control = controlFor(options);
  const resolved = resolvedPaths(paths);
  const existing = queryService(control);
  if (existing.skipped) {
    return { action: 'bootout', service: WINDOWS_SERVICE_NAME, already_stopped: true, skipped: true, reason: existing.reason };
  }
  if (existing.unavailable) throw new CliError('SERVICE_ACCESS_DENIED', existing.reason);
  if (!existing.registered) {
    return { action: 'bootout', service: WINDOWS_SERVICE_NAME, already_stopped: true };
  }
  if (existing.state_code !== STOPPED_STATE) {
    const stop = runSc(control, ['stop', WINDOWS_SERVICE_NAME]);
    if (stop.status !== 0
      && stop.status !== ERROR_SERVICE_NOT_ACTIVE
      && stop.status !== ERROR_SERVICE_DOES_NOT_EXIST) {
      throw scFailure(['stop', WINDOWS_SERVICE_NAME], stop);
    }
    waitForStopped(control, options.unloadTimeoutMs ?? 10_000);
  }
  const removed = runSc(control, ['delete', WINDOWS_SERVICE_NAME]);
  if (removed.status === ERROR_SERVICE_DOES_NOT_EXIST || removed.status === ERROR_SERVICE_MARKED_FOR_DELETE) {
    return { action: 'bootout', service: WINDOWS_SERVICE_NAME, removed: true, socket_removed: !fs.existsSync(resolved.socket) };
  }
  if (removed.status !== 0) throw scFailure(['delete', WINDOWS_SERVICE_NAME], removed);
  return { action: 'bootout', service: WINDOWS_SERVICE_NAME, removed: true, socket_removed: !fs.existsSync(resolved.socket) };
}

function serviceConfiguration(control) {
  const result = runSc(control, ['qc', WINDOWS_SERVICE_NAME]);
  if (result.skipped) return { skipped: true, reason: result.reason || 'sc.exe neutralized by test seam' };
  if (result.status === ERROR_SERVICE_DOES_NOT_EXIST) return { registered: false };
  if (result.status !== 0) throw scFailure(['qc', WINDOWS_SERVICE_NAME], result);
  const match = String(result.stdout || '').match(/^\s*BINARY_PATH_NAME\s*:\s*(.*)$/imu);
  return { registered: true, bin_path: match ? match[1].trim() : null };
}

// The executable SCM will launch — the Windows twin of the plist's first
// ProgramArguments entry and the systemd unit's ExecStart program.
function firstServiceProgram(binPath) {
  if (typeof binPath !== 'string' || binPath.trim() === '') return null;
  const text = binPath.trim();
  if (text.startsWith('"')) {
    const end = text.indexOf('"', 1);
    return end > 1 ? text.slice(1, end) : null;
  }
  const end = text.search(/\s/u);
  return (end === -1 ? text : text.slice(0, end)) || null;
}

// Activation health/rollback over SCM: the same drain/claim prelude, the same
// "a NEW process runs the selected artifact's exact bytes and self-reports its
// identity" verification, and the same rollback-to-retained-bytes contract as
// the launchd and systemd backends.  unload = stop + bounded STOPPED
// confirmation; load = sc.exe start; the definition update is sc.exe config
// on BINARY_PATH_NAME.
export async function activateServiceWindows(paths, candidate, options = {}) {
  const control = controlFor(options);
  const rpc = options.callDaemon || callDaemon;
  const configuration = serviceConfiguration(control);
  if (configuration.skipped) throw new CliError('SERVICE_NOT_VERIFIED', 'sc.exe control was skipped');
  if (!configuration.registered) throw new CliError('SERVICE_NOT_INSTALLED', 'the Windows service is not registered');
  const oldPath = firstServiceProgram(configuration.bin_path);
  if (!oldPath) throw new CliError('SERVICE_DEFINITION_INVALID', 'installed service has no executable entry');

  const servicePid = async () => {
    const view = queryService(control);
    if (view.skipped) throw new CliError('SERVICE_NOT_VERIFIED', 'sc.exe control was skipped');
    if (view.unavailable) throw new CliError('SERVICE_ACCESS_DENIED', view.reason);
    if (!view.registered) return null;
    return isLive(view) ? view.pid : null;
  };
  const oldPid = await servicePid();
  const oldStatus = oldPid ? await rpc(paths.socket, 'status', {}) : null;
  // The running daemon's self-reported digest is the truth about which bytes
  // the service was executing; the file at oldPath may already carry the NEXT
  // version after an npm install replaced the package directory in place.
  const runningArtifact = oldStatus?.identity?.daemon?.artifact;
  const oldSha = (typeof runningArtifact?.sha256 === 'string' && runningArtifact.sha256)
    || (fs.existsSync(oldPath) ? sha256(fs.readFileSync(oldPath)) : null);
  if (oldPid) {
    const drain = await rpc(paths.socket, 'drain-status', {});
    if (!drain.ready_for_activation) throw new CliError('DAEMON_NOT_DRAINED', 'daemon still owns active or unreaped work');
    const claim = await rpc(paths.socket, 'activate-ready', {});
    if (!claim.activation_claim) throw new CliError('ACTIVATION_NOT_CLAIMED', 'daemon did not grant activation');
  }
  const rollbackExecutable = () => {
    if (oldSha && fs.existsSync(oldPath) && sha256(fs.readFileSync(oldPath)) === oldSha) {
      return { path: oldPath, sha256: oldSha };
    }
    const retained = options.rollbackPayload;
    if (retained && typeof retained.path === 'string' && typeof retained.sha256 === 'string'
      && fs.existsSync(retained.path) && sha256(fs.readFileSync(retained.path)) === retained.sha256) {
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
    const view = queryService(control);
    if (view.skipped) return;
    if (view.unavailable) throw new CliError('SERVICE_ACCESS_DENIED', view.reason);
    if (!view.registered || view.state_code === STOPPED_STATE) return;
    const stop = runSc(control, ['stop', WINDOWS_SERVICE_NAME]);
    if (stop.status !== 0 && stop.status !== ERROR_SERVICE_NOT_ACTIVE && stop.status !== ERROR_SERVICE_DOES_NOT_EXIST) {
      throw scFailure(['stop', WINDOWS_SERVICE_NAME], stop);
    }
    waitForStopped(control, options.unloadTimeoutMs ?? 10_000);
  };
  const configure = (entryPath) => {
    const next = windowsServiceBinPath(paths, { ...options, daemonPath: entryPath });
    const result = runSc(control, ['config', WINDOWS_SERVICE_NAME, 'binPath=', next]);
    if (result.status !== 0) throw scFailure(['config', WINDOWS_SERVICE_NAME], result);
    return next;
  };
  const restoreDefinition = (entryPath) => configure(entryPath);
  const load = async () => {
    const start = runSc(control, ['start', WINDOWS_SERVICE_NAME]);
    if (start.status !== 0 && start.status !== ERROR_SERVICE_ALREADY_RUNNING) {
      throw scFailure(['start', WINDOWS_SERVICE_NAME], start);
    }
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
    configure(candidate.path);
    await load();
    const health = await healthy(candidate, oldPid, oldStatus?.service_generation);
    return {
      ...health,
      rollback: async () => {
        const restored = await rollbackService();
        if (oldSha && fs.existsSync(oldPath) && sha256(fs.readFileSync(oldPath)) !== oldSha) {
          return { ...restored, restored_from: 'retained_payload' };
        }
        return restored;
      },
    };
  } catch (error) {
    if (!oldPid) {
      try {
        await unload();
        restoreDefinition(oldPath);
        error.rollback = { attempted: true, restored: false, definition_restored: true, error: 'no previous service identity to verify' };
      } catch (rollbackError) {
        error.rollback = { attempted: true, restored: false, definition_restored: false, error: rollbackError.message };
      }
      throw error;
    }
    try {
      error.rollback = await rollbackService();
    } catch (rollbackError) { error.rollbackError = rollbackError.message; }
    throw error;
  }
}
