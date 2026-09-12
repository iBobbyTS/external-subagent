import { reconcileCodexHomes } from '../install/reconcile.mjs';
import { reconcileInstallation, updateInstallation } from '../install/update.mjs';
import { callDaemon } from '../rpc.mjs';
import fs from 'node:fs';
import { atomicWrite, jsonBytes } from '../fs-atomic.mjs';

const receiptPath = (paths) => `${paths.state}.activation.json`;

export async function updateCommand(paths, args = [], daemon = {}) {
  const cancelActive = args.includes('--cancel-active');
  const yes = args.includes('--yes');
  if (cancelActive && !yes) throw new Error('--cancel-active requires --yes');
  const prior = (() => { try { return JSON.parse(fs.readFileSync(receiptPath(paths), 'utf8')); } catch { return null; } })();
  const requestedVersion = args.find((arg) => arg.startsWith('--version='))?.slice(10) || 'current';
  const socket = daemon.socket || process.env.ZCODE_AGENTD_SOCKET || paths.socket;
  const rpc = daemon.callDaemon || callDaemon;
  const begin = await rpc(socket, 'drain', cancelActive ? { cancel_active: true } : {});
  let status = begin;
  while (!status.ready_for_activation) {
    await new Promise((resolve) => setTimeout(resolve, 50));
    status = await rpc(socket, 'drain-status', {});
  }
  const activation = await rpc(socket, 'activate-ready', {});
  if (!activation.activation_claim) return { ...status, activation_claim: null, update: 'not_activated' };
  if (prior?.status === 'success' && prior.claim === activation.activation_claim && prior.version === requestedVersion) return prior.result;
  let result;
  try {
    result = args.includes('reconcile')
      ? { ...reconcileInstallation(paths, { cancelActive, yes }), homes: reconcileCodexHomes(paths) }
      : await (daemon.updateInstallation || updateInstallation)(paths, { version: requestedVersion });
    if (result?.phase && result.phase !== 'active') {
      throw new Error(`installation update did not activate payload (phase=${result.phase})`);
    }
    const receiptVersion = result?.active?.version || result?.version || requestedVersion;
    atomicWrite(receiptPath(paths), jsonBytes({ claim: activation.activation_claim, version: receiptVersion, status: 'success', result }));
    return result;
  } catch (error) {
    atomicWrite(receiptPath(paths), jsonBytes({ claim: activation.activation_claim, version: requestedVersion, status: 'failed', error: error.message }));
    throw error;
  }
}
