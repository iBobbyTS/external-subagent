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
// dependencies; user-level runtimes (nvm, ~/.local) are never assumed — the
// service forwards configured runtimes as absolute paths instead.
export const SYSTEMD_FIXED_PATH = '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin';

export function fixedServicePath(env = process.env) {
  return platform(env) === 'darwin' ? LAUNCHD_FIXED_PATH : SYSTEMD_FIXED_PATH;
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
      : { systemd: { path: SYSTEMD_FIXED_PATH, daemon_entry: daemon, mcp_entry: facade } }),
    stable_entries_absolute: Boolean(daemon && facade && path.isAbsolute(daemon) && path.isAbsolute(facade)),
    shell_profile_writes: 'none',
    node_executable: process.execPath,
  };
}
