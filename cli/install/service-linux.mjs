import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { DAEMON_BIN_NAME, PRODUCT_NAME, ZCODE_RUNTIME } from '../constants.mjs';
import { CliError } from '../errors.mjs';
import { atomicWrite } from '../fs-atomic.mjs';
import { nativeBinary } from './layout.mjs';
import { systemdServicePath } from './path.mjs';
import { readConfig } from '../config/read.mjs';

// The one Linux service backend for the product daemon: a systemd USER unit,
// never the system manager.  The unit pins absolute payload paths and a fixed
// PATH so the user-manager environment never depends on the interactive shell,
// exactly the contract the launchd plist owns on macOS (service-macos.mjs is
// the structural twin; service-macos.mjs also performs the platform dispatch
// between the two backends).  Bootstrap = daemon-reload + enable --now; bootout
// = disable --now + a bounded inactive confirmation — the same idempotence and
// removal-confirmation semantics, carried by `systemctl --user` and honoring
// the EXTERNAL_SUBAGENT_TEST_NO_SYSTEMCTL seam (the structural twin of
// EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL) so fixture runs never touch the real
// user manager.

export const SYSTEMD_UNIT_NAME = `${PRODUCT_NAME}.service`;
export const SYSTEMCTL_PATH = '/usr/bin/systemctl';

// D-S04: a host without a reachable user manager gets an actionable
// explanation, never an automatic `loginctl enable-linger` and never a detour
// through the system manager.
const NO_SESSION_HINT = 'log in through a real session (which provides $XDG_RUNTIME_DIR), or run \'loginctl enable-linger <user>\' so the user manager stays available without one; this product never enables linger, never modifies unrelated units, and never contacts the system (PID 1) manager';

function noSessionError(detail = null) {
  const cause = detail ?? 'neither $XDG_RUNTIME_DIR nor $DBUS_SESSION_BUS_ADDRESS is set';
  return new CliError('NO_USER_SYSTEMD_SESSION', `systemctl --user cannot reach the systemd user manager: ${cause}. ${NO_SESSION_HINT}`);
}

export function hasUserSystemdSession(env = process.env) {
  return Boolean(
    (typeof env.XDG_RUNTIME_DIR === 'string' && env.XDG_RUNTIME_DIR !== '')
      || (typeof env.DBUS_SESSION_BUS_ADDRESS === 'string' && env.DBUS_SESSION_BUS_ADDRESS !== ''),
  );
}

// systemd's two expansion grammars differ, and both were verified against the
// running user manager (systemd 259):
//   Environment= values — NO variable expansion ("the '$' character has no
//     special meaning", man systemd.exec; a literal `$HOME` is passed through
//     verbatim), but %-specifier expansion IS performed (`%h` becomes the
//     home directory; `%%` is a literal `%`).
//   ExecStart arguments — BOTH expansions apply: `$`/`${}` at spawn time
//     (`$$` passes a literal dollar, man systemd.service) and %-specifiers at
//     load time (`%%` literal).
// Each side therefore escapes only what its grammar consumes; both quote the
// value with `\"`/`\\` per systemd.syntax (an unrecognized `\x` escape makes
// systemd DROP the whole Environment assignment, so every backslash is
// always doubled).  The `$`/`%` doublings use replacer functions because a
// plain `'$$'` replacement string is itself a $-pattern ("insert one $") and
// would silently cancel the escape.
function escapeQuotedCharacters(value) {
  return String(value)
    .replaceAll('\\', () => '\\\\')
    .replaceAll('"', () => '\\"');
}

function escapeEnvironmentValue(value) {
  return escapeQuotedCharacters(value).replaceAll('%', () => '%%');
}

function escapeExecStartArgument(value) {
  return escapeQuotedCharacters(value).replaceAll('$', () => '$$').replaceAll('%', () => '%%');
}

// A newline inside a unit directive cannot be escaped away: systemd ends the
// assignment there and parses the remainder as NEW directive lines, so a
// config value like "acp\nRestart=no\n#" silently overrides the product-fixed
// Restart=always — and systemd-analyze verify accepts the corrupted unit
// without a warning (reproduced with the real generator).  Both forwarding
// paths therefore refuse such values loudly instead of writing a corrupted
// unit; the macOS plist carries the same value as inert XML data and keeps
// forwarding it (its generator is deliberately untouched).
function assertUnitLineSafe(value, label) {
  if (/[\n\r]/u.test(String(value))) {
    throw new CliError('SERVICE_DEFINITION_INVALID',
      `the ${label} contains a newline (line feed or carriage return); systemd unit directive lines cannot represent it, and the service definition is refused rather than written corrupted — remove the newline from the configuration value`);
  }
}

function quotedArgument(value) {
  assertUnitLineSafe(value, 'ExecStart argument');
  return `"${escapeExecStartArgument(value)}"`;
}

// The assignment is quoted as a whole (`Environment="NAME=value"`): systemd
// parses one assignment per line, and quotes around only the value would be
// taken literally.
function environmentLine(name, value) {
  assertUnitLineSafe(value, `${name} environment value`);
  return `Environment="${name}=${escapeEnvironmentValue(value)}"`;
}

function unescapeUnitValue(text) {
  let out = '';
  for (let index = 0; index < text.length; index += 1) {
    const char = text[index];
    if (char === '\\' && index + 1 < text.length) { out += text[index + 1]; index += 1; continue; }
    if (char === '$' && text[index + 1] === '$') { out += '$'; index += 1; continue; }
    if (char === '%' && text[index + 1] === '%') { out += '%'; index += 1; continue; }
    out += char;
  }
  return out;
}

function firstExecStartToken(line) {
  const rest = line.replace(/^[+!:-]+/u, ''); // systemd command prefixes (+, -, :, !, !!)
  if (rest.startsWith('"')) {
    for (let index = 1; index < rest.length; index += 1) {
      if (rest[index] === '\\') { index += 1; continue; }
      if (rest[index] === '"') return { token: rest.slice(1, index), after: rest.slice(index + 1) };
    }
    return null; // unterminated quote
  }
  const end = rest.search(/\s/u);
  return { token: end === -1 ? rest : rest.slice(0, end), after: end === -1 ? '' : rest.slice(end) };
}

// The executable the unit pins — the systemd twin of the plist's first
// ProgramArguments entry — used by activation to learn the old program and to
// restore it byte-for-byte on rollback.
export function execStartProgram(unitBytes) {
  const match = unitBytes.toString('utf8').match(/^ExecStart=(.*)$/m);
  if (!match) return null;
  const token = firstExecStartToken(match[1]);
  return token ? unescapeUnitValue(token.token) : null;
}

export function replaceExecStartProgram(unitBytes, program) {
  const text = unitBytes.toString('utf8');
  const match = text.match(/^ExecStart=(.*)$/m);
  if (!match) return null;
  const token = firstExecStartToken(match[1]);
  if (!token) return null;
  const prefix = match[1].match(/^[+!:-]*/u)[0];
  const line = `ExecStart=${prefix}${quotedArgument(program)}${token.after}`;
  // Replacer function: a raw replacement string would reinterpret $-patterns
  // inside the restored unit text.
  return Buffer.from(text.replace(/^ExecStart=.*$/m, () => line), 'utf8');
}

// The same parameterization the plist generator carries (one-to-one): the
// verified daemon artifact, the data paths the daemon takes as arguments, the
// pinned ZCode runtime forwarded exactly when that installation exists, and
// the persisted subagent environment.  Restart=always mirrors KeepAlive;
// WantedBy=default.target mirrors RunAtLoad.  The daemon's own
// --diagnostic-log writer puts daemon-error.log under the XDG state log
// directory; stdout/stderr stay with the user journal, the systemd-native
// sink (StandardOutput=append: cannot represent paths containing spaces).
export function systemdUnit(paths, options = {}) {
  const daemon = options.daemonPath || nativeBinary(DAEMON_BIN_NAME);
  const config = readConfig(paths.config);
  const { runtime_path: dshRuntime, home: dshHome, profile: dshProfile, version: dshVersion } = config.subagents.dsh;
  const { runtime_path: codexRuntime, home: codexHome } = config.subagents.codex;
  const { runtime_path: agyRuntime } = config.subagents.agy;
  const configRevision = config.revision;
  const zcodeRuntime = options.zcodeRuntime ?? ZCODE_RUNTIME;
  const execStart = [
    quotedArgument(daemon),
    quotedArgument('--database'),
    quotedArgument(paths.database),
    quotedArgument('--socket'),
    quotedArgument(paths.socket),
    ...(fs.existsSync(zcodeRuntime) ? [quotedArgument('--runtime'), quotedArgument(zcodeRuntime)] : []),
    quotedArgument('--diagnostic-log'),
    quotedArgument(path.join(paths.logs, 'daemon-error.log')),
  ].join(' ');
  const environment = [
    environmentLine('PATH', systemdServicePath()),
    ...(configRevision === null ? [] : [environmentLine('EXTERNAL_SUBAGENT_CONFIG_REVISION', configRevision)]),
    ...(dshRuntime ? [environmentLine('DSH_RUNTIME_PATH', dshRuntime)] : []),
    ...(dshHome ? [environmentLine('DSH_HOME', dshHome)] : []),
    ...(dshProfile ? [environmentLine('DSH_PROFILE', dshProfile)] : []),
    ...(dshVersion ? [environmentLine('DSH_VERSION', dshVersion)] : []),
    ...(codexRuntime ? [environmentLine('CODEX_RUNTIME_PATH', codexRuntime)] : []),
    ...(codexHome ? [environmentLine('CODEX_HOME', codexHome)] : []),
    ...(agyRuntime ? [environmentLine('AGY_RUNTIME_PATH', agyRuntime)] : []),
  ];
  return Buffer.from([
    `# Managed by ${PRODUCT_NAME}; regenerate with 'external-subagent init'.`,
    '[Unit]',
    `Description=${PRODUCT_NAME} daemon`,
    '',
    '[Service]',
    'Type=simple',
    `ExecStart=${execStart}`,
    ...environment,
    'Restart=always',
    '',
    '[Install]',
    'WantedBy=default.target',
    '',
  ].join('\n'), 'utf8');
}

export function installServiceUnit(paths, options = {}) {
  const unit = options.unit || systemdUnit(paths);
  atomicWrite(paths.launchAgent, unit, 0o600);
  return { installed: true, path: paths.launchAgent, unit: SYSTEMD_UNIT_NAME };
}

export function systemctl(args) {
  if (process.env.EXTERNAL_SUBAGENT_TEST_NO_SYSTEMCTL === '1') {
    return { action: args[0], skipped: true, reason: 'systemd neutralized by test seam' };
  }
  if (!hasUserSystemdSession()) throw noSessionError();
  const result = spawnSync(SYSTEMCTL_PATH, ['--user', ...args], { encoding: 'utf8' });
  if (result.error) throw new CliError('DAEMON_CONTROL_FAILED', `systemctl is unavailable: ${result.error.message}`);
  if (result.status !== 0 && /Failed to connect to (?:user scope )?bus/iu.test(result.stderr || '')) {
    throw noSessionError((result.stderr || '').trim());
  }
  if (result.status !== 0) {
    throw new CliError('DAEMON_CONTROL_FAILED', (result.stderr || result.stdout || `systemctl ${args[0]} failed`).trim());
  }
  return { action: args[0], status: result.status, stdout: result.stdout };
}

// The single `show`-shaped probe every decision below reuses — the structural
// twin of the launchctl print/absent discriminator.  A unit the user manager
// has loaded reports its real state (`failed` included), so a status view built
// on it can never turn a crashed unit into "healthy".
export function unitServiceStatus(control, unit = SYSTEMD_UNIT_NAME) {
  const result = control(['show', unit]);
  if (result.skipped) return { skipped: true, reason: result.reason };
  const properties = Object.fromEntries(
    (result.stdout || '').split('\n').filter((line) => /^[A-Za-z]+=/.test(line)).map((line) => {
      const separator = line.indexOf('=');
      return [line.slice(0, separator), line.slice(separator + 1)];
    }),
  );
  const pid = Number(properties.MainPID);
  return {
    registered: properties.LoadState === 'loaded',
    load_state: properties.LoadState ?? null,
    state: properties.ActiveState ?? null,
    sub_state: properties.SubState ?? null,
    pid: Number.isInteger(pid) && pid > 0 ? pid : null,
  };
}

// ActiveState values that mean "the service has a live process right now".
const LIVE_STATES = new Set(['active', 'activating', 'reloading']);

// Read-only registration view for `status`.  A missing user session degrades
// into an explicit unavailable view (with the linger hint) instead of failing
// the whole read-only command; every other control failure still surfaces.
export function systemdServiceRegistrationStatus(options = {}) {
  const control = options.systemctl || systemctl;
  try {
    const view = unitServiceStatus(control);
    if (view.skipped) return { query: 'skipped', registered: null, reason: view.reason };
    const { registered, state, sub_state: subState, pid } = view;
    return registered ? { registered: true, state, sub_state: subState, pid } : { registered: false };
  } catch (error) {
    if (error.code === 'NO_USER_SYSTEMD_SESSION') {
      return { query: 'unavailable', registered: null, reason: error.message };
    }
    throw error;
  }
}

// systemd natively answers a repeat `enable --now` with success even on a
// running unit, but the product keeps the launchd contract: an already-live
// service is reported as already_loaded with its real state/pid and is never
// disturbed, and an enable that loses a race to another starter is settled
// through the same probe instead of surfacing a phantom failure.
export function bootstrapServiceSystemd(paths, _uid, options = {}) {
  const control = options.systemctl || systemctl;
  const alreadyLoaded = (view) => ({
    action: 'bootstrap', unit: SYSTEMD_UNIT_NAME, already_loaded: true,
    state: view.state ?? null, pid: view.pid ?? null,
  });
  const isLive = (view) => !view.skipped && view.registered === true && LIVE_STATES.has(view.state);
  const existing = unitServiceStatus(control);
  if (isLive(existing)) return alreadyLoaded(existing);
  try {
    control(['daemon-reload']);
    return { ...control(['enable', '--now', SYSTEMD_UNIT_NAME]), unit: SYSTEMD_UNIT_NAME };
  } catch (error) {
    const settled = unitServiceStatus(control);
    if (isLive(settled)) return alreadyLoaded(settled);
    throw error;
  }
}

// A disable --now returns before the unit has necessarily finished its
// teardown, and a start issued inside that window can race the still-running
// stop job — the same observed hazard the launchd bootout confirmation loop
// guards.  stop therefore confirms the unit is actually inactive (and its
// process gone or the socket removed) before reporting success, bounded, and
// a unit that was never loaded is an idempotent already_stopped.
export function bootoutServiceSystemd(paths, _uid, options = {}) {
  const control = options.systemctl || systemctl;
  // A skipped probe (test seam) reads as not-registered, exactly like the
  // launchd bootout owner treats a neutralized print: fixture stops are
  // already_stopped, never a real disable.
  const stopped = (view) => view.skipped || view.registered !== true
    || (view.state === 'inactive' && view.pid === null);
  const existing = unitServiceStatus(control);
  if (stopped(existing)) return { action: 'bootout', unit: SYSTEMD_UNIT_NAME, already_stopped: true };
  let result;
  try {
    result = control(['disable', '--now', SYSTEMD_UNIT_NAME]);
  } catch (error) {
    // A racing removal (another stop, or the user manager itself) may have
    // taken the unit down between the probe and the disable; confirm before
    // reporting failure, so a repeated stop stays idempotent.
    const settled = unitServiceStatus(control);
    if (stopped(settled)) return { action: 'bootout', unit: SYSTEMD_UNIT_NAME, already_stopped: true };
    throw error;
  }
  const deadline = Date.now() + (options.unloadTimeoutMs ?? 10_000);
  for (;;) {
    const probe = unitServiceStatus(control);
    if (stopped(probe)) {
      return { ...result, unit: SYSTEMD_UNIT_NAME, removed: true, socket_removed: !fs.existsSync(paths.socket) };
    }
    if (Date.now() >= deadline) throw new CliError('SERVICE_UNLOAD_TIMEOUT', 'systemd service remained active after disable --now');
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 50);
  }
}
