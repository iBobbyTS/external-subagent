import os from 'node:os';
import path from 'node:path';
import { LAUNCH_AGENT_LABEL, PRODUCT_NAME } from './constants.mjs';
import { PLUGIN_NAME } from './install/layout.mjs';

export function platform() {
  return process.env.EXTERNAL_SUBAGENT_TEST_PLATFORM || process.platform;
}

// State paths derive from PRODUCT_NAME, so the debug variant gets a fully
// parallel tree (its own data directory, database, socket, and logs) and
// never touches the released product's state.
export function productPaths(home = os.homedir()) {
  const data = path.join(home, 'Library', 'Application Support', PRODUCT_NAME);
  return {
    home,
    data,
    config: path.join(data, 'config.json'),
    state: path.join(data, 'install-state.json'),
    hookProvenance: path.join(data, 'zcode-agent-hook-provenance.json'),
    database: path.join(data, `${PRODUCT_NAME}.sqlite3`),
    socket: path.join(data, `${PRODUCT_NAME}.sock`),
    logs: path.join(home, 'Library', 'Logs', PRODUCT_NAME),
    launchAgent: path.join(home, 'Library', 'LaunchAgents', `${LAUNCH_AGENT_LABEL}.plist`),
    zcodeConfig: path.join(home, '.zcode', 'cli', 'config.json'),
    zcodePlugin: path.join(data, 'zcode-plugin', PLUGIN_NAME),
    profiles: path.join(data, 'profiles'),
  };
}

// Mirror the daemon's `profiles_directory()` exactly (rpc/profiles.rs):
// `EXTERNAL_SUBAGENT_CONFIG` wins when it is *exported*, even when exported as
// an empty string, and only then does `ZCODE_AGENT_CONFIG` apply; presence, not
// truthiness, selects the variable (`var_os(...).or_else(...)`). An empty
// exported path has no parent in Rust (`Path::new("").parent() == None`), so the
// daemon loads no profiles directory at all; return null to represent that.
// Otherwise take the sibling `profiles/` of the config file, or fall back to the
// product data directory.

// Rust `Path::parent()` semantics on Unix, implemented with plain string
// slicing (never `path.dirname`/`path.join`, which lexically normalise and would
// fold `..` components away). Trailing separators are ignored, a bare relative
// name has the empty path as parent (`Some("")`), and a path consisting only of
// separators (e.g. "/") has no parent (`None`).
function rustPathParent(value) {
  if (value === '') return null;
  let end = value.length;
  while (end > 1 && value[end - 1] === '/') end -= 1;
  const trimmed = value.slice(0, end);
  if (/^\/+$/u.test(trimmed)) return null; // root has no parent
  const slash = trimmed.lastIndexOf('/');
  if (slash < 0) return ''; // bare name: parent is Some("")
  if (slash === 0) return '/';
  return trimmed.slice(0, slash);
}

// Rust `Path::parent().join("profiles")` without normalisation: appending to the
// empty path yields "profiles", and existing `..`/`.` components are preserved.
function joinProfiles(parent) {
  if (parent === '') return 'profiles';
  if (parent === '/') return '/profiles';
  return `${parent}/profiles`;
}

export function profilesDir(env = {}, home = os.homedir()) {
  const names = ['EXTERNAL_SUBAGENT_CONFIG', 'ZCODE_AGENT_CONFIG'];
  let envPath;
  let exported = false;
  for (const name of names) {
    if (env[name] !== undefined) {
      envPath = env[name];
      exported = true;
      break;
    }
  }
  if (!exported) return path.join(productPaths(home).data, 'profiles');
  const parent = rustPathParent(envPath);
  if (parent === null) return null;
  return joinProfiles(parent);
}
