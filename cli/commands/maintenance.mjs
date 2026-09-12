import { backupData, cleanupLegacy, purge, restoreData, uninstall as removeService } from '../maintenance.mjs';
import { unregisterAllCodexHomes } from '../install/reconcile.mjs';

// Maintenance command coordination.  Uninstalling releases every claimed
// Codex home from the D08 registry before removing the service registration;
// product data is retained by contract, and the empty registry travels with
// it so no home remains claimed.

export function uninstall(paths) {
  const released = unregisterAllCodexHomes(paths);
  return { ...removeService(paths), codex_homes_unregistered: released.unregistered };
}

export { backupData, cleanupLegacy, purge, restoreData };
