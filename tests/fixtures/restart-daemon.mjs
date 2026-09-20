import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { productPaths } from '../../cli/paths.mjs';
import { callDaemon } from '../../cli/rpc.mjs';

export async function daemonHarness({ root, home, runtime, env = {}, spawnProcess = spawn, timeoutMs = 10000 }) {
  const socket = path.join(root, 'd.sock');
  let child;
  let exited;
  let stderr = '';
  async function waitForExit(signal) {
    let timer;
    try {
      await Promise.race([exited, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`daemon ${signal} timeout: ${stderr}`)), timeoutMs); })]);
    } finally { clearTimeout(timer); }
  }
  async function stop() {
    if (!child || child.exitCode !== null || child.signalCode !== null) return;
    child.kill('SIGTERM');
    try {
      await waitForExit('SIGTERM');
    } catch (error) {
      if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
      await waitForExit('SIGKILL');
      throw error;
    }
  }
  async function start() {
    const clean = { ...process.env, ...env, HOME: home };
    for (const key of ['EXTERNAL_SUBAGENT_CONFIG', 'EXTERNAL_SUBAGENT_CONFIG_REVISION', 'DSH_RUNTIME_PATH', 'CODEX_RUNTIME_PATH', 'ZCODE_AGENT_CONFIG']) delete clean[key];
    stderr = '';
    child = spawnProcess(path.resolve('target/debug/external-subagentd'), [
      '--agent-config', productPaths(home).config, '--database', path.join(root, 'd.sqlite'), '--socket', socket, '--runtime', runtime,
    ], { env: clean, stdio: ['ignore', 'ignore', 'pipe'] });
    let spawnError;
    exited = new Promise((resolve) => {
      child.once('exit', resolve);
      child.once('error', (error) => { spawnError = error; resolve(); });
    });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    try {
      const deadline = Date.now() + timeoutMs;
      while (Date.now() < deadline) {
        if (spawnError) throw spawnError;
        assert.equal(child.exitCode, null, stderr);
        assert.equal(child.signalCode, null, stderr);
        if (fs.existsSync(socket)) {
          try { await callDaemon(socket, 'status', {}); return; } catch {}
        }
        await new Promise((resolve) => setTimeout(resolve, 40));
      }
      throw new Error('daemon startup timeout: ' + stderr);
    } catch (error) {
      try { await stop(); } catch (cleanupError) {
        throw new AggregateError([error, cleanupError], `${error.message}; cleanup: ${cleanupError.message}`);
      }
      throw error;
    }
  }
  await start();
  return { socket, stop, restart: async () => { await stop(); await start(); } };
}
