import { readConfig } from '../config/read.mjs';
import { CliError } from '../errors.mjs';

const INPUT_FIELDS = new Set(['operation', 'subagent', 'through', 'workspace', 'home']);

function validateInput(input) {
  if (!input || typeof input !== 'object' || Array.isArray(input)) throw new CliError('INVALID_ARGUMENT', 'subagents input must be an object', 2);
  for (const key of Object.keys(input)) {
    if (!INPUT_FIELDS.has(key)) throw new CliError('INVALID_ARGUMENT', `subagents contains unsupported field: ${key}`, 2);
  }
  if (Object.prototype.hasOwnProperty.call(input, 'subagent')) {
    if (input.subagent === null) throw new CliError('INVALID_ARGUMENT', 'subagent must be omitted or a supported subagent id; null is invalid', 2);
    if (typeof input.subagent !== 'string' || input.subagent.length === 0) throw new CliError('INVALID_ARGUMENT', 'subagent must be a non-empty string', 2);
  }
}

function parseProbeArgs(rest) {
  const subagent = rest.shift();
  if (!subagent || subagent.startsWith('--')) throw new CliError('subagent_required', 'subagents probe requires a subagent', 2);
  let through = 'local';
  let selectedLayer = false;
  const input = { operation: 'probe', subagent };
  while (rest.length > 0) {
    const option = rest.shift();
    if (['--local', '--auth', '--hi'].includes(option)) {
      if (selectedLayer) throw new CliError('INVALID_ARGUMENT', 'subagents probe accepts exactly one of --local, --auth, or --hi', 2);
      through = option.slice(2);
      selectedLayer = true;
      continue;
    }
    if (option === '--workspace' || option === '--home') {
      const name = option.slice(2);
      const value = rest.shift();
      if (!value || value.startsWith('--')) throw new CliError('INVALID_ARGUMENT', `${option} requires a value`, 2);
      if (input[name] !== undefined) throw new CliError('INVALID_ARGUMENT', `${option} may be provided only once`, 2);
      input[name] = value;
      continue;
    }
    throw new CliError('INVALID_ARGUMENT', `unsupported subagents probe option: ${option}`, 2);
  }
  return { ...input, through };
}

function parseModelsArgs(rest) {
  const subagent = rest.shift();
  if (!subagent || subagent.startsWith('--')) throw new CliError('subagent_required', 'subagents models requires a subagent', 2);
  const input = { operation: 'models', subagent };
  while (rest.length > 0) {
    const option = rest.shift();
    if (option !== '--workspace' && option !== '--home') throw new CliError('INVALID_ARGUMENT', `unsupported subagents models option: ${option}`, 2);
    const name = option.slice(2);
    const value = rest.shift();
    if (!value || value.startsWith('--')) throw new CliError('INVALID_ARGUMENT', `${option} requires a value`, 2);
    if (input[name] !== undefined) throw new CliError('INVALID_ARGUMENT', `${option} may be provided only once`, 2);
    input[name] = value;
  }
  return input;
}

export function parseSubagentsArgs(args) {
  if (args.length === 0) return { operation: 'list' };
  const [operation, ...rest] = args;
  if (!['list', 'status', 'probe', 'models'].includes(operation)) throw new CliError('INVALID_ARGUMENT', `unsupported subagents operation: ${operation}`, 2);
  if (operation === 'probe') return parseProbeArgs(rest);
  if (operation === 'models') return parseModelsArgs(rest);
  if (operation === 'list' && rest.length !== 0) throw new CliError('INVALID_ARGUMENT', 'usage: subagents list', 2);
  if (operation !== 'list' && rest.length > 1) throw new CliError('INVALID_ARGUMENT', `usage: subagents ${operation} [subagent]`, 2);
  return { operation, ...(rest[0] ? { subagent: rest[0] } : {}) };
}

export async function subagentsCommand(paths, input = {}, options = {}) {
  validateInput(input);
  const operation = input.operation ?? 'list';
  const config = readConfig(paths.config);
  if (operation === 'list') {
    return { default_subagent: config.default_subagent, config_revision: config.revision, subagents: Object.entries(config.subagents).map(([id, value]) => ({ subagent: id, ...value })) };
  }
  if (operation === 'models') {
    if (typeof options.callDaemon !== 'function' || typeof options.socket !== 'string') throw new CliError('INTERNAL_ERROR', 'subagents models requires a daemon connection');
    if (!input.subagent) throw new CliError('subagent_required', 'subagents models requires a subagent', 2);
    const scope = {};
    if (input.workspace !== undefined) scope.workspace = input.workspace;
    if (input.home !== undefined) scope.home = input.home;
    const result = await options.callDaemon(options.socket, 'agent-models', { subagent: input.subagent, scope });
    return subagentView(result);
  }
  if (operation === 'probe') {
    if (typeof options.callDaemon !== 'function' || typeof options.socket !== 'string') throw new CliError('INTERNAL_ERROR', 'subagents probe requires a daemon connection');
    if (!input.subagent) throw new CliError('subagent_required', 'subagents probe requires a subagent', 2);
    const scope = {};
    if (input.workspace !== undefined) scope.workspace = input.workspace;
    if (input.home !== undefined) scope.home = input.home;
    const result = await options.callDaemon(options.socket, 'agent-probe', { subagent: input.subagent, through: input.through ?? 'local', scope });
    return { ...result, evidence: subagentView(result.evidence), status: subagentView(result.status) };
  }
  if (operation !== 'status') throw new CliError('INVALID_ARGUMENT', `unsupported subagents operation: ${operation}`, 2);
  if (typeof options.callDaemon !== 'function' || typeof options.socket !== 'string') throw new CliError('INTERNAL_ERROR', 'subagents status requires a daemon connection');
  const status = await options.callDaemon(options.socket, 'status', {});
  const entries = status.subagents ?? status.agents;
  if (!Array.isArray(entries)) throw new CliError('PROTOCOL_ERROR', 'daemon status did not include subagent status');
  const subagents = entries.filter((entry) => input.subagent === undefined || (entry.subagent ?? entry.agent) === input.subagent).map(subagentView);
  if (input.subagent !== undefined && subagents.length === 0) throw new CliError('subagent_unknown', `daemon did not report subagent: ${input.subagent}`, 2);
  return { service_generation: status.service_generation ?? null, subagents };
}

// Source compatibility for internal callers while the public CLI name is
// `subagents`; wire-level agent fields remain S02's responsibility.
export const agentsCommand = subagentsCommand;
export const parseAgentsArgs = parseSubagentsArgs;

// The daemon wire contract is migrated by S02; the CLI already exposes the
// canonical execution-target name at its own boundary.
function subagentView(value) {
  if (!value) return value;
  const { agent, ...rest } = value;
  return { subagent: value.subagent ?? agent, ...rest };
}
