import { CliError } from '../errors.mjs';

export const CONFIG_SCHEMA_VERSION = 2;
export const SUBAGENT_IDS = Object.freeze(['zcode', 'dsh', 'codex']);
const CONFIG_FIELDS = new Set(['schema_version', 'revision', 'default_subagent', 'subagents', 'runtime', 'database', 'socket']);
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
    default_subagent: null,
    subagents: {
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
  if (input.default_subagent !== undefined && input.default_subagent !== null && !SUBAGENT_IDS.includes(input.default_subagent)) throw new CliError('CONFIG_INVALID', 'default_subagent is unknown', 2);
  config.revision = input.revision ?? 0;
  config.default_subagent = input.default_subagent ?? null;
  for (const field of ['runtime', 'database', 'socket']) {
    if (input[field] !== undefined) {
      if (typeof input[field] !== 'string' || input[field].length === 0) throw new CliError('CONFIG_INVALID', `${field} must be a non-empty string`, 2);
      config[field] = input[field];
    }
  }
  if (input.subagents !== undefined && !plainObject(input.subagents)) throw new CliError('CONFIG_INVALID', 'subagents must be an object', 2);
  for (const agent of Object.keys(input.subagents || {})) {
    if (!SUBAGENT_IDS.includes(agent)) throw new CliError('CONFIG_INVALID', `subagents contains unknown subagent: ${agent}`, 2);
  }
  for (const agent of SUBAGENT_IDS) {
    const value = input.subagents?.[agent];
    if (value === undefined) continue;
    if (!plainObject(value)) throw new CliError('CONFIG_INVALID', `subagents.${agent} must be an object`, 2);
    rejectUnknownFields(value, AGENT_FIELDS, `subagents.${agent}`);
    if (value.enabled !== undefined && typeof value.enabled !== 'boolean') throw new CliError('CONFIG_INVALID', `subagents.${agent}.enabled must be boolean`, 2);
    if (value.spawn_supported !== undefined && typeof value.spawn_supported !== 'boolean') throw new CliError('CONFIG_INVALID', `subagents.${agent}.spawn_supported must be boolean`, 2);
    if (value.default_model !== undefined && value.default_model !== null && (typeof value.default_model !== 'string' || value.default_model.length === 0)) throw new CliError('CONFIG_INVALID', `subagents.${agent}.default_model must be a non-empty string or null`, 2);
    for (const field of ['runtime_path', 'home', 'profile', 'version']) { if (value[field] !== undefined && value[field] !== null && (typeof value[field] !== 'string' || value[field].length === 0)) throw new CliError('CONFIG_INVALID', `subagents.${agent}.${field} must be a non-empty string or null`, 2); }
    rejectModel(agent, value.default_model, 'default_model');
    config.subagents[agent] = { ...config.subagents[agent], ...value };
  }
  if (config.default_subagent && !config.subagents[config.default_subagent].enabled) throw new CliError('CONFIG_INVALID', 'default_subagent must be enabled', 2);
  return config;
}
