import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { atomicWrite, jsonBytes } from '../fs-atomic.mjs';
import { validateConfig } from './schema.mjs';
import { readConfig } from './read.mjs';

export function writeConfig(file, value) {
  const config = validateConfig(value);
  config.revision += 1;
  atomicWrite(file, jsonBytes(config));
  return config;
}

// The read-modify-write config transaction holds an exclusive lock on
// `<config>.lock` for its whole duration. The lock is owned by a helper
// process so a crashed writer releases it automatically: the helper acquires
// the lock, creates a `ready` acknowledgement, then holds the lock until this
// process kills the helper group. macOS uses lockf and Linux uses flock — both
// take an exclusive advisory lock on a file, run a command while holding it,
// and give up after a bounded wait, so the timeout/mutual-exclusion/failure
// semantics are equivalent. The binary is chosen by the host OS (a real
// capability), not by the EXTERNAL_SUBAGENT_TEST_PLATFORM layout seam.
const LOCK_TIMEOUT_SECONDS = 2;
const LOCK_READY_SCRIPT = 'ready="$1"; parent="$2"; printf ready > "$ready"; while kill -0 "$parent" 2>/dev/null; do sleep 0.05; done';

export function lockHelperInvocation(lock, ready, platform = process.platform) {
  const tail = ['/bin/sh', '-c', LOCK_READY_SCRIPT, 'external-subagent-lock', ready, String(process.pid)];
  return platform === 'darwin'
    ? { command: '/usr/bin/lockf', args: ['-t', String(LOCK_TIMEOUT_SECONDS), lock, ...tail] }
    : { command: '/usr/bin/flock', args: ['-w', String(LOCK_TIMEOUT_SECONDS), lock, ...tail] };
}

export function updateConfig(file, updater) {
  const lock = `${path.resolve(file)}.lock`;
  fs.mkdirSync(path.dirname(lock), { recursive: true, mode: 0o700 });
  const ready = `${lock}.${process.pid}.${Date.now()}.ready`;
  const { command, args } = lockHelperInvocation(lock, ready);
  const helper = spawn(command, args, { stdio: 'ignore', detached: true });
  const wait = new Int32Array(new SharedArrayBuffer(4));
  const deadline = Date.now() + 2500;
  while (!fs.existsSync(ready)) {
    if (helper.exitCode !== null || Date.now() >= deadline) {
      try { process.kill(-helper.pid, 'SIGTERM'); } catch {}
      throw new Error('CONFIG_LOCK_TIMEOUT');
    }
    Atomics.wait(wait, 0, 0, 5);
  }
  try {
    const current = readConfig(file);
    const next = updater(current);
    return next === current ? current : writeConfig(file, next);
  } finally {
    try { process.kill(-helper.pid, 'SIGTERM'); } catch {}
    try { fs.unlinkSync(ready); } catch {}
  }
}
