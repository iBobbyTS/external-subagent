// Table-driven tests for scripts/test/affected.mjs (block map, supersede
// normalization, --fast, unknown-path policy) plus a real-git collection
// test covering the rename/untracked cases the plan review called out.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import {
  BLOCKS, IMPACT_RULES, ruleForPath, selectBlocks, normalizeSelection, applyFast,
} from '../../scripts/test/affected.mjs';

const ids = (paths) => [...selectBlocks(paths).keys()].sort();

test('blocks table is internally consistent', () => {
  for (const [id, def] of Object.entries(BLOCKS)) {
    assert.ok(def.desc, `${id} needs a description`);
    assert.equal(typeof def.cost, 'number', `${id} cost`);
    for (const sub of def.covers ?? []) {
      assert.ok(BLOCKS[sub], `${id} covers unknown block ${sub}`);
      assert.notEqual(sub, id);
    }
  }
  // every block referenced by any rule must exist
  for (const rule of IMPACT_RULES) {
    for (const b of rule.blocks ?? []) assert.ok(BLOCKS[b], `rule ${rule.name} references unknown block ${b}`);
  }
});

test('source-area mapping selects the expected blocks', () => {
  assert.deepEqual(ids(['crates/external-store/src/tasks.rs']), [
    'node:install', 'node:integration', 'node:upgrade', 'rust:daemon-full', 'rust:fixture-zcode', 'rust:store',
  ]);
  assert.ok(ids(['crates/external-daemon/src/scheduler/tests.rs']).includes('rust:daemon-scheduler'));
  assert.ok(ids(['crates/external-daemon/src/scheduler.rs']).includes('rust:daemon-scheduler'));
  assert.ok(ids(['crates/external-daemon/src/agent_status.rs']).includes('rust:daemon-adapters'));
  assert.ok(ids(['crates/external-daemon/src/projection.rs']).includes('rust:daemon-full'));
  // unmapped daemon root module stays conservative, never zero blocks
  assert.ok(ids(['crates/external-daemon/src/weird_new_module.rs']).includes('rust:daemon-full'));
  assert.ok(ids(['cli/rpc.mjs']).includes('node:upgrade'));
  assert.ok(ids(['bin/external-subagent.mjs']).includes('node:contract'));
  assert.deepEqual(ids(['schema/external-subagent-public-api.json']), ['node:cli', 'node:contract']);
  assert.ok(ids(['docs/acceptance/productization.md']).includes('node:acceptance'));
  assert.ok(ids(['tests/fixtures/subagent-config-matrix.json']).includes('rust:daemon-rpc'));
  assert.deepEqual(ids(['tests/cli/rpc.test.mjs']), ['node:cli']);
  assert.deepEqual(ids(['tests/provider-conformance/provider.test.mjs']), ['node:provider']);
  assert.deepEqual(ids(['tests/upgrade/reconcile.test.mjs']), ['node:upgrade']);
  assert.ok(ids(['tests/live-agent/non-git-based/observation-public-fixture.json']).includes('rust:daemon-full'));
});

test('no-test whitelist and unknown-path policy', () => {
  assert.deepEqual(ids(['README.md', 'docs/mcp-api.md', '.agents/skills/x/SKILL.md']), []);
  assert.equal(ruleForPath('docs/mcp-api.md').blocks.length, 0);
  // anything unmapped selects the conservative full set, never silently nothing
  const unknown = ids(['some/new/dir/file.txt']);
  assert.ok(unknown.includes('rust:workspace-full'));
  assert.ok(unknown.includes('node:upgrade'));
});

test('full blocks supersede their sub-blocks with merged reasons', () => {
  const sel = selectBlocks([
    'crates/external-store/src/tasks.rs',   // daemon-full + store + …
    'crates/external-daemon/src/rpc/views.rs', // daemon-rpc + …
  ]);
  const { selection, covered } = normalizeSelection(sel);
  assert.ok(selection.has('rust:daemon-full'));
  assert.ok(!selection.has('rust:daemon-rpc'));
  assert.ok(covered.some((c) => c.id === 'rust:daemon-rpc' && c.by === 'rust:daemon-full'));
  const fullReasons = [...selection.get('rust:daemon-full').reasons].join('\n');
  assert.match(fullReasons, /covered from rust:daemon-rpc/);

  const ws = selectBlocks(['Cargo.toml']);
  const wsNorm = normalizeSelection(ws);
  assert.ok(wsNorm.selection.has('rust:workspace-full'));
  assert.ok(!wsNorm.selection.has('rust:store'));
});

test('--fast moves the slow lane to NOT RUN', () => {
  const sel = selectBlocks(['cli/main.mjs']);
  const { selection } = normalizeSelection(sel);
  assert.ok(selection.has('node:install') && selection.has('node:upgrade'));
  const { selection: fast, skipped } = applyFast(selection);
  assert.ok(!fast.has('node:install') && !fast.has('node:upgrade'));
  assert.deepEqual(skipped.map((s) => s.id).sort(), ['node:install', 'node:upgrade']);
});

test('git collection: rename keeps both ends, untracked dirs are expanded', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'affected-git-'));
  const git = (args) => {
    const res = spawnSync('git', args, { cwd: dir, encoding: 'utf8' });
    assert.equal(res.status, 0, `git ${args.join(' ')}: ${res.stderr}`);
    return res.stdout;
  };
  git(['init', '-q']);
  git(['config', 'user.email', 't@t']); git(['config', 'user.name', 't']);
  fs.mkdirSync(path.join(dir, 'cli'), { recursive: true });
  fs.writeFileSync(path.join(dir, 'cli/x.mjs'), '1\n');
  git(['add', '.']); git(['commit', '-qm', 'a']);
  fs.renameSync(path.join(dir, 'cli/x.mjs'), path.join(dir, 'docs-x.mjs'));
  fs.mkdirSync(path.join(dir, 'tests/cli/newdir'), { recursive: true });
  fs.writeFileSync(path.join(dir, 'tests/cli/newdir/n.test.mjs'), '2\n');
  // mirror the collector's git invocations against this scratch repo
  const out = spawnSync('git', ['diff', '--name-only', '-z', '--no-renames', 'HEAD'], { cwd: dir, encoding: 'utf8' }).stdout;
  const untracked = spawnSync('git', ['ls-files', '--others', '--exclude-standard', '-z'], { cwd: dir, encoding: 'utf8' }).stdout;
  const files = [...out.split('\0'), ...untracked.split('\0')].filter(Boolean);
  assert.ok(files.includes('cli/x.mjs'), 'rename source end must be collected');
  assert.ok(files.includes('docs-x.mjs'), 'rename target end must be collected');
  assert.ok(files.includes('tests/cli/newdir/n.test.mjs'), 'untracked file under a new dir must be expanded');
  // and that union maps to real blocks
  const sel = ids(files);
  assert.ok(sel.includes('node:cli'));
});
