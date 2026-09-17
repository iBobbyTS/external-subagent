import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { BUSINESS_COMMANDS, PRODUCT_NAME, VERSION, ZCODE_RUNTIME } from './constants.mjs';
import { CliError } from './errors.mjs';
import { installHooks, installPlan, runInit } from './install/init.mjs';
import { nativeBinary } from './install/layout.mjs';
import { cleanupLegacy, purge, restoreData, backupData, uninstall } from './commands/maintenance.mjs';
import { updateCommand } from './commands/update.mjs';
import { localInstallStatus, startDaemon, stopDaemon } from './commands/daemon.mjs';
import { serviceRegistrationStatus } from './install/service-macos.mjs';
import { mcpCommand, pluginCommand } from './commands/plugin.mjs';
import { platform, productPaths } from './paths.mjs';
import { callDaemon, parseDaemonInput } from './rpc.mjs';
import { configCommand } from './commands/config.mjs';
import { parseConfigArgs } from './commands/config.mjs';
import { subagentsCommand, parseSubagentsArgs } from './commands/agents.mjs';
import { parseSpawnArgs, prepareSpawnInput } from './commands/tasks.mjs';

const HELP = `external-subagent ${VERSION}\n\nUsage: external-subagent <command> [options]\n\nCommands:\n  help, version               Show basic product information\n  init [--dry-run] [--resume] [--install-hooks]\n      [--skip-runtime-probe] [--skip-codex-plugin] [--skip-service-start]\n      [--codex-home <path>]    Install service, bind Codex, claim the Codex home\n  hooks install [--dry-run]  Install ZCode policy hooks explicitly\n  install-plugin [--dry-run|--uninstall] [--codex-home <path>]\n                             Install or remove the managed Codex plugin (MCP + skill)\n  install-mcp [--dry-run|--uninstall] [--codex-home <path>]\n                             Install or remove the direct Codex MCP TOML binding\n  status, diagnose            Inspect local service and runtime state\n  start, stop                 Bootstrap or boot out the daemon LaunchAgent\n  backup --output <dir>       Back up retained product data\n  restore --input <dir>       Verify and restore product data\n  uninstall                   Release Codex claims; remove service registration; retain data\n  purge --yes                 Explicitly delete new product data\n  cleanup-legacy --yes        Delete old unpublished installation (no migration)\n`;
const DAEMON_HELP = `  config get [key] | config set <key> <value>\n  subagents list | subagents status [subagent] | subagents probe/models [subagent]\n  create/spawn, wait, list, send, respond, cancel, result, close, observe\n                             Daemon calls accept --json '<object>' or JSON stdin\n                             list JSON requires repository (workspace is an alias)\n                             observe JSON requires only agent_id\n`;

function structuredInput(args, parser) {
  return args.length > 0 && !args[0].startsWith('--') ? parser(args) : parseDaemonInput(args);
}

function value(args, name) {
  const index = args.indexOf(name);
  if (index < 0) return undefined;
  const result = args[index + 1];
  if (!result || result.startsWith('--')) throw new CliError('INVALID_ARGUMENT', `${name} requires a value`, 2);
  return result;
}

function output(valueToWrite) {
  process.stdout.write(`${JSON.stringify({ ok: true, product: PRODUCT_NAME, ...valueToWrite }, null, 2)}\n`);
}

function summarizeAgentStatus(agent) {
  if (!agent || typeof agent !== 'object') return agent;
  const scopes = {};
  for (const scope of ['local', 'auth', 'hi']) {
    if (agent[scope] && typeof agent[scope] === 'object') {
      scopes[scope] = {
        state: agent[scope].state ?? 'UNKNOWN',
        checked_at_ms: agent[scope].checked_at_ms ?? null,
      };
    }
  }
  return {
    subagent: agent.subagent ?? agent.agent,
    configured: agent.configured,
    enabled: agent.enabled,
    spawn_supported: agent.spawn_supported,
    ...scopes,
  };
}

function publicDaemonStatus(status, { verbose = false } = {}) {
  if (!status || typeof status !== 'object') return status;
  if (verbose) return status;
  return {
    ...(status.mcp_version === undefined ? {} : { mcp_version: status.mcp_version }),
    ...(status.components === undefined ? {} : { components: status.components }),
    ...(Array.isArray(status.subagents ?? status.agents) ? { subagents: (status.subagents ?? status.agents).map(summarizeAgentStatus) } : {}),
  };
}

const DIAGNOSTIC_TAIL_BYTES = 16 * 1024;
const DIAGNOSTIC_TOTAL_BYTES = 32 * 1024;
const DIAGNOSTIC_LOG_NAMES = ['daemon.log', 'daemon-error.log'];

function fileArtifact(target, source, capturedAtMs = Date.now()) {
  const artifact = { path: target, source, captured_at_ms: capturedAtMs };
  try {
    const stat = fs.lstatSync(target);
    if (!stat.isFile() || stat.isSymbolicLink()) return artifact;
    artifact.sha256 = crypto.createHash('sha256').update(fs.readFileSync(target)).digest('hex');
  } catch {}
  return artifact;
}

function preserveDiagnosticText(text) { return text; }

function diagnosticFields(record) {
  return JSON.stringify(Object.fromEntries(
    ['agent_id', 'session_id', 'stage', 'error_code', 'message', 'stderr_tail', 'operation', 'remote_code', 'remote_message', 'cleanup_result']
      .filter((field) => typeof record[field] === 'string' || record[field] === null || (field === 'remote_code' && Number.isSafeInteger(record[field])))
      .map((field) => [field, record[field]]),
  ));
}

// Budget the serialized fields, so escaping and UTF-8 cannot invalidate JSON.
// Metadata has bounded prefixes; stderr receives the remaining budget as a tail.
function boundedFailureRecord(record) {
  const projected = diagnosticFields(record);
  if (Buffer.byteLength(projected) <= DIAGNOSTIC_TAIL_BYTES) return { text: projected, truncated: false };
  const fields = JSON.parse(projected);
  for (const key of Object.keys(fields)) {
    if (key !== 'stderr_tail' && typeof fields[key] === 'string') {
      fields[key] = Array.from(fields[key]).slice(0, key === 'message' ? 512 : 256).join('');
    }
  }
  const tail = Array.from(fields.stderr_tail || '');
  let low = 0;
  let high = tail.length;
  while (low < high) {
    const keep = Math.ceil((low + high) / 2);
    fields.stderr_tail = tail.slice(tail.length - keep).join('');
    if (Buffer.byteLength(JSON.stringify(fields)) <= DIAGNOSTIC_TAIL_BYTES) low = keep;
    else high = keep - 1;
  }
  fields.stderr_tail = tail.slice(tail.length - low).join('');
  return { text: JSON.stringify(fields), truncated: true };
}

function diagnosticTail(text) {
  let incomplete = false;
  const decoded = text.split('\n').map((line) => {
    const match = line.match(/^(\[zcode-agentd\] failure agent=[^:\r\n]+: )(.+)$/u);
    if (match && match[2].startsWith('{')) {
      try {
        const record = JSON.parse(match[2]);
        if (record && typeof record.agent_id === 'string') return preserveDiagnosticText(match[1]) + diagnosticFields(record);
      } catch {
        incomplete = true;
        return match[1] + '[INCOMPLETE_FAILURE_RECORD]';
      }
    }
    return line;
  }).join('\n');
  return { text: preserveDiagnosticText(decoded), incomplete };
}

function diagnosticLogs(logDirectory) {
  const incomplete = [];
  if (!fs.existsSync(logDirectory)) return { directory: logDirectory, complete: false, incomplete: ['log_directory_missing'], files: [] };
  const files = [];
  let totalBytes = 0;
  for (const name of DIAGNOSTIC_LOG_NAMES) {
    const target = path.join(logDirectory, name);
    let targetStat;
    try { targetStat = fs.lstatSync(target); } catch (error) { targetStat = null; }
    const rotated = ['.1', '.2', '.old', '.gz'].some((suffix) => {
      try { return fs.lstatSync(`${target}${suffix}`) != null; } catch { return false; }
    });
    if (rotated) incomplete.push(`log_rotated:${name}`);
    if (!fs.existsSync(target)) continue;
    if (!targetStat || !targetStat.isFile() || targetStat.isSymbolicLink()) {
      incomplete.push(`log_unreadable:${name}:symlink_or_non_file`);
      continue;
    }
    try {
      const stat = fs.statSync(target);
      const remaining = Math.max(0, DIAGNOSTIC_TOTAL_BYTES - totalBytes);
      const take = Math.min(DIAGNOSTIC_TAIL_BYTES, remaining);
      const start = Math.max(0, stat.size - take);
      // A complete producer record can exceed the display window. Keep a
      // bounded record-sized lookbehind so its prefix survives until decoding.
      const readStart = Math.max(0, start - DIAGNOSTIC_RECORD_BYTES);
      const readLength = Math.min(take + (start - readStart), stat.size - readStart);
      const fd = fs.openSync(target, 'r');
      const buffer = Buffer.alloc(readLength);
      const read = fs.readSync(fd, buffer, 0, readLength, readStart);
      fs.closeSync(fd);
      let decodeStart = Math.max(0, start - 256) - readStart;
      if (decodeStart > 0) {
        const lineStart = buffer.lastIndexOf(0x0a, decodeStart - 1) + 1;
        if (buffer.subarray(lineStart, read).toString('utf8').startsWith('[zcode-agentd] failure agent=')) decodeStart = lineStart;
      }
      // Legacy text keeps its original lookbehind; known records are decoded
      // (or marked incomplete) before any display clipping can hide the prefix.
      const projected = diagnosticTail(buffer.subarray(decodeStart, read).toString('utf8'));
      if (projected.incomplete) incomplete.push(`record_incomplete:${name}`);
      const encoded = Buffer.from(projected.text, 'utf8');
      let tailStart = Math.max(0, encoded.length - Math.min(take, remaining));
      while (tailStart < encoded.length && (encoded[tailStart] & 0xc0) === 0x80) tailStart += 1;
      const bounded = encoded.subarray(tailStart);
      const tail = bounded.toString('utf8');
      totalBytes += Buffer.byteLength(tail);
      if (start > 0 || take < stat.size || bounded.length < encoded.length) incomplete.push(`log_truncated:${name}`);
      files.push({ name, read_status: 'read', bytes: stat.size, modified_at_ms: stat.mtimeMs, rotated, truncated: start > 0 || take < stat.size || bounded.length < encoded.length, tail });
    } catch (error) { incomplete.push(`log_unreadable:${name}:${error.code || 'error'}`); }
  }
  if (files.length === 0) incomplete.push('log_files_missing');
  return { directory: logDirectory, complete: incomplete.length === 0, incomplete, total_bytes: totalBytes, files };
}

// The writer retains the current file and two 1 MiB rotations. Search that
// finite window, independently of the much smaller global display tails.
const DIAGNOSTIC_RETAINED_BYTES = 1024 * 1024;
const DIAGNOSTIC_RECORD_BYTES = 192 * 1024;
function agentDiagnosticLogs(logDirectory, agentId) {
  const report = { status: 'target_record_missing', scope: 'retained_logs', scan_complete: true, scanned_bytes: 0, record: null, incomplete: [] };
  for (const name of ['daemon-error.log', 'daemon-error.log.1', 'daemon-error.log.2']) {
    let fd;
    try {
      const target = path.join(logDirectory, name);
      const stat = fs.lstatSync(target);
      if (!stat.isFile() || stat.isSymbolicLink()) throw Object.assign(new Error('not a regular file'), { code: 'NON_FILE' });
      fd = fs.openSync(target, 'r');
      const size = fs.fstatSync(fd).size;
      const start = Math.max(0, size - DIAGNOSTIC_RETAINED_BYTES);
      const bytes = Buffer.alloc(Math.min(size, DIAGNOSTIC_RETAINED_BYTES));
      const read = fs.readSync(fd, bytes, 0, bytes.length, start);
      report.scanned_bytes += read;
      if (start > 0 || read < bytes.length) report.incomplete.push(`scan_truncated:${name}`);
      const text = bytes.subarray(0, read).toString('utf8');
      const lines = text.split('\n');
      if (start > 0) lines.shift(); // Never associate a partial first record.
      if (lines.pop()) report.incomplete.push(`record_incomplete:${name}`); // The writer may still be appending.
      for (const line of lines.reverse()) {
        if (Buffer.byteLength(line) > DIAGNOSTIC_RECORD_BYTES) { report.incomplete.push(`record_truncated:${name}`); continue; }
        const prefix = `[zcode-agentd] failure agent=${agentId}: `;
        if (!line.startsWith(prefix)) continue;
        const raw = line.slice(prefix.length);
        // JSON records repeat the identifier; reject misleading prefix matches.
        let structured;
        try { structured = JSON.parse(raw); } catch { structured = null; }
        if (structured && structured.agent_id !== agentId) continue;
        if (structured) {
          report.record = { file: name, ...boundedFailureRecord(structured) };
        } else {
          const encoded = Buffer.from(preserveDiagnosticText(raw));
          let start = Math.max(0, encoded.length - DIAGNOSTIC_TAIL_BYTES);
          while (start < encoded.length && (encoded[start] & 0xc0) === 0x80) start += 1;
          report.record = { file: name, text: encoded.subarray(start).toString('utf8'), truncated: start > 0 };
        }
        report.status = 'found';
        break;
      }
    } catch (error) {
      if (error.code !== 'ENOENT') report.incomplete.push(`scan_unreadable:${name}:${error.code || 'error'}`);
    } finally { if (fd !== undefined) fs.closeSync(fd); }
    if (report.record) break;
  }
  report.scan_complete = report.incomplete.length === 0;
  return report;
}

function diagnoseInput(args) {
  const rawAgent = value(args, '--agent');
  const agent = rawAgent === undefined ? undefined : Number(rawAgent);
  if (rawAgent !== undefined && (!Number.isInteger(agent) || agent < 10_000_000 || agent > 99_999_999 || String(agent) !== rawAgent)) {
    throw new CliError('INVALID_ARGUMENT', 'agent_id must be an integer between 10000000 and 99999999', 2);
  }
  const outputDirectory = value(args, '--output');
  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index];
    if (!arg.startsWith('--')) throw new CliError('INVALID_ARGUMENT', `unexpected diagnose argument: ${arg}`, 2);
    if (!['--agent', '--output'].includes(arg)) throw new CliError('INVALID_ARGUMENT', `unsupported diagnose option: ${arg}`, 2);
    index += 1;
  }
  return { agent, outputDirectory };
}

async function diagnose(paths, args) {
  const { agent, outputDirectory } = diagnoseInput(args);
  const socket = process.env.ZCODE_AGENTD_SOCKET || paths.socket;
  const report = {
    schema_version: 1,
    scope: agent ? { agent_id: agent } : { kind: 'global' },
    platform: platform(),
    runtime: {
      configured_artifact: fileArtifact(ZCODE_RUNTIME, 'cli_packaged_configuration'),
      running_identity: null,
      running_identity_source: 'not_observed_by_cli',
    },
    facade: {
      running_identity: null,
      running_identity_source: 'not_observed_by_cli',
      packaged_artifact: fileArtifact(nativeBinary('external-subagent-mcp'), 'distributed_payload'),
    },
    daemon: { socket, socket_exists: fs.existsSync(socket), query_status: 'unqueried', available: null },
    logs: diagnosticLogs(paths.logs),
  };
  try {
    report.daemon.status = await callDaemon(socket, 'status', {});
    report.daemon.available = true;
    report.daemon.query_status = 'queried';
  } catch (error) {
    report.daemon.available = Boolean(error.daemonResponded);
    report.daemon.query_status = error.daemonResponded ? 'query_failed' : 'unavailable';
    report.daemon.error = { code: error.code || 'DAEMON_ERROR', message: error.message };
  }
  if (agent) {
    const diagnostics = agentDiagnosticLogs(paths.logs, String(agent));
    try {
      const snapshot = await callDaemon(socket, 'wait', { agent_id: agent, wait_time: 0 });
      report.daemon.available = true;
      report.agent = {
        diagnostics,
        task: snapshot.task,
        activity: snapshot.activity,
        session_id: snapshot.task?.session_id ?? null,
        turn_id: snapshot.task?.turn_id ?? null,
        request_ids: Array.isArray(snapshot.pending_requests) ? snapshot.pending_requests.map((request) => request.request_id).filter(Boolean) : [],
        query_request_id: snapshot.__request_id ?? null,
        identifiers_complete: Boolean(snapshot.task?.turn_id || (Array.isArray(snapshot.pending_requests) && snapshot.pending_requests.some((request) => request.request_id))),
        pending_request_count: Array.isArray(snapshot.pending_requests) ? snapshot.pending_requests.length : null,
        result_available: snapshot.result_available ?? false,
        observed_at_ms: Date.now(),
      };
    } catch (error) {
      report.daemon.error = { code: error.code || 'DAEMON_ERROR', message: error.message };
      const missing = error.code === 'not_found';
      report.logs.incomplete.push(missing ? 'agent_missing' : 'agent_store_unavailable');
      report.logs.complete = false;
      report.agent = { agent_id: agent, query_status: missing ? 'missing' : 'unavailable', unavailable: !missing, missing, diagnostics };
    }
  }
  if (outputDirectory) {
    const destination = path.resolve(outputDirectory);
    const target = path.join(destination, 'diagnose.json');
    report.output = { path: target, complete: report.logs.complete && report.daemon.query_status === 'queried' && !report.agent?.unavailable && !report.agent?.missing };
    try {
      fs.mkdirSync(destination, { recursive: true, mode: 0o700 });
      fs.writeFileSync(target, `${JSON.stringify(report, null, 2)}\n`, { mode: 0o600 });
    } catch (error) {
      report.output = { path: target, complete: false, error: { code: error.code || 'OUTPUT_WRITE_FAILED', message: error.message } };
    }
  }
  return report;
}

export async function main(args) {
  const command = args[0] || 'help';
  if (command === 'help' || command === '--help' || command === '-h') {
    process.stdout.write(HELP.replace('status, diagnose            Inspect local service and runtime state', 'status [--verbose]          Inspect essential service and runtime state\n  diagnose                    Export bounded diagnostic details') + DAEMON_HELP); return;
  }
  if (command === 'version' || command === '--version' || command === '-v') {
    process.stdout.write(`${VERSION}\n`); return;
  }
  if (!BUSINESS_COMMANDS.has(command)) throw new CliError('UNKNOWN_COMMAND', `unknown command: ${command}`, 2);
  if (platform() !== 'darwin') throw new CliError('UNSUPPORTED_PLATFORM', `${command} is supported only on macOS`);

  const paths = productPaths();
  if (command === 'init') {
    const flags = args.slice(1);
    const known = ['--dry-run', '--resume', '--install-hooks', '--skip-runtime-probe', '--skip-codex-plugin', '--skip-service-start'];
    const codexHomeIndex = flags.indexOf('--codex-home');
    let codexHome;
    if (codexHomeIndex >= 0) {
      codexHome = flags[codexHomeIndex + 1];
      if (!codexHome || codexHome.startsWith('--')) throw new CliError('INVALID_ARGUMENT', '--codex-home requires a value', 2);
    }
    for (let index = 0; index < flags.length; index += 1) {
      if (index === codexHomeIndex) { index += 1; continue; }
      if (!known.includes(flags[index])) throw new CliError('INVALID_ARGUMENT', `unsupported init option: ${flags[index]}`, 2);
    }
    output(runInit({
      paths,
      dryRun: flags.includes('--dry-run'),
      resume: flags.includes('--resume'),
      installHooks: flags.includes('--install-hooks'),
      skipRuntimeProbe: flags.includes('--skip-runtime-probe'),
      skipCodexPlugin: flags.includes('--skip-codex-plugin'),
      skipServiceStart: flags.includes('--skip-service-start'),
      codexHome,
    }));
    return;
  }
  if (command === 'hooks') {
    if (args[1] !== 'install') throw new CliError('INVALID_ARGUMENT', 'usage: hooks install [--dry-run]', 2);
    output(installHooks(paths, { dryRun: args.includes('--dry-run') })); return;
  }
  if (command === 'install-plugin') {
    output(pluginCommand(paths, args.slice(1))); return;
  }
  if (command === 'install-mcp') {
    output(mcpCommand(paths, args.slice(1))); return;
  }
  if (command === 'status') {
    const verbose = args.includes('--verbose');
    const local = localInstallStatus(paths, { verbose });
    // `service` is the read-only launchd view (registered job + process);
    // `daemon_status` stays the RPC view, so a loaded-but-unready or
    // ready-but-unregistered install reads differently instead of blurring.
    const serviceRaw = serviceRegistrationStatus();
    // PID is an implementation detail and may be reused by another process;
    // keep it out of the ordinary status projection.
    const { pid: _pid, ...service } = serviceRaw;
    try {
      output({ ...local, service, daemon_status: publicDaemonStatus(await callDaemon(process.env.ZCODE_AGENTD_SOCKET || paths.socket, 'status', {}), { verbose }) });
    } catch (error) {
      output({ ...local, service, daemon_status: null, daemon_error: { code: error.code || 'DAEMON_ERROR', message: error.message } });
    }
    return;
  }
  if (command === 'config' || command === 'subagents' || command === 'agents') {
    const input = structuredInput(args.slice(1), command === 'config' ? parseConfigArgs : parseSubagentsArgs);
    output(command === 'config'
      ? configCommand(paths, input)
      : await subagentsCommand(paths, input, { callDaemon, socket: process.env.ZCODE_AGENTD_SOCKET || paths.socket }));
    return;
  }
  if (command === 'diagnose') {
    const report = await diagnose(paths, args.slice(1));
    output({
      ...report,
      daemon_packaged_artifact: fileArtifact(nativeBinary('external-subagentd'), 'distributed_payload'),
    }); return;
  }
  if (command === 'backup') { output(backupData(value(args, '--output'), paths)); return; }
  if (command === 'restore') { output(restoreData(value(args, '--input'), paths)); return; }
  if (command === 'start') { output(startDaemon(paths)); return; }
  if (command === 'stop') { output(stopDaemon(paths)); return; }
  if (command === 'update' || command === 'reconcile') { output(await updateCommand(paths, command === 'reconcile' ? ['reconcile', ...args.slice(1)] : args.slice(1), { socket: process.env.ZCODE_AGENTD_SOCKET || paths.socket })); return; }
  if (command === 'uninstall') { output(uninstall(paths)); return; }
  if (command === 'purge') {
    if (!args.includes('--yes')) throw new CliError('CONFIRMATION_REQUIRED', 'purge requires --yes');
    output(purge(paths)); return;
  }
  if (command === 'cleanup-legacy') {
    if (!args.includes('--yes')) throw new CliError('CONFIRMATION_REQUIRED', 'cleanup-legacy requires --yes');
    output(cleanupLegacy(paths.home)); return;
  }
  let input;
  if (command === 'create' || command === 'spawn') {
    const spawnArgs = args.slice(1);
    input = spawnArgs.length === 0 || spawnArgs.some((arg) => arg === '--json' || arg.startsWith('--json='))
      ? prepareSpawnInput(parseDaemonInput(spawnArgs))
      : parseSpawnArgs(spawnArgs);
  } else input = parseDaemonInput(args.slice(1));
  const result = await callDaemon(process.env.ZCODE_AGENTD_SOCKET || paths.socket, command, input);
  output({ command, result });
}

export { DAEMON_HELP, HELP, installPlan, diagnose, diagnosticLogs, fileArtifact, publicDaemonStatus, summarizeAgentStatus };
