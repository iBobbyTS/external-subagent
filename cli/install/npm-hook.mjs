import { productPaths } from '../paths.mjs';
import { npmUpdateCoordination } from './reconcile.mjs';
import { updateCommand } from '../commands/update.mjs';
import { callDaemon } from '../rpc.mjs';

// npm runs this hook after replacing an installed package.  A package that
// has never been initialized must remain stage-only; only an initialized
// installation with a locally detectable active-version drift enters the
// existing reconcile owner.  No provider probe or daemon RPC is performed by
// the detection itself.
const paths = productPaths();
const globalInstall = process.env.npm_config_global === 'true';
let detected;
try {
  detected = npmUpdateCoordination(paths);
} catch (error) {
  process.stderr.write(`${JSON.stringify({ external_subagent: 'postinstall', coordinated: false, error: { code: error.code || 'UPDATE_STATE_READ_FAILED', message: error.message } })}\n`);
  process.exitCode = 1;
  detected = null;
}

if (!globalInstall || !detected || !detected.initialized || !detected.update_pending) {
  process.stdout.write(`${JSON.stringify({ external_subagent: 'postinstall', ...detected })}\n`);
  process.exit(0);
}

let interrupted = false;
let handlingSignal = null;
const abortOnSignal = (signal) => {
  if (handlingSignal) return;
  interrupted = true;
  handlingSignal = callDaemon(paths.socket, 'drain-abort', {})
    .catch(() => null)
    .finally(() => { process.exitCode = signal === 'SIGINT' ? 130 : 143; });
};
process.once('SIGINT', () => abortOnSignal('SIGINT'));
process.once('SIGTERM', () => abortOnSignal('SIGTERM'));

try {
  const result = await updateCommand(paths, ['reconcile'], { drainTimeoutMs: 30_000 });
  if (interrupted) process.exit(process.exitCode || 130);
  process.stdout.write(`${JSON.stringify({ external_subagent: 'postinstall', coordinated: true, result })}\n`);
} catch (error) {
  if (handlingSignal) await handlingSignal;
  process.stderr.write(`${JSON.stringify({ external_subagent: 'postinstall', coordinated: false, error: { code: error.code || 'UPDATE_FAILED', message: error.message } })}\n`);
  process.exitCode = 1;
}
