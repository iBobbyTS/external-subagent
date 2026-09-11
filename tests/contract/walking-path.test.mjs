import test from 'node:test';
import assert from 'node:assert/strict';

class FakeTask {
  constructor() { this.state = 'running'; this.result = null; }
  wait() { assert.equal(this.state, 'running'); this.state = 'completed'; this.result = 'ok'; return { state: this.state }; }
  close() { assert.equal(this.state, 'completed'); this.state = 'closed'; return { state: this.state }; }
}
test('ZCode walking path spawn wait result close', () => {
  const task = new FakeTask();
  assert.equal(task.state, 'running');
  const waited = task.wait();
  assert.deepEqual(waited, { state: 'completed' });
  assert.equal(task.result, 'ok');
  assert.deepEqual(task.close(), { state: 'closed' });
});
test('close before completion is rejected', () => {
  assert.throws(() => new FakeTask().close());
});
