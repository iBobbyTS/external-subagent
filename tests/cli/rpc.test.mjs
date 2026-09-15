import assert from 'node:assert/strict';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { AGENT_PROBE_TRANSPORT_TIMEOUT_MS, callDaemon, daemonTransportTimeoutMs, MAX_RESULT_CHUNK_BYTES, projectDaemonResult, waitTransportTimeoutMs } from '../../cli/rpc.mjs';

test('wait transport timeout covers maximum wait without sleeping', () => {
  assert.equal(waitTransportTimeoutMs(0), 5000);
  assert.equal(waitTransportTimeoutMs(299), 304000);
  assert.throws(() => waitTransportTimeoutMs(300), /wait_time/);
});

test('probe transport timeout covers local, two runtime deadlines, and cleanup', () => {
  assert.equal(daemonTransportTimeoutMs('agent-probe'), AGENT_PROBE_TRANSPORT_TIMEOUT_MS);
  assert.equal(daemonTransportTimeoutMs('agent-models'), AGENT_PROBE_TRANSPORT_TIMEOUT_MS);
  assert.ok(AGENT_PROBE_TRANSPORT_TIMEOUT_MS >= 187000);
});
import { CliError } from '../../cli/errors.mjs';
import { DAEMON_HELP } from '../../cli/main.mjs';

test('CLI help documents JSON list scope instead of nonexistent flags', () => {
  assert.match(DAEMON_HELP, /list JSON requires repository \(workspace is an alias\)/u);
  assert.doesNotMatch(DAEMON_HELP, /--repository|--workspace/u);
});

test('CLI sends daemon RPC and preserves success result', async () => {
  const socketPath = path.join(os.tmpdir(), `zcode-cli-rpc-${process.pid}-${Date.now()}.sock`);
  let clientEnded = false;
  const server = net.createServer((socket) => {
    let body = '';
    socket.on('end', () => { clientEnded = true; });
    socket.on('data', (chunk) => {
      body += chunk;
      if (!body.includes('\n')) return;
      const request = JSON.parse(body);
      assert.equal(Object.hasOwn(request, 'version'), false, 'requests carry no custom protocol version');
      assert.equal(request.method, 'task_wait');
      assert.equal(request.params.agent_id, '10000001');
      socket.end(JSON.stringify({
        request_id: request.request_id,
        outcome: 'success',
        result: {
          kind: 'task_wait',
          task: { agent_id: '10000001', phase: 'RUNNING', outcome: null, reason_code: null, stop_requested: false, close_requested: false, closed: false, reaped: false },
          pending_requests: [{ request_id: 'read-1', kind: 'permission', state: 'pending', respondable: true, tool_name: 'Read', operation: 'read', summary: 'target input.txt', policy_preview: 'official_permission_request' }],
          result_available: false,
          activity: { state: 'active', active_tools: [], window_60s: {}, telemetry_status: 'healthy' },
          latest_progress: null,
          result: null,
          instruction: 'A permission request is pending; respond now with external_subagent_respond using decision allow or deny.',
          timed_out: false,
        },
      }) + '\n');
    });
  });
  await new Promise((resolve) => server.listen(socketPath, resolve));
  try {
    const result = await callDaemon(socketPath, 'wait', { agent_id: 10000001, wait_time: 0 });
    assert.equal(clientEnded, false, 'sending a request must not half-close the RPC socket');
    assert.equal(result.task.cancel_requested, false);
    assert.equal(result.task.resources_reaped, false);
    assert.equal(result.activity.latest_progress, undefined);
    assert.equal(result.result, null);
    assert.equal(result.pending_requests[0].tool_name, 'Read');
    assert.equal(result.pending_requests[0].respondable, true);
    assert.equal(result.timed_out, false);
    assert.equal(result.kind, undefined);
  }
  finally { await new Promise((resolve) => server.close(resolve)); }
});

test('CLI preserves daemon error code, message, and active agent id', async () => {
  const socketPath = path.join(os.tmpdir(), `zcode-cli-rpc-error-${process.pid}-${Date.now()}.sock`);
  const server = net.createServer((socket) => { socket.once('data', (chunk) => { const request = JSON.parse(chunk); socket.end(JSON.stringify({ request_id: request.request_id, outcome: 'error', error: { code: 'not_found', message: 'task was not found', active_agent_id: 10000002 } }) + '\n'); }); });
  await new Promise((resolve) => server.listen(socketPath, resolve));
  try { await assert.rejects(() => callDaemon(socketPath, 'result', { agent_id: 10000002 }), (error) => error instanceof CliError && error.code === 'not_found' && error.agentId === 10000002); }
  finally { await new Promise((resolve) => server.close(resolve)); }
});

test('CLI rejects daemon responses for a different request id', async () => {
  for (const [index, response] of [
    { request_id: 'other-request', outcome: 'success', result: {} },
    { request_id: null, outcome: 'success', result: {} },
  ].entries()) {
    const socketPath = path.join(os.tmpdir(), `zcode-cli-rpc-protocol-${process.pid}-${Date.now()}-${index}.sock`);
    const server = net.createServer((socket) => { socket.once('data', () => socket.end(JSON.stringify(response) + '\n')); });
    await new Promise((resolve) => server.listen(socketPath, resolve));
    try { await assert.rejects(() => callDaemon(socketPath, 'result', { agent_id: 10000001 }), (error) => error instanceof CliError && error.code === 'PROTOCOL_ERROR'); }
    finally { await new Promise((resolve) => server.close(resolve)); }
  }
});

test('CLI rejects obsolete protocol fields before connecting', () => {
  assert.throws(() => callDaemon(path.join(os.tmpdir(), 'x'), 'wait', { agent_id: 10000001, after_revision: 0 }), /after_revision/);
  assert.throws(() => callDaemon(path.join(os.tmpdir(), 'x'), 'send', { agent_id: 10000001, mode: 'queue', content: 'x' }), /mode/);
  assert.throws(() => callDaemon(path.join(os.tmpdir(), 'x'), 'respond', { agent_id: 10000001, request_id: 'r', decision: 'deny', reason: 'because' }), /reason/);
});

test('CLI forwards supports_answer only when declared', async () => {
  const observed = [];
  const respond = (request) => {
    observed.push(request.params);
    return { request_id: request.request_id, outcome: 'success', result: { kind: 'task_wait', task: { agent_id: '10000001', phase: 'RUNNING', outcome: null, reason_code: null, stop_requested: false, close_requested: false, closed: false, reaped: false }, pending_requests: [], result_available: false, activity: { state: 'active', active_tools: [], window_60s: {}, telemetry_status: 'healthy' }, latest_progress: null, result: null, instruction: null, timed_out: true } };
  };
  for (const input of [{ agent_id: 10000001, wait_time: 0 }, { agent_id: 10000001, wait_time: 0, supports_answer: true }]) {
    const socketPath = path.join(os.tmpdir(), `zcode-cli-wait-cap-${process.pid}-${Date.now()}-${observed.length}.sock`);
    const server = net.createServer((socket) => { socket.once('data', (chunk) => socket.end(`${JSON.stringify(respond(JSON.parse(chunk)))}\n`)); });
    await new Promise((resolve) => server.listen(socketPath, resolve));
    try { await callDaemon(socketPath, 'wait', input); }
    finally { await new Promise((resolve) => server.close(resolve)); }
  }
  assert.equal(observed[0].supports_answer, undefined);
  assert.equal(observed[1].supports_answer, true);
  assert.equal(observed.every((params) => params.after_revision === undefined), true);
});

test('CLI omits send message_id for daemon generation and projects the returned id', async () => {
  const socketPath = path.join(os.tmpdir(), `zcode-cli-send-${process.pid}-${Date.now()}.sock`);
  let observedParams;
  const server = net.createServer((socket) => { socket.once('data', (chunk) => {
    const request = JSON.parse(chunk);
    observedParams = request.params;
    socket.end(JSON.stringify({ request_id: request.request_id, outcome: 'success', result: { kind: 'message', message_id: 'subagent-message-generated', disposition: 'queued', task: { agent_id: '10000001', phase: 'RUNNING', outcome: null, reason_code: null, stop_requested: false, close_requested: false, closed: false, reaped: false } } }) + '\n');
  }); });
  await new Promise((resolve) => server.listen(socketPath, resolve));
  try {
    const result = await callDaemon(socketPath, 'send', { agent_id: 10000001, content: 'continue' });
    assert.deepEqual(observedParams, { agent_id: '10000001', content: 'continue' });
    assert.deepEqual(result, { message_id: 'subagent-message-generated', disposition: 'queued' });
  } finally { await new Promise((resolve) => server.close(resolve)); }
});

test('CLI rejects list without repository or workspace scope before connecting', async () => {
  assert.throws(() => callDaemon(path.join(os.tmpdir(), `missing-zcode-list-${process.pid}.sock`), 'list', {}), (error) => error instanceof CliError && error.code === 'INVALID_ARGUMENT');
});

test('CLI maps workspace list scope to daemon repository scope', async () => {
  const socketPath = path.join(os.tmpdir(), `zcode-cli-rpc-list-${process.pid}-${Date.now()}.sock`);
  const server = net.createServer((socket) => { socket.once('data', (chunk) => { const request = JSON.parse(chunk); assert.equal(request.params.repository, '/workspace'); socket.end(JSON.stringify({ request_id: request.request_id, outcome: 'success', result: { tasks: [] } }) + '\n'); }); });
  await new Promise((resolve) => server.listen(socketPath, resolve));
  try { await callDaemon(socketPath, 'list', { workspace: '/workspace' }); }
  finally { await new Promise((resolve) => server.close(resolve)); }
});

test('CLI passes list agent filter and projects persisted input identity', async () => {
  const socketPath = path.join(os.tmpdir(), `external-cli-list-identity-${process.pid}-${Date.now()}.sock`);
  const identity = {
    admission: { agent: 'zcode', config_revision: 7, adapter_version: '0.1.0', model: null, model_source: 'native' },
    workspace_path: '/workspace', permission_mode: 'build', caller_prompt_sha256: 'private',
  };
  const server = net.createServer((socket) => socket.once('data', (chunk) => {
    const request = JSON.parse(chunk);
    assert.equal(request.method, 'task_list');
    assert.equal(request.params.agent, 'future-provider');
    socket.end(`${JSON.stringify({ request_id: request.request_id, outcome: 'success', result: {
      kind: 'task_listed', tasks: [{ agent_id: '10000001', phase: 'RUNNING', outcome: null, reason_code: null,
        stop_requested: false, close_requested: false, closed: false, reaped: false, input_identity: identity }], next_cursor: null,
    } })}\n`);
  }));
  await new Promise((resolve) => server.listen(socketPath, resolve));
  try {
    const result = await callDaemon(socketPath, 'list', { agent: 'future-provider', repository: '/workspace' });
    assert.deepEqual(result.tasks[0].input_identity, {
      agent: 'zcode', config_revision: 7, adapter_version: '0.1.0', model: null,
      model_source: 'native', workspace_path: '/workspace', permission_mode: 'build',
    });
  } finally { await new Promise((resolve) => server.close(resolve)); }
});

test('CLI applies documented list and result defaults before connecting', async () => {
  for (const [command, input, expected] of [
    ['list', { repository: '/workspace' }, { phase: null, outcome: null, cursor: null, limit: 100 }],
    ['result', { agent_id: 10000001 }, { offset: 0, limit: MAX_RESULT_CHUNK_BYTES }],
  ]) {
    const socketPath = path.join(os.tmpdir(), `zcode-cli-rpc-default-${command}-${process.pid}-${Date.now()}.sock`);
    const server = net.createServer((socket) => { socket.once('data', (chunk) => {
      const request = JSON.parse(chunk);
      for (const [name, value] of Object.entries(expected)) assert.deepEqual(request.params[name], value);
      const result = command === 'list'
        ? { kind: 'task_listed', tasks: [], next_cursor: null }
        : { kind: 'task_result', task: { agent_id: '10000001', phase: 'RUNNING', outcome: null, reason_code: null, stop_requested: false, close_requested: false, closed: false, reaped: false }, result: null };
      socket.end(JSON.stringify({ request_id: request.request_id, outcome: 'success', result }) + '\n');
    }); });
    await new Promise((resolve) => server.listen(socketPath, resolve));
    try { await callDaemon(socketPath, command, input); }
    finally { await new Promise((resolve) => server.close(resolve)); }
  }
});

test('CLI observe uses the shared read-only daemon snapshot without adding fields', async () => {
  const socketPath = path.join(os.tmpdir(), `zcode-cli-rpc-observe-${process.pid}-${Date.now()}.sock`);
  const observation = {
    agent_id: 10000001,
    tools: [],
    reasoning: { text: '', truncated: false },
    coverage: { tool_history_complete: false, reasoning_complete: false, dropped_events: 0 },
  };
  const server = net.createServer((socket) => { socket.once('data', (chunk) => {
    const request = JSON.parse(chunk);
    assert.equal(request.method, 'task_observe');
    assert.deepEqual(request.params, { agent_id: '10000001' });
    socket.end(`${JSON.stringify({ request_id: request.request_id, outcome: 'success', result: { kind: 'task_observed', observation } })}\n`);
  }); });
  await new Promise((resolve) => server.listen(socketPath, resolve));
  try {
    assert.deepEqual(await callDaemon(socketPath, 'observe', { agent_id: 10000001 }), { ...observation, agent_id: 10000001 });
  } finally { await new Promise((resolve) => server.close(resolve)); }
});

test('CLI public projection removes private RPC fields and result digest', () => {
  const task = { agent_id: '10000001', phase: 'TERMINAL', outcome: 'COMPLETED', reason_code: null, stop_requested: false, close_requested: false, closed: false, reaped: true };
  const projected = projectDaemonResult('result', {
    kind: 'task_result',
    task,
    result: { outcome: 'COMPLETED', final_text: 'ok', partial: false, result_sha256: 'private', offset: 0, total_bytes: 2, next_offset: null, complete: true },
  });
  assert.deepEqual(Object.keys(projected).sort(), ['question', 'result', 'task']);
  assert.equal(projected.task.resources_reaped, true);
  assert.equal(projected.task.reaped, undefined);
  assert.equal(projected.result.result_sha256, undefined);
  assert.equal(projected.question, null);
});

test('CLI result pages a recoverable pending question by request_id', async () => {
  const socketPath = path.join(os.tmpdir(), `zcode-cli-rpc-question-${process.pid}-${Date.now()}.sock`);
  const question = {
    text: 'Deploy to which environment? options: [production, staging]',
    offset: 2048,
    total_bytes: 2548,
    next_offset: null,
    complete: true,
  };
  const server = net.createServer((socket) => { socket.once('data', (chunk) => {
    const request = JSON.parse(chunk);
    assert.equal(request.method, 'task_result');
    assert.equal(request.params.agent_id, '10000001');
    assert.equal(request.params.request_id, 'question-1');
    assert.equal(request.params.offset, 2048);
    socket.end(JSON.stringify({
      request_id: request.request_id,
      outcome: 'success',
      result: {
        kind: 'task_result',
        task: { agent_id: '10000001', phase: 'WAITING_INPUT', outcome: null, reason_code: null, stop_requested: false, close_requested: false, closed: false, reaped: false },
        result: null,
        question,
      },
    }) + '\n');
  }); });
  await new Promise((resolve) => server.listen(socketPath, resolve));
  try {
    const projected = await callDaemon(socketPath, 'result', { agent_id: 10000001, request_id: 'question-1', offset: 2048 });
    assert.deepEqual(projected.question, question);
    assert.equal(projected.result, null);
    assert.throws(
      () => callDaemon(socketPath, 'result', { agent_id: 10000001, bogus: true }),
      (error) => error.code === 'INVALID_ARGUMENT' && /unsupported field/.test(error.message),
    );
  } finally { await new Promise((resolve) => server.close(resolve)); }
});

test('CLI reports unavailable daemon socket', async () => {
  await assert.rejects(() => callDaemon(path.join(os.tmpdir(), `missing-zcode-${process.pid}.sock`), 'cancel', { agent_id: 10000001 }), (error) => error.code === 'SOCKET_UNAVAILABLE');
});

test('CLI accepts a response exactly at the 2MiB frame cap and rejects one byte over', async () => {
  const responseFor = (request, targetBytes) => {
    const response = { request_id: request.request_id, outcome: 'success', result: { tasks: [] }, padding: '' };
    const base = Buffer.byteLength(JSON.stringify(response), 'utf8');
    response.padding = 'x'.repeat(Math.max(0, targetBytes - base));
    while (Buffer.byteLength(JSON.stringify(response), 'utf8') < targetBytes) response.padding += 'x';
    while (Buffer.byteLength(JSON.stringify(response), 'utf8') > targetBytes) response.padding = response.padding.slice(0, -1);
    return JSON.stringify(response) + '\n';
  };
  for (const [targetBytes, expectedError] of [[2 * 1024 * 1024 - 1, false], [2 * 1024 * 1024, true]]) {
    const socketPath = path.join(os.tmpdir(), `zcode-cli-rpc-cap-${process.pid}-${targetBytes}.sock`);
    const server = net.createServer((socket) => {
      let body = '';
      socket.on('data', (chunk) => { body += chunk; if (body.includes('\n')) socket.end(responseFor(JSON.parse(body), targetBytes)); });
    });
    await new Promise((resolve) => server.listen(socketPath, resolve));
    try {
      const call = callDaemon(socketPath, 'list', { repository: '/workspace' });
      if (expectedError) await assert.rejects(call, (error) => error instanceof CliError && error.code === 'OVERSIZED');
      else await call;
    } finally { await new Promise((resolve) => server.close(resolve)); }
  }
});

test('CLI rejects a chunked response that exceeds the cap before newline', async () => {
  const socketPath = path.join(os.tmpdir(), `zcode-cli-rpc-chunked-${process.pid}-${Date.now()}.sock`);
  const server = net.createServer((socket) => socket.once('data', () => {
    socket.write(Buffer.alloc(1024 * 1024, 0x78));
    setImmediate(() => socket.end(Buffer.alloc(1024 * 1024 + 1, 0x78)));
  }));
  await new Promise((resolve) => server.listen(socketPath, resolve));
  try { await assert.rejects(() => callDaemon(socketPath, 'list', { repository: '/workspace' }), (error) => error instanceof CliError && error.code === 'OVERSIZED'); }
  finally { await new Promise((resolve) => server.close(resolve)); }
});

test('CLI preserves a maximum control and Unicode result page', async () => {
  const socketPath = path.join(os.tmpdir(), `zcode-cli-rpc-result-${process.pid}-${Date.now()}.sock`);
  const expected = '\u0001'.repeat(100000) + '你好🙂';
  const server = net.createServer((socket) => socket.once('data', (chunk) => {
    const request = JSON.parse(chunk);
    socket.end(JSON.stringify({ request_id: request.request_id, outcome: 'success', result: { kind: 'task_result', task: { agent_id: '10000001', phase: 'TERMINAL', outcome: 'COMPLETED', reason_code: null, stop_requested: false, close_requested: false, reaped: true }, result: { outcome: 'COMPLETED', final_text: expected, partial: false, offset: 0, total_bytes: Buffer.byteLength(expected), next_offset: null, complete: true } } }) + '\n');
  }));
  await new Promise((resolve) => server.listen(socketPath, resolve));
  try { const result = await callDaemon(socketPath, 'result', { agent_id: 10000001, limit: 100000 }); assert.equal(result.result.final_text, expected); }
  finally { await new Promise((resolve) => server.close(resolve)); }
});
