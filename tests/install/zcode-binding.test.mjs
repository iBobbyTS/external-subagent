// ZCode host binding oracles.  The zcode installer never shells out to a
// host CLI (ZCode has none), so unlike the codex binding there is no fake
// CLI: the entire contract is local state.  Installation must stage the
// plugin into the product-owned tree and register exactly one
// plugins.dirs entry in ~/.zcode/cli/config.json while preserving every
// foreign key (provider, model, hooks, unrelated plugin dirs); anything
// the module does not understand about that config fails closed with the
// original bytes untouched.  Uninstallation releases only this product's
// entry and tree.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { installZcodePlugin, reconcileZcodeBinding, uninstallZcodePlugin, zcodeMcpBinding } from '../../cli/install/zcode.mjs';
import { stagePlugin, treeDigest } from '../../cli/install/plugin-stage.mjs';
import { nativeBinary, pluginSourceRoot } from '../../cli/install/layout.mjs';
import { productPaths } from '../../cli/paths.mjs';

function fixtureHome(prefix = 'external-subagent-zcode-') {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

// Realistic foreign config content: the machine-global file this binding
// merges into carries provider credentials, model picks, and hook state
// that must survive byte-for-byte in value terms.
function foreignConfig() {
  return {
    provider: { zai: { kind: 'anthropic', options: { apiKey: 'opaque' } } },
    model: { main: 'zai/glm-5.3', lite: 'zai/glm-5.3-flash' },
    hooks: { enabled: true, events: { PreToolUse: [{ matcher: 'Bash', hooks: [] }] } },
  };
}

function fixture({ config } = {}) {
  const home = fixtureHome();
  const paths = productPaths(home);
  if (config !== undefined) {
    fs.mkdirSync(path.dirname(paths.zcodeConfig), { recursive: true });
    fs.writeFileSync(paths.zcodeConfig, typeof config === 'string' ? config : `${JSON.stringify(config, null, 2)}\n`);
  }
  return { home, paths };
}

const readConfig = (paths) => JSON.parse(fs.readFileSync(paths.zcodeConfig, 'utf8'));
const stagedServer = (paths) => JSON.parse(fs.readFileSync(path.join(paths.zcodePlugin, '.mcp.json'), 'utf8')).mcpServers.external_subagent;

test('install stages the plugin tree and registers one inline dirs entry, preserving foreign config keys', () => {
  const { home, paths } = fixture({ config: foreignConfig() });
  try {
    const result = installZcodePlugin(paths);
    assert.equal(result.installed, true);
    assert.equal(result.host, 'zcode');
    assert.equal(result.plugin_id, 'external-subagent@inline');
    assert.equal(result.config_verified, true);
    assert.equal(result.staging, paths.zcodePlugin);
    assert.equal(result.digest, treeDigest(paths.zcodePlugin), 'the reported digest is the staged tree digest');

    const manifest = JSON.parse(fs.readFileSync(path.join(paths.zcodePlugin, '.codex-plugin', 'plugin.json'), 'utf8'));
    assert.equal(manifest.name, 'external-subagent');
    const server = stagedServer(paths);
    assert.equal(server.command, process.execPath, 'the staged MCP command is the installing node, not the native facade (ZCode kills ad-hoc native binaries)');
    assert.deepEqual(server.args, [path.join(paths.zcodePlugin, 'scripts', 'mcp-stdio-bridge.mjs')], 'the staged MCP args run the node stdio bridge from the staged tree');
    assert.ok(fs.existsSync(server.args[0]), 'the pinned bridge script exists in the staged tree');
    assert.equal(server.env.ZCODE_AGENTD_SOCKET, paths.socket, 'the staged MCP env pins the product daemon socket');

    const config = readConfig(paths);
    assert.deepEqual(config.provider, foreignConfig().provider, 'foreign provider state survives');
    assert.deepEqual(config.model, foreignConfig().model, 'foreign model state survives');
    assert.deepEqual(config.hooks, foreignConfig().hooks, 'foreign hook state survives');
    assert.deepEqual(config.plugins.dirs, [path.resolve(paths.zcodePlugin)], 'exactly one managed inline entry is registered');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('reinstall is idempotent and refreshes tampered staging bytes from the source', () => {
  const { home, paths } = fixture({ config: foreignConfig() });
  try {
    installZcodePlugin(paths);
    const first = readConfig(paths);
    const tampered = path.join(paths.zcodePlugin, 'skills', 'external-subagent', 'SKILL.md');
    const original = fs.readFileSync(tampered, 'utf8');
    fs.writeFileSync(tampered, 'tampered\n');
    const result = installZcodePlugin(paths);
    assert.equal(result.installed, true);
    assert.equal(fs.readFileSync(tampered, 'utf8'), original, 'managed staging is refreshed from the source');
    const second = readConfig(paths);
    assert.deepEqual(second.plugins.dirs, first.plugins.dirs, 'reinstall never duplicates the dirs entry');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('unrelated plugins.dirs entries survive install and uninstall', () => {
  const foreign = fixtureHome('external-subagent-zcode-foreign-');
  const { home, paths } = fixture({ config: { ...foreignConfig(), plugins: { enabledPlugins: { 'other@inline': true } }, ...{} } });
  // plugins.dirs is provided through a pre-existing foreign entry below.
  const base = readConfig(paths);
  base.plugins.dirs = [foreign];
  fs.writeFileSync(paths.zcodeConfig, `${JSON.stringify(base, null, 2)}\n`);
  try {
    installZcodePlugin(paths);
    assert.deepEqual(readConfig(paths).plugins.dirs, [foreign, path.resolve(paths.zcodePlugin)], 'the managed entry is appended after foreign entries');
    assert.equal(readConfig(paths).plugins.enabledPlugins['other@inline'], true, 'foreign plugin keys are preserved');

    const removed = uninstallZcodePlugin(paths);
    assert.equal(removed.uninstalled, true);
    assert.equal(removed.staging_removed, true);
    const after = readConfig(paths);
    assert.deepEqual(after.plugins.dirs, [foreign], 'only the managed entry is released');
    assert.equal(after.plugins.enabledPlugins['other@inline'], true, 'foreign plugin keys survive removal');
    assert.equal(fs.existsSync(paths.zcodePlugin), false, 'the staged tree is deleted');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(foreign, { recursive: true, force: true });
  }
});

test('an unreadable ZCode config fails closed and is never overwritten', () => {
  const { home, paths } = fixture({ config: '{ not json' });
  const bytesBefore = fs.readFileSync(paths.zcodeConfig);
  try {
    assert.throws(() => installZcodePlugin(paths), (error) => {
      assert.equal(error.code, 'ZCODE_CONFIG_INVALID');
      assert.match(error.message, /not readable JSON/u);
      return true;
    });
    assert.deepEqual(fs.readFileSync(paths.zcodeConfig), bytesBefore, 'the broken foreign config keeps its exact bytes');
    assert.equal(fs.existsSync(paths.zcodePlugin), false, 'no staging is created behind a failed config merge');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('a plugins.dirs entry that already exposes an external-subagent plugin conflicts', () => {
  const rival = fixtureHome('external-subagent-zcode-rival-');
  fs.mkdirSync(path.join(rival, '.zcode-plugin'), { recursive: true });
  fs.writeFileSync(path.join(rival, '.zcode-plugin', 'plugin.json'), JSON.stringify({ name: 'external-subagent', version: '0.0.0' }));
  const { home, paths } = fixture({ config: { plugins: { dirs: [rival] } } });
  try {
    assert.throws(() => installZcodePlugin(paths), (error) => {
      assert.equal(error.code, 'ZCODE_PLUGIN_CONFLICT');
      assert.match(error.message, /another external-subagent plugin directory/u);
      return true;
    });
    assert.deepEqual(readConfig(paths).plugins.dirs, [rival], 'the conflicting config is left untouched');
    assert.equal(fs.existsSync(paths.zcodePlugin), false, 'no staging is created for a conflicting registration');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(rival, { recursive: true, force: true });
  }
});

test('a foreign-owned staging tree with a different binding is refused', () => {
  const { home, paths } = fixture();
  try {
    fs.mkdirSync(path.join(paths.zcodePlugin, '.codex-plugin'), { recursive: true });
    fs.writeFileSync(path.join(paths.zcodePlugin, '.codex-plugin', 'plugin.json'), JSON.stringify({ name: 'external-subagent' }));
    fs.writeFileSync(path.join(paths.zcodePlugin, '.mcp.json'), JSON.stringify({
      mcpServers: { external_subagent: { command: '/usr/local/bin/something-else', env: { ZCODE_AGENTD_SOCKET: '/tmp/other.sock' } } },
    }));
    assert.throws(() => installZcodePlugin(paths), (error) => {
      assert.equal(error.code, 'PLUGIN_STAGING_CONFLICT');
      return true;
    });
    assert.equal(fs.existsSync(paths.zcodeConfig), false, 'a staging conflict never reaches the config');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('a prior native-facade staging from this product is refreshed to the node bridge, not conflicted', () => {
  const { home, paths } = fixture({ config: foreignConfig() });
  try {
    // Pre-create staging in the earlier managed form: the native facade
    // command plus this product's socket — what install-plugin zcode wrote
    // before the bridge switch.
    fs.mkdirSync(path.join(paths.zcodePlugin, '.codex-plugin'), { recursive: true });
    fs.writeFileSync(path.join(paths.zcodePlugin, '.codex-plugin', 'plugin.json'), JSON.stringify({ name: 'external-subagent', version: '0.1.2' }));
    fs.writeFileSync(path.join(paths.zcodePlugin, '.mcp.json'), JSON.stringify({
      mcpServers: { external_subagent: { command: nativeBinary('external-subagent-mcp'), args: [], env: { ZCODE_AGENTD_SOCKET: paths.socket } } },
    }));
    const result = installZcodePlugin(paths);
    assert.equal(result.installed, true, 'an older managed binding upgrades in place');
    const server = stagedServer(paths);
    assert.equal(server.command, process.execPath);
    assert.deepEqual(server.args, zcodeMcpBinding(paths.zcodePlugin).args);
    assert.equal(readConfig(paths).plugins.dirs.length, 1);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('an interpreter-path drift between installs refreshes in place instead of conflicting (REV-001)', () => {
  const { home, paths } = fixture();
  const source = pluginSourceRoot();
  const staging = paths.zcodePlugin;
  const bridge = path.join(staging, 'scripts', 'mcp-stdio-bridge.mjs');
  try {
    // First install pins interpreter A (e.g. a Homebrew Cellar node).
    stagePlugin(source, staging, paths, { command: '/opt/homebrew/Cellar/node/26.5.0_1/bin/node', args: [bridge] });
    // brew upgrade node: the Cellar path changes; re-stage with interpreter B.
    stagePlugin(source, staging, paths, { command: '/opt/homebrew/Cellar/node/26.6.0/bin/node', args: [bridge] });
    const server = stagedServer(paths);
    assert.equal(server.command, '/opt/homebrew/Cellar/node/26.6.0/bin/node', 'the drifted interpreter is repinned in place');
    assert.deepEqual(server.args, [bridge], 'the staged bridge args are unchanged');

    // Ownership via staged args never excuses a foreign command: an unknown
    // interpreter whose script lives OUTSIDE this staging still conflicts.
    stagePlugin(source, staging, paths, { command: '/opt/homebrew/Cellar/node/26.6.0/bin/node', args: [bridge] });
    fs.writeFileSync(path.join(staging, '.mcp.json'), JSON.stringify({
      mcpServers: { external_subagent: { command: '/usr/local/bin/other-node', args: ['/tmp/foreign-bridge.mjs'], env: { ZCODE_AGENTD_SOCKET: paths.socket } } },
    }));
    assert.throws(() => stagePlugin(source, staging, paths, { command: '/opt/homebrew/Cellar/node/26.6.0/bin/node', args: [bridge] }), (error) => {
      assert.equal(error.code, 'PLUGIN_STAGING_CONFLICT');
      return true;
    });
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('dry-run reports the plan without touching config or staging', () => {
  const { home, paths } = fixture({ config: foreignConfig() });
  const bytesBefore = fs.readFileSync(paths.zcodeConfig);
  try {
    const result = installZcodePlugin(paths, { dryRun: true });
    assert.equal(result.dry_run, true);
    assert.equal(result.operation, 'install');
    assert.equal(result.host, 'zcode');
    assert.equal(result.plugin_id, 'external-subagent@inline');
    assert.deepEqual(fs.readFileSync(paths.zcodeConfig), bytesBefore);
    assert.equal(fs.existsSync(paths.zcodePlugin), false);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('a disabled plugins master switch installs with an explicit warning', () => {
  const { home, paths } = fixture({ config: { ...foreignConfig(), plugins: { enabled: false } } });
  try {
    const result = installZcodePlugin(paths);
    assert.equal(result.installed, true);
    assert.match(result.warning, /plugins\.enabled is false/u);
    assert.equal(readConfig(paths).plugins.enabled, false, 'the foreign master switch is preserved as-is');
    assert.equal(readConfig(paths).plugins.dirs.length, 1);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('uninstall without a binding is a no-op that reports absence', () => {
  const { home, paths } = fixture();
  try {
    const result = uninstallZcodePlugin(paths);
    assert.equal(result.uninstalled, false);
    assert.match(result.note, /no zcode binding present/u);
    assert.equal(fs.existsSync(paths.zcodeConfig), false, 'uninstall never creates a config');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('uninstall removes the plugins object entirely when it held only the managed entry', () => {
  const { home, paths } = fixture({ config: foreignConfig() });
  try {
    installZcodePlugin(paths);
    const result = uninstallZcodePlugin(paths);
    assert.equal(result.uninstalled, true);
    assert.equal(result.staging_removed, true);
    const config = readConfig(paths);
    assert.equal('plugins' in config, false, 'an emptied plugins object is removed, not left behind');
    assert.deepEqual(config.provider, foreignConfig().provider, 'foreign state survives');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('uninstall leaves a foreign-owned tree at the staging path in place and reports it', () => {
  const { home, paths } = fixture();
  try {
    installZcodePlugin(paths);
    // Replace the staged manifest with a foreign plugin after binding.
    fs.rmSync(path.join(paths.zcodePlugin, '.codex-plugin'), { recursive: true, force: true });
    fs.mkdirSync(path.join(paths.zcodePlugin, '.zcode-plugin'), { recursive: true });
    fs.writeFileSync(path.join(paths.zcodePlugin, '.zcode-plugin', 'plugin.json'), JSON.stringify({ name: 'someone-elses' }));
    const result = uninstallZcodePlugin(paths);
    assert.equal(result.uninstalled, true);
    assert.equal(result.staging_removed, false);
    assert.match(result.note, /not managed by this product/u);
    assert.equal(fs.existsSync(paths.zcodePlugin), true, 'a foreign tree is never deleted');
    assert.equal(readConfig(paths).plugins, undefined, 'the config entry is still released');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('reconcile is stateless: absent, refreshed, and failed detection', () => {
  const absentHome = fixtureHome('external-subagent-zcode-absent-');
  const absentPaths = productPaths(absentHome);
  const brokenHome = fixtureHome('external-subagent-zcode-broken-');
  const brokenPaths = productPaths(brokenHome);
  const { home, paths } = fixture({ config: foreignConfig() });
  try {
    // Partial, non-product path shapes must never leak into the real user
    // config through the home fallback.
    assert.deepEqual(reconcileZcodeBinding({ data: '/tmp/x', state: '/tmp/x/state.json' }), {
      bound: false, status: 'absent', note: 'zcode binding paths are unavailable',
    });
    assert.deepEqual(reconcileZcodeBinding(absentPaths), { bound: false, status: 'absent' });

    fs.mkdirSync(path.dirname(brokenPaths.zcodeConfig), { recursive: true });
    fs.writeFileSync(brokenPaths.zcodeConfig, 'not json');
    const failed = reconcileZcodeBinding(brokenPaths);
    assert.equal(failed.bound, false);
    assert.equal(failed.status, 'failed');
    assert.equal(failed.error.code, 'ZCODE_CONFIG_INVALID');

    installZcodePlugin(paths);
    const refreshed = reconcileZcodeBinding(paths);
    assert.equal(refreshed.bound, true);
    assert.equal(refreshed.status, 'updated');
    assert.equal(refreshed.digest, treeDigest(paths.zcodePlugin));
  } finally {
    for (const dir of [home, absentHome, brokenHome]) fs.rmSync(dir, { recursive: true, force: true });
  }
});
