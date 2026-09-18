// S05 CLI regression: the --codex-home option must actually reach the target
// Codex home.  install-mcp resolves its config.toml (for install and removal
// alike) inside the requested home and never writes the default home;
// install-plugin claims the home in the D08 registry with the real staged
// plugin tree digest, not the marketplace name.  The codex CLI is a recording
// fake; every write stays inside throwaway temp homes.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { mcpCommand, pluginCommand } from '../../cli/commands/plugin.mjs';
import { installMcp } from '../../cli/install/codex.mjs';
import { treeDigest } from '../../cli/install/plugin-stage.mjs';
import { loadCodexHomes } from '../../cli/install/reconcile.mjs';
import { CliError } from '../../cli/errors.mjs';
import { productPaths } from '../../cli/paths.mjs';

function fixtureHome(prefix = 'external-subagent-cli-') {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

// Minimal stand-in for the verified codex CLI surface (0.153.4 JSON shapes).
// Named `codex` and exposed through PATH: the command layer resolves the CLI
// the same way the real binary would be found.  `plugin add` materializes
// the plugin cache the real CLI writes, so the installer's read-back
// verification is exercised through the public command surface too.  With
// `materialize: false` the fake reports success (with installedPath) without
// writing any cache — the never-materialized counterexample.
function fakeCodexCli(directory, { materialize = true } = {}) {
  const log = path.join(directory, 'codex-invocations.jsonl');
  const script = path.join(directory, 'codex');
  fs.writeFileSync(script, `#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
const args = process.argv.slice(2);
const stateDir = ${JSON.stringify(directory)};
const rootsFile = path.join(stateDir, 'marketplace-roots.json');
const text = (value) => { process.stdout.write(JSON.stringify(value, null, 2) + '\\n'); };
const loadRoots = () => { try { return JSON.parse(fs.readFileSync(rootsFile, 'utf8')); } catch { return {}; } };
if (args[0] === 'plugin' && args[1] === 'add' && args.includes('--help')) { process.stdout.write('usage\\n'); process.exit(0); }
if (args[0] === 'plugin' && args[1] === 'marketplace' && args[2] === 'add') {
  const roots = loadRoots();
  roots[process.env.CODEX_HOME] = args[3];
  fs.writeFileSync(rootsFile, JSON.stringify(roots));
  text({ marketplaceName: 'personal', installedRoot: args[3] }); process.exit(0);
}
if (args[0] === 'plugin' && args[1] === 'add') {
  const name = args[2]; const marketplace = args[args.indexOf('--marketplace') + 1];
  const root = loadRoots()[process.env.CODEX_HOME];
  if (!root) { process.stderr.write('no marketplace registered for this CODEX_HOME\\n'); process.exit(1); }
  const doc = JSON.parse(fs.readFileSync(path.join(root, '.agents', 'plugins', 'marketplace.json'), 'utf8'));
  const staging = path.resolve(root, doc.plugins.find((plugin) => plugin.name === name).source.path);
  const version = JSON.parse(fs.readFileSync(path.join(staging, '.codex-plugin', 'plugin.json'), 'utf8')).version;
  const cache = path.join(process.env.CODEX_HOME || '', 'plugins', 'cache', marketplace, name, String(version));
  ${materialize ? `
  fs.rmSync(cache, { recursive: true, force: true });
  fs.mkdirSync(path.dirname(cache), { recursive: true });
  fs.cpSync(staging, cache, { recursive: true });` : `
  /* cache-less success: report the install without materializing the cache */`}
  text({ pluginId: name + '@' + marketplace, name, marketplaceName: marketplace, version, installedPath: cache });
  process.exit(0);
}
process.stderr.write('unexpected codex invocation: ' + JSON.stringify(args) + '\\n');
process.exit(1);
`);
  fs.chmodSync(script, 0o755);
  return { dir: directory, cli: script, log };
}

test('install-mcp --codex-home writes only the targeted codex home, never the default', () => {
  const productHome = fixtureHome('external-subagent-mcp-cli-');
  const codexA = fixtureHome('external-subagent-mcp-home-a-');
  const codexB = fixtureHome('external-subagent-mcp-home-b-');
  const paths = productPaths(productHome);
  const priorEnvHome = process.env.CODEX_HOME;
  delete process.env.CODEX_HOME;
  try {
    // The command layer must resolve the config inside each requested home.
    const dryA = mcpCommand(paths, ['--codex-home', codexA, '--dry-run']);
    assert.equal(dryA.dry_run, true);
    assert.equal(dryA.config, path.join(codexA, 'config.toml'), 'dry-run must resolve config inside the requested codex home');
    const dryB = mcpCommand(paths, ['--codex-home', codexB, '--dry-run']);
    assert.equal(dryB.config, path.join(codexB, 'config.toml'));

    const installed = installMcp(paths, { codexHome: codexA, skipNativeProbe: true });
    assert.equal(installed.installed, true);
    assert.equal(installed.config, path.join(codexA, 'config.toml'));
    assert.match(fs.readFileSync(path.join(codexA, 'config.toml'), 'utf8'), /\[mcp_servers\.external_subagent\]/u);
    assert.equal(fs.existsSync(path.join(productHome, '.codex')), false, 'the default codex home must not be written');
    assert.equal(fs.existsSync(path.join(codexB, 'config.toml')), false, 'the second codex home must not be written');

    installMcp(paths, { codexHome: codexB, skipNativeProbe: true });
    assert.ok(fs.existsSync(path.join(codexB, 'config.toml')), 'the second targeted home receives its own binding');

    const removed = installMcp(paths, { codexHome: codexA, uninstall: true, skipNativeProbe: true });
    assert.equal(removed.uninstalled, true);
    assert.equal(fs.readFileSync(path.join(codexA, 'config.toml'), 'utf8'), '', 'uninstall removes the managed section from the targeted home');
    assert.match(fs.readFileSync(path.join(codexB, 'config.toml'), 'utf8'), /\[mcp_servers\.external_subagent\]/u, 'the second home keeps its binding');
    assert.equal(fs.existsSync(path.join(productHome, '.codex')), false, 'uninstall must not fall back to the default home');
  } finally {
    if (priorEnvHome === undefined) delete process.env.CODEX_HOME; else process.env.CODEX_HOME = priorEnvHome;
    for (const dir of [productHome, codexA, codexB]) fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('install-plugin --codex-home claims the registry with the real plugin tree digest', () => {
  const home = fixtureHome('external-subagent-claim-');
  const fake = fakeCodexCli(home);
  const paths = productPaths(home);
  const codexHome = path.join(home, 'codex-target');
  const priorPath = process.env.PATH;
  process.env.PATH = `${fake.dir}${path.delimiter}${priorPath}`;
  try {
    const result = pluginCommand(paths, ['--codex-home', codexHome]);
    assert.equal(result.installed, true);
    assert.equal(result.cache_verified, true, 'the command layer surfaces the cache read-back verification');
    assert.equal(result.claim.registered, true);

    const registry = loadCodexHomes(paths).registry;
    const claimed = registry.homes.find((entry) => entry.home === fs.realpathSync(codexHome));
    assert.ok(claimed, 'the requested codex home must be the claimed one');
    assert.match(claimed.digest, /^[0-9a-f]{64}$/u, 'the claim digest is a sha256 hex digest');
    assert.equal(claimed.digest, result.digest, 'the claim stores the install-reported tree digest');
    assert.equal(claimed.digest, treeDigest(result.staging), 'the claim stores the digest of the staged plugin tree');
    assert.notEqual(claimed.digest, result.marketplace_name, 'the claim must not record the marketplace name as the digest');
  } finally {
    process.env.PATH = priorPath;
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// P1 counterexample through the public command surface: when codex reports
// success but never materializes a plugin cache, install-plugin fails closed
// and the D08 registry keeps no success record for that home.
test('install-plugin records no registry claim when the cache cannot be verified', () => {
  const home = fixtureHome('external-subagent-noclaim-');
  const fake = fakeCodexCli(home, { materialize: false });
  const paths = productPaths(home);
  const codexHome = path.join(home, 'codex-target');
  const priorPath = process.env.PATH;
  process.env.PATH = `${fake.dir}${path.delimiter}${priorPath}`;
  try {
    assert.throws(() => pluginCommand(paths, ['--codex-home', codexHome]), (error) => {
      assert.ok(error instanceof CliError);
      assert.equal(error.code, 'CODEX_CACHE_UNVERIFIABLE');
      assert.match(error.message, /no plugin cache was materialized/u);
      return true;
    });
    assert.deepEqual(loadCodexHomes(paths).registry.homes, [], 'an unverified install must not be recorded as a claimed home');
    assert.equal(fs.existsSync(path.join(home, 'plugins', 'external-subagent')), false, 'the public command surface rolls staging back');
    assert.equal(fs.existsSync(path.join(home, '.agents', 'plugins', 'marketplace.json')), false, 'the public command surface rolls the marketplace back');
  } finally {
    process.env.PATH = priorPath;
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// Host dispatch: `install-plugin zcode` goes through the stateless ZCode
// binding (no codex CLI is ever invoked), `--codex-home` is codex-only, and
// unknown hosts are rejected at the argument parser.
test('install-plugin zcode installs and removes the inline binding without touching codex', () => {
  const home = fixtureHome('external-subagent-zcode-cli-');
  const paths = productPaths(home);
  try {
    const dry = pluginCommand(paths, ['zcode', '--dry-run']);
    assert.equal(dry.dry_run, true);
    assert.equal(dry.host, 'zcode');
    assert.equal(dry.plugin_id, 'external-subagent@inline');
    assert.equal(fs.existsSync(paths.zcodeConfig), false, 'dry-run writes nothing');

    const installed = pluginCommand(paths, ['zcode']);
    assert.equal(installed.installed, true);
    assert.equal(installed.plugin_id, 'external-subagent@inline');
    const config = JSON.parse(fs.readFileSync(paths.zcodeConfig, 'utf8'));
    assert.deepEqual(config.plugins.dirs, [path.resolve(paths.zcodePlugin)]);
    assert.equal(fs.existsSync(path.join(home, '.codex')), false, 'the zcode host never creates a codex home');

    const removed = pluginCommand(paths, ['zcode', '--uninstall']);
    assert.equal(removed.uninstalled, true);
    assert.equal(removed.staging_removed, true);
    assert.equal('plugins' in JSON.parse(fs.readFileSync(paths.zcodeConfig, 'utf8')), false);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('install-plugin rejects codex-only flags on the zcode host and unknown hosts', () => {
  const home = fixtureHome('external-subagent-zcode-args-');
  const paths = productPaths(home);
  try {
    assert.throws(() => pluginCommand(paths, ['zcode', '--codex-home', path.join(home, 'codex')]), (error) => {
      assert.equal(error.code, 'INVALID_ARGUMENT');
      assert.match(error.message, /applies to the codex host only/u);
      return true;
    });
    assert.throws(() => pluginCommand(paths, ['claude']), (error) => {
      assert.equal(error.code, 'INVALID_ARGUMENT');
      assert.match(error.message, /unsupported plugin host: claude/u);
      return true;
    });
    assert.throws(() => pluginCommand(paths, ['zcode', 'codex']), (error) => {
      assert.equal(error.code, 'INVALID_ARGUMENT');
      assert.match(error.message, /plugin host was already given/u);
      return true;
    });
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// treeDigest pins: valid symlink targets may live OUTSIDE the traversed root
// and be reachable only through their links — content edits there must change
// the digest — while dangling links contribute no content and directory
// cycles stay bounded instead of re-walking the tree.
test('treeDigest follows symlink targets outside the traversed root and stays safe on dangling links and cycles', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-digest-'));
  try {
    const tree = path.join(root, 'tree');
    fs.mkdirSync(tree);
    const outsideFile = path.join(root, 'outside', 'linked.txt');
    fs.mkdirSync(path.dirname(outsideFile), { recursive: true });
    fs.writeFileSync(outsideFile, 'first');
    fs.symlinkSync(outsideFile, path.join(tree, 'file-link'));
    const outsideDir = path.join(root, 'outside-dir');
    fs.mkdirSync(outsideDir);
    fs.writeFileSync(path.join(outsideDir, 'note.txt'), 'one');
    fs.symlinkSync(outsideDir, path.join(tree, 'dir-link'));
    fs.symlinkSync(path.join(root, 'gone', 'target'), path.join(tree, 'dangling-link'));

    const before = treeDigest(tree);
    assert.match(before, /^[0-9a-f]{64}$/u);
    fs.writeFileSync(outsideFile, 'second');
    assert.notEqual(treeDigest(tree), before, 'editing an out-of-root file target reachable only via its link must change the digest');
    const afterFile = treeDigest(tree);
    fs.writeFileSync(path.join(outsideDir, 'note.txt'), 'two');
    assert.notEqual(treeDigest(tree), afterFile, 'editing content inside an out-of-root linked directory must change the digest');

    // Dangling links carry no content to hash, and a link back to the walked
    // root is bounded by its already-seen real path.
    const bounded = treeDigest(tree);
    fs.rmSync(path.join(tree, 'dangling-link'));
    assert.equal(treeDigest(tree), bounded, 'a dangling link contributes no content to the digest');
    fs.symlinkSync(tree, path.join(tree, 'cycle-link'));
    assert.equal(treeDigest(tree), bounded, 'a directory cycle is bounded, not re-walked');
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
