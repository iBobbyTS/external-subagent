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
  const output = execFileSync('cargo', ['test', '-p', 'external-runtime', 'oversized_line_is_classified_and_discarded', '--', '--exact'], { cwd, encoding: 'utf8' });
  assert.match(output, /test result: ok/);
});
