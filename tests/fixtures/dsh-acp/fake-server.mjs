#!/usr/bin/env node
import readline from 'node:readline';

const rl = readline.createInterface({ input: process.stdin });
const send = (message) => process.stdout.write(`${JSON.stringify(message)}\n`);
rl.on('line', (line) => {
  let request;
  try { request = JSON.parse(line); } catch { process.stderr.write('fixture malformed input\n'); return; }
  if (request.method === 'initialize') send({ jsonrpc: '2.0', id: request.id, result: { protocolVersion: 1, capabilities: { models: true, cancel: true, permission: true } } });
  else if (request.method === 'session/new') send({ jsonrpc: '2.0', id: request.id, result: { sessionId: 'fixture-session' } });
  else if (request.method === 'models/list') send({ jsonrpc: '2.0', id: request.id, result: { models: [{ id: 'fixture-model', name: 'Fixture' }] } });
  else if (request.method === 'session/prompt') {
    send({ jsonrpc: '2.0', method: 'session/update', params: { sessionId: 'fixture-session', update: { type: 'tool_call', toolCallId: 'tool-1', title: 'read-only probe' } } });
    if (request.params.prompt.includes('permission')) send({ jsonrpc: '2.0', method: 'session/request_permission', params: { requestId: 'permission-1', toolCallId: 'tool-1', options: [{ kind: 'allow_once' }, { kind: 'reject_once' }] } });
    send({ jsonrpc: '2.0', method: 'session/update', params: { sessionId: 'fixture-session', update: { type: 'agent_message', messageId: 'message-1', content: [{ type: 'text', text: 'hi' }] } } });
    send({ jsonrpc: '2.0', id: request.id, result: { stopReason: 'end_turn', messageId: 'message-1' } });
  } else if (request.method === 'session/cancel') send({ jsonrpc: '2.0', id: request.id, result: { cancelled: true } });
  else send({ jsonrpc: '2.0', id: request.id, error: { code: -32601, message: 'method not found' } });
});
