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

// Throwaway copy of the shipped plugin source a test can mutate (delete a
// skill, corrupt `.mcp.json`, make a file unreadable) without touching the
// repository tree.
function tempSource(tweak) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-source-'));
  fs.cpSync(pluginSourceRoot(), dir, { recursive: true });
  if (tweak) tweak(dir);
  return dir;
}

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
    assert.equal(server.timeoutMs, 300000, 'the staged MCP entry pins timeoutMs=300000 because the zcode host default tool timeout is 30000ms, below the 299s wait ceiling');
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

// AUD-002 regression oracles: staging publishes by validated REPLACEMENT,
// never by overlaying the live managed tree.  A skill deleted from the
// source disappears from the published tree, unrelated user files beside
// the staging survive, and no candidate/prior sibling residue is left.
test('refresh publishes a replacement tree: deleted source files disappear and unrelated files survive (AUD-002)', () => {
  const { home, paths } = fixture({ config: foreignConfig() });
  const source = tempSource();
  try {
    const parent = path.dirname(paths.zcodePlugin);
    fs.mkdirSync(parent, { recursive: true, mode: 0o700 });
    fs.writeFileSync(path.join(parent, 'unrelated-user-file.txt'), 'keep me');
    installZcodePlugin(paths, { source });
    assert.equal(fs.existsSync(path.join(paths.zcodePlugin, 'skills', 'external-subagent', 'SKILL.md')), true, 'the file is staged on first install');
    fs.rmSync(path.join(source, 'skills', 'external-subagent', 'SKILL.md'));
    const removed = path.join(paths.zcodePlugin, 'skills', 'external-subagent', 'SKILL.md');
    const result = installZcodePlugin(paths, { source });
    assert.equal(result.installed, true);
    assert.equal(fs.existsSync(removed), false, 'a file deleted from the source disappears from the published tree');
    const server = stagedServer(paths);
    assert.equal(server.command, process.execPath, 'the replacement tree still carries the final binding paths');
    assert.equal(server.env.ZCODE_AGENTD_SOCKET, paths.socket);
    assert.equal(fs.readFileSync(path.join(parent, 'unrelated-user-file.txt'), 'utf8'), 'keep me', 'unrelated files beside the staging survive the swap');
    assert.deepEqual(fs.readdirSync(parent).sort(), ['external-subagent', 'unrelated-user-file.txt'], 'no candidate or prior sibling residue remains');
    assert.equal(readConfig(paths).plugins.dirs.length, 1, 'the binding stays a single entry');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(source, { recursive: true, force: true });
  }
});

test('an invalid MCP JSON candidate never reaches the live tree (AUD-002)', () => {
  const { home, paths } = fixture({ config: foreignConfig() });
  const source = tempSource((dir) => fs.writeFileSync(path.join(dir, '.mcp.json'), '{ not valid json'));
  try {
    installZcodePlugin(paths);
    const treeBefore = snapshotTree(paths.zcodePlugin);
    const configBefore = fs.readFileSync(paths.zcodeConfig);
    assert.throws(() => installZcodePlugin(paths, { source }), (error) => {
      assert.equal(error.code, 'INVALID_PLUGIN_SOURCE');
      assert.match(error.message, /not readable JSON/u);
      return true;
    });
    assert.deepEqual(snapshotTree(paths.zcodePlugin), treeBefore, 'the last good tree keeps its exact bytes');
    assert.deepEqual(fs.readFileSync(paths.zcodeConfig), configBefore, 'the host config is untouched');
    assert.deepEqual(fs.readdirSync(path.dirname(paths.zcodePlugin)), ['external-subagent'], 'the rejected candidate leaves no sibling residue');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(source, { recursive: true, force: true });
  }
});

test('a copy failure preserves the prior tree and leaves no residue (AUD-002)', () => {
  const { home, paths } = fixture({ config: foreignConfig() });
  const source = tempSource((dir) => fs.chmodSync(path.join(dir, 'skills', 'external-subagent', 'SKILL.md'), 0o000));
  try {
    installZcodePlugin(paths);
    const treeBefore = snapshotTree(paths.zcodePlugin);
    assert.throws(() => installZcodePlugin(paths, { source }), (error) => error.code === 'EACCES');
    assert.deepEqual(snapshotTree(paths.zcodePlugin), treeBefore, 'the prior tree keeps its exact bytes');
    assert.deepEqual(fs.readdirSync(path.dirname(paths.zcodePlugin)), ['external-subagent'], 'the failed copy leaves no candidate residue');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.chmodSync(path.join(source, 'skills', 'external-subagent', 'SKILL.md'), 0o644);
    fs.rmSync(source, { recursive: true, force: true });
  }
});

test('a failed publish swap restores the prior tree in place (AUD-002)', () => {
  const { home, paths } = fixture({ config: foreignConfig() });
  try {
    installZcodePlugin(paths);
    // Make the last good tree distinguishable from the candidate the next
    // install will try to swap in.
    fs.writeFileSync(path.join(paths.zcodePlugin, 'skills', 'external-subagent', 'SKILL.md'), 'last-good bytes\n');
    const treeBefore = snapshotTree(paths.zcodePlugin);
    const configBefore = fs.readFileSync(paths.zcodeConfig);
    // Inject a failure into the swap's second rename (candidate -> target),
    // after the live tree has already moved to its retained prior sibling.
    const realRename = fs.renameSync;
    let renames = 0;
    fs.renameSync = function injected(from, to) {
      renames += 1;
      if (renames === 2) throw Object.assign(new Error(`injected swap failure (${from} -> ${to})`), { code: 'EACCES' });
      return realRename(from, to);
    };
    try {
      assert.throws(() => installZcodePlugin(paths), (error) => error.code === 'EACCES');
    } finally {
      fs.renameSync = realRename;
    }
    assert.deepEqual(snapshotTree(paths.zcodePlugin), treeBefore, 'the prior tree is back at the staging path after the failed swap');
    assert.deepEqual(fs.readFileSync(paths.zcodeConfig), configBefore, 'the host config was never reached');
    assert.deepEqual(fs.readdirSync(path.dirname(paths.zcodePlugin)), ['external-subagent'], 'the failed swap leaves no candidate or prior residue');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('a host config failure after publish restores the prior binding and tree (AUD-002)', () => {
  const { home, paths } = fixture({ config: foreignConfig() });
  try {
    installZcodePlugin(paths);
    fs.writeFileSync(path.join(paths.zcodePlugin, 'skills', 'external-subagent', 'SKILL.md'), 'last-good bytes\n');
    const treeBefore = snapshotTree(paths.zcodePlugin);
    const configBefore = fs.readFileSync(paths.zcodeConfig);
    // Fail exactly the first config write after the swap; the rollback's own
    // write (restoring the prior bytes) must still go through.
    const realWrite = fs.writeFileSync;
    let injected = false;
    fs.writeFileSync = function injectedWrite(file, ...rest) {
      if (!injected && path.dirname(path.resolve(String(file))) === path.dirname(paths.zcodeConfig)) {
        injected = true;
        throw Object.assign(new Error('injected config write failure'), { code: 'EACCES' });
      }
      return realWrite(file, ...rest);
    };
    try {
      assert.throws(() => installZcodePlugin(paths), (error) => error.code === 'EACCES');
    } finally {
      fs.writeFileSync = realWrite;
    }
    assert.deepEqual(snapshotTree(paths.zcodePlugin), treeBefore, 'the prior managed tree is restored byte-for-byte');
    assert.deepEqual(fs.readFileSync(paths.zcodeConfig), configBefore, 'the host config keeps its exact prior bytes');
    assert.deepEqual(fs.readdirSync(path.dirname(paths.zcodePlugin)), ['external-subagent'], 'no candidate or prior residue remains');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('a verification failure after commit restores the prior binding and tree (AUD-002)', () => {
  const { home, paths } = fixture({ config: foreignConfig() });
  try {
    installZcodePlugin(paths);
    fs.writeFileSync(path.join(paths.zcodePlugin, 'skills', 'external-subagent', 'SKILL.md'), 'last-good bytes\n');
    const treeBefore = snapshotTree(paths.zcodePlugin);
    const configBefore = fs.readFileSync(paths.zcodeConfig);
    // Fail the staged `.mcp.json` read inside verifyZcodeBinding.  The first
    // read of that path is the prepare-time ownership probe (allowed); the
    // second is the post-commit verification.
    const realRead = fs.readFileSync;
    const verifyTarget = path.join(paths.zcodePlugin, '.mcp.json');
    let reads = 0;
    fs.readFileSync = function injectedRead(file, ...rest) {
      if (path.resolve(String(file)) === verifyTarget) {
        reads += 1;
        if (reads === 2) throw Object.assign(new Error('injected verify read failure'), { code: 'EIO' });
      }
      return realRead(file, ...rest);
    };
    try {
      assert.throws(() => installZcodePlugin(paths), (error) => error.code === 'EIO');
    } finally {
      fs.readFileSync = realRead;
    }
    assert.deepEqual(snapshotTree(paths.zcodePlugin), treeBefore, 'the prior managed tree is restored byte-for-byte');
    assert.deepEqual(fs.readFileSync(paths.zcodeConfig), configBefore, 'the host config is rolled back to its prior bytes');
    assert.deepEqual(fs.readdirSync(path.dirname(paths.zcodePlugin)), ['external-subagent'], 'no candidate or prior residue remains');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// Real permission bits only bind for ordinary users; root bypasses them,
// so the persistent-denial oracle below skips instead of pretending its
// chmod verified anything.
const notRoot = process.getuid?.() !== 0 ? false : 'permission bits are not enforced for root';

// The AUD-002 reopened counterexample (audit 20260918-1319): the rollback
// used to be a sequential chain, so a config parent that STAYS read-only
// failed the config restore and thereby skipped the staging restore — the
// command failed while the old binding's path silently held the NEW tree.
// Both restores are now independent guarded attempts: the staging restore
// must still run and succeed, the config keeps its exact prior bytes, and
// the unrestored config is named on the thrown error, not just logged.
test('a persistently read-only config parent still restores the prior staging tree and reports the unrestored config (AUD-002)', { skip: notRoot }, () => {
  const { home, paths } = fixture({ config: foreignConfig() });
  const source = tempSource((dir) => fs.appendFileSync(path.join(dir, 'skills', 'external-subagent', 'SKILL.md'), '\ncandidate marker\n'));
  const configParent = path.dirname(paths.zcodeConfig);
  try {
    installZcodePlugin(paths);
    fs.writeFileSync(path.join(paths.zcodePlugin, 'skills', 'external-subagent', 'SKILL.md'), 'last-good bytes\n');
    const treeBefore = snapshotTree(paths.zcodePlugin);
    const configBefore = fs.readFileSync(paths.zcodeConfig);
    fs.chmodSync(configParent, 0o500);
    let failure = null;
    try { installZcodePlugin(paths, { source }); } catch (error) { failure = error; }
    assert.ok(failure, 'the refresh must fail while the config parent is read-only');
    assert.equal(failure.code, 'EACCES', 'the original install failure surfaces, not a secondary restore error');
    assert.ok(Array.isArray(failure.restoreFailures), 'the error carries the unrestored-resource list');
    assert.deepEqual(failure.restoreFailures.map((entry) => entry.resource), [paths.zcodeConfig], 'exactly the config restore is reported unrestored');
    assert.equal(failure.restoreFailures[0].code, 'EACCES');
    assert.deepEqual(snapshotTree(paths.zcodePlugin), treeBefore, 'the staging restore ran despite the denied config restore and returned the prior tree');
    assert.deepEqual(fs.readFileSync(paths.zcodeConfig), configBefore, 'the config keeps its exact prior bytes');
    assert.deepEqual(fs.readdirSync(path.dirname(paths.zcodePlugin)), ['external-subagent'], 'the restored prior tree leaves no sibling residue');
  } finally {
    fs.chmodSync(configParent, 0o700);
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(source, { recursive: true, force: true });
  }
});

// The mirror image of the counterexample: when the restore that fails is
// the STAGING one, the retained prior tree must survive as the last-good
// recovery material (complete() must never run to delete it), and the
// unrestored staging path must ride the original error.
test('a failed staging restore keeps the retained prior tree and is reported on the error (AUD-002)', () => {
  const { home, paths } = fixture({ config: foreignConfig() });
  try {
    installZcodePlugin(paths);
    fs.writeFileSync(path.join(paths.zcodePlugin, 'skills', 'external-subagent', 'SKILL.md'), 'last-good bytes\n');
    const configBefore = fs.readFileSync(paths.zcodeConfig);
    // Two injections: fail the post-commit verification (2nd read of the
    // staged `.mcp.json`), then fail restore()'s prior -> staging rename —
    // identified by its source path, the retained prior sibling, so the
    // injection cannot be confused with publish's own renames or the
    // config rollback's atomic rename.
    const realRead = fs.readFileSync;
    const realRename = fs.renameSync;
    const verifyTarget = path.join(paths.zcodePlugin, '.mcp.json');
    let reads = 0;
    fs.readFileSync = function injectedRead(file, ...rest) {
      if (path.resolve(String(file)) === verifyTarget) {
        reads += 1;
        if (reads === 2) throw Object.assign(new Error('injected verify read failure'), { code: 'EIO' });
      }
      return realRead(file, ...rest);
    };
    fs.renameSync = function injectedRename(from, to) {
      if (path.basename(String(from)).startsWith('.external-subagent.prior.')) {
        throw Object.assign(new Error(`injected restore rename failure (${from} -> ${to})`), { code: 'EACCES' });
      }
      return realRename(from, to);
    };
    let failure = null;
    try {
      try { installZcodePlugin(paths); } catch (error) { failure = error; }
    } finally {
      fs.readFileSync = realRead;
      fs.renameSync = realRename;
    }
    assert.ok(failure, 'the refresh must fail at the injected verification');
    assert.equal(failure.code, 'EIO', 'the original verification failure surfaces, not a secondary restore error');
    assert.deepEqual(failure.restoreFailures.map((entry) => entry.code), ['PLUGIN_STAGING_RESTORE_FAILED'], 'the failed staging restore is reported on the error');
    assert.equal(failure.restoreFailures[0].resource, paths.zcodePlugin);
    assert.deepEqual(fs.readFileSync(paths.zcodeConfig), configBefore, 'the independent config restore still succeeded');
    assert.equal(fs.existsSync(paths.zcodePlugin), false, 'the freshly published tree was discarded');
    const siblings = fs.readdirSync(path.dirname(paths.zcodePlugin));
    assert.equal(siblings.length, 1, 'exactly one retained tree remains beside the staging path');
    assert.match(siblings[0], /^\.external-subagent\.prior\./u, 'the last-good prior copy is kept as recovery material');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('a failed first install leaves no binding behind (AUD-002)', () => {
  const { home, paths } = fixture();
  try {
    fs.mkdirSync(path.dirname(paths.zcodeConfig), { recursive: true, mode: 0o700 });
    fs.chmodSync(path.dirname(paths.zcodeConfig), 0o500);
    assert.throws(() => installZcodePlugin(paths), (error) => error.code === 'EACCES');
    assert.equal(fs.existsSync(paths.zcodePlugin), false, 'no staging tree is left behind');
    assert.equal(fs.existsSync(paths.zcodeConfig), false, 'no config binding is left behind');
    assert.deepEqual(fs.readdirSync(path.dirname(paths.zcodePlugin)), [], 'no candidate or prior residue remains');
  } finally {
    fs.chmodSync(path.dirname(paths.zcodeConfig), 0o700);
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
    assert.equal(server.timeoutMs, zcodeMcpBinding(paths.zcodePlugin).timeoutMs, 'the refreshed binding carries the managed tool timeout');
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
