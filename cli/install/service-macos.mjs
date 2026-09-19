import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { DAEMON_BIN_NAME, LAUNCH_AGENT_LABEL, ZCODE_RUNTIME } from '../constants.mjs';
import { CliError } from '../errors.mjs';
import { atomicWrite } from '../fs-atomic.mjs';
import { nativeBinary } from './layout.mjs';
import { LAUNCHD_FIXED_PATH } from './path.mjs';
import { readConfig } from '../config/read.mjs';

// The one macOS service template for the product daemon.  The plist pins
// absolute payload paths and a fixed PATH so the GUI/launchd environment can
// never depend on the interactive shell.  Launchctl activation honors the
// EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL seam so fixture runs never load real
// services; everything else still writes real files.

function escapeXml(value) {
  return value.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
}

export function launchAgentPlist(paths, options = {}) {
  const daemon = nativeBinary(DAEMON_BIN_NAME);
  const config = readConfig(paths.config);
  const { runtime_path: dshRuntime, home: dshHome, profile: dshProfile, version: dshVersion } = config.subagents.dsh;
  const { runtime_path: codexRuntime, home: codexHome } = config.subagents.codex;
  const configRevision = config.revision;
  // AUD-005/D1: the standalone service never depends on an unrelated runtime.
  // The pinned ZCode runtime is forwarded only when that installation exists;
  // without it the daemon still starts and the zcode adapter fails closed at
  // spawn ("ZCODE_RUNTIME_PATH is unavailable") instead of the whole service
  // dying on launchd because --runtime cannot canonicalize.  Installing (or
  // removing) ZCode later is picked up by the next init, which rewrites the
  // plist.  The seam exists so tests can pin both branches deterministically.
  const zcodeRuntime = options.zcodeRuntime ?? ZCODE_RUNTIME;
  const programArguments = [
    `<string>${escapeXml(daemon)}</string>`,
    '<string>--database</string>',
    `<string>${escapeXml(paths.database)}</string>`,
    '<string>--socket</string>',
    `<string>${escapeXml(paths.socket)}</string>`,
    ...(fs.existsSync(zcodeRuntime)
      ? ['<string>--runtime</string>', `<string>${escapeXml(zcodeRuntime)}</string>`]
      : []),
    '<string>--diagnostic-log</string>',
    `<string>${escapeXml(path.join(paths.logs, 'daemon-error.log'))}</string>`,
  ];
  const dshEnvironment = [
    `<key>PATH</key><string>${LAUNCHD_FIXED_PATH}</string>`,
    ...(configRevision === null ? [] : [`<key>EXTERNAL_SUBAGENT_CONFIG_REVISION</key><string>${configRevision}</string>`]),
    ...(dshRuntime ? [`<key>DSH_RUNTIME_PATH</key><string>${escapeXml(dshRuntime)}</string>`] : []),
    ...(dshHome ? [`<key>DSH_HOME</key><string>${escapeXml(dshHome)}</string>`] : []),
    ...(dshProfile ? [`<key>DSH_PROFILE</key><string>${escapeXml(dshProfile)}</string>`] : []),
    ...(dshVersion ? [`<key>DSH_VERSION</key><string>${escapeXml(dshVersion)}</string>`] : []),
    // The persisted Codex runtime/home pair is forwarded so the service-side
    // daemon resolves the same launch contract the interactive one does. The
    // home is only exported when configured; an inherited launchd CODEX_HOME
    // (if any) stays the second-priority source, and neither being present
    // fails Codex admission closed instead of falling back to ~/.codex.
    ...(codexRuntime ? [`<key>CODEX_RUNTIME_PATH</key><string>${escapeXml(codexRuntime)}</string>`] : []),
    ...(codexHome ? [`<key>CODEX_HOME</key><string>${escapeXml(codexHome)}</string>`] : []),
  ].join('');
  return Buffer.from(`<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0"><dict>\n<key>Label</key><string>${LAUNCH_AGENT_LABEL}</string>\n<key>ProgramArguments</key><array>${programArguments.join('')}</array>\n<key>EnvironmentVariables</key><dict>${dshEnvironment}</dict>\n<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>\n<key>StandardOutPath</key><string>${escapeXml(path.join(paths.logs, 'daemon.log'))}</string>\n<key>StandardErrorPath</key><string>${escapeXml(path.join(paths.logs, 'daemon-error.log'))}</string>\n</dict></plist>\n`);
}

export function installLaunchAgent(paths, options = {}) {
  const plist = options.plist || launchAgentPlist(paths);
  atomicWrite(paths.launchAgent, plist, 0o600);
  return { installed: true, path: paths.launchAgent, label: LAUNCH_AGENT_LABEL };
}

export function launchctl(args) {
  if (process.env.EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL === '1') {
    return { action: args[0], skipped: true, reason: 'launchd neutralized by test seam' };
  }
  const result = spawnSync('/bin/launchctl', args, { encoding: 'utf8' });
  if (args[0] === 'print' && result.status === 113) return { action: 'print', absent: true };
  if (result.error || result.status !== 0) {
    throw new CliError('DAEMON_CONTROL_FAILED', (result.stderr || result.error?.message || 'launchctl failed').trim());
  }
  return { action: args[0], status: result.status, stdout: result.stdout };
}

// launchd answers a bootstrap whose label is already loaded in the target
// domain with "Bootstrap failed: 5: Input/output error" (reproduced live on
// this GUI session; the same EIO was logged by the first acceptance run).
// A repeat init/start must stay idempotent instead of failing the whole run,
// so the registration lookup below — the same print/absent discriminator the
// update owner uses — decides between "already loaded" and a real failure.
export function serviceRegistrationStatus(uid = process.getuid(), options = {}) {
  const control = options.launchctl || launchctl;
  const result = control(['print', `gui/${uid}/${LAUNCH_AGENT_LABEL}`]);
  if (result.skipped) return { query: 'skipped', registered: null, reason: result.reason };
  if (result.absent) return { registered: false };
  const pid = Number(result.stdout?.match(/\bpid = (\d+)/)?.[1]);
  const state = result.stdout?.match(/^\s*state = (\S+)$/m)?.[1] ?? null;
  return { registered: true, state, pid: Number.isInteger(pid) && pid > 0 ? pid : null };
}

export function bootstrapService(paths, uid = process.getuid(), options = {}) {
  const control = options.launchctl || launchctl;
  const alreadyLoaded = (lookup) => ({
    action: 'bootstrap', label: LAUNCH_AGENT_LABEL, already_loaded: true,
    state: lookup.state ?? null, pid: lookup.pid ?? null,
  });
  const existing = serviceRegistrationStatus(uid, { launchctl: control });
  if (existing.registered) return alreadyLoaded(existing);
  try {
    return control(['bootstrap', `gui/${uid}`, paths.launchAgent]);
  } catch (error) {
    // Another loader (a racing init, or launchd itself re-reading the plist)
    // may have won after the check above; confirm before reporting failure.
    const loaded = serviceRegistrationStatus(uid, { launchctl: control });
    if (loaded.registered) return alreadyLoaded(loaded);
    throw error;
  }
}

// A bootout call returns before launchd has necessarily finished tearing the
// job down, and a bootstrap issued inside that window can be swept away by the
// still-running removal (observed live: rapid stop→start cycles left the
// service unregistered roughly one time in three).  The stop owner therefore
// confirms the job is actually gone — bounded, with the same print/absent
// discriminator the update owner's unload uses — before reporting success.
export function bootoutService(paths, uid = process.getuid(), options = {}) {
  const control = options.launchctl || launchctl;
  const existing = serviceRegistrationStatus(uid, { launchctl: control });
  if (!existing.registered) return { action: 'bootout', label: LAUNCH_AGENT_LABEL, already_stopped: true };
  let result;
  try {
    result = control(['bootout', `gui/${uid}/${LAUNCH_AGENT_LABEL}`]);
  } catch (error) {
    // A racing removal (another stop, or launchd itself) may have taken the
    // job out between the registration probe and the bootout; confirm before
    // reporting failure, so a repeated stop stays idempotent.
    const settled = serviceRegistrationStatus(uid, { launchctl: control });
    if (!settled.registered) return { action: 'bootout', label: LAUNCH_AGENT_LABEL, already_stopped: true };
    throw error;
  }
  const deadline = Date.now() + (options.unloadTimeoutMs ?? 10_000);
  for (;;) {
    const probe = serviceRegistrationStatus(uid, { launchctl: control });
    if (!probe.registered) return { ...result, label: LAUNCH_AGENT_LABEL, removed: true };
    if (Date.now() >= deadline) throw new CliError('SERVICE_UNLOAD_TIMEOUT', 'launchd service remained registered after bootout');
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 50);
  }
}
