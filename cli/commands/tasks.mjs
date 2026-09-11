import { CliError } from '../errors.mjs';

const SPAWN_FIELDS = new Set(['agent', 'repository', 'permission_mode', 'prompt', 'model', 'write_manifest']);

export function prepareSpawnInput(input) {
  if (!input || typeof input !== 'object' || Array.isArray(input)) throw new CliError('INVALID_ARGUMENT', 'spawn input must be an object', 2);
  for (const key of Object.keys(input)) {
    if (!SPAWN_FIELDS.has(key)) throw new CliError('INVALID_ARGUMENT', `spawn contains unsupported field: ${key}`, 2);
  }
  if (Object.prototype.hasOwnProperty.call(input, 'agent') && input.agent === null) {
    throw new CliError('agent_required', 'agent must be omitted or a supported agent id; null is invalid', 2);
  }
  if (input.agent !== undefined && (typeof input.agent !== 'string' || input.agent.length === 0)) {
    throw new CliError('INVALID_ARGUMENT', 'agent must be a non-empty string', 2);
  }
  if (Object.prototype.hasOwnProperty.call(input, 'model') && input.model === null) {
    throw new CliError('INVALID_ARGUMENT', 'model must be omitted or a non-null model token', 2);
  }
  if (input.model !== undefined && (typeof input.model !== 'string' || input.model.length === 0)) {
    throw new CliError('INVALID_ARGUMENT', 'model must be a non-empty string', 2);
  }
  return { ...input };
}
