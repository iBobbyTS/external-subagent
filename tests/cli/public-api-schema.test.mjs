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
  assert.equal(schema.properties.spawn.properties.effort.pattern, undefined);
  assert.equal(schema.properties.spawn.properties.effort.maxLength, undefined);
  assert.equal(schema.properties.spawn.properties.developer_instructions.type, 'string');
  assert.equal(schema.properties.spawn.properties.developer_instructions.maxLength, 1048576);
  assert.equal(schema.properties.list.properties.limit.default, 100);
  assert.equal(schema.properties.wait.properties.wait_time.default, 290);
  assert.equal(schema.properties.wait.properties.wait_time.maximum, 600);
  assert.equal(schema.properties.wait.properties.wait_mode.default, 'all');
  assert.deepEqual(schema.properties.wait.properties.wait_mode.enum, ['all', 'any']);
  assert.ok(schema.properties.list.required.includes('repository'));
  assert.equal(schema.properties.wait.properties.supports_answer, undefined);
  assert.equal(schema.properties.wait.properties.after_revision, undefined);
  assert.match(schema.properties.wait.description, /actionable pending request/u);
  assert.match(schema.properties.wait.description, /embedded question/u);
  assert.match(schema.properties.wait.description, /in_flight_tool_count/u);
  assert.match(schema.properties.wait.description, /oldest_in_flight_tool_age_ms/u);
  assert.match(schema.properties.wait.description, /model_activity/u);
  assert.match(schema.properties.wait.description, /diagnostics_truncated/u);
  assert.match(schema.properties.wait.description, /summary_limit/u);
  assert.equal(schema.properties.wait.description.includes('last_tool_calls'), false);
  assert.equal(schema.properties.wait.description.includes('reasoning tail'), false);
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
    'agent_id', 'wait_time', 'message_id', 'wait_mode',
  ]);
  assert.deepEqual(schema.properties.contracts.properties.external_subagent_wait.output, [
    'task', 'pending_requests', 'activity', 'result', 'timed_out', 'message_receipt',
  ]);
  assert.deepEqual(schema.properties.error_projection.required, ['error']);
  assert.deepEqual(schema.properties.error_projection.properties.error.required, ['code', 'message']);
  assert.deepEqual(Object.keys(schema.properties.error_projection.properties.error.properties).sort(), [
    'agent_id', 'code', 'component', 'message', 'native_error', 'operation', 'prompt_count', 'request_id',
  ]);
  const nativeError = schema.properties.error_projection.properties.error.properties.native_error;
  const diagnostic = schema.properties.activity_diagnostic_projection;
  assert.deepEqual(diagnostic.required, [
    'source', 'reason', 'impact', 'discriminator', 'count', 'last_observed_at',
  ]);
  assert.equal(diagnostic.additionalProperties, false);
  assert.ok(diagnostic.properties.source.enum.includes('zcode_session'));
  assert.deepEqual(diagnostic.properties.impact.oneOf[0].properties.scope.const, 'unknown');
  assert.deepEqual(diagnostic.properties.impact.oneOf[1].properties.dimensions.items.enum, [
    'text', 'tools', 'public_reasoning', 'model_activity', 'input', 'turn',
  ]);
  assert.equal(diagnostic.properties.count.minimum, 1);
  assert.deepEqual(nativeError.required, ['truncated', 'total_bytes', 'limit_bytes']);
  assert.equal(nativeError.additionalProperties, false);
  assert.ok(Object.hasOwn(nativeError.properties, 'original'));
  assert.ok(Object.hasOwn(nativeError.properties, 'projection'));
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
  assert.deepEqual(schema.properties.contracts.properties.external_subagent_spawn.output, ['agent_id', 'status', 'session_id', 'instruction']);
  assert.deepEqual(schema.properties.contracts.properties.external_subagent_spawn.input, [
    'profile', 'subagent', 'repository', 'permission_mode', 'prompt', 'model', 'effort', 'developer_instructions', 'write_manifest',
  ]);
  assert.deepEqual(schema.properties.contracts.properties.external_subagent_spawn.idempotent, false);
  assert.deepEqual(schema.properties.result_projection.required, [
    'outcome', 'final_text', 'partial', 'offset', 'total_bytes', 'next_offset', 'reason_code',
    'failure_message',
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


test('observation contract keeps public tails and typed native evidence', () => {
  const schema = JSON.parse(fs.readFileSync(path.join(root, 'schema/observation.schema.json'), 'utf8'));
  assert.deepEqual(schema.properties.reasoning.type, ['object', 'null']);
  assert.equal(schema.properties.reasoning.properties.text.maxLength, 200);
  assert.deepEqual(schema.properties.reasoning.required, ['text', 'truncated']);
  // Reasoning identity is the observed native protocol/channel, not a
  // per-adapter capability table or retired wire-pointer labels.
  const source = schema.properties.reasoning.properties.source;
  assert.equal(source.type, 'object');
  assert.deepEqual(source.required, ['protocol', 'channel']);
  assert.equal(source.additionalProperties, false);
  assert.deepEqual(source.properties.channel.enum, ['summary', 'thought']);
  assert.ok(source.properties.protocol.enum.includes('zcode_session'));
  assert.ok(source.properties.protocol.enum.includes('dsh_acp_v1'));
  assert.equal(schema.properties.reasoning.properties.status, undefined);
  assert.equal(schema.properties.reasoning.properties.delta_pointer, undefined);

  const call = schema.properties.tools.items.properties.recent_calls.items;
  assert.ok(call.required.includes('status'));
  assert.deepEqual(call.properties.status.enum, [
    'unknown', 'pending', 'running', 'finished', 'succeeded', 'failed', 'cancelled',
  ]);
});

test('agy spawn contract leaves model and effort to the runtime and requires build/yolo and empty manifests', () => {
  const schema = JSON.parse(fs.readFileSync(path.join(root, 'schema/external-subagent-public-api.json'), 'utf8'));
  const spawn = schema.properties.spawn;
  assert.match(spawn.properties.write_manifest.description, /agy_write_manifest_unsupported/u);
  assert.match(spawn.properties.model.description, /agy uses its native model token/u);
  const rule = spawn.allOf.find((rule) => rule.if.properties.subagent?.const === 'agy');
  assert.deepEqual(rule.if.required, ['subagent']);
  assert.deepEqual(rule.then.properties.permission_mode.enum, ['build', 'yolo']);
  assert.equal(rule.then.properties.effort, undefined);
  assert.equal(rule.then.properties.model, undefined);
  assert.equal(spawn.properties.effort.pattern, undefined);
  assert.equal(spawn.properties.effort.maxLength, undefined);
  assert.equal(rule.then.properties.write_manifest.maxItems, 0);
  assert.deepEqual(schema.properties.subagent_status.properties.subagent.enum, ['zcode', 'dsh', 'codex', 'agy']);
});

test('zcode spawn contract requires an explicit provider/model selection', () => {
  const schema = JSON.parse(fs.readFileSync(path.join(root, 'schema/external-subagent-public-api.json'), 'utf8'));
  const model = schema.properties.spawn.properties.model.description;
  assert.match(model, /zcode requires an explicit provider\/model token/u);
  assert.doesNotMatch(model, /bare token keeps/u);
  assert.doesNotMatch(model, /default_model for dsh, zcode and agy/u);
});
