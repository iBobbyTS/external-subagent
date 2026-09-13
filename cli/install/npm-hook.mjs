import { productPaths } from '../paths.mjs';
import { npmUpdateCoordination } from './reconcile.mjs';
import { updateCommand } from '../commands/update.mjs';

// npm runs this hook after replacing an installed package.  A package that
// has never been initialized must remain stage-only; only an initialized
// installation with a locally detectable active-version drift enters the
// existing reconcile owner.  No provider probe or daemon RPC is performed by
// the detection itself.
const paths = productPaths();
const detected = npmUpdateCoordination(paths);

if (!detected.initialized || !detected.update_pending) {
  process.stdout.write(`${JSON.stringify({ external_subagent: 'postinstall', ...detected })}\n`);
  process.exit(0);
}

try {
  const result = await updateCommand(paths, ['reconcile']);
  process.stdout.write(`${JSON.stringify({ external_subagent: 'postinstall', coordinated: true, result })}\n`);
} catch (error) {
  process.stderr.write(`${JSON.stringify({ external_subagent: 'postinstall', coordinated: false, error: { code: error.code || 'UPDATE_FAILED', message: error.message } })}\n`);
  process.exitCode = 1;
}
