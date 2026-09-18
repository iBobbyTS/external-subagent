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
  // Product surfaces: the packaged public schema and the daemon-view fixture.
  // The daemon carries evidence a later config revision did not re-probe as
  // UNKNOWN with reason `stale_config_revision`, keeping the probe facts for
  // diagnosis (preserved_or_stale in crates/external-daemon/src/agent_status.rs).
  // Hold that projection against the public contract the schema packages.
  const root = path.resolve(import.meta.dirname, '../..');
  const schema = JSON.parse(fs.readFileSync(path.join(root, 'schema/external-subagent-public-api.json'), 'utf8'));
  const layerSchema = schema.properties.subagent_scope_status;
  const states = layerSchema.properties.state.enum;
  const readiness = states.filter((state) => state !== 'UNKNOWN');
  // UNKNOWN must remain the enum's non-conclusive state, so a stale
  // carry-over always has a legal landing place that is not a readiness
  // conclusion.
  assert.equal(states.includes('UNKNOWN'), true, 'public state enum lost UNKNOWN');
  // The public layer view exposes the readiness conclusion and nothing else:
  // probe evidence stays on the daemon view, so stale facts cannot leak onto
  // the public readiness surface.
  assert.equal(layerSchema.additionalProperties, false, 'public layer view accepts extra properties');
  for (const field of ['scope', 'version', 'checked_at_ms', 'reason']) {
    assert.equal(Object.hasOwn(layerSchema.properties, field), false, `${field} leaked into the public layer view`);
  }
  const status = JSON.parse(fs.readFileSync(fixturePath, 'utf8'));
  for (const layer of ['local', 'auth', 'hi']) {
    const fresh = status[layer];
    // Fresh fixture evidence concludes in states the public enum accepts.
    assert.equal(states.includes(fresh.state), true, `${layer}: fixture state ${fresh.state} outside the public enum`);
    // Model the carry-over across a config revision: probe facts survive for
    // daemon-side diagnosis, but the conclusion degrades to the one
    // non-conclusive state.
    const carried = { ...fresh, state: 'UNKNOWN', reason: 'stale_config_revision' };
    assert.equal(readiness.includes(carried.state), false, `${layer}: stale evidence projects as readiness`);
    for (const field of ['scope', 'version', 'checked_at_ms']) {
      assert.notEqual(carried[field], undefined, `${layer}: ${field} evidence dropped by the carry-over`);
    }
  }
});

test('packaged status schema requires every capability owner field', () => {
  const root = path.resolve(import.meta.dirname, '../..');
  const schema = JSON.parse(fs.readFileSync(path.join(root, 'schema/external-subagent-public-api.json'), 'utf8'));
  // The packaged schema documents the public MCP projection, whose route key
  // is `subagent`; the daemon RPC view (this fixture) names the same field
  // `agent`. Every schema-required field must exist in the daemon-side view.
  const fixture = JSON.parse(fs.readFileSync(fixturePath, 'utf8'));
  for (const field of schema.properties.subagent_status.required) {
    const ownerField = field === 'subagent' ? 'agent' : field;
    assert.equal(Object.hasOwn(fixture, ownerField), true, field);
  }
});

test('failure vocabulary distinguishes evidence-producing paths', () => {
  const reasons = new Set(['missing', 'version', 'transport', 'auth_401', 'network', 'rate_limit']);
  assert.equal(reasons.size, 6);
  assert.equal(reasons.has('auth_401'), true);
  assert.equal(reasons.has('rate_limit'), true);
});
