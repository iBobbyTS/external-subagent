import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';

const fixture = fileURLToPath(new URL('../fixtures/dsh-acp/fake-server.mjs', import.meta.url));

test('dsh malformed and unsupported requests fail closed', async () => {
  const child = spawn(process.execPath, [fixture], { stdio: ['pipe', 'pipe', 'ignore'] });
  const lines = readline.createInterface({ input: child.stdout });
  const frames = [];
  lines.on('line', (line) => frames.push(JSON.parse(line)));
  child.stdin.write('not-json\n');
  child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: 7, method: 'unknown' }) + '\n');
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.equal(frames[0].error.code, -32601);
  child.kill('SIGTERM');
});

test('cancel is an explicit bounded protocol operation', async () => {
  const child = spawn(process.execPath, [fixture], { stdio: ['pipe', 'pipe', 'ignore'] });
  const lines = readline.createInterface({ input: child.stdout });
  const frames = [];
  lines.on('line', (line) => frames.push(JSON.parse(line)));
  child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'session/cancel', params: { sessionId: 'fixture-session' } }) + '\n');
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.deepEqual(frames[0].result, { cancelled: true });
  child.kill('SIGTERM');
});

test('runtime observes bounded oversized and reap failure paths', () => {
  const cwd = fileURLToPath(new URL('../..', import.meta.url));
  const output = execFileSync('cargo', ['test', '-p', 'external-runtime', 'malformed_and_child_exit_are_visible', '--', '--exact'], { cwd, encoding: 'utf8' });
  assert.match(output, /test result: ok/);
});

test('daemon DSH lifecycle oracle covers cancellation, max_tokens and reap', () => {
  const cwd = fileURLToPath(new URL('../..', import.meta.url));
  const output = execFileSync('cargo', ['test', '-p', 'external-daemon', 'build_task_flows_model_permission_and_result_through_the_shared_lifecycle', '--', '--nocapture'], { cwd, encoding: 'utf8' });
  assert.match(output, /test result: ok/);
});

test('daemon DSH max_tokens oracle preserves failed settlement', () => {
  const cwd = fileURLToPath(new URL('../..', import.meta.url));
  const output = execFileSync('cargo', ['test', '-p', 'external-daemon', 'max_tokens_settlement_fails_the_task_without_faking_completion', '--', '--nocapture'], { cwd, encoding: 'utf8' });
  assert.match(output, /test result: ok/);
});

test('daemon DSH pending cancellation stays cancelling', () => {
  const cwd = fileURLToPath(new URL('../..', import.meta.url));
  const output = execFileSync('cargo', ['test', '-p', 'external-daemon', 'dsh_pending_task_cancel_is_terminal_and_non_resurrecting'], { cwd, encoding: 'utf8' });
  assert.match(output, /test result: ok\. 1 passed/);
});

test('daemon DSH active cancellation sends session/cancel and reaps without result', () => {
  const cwd = fileURLToPath(new URL('../..', import.meta.url));
  const output = execFileSync('cargo', ['test', '-p', 'external-daemon', 'dsh_active_task_cancel_sends_session_cancel_and_reaps_without_result', '--', '--exact', '--nocapture'], { cwd, encoding: 'utf8' });
  assert.match(output, /test result: ok\. 1 passed/);
});

test('runtime executes malformed, EOF and oversized frame or cancellation oracles', () => {
  const cwd = fileURLToPath(new URL('../..', import.meta.url));
  const output = execFileSync('cargo', ['test', '-p', 'external-runtime'], { cwd, encoding: 'utf8' });
  assert.match(output, /test result: ok/);
  assert.match(output, /test result: ok\. 29 passed/);
});
