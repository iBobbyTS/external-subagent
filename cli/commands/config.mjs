import { readConfig } from '../config/read.mjs';
import { writeConfig } from '../config/write.mjs';

export function configCommand(paths, input = {}) {
  const current = readConfig(paths.config);
  if (input.operation === 'set') {
    const next = { ...current, ...(input.patch || {}), agents: { ...current.agents, ...(input.patch?.agents || {}) } };
    delete next.patch;
    return { config: writeConfig(paths.config, next) };
  }
  return { config: current };
}
