import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { LAUNCH_AGENT_LABEL, ZCODE_RUNTIME } from '../constants.mjs';
import { CliError } from '../errors.mjs';
import { atomicWrite } from '../fs-atomic.mjs';
import { nativeBinary } from './layout.mjs';
import { LAUNCHD_FIXED_PATH } from './path.mjs';

// The one macOS service template for the product daemon.  The plist pins
// absolute payload paths and a fixed PATH so the GUI/launchd environment can
// never depend on the interactive shell.  Launchctl activation honors the
// EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL seam so fixture runs never load real
// services; everything else still writes real files.

function escapeXml(value) {
  return value.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
}

export function launchAgentPlist(paths) {
  const daemon = nativeBinary('external-subagentd');
  const dshRuntime = process.env.DSH_RUNTIME_PATH;
  const dshHome = process.env.DSH_HOME;
  const dshEnvironment = [
    `<key>PATH</key><string>${LAUNCHD_FIXED_PATH}</string>`,
    ...(dshRuntime ? [`<key>DSH_RUNTIME_PATH</key><string>${escapeXml(dshRuntime)}</string>`] : []),
    ...(dshHome ? [`<key>DSH_HOME</key><string>${escapeXml(dshHome)}</string>`] : []),
  ].join('');
  return Buffer.from(`<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0"><dict>\n<key>Label</key><string>${LAUNCH_AGENT_LABEL}</string>\n<key>ProgramArguments</key><array><string>${escapeXml(daemon)}</string><string>--database</string><string>${escapeXml(paths.database)}</string><string>--socket</string><string>${escapeXml(paths.socket)}</string><string>--runtime</string><string>${escapeXml(ZCODE_RUNTIME)}</string><string>--diagnostic-log</string><string>${escapeXml(path.join(paths.logs, 'daemon-error.log'))}</string></array>\n<key>EnvironmentVariables</key><dict>${dshEnvironment}</dict>\n<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>\n<key>StandardOutPath</key><string>${escapeXml(path.join(paths.logs, 'daemon.log'))}</string>\n<key>StandardErrorPath</key><string>${escapeXml(path.join(paths.logs, 'daemon-error.log'))}</string>\n</dict></plist>\n`);
}

export function installLaunchAgent(paths, options = {}) {
  const plist = options.plist || launchAgentPlist(paths);
  atomicWrite(paths.launchAgent, plist, 0o600);
  return { installed: true, path: paths.launchAgent, label: LAUNCH_AGENT_LABEL };
}

function launchctl(args) {
  if (process.env.EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL === '1') {
    return { action: args[0], skipped: true, reason: 'launchd neutralized by test seam' };
  }
  const result = spawnSync('/bin/launchctl', args, { encoding: 'utf8' });
  if (result.error || result.status !== 0) {
    throw new CliError('DAEMON_CONTROL_FAILED', (result.stderr || result.error?.message || 'launchctl failed').trim());
  }
  return { action: args[0], status: result.status };
}

export function bootstrapService(paths, uid = process.getuid()) {
  return launchctl(['bootstrap', `gui/${uid}`, paths.launchAgent]);
}

export function bootoutService(paths, uid = process.getuid()) {
  return launchctl(['bootout', `gui/${uid}/${LAUNCH_AGENT_LABEL}`]);
}
