import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { installMcp } from '../../cli/install/codex.mjs';

// Legacy brand literals are synthesized at runtime so this anti-residue guard
// carries no legacy literal in its own source.
const legacyShort = ['z', 'a', 's'].join('');
const legacyMcpKey = ['zcode', 'as', 'subagent'].join('_');

test('canonical CLI and plugin assets use external-subagent identity', () => {
  const root = path.resolve(import.meta.dirname, '../..');
  const help = spawnSync(process.execPath, [path.join(root, 'bin/external-subagent.mjs'), 'help'], { encoding: 'utf8' });
  assert.equal(help.status, 0);
  assert.match(help.stdout, /^external-subagent /u);
  assert.doesNotMatch(help.stdout, new RegExp(`Usage: ${legacyShort}`, 'u'));
  const manifest = JSON.parse(fs.readFileSync(path.join(root, 'plugins/codex/external-subagent/.codex-plugin/plugin.json'), 'utf8'));
  const mcp = JSON.parse(fs.readFileSync(path.join(root, 'plugins/codex/external-subagent/.mcp.json'), 'utf8'));
  assert.equal(manifest.name, 'external-subagent');
  assert.ok(mcp.mcpServers.external_subagent);
  assert.equal(mcp.mcpServers[legacyMcpKey], undefined);
});

test('Codex MCP installer writes canonical external_subagent section', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-install-'));
  const paths = { home, socket: path.join(home, 'external-subagent.sock') };
  const config = path.join(home, 'config.toml');
  const result = installMcp(paths, { configPath: config, dryRun: false, skipNativeProbe: true });
  assert.equal(result.installed, true);
  const text = fs.readFileSync(config, 'utf8');
  assert.match(text, /^\[mcp_servers\.external_subagent\]/mu);
  assert.doesNotMatch(text, new RegExp(legacyMcpKey, 'u'));
  fs.rmSync(home, { recursive: true, force: true });
});
