import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';

const fixturePath = path.resolve(import.meta.dirname, '../fixtures/agent-status/scoped-probe.json');

test('agent status fixture keeps layer evidence scoped and failure reasons explicit', () => {
  const status = JSON.parse(fs.readFileSync(fixturePath, 'utf8'));
  assert.equal(status.agent, 'zcode');
  assert.equal(status.config_revision, 7);
  assert.equal(status.configured, true);
  assert.deepEqual(status.transport_support, {
    transport: 'zcode_app_server', probe: true, spawn: true,
  });
  assert.deepEqual(status.permission_modes, ['build', 'edit', 'plan', 'yolo']);
  assert.deepEqual(status.model_selection, { supported: false, mode: 'native_only' });
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

test('stale probe evidence is never projected as current readiness', () => {
  const stale = {
    state: 'UNKNOWN',
    scope: { workspace: '/fixtures/workspace-a' },
    version: '3.8.1',
    checked_at_ms: 1234,
    reason: 'stale_config_revision',
  };
  assert.equal(stale.state, 'UNKNOWN');
  assert.equal(stale.reason, 'stale_config_revision');
});

test('packaged status schema requires every capability owner field', () => {
  const root = path.resolve(import.meta.dirname, '../..');
  const schema = JSON.parse(fs.readFileSync(path.join(root, 'schema/zcode-subagent-public-api.json'), 'utf8'));
  for (const field of schema.properties.agent_status.required) {
    assert.equal(Object.hasOwn(JSON.parse(fs.readFileSync(fixturePath, 'utf8')), field), true, field);
  }
});

test('failure vocabulary distinguishes evidence-producing paths', () => {
  const reasons = new Set(['missing', 'version', 'transport', 'auth_401', 'network', 'rate_limit']);
  assert.equal(reasons.size, 6);
  assert.equal(reasons.has('auth_401'), true);
  assert.equal(reasons.has('rate_limit'), true);
});
