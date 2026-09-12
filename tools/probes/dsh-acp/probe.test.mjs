import test from 'node:test';
import assert from 'node:assert/strict';
import { access } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { runProbe, SCENARIOS } from './probe.mjs';

const fixture = fileURLToPath(new URL('../../../tests/fixtures/dsh-acp/fake-server.mjs', import.meta.url));

test('probe exposes bounded scenarios and records ACP initialize/catalog', async () => {
  assert.deepEqual(SCENARIOS, ['initialize', 'catalog', 'hi', 'permission', 'cancel', 'malformed']);
  await access(fixture);
  const result = await runProbe({ executable: process.execPath, args: [fixture], scenario: 'catalog', timeout: 1000 });
  assert.equal(result.protocol, 'jsonrpc-over-stdio');
  assert.equal(result.malformedFrames, 0);
  assert.equal(result.responses[0].result.protocolVersion, 1);
  assert.deepEqual(result.responses[1].result.configOptions[0].options.map((entry) => entry.value), ['fixture-model', 'fixture-alt']);
});

test('probe captures permission updates, message identity, and keeps stderr separate', async () => {
  const result = await runProbe({ executable: process.execPath, args: [fixture], scenario: 'permission', timeout: 1000 });
  assert.equal(result.responses.some((entry) => entry.method === 'session/request_permission'), true);
  assert.equal(result.responses.some((entry) => entry.params?.update?.messageId === 'message-1'), true);
  assert.equal(result.stderr, '');
});

test('probe rejects absent executable and malformed timeout before spawning', async () => {
  await assert.rejects(() => runProbe({ scenario: 'hi', timeout: 1000 }), /executable_required/);
  await assert.rejects(() => runProbe({ executable: process.execPath, args: [fixture], scenario: 'hi', timeout: 99 }), /invalid_timeout/);
});
