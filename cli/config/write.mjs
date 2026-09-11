import fs from 'node:fs';
import path from 'node:path';
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
  const wait = new Int32Array(new SharedArrayBuffer(4));
  let descriptor;
  const deadline = Date.now() + 2000;
  while (!descriptor) {
    try {
      descriptor = fs.openSync(lock, 'wx', 0o600);
      fs.writeFileSync(descriptor, `${process.pid}\n`);
    }
    catch (error) {
      if (error.code !== 'EEXIST' || Date.now() >= deadline) throw error;
      let stale = false;
      try {
        const owner = fs.readFileSync(lock, 'utf8').trim();
        if (owner && /^\d+$/u.test(owner)) {
          try { process.kill(Number(owner), 0); } catch (probeError) { stale = probeError.code === 'ESRCH'; }
        } else {
          const stat = fs.statSync(lock);
          stale = stat.size === 0 && Date.now() - stat.mtimeMs > 2000;
        }
      } catch (probeError) { stale = probeError.code === 'ENOENT'; }
      if (stale) { try { fs.unlinkSync(lock); } catch {} continue; }
      Atomics.wait(wait, 0, 0, 5);
    }
  }
  try {
    const current = readConfig(file);
    const next = updater(current);
    return writeConfig(file, next);
  } finally {
    fs.closeSync(descriptor);
    fs.unlinkSync(lock);
  }
}
