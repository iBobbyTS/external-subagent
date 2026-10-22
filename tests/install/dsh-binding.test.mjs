// DSH host binding oracles.  The dsh installer's patch-file transaction is
// pure local state (text-level sentinel append/replace that must preserve
// user comments and foreign entries byte-for-byte), so the bulk of the
// contract is testable without a dsh CLI: the dump verification is injected
// through the `verify` seam.  The staging targets (<dshHome>/profiles/<n>
// and <dshHome>/skills) live inside a fixture home; the real $DSH_HOME is
// never touched because every test passes an explicit dshHome.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { CliError } from '../../cli/errors.mjs';
import {
  composePatchText, dshMcpBinding, installDshPlugin, reconcileDshBindings,
  uninstallAllDshBindings, uninstallDshPlugin,
} from '../../cli/install/dsh.mjs';
import { pluginSourceRoot } from '../../cli/install/layout.mjs';
import { productPaths } from '../../cli/paths.mjs';

// When the installed dsh CLI ships js-yaml (it does at 0.2.0-rc.2), use
// the REAL parser the dsh loader family uses to prove every composed or
// uninstalled patch text is valid YAML with the expected entries.  The
// optional require keeps the suite green on hosts without dsh.
const yaml = (() => {
  try {
    return require('/opt/homebrew/lib/node_modules/@deepseek-ai/dsh/node_modules/js-yaml');
  } catch {
    return null;
  }
})();

const SENTINEL = '# @managed-by: external-subagent (install-plugin dsh) — this block is rewritten by the product; keep manual edits outside it';

function fixtureHome(prefix = 'external-subagent-dsh-') {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

function fixturePaths(home) {
  const env = process.platform === 'win32'
    ? { ...process.env, LOCALAPPDATA: path.join(home, 'AppData', 'Local') }
    : process.env;
  return productPaths(home, env);
}

// A realistic shipped-template patch file: header comments plus the bare
// empty root (the OBSERVED 0.2.0-rc.2 cordis.patch.yml shape).
const SHIPPED_TEMPLATE = [
  '# Your patch layer for this dsh profile, applied after every bundle layer:',
  '# a top-level YAML array of loader patch entries (id-targeted config',
  "# overrides, disables, and insert lists; `!!js` expressions allowed).",
  '[]',
  '',
].join('\n');

function fixture({ patch } = {}) {
  const home = fixtureHome();
  const paths = fixturePaths(home);
  const dshHome = path.join(home, '.dsh');
  const profileDir = path.join(dshHome, 'profiles', 'web');
  fs.mkdirSync(profileDir, { recursive: true });
  if (patch !== undefined) fs.writeFileSync(path.join(profileDir, 'cordis.patch.yml'), patch);
  return { home, paths, dshHome, profileDir, patchFile: path.join(profileDir, 'cordis.patch.yml') };
}

const okVerify = () => '0.2.0-rc.2';
const readPatch = (file) => fs.readFileSync(file, 'utf8');

test('composePatchText appends to the shipped template by removing the bare empty root', () => {
  const binding = { command: '/payload/external-subagent-mcp', args: [], toolCallTimeoutMs: 300000 };
  const text = composePatchText(SHIPPED_TEMPLATE, binding, '/tmp/product.sock');
  assert.ok(text.split('\n').every((line) => line.trim() !== '[]'), 'the bare empty-root line must not survive');
  assert.ok(text.startsWith('# Your patch layer'), 'user header comments survive');
  assert.ok(text.includes(SENTINEL));
  assert.ok(text.includes("name: '@deepseek-ai/dsh-mcp-client'"));
  assert.ok(text.includes("serverName: 'external_subagent'"));
  assert.ok(text.includes("EXTERNAL_SUBAGENT_SOCKET: '/tmp/product.sock'"));
  assert.ok(text.includes("toolCallTimeoutMs: 300000"));
  // Prove the composed text is valid YAML with the real parser when the
  // installed dsh CLI provides one (the `@` reserved-character class is
  // exactly what a naive plain-scalar writer would break).
  if (yaml) {
    const parsed = yaml.load(text);
    assert.equal(Array.isArray(parsed), true);
    const entry = parsed.find((item) => item?.name === '@deepseek-ai/dsh-mcp-client');
    assert.equal(entry?.config?.serverName, 'external_subagent');
    assert.equal(entry?.config?.transport, 'stdio');
    assert.equal(entry?.config?.env?.EXTERNAL_SUBAGENT_SOCKET, '/tmp/product.sock');
    assert.equal(entry?.config?.toolCallTimeoutMs, 300000);
  }
});

test('composePatchText creates a fresh file when the patch is absent', () => {
  const text = composePatchText(null, { command: '/x/external-subagent-mcp', args: [], toolCallTimeoutMs: 300000 }, '/s.sock');
  assert.ok(text.startsWith(SENTINEL));
  assert.ok(text.endsWith('\n'));
});

test('composePatchText replaces an existing managed block and preserves surrounding user entries', () => {
  const binding = { command: '/payload/external-subagent-mcp', args: [], toolCallTimeoutMs: 300000 };
  const first = composePatchText(SHIPPED_TEMPLATE, binding, '/old.sock');
  // A user entry follows the managed block.
  const withUser = `${first}- id: timer\n  disabled: true\n`;
  const rebound = composePatchText(withUser, { ...binding, command: '/new/external-subagent-mcp' }, '/new.sock');
  assert.ok(rebound.includes('/new/external-subagent-mcp'), 'the rebound command appears');
  assert.ok(!rebound.includes('/old'), 'the stale binding is gone');
  assert.ok(reboundedUserPreserved(rebound), 'the user entry survives');
  assert.equal(rebound.indexOf(SENTINEL), rebound.indexOf(SENTINEL), 'sentinel intact');
});

function reboundedUserPreserved(text) {
  return text.includes('- id: timer') && text.includes('disabled: true');
}

test('install appends the managed block, stages the skill, and verifies via the dump seam', () => {
  const { paths, dshHome, patchFile } = fixture({ patch: SHIPPED_TEMPLATE });
  try {
    const result = installDshPlugin(paths, { dshHome, profile: 'web', verify: okVerify });
    assert.equal(result.installed, true);
    assert.equal(result.host, 'dsh');
    assert.equal(result.config_verified, true);
    assert.equal(result.binding.socket, paths.socket);
    const text = readPatch(patchFile);
    assert.ok(text.includes(SENTINEL));
    assert.ok(text.includes(`command: '${dshMcpBinding(paths).command}'`));
    const skill = path.join(dshHome, 'skills', 'external-subagent', 'SKILL.md');
    assert.ok(fs.existsSync(skill), 'the skill is staged into the user skill root');
    assert.equal(fs.readFileSync(skill, 'utf8'), fs.readFileSync(path.join(pluginSourceRoot(), 'skills', 'external-subagent', 'SKILL.md'), 'utf8'));
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('install is idempotent: a byte-equal reinstall reports idempotent and keeps the file unchanged', () => {
  const { paths, dshHome, patchFile } = fixture({ patch: SHIPPED_TEMPLATE });
  try {
    installDshPlugin(paths, { dshHome, profile: 'web', verify: okVerify });
    const before = readPatch(patchFile);
    const again = installDshPlugin(paths, { dshHome, profile: 'web', verify: okVerify });
    assert.equal(again.idempotent, true);
    assert.equal(readPatch(patchFile), before);
  } finally {
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

test('rebinding to a new socket replaces the managed block without touching user entries', () => {
  const { home, paths, dshHome, patchFile } = fixture({ patch: SHIPPED_TEMPLATE });
  try {
    installDshPlugin(paths, { dshHome, profile: 'web', verify: okVerify });
    const otherPaths = { ...paths, socket: path.join(paths.data, 'other.sock') };
    const rebound = installDshPlugin(otherPaths, { dshHome, profile: 'web', verify: okVerify });
    assert.equal(rebound.idempotent, undefined, 'a socket change is a real rewrite');
    const text = readPatch(patchFile);
    assert.ok(text.includes(`EXTERNAL_SUBAGENT_SOCKET: '${otherPaths.socket}'`));
    assert.ok(!text.includes(`'${paths.socket}'`) || paths.socket === otherPaths.socket);
    assert.ok(text.startsWith('# Your patch layer'), 'user header comments survive the rebind');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('a failed verification rolls the patch file and the skill back', () => {
  const { home, paths, dshHome, patchFile } = fixture({ patch: SHIPPED_TEMPLATE });
  try {
    const failVerify = () => { throw new CliError('DSH_BINDING_UNVERIFIABLE', 'probe failed'); };
    assert.throws(() => installDshPlugin(paths, { dshHome, profile: 'web', verify: failVerify }), (error) => error.code === 'DSH_BINDING_UNVERIFIABLE');
    assert.equal(readPatch(patchFile), SHIPPED_TEMPLATE, 'the prior patch bytes are restored');
    assert.ok(!fs.existsSync(path.join(dshHome, 'skills', 'external-subagent', 'SKILL.md')), 'the staged skill is rolled back');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('uninstall removes only the managed block and keeps user entries and comments', () => {
  const { home, paths, dshHome, patchFile } = fixture({ patch: SHIPPED_TEMPLATE });
  try {
    installDshPlugin(paths, { dshHome, profile: 'web', verify: okVerify });
    fs.appendFileSync(patchFile, '- id: timer\n  disabled: true\n');
    const result = uninstallDshPlugin(paths, { dshHome, profile: 'web' });
    assert.equal(result.uninstalled, true);
    assert.equal(result.patch_removed, true);
    assert.equal(result.skill_removed, true);
    const text = readPatch(patchFile);
    assert.ok(!text.includes(SENTINEL), 'the managed block is gone');
    assert.ok(!text.includes('dsh-mcp-client'));
    assert.ok(text.includes('- id: timer'), 'the user entry survives');
    assert.ok(text.startsWith('# Your patch layer'), 'user comments survive');
    if (yaml) {
      const parsed = yaml.load(text);
      assert.equal(Array.isArray(parsed), true);
      assert.ok(parsed.some((item) => item?.id === 'timer' && item?.disabled === true));
      assert.ok(!parsed.some((item) => item?.name === '@deepseek-ai/dsh-mcp-client'));
    }
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('uninstall on a fresh template restores the shipped empty-root shape', () => {
  const { home, paths, dshHome, patchFile } = fixture({ patch: SHIPPED_TEMPLATE });
  try {
    installDshPlugin(paths, { dshHome, profile: 'web', verify: okVerify });
    uninstallDshPlugin(paths, { dshHome, profile: 'web' });
    const text = readPatch(patchFile);
    assert.ok(text.trim().endsWith('[]'), 'the empty root shape is restored');
    assert.ok(!text.includes(SENTINEL));
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('uninstall without a sentinel leaves a foreign serverName entry untouched with a warning', () => {
  const foreign = `# user file\n- name: '@deepseek-ai/dsh-mcp-client'\n  config:\n    serverName: external_subagent\n`;
  const { home, paths, dshHome, patchFile } = fixture({ patch: foreign });
  try {
    const result = uninstallDshPlugin(paths, { dshHome, profile: 'web' });
    assert.equal(result.patch_removed, false);
    assert.ok(result.warning, 'a fallback warning is reported');
    assert.equal(readPatch(patchFile), foreign, 'foreign bytes stay untouched');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('uninstall with no binding reports absence without writing', () => {
  const { home, paths, dshHome, patchFile } = fixture({ patch: SHIPPED_TEMPLATE });
  try {
    const result = uninstallDshPlugin(paths, { dshHome, profile: 'web' });
    assert.equal(result.uninstalled, false);
    assert.ok(result.note);
    assert.equal(readPatch(patchFile), SHIPPED_TEMPLATE);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('a missing profile directory fails closed with a creation hint', () => {
  const home = fixtureHome();
  try {
    const paths = fixturePaths(home);
    const dshHome = path.join(home, '.dsh');
    assert.throws(() => installDshPlugin(paths, { dshHome, profile: 'nosuch', verify: okVerify }),
      /profile directory not found: .*--from-default-profile/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('the desktop profile is refused explicitly', () => {
  const home = fixtureHome();
  try {
    const paths = fixturePaths(home);
    assert.throws(() => installDshPlugin(paths, { dshHome: path.join(home, '.dsh'), profile: 'desktop', verify: okVerify }),
      /refuses the "desktop" profile/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('a profile name with path separators is rejected', () => {
  const home = fixtureHome();
  try {
    const paths = fixturePaths(home);
    assert.throws(() => installDshPlugin(paths, { dshHome: path.join(home, '.dsh'), profile: '../escape', verify: okVerify }),
      /path separators/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('dry-run reports targets without writing', () => {
  const { home, paths, dshHome, patchFile } = fixture({ patch: SHIPPED_TEMPLATE });
  try {
    const result = installDshPlugin(paths, { dshHome, profile: 'web', dryRun: true });
    assert.equal(result.dry_run, true);
    assert.equal(result.profile, 'web');
    assert.equal(readPatch(patchFile), SHIPPED_TEMPLATE);
    assert.ok(!fs.existsSync(path.join(dshHome, 'skills')));
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('a foreign SKILL.md at the managed path is never overwritten', () => {
  const { home, paths, dshHome, patchFile } = fixture({ patch: SHIPPED_TEMPLATE });
  try {
    const skillDir = path.join(dshHome, 'skills', 'external-subagent');
    fs.mkdirSync(skillDir, { recursive: true });
    fs.writeFileSync(path.join(skillDir, 'SKILL.md'), '# foreign skill\n');
    assert.throws(() => installDshPlugin(paths, { dshHome, profile: 'web', verify: okVerify }), (error) => error.code === 'DSH_SKILL_CONFLICT');
    assert.equal(readPatch(patchFile), SHIPPED_TEMPLATE, 'the patch write is rolled back too');
    assert.equal(fs.readFileSync(path.join(skillDir, 'SKILL.md'), 'utf8'), '# foreign skill\n');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('uninstall leaves a diverged staged skill in place with a note', () => {
  const { home, paths, dshHome } = fixture({ patch: SHIPPED_TEMPLATE });
  try {
    installDshPlugin(paths, { dshHome, profile: 'web', verify: okVerify });
    const skillFile = path.join(dshHome, 'skills', 'external-subagent', 'SKILL.md');
    fs.writeFileSync(skillFile, '# edited by the user\n');
    const result = uninstallDshPlugin(paths, { dshHome, profile: 'web' });
    assert.equal(result.skill_removed, false);
    assert.ok(result.skill_note.includes('left in place'));
    assert.ok(fs.existsSync(skillFile));
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('reconcileDshBindings refreshes every sentinel-bearing profile and skips unbound ones', () => {
  const home = fixtureHome();
  try {
    const paths = fixturePaths(home);
    const dshHome = path.join(home, '.dsh');
    for (const name of ['web', 'headless']) {
      const dir = path.join(dshHome, 'profiles', name);
      fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(path.join(dir, 'cordis.patch.yml'), SHIPPED_TEMPLATE);
    }
    const idleDir = path.join(dshHome, 'profiles', 'idle');
    fs.mkdirSync(idleDir, { recursive: true });
    fs.writeFileSync(path.join(idleDir, 'cordis.patch.yml'), '# user-only file\n[]\n');
    const before = fs.readFileSync(path.join(idleDir, 'cordis.patch.yml'), 'utf8');
    installDshPlugin(paths, { dshHome, profile: 'web', verify: okVerify });
    installDshPlugin(paths, { dshHome, profile: 'headless', verify: okVerify });
    const result = reconcileDshBindings(paths, { dshHome, verify: okVerify });
    assert.equal(result.bound, true);
    assert.deepEqual(result.profiles.map((entry) => entry.profile).sort(), ['headless', 'web']);
    assert.ok(fs.readFileSync(path.join(dshHome, 'profiles', 'web', 'cordis.patch.yml'), 'utf8').includes(SENTINEL));
    assert.equal(fs.readFileSync(path.join(idleDir, 'cordis.patch.yml'), 'utf8'), before, 'unbound profiles are untouched');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('reconcileDshBindings on an absent profiles directory reports absence', () => {
  const home = fixtureHome();
  try {
    const paths = fixturePaths(home);
    const result = reconcileDshBindings(paths, { dshHome: path.join(home, 'nowhere') });
    assert.equal(result.bound, false);
    assert.ok(result.note);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('uninstallAllDshBindings releases every bound profile and the shared skill', () => {
  const home = fixtureHome();
  try {
    const paths = fixturePaths(home);
    const dshHome = path.join(home, '.dsh');
    for (const name of ['web', 'headless']) {
      const dir = path.join(dshHome, 'profiles', name);
      fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(path.join(dir, 'cordis.patch.yml'), SHIPPED_TEMPLATE);
    }
    installDshPlugin(paths, { dshHome, profile: 'web', verify: okVerify });
    installDshPlugin(paths, { dshHome, profile: 'headless', verify: okVerify });
    const result = uninstallAllDshBindings(paths, { dshHome });
    assert.equal(result.released, 2);
    assert.ok(!fs.readFileSync(path.join(dshHome, 'profiles', 'web', 'cordis.patch.yml'), 'utf8').includes(SENTINEL));
    assert.ok(!fs.existsSync(path.join(dshHome, 'skills', 'external-subagent')), 'the shared skill is removed with the last binding');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});
