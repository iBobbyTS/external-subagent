import os from 'node:os';
import path from 'node:path';
import { LAUNCH_AGENT_LABEL, PRODUCT_NAME } from './constants.mjs';
import { PLUGIN_NAME } from './install/layout.mjs';

// The process platform, overridable by the EXTERNAL_SUBAGENT_TEST_PLATFORM seam
// so both layouts are exercisable on any host.  The seam keeps its original
// meaning: it is a *process platform* override, nothing else.
export function platform(env = process.env) {
  return env.EXTERNAL_SUBAGENT_TEST_PLATFORM || process.platform;
}

// macOS derives the whole tree from `~/Library`; the bytes of every field below
// are contractually frozen (existing installs must keep resolving their state).
function darwinProductPaths(home) {
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

// XDG Base Directory resolution (D-S02).  A variable selects its directory only
// when it is a non-empty absolute path; relative or empty values are ignored and
// the spec fallback under $HOME applies.  Only the three XDG variables the
// product owns are read — no other user XDG directory is probed.
function xdgDirectory(env, variable, home, fallback) {
  const configured = env[variable];
  if (typeof configured === 'string' && configured !== '' && path.isAbsolute(configured)) {
    return configured;
  }
  return path.join(home, ...fallback);
}

// Linux keeps the same internal relative layout as macOS (config, install
// state, database, socket, hook provenance, plugin staging, profiles all live
// inside the product data directory) so the two platforms stay structurally
// parallel.  Data and logs follow XDG; the systemd user unit directory is
// provided for the S03 service backend.
function linuxProductPaths(home, env) {
  const data = path.join(xdgDirectory(env, 'XDG_DATA_HOME', home, ['.local', 'share']), PRODUCT_NAME);
  const logs = path.join(xdgDirectory(env, 'XDG_STATE_HOME', home, ['.local', 'state']), PRODUCT_NAME);
  const configHome = xdgDirectory(env, 'XDG_CONFIG_HOME', home, ['.config']);
  return {
    home,
    data,
    config: path.join(data, 'config.json'),
    state: path.join(data, 'install-state.json'),
    hookProvenance: path.join(data, 'zcode-agent-hook-provenance.json'),
    database: path.join(data, `${PRODUCT_NAME}.sqlite3`),
    socket: path.join(data, `${PRODUCT_NAME}.sock`),
    logs,
    launchAgent: path.join(configHome, 'systemd', 'user', `${PRODUCT_NAME}.service`),
    zcodeConfig: path.join(home, '.zcode', 'cli', 'config.json'),
    zcodePlugin: path.join(data, 'zcode-plugin', PLUGIN_NAME),
    profiles: path.join(data, 'profiles'),
  };
}

// State paths derive from PRODUCT_NAME, so the debug variant gets a fully
// parallel tree (its own data directory, database, socket, and logs) and
// never touches the released product's state.  The layout is selected by the
// process platform: macOS `~/Library`, every other supported host the XDG
// baseline.  `zcodeConfig` is `~/.zcode/cli/config.json` on both platforms.
export function productPaths(home = os.homedir(), env = process.env) {
  return platform(env) === 'darwin' ? darwinProductPaths(home) : linuxProductPaths(home, env);
}

// Mirror the daemon's `profiles_directory()` exactly (rpc/profiles.rs):
// `EXTERNAL_SUBAGENT_CONFIG` wins when it is *exported*, even when exported as
// an empty string, and only then does `ZCODE_AGENT_CONFIG` apply; presence, not
// truthiness, selects the variable (`var_os(...).or_else(...)`). An empty
// exported path has no parent in Rust (`Path::new("").parent() == None`), so the
// daemon loads no profiles directory at all; return null to represent that.
// Otherwise take the sibling `profiles/` of the config file, or fall back to the
// product data directory (the daemon's data fallback is the same XDG/macOS data
// root selected here, so both sides resolve one directory).

// Rust `Path::parent()` semantics on Unix, implemented with plain string
// slicing (never `path.dirname`/`path.join`, which lexically normalise and would
// fold `..` components away). Rust's `Path::components` drops trailing
// separators and any `.` component that is not the first, so a trailing `.` is
// not a component of its own: `a/.` has parent `Some("")`, `a/b/.` has parent
// `Some("a")`, and `/.` is the root with no parent (`None`). `..` is retained.
function rustPathParent(value) {
  if (value === '') return null;
  let end = value.length;
  for (;;) {
    while (end > 1 && value[end - 1] === '/') end -= 1;
    const componentStart = value.lastIndexOf('/', end - 1) + 1;
    if (end - componentStart === 1 && value[componentStart] === '.' && componentStart > 0) {
      end = componentStart - 1; // drop the non-leading "." and its separator
      continue;
    }
    break;
  }
  const trimmed = value.slice(0, end);
  if (trimmed === '') return null; // "/." and friends normalise to the root
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
  if (!exported) return path.join(productPaths(home, env).data, 'profiles');
  const parent = rustPathParent(envPath);
  if (parent === null) return null;
  return joinProfiles(parent);
}
