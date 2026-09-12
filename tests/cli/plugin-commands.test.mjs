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
import { installMcp, treeDigest } from '../../cli/install/codex.mjs';
import { loadCodexHomes } from '../../cli/install/reconcile.mjs';
import { productPaths } from '../../cli/paths.mjs';

function fixtureHome(prefix = 'external-subagent-cli-') {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

// Minimal stand-in for the verified codex CLI surface (0.153.4 JSON shapes).
// Named `codex` and exposed through PATH: the command layer resolves the CLI
// the same way the real binary would be found.
function fakeCodexCli(directory) {
  const log = path.join(directory, 'codex-invocations.jsonl');
  const script = path.join(directory, 'codex');
  fs.writeFileSync(script, `#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
const args = process.argv.slice(2);
const text = (value) => { process.stdout.write(JSON.stringify(value, null, 2) + '\\n'); };
if (args[0] === 'plugin' && args[1] === 'add' && args.includes('--help')) { process.stdout.write('usage\\n'); process.exit(0); }
if (args[0] === 'plugin' && args[1] === 'marketplace' && args[2] === 'add') { text({ marketplaceName: 'personal', installedRoot: args[3] }); process.exit(0); }
if (args[0] === 'plugin' && args[1] === 'add') {
  const name = args[2]; const marketplace = args[args.indexOf('--marketplace') + 1];
  text({ pluginId: name + '@' + marketplace, name, marketplaceName: marketplace, version: '0.1.0',
    installedPath: path.join(process.env.CODEX_HOME || '', 'plugins', 'cache', marketplace, name, '0.1.0') });
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
