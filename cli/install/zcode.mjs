import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { CliError } from '../errors.mjs';
import { atomicWrite, jsonBytes, readOptional, restoreOptional } from '../fs-atomic.mjs';
import { pluginSourceRoot, PLUGIN_NAME } from './layout.mjs';
import { pluginManifest, treeDigest, stagePlugin } from './plugin-stage.mjs';

// Managed ZCode host binding.  ZCode has no headless CLI for plugin
// management (the GUI drives internal IPC only), but its user config
// exposes an inline plugin discovery source: every path in
// `plugins.dirs` under ~/.zcode/cli/config.json is loaded as a plugin
// root with id <name>@inline and default-enabled status (verified
// against ZCode 25.6.0, see docs/compatibility/zcode.md).  This module
// stages the plugin into a product-owned tree and registers exactly that
// one directory entry; it never touches marketplace, cache, or install
// record state that belongs to the ZCode client.  There is no registry
// file: the binding is fully derivable from the config (ZCode has a
// single user config, no multi-home installs), so reconcile and uninstall
// detect rather than remember it.

const ZCODE_PLUGIN_ID = `${PLUGIN_NAME}@inline`;
const MANIFEST_PROBES = ['.zcode-plugin/plugin.json', '.claude-plugin/plugin.json', '.codex-plugin/plugin.json'];

// ZCode spawns plugin MCP servers inside a restricted execution context
// that kills ad-hoc-signed native binaries (observed live: SIGKILL,
// "Connection closed"), while node processes and unix sockets are
// allowed.  The zcode binding therefore pins the installing node running
// the staged stdio bridge instead of the native facade binary.
export function zcodeMcpBinding(staging) {
  return { command: process.execPath, args: [path.join(staging, 'scripts', 'mcp-stdio-bridge.mjs')] };
}

export function zcodeConfigFor(options = {}, paths) {
  if (typeof options.zcodeConfig === 'string' && options.zcodeConfig.length > 0) return options.zcodeConfig;
  return (paths && paths.zcodeConfig) || path.join((paths && paths.home) || os.homedir(), '.zcode', 'cli', 'config.json');
}

// Same-path comparison the way the host resolves plugin dirs: absolute,
// realpath'd when it exists, resolved when it does not.
function normalizeDir(target) {
  const absolute = path.resolve(target);
  try { return fs.realpathSync(absolute); } catch { return absolute; }
}

function sameDir(entry, dir) {
  return typeof entry === 'string' && normalizeDir(entry) === normalizeDir(dir);
}

function pluginNameAt(dir) {
  for (const probe of MANIFEST_PROBES) {
    try {
      const manifest = JSON.parse(fs.readFileSync(path.join(dir, probe), 'utf8'));
      return typeof manifest?.name === 'string' ? manifest.name : null;
    } catch { /* probe the next manifest location */ }
  }
  return null;
}

function readZcodeConfig(file) {
  const bytes = readOptional(file);
  if (bytes === null) return { doc: {}, bytes: null };
  let doc;
  try { doc = JSON.parse(bytes.toString('utf8')); } catch (error) {
    throw new CliError('ZCODE_CONFIG_INVALID', `ZCode config is not readable JSON and will not be overwritten (${file}: ${error.message})`);
  }
  if (!doc || typeof doc !== 'object' || Array.isArray(doc)) {
    throw new CliError('ZCODE_CONFIG_INVALID', `ZCode config must be a JSON object (${file})`);
  }
  return { doc, bytes };
}

// Extract the plugins.dirs array the way the host validates it (zod:
// array of non-empty strings).  Anything else is config this module does
// not understand; fail closed instead of rewriting it.
function pluginDirs(doc, file) {
  const plugins = doc.plugins;
  if (plugins === undefined) return [];
  if (!plugins || typeof plugins !== 'object' || Array.isArray(plugins)) {
    throw new CliError('ZCODE_CONFIG_INVALID', `plugins must be a JSON object in ${file}`);
  }
  const dirs = plugins.dirs;
  if (dirs === undefined) return [];
  if (!Array.isArray(dirs) || dirs.some((entry) => typeof entry !== 'string' || entry.length === 0)) {
    throw new CliError('ZCODE_CONFIG_INVALID', `plugins.dirs must be an array of non-empty directory paths in ${file}`);
  }
  return dirs;
}

// Read back what this binding actually wrote: the staging entry in the
// config plus the interpreter command, bridge script, and daemon socket
// the staging pinned.
function verifyZcodeBinding({ config, staging, dir }) {
  const doc = JSON.parse(fs.readFileSync(config, 'utf8'));
  if (!pluginDirs(doc, config).some((entry) => sameDir(entry, dir))) {
    throw new CliError('ZCODE_BINDING_UNVERIFIABLE', `the staged plugin directory is absent from plugins.dirs after install (${config})`);
  }
  const server = JSON.parse(fs.readFileSync(path.join(staging, '.mcp.json'), 'utf8')).mcpServers?.external_subagent;
  const binding = zcodeMcpBinding(staging);
  if (!server || server.command !== binding.command || JSON.stringify(server.args || []) !== JSON.stringify(binding.args)
    || server.env?.ZCODE_AGENTD_SOCKET === undefined) {
    throw new CliError('ZCODE_BINDING_UNVERIFIABLE', `the staged plugin MCP binding is not the managed endpoint (${staging})`);
  }
  return server;
}

export function installZcodePlugin(paths, options = {}) {
  const source = options.source || pluginSourceRoot();
  const staging = options.stagingPath || paths.zcodePlugin;
  const config = options.configPath || zcodeConfigFor(options, paths);
  if (options.dryRun) {
    return { dry_run: true, operation: options.uninstall ? 'uninstall' : 'install', host: 'zcode', source, staging, config, plugin_id: ZCODE_PLUGIN_ID, cli: 'zcode-config' };
  }
  if (options.uninstall) return uninstallZcodePlugin(paths, options);
  pluginManifest(source);
  const prior = readZcodeConfig(config);
  const dirs = pluginDirs(prior.doc, config);
  // Only one external-subagent may be bound through plugins.dirs: a
  // foreign directory exposing the same plugin name would shadow or be
  // shadowed by ours under the shared inline id.
  for (const entry of dirs) {
    if (!sameDir(entry, staging) && pluginNameAt(entry) === PLUGIN_NAME) {
      throw new CliError('ZCODE_PLUGIN_CONFLICT', `another ${PLUGIN_NAME} plugin directory is already registered in plugins.dirs (${entry})`);
    }
  }
  const priorStagingExisted = fs.existsSync(staging);
  stagePlugin(source, staging, paths, zcodeMcpBinding(staging));
  try {
    const next = structuredClone(prior.doc);
    next.plugins ??= {};
    if (!(next.plugins.dirs || []).some((entry) => sameDir(entry, staging))) next.plugins.dirs = [...(next.plugins.dirs || []), path.resolve(staging)];
    atomicWrite(config, jsonBytes(next));
    const server = verifyZcodeBinding({ config, staging, dir: staging });
    const result = {
      installed: true, host: 'zcode', source, staging, config,
      plugin_id: ZCODE_PLUGIN_ID,
      digest: treeDigest(staging),
      config_verified: true,
      binding: { command: server.command, socket: server.env.ZCODE_AGENTD_SOCKET },
    };
    if (prior.doc.plugins?.enabled === false) {
      result.warning = 'plugins.enabled is false in the ZCode config; the binding will not load until plugins are enabled';
    }
    return result;
  } catch (error) {
    restoreOptional(config, prior.bytes);
    if (!priorStagingExisted) fs.rmSync(staging, { recursive: true, force: true });
    throw error;
  }
}

export function uninstallZcodePlugin(paths, options = {}) {
  const source = options.source || pluginSourceRoot();
  const staging = options.stagingPath || paths.zcodePlugin;
  const config = options.configPath || zcodeConfigFor(options, paths);
  if (options.dryRun) {
    return { dry_run: true, operation: 'uninstall', host: 'zcode', source, staging, config, plugin_id: ZCODE_PLUGIN_ID, cli: 'zcode-config' };
  }
  const prior = readZcodeConfig(config);
  const dirs = pluginDirs(prior.doc, config);
  const kept = dirs.filter((entry) => !sameDir(entry, staging));
  const bound = kept.length !== dirs.length;
  if (!bound && !fs.existsSync(staging)) {
    return { uninstalled: false, host: 'zcode', config, staging, note: 'no zcode binding present' };
  }
  if (bound) {
    const next = structuredClone(prior.doc);
    if (kept.length === 0) delete next.plugins.dirs; else next.plugins.dirs = kept;
    if (Object.keys(next.plugins).length === 0) delete next.plugins;
    atomicWrite(config, jsonBytes(next));
    const doc = JSON.parse(fs.readFileSync(config, 'utf8'));
    if (pluginDirs(doc, config).some((entry) => sameDir(entry, staging))) {
      restoreOptional(config, prior.bytes);
      throw new CliError('ZCODE_BINDING_UNVERIFIABLE', `the staged plugin directory survived removal from plugins.dirs (${config})`);
    }
  }
  // The staged tree is only deleted when it is unmistakably ours; a
  // foreign tree at the staging path is left in place and reported.
  let stagingRemoved = false;
  if (fs.existsSync(staging)) {
    if (pluginNameAt(staging) === PLUGIN_NAME) {
      fs.rmSync(staging, { recursive: true, force: true });
      stagingRemoved = true;
    }
  }
  const result = { uninstalled: true, host: 'zcode', config, staging, staging_removed: stagingRemoved };
  if (fs.existsSync(staging) && !stagingRemoved) result.note = 'staging path is not managed by this product; directory left in place';
  return result;
}

// Stateless reconcile for update flows: the binding exists iff the config
// still points at the staged directory, so detection replaces a registry.
export function reconcileZcodeBinding(paths, options = {}) {
  // Callers without product-shaped paths (test fixtures, partial states)
  // must not leak into the real user config through the home fallback.
  if (!paths || (!paths.zcodeConfig && !paths.home)) {
    return { bound: false, status: 'absent', note: 'zcode binding paths are unavailable' };
  }
  const config = options.configPath || zcodeConfigFor(options, paths);
  const staging = options.stagingPath || paths.zcodePlugin;
  let bound;
  try {
    const { doc } = readZcodeConfig(config);
    bound = pluginDirs(doc, config).some((entry) => sameDir(entry, staging));
  } catch (error) {
    return { bound: false, status: 'failed', error: { code: error.code || 'ZCODE_CONFIG_INVALID', message: error.message } };
  }
  if (!bound) return { bound: false, status: 'absent' };
  try {
    const install = installZcodePlugin(paths, options);
    return { bound: true, status: 'updated', digest: install.digest };
  } catch (error) {
    return { bound: true, status: 'failed', error: { code: error.code || 'ZCODE_BINDING_FAILED', message: error.message } };
  }
}
