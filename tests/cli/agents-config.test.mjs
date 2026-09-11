import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { agentsCommand } from '../../cli/commands/agents.mjs';
import { configCommand } from '../../cli/commands/config.mjs';
import { readConfig } from '../../cli/config/read.mjs';
import { validateSpawnSelection } from '../../cli/config/schema.mjs';
import { productPaths } from '../../cli/paths.mjs';

function fixture() { const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-agents-')); return { home, paths: productPaths(home) }; }

test('config has no default and lists layered agent support', () => {
  const { paths } = fixture();
  const listed = agentsCommand(paths);
  assert.equal(listed.default_agent, null);
  assert.deepEqual(listed.agents.map((agent) => [agent.agent, agent.spawn_supported]), [['zcode', true], ['dsh', false]]);
  assert.throws(() => validateSpawnSelection(readConfig(paths.config), {}), (error) => error.code === 'agent_required');
});

test('zcode model is rejected before prompt and dsh remains discovery-only', () => {
  const { paths } = fixture();
  assert.throws(() => configCommand(paths, { operation: 'set', patch: { default_agent: 'zcode', agents: { zcode: { default_model: 'glm-4' } } } }), (error) => error.code === 'model_selection_unsupported');
  const config = configCommand(paths, { operation: 'set', patch: { agents: { dsh: { enabled: true } } } }).config;
  assert.throws(() => validateSpawnSelection(config, { agent: 'dsh' }), (error) => error.code === 'agent_unsupported');
});

test('config writes a revision and keeps existing task snapshots independent', () => {
  const { paths } = fixture();
  const first = configCommand(paths, { operation: 'set', patch: { default_agent: 'zcode' } }).config;
  const second = configCommand(paths, { operation: 'set', patch: { agents: { dsh: { enabled: true } } } }).config;
  assert.equal(first.revision, 1);
  assert.equal(second.revision, 2);
  assert.equal(first.default_agent, 'zcode');
  assert.equal(second.agents.dsh.spawn_supported, false);
});
