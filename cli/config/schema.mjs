import { CliError } from '../errors.mjs';

export const CONFIG_SCHEMA_VERSION = 1;
export const AGENT_IDS = Object.freeze(['zcode', 'dsh']);

export function defaultConfig() {
  return {
    schema_version: CONFIG_SCHEMA_VERSION,
    revision: 0,
    default_agent: null,
    agents: {
      zcode: { enabled: true, spawn_supported: true, default_model: null },
      dsh: { enabled: false, spawn_supported: false, default_model: null },
    },
  };
}

function rejectModel(agent, value, field) {
  if (agent === 'zcode' && value != null) {
    throw new CliError('model_selection_unsupported', `${field} is unsupported for zcode`, 2);
  }
}

export function validateConfig(input) {
  if (!input || typeof input !== 'object' || Array.isArray(input)) throw new CliError('CONFIG_INVALID', 'config must be an object', 2);
  const config = defaultConfig();
  if (input.schema_version !== undefined && input.schema_version !== CONFIG_SCHEMA_VERSION) throw new CliError('CONFIG_INVALID', 'unsupported config schema version', 2);
  if (input.revision !== undefined && (!Number.isInteger(input.revision) || input.revision < 0)) throw new CliError('CONFIG_INVALID', 'revision must be a non-negative integer', 2);
  if (input.default_agent !== undefined && input.default_agent !== null && !AGENT_IDS.includes(input.default_agent)) throw new CliError('CONFIG_INVALID', 'default_agent is unknown', 2);
  config.revision = input.revision ?? 0;
  config.default_agent = input.default_agent ?? null;
  for (const agent of AGENT_IDS) {
    const value = input.agents?.[agent];
    if (value === undefined) continue;
    if (!value || typeof value !== 'object' || Array.isArray(value)) throw new CliError('CONFIG_INVALID', `agents.${agent} must be an object`, 2);
    if (value.enabled !== undefined && typeof value.enabled !== 'boolean') throw new CliError('CONFIG_INVALID', `agents.${agent}.enabled must be boolean`, 2);
    if (value.spawn_supported !== undefined && typeof value.spawn_supported !== 'boolean') throw new CliError('CONFIG_INVALID', `agents.${agent}.spawn_supported must be boolean`, 2);
    rejectModel(agent, value.default_model, 'default_model');
    config.agents[agent] = { ...config.agents[agent], ...value };
  }
  config.agents.dsh.spawn_supported = false;
  if (config.default_agent && !config.agents[config.default_agent].enabled) throw new CliError('CONFIG_INVALID', 'default_agent must be enabled', 2);
  return config;
}

export function validateSpawnSelection(config, input) {
  const agent = input?.agent ?? config.default_agent;
  if (!agent) throw new CliError('agent_required', 'agent is required when no default_agent is configured', 2);
  if (!AGENT_IDS.includes(agent)) throw new CliError('agent_unknown', `unknown agent: ${agent}`, 2);
  const record = config.agents[agent];
  if (!record.enabled) throw new CliError('agent_disabled', `agent is disabled: ${agent}`, 2);
  rejectModel(agent, input?.model, 'model');
  rejectModel(agent, record.default_model, 'default_model');
  if (!record.spawn_supported) throw new CliError('agent_unsupported', `agent is not spawn-supported: ${agent}`, 2);
  return agent;
}
