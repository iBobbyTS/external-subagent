import { readConfig } from '../config/read.mjs';
import { updateConfig } from '../config/write.mjs';
import { SUBAGENT_IDS } from '../config/schema.mjs';
import path from 'node:path';
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
  if (!['list', 'status', 'probe', 'models', 'enable'].includes(operation)) throw new CliError('INVALID_ARGUMENT', `unsupported subagents operation: ${operation}`, 2);
  if (operation === 'enable') {
    if (rest.length !== 1 || !SUBAGENT_IDS.includes(rest[0])) throw new CliError('INVALID_ARGUMENT', 'usage: agents enable <zcode|dsh|codex>', 2);
    return { operation, subagent: rest[0] };
  }
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
  if (operation === 'enable') {
    const agent = input.subagent;
    if (!SUBAGENT_IDS.includes(agent) || ['through', 'workspace', 'home'].some((key) => input[key] !== undefined)) {
      throw new CliError('INVALID_ARGUMENT', 'usage: agents enable <zcode|dsh|codex>', 2);
    }
    if (typeof options.callDaemon !== 'function' || typeof options.socket !== 'string') throw new CliError('INTERNAL_ERROR', 'agents enable requires a daemon connection');
    const entry = config.subagents[agent];
    const scope = Object.fromEntries(['home', 'profile'].filter((key) => entry[key] != null).map((key) => [key, entry[key]]));
    const result = await options.callDaemon(options.socket, 'agent-probe', { subagent: agent, through: 'local', scope });
    const local = result.evidence?.local;
    if (local?.state !== 'READY') throw new CliError('agent_probe_failed', `Cannot enable ${agent}: ${local?.reason ?? 'local runtime probe did not succeed'}`, 2);
    const requiredVersion = result.status?.required_version;
    if (agent === 'dsh' && (typeof requiredVersion !== 'string' || !requiredVersion.trim())) {
      throw new CliError('agent_probe_failed', 'Cannot enable dsh: daemon did not report required_version; update and restart the daemon, then retry', 2);
    }
    if (requiredVersion != null && local.version !== requiredVersion) {
      throw new CliError('agent_probe_failed', `Cannot enable ${agent}: expected version ${requiredVersion}, observed ${local.version ?? 'unknown'}; install the required runtime version and retry`, 2);
    }
    const patch = { enabled: true, spawn_supported: true };
    if (agent !== 'zcode') {
      if (typeof local.runtime_path !== 'string' || !path.isAbsolute(local.runtime_path) || typeof local.version !== 'string' || !local.version.trim()) {
        throw new CliError('agent_probe_failed', 'Probe did not return an absolute runtime_path and observed version', 2);
      }
      const home = local.scope?.home;
      if (typeof home !== 'string' || !path.isAbsolute(home)) throw new CliError('agent_probe_failed', `Cannot enable ${agent}: configure subagents.${agent}.home or export ${agent.toUpperCase()}_HOME before starting the daemon`, 2);
      Object.assign(patch, { runtime_path: local.runtime_path, home, profile: agent === 'dsh' ? 'acp' : (entry.profile ?? null), version: local.version });
    }
    const updated = updateConfig(paths.config, (latest) => {
      if (Object.entries(patch).every(([key, value]) => latest.subagents[agent][key] === value)) return latest;
      return { ...latest, subagents: { ...latest.subagents, [agent]: { ...latest.subagents[agent], ...patch } } };
    });
    const restart = agent !== 'zcode';
    return {
      subagent: agent, enabled: true, config_revision: updated.revision,
      config: updated, evidence: subagentView(result.evidence), restart_required: restart,
      message: restart ? '配置已保存，重启 daemon 后生效。' : '配置已保存，新的 zcode 任务即时生效，无需重启。',
    };
  }
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
