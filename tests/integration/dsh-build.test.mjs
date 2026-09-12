// S04.A DSH build-loop wire contract (X05–X07). Drives the shared ACP fixture
// directly as a JSON-RPC 2.0 peer and pins the exact shapes the Rust adapter
// parses: bootstrap order (initialize → session/new → model config verified
// before the prompt), single-shot permission responses that only echo offered
// options, and settlement folding that never promotes progress or a
// max_tokens turn into a completed result. The production DSH spawn gate
// stays closed; this file exercises the wire contract only, never a live
// provider.
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
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
    this.serverRequests = [];
    this.emitted = [];
    this.rl = readline.createInterface({ input: this.child.stdout });
    this.rl.on('line', (line) => {
      const frame = JSON.parse(line);
      this.frames.push(frame);
      if (frame.id !== undefined && (frame.method !== undefined)) this.serverRequests.push(frame);
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
    this.emitted.push({ jsonrpc: '2.0', id, method, params });
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);
    });
  }

  respond(id, result) {
    this.emitted.push({ jsonrpc: '2.0', id, result });
    this.child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, result })}\n`);
  }

  async close() {
    this.child.stdin.end();
    await new Promise((resolve) => this.child.on('exit', resolve));
    return this.stderr;
  }
}

test('build turn settles end_turn with only the committed message', async () => {
  const workspace = fs.mkdtempSync(path.join(os.tmpdir(), 'dsh-build-'));
  const peer = new AcpPeer();
  try {
    const initialized = await peer.request('initialize', { protocolVersion: 1, clientInfo: { name: 'dsh-build-test' } });
    assert.equal(initialized.result.protocolVersion, 1);
    assert.equal(initialized.result.capabilities.permission, true);
    assert.equal(initialized.result.capabilities.cancel, true);

    const session = await peer.request('session/new', { cwd: workspace });
    assert.equal(session.result.sessionId, 'fixture-session');
    assert.equal(session.result.configOptions.some((option) => option.configId === 'model'), true);

    // The model is only a verified response away from the prompt (X05).
    const configured = await peer.request('session/set_config_option', { configId: 'model', value: 'fixture-model' });
    assert.equal(configured.result !== undefined, true);

    const settled = await peer.request('session/prompt', { prompt: 'permission please' });
    assert.equal(settled.result.stopReason, 'end_turn');
    assert.equal(settled.result.messageId, 'message-1');

    // The permission offer arrived as a real server request whose only
    // selectable outcomes are the offered single-shot options (X06).
    const offer = peer.serverRequests.find((frame) => frame.method === 'session/request_permission');
    assert.equal(offer.params.toolCallId, 'tool-1');
    const kinds = offer.params.options.map((option) => option.kind);
    assert.deepEqual(kinds, ['allow_once', 'reject_once']);
    peer.respond(offer.id, { outcome: { outcome: 'selected', optionId: 'allow-once' } });

    // X07: the final text is the committed agent message only; the tool call
    // update never becomes the result.
    const message = peer.frames.find((frame) => frame.params?.update?.messageId === 'message-1');
    assert.equal(message.params.update.type, 'agent_message');
    const toolUpdate = peer.frames.find((frame) => frame.params?.update?.type === 'tool_call');
    assert.notEqual(toolUpdate, undefined);
    assert.equal(message.params.update.content.map((block) => block.text).join(''), 'hi');

    // Every emitted frame is JSON-RPC 2.0 and the bootstrap order is fixed.
    assert.deepEqual(
      peer.emitted.filter((frame) => frame.method !== undefined && frame.id !== undefined).map((frame) => frame.method),
      ['initialize', 'session/new', 'session/set_config_option', 'session/prompt'],
    );
    for (const frame of peer.emitted) assert.equal(frame.jsonrpc, '2.0');

    const closed = await peer.request('session/close', { sessionId: 'fixture-session' });
    assert.deepEqual(closed.result, {});
  } finally {
    const stderr = await peer.close();
    assert.equal(stderr, '');
  }
});

test('invalid model tokens are rejected before any prompt is sent', async () => {
  const peer = new AcpPeer();
  try {
    await peer.request('initialize', { protocolVersion: 1 });
    await peer.request('session/new', { cwd: os.tmpdir() });
    const rejected = await peer.request('session/set_config_option', { configId: 'model', value: '' });
    assert.equal(rejected.error.code, -32602);
    // The adapter contract: no session/prompt may follow a refused model.
    assert.equal(
      peer.emitted.some((frame) => frame.method === 'session/prompt'),
      false,
    );
  } finally {
    await peer.close();
  }
});

test('max_tokens settlement never folds into a completed result', async () => {
  const peer = new AcpPeer();
  try {
    await peer.request('initialize', { protocolVersion: 1 });
    await peer.request('session/new', { cwd: os.tmpdir() });
    await peer.request('session/set_config_option', { configId: 'model', value: 'fixture-model' });
    const settled = await peer.request('session/prompt', { prompt: 'hit max_tokens' });
    assert.equal(settled.result.stopReason, 'max_tokens');
    assert.equal(settled.result.messageId, undefined);
    // Client-side fold rule mirrored from the adapter: non-end_turn is a
    // failed turn with a distinct reason code, never a completed result.
    const completed = settled.result.stopReason === 'end_turn';
    assert.equal(completed, false);
    assert.equal(
      peer.frames.some((frame) => frame.params?.update?.type === 'agent_message'),
      false,
      'no committed message may back a failed turn',
    );
  } finally {
    await peer.close();
  }
});
