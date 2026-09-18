// S05 managed Codex binding and the D08 global Codex-homes registry.
//
// The binding oracles pin the verified codex-cli interface (JSON shapes
// verified on 0.153.4; add/marketplace paths re-verified on 0.154.0 —
// captured in docs/compatibility/codex.md): `plugin marketplace add
// <root> --json`, `plugin add <name> --marketplace <marketplace> --json`,
// and `plugin remove <name>@<marketplace> --json`.  A recording fake CLI
// stands in for codex so the tests never touch a real Codex installation;
// one opt-out gated test exercises the real CLI against a throwaway
// CODEX_HOME only.
// The registry oracles pin D08: claim on successful install, unclaim on
// removal, idempotent dedupe, atomic corruption recovery, and the guarantee
// that only registered, writable homes are ever written.  The AUD-010
// store oracles pin that a same-identity cache whose MANAGED CONTENT is not
// this candidate's (older SKILL, missing file, retained source-deleted file)
// fails closed with CODEX_CACHE_CONTENT_MISMATCH and never records success,
// while a never-used identity with correct materialized content verifies.
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

// Byte snapshot of every regular file under a root, so last-good-tree
// claims compare real bytes instead of mere file existence.
function snapshotTree(root) {
  const files = new Map();
  if (!fs.existsSync(root)) return files;
  const walk = (dir) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else if (entry.isFile()) files.set(path.relative(root, full), fs.readFileSync(full));
    }
  };
  walk(root);
  return files;
}

// Throwaway copy of the shipped plugin source a test can mutate without
// touching the repository tree.
function tempSource(tweak) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-source-'));
  fs.cpSync(pluginSourceRoot(), dir, { recursive: true });
  if (tweak) tweak(dir);
  return dir;
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
// first.  codex 0.154.0 no longer behaves this way for non-reserved
// marketplace names (it re-materializes each add from the registered root;
// the reserved name `personal` instead resolves machine-globally to the
// real user root), so these oracles pin the fail-closed answer to store
// reuse as deliberate defense against freeze-behavior CLIs and future
// regressions, not as a model of live 0.154.0 non-reserved adds.
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
    assert.match(failure.message, /machine-global content store/u, 'the store-reuse cause stays named');
    assert.match(failure.message, /release a distinct plugin version/u, 'the store-reuse remediation stays named');
    assert.match(failure.message, /reserved marketplace name personal/u, 'the reserved-name cause stays named');
    assert.match(failure.message, /non-reserved marketplace name in isolated homes/u, 'the reserved-name remediation stays named');
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

// AUD-010: same plugin identity, same marketplace, same binding (same home,
// socket, and facade command) must still fail closed when the store's cached
// managed CONTENT is not this candidate's.  The store fake seeds the
// machine-global store from the FIRST install's staged tree, so the second
// install of the same identity materializes the old bytes while its own
// staging carries the new candidate — exactly the shape the metadata/binding
// comparison alone used to accept as `cache_verified: true`.  Each negative
// pins exactly ONE difference between the seeded bytes and the candidate.
test('a same-identity cache with changed managed content fails closed through the store (AUD-010)', () => {
  const shippedVersion = JSON.parse(fs.readFileSync(path.join(pluginSourceRoot(), '.codex-plugin', 'plugin.json'), 'utf8')).version;
  const skillAt = (root) => path.join(root, 'skills', 'external-subagent', 'SKILL.md');
  const negatives = [
    {
      name: 'older cached SKILL',
      seed: (dir) => fs.writeFileSync(skillAt(dir), 'OLD SKILL bytes\n'),
      candidate: (dir) => fs.writeFileSync(skillAt(dir), 'NEW SKILL bytes\n'),
      staleProof: (cache) => assert.equal(fs.readFileSync(skillAt(cache), 'utf8'), 'OLD SKILL bytes\n', 'the cache keeps the older SKILL'),
    },
    {
      name: 'managed file missing from the cache',
      seed: () => {},
      candidate: (dir) => fs.writeFileSync(path.join(dir, 'skills', 'external-subagent', 'reference.md'), 'new managed file\n'),
      staleProof: (cache) => assert.equal(fs.existsSync(path.join(cache, 'skills', 'external-subagent', 'reference.md')), false, 'the cache lacks the candidate\'s new managed file'),
    },
    {
      name: 'source-deleted file the cache still retains',
      seed: (dir) => fs.writeFileSync(path.join(dir, 'obsolete-managed-file.txt'), 'stale retained bytes\n'),
      // The candidate is a fresh copy of the shipped source, which never
      // carried the seeded file — its absence IS the source deletion.
      candidate: () => {},
      staleProof: (cache) => assert.equal(fs.existsSync(path.join(cache, 'obsolete-managed-file.txt')), true, 'the cache retains the file this candidate deleted'),
    },
  ];
  for (const negative of negatives) {
    const state = fixtureHome('external-subagent-aud010-');
    const fake = fakeCodexCli(state, { store: true });
    const home = fixtureHome('external-subagent-aud010-home-');
    const codexHome = path.join(state, 'codex-home');
    const staging = path.join(home, 'plugins', 'external-subagent');
    const marketplace = path.join(home, '.agents', 'plugins', 'marketplace.json');
    try {
      const paths = productPaths(home);
      const options = { codexCli: fake.cli, codexHome };
      const first = installPlugin(paths, { ...options, source: tempSource(negative.seed) });
      assert.equal(first.cache_verified, true, `${negative.name}: the seeding install of the old content succeeds`);
      const treeBefore = snapshotTree(staging);
      const marketBefore = fs.readFileSync(marketplace);

      // Same identity (version untouched), same binding — only the managed
      // content moved.  The store hands back the old bytes; the install must
      // fail closed instead of reporting this candidate as verified.
      let failure = null;
      try { installPlugin(paths, { ...options, source: tempSource(negative.candidate) }); } catch (error) { failure = error; }
      assert.ok(failure, `${negative.name}: the stale-content install must not succeed`);
      assert.equal(failure.code, 'CODEX_CACHE_CONTENT_MISMATCH');
      assert.match(failure.message, new RegExp(`external-subagent@personal@${shippedVersion.replace(/\./gu, '\\.')}\\b`, 'u'), 'the error names the reused identity');
      assert.match(failure.message, /release a distinct plugin version/u, 'the freeze-behavior remediation stays named');
      assert.match(failure.message, /codex 0\.153\.4 store behavior/u, 'the freeze-behavior cause stays scoped to the CLI shape that freezes');
      assert.match(failure.message, /refresh the registered marketplace root and re-add/u, 'the re-materializing-CLI remediation stays named');
      assert.ok(failure.message.includes(first.cache), 'the error shows the cache that was rejected');

      // The failed refresh restores the prior coherent staging/marketplace
      // bytes (S01), and the codex-owned cache is left exactly as codex wrote
      // it — still carrying the stale content this product rejected.
      assert.deepEqual(snapshotTree(staging), treeBefore, `${negative.name}: the prior staging tree is restored`);
      assert.deepEqual(fs.readFileSync(marketplace), marketBefore, `${negative.name}: the marketplace keeps its exact prior bytes`);
      negative.staleProof(first.cache);
    } finally {
      for (const dir of [state, home]) fs.rmSync(dir, { recursive: true, force: true });
    }
  }
});

// The same stale-content failure must not record success anywhere: reconcile
// reports the home as failed with CODEX_CACHE_CONTENT_MISMATCH, and the prior
// claim's digest survives untouched.
test('a stale-content reconcile records failure, never a successful claim (AUD-010)', () => {
  const state = fixtureHome('external-subagent-aud010-claim-');
  const fake = fakeCodexCli(state, { store: true });
  const home = fixtureHome('external-subagent-aud010-claim-home-');
  const codexHome = path.join(state, 'codex-home');
  const skillAt = path.join('skills', 'external-subagent', 'SKILL.md');
  try {
    const paths = productPaths(home);
    installPlugin(paths, { codexCli: fake.cli, codexHome, source: tempSource((dir) => fs.appendFileSync(path.join(dir, skillAt), '\nOLD CANDIDATE\n')) });
    registerCodexHome(paths, codexHome, { version: '0.1.0', digest: 'sentinel-digest', status: 'claimed' });
    const candidate = tempSource((dir) => fs.appendFileSync(path.join(dir, skillAt), '\nNEW CANDIDATE\n'));
    const report = reconcileCodexHomes(paths, { codexCli: fake.cli, source: candidate });
    assert.equal(report.homes.length, 1);
    assert.equal(report.homes[0].status, 'failed');
    assert.equal(report.homes[0].error.code, 'CODEX_CACHE_CONTENT_MISMATCH');
    assert.equal(report.all_updated, false);
    const entry = loadCodexHomes(paths).registry.homes[0];
    assert.equal(entry.last_status, 'failed', 'reconcile must not record updated for stale content');
    assert.equal(entry.digest, 'sentinel-digest', 'a failed sync must not overwrite the recorded digest');
    assert.equal(entry.last_sync_ms, null, 'a failed sync must not record a sync timestamp');
  } finally {
    for (const dir of [state, home]) fs.rmSync(dir, { recursive: true, force: true });
  }
});

// The positive counterpart: a plugin version identity no store has ever
// cached gets its own store entry seeded from THIS run's staged tree, so
// changed content under a fresh identity with the SAME binding verifies.
test('a never-used candidate identity with correct materialized content verifies (AUD-010)', () => {
  const state = fixtureHome('external-subagent-aud010-fresh-');
  const fake = fakeCodexCli(state, { store: true });
  const home = fixtureHome('external-subagent-aud010-fresh-home-');
  const codexHome = path.join(state, 'codex-home');
  const skillAt = path.join('skills', 'external-subagent', 'SKILL.md');
  try {
    const paths = productPaths(home);
    // Burn the shipped identity in the store with older content first.
    installPlugin(paths, { codexCli: fake.cli, codexHome, source: tempSource((dir) => fs.writeFileSync(path.join(dir, skillAt), 'OLD CANDIDATE SKILL\n')) });
    // The remediation: a distinct, never-cached plugin version carrying the
    // new content installs cleanly under the same binding.
    const source = tempSource((dir) => {
      fs.writeFileSync(path.join(dir, skillAt), 'FRESH CANDIDATE SKILL\n');
      const manifestFile = path.join(dir, '.codex-plugin', 'plugin.json');
      const manifest = JSON.parse(fs.readFileSync(manifestFile, 'utf8'));
      const [major, minor, patch] = manifest.version.split('.').map(Number);
      manifest.version = `${major}.${minor}.${patch + 1}`;
      fs.writeFileSync(manifestFile, `${JSON.stringify(manifest, null, 2)}\n`);
    });
    const result = installPlugin(paths, { codexCli: fake.cli, codexHome, source });
    assert.equal(result.installed, true);
    assert.equal(result.cache_verified, true);
    assert.equal(typeof result.digest, 'string', 'the successful install carries the digest the claim records');
    assert.equal(path.basename(result.cache), JSON.parse(fs.readFileSync(path.join(source, '.codex-plugin', 'plugin.json'), 'utf8')).version, 'the fresh identity materializes its own cache directory');
    assert.equal(fs.readFileSync(path.join(result.cache, skillAt), 'utf8'), 'FRESH CANDIDATE SKILL\n', 'the cache carries this candidate\'s content, not the store\'s previous bytes');
    const server = JSON.parse(fs.readFileSync(path.join(result.cache, '.mcp.json'), 'utf8')).mcpServers.external_subagent;
    assert.equal(server.env.ZCODE_AGENTD_SOCKET, paths.socket, 'the same binding verifies alongside the new content');
  } finally {
    for (const dir of [state, home]) fs.rmSync(dir, { recursive: true, force: true });
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

// AUD-002 regression oracles for the codex host: staging publishes by
// validated replacement, so a source file deleted since the last install
// disappears from the published tree, and a refresh whose verification
// fails restores the prior coherent staging tree and marketplace bytes
// instead of leaving an overlaid mix behind.
test('a codex refresh drops source files deleted since the last install (AUD-002)', () => {
  const home = fixtureHome('external-subagent-deletion-');
  const fake = fakeCodexCli(home);
  const { paths, options } = binding(home, { cli: fake.cli });
  const source = tempSource();
  const staging = path.join(home, 'plugins', 'external-subagent');
  try {
    fs.mkdirSync(path.join(home, 'plugins'), { recursive: true, mode: 0o700 });
    fs.writeFileSync(path.join(home, 'plugins', 'unrelated-user-file.txt'), 'keep me');
    installPlugin(paths, { ...options, source });
    const removed = path.join(staging, 'skills', 'external-subagent', 'SKILL.md');
    assert.equal(fs.existsSync(removed), true, 'the file is staged on first install');
    fs.rmSync(path.join(source, 'skills', 'external-subagent', 'SKILL.md'));
    const result = installPlugin(paths, { ...options, source });
    assert.equal(result.installed, true);
    assert.equal(fs.existsSync(removed), false, 'a file deleted from the source disappears from the published tree');
    const server = JSON.parse(fs.readFileSync(path.join(staging, '.mcp.json'), 'utf8')).mcpServers.external_subagent;
    assert.equal(server.command, nativeBinary('external-subagent-mcp'), 'the replacement tree still carries the managed binding');
    assert.equal(server.env.ZCODE_AGENTD_SOCKET, paths.socket);
    assert.equal(fs.readFileSync(path.join(home, 'plugins', 'unrelated-user-file.txt'), 'utf8'), 'keep me', 'unrelated files beside the staging survive the swap');
    assert.deepEqual(fs.readdirSync(path.join(home, 'plugins')).sort(), ['external-subagent', 'unrelated-user-file.txt'], 'no candidate or prior sibling residue remains');
    const marketplace = JSON.parse(fs.readFileSync(path.join(home, '.agents', 'plugins', 'marketplace.json'), 'utf8'));
    assert.equal(marketplace.plugins.filter((entry) => entry.name === 'external-subagent').length, 1, 'the refresh keeps exactly one managed marketplace entry');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(source, { recursive: true, force: true });
  }
});

test('a failed codex refresh restores the prior staging tree and marketplace bytes (AUD-002)', () => {
  const home = fixtureHome('external-subagent-refresh-rollback-');
  const goodDir = path.join(home, 'fake-good');
  const lyingDir = path.join(home, 'fake-lying');
  fs.mkdirSync(goodDir, { recursive: true, mode: 0o700 });
  fs.mkdirSync(lyingDir, { recursive: true, mode: 0o700 });
  const good = fakeCodexCli(goodDir);
  const lying = fakeCodexCli(lyingDir, { materialize: false });
  const { paths, options } = binding(home, { cli: good.cli });
  const staging = path.join(home, 'plugins', 'external-subagent');
  const marketplace = path.join(home, '.agents', 'plugins', 'marketplace.json');
  try {
    const first = installPlugin(paths, options);
    assert.equal(first.cache_verified, true);
    fs.writeFileSync(path.join(staging, 'skills', 'external-subagent', 'SKILL.md'), 'last-good bytes\n');
    const treeBefore = snapshotTree(staging);
    const marketBefore = fs.readFileSync(marketplace);
    // The cache disappears (codex-owned state this product never rewrites);
    // the lying fake then reports a cache-less `plugin add` success on
    // refresh, so verification must fail closed after the swap.
    fs.rmSync(first.cache, { recursive: true, force: true });
    assert.throws(() => installPlugin(paths, { ...options, codexCli: lying.cli }), (error) => {
      assert.equal(error.code, 'CODEX_CACHE_UNVERIFIABLE');
      return true;
    });
    assert.deepEqual(snapshotTree(staging), treeBefore, 'the prior managed tree is restored byte-for-byte');
    assert.deepEqual(fs.readFileSync(marketplace), marketBefore, 'the marketplace keeps its exact prior bytes');
    assert.deepEqual(fs.readdirSync(path.join(home, 'plugins')), ['external-subagent'], 'no candidate or prior residue remains');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// Real permission bits only bind for ordinary users; root bypasses them,
// so the persistent-denial oracles below skip instead of pretending their
// chmod verified anything.
const notRoot = process.getuid?.() !== 0 ? false : 'permission bits are not enforced for root';

// The AUD-002 reopened counterexample, codex shape (catch one: the
// publish/updateMarketplace transaction): a marketplace parent that STAYS
// read-only fails the marketplace write and then the marketplace restore
// too, but the staging restore is an independent attempt that must still
// run and succeed, and the unrestored marketplace must be named on the
// thrown error rather than inferred from logs.
test('a persistently unwritable marketplace parent still restores the prior staging tree when the marketplace write fails (AUD-002)', { skip: notRoot }, () => {
  const home = fixtureHome('external-subagent-market-locked-');
  const fake = fakeCodexCli(home);
  const { paths, options } = binding(home, { cli: fake.cli });
  const staging = path.join(home, 'plugins', 'external-subagent');
  const marketplace = path.join(home, '.agents', 'plugins', 'marketplace.json');
  const marketParent = path.dirname(marketplace);
  const source = tempSource((dir) => fs.appendFileSync(path.join(dir, 'skills', 'external-subagent', 'SKILL.md'), '\ncandidate marker\n'));
  try {
    installPlugin(paths, options);
    fs.writeFileSync(path.join(staging, 'skills', 'external-subagent', 'SKILL.md'), 'last-good bytes\n');
    const treeBefore = snapshotTree(staging);
    // Drop the managed entry so the refresh must REWRITE the marketplace,
    // then keep the marketplace parent persistently read-only.
    const doc = JSON.parse(fs.readFileSync(marketplace, 'utf8'));
    doc.plugins = doc.plugins.filter((entry) => entry.name !== 'external-subagent');
    fs.writeFileSync(marketplace, `${JSON.stringify(doc, null, 2)}\n`, { mode: 0o600 });
    const marketBefore = fs.readFileSync(marketplace);
    fs.chmodSync(marketParent, 0o500);
    let failure = null;
    try { installPlugin(paths, { ...options, source }); } catch (error) { failure = error; }
    assert.ok(failure, 'the refresh must fail while the marketplace parent is read-only');
    assert.equal(failure.code, 'EACCES', 'the original marketplace write failure surfaces, not a secondary restore error');
    assert.ok(Array.isArray(failure.restoreFailures), 'the error carries the unrestored-resource list');
    assert.deepEqual(failure.restoreFailures.map((entry) => entry.resource), [marketplace], 'exactly the marketplace restore is reported unrestored');
    assert.equal(failure.restoreFailures[0].code, 'EACCES');
    assert.deepEqual(snapshotTree(staging), treeBefore, 'the staging restore ran despite the denied marketplace restore and returned the prior tree');
    assert.deepEqual(fs.readFileSync(marketplace), marketBefore, 'the marketplace keeps its exact prior bytes');
    assert.deepEqual(fs.readdirSync(path.dirname(staging)), ['external-subagent'], 'the restored prior tree leaves no sibling residue');
  } finally {
    fs.chmodSync(marketParent, 0o700);
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(source, { recursive: true, force: true });
  }
});

// The same persistent denial against the second catch (marketplace
// registration / plugin add / cache verification): the primary failure is
// the unverifiable cache, and the denied marketplace restore must neither
// skip the staging restore nor replace the original error.
test('a persistently unwritable marketplace parent still restores the prior staging tree when cache verification fails (AUD-002)', { skip: notRoot }, () => {
  const home = fixtureHome('external-subagent-verify-locked-');
  const goodDir = path.join(home, 'fake-good');
  const lyingDir = path.join(home, 'fake-lying');
  fs.mkdirSync(goodDir, { recursive: true, mode: 0o700 });
  fs.mkdirSync(lyingDir, { recursive: true, mode: 0o700 });
  const good = fakeCodexCli(goodDir);
  const lying = fakeCodexCli(lyingDir, { materialize: false });
  const { paths, options } = binding(home, { cli: good.cli });
  const staging = path.join(home, 'plugins', 'external-subagent');
  const marketplace = path.join(home, '.agents', 'plugins', 'marketplace.json');
  const marketParent = path.dirname(marketplace);
  try {
    const first = installPlugin(paths, options);
    fs.writeFileSync(path.join(staging, 'skills', 'external-subagent', 'SKILL.md'), 'last-good bytes\n');
    const treeBefore = snapshotTree(staging);
    const marketBefore = fs.readFileSync(marketplace);
    // The cache disappears (codex-owned state this product never rewrites);
    // the lying fake then reports a cache-less `plugin add` success on
    // refresh, so verification must fail closed after the swap.
    fs.rmSync(first.cache, { recursive: true, force: true });
    fs.chmodSync(marketParent, 0o500);
    let failure = null;
    try { installPlugin(paths, { ...options, codexCli: lying.cli }); } catch (error) { failure = error; }
    assert.ok(failure, 'the refresh must fail on the cache-less plugin-add success');
    assert.equal(failure.code, 'CODEX_CACHE_UNVERIFIABLE', 'the original verification failure surfaces, not a secondary restore error');
    assert.ok(Array.isArray(failure.restoreFailures), 'the error carries the unrestored-resource list');
    assert.deepEqual(failure.restoreFailures.map((entry) => entry.resource), [marketplace], 'exactly the marketplace restore is reported unrestored');
    assert.equal(failure.restoreFailures[0].code, 'EACCES');
    assert.deepEqual(snapshotTree(staging), treeBefore, 'the staging restore ran despite the denied marketplace restore and returned the prior tree');
    assert.deepEqual(fs.readFileSync(marketplace), marketBefore, 'the marketplace keeps its exact prior bytes');
    assert.deepEqual(fs.readdirSync(path.dirname(staging)), ['external-subagent'], 'the restored prior tree leaves no sibling residue');
  } finally {
    fs.chmodSync(marketParent, 0o700);
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// The codex analogue of the audit's zcode counterexample, pinned at the
// exact sequential-skip symptom the base code produced.  The base catch #2
// restored the marketplace IN PLACE (writeFileSync on the existing file),
// which needs the file's own write bit — not parent-dir permission — so a
// persistently read-only marketplace FILE made that restore throw EACCES
// BEFORE staged.restore() ever ran: the command failed while the old
// binding's staging silently held the NEW tree.  The file is denied for
// that base path; the parent directory is denied as well because the
// FIXED code replaces the manifest atomically (temp file beside the target
// plus rename, which needs only parent-dir permission — a read-only file
// alone would not stop it).
test('a persistently read-only marketplace file cannot skip the staging restore when verification fails (AUD-002)', { skip: notRoot }, () => {
  const home = fixtureHome('external-subagent-market-file-locked-');
  const goodDir = path.join(home, 'fake-good');
  const lyingDir = path.join(home, 'fake-lying');
  fs.mkdirSync(goodDir, { recursive: true, mode: 0o700 });
  fs.mkdirSync(lyingDir, { recursive: true, mode: 0o700 });
  const good = fakeCodexCli(goodDir);
  const lying = fakeCodexCli(lyingDir, { materialize: false });
  const { paths, options } = binding(home, { cli: good.cli });
  const staging = path.join(home, 'plugins', 'external-subagent');
  const marketplace = path.join(home, '.agents', 'plugins', 'marketplace.json');
  const marketParent = path.dirname(marketplace);
  try {
    const first = installPlugin(paths, options);
    fs.writeFileSync(path.join(staging, 'skills', 'external-subagent', 'SKILL.md'), 'last-good bytes\n');
    const treeBefore = snapshotTree(staging);
    const marketBefore = fs.readFileSync(marketplace);
    // The cache disappears (codex-owned state this product never rewrites);
    // the lying fake then reports a cache-less `plugin add` success on
    // refresh, so verification fails closed after the swap.
    fs.rmSync(first.cache, { recursive: true, force: true });
    fs.chmodSync(marketplace, 0o400);
    fs.chmodSync(marketParent, 0o500);
    let failure = null;
    try { installPlugin(paths, { ...options, codexCli: lying.cli }); } catch (error) { failure = error; }
    assert.ok(failure, 'the refresh must fail on the cache-less plugin-add success');
    // THE BASE FAILURE POINT: with the restores chained, the denied
    // marketplace restore skipped staged.restore() entirely and the staging
    // path kept the freshly published candidate.
    assert.deepEqual(snapshotTree(staging), treeBefore, 'the staging restore must run despite the denied marketplace restore');
    assert.equal(failure.code, 'CODEX_CACHE_UNVERIFIABLE', 'the original verification failure surfaces, not the secondary restore denial');
    assert.deepEqual(failure.restoreFailures.map((entry) => entry.resource), [marketplace], 'exactly the marketplace restore is reported unrestored');
    assert.equal(failure.restoreFailures[0].code, 'EACCES');
    assert.deepEqual(fs.readFileSync(marketplace), marketBefore, 'the marketplace keeps its exact prior bytes');
    assert.deepEqual(fs.readdirSync(path.dirname(staging)), ['external-subagent'], 'the restored prior tree leaves no sibling residue');
  } finally {
    fs.chmodSync(marketplace, 0o600);
    fs.chmodSync(marketParent, 0o700);
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// updateMarketplace now writes atomically, so a LATER failure inside it
// (here: the post-write tree digest) must still roll the already-written
// managed marketplace content back to the prior backup bytes — foreign
// entries intact — alongside the staging restore, with the original error
// unmasked and no unrestored resources to report.
test('a marketplace write that succeeded before a later failure rolls back to the prior bytes with foreign entries intact (AUD-002)', () => {
  const home = fixtureHome('external-subagent-market-rollback-');
  const fake = fakeCodexCli(home);
  const { paths, options } = binding(home, { cli: fake.cli });
  const staging = path.join(home, 'plugins', 'external-subagent');
  const marketplace = path.join(home, '.agents', 'plugins', 'marketplace.json');
  fs.mkdirSync(path.dirname(marketplace), { recursive: true, mode: 0o700 });
  fs.writeFileSync(marketplace, `${JSON.stringify({
    name: 'personal',
    interface: { displayName: 'Personal' },
    plugins: [{ name: 'other-tool', source: { source: 'local', path: './plugins/other-tool' }, policy: { installation: 'AVAILABLE', authentication: 'ON_INSTALL' }, category: 'Productivity' }],
  }, null, 2)}\n`, { mode: 0o600 });
  const foreignOnly = fs.readFileSync(marketplace);
  try {
    // Fail the first read of the published tree's SKILL.md — the tree digest
    // inside updateMarketplace, i.e. AFTER the managed entry was written.
    const realRead = fs.readFileSync;
    const skillAt = path.join(staging, 'skills', 'external-subagent', 'SKILL.md');
    fs.readFileSync = function injectedRead(file, ...rest) {
      if (path.resolve(String(file)) === skillAt) {
        throw Object.assign(new Error('injected tree digest failure'), { code: 'EIO' });
      }
      return realRead(file, ...rest);
    };
    let failure = null;
    try {
      try { installPlugin(paths, options); } catch (error) { failure = error; }
    } finally {
      fs.readFileSync = realRead;
    }
    assert.ok(failure, 'the install must fail at the injected digest read');
    assert.equal(failure.code, 'EIO', 'the original digest failure surfaces');
    assert.equal(failure.restoreFailures, undefined, 'both restores succeeded, so nothing is reported unrestored');
    assert.deepEqual(fs.readFileSync(marketplace), foreignOnly, 'the managed entry is rolled back to the prior bytes and the foreign entry survives');
    assert.equal(fs.existsSync(staging), false, 'the first install leaves no staging tree behind');
    assert.deepEqual(fs.readdirSync(path.dirname(staging)), [], 'no candidate or prior residue remains');
    // The failure predates every codex side effect: only the --help probe ran.
    assert.ok(invocations(fake.log).every((call) => call.args.includes('--help')), 'no marketplace registration or plugin add happened');
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

// codex resolves the reserved marketplace name `personal` (the product
// default) machine-globally to the real user root regardless of CODEX_HOME
// (verified live on 0.154.0; the earlier 0.153.4 "store reuse" observation
// is this same reserved-name resolution): when the user's real ~/.codex
// already caches the same plugin@marketplace@version, an isolated CODEX_HOME
// still receives the REAL installation's bytes (observed live: the cached
// .mcp.json carried the real-home socket while the staged tree carried the
// throwaway socket).  installPlugin now reads the cache back and fails
// closed on that mismatch; the real-CLI oracle below additionally pins the
// fresh-machine contract, so it only runs where the reserved-name/store
// resolution cannot turn the run into the mismatch path it now shares with
// the unit oracle above.
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
