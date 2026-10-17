import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
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
// `<config>.lock` for its whole duration. On POSIX the lock is owned by a
// helper process so a crashed writer releases it automatically: the helper
// acquires the lock, creates a `ready` acknowledgement, then holds the lock
// until this process kills the helper group. macOS uses lockf and Linux uses
// flock — both take an exclusive advisory lock on a file, run a command while
// holding it, and give up after a bounded wait, so the
// timeout/mutual-exclusion/failure semantics are equivalent. The binary is
// chosen by the host OS (a real capability), not by the
// EXTERNAL_SUBAGENT_TEST_PLATFORM layout seam.
//
// Windows has neither helper. The transaction locks by exclusive creation of
// the same `<config>.lock` file (the atomic `wx` flag). A crashed writer
// leaves the file behind, so a waiter snapshots it, renames it to a unique
// tombstone (the atomic claim), re-reads the tombstone, and only removes that
// tombstone when it still matches the verified snapshot and is stale. A file
// that was replaced in between belongs to a concurrent writer and is never
// deleted; it is restored with create-or-fail hard-link semantics, which
// cannot displace a newer lock at the canonical path. Release applies the
// same snapshot check to the acquisition's exact bytes/token. Both backends
// keep the same 2s deadline and the same CONFIG_LOCK_TIMEOUT failure contract.
const LOCK_TIMEOUT_SECONDS = 2;
const LOCK_DEADLINE_MS = 2500;
const LOCK_STALE_MS = 60_000;
// A creator writes pid/token immediately after the exclusive create. Do not
// reclaim an unparseable/empty lock during that window (crash between create
// and write is reclaimed after this grace period).
const LOCK_WRITE_GRACE_MS = 5_000;
const LOCK_READY_SCRIPT = 'ready="$1"; parent="$2"; printf ready > "$ready"; while kill -0 "$parent" 2>/dev/null; do sleep 0.05; done';

export function lockStrategy(platform = process.platform) {
  return platform === 'win32' ? 'exclusive-create' : 'helper';
}

export function lockHelperInvocation(lock, ready, platform = process.platform) {
  if (platform === 'win32') return { strategy: 'exclusive-create', lock, ready };
  const tail = ['/bin/sh', '-c', LOCK_READY_SCRIPT, 'external-subagent-lock', ready, String(process.pid)];
  return platform === 'darwin'
    ? { command: '/usr/bin/lockf', args: ['-t', String(LOCK_TIMEOUT_SECONDS), lock, ...tail] }
    : { command: '/usr/bin/flock', args: ['-w', String(LOCK_TIMEOUT_SECONDS), lock, ...tail] };
}

function processIsAlive(pid) {
  if (!Number.isInteger(pid) || pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error?.code === 'EPERM';
  }
}

// An immutable snapshot of the lock file: the exact bytes plus the stat
// identity. Reclaiming renames the file first and then compares the *moved*
// snapshot, so the verify-then-remove sequence never races a concurrent
// creator at the original path.
function lockSnapshot(lock) {
  let raw;
  let stat;
  try {
    raw = fs.readFileSync(lock, 'utf8');
    stat = fs.statSync(lock);
  } catch {
    return null;
  }
  let pid = null;
  try {
    const parsed = JSON.parse(raw);
    if (Number.isInteger(parsed) && parsed > 0) pid = parsed;
    else if (Number.isInteger(parsed?.pid) && parsed.pid > 0) pid = parsed.pid;
  } catch {
    const parsed = Number.parseInt(String(raw).trim(), 10);
    if (Number.isInteger(parsed) && parsed > 0) pid = parsed;
  }
  return {
    raw,
    pid,
    ageMs: Date.now() - stat.mtimeMs,
    mtimeMs: stat.mtimeMs,
    size: stat.size,
  };
}

function snapshotIsStale(snapshot) {
  if (snapshot === null) return false;
  if (snapshot.pid === null) return snapshot.ageMs > LOCK_WRITE_GRACE_MS;
  return !processIsAlive(snapshot.pid) || snapshot.ageMs > LOCK_STALE_MS;
}

function sameLockSnapshot(left, right) {
  return left !== null
    && right !== null
    && left.raw === right.raw
    && left.size === right.size
    && left.mtimeMs === right.mtimeMs;
}

// Restore a lock file without ever displacing a newer one. `rename` REPLACES
// an existing destination on both POSIX and Windows, so it cannot be used for
// a restore. `linkSync` is create-or-fail: EEXIST means a concurrent writer
// owns the canonical path, and the saved file is left untouched for that
// writer's own release. On success the temporary name is removed.
export function restoreLockWithoutDisplacing(saved, target) {
  try {
    fs.linkSync(saved, target);
  } catch {
    return false;
  }
  try { fs.unlinkSync(saved); } catch {}
  return true;
}

function reclaimStaleLock(lock) {
  const before = lockSnapshot(lock);
  if (!snapshotIsStale(before)) return false;
  // Rename is the atomic claim: a losing racer gets ENOENT and never touches
  // the winner's file. Everything after this line operates on the tombstone,
  // never on the original path.
  const tombstone = `${lock}.stale-${process.pid}-${crypto.randomUUID()}`;
  try {
    fs.renameSync(lock, tombstone);
  } catch {
    return false;
  }
  const moved = lockSnapshot(tombstone);
  if (sameLockSnapshot(before, moved) && snapshotIsStale(moved)) {
    try { fs.unlinkSync(tombstone); } catch {}
    return true;
  }
  // The file we moved was replaced between the snapshot and the rename: it is
  // a concurrent writer's fresh lock. Never delete it, and restore only with
  // create-or-fail semantics so a newer lock at the canonical path is never
  // overwritten.
  restoreLockWithoutDisplacing(tombstone, lock);
  return false;
}

function releaseExclusiveFileLock(lock, expectedRaw) {
  // Verify-then-remove on an immutable snapshot: only a lock whose exact
  // bytes are still this acquisition's may be removed. The rename is the
  // atomic claim; if the original path was replaced between the read and the
  // rename, the moved file belongs to another writer and is restored without
  // displacing whatever now owns the canonical path.
  const current = lockSnapshot(lock);
  if (current === null || current.raw !== expectedRaw) return;
  const tombstone = `${lock}.release-${process.pid}-${crypto.randomUUID()}`;
  try {
    fs.renameSync(lock, tombstone);
  } catch {
    return;
  }
  const moved = lockSnapshot(tombstone);
  if (moved !== null && moved.raw === expectedRaw) {
    try { fs.unlinkSync(tombstone); } catch {}
    return;
  }
  restoreLockWithoutDisplacing(tombstone, lock);
}

function acquireExclusiveFileLock(lock) {
  const wait = new Int32Array(new SharedArrayBuffer(4));
  const deadline = Date.now() + LOCK_DEADLINE_MS;
  for (;;) {
    let fd;
    try {
      fd = fs.openSync(lock, 'wx', 0o600);
    } catch (error) {
      // Windows may surface a concurrent create/delete race as EACCES/EPERM
      // instead of EEXIST; both are contention, not a configuration failure.
      if (!['EEXIST', 'EACCES', 'EPERM'].includes(error?.code)) throw error;
    }
    if (fd !== undefined) {
      const token = crypto.randomUUID();
      const contents = JSON.stringify({ pid: process.pid, token, started_at_ms: Date.now() });
      try {
        fs.writeSync(fd, contents);
      } catch (error) {
        try { fs.unlinkSync(lock); } catch {}
        throw error;
      } finally {
        fs.closeSync(fd);
      }
      return () => releaseExclusiveFileLock(lock, contents);
    }
    if (reclaimStaleLock(lock)) continue;
    if (Date.now() >= deadline) throw new Error('CONFIG_LOCK_TIMEOUT');
    Atomics.wait(wait, 0, 0, 5);
  }
}

function acquireHelperLock(lock) {
  const ready = `${lock}.${process.pid}.${Date.now()}.ready`;
  const { command, args } = lockHelperInvocation(lock, ready);
  const helper = spawn(command, args, { stdio: 'ignore', detached: true });
  const wait = new Int32Array(new SharedArrayBuffer(4));
  const deadline = Date.now() + LOCK_DEADLINE_MS;
  while (!fs.existsSync(ready)) {
    if (helper.exitCode !== null || Date.now() >= deadline) {
      try { process.kill(-helper.pid, 'SIGTERM'); } catch {}
      throw new Error('CONFIG_LOCK_TIMEOUT');
    }
    Atomics.wait(wait, 0, 0, 5);
  }
  return () => {
    try { process.kill(-helper.pid, 'SIGTERM'); } catch {}
    try { fs.unlinkSync(ready); } catch {}
  };
}

export function updateConfig(file, updater) {
  const lock = `${path.resolve(file)}.lock`;
  fs.mkdirSync(path.dirname(lock), { recursive: true, mode: 0o700 });
  const release = lockStrategy() === 'exclusive-create'
    ? acquireExclusiveFileLock(lock)
    : acquireHelperLock(lock);
  try {
    const current = readConfig(file);
    const next = updater(current);
    return next === current ? current : writeConfig(file, next);
  } finally {
    release();
  }
}
