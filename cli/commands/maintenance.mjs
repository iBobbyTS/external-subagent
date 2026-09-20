import { backupData, purge, restoreData, uninstall as removeService } from '../maintenance.mjs';
import { unregisterAllCodexHomes } from '../install/reconcile.mjs';
import { uninstallZcodePlugin } from '../install/zcode.mjs';

// Maintenance command coordination.  The ES-owned service is booted out and
// its definition removed before any D08 claim is released: a bootout that
// cannot complete fails the command with the registry exactly as it was, so
// an uninstall retry (or a reconcile) still sees every claimed home instead
// of a wiped registry.  Only a fully removed service releases the claims;
// product data is retained by contract, and the emptied registry travels
// with it so no home remains claimed.  The stateless ZCode binding is
// released the same way: the product-owned staging tree and its single
// plugins.dirs entry go away together.

export function uninstall(paths, options = {}) {
  const removed = removeService(paths, options);
  const released = unregisterAllCodexHomes(paths);
  // An unreadable ZCode config is foreign state, not a product claim: report
  // the release failure instead of failing the whole uninstall over it.
  let zcode;
  try { zcode = uninstallZcodePlugin(paths); } catch (error) {
    zcode = { uninstalled: false, error: { code: error.code || 'ZCODE_BINDING_FAILED', message: error.message } };
  }
  return { ...removed, codex_homes_unregistered: released.unregistered, zcode_binding: zcode };
}

export { backupData, purge, restoreData };
