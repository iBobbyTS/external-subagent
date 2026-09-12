import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { agentsCommand } from '../../cli/commands/agents.mjs';
import { productPaths } from '../../cli/paths.mjs';
import { execFileSync } from 'node:child_process';

const providers = ['zcode', 'dsh'];

for (const provider of providers) {
  test(`${provider} is reported by the real shared agent registry`, async () => {
    const paths = productPaths(fs.mkdtempSync(path.join(os.tmpdir(), 'provider-contract-')));
    const listed = await agentsCommand(paths);
    const entry = listed.agents.find((agent) => agent.agent === provider);
    assert.ok(entry);
    assert.deepEqual(Object.keys(entry).sort(), ['agent', 'default_model', 'enabled', 'spawn_supported']);
  });
}

test('provider adapters do not share a workspace concurrently', () => {
  const output = execFileSync('cargo', ['test', '-p', 'external-daemon', 'cross_provider_shared_scheduler_contract', '--', '--nocapture'], { cwd: new URL('../..', import.meta.url), encoding: 'utf8' });
  assert.match(output, /test result: ok/);
  assert.match(output, /1 passed/);
});
