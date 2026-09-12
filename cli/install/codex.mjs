import fs from 'node:fs';
import os from 'node:os';
import crypto from 'node:crypto';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { CliError } from '../errors.mjs';
import { atomicWrite, readOptional } from '../fs-atomic.mjs';
import { codexConfigPath } from '../paths.mjs';
import { nativeBinary, pluginSourceRoot, PLUGIN_NAME } from './layout.mjs';

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

function pluginManifest(source) {
  const manifest = JSON.parse(fs.readFileSync(path.join(source, '.codex-plugin', 'plugin.json'), 'utf8'));
  if (manifest.name !== PLUGIN_NAME || manifest.skills !== './skills/' || manifest.mcpServers !== './.mcp.json') {
    throw new CliError('INVALID_PLUGIN_SOURCE', 'plugin manifest identity or relative paths are invalid');
  }
  return manifest;
}

function treeDigest(root) {
  const hash = crypto.createHash('sha256');
  const walk = (dir) => fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name)).forEach((entry) => {
    const target = path.join(dir, entry.name);
    if (entry.isDirectory()) walk(target);
    else if (entry.isFile() && entry.name !== '.mcp.json') { hash.update(path.relative(root, target)); hash.update(fs.readFileSync(target)); }
  });
  walk(root); return hash.digest('hex');
}

function stagePlugin(source, staging, paths) {
  if (fs.existsSync(staging)) {
    const existing = path.join(staging, '.codex-plugin', 'plugin.json');
    if (!fs.existsSync(existing) || JSON.parse(fs.readFileSync(existing, 'utf8')).name !== PLUGIN_NAME) {
      throw new CliError('PLUGIN_STAGING_CONFLICT', `staging path is not managed by ${PLUGIN_NAME}`);
    }
    // Managed staging is refreshable: skill/docs may have changed since the
    // last install. Keep the ownership check above, then replace its contents
    // from the current source.
    const priorMcp = JSON.parse(fs.readFileSync(path.join(staging, '.mcp.json'), 'utf8'));
    const priorServer = priorMcp.mcpServers?.external_subagent;
    const managedCommand = nativeBinary('external-subagent-mcp');
    if (!priorServer || (priorServer.command !== managedCommand && priorServer.command !== '__EXTERNAL_SUBAGENT_MCP_EXECUTABLE__') || priorServer.env?.ZCODE_AGENTD_SOCKET !== paths.socket) {
      throw new CliError('PLUGIN_STAGING_CONFLICT', 'staging MCP binding differs from the managed product endpoint');
    }
  }
  fs.mkdirSync(path.dirname(staging), { recursive: true, mode: 0o700 });
  fs.cpSync(source, staging, { recursive: true, force: true });
  const mcpPath = path.join(staging, '.mcp.json');
  const mcp = JSON.parse(fs.readFileSync(mcpPath, 'utf8'));
  const server = mcp.mcpServers?.external_subagent;
  if (!server) throw new CliError('INVALID_PLUGIN_SOURCE', 'plugin MCP server is missing');
  server.command = nativeBinary('external-subagent-mcp');
  server.env = { ...(server.env || {}), ZCODE_AGENTD_SOCKET: paths.socket };
  fs.writeFileSync(mcpPath, `${JSON.stringify(mcp, null, 2)}\n`, { mode: 0o600 });
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
  fs.writeFileSync(file, `${JSON.stringify(doc, null, 2)}\n`, { mode: 0o600 });
  return { marketplace: file, marketplace_name: doc.name || MARKETPLACE_NAME, entry: rel, digest: treeDigest(staging) };
}

function resolveStaging(home, options) {
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
  const priorStaging = fs.existsSync(staging);
  const priorMarketplace = fs.existsSync(marketplace) ? fs.readFileSync(marketplace) : null;
  stagePlugin(source, staging, paths);
  let market;
  try { market = updateMarketplace(marketplace, staging); } catch (error) {
    if (!priorStaging) fs.rmSync(staging, { recursive: true, force: true });
    throw error;
  }
  // Explicitly configured marketplaces must be registered; the personal
  // default is implicit. Both registrations are idempotent in codex.
  let add;
  try {
    if (options.registerMarketplace !== false) runCodex(['plugin', 'marketplace', 'add', marketplaceRootFor(marketplace), '--json'], cli);
    add = runCodex(['plugin', 'add', PLUGIN_NAME, '--marketplace', market.marketplace_name, '--json'], cli);
  } catch (error) {
    if (priorMarketplace === null) fs.rmSync(marketplace, { force: true }); else fs.writeFileSync(marketplace, priorMarketplace, { mode: 0o600 });
    if (!priorStaging) fs.rmSync(staging, { recursive: true, force: true });
    throw error;
  }
  return {
    installed: true, source, staging, marketplace, codex_home: codexHome,
    cache: add.json?.installedPath || add.json?.installed_path || null,
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
  const config = options.configPath || codexConfigPath(paths.home);
  const command = nativeBinary('external-subagent-mcp');
  if (options.dryRun) {
    return { dry_run: true, operation: options.uninstall ? 'uninstall' : 'install', platform: 'codex', config, command, socket: paths.socket };
  }
  if (options.uninstall) {
    const prior = readOptional(config);
    if (prior === null) return { uninstalled: false, platform: 'codex', config };
    const preserved = removeTomlSection(prior.toString('utf8'), CODEX_MCP_SECTION).replace(/^\n+|\n+$/gu, '');
    atomicWrite(config, Buffer.from(preserved ? `${preserved}\n` : ''));
    return { uninstalled: true, platform: 'codex', config };
  }
  if (!fs.existsSync(command) && !options.skipNativeProbe) {
    throw new CliError('NATIVE_BINARY_NOT_FOUND', 'npm package does not contain the macOS MCP binary');
  }
  const prior = readOptional(config);
  const base = prior === null ? '' : prior.toString('utf8');
  const preserved = removeTomlSection(base, CODEX_MCP_SECTION).replace(/\s*$/u, '');
  const next = `${preserved ? `${preserved}\n\n` : ''}${codexMcpConfig(paths)}`;
  atomicWrite(config, Buffer.from(next));
  return { installed: true, platform: 'codex', config, command, socket: paths.socket, tools: [...CODEX_TOOLS] };
}
