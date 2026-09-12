import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { installMcp } from '../../cli/install/codex.mjs';
import { installHooks } from '../../cli/install/init.mjs';

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

test('hook installer installs a verified real file policy without dropping unrelated hooks', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-hooks-'));
  const paths = { zcodeConfig: path.join(home, 'zcode.json'), hookProvenance: path.join(home, 'hooks.json') };
  const original = Buffer.from('{"hooks":{"enabled":false,"events":{"PreToolUse":[{"matcher":"Custom","hooks":[{"type":"process","command":"existing"}]}]}}}\n');
  fs.writeFileSync(paths.zcodeConfig, original);
  const result = installHooks(paths);
  assert.equal(typeof result.file_policy_sha256, 'string');
  const installed = JSON.parse(fs.readFileSync(paths.zcodeConfig, 'utf8'));
  assert.equal(installed.hooks.enabled, true);
  assert.equal(installed.hooks.events.PreToolUse.length, 3);
  assert.equal(installed.hooks.events.PostToolUse.length, 1);
  assert.equal(fs.existsSync(paths.hookProvenance), true);
  assert.equal(fs.existsSync(path.join(home, 'external-subagent-policy-verifier')), true);
  fs.rmSync(home, { recursive: true, force: true });
});

test('hook installer reports provenance mismatch before unsupported policy', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-hooks-provenance-'));
  const paths = { zcodeConfig: path.join(home, 'zcode.json'), hookProvenance: path.join(home, 'hooks.json') };
  const original = Buffer.from('{"hooks":{}}\n');
  const provenance = Buffer.from('{"schema_version":1,"product":"external-subagent","effective_config_path":"wrong"}\n');
  fs.writeFileSync(paths.zcodeConfig, original);
  fs.writeFileSync(paths.hookProvenance, provenance);
  assert.throws(() => installHooks(paths), (error) => error.code === 'HOOK_INSTALL_FAILED' && /HOOK_PROVENANCE_INVALID/u.test(error.message));
  assert.deepEqual(fs.readFileSync(paths.zcodeConfig), original);
  assert.deepEqual(fs.readFileSync(paths.hookProvenance), provenance);
  fs.rmSync(home, { recursive: true, force: true });
});
