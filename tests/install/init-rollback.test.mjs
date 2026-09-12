// S05 init rollback and D08 claim oracles at the unit level (the full packed
// CLI acceptance lives in fresh-install.test.mjs).  A successful init claims
// the Codex home with the real staged plugin tree digest; a failure in the
// steps after install-codex-plugin rolls the product-owned Codex artifacts
// (staging tree, marketplace manifest, and directories this run created)
// back, while a pre-existing CODEX_HOME and the official codex cache inside
// it are never touched.  The codex CLI is a recording fake; all writes stay
// inside throwaway temp homes.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { runInit } from '../../cli/install/init.mjs';
import { treeDigest } from '../../cli/install/codex.mjs';
import { loadCodexHomes } from '../../cli/install/reconcile.mjs';
import { productPaths } from '../../cli/paths.mjs';

function fixtureHome(prefix = 'external-subagent-init-') {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

// Minimal stand-in for the verified codex CLI surface; the cacheWriter
// variant adds a fake plugin-cache write so tests can pin that codex-owned
// content survives rollback.
function fakeCodexCli(directory, { cacheWriter = false } = {}) {
  const log = path.join(directory, 'codex-invocations.jsonl');
  const script = path.join(directory, 'codex-fake.mjs');
  fs.writeFileSync(script, `#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
const args = process.argv.slice(2);
const text = (value) => { process.stdout.write(JSON.stringify(value, null, 2) + '\\n'); };
if (args[0] === 'plugin' && args[1] === 'add' && args.includes('--help')) { process.stdout.write('usage\\n'); process.exit(0); }
if (args[0] === 'plugin' && args[1] === 'marketplace' && args[2] === 'add') { text({ marketplaceName: 'personal', installedRoot: args[3] }); process.exit(0); }
if (args[0] === 'plugin' && args[1] === 'add') {
  const name = args[2]; const marketplace = args[args.indexOf('--marketplace') + 1];
  ${cacheWriter ? `const cache = path.join(process.env.CODEX_HOME || '', 'plugins', 'cache', marketplace, name); fs.mkdirSync(cache, { recursive: true }); fs.writeFileSync(path.join(cache, 'codex-owned.txt'), 'official cache');` : ''}
  text({ pluginId: name + '@' + marketplace, name, marketplaceName: marketplace, version: '0.1.0', installedPath: path.join(process.env.CODEX_HOME || '', 'plugins', 'cache', marketplace, name, '0.1.0') });
  process.exit(0);
}
process.stderr.write('unexpected codex invocation: ' + JSON.stringify(args) + '\\n');
process.exit(1);
`);
  fs.chmodSync(script, 0o755);
  return { cli: script, log };
}

// Every init runs against the fake codex CLI with the environmental probes
// neutralized, so only the install/rollback machinery executes.
function initFixture({ failStep, cacheWriter = false } = {}) {
  const home = fixtureHome();
  const paths = productPaths(home);
  const fake = fakeCodexCli(home, { cacheWriter });
  const codexHome = path.join(home, 'codex-home');
  const run = (overrides = {}) => runInit({
    paths,
    skipRuntimeProbe: true,
    skipPayloadProbe: true,
    skipServiceStart: true,
    codexCli: fake.cli,
    codexHome,
    ...(failStep ? { _failStep: failStep } : {}),
    ...overrides,
  });
  return { home, paths, fake, codexHome, run };
}

test('successful init claims the codex home with the staged plugin tree digest', () => {
  const { paths, codexHome, run } = initFixture();
  try {
    const report = run();
    assert.equal(report.installed, true);
    assert.equal(report.codex.status, 'installed');

    const entry = loadCodexHomes(paths).registry.homes.find((claim) => claim.home === fs.realpathSync(codexHome));
    assert.ok(entry, 'the configured codex home is claimed');
    assert.match(entry.digest, /^[0-9a-f]{64}$/u, 'the claim digest is a sha256 hex digest');
    assert.equal(entry.digest, treeDigest(path.join(paths.home, 'plugins', 'external-subagent')), 'the claim records the staged plugin tree digest');
    assert.equal(entry.digest, report.codex.digest, 'the report and the registry agree on the digest');
    assert.notEqual(entry.digest, 'personal', 'the marketplace name must never be stored as the digest');
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('init preserves configured agents and advances the service config revision', () => {
  const { paths, run } = initFixture();
  try {
    fs.mkdirSync(path.dirname(paths.config), { recursive: true });
    fs.writeFileSync(paths.config, JSON.stringify({
      schema_version: 1,
      revision: 7,
      default_agent: 'dsh',
      agents: {
        zcode: { enabled: true, spawn_supported: true, default_model: null },
        dsh: { enabled: true, spawn_supported: true, default_model: 'opaque-model' },
      },
    }));
    run({ skipCodexPlugin: true });
    const configured = JSON.parse(fs.readFileSync(paths.config, 'utf8'));
    assert.equal(configured.revision, 8);
    assert.equal(configured.default_agent, 'dsh');
    assert.equal(configured.agents.dsh.enabled, true);
    assert.equal(configured.agents.dsh.spawn_supported, true);
    assert.equal(configured.agents.dsh.default_model, 'opaque-model');
    const plist = fs.readFileSync(paths.launchAgent, 'utf8');
    assert.match(plist, /EXTERNAL_SUBAGENT_CONFIG_REVISION<\/key><string>8<\/string>/u);
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('failure after the plugin binding rolls back staging, marketplace, and the created codex home', () => {
  const { paths, codexHome, run } = initFixture({ failStep: 'claim-codex-home' });
  try {
    assert.throws(() => run(), /injected failure at claim-codex-home/u);
    assert.equal(fs.existsSync(path.join(paths.home, 'plugins', 'external-subagent')), false, 'the staging tree is removed');
    assert.equal(fs.existsSync(path.join(paths.home, 'plugins')), false, 'the staging parent directory is pruned');
    assert.equal(fs.existsSync(path.join(paths.home, '.agents', 'plugins', 'marketplace.json')), false, 'the marketplace manifest is removed');
    assert.equal(fs.existsSync(path.join(paths.home, '.agents')), false, 'the marketplace parent directories are pruned');
    assert.equal(fs.existsSync(codexHome), false, 'a codex home this run created disappears when codex wrote nothing');
    assert.equal(fs.existsSync(paths.data), false, 'product data rolls back');
    assert.equal(fs.existsSync(paths.config), false, 'product config rolls back');
    assert.equal(fs.existsSync(paths.launchAgent), false, 'the LaunchAgent rolls back');
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('a pre-existing codex home survives rollback untouched', () => {
  const { paths, codexHome, run } = initFixture({ failStep: 'claim-codex-home' });
  fs.mkdirSync(codexHome, { recursive: true, mode: 0o700 });
  try {
    assert.throws(() => run(), /injected failure at claim-codex-home/u);
    assert.equal(fs.existsSync(codexHome), true, 'init never deletes a codex home it did not create');
    assert.equal(fs.existsSync(path.join(paths.home, 'plugins', 'external-subagent')), false, 'staging still rolls back');
    assert.equal(fs.existsSync(path.join(paths.home, '.agents')), false, 'the marketplace manifest still rolls back');
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('official codex cache writes inside a created codex home are never rolled back', () => {
  const { paths, codexHome, run } = initFixture({ failStep: 'claim-codex-home', cacheWriter: true });
  try {
    assert.throws(() => run(), /injected failure at claim-codex-home/u);
    const cacheFile = path.join(codexHome, 'plugins', 'cache', 'personal', 'external-subagent', 'codex-owned.txt');
    assert.ok(fs.existsSync(cacheFile), 'codex-owned cache content survives the rollback');
    assert.equal(fs.existsSync(codexHome), true, 'the codex home stays because codex wrote into it');
    assert.equal(fs.existsSync(path.join(paths.home, 'plugins', 'external-subagent')), false, 'product-owned staging still rolls back');
    assert.equal(fs.existsSync(path.join(paths.home, '.agents')), false, 'the product-owned marketplace still rolls back');
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('a failed re-init keeps the prior successful installation intact', () => {
  const { paths, codexHome, run } = initFixture();
  try {
    assert.equal(run().installed, true);

    assert.throws(() => run({ _failStep: 'claim-codex-home' }), /injected failure at claim-codex-home/u);
    assert.ok(fs.existsSync(path.join(paths.home, 'plugins', 'external-subagent')), 'pre-existing managed staging stays managed and refreshable');
    const marketplace = JSON.parse(fs.readFileSync(path.join(paths.home, '.agents', 'plugins', 'marketplace.json'), 'utf8'));
    assert.equal(marketplace.plugins.filter((entry) => entry.name === 'external-subagent').length, 1, 'the marketplace keeps its managed entry');
    assert.equal(fs.existsSync(codexHome), true, 'the claimed codex home stays');
    const registry = loadCodexHomes(paths).registry;
    assert.deepEqual(registry.homes.map((entry) => entry.home), [fs.realpathSync(codexHome)], 'the prior D08 claim survives the failed re-init');
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});
