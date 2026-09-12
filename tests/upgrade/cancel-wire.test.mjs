import test from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import fs from 'node:fs';
import path from 'node:path';
import { updateCommand } from '../../cli/commands/update.mjs';

test('update cancel-active survives the real CLI RPC encoder', async (t) => {
  const base = path.resolve('tests/live-agent/workspace');
  fs.mkdirSync(base, { recursive: true });
  const root = fs.mkdtempSync(path.join(base, 'cancel-'));
  const socketPath = path.join(root, 'd.sock');
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const frames = [];
  let readyAt = 0;
  const server = net.createServer((socket) => {
    let body = '';
    socket.on('data', (chunk) => {
      body += chunk;
      if (!body.includes('\n')) return;
      const request = JSON.parse(body);
      frames.push(request);
      if (request.method === 'daemon_begin_drain') readyAt = request.params.cancel_active ? Date.now() + 6100 : 0;
      const ready = Date.now() >= readyAt;
      if (request.method === 'daemon_activate_ready') assert.equal(ready, true, 'activation preceded reap readiness');
      socket.end(JSON.stringify({ version: 13, request_id: request.request_id, outcome: 'success',
        result: { kind: 'daemon_drain_status', ready_for_activation: ready, resources_reaped: ready, activation_claim: null },
      }) + '\n');
    });
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(socketPath, resolve); });
  t.after(() => new Promise((resolve) => server.close(resolve)));
  await updateCommand({ state: path.join(root, 'state'), socket: socketPath }, ['--cancel-active', '--yes']);
  assert.equal(frames[0].method, 'daemon_begin_drain');
  assert.deepEqual(frames[0].params, { cancel_active: true });
  assert.equal(frames[1].method, 'daemon_drain_status');
  assert.equal(frames.at(-1).method, 'daemon_activate_ready');
  frames.length = 0;
  await updateCommand({ state: path.join(root, 'state'), socket: socketPath }, []);
  assert.deepEqual(frames[0].params, { cancel_active: false });
});
