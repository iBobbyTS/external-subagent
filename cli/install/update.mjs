import fs from 'node:fs';
import path from 'node:path';
import { atomicWrite, jsonBytes } from '../fs-atomic.mjs';
import { CliError } from '../errors.mjs';
import { reconcileCodexHomes } from './reconcile.mjs';
import { packageVersion, packageRoot } from './layout.mjs';
import { verifyPayload } from './payload.mjs';

const SCHEMA_VERSION = 2;
const lockPath = (paths) => path.join(paths.data, 'install.lock');

function withLock(paths, fn) {
  fs.mkdirSync(paths.data, { recursive: true, mode: 0o700 });
  let fd;
  try { fd = fs.openSync(lockPath(paths), 'wx', 0o600); fs.writeSync(fd, JSON.stringify({ pid: process.pid, started_at_ms: Date.now() })); } catch (error) {
    if (error.code === 'EEXIST') {
      let stale = false;
      try {
        const lock = JSON.parse(fs.readFileSync(lockPath(paths), 'utf8'));
        if (!Number.isInteger(lock.pid) || lock.pid <= 0) stale = true;
        else { try { process.kill(lock.pid, 0); } catch (probe) { stale = probe.code === 'ESRCH'; } }
      } catch { stale = true; }
      if (stale) { try { fs.unlinkSync(lockPath(paths)); } catch {} return withLock(paths, fn); }
      throw new CliError('UPDATE_IN_PROGRESS', 'another installation update is in progress');
    }
    throw error;
  }
  try { return fn(); } finally { fs.closeSync(fd); fs.unlinkSync(lockPath(paths)); }
}

function readState(paths) {
  try { return JSON.parse(fs.readFileSync(paths.state, 'utf8')); } catch { return { schema_version: SCHEMA_VERSION, candidate: null, active: null, phase: 'idle' }; }
}

export function updateInstallation(paths, options = {}) {
  if (options.dryRun) return { dry_run: true, phase: 'candidate', version: options.version || 'current' };
  return withLock(paths, () => {
    const requested = options.version || 'current';
    const candidateRoot = options.candidateRoot || null;
    const payload = candidateRoot ? verifyPayload({ root: candidateRoot, platform: options.platform }) : null;
    const version = requested === 'current' ? packageVersion() : requested;
    const available = options.availableVersions || [packageVersion()];
    if (typeof version !== 'string' || !/^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$/.test(version) || !available.includes(version)) {
      throw new CliError('PAYLOAD_VERSION_UNAVAILABLE', `requested payload version is unavailable: ${version}`);
    }
    const prior = readState(paths);
    const state = { schema_version: SCHEMA_VERSION, candidate: { version, ...(candidateRoot ? { root: candidateRoot, payload: payload.files } : {}) }, active: prior.active, phase: 'candidate', updated_at_ms: Date.now() };
    atomicWrite(paths.state, jsonBytes(state));
    const sync = reconcileCodexHomes(paths, options);
    const ok = sync.homes.length === 0 || sync.all_updated;
    state.phase = ok ? 'active' : (sync.homes.some((h) => h.status === 'failed') ? 'failed' : 'partial');
    if (ok) { state.active = state.candidate; state.candidate = null; if (candidateRoot) state.active.entry = path.join(candidateRoot, 'bin', 'external-subagent'); }
    atomicWrite(paths.state, jsonBytes(state));
    return state;
  });
}

export function reconcileInstallation(paths, options = {}) {
  if (options.cancelActive && !options.yes) throw new CliError('CONFIRMATION_REQUIRED', '--cancel-active requires --yes');
  return withLock(paths, () => readState(paths));
}
