// R2 upgrade oracle: two REAL npm tarballs at different versions drive an
// isolated-prefix upgrade through the public update/reconcile commands.  vA is
// packed from this repository; vB is packed from a temporary copy whose
// package/CLI/daemon-crate versions are bumped and whose native payload is
// rebuilt, so the daemon artifact genuinely changes bytes.  vA installs into a
// throwaway npm prefix, an explicit `init` stages the service, claims the
// Codex home, and publishes the vA active/retention baseline itself (B-3:
// `npm A -> init A -> use A -> npm B` with no hidden extra A update).  Installing
// vB into the SAME prefix and running the public update must republish active
// with vB's version and verified daemon artifact, hand that verified identity
// to service activation, and leave the retired vA identity unavailable; a
// failed activation must preserve the published active byte-for-byte.
//
// The core defect this file pins: active.entry is the npm bin SHIM
// (bin/external-subagent.mjs) while the LaunchAgent runs the NATIVE daemon
// binary whose self-reported identity (current_exe + digest + version) is what
// service health verification checks.  The proof is dynamic, not mocked: a
// faithful launchctl seam (the only thing tests may never do is touch real
// launchd) loads the REAL plist — spawning the REAL daemon binary from the
// installed payload with the plist's own argv and environment — and the REAL
// activateService then runs its full unload/rewrite/bootstrap/health dance.
// Feeding it the shim identity from active.entry rewrites the service onto the
// shim, launches it, and can never health-verify (SERVICE_HEALTH_FAILED);
// feeding it the verified daemon artifact health-verifies against the daemon's
// own reported identity.  The public update command is exercised the same way
// with only the launchctl seam injected: its drain/claim prelude talks to the
// REAL running daemon over the REAL socket.
//
// All writes stay inside mkdtemp fixtures (a short /tmp base keeps the product
// socket path under macOS SUN_LEN, exactly like a short real HOME): npm
// prefix/cache and HOME are per-test temp directories, launchd stays
// neutralized for init through EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL, and no
// registry is ever published.  Real launchd bootstrap/bootout against the
// user's actual GUI session and installation into a real user HOME are NOT_RUN
// here and remain acceptance-gated outside this file.
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
  return { home: ctx.home, data, state: path.join(data, 'install-state.json'), socket: path.join(data, 'external-subagent.sock'),
    launchAgent: path.join(ctx.home, 'Library', 'LaunchAgents', 'com.external-subagent.daemon.plist') };
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
const launchctlState = () => {
  try { return JSON.parse(fs.readFileSync(path.join(ctx.workDir, 'launchctl-state.json'), 'utf8')); } catch { return { pid: null, program: null }; }
};
const launchctlLog = () => (fs.existsSync(path.join(ctx.workDir, 'launchctl-log.jsonl'))
  ? fs.readFileSync(path.join(ctx.workDir, 'launchctl-log.jsonl'), 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse) : []);
const plistProgram = () => {
  const text = fs.readFileSync(paths().launchAgent, 'utf8');
  const argv = [...text.match(/<key>ProgramArguments<\/key>\s*<array>([\s\S]*?)<\/array>/)[1].matchAll(/<string>([^<]*)<\/string>/g)]
    .map((match) => match[1].replaceAll('&lt;', '<').replaceAll('&gt;', '>').replaceAll('&amp;', '&'));
  return argv[0];
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
      R2_LAUNCHCTL_STATE: path.join(ctx.workDir, 'launchctl-state.json'),
      R2_LAUNCHCTL_LOG: path.join(ctx.workDir, 'launchctl-log.jsonl'),
      R2_CLAIM: `r2-${label}`,
      R2_LABEL: label,
      R2_ARGS: JSON.stringify(args),
      R2_MODE: options.mode || 'seam',
      R2_FAIL_ACTIVATION: options.failActivation ? '1' : '0',
      R2_HEALTH_TIMEOUT_MS: String(options.healthTimeoutMs ?? 20_000),
    }),
  });
  assert.equal(result.stderr, '', `driver ${label} stderr: ${result.stderr}`);
  assert.ok(result.stdout.trim().startsWith('{'), `driver ${label} printed no JSON: ${result.stdout.slice(0, 200)}`);
  return JSON.parse(result.stdout);
}

// The fake codex CLI also materializes the plugin cache (staged manifest +
// .mcp.json under plugins/cache/<marketplace>/<plugin>/<version> in
// CODEX_HOME) like the real CLI, so every init/update/reconcile in this file
// passes installPlugin's read-back cache verification.
function fakeCodexCli(directory) {
  const log = path.join(directory, 'codex-invocations.jsonl');
  const script = path.join(directory, 'codex');
  fs.writeFileSync(script, `#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const log = process.env.FAKE_CODEX_LOG || path.join(path.dirname(fileURLToPath(import.meta.url)), 'codex-invocations.jsonl');
const stateDir = path.dirname(log);
const rootsFile = path.join(stateDir, 'marketplace-roots.json');
const args = process.argv.slice(2);
fs.appendFileSync(log, JSON.stringify({ args, codex_home: process.env.CODEX_HOME }) + '\\n');
const text = (value) => { process.stdout.write(JSON.stringify(value, null, 2) + '\\n'); };
const loadRoots = () => { try { return JSON.parse(fs.readFileSync(rootsFile, 'utf8')); } catch { return {}; } };
if (args[0] === 'plugin' && args[1] === 'add' && args.includes('--help')) { process.stdout.write('usage\\n'); process.exit(0); }
if (args[0] === 'plugin' && args[1] === 'marketplace' && args[2] === 'add') {
  const roots = loadRoots();
  roots[process.env.CODEX_HOME] = args[3];
  fs.writeFileSync(rootsFile, JSON.stringify(roots));
  text({ marketplaceName: 'personal', installedRoot: args[3], alreadyAdded: false });
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
  text({ pluginId: name + '@' + marketplace, name, marketplaceName: marketplace, version,
    installedPath: cache, authPolicy: 'ON_INSTALL' });
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

// The driver always imports the INSTALLED package's own modules; the repository
// copy stays out of the oracle.  Modes:
//   seam            update/reconcile with controlled daemon RPC + service seams
//   service-up      load the real plist through the faithful launchctl seam
//   shim-activation REAL activateService fed the active.entry shim identity
//   real-update     public updateCommand with ONLY the launchctl seam injected
function writeDriver(directory) {
  const driver = path.join(directory, 'r2-driver.mjs');
  fs.writeFileSync(driver, [
    '#!/usr/bin/env node',
    'import fs from \'node:fs\';',
    'import path from \'node:path\';',
    'import { spawn } from \'node:child_process\';',
    'import { pathToFileURL } from \'node:url\';',
    'const pkgRoot = process.env.R2_PACKAGE_ROOT;',
    'const home = process.env.R2_HOME;',
    'const mode = process.env.R2_MODE || \'seam\';',
    'const args = process.env.R2_ARGS ? JSON.parse(process.env.R2_ARGS) : [];',
    'const claim = process.env.R2_CLAIM;',
    'const healthTimeoutMs = Number(process.env.R2_HEALTH_TIMEOUT_MS || 20000);',
    'const { updateCommand } = await import(pathToFileURL(path.join(pkgRoot, \'cli\', \'commands\', \'update.mjs\')).href);',
    'const { productPaths } = await import(pathToFileURL(path.join(pkgRoot, \'cli\', \'paths.mjs\')).href);',
    'const { callDaemon } = await import(pathToFileURL(path.join(pkgRoot, \'cli\', \'rpc.mjs\')).href);',
    'const { activateService: realActivateService } = await import(pathToFileURL(path.join(pkgRoot, \'cli\', \'install\', \'service-activation.mjs\')).href);',
    'const paths = productPaths(home);',
    'const emit = (doc) => { process.stdout.write(JSON.stringify(doc) + \'\\n\'); };',
    '',
    '// Faithful launchd stand-in: print/bootout/bootstrap against the REAL plist.',
    '// bootstrap spawns the plist\'s exact ProgramArguments (the real daemon',
    '// binary with its real --database/--socket/--runtime flags and plist env),',
    '// exactly what launchd does at load; nothing else is simulated.',
    'const unxml = (s) => s.replaceAll(\'&lt;\', \'<\').replaceAll(\'&gt;\', \'>\').replaceAll(\'&amp;\', \'&\');',
    'const stateFile = process.env.R2_LAUNCHCTL_STATE;',
    'const logFile = process.env.R2_LAUNCHCTL_LOG;',
    'const readState = () => { try { return JSON.parse(fs.readFileSync(stateFile, \'utf8\')); } catch { return { pid: null, program: null }; } };',
    'const alive = (pid) => { try { process.kill(pid, 0); return true; } catch { return false; } };',
    'const log = (entry) => fs.appendFileSync(logFile, JSON.stringify(entry) + \'\\n\');',
    'const launchctl = async (argv) => {',
    '  log({ action: argv[0], target: argv[1] });',
    '  if (argv[0] === \'print\') {',
    '    const state = readState();',
    '    if (!state.pid || !alive(state.pid)) return { action: \'print\', absent: true };',
    '    return { action: \'print\', status: 0, stdout: \'\\tpid = \' + state.pid + \'\\n\' };',
    '  }',
    '  if (argv[0] === \'bootout\') {',
    '    const state = readState();',
    '    if (state.pid && alive(state.pid)) {',
    '      process.kill(state.pid, \'SIGTERM\');',
    '      const deadline = Date.now() + 10000;',
    '      while (alive(state.pid) && Date.now() < deadline) await new Promise((resolve) => setTimeout(resolve, 50));',
    '    }',
    '    fs.writeFileSync(stateFile, JSON.stringify({ pid: null, program: null }));',
    '    return { action: \'bootout\', status: 0 };',
    '  }',
    '  if (argv[0] === \'bootstrap\') {',
    '    const text = fs.readFileSync(argv[2], \'utf8\');',
    '    const plistArgv = [...text.match(/<key>ProgramArguments<\\/key>\\s*<array>([\\s\\S]*?)<\\/array>/)[1].matchAll(/<string>([^<]*)<\\/string>/g)].map((m) => unxml(m[1]));',
    '    const plistEnv = {};',
    '    const dict = text.match(/<key>EnvironmentVariables<\\/key>\\s*<dict>([\\s\\S]*?)<\\/dict>/);',
    '    if (dict) for (const m of dict[1].matchAll(/<key>([^<]+)<\\/key>\\s*<string>([^<]*)<\\/string>/g)) plistEnv[unxml(m[1])] = unxml(m[2]);',
    '    try {',
    '      const child = spawn(plistArgv[0], plistArgv.slice(1), { stdio: \'ignore\', cwd: pkgRoot, env: { ...process.env, ...plistEnv } });',
    '      child.unref();',
    '      fs.writeFileSync(stateFile, JSON.stringify({ pid: child.pid, program: plistArgv[0] }));',
    '      log({ action: \'spawned\', program: plistArgv[0], pid: child.pid });',
    '      return { action: \'bootstrap\', status: 0 };',
    '    } catch (error) {',
    '      fs.writeFileSync(stateFile, JSON.stringify({ pid: null, program: plistArgv[0] }));',
    '      log({ action: \'spawn-error\', program: plistArgv[0], error: error.message });',
    '      return { action: \'bootstrap\', status: 0 };',
    '    }',
    '  }',
    '  throw new Error(\'unexpected launchctl args: \' + JSON.stringify(argv));',
    '};',
    '',
    'if (mode === \'service-up\') {',
    '  await launchctl([\'bootstrap\', \'gui/\' + process.getuid(), paths.launchAgent]);',
    '  const deadline = Date.now() + 20000;',
    '  let status = null;',
    '  while (!status && Date.now() < deadline) {',
    '    try { status = await callDaemon(paths.socket, \'status\', {}); }',
    '    catch { await new Promise((resolve) => setTimeout(resolve, 100)); }',
    '  }',
    '  if (!status) { emit({ ok: false, message: \'daemon never answered status\' }); process.exit(1); }',
    '  emit({ ok: true, service: { pid: readState().pid, version: status.identity?.daemon?.version, service_generation: status.service_generation, artifact: status.identity?.daemon?.artifact } });',
    '} else if (mode === \'shim-activation\') {',
    '  const state = JSON.parse(fs.readFileSync(paths.state, \'utf8\'));',
    '  const candidate = { path: state.active.entry, sha256: state.active.entry_sha256, version: state.active.version };',
    '  const drain = await callDaemon(paths.socket, \'drain\', {});',
    '  try {',
    '    const service = await realActivateService(paths, candidate, { launchctl, healthTimeoutMs });',
    '    emit({ ok: true, drain_ready: drain.ready_for_activation, candidate, service });',
    '  } catch (error) {',
    '    emit({ ok: false, drain_ready: drain.ready_for_activation, candidate, code: error.code ?? null, message: error.message, rollback: error.rollback ?? null, rollback_error: error.rollbackError ?? null });',
    '  }',
    '} else if (mode === \'real-update\') {',
    '  try {',
    '    const result = await updateCommand(paths, args, { launchctl, healthTimeoutMs });',
    '    emit({ ok: true, result });',
    '  } catch (error) {',
    '    emit({ ok: false, code: error.code ?? null, message: error.message });',
    '    process.exitCode = 1;',
    '  }',
    '} else {',
    '  const callDaemon = async (_socket, command) => (command === \'activate-ready\'',
    '    ? { ready_for_activation: true, activation_claim: claim }',
    '    : { ready_for_activation: true });',
    '  const activateService = async (_paths, candidate) => {',
    '    fs.appendFileSync(process.env.R2_SERVICE_LOG, JSON.stringify({ label: process.env.R2_LABEL, candidate }) + \'\\n\');',
    '    if (process.env.R2_FAIL_ACTIVATION === \'1\') throw new Error(\'SERVICE_BOOTSTRAP_INJECTED_FAILURE\');',
    '    return { pid: 4321, service_generation: 9 };',
    '  };',
    '  try {',
    '    const result = await updateCommand(paths, args, { callDaemon, activateService });',
    '    emit({ ok: true, result });',
    '  } catch (error) {',
    '    emit({ ok: false, code: error.code ?? null, message: error.message });',
    '    process.exitCode = 1;',
    '  }',
    '}',
    '',
  ].join('\n'));
  return driver;
}

// vB is a REAL second release: copy the shippable tree, bump the package, CLI,
// and daemon-crate versions, rebuild the native payload against the shared
// dependency cache, and let the release script restage the payload manifest.
function packVersionB() {
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
  // A short base keeps the product socket path below macOS SUN_LEN, exactly
  // like the short-HOME fixture the S05 live test uses.
  ctx.workDir = fs.mkdtempSync('/tmp/esr2-');
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
  const vbSha = packVersionB();
  ctx.tgzB = npmPack(path.join(ctx.workDir, 'vb-src'));
  assert.equal(path.basename(ctx.tgzB), `external-subagent-${VERSION_B}.tgz`);
  const repoSha = (() => {
    const manifest = JSON.parse(fs.readFileSync(path.join(repoRoot, PLATFORM_DIR, 'payload.json'), 'utf8'));
    return manifest.files.find((file) => file.name === 'external-subagentd').sha256;
  })();
  assert.notEqual(vbSha, repoSha, 'the two real tarballs must carry different daemon artifacts');

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

after(() => {
  if (!ctx.workDir) return;
  const state = launchctlState();
  if (state.pid) {
    try { process.kill(state.pid, 'SIGTERM'); } catch { /* already gone */ }
  }
  fs.rmSync(ctx.workDir, { recursive: true, force: true });
});

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

test('vA tarball installs stage-only; init publishes the verified vA active/retention baseline', { skip: !testable }, async () => {
  await ensureR2();
  assertInstalledPayload(VERSION_A);
  assert.equal(run(ctx.cli, ['version'], { env: childEnv() }).stdout.trim(), VERSION_A, 'the installed vA CLI reports its own version');

  const report = ctx.initReport;
  for (const step of ['verify-payload', 'install-launch-agent', 'install-codex-plugin', 'claim-codex-home', 'publish-active-payload']) {
    assert.ok(report.completed.includes(step), `init must complete ${step}`);
  }
  assert.equal(report.service.skipped, true, 'fixtures neutralize launchd through the documented seam');
  assert.equal(report.payload.status, 'verified');
  assert.equal(report.payload.version, VERSION_A);
  assert.equal(report.baseline.version, VERSION_A);
  assert.equal(report.baseline.daemon_entry_sha256, ctx.shaA, 'the report carries the verified baseline identity');

  // B-3: the baseline comes from init alone.  The standard public sequence is
  // `npm A -> init A -> use A -> npm B`; no extra "A update" that a real user
  // never runs may be a prerequisite for the first upgrade.
  const state = readState();
  assert.equal(state.schema_version, 2, 'a successful verified init publishes the activation state itself');
  assert.equal(state.candidate, null);
  assert.equal(state.active.version, VERSION_A);
  assert.equal(state.active.daemon_entry, daemonEntry(), 'active keeps the verified daemon artifact identity');
  assert.equal(state.active.retained.daemon_entry, path.join(paths().data, 'payload-store', VERSION_A, 'external-subagentd'));
  assert.equal(state.active.retained.daemon_entry_sha256, state.active.daemon_entry_sha256);
  assert.equal(state.active.daemon_entry_sha256, ctx.shaA, 'the published daemon digest is the verified vA payload digest');
  assert.equal(state.active.entry, binEntry());
  const retainedA = path.join(paths().data, 'payload-store', VERSION_A, 'external-subagentd');
  assert.ok(fs.existsSync(retainedA), 'init retains the verified vA daemon bytes outside the npm-replaced tree');
  assert.equal(sha256(fs.readFileSync(retainedA)), ctx.shaA, 'the retained bytes match the verified vA digest');
  assert.equal(fs.existsSync(`${paths().state}.activation.json`), false, 'init never claims an activation receipt');
  assert.ok(fs.readFileSync(paths().launchAgent, 'utf8').includes(daemonEntry()), 'the LaunchAgent pins the installed daemon artifact path');
  const registry = JSON.parse(fs.readFileSync(path.join(paths().data, 'codex-homes.json'), 'utf8'));
  assert.equal(registry.homes.length, 1);
  assert.equal(registry.homes[0].version, VERSION_A);

  // An explicit update on the SAME version is a reaffirm through the public
  // command, never a hidden prerequisite of the baseline.
  const va = runDriver('va');
  assert.equal(va.ok, true, `vA reaffirm failed: ${va.message}`);
  assert.equal(va.result.phase, 'active');
  assert.equal(va.result.active.version, VERSION_A);
  assert.equal(va.result.active.retained.daemon_entry, path.join(paths().data, 'payload-store', VERSION_A, 'external-subagentd'));
  assert.equal(va.result.active.daemon_entry_sha256, ctx.shaA);
  const afterVa = readState();
  assert.equal(afterVa.candidate, null);
  assert.equal(afterVa.active.version, VERSION_A);
  assert.equal(afterVa.active.daemon_entry_sha256, ctx.shaA);
  assert.deepEqual(serviceActivations().at(-1).candidate, { path: path.join(paths().data, 'payload-store', VERSION_A, 'external-subagentd'), sha256: ctx.shaA, version: VERSION_A },
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
  assert.equal(vb.result.active.retained.daemon_entry, path.join(paths().data, 'payload-store', VERSION_B, 'external-subagentd'));
  assert.equal(vb.result.active.retained.daemon_entry_sha256, vb.result.active.daemon_entry_sha256);
  assert.equal(vb.result.active.daemon_entry_sha256, ctx.shaB, 'the active daemon digest is the verified vB payload digest');
  assert.notEqual(vb.result.active.daemon_entry_sha256, ctx.shaA);
  assert.equal(vb.result.active.entry, binEntry());

  const state = readState();
  assert.equal(state.candidate, null, 'a completed activation leaves no candidate behind');
  assert.equal(state.active.version, VERSION_B);
  assert.equal(state.active.daemon_entry_sha256, ctx.shaB);
  assert.notEqual(state.active.version, VERSION_A, 'the retired vA entry is no longer active');
  assert.notEqual(state.active.daemon_entry_sha256, ctx.shaA);
  assert.deepEqual(serviceActivations().at(-1).candidate, { path: path.join(paths().data, 'payload-store', VERSION_B, 'external-subagentd'), sha256: ctx.shaB, version: VERSION_B },
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
  assert.deepEqual(serviceActivations().at(-1).candidate, { path: path.join(paths().data, 'payload-store', VERSION_B, 'external-subagentd'), sha256: ctx.shaB, version: VERSION_B },
    'the failed attempt still received the verified identity before failing');
  const receipt = readReceipt();
  assert.equal(receipt.status, 'failed');
  assert.equal(receipt.retryable, true);
  assert.equal(receipt.claim, 'r2-vbfail');
  assert.equal(receipt.rollback.restored, true);
});

test('a partial Codex-home sync keeps the verified activation and reports per-home statuses', { skip: !testable }, async () => {
  await ensureR2();
  await ensureVersionB();

  // A second registered home the sync cannot write: reconcile must skip it
  // per home, and the public update must surface that partial outcome without
  // rolling the completed service activation back (S02's diagnostic limit).
  const lockedHome = path.join(ctx.workDir, 'codex-locked');
  fs.mkdirSync(lockedHome, { recursive: true });
  fs.chmodSync(lockedHome, 0o500);
  const registryFile = path.join(paths().data, 'codex-homes.json');
  const registry = JSON.parse(fs.readFileSync(registryFile, 'utf8'));
  registry.homes.push({ home: fs.realpathSync(lockedHome), claimed_at_ms: Date.now(), version: VERSION_A, digest: null, last_sync_ms: null, last_status: 'claimed' });
  fs.writeFileSync(registryFile, `${JSON.stringify(registry, null, 2)}\n`);
  const activationsBefore = serviceActivations().length;

  const partial = runDriver('vbpartial');
  assert.equal(partial.ok, false, 'a partial home sync must not be reported as success');
  assert.equal(partial.code, 'CODEX_SYNC_PARTIAL');
  assert.match(partial.message, /skipped_not_writable/);
  assert.match(partial.message, new RegExp(fs.realpathSync(lockedHome)));
  assert.match(partial.message, /external-subagent reconcile/);

  // The verified vB activation survives: active stays published, the service
  // was switched (and not rolled back), and the registry keeps the per-home
  // statuses the sync recorded instead of restoring the pre-update bytes.
  const state = readState();
  assert.equal(state.phase, 'active');
  assert.equal(state.active.version, VERSION_B);
  assert.equal(state.active.daemon_entry_sha256, ctx.shaB);
  assert.equal(serviceActivations().length, activationsBefore + 1, 'service activation must not be rolled back');
  const byHome = Object.fromEntries(JSON.parse(fs.readFileSync(registryFile, 'utf8')).homes.map((entry) => [path.basename(entry.home), entry.last_status]));
  assert.equal(byHome['.codex'], 'updated');
  assert.equal(byHome['codex-locked'], 'skipped_not_writable');
  const receipt = readReceipt();
  assert.equal(receipt.status, 'partial');
  assert.equal(receipt.retryable, true);
  assert.equal(receipt.homes.all_updated, false);
  assert.deepEqual(receipt.homes.homes.map((home) => home.status).sort(), ['skipped_not_writable', 'updated']);

  // Once the blocked home is writable again, the public reconcile finishes
  // the remaining homes idempotently — no re-activation ritual required.
  fs.chmodSync(lockedHome, 0o700);
  const rec = runDriver('vbpartial-rec', ['reconcile']);
  assert.equal(rec.ok, true, `follow-up reconcile failed: ${rec.message}`);
  assert.equal(rec.result.homes.all_updated, true);
  assert.deepEqual(rec.result.homes.homes.map((home) => home.status), ['updated', 'updated']);
});

test('the public reconcile command re-affirms the published active and rebinds homes', { skip: !testable }, async () => {
  await ensureR2();
  await ensureVersionB();
  const rec = runDriver('rec', ['reconcile']);
  assert.equal(rec.ok, true, `reconcile failed: ${rec.message}`);
  assert.equal(rec.result.phase, 'active');
  assert.equal(rec.result.active.version, VERSION_B);
  assert.equal(rec.result.active.retained.daemon_entry, path.join(paths().data, 'payload-store', VERSION_B, 'external-subagentd'));
  assert.equal(rec.result.active.daemon_entry_sha256, ctx.shaB);
  assert.deepEqual(serviceActivations().at(-1).candidate, { path: path.join(paths().data, 'payload-store', VERSION_B, 'external-subagentd'), sha256: ctx.shaB, version: VERSION_B },
    'reconcile activates from the published verified identity');
  assert.equal(rec.result.homes.all_updated, true);
  assert.equal(readReceipt().status, 'success');
});

test('the shim identity from active.entry cannot health-verify the real service the LaunchAgent pins', { skip: !testable }, async () => {
  await ensureR2();
  await ensureVersionB();

  // Load the real service exactly like launchd would: the real plist spawns
  // the real daemon binary, which answers status with its own identity.
  const up = runDriver('service-up', [], { mode: 'service-up' });
  assert.equal(up.ok, true, `service-up failed: ${up.message}`);
  assert.equal(up.service.artifact.path, daemonEntry(), 'the running service is the native daemon the plist pins');
  assert.equal(up.service.artifact.sha256, ctx.shaB);
  assert.equal(up.service.version, VERSION_B);
  const daemonPid = up.service.pid;

  // THE PROOF: the REAL activateService, fed exactly the pre-fix identity
  // (active.entry + entry_sha256), rewrites the service onto the npm shim,
  // launches it, and can never become healthy — the service needs the native
  // daemon artifact identity, not the bin shim.
  const spawnCountBefore = launchctlLog().filter((entry) => entry.action === 'spawned').length;
  const shim = runDriver('shim', [], { mode: 'shim-activation', healthTimeoutMs: 4_000 });
  assert.equal(shim.ok, false, 'activating the shim identity must fail');
  assert.equal(shim.drain_ready, true);
  assert.equal(shim.candidate.path, binEntry(), 'the candidate is the npm bin shim published as active.entry');
  assert.equal(shim.code, 'SERVICE_HEALTH_FAILED', 'the real health verification rejects the shim identity');
  const spawned = launchctlLog().filter((entry) => entry.action === 'spawned').slice(spawnCountBefore);
  assert.equal(spawned[0].program, binEntry(), 'activation actually rewrote the service onto the shim and launched it');
  assert.equal(spawned.at(-1).program, daemonEntry(), 'rollback restored and relaunched the native daemon');
  assert.ok(shim.rollback, 'the failed activation leaves rollback evidence');
  assert.equal(plistProgram(), daemonEntry(), 'the service definition is restored to the daemon artifact');
  const state = readState();
  assert.equal(state.active.version, VERSION_B, 'a direct service failure leaves the published active untouched');
  assert.notEqual(launchctlState().pid, daemonPid, 'the rollback produced a fresh daemon process');
  assert.ok(launchctlState().pid, 'a daemon is running again after rollback');
});

test('the public update drives the real activation and health-verifies the verified daemon artifact', { skip: !testable }, async () => {
  await ensureR2();
  await ensureVersionB();
  const pidBefore = launchctlState().pid;
  assert.ok(pidBefore, 'the service from the previous leg is still the running daemon');
  const spawnCountBefore = launchctlLog().filter((entry) => entry.action === 'spawned').length;

  // Only the launchctl seam is injected: drain, claim, updateInstallation,
  // activateService, health verification, and the real daemon RPC on the real
  // socket all run for real from the installed package.
  const real = runDriver('real', [], { mode: 'real-update' });
  assert.equal(real.ok, true, `real update failed: ${real.code} ${real.message}`);
  assert.equal(real.result.phase, 'active');
  assert.equal(real.result.active.version, VERSION_B);
  assert.equal(real.result.active.retained.daemon_entry, path.join(paths().data, 'payload-store', VERSION_B, 'external-subagentd'));
  assert.equal(real.result.active.daemon_entry_sha256, ctx.shaB);

  const service = real.result.service;
  assert.ok(Number.isInteger(service.pid) && service.pid > 0, 'health verification returned a real daemon pid');
  assert.notEqual(service.pid, pidBefore, 'the daemon was replaced, not reused');
  assert.equal(service.version, VERSION_B, 'the running daemon self-reports the payload version');
  assert.equal(service.artifact.path, real.result.active.retained.daemon_entry, 'the daemon self-reports the verified daemon artifact path');
  assert.equal(service.artifact.sha256, ctx.shaB, 'the daemon self-reports the verified payload digest');
  assert.ok(service.service_generation, 'health verification captured the new service generation');

  const spawned = launchctlLog().filter((entry) => entry.action === 'spawned').slice(spawnCountBefore);
  assert.deepEqual(spawned.map((entry) => entry.program), [real.result.active.retained.daemon_entry], 'the public update relaunched exactly the native daemon, never the shim');
  assert.equal(plistProgram(), real.result.active.retained.daemon_entry, 'the LaunchAgent keeps pinning the daemon artifact');
  const receipt = readReceipt();
  assert.equal(receipt.status, 'success');
  assert.match(receipt.claim, /^agentd-\d+-activation$/, 'the receipt carries the real daemon-issued activation claim');
  const registry = JSON.parse(fs.readFileSync(path.join(paths().data, 'codex-homes.json'), 'utf8'));
  assert.equal(registry.homes[0].last_status, 'updated', 'the real update re-bound the claimed codex home');
});

test('the retired vA version is rejected without touching the published vB active', { skip: !testable }, async () => {
  await ensureR2();
  await ensureVersionB();
  const stateBytes = fs.readFileSync(paths().state);
  const receiptBytes = fs.readFileSync(`${paths().state}.activation.json`);
  const old = runDriver('old', [`--version=${VERSION_A}`]);
  assert.equal(old.ok, false);
  assert.equal(old.code, 'PAYLOAD_VERSION_UNAVAILABLE', 'the replaced tarball version can never be re-activated');
  assert.deepEqual(fs.readFileSync(paths().state), stateBytes, 'a rejected version probe leaves the published active untouched');
  // The rejection happens in the coordinator's pre-drain preflight, so the
  // running daemon was never drained and no claim was ever issued: the
  // previous activation receipt must survive byte-for-byte as the retry
  // idempotency evidence.
  assert.deepEqual(fs.readFileSync(`${paths().state}.activation.json`), receiptBytes, 'a pre-drain rejection writes no receipt');
  const state = readState();
  assert.equal(state.active.version, VERSION_B);
  assert.equal(sha256(fs.readFileSync(daemonEntry())), ctx.shaB, 'the active daemon artifact on disk still matches the verified digest');
  const receipt = readReceipt();
  assert.equal(receipt.status, 'success');
  assert.match(receipt.claim, /^agentd-\d+-activation$/);
});
