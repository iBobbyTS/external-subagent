import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { subagentsCommand } from '../../cli/commands/agents.mjs';
import { productPaths } from '../../cli/paths.mjs';
import { execFileSync } from 'node:child_process';

const providers = ['zcode', 'dsh'];

for (const provider of providers) {
  test(`${provider} is reported by the real shared agent registry`, async () => {
    const paths = productPaths(fs.mkdtempSync(path.join(os.tmpdir(), 'provider-contract-')));
    const listed = await subagentsCommand(paths);
    const entry = listed.subagents.find((agent) => agent.subagent === provider);
    assert.ok(entry);
    const fields = ['subagent', 'default_model', 'enabled', 'spawn_supported'];
    if (provider === 'dsh') fields.push('runtime_path', 'home', 'profile', 'version');
    assert.deepEqual(Object.keys(entry).sort(), fields.sort());
  });
}

test('provider adapters do not share a workspace concurrently', () => {
  const output = execFileSync('cargo', ['test', '-p', 'external-daemon', 'dsh::tests::cross_provider_shared_scheduler_contract', '--', '--exact', '--nocapture'], { cwd: new URL('../..', import.meta.url), encoding: 'utf8' });
  assert.match(output, /test result: ok\. 1 passed/);
});
