import fs from 'node:fs';
import path from 'node:path';
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

// Roll back the product-owned half of the Codex binding after a failed init:
// the marketplace manifest returns to its prior bytes, a staging tree this
// init created is removed, and directories this init created disappear while
// they are empty.  Directories that already existed — a pre-created
// CODEX_HOME, or one codex populated with its official cache — are never
// emptied or deleted; the codex-owned cache is not ours to roll back.  The
// product-namespaced marketplace root is the one exception: it is pruned
// whenever codex left it empty.
export function rollbackCodexArtifacts(artifacts) {
  const rollbackErrors = [];
  try {
    restoreSnapshotFile(artifacts.marketplace.file, artifacts.marketplace.snapshot);
  } catch (rollbackError) { rollbackErrors.push(rollbackError); }
  if (!artifacts.staging.existed && fs.existsSync(artifacts.staging.path)) {
    try { fs.rmSync(artifacts.staging.path, { recursive: true, force: true }); } catch (rollbackError) { rollbackErrors.push(rollbackError); }
  }
  const prunable = (directory) => directory === artifacts.productRoot
    || directory.startsWith(`${artifacts.productRoot}${path.sep}`)
    || !artifacts.preexisting.has(directory);
  const deepestFirst = [...artifacts.directories].sort((a, b) => b.length - a.length);
  for (const directory of deepestFirst) {
    if (!prunable(directory)) continue;
    try { fs.rmdirSync(directory); } catch (error) {
      // ENOTEMPTY means codex or the user owns the remaining content; ENOENT
      // means nothing was created there.  Neither is a rollback failure.
      if (error?.code !== 'ENOENT' && error?.code !== 'ENOTEMPTY') rollbackErrors.push(error);
    }
  }
  return rollbackErrors;
}
