import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { atomicWrite, jsonBytes, sha256 } from '../fs-atomic.mjs';
import { CliError } from '../errors.mjs';
import { CLI_ENTRY_NAME, DAEMON_BIN_NAME, NATIVE_DIR_NAME } from '../constants.mjs';
import { reconcileCodexHomes } from './reconcile.mjs';
import { packageVersion, packageRoot } from './layout.mjs';
import { loadUpdateState } from './recovery.mjs';
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
  const { state, recovery } = loadUpdateState(paths.state);
  return { state: state ?? { schema_version: SCHEMA_VERSION, candidate: null, active: null, phase: 'idle' }, recovery };
}

// npm replaces the package directory in place, so the bytes the published
// active identity was verified against disappear with the next install.  The
// retained payload store keeps byte-for-byte copies of the verified release
// (native artifacts, stable entry, release manifest) under versioned product
// data, outside the directory npm is about to overwrite: active/rollback keep
// pointing at readable, verifiable old bytes.
function retainPayload(paths, candidateRoot, verified) {
  const root = path.join(paths.data, 'payload-store', verified.version);
  fs.mkdirSync(root, { recursive: true, mode: 0o700 });
  const platformDir = path.join(fs.realpathSync(candidateRoot), 'npm', NATIVE_DIR_NAME, verified.payload.platform);
  for (const file of verified.payload.files) {
    atomicWrite(path.join(root, file.name), fs.readFileSync(path.join(platformDir, file.name)), 0o755);
  }
  atomicWrite(path.join(root, CLI_ENTRY_NAME), fs.readFileSync(verified.stable.entry), 0o755);
  atomicWrite(path.join(root, 'payload.json'), fs.readFileSync(path.join(platformDir, 'payload.json')));
  const retained = {
    root,
    entry: path.join(root, CLI_ENTRY_NAME),
    entry_sha256: verified.stable.digest,
    daemon_entry: path.join(root, DAEMON_BIN_NAME),
    daemon_entry_sha256: verified.daemon.digest,
  };
  for (const [file, digest] of [[retained.daemon_entry, verified.daemon.digest], [retained.entry, verified.stable.digest]]) {
    if (sha256(fs.readFileSync(file)) !== digest) {
      throw new CliError('PAYLOAD_RETENTION_FAILED', `retained copy of ${path.basename(file)} does not match the verified digest`);
    }
  }
  return retained;
}

function verifyStableEntry(candidateRoot) {
  const root = fs.realpathSync(candidateRoot);
  const entry = path.join(root, 'bin', CLI_ENTRY_NAME);
  let stat;
  try { stat = fs.lstatSync(entry); } catch { throw new CliError('PAYLOAD_ENTRY_MISSING', 'candidate stable entry is missing'); }
  if (!stat.isFile() || stat.isSymbolicLink()) throw new CliError('PAYLOAD_ENTRY_INVALID', 'candidate stable entry must be a regular file');
  if ((stat.mode & 0o111) === 0) throw new CliError('PAYLOAD_ENTRY_INVALID', 'candidate stable entry must be executable');
  const resolved = fs.realpathSync(entry);
  if (resolved !== entry || !resolved.startsWith(`${root}${path.sep}`)) throw new CliError('PAYLOAD_ENTRY_INVALID', 'candidate stable entry escapes candidate root');
  const digest = crypto.createHash('sha256').update(fs.readFileSync(entry)).digest('hex');
  return { entry, digest };
}

// The service runs the native daemon binary, never the npm bin shim: the
// activation identity is the external-subagentd payload artifact whose digest
// verifyPayload already checked against the release manifest.
function verifyDaemonArtifact(candidateRoot, payload) {
  const record = payload.files.find((file) => file.name === DAEMON_BIN_NAME);
  if (!record) throw new CliError('PAYLOAD_DAEMON_MISSING', 'candidate payload does not carry the daemon artifact');
  const root = fs.realpathSync(candidateRoot);
  const entry = path.join(root, 'npm', NATIVE_DIR_NAME, payload.platform, record.name);
  let stat;
  try { stat = fs.lstatSync(entry); } catch { throw new CliError('PAYLOAD_DAEMON_MISSING', 'candidate daemon artifact is missing'); }
  if (!stat.isFile() || stat.isSymbolicLink()) throw new CliError('PAYLOAD_ENTRY_INVALID', 'candidate daemon artifact must be a regular file');
  if ((stat.mode & 0o111) === 0) throw new CliError('PAYLOAD_ENTRY_INVALID', 'candidate daemon artifact must be executable');
  const digest = crypto.createHash('sha256').update(fs.readFileSync(entry)).digest('hex');
  if (digest !== record.sha256) throw new CliError('PAYLOAD_DIGEST_MISMATCH', 'candidate daemon artifact does not match the verified payload digest');
  return { entry, digest };
}

// The pure validation prologue of a locked update, extracted so the
// coordinator can run the SAME rules (verifyPayload, the stable-entry and
// daemon-artifact owners) before it takes the working daemon offline.  It
// writes nothing — no install state, no receipt — never contacts the daemon
// or a provider, and derives everything from the candidate root, never the
// user HOME, so a rejected candidate cannot disturb a working installation.
export function preflightUpdate(options = {}) {
  // An explicit update always has a candidate root: the controlled staged
  // payload when the caller passes one, otherwise the installed package the
  // CLI itself runs from.  There is no rootless "version-only" activation.
  const candidateRoot = options.candidateRoot || packageRoot();
  const requested = options.version || 'current';
  const version = requested === 'current' ? packageVersion() : requested;
  const available = options.availableVersions || [packageVersion()];
  if (typeof version !== 'string' || !/^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$/.test(version) || !available.includes(version)) {
    throw new CliError('PAYLOAD_VERSION_UNAVAILABLE', `requested payload version is unavailable: ${version}`);
  }
  const payload = verifyPayload({ root: candidateRoot, platform: options.platform });
  if (version !== payload.version) throw new CliError('PAYLOAD_VERSION_MISMATCH', `requested version ${version} differs from candidate ${payload.version}`);
  // Reject a doomed candidate up front: an unusable stable entry or daemon
  // artifact must never publish candidate state or touch a single Codex home.
  const stable = verifyStableEntry(candidateRoot);
  const daemon = verifyDaemonArtifact(candidateRoot, payload);
  return { candidateRoot, version, payload, stable, daemon };
}

export function updateInstallation(paths, options = {}) {
  if (options.dryRun) return { dry_run: true, phase: 'candidate', version: options.version || 'current' };
  return withLock(paths, () => {
    const { candidateRoot, payload, stable, daemon } = preflightUpdate(options);
    const verifiedVersion = payload.version;
    const { state: prior, recovery } = readState(paths);
    const state = { schema_version: SCHEMA_VERSION, candidate: { version: verifiedVersion, root: candidateRoot, payload: payload.files }, active: prior.active, phase: 'candidate', updated_at_ms: Date.now() };
    atomicWrite(paths.state, jsonBytes(state));
    const publishedCandidate = state.candidate;
    try {
      const sync = options.deferCodexSync ? { homes: [], all_updated: true, deferred: true } : reconcileCodexHomes(paths, options);
      const ok = sync.homes.length === 0 || sync.all_updated;
      state.phase = ok ? 'active' : (sync.homes.some((h) => h.status === 'failed') ? 'failed' : 'partial');
      if (ok) {
        // Re-verify immediately before publishing: active may only ever point
        // at the entry bytes verified above, never a candidate mutated since.
        const stableNow = verifyStableEntry(candidateRoot);
        const daemonNow = verifyDaemonArtifact(candidateRoot, payload);
        if (stableNow.entry !== stable.entry || stableNow.digest !== stable.digest
          || daemonNow.entry !== daemon.entry || daemonNow.digest !== daemon.digest) {
          throw new CliError('PAYLOAD_ENTRY_CHANGED', 'candidate verified artifacts changed during activation');
        }
        state.active = state.candidate; state.candidate = null;
        state.active.entry = stableNow.entry;
        state.active.entry_sha256 = stableNow.digest;
        state.active.daemon_entry = daemonNow.entry;
        state.active.daemon_entry_sha256 = daemonNow.digest;
        state.active.retained = retainPayload(paths, candidateRoot, { version: verifiedVersion, payload, stable: stableNow, daemon: daemonNow });
      }
      atomicWrite(paths.state, jsonBytes(state));
      return recovery ? { ...state, recovery } : state;
    } catch (error) {
      // Record retryable failure evidence instead of a phantom in-flight
      // candidate: the prior active stays published and the rejected
      // candidate remains identified for the next attempt.
      const evidence = { ...state, candidate: state.candidate ?? publishedCandidate, active: prior.active, phase: 'failed', error: { code: error.code || 'UPDATE_FAILED', message: error.message }, failed_at_ms: Date.now() };
      try { atomicWrite(paths.state, jsonBytes(evidence)); } catch { /* evidence is best effort; the thrown error stays authoritative */ }
      throw error;
    }
  });
}

export function reconcileInstallation(paths, options = {}) {
  if (options.cancelActive && !options.yes) throw new CliError('CONFIRMATION_REQUIRED', '--cancel-active requires --yes');
  return withLock(paths, () => readState(paths).state);
}
