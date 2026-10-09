import fs from 'node:fs';
import { bootoutService, bootstrapService } from '../install/service-macos.mjs';
import { loadCodexHomes } from '../install/reconcile.mjs';
import { verifyPayload } from '../install/payload.mjs';
import { platform } from '../paths.mjs';

// Daemon/service command surface: explicit start/stop through the managed
// service definition, and the local installation summary consumed by `status`.

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
  // The service-definition path is the macOS LaunchAgent plist or the Linux
  // systemd user unit (paths.mjs).  The reported field stays platform-truthful
  // so a Linux install never reads as a launchd agent; the Linux service view
  // itself is wired by S03.
  const serviceDefinition = platform() === 'darwin'
    ? { launch_agent: fs.existsSync(paths.launchAgent) }
    : { service_definition: fs.existsSync(paths.launchAgent) };
  return {
    installed: fs.existsSync(paths.state),
    ...serviceDefinition,
    data: fs.existsSync(paths.data),
    payload: publicPayload,
    // Home paths are installation internals; status reports only the
    // aggregate binding state. Detailed paths remain available to diagnose.
    codex_homes: registry.registry.homes.map((entry) => ({ version: entry.version, last_status: entry.last_status })),
    ...(verbose && registry.recovery ? { codex_homes_recovery: registry.recovery } : {}),
  };
}
