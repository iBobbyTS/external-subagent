import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../..', import.meta.url));

function runDaemonOracle(name) {
  const output = execFileSync(
    'cargo',
    ['test', '-p', 'external-daemon', name, '--', '--exact'],
    { cwd: root, encoding: 'utf8' },
  );
  assert.match(output, /test result: ok\. 1 passed/);
}

test('draining rejects new business send but preserves admitted messages', () => {
  runDaemonOracle('rpc::wait_tests::draining_existing_task_message_is_idempotent_but_new_rejected');
});

test('draining keeps admitted lifecycle operations available', () => {
  runDaemonOracle('rpc::wait_tests::draining_lifecycle_methods_are_not_gate_rejected');
});

test('status is read-only and explicit activation fires once', () => {
  runDaemonOracle('rpc::agent_probe_tests::draining_status_read_is_side_effect_free_and_activation_is_once');
});

test('aborting a drain reopens admission and preserves task facts', () => {
  runDaemonOracle('rpc::wait_tests::abort_drain_reopens_admission_and_preserves_task_facts');
});

test('aborting a drain keeps the issued claim for the retry', () => {
  runDaemonOracle('rpc::agent_probe_tests::abort_drain_preserves_the_issued_claim_for_the_retry');
});

test('aborting a drain reopens real provider admission while the drained task answers', () => {
  runDaemonOracle('dsh::tests::aborted_drain_reopens_dsh_admission_while_the_drained_task_keeps_answering');
});

test('aborting a drain is refused while explicit cancellation is in flight', () => {
  runDaemonOracle('dsh::tests::abort_drain_is_refused_while_explicit_cancellation_is_in_flight');
});
