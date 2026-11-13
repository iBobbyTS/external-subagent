// DSH ACP standard-update wire contract.
//
// The reference server emits standard semantic updates: committed assistant
// text as ordered `agent_message_chunk` deltas, a prompt settlement that
// carries only `stopReason`, and a generic tool lifecycle whose `title` is the
// tool's real name, whose `kind` is the category `other`, and whose `status`
// carries the pending/in_progress/completed/failed phase. This file pins those
// public shapes on the shared fixture; the adapter-level assertions live in
// the Rust contract tests selected by `rust:daemon-adapters`. No live provider
// is contacted.
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';

const fixture = fileURLToPath(new URL('../../tests/fixtures/dsh-acp/fake-server.mjs', import.meta.url));

class AcpPeer {
  constructor() {
    this.child = spawn(process.execPath, [fixture], { stdio: ['pipe', 'pipe', 'pipe'] });
    this.nextId = 1;
    this.pending = new Map();
    this.frames = [];
    this.rl = readline.createInterface({ input: this.child.stdout });
    this.rl.on('line', (line) => {
      const frame = JSON.parse(line);
      this.frames.push(frame);
      if (frame.id !== undefined && frame.method === undefined && this.pending.has(frame.id)) {
        const { resolve } = this.pending.get(frame.id);
        this.pending.delete(frame.id);
        resolve(frame);
      }
    });
    this.stderr = '';
    this.child.stderr.on('data', (chunk) => { this.stderr += chunk; });
  }

  request(method, params) {
    const id = this.nextId++;
    return new Promise((resolve) => {
      this.pending.set(id, { resolve });
      this.child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);
    });
  }

  updates() {
    return this.frames
      .filter((frame) => frame.method === 'session/update')
      .map((frame) => frame.params.update);
  }

  async close() {
    this.child.stdin.end();
    await new Promise((resolve) => this.child.on('exit', resolve));
    return this.stderr;
  }
}

async function openSession(peer) {
  const initialized = await peer.request('initialize', { protocolVersion: 1, clientInfo: { name: 'dsh-standard-updates' } });
  assert.equal(initialized.result.protocolVersion, 1);
  const session = await peer.request('session/new', { cwd: os.tmpdir() });
  assert.equal(session.result.sessionId, 'fixture-session');
  const configured = await peer.request('session/set_config_option', { configId: 'model', value: 'fixture-model' });
  assert.notEqual(configured.result, undefined);
}

test('standard chunks and a bare stopReason settle the prompt', async () => {
  const peer = new AcpPeer();
  try {
    await openSession(peer);
    const settled = await peer.request('session/prompt', { prompt: 'standard chunk please' });
    // The settlement carries the stop reason only: no messageId, no
    // committed flag, and nothing the client must echo back.
    assert.deepEqual(settled.result, { stopReason: 'end_turn' });

    const chunks = peer.updates().filter((update) => update.sessionUpdate === 'agent_message_chunk');
    assert.equal(chunks.length, 2);
    assert.equal(chunks[0].messageId, 'fixture-message');
    assert.equal(chunks[0].content.text, 'standard ');
    // A chunk may omit its message id; the prompt is the only boundary the
    // standard flow guarantees.
    assert.equal(Object.hasOwn(chunks[1], 'messageId'), false);
    assert.equal(chunks[1].content.text, 'answer');
  } finally {
    const stderr = await peer.close();
    assert.equal(stderr, '');
  }
});

test('a failed tool update carries the title name and a terminal status', async () => {
  const peer = new AcpPeer();
  try {
    await openSession(peer);
    const settled = await peer.request('session/prompt', { prompt: 'failed tool please' });
    assert.equal(settled.result.stopReason, 'end_turn');

    const call = peer.updates().find((update) => update.sessionUpdate === 'tool_call' && update.toolCallId === 'tool-2');
    assert.equal(call.title, 'Probe the workspace');
    // `other` is the category for a generic lifecycle, never the tool name.
    assert.equal(call.kind, 'other');
    assert.equal(call.status, 'in_progress');
    assert.deepEqual(call.rawInput, { path: '.' });

    const finished = peer.updates().find((update) => update.sessionUpdate === 'tool_call_update' && update.toolCallId === 'tool-2');
    assert.equal(finished.status, 'failed');
    // A content array may accompany a terminal status; it is not the evidence
    // that decides the outcome.
    assert.equal(Array.isArray(finished.content), true);
  } finally {
    const stderr = await peer.close();
    assert.equal(stderr, '');
  }
});
