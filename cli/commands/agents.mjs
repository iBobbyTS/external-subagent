import { readConfig } from '../config/read.mjs';
import { AGENT_IDS } from '../config/schema.mjs';
import { CliError } from '../errors.mjs';

const INPUT_FIELDS = new Set(['operation', 'agent', 'through', 'workspace', 'home']);

function validateInput(input) {
  if (!input || typeof input !== 'object' || Array.isArray(input)) throw new CliError('INVALID_ARGUMENT', 'agents input must be an object', 2);
  for (const key of Object.keys(input)) {
    if (!INPUT_FIELDS.has(key)) throw new CliError('INVALID_ARGUMENT', `agents contains unsupported field: ${key}`, 2);
  }
  if (Object.prototype.hasOwnProperty.call(input, 'agent')) {
    if (input.agent === null) throw new CliError('INVALID_ARGUMENT', 'agent must be omitted or a supported agent id; null is invalid', 2);
    if (!AGENT_IDS.includes(input.agent)) throw new CliError('agent_unknown', `unknown agent: ${input.agent}`, 2);
  }
}

export function parseAgentsArgs(args) {
  if (args.length === 0) return { operation: 'list' };
  const [operation, ...rest] = args;
  if (!['list', 'status', 'probe', 'models'].includes(operation)) throw new CliError('INVALID_ARGUMENT', `unsupported agents operation: ${operation}`, 2);
  if (operation === 'list' && rest.length !== 0) throw new CliError('INVALID_ARGUMENT', 'usage: agents list', 2);
  if (operation !== 'list' && rest.length > 1) throw new CliError('INVALID_ARGUMENT', `usage: agents ${operation} [agent]`, 2);
  return { operation, ...(rest[0] ? { agent: rest[0] } : {}) };
}

export async function agentsCommand(paths, input = {}, options = {}) {
  validateInput(input);
  const operation = input.operation ?? 'list';
  const config = readConfig(paths.config);
  if (operation === 'list') {
    return { default_agent: config.default_agent, config_revision: config.revision, agents: Object.entries(config.agents).map(([id, value]) => ({ agent: id, ...value })) };
  }
  if (operation === 'models') {
    throw new CliError('agent_operation_unsupported', 'agents models requires a provider catalog implementation', 2);
  }
  if (operation === 'probe') {
    if (typeof options.callDaemon !== 'function' || typeof options.socket !== 'string') throw new CliError('INTERNAL_ERROR', 'agents probe requires a daemon connection');
    if (!input.agent) throw new CliError('agent_required', 'agents probe requires an agent', 2);
    const scope = {};
    if (input.workspace !== undefined) scope.workspace = input.workspace;
    if (input.home !== undefined) scope.home = input.home;
    return options.callDaemon(options.socket, 'agent-probe', { agent: input.agent, through: input.through ?? 'local', scope });
  }
  if (operation !== 'status') throw new CliError('INVALID_ARGUMENT', `unsupported agents operation: ${operation}`, 2);
  if (typeof options.callDaemon !== 'function' || typeof options.socket !== 'string') throw new CliError('INTERNAL_ERROR', 'agents status requires a daemon connection');
  const status = await options.callDaemon(options.socket, 'status', {});
  if (!Array.isArray(status?.agents)) throw new CliError('PROTOCOL_ERROR', 'daemon status did not include agent status');
  const agents = input.agent === undefined ? status.agents : status.agents.filter((agent) => agent.agent === input.agent);
  if (input.agent !== undefined && agents.length === 0) throw new CliError('agent_unknown', `daemon did not report agent: ${input.agent}`, 2);
  return { service_generation: status.service_generation ?? null, agents };
}
