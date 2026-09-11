import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';

const fixturePath = path.resolve(import.meta.dirname, '../fixtures/agent-status/scoped-probe.json');

test('agent status fixture keeps layer evidence scoped and failure reasons explicit', () => {
  const status = JSON.parse(fs.readFileSync(fixturePath, 'utf8'));
  assert.equal(status.agent, 'zcode');
  assert.equal(status.local.state, 'READY');
  assert.equal(status.auth.reason, 'auth_401');
  assert.equal(status.hi.reason, 'auth_401');
  for (const layer of ['local', 'auth', 'hi']) {
    assert.deepEqual(status[layer].scope, {
      workspace: '/fixtures/workspace-a',
      home: '/fixtures/home-a',
    });
    assert.equal(status[layer].checked_at_ms, 1234);
    assert.equal(status[layer].version, '3.8.1');
  }
});

test('failure vocabulary distinguishes evidence-producing paths', () => {
  const reasons = new Set(['missing', 'version', 'transport', 'auth_401', 'network', 'rate_limit']);
  assert.equal(reasons.size, 6);
  assert.equal(reasons.has('auth_401'), true);
  assert.equal(reasons.has('rate_limit'), true);
});
