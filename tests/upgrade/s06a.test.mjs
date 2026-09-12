import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { reconcileInstallation, updateInstallation } from '../../cli/install/update.mjs';

function paths(root) {
  return { data: path.join(root, 'data'), home: path.join(root, 'home'), state: path.join(root, 'data', 'install-state.json') };
}

test('upgrade writes versioned candidate and active pointer atomically', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-'));
  const p = paths(root);
  fs.mkdirSync(p.data, { recursive: true });
  const result = updateInstallation(p, { version: '2.0.0', dryRun: false });
  assert.equal(result.phase, 'active');
  const state = JSON.parse(fs.readFileSync(p.state, 'utf8'));
  assert.equal(state.schema_version, 2);
  assert.equal(state.active.version, '2.0.0');
  assert.equal(state.candidate, null);
});

test('reconcile rejects active cancellation unless --yes is explicit', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-upgrade-'));
  const p = paths(root);
  fs.mkdirSync(p.data, { recursive: true });
  assert.throws(() => reconcileInstallation(p, { cancelActive: true }), /--yes/);
});
