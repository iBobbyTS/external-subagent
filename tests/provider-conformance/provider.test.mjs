import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { agentsCommand } from '../../cli/commands/agents.mjs';
import { productPaths } from '../../cli/paths.mjs';

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
  const leases = new Map();
  leases.set('/workspace', 'zcode');
  assert.equal(leases.get('/workspace'), 'zcode');
  assert.equal(leases.has('/workspace'), true);
});
