// Init rollback and standalone-boundary oracles at the unit level (the
// full packed CLI acceptance lives in fresh-install.test.mjs).  AUD-005/D1:
// init installs the STANDALONE SERVICE ONLY — no fixed-ZCode runtime probe,
// no managed Codex plugin install, no D08 claim — so a successful init leaves
// no Codex-owned or plugin-owned artifact behind, and missing unrelated
// runtimes never block setup.  Hosts are bound afterwards through the explicit
// install-plugin/install-mcp owners.  Failures in the remaining steps roll
// tracked files and created directories back through recovery.mjs; the codex
// CLI fake in the fixture proves by its never-materialized outputs that no
// implicit host install happens.  All writes stay inside throwaway temp homes.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { ZCODE_RUNTIME } from '../../cli/constants.mjs';
import { runInit } from '../../cli/install/init.mjs';
import { installPlugin } from '../../cli/install/codex.mjs';
import { registerCodexHome, loadCodexHomes } from '../../cli/install/reconcile.mjs';
import { readConfig } from '../../cli/config/read.mjs';
import { configCommand, parseConfigArgs } from '../../cli/commands/config.mjs';
import { platform, productPaths } from '../../cli/paths.mjs';

// The installed service definition is the launchd plist on macOS and the
// systemd user unit on Linux; Windows registers the definition in SCM instead
// (paths.launchAgent is null), so the forwarding assertions below are stated
// once per file-based backend and the Windows host is exercised through the
// injected sc.exe seam.
const macos = platform() === 'darwin';
const windowsHost = platform() === 'win32';
const assertForwards = (definition, key, value) => {
  if (windowsHost) return;
  if (macos) assert.match(definition, new RegExp(`<key>${key}</key><string>${value}</string>`, 'u'));
  else assert.match(definition, new RegExp(`^Environment="${key}=${value}"$`, 'mu'));
};

function definitionExists(paths) {
  return Boolean(paths.launchAgent) && fs.existsSync(paths.launchAgent);
}

function serviceDefinition(paths) {
  return paths.launchAgent ? fs.readFileSync(paths.launchAgent, 'utf8') : '';
}

// Minimal sc.exe double for the Windows host: no service is registered and
// every mutating verb succeeds, so init's service steps run without touching a
// real SCM database.  Individual tests override one verb to inject a failure.
function windowsScStub(overrides = {}) {
  const calls = [];
  const control = (args) => {
    calls.push(args);
    if (overrides[args[0]]) return overrides[args[0]](args);
    if (args[0] === 'queryex') return { status: 1060, stdout: '' };
    if (['create', 'start', 'stop', 'delete', 'config'].includes(args[0])) return { status: 0, stdout: '' };
    throw new Error(`unexpected sc.exe call: ${args.join(' ')}`);
  };
  return { calls, control };
}

function fixtureHome(prefix = 'external-subagent-init-') {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

// Minimal stand-in for the verified codex CLI surface, kept so any accidental
// implicit host install during init leaves detectable artifacts (staging
// tree, marketplace manifest, plugin cache) instead of passing silently.
function fakeCodexCli(directory) {
  const script = path.join(directory, 'codex-fake.mjs');
  fs.writeFileSync(script, `#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
const args = process.argv.slice(2);
const rootsFile = path.join(${JSON.stringify(directory)}, 'marketplace-roots.json');
const text = (value) => { process.stdout.write(JSON.stringify(value, null, 2) + '\\n'); };
const loadRoots = () => { try { return JSON.parse(fs.readFileSync(rootsFile, 'utf8')); } catch { return {}; } };
if (args[0] === 'plugin' && args[1] === 'add' && args.includes('--help')) { process.stdout.write('usage\\n'); process.exit(0); }
if (args[0] === 'plugin' && args[1] === 'marketplace' && args[2] === 'add') {
  const roots = loadRoots();
  roots[process.env.CODEX_HOME] = args[3];
  fs.writeFileSync(rootsFile, JSON.stringify(roots));
  text({ marketplaceName: 'personal', installedRoot: args[3] });
  process.exit(0);
}
if (args[0] === 'plugin' && args[1] === 'add') {
  const name = args[2]; const marketplace = args[args.indexOf('--marketplace') + 1];
  const root = loadRoots()[process.env.CODEX_HOME];
  if (!root) { process.stderr.write('no marketplace registered for this CODEX_HOME\\n'); process.exit(1); }
  const doc = JSON.parse(fs.readFileSync(path.join(root, '.agents', 'plugins', 'marketplace.json'), 'utf8'));
  const staging = path.resolve(root, doc.plugins.find((plugin) => plugin.name === name).source.path);
  const version = JSON.parse(fs.readFileSync(path.join(staging, '.codex-plugin', 'plugin.json'), 'utf8')).version;
  const cache = path.join(process.env.CODEX_HOME || '', 'plugins', 'cache', marketplace, name, String(version));
  fs.rmSync(cache, { recursive: true, force: true });
  fs.mkdirSync(path.dirname(cache), { recursive: true });
  fs.cpSync(staging, cache, { recursive: true });
  text({ pluginId: name + '@' + marketplace, name, marketplaceName: marketplace, version, installedPath: cache });
  process.exit(0);
}
process.stderr.write('unexpected codex invocation: ' + JSON.stringify(args) + '\\n');
process.exit(1);
`);
  fs.chmodSync(script, 0o755);
  return script;
}

// Every init runs with the environmental probes neutralized (no service
// bootstrap, no payload verification), so only the install/rollback machinery
// executes.  The codex fake is wired exactly like the pre-D1 implicit install
// wired it, so a regression back to implicit binding fails these oracles.
function initFixture({ failStep } = {}) {
  const home = fixtureHome();
  // A real Windows host exports LOCALAPPDATA; passing the empty env keeps the
  // whole fixture inside the throwaway home instead of the user's product
  // directory (the layout falls back to <home>/AppData/Local).
  const paths = productPaths(home, windowsHost ? {} : process.env);
  const fake = fakeCodexCli(home);
  const codexHome = path.join(home, 'codex-home');
  // A Windows init renders an SCM command line for an executable it expects to
  // exist; a throwaway file keeps the fixture independent of build outputs.
  const daemonPath = windowsHost ? path.join(home, 'external-subagentd.exe') : undefined;
  if (windowsHost) fs.writeFileSync(daemonPath, 'fixture daemon');
  const windowsOptions = windowsHost
    ? { sc: windowsScStub().control, daemonPath }
    : {};
  const run = (overrides = {}) => runInit({
    paths,
    skipPayloadProbe: true,
    skipServiceStart: true,
    codexCli: fake,
    codexHome,
    ...windowsOptions,
    ...(failStep ? { _failStep: failStep } : {}),
    ...overrides,
  });
  return { home, paths, fake, codexHome, daemonPath, run };
}

test('successful init installs the standalone service only and binds no host', () => {
  const { paths, codexHome, run } = initFixture();
  try {
    const report = run();
    assert.equal(report.installed, true);
    assert.equal(report.codex, undefined, 'the D1 init report carries no codex binding result');
    assert.equal(report.runtimes.zcode.present, fs.existsSync(ZCODE_RUNTIME), 'the runtime report mirrors reality instead of asserting a dependency');
    assert.equal(report.runtimes.zcode.path, ZCODE_RUNTIME);
    // No implicit host install: no staging tree, no marketplace manifest, no
    // D08 registry, and the would-be codex home is never even created.
    assert.equal(fs.existsSync(path.join(paths.home, 'plugins', 'external-subagent')), false, 'no managed staging tree');
    assert.equal(fs.existsSync(path.join(paths.home, '.agents')), false, 'no marketplace manifest');
    assert.equal(fs.existsSync(path.join(paths.data, 'codex-homes.json')), false, 'no D08 registry claim');
    assert.equal(fs.existsSync(codexHome), false, 'the codex home is never created');
    assert.equal(definitionExists(paths), !windowsHost, windowsHost
      ? 'Windows registers the definition in SCM at start-service time'
      : 'the standalone LaunchAgent is installed');
    assert.ok(fs.existsSync(paths.config), 'the product config is published');
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('missing unrelated runtimes never block standalone setup (DSH-only adapter config)', () => {
  const { paths, run } = initFixture();
  try {
    fs.mkdirSync(path.dirname(paths.config), { recursive: true });
    fs.writeFileSync(paths.config, JSON.stringify({
      schema_version: 2,
      revision: 3,
      default_subagent: 'dsh',
      subagents: { dsh: { enabled: true, spawn_supported: true, runtime_path: '/opt/dsh/acp', home: '/var/lib/dsh', profile: 'acp', version: '0.1.5' } },
    }));
    // No ZCode app requirement is probed, no Codex CLI is invoked, and the
    // only failure mode left is the product's own machinery.
    const report = run();
    assert.equal(report.installed, true);
    const definition = serviceDefinition(paths);
    assertForwards(definition, 'DSH_RUNTIME_PATH', '/opt/dsh/acp');
    assertForwards(definition, 'EXTERNAL_SUBAGENT_CONFIG_REVISION', '4');
    if (!windowsHost) {
      assert.match(definition, /EXTERNAL_SUBAGENT_CONFIG_REVISION/u, 'the rewritten config advances the revision');
      // The pinned ZCode runtime argument is forwarded exactly when that
      // installation exists — never as an unconditional dependency.
      assert.equal(definition.includes('--runtime'), fs.existsSync(ZCODE_RUNTIME));
    }
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('an explicit codex host binding still works after standalone init', {
  // The fake CLI is a POSIX shebang script; a real Windows host runs codex.exe
  // (or its npm .cmd shim), which the fixture does not model.  The binding
  // machinery itself is host-neutral and stays covered on POSIX.
  skip: windowsHost && 'the codex CLI fixture is a POSIX shebang executable',
}, () => {
  const { paths, codexHome, fake, run } = initFixture();
  try {
    assert.equal(run().installed, true);
    // The explicit install-plugin owner (the same call pluginCommand makes)
    // stages, adds through the official CLI, and claims the D08 registry.
    const install = installPlugin(paths, { codexCli: fake, codexHome });
    assert.equal(install.installed, true);
    const claim = registerCodexHome(paths, install.codex_home, { version: null, digest: install.digest, binding_mode: 'plugin' });
    assert.equal(claim.registered, true);
    const registry = loadCodexHomes(paths).registry;
    assert.deepEqual(registry.homes.map((entry) => entry.home), [fs.realpathSync(codexHome)], 'the explicit binding claims exactly the targeted home');
    assert.ok(fs.existsSync(path.join(paths.home, 'plugins', 'external-subagent')), 'explicit staging appears only after the explicit install');
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
    run();
    // The accepted write contract (agents-config.test.mjs) canonicalizes a
    // legacy schema-1 document on the first locked write: the persisted file
    // keeps the configured agents under subagents/default_subagent and the
    // revision advances, while the legacy names disappear.
    const configured = JSON.parse(fs.readFileSync(paths.config, 'utf8'));
    assert.equal(configured.revision, 8);
    assert.equal(configured.schema_version, 2);
    assert.equal(configured.default_agent, undefined);
    assert.equal(configured.agents, undefined);
    assert.equal(configured.default_subagent, 'dsh');
    assert.equal(configured.subagents.dsh.enabled, true);
    assert.equal(configured.subagents.dsh.spawn_supported, true);
    assert.equal(configured.subagents.dsh.default_model, 'opaque-model');
    assert.equal(configured.subagents.zcode.enabled, true);
    assertForwards(serviceDefinition(paths), 'EXTERNAL_SUBAGENT_CONFIG_REVISION', '8');
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('failure after the service template rolls config, data, and the LaunchAgent back', () => {
  const { paths, run } = initFixture({ failStep: 'install-launch-agent' });
  try {
    assert.throws(() => run(), /injected failure at install-launch-agent/u);
    assert.equal(fs.existsSync(paths.data), false, 'product data rolls back');
    assert.equal(fs.existsSync(paths.config), false, 'product config rolls back');
    assert.equal(definitionExists(paths), false, 'the LaunchAgent rolls back');
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('a failed init never touches codex-owned paths', () => {
  const { paths, codexHome, run } = initFixture({ failStep: 'install-launch-agent' });
  try {
    fs.mkdirSync(codexHome, { recursive: true, mode: 0o700 });
    fs.writeFileSync(path.join(codexHome, 'user-config.toml'), 'owned by the host user');
    assert.throws(() => run(), /injected failure at install-launch-agent/u);
    assert.equal(fs.readFileSync(path.join(codexHome, 'user-config.toml'), 'utf8'), 'owned by the host user', 'init never writes inside a codex home');
    assert.equal(fs.existsSync(path.join(paths.home, 'plugins')), false, 'no staging is ever created');
    assert.equal(fs.existsSync(path.join(paths.home, '.agents')), false, 'no marketplace is ever created');
    assert.equal(fs.existsSync(path.join(paths.data, 'codex-homes.json')), false, 'no registry is ever written');
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('init republishes away a legacy zcode runtime_path instead of failing closed', () => {
  // `subagents.zcode.runtime_path` was a first-class `config set` key before
  // D1 rejected it, so a real install predating that rejection can carry it in either the
  // schema-2 or the legacy schema-1 shape.  The republish step retires the
  // field before validation, so `npm new -> init` self-heals both shapes and
  // the strict read gate never bricks an upgrade.
  const schema2 = initFixture();
  const schema1 = initFixture();
  try {
    fs.mkdirSync(path.dirname(schema2.paths.config), { recursive: true });
    fs.writeFileSync(schema2.paths.config, JSON.stringify({
      schema_version: 2,
      revision: 5,
      default_subagent: null,
      subagents: { zcode: { enabled: true, spawn_supported: true, runtime_path: '/opt/legacy-zcode.cjs' } },
    }));
    assert.equal(schema2.run().installed, true, 'init self-heals a schema-2 config written by the legacy CLI');
    let configured = JSON.parse(fs.readFileSync(schema2.paths.config, 'utf8'));
    assert.equal(configured.subagents.zcode.runtime_path, undefined, 'the retired field is gone from the republished schema-2 config');
    assert.equal(configured.subagents.zcode.enabled, true, 'unrelated zcode settings survive the republish');
    assert.equal(readConfig(schema2.paths.config).revision, 6, 'the republished config reads back through the strict gate');
    // The reviewer-reproduced lockout path: `config unset` reads the file
    // before its null patch applies, so it must succeed on the healed config.
    // The config transaction helper is POSIX-only (flock/lockf) on this
    // baseline; the Windows lock backend is a separate host gap, so init's
    // self-healing assertions above stay covered while this CLI call is not
    // available on the Windows host.
    if (!windowsHost) {
      configCommand(schema2.paths, parseConfigArgs(['unset', 'subagents.zcode.runtime_path']));
    }
    fs.mkdirSync(path.dirname(schema1.paths.config), { recursive: true });
    fs.writeFileSync(schema1.paths.config, JSON.stringify({
      schema_version: 1,
      revision: 3,
      default_agent: 'zcode',
      agents: { zcode: { enabled: true, spawn_supported: true, runtime_path: '/opt/legacy-zcode.cjs' } },
    }));
    assert.equal(schema1.run().installed, true, 'init self-heals a legacy schema-1 config written by the legacy CLI');
    configured = JSON.parse(fs.readFileSync(schema1.paths.config, 'utf8'));
    assert.equal(configured.schema_version, 2, 'the schema-1 prior is migrated by the republish');
    assert.equal(configured.subagents.zcode.runtime_path, undefined, 'the retired field is gone from the migrated config');
    assert.equal(readConfig(schema1.paths.config).default_subagent, 'zcode', 'the healed config reads back through the strict gate');
    if (!windowsHost) {
      configCommand(schema1.paths, parseConfigArgs(['unset', 'subagents.zcode.runtime_path']));
    }
    assert.equal(definitionExists(schema1.paths), !windowsHost, 'the service template renders from the healed config');
  } finally {
    fs.rmSync(schema2.paths.home, { recursive: true, force: true });
    fs.rmSync(schema1.paths.home, { recursive: true, force: true });
  }
});

test('a failed re-init keeps the prior successful installation intact', () => {
  const { paths, run } = initFixture();
  try {
    assert.equal(run().installed, true);
    const configBefore = fs.readFileSync(paths.config, 'utf8');
    const definitionBefore = serviceDefinition(paths);

    assert.throws(() => run({ _failStep: 'write-product-config' }), /injected failure at write-product-config/u);
    assert.equal(fs.readFileSync(paths.config, 'utf8'), configBefore, 'the published config survives byte-for-byte');
    assert.equal(serviceDefinition(paths), definitionBefore, 'the installed service definition survives byte-for-byte');
    assert.ok(fs.existsSync(paths.data), 'product data stays');
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('win32 init forwards the injected sc.exe backend and surfaces AccessDenied structurally', { skip: !windowsHost }, () => {
  const { paths, run, daemonPath } = initFixture();
  const sc = windowsScStub({ create: () => ({ status: 5, stdout: '' }) });
  try {
    assert.throws(
      () => run({ skipServiceStart: false, sc: sc.control, daemonPath }),
      (error) => {
        assert.equal(error.code, 'SERVICE_ACCESS_DENIED');
        assert.match(error.message, /Administrator/u);
        return true;
      },
    );
    assert.ok(sc.calls.some((call) => call[0] === 'create'), 'init reached the injected sc.exe create');
    assert.equal(fs.existsSync(paths.data), false, 'product data rolls back after the service failure');
    assert.equal(fs.existsSync(paths.config), false, 'product config rolls back after the service failure');
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});
