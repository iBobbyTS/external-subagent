import { backupData, cleanupLegacy, purge, restoreData, uninstall as removeService } from '../maintenance.mjs';
import { unregisterAllCodexHomes } from '../install/reconcile.mjs';

// Maintenance command coordination.  The ES-owned service is booted out and
// its definition removed before any D08 claim is released: a bootout that
// cannot complete fails the command with the registry exactly as it was, so
// an uninstall retry (or a reconcile) still sees every claimed home instead
// of a wiped registry.  Only a fully removed service releases the claims;
// product data is retained by contract, and the emptied registry travels
// with it so no home remains claimed.

export function uninstall(paths, options = {}) {
  const removed = removeService(paths, options);
  const released = unregisterAllCodexHomes(paths);
  return { ...removed, codex_homes_unregistered: released.unregistered };
}

export { backupData, cleanupLegacy, purge, restoreData };
