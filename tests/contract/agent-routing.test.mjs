import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { configCommand } from '../../cli/commands/config.mjs';
import { productPaths } from '../../cli/paths.mjs';

const cli = path.resolve('bin/zas.mjs');

function runCli(home, socket, args) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [cli, ...args], {
      env: { ...process.env, HOME: home, ZCODE_AS_SUBAGENT_TEST_PLATFORM: 'darwin', ZCODE_AGENTD_SOCKET: socket },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let stdout = ''; let stderr = '';
    child.stdout.on('data', (chunk) => { stdout += chunk; });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    child.on('close', (code) => resolve({ code, stdout, stderr }));
  });
}

async function withServer(socket, respond, body) {
  let observed = null;
  const server = net.createServer((connection) => connection.once('data', (chunk) => {
    observed = JSON.parse(chunk.toString('utf8'));
    connection.end(`${JSON.stringify({ version: 13, request_id: observed.request_id, ...respond(observed) })}\n`);
  }));
  await new Promise((resolve) => server.listen(socket, resolve));
  try { return { result: await body(), observed: () => observed }; }
  finally { await new Promise((resolve) => server.close(resolve)); }
}

test('spawn resolves only the configured default locally and delegates enabled admission to daemon', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-routing-'));
  const paths = productPaths(home);
  const socket = path.join(os.tmpdir(), `es-r-${process.pid}-${Date.now()}.sock`);
  configCommand(paths, { operation: 'set', patch: { default_agent: 'zcode' } });
  const fixture = await withServer(socket, (request) => ({ outcome: 'success', result: {
    kind: 'task_submitted', disposition: 'created', task: { agent_id: '10000001', phase: 'QUEUED' },
  } }), () => runCli(home, socket, ['spawn', '--json', JSON.stringify({ repository: '/repo', prompt: 'hi' })]));
  assert.equal(fixture.result.code, 0, fixture.result.stderr);
  assert.equal(fixture.observed().method, 'submit_general');
  assert.equal(fixture.observed().params.input.agent, 'zcode');
});

test('disabled-agent result comes from daemon canonical admission', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-routing-disabled-'));
  const paths = productPaths(home);
  const socket = path.join(os.tmpdir(), `es-d-${process.pid}-${Date.now()}.sock`);
  configCommand(paths, { operation: 'set', patch: { agents: { zcode: { enabled: false } } } });
  const fixture = await withServer(socket, () => ({ outcome: 'error', error: { code: 'agent_disabled', message: 'agent is disabled' } }),
    () => runCli(home, socket, ['spawn', '--json', JSON.stringify({ agent: 'zcode', repository: '/repo', prompt: 'hi' })]));
  assert.equal(fixture.observed().method, 'submit_general');
  assert.equal(fixture.result.code, 1);
  assert.equal(JSON.parse(fixture.result.stderr).error.code, 'agent_disabled');
});

test('null and unknown spawn fields fail before transport', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-routing-invalid-'));
  const socket = path.join(home, 'missing.sock');
  for (const input of [
    { agent: null, repository: '/repo', prompt: 'hi' },
    { agent: 'zcode', model: null, repository: '/repo', prompt: 'hi' },
    { agent: 'zcode', repository: '/repo', prompt: 'hi', surprise: true },
  ]) {
    const result = await runCli(home, socket, ['spawn', '--json', JSON.stringify(input)]);
    assert.equal(result.code, 2);
    assert.notEqual(JSON.parse(result.stderr).error.code, 'SOCKET_UNAVAILABLE');
  }
});

test('human config and agents forms execute instead of falling through to JSON defaults', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-human-'));
  const socket = path.join(os.tmpdir(), `es-h-${process.pid}-${Date.now()}.sock`);
  const set = await runCli(home, socket, ['config', 'set', 'default_agent', 'zcode']);
  assert.equal(set.code, 0, set.stderr);
  assert.equal(JSON.parse(set.stdout).config.default_agent, 'zcode');
  const get = await runCli(home, socket, ['config', 'get', 'default_agent']);
  assert.equal(get.code, 0, get.stderr);
  assert.equal(JSON.parse(get.stdout).value, 'zcode');
  const list = await runCli(home, socket, ['agents', 'list']);
  assert.equal(list.code, 0, list.stderr);
  assert.deepEqual(JSON.parse(list.stdout).agents.map((agent) => agent.agent), ['zcode', 'dsh']);
  for (const operation of ['probe', 'models']) {
    const unsupported = await runCli(home, socket, ['agents', operation, 'zcode']);
    assert.equal(unsupported.code, 2);
    assert.equal(JSON.parse(unsupported.stderr).error.code, 'agent_operation_unsupported');
  }
  const status = {
    service_generation: 'generation-cli',
    agents: [{ agent: 'zcode', enabled: true, spawn_supported: true, local: { status: 'ready', version: '1.0.0', checked_at_ms: 10 }, auth: { status: 'unknown' }, hi: { status: 'unknown' } }],
  };
  const fixture = await withServer(socket, () => ({ outcome: 'success', result: { kind: 'system_status', status } }),
    () => runCli(home, socket, ['agents', 'status', 'zcode']));
  assert.equal(fixture.result.code, 0, fixture.result.stderr);
  assert.equal(fixture.observed().method, 'system_status');
  assert.deepEqual(JSON.parse(fixture.result.stdout).agents, status.agents);
});
