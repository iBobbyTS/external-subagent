import { backupData, purge, restoreData, uninstall as removeService } from '../maintenance.mjs';
import { unregisterAllCodexHomes } from '../install/reconcile.mjs';
import { uninstallZcodePlugin } from '../install/zcode.mjs';
import { uninstallAllDshBindings } from '../install/dsh.mjs';
import fs from 'node:fs';
import path from 'node:path';
import { atomicWrite, jsonBytes, readOptional } from '../fs-atomic.mjs';

// Maintenance command coordination.  The ES-owned service is booted out and
// its definition removed before any D08 claim is released: a bootout that
// cannot complete fails the command with the registry exactly as it was, so
// an uninstall retry (or a reconcile) still sees every claimed home instead
// of a wiped registry.  Only a fully removed service releases the claims;
// product data is retained by contract, and the emptied registry travels
// with it so no home remains claimed.  The stateless ZCode binding is
// released the same way: the product-owned staging tree and its single
// plugins.dirs entry go away together.

// Event-bound legacy shapes. The retired installer wrote each script under
// exactly these events and always placed the script as the single process
// argument.
const MANAGED_HOOK_EVENTS = {
  PreToolUse: new Map([
    ['guard-bash.mjs', 'Bash'],
    ['check-agent-files.mjs', '^(Read|Grep|Glob|Write|Edit|Delete|Move)$'],
  ]),
  PostToolUse: new Map([['audit-bash-result.mjs', 'Bash']]),
  PostToolUseFailure: new Map([['audit-bash-result.mjs', 'Bash']]),
};
const PROVENANCE_WRAPPER_KEYS = [
  'effective_guard_wrapper_path',
  'effective_audit_wrapper_path',
  'effective_file_wrapper_path',
];
const POLICY_VERIFIER_NAME = 'external-subagent-policy-verifier';

function exactWrapperPaths(provenance) {
  const paths = new Set();
  if (!provenance || typeof provenance !== 'object' || Array.isArray(provenance)) return paths;
  for (const key of PROVENANCE_WRAPPER_KEYS) {
    const value = provenance[key];
    if (typeof value === 'string' && value.length > 0) paths.add(path.resolve(value));
  }
  return paths;
}

// A managed entry is exactly the installer's written shape: a process hook
// whose script position (`args[0]`) is either a provenance-recorded exact
// wrapper path or an event-bound legacy `<plugin root>/hooks/<managed
// script>` with the matching matcher. Arguments after the script position
// are data, never executable positions.
function managedHookEntry(entry, exactPaths, managedScripts) {
  if (!entry || typeof entry !== 'object' || !Array.isArray(entry.hooks)) return false;
  const hooks = entry.hooks.filter((hook) => hook?.type === 'process' && Array.isArray(hook.args));
  for (const hook of hooks) {
    const script = hook.args[0];
    if (typeof script === 'string' && exactPaths.has(path.resolve(script))) return true;
  }
  if (typeof entry.matcher !== 'string') return false;
  for (const hook of hooks) {
    const script = hook.args[0];
    if (typeof script !== 'string') continue;
    const expectedMatcher = managedScripts.get(path.basename(script));
    if (expectedMatcher !== undefined
      && path.basename(path.dirname(script)) === 'hooks'
      && entry.matcher === expectedMatcher) {
      return true;
    }
  }
  return false;
}

// Only a genuinely absent record routes to the empty path. A real I/O read
// failure propagates so the caller treats the config phase as failed and
// retains the exact-path evidence a retry needs. An unparseable record cannot
// contribute exact paths, so it is treated as absent.
function readProvenance(file) {
  if (typeof file !== 'string') return {};
  const bytes = readOptional(file);
  if (bytes === null) return {};
  try {
    const doc = JSON.parse(bytes.toString('utf8'));
    return doc && typeof doc === 'object' && !Array.isArray(doc) ? doc : {};
  } catch {
    return {};
  }
}

function removeFile(file) {
  try {
    fs.unlinkSync(file);
    return true;
  } catch (error) {
    if (error?.code === 'ENOENT') return false;
    throw error;
  }
}

// Inverse of the retired hook installer, bounded to the references it
// actually wrote: recognized managed entries are removed from the three
// event lists, while every foreign entry, unknown hooks key, and unrelated
// top-level key stays untouched.  The write is atomic.  Afterwards the
// provenance record and the verifier staged beside it are deleted — but only
// when the config phase completed, so a failed read/parse/write retains the
// exact-path evidence a retry needs.
function removeManagedHooks(paths) {
  const configPath = paths.zcodeConfig;
  const provenancePath = paths.hookProvenance;
  const result = {
    config: configPath ?? null,
    entries_removed: 0,
    config_updated: false,
    provenance_removed: false,
    verifier_removed: false,
  };
  let configPhaseFailed = false;
  try {
    const exactPaths = exactWrapperPaths(readProvenance(provenancePath));
    const bytes = typeof configPath === 'string' ? readOptional(configPath) : null;
    if (bytes !== null) {
      const config = JSON.parse(bytes.toString('utf8'));
      const events = config?.hooks && typeof config.hooks === 'object' && !Array.isArray(config.hooks)
        ? config.hooks.events
        : undefined;
      if (events && typeof events === 'object' && !Array.isArray(events)) {
        let removed = 0;
        for (const [event, managedScripts] of Object.entries(MANAGED_HOOK_EVENTS)) {
          const entries = events[event];
          if (!Array.isArray(entries)) continue;
          const kept = entries.filter((entry) => !managedHookEntry(entry, exactPaths, managedScripts));
          removed += entries.length - kept.length;
          if (kept.length !== entries.length) events[event] = kept;
        }
        if (removed > 0) {
          atomicWrite(configPath, jsonBytes(config));
          result.entries_removed = removed;
          result.config_updated = true;
        }
      }
    }
  } catch (error) {
    configPhaseFailed = true;
    result.error = { code: error?.code || 'ZCODE_HOOKS_CLEANUP_FAILED', message: error.message };
  }
  if (!configPhaseFailed) {
    try {
      if (typeof provenancePath === 'string') result.provenance_removed = removeFile(provenancePath);
    } catch (error) {
      result.error ??= { code: error?.code || 'ZCODE_HOOKS_CLEANUP_FAILED', message: error.message };
    }
    try {
      if (typeof provenancePath === 'string') {
        result.verifier_removed = removeFile(path.join(path.dirname(provenancePath), POLICY_VERIFIER_NAME));
      }
    } catch (error) {
      result.error ??= { code: error?.code || 'ZCODE_HOOKS_CLEANUP_FAILED', message: error.message };
    }
  }
  return result;
}

export function uninstall(paths, options = {}) {
  const removed = removeService(paths, options);
  const released = unregisterAllCodexHomes(paths);
  // An unreadable ZCode config is foreign state, not a product claim: report
  // the release failure instead of failing the whole uninstall over it.
  let zcode;
  try { zcode = uninstallZcodePlugin(paths); } catch (error) {
    zcode = { uninstalled: false, error: { code: error.code || 'ZCODE_BINDING_FAILED', message: error.message } };
  }
  // DSH bindings are released by scanning every profile's patch sentinel;
  // the same per-item reporting posture as the codex homes release.
  let dsh;
  try { dsh = uninstallAllDshBindings(paths); } catch (error) {
    dsh = { released: 0, error: { code: error.code || 'DSH_BINDING_RELEASE_FAILED', message: error.message } };
  }
  // Retained installations may still carry managed hooks pointing at the
  // removed scripts.  Cleanup is bounded to recognized entries and reports
  // its own failure without failing the rest of the uninstall.
  const zcode_hooks = removeManagedHooks(paths);
  return { ...removed, codex_homes_unregistered: released.unregistered, zcode_binding: zcode, dsh_bindings: dsh, zcode_hooks };
}

export { backupData, purge, restoreData };
