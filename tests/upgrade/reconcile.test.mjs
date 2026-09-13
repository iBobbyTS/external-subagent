// R2 upgrade oracle: two REAL npm tarballs at different versions drive an
// isolated-prefix upgrade through the public update/reconcile commands.  vA is
// packed from this repository; vB is packed from a temporary copy whose
// package/CLI/daemon-crate versions are bumped and whose native payload is
// rebuilt, so the daemon artifact genuinely changes bytes.  vA installs into a
// throwaway npm prefix, an explicit `init` stages the service and claims the
// Codex home, and the installed tree's own updateCommand establishes the vA
// active identity.  Installing vB into the SAME prefix and running the public
// update must republish active with vB's version and verified daemon artifact,
// hand that verified identity to service activation, and leave the retired vA
// identity unavailable; a failed activation must preserve the published active
// byte-for-byte.
//
// All writes stay inside mkdtemp fixtures: npm prefix/cache and HOME are
// per-test temp directories, launchd stays neutralized through the documented
// EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL seam, and the daemon RPC/service
// activation are controlled seams injected by a driver subprocess that imports
// the INSTALLED package's own modules (never the repository copy).  Real
// launchd bootstrap/bootout, a real running daemon PID/health check, and real
// user HOME installation are NOT_RUN here and stay acceptance-gated outside
// this file.
import test, { after } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawnSync } from 'node:child_process';

const repoRoot = path.resolve(import.meta.dirname, '../..');
const testable = process.platform === 'darwin' && process.arch === 'arm64';
const VERSION_A = JSON.parse(fs.readFileSync(path.join(repoRoot, 'package.json'), 'utf8')).version;
const VERSION_B = (() => { const [major, minor, patch] = VERSION_A.split('.').map(Number); return `${major}.${minor}.${patch + 1}`; })();
const PLATFORM_DIR = path.join('npm', 'native', 'darwin-arm64');

const ctx = { workDir: null, home: null, prefix: null, packageRoot: null, cli: null, tgzA: null, tgzB: null,
  driver: null, fake: null, serviceLog: null, initReport: null, shaA: null, shaB: null, vbPromise: null };

const sha256 = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');
const run = (command, args, options = {}) => spawnSync(command, args, { encoding: 'utf8', ...options });

function paths() {
  const data = path.join(ctx.home, 'Library', 'Application Support', 'external-subagent');
  return { home: ctx.home, data, state: path.join(data, 'install-state.json'), launchAgent: path.join(ctx.home, 'Library', 'LaunchAgents', 'com.external-subagent.daemon.plist') };
}
const readState = () => JSON.parse(fs.readFileSync(paths().state, 'utf8'));
const readReceipt = () => JSON.parse(fs.readFileSync(`${paths().state}.activation.json`, 'utf8'));
const serviceActivations = () => (fs.existsSync(ctx.serviceLog) ? fs.readFileSync(ctx.serviceLog, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse) : []);
const daemonEntry = () => path.join(fs.realpathSync(ctx.packageRoot), PLATFORM_DIR, 'external-subagentd');
const binEntry = () => path.join(fs.realpathSync(ctx.packageRoot), 'bin', 'external-subagent.mjs');
const installedDaemonSha = () => {
  const manifest = JSON.parse(fs.readFileSync(path.join(ctx.packageRoot, PLATFORM_DIR, 'payload.json'), 'utf8'));
  return manifest.files.find((file) => file.name === 'external-subagentd').sha256;
};

// The installed directory is the oracle's source of truth: the package
// manifest, the payload manifest, and every payload byte must agree.
function assertInstalledPayload(version) {
  const pkg = JSON.parse(fs.readFileSync(path.join(ctx.packageRoot, 'package.json'), 'utf8'));
  assert.equal(pkg.version, version, 'installed package.json version');
  const dir = path.join(ctx.packageRoot, PLATFORM_DIR);
  const manifest = JSON.parse(fs.readFileSync(path.join(dir, 'payload.json'), 'utf8'));
  assert.equal(manifest.version, version, 'installed payload manifest version');
  assert.equal(manifest.platform, 'darwin-arm64');
  assert.deepEqual(manifest.files.map((file) => file.name).sort(), ['external-subagent-mcp', 'external-subagentd']);
  for (const file of manifest.files) {
    const target = path.join(dir, file.name);
    const stat = fs.statSync(target);
    assert.equal(stat.mode & 0o777, 0o755, `${file.name} keeps release permissions`);
    const bytes = fs.readFileSync(target);
    assert.equal(bytes.length, file.bytes, `${file.name} byte count matches the manifest`);
    assert.equal(sha256(bytes), file.sha256, `${file.name} digest matches the manifest`);
    assert.equal(bytes.subarray(0, 4).toString('hex'), 'cffaedfe', `${file.name} is a little-endian Mach-O 64-bit image`);
    assert.equal(bytes.readUInt32LE(4), 0x0100000c, `${file.name} is arm64`);
  }
}

function childEnv(extra = {}) {
  const env = {
    ...process.env,
    HOME: ctx.home,
    CODEX_HOME: path.join(ctx.home, '.codex'),
    EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL: '1',
    PATH: `${ctx.fake.dir}:${process.env.PATH || ''}`,
    FAKE_CODEX_LOG: ctx.fake.log,
    ...extra,
  };
  delete env.ZCODE_AGENTD_SOCKET;
  return env;
}

function runDriver(label, args = [], options = {}) {
  const result = run(process.execPath, [ctx.driver], {
    env: childEnv({
      R2_PACKAGE_ROOT: ctx.packageRoot,
      R2_HOME: ctx.home,
      R2_SERVICE_LOG: ctx.serviceLog,
      R2_CLAIM: `r2-${label}`,
      R2_LABEL: label,
      R2_ARGS: JSON.stringify(args),
      R2_FAIL_ACTIVATION: options.failActivation ? '1' : '0',
    }),
  });
  assert.equal(result.stderr, '', `driver ${label} stderr: ${result.stderr}`);
  assert.ok(result.stdout.trim().startsWith('{'), `driver ${label} printed no JSON: ${result.stdout.slice(0, 200)}`);
  return JSON.parse(result.stdout);
}

function fakeCodexCli(directory) {
  const log = path.join(directory, 'codex-invocations.jsonl');
  const script = path.join(directory, 'codex');
  fs.writeFileSync(script, `#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const log = process.env.FAKE_CODEX_LOG || path.join(path.dirname(fileURLToPath(import.meta.url)), 'codex-invocations.jsonl');
const args = process.argv.slice(2);
fs.appendFileSync(log, JSON.stringify({ args, codex_home: process.env.CODEX_HOME }) + '\\n');
const text = (value) => { process.stdout.write(JSON.stringify(value, null, 2) + '\\n'); };
if (args[0] === 'plugin' && args[1] === 'add' && args.includes('--help')) { process.stdout.write('usage\\n'); process.exit(0); }
if (args[0] === 'plugin' && args[1] === 'marketplace' && args[2] === 'add') { text({ marketplaceName: 'personal', installedRoot: args[3], alreadyAdded: false }); process.exit(0); }
if (args[0] === 'plugin' && args[1] === 'add') {
  const name = args[2]; const marketplace = args[args.indexOf('--marketplace') + 1];
  text({ pluginId: name + '@' + marketplace, name, marketplaceName: marketplace, version: '0.1.0',
    installedPath: path.join(process.env.CODEX_HOME || '', 'plugins', 'cache', marketplace, name, '0.1.0'), authPolicy: 'ON_INSTALL' });
  process.exit(0);
}
if (args[0] === 'plugin' && args[1] === 'remove') {
  const [name, marketplace] = String(args[2]).split('@');
  text({ pluginId: name + '@' + marketplace, name, marketplaceName: marketplace });
  process.exit(0);
}
process.stderr.write('unexpected codex invocation: ' + JSON.stringify(args) + '\\n');
process.exit(1);
`);
  fs.chmodSync(script, 0o755);
  return { dir: directory, log };
}

function writeDriver(directory) {
  const driver = path.join(directory, 'r2-driver.mjs');
  fs.writeFileSync(driver, [
    '#!/usr/bin/env node',
    'import fs from \'node:fs\';',
    'import path from \'node:path\';',
    'import { pathToFileURL } from \'node:url\';',
    'const pkgRoot = process.env.R2_PACKAGE_ROOT;',
    'const label = process.env.R2_LABEL;',
    'const args = process.env.R2_ARGS ? JSON.parse(process.env.R2_ARGS) : [];',
    'const claim = process.env.R2_CLAIM;',
    'const failActivation = process.env.R2_FAIL_ACTIVATION === \'1\';',
    '// Only the INSTALLED package\'s own modules run here; the repository copy stays out of the oracle.',
    'const { updateCommand } = await import(pathToFileURL(path.join(pkgRoot, \'cli\', \'commands\', \'update.mjs\')).href);',
    'const { productPaths } = await import(pathToFileURL(path.join(pkgRoot, \'cli\', \'paths.mjs\')).href);',
    'const callDaemon = async (_socket, command) => (command === \'activate-ready\'',
    '  ? { ready_for_activation: true, activation_claim: claim }',
    '  : { ready_for_activation: true });',
    'const activateService = async (_paths, candidate) => {',
    '  fs.appendFileSync(process.env.R2_SERVICE_LOG, JSON.stringify({ label, candidate }) + \'\\n\');',
    '  if (failActivation) throw new Error(\'SERVICE_BOOTSTRAP_INJECTED_FAILURE\');',
    '  return { pid: 4321, service_generation: 9 };',
    '};',
    'try {',
    '  const result = await updateCommand(productPaths(process.env.R2_HOME), args, { callDaemon, activateService });',
    '  process.stdout.write(JSON.stringify({ ok: true, result }) + \'\\n\');',
    '} catch (error) {',
    '  process.stdout.write(JSON.stringify({ ok: false, code: error.code ?? null, message: error.message }) + \'\\n\');',
    '  process.exitCode = 1;',
    '}',
    '',
  ].join('\n'));
  return driver;
}

// vB is a REAL second release: copy the shippable tree, bump the package, CLI,
// and daemon-crate versions, rebuild the native payload against the shared
// dependency cache, and let the release script restage the payload manifest.
function packVersionB(packDir) {
  const vbSrc = path.join(ctx.workDir, 'vb-src');
  fs.mkdirSync(vbSrc);
  for (const entry of ['bin', 'cli', 'crates', 'profiles', 'npm', 'plugins', 'schema', 'launchd', 'scripts']) {
    fs.cpSync(path.join(repoRoot, entry), path.join(vbSrc, entry), { recursive: true });
  }
  for (const file of ['package.json', 'Cargo.toml', 'Cargo.lock', 'README.md', 'LICENSE']) {
    fs.copyFileSync(path.join(repoRoot, file), path.join(vbSrc, file));
  }
  const pkg = JSON.parse(fs.readFileSync(path.join(vbSrc, 'package.json'), 'utf8'));
  pkg.version = VERSION_B;
  fs.writeFileSync(path.join(vbSrc, 'package.json'), `${JSON.stringify(pkg, null, 2)}\n`);
  fs.writeFileSync(path.join(vbSrc, 'cli', 'constants.mjs'),
    fs.readFileSync(path.join(vbSrc, 'cli', 'constants.mjs'), 'utf8').replace(/export const VERSION = '[^']+';/, `export const VERSION = '${VERSION_B}';`));
  fs.writeFileSync(path.join(vbSrc, 'crates', 'external-daemon', 'Cargo.toml'),
    fs.readFileSync(path.join(vbSrc, 'crates', 'external-daemon', 'Cargo.toml'), 'utf8').replace(/^version = "[^"]+"$/m, `version = "${VERSION_B}"`));

  const cargoEnv = { ...process.env, CARGO_TARGET_DIR: path.join(repoRoot, 'target'), CARGO_NET_OFFLINE: 'true' };
  const build = run('cargo', ['build', '--release', '-p', 'external-daemon', '-p', 'external-mcp'], { cwd: vbSrc, env: cargoEnv });
  assert.equal(build.status, 0, `vB cargo build failed: ${build.stderr}`);
  // The release script copies from <root>/target/release; stage the freshly
  // built binaries there so its own (cached) build is a no-op.
  const releaseDir = path.join(vbSrc, 'target', 'release');
  fs.mkdirSync(releaseDir, { recursive: true });
  for (const name of ['external-subagentd', 'external-subagent-mcp']) {
    fs.copyFileSync(path.join(repoRoot, 'target', 'release', name), path.join(releaseDir, name));
  }
  const stage = run(process.execPath, [path.join(vbSrc, 'scripts', 'release', 'build-native-payload.mjs')], { cwd: vbSrc, env: cargoEnv });
  assert.equal(stage.status, 0, `vB payload staging failed: ${stage.stderr}`);

  const manifest = JSON.parse(fs.readFileSync(path.join(vbSrc, PLATFORM_DIR, 'payload.json'), 'utf8'));
  assert.equal(manifest.version, VERSION_B, 'vB payload manifest must carry the bumped version');
  return manifest.files.find((file) => file.name === 'external-subagentd').sha256;
}

function npmPack(cwd) {
  const packDir = path.join(ctx.workDir, 'pack');
  fs.mkdirSync(packDir, { recursive: true });
  const packed = run('npm', ['pack', '--json', `--pack-destination=${packDir}`], {
    cwd,
    env: { ...process.env, HOME: ctx.workDir, npm_config_cache: path.join(ctx.workDir, 'npm-cache') },
  });
  assert.equal(packed.status, 0, `npm pack failed in ${cwd}: ${packed.stderr}`);
  return path.join(packDir, JSON.parse(packed.stdout)[0].filename);
}

let setupPromise = null;
const ensureR2 = () => (setupPromise ??= doSetup());

async function doSetup() {
  ctx.workDir = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-r2-'));
  ctx.home = path.join(ctx.workDir, 'home');
  fs.mkdirSync(ctx.home, { recursive: true });
  const shimDir = path.join(ctx.workDir, 'shim');
  fs.mkdirSync(shimDir, { recursive: true });
  ctx.fake = fakeCodexCli(shimDir);
  ctx.serviceLog = path.join(ctx.workDir, 'service-activations.jsonl');
  ctx.driver = writeDriver(ctx.workDir);

  const build = run(process.execPath, [path.join(repoRoot, 'scripts', 'release', 'build-native-payload.mjs'), '--if-stale'], { cwd: repoRoot });
  assert.equal(build.status, 0, `vA payload build failed: ${build.stderr}`);
  ctx.tgzA = npmPack(repoRoot);
  assert.equal(path.basename(ctx.tgzA), `external-subagent-${VERSION_A}.tgz`);
  ctx.shaB = packVersionB();
  ctx.tgzB = npmPack(path.join(ctx.workDir, 'vb-src'));
  assert.equal(path.basename(ctx.tgzB), `external-subagent-${VERSION_B}.tgz`);
  ctx.shaA = (() => {
    const manifest = JSON.parse(fs.readFileSync(path.join(repoRoot, PLATFORM_DIR, 'payload.json'), 'utf8'));
    return manifest.files.find((file) => file.name === 'external-subagentd').sha256;
  })();
  assert.notEqual(ctx.shaB, ctx.shaA, 'the two real tarballs must carry different daemon artifacts');

  ctx.prefix = path.join(ctx.workDir, 'prefix');
  ctx.packageRoot = path.join(ctx.prefix, 'lib', 'node_modules', 'external-subagent');
  ctx.cli = path.join(ctx.prefix, 'bin', 'external-subagent');
  const installHome = path.join(ctx.workDir, 'install-home');
  fs.mkdirSync(installHome, { recursive: true });
  const install = run('npm', ['install', '--global', `--prefix=${ctx.prefix}`, '--no-audit', '--no-fund', ctx.tgzA], {
    cwd: ctx.workDir,
    env: { ...process.env, HOME: installHome, npm_config_cache: path.join(ctx.workDir, 'npm-cache') },
  });
  assert.equal(install.status, 0, `npm install of vA failed: ${install.stderr}`);
  ctx.shaA = installedDaemonSha();

  const init = run(ctx.cli, ['init'], { env: childEnv() });
  assert.equal(init.status, 0, `init failed: ${init.stderr}`);
  ctx.initReport = JSON.parse(init.stdout);
  assert.equal(ctx.initReport.ok, true);
}

after(() => { if (ctx.workDir) fs.rmSync(ctx.workDir, { recursive: true, force: true }); });

const ensureVersionB = () => (ctx.vbPromise ??= doInstallVersionB());

async function doInstallVersionB() {
  const installHome = path.join(ctx.workDir, 'install-home');
  const install = run('npm', ['install', '--global', `--prefix=${ctx.prefix}`, '--no-audit', '--no-fund', ctx.tgzB], {
    cwd: ctx.workDir,
    env: { ...process.env, HOME: installHome, npm_config_cache: path.join(ctx.workDir, 'npm-cache') },
  });
  assert.equal(install.status, 0, `npm install of vB failed: ${install.stderr}`);
  ctx.shaB = installedDaemonSha();
}

test('vA tarball installs stage-only and an explicit update publishes the verified vA identity', { skip: !testable }, async () => {
  await ensureR2();
  assertInstalledPayload(VERSION_A);
  assert.equal(run(ctx.cli, ['version'], { env: childEnv() }).stdout.trim(), VERSION_A, 'the installed vA CLI reports its own version');

  const report = ctx.initReport;
  for (const step of ['verify-payload', 'install-launch-agent', 'install-codex-plugin', 'claim-codex-home']) {
    assert.ok(report.completed.includes(step), `init must complete ${step}`);
  }
  assert.equal(report.service.skipped, true, 'fixtures neutralize launchd through the documented seam');
  assert.equal(report.payload.status, 'verified');
  assert.equal(report.payload.version, VERSION_A);

  const state = readState();
  assert.equal(state.schema_version, 1, 'a plain install plus init stays stage-only');
  assert.equal(state.active, undefined);
  assert.equal(state.candidate, undefined);
  assert.equal(fs.existsSync(`${paths().state}.activation.json`), false, 'init never claims an activation receipt');
  assert.ok(fs.readFileSync(paths().launchAgent, 'utf8').includes(daemonEntry()), 'the LaunchAgent pins the installed daemon artifact path');
  const registry = JSON.parse(fs.readFileSync(path.join(paths().data, 'codex-homes.json'), 'utf8'));
  assert.equal(registry.homes.length, 1);
  assert.equal(registry.homes[0].version, VERSION_A);

  const va = runDriver('va');
  assert.equal(va.ok, true, `vA update failed: ${va.message}`);
  assert.equal(va.result.phase, 'active');
  assert.equal(va.result.active.version, VERSION_A);
  assert.equal(va.result.active.daemon_entry, daemonEntry(), 'active must publish the daemon payload artifact, not the npm bin shim');
  assert.equal(va.result.active.daemon_entry_sha256, ctx.shaA, 'the published daemon digest is the verified vA payload digest');
  assert.equal(va.result.active.entry, binEntry());
  const afterVa = readState();
  assert.equal(afterVa.candidate, null);
  assert.equal(afterVa.active.version, VERSION_A);
  assert.equal(afterVa.active.daemon_entry_sha256, ctx.shaA);
  assert.deepEqual(serviceActivations().at(-1).candidate, { path: daemonEntry(), sha256: ctx.shaA, version: VERSION_A },
    'service activation must receive the verified vA daemon identity');
  assert.equal(readReceipt().status, 'success');
});

test('installing the vB tarball into the same prefix and running the public update switches the verified active', { skip: !testable }, async () => {
  await ensureR2();
  await ensureVersionB();
  assertInstalledPayload(VERSION_B);
  assert.notEqual(ctx.shaB, ctx.shaA, 'the replaced payload must carry a different daemon artifact');
  assert.equal(run(ctx.cli, ['version'], { env: childEnv() }).stdout.trim(), VERSION_B, 'the installed vB CLI reports its own version');

  const before = readState();
  assert.equal(before.active.version, VERSION_A);
  const vb = runDriver('vb');
  assert.equal(vb.ok, true, `vB update failed: ${vb.message}`);
  assert.equal(vb.result.phase, 'active');
  assert.equal(vb.result.active.version, VERSION_B, 'the active version advances from vA to vB');
  assert.equal(vb.result.active.root, fs.realpathSync(ctx.packageRoot));
  assert.equal(vb.result.active.daemon_entry, daemonEntry());
  assert.equal(vb.result.active.daemon_entry_sha256, ctx.shaB, 'the active daemon digest is the verified vB payload digest');
  assert.notEqual(vb.result.active.daemon_entry_sha256, ctx.shaA);
  assert.equal(vb.result.active.entry, binEntry());

  const state = readState();
  assert.equal(state.candidate, null, 'a completed activation leaves no candidate behind');
  assert.equal(state.active.version, VERSION_B);
  assert.equal(state.active.daemon_entry_sha256, ctx.shaB);
  assert.notEqual(state.active.version, VERSION_A, 'the retired vA entry is no longer active');
  assert.notEqual(state.active.daemon_entry_sha256, ctx.shaA);
  assert.deepEqual(serviceActivations().at(-1).candidate, { path: daemonEntry(), sha256: ctx.shaB, version: VERSION_B },
    'service activation must receive the verified NEW daemon identity');
  assert.equal(vb.result.homes.all_updated, true);
  assert.equal(vb.result.homes.homes[0].status, 'updated');
  assert.deepEqual(vb.result.service, { pid: 4321, service_generation: 9 });
  assert.ok(fs.readFileSync(paths().launchAgent, 'utf8').includes(daemonEntry()), 'the LaunchAgent keeps pinning the installed daemon path');
  const receipt = readReceipt();
  assert.equal(receipt.status, 'success');
  assert.equal(receipt.claim, 'r2-vb');
  assert.equal(receipt.version, VERSION_B);
});

test('a failed service activation preserves the published vB active byte-for-byte', { skip: !testable }, async () => {
  await ensureR2();
  await ensureVersionB();
  const stateBytes = fs.readFileSync(paths().state);
  const failure = runDriver('vbfail', [], { failActivation: true });
  assert.equal(failure.ok, false);
  assert.match(failure.message, /SERVICE_BOOTSTRAP_INJECTED_FAILURE/);
  assert.deepEqual(fs.readFileSync(paths().state), stateBytes, 'the old active must survive an activation failure untouched');
  const preserved = readState();
  assert.equal(preserved.phase, 'active');
  assert.equal(preserved.active.version, VERSION_B);
  assert.equal(preserved.active.daemon_entry_sha256, ctx.shaB);
  assert.deepEqual(serviceActivations().at(-1).candidate, { path: daemonEntry(), sha256: ctx.shaB, version: VERSION_B },
    'the failed attempt still received the verified identity before failing');
  const receipt = readReceipt();
  assert.equal(receipt.status, 'failed');
  assert.equal(receipt.retryable, true);
  assert.equal(receipt.claim, 'r2-vbfail');
  assert.equal(receipt.rollback.restored, true);
});

test('the public reconcile command re-affirms the published active and rebinds homes', { skip: !testable }, async () => {
  await ensureR2();
  await ensureVersionB();
  const rec = runDriver('rec', ['reconcile']);
  assert.equal(rec.ok, true, `reconcile failed: ${rec.message}`);
  assert.equal(rec.result.phase, 'active');
  assert.equal(rec.result.active.version, VERSION_B);
  assert.equal(rec.result.active.daemon_entry, daemonEntry());
  assert.equal(rec.result.active.daemon_entry_sha256, ctx.shaB);
  assert.deepEqual(serviceActivations().at(-1).candidate, { path: daemonEntry(), sha256: ctx.shaB, version: VERSION_B },
    'reconcile activates from the published verified identity');
  assert.equal(rec.result.homes.all_updated, true);
  assert.equal(readReceipt().status, 'success');
});

test('the retired vA version is rejected without touching the published vB active', { skip: !testable }, async () => {
  await ensureR2();
  await ensureVersionB();
  const stateBytes = fs.readFileSync(paths().state);
  const old = runDriver('old', [`--version=${VERSION_A}`]);
  assert.equal(old.ok, false);
  assert.equal(old.code, 'PAYLOAD_VERSION_UNAVAILABLE', 'the replaced tarball version can never be re-activated');
  assert.deepEqual(fs.readFileSync(paths().state), stateBytes, 'a rejected version probe leaves the published active untouched');
  const state = readState();
  assert.equal(state.active.version, VERSION_B);
  assert.equal(sha256(fs.readFileSync(daemonEntry())), ctx.shaB, 'the active daemon artifact on disk still matches the verified digest');
  const receipt = readReceipt();
  assert.equal(receipt.status, 'failed');
  assert.equal(receipt.retryable, true);
});
