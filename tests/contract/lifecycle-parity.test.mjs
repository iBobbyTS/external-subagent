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

function task(phase = 'running', outcome = null) {
  return {
    agent_id: agentId,
    phase,
    outcome,
    stop_requested: phase === 'cancelling',
    close_requested: phase === 'closed',
    closed: phase === 'closed',
    reaped: phase === 'closed',
  };
}

function rpcResult(requestId, result) {
  return `${JSON.stringify({ version: 13, request_id: requestId, outcome: 'success', result })}\n`;
}

async function lifecycleServer(socketPath) {
  let phase = 'running';
  let respondCount = 0;
  const pending = Array.from({ length: 101 }, (_, index) => ({
    request_id: `request-${index + 1}`,
    kind: 'permission', state: 'pending', respondable: true,
    tool_name: 'Read', operation: 'read', summary: `read ${index + 1}`,
    policy_preview: 'allow_once',
  }));
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
        result = { task: task(), disposition: 'admitted' };
      } else if (request.method === 'task_message') {
        assert.equal(phase, 'running');
        result = { message_id: request.params.message_id, disposition: 'queued', task: task() };
      } else if (request.method === 'task_wait') {
        // The public projection is capped at 100; the 101st request must not
        // wake this wait and must remain undisclosed.
        result = {
          task: task(), revision: 7, next_revision: 7, pending_requests: pending.slice(0, 100),
          command_pending_approval: false, result_available: false,
          activity: { state: 'active', latest_text_tail: '', latest_text_updated_at: null,
            latest_text_truncated: false, active_tools: [], window_60s: {}, telemetry_status: 'healthy' },
          latest_progress: null, result: null,
          instruction: 'Not finished yet, call wait again', timed_out: true,
        };
      } else if (request.method === 'task_respond') {
        respondCount += 1;
        if (phase === 'cancelling' || respondCount > 1) {
          socket.end(`${JSON.stringify({ version: 13, request_id: request.request_id, outcome: 'error', error: { code: 'REQUEST_NOT_PENDING', message: 'late or duplicate response' } })}\n`);
          return;
        }
        result = { outcome: { disposition: 'accepted', policy_reason_code: 'allow_once' }, task: task() };
      } else if (request.method === 'task_cancel') {
        phase = 'cancelling';
        result = { task: task('cancelling') };
      } else if (request.method === 'task_observe') {
        result = { observation: { schema: 'external-subagent-observation/1.0', agent_id: String(agentId),
          service_generation: 'fixture', snapshot_seq: 1, count_scope: 'agent_lifetime', tools: [],
          reasoning: { text: 'bounded', char_count: 7, truncated: false, source: { status: 'VERIFIED' } },
          coverage: { tool_history_complete: true, reasoning_complete: true, dropped_events: 0 } } };
      } else if (request.method === 'task_close') {
        phase = 'closed';
        result = { task: task('closed', 'cancelled') };
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
      env: { ...process.env, ZCODE_AGENTD_SOCKET: socket }, cwd: root,
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
    const created = await runCli(socket, 'spawn', { agent: 'zcode', repository: directory, prompt: 'hello' });
    assert.equal(created.result.agent_id, agentId);
    const sent = await runCli(socket, 'send', { agent_id: agentId, message_id: 'message-1', content: 'follow up' });
    assert.equal(sent.result.disposition, 'queued');
    const waited = await runCli(socket, 'wait', { agent_id: agentId, wait_time: 0 });
    assert.equal(waited.result.pending_requests.length, 100);
    assert.equal(waited.result.command_pending_approval, false);
    const response = await runCli(socket, 'respond', { agent_id: agentId, request_id: 'request-1', decision: 'allow' });
    assert.equal(response.result.disposition, 'accepted');
    await assert.rejects(() => runCli(socket, 'respond', { agent_id: agentId, request_id: 'request-1', decision: 'allow' }), /late or duplicate|REQUEST_NOT_PENDING/);
    await runCli(socket, 'cancel', { agent_id: agentId });
    await assert.rejects(() => runCli(socket, 'respond', { agent_id: agentId, request_id: 'request-2', decision: 'deny' }), /late or duplicate|REQUEST_NOT_PENDING/);
    const observed = await runCli(socket, 'observe', { agent_id: agentId });
    assert.equal(observed.result.schema, 'external-subagent-observation/1.0');
    assert.equal(observed.result.agent_id, agentId);
    const closed = await runCli(socket, 'close', { agent_id: agentId });
    assert.equal(closed.result.task.closed, true);
    assert.equal(fixture.getRespondCount(), 3);
  } finally {
    await new Promise((resolve) => fixture.server.close(resolve));
    await fs.rm(directory, { recursive: true, force: true });
  }
});
