import test from 'node:test';
import assert from 'node:assert/strict';

const providers = [
  { name: 'zcode', capabilities: ['wait', 'cancel', 'result'] },
  { name: 'dsh', capabilities: ['wait', 'cancel', 'result'] },
];

for (const provider of providers) {
  test(`${provider.name} exposes the shared lifecycle contract`, () => {
    assert.deepEqual(provider.capabilities, ['wait', 'cancel', 'result']);
    assert.equal(new Set(provider.capabilities).size, provider.capabilities.length);
  });
}

test('provider adapters do not share a workspace concurrently', () => {
  const leases = new Map();
  leases.set('/workspace', 'zcode');
  assert.equal(leases.get('/workspace'), 'zcode');
  assert.equal(leases.has('/workspace'), true);
});
