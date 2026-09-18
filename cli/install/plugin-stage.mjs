import fs from 'node:fs';
import crypto from 'node:crypto';
import path from 'node:path';
import { CliError } from '../errors.mjs';
import { nativeBinary, PLUGIN_NAME } from './layout.mjs';

// Host-agnostic plugin staging: validating the shipped plugin source,
// materializing it into a product-owned staging tree, and fingerprinting
// that tree.  Every host binding (codex, zcode) stages the same source and
// pins the same MCP binding — the facade command plus the daemon socket —
// into the staged `.mcp.json`; only the registration surface around the
// staged tree differs per host.

export function pluginManifest(source) {
  const manifest = JSON.parse(fs.readFileSync(path.join(source, '.codex-plugin', 'plugin.json'), 'utf8'));
  if (manifest.name !== PLUGIN_NAME || manifest.skills !== './skills/' || manifest.mcpServers !== './.mcp.json') {
    throw new CliError('INVALID_PLUGIN_SOURCE', 'plugin manifest identity or relative paths are invalid');
  }
  return manifest;
}

// Digest of the staged plugin tree.  Valid symlinks are followed: the linked
// content is hashed under the link's own relative path, so an edit of a
// target that lives OUTSIDE the traversed root and is reachable only through
// the link still changes the digest.  Dangling links are skipped (there is
// no content to hash) and directory loops/diamonds are bounded by resolved
// real path plus a depth backstop, so the walk always terminates.
export function treeDigest(root) {
  const hash = crypto.createHash('sha256');
  const seen = new Set([fs.realpathSync(root)]);
  const walk = (dir, depth) => {
    if (depth > 64) return;
    for (const entry of fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      const target = path.join(dir, entry.name);
      const isLink = entry.isSymbolicLink();
      let resolvedStat = entry;
      if (isLink) {
        try { resolvedStat = fs.statSync(target); } catch { continue; }
      }
      if (resolvedStat.isDirectory()) {
        const real = fs.realpathSync(target);
        if (seen.has(real)) continue;
        seen.add(real);
        walk(target, depth + 1);
      } else if (resolvedStat.isFile() && entry.name !== '.mcp.json') {
        hash.update(path.relative(root, target));
        hash.update(fs.readFileSync(target));
      }
    }
  };
  walk(root, 0); return hash.digest('hex');
}

// `binding` overrides what gets pinned as the staged MCP command.  Codex
// hosts spawn the native facade directly (the default); hosts with
// restricted spawn environments (zcode) pass an interpreter command plus
// args — e.g. the installing node running the staged stdio bridge.
export function stagePlugin(source, staging, paths, binding = {}) {
  const command = binding.command || nativeBinary('external-subagent-mcp');
  const args = Array.isArray(binding.args) ? binding.args : null;
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
    const priorCommand = priorServer?.command;
    // Refreshable when the prior binding is any form this product wrote:
    // the requested binding, the native facade (earlier managed binding),
    // or the shipped placeholder.
    const managedCommands = new Set([command, nativeBinary('external-subagent-mcp'), '__EXTERNAL_SUBAGENT_MCP_EXECUTABLE__']);
    const commandMatches = managedCommands.has(priorCommand);
    // An interpreter-form binding also pins its script inside the staged
    // tree itself, so a prior args[0] living in this staging proves the
    // binding is ours even after the pinned interpreter path drifted
    // (e.g. a Homebrew node upgrade replaced the Cellar path).
    const priorArgs = Array.isArray(priorServer?.args) ? priorServer.args : [];
    const argsOwned = typeof priorArgs[0] === 'string'
      && path.resolve(priorArgs[0]).startsWith(`${path.resolve(staging)}${path.sep}`);
    // Args are only load-bearing for interpreter bindings (e.g. node running
    // the staged bridge); a prior native-form binding carried none, and the
    // rewrite below installs the requested form regardless.
    const argsMatch = args === null || priorCommand !== command || JSON.stringify(priorArgs) === JSON.stringify(args);
    if (!priorServer || (!commandMatches && !argsOwned) || !argsMatch || priorServer.env?.ZCODE_AGENTD_SOCKET !== paths.socket) {
      throw new CliError('PLUGIN_STAGING_CONFLICT', 'staging MCP binding differs from the managed product endpoint');
    }
  }
  fs.mkdirSync(path.dirname(staging), { recursive: true, mode: 0o700 });
  fs.cpSync(source, staging, { recursive: true, force: true });
  const mcpPath = path.join(staging, '.mcp.json');
  const mcp = JSON.parse(fs.readFileSync(mcpPath, 'utf8'));
  const server = mcp.mcpServers?.external_subagent;
  if (!server) throw new CliError('INVALID_PLUGIN_SOURCE', 'plugin MCP server is missing');
  server.command = command;
  if (args !== null) server.args = args;
  server.env = { ...(server.env || {}), ZCODE_AGENTD_SOCKET: paths.socket };
  fs.writeFileSync(mcpPath, `${JSON.stringify(mcp, null, 2)}\n`, { mode: 0o600 });
}
