import fs from 'node:fs';
import path from 'node:path';
import { nativeBinary } from './layout.mjs';
import { platform } from '../paths.mjs';

// The launchd/GUI and systemd/user-manager environments do not inherit the
// interactive shell PATH.  Every managed entry point therefore resolves to an
// absolute file inside the installed package, and the fixed PATHs below are
// only what the daemon itself may use to locate system tools.  This module
// never writes user profiles; PATH findings are reported so an explicit
// repair stays a user decision.
export const LAUNCHD_FIXED_PATH = '/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin';
// The Linux fixed set covers systemctl and the payload's system-tool
// dependencies.  systemdServicePath() below appends the interpreter directory
// of the Node that rendered the unit, so a persisted shim-style subagent
// runtime (an nvm-installed Codex launcher) resolves `node` from the service
// environment instead of relying on the interactive shell PATH.
export const SYSTEMD_FIXED_PATH = '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin';

// The Linux service PATH is the fixed system-tool set plus the interpreter
// directory of the node that rendered the unit.  The persisted Codex runtime
// is commonly an nvm-installed Node launcher whose `#!/usr/bin/env node`
// shebang resolves `node` from PATH; without that directory the
// systemd-managed daemon cannot execute it.  This mirrors the macOS fixed PATH
// carrying the Homebrew bin that holds its own `node`.  The appended directory
// is absolute and newline-free by construction, so the unit generator's
// assertUnitLineSafe admits it unconditionally.
export function systemdServicePath() {
  return `${SYSTEMD_FIXED_PATH}:${path.dirname(process.execPath)}`;
}

export function fixedServicePath(env = process.env) {
  return platform(env) === 'darwin' ? LAUNCHD_FIXED_PATH : systemdServicePath();
}

export function which(name, env = process.env) {
  const searchPath = (env.PATH || '').split(path.delimiter).filter(Boolean);
  for (const dir of searchPath) {
    const candidate = path.join(dir, name);
    try {
      const stat = fs.statSync(candidate);
      if (stat.isFile() && (stat.mode & 0o111) !== 0) return candidate;
    } catch { /* keep scanning */ }
  }
  return null;
}

export function pathReport(options = {}) {
  const env = options.env || process.env;
  const daemon = nativeBinary('external-subagentd');
  const facade = nativeBinary('external-subagent-mcp');
  const darwin = platform(env) === 'darwin';
  return {
    shell: {
      path: env.PATH || null,
      cli_on_path: which('external-subagent', env),
      mcp_on_path: which('external-subagent-mcp', env),
    },
    // The fixed service-environment PATH, keyed by the backend that owns it
    // so the report never calls a systemd unit "launchd".
    ...(darwin
      ? { launchd: { path: LAUNCHD_FIXED_PATH, daemon_entry: daemon, mcp_entry: facade } }
      : { systemd: { path: systemdServicePath(), daemon_entry: daemon, mcp_entry: facade } }),
    stable_entries_absolute: Boolean(daemon && facade && path.isAbsolute(daemon) && path.isAbsolute(facade)),
    shell_profile_writes: 'none',
    node_executable: process.execPath,
  };
}
