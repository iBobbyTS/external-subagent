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

// Windows base directory resolution (S04): %LOCALAPPDATA% is used only when it is a
// non-empty absolute path; relative or empty values are ignored and the fallback
// under %USERPROFILE%\AppData\Local applies. APPDATA (Roaming) is deliberately
// not consulted as the frozen field set has no roaming consumers.
function localAppDataDirectory(env, home) {
  const configured = env.LOCALAPPDATA;
  if (typeof configured === 'string' && configured !== '' && path.isAbsolute(configured)) {
    return configured;
  }
  return path.join(home, 'AppData', 'Local');
}

// Windows product layout (S04): structurally parallel to darwin/linux.
// Data, database, config, state, hook provenance, plugin staging, and profiles
// live under %LOCALAPPDATA%\<product>.
// Socket uses the Windows named pipe namespace (\\.\pipe\<product>-daemon).
// launchAgent is null as Windows uses SCM rather than a file-based unit definition.
function windowsProductPaths(home, env) {
  const localAppData = localAppDataDirectory(env, home);
  const data = path.join(localAppData, PRODUCT_NAME);
  return {
    home,
    data,
    config: path.join(data, 'config.json'),
    state: path.join(data, 'install-state.json'),
    hookProvenance: path.join(data, 'zcode-agent-hook-provenance.json'),
    database: path.join(data, `${PRODUCT_NAME}.sqlite3`),
    // Named pipe namespace placeholder (pending phase 1 real-machine implementation,
    // not a filesystem path; prefix matches S03 windows.rs \\.\pipe\ literal).
    socket: `\\\\.\\pipe\\${PRODUCT_NAME}-daemon`,
    logs: path.join(localAppData, PRODUCT_NAME, 'logs'),
    // Windows has no file-based service definition; pending phase 3 SCM integration decision.
    launchAgent: null,
    zcodeConfig: path.join(home, '.zcode', 'cli', 'config.json'),
    zcodePlugin: path.join(data, 'zcode-plugin', PLUGIN_NAME),
    profiles: path.join(data, 'profiles'),
  };
}

// State paths derive from PRODUCT_NAME, so the debug variant gets a fully
// parallel tree (its own data directory, database, socket, and logs) and
// never touches the released product's state.  The layout is selected by the
// process platform: macOS `~/Library`, Windows `%LOCALAPPDATA%`, every other
// supported host the XDG baseline.  `zcodeConfig` is `~/.zcode/cli/config.json`.
export function productPaths(home = os.homedir(), env = process.env) {
  const currentPlatform = platform(env);
  if (currentPlatform === 'darwin') return darwinProductPaths(home);
  if (currentPlatform === 'win32') return windowsProductPaths(home, env);
  return linuxProductPaths(home, env);
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
function rustPathParentUnix(value) {
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

// ---------------------------------------------------------------------------
// Windows twin of the two helpers above.
//
// Rust compiles `std::path` against the TARGET platform's grammar, so the
// daemon running on Windows reads config paths as Win32 paths: both `\` and
// `/` are separators, a Disk/UNC/Verbatim prefix is part of the path, and
// `Path::parent()` never returns a parent for a root.  The functions below are
// derived case-by-case from real `Path::parent()` / `Path::join("profiles")`
// output on Windows (see tests/platform/windows-paths.test.mjs); they preserve
// the original bytes (mixed separators included) and never lexically fold
// `..`, exactly like the Unix implementation above.
//
// The Unix helpers stay untouched: `profilesDir` selects the pair by the same
// process-platform seam that selects the product layout.
function isWindowsSeparator(char) {
  return char === '\\' || char === '/';
}

// Prefix grammar (the subset the product can encounter):
//   VerbatimDisk   \\?\C:
//   VerbatimUNC    \\?\UNC\server\share
//   Verbatim       \\?\...
//   UNC            \\server\share        (requires both components)
//   Disk           C:
// A path with fewer UNC components (`\\server`, `//foo`) is NOT a prefix in
// Rust: the leading separator is the root instead.
function windowsPrefixEnd(value) {
  if (value.startsWith('\\\\?\\') || value.startsWith('//?/')) {
    let index = 4;
    if (value.slice(index, index + 3).toUpperCase() === 'UNC'
      && index + 3 < value.length && isWindowsSeparator(value[index + 3])) {
      index += 4;
      // server
      while (index < value.length && !isWindowsSeparator(value[index])) index += 1;
      while (index < value.length && isWindowsSeparator(value[index])) index += 1;
      // share
      while (index < value.length && !isWindowsSeparator(value[index])) index += 1;
      return index;
    }
    if (index + 1 < value.length && /[A-Za-z]/u.test(value[index]) && value[index + 1] === ':') {
      return index + 2;
    }
    return 4;
  }
  if (value.length >= 2 && isWindowsSeparator(value[0]) && isWindowsSeparator(value[1])) {
    let index = 2;
    const serverStart = index;
    while (index < value.length && !isWindowsSeparator(value[index])) index += 1;
    if (index === serverStart) return 0; // repeated separator: root-relative
    while (index < value.length && isWindowsSeparator(value[index])) index += 1;
    const shareStart = index;
    while (index < value.length && !isWindowsSeparator(value[index])) index += 1;
    if (index === shareStart) return 0; // no share: not a UNC prefix
    return index;
  }
  if (value.length >= 2 && /[A-Za-z]/u.test(value[0]) && value[1] === ':') return 2;
  return 0;
}

function windowsComponents(value, start) {
  const components = [];
  let index = start;
  while (index < value.length) {
    while (index < value.length && isWindowsSeparator(value[index])) index += 1;
    if (index >= value.length) break;
    const componentStart = index;
    while (index < value.length && !isWindowsSeparator(value[index])) index += 1;
    // Rust's Path::components drops a non-leading "." (a leading "." with no
    // prefix/root is CurDir and stays a component).
    if (value.slice(componentStart, index) === '.' && componentStart !== 0) continue;
    components.push({ start: componentStart, end: index });
  }
  return components;
}

function rustPathParentWindows(value) {
  if (value === '') return null;
  const prefixEnd = windowsPrefixEnd(value);
  let rootEnd = prefixEnd;
  if (rootEnd < value.length && isWindowsSeparator(value[rootEnd])) rootEnd += 1;
  const components = windowsComponents(value, rootEnd);
  if (components.length === 0) return null; // root, prefix-only, and empty paths
  if (components.length === 1) return rootEnd === 0 ? '' : value.slice(0, rootEnd);
  return value.slice(0, components[components.length - 2].end);
}

// Platform-selected Rust `Path::parent()` (the target platform's grammar, not
// the host JS module's).  Exported so the path contract is directly testable
// without going through the product layout.
export function rustPathParent(value, currentPlatform = process.platform) {
  return currentPlatform === 'win32' ? rustPathParentWindows(value) : rustPathParentUnix(value);
}

// Rust `Path::join` on Windows appends with `\`; a trailing separator (roots)
// and a drive-relative `C:` prefix append directly.
function joinProfilesWindows(parent) {
  if (parent === '') return 'profiles';
  if (/[\\/]$/u.test(parent) || /^[A-Za-z]:$/u.test(parent)) return `${parent}profiles`;
  return `${parent}\\profiles`;
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
  const windows = platform(env) === 'win32';
  const parent = rustPathParent(envPath, platform(env));
  if (parent === null) return null;
  return windows ? joinProfilesWindows(parent) : joinProfiles(parent);
}
