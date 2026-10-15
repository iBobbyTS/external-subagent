// Windows named-pipe RPC round-trip fixture (stage 1).
//
// Drives the real daemon binary end-to-end over the Win32 named pipe:
// direct start, `cli/rpc.mjs` wire round-trip, concurrent connections, a
// squatter-endpoint bind rejection (FILE_FLAG_FIRST_PIPE_INSTANCE), and
// shutdown. win32-only: the pipe namespace and the daemon binary are Windows
// specific, so the whole fixture is skipped elsewhere.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { test } from 'node:test';
import { callDaemon } from '../../cli/rpc.mjs';

const isWindows = process.platform === 'win32';
const daemonPath = path.resolve('target/debug/external-subagentd');
let counter = 0;
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function uniquePipe() {
  return `\\\\.\\pipe\\external-subagent-s01-fixture-${process.pid}-${counter++}`;
}

function spawnDaemon({ database, pipe }) {
  fs.mkdirSync(path.dirname(database), { recursive: true });
  const child = spawn(daemonPath, ['--database', database, '--socket', pipe], {
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  let stderr = '';
  child.stderr.on('data', (chunk) => { stderr += chunk; });
  const exited = new Promise((resolve) => {
    child.once('exit', (code) => resolve(code));
    child.once('error', () => resolve(-1));
  });
  return { child, exited, stderr: () => stderr };
}

async function waitForStatus(pipe, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  let last;
  while (Date.now() < deadline) {
    try { return await callDaemon(pipe, 'status', {}); } catch (error) { last = error; }
    await delay(50);
  }
  throw new Error(`daemon did not answer on ${pipe}: ${last && last.message}`);
}

async function waitForExit(exited, timeoutMs) {
  let timer;
  try {
    return await Promise.race([
      exited,
      new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('daemon did not exit')), timeoutMs); }),
    ]);
  } finally { clearTimeout(timer); }
}

test('named pipe daemon round-trips status and concurrent calls', { skip: !isWindows }, async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 's01-pipe-'));
  const pipe = uniquePipe();
  const daemon = spawnDaemon({ database: path.join(directory, 'external-subagent.sqlite3'), pipe });
  try {
    const status = await waitForStatus(pipe);
    assert.equal(typeof status, 'object');
    assert.ok(status && typeof status === 'object', 'status projection must be returned');
    const results = await Promise.all(Array.from({ length: 8 }, () => callDaemon(pipe, 'status', {})));
    assert.equal(results.length, 8);
    for (const result of results) assert.equal(typeof result, 'object');
  } finally {
    daemon.child.kill();
    try { await waitForExit(daemon.exited, 10000); } catch { daemon.child.kill('SIGKILL'); }
    fs.rmSync(directory, { recursive: true, force: true });
  }
});

test('a squatted endpoint rejects the daemon bind (anti-hijack)', { skip: !isWindows }, async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 's01-pipe-hijack-'));
  const pipe = uniquePipe();
  const squatter = net.createServer();
  await new Promise((resolve, reject) => {
    squatter.once('error', reject);
    squatter.listen(pipe, resolve);
  });
  try {
    const daemon = spawnDaemon({ database: path.join(directory, 'external-subagent.sqlite3'), pipe });
    const code = await waitForExit(daemon.exited, 10000);
    assert.notEqual(code, 0, 'the daemon must reject an already-squatted endpoint');
    assert.match(
      daemon.stderr(),
      /named pipe endpoint already exists|in use|ACCESS_DENIED/i,
      `unexpected stderr: ${daemon.stderr()}`,
    );
  } finally {
    await new Promise((resolve) => squatter.close(resolve));
    fs.rmSync(directory, { recursive: true, force: true });
  }
});
