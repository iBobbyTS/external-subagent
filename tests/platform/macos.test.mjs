import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { ZCODE_RUNTIME } from '../../cli/constants.mjs';

test('macOS dry-run remains PATH-independent, host-neutral, and side-effect free', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-mac-'));
  const result = spawnSync(process.execPath, [path.resolve('bin/external-subagent.mjs'), 'init', '--dry-run'], {
    encoding: 'utf8', env: { HOME: home, PATH: '', ZCODE_AS_SUBAGENT_TEST_PLATFORM: 'darwin' },
  });
  assert.equal(result.status, 0, result.stderr);
  const { plan } = JSON.parse(result.stdout);
  assert.equal(plan[0].id, 'verify-payload');
  // AUD-005/D1: the standalone init plan probes no fixed runtime, installs no
  // host plugin, and claims no codex home — the only absolute runtime path it
  // may mention is the payload it ships itself.
  assert.equal(plan.some((step) => step.path === ZCODE_RUNTIME), false, 'no step probes the fixed ZCode runtime');
  assert.equal(plan.some((step) => ['probe-runtime', 'install-codex-plugin', 'claim-codex-home'].includes(step.id)), false, 'the retired implicit host steps stay out of the plan');
  assert.deepEqual(fs.readdirSync(home), []);
});
