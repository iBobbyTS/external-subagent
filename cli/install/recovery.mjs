import fs from 'node:fs';
import { CliError } from '../errors.mjs';
import { atomicWrite, jsonBytes, readOptional, restoreOptional, sha256 } from '../fs-atomic.mjs';

// Install-state snapshots, step journal, and rollback.  A failed init restores
// every tracked file to its prior bytes (or removes files it created) and
// removes directories it created, so a partial installation never masquerades
// as a complete one.

export function loadInstallState(file) {
  const bytes = readOptional(file);
  if (bytes === null) return { schema_version: 1, completed: [] };
  try { return JSON.parse(bytes); } catch { throw new CliError('INVALID_INSTALL_STATE', 'install state is invalid JSON'); }
}

// The updater's schema-2 state (candidate/active/phase).  Corrupted bytes
// are evidence, never an empty state: the original file is preserved under a
// timestamped name so the recovery can be inspected, and the caller restarts
// from an empty state with the preservation reported alongside the result.
export function loadUpdateState(file) {
  const bytes = readOptional(file);
  if (bytes === null) return { state: null, recovery: null };
  try {
    const state = JSON.parse(bytes);
    if (!state || typeof state !== 'object' || Array.isArray(state)) {
      throw new SyntaxError('update state is not a JSON object');
    }
    return { state, recovery: null };
  } catch (error) {
    const backup = `${file}.corrupt-${Date.now()}`;
    try { fs.renameSync(file, backup); } catch (renameError) {
      throw new CliError('INSTALL_STATE_CORRUPT', `install state is corrupted and could not be preserved: ${renameError.message}`);
    }
    return {
      state: null,
      recovery: { status: 'recovered', code: 'INSTALL_STATE_CORRUPT', backup, error: error.message },
    };
  }
}

export function markInstallStep(file, completed, id) {
  completed.add(id);
  atomicWrite(file, jsonBytes({ schema_version: 1, completed: [...completed] }));
}

export function snapshotFile(file) {
  const bytes = readOptional(file);
  return { bytes, sha256: bytes === null ? null : sha256(bytes) };
}

export function restoreSnapshotFile(file, snapshot) {
  if (snapshot.bytes !== null && sha256(snapshot.bytes) !== snapshot.sha256) {
    throw new Error(`snapshot hash mismatch for ${file}`);
  }
  restoreOptional(file, snapshot.bytes);
  const restored = readOptional(file);
  if ((snapshot.bytes === null && restored !== null)
    || (snapshot.bytes !== null && (!restored || sha256(restored) !== snapshot.sha256))) {
    throw new Error(`rollback verification failed for ${file}`);
  }
}

export function rollbackFiles(tracked) {
  const rollbackErrors = [];
  for (const entry of Object.values(tracked)) {
    try { restoreSnapshotFile(entry.file, entry.snapshot); } catch (rollbackError) { rollbackErrors.push(rollbackError); }
  }
  return rollbackErrors;
}

export function removeCreatedDirectories(directories) {
  const rollbackErrors = [];
  for (const [name, directory] of Object.entries(directories)) {
    if (!directory.existed && fs.existsSync(directory.path)) {
      try { fs.rmSync(directory.path, { recursive: true, force: true }); } catch (rollbackError) { rollbackErrors.push(rollbackError); }
    }
  }
  return rollbackErrors;
}
