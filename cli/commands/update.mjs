import { reconcileCodexHomes } from '../install/reconcile.mjs';
import { reconcileInstallation, updateInstallation } from '../install/update.mjs';
import { callDaemon } from '../rpc.mjs';
import fs from 'node:fs';

const receiptPath = (paths) => `${paths.state}.activation.json`;

export async function updateCommand(paths, args = [], daemon = {}) {
  const cancelActive = args.includes('--cancel-active');
  const yes = args.includes('--yes');
  if (cancelActive && !yes) throw new Error('--cancel-active requires --yes');
  const prior = (() => { try { return JSON.parse(fs.readFileSync(receiptPath(paths), 'utf8')); } catch { return null; } })();
  if (prior?.status === 'success') return prior.result;
  const socket = daemon.socket || process.env.ZCODE_AGENTD_SOCKET || paths.socket;
  const begin = await callDaemon(socket, 'drain', {});
  let status = begin;
  for (let i = 0; i < 30 && !status.ready_for_activation; i += 1) {
    await new Promise((resolve) => setTimeout(resolve, 50));
    status = await callDaemon(socket, 'drain-status', {});
  }
  if (!status.ready_for_activation) throw new Error('daemon drain timed out before activation readiness');
  const activation = await callDaemon(socket, 'activate-ready', {});
  if (!activation.activation_claim) return { ...status, activation_claim: null, update: 'not_activated' };
  let result;
  try {
    result = args.includes('reconcile')
      ? { ...reconcileInstallation(paths, { cancelActive, yes }), homes: reconcileCodexHomes(paths) }
      : updateInstallation(paths, { version: args.find((arg) => arg.startsWith('--version='))?.slice(10) });
    fs.writeFileSync(receiptPath(paths), JSON.stringify({ claim: activation.activation_claim, status: 'success', result }));
    return result;
  } catch (error) {
    fs.writeFileSync(receiptPath(paths), JSON.stringify({ claim: activation.activation_claim, status: 'failed', error: error.message }));
    throw error;
  }
}
