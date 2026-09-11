import { readConfig } from '../config/read.mjs';
import { validateSpawnSelection } from '../config/schema.mjs';

export function agentsCommand(paths, input = {}) {
  const config = readConfig(paths.config);
  if (input.operation === 'validate_spawn') return { agent: validateSpawnSelection(config, input) };
  return { default_agent: config.default_agent, agents: Object.entries(config.agents).map(([id, value]) => ({ agent: id, ...value })) };
}
