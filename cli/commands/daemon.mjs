import fs from 'node:fs';
import { bootoutService, bootstrapService } from '../install/service-macos.mjs';
import { loadCodexHomes } from '../install/reconcile.mjs';
import { verifyPayload } from '../install/payload.mjs';

// Daemon/service command surface: explicit start/stop through the managed
// LaunchAgent, and the local installation summary consumed by `status`.

export function startDaemon(paths) {
  return bootstrapService(paths);
}

export function stopDaemon(paths) {
  return bootoutService(paths);
}

export function localInstallStatus(paths, { verbose = false } = {}) {
  let payload = { status: 'unverified' };
  try { payload = verifyPayload(); } catch (error) {
    payload = { status: 'invalid', error: { code: error.code || 'PAYLOAD_INVALID', message: error.message } };
  }
  const registry = loadCodexHomes(paths);
  const publicPayload = verbose
    ? payload
    : { status: payload.status, platform: payload.platform, version: payload.version };
  return {
    installed: fs.existsSync(paths.state),
    launch_agent: fs.existsSync(paths.launchAgent),
    data: fs.existsSync(paths.data),
    payload: publicPayload,
    // Home paths are installation internals; status reports only the
    // aggregate binding state. Detailed paths remain available to diagnose.
    codex_homes: registry.registry.homes.map((entry) => ({ version: entry.version, last_status: entry.last_status })),
    ...(verbose && registry.recovery ? { codex_homes_recovery: registry.recovery } : {}),
  };
}
