import fs from 'node:fs';
import path from 'node:path';
import { CliError } from './errors.mjs';
import { jsonBytes, sha256 } from './fs-atomic.mjs';
import { platform, productPaths } from './paths.mjs';
import { bootoutService } from './install/service-macos.mjs';

function copyTree(source, destination, records, root = source) {
  if (!fs.existsSync(source)) return;
  for (const entry of fs.readdirSync(source, { withFileTypes: true })) {
    const from = path.join(source, entry.name);
    const relative = path.relative(root, from);
    const to = path.join(destination, relative);
    if (entry.isSymbolicLink()) throw new CliError('UNSAFE_BACKUP_ENTRY', `refusing symlink: ${from}`);
    if (entry.isDirectory()) copyTree(from, destination, records, root);
    else if (entry.isFile()) {
      fs.mkdirSync(path.dirname(to), { recursive: true, mode: 0o700 });
      const bytes = fs.readFileSync(from);
      fs.writeFileSync(to, bytes, { mode: 0o600 });
      records.push({ path: relative, bytes: bytes.length, sha256: sha256(bytes) });
    }
  }
}

export function backupData(destination, paths = productPaths()) {
  const resolved = path.resolve(destination);
  const source = path.resolve(paths.data);
  const relativeToSource = path.relative(source, resolved);
  if (relativeToSource === '' || (!relativeToSource.startsWith('..') && !path.isAbsolute(relativeToSource))) {
    throw new CliError('BACKUP_DESTINATION_IN_DATA', 'backup destination must not be inside product data');
  }
  if (fs.existsSync(resolved)) throw new CliError('BACKUP_EXISTS', 'backup destination already exists');
  fs.mkdirSync(resolved, { recursive: false, mode: 0o700 });
  const records = [];
  copyTree(paths.data, path.join(resolved, 'data'), records);
  const manifest = { schema_version: 1, product: 'external-subagent', files: records.sort((a, b) => a.path.localeCompare(b.path)) };
  fs.writeFileSync(path.join(resolved, 'manifest.json'), jsonBytes(manifest), { mode: 0o600 });
  return { destination: resolved, files: records.length };
}

export function restoreData(source, paths = productPaths()) {
  const resolved = path.resolve(source);
  const manifest = JSON.parse(fs.readFileSync(path.join(resolved, 'manifest.json'), 'utf8'));
  if (manifest.product !== 'external-subagent' || !Array.isArray(manifest.files)) throw new CliError('BACKUP_INVALID', 'backup manifest is invalid');
  for (const record of manifest.files) {
    if (path.isAbsolute(record.path) || record.path.split(path.sep).includes('..')) throw new CliError('BACKUP_INVALID', 'backup contains an unsafe path');
    const bytes = fs.readFileSync(path.join(resolved, 'data', record.path));
    if (bytes.length !== record.bytes || sha256(bytes) !== record.sha256) throw new CliError('BACKUP_CORRUPT', `backup verification failed: ${record.path}`);
  }
  const temporary = `${paths.data}.restore-${process.pid}`;
  if (fs.existsSync(temporary)) fs.rmSync(temporary, { recursive: true });
  fs.mkdirSync(temporary, { recursive: true, mode: 0o700 });
  copyTree(path.join(resolved, 'data'), temporary, []);
  const displaced = `${paths.data}.previous-${process.pid}`;
  try {
    if (fs.existsSync(paths.data)) fs.renameSync(paths.data, displaced);
    fs.renameSync(temporary, paths.data);
    if (fs.existsSync(displaced)) fs.rmSync(displaced, { recursive: true });
  } catch (error) {
    if (!fs.existsSync(paths.data) && fs.existsSync(displaced)) fs.renameSync(displaced, paths.data);
    throw error;
  }
  return { restored: true, files: manifest.files.length };
}

function removeOne(target) {
  try {
    const stat = fs.lstatSync(target);
    if (stat.isDirectory() && !stat.isSymbolicLink()) fs.rmSync(target, { recursive: true });
    else fs.unlinkSync(target);
    return true;
  } catch (error) {
    if (error?.code === 'ENOENT') return false;
    throw error;
  }
}

// Deleting the plist alone leaves a job launchd already loaded running until
// the next logout (observed live: uninstall reported success while the daemon
// process, the launchd registration, and the RPC socket all stayed alive).
// The ES-owned service is therefore booted out — with the bounded
// removal-confirmation bootoutService owns — before its definition file is
// removed.  A service that is not registered is not an error (idempotent
// uninstall); a bootout that cannot complete fails the command rather than
// deleting the definition out from under a still-running service.  The
// launchd bootout and the `removed_launch_agent` field are macOS-only; on
// Linux the systemd stop and `removed_service_definition` are wired by S03,
// and S02 removes the definition file without inventing a launchd result.
export function uninstall(paths = productPaths(), options = {}) {
  const macos = platform() === 'darwin';
  const service = macos
    ? bootoutService(paths, process.getuid(), options)
    : { already_stopped: true, skipped: true, reason: 'systemd service backend is wired in S03' };
  const removed = removeOne(paths.launchAgent);
  return {
    service_stopped: true,
    service_already_stopped: Boolean(service.already_stopped),
    ...(macos ? { removed_launch_agent: removed } : { removed_service_definition: removed }),
    data_retained: true,
    data: paths.data,
  };
}

export function purge(paths = productPaths()) {
  return { purged: removeOne(paths.data), logs_purged: removeOne(paths.logs) };
}
