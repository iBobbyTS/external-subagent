import test from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import fs from 'node:fs/promises';
import { spawn } from 'node:child_process';

const root = path.resolve(import.meta.dirname, '../..');
const cli = path.join(root, 'bin/external-subagent.mjs');

test('public schema matches the external MCP tool namespace', async () => {
  const schema = JSON.parse(await fs.readFile(path.join(root, 'schema/zcode-subagent-public-api.json'), 'utf8'));
  const names = schema.properties.tools.const;
  assert.equal(names.every((name) => name.startsWith('external_subagent_')), true);
  assert.equal(JSON.stringify(schema).includes('zcode_subagent_'), false);
});
function task(status = 'running') { return { agent_id: 10000001, status, session_id: null, input_identity: null }; }
async function fixtureServer(socketPath) {
  let status = 'running';
  let promptCount = 0;
  const server = net.createServer((socket) => { let buffer = ''; socket.setEncoding('utf8'); socket.on('data', (chunk) => { buffer += chunk; const newline = buffer.indexOf('\n'); if (newline < 0) return; const request = JSON.parse(buffer.slice(0, newline)); let result; if (request.method === 'submit_general') { promptCount += 1; assert.equal(request.params.manifest.agent, undefined); assert.equal(Object.hasOwn(request.params, 'input'), false); result = { task: task(), disposition: 'admitted' }; } else if (request.method === 'task_wait') { status = 'completed'; result = { task: task(status), pending_requests: [], result_available: true, activity: { latest_text_tail: '', latest_text_truncated: false, latest_reasoning: '', tool_calls_last_60s: 0, telemetry_status: 'healthy' }, result: { outcome: 'completed', final_text: 'fixture result', partial: false, offset: 0, total_bytes: 14, next_offset: null, complete: true } }; } else if (request.method === 'task_result') result = { task: task(status), result: { outcome: 'completed', final_text: 'fixture result', partial: false, offset: 0, total_bytes: 14, next_offset: null, complete: true } }; else if (request.method === 'task_close') { assert.equal(status, 'completed'); status = 'closed'; result = { task: task(status) }; } else result = { status: { ready: true } }; socket.end(`${JSON.stringify({ request_id: request.request_id, outcome: 'success', result })}\n`); }); });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(socketPath, resolve); }); server.promptCount = () => promptCount; return server;
}
function runCli(socket, command, input) { return new Promise((resolve, reject) => { const child = spawn(process.execPath, [cli, command, '--json', JSON.stringify(input)], { env: { ...process.env, ZCODE_AGENTD_SOCKET: socket }, cwd: root }); let stdout = ''; let stderr = ''; child.stdout.on('data', (chunk) => { stdout += chunk; }); child.stderr.on('data', (chunk) => { stderr += chunk; }); child.on('error', reject); child.on('close', (code) => code === 0 ? resolve(JSON.parse(stdout)) : reject(new Error(`${stderr}\n${stdout}`))); }); }
test('CLI walking path uses RPC spawn wait result close', async () => { const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'external-subagent-walking-')); const socket = path.join(directory, 'daemon.sock'); const server = await fixtureServer(socket); try { const created = await runCli(socket, 'spawn', { subagent: 'zcode', repository: directory, prompt: 'hello' }); assert.deepEqual(created.result, { agent_id: 10000001, submission_disposition: 'admitted', status: 'running' }); const waited = await runCli(socket, 'wait', { agent_id: 10000001, wait_time: 0 }); assert.equal(waited.result.result.final_text, 'fixture result'); const result = await runCli(socket, 'result', { agent_id: 10000001 }); assert.equal(result.result.result.complete, true); const closed = await runCli(socket, 'close', { agent_id: 10000001 }); assert.equal(closed.result.task.status, 'closed'); assert.equal(server.promptCount(), 1); } finally { await new Promise((resolve) => server.close(resolve)); await fs.rm(directory, { recursive: true, force: true }); } });

test('CLI rejects malformed spawn fields before transport', async () => { const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'external-subagent-agent-contract-')); try { await assert.rejects(() => runCli('/missing/socket', 'spawn', { subagent: null, repository: directory, prompt: 'hello' }), /subagent must be omitted/); await assert.rejects(() => runCli('/missing/socket', 'spawn', { subagent: 'zcode', model: null, repository: directory, prompt: 'hello' }), /model must be omitted/); await assert.rejects(() => runCli('/missing/socket', 'spawn', { subagent: 'zcode', repository: directory, prompt: 'hello', extra: true }), /unsupported field/); } finally { await fs.rm(directory, { recursive: true, force: true }); } });
test('CLI rejects malformed task identifiers before contacting RPC', async () => { await assert.rejects(() => runCli('/missing/socket', 'close', { agent_id: 1 }), /agent_id must be an integer/); });
