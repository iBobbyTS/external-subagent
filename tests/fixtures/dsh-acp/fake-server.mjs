#!/usr/bin/env node
// DSH ACP wire fixture shared by the discovery probe, the catalog
// probe, and the build-loop contract tests. It speaks exactly one
// dialect: JSON-RPC 2.0 newline frames with protocolVersion 1, a `model`
// config option, single-shot permission options on a real server request id,
// and prompt settlements that fold a committed agent message.
//
// Prompt keywords select the standard-contract scenarios the reference
// server produces: `standard chunk` sends ordered `agent_message_chunk`
// updates and settles with a bare `stopReason`, and `failed tool` sends a
// generic tool lifecycle whose `title` is the tool's real name, `kind` is the
// category `other`, and `status` drives the completed/failed phase.
import readline from 'node:readline';

const rl = readline.createInterface({ input: process.stdin });
const send = (message) => process.stdout.write(`${JSON.stringify(message)}\n`);
const update = (payload) => send({ jsonrpc: '2.0', method: 'session/update', params: { sessionId: 'fixture-session', update: payload } });
rl.on('line', (line) => {
  let request;
  try { request = JSON.parse(line); } catch { process.stderr.write('fixture malformed input\n'); return; }
  if (request.method === 'initialize') send({ jsonrpc: '2.0', id: request.id, result: { protocolVersion: 1, capabilities: { models: true, cancel: true, permission: true } } });
  else if (request.method === 'session/new') send({ jsonrpc: '2.0', id: request.id, result: { sessionId: 'fixture-session', configOptions: [{ configId: 'model', currentValue: 'fixture-model', options: [{ value: 'fixture-model' }, { value: 'fixture-alt' }] }] } });
  else if (request.method === 'models/list') send({ jsonrpc: '2.0', id: request.id, result: { models: [{ id: 'fixture-model', name: 'Fixture' }] } });
  else if (request.method === 'session/set_config_option') {
    if (request.params?.configId === 'model' && typeof request.params?.value === 'string' && request.params.value.length > 0 && !request.params.value.includes('\u0000')) {
      send({ jsonrpc: '2.0', id: request.id, result: { configOptions: [] } });
    } else {
      send({ jsonrpc: '2.0', id: request.id, error: { code: -32602, message: `unknown model option: ${request.params?.value}` } });
    }
  } else if (request.method === 'session/prompt') {
    send({ jsonrpc: '2.0', method: 'session/update', params: { sessionId: 'fixture-session', update: { type: 'tool_call', toolCallId: 'tool-1', title: 'read-only probe' } } });
    if (request.params.prompt.includes('permission')) send({ jsonrpc: '2.0', id: 'permission-1', method: 'session/request_permission', params: { requestId: 'permission-1', sessionId: 'fixture-session', toolCallId: 'tool-1', options: [{ optionId: 'allow-once', kind: 'allow_once' }, { optionId: 'reject-once', kind: 'reject_once' }] } });
    if (request.params.prompt.includes('max_tokens')) {
      send({ jsonrpc: '2.0', id: request.id, result: { stopReason: 'max_tokens' } });
      return;
    }
    if (request.params.prompt.includes('standard chunk')) {
      update({ sessionUpdate: 'agent_message_chunk', messageId: 'fixture-message', content: { type: 'text', text: 'standard ' } });
      update({ sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'answer' } });
      send({ jsonrpc: '2.0', id: request.id, result: { stopReason: 'end_turn' } });
      return;
    }
    if (request.params.prompt.includes('failed tool')) {
      update({ sessionUpdate: 'tool_call', toolCallId: 'tool-2', title: 'Probe the workspace', kind: 'other', status: 'in_progress', rawInput: { path: '.' } });
      update({ sessionUpdate: 'tool_call_update', toolCallId: 'tool-2', status: 'failed', content: [{ type: 'content', content: { type: 'text', text: 'permission denied' } }] });
      update({ sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'tool failure noted' } });
      send({ jsonrpc: '2.0', id: request.id, result: { stopReason: 'end_turn' } });
      return;
    }
    send({ jsonrpc: '2.0', method: 'session/update', params: { sessionId: 'fixture-session', update: { type: 'agent_message', messageId: 'message-1', content: [{ type: 'text', text: 'hi' }] } } });
    send({ jsonrpc: '2.0', id: request.id, result: { stopReason: 'end_turn', messageId: 'message-1' } });
  } else if (request.method === 'session/cancel') send({ jsonrpc: '2.0', id: request.id, result: { cancelled: true } });
  else if (request.method === 'session/close') send({ jsonrpc: '2.0', id: request.id, result: {} });
  else send({ jsonrpc: '2.0', id: request.id, error: { code: -32601, message: 'method not found' } });
});
