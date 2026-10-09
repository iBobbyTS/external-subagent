#!/usr/bin/env node
// Codex app-server wire probe for the external-subagent compatibility record.
//
// It speaks the exact JSON-RPC-over-stdio handshake the daemon drives
// (`initialize` + `initialized`, then `thread/start` / `turn/start`) against a
// caller-supplied Codex runtime and `CODEX_HOME`.  Scenarios are read-only at
// the transport level: `handshake` and `posture` send no prompt, and `turn`
// sends one caller-supplied prompt.  Nothing is written outside the supplied
// home, no credentials are emitted (token-shaped keys are redacted), and no
// provider is installed.  `--codex-home` must point at a throwaway directory.
import { spawn } from 'node:child_process';

export const SCENARIOS = ['handshake', 'posture', 'turn'];

const SECRET_KEY = /token|secret|authorization|api[_-]?key|cookie|password/i;

function redactValue(value, key = '') {
  if (SECRET_KEY.test(key)) return '<redacted>';
  if (Array.isArray(value)) return value.map((entry) => redactValue(entry));
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, redactValue(v, k)]));
  }
  return value;
}

function parseArgs(argv) {
  const out = { timeout: 20000 };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--help' || arg === '-h') out.help = true;
    else if (arg === '--executable') out.executable = argv[++i];
    else if (arg === '--codex-home') out.codexHome = argv[++i];
    else if (arg === '--workspace') out.workspace = argv[++i];
    else if (arg === '--model') out.model = argv[++i];
    else if (arg === '--prompt') out.prompt = argv[++i];
    else if (arg === '--scenario') out.scenario = argv[++i];
    else if (arg === '--timeout-ms') out.timeout = Number(argv[++i]);
    else throw new Error(`unknown argument: ${arg}`);
  }
  return out;
}

export function usage() {
  return [
    'Usage: probe.mjs --executable <codex> --codex-home <throwaway-dir> --scenario <name>',
    `Scenarios: ${SCENARIOS.join(', ')}`,
    'Optional: --workspace <dir> --model <token> --prompt <text> --timeout-ms <n>',
    'The probe requires an absolute throwaway --codex-home and never writes outside it.',
  ].join('\n');
}

function boundedClient(child, timeout) {
  let buffer = '';
  const notifications = [];
  const waiters = new Map();
  let nextId = 1;
  const request = (method, params) => new Promise((resolve, reject) => {
    const id = nextId++;
    const timer = setTimeout(() => reject(new Error(`${method} timed out`)), timeout);
    waiters.set(id, { resolve, timer });
    child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);
  });
  const notify = (method, params) => child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', method, params: params ?? {} })}\n`);
  child.stdout.setEncoding('utf8');
  child.stdout.on('data', (chunk) => {
    buffer += chunk;
    let newline;
    while ((newline = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, newline).trim();
      buffer = buffer.slice(newline + 1);
      if (!line) continue;
      let frame;
      try { frame = JSON.parse(line); } catch { continue; }
      if (frame.id !== undefined && waiters.has(frame.id)) {
        const waiter = waiters.get(frame.id);
        waiters.delete(frame.id);
        clearTimeout(waiter.timer);
        if (frame.error) waiter.reject(new Error(`${JSON.stringify(frame.error)}`));
        else waiter.resolve(frame.result ?? null);
      } else if (typeof frame.method === 'string') {
        notifications.push({ method: frame.method, params: frame.params ?? null });
      }
    }
  });
  child.stderr.setEncoding('utf8');
  return { request, notify, notifications };
}

export async function runProbe(options) {
  if (!options.executable) throw new Error('executable_required');
  if (!options.codexHome) throw new Error('codex_home_required');
  if (!options.scenario || !SCENARIOS.includes(options.scenario)) throw new Error('scenario_required');
  if (!Number.isInteger(options.timeout) || options.timeout < 100) throw new Error('invalid_timeout');
  const home = options.codexHome;
  const env = { ...process.env, CODEX_HOME: home };
  delete env.OPENAI_API_KEY;
  delete env.OPENAI_BASE_URL;
  const child = spawn(options.executable, ['app-server', '--listen', 'stdio://'], {
    cwd: options.workspace,
    stdio: ['pipe', 'pipe', 'pipe'],
    env,
  });
  const client = boundedClient(child, options.timeout);
  const frames = [];
  try {
    const init = await client.request('initialize', {
      clientInfo: { name: 'external-subagent-probe', version: '0' },
      capabilities: {},
    });
    client.notify('initialized');
    frames.push({ request: 'initialize', result: redactValue(init) });
    if (options.scenario === 'handshake') return { scenario: 'handshake', codexHome: home, frames, notifications: client.notifications.map((n) => n.method) };

    const postures = options.scenario === 'posture'
      ? [['workspace-write', 'build/edit'], ['read-only', 'plan'], ['danger-full-access', 'yolo']]
      : [['workspace-write', 'build/edit']];
    const threads = [];
    for (const [sandbox, label] of postures) {
      const params = {
        model: options.model,
        cwd: options.workspace,
        approvalPolicy: 'never',
        sandbox,
        ephemeral: false,
      };
      const result = await client.request('thread/start', params);
      frames.push({ request: 'thread/start', label, params: redactValue(params), result: redactValue(result) });
      threads.push({ sandbox, label, threadId: result?.thread?.id });
    }
    if (options.scenario === 'posture') return { scenario: 'posture', codexHome: home, frames, notifications: client.notifications.map((n) => n.method) };

    const active = threads[0];
    const turnParams = {
      threadId: active.threadId,
      model: options.model,
      effort: 'low',
      input: [{ type: 'text', text: options.prompt }],
    };
    const started = await client.request('turn/start', turnParams);
    frames.push({ request: 'turn/start', params: redactValue(turnParams), result: redactValue(started) });
    const turnId = started?.turn?.id;
    const terminal = await new Promise((resolve) => {
      const done = (n) => n.method === 'turn/completed' && (!turnId || n.params?.turn?.id === turnId);
      if (client.notifications.some(done)) return resolve(true);
      const interval = setInterval(() => {
        if (client.notifications.some(done)) { clearInterval(interval); resolve(true); }
      }, 50);
      setTimeout(() => { clearInterval(interval); resolve(false); }, options.timeout);
    });
    const completedText = client.notifications
      .filter((n) => n.method === 'item/completed')
      .map((n) => n.params?.item)
      .filter((item) => item?.type === 'agentMessage')
      .map((item) => item.text)
      .join('');
    return {
      scenario: 'turn',
      codexHome: home,
      frames,
      notifications: client.notifications.map((n) => n.method),
      turnCompleted: terminal,
      finalText: completedText,
    };
  } finally {
    child.kill('SIGTERM');
  }
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.help) { process.stdout.write(`${usage()}\n`); return; }
  try {
    process.stdout.write(`${JSON.stringify(await runProbe(options), null, 2)}\n`);
  } catch (error) {
    process.stderr.write(`${JSON.stringify({ error: error.message })}\n`);
    process.exitCode = 1;
  }
}

if (import.meta.url === `file://${process.argv[1]}`) main();
