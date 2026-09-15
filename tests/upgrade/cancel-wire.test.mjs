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
      if (request.method === 'daemon_begin_drain') readyAt = request.params?.cancel_active ? Date.now() + 6100 : 0;
      const ready = Date.now() >= readyAt;
      if (request.method === 'daemon_activate_ready') assert.equal(ready, true, 'activation preceded reap readiness');
      socket.end(JSON.stringify({ request_id: request.request_id, outcome: 'success',
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
  assert.equal(Object.hasOwn(frames[0], 'params'), false);
});

test('a failed update aborts the drain through the real CLI RPC encoder', async (t) => {
  const base = path.resolve('tests/live-agent/workspace');
  fs.mkdirSync(base, { recursive: true });
  const root = fs.mkdtempSync(path.join(base, 'abort-'));
  const socketPath = path.join(root, 'd.sock');
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const frames = [];
  const server = net.createServer((socket) => {
    let body = '';
    socket.on('data', (chunk) => {
      body += chunk;
      if (!body.includes('\n')) return;
      const request = JSON.parse(body);
      frames.push(request);
      const result = request.method === 'daemon_activate_ready'
        ? { kind: 'daemon_drain_status', ready_for_activation: true, resources_reaped: true, activation_claim: 'abort-claim' }
        : request.method === 'daemon_abort_drain'
          ? { kind: 'daemon_drain_status', is_draining: false, ready_for_activation: false, activation_claim: null }
          : { kind: 'daemon_drain_status', ready_for_activation: true, resources_reaped: true, activation_claim: null };
      socket.end(JSON.stringify({ request_id: request.request_id, outcome: 'success', result }) + '\n');
    });
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(socketPath, resolve); });
  t.after(() => new Promise((resolve) => server.close(resolve)));
  await assert.rejects(updateCommand({ state: path.join(root, 'state'), socket: socketPath }, ['--version=2'], {
    preflightUpdate: () => ({}),
    updateInstallation: async () => { throw new Error('boom'); },
  }), /boom/);
  assert.equal(frames[0].method, 'daemon_begin_drain');
  assert.equal(frames[1].method, 'daemon_activate_ready');
  const abort = frames.at(-1);
  assert.equal(abort.method, 'daemon_abort_drain', 'the failed update must abort the drain over the real encoder');
  assert.equal(Object.hasOwn(abort, 'params'), false, 'the abort is a unit-variant method like drain-status');
  const receipt = JSON.parse(fs.readFileSync(path.join(root, 'state.activation.json'), 'utf8'));
  assert.equal(receipt.status, 'failed');
  assert.equal(receipt.drain_aborted, true);
});

// R2 compatibility oracle: a NEW CLI upgrading against an OLD daemon whose
// method table predates daemon_abort_drain. The error frame below is the
// daemon's real wire behavior, not an invented one: every released daemon
// since the s02a migration gates method names through RpcMethod::is_known,
// and RpcErrorCode serializes snake_case — so the unknown abort method
// answers { code: 'unknown_method', message: 'unknown RPC method' } (see
// crates/external-daemon/src/rpc.rs). This is a controlled old-protocol wire
// fixture running through the REAL CLI encoder/parser — compatibility
// evidence, not a full old-binary proof.
test('a legacy daemon without drain-abort keeps the original failure and the still-draining evidence over the real wire', async (t) => {
  const base = path.resolve('tests/live-agent/workspace');
  fs.mkdirSync(base, { recursive: true });
  const root = fs.mkdtempSync(path.join(base, 'legacy-abort-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const socketPath = path.join(root, 'd.sock');
  const frames = [];
  const server = net.createServer((socket) => {
    let body = '';
    socket.on('data', (chunk) => {
      body += chunk;
      if (!body.includes('\n')) return;
      const request = JSON.parse(body);
      frames.push(request);
      if (request.method === 'daemon_abort_drain') {
        socket.end(JSON.stringify({ request_id: request.request_id, outcome: 'error',
          error: { code: 'unknown_method', message: 'unknown RPC method' } }) + '\n');
        return;
      }
      const result = request.method === 'daemon_activate_ready'
        ? { kind: 'daemon_drain_status', ready_for_activation: true, resources_reaped: true, activation_claim: 'legacy-abort-claim' }
        : { kind: 'daemon_drain_status', ready_for_activation: true, resources_reaped: true, activation_claim: null };
      socket.end(JSON.stringify({ request_id: request.request_id, outcome: 'success', result }) + '\n');
    });
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(socketPath, resolve); });
  t.after(() => new Promise((resolve) => server.close(resolve)));
  await assert.rejects(updateCommand({ state: path.join(root, 'state'), socket: socketPath }, ['--version=2'], {
    preflightUpdate: () => ({}),
    updateInstallation: async () => { throw new Error('boom'); },
  }), /boom/);
  assert.deepEqual(frames.map((request) => request.method),
    ['daemon_begin_drain', 'daemon_activate_ready', 'daemon_abort_drain']);
  const receipt = JSON.parse(fs.readFileSync(path.join(root, 'state.activation.json'), 'utf8'));
  assert.equal(receipt.status, 'failed', 'the failed abort must never be masked as success');
  assert.equal(receipt.retryable, true);
  assert.equal(receipt.error, 'boom', 'the receipt keeps the original update failure reason');
  assert.equal(receipt.drain_aborted, false);
  assert.equal(receipt.drain_abort_error.code, 'UNKNOWN_METHOD',
    'the raw snake_case wire code is recorded in the canonical CLI vocabulary');
  assert.equal(receipt.drain_abort_error.message, 'unknown RPC method',
    'the daemon evidence message is preserved verbatim');
  assert.equal(receipt.daemon_may_be_draining, true,
    'a legacy daemon that never aborted the drain may still be draining');
});

test('legacy daemon accepts default drain and rejects explicit cancellation without fallback', async (t) => {
  const base = path.resolve('tests/live-agent/workspace');
  fs.mkdirSync(base, { recursive: true });
  const root = fs.mkdtempSync(path.join(base, 'legacy-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const socketPath = path.join(root, 'd.sock');
  const frames = [];
  const server = net.createServer((socket) => {
    let body = '';
    socket.on('data', (chunk) => {
      body += chunk;
      if (!body.includes('\n')) return;
      const request = JSON.parse(body);
      frames.push(request);
      // Previous RpcMethod::DaemonBeginDrain is a unit variant. Its decoder
      // accepts no params; both false and true parameter objects are invalid.
      // The rejected frame is the daemon's real wire bytes: RpcErrorCode
      // serializes snake_case ("validation"), never a SCREAMING CLI code.
      const rejected = Object.hasOwn(request, 'params');
      socket.end(JSON.stringify({ request_id: request.request_id,
        ...(rejected ? { outcome: 'error', error: { code: 'validation', message: 'request fields are invalid' } }
          : { outcome: 'success', result: { kind: 'daemon_drain_status', ready_for_activation: true, activation_claim: null } }),
      }) + '\n');
    });
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(socketPath, resolve); });
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const paths = { state: path.join(root, 'state'), socket: socketPath };
  await updateCommand(paths, []);
  assert.deepEqual(frames.map((request) => request.method), ['daemon_begin_drain', 'daemon_activate_ready']);
  assert.equal(Object.hasOwn(frames[0], 'params'), false);
  frames.length = 0;
  await assert.rejects(updateCommand(paths, ['--cancel-active', '--yes']), { code: 'CANCEL_ACTIVE_UNSUPPORTED' });
  assert.equal(frames.length, 1, 'must not retry without cancellation or activate');
  assert.deepEqual(frames[0].params, { cancel_active: true });
});
