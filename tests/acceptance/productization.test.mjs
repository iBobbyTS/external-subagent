// Productization acceptance invariants (S04 consumer surface). These are
// controlled, always-on checks for the seams the productization closeout
// touched: the managed plugin's cache identity (codex materializes plugin
// caches keyed by plugin@marketplace@version, so the manifest version is a
// release-bearing field), the staged MCP binding placeholders, and the
// acceptance-matrix deliverable that must stay tied to the recorded live
// evidence. Live consumer runs live in tests/integration/double-provider.test.mjs
// and docs/acceptance/productization.md, not here.
import fs from 'node:fs';
import path from 'node:path';
import assert from 'node:assert/strict';
import { test } from 'node:test';

const root = path.resolve(import.meta.dirname, '../..');
const pluginRoot = path.join(root, 'plugins', 'codex', 'external-subagent');

test('managed plugin manifest carries a distinct semver cache identity', () => {
  const manifest = JSON.parse(fs.readFileSync(path.join(pluginRoot, '.codex-plugin', 'plugin.json'), 'utf8'));
  assert.equal(manifest.name, 'external-subagent');
  assert.equal(manifest.skills, './skills/');
  assert.equal(manifest.mcpServers, './.mcp.json');
  // codex 0.153.4 materializes `plugin add` caches from a machine-global
  // content store keyed by plugin@marketplace@version: two installations of
  // the same identity share the store's bytes, so a released candidate must
  // carry its own version identity (see docs/compatibility/codex.md).
  assert.match(manifest.version, /^\d+\.\d+\.\d+$/u, 'plugin version must be a plain semver identity');
  assert.notEqual(manifest.version, '0.1.0', 'identity 0.1.0 is already cached by the historical installation; a release must not reuse it');
});

test('staged plugin MCP binding keeps exactly the managed placeholders', () => {
  const mcp = JSON.parse(fs.readFileSync(path.join(pluginRoot, '.mcp.json'), 'utf8'));
  assert.deepEqual(Object.keys(mcp.mcpServers), ['external_subagent']);
  const server = mcp.mcpServers.external_subagent;
  assert.equal(server.command, '__EXTERNAL_SUBAGENT_MCP_EXECUTABLE__');
  assert.equal(server.env.ZCODE_AGENTD_SOCKET, '__EXTERNAL_SUBAGENT_SOCKET__');
  assert.equal(mcp.mcpServers.zcode_as_subagent, undefined);
});

test('acceptance matrix deliverable records the four live consumer cells', () => {
  const doc = fs.readFileSync(path.join(root, 'docs', 'acceptance', 'productization.md'), 'utf8');
  for (const marker of ['S04_CLI_DSH_OK', 'S04_CLI_ZCODE_OK', 'S04_MCP_DSH_OK', 'S04_MCP_ZCODE_OK']) {
    assert.ok(doc.includes(marker), `acceptance matrix must record the ${marker} cell`);
  }
  assert.ok(doc.includes('REGISTRY_PUBLICATION_PENDING'), 'publication status must stay explicit until authorized');
});

test('acceptance doc attributes the zcode probe home to the real user home, not an isolated HOME', () => {
  const doc = fs.readFileSync(path.join(root, 'docs', 'acceptance', 'productization.md'), 'utf8');
  // Native-review correction: the launchd-run daemon had no HOME from the
  // bootstrapping shell (the plist sets none), so the zcode probe's recorded
  // scope.home was the real /Users/ibobby. The doc must attribute the
  // policy_unverified boundary to that evidence gap — never to an isolated
  // scratch HOME — while the ZCode spawn cells stay the capability proof.
  assert.match(doc, /scope\.home/u, 'the probe scope attribution must stay explicit');
  assert.match(doc, /\/Users\/ibobby/u);
  assert.match(doc, /launchd set no `HOME`/u);
  assert.doesNotMatch(doc, /isolated HOME without the policy hook/u, 'the withdrawn isolated-HOME attribution must not return');
});
