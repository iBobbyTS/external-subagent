import { backupData, purge, restoreData, uninstall as removeService } from '../maintenance.mjs';
import { unregisterAllCodexHomes } from '../install/reconcile.mjs';
import { uninstallZcodePlugin } from '../install/zcode.mjs';
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

const MANAGED_HOOK_EVENTS = ['PreToolUse', 'PostToolUse', 'PostToolUseFailure'];
const MANAGED_HOOK_MATCHERS = new Map([
  ['guard-bash.mjs', 'Bash'],
  ['audit-bash-result.mjs', 'Bash'],
  ['check-agent-files.mjs', '^(Read|Grep|Glob|Write|Edit|Delete|Move)$'],
]);
const PROVENANCE_WRAPPER_KEYS = [
  'effective_guard_wrapper_path',
  'effective_audit_wrapper_path',
  'effective_file_wrapper_path',
];
const POLICY_VERIFIER_NAME = 'external-subagent-policy-verifier';

function processHookArgs(entry) {
  if (!entry || !Array.isArray(entry.hooks)) return [];
  return entry.hooks
    .filter((hook) => hook?.type === 'process' && Array.isArray(hook.args))
    .flatMap((hook) => hook.args)
    .filter((arg) => typeof arg === 'string');
}

function exactWrapperPaths(provenance) {
  const paths = new Set();
  if (!provenance || typeof provenance !== 'object' || Array.isArray(provenance)) return paths;
  for (const key of PROVENANCE_WRAPPER_KEYS) {
    const value = provenance[key];
    if (typeof value === 'string' && value.length > 0) paths.add(path.resolve(value));
  }
  return paths;
}

// A managed entry is either one the provenance recorded byte-for-byte, or a
// legacy shape written by the retired installer: a process hook whose args
// point at `<any plugin root>/hooks/<managed script>`.  The matcher must
// match the script's event role so a foreign entry that reuses a name is
// left alone.
function managedHookEntry(entry, exactPaths) {
  if (!entry || typeof entry !== 'object' || typeof entry.matcher !== 'string') return false;
  for (const arg of processHookArgs(entry)) {
    if (exactPaths.has(path.resolve(arg))) return true;
    const script = path.basename(arg).toLowerCase();
    const expectedMatcher = MANAGED_HOOK_MATCHERS.get(script);
    if (expectedMatcher !== undefined
      && path.basename(path.dirname(arg)) === 'hooks'
      && entry.matcher === expectedMatcher) {
      return true;
    }
  }
  return false;
}

function readProvenance(file) {
  if (typeof file !== 'string') return {};
  try {
    const bytes = readOptional(file);
    if (bytes === null) return {};
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
// provenance record and the verifier staged beside it are deleted.
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
  try {
    const exactPaths = exactWrapperPaths(readProvenance(provenancePath));
    const bytes = typeof configPath === 'string' ? readOptional(configPath) : null;
    if (bytes !== null) {
      const config = JSON.parse(bytes.toString('utf8'));
      const events = config?.hooks && typeof config.hooks === 'object' && !Array.isArray(config.hooks)
        ? config.hooks.events
        : undefined;
      if (events && typeof events === 'object' && !Array.isArray(events)) {
        for (const event of MANAGED_HOOK_EVENTS) {
          const entries = events[event];
          if (!Array.isArray(entries)) continue;
          const kept = entries.filter((entry) => !managedHookEntry(entry, exactPaths));
          result.entries_removed += entries.length - kept.length;
          if (kept.length !== entries.length) events[event] = kept;
        }
        if (result.entries_removed > 0) {
          atomicWrite(configPath, jsonBytes(config));
          result.config_updated = true;
        }
      }
    }
  } catch (error) {
    result.error = { code: error?.code || 'ZCODE_HOOKS_CLEANUP_FAILED', message: error.message };
  }
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
  // Retained installations may still carry managed hooks pointing at the
  // removed scripts.  Cleanup is bounded to recognized entries and reports
  // its own failure without failing the rest of the uninstall.
  const zcode_hooks = removeManagedHooks(paths);
  return { ...removed, codex_homes_unregistered: released.unregistered, zcode_binding: zcode, zcode_hooks };
}

export { backupData, purge, restoreData };
