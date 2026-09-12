import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import net from 'node:net';

const rpcHealth = (socket) => new Promise((resolve, reject) => {
  const client = net.createConnection(socket);
  let data = '';
  client.on('data', (chunk) => { data += chunk; });
  client.on('end', () => resolve(JSON.parse(data)));
  client.on('error', reject);
  client.end('{}');
});

const stop = async (child) => {
  if (child.exitCode !== null) return;
  child.kill('SIGTERM');
  await new Promise((resolve) => child.once('exit', resolve));
};

test('controlled process replacement proves PID, argv, version and RPC health', async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'identity-'));
  const children = [];
  const run = (version) => new Promise((resolve, reject) => {
    const file = path.join(dir, `${version}.json`);
    const socket = path.join(dir, `${version}.sock`);
    const child = spawn(process.execPath, ['tests/fixtures/versioned-daemon.mjs', version, file, socket], { stdio: 'ignore' });
    children.push(child);
    const deadline = Date.now() + 2_000;
    const timer = setInterval(() => {
      if (fs.existsSync(file)) {
        clearInterval(timer);
        resolve({ child, file, socket });
      } else if (child.exitCode !== null || Date.now() >= deadline) {
        clearInterval(timer);
        reject(new Error(`versioned daemon ${version} failed to become ready`));
      }
    }, 5);
    child.once('error', (error) => { clearInterval(timer); reject(error); });
  });
  try {
    const first = await run('1.0.0');
    const a = JSON.parse(fs.readFileSync(first.file));
    assert.equal(a.pid, first.child.pid);
    assert.equal(a.version, '1.0.0');
    assert.deepEqual(await rpcHealth(first.socket), { ok: true, version: '1.0.0' });
    await stop(first.child);

    const second = await run('2.0.0');
    const b = JSON.parse(fs.readFileSync(second.file));
    assert.equal(b.pid, second.child.pid);
    assert.notEqual(b.pid, a.pid);
    assert.equal(b.version, '2.0.0');
    assert.deepEqual(b.argv.slice(0, 1), ['2.0.0']);
    assert.deepEqual(await rpcHealth(second.socket), { ok: true, version: '2.0.0' });
  } finally {
    await Promise.all(children.map(stop));
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
