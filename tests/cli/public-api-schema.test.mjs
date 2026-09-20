import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';

const root = path.resolve(import.meta.dirname, '../..');

test('packaged public schema is the reduced external_subagent catalog', () => {
  const schema = JSON.parse(fs.readFileSync(path.join(root, 'schema/external-subagent-public-api.json'), 'utf8'));
  assert.deepEqual(schema.properties.tools.const, [
    'external_subagent_cancel', 'external_subagent_close', 'external_subagent_list',
    'external_subagent_observe', 'external_subagent_wait', 'external_subagent_respond', 'external_subagent_result',
    'external_subagent_send', 'external_subagent_spawn', 'external_subagent_status',
  ]);
  const serialized = JSON.stringify(schema);
  for (const forbidden of ['git', 'worktree', 'artifact', 'budget', 'legacy', 'base_ref', 'HEAD']) {
    assert.equal(serialized.includes(forbidden), false, `public schema exposes ${forbidden}`);
  }
  assert.deepEqual(schema.properties.spawn.additionalProperties, false);
  assert.deepEqual(schema.properties.subagent_status.required, [
    'subagent', 'configured', 'enabled', 'spawn_supported',
    'permission_modes', 'model_selection', 'effort_selection', 'local', 'auth', 'hi',
  ]);
  assert.equal(schema.properties.subagent_status.properties.config_revision, undefined);
  assert.equal(schema.properties.subagent_status.properties.transport_support, undefined);
  assert.deepEqual(schema.properties.subagent_scope_status.required, ['state']);
  assert.deepEqual(schema.properties.subagent_scope_status.properties.state.enum, [
    'READY', 'DEGRADED', 'UNAVAILABLE', 'UNKNOWN',
  ]);
  assert.deepEqual(schema.properties.subagent_status.properties.model_selection.properties.mode.enum, [
    'native_only', 'catalog_token',
  ]);
  assert.deepEqual(schema.properties.subagent_status.properties.effort_selection.required, [
    'supported', 'mode',
  ]);
  assert.deepEqual(schema.properties.subagent_status.properties.effort_selection.properties.mode.enum, [
    'closed_set', 'passthrough_token',
  ]);
  assert.deepEqual(schema.properties.subagent_status.properties.effort_selection.additionalProperties, false);
  assert.deepEqual(schema.properties.spawn.properties.write_manifest.items.type, 'string');
  assert.equal(schema.properties.spawn.properties.prompt.type, 'string');
  assert.equal(schema.properties.spawn.properties.model.type, 'string');
  assert.equal(schema.properties.spawn.properties.subagent.type, 'string');
  assert.equal(schema.properties.spawn.properties.effort.type, 'string');
  assert.equal(schema.properties.spawn.properties.effort.pattern, '^[a-z0-9_]+$');
  assert.equal(schema.properties.spawn.properties.effort.maxLength, 24);
  assert.equal(schema.properties.list.properties.limit.default, 100);
  assert.equal(schema.properties.wait.properties.wait_time.default, 290);
  assert.equal(schema.properties.wait.properties.supports_answer, undefined);
  assert.equal(schema.properties.wait.properties.after_revision, undefined);
  assert.match(schema.properties.wait.description, /actionable pending request/u);
  assert.match(schema.properties.wait.description, /embedded question/u);
  assert.equal(schema.properties.result.properties.offset.default, 0);
  assert.equal(schema.properties.result.properties.limit.default, 262144);
  assert.deepEqual(schema.properties.observe.required, ['agent_id']);
  assert.equal(schema.properties.observe.additionalProperties, false);
  assert.deepEqual(schema.properties.contracts.properties.external_subagent_observe.input, ['agent_id']);
  assert.deepEqual(schema.properties.contracts.properties.external_subagent_observe.output, [
    'tools', 'reasoning', 'coverage',
  ]);
  assert.deepEqual(schema.properties.contracts.properties.external_subagent_status.output, [
    'mcp_version', 'components', 'capabilities', 'subagents',
  ]);
  assert.deepEqual(schema.properties.contracts.properties.external_subagent_wait.input, [
    'agent_id', 'wait_time', 'message_id',
  ]);
  assert.deepEqual(schema.properties.contracts.properties.external_subagent_wait.output, [
    'task', 'pending_requests', 'result_available', 'activity',
    'result', 'instruction', 'timed_out', 'message_receipt',
  ]);
  assert.deepEqual(schema.properties.error_projection.required, ['error']);
  assert.deepEqual(schema.properties.error_projection.properties.error.required, ['code', 'message']);
  assert.deepEqual(Object.keys(schema.properties.error_projection.properties.error.properties).sort(), [
    'agent_id', 'code', 'component', 'message', 'operation', 'prompt_count', 'request_id',
  ]);
  assert.deepEqual(schema.properties.contracts.properties.external_subagent_result.output, ['task', 'result']);
  assert.deepEqual(schema.properties.contracts.properties.external_subagent_result.input, [
    'agent_id', 'offset', 'limit',
  ]);
  assert.deepEqual(schema.properties.question_projection.required, ['text', 'truncated']);
  assert.equal(schema.properties.question_projection.additionalProperties, false);
  assert.equal(schema.properties.result.properties.request_id, undefined);
  assert.deepEqual(schema.properties.contracts.properties.external_subagent_respond.input, [
    'agent_id', 'request_id', 'decision', 'content',
  ]);
  assert.deepEqual(schema.properties.contracts.properties.external_subagent_spawn.output, ['agent_id', 'status']);
  assert.deepEqual(schema.properties.contracts.properties.external_subagent_spawn.input, [
    'subagent', 'repository', 'permission_mode', 'prompt', 'model', 'effort', 'write_manifest',
  ]);
  assert.deepEqual(schema.properties.contracts.properties.external_subagent_spawn.idempotent, false);
  assert.deepEqual(schema.properties.result_projection.required, [
    'outcome', 'final_text', 'partial', 'offset', 'total_bytes', 'next_offset', 'complete',
  ]);
  assert.deepEqual(Object.keys(schema.properties.contracts.properties).sort(), schema.properties.tools.const.slice().sort());
  for (const contract of Object.values(schema.properties.contracts.properties)) {
    assert.ok(Array.isArray(contract.input));
    assert.ok(Array.isArray(contract.output));
  }
});

test('codex spawn contract exposes four postures and rejects non-empty manifests', () => {
  const schema = JSON.parse(fs.readFileSync(path.join(root, 'schema/external-subagent-public-api.json'), 'utf8'));
  const spawn = schema.properties.spawn;
  assert.match(spawn.properties.permission_mode.description, /build\/edit \(workspace-write\)/u);
  assert.match(spawn.properties.permission_mode.description, /approvalPolicy=never/u);
  assert.match(spawn.properties.write_manifest.description, /codex_write_manifest_unsupported/u);
  const rule = spawn.allOf.find((rule) => rule.if.properties.subagent?.const === 'codex');
  assert.deepEqual(rule.if.required, ['subagent']);
  assert.equal(rule.then.properties.write_manifest.maxItems, 0);
  assert.equal(spawn.allOf.find((rule) => rule.if.properties.permission_mode?.const === 'plan').then.properties.write_manifest.maxItems, 0);
});


test('observation contract preserves public tails and object-level hidden reasoning', () => {
  const schema = JSON.parse(fs.readFileSync(path.join(root, 'schema/observation.schema.json'), 'utf8'));
  assert.deepEqual(schema.properties.reasoning.type, ['object', 'null']);
  assert.equal(schema.properties.reasoning.properties.text.maxLength, 200);
  assert.deepEqual(schema.properties.reasoning.required, ['text', 'truncated']);
  const { subagents } = JSON.parse(fs.readFileSync(path.join(root, 'schema/public-reasoning-source.json'), 'utf8'));
  assert.deepEqual(Object.keys(subagents), ['zcode', 'dsh', 'codex']);
  assert.equal(subagents.zcode.status, 'VERIFIED_RUNTIME_PUBLIC');
  assert.equal(subagents.zcode.runtime_version, '3.11.2');
  assert.equal(subagents.zcode.runtime_sha256, 'e9f1868c0fdb863537ed910ee3828b9be96b8c2fd805473f63b439e1113266b8');
  assert.equal(subagents.zcode.delta_pointer, '/params/payload/delta');
  assert.equal(subagents.dsh.public, true);
  assert.equal(subagents.dsh.status, 'VERIFIED_PROTOCOL_PUBLIC');
  assert.equal(subagents.codex.public, false);
  assert.equal(subagents.codex.collected, false);
  assert.equal(subagents.codex.reasoning, null);
});
