import fs from 'node:fs';
import path from 'node:path';
import { CliError } from '../errors.mjs';
import { atomicWrite, jsonBytes, readOptional } from '../fs-atomic.mjs';

// D28: the persistent registry of bound DSH profiles.  The dsh binding is
// derivable from the patch sentinel in daily operation, but the sentinel
// itself lives inside a file the dsh desktop rewrites wholesale on upgrade
// (OBSERVED 2026-10-28, dsh 0.2.1-rc.3: both bound profiles lost the
// managed block AND the staged skill, while the product's sentinel-only
// reconcile reported bound:false and silently forgot the machines ever
// bound).  This registry remembers what the user explicitly bound so
// reconcile can RESTORE a dropped binding instead of merely refreshing
// surviving ones.  It follows the codex-homes D08 model: atomic writes,
// per-entry status bookkeeping, and corruption recovery that preserves the
// bad bytes as evidence and continues with an empty table rather than
// aborting the caller.

const REGISTRY_SCHEMA_VERSION = 1;

export function dshBindingsRegistryPath(paths) {
  return path.join(paths.data, 'dsh-bindings.json');
}

function emptyRegistry() {
  return { schema_version: REGISTRY_SCHEMA_VERSION, product: 'external-subagent', bindings: [] };
}

// Resolve a home to its canonical registry key.  A plain realpath fails on
// a not-yet-created directory (a dsh home registered before its first
// boot), and on macOS the /var -> /private/var symlink then yields the
// UNRESOLVED prefix — so the same home would register and reconcile under
// two different strings depending on whether it existed at the time.
// Resolve the deepest EXISTING ancestor and append the remainder instead:
// the key is stable across creation, removal, and symlinked tmpdirs.
export function canonicalHome(home) {
  if (typeof home !== 'string' || home.length === 0) throw new CliError('INVALID_ARGUMENT', 'dsh home must be a non-empty string', 2);
  const absolute = path.resolve(home);
  try { return fs.realpathSync(absolute); } catch { /* fall through to ancestor resolution */ }
  const parent = path.dirname(absolute);
  if (parent === absolute) return absolute;
  return path.join(canonicalHome(parent), path.basename(absolute));
}

function resolveHome(home) {
  return canonicalHome(home);
}

function normalizeEntry(entry) {
  if (!entry || typeof entry.home !== 'string' || typeof entry.profile !== 'string') {
    throw new CliError('DSH_BINDINGS_REGISTRY_INVALID', 'registry entry is missing its home or profile path');
  }
  if (/[\\/]/u.test(entry.profile)) {
    throw new CliError('DSH_BINDINGS_REGISTRY_INVALID', 'registry entry profile must not contain path separators');
  }
  return {
    home: entry.home,
    profile: entry.profile,
    claimed_at_ms: Number.isInteger(entry.claimed_at_ms) ? entry.claimed_at_ms : null,
    last_sync_ms: Number.isInteger(entry.last_sync_ms) ? entry.last_sync_ms : null,
    last_status: typeof entry.last_status === 'string' ? entry.last_status : null,
  };
}

function parseRegistry(bytes, file) {
  const doc = JSON.parse(bytes.toString('utf8'));
  if (doc?.schema_version !== REGISTRY_SCHEMA_VERSION || doc?.product !== 'external-subagent' || !Array.isArray(doc.bindings)) {
    throw new CliError('DSH_BINDINGS_REGISTRY_INVALID', 'registry identity or bindings table is invalid');
  }
  const registry = emptyRegistry();
  registry.bindings = doc.bindings.map(normalizeEntry);
  return registry;
}

// Corrupted registry: preserve the bytes for inspection, atomically stage a
// fresh registry, and report a recoverable error — never abort the caller.
export function loadDshBindings(paths) {
  const file = dshBindingsRegistryPath(paths);
  const bytes = readOptional(file);
  if (bytes === null) return { registry: emptyRegistry(), recovery: null };
  try {
    return { registry: parseRegistry(bytes, file), recovery: null };
  } catch (error) {
    const backup = `${file}.corrupt-${Date.now()}`;
    try { fs.renameSync(file, backup); } catch (renameError) {
      throw new CliError('DSH_BINDINGS_REGISTRY_CORRUPT', `registry is corrupted and could not be preserved: ${renameError.message}`);
    }
    atomicWrite(file, jsonBytes(emptyRegistry()));
    return {
      registry: emptyRegistry(),
      recovery: { status: 'recovered', code: 'DSH_BINDINGS_REGISTRY_CORRUPT', backup, error: error.message },
    };
  }
}

export function persistRegistry(paths, registry) {
  atomicWrite(dshBindingsRegistryPath(paths), jsonBytes(registry));
}

export function registerDshBinding(paths, dshHome, profile, meta = {}) {
  const { registry, recovery } = loadDshBindings(paths);
  const home = resolveHome(dshHome);
  const existing = registry.bindings.find((entry) => entry.home === home && entry.profile === profile);
  let entry;
  if (existing) {
    entry = existing;
    if (typeof meta.status === 'string') entry.last_status = meta.status;
  } else {
    entry = {
      home,
      profile,
      claimed_at_ms: Date.now(),
      last_sync_ms: null,
      last_status: typeof meta.status === 'string' ? meta.status : 'claimed',
    };
    registry.bindings.push(entry);
  }
  persistRegistry(paths, registry);
  return { registered: true, home, profile, deduplicated: Boolean(existing), bindings: registry.bindings.length, recovery };
}

export function unregisterDshBinding(paths, dshHome, profile) {
  const { registry } = loadDshBindings(paths);
  const home = resolveHome(dshHome);
  const remaining = registry.bindings.filter((entry) => !(entry.home === home && entry.profile === profile));
  const unregistered = remaining.length !== registry.bindings.length;
  registry.bindings = remaining;
  if (unregistered) persistRegistry(paths, registry);
  return { unregistered, home, profile, bindings: registry.bindings.length };
}

// Product uninstall: no profile remains claimed.  The registry file itself
// stays inside retained product data, matching the codex-homes posture.
export function unregisterAllDshBindings(paths) {
  const { registry } = loadDshBindings(paths);
  const released = registry.bindings.map((entry) => ({ home: entry.home, profile: entry.profile }));
  const file = dshBindingsRegistryPath(paths);
  if (released.length > 0 || fs.existsSync(file)) {
    registry.bindings = [];
    persistRegistry(paths, registry);
  }
  return { unregistered: released.length, bindings: released };
}
