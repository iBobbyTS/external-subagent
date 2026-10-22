import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { CliError } from '../errors.mjs';
import { atomicWrite, readOptional, restoreOptional } from '../fs-atomic.mjs';
import { MCP_BIN_NAME } from '../constants.mjs';
import { nativeBinary, pluginSourceRoot, PLUGIN_NAME } from './layout.mjs';
import { guardedRestores } from './plugin-stage.mjs';

// Managed DSH host binding.  DSH boots profiles as an ordered stack of
// plugin-bundle patch layers under the user's own overrides; the user layer
// lives in $DSH_HOME/profiles/<name>/cordis.patch.yml as a top-level YAML
// array of loader patch entries.  This module appends exactly one managed
// `dsh-mcp-client` stdio entry — the MCP facade plus the daemon socket —
// to that file (marked by a sentinel comment) and stages the product skill
// into the user skill root (<dshHome>/skills), which dsh-skill-filesystem
// scans by default (rank 400; OBSERVED against dsh 0.2.0-rc.2).  It never
// touches the profile's package.json, bundles, node_modules, or any dsh
// market/plugin-install state.  There is no registry file: the binding is
// fully derivable from the patch sentinel plus the staged skill, so
// reconcile and uninstall detect rather than remember it (the zcode model).

// The MCP client plugin is resolved by the dsh loader from the CLI's own
// install tree when the bare package name is used (OBSERVED: a fresh
// DSH_HOME with no profile node_modules composes an insert row with this
// name cleanly).  The physical presence check below is a fail-closed guard
// against a future dsh build that stops shipping it.
const MCP_CLIENT_PACKAGE = '@deepseek-ai/dsh-mcp-client';
// The npm dsh CLI rejects dump and plugin-management requests for the
// desktop profile (reserved for the Electron-owned installation), so it
// cannot be verified from here at all.
export const DSH_FORBIDDEN_PROFILES = ['desktop'];
// `external_subagent_wait` defaults to 290s with a 299s ceiling; dsh's
// mcp-client caps tool calls at 60s unless overridden, which would cut
// every long wait.  300000ms covers the ceiling with margin (the same
// posture as the zcode binding's timeoutMs).
const MCP_TOOL_CALL_TIMEOUT_MS = 300000;
// The sentinel comment marking the managed block.  ASCII-only on purpose:
// the patch file is user-facing YAML and must survive any editor.
const SENTINEL = '# @managed-by: external-subagent (install-plugin dsh) — this block is rewritten by the product; keep manual edits outside it';

export function dshHomeFor(options = {}, paths) {
  if (typeof options.dshHome === 'string' && options.dshHome.length > 0) return options.dshHome;
  if (typeof process.env.DSH_HOME === 'string' && process.env.DSH_HOME.length > 0) return process.env.DSH_HOME;
  const home = (paths && paths.home) || os.homedir();
  return path.join(home, '.dsh');
}

export function dshProfileDir(options = {}, paths) {
  const name = options.profile;
  if (typeof name !== 'string' || name.length === 0) {
    throw new CliError('INVALID_ARGUMENT', 'the dsh host requires --profile <name> (the profile directory under $DSH_HOME/profiles)', 2);
  }
  if (DSH_FORBIDDEN_PROFILES.includes(name)) {
    throw new CliError('INVALID_ARGUMENT', `the dsh CLI refuses the "${name}" profile for external management; bind web, headless, acp, or a custom profile instead`, 2);
  }
  if (/[\\/]/u.test(name)) throw new CliError('INVALID_ARGUMENT', 'dsh profile name must not contain path separators', 2);
  return path.join(dshHomeFor(options, paths), 'profiles', name);
}

// Resolve the dsh executable for local probes: an explicit absolute
// DSH_RUNTIME_PATH file first (the same variable the daemon's own discovery
// consumes), otherwise `dsh` from PATH.  Returns { exe, explicit } where exe
// may be a PATH token the caller resolves through the shell.
function dshExecutable() {
  const configured = process.env.DSH_RUNTIME_PATH;
  if (typeof configured === 'string' && configured !== '' && path.isAbsolute(configured) && fs.existsSync(configured)) {
    return { exe: configured, explicit: true };
  }
  return { exe: 'dsh', explicit: false };
}

// The native facade command: same choice as the codex binding (dsh has no
// ZCode-style restricted spawn context that kills signed native binaries;
// OBSERVED: dsh spawns stdio MCP servers as ordinary child processes).
export function dshMcpBinding(paths) {
  const command = nativeBinary(MCP_BIN_NAME);
  if (!command) throw new CliError('UNSUPPORTED_PAYLOAD_PLATFORM', 'no native MCP facade payload is available for this platform');
  return { command, args: [], toolCallTimeoutMs: MCP_TOOL_CALL_TIMEOUT_MS };
}

function mcpClientPackagePresent() {
  // The dsh CLI carries its plugin packages under
  // <dsh-lib-root>/node_modules/@deepseek-ai.  Resolve the lib root from
  // the dsh executable; absence is advisory, not fatal, because a
  // differently laid-out dsh build may still resolve the package.
  const { exe, explicit } = dshExecutable();
  const which = spawnSync(exe, ['--version'], { encoding: 'utf8', timeout: 15000 });
  if (which.error || which.status !== 0) return { present: false, version: null };
  const version = (which.stdout || '').split(/\s+/u).find((word) => /^\d/u.test(word)) || null;
  const resolved = explicit ? path.resolve(exe) : (() => {
    const bin = spawnSync('sh', ['-c', 'command -v dsh'], { encoding: 'utf8', timeout: 15000 });
    return (bin.stdout || '').trim() || null;
  })();
  if (!resolved) return { present: false, version };
  // <prefix>/bin/dsh (npm global, Homebrew) -> <prefix>/lib/node_modules/@deepseek-ai/dsh/node_modules/@deepseek-ai
  try {
    const prefix = path.dirname(path.dirname(resolved));
    const probe = path.join(prefix, 'lib', 'node_modules', '@deepseek-ai', 'dsh', 'node_modules', '@deepseek-ai', MCP_CLIENT_PACKAGE);
    return { present: fs.existsSync(probe), version };
  } catch {
    return { present: false, version };
  }
}

// Serialize the managed block as YAML text.  Dynamic values are single-
// quoted with YAML's doubled-quote escaping: the package name contains a
// reserved `@`, and paths may contain characters that are ambiguous as
// plain scalars, so quoting is unconditional rather than value-dependent
// (a plain-scalar allowlist would silently break on the next odd path).
function yamlScalar(value) {
  return `'${String(value).replaceAll("'", "''")}'`;
}

function managedBlock(binding, socket) {
  return [
    SENTINEL,
    '- insert:',
    `    - name: ${yamlScalar(MCP_CLIENT_PACKAGE)}`,
    '      config:',
    `        serverName: ${yamlScalar('external_subagent')}`,
    `        transport: ${yamlScalar('stdio')}`,
    `        command: ${yamlScalar(binding.command)}`,
    `        args: ${JSON.stringify(binding.args)}`,
    '        env:',
    `          EXTERNAL_SUBAGENT_SOCKET: ${yamlScalar(socket)}`,
    `        toolCallTimeoutMs: ${binding.toolCallTimeoutMs}`,
  ].join('\n');
}

// Append (or replace) the managed block in the profile patch text.  This is
// a text-level transaction, deliberately NOT a YAML parse/rewrite: user
// comments and formatting survive byte-for-byte outside the managed block.
//
// Fresh shipped templates end with a bare `[]` root; appending list items
// after it would be invalid YAML, so that exact trailing empty-array line
// is removed first (the shipped template shape, OBSERVED 0.2.0-rc.2).
export function composePatchText(priorText, binding, socket) {
  const block = managedBlock(binding, socket);
  const prior = priorText === null ? '' : priorText;
  const range = managedRange(prior);
  if (range !== null) {
    // Replace in place, preserving the exact blank-line separation that
    // surrounded the prior block (head keeps its trailing blank line, tail
    // its leading one), so an unchanged binding recomposes byte-for-byte.
    const head = prior.slice(0, range.start).replace(/\n{3,}$/u, '\n\n');
    const tail = prior.slice(range.end).replace(/^\n{3,}/u, '\n\n');
    const suffix = tail.length === 0 ? '\n' : '';
    return `${head}${block}${suffix}${tail}`;
  }
  let body = prior;
  const lines = body.split('\n');
  const lastNonEmpty = lines.reduce((acc, line, index) => (line.trim() === '' ? acc : index), -1);
  if (lastNonEmpty !== -1 && lines[lastNonEmpty].trim() === '[]') {
    lines.splice(lastNonEmpty, 1);
    body = lines.join('\n');
  }
  const trimmed = body.replace(/\s+$/u, '');
  return trimmed.length === 0 ? `${block}\n` : `${trimmed}\n\n${block}\n`;
}

// Locate the managed block boundaries in current text: [start, end).
// start is the sentinel line; the block then covers the managed entry —
// the first `- ` line at or after the sentinel — and every following line
// until the next non-empty, non-comment line at column 0 (a following
// top-level item) or EOF.  Returns null when the sentinel is absent, and
// a sentinel-only range when the entry itself was deleted by hand.
function managedRange(text) {
  const sentinelIndex = text.indexOf(SENTINEL);
  if (sentinelIndex === -1) return null;
  const afterSentinelLine = text.indexOf('\n', sentinelIndex);
  const afterSentinel = afterSentinelLine === -1 ? text.length : afterSentinelLine + 1;
  // Find the managed entry's `- ` start, allowing blank/comment lines
  // between the sentinel and the entry.
  const head = text.slice(afterSentinel);
  const entryOffset = head.search(/^- /mu);
  if (entryOffset === -1) {
    // Sentinel present but the entry is gone: the block is the sentinel line.
    return { start: sentinelIndex, end: afterSentinel };
  }
  const entryStart = afterSentinel + entryOffset;
  // The entry consumes its own line plus every indented (or blank/comment)
  // line that follows, stopping before the next column-0 content line.
  const lines = text.slice(entryStart).split('\n');
  let end = entryStart + (lines[0]?.length ?? 0);
  for (let index = 1; index < lines.length; index += 1) {
    const line = lines[index];
    if (line.trim() !== '' && !line.startsWith(' ') && !line.startsWith('#')) break;
    end += 1 + line.length;
  }
  return { start: sentinelIndex, end: Math.min(end, text.length) };
}

function skillPaths(dshHome) {
  const dir = path.join(dshHome, 'skills', 'external-subagent');
  return { dir, file: path.join(dir, 'SKILL.md') };
}

function stageSkill(dshHome, source) {
  const from = path.join(source, 'skills', 'external-subagent', 'SKILL.md');
  const bytes = readOptional(from);
  if (bytes === null) throw new CliError('INVALID_PLUGIN_SOURCE', `plugin skill source is missing (${from})`);
  const { dir, file } = skillPaths(dshHome);
  const prior = readOptional(file);
  // Foreign content at the managed skill path is never overwritten; a
  // byte-equal prior is an idempotent no-op.
  if (prior !== null && !prior.equals(bytes)) {
    throw new CliError('DSH_SKILL_CONFLICT', `a non-product SKILL.md already exists at ${file}; remove it or install manually`);
  }
  if (prior === null) {
    atomicWrite(file, bytes);
    return { dir, written: true };
  }
  return { dir, written: false };
}

// Remove the staged skill only when it is unmistakably ours: byte-equal
// content, or an absent file.  Any other content stays and is reported.
function unstageSkill(dshHome, source) {
  const from = path.join(source, 'skills', 'external-subagent', 'SKILL.md');
  const ours = readOptional(from);
  const { dir, file } = skillPaths(dshHome);
  const staged = readOptional(file);
  if (staged === null) return { removed: false, note: 'no staged skill present' };
  if (ours === null || !ours.equals(staged)) {
    return { removed: false, note: `staged skill at ${file} differs from the product source; left in place` };
  }
  fs.rmSync(dir, { recursive: true, force: true });
  return { removed: true, dir };
}

// Compose the profile through the real dsh CLI and prove the managed entry
// survives: a clean dump (stderr without a `patch:` failure marker) whose
// tail carries our serverName under the patch banner.  The probe is bounded
// and read-only; `desktop` and other refusals surface as verification
// failures.  Returns the observed dsh version token (advisory evidence).
// `options.verify` is the test seam: an injected verifier replaces the
// spawn without touching any other install logic.
function verifyWithDump(dshHome, profileName, options = {}) {
  if (typeof options.verify === 'function') return options.verify({ dshHome, profile: profileName });
  const { exe } = dshExecutable();
  const dump = spawnSync(exe, ['--profile', profileName, '--dump-config'], {
    encoding: 'utf8',
    timeout: 30000,
    env: { ...process.env, DSH_HOME: dshHome, DSH_PERMISSION_MODE: 'read-only' },
  });
  if (dump.error) {
    throw new CliError('DSH_BINDING_UNVERIFIABLE', `dsh --dump-config could not run: ${dump.error.message}`);
  }
  const stderr = dump.stderr || '';
  if (dump.status !== 0) {
    throw new CliError('DSH_BINDING_UNVERIFIABLE', `dsh --dump-config exited ${dump.status}: ${stderr.trim().split('\n').slice(-4).join(' | ')}`);
  }
  // dsh 0.2.0-rc.2 exits 0 even when a patch row fails, printing the reason
  // on stderr (OBSERVED classes: `patch: entry "<id>" not found` for a
  // missing target, `patch: id is required for non-insert patches` for a
  // malformed row).  Fail-open by design upstream — so stderr is a
  // first-class signal here, not a footnote: ANY `patch:` diagnostic line
  // about our patch file rejects the install.
  const patchDiagnostics = stderr.split('\n').filter((line) => line.includes('patch:'));
  if (patchDiagnostics.length > 0) {
    throw new CliError('DSH_BINDING_UNVERIFIABLE', `dsh reported failed patch rows: ${patchDiagnostics.join(' | ')}`);
  }
  const out = dump.stdout || '';
  // Presence must be established by a STRUCTURAL match, not a loose
  // substring over the dump tail: a stale or foreign entry elsewhere in the
  // composition (an older manual row, another profile's leftovers in a
  // copied fixture) could otherwise satisfy a substring check and mask a
  // silently-dropped managed row.  The composed dump re-serializes the
  // entry, so anchor on the plugin name line and require the serverName
  // within the following mapping block.
  const anchor = out.indexOf(`name: ${MCP_CLIENT_PACKAGE}`);
  if (anchor === -1) {
    throw new CliError('DSH_BINDING_UNVERIFIABLE', 'the managed mcp-client entry is absent from the composed profile dump');
  }
  const window = out.slice(anchor, anchor + 1200);
  if (!window.includes('serverName: external_subagent')) {
    throw new CliError('DSH_BINDING_UNVERIFIABLE', 'the managed mcp-client entry is present but its serverName is missing from the composed dump');
  }
  return (dump.stdout.match(/dsh[^\n]*?(\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?)/u) || [])[1]
    || stderr.match(/version[^\n]*?(\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?)/ui)?.[1]
    || null;
}

export function installDshPlugin(paths, options = {}) {
  const source = options.source || pluginSourceRoot();
  const dshHome = dshHomeFor(options, paths);
  const profileName = options.profile;
  const profileDir = dshProfileDir(options, paths);
  if (!fs.existsSync(profileDir) || !fs.statSync(profileDir).isDirectory()) {
    throw new CliError('INVALID_ARGUMENT', `dsh profile directory not found: ${profileDir} (boot the profile once or create it with: dsh --profile ${profileName} --from-default-profile web)`, 2);
  }
  const patchFile = path.join(profileDir, 'cordis.patch.yml');
  if (options.dryRun) {
    return {
      dry_run: true, operation: options.uninstall ? 'uninstall' : 'install', host: 'dsh',
      dsh_home: dshHome, profile: profileName, profile_dir: profileDir, patch_file: patchFile,
      skill_dir: skillPaths(dshHome).dir, cli: 'dsh-patch', source,
    };
  }
  if (options.uninstall) return uninstallDshPlugin(paths, options);
  const packageProbe = mcpClientPackagePresent();
  const binding = dshMcpBinding(paths);
  const priorPatch = readOptional(patchFile);
  const nextText = composePatchText(priorPatch === null ? null : priorPatch.toString('utf8'), binding, paths.socket);
  if (priorPatch !== null && priorPatch.toString('utf8') === nextText) {
    // Byte-equal idempotent reinstall: still ensure the skill is present.
    const skill = stageSkill(dshHome, source);
    return {
      installed: true, host: 'dsh', dsh_home: dshHome, profile: profileName, patch_file: patchFile,
      idempotent: true, config_verified: true,
      binding: { command: binding.command, socket: paths.socket },
      skill: { dir: skill.dir, written: skill.written },
      dsh_version: packageProbe.version,
    };
  }
  try {
    atomicWrite(patchFile, Buffer.from(nextText, 'utf8'));
    const skill = stageSkill(dshHome, source);
    const version = verifyWithDump(dshHome, profileName, options);
    const result = {
      installed: true, host: 'dsh', dsh_home: dshHome, profile: profileName, patch_file: patchFile,
      config_verified: true,
      binding: { command: binding.command, socket: paths.socket },
      skill: { dir: skill.dir, written: skill.written },
      dsh_version: version,
    };
    if (!packageProbe.present) {
      result.warning = `the ${MCP_CLIENT_PACKAGE} plugin was not found beside the dsh CLI; if the profile fails to load the entry, run: dsh plugin --profile ${profileName} add ${MCP_CLIENT_PACKAGE}`;
    }
    return result;
  } catch (error) {
    throw guardedRestores(error, [
      { resource: patchFile, restore: () => restoreOptional(patchFile, priorPatch) },
      { resource: skillPaths(dshHome).dir, restore: () => unstageSkill(dshHome, source) },
    ]);
  }
}

export function uninstallDshPlugin(paths, options = {}) {
  const source = options.source || pluginSourceRoot();
  const dshHome = dshHomeFor(options, paths);
  const profileName = options.profile;
  const profileDir = dshProfileDir(options, paths);
  const patchFile = path.join(profileDir, 'cordis.patch.yml');
  if (options.dryRun) {
    return { dry_run: true, operation: 'uninstall', host: 'dsh', dsh_home: dshHome, profile: profileName, patch_file: patchFile, skill_dir: skillPaths(dshHome).dir };
  }
  const priorPatch = readOptional(patchFile);
  let patchRemoved = false;
  let fallback = null;
  if (priorPatch !== null) {
    const text = priorPatch.toString('utf8');
    const range = managedRange(text);
    if (range !== null) {
      const next = text.slice(0, range.start) + text.slice(range.end);
      const trimmed = next.replace(/\s+$/u, '');
      const remainderHasEntries = trimmed.split('\n').some((line) => /^- /u.test(line));
      if (!remainderHasEntries) {
        // Nothing user-owned remains (comments at most): restore the
        // shipped empty-root shape so the file stays valid YAML.
        atomicWrite(patchFile, Buffer.from('# Your patch layer for this dsh profile, applied after every bundle layer:\n# a top-level YAML array of loader patch entries (id-targeted config\n# overrides, disables, and insert lists; `!!js` expressions allowed).\n[]\n', 'utf8'));
      } else {
        atomicWrite(patchFile, Buffer.from(`${trimmed}\n`, 'utf8'));
      }
      patchRemoved = true;
    } else if (text.includes('serverName: external_subagent')) {
      fallback = 'managed sentinel not found; the patch file was left untouched (a serverName: external_subagent entry remains — remove it manually)';
    }
  }
  // The shared skill is released only after the patch removal succeeded or
  // proved absent — a failed patch removal must not strand the skill.
  const skill = patchRemoved || priorPatch === null ? unstageSkill(dshHome, source) : { removed: false };
  const existed = patchRemoved || skill.removed;
  return {
    uninstalled: existed || patchRemoved,
    host: 'dsh', dsh_home: dshHome, profile: profileName, patch_file: patchFile,
    patch_removed: patchRemoved,
    skill_removed: skill.removed,
    ...(skill.note ? { skill_note: skill.note } : {}),
    ...(fallback ? { warning: fallback } : {}),
    ...(!existed && !patchRemoved ? { note: 'no dsh binding present' } : {}),
  };
}

// Stateless reconcile for update flows (the zcode model): every profile
// directory whose patch file carries the sentinel is rebound; the skill is
// refreshed alongside.  Unknown/corrupt states are reported, never fatal to
// the caller's broader reconcile.
export function reconcileDshBindings(paths, options = {}) {
  const dshHome = dshHomeFor(options, paths);
  const profilesRoot = path.join(dshHome, 'profiles');
  let entries;
  try {
    entries = fs.readdirSync(profilesRoot, { withFileTypes: true }).filter((entry) => entry.isDirectory());
  } catch (error) {
    if (error?.code === 'ENOENT') return { bound: false, profiles: [], note: 'no dsh profiles directory' };
    return { bound: false, profiles: [], error: { code: 'DSH_PROFILES_UNREADABLE', message: error.message } };
  }
  const results = [];
  for (const entry of entries) {
    if (DSH_FORBIDDEN_PROFILES.includes(entry.name)) continue;
    const patchFile = path.join(profilesRoot, entry.name, 'cordis.patch.yml');
    const bytes = readOptional(patchFile);
    if (bytes === null || !bytes.toString('utf8').includes(SENTINEL)) continue;
    try {
      const install = installDshPlugin(paths, { ...options, profile: entry.name });
      results.push({ profile: entry.name, status: 'updated', idempotent: Boolean(install.idempotent) });
    } catch (error) {
      results.push({ profile: entry.name, status: 'failed', error: { code: error.code || 'DSH_BINDING_FAILED', message: error.message } });
    }
  }
  return { bound: results.length > 0, profiles: results };
}

// Product uninstall: release every detected binding at once.  Detection is
// the same sentinel scan as reconcile; each profile is uninstalled
// independently (a failure on one never skips the rest, matching the
// per-home codex release posture), and the shared staged skill is removed
// when no bound profile remains.
export function uninstallAllDshBindings(paths, options = {}) {
  const dshHome = dshHomeFor(options, paths);
  const profilesRoot = path.join(dshHome, 'profiles');
  let entries;
  try {
    entries = fs.readdirSync(profilesRoot, { withFileTypes: true }).filter((entry) => entry.isDirectory());
  } catch {
    return { released: 0, profiles: [] };
  }
  const results = [];
  for (const entry of entries) {
    if (DSH_FORBIDDEN_PROFILES.includes(entry.name)) continue;
    const patchFile = path.join(profilesRoot, entry.name, 'cordis.patch.yml');
    const bytes = readOptional(patchFile);
    if (bytes === null || !bytes.toString('utf8').includes(SENTINEL)) continue;
    try {
      const removed = uninstallDshPlugin(paths, { ...options, profile: entry.name });
      results.push({ profile: entry.name, status: removed.uninstalled ? 'released' : 'absent' });
    } catch (error) {
      results.push({ profile: entry.name, status: 'failed', error: { code: error.code || 'DSH_BINDING_RELEASE_FAILED', message: error.message } });
    }
  }
  // The skill is shared across profiles; remove it once no binding remains.
  if (results.length > 0 && results.every((result) => result.status === 'released')) {
    try {
      unstageSkill(dshHome, options.source || pluginSourceRoot());
    } catch { /* reported per-profile above; a stuck skill never fails the release */ }
  }
  return { released: results.filter((result) => result.status === 'released').length, profiles: results };
}
