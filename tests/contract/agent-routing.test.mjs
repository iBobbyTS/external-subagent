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

function runCliWithStdin(home, socket, command, input) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [cli, command], {
      env: { ...process.env, HOME: home, ZCODE_AS_SUBAGENT_TEST_PLATFORM: 'darwin', ZCODE_AGENTD_SOCKET: socket },
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    let stdout = ''; let stderr = '';
    child.stdout.on('data', (chunk) => { stdout += chunk; });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    child.on('close', (code) => resolve({ code, stdout, stderr }));
    child.stdin.end(JSON.stringify(input));
  });
}

async function withServer(socket, respond, body) {
  let observed = null;
  const server = net.createServer((connection) => connection.once('data', (chunk) => {
    observed = JSON.parse(chunk.toString('utf8'));
    connection.end(`${JSON.stringify({ request_id: observed.request_id, ...respond(observed) })}\n`);
  }));
  await new Promise((resolve) => server.listen(socket, resolve));
  try { return { result: await body(), observed: () => observed }; }
  finally { await new Promise((resolve) => server.close(resolve)); }
}

test('spawn omits implicit agent and model so daemon owns default admission', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-routing-'));
  const paths = productPaths(home);
  const socket = path.join(os.tmpdir(), `es-r-${process.pid}-${Date.now()}.sock`);
  configCommand(paths, { operation: 'set', patch: { default_subagent: 'zcode' } });
  const fixture = await withServer(socket, (request) => ({ outcome: 'success', result: {
    kind: 'task_submitted', disposition: 'created', task: { agent_id: '10000001', status: 'queued' },
  } }), () => runCli(home, socket, ['spawn', '--json', JSON.stringify({ repository: '/repo', prompt: 'hi' })]));
  assert.equal(fixture.result.code, 0, fixture.result.stderr);
  assert.equal(fixture.observed().method, 'submit_general');
  assert.equal(Object.hasOwn(fixture.observed().params, 'input'), false);
  assert.equal(Object.hasOwn(fixture.observed().params, 'agent'), false);
  assert.equal(Object.hasOwn(fixture.observed().params, 'model'), false);
});

test('spawn and create flags produce the same daemon DTO as JSON', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-routing-flags-'));
  const jsonInput = {
    subagent: 'future-provider', repository: '/repo', prompt: 'hi', permission_mode: 'build',
    model: 'catalog-token', write_manifest: ['src/**', 'tests/**'],
  };
  const invocations = [
    ['spawn', '--json', JSON.stringify(jsonInput)],
    ['spawn', '--subagent', 'future-provider', '--repository', '/repo', '--prompt', 'hi', '--permission-mode', 'build', '--model', 'catalog-token', '--write-manifest', 'src/**', '--write-manifest', 'tests/**'],
    ['create', '--subagent', 'future-provider', '--repository', '/repo', '--prompt', 'hi', '--permission-mode', 'build', '--model', 'catalog-token', '--write-manifest', 'src/**', '--write-manifest', 'tests/**'],
  ];
  const captured = [];
  for (let index = 0; index < invocations.length; index += 1) {
    const socket = path.join(os.tmpdir(), `es-f-${process.pid}-${index}-${Date.now()}.sock`);
    const fixture = await withServer(socket, (request) => ({ outcome: 'success', result: {
      kind: 'task_submitted', disposition: 'created', task: { agent_id: '10000001', status: 'queued' },
    } }), () => runCli(home, socket, invocations[index]));
    assert.equal(fixture.result.code, 0, fixture.result.stderr);
    const params = structuredClone(fixture.observed().params);
    params.manifest.agent_id = '<request-id>';
    captured.push(params);
  }
  assert.deepEqual(captured[1], captured[0]);
  assert.deepEqual(captured[2], captured[0]);
});

test('spawn and create JSON stdin preserve the same daemon wire DTO', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-routing-stdin-'));
  const jsonInput = {
    subagent: 'future-provider', repository: '/repo', prompt: 'stdin hi', permission_mode: 'edit',
    model: 'catalog-token', write_manifest: ['src/**'],
  };
  const captured = [];
  for (const [index, command] of ['spawn', 'create'].entries()) {
    const socket = path.join(os.tmpdir(), `es-i-${process.pid}-${index}-${Date.now()}.sock`);
    const fixture = await withServer(socket, () => ({ outcome: 'success', result: {
      kind: 'task_submitted', disposition: 'created', task: { agent_id: '10000001', status: 'queued' },
    } }), () => runCliWithStdin(home, socket, command, jsonInput));
    assert.equal(fixture.result.code, 0, fixture.result.stderr);
    const params = structuredClone(fixture.observed().params);
    params.manifest.agent_id = '<request-id>';
    captured.push(params);
  }
  assert.deepEqual(captured[1], captured[0]);
});

test('spawn flags reject unknown and missing values before transport', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-routing-invalid-flags-'));
  const socket = path.join(home, 'missing.sock');
  for (const args of [
    ['spawn', '--repository', '/repo'],
    ['spawn', '--repository', '/repo', '--prompt'],
    ['spawn', '--repository', '/repo', '--prompt', 'hi', '--unknown', 'x'],
  ]) {
    const result = await runCli(home, socket, args);
    assert.equal(result.code, 2);
    assert.notEqual(JSON.parse(result.stderr).error.code, 'SOCKET_UNAVAILABLE');
  }
});

test('literal null flag strings reach daemon while structured JSON null is rejected', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-routing-null-'));
  const socket = path.join(os.tmpdir(), `es-n-${process.pid}-${Date.now()}.sock`);
  const fixture = await withServer(socket, (request) => {
    assert.equal(request.params.subagent, 'zcode');
    assert.equal(request.params.model, 'null');
    assert.equal(request.params.manifest.prompt, 'null');
    return { outcome: 'error', error: { code: 'model_selection_unsupported', message: 'model selection is unsupported' } };
  }, () => runCli(home, socket, ['spawn', '--subagent', 'zcode', '--model', 'null', '--repository', '/repo', '--prompt', 'null']));
  assert.equal(fixture.result.code, 1);
  assert.equal(JSON.parse(fixture.result.stderr).error.code, 'model_selection_unsupported');
  for (const input of [
    { subagent: null, repository: '/repo', prompt: 'hi' },
    { subagent: 'zcode', model: null, repository: '/repo', prompt: 'hi' },
  ]) {
    const rejected = await runCli(home, '/missing/socket', ['spawn', '--json', JSON.stringify(input)]);
    assert.equal(rejected.code, 2);
    assert.equal(JSON.parse(rejected.stderr).error.code, 'INVALID_ARGUMENT');
  }
});

test('disabled-agent result comes from daemon canonical admission', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-routing-disabled-'));
  const paths = productPaths(home);
  const socket = path.join(os.tmpdir(), `es-d-${process.pid}-${Date.now()}.sock`);
  configCommand(paths, { operation: 'set', patch: { subagents: { zcode: { enabled: false } } } });
  const fixture = await withServer(socket, () => ({ outcome: 'error', error: { code: 'agent_disabled', message: 'agent is disabled' } }),
    () => runCli(home, socket, ['spawn', '--json', JSON.stringify({ subagent: 'zcode', repository: '/repo', prompt: 'hi' })]));
  assert.equal(fixture.observed().method, 'submit_general');
  assert.equal(fixture.result.code, 1);
  assert.equal(JSON.parse(fixture.result.stderr).error.code, 'agent_disabled');
});

test('unknown agent, dsh support, and model decisions all come from daemon', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-routing-semantics-'));
  const cases = [
    [{ subagent: 'future-provider', repository: '/repo', prompt: 'hi' }, 'subagent_unknown'],
    [{ subagent: 'dsh', repository: '/repo', prompt: 'hi' }, 'agent_unsupported'],
    [{ subagent: 'zcode', model: 'provider-token', repository: '/repo', prompt: 'hi' }, 'model_selection_unsupported'],
  ];
  for (let index = 0; index < cases.length; index += 1) {
    const [input, code] = cases[index];
    const socket = path.join(os.tmpdir(), `es-s-${process.pid}-${index}-${Date.now()}.sock`);
    const fixture = await withServer(socket, (request) => {
      assert.equal(request.params.subagent, input.subagent);
      if (input.model === undefined) assert.equal(Object.hasOwn(request.params, 'model'), false);
      else assert.equal(request.params.model, input.model);
      return { outcome: 'error', error: { code, message: code } };
    }, () => runCli(home, socket, ['spawn', '--json', JSON.stringify(input)]));
    assert.equal(fixture.result.code, 1);
    assert.equal(JSON.parse(fixture.result.stderr).error.code, code);
  }
});

test('null and unknown spawn fields fail before transport', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-routing-invalid-'));
  const socket = path.join(home, 'missing.sock');
  for (const input of [
    { subagent: null, repository: '/repo', prompt: 'hi' },
    { subagent: 'zcode', model: null, repository: '/repo', prompt: 'hi' },
    { subagent: 'zcode', repository: '/repo', prompt: 'hi', surprise: true },
  ]) {
    const result = await runCli(home, socket, ['spawn', '--json', JSON.stringify(input)]);
    assert.equal(result.code, 2);
    assert.notEqual(JSON.parse(result.stderr).error.code, 'SOCKET_UNAVAILABLE');
  }
});

test('human config and agents forms execute instead of falling through to JSON defaults', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-human-'));
  const socket = path.join(os.tmpdir(), `es-h-${process.pid}-${Date.now()}.sock`);
  const set = await runCli(home, socket, ['config', 'set', 'default_subagent', 'zcode']);
  assert.equal(set.code, 0, set.stderr);
  assert.equal(JSON.parse(set.stdout).config.default_subagent, 'zcode');
  const get = await runCli(home, socket, ['config', 'get', 'default_subagent']);
  assert.equal(get.code, 0, get.stderr);
  assert.equal(JSON.parse(get.stdout).value, 'zcode');
  const list = await runCli(home, socket, ['subagents', 'list']);
  assert.equal(list.code, 0, list.stderr);
  assert.deepEqual(JSON.parse(list.stdout).subagents.map((agent) => agent.subagent), ['zcode', 'dsh', 'codex']);
  const models = await withServer(socket, () => ({ outcome: 'success', result: {
    kind: 'agent_models', catalog: { subagent: 'zcode', scope: {}, models: [], supported: false },
  } }), () => runCli(home, socket, ['subagents', 'models', 'zcode']));
  assert.equal(models.result.code, 0, models.result.stderr);
  assert.equal(models.observed().method, 'agent_models');
  assert.deepEqual(models.observed().params, { subagent: 'zcode', scope: {} });
  const modelOutput = JSON.parse(models.result.stdout);
  assert.equal(modelOutput.subagent, 'zcode');
  assert.equal(modelOutput.agent, undefined);
  assert.equal(modelOutput.kind, undefined);
  const status = {
    service_generation: 'generation-cli',
    subagents: [{
      subagent: 'zcode', config_revision: 1, configured: true, enabled: true, spawn_supported: true,
      transport_support: { transport: 'zcode_app_server', probe: true, spawn: true },
      permission_modes: ['build', 'edit', 'plan', 'yolo'],
      model_selection: { supported: false, mode: 'native_only' },
      effort_selection: { supported: true, mode: 'passthrough_token' },
      local: { state: 'READY', version: '1.0.0', checked_at_ms: 10, scope: {} },
      auth: { state: 'UNKNOWN', checked_at_ms: null, scope: {} },
      hi: { state: 'UNKNOWN', checked_at_ms: null, scope: {} },
    }],
  };
  const fixture = await withServer(socket, () => ({ outcome: 'success', result: { kind: 'system_status', status } }),
    () => runCli(home, socket, ['subagents', 'status', 'zcode']));
  assert.equal(fixture.result.code, 0, fixture.result.stderr);
  assert.equal(fixture.observed().method, 'system_status');
  assert.deepEqual(JSON.parse(fixture.result.stdout).subagents, status.subagents);

  const probe = await withServer(socket, (request) => ({ outcome: 'success', result: {
    kind: 'agent_probed', evidence: { subagent: 'zcode' }, status: { subagent: 'zcode' },
  } }), () => runCli(home, socket, ['subagents', 'probe', 'zcode', '--hi', '--workspace', '/workspace', '--home', '/home']));
  assert.equal(probe.result.code, 0, probe.result.stderr);
  assert.equal(probe.observed().method, 'agent_probe');
  assert.deepEqual(JSON.parse(probe.result.stdout), { ok: true, product: 'external-subagent', evidence: { subagent: 'zcode' }, status: { subagent: 'zcode' } });
  assert.deepEqual(probe.observed().params, {
    subagent: 'zcode', through: 'hi', scope: { workspace: '/workspace', home: '/home' },
  });
});
