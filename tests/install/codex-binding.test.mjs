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
import { nativeBinary, pluginSourceRoot } from '../../cli/install/layout.mjs';
import { productPaths } from '../../cli/paths.mjs';

const repoRoot = path.resolve(import.meta.dirname, '../..');

function fixtureHome(prefix = 'external-subagent-bind-') {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

// Minimal stand-in for the verified codex CLI surface.  Every invocation is
// appended to a JSONL log; responses mirror the real 0.153.4 JSON shapes and
// `plugin add` materializes the cache directory the real CLI writes.  With
// `store: true` the fake reproduces the machine-global content store: the
// first `plugin add` of a plugin@marketplace@version seeds the store from
// that caller's staged tree, and every later home materializes the STORE's
// bytes for the identity — not its own staged tree.  With `materialize:
// false` the fake reports `plugin add` success without writing any cache
// (the never-materialized counterexample); `reportPath: false` additionally
// omits installedPath from the success JSON.
function fakeCodexCli(directory, { store = false, materialize = true, reportPath = true } = {}) {
  const log = path.join(directory, 'codex-invocations.jsonl');
  const script = path.join(directory, 'codex-fake.mjs');
  fs.writeFileSync(script, `#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const log = process.env.FAKE_CODEX_LOG || path.join(path.dirname(fileURLToPath(import.meta.url)), 'codex-invocations.jsonl');
const stateDir = path.dirname(log);
const rootsFile = path.join(stateDir, 'marketplace-roots.json');
const storeRoot = path.join(stateDir, 'content-store');
const args = process.argv.slice(2);
fs.appendFileSync(log, JSON.stringify({ args, codex_home: process.env.CODEX_HOME }) + '\\n');
const text = (value) => { process.stdout.write(JSON.stringify(value, null, 2) + '\\n'); };
const loadRoots = () => { try { return JSON.parse(fs.readFileSync(rootsFile, 'utf8')); } catch { return {}; } };
if (args[0] === 'plugin' && args[1] === 'add' && args.includes('--help')) { process.stdout.write('usage: codex plugin add\\n'); process.exit(0); }
if (args[0] === 'plugin' && args[1] === 'marketplace' && args[2] === 'add') {
  const roots = loadRoots();
  roots[process.env.CODEX_HOME] = args[3];
  fs.writeFileSync(rootsFile, JSON.stringify(roots));
  text({ marketplaceName: 'personal', installedRoot: args[3], alreadyAdded: false });
  process.exit(0);
}
if (args[0] === 'plugin' && args[1] === 'add') {
  const name = args[2];
  const marketplace = args[args.indexOf('--marketplace') + 1];
  const root = loadRoots()[process.env.CODEX_HOME];
  if (!root) { process.stderr.write('no marketplace registered for this CODEX_HOME\\n'); process.exit(1); }
  const doc = JSON.parse(fs.readFileSync(path.join(root, '.agents', 'plugins', 'marketplace.json'), 'utf8'));
  const entry = doc.plugins.find((plugin) => plugin.name === name);
  const staging = path.resolve(root, entry.source.path);
  const version = JSON.parse(fs.readFileSync(path.join(staging, '.codex-plugin', 'plugin.json'), 'utf8')).version;
  const storeKey = path.join(storeRoot, marketplace, name, String(version));
  if (${store} && !fs.existsSync(storeKey)) {
    fs.mkdirSync(path.dirname(storeKey), { recursive: true });
    fs.cpSync(staging, storeKey, { recursive: true });
  }
  const cache = path.join(process.env.CODEX_HOME || '', 'plugins', 'cache', marketplace, name, String(version));
  ${materialize ? `
  fs.rmSync(cache, { recursive: true, force: true });
  fs.mkdirSync(path.dirname(cache), { recursive: true });
  fs.cpSync(${store} ? storeKey : staging, cache, { recursive: true });` : `
  /* cache-less success: report the install without materializing the cache */`}
  const reported = ${reportPath} ? { installedPath: cache } : {};
  text({ pluginId: name + '@' + marketplace, name, marketplaceName: marketplace, version,
    authPolicy: 'ON_INSTALL', ...reported });
  process.exit(0);
}
if (args[0] === 'plugin' && args[1] === 'remove') {
  const [name, marketplace] = String(args[2]).split('@');
  if (!marketplace) { process.stderr.write('plugin requires --marketplace unless passed as <plugin>@<marketplace>\\n'); process.exit(1); }
  fs.rmSync(path.join(process.env.CODEX_HOME || '', 'plugins', 'cache', marketplace, name), { recursive: true, force: true });
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

  // The materialized cache is read back and verified against the staged
  // binding before installPlugin reports success.
  assert.equal(result.cache_verified, true);
  assert.ok(result.cache.startsWith(path.join(home, '.codex')));
  const cacheMcp = JSON.parse(fs.readFileSync(path.join(result.cache, '.mcp.json'), 'utf8'));
  assert.equal(cacheMcp.mcpServers.external_subagent.command, nativeBinary('external-subagent-mcp'));
  assert.equal(cacheMcp.mcpServers.external_subagent.env.ZCODE_AGENTD_SOCKET, paths.socket);
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

// The store-backed fake below reproduces the 0.153.4 machine-global content
// store: one shared store across every CODEX_HOME, keyed by
// plugin@marketplace@version, seeded by whichever binding was installed
// first.  These tests pin the fail-closed answer to store reuse.
test('a second binding of the same plugin identity fails closed on store-reused cache bytes', () => {
  const state = fixtureHome('external-subagent-store-');
  const fake = fakeCodexCli(state, { store: true });
  const homeA = fixtureHome('external-subagent-bind-a-');
  const homeB = fixtureHome('external-subagent-bind-b-');
  const codexA = path.join(state, 'codex-a');
  const codexB = path.join(state, 'codex-b');
  try {
    const pathsA = productPaths(homeA);
    const pathsB = productPaths(homeB);
    const first = installPlugin(pathsA, { codexCli: fake.cli, codexHome: codexA });
    assert.equal(first.cache_verified, true);
    assert.equal(
      JSON.parse(fs.readFileSync(path.join(first.cache, '.mcp.json'), 'utf8')).mcpServers.external_subagent.env.ZCODE_AGENTD_SOCKET,
      pathsA.socket,
    );

    // Same plugin@marketplace@version, different valid binding (different
    // staging root and daemon socket): the store hands home B binding A's
    // bytes, and the install must fail closed instead of reporting success.
    let failure = null;
    try { installPlugin(pathsB, { codexCli: fake.cli, codexHome: codexB }); } catch (error) { failure = error; }
    assert.ok(failure, 'the store-reused install must not succeed');
    assert.equal(failure.code, 'CODEX_CACHE_BINDING_MISMATCH');
    assert.match(failure.message, /machine-global content store/u);
    assert.match(failure.message, new RegExp(pathsB.socket.replace(/[/\\]/gu, '\\$&'), 'u'));

    // Fail-closed rollback of product-owned state from this run...
    assert.equal(fs.existsSync(path.join(homeB, 'plugins', 'external-subagent')), false, 'the second binding\'s staging is rolled back');
    assert.equal(fs.existsSync(path.join(homeB, '.agents', 'plugins', 'marketplace.json')), false, 'the second binding\'s marketplace is rolled back');
    // ...while the codex-owned cache is left exactly as codex wrote it: it
    // still carries binding A's bytes, proving both the reuse and that the
    // product never edits the cache it rejected.
    const reused = JSON.parse(fs.readFileSync(path.join(codexB, 'plugins', 'cache', 'personal', 'external-subagent', JSON.parse(fs.readFileSync(path.join(first.cache, '.codex-plugin', 'plugin.json'), 'utf8')).version, '.mcp.json'), 'utf8'));
    assert.equal(reused.mcpServers.external_subagent.env.ZCODE_AGENTD_SOCKET, pathsA.socket);
    assert.equal(pathsA.socket === pathsB.socket, false, 'the two bindings must differ for this oracle to mean anything');
  } finally {
    for (const dir of [state, homeA, homeB]) fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('store reuse is answered by a distinct release identity, and a repeat of the same binding verifies', () => {
  const state = fixtureHome('external-subagent-store-ok-');
  const fake = fakeCodexCli(state, { store: true });
  const homeA = fixtureHome('external-subagent-bind-repeat-');
  const homeB = fixtureHome('external-subagent-bind-bump-');
  const codexA = path.join(state, 'codex-a');
  const codexB = path.join(state, 'codex-b');
  const sourceB = path.join(state, 'source-b');
  try {
    const pathsA = productPaths(homeA);
    const pathsB = productPaths(homeB);
    installPlugin(pathsA, { codexCli: fake.cli, codexHome: codexA });
    // Repeating the SAME binding hits the store's identical bytes and still
    // verifies against this run's staged tree.
    const repeat = installPlugin(pathsA, { codexCli: fake.cli, codexHome: codexA });
    assert.equal(repeat.cache_verified, true);
    assert.equal(
      JSON.parse(fs.readFileSync(path.join(repeat.cache, '.mcp.json'), 'utf8')).mcpServers.external_subagent.env.ZCODE_AGENTD_SOCKET,
      pathsA.socket,
    );

    // A second binding with a DISTINCT plugin version identity gets its own
    // store entry and installs cleanly — the documented remediation.
    fs.cpSync(pluginSourceRoot(), sourceB, { recursive: true });
    const manifestFile = path.join(sourceB, '.codex-plugin', 'plugin.json');
    const manifest = JSON.parse(fs.readFileSync(manifestFile, 'utf8'));
    const [major, minor, patch] = manifest.version.split('.').map(Number);
    manifest.version = `${major}.${minor}.${patch + 1}`;
    fs.writeFileSync(manifestFile, `${JSON.stringify(manifest, null, 2)}\n`);
    const second = installPlugin(pathsB, { source: sourceB, codexCli: fake.cli, codexHome: codexB });
    assert.equal(second.cache_verified, true);
    assert.equal(path.basename(second.cache), manifest.version, 'the distinct identity materializes its own cache directory');
    assert.equal(
      JSON.parse(fs.readFileSync(path.join(second.cache, '.mcp.json'), 'utf8')).mcpServers.external_subagent.env.ZCODE_AGENTD_SOCKET,
      pathsB.socket,
      'the second home\'s cache carries the second binding, not the first\'s',
    );
  } finally {
    for (const dir of [state, homeA, homeB]) fs.rmSync(dir, { recursive: true, force: true });
  }
});

// P1 counterexample: `plugin add` reports success but no cache is ever
// materialized — neither the reported installedPath (when the CLI claims
// one) nor the derived cache location.  installPlugin must fail closed with
// CODEX_CACHE_UNVERIFIABLE, roll the product-owned state of this run back,
// and neither the install nor a later reconcile may record success for it.
test('a plugin-add success with no materialized cache fails closed and records no success', () => {
  const home = fixtureHome('external-subagent-nocache-');
  const liedDir = path.join(home, 'fake-lied');
  const silentDir = path.join(home, 'fake-silent');
  fs.mkdirSync(liedDir, { recursive: true, mode: 0o700 });
  fs.mkdirSync(silentDir, { recursive: true, mode: 0o700 });
  const lying = fakeCodexCli(liedDir, { materialize: false });          // reports installedPath it never wrote
  const silent = fakeCodexCli(silentDir, { materialize: false, reportPath: false }); // reports success with no path
  const paths = productPaths(home);
  const codexHome = path.join(home, 'codex-target');
  fs.mkdirSync(codexHome, { recursive: true, mode: 0o700 });
  const staging = path.join(home, 'plugins', 'external-subagent');
  const marketplace = path.join(home, '.agents', 'plugins', 'marketplace.json');
  try {
    for (const fake of [lying, silent]) {
      let failure = null;
      try { installPlugin(paths, { codexCli: fake.cli, codexHome }); } catch (error) { failure = error; }
      assert.ok(failure, 'a cache-less plugin-add success must not install');
      assert.equal(failure.code, 'CODEX_CACHE_UNVERIFIABLE');
      assert.match(failure.message, /external-subagent@personal@\d+\.\d+\.\d+/u, 'the error names the unverifiable identity');
      assert.match(failure.message, /no plugin cache was materialized/u);
      assert.ok(failure.message.includes(codexHome), 'the error shows where the cache was expected');
      assert.equal(fs.existsSync(staging), false, 'the failed install rolls its staging back');
      assert.equal(fs.existsSync(marketplace), false, 'the failed install rolls its marketplace back');
    }

    // Reconcile over the same unverifiable install: the home is recorded as
    // failed — never updated — and the prior claim's digest survives.
    registerCodexHome(paths, codexHome, { version: '0.1.0', digest: 'sentinel-digest', status: 'claimed' });
    const report = reconcileCodexHomes(paths, { codexCli: silent.cli });
    assert.equal(report.homes.length, 1);
    assert.equal(report.homes[0].status, 'failed');
    assert.equal(report.homes[0].error.code, 'CODEX_CACHE_UNVERIFIABLE');
    assert.equal(report.all_updated, false);
    const entry = loadCodexHomes(paths).registry.homes[0];
    assert.equal(entry.last_status, 'failed', 'reconcile must not record updated for an unverifiable install');
    assert.equal(entry.digest, 'sentinel-digest', 'a failed sync must not overwrite the recorded digest');
    assert.equal(entry.last_sync_ms, null, 'a failed sync must not record a sync timestamp');
    assert.equal(fs.existsSync(staging), false, 'the failed reconcile attempt also rolls its staging back');
    assert.equal(fs.existsSync(marketplace), false, 'the failed reconcile attempt also rolls its marketplace back');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
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

// codex 0.153.4 materializes a `plugin add` cache from a machine-global
// content store keyed by the plugin identity: when the user's real ~/.codex
// already caches the same plugin@marketplace@version, an isolated CODEX_HOME
// still receives the REAL installation's bytes (observed live: the cached
// .mcp.json carried the real-home socket while the staged tree carried the
// throwaway socket).  installPlugin now reads the cache back and fails
// closed on that mismatch; the real-CLI oracle below additionally pins the
// fresh-machine contract, so it only runs where store dedupe cannot turn
// the run into the mismatch path it now shares with the unit oracle above.
const realCodexCacheConflict = fs.existsSync(path.join(os.homedir(), '.codex', 'plugins', 'cache', 'personal', 'external-subagent'));

test('real codex CLI binds a throwaway CODEX_HOME when explicitly available', { skip: !(process.platform === 'darwin' && process.env.EXTERNAL_SUBAGENT_TEST_REAL_CODEX !== '0' && !realCodexCacheConflict && spawnSync('codex', ['--version'], { encoding: 'utf8' }).status === 0) }, () => {
  const home = fixtureHome('external-subagent-real-');
  const codexHome = path.join(home, 'codex-home');
  fs.mkdirSync(codexHome, { recursive: true, mode: 0o700 });
  const paths = productPaths(home);
  const result = installPlugin(paths, { codexHome, env: { CODEX_HOME: codexHome } });
  assert.equal(result.installed, true);
  assert.equal(result.cache_verified, true, 'the real materialized cache must verify against the staged binding');
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
