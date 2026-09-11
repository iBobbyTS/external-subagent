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

export function updateConfig(file, updater) {
  const lock = `${path.resolve(file)}.lock`;
  fs.mkdirSync(path.dirname(lock), { recursive: true, mode: 0o700 });
  const ready = `${lock}.${process.pid}.${Date.now()}.ready`;
  const helper = spawn('/usr/bin/lockf', ['-t', '2', lock, '/bin/sh', '-c',
    'ready="$1"; parent="$2"; printf ready > "$ready"; while kill -0 "$parent" 2>/dev/null; do sleep 0.05; done',
    'external-subagent-lock', ready, String(process.pid)], { stdio: 'ignore', detached: true });
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
    return writeConfig(file, next);
  } finally {
    try { process.kill(-helper.pid, 'SIGTERM'); } catch {}
    try { fs.unlinkSync(ready); } catch {}
  }
}
