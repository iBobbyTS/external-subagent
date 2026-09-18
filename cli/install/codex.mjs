import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { CliError } from '../errors.mjs';
import { atomicWrite, jsonBytes, readOptional, restoreOptional } from '../fs-atomic.mjs';
import { nativeBinary, pluginSourceRoot, PLUGIN_NAME } from './layout.mjs';
import { pluginManifest, treeDigest, preparePluginStage, guardedRestores } from './plugin-stage.mjs';

// Managed Codex binding.  The staging tree plus a local source marketplace are
// the product-owned half; every installation/removal goes through the official
// codex CLI (verified against codex-cli 0.153.4, see docs/compatibility/codex.md):
//   codex plugin marketplace add <root> --json
//   codex plugin add <name> --marketplace <marketplace> --json
//   codex plugin remove <name>@<marketplace> --json
// The codex CLI owns its own config/cache writes; this module never edits the
// Codex cache directly.

const CODEX_MCP_SECTION = 'mcp_servers.external_subagent';
const MARKETPLACE_NAME = 'personal';

export const CODEX_TOOLS = Object.freeze([
  'external_subagent_cancel',
  'external_subagent_close',
  'external_subagent_list',
  'external_subagent_observe',
  'external_subagent_respond',
  'external_subagent_result',
  'external_subagent_send',
  'external_subagent_spawn',
  'external_subagent_status',
  'external_subagent_wait',
]);

export function codexHomeFor(options = {}, paths) {
  if (typeof options.codexHome === 'string' && options.codexHome.length > 0) return options.codexHome;
  if (process.env.CODEX_HOME) return process.env.CODEX_HOME;
  return path.join((paths && paths.home) || os.homedir(), '.codex');
}

function runCodex(args, options = {}) {
  const cli = options.codexCli || 'codex';
  const result = spawnSync(cli, args, { encoding: 'utf8', env: { ...process.env, ...(options.env || {}) } });
  if (result.error) throw new CliError('CODEX_CLI_UNAVAILABLE', result.error.message);
  if (result.status !== 0) throw new CliError('CODEX_CLI_FAILED', (result.stderr || result.stdout || '').trim() || `codex exited ${result.status}`);
  let parsed = null;
  if ((result.stdout || '').trim()) { try { parsed = JSON.parse(result.stdout); } catch { /* help text is not JSON */ } }
  return { status: result.status, stdout: result.stdout || '', stderr: result.stderr || '', json: parsed };
}

// Claiming a Codex home includes bringing it into existence; codex itself
// refuses to resolve a CODEX_HOME that is missing.
function ensureCodexHome(codexHome) {
  if (!fs.existsSync(codexHome)) fs.mkdirSync(codexHome, { recursive: true, mode: 0o700 });
  return codexHome;
}

function marketplaceRootFor(file) {
  return path.basename(path.dirname(file)) === 'plugins' && path.basename(path.dirname(path.dirname(file))) === '.agents'
    ? path.dirname(path.dirname(path.dirname(file)))
    : path.dirname(file);
}

function updateMarketplace(file, staging) {
  const root = marketplaceRootFor(file);
  fs.mkdirSync(root, { recursive: true, mode: 0o700 });
  fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  let doc = { name: MARKETPLACE_NAME, interface: { displayName: 'Personal' }, plugins: [] };
  if (fs.existsSync(file)) doc = JSON.parse(fs.readFileSync(file, 'utf8'));
  if (!Array.isArray(doc.plugins)) throw new CliError('PLUGIN_MARKETPLACE_CONFLICT', 'marketplace file is not a source marketplace manifest');
  const rel = `./${path.relative(root, staging)}`;
  const entry = { name: PLUGIN_NAME, source: { source: 'local', path: rel }, policy: { installation: 'AVAILABLE', authentication: 'ON_INSTALL' }, category: 'Productivity' };
  const index = doc.plugins.findIndex((p) => p.name === PLUGIN_NAME);
  if (index >= 0) {
    const current = doc.plugins[index];
    const currentPath = current?.source?.path;
    const resolved = currentPath ? path.resolve(root, currentPath) : null;
    const manifest = resolved && fs.existsSync(path.join(resolved, '.codex-plugin', 'plugin.json')) ? JSON.parse(fs.readFileSync(path.join(resolved, '.codex-plugin', 'plugin.json'), 'utf8')) : null;
    if (currentPath !== entry.source.path || !manifest || manifest.name !== PLUGIN_NAME) throw new CliError('PLUGIN_MARKETPLACE_CONFLICT', 'marketplace entry is not a managed external-subagent source');
    return { marketplace: file, marketplace_name: doc.name || MARKETPLACE_NAME, entry: currentPath, digest: treeDigest(staging) };
  }
  doc.plugins.push(entry);
  // atomicWrite, not a bare writeFileSync: a failed/partial write either
  // lands whole or leaves the prior bytes untouched, so the rollback below
  // never inherits a half-written manifest it might skip restoring.
  atomicWrite(file, jsonBytes(doc));
  return { marketplace: file, marketplace_name: doc.name || MARKETPLACE_NAME, entry: rel, digest: treeDigest(staging) };
}

// The staged managed binding this install just wrote: the manifest identity
// plus the facade command and daemon socket stagePlugin pinned.  The cache
// verification below compares the codex-materialized copy against exactly
// these values, never against recomputed guesses.
function stagedManagedBinding(staging) {
  const manifest = JSON.parse(fs.readFileSync(path.join(staging, '.codex-plugin', 'plugin.json'), 'utf8'));
  const server = JSON.parse(fs.readFileSync(path.join(staging, '.mcp.json'), 'utf8')).mcpServers?.external_subagent;
  if (!server || typeof server.command !== 'string' || typeof server.env?.ZCODE_AGENTD_SOCKET !== 'string') {
    throw new CliError('INVALID_PLUGIN_SOURCE', 'staged plugin MCP binding is incomplete');
  }
  return { manifest, command: server.command, socket: server.env.ZCODE_AGENTD_SOCKET };
}

function readCacheJson(file, cache, identity) {
  try {
    return JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch (error) {
    throw new CliError('CODEX_CACHE_UNVERIFIABLE', `codex plugin cache for ${identity} is present but unreadable (${file}: ${error.message})`);
  }
}

// codex 0.153.4 materializes `plugin add` caches from a machine-global
// content store keyed by plugin@marketplace@version, so when the same
// identity was first cached from a different binding (another staging root,
// installed facade, or daemon socket), this home receives those foreign
// bytes while `plugin add` still reports success.  Before installPlugin may
// return success, the materialized cache is read back and compared with this
// run's staged binding; any surprise fails closed below.  The cache is only
// ever READ here — codex-owned state is never edited, and the remediation
// for store-reused bytes is a distinct release identity, not a rewrite.
function verifyCodexCache(add, { codexHome, staging, marketplaceName }) {
  const expected = stagedManagedBinding(staging);
  const identity = `${PLUGIN_NAME}@${marketplaceName}@${expected.manifest.version}`;
  const candidates = [];
  const reported = add.json?.installedPath || add.json?.installed_path;
  if (typeof reported === 'string' && reported.length > 0) candidates.push(reported);
  candidates.push(path.join(codexHome, 'plugins', 'cache', add.json?.marketplaceName || marketplaceName, PLUGIN_NAME, String(expected.manifest.version)));
  for (const cache of [...new Set(candidates)]) {
    if (!fs.existsSync(cache)) continue;
    const manifest = readCacheJson(path.join(cache, '.codex-plugin', 'plugin.json'), cache, identity);
    for (const field of ['name', 'version', 'skills', 'mcpServers']) {
      if (manifest[field] !== expected.manifest[field]) {
        throw new CliError('CODEX_CACHE_BINDING_MISMATCH', `codex plugin cache for ${identity} has manifest ${field}=${JSON.stringify(manifest[field])}, expected the staged ${JSON.stringify(expected.manifest[field])} (${cache})`);
      }
    }
    const server = readCacheJson(path.join(cache, '.mcp.json'), cache, identity).mcpServers?.external_subagent;
    const socket = server?.env?.ZCODE_AGENTD_SOCKET;
    if (!server || server.command !== expected.command || socket !== expected.socket) {
      throw new CliError('CODEX_CACHE_BINDING_MISMATCH', `codex plugin cache for ${identity} carries a different managed binding (command=${server?.command}, socket=${socket}); expected this install's staged binding (command=${expected.command}, socket=${expected.socket}). The machine-global content store reused another installation's bytes for the same identity (${cache}); release a distinct plugin version instead of accepting them`);
    }
    return cache;
  }
  // `plugin add` reported success, yet neither the CLI-reported path nor the
  // derived cache location exists on disk — there are no bytes to verify this
  // install against.  Real codex always materializes the cache, so a success
  // without one is an unverifiable install: fail closed here instead of
  // reporting installed/cache_verified:false, which let public installs and
  // reconciles record success (claims, last_status=updated) for an install
  // whose binding this run never saw.
  const lookedIn = [...new Set(candidates)].join(', ');
  throw new CliError('CODEX_CACHE_UNVERIFIABLE', `codex plugin add reported success for ${identity} but no plugin cache was materialized to verify (looked in: ${lookedIn}); refusing to record an unverified install`);
}

export function resolveStaging(home, options) {
  let staging = options.stagingPath || path.join(home, 'plugins', PLUGIN_NAME);
  let marketplace = options.marketplacePath || path.join(home, '.agents', 'plugins', 'marketplace.json');
  // The default ~/.agents file can be an installed-registry projection, not a
  // source. Never overwrite it; use a private local marketplace root instead.
  if (!options.marketplacePath && fs.existsSync(marketplace)) {
    const reprobe = () => {
      const root = path.join(home, '.external-subagent-marketplace');
      staging = path.join(root, 'plugins', PLUGIN_NAME);
      marketplace = path.join(root, '.agents', 'plugins', 'marketplace.json');
    };
    try {
      if (!Array.isArray(JSON.parse(fs.readFileSync(marketplace, 'utf8')).plugins)) reprobe();
    } catch { reprobe(); }
  }
  return { staging, marketplace };
}

export function installPlugin(paths, options = {}) {
  const source = options.source || pluginSourceRoot();
  const home = options.home || paths.home;
  const { staging, marketplace } = resolveStaging(home, options);
  const codexHome = ensureCodexHome(codexHomeFor(options, paths));
  const env = { ...(options.env || {}), CODEX_HOME: codexHome };
  const cli = { codexCli: options.codexCli, env };
  const probe = runCodex(['plugin', 'add', '--help'], cli);
  if (options.dryRun) {
    return { dry_run: true, operation: options.uninstall ? 'uninstall' : 'install', source, staging, marketplace, codex_home: codexHome, cli: 'codex', help_exit: probe.status };
  }
  if (options.uninstall) return uninstallPlugin(paths, options);
  pluginManifest(source);
  const priorMarketplace = fs.existsSync(marketplace) ? fs.readFileSync(marketplace) : null;
  const staged = preparePluginStage(source, staging, paths);
  // AUD-002: stage by validated replacement.  publish() is self-contained
  // (a failed copy/parse/swap never leaves the live tree moved or damaged);
  // every later failure rolls the marketplace back to its prior bytes and
  // restores the prior coherent staging tree — each as an INDEPENDENT
  // guarded attempt, so a restore that itself fails (a marketplace parent
  // that stays read-only) neither skips the staging restore nor masks the
  // original error, and the unrestored resources ride the thrown error.
  const restoreStaging = () => {
    if (!staged.restore()) {
      throw new CliError('PLUGIN_STAGING_RESTORE_FAILED', `the prior staging tree could not be restored at ${staging}; the retained prior copy is left beside it as recovery material`);
    }
  };
  const restoreMarketplace = () => restoreOptional(marketplace, priorMarketplace);
  let market;
  try {
    staged.publish();
    market = updateMarketplace(marketplace, staging);
  } catch (error) {
    // The marketplace restore runs here too: updateMarketplace may already
    // have written the managed entry (its write is atomic, but a later step
    // in it can still fail), and those bytes must go back to the prior
    // backup — foreign entries included — alongside the staging restore.
    throw guardedRestores(error, [
      { resource: marketplace, restore: restoreMarketplace },
      { resource: staging, restore: restoreStaging },
    ]);
  }
  // Explicitly configured marketplaces must be registered; the personal
  // default is implicit. Both registrations are idempotent in codex.
  let add;
  let verifiedCache = null;
  try {
    if (options.registerMarketplace !== false) runCodex(['plugin', 'marketplace', 'add', marketplaceRootFor(marketplace), '--json'], cli);
    add = runCodex(['plugin', 'add', PLUGIN_NAME, '--marketplace', market.marketplace_name, '--json'], cli);
    verifiedCache = verifyCodexCache(add, { codexHome, staging, marketplaceName: market.marketplace_name });
  } catch (error) {
    throw guardedRestores(error, [
      { resource: marketplace, restore: restoreMarketplace },
      { resource: staging, restore: restoreStaging },
    ]);
  }
  staged.complete();
  return {
    installed: true, source, staging, marketplace, codex_home: codexHome,
    cache: verifiedCache,
    cache_verified: true,
    codex: add.json || add.stdout.trim(), ...market,
  };
}

export function uninstallPlugin(paths, options = {}) {
  const source = options.source || pluginSourceRoot();
  const home = options.home || paths.home;
  const { staging, marketplace } = resolveStaging(home, options);
  const codexHome = codexHomeFor(options, paths);
  const env = { ...(options.env || {}), CODEX_HOME: codexHome };
  if (options.dryRun) {
    return { dry_run: true, operation: 'uninstall', source, staging, marketplace, codex_home: codexHome, cli: 'codex' };
  }
  let marketplaceName = MARKETPLACE_NAME;
  try {
    if (fs.existsSync(marketplace)) marketplaceName = JSON.parse(fs.readFileSync(marketplace, 'utf8')).name || MARKETPLACE_NAME;
  } catch { /* keep the default marketplace name */ }
  if (!fs.existsSync(codexHome)) {
    return { uninstalled: true, source, staging, marketplace, codex_home: codexHome, marketplace_name: marketplaceName, codex: null, note: 'codex home absent; nothing to remove' };
  }
  // The verified removal form is <plugin>@<marketplace>; a bare name is
  // rejected by codex 0.153.4.
  const result = runCodex(['plugin', 'remove', `${PLUGIN_NAME}@${marketplaceName}`, '--json'], { codexCli: options.codexCli, env });
  return { uninstalled: true, source, staging, marketplace, codex_home: codexHome, marketplace_name: marketplaceName, codex: result.json || result.stdout.trim() };
}

function codexMcpConfig(paths) {
  const command = nativeBinary('external-subagent-mcp');
  const tools = CODEX_TOOLS.map((tool) => `  "${tool}",`).join('\n');
  return `[${CODEX_MCP_SECTION}]\ncommand = ${JSON.stringify(command)}\nenabled = true\nrequired = true\nstartup_timeout_sec = 10\ntool_timeout_sec = 304\nenabled_tools = [\n${tools}\n]\ndefault_tools_approval_mode = "prompt"\n\n[${CODEX_MCP_SECTION}.env]\nZCODE_AGENTD_SOCKET = ${JSON.stringify(paths.socket)}\n\n[${CODEX_MCP_SECTION}.tools.external_subagent_status]\napproval_mode = "auto"\n\n[${CODEX_MCP_SECTION}.tools.external_subagent_list]\napproval_mode = "auto"\n\n[${CODEX_MCP_SECTION}.tools.external_subagent_wait]\napproval_mode = "auto"\n\n[${CODEX_MCP_SECTION}.tools.external_subagent_result]\napproval_mode = "auto"\n`;
}

function removeTomlSection(text, section) {
  const lines = text.split(/(?<=\n)/u);
  let removing = false;
  const kept = [];
  for (const line of lines) {
    const match = line.match(/^\s*\[([^\]]+)\]\s*\r?\n?$/u);
    if (match) removing = match[1] === section || match[1].startsWith(`${section}.`);
    if (!removing) kept.push(line);
  }
  return kept.join('').replace(/\n{3,}$/u, '\n\n');
}

export function installMcp(paths, options = {}) {
  // The TOML binding lives inside the Codex home, so an explicit --codex-home
  // (then CODEX_HOME) selects the config file for install and removal alike.
  const config = options.configPath || path.join(codexHomeFor(options, paths), 'config.toml');
  const codexHome = codexHomeFor(options, paths);
  const command = nativeBinary('external-subagent-mcp');
  if (options.dryRun) {
    return { dry_run: true, operation: options.uninstall ? 'uninstall' : 'install', platform: 'codex', config, codex_home: codexHome, command, socket: paths.socket };
  }
  if (options.uninstall) {
    const prior = readOptional(config);
    if (prior === null) return { uninstalled: false, platform: 'codex', config, codex_home: codexHome };
    const preserved = removeTomlSection(prior.toString('utf8'), CODEX_MCP_SECTION).replace(/^\n+|\n+$/gu, '');
    atomicWrite(config, Buffer.from(preserved ? `${preserved}\n` : ''));
    return { uninstalled: true, platform: 'codex', config, codex_home: codexHome };
  }
  if (!fs.existsSync(command) && !options.skipNativeProbe) {
    throw new CliError('NATIVE_BINARY_NOT_FOUND', 'npm package does not contain the macOS MCP binary');
  }
  const prior = readOptional(config);
  const base = prior === null ? '' : prior.toString('utf8');
  const preserved = removeTomlSection(base, CODEX_MCP_SECTION).replace(/\s*$/u, '');
  const next = `${preserved ? `${preserved}\n\n` : ''}${codexMcpConfig(paths)}`;
  atomicWrite(config, Buffer.from(next));
  return { installed: true, platform: 'codex', config, codex_home: codexHome, command, socket: paths.socket, tools: [...CODEX_TOOLS] };
}
