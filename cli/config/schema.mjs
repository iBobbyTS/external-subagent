import { CliError } from '../errors.mjs';

export const CONFIG_SCHEMA_VERSION = 1;
export const AGENT_IDS = Object.freeze(['zcode', 'dsh', 'codex']);
const CONFIG_FIELDS = new Set(['schema_version', 'revision', 'default_agent', 'agents', 'runtime', 'database', 'socket']);
const AGENT_FIELDS = new Set(['enabled', 'spawn_supported', 'default_model', 'runtime_path', 'home', 'profile', 'version']);

function plainObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function rejectUnknownFields(value, allowed, scope) {
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) throw new CliError('CONFIG_INVALID', `${scope} contains unknown field: ${key}`, 2);
  }
}

export function defaultConfig() {
  return {
    schema_version: CONFIG_SCHEMA_VERSION,
    revision: 0,
    default_agent: null,
    agents: {
      zcode: { enabled: true, spawn_supported: true, default_model: null },
      dsh: { enabled: false, spawn_supported: false, default_model: null, runtime_path: null, home: null, profile: null, version: null },
      codex: { enabled: false, spawn_supported: false, default_model: null, runtime_path: null, home: null, profile: null, version: null },
    },
  };
}

function rejectModel(agent, value, field) {
  if (agent === 'zcode' && value != null) {
    throw new CliError('model_selection_unsupported', `${field} is unsupported for zcode`, 2);
  }
}

export function validateConfig(input) {
  if (!plainObject(input)) throw new CliError('CONFIG_INVALID', 'config must be an object', 2);
  rejectUnknownFields(input, CONFIG_FIELDS, 'config');
  const config = defaultConfig();
  if (input.schema_version !== undefined && input.schema_version !== CONFIG_SCHEMA_VERSION) throw new CliError('CONFIG_INVALID', 'unsupported config schema version', 2);
  if (input.revision !== undefined && (!Number.isInteger(input.revision) || input.revision < 0)) throw new CliError('CONFIG_INVALID', 'revision must be a non-negative integer', 2);
  if (input.default_agent !== undefined && input.default_agent !== null && !AGENT_IDS.includes(input.default_agent)) throw new CliError('CONFIG_INVALID', 'default_agent is unknown', 2);
  config.revision = input.revision ?? 0;
  config.default_agent = input.default_agent ?? null;
  for (const field of ['runtime', 'database', 'socket']) {
    if (input[field] !== undefined) {
      if (typeof input[field] !== 'string' || input[field].length === 0) throw new CliError('CONFIG_INVALID', `${field} must be a non-empty string`, 2);
      config[field] = input[field];
    }
  }
  if (input.agents !== undefined && !plainObject(input.agents)) throw new CliError('CONFIG_INVALID', 'agents must be an object', 2);
  for (const agent of Object.keys(input.agents || {})) {
    if (!AGENT_IDS.includes(agent)) throw new CliError('CONFIG_INVALID', `agents contains unknown agent: ${agent}`, 2);
  }
  for (const agent of AGENT_IDS) {
    const value = input.agents?.[agent];
    if (value === undefined) continue;
    if (!plainObject(value)) throw new CliError('CONFIG_INVALID', `agents.${agent} must be an object`, 2);
    rejectUnknownFields(value, AGENT_FIELDS, `agents.${agent}`);
    if (value.enabled !== undefined && typeof value.enabled !== 'boolean') throw new CliError('CONFIG_INVALID', `agents.${agent}.enabled must be boolean`, 2);
    if (value.spawn_supported !== undefined && typeof value.spawn_supported !== 'boolean') throw new CliError('CONFIG_INVALID', `agents.${agent}.spawn_supported must be boolean`, 2);
    if (value.default_model !== undefined && value.default_model !== null && (typeof value.default_model !== 'string' || value.default_model.length === 0)) throw new CliError('CONFIG_INVALID', `agents.${agent}.default_model must be a non-empty string or null`, 2);
    for (const field of ['runtime_path', 'home', 'profile', 'version']) { if (value[field] !== undefined && value[field] !== null && (typeof value[field] !== 'string' || value[field].length === 0)) throw new CliError('CONFIG_INVALID', `agents.${agent}.${field} must be a non-empty string or null`, 2); }
    rejectModel(agent, value.default_model, 'default_model');
    config.agents[agent] = { ...config.agents[agent], ...value };
  }
  if (config.default_agent && !config.agents[config.default_agent].enabled) throw new CliError('CONFIG_INVALID', 'default_agent must be enabled', 2);
  return config;
}
