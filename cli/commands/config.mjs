import { readConfig } from '../config/read.mjs';
import { updateConfig, writeConfig } from '../config/write.mjs';
import { AGENT_IDS } from '../config/schema.mjs';
import { CliError } from '../errors.mjs';

const CONFIG_INPUT_FIELDS = new Set(['operation', 'patch', 'key']);
const GET_KEYS = new Set([
  'default_agent',
  ...AGENT_IDS.flatMap((agent) => [`agents.${agent}.enabled`, `agents.${agent}.spawn_supported`, `agents.${agent}.default_model`, `agents.${agent}.runtime_path`, `agents.${agent}.home`, `agents.${agent}.profile`, `agents.${agent}.version`]),
]);
const SET_KEYS = GET_KEYS;

function rejectUnknownInput(input, allowed = CONFIG_INPUT_FIELDS) {
  if (!input || typeof input !== 'object' || Array.isArray(input)) throw new CliError('INVALID_ARGUMENT', 'config input must be an object', 2);
  for (const key of Object.keys(input)) {
    if (!allowed.has(key)) throw new CliError('INVALID_ARGUMENT', `config contains unsupported field: ${key}`, 2);
  }
}

function parseValue(raw) {
  if (raw === 'null') return null;
  if (raw === 'true') return true;
  if (raw === 'false') return false;
  return raw;
}

function patchFor(key, value) {
  if (!SET_KEYS.has(key)) throw new CliError('INVALID_ARGUMENT', `unsupported config key: ${key}`, 2);
  const parts = key.split('.');
  if (parts.length === 1) return { [key]: value };
  return { agents: { [parts[1]]: { [parts[2]]: value } } };
}

function unsetPatch(key) {
  if (!SET_KEYS.has(key)) throw new CliError('INVALID_ARGUMENT', `unsupported config key: ${key}`, 2);
  if (key === 'default_agent') return { default_agent: null };
  const [, agent, field] = key.split('.');
  const defaults = {
    enabled: agent === 'zcode',
    spawn_supported: agent === 'zcode',
    default_model: null, runtime_path: null, home: null, profile: null, version: null,
  };
  return { agents: { [agent]: { [field]: defaults[field] } } };
}

function valueFor(config, key) {
  if (!GET_KEYS.has(key)) throw new CliError('INVALID_ARGUMENT', `unsupported config key: ${key}`, 2);
  return key.split('.').reduce((value, part) => value?.[part], config);
}

function mergePatch(current, patch) {
  return {
    ...current,
    ...patch,
    agents: {
      ...current.agents,
      ...Object.fromEntries(Object.entries(patch.agents || {}).map(([agent, value]) => [
        agent,
        { ...current.agents[agent], ...value },
      ])),
    },
  };
}

export function parseConfigArgs(args) {
  if (args.length === 0) return { operation: 'get' };
  const [operation, ...rest] = args;
  if (operation === 'get') {
    if (rest.length > 1) throw new CliError('INVALID_ARGUMENT', 'usage: config get [key]', 2);
    return { operation, ...(rest[0] ? { key: rest[0] } : {}) };
  }
  if (operation === 'set') {
    if (rest.length !== 2) throw new CliError('INVALID_ARGUMENT', 'usage: config set <key> <value>', 2);
    return { operation, patch: patchFor(rest[0], parseValue(rest[1])) };
  }
  if (operation === 'show') {
    if (rest.length !== 0) throw new CliError('INVALID_ARGUMENT', 'usage: config show', 2);
    return { operation };
  }
  if (operation === 'unset') {
    if (rest.length !== 1) throw new CliError('INVALID_ARGUMENT', 'usage: config unset <key>', 2);
    unsetPatch(rest[0]);
    return { operation, key: rest[0] };
  }
  throw new CliError('INVALID_ARGUMENT', `unsupported config operation: ${operation}`, 2);
}

export function configCommand(paths, input = {}) {
  rejectUnknownInput(input);
  const operation = input.operation ?? 'get';
  const current = readConfig(paths.config);
  if (operation === 'set') {
    if (!input.patch || typeof input.patch !== 'object' || Array.isArray(input.patch)) throw new CliError('INVALID_ARGUMENT', 'config set requires an object patch', 2);
    if (Object.hasOwn(input.patch, 'revision')) throw new CliError('INVALID_ARGUMENT', 'config revision is managed by the writer', 2);
    if (input.patch.agents !== undefined && (!input.patch.agents || typeof input.patch.agents !== 'object' || Array.isArray(input.patch.agents))) {
      throw new CliError('INVALID_ARGUMENT', 'config agents patch must be an object', 2);
    }
    for (const [agent, value] of Object.entries(input.patch.agents || {})) {
      if (value === null || typeof value !== 'object' || Array.isArray(value)) throw new CliError('CONFIG_INVALID', `agents.${agent} must be an object`, 2);
    }
    return { config: updateConfig(paths.config, (latest) => mergePatch(latest, input.patch)) };
  }
  if (operation === 'show') {
    if (input.patch !== undefined || input.key !== undefined) throw new CliError('INVALID_ARGUMENT', 'config show does not accept key or patch', 2);
    return { config: current };
  }
  if (operation === 'unset') {
    if (input.patch !== undefined || typeof input.key !== 'string') throw new CliError('INVALID_ARGUMENT', 'config unset requires exactly one supported key', 2);
    return { config: updateConfig(paths.config, (latest) => mergePatch(latest, unsetPatch(input.key))) };
  }
  if (operation === 'get') {
    if (input.patch !== undefined) throw new CliError('INVALID_ARGUMENT', 'config get does not accept patch', 2);
    if (input.key === undefined) return { config: current };
    return { key: input.key, value: valueFor(current, input.key), revision: current.revision };
  }
  throw new CliError('INVALID_ARGUMENT', `unsupported config operation: ${operation}`, 2);
}
