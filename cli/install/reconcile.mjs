import fs from 'node:fs';
import path from 'node:path';
import { CliError } from '../errors.mjs';
import { atomicWrite, jsonBytes, readOptional } from '../fs-atomic.mjs';
import { installPlugin } from './codex.mjs';

// D08: the single owner of the global installed-Codex-homes registry.  The
// registry records every Codex home this product has successfully claimed via
// an explicit init or plugin install; reconcile only ever writes homes that
// are both registered and actually writable, so unrelated CODEX_HOME
// directories are never touched.  A corrupted registry is atomically replaced
// and reported as a recoverable error instead of aborting the caller.

const REGISTRY_SCHEMA_VERSION = 1;

export function codexHomesRegistryPath(paths) {
  return path.join(paths.data, 'codex-homes.json');
}

function emptyRegistry() {
  return { schema_version: REGISTRY_SCHEMA_VERSION, product: 'external-subagent', homes: [] };
}

function resolveHome(home) {
  if (typeof home !== 'string' || home.length === 0) throw new CliError('INVALID_ARGUMENT', 'codex home must be a non-empty string', 2);
  const absolute = path.resolve(home);
  try { return fs.realpathSync(absolute); } catch { return absolute; }
}

function normalizeEntry(entry) {
  if (!entry || typeof entry.home !== 'string') throw new CliError('CODEX_HOMES_REGISTRY_INVALID', 'registry entry is missing its home path');
  return {
    home: entry.home,
    claimed_at_ms: Number.isInteger(entry.claimed_at_ms) ? entry.claimed_at_ms : null,
    version: typeof entry.version === 'string' ? entry.version : null,
    digest: typeof entry.digest === 'string' ? entry.digest : null,
    last_sync_ms: Number.isInteger(entry.last_sync_ms) ? entry.last_sync_ms : null,
    last_status: typeof entry.last_status === 'string' ? entry.last_status : null,
  };
}

function parseRegistry(bytes, file) {
  const doc = JSON.parse(bytes.toString('utf8'));
  if (doc?.schema_version !== REGISTRY_SCHEMA_VERSION || doc?.product !== 'external-subagent' || !Array.isArray(doc.homes)) {
    throw new CliError('CODEX_HOMES_REGISTRY_INVALID', 'registry identity or homes table is invalid');
  }
  const registry = emptyRegistry();
  registry.homes = doc.homes.map(normalizeEntry);
  return registry;
}

export function loadCodexHomes(paths) {
  const file = codexHomesRegistryPath(paths);
  const bytes = readOptional(file);
  if (bytes === null) return { registry: emptyRegistry(), recovery: null };
  try {
    return { registry: parseRegistry(bytes, file), recovery: null };
  } catch (error) {
    // Corrupted registry: preserve the bytes for inspection, atomically stage
    // a fresh registry, and report a recoverable error to the caller.
    const backup = `${file}.corrupt-${Date.now()}`;
    try { fs.renameSync(file, backup); } catch (renameError) {
      throw new CliError('CODEX_HOMES_REGISTRY_CORRUPT', `registry is corrupted and could not be preserved: ${renameError.message}`);
    }
    atomicWrite(file, jsonBytes(emptyRegistry()));
    return {
      registry: emptyRegistry(),
      recovery: { status: 'recovered', code: 'CODEX_HOMES_REGISTRY_CORRUPT', backup, error: error.message },
    };
  }
}

function persistRegistry(paths, registry) {
  atomicWrite(codexHomesRegistryPath(paths), jsonBytes(registry));
}

export function registerCodexHome(paths, home, meta = {}) {
  const { registry, recovery } = loadCodexHomes(paths);
  const resolved = resolveHome(home);
  const existing = registry.homes.find((entry) => entry.home === resolved);
  let entry;
  if (existing) {
    entry = existing;
    if (typeof meta.version === 'string') entry.version = meta.version;
    if (typeof meta.digest === 'string') entry.digest = meta.digest;
    if (typeof meta.status === 'string') entry.last_status = meta.status;
  } else {
    entry = {
      home: resolved,
      claimed_at_ms: Date.now(),
      version: typeof meta.version === 'string' ? meta.version : null,
      digest: typeof meta.digest === 'string' ? meta.digest : null,
      last_sync_ms: null,
      last_status: typeof meta.status === 'string' ? meta.status : 'claimed',
    };
    registry.homes.push(entry);
  }
  persistRegistry(paths, registry);
  return { registered: true, home: resolved, deduplicated: Boolean(existing), homes: registry.homes.length, recovery };
}

export function unregisterCodexHome(paths, home) {
  const { registry } = loadCodexHomes(paths);
  const resolved = resolveHome(home);
  const remaining = registry.homes.filter((entry) => entry.home !== resolved);
  const unregistered = remaining.length !== registry.homes.length;
  registry.homes = remaining;
  if (unregistered) persistRegistry(paths, registry);
  return { unregistered, home: resolved, homes: registry.homes.length };
}

// Product uninstall: release every claim at once.  The registry file itself
// stays inside retained product data, but no home remains claimed.  A
// never-initialized product is not given a registry file by uninstalling.
export function unregisterAllCodexHomes(paths) {
  const { registry } = loadCodexHomes(paths);
  const released = registry.homes.map((entry) => entry.home);
  const file = codexHomesRegistryPath(paths);
  if (released.length > 0 || fs.existsSync(file)) {
    registry.homes = [];
    persistRegistry(paths, registry);
  }
  return { unregistered: released.length, homes: released };
}

function writableDirectory(home) {
  try {
    const stat = fs.statSync(home);
    if (!stat.isDirectory()) return false;
    fs.accessSync(home, fs.constants.W_OK);
    return true;
  } catch { return false; }
}

// Refresh the managed binding in every registered, writable Codex home.
// Results are reported per home; a skipped or failed home never collapses
// into a single success for the whole reconcile (P13).
export function reconcileCodexHomes(paths, options = {}) {
  const { registry } = loadCodexHomes(paths);
  const results = [];
  for (const entry of registry.homes) {
    if (!writableDirectory(entry.home)) {
      entry.last_status = 'skipped_not_writable';
      results.push({ home: entry.home, status: 'skipped_not_writable' });
      continue;
    }
    try {
      const install = installPlugin(paths, { ...options, codexHome: entry.home });
      entry.last_status = 'updated';
      entry.last_sync_ms = Date.now();
      if (install.digest) entry.digest = install.digest;
      results.push({ home: entry.home, status: 'updated', cache: install.cache || null });
    } catch (error) {
      entry.last_status = 'failed';
      results.push({ home: entry.home, status: 'failed', error: { code: error.code || 'CODEX_BINDING_FAILED', message: error.message } });
    }
  }
  persistRegistry(paths, registry);
  return {
    homes: results,
    all_updated: results.length > 0 && results.every((result) => result.status === 'updated'),
  };
}
