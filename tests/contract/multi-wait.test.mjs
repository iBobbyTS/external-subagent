import { test } from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { callDaemon } from '../../cli/rpc.mjs';

// Wire contract for list-mode task_wait: the request carries the id array
// verbatim and the response is a FLAT keyed object — one key per distinct
// agent id carrying the wait body, plus one concentrated `error` key for
// duplicated/unknown ids. This is a wire-level fixture, not a real daemon.
const win32Sockets = process.platform === 'win32'
  ? 'win32 cannot bind a filesystem *.sock fixture'
  : false;

function waitBody(id, overrides = {}) {
  return {
    task: { agent_id: String(id), status: 'terminal' },
    pending_requests: [],
    activity: { latest_text_tail: '', latest_text_truncated: false, tool_calls_last_60s: 0, telemetry_status: 'healthy' },
    result: { outcome: 'COMPLETED', final_text: `done-${id}`, partial: false, offset: 0, total_bytes: 9, next_offset: null, reason_code: null, failure_message: null },
    timed_out: false,
    message_receipt: null,
    ...overrides,
  };
}

test('list-mode wait round-trips the flat keyed contract', { skip: win32Sockets }, async () => {
  const socketPath = path.join(os.tmpdir(), `es-wait-many-contract-${process.pid}-${Date.now()}.sock`);
  const seenRequests = [];
  const server = net.createServer((socket) => {
    let buffer = '';
    socket.setEncoding('utf-8');
    socket.on('data', (chunk) => {
      buffer += chunk;
      const newline = buffer.indexOf('\n');
      if (newline < 0) return;
      const request = JSON.parse(buffer.slice(0, newline));
      seenRequests.push(request);
      const distinct = [...new Set(request.params.agent_id)];
      const response = { kind: 'task_wait_many' };
      for (const id of distinct) {
        response[id] = id === '10000002' ? undefined : waitBody(id);
      }
      response.error = { '10000002': 'unknown_agent' };
      socket.end(`${JSON.stringify({
        request_id: request.request_id,
        outcome: 'success',
        result: response,
      })}\n`);
    });
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(socketPath, resolve); });
  try {
    const result = await callDaemon(socketPath, 'wait', { agent_id: [10000001, 10000002, 10000001], wait_time: 0 });
    // Request side: the array is forwarded verbatim (duplication preserved).
    assert.deepEqual(seenRequests[0].params.agent_id, ['10000001', '10000002', '10000001']);
    // Response side: flat public-id keys; the concentrated error map carries
    // the unknown id. The duplicated id's body is served under its own key.
    assert.deepEqual(Object.keys(result).sort(), ['10000001', 'error']);
    assert.equal(result['10000001'].result.final_text, 'done-10000001');
    assert.deepEqual(result['10000001'].task, { agent_id: 10000001, status: 'terminal' });
    assert.equal(result['10000001'].message_receipt, null);
    assert.deepEqual(result.error, { '10000002': 'unknown_agent' });
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }
});
