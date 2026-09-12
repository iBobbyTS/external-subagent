import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';

test('controlled service replacement proves PID, argv, version and RPC health', async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'identity-'));
  const run = (version) => new Promise((resolve) => {
    const file = path.join(dir, `${version}.json`);
    const child = spawn(process.execPath, ['tests/fixtures/versioned-daemon.mjs', version, file], { stdio: 'ignore' });
    const timer = setInterval(() => { if (fs.existsSync(file)) { clearInterval(timer); resolve({ child, file }); } }, 5);
  });
  const first = await run('1.0.0');
  const a = JSON.parse(fs.readFileSync(first.file));
  assert.equal(a.pid, first.child.pid); assert.equal(a.version, '1.0.0'); assert.equal(a.rpc, 'healthy');
  first.child.kill('SIGTERM'); await new Promise(r => first.child.once('exit', r));
  const second = await run('2.0.0'); const b = JSON.parse(fs.readFileSync(second.file));
  assert.equal(b.pid, second.child.pid); assert.notEqual(b.pid, a.pid); assert.deepEqual(b.argv.slice(0,1), ['2.0.0']); assert.equal(b.rpc, 'healthy');
  second.child.kill('SIGTERM'); await new Promise(r => second.child.once('exit', r)); fs.rmSync(dir, { recursive: true, force: true });
});
