import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { pathToFileURL } from 'node:url';
import { stageDebugPlugin } from '../../scripts/release/stage-debug-plugin.mjs';
import { installMcp } from '../../cli/install/codex.mjs';
import { installZcodePlugin } from '../../cli/install/zcode.mjs';
import { productPaths } from '../../cli/paths.mjs';
import { pluginSourceRoot } from '../../cli/install/layout.mjs';

const repoRoot = path.resolve(import.meta.dirname, '../..');

function moduleUrl(relative) {
  return JSON.stringify(pathToFileURL(path.join(repoRoot, relative)).href);
}

// constants.mjs reads the variant token at module load, so every variant
// assertion runs in a child process with the env set before import.
function variantProbe(extraEnv, home) {
  const code = `
    const constants = await import(${moduleUrl('cli/constants.mjs')});
    const layout = await import(${moduleUrl('cli/install/layout.mjs')});
    const paths = await import(${moduleUrl('cli/paths.mjs')});
    const product = paths.productPaths(${JSON.stringify(home)});
    process.stdout.write(JSON.stringify({
      productName: constants.PRODUCT_NAME,
      productId: constants.PRODUCT_ID,
      label: constants.LAUNCH_AGENT_LABEL,
      daemonBin: constants.DAEMON_BIN_NAME,
      mcpBin: constants.MCP_BIN_NAME,
      nativeDir: constants.NATIVE_DIR_NAME,
      binaries: layout.NATIVE_BINARIES,
      payloadDir: layout.nativePayloadDir(),
      pluginSource: layout.pluginSourceRoot(),
      data: product.data,
      socket: product.socket,
      launchAgent: product.launchAgent,
    }));
  `;
  const result = spawnSync(process.execPath, ['--input-type=module', '-e', code], {
    encoding: 'utf8',
    env: { ...process.env, ...extraEnv },
  });
  assert.equal(result.status, 0, result.stderr);
  return JSON.parse(result.stdout);
}

test('release variant keeps the released identity', () => {
  const probe = variantProbe({}, '/tmp/es-variant-home');
  assert.equal(probe.productName, 'external-subagent');
  assert.equal(probe.productId, 'external_subagent');
  assert.equal(probe.label, 'com.external-subagent.daemon');
  assert.deepEqual([...probe.binaries], ['external-subagentd', 'external-subagent-mcp']);
  assert.ok(probe.payloadDir.endsWith(path.join('npm', 'native', 'darwin-arm64')), probe.payloadDir);
  assert.ok(probe.data.endsWith(path.join('Application Support', 'external-subagent')), probe.data);
  assert.ok(probe.socket.endsWith('external-subagent.sock'), probe.socket);
  assert.ok(probe.pluginSource.endsWith(path.join('plugins', 'codex', 'external-subagent')), probe.pluginSource);
});

test('debug variant derives a fully parallel identity', () => {
  const release = variantProbe({}, '/tmp/es-variant-home');
  const probe = variantProbe({ EXTERNAL_SUBAGENT_VARIANT: 'debug' }, '/tmp/es-variant-home');
  assert.equal(probe.productName, 'external-subagent-debug');
  assert.equal(probe.productId, 'external_subagent_debug');
  assert.equal(probe.label, 'com.external-subagent-debug.daemon');
  assert.equal(probe.daemonBin, 'external-subagent-debugd');
  assert.equal(probe.mcpBin, 'external-subagent-debug-mcp');
  assert.deepEqual([...probe.binaries], ['external-subagent-debugd', 'external-subagent-debug-mcp']);
  assert.ok(probe.payloadDir.endsWith(path.join('npm', 'native-debug', 'darwin-arm64')), probe.payloadDir);
  assert.ok(probe.data.endsWith(path.join('Application Support', 'external-subagent-debug')), probe.data);
  assert.ok(probe.socket.endsWith('external-subagent-debug.sock'), probe.socket);
  assert.ok(probe.launchAgent.endsWith('com.external-subagent-debug.daemon.plist'), probe.launchAgent);
  assert.ok(probe.pluginSource.endsWith(path.join('plugins', 'codex', 'external-subagent-debug')), probe.pluginSource);
  for (const field of ['productName', 'label', 'payloadDir', 'data', 'socket', 'launchAgent']) {
    assert.notEqual(probe[field], release[field], `${field} must differ between variants`);
  }
});

test('debug plugin source staging renames identity and keeps managed content', () => {
  const target = stageDebugPlugin(repoRoot);
  try {
    const manifest = JSON.parse(fs.readFileSync(path.join(target, '.codex-plugin', 'plugin.json'), 'utf8'));
    assert.equal(manifest.name, 'external-subagent-debug');
    assert.match(manifest.interface.displayName, /Debug$/u);
    assert.equal(manifest.version,
      JSON.parse(fs.readFileSync(path.join(repoRoot, 'plugins', 'codex', 'external-subagent', '.codex-plugin', 'plugin.json'), 'utf8')).version);
    assert.ok(fs.existsSync(path.join(target, 'skills', 'external-subagent-debug', 'SKILL.md')));
    assert.ok(!fs.existsSync(path.join(target, 'skills', 'external-subagent')));
    const skill = fs.readFileSync(path.join(target, 'skills', 'external-subagent-debug', 'SKILL.md'), 'utf8');
    assert.match(skill.slice(0, 200), /^---\nname: external-subagent-debug\n/u);
    assert.equal(fs.readFileSync(path.join(target, '.mcp.json'), 'utf8'),
      fs.readFileSync(path.join(repoRoot, 'plugins', 'codex', 'external-subagent', '.mcp.json'), 'utf8'));
    const hooks = fs.readdirSync(path.join(repoRoot, 'plugins', 'codex', 'external-subagent', 'hooks')).sort();
    assert.deepEqual(fs.readdirSync(path.join(target, 'hooks')).sort(), hooks);
  } finally {
    fs.rmSync(target, { recursive: true, force: true });
  }
});

test('codex TOML binding section is variant-scoped', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'es-variant-codex-'));
  try {
    const releaseConfig = path.join(home, 'release.config.toml');
    installMcp(productPaths(home), { configPath: releaseConfig, codexHome: home });
    assert.match(fs.readFileSync(releaseConfig, 'utf8'), /\[mcp_servers\.external_subagent\]/u);

    const debugConfig = path.join(home, 'debug.config.toml');
    const script = path.join(home, 'debug-install-mcp.mjs');
    fs.writeFileSync(script, `
      process.env.EXTERNAL_SUBAGENT_VARIANT = 'debug';
      const { installMcp } = await import(${moduleUrl('cli/install/codex.mjs')});
      const { productPaths } = await import(${moduleUrl('cli/paths.mjs')});
      installMcp(productPaths(${JSON.stringify(home)}), { configPath: ${JSON.stringify(debugConfig)}, codexHome: ${JSON.stringify(home)}, skipNativeProbe: true });
    `);
    const run = spawnSync(process.execPath, [script], { encoding: 'utf8' });
    assert.equal(run.status, 0, run.stderr);
    const toml = fs.readFileSync(debugConfig, 'utf8');
    assert.match(toml, /\[mcp_servers\.external_subagent_debug\]/u);
    assert.doesNotMatch(toml, /\[mcp_servers\.external_subagent\]/u);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('zcode binding accepts the debug plugin beside the release plugin', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'es-variant-zcode-'));
  const debugSource = stageDebugPlugin(repoRoot);
  try {
    const config = path.join(home, '.zcode', 'cli', 'config.json');
    fs.mkdirSync(path.dirname(config), { recursive: true });
    const releasePaths = productPaths(home);
    const releaseStaging = path.join(home, 'stage-release');
    const release = installZcodePlugin(releasePaths, {
      source: pluginSourceRoot(),
      stagingPath: releaseStaging,
      configPath: config,
    });
    assert.equal(release.installed, true);

    const debugStaging = path.join(home, 'stage-debug');
    const script = path.join(home, 'debug-install-plugin.mjs');
    fs.writeFileSync(script, `
      process.env.EXTERNAL_SUBAGENT_VARIANT = 'debug';
      const { installZcodePlugin } = await import(${moduleUrl('cli/install/zcode.mjs')});
      const { productPaths } = await import(${moduleUrl('cli/paths.mjs')});
      const result = installZcodePlugin(productPaths(${JSON.stringify(home)}), {
        source: ${JSON.stringify(debugSource)},
        stagingPath: ${JSON.stringify(debugStaging)},
        configPath: ${JSON.stringify(config)},
      });
      process.stdout.write(JSON.stringify({ installed: result.installed, plugin_id: result.plugin_id }));
    `);
    const run = spawnSync(process.execPath, [script], { encoding: 'utf8' });
    assert.equal(run.status, 0, run.stderr);
    const debug = JSON.parse(run.stdout);
    assert.equal(debug.installed, true);
    assert.equal(debug.plugin_id, 'external-subagent-debug@inline');

    const doc = JSON.parse(fs.readFileSync(config, 'utf8'));
    assert.deepEqual(doc.plugins.dirs.sort(), [releaseStaging, debugStaging].sort());
    const releaseServer = JSON.parse(fs.readFileSync(path.join(releaseStaging, '.mcp.json'), 'utf8')).mcpServers.external_subagent;
    const debugServer = JSON.parse(fs.readFileSync(path.join(debugStaging, '.mcp.json'), 'utf8')).mcpServers.external_subagent;
    assert.equal(releaseServer.env.ZCODE_AGENTD_SOCKET, releasePaths.socket);
    assert.equal(debugServer.env.ZCODE_AGENTD_SOCKET, productPathsVariantDebug(home));
    assert.notEqual(releaseServer.env.ZCODE_AGENTD_SOCKET, debugServer.env.ZCODE_AGENTD_SOCKET);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(debugSource, { recursive: true, force: true });
  }
});

// Expected debug socket computed out-of-process so this test file stays a
// release-variant module; the string is stable by construction.
function productPathsVariantDebug(home) {
  return path.join(home, 'Library', 'Application Support', 'external-subagent-debug', 'external-subagent-debug.sock');
}
