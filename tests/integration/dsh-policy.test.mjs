import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';

// DSH remains discovery-only until the S04 gate is explicitly opened.
test('dsh policy is fail-closed for unsupported spawn modes', async () => {
  const { agentsCommand } = await import('../../cli/commands/agents.mjs');
  const config = fs.mkdtempSync(path.join(os.tmpdir(), 'dsh-policy-'));
  fs.writeFileSync(path.join(config, 'config.json'), JSON.stringify({ schema_version: 1, revision: 1, default_agent: 'zcode', agents: { zcode: { enabled: true, spawn_supported: true }, dsh: { enabled: true, spawn_supported: false } } }));
  const result = await agentsCommand({ config: path.join(config, 'config.json') }, { operation: 'list' });
  const dsh = result.agents.find((agent) => agent.agent === 'dsh');
  assert.equal(dsh.spawn_supported, false);
});

test('strictplan rejects non-empty manifests before dispatch', () => {
  const output = execFileSync('cargo', ['test', '-p', 'external-core', 'strict_plan_admission_rejects_non_empty_write_manifest', '--', '--nocapture'], { cwd: path.resolve(new URL('../..', import.meta.url).pathname), encoding: 'utf8' });
  assert.match(output, /test result: ok/);
  assert.match(output, /1 passed/);
});
