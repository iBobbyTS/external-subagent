import { reconcileCodexHomes } from '../install/reconcile.mjs';
import { reconcileInstallation, updateInstallation } from '../install/update.mjs';

export function updateCommand(paths, args = []) {
  const cancelActive = args.includes('--cancel-active');
  const yes = args.includes('--yes');
  if (args.includes('reconcile')) return { ...reconcileInstallation(paths, { cancelActive, yes }), homes: reconcileCodexHomes(paths) };
  return updateInstallation(paths, { version: args.find((arg) => arg.startsWith('--version='))?.slice(10) });
}
