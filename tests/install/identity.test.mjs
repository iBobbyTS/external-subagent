import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { installMcp, installHooks } from '../../cli/installer.mjs';

test('canonical CLI and plugin assets use external-subagent identity', () => {
  const root = path.resolve(import.meta.dirname, '../..');
  const help = spawnSync(process.execPath, [path.join(root, 'bin/external-subagent.mjs'), 'help'], { encoding: 'utf8' });
  assert.equal(help.status, 0);
  assert.match(help.stdout, /^external-subagent /u);
  assert.doesNotMatch(help.stdout, /Usage: zas/u);
  const manifest = JSON.parse(fs.readFileSync(path.join(root, 'plugins/codex/external-subagent/.codex-plugin/plugin.json'), 'utf8'));
  const mcp = JSON.parse(fs.readFileSync(path.join(root, 'plugins/codex/external-subagent/.mcp.json'), 'utf8'));
  assert.equal(manifest.name, 'external-subagent');
  assert.ok(mcp.mcpServers.external_subagent);
  assert.equal(mcp.mcpServers.zcode_as_subagent, undefined);
});

test('Codex MCP installer writes canonical external_subagent section', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-install-'));
  const paths = { home, socket: path.join(home, 'external-subagent.sock') };
  const config = path.join(home, 'config.toml');
  const result = installMcp(paths, { configPath: config, dryRun: false, skipNativeProbe: true });
  assert.equal(result.installed, true);
  const text = fs.readFileSync(config, 'utf8');
  assert.match(text, /^\[mcp_servers\.external_subagent\]/mu);
  assert.doesNotMatch(text, /zcode_as_subagent/u);
  fs.rmSync(home, { recursive: true, force: true });
});

test('hook installer targets the shipped external-subagent plugin', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-hooks-'));
  const paths = { zcodeConfig: path.join(home, 'zcode.json'), hookProvenance: path.join(home, 'hooks.json') };
  const result = installHooks(paths);
  assert.equal(result.config, path.resolve(paths.zcodeConfig));
  const config = JSON.parse(fs.readFileSync(paths.zcodeConfig, 'utf8'));
  assert.equal(config.hooks.enabled, true);
  assert.match(JSON.stringify(config), /external-subagent/u);
  fs.rmSync(home, { recursive: true, force: true });
});
