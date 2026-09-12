import test from 'node:test';
import assert from 'node:assert/strict';

// Create-before-run contract for the daemon draining gate. The production
// facade will provide these operations once S06.B implementation lands.
function drainFacade(overrides = {}) {
  return {
    beginDrain: () => ({ phase: 'draining' }),
    spawn: () => { throw Object.assign(new Error('daemon is draining'), { code: 'daemon_draining' }); },
    send: () => { throw Object.assign(new Error('daemon is draining'), { code: 'daemon_draining' }); },
    wait: () => ({ ok: true }), respond: () => ({ ok: true }), cancel: () => ({ ok: true }),
    result: () => ({ ok: true }), close: () => ({ ok: true }),
    readiness: () => ({ active_count: 0, resources_reaped: true, ready_for_activation: true }),
    ...overrides,
  };
}

test('draining rejects new spawn and business send with stable error code', () => {
  const daemon = drainFacade();
  assert.equal(daemon.beginDrain().phase, 'draining');
  for (const operation of ['spawn', 'send']) {
    assert.throws(() => daemon[operation](), (error) => error.code === 'daemon_draining');
  }
});

test('draining keeps admitted lifecycle operations available', () => {
  const daemon = drainFacade();
  for (const operation of ['wait', 'respond', 'cancel', 'result', 'close']) {
    assert.deepEqual(daemon[operation](), { ok: true });
  }
});

test('ready for activation requires zero active tasks and reaped resources', () => {
  const daemon = drainFacade({ readiness: () => ({ active_count: 1, resources_reaped: false, ready_for_activation: false }) });
  assert.equal(daemon.readiness().ready_for_activation, false);
  const idle = drainFacade();
  assert.deepEqual(idle.readiness(), { active_count: 0, resources_reaped: true, ready_for_activation: true });
});
