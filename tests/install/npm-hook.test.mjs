import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { updateCommand } from '../../cli/commands/update.mjs';

const repoRoot = path.resolve(import.meta.dirname, '../..');
const hook = path.join(repoRoot, 'cli/install/npm-hook.mjs');

test('non-global postinstall never coordinates an initialized version drift', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-hook-'));
  try {
    const data = path.join(home, 'Library', 'Application Support', 'external-subagent');
    fs.mkdirSync(data, { recursive: true });
    const state = { schema_version: 1, phase: 'active', active: { version: '9.9.9' } };
    fs.writeFileSync(path.join(data, 'install-state.json'), JSON.stringify(state));
    const result = spawnSync(process.execPath, [hook], {
      cwd: repoRoot,
      env: { ...process.env, HOME: home, npm_config_global: 'false' },
      encoding: 'utf8',
      timeout: 5000,
    });
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /"stage_only":false/u);
    assert.deepEqual(JSON.parse(fs.readFileSync(path.join(data, 'install-state.json'), 'utf8')), state);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('update coordination aborts a drain when its bounded deadline expires', async () => {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-drain-timeout-'));
  const paths = { data, state: path.join(data, 'state.json'), socket: path.join(data, 'daemon.sock') };
  fs.writeFileSync(paths.state, JSON.stringify({ active: { version: '0.0.1' } }));
  const calls = [];
  try {
    await assert.rejects(
      updateCommand(paths, ['reconcile'], {
        drainTimeoutMs: 5,
        callDaemon: async (_socket, method) => {
          calls.push(method);
          if (method === 'drain' || method === 'drain-status') return { ready_for_activation: false };
          if (method === 'drain-abort') return { aborted: true };
          throw new Error(`unexpected ${method}`);
        },
        preflightUpdate: () => {},
        updateInstallation: () => ({ phase: 'active', active: { version: '0.1.0' } }),
        hasInstalledService: () => false,
      }),
      (error) => error.code === 'UPDATE_DRAIN_TIMEOUT',
    );
    assert.deepEqual(calls, ['drain', 'drain-status', 'drain-abort']);
  } finally {
    fs.rmSync(data, { recursive: true, force: true });
  }
});
