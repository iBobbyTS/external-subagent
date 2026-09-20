import test from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import fs from 'node:fs/promises';
import { spawn } from 'node:child_process';

const root = path.resolve(import.meta.dirname, '../..');
const cli = path.join(root, 'bin/external-subagent.mjs');
const agentId = 10000011;

function task(status = 'running') {
  return {
    agent_id: agentId,
    status,
    session_id: null,
    input_identity: null,
  };
}

function rpcResult(requestId, result) {
  return `${JSON.stringify({ request_id: requestId, outcome: 'success', result })}\n`;
}

async function lifecycleServer(socketPath) {
  let status = 'running';
  let respondCount = 0;
  // This is a wire-level fixture for CLI/RPC mapping evidence. It does not
  // claim to be a real daemon or to exercise a provider runtime. The wire
  // contract mirrors the slim public views: every actionable pending record
  // (permission or user_input, still pending) wakes the wait and only those
  // records are projected.
  const pending = Array.from({ length: 101 }, (_, index) => ({
    request_id: `request-${index + 1}`,
    kind: 'permission',
    tool_name: 'Read',
    operation: 'read',
    summary: `read ${index + 1}`,
    state: 'pending',
  }));
  const actionable = () => pending
    .filter((item) => item.state === 'pending' && ['permission', 'user_input'].includes(item.kind))
    .slice(0, 100);
  const server = net.createServer((socket) => {
    let buffer = '';
    socket.setEncoding('utf8');
    socket.on('data', (chunk) => {
      buffer += chunk;
      const newline = buffer.indexOf('\n');
      if (newline < 0) return;
      const request = JSON.parse(buffer.slice(0, newline));
      let result;
      if (request.method === 'submit_general') {
        result = { task: task(), disposition: 'created' };
      } else if (request.method === 'task_message') {
        assert.equal(status, 'running');
        result = { message_id: request.params.message_id, disposition: 'queued', task: task() };
      } else if (request.method === 'task_wait') {
        const projected = actionable();
        const timedOut = projected.length === 0;
        result = {
          task: task(status),
          pending_requests: projected.map(({ state, ...view }) => view),
          result_available: false,
          activity: { latest_text_tail: '', latest_text_truncated: false, latest_reasoning: '',
            tool_calls_last_60s: 0, telemetry_status: 'healthy' },
          result: null,
          instruction: timedOut ? 'Not finished yet, call wait again' : null,
          timed_out: timedOut,
          message_receipt: request.params.message_id
            ? { message_id: request.params.message_id, state: 'queued', failure_code: null }
            : null,
        };
      } else if (request.method === 'task_respond') {
        respondCount += 1;
        const target = pending.find((item) => item.request_id === request.params.request_id);
        if (status === 'cancelling' || !target || target.state !== 'pending') {
          socket.end(`${JSON.stringify({ request_id: request.request_id, outcome: 'error', error: { code: 'REQUEST_NOT_PENDING', message: 'late or duplicate response' } })}\n`);
          return;
        }
        target.state = 'responded';
        result = { outcome: { disposition: 'accepted', policy_reason_code: null }, task: task() };
      } else if (request.method === 'task_cancel') {
        status = 'cancelling';
        result = { task: task('cancelling') };
      } else if (request.method === 'task_observe') {
        result = { observation: { schema: 'external-subagent-observation/1.0', agent_id: String(agentId),
          count_scope: 'agent_lifetime', tools: [],
          reasoning: { text: 'bounded', truncated: false, source: { status: 'VERIFIED' } },
          coverage: { tool_history_complete: true, reasoning_complete: true, dropped_events: 0 } } };
      } else if (request.method === 'task_close') {
        status = 'closed';
        result = { task: task('closed') };
      } else {
        result = { status: { ready: true } };
      }
      socket.end(rpcResult(request.request_id, result));
    });
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(socketPath, resolve); });
  return { server, getRespondCount: () => respondCount };
}

function runCli(socket, command, input) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [cli, command, '--json', JSON.stringify(input)], {
      env: { ...process.env, EXTERNAL_SUBAGENT_SOCKET: socket }, cwd: root,
    });
    let stdout = ''; let stderr = '';
    child.stdout.on('data', (chunk) => { stdout += chunk; });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    child.on('error', reject);
    child.on('close', (code) => code === 0
      ? resolve(JSON.parse(stdout))
      : reject(Object.assign(new Error(`${stderr}\n${stdout}`), { stdout, stderr })));
  });
}

test('lifecycle parity preserves queue, bounded approval, cancellation and observe semantics', async () => {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'external-subagent-parity-'));
  const socket = path.join(directory, 'daemon.sock');
  const fixture = await lifecycleServer(socket);
  try {
    const created = await runCli(socket, 'spawn', { subagent: 'zcode', repository: directory, prompt: 'hello' });
    assert.equal(created.result.agent_id, agentId);
    assert.equal(created.result.status, 'running');
    const sent = await runCli(socket, 'send', { agent_id: agentId, message_id: 'message-1', content: 'follow up' });
    assert.equal(sent.result.disposition, 'queued');
    const waited = await runCli(socket, 'wait', { agent_id: agentId, wait_time: 0 });
    // The projection stays capped at 100 actionable records and the first
    // actionable request wakes the wait.
    assert.equal(waited.result.pending_requests.length, 100);
    assert.equal(waited.result.timed_out, false);
    assert.deepEqual(waited.result.pending_requests[0], {
      request_id: 'request-1', kind: 'permission',
      tool_name: 'Read', operation: 'read', summary: 'read 1',
    });
    assert.equal(waited.result.message_receipt, null);
    const response = await runCli(socket, 'respond', { agent_id: agentId, request_id: 'request-1', decision: 'allow' });
    assert.equal(response.result.disposition, 'accepted');
    // Every remaining request is still actionable: the next wait wakes again
    // with the responded record dropped from the projection.
    const afterResponse = await runCli(socket, 'wait', { agent_id: agentId, wait_time: 0 });
    assert.equal(afterResponse.result.timed_out, false);
    assert.equal(afterResponse.result.pending_requests.length, 100);
    assert.equal(afterResponse.result.pending_requests[0].request_id, 'request-2');
    assert.equal(afterResponse.result.pending_requests.some(({ request_id }) => request_id === 'request-1'), false);
    await assert.rejects(() => runCli(socket, 'respond', { agent_id: agentId, request_id: 'request-1', decision: 'allow' }), /late or duplicate|REQUEST_NOT_PENDING/);
    await runCli(socket, 'cancel', { agent_id: agentId });
    const cancelled = await runCli(socket, 'wait', { agent_id: agentId, wait_time: 0 });
    assert.equal(cancelled.result.task.status, 'cancelling');
    await assert.rejects(() => runCli(socket, 'respond', { agent_id: agentId, request_id: 'request-2', decision: 'deny' }), /late or duplicate|REQUEST_NOT_PENDING/);
    const observed = await runCli(socket, 'observe', { agent_id: agentId });
    assert.equal(observed.result.schema, 'external-subagent-observation/1.0');
    assert.equal(observed.result.agent_id, agentId);
    const closed = await runCli(socket, 'close', { agent_id: agentId });
    assert.equal(closed.result.task.status, 'closed');
    assert.equal(fixture.getRespondCount(), 3);
  } finally {
    await new Promise((resolve) => fixture.server.close(resolve));
    await fs.rm(directory, { recursive: true, force: true });
  }
});
