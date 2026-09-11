import { readConfig } from '../config/read.mjs';
import { writeConfig } from '../config/write.mjs';
import { AGENT_IDS } from '../config/schema.mjs';
import { CliError } from '../errors.mjs';

const CONFIG_INPUT_FIELDS = new Set(['operation', 'patch', 'key']);
const GET_KEYS = new Set([
  'default_agent',
  ...AGENT_IDS.flatMap((agent) => [`agents.${agent}.enabled`, `agents.${agent}.default_model`]),
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

function valueFor(config, key) {
  if (!GET_KEYS.has(key)) throw new CliError('INVALID_ARGUMENT', `unsupported config key: ${key}`, 2);
  return key.split('.').reduce((value, part) => value?.[part], config);
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
  throw new CliError('INVALID_ARGUMENT', `unsupported config operation: ${operation}`, 2);
}

export function configCommand(paths, input = {}) {
  rejectUnknownInput(input);
  const operation = input.operation ?? 'get';
  const current = readConfig(paths.config);
  if (operation === 'set') {
    if (!input.patch || typeof input.patch !== 'object' || Array.isArray(input.patch)) throw new CliError('INVALID_ARGUMENT', 'config set requires an object patch', 2);
    if (input.patch.agents !== undefined && (!input.patch.agents || typeof input.patch.agents !== 'object' || Array.isArray(input.patch.agents))) {
      throw new CliError('INVALID_ARGUMENT', 'config agents patch must be an object', 2);
    }
    const next = {
      ...current,
      ...input.patch,
      agents: {
        ...current.agents,
        ...Object.fromEntries(Object.entries(input.patch.agents || {}).map(([agent, value]) => [
          agent,
          { ...current.agents[agent], ...value },
        ])),
      },
    };
    return { config: writeConfig(paths.config, next) };
  }
  if (operation === 'get') {
    if (input.patch !== undefined) throw new CliError('INVALID_ARGUMENT', 'config get does not accept patch', 2);
    if (input.key === undefined) return { config: current };
    return { key: input.key, value: valueFor(current, input.key), revision: current.revision };
  }
  throw new CliError('INVALID_ARGUMENT', `unsupported config operation: ${operation}`, 2);
}
