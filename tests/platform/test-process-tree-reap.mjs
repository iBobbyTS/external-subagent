// Windows deep process-tree cascade-reap fixture (stage 2).
//
// The real daemon is started with a controlled zcode runtime whose only job is
// to derive a deep process tree (node.exe -> cmd.exe -> powershell.exe) through
// the real `gated_spawn` / Win32 Job Object path, and then to keep the turn
// active. Cancelling the task must close the job and atomically reap every
// descendant; the test proves it by polling `tasklist` for each recorded PID
// until none survives. The tree is generated deterministically by the fixture
// (`gated_spawn` really ran) — it is never inferred from `create` returning.
//
// win32-only: the Job Object supervision, the fixture's process tree and the
// `tasklist` probe are all Windows specific.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import { test } from 'node:test';
import { callDaemon } from '../../cli/rpc.mjs';

const isWindows = process.platform === 'win32';
const daemonPath = path.resolve('target/debug/external-subagentd');
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// A zcode app-server stand-in that answers the strict bootstrap sequence
// (session/create, session/setModel, session/subscribe, session/send), leaves
// the turn active, and spawns the deep tree through the same supervised
// process creation.
const RUNTIME_FIXTURE = `'use strict';
const fs = require('node:fs');
const path = require('node:path');
const readline = require('node:readline');
const { spawn } = require('node:child_process');

const pidFile = process.env.ES_TREE_PID_FILE;
const psScript = pidFile + '.ps1';
const psPidFile = pidFile + '.ps';
fs.writeFileSync(
  psScript,
  "$PID | Out-File -Encoding ascii -FilePath '" + psPidFile.replace(/'/g, "''") + "'; Start-Sleep -Seconds 600",
);
// node.exe -> cmd.exe -> powershell.exe; descendants join the leader's job.
const child = spawn('cmd.exe', ['/c', 'powershell', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', psScript], { stdio: 'ignore' });
fs.writeFileSync(pidFile, JSON.stringify({ node: process.pid, cmd: child.pid }));

const rl = readline.createInterface({ input: process.stdin });
const reply = (value) => process.stdout.write(JSON.stringify(value) + '\\n');
let reads = 0;
rl.on('line', () => {
  reads += 1;
  if (reads === 1) reply({ id: 1, result: { session: { sessionId: 'windows-tree-session' }, settings: { model: { available: [{ ref: { providerId: 'provider', modelId: 'model' } }] } } } });
  else if (reads === 2) reply({ id: 2, result: { settings: { model: { current: { providerId: 'provider', modelId: 'model' } } } } });
  else if (reads === 3) reply({ id: 3, result: {} });
  else if (reads === 4) {
    reply({ id: 4, result: { turnId: 't1' } });
    reply({ method: 'session/event', params: { type: 'turn.started' } });
  }
  // The turn is never completed: it stays active until the task is cancelled.
});
process.stdin.on('end', () => process.exit(0));
`;

function alive(pid) {
  const result = spawnSync('tasklist', ['/FI', `PID eq ${pid}`, '/NH', '/FO', 'CSV'], { encoding: 'utf8' });
  if (result.error) throw result.error;
  return new RegExp(`"${pid}"`).test(result.stdout || '');
}

async function waitForFile(file, timeoutMs = 20000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (fs.existsSync(file)) return fs.readFileSync(file, 'utf8');
    await delay(50);
  }
  throw new Error(`fixture file never appeared: ${file}`);
}

async function waitForPid(file, timeoutMs = 20000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (fs.existsSync(file)) {
      const pid = Number.parseInt(fs.readFileSync(file, 'utf8').trim(), 10);
      if (Number.isInteger(pid) && pid > 1) return pid;
    }
    await delay(50);
  }
  throw new Error(`powershell pid never appeared: ${file}`);
}

async function waitForStatus(pipe, timeoutMs = 20000) {
  const deadline = Date.now() + timeoutMs;
  let last;
  while (Date.now() < deadline) {
    try { return await callDaemon(pipe, 'status', {}); } catch (error) { last = error; }
    await delay(50);
  }
  throw new Error(`daemon did not answer on ${pipe}: ${last && last.message}`);
}

function spawnDaemon({ database, pipe, config, runtime, env }) {
  fs.mkdirSync(path.dirname(database), { recursive: true });
  const child = spawn(daemonPath, [
    '--agent-config', config,
    '--database', database,
    '--socket', pipe,
    '--runtime', runtime,
  ], { env: { ...process.env, ...env }, stdio: ['ignore', 'ignore', 'pipe'] });
  let stderr = '';
  child.stderr.on('data', (chunk) => { stderr += chunk; });
  const exited = new Promise((resolve) => {
    child.once('exit', (code) => resolve(code));
    child.once('error', () => resolve(-1));
  });
  return { child, exited, stderr: () => stderr };
}

test('cancelling a task reaps the node->cmd->powershell tree with no orphan PID', { skip: !isWindows }, async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'tree-'));
  const pipe = `\\\\.\\pipe\\external-subagent-tree-${process.pid}`;
  const pidFile = path.join(root, 'tree-pids.json');
  const runtime = path.join(root, 'runtime.cmd');
  const config = path.join(root, 'config.json');
  const workspace = path.join(root, 'workspace');
  fs.mkdirSync(workspace, { recursive: true });
  // The daemon resolves `--runtime` with `fs::canonicalize`, which yields a
  // verbatim `\\?\C:\...` path that Node rejects as a script argument. A `.cmd`
  // wrapper is spawned as a native program and re-invokes Node with a clean
  // relative script path, so the fixture runs without a verbatim argument.
  fs.writeFileSync(path.join(root, 'runtime.cjs'), RUNTIME_FIXTURE);
  fs.writeFileSync(runtime, '@echo off\r\nnode "%~dp0runtime.cjs" app-server\r\n');
  fs.writeFileSync(config, JSON.stringify({
    schema_version: 2,
    revision: 1,
    subagents: { zcode: { enabled: true, spawn_supported: true, default_model: null } },
  }));

  const daemon = spawnDaemon({
    database: path.join(root, 'external-subagent.sqlite3'),
    pipe,
    config,
    runtime,
    env: { ES_TREE_PID_FILE: pidFile },
  });
  try {
    await waitForStatus(pipe);

    const created = await callDaemon(pipe, 'create', {
      repository: workspace,
      prompt: 'derive the controlled deep process tree',
      subagent: 'zcode',
      model: 'provider/model',
    });
    const agentId = created.agent_id;

    // `gated_spawn` really ran: the fixture recorded its own pid and the cmd
    // child pid, and powershell recorded its own.
    const recorded = JSON.parse(await waitForFile(pidFile));
    const psPid = await waitForPid(pidFile + '.ps');
    const pids = [recorded.node, recorded.cmd, psPid];
    assert.equal(pids.length, 3);
    for (const pid of pids) assert.ok(Number.isInteger(pid) && pid > 1, `bad pid ${pid}`);
    assert.ok(alive(recorded.node), 'node fixture process must be alive before cancel');
    assert.ok(alive(recorded.cmd), 'cmd child must be alive before cancel');
    assert.ok(alive(psPid), 'powershell grandchild must be alive before cancel');

    await callDaemon(pipe, 'cancel', { agent_id: agentId });

    const deadline = Date.now() + 20000;
    let survivors = pids.filter(alive);
    while (survivors.length > 0 && Date.now() < deadline) {
      await delay(200);
      survivors = pids.filter(alive);
    }
    assert.deepEqual(survivors, [], `orphan PIDs survived cancellation: ${survivors.join(', ')}`);
  } finally {
    daemon.child.kill();
    await Promise.race([daemon.exited, delay(8000)]);
    if (daemon.child.exitCode === null && daemon.child.signalCode === null) daemon.child.kill('SIGKILL');
    fs.rmSync(root, { recursive: true, force: true });
  }
});
