#!/usr/bin/env node
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { realpath } from 'node:fs/promises';

export const SCENARIOS = ['initialize', 'catalog', 'hi', 'permission', 'cancel', 'malformed'];

function parseArgs(argv) {
  const out = { args: [], timeout: 5000 };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--help' || arg === '-h') out.help = true;
    else if (arg === '--scenario') out.scenario = argv[++i];
    else if (arg === '--executable') out.executable = argv[++i];
    else if (arg === '--arg') out.args.push(argv[++i]);
    else if (arg === '--timeout-ms') out.timeout = Number(argv[++i]);
    else if (arg === '--workspace') out.workspace = argv[++i];
    else throw new Error(`unknown argument: ${arg}`);
  }
  return out;
}

export function usage() {
  return [
    'Usage: probe.mjs --executable <path> --scenario <name> [--arg <arg> ...] [--timeout-ms <n>]',
    `Scenarios: ${SCENARIOS.join(', ')}`,
    'The probe never installs a provider, writes credentials, or invents a provider command.',
  ].join('\n');
}

function frame(method, params = {}, id = 1) {
  return JSON.stringify({ jsonrpc: '2.0', id, method, params });
}

export async function runProbe(options) {
  if (!options.executable) throw new Error('executable_required');
  if (!SCENARIOS.includes(options.scenario)) throw new Error('scenario_required');
  if (!Number.isInteger(options.timeout) || options.timeout < 100) throw new Error('invalid_timeout');

  const executable = await realpath(options.executable);
  const child = spawn(executable, options.args ?? [], {
    cwd: options.workspace,
    stdio: ['pipe', 'pipe', 'pipe'],
    env: { ...process.env, DSH_ACP_PROBE: '1' },
  });
  const stdout = [];
  const stderr = [];
  let buffer = '';
  let parseErrors = 0;
  child.stdout.setEncoding('utf8');
  child.stderr.setEncoding('utf8');
  child.stdout.on('data', (chunk) => {
    buffer += chunk;
    let newline;
    while ((newline = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, newline).trim();
      buffer = buffer.slice(newline + 1);
      if (!line) continue;
      try { stdout.push(JSON.parse(line)); } catch { parseErrors += 1; }
    }
  });
  child.stderr.on('data', (chunk) => stderr.push(chunk));

  const send = (method, params, id) => child.stdin.write(`${frame(method, params, id)}\n`);
  const startedAt = new Date().toISOString();
  const timer = setTimeout(() => child.kill('SIGTERM'), options.timeout);
  try {
    send('initialize', { protocolVersion: 1, clientInfo: { name: 'external-subagent-dsh-probe', version: '0.1.0' } }, 1);
    if (options.scenario !== 'initialize') send('session/new', { cwd: options.workspace ?? process.cwd() }, 2);
    if (options.scenario === 'catalog') send('models/list', {}, 3);
    if (options.scenario === 'hi') send('session/prompt', { prompt: 'hi', model: options.model }, 3);
    if (options.scenario === 'permission') send('session/prompt', { prompt: 'probe permission' }, 3);
    if (options.scenario === 'cancel') {
      send('session/prompt', { prompt: 'probe cancellation' }, 3);
      send('session/cancel', {}, 4);
    }
    if (options.scenario === 'malformed') child.stdin.write('{"jsonrpc":\n');
    child.stdin.end();
    await once(child, 'close');
  } finally {
    clearTimeout(timer);
    if (!child.killed) child.kill('SIGTERM');
  }
  return {
    scenario: options.scenario,
    executable,
    argv: options.args ?? [],
    startedAt,
    exitCode: child.exitCode,
    signal: child.signalCode,
    responses: stdout,
    stderr: stderr.join('').replaceAll(/\b(token|password|secret|api[_-]?key)\s*[=:]\s*[^\s]+/gi, '$1=<redacted>'),
    malformedFrames: parseErrors,
    protocol: 'jsonrpc-over-stdio',
  };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  try {
    const options = parseArgs(process.argv.slice(2));
    if (options.help || !options.scenario) { process.stdout.write(`${usage()}\n`); process.exit(options.help ? 0 : 2); }
    const result = await runProbe(options);
    process.stdout.write(`${JSON.stringify(result)}\n`);
  } catch (error) {
    process.stderr.write(`${error.message}\n`);
    process.exitCode = 2;
  }
}
