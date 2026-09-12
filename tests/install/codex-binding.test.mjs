// S05 managed Codex binding and the D08 global Codex-homes registry.
//
// The binding oracles pin the verified codex-cli 0.153.4 interface (captured in
// docs/compatibility/codex.md): `plugin marketplace add <root> --json`,
// `plugin add <name> --marketplace <marketplace> --json`, and
// `plugin remove <name>@<marketplace> --json`.  A recording fake CLI stands in
// for codex so the tests never touch a real Codex installation; one opt-out
// gated test exercises the real CLI against a throwaway CODEX_HOME only.
// The registry oracles pin D08: claim on successful install, unclaim on
// removal, idempotent dedupe, atomic corruption recovery, and the guarantee
// that only registered, writable homes are ever written.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { CODEX_TOOLS, installMcp, installPlugin, uninstallPlugin } from '../../cli/install/codex.mjs';
import {
  codexHomesRegistryPath,
  loadCodexHomes,
  reconcileCodexHomes,
  registerCodexHome,
  unregisterCodexHome,
} from '../../cli/install/reconcile.mjs';
import { nativeBinary } from '../../cli/install/layout.mjs';
import { productPaths } from '../../cli/paths.mjs';

const repoRoot = path.resolve(import.meta.dirname, '../..');

function fixtureHome(prefix = 'external-subagent-bind-') {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

// Minimal stand-in for the verified codex CLI surface.  Every invocation is
// appended to a JSONL log; responses mirror the real 0.153.4 JSON shapes.
function fakeCodexCli(directory) {
  const log = path.join(directory, 'codex-invocations.jsonl');
  const script = path.join(directory, 'codex-fake.mjs');
  fs.writeFileSync(script, `#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const log = process.env.FAKE_CODEX_LOG || path.join(path.dirname(fileURLToPath(import.meta.url)), 'codex-invocations.jsonl');
const args = process.argv.slice(2);
fs.appendFileSync(log, JSON.stringify({ args, codex_home: process.env.CODEX_HOME }) + '\\n');
const text = (value) => { process.stdout.write(JSON.stringify(value, null, 2) + '\\n'); };
const join = (sub, rest) => [sub, ...rest];
if (args[0] === 'plugin' && args[1] === 'add' && args.includes('--help')) { process.stdout.write('usage: codex plugin add\\n'); process.exit(0); }
if (args[0] === 'plugin' && args[1] === 'marketplace' && args[2] === 'add') {
  text({ marketplaceName: 'personal', installedRoot: args[3], alreadyAdded: false });
  process.exit(0);
}
if (args[0] === 'plugin' && args[1] === 'add') {
  const name = args[2];
  const marketplace = args[args.indexOf('--marketplace') + 1];
  text({ pluginId: name + '@' + marketplace, name, marketplaceName: marketplace, version: '0.1.0',
    installedPath: path.join(process.env.CODEX_HOME || '', 'plugins', 'cache', marketplace, name, '0.1.0'), authPolicy: 'ON_INSTALL' });
  process.exit(0);
}
if (args[0] === 'plugin' && args[1] === 'remove') {
  const [name, marketplace] = String(args[2]).split('@');
  if (!marketplace) { process.stderr.write('plugin requires --marketplace unless passed as <plugin>@<marketplace>\\n'); process.exit(1); }
  text({ pluginId: name + '@' + marketplace, name, marketplaceName: marketplace });
  process.exit(0);
}
process.stderr.write('unexpected codex invocation: ' + JSON.stringify(args) + '\\n');
process.exit(1);
`);
  fs.chmodSync(script, 0o755);
  return { cli: script, log };
}

function invocations(log) {
  return fs.readFileSync(log, 'utf8').trim().split('\n').filter(Boolean).map((line) => JSON.parse(line));
}

function binding(home, extra = {}) {
  const paths = productPaths(home);
  const codexHome = extra.codexHome || path.join(home, '.codex');
  return { paths, options: { codexCli: extra.cli, codexHome, ...extra.options } };
}

test('managed plugin install stages a PATH-independent MCP binding via the official interface', () => {
  const home = fixtureHome();
  const fake = fakeCodexCli(home);
  const { paths, options } = binding(home, { cli: fake.cli });
  const result = installPlugin(paths, options);
  assert.equal(result.installed, true);
  assert.equal(result.marketplace_name, 'personal');

  const staging = path.join(home, 'plugins', 'external-subagent');
  const manifest = JSON.parse(fs.readFileSync(path.join(staging, '.codex-plugin', 'plugin.json'), 'utf8'));
  assert.equal(manifest.name, 'external-subagent');
  const mcp = JSON.parse(fs.readFileSync(path.join(staging, '.mcp.json'), 'utf8'));
  const server = mcp.mcpServers.external_subagent;
  assert.equal(server.command, nativeBinary('external-subagent-mcp'));
  assert.ok(path.isAbsolute(server.command), 'MCP entry must be an absolute stable path');
  assert.ok(!server.command.includes('~') && !server.command.includes('nvm'), 'MCP entry must not depend on user PATH fragments');
  assert.equal(server.env.ZCODE_AGENTD_SOCKET, paths.socket);

  const marketplace = JSON.parse(fs.readFileSync(path.join(home, '.agents', 'plugins', 'marketplace.json'), 'utf8'));
  assert.equal(marketplace.plugins.filter((entry) => entry.name === 'external-subagent').length, 1);

  const calls = invocations(fake.log);
  const addMarketplace = calls.find((call) => call.args[1] === 'marketplace');
  assert.deepEqual(addMarketplace.args, ['plugin', 'marketplace', 'add', home, '--json']);
  const add = calls.find((call) => call.args[0] === 'plugin' && call.args[1] === 'add' && !call.args.includes('--help'));
  assert.deepEqual(add.args.slice(0, 2), ['plugin', 'add']);
  assert.equal(add.args[2], 'external-subagent');
  assert.ok(add.args.includes('--marketplace'), 'plugin add must select the marketplace explicitly');
  assert.ok(add.args.includes('--json'));
  assert.equal(add.codex_home, path.join(home, '.codex'), 'CODEX_HOME must confine the install');
  fs.rmSync(home, { recursive: true, force: true });
});

test('marketplace merge preserves unrelated plugins and repeats stay idempotent', () => {
  const home = fixtureHome('external-subagent-merge-');
  const fake = fakeCodexCli(home);
  const { paths, options } = binding(home, { cli: fake.cli });
  const marketplacePath = path.join(home, '.agents', 'plugins', 'marketplace.json');
  fs.mkdirSync(path.dirname(marketplacePath), { recursive: true, mode: 0o700 });
  const unrelated = {
    name: 'personal',
    interface: { displayName: 'Personal' },
    plugins: [{ name: 'other-tool', source: { source: 'local', path: './plugins/other-tool' }, policy: { installation: 'AVAILABLE', authentication: 'ON_INSTALL' }, category: 'Productivity' }],
  };
  fs.writeFileSync(marketplacePath, `${JSON.stringify(unrelated, null, 2)}\n`, { mode: 0o600 });

  installPlugin(paths, options);
  installPlugin(paths, options);
  const marketplace = JSON.parse(fs.readFileSync(marketplacePath, 'utf8'));
  assert.equal(marketplace.plugins.length, 2, 'unrelated marketplace entries must survive');
  assert.ok(marketplace.plugins.some((entry) => entry.name === 'other-tool'));
  assert.equal(marketplace.plugins.filter((entry) => entry.name === 'external-subagent').length, 1, 'repeat install must not duplicate the managed entry');
  const stagingChildren = fs.readdirSync(path.join(home, 'plugins', 'external-subagent'));
  assert.ok(stagingChildren.includes('.mcp.json'));
  fs.rmSync(home, { recursive: true, force: true });
});

test('foreign staging and drifted marketplace entries are rejected, not overwritten', () => {
  const home = fixtureHome('external-subagent-drift-');
  const fake = fakeCodexCli(home);
  const { paths, options } = binding(home, { cli: fake.cli });
  const staging = path.join(home, 'plugins', 'external-subagent');
  fs.mkdirSync(path.join(staging, '.codex-plugin'), { recursive: true, mode: 0o700 });
  fs.writeFileSync(path.join(staging, '.codex-plugin', 'plugin.json'), JSON.stringify({ name: 'someone-elses' }));
  assert.throws(() => installPlugin(paths, options), (error) => error.code === 'PLUGIN_STAGING_CONFLICT');
  fs.rmSync(staging, { recursive: true, force: true });

  const marketplacePath = path.join(home, '.agents', 'plugins', 'marketplace.json');
  fs.mkdirSync(path.dirname(marketplacePath), { recursive: true, mode: 0o700 });
  fs.writeFileSync(marketplacePath, JSON.stringify({
    name: 'personal',
    plugins: [{ name: 'external-subagent', source: { source: 'local', path: './plugins/not-ours' } }],
  }));
  assert.throws(() => installPlugin(paths, options), (error) => error.code === 'PLUGIN_MARKETPLACE_CONFLICT');
  assert.equal(JSON.parse(fs.readFileSync(marketplacePath, 'utf8')).plugins[0].source.path, './plugins/not-ours');
  fs.rmSync(home, { recursive: true, force: true });
});

test('uninstall uses the verified name@marketplace removal form', () => {
  const home = fixtureHome('external-subagent-remove-');
  const fake = fakeCodexCli(home);
  const { paths, options } = binding(home, { cli: fake.cli });
  installPlugin(paths, options);
  const result = uninstallPlugin(paths, options);
  assert.equal(result.uninstalled, true);
  const remove = invocations(fake.log).find((call) => call.args[1] === 'remove');
  assert.deepEqual(remove.args, ['plugin', 'remove', 'external-subagent@personal', '--json']);
  fs.rmSync(home, { recursive: true, force: true });
});

test('direct MCP TOML binding covers all ten tools and removes only its own section', () => {
  const home = fixtureHome('external-subagent-mcp-');
  const paths = productPaths(home);
  const config = path.join(home, 'codex-config.toml');
  fs.writeFileSync(config, '[mcp_servers.user_owned]\ncommand = "/usr/local/bin/keep"\nenabled = true\n', { mode: 0o600 });
  const installed = installMcp(paths, { configPath: config, dryRun: false, skipNativeProbe: true });
  assert.equal(installed.installed, true);
  const text = fs.readFileSync(config, 'utf8');
  assert.equal(CODEX_TOOLS.length, 10, 'the product exposes exactly ten MCP tools');
  for (const tool of CODEX_TOOLS) assert.ok(text.includes(`"${tool}"`), `enabled_tools must include ${tool}`);
  assert.ok(text.includes('external_subagent_observe'), 'observe must be advertised');
  assert.match(text, /\[mcp_servers\.user_owned\]/u, 'unrelated MCP servers must survive');
  assert.match(text, /command = "\/usr\/local\/bin\/keep"/u);
  const removed = installMcp(paths, { configPath: config, dryRun: false, uninstall: true, skipNativeProbe: true });
  assert.equal(removed.uninstalled, true);
  const after = fs.readFileSync(config, 'utf8');
  assert.doesNotMatch(after, /external_subagent/u, 'only the managed section is removed');
  assert.match(after, /mcp_servers\.user_owned/u);
  fs.rmSync(home, { recursive: true, force: true });
});

test('D08 registry claims, dedupes, and unclaims Codex homes', () => {
  const home = fixtureHome('external-subagent-registry-');
  const paths = productPaths(home);
  const codexA = path.join(home, 'codex-a');
  const codexB = path.join(home, 'codex-b');
  fs.mkdirSync(codexA, { recursive: true, mode: 0o700 });
  fs.mkdirSync(codexB, { recursive: true, mode: 0o700 });

  const first = registerCodexHome(paths, codexA, { version: '0.1.0', status: 'claimed' });
  assert.equal(first.registered, true);
  assert.equal(first.deduplicated, false);
  const again = registerCodexHome(paths, path.join(codexA, 'nested', '..'), { version: '0.1.0', status: 'claimed' });
  assert.equal(again.deduplicated, true);
  registerCodexHome(paths, codexB, { version: '0.1.0', status: 'claimed' });
  const registry = loadCodexHomes(paths).registry;
  assert.deepEqual(registry.homes.map((entry) => path.basename(entry.home)).sort(), ['codex-a', 'codex-b'], 'duplicate homes collapse to one entry');
  assert.ok(registry.homes.every((entry) => Number.isInteger(entry.claimed_at_ms) && typeof entry.version === 'string'));

  const removed = unregisterCodexHome(paths, codexA);
  assert.equal(removed.unregistered, true);
  assert.equal(unregisterCodexHome(paths, codexA).unregistered, false);
  assert.deepEqual(loadCodexHomes(paths).registry.homes.map((entry) => path.basename(entry.home)), ['codex-b']);
  fs.rmSync(home, { recursive: true, force: true });
});

test('D08 registry replaces corrupted JSON atomically and reports a recoverable error', () => {
  const home = fixtureHome('external-subagent-corrupt-');
  const paths = productPaths(home);
  const file = codexHomesRegistryPath(paths);
  const corrupted = Buffer.from('{ this is not json');
  fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  fs.writeFileSync(file, corrupted, { mode: 0o600 });

  const loaded = loadCodexHomes(paths);
  assert.equal(loaded.recovery.code, 'CODEX_HOMES_REGISTRY_CORRUPT');
  assert.equal(loaded.recovery.status, 'recovered');
  assert.deepEqual(loadCodexHomes(paths).registry.homes, [], 'replacement registry starts empty');
  assert.ok(fs.existsSync(loaded.recovery.backup), 'corrupted bytes are preserved for inspection');
  assert.deepEqual(fs.readFileSync(loaded.recovery.backup), corrupted);

  const registered = registerCodexHome(paths, path.join(home, '.codex'), { version: '0.1.0' });
  assert.equal(registered.registered, true);
  assert.equal(loadCodexHomes(paths).registry.homes.length, 1, 'registry remains usable after recovery');
  fs.rmSync(home, { recursive: true, force: true });
});

test('D08 reconcile updates only registered writable homes and never touches strangers', () => {
  const home = fixtureHome('external-subagent-reconcile-');
  const fake = fakeCodexCli(home);
  const { paths } = binding(home, { cli: fake.cli });
  const writableHome = path.join(home, 'codex-registered');
  const readonlyHome = path.join(home, 'codex-readonly');
  const strangerHome = path.join(home, 'codex-stranger');
  for (const dir of [writableHome, readonlyHome, strangerHome]) fs.mkdirSync(dir, { recursive: true, mode: 0o700 });

  registerCodexHome(paths, writableHome, { version: '0.1.0' });
  registerCodexHome(paths, readonlyHome, { version: '0.1.0' });
  fs.chmodSync(readonlyHome, 0o500);

  const report = reconcileCodexHomes(paths, { codexCli: fake.cli });
  const byHome = Object.fromEntries(report.homes.map((entry) => [path.basename(entry.home), entry]));
  assert.equal(byHome['codex-registered'].status, 'updated');
  assert.equal(byHome['codex-readonly'].status, 'skipped_not_writable', 'unwritable homes are skipped, not failed globally');
  assert.equal(report.all_updated, false, 'a skipped home must not be summarized as full success');

  assert.equal(fs.readdirSync(strangerHome).length, 0, 'an unregistered home is never written');
  const updatedHomes = invocations(fake.log).filter((call) => call.args[1] === 'add' && !call.args.includes('--help'));
  assert.deepEqual(updatedHomes.map((call) => path.basename(call.codex_home)), ['codex-registered']);

  const registry = loadCodexHomes(paths).registry;
  const statuses = Object.fromEntries(registry.homes.map((entry) => [path.basename(entry.home), entry.last_status]));
  assert.equal(statuses['codex-registered'], 'updated');
  assert.equal(statuses['codex-readonly'], 'skipped_not_writable');
  fs.chmodSync(readonlyHome, 0o700);
  fs.rmSync(home, { recursive: true, force: true });
});

test('real codex CLI binds a throwaway CODEX_HOME when explicitly available', { skip: !(process.platform === 'darwin' && process.env.EXTERNAL_SUBAGENT_TEST_REAL_CODEX !== '0' && spawnSync('codex', ['--version'], { encoding: 'utf8' }).status === 0) }, () => {
  const home = fixtureHome('external-subagent-real-');
  const codexHome = path.join(home, 'codex-home');
  fs.mkdirSync(codexHome, { recursive: true, mode: 0o700 });
  const paths = productPaths(home);
  const result = installPlugin(paths, { codexHome, env: { CODEX_HOME: codexHome } });
  assert.equal(result.installed, true);
  assert.ok(result.cache, 'codex reports the installed cache path');
  assert.ok(result.cache.startsWith(fs.realpathSync(codexHome)), 'the plugin cache must live inside the claimed CODEX_HOME');
  const cacheMcp = JSON.parse(fs.readFileSync(path.join(result.cache, '.mcp.json'), 'utf8'));
  assert.equal(cacheMcp.mcpServers.external_subagent.command, nativeBinary('external-subagent-mcp'), 'cached copy keeps the absolute stable MCP entry');
  assert.equal(cacheMcp.mcpServers.external_subagent.env.ZCODE_AGENTD_SOCKET, paths.socket);
  const removed = uninstallPlugin(paths, { codexHome, env: { CODEX_HOME: codexHome } });
  assert.equal(removed.uninstalled, true);
  assert.equal(fs.readdirSync(path.join(fs.realpathSync(codexHome), 'plugins', 'cache', 'personal')).length, 0, 'official remove clears the cache');
  fs.rmSync(home, { recursive: true, force: true });
});
