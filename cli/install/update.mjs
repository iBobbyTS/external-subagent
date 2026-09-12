import fs from 'node:fs';
import path from 'node:path';
import { atomicWrite, jsonBytes } from '../fs-atomic.mjs';
import { CliError } from '../errors.mjs';

const SCHEMA_VERSION = 2;
const lockPath = (paths) => path.join(paths.data, 'install.lock');

function withLock(paths, fn) {
  fs.mkdirSync(paths.data, { recursive: true, mode: 0o700 });
  let fd;
  try { fd = fs.openSync(lockPath(paths), 'wx', 0o600); } catch (error) {
    if (error.code === 'EEXIST') throw new CliError('UPDATE_IN_PROGRESS', 'another installation update is in progress');
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
    const version = options.version || 'current';
    const state = { schema_version: SCHEMA_VERSION, candidate: { version }, active: { version }, phase: 'active', updated_at_ms: Date.now() };
    state.candidate = null;
    atomicWrite(paths.state, jsonBytes(state));
    return state;
  });
}

export function reconcileInstallation(paths, options = {}) {
  if (options.cancelActive && !options.yes) throw new CliError('CONFIRMATION_REQUIRED', '--cancel-active requires --yes');
  return withLock(paths, () => readState(paths));
}
