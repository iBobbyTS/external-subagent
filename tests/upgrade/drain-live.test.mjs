// R2 live active-task vA→vB oracle: the drain half the controlled suites
// cannot prove.
//
// Two REAL npm tarballs (vA packed from this repository, vB packed from a
// version-bumped copy with a genuinely rebuilt native daemon) drive ONE
// installed prefix through the real upgrade with a REAL task in flight:
//
//   1. vA installs stage-only (--ignore-scripts: no lifecycle hook fires);
//      an explicit init publishes the vA active/retention baseline itself
//      (B-3: no hidden extra "A update"); the faithful launchctl seam loads
//      the REAL plist — spawning the REAL vA daemon binary with the plist's
//      own argv and environment.
//   2. Two REAL upstream tasks run on the vA daemon: a ZCode build task that
//      must stop on a pending permission request, and a long DSH task.
//   3. The vB tarball is npm-installed into the SAME prefix while the vA
//      daemon keeps running its tasks.
//   4. The PUBLIC update command (only the launchctl seam injected — drain,
//      claim, updater, service activation and health verification all run
//      for real from the installed package) performs the default safe
//      drain/coordinate: during drain the old task still answers
//      wait/respond/cancel/result/close, new spawns are rejected with
//      daemon_draining, and once the tasks are reaped the SAME update run
//      activates vB automatically — no second manual update.
//   5. The activated service is proven by process identity: a fresh PID, a
//      new service_generation, the vB version, and the verified native
//      daemon artifact path/digest; the retained vA payload bytes survive
//      the npm replacement.
//   6. After the upgrade the NEW daemon still serves both upstreams.
//
// Provider authentication is bridged read-only exactly like the accepted
// dual-provider harness: isolated provider homes whose credential files are
// symlinks to the real user configuration, digest-checked unchanged in the
// guaranteed finally. No secret is copied, printed, or read into the test;
// business workspaces, the store, and sockets live under a fresh temp root;
// real launchd is never touched (the faithful seam is the only service
// control); no registry is published. The test self-skips with an explicit
// reason when the real upstreams are not installed on this machine.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawn, spawnSync } from 'node:child_process';
import { callDaemon } from '../../cli/rpc.mjs';
import { readConfig } from '../../cli/config/read.mjs';
import { writeConfig } from '../../cli/config/write.mjs';
import { productPaths } from '../../cli/paths.mjs';

const repoRoot = path.resolve(import.meta.dirname, '../..');
const testable = process.platform === 'darwin' && process.arch === 'arm64';
const VERSION_A = JSON.parse(fs.readFileSync(path.join(repoRoot, 'package.json'), 'utf8')).version;
const VERSION_B = (() => { const [major, minor, patch] = VERSION_A.split('.').map(Number); return `${major}.${minor}.${patch + 1}`; })();
const PLATFORM_DIR = path.join('npm', 'native', 'darwin-arm64');
const DSH_RUNTIME = process.env.EXTERNAL_SUBAGENT_LIVE_DSH_RUNTIME || '/opt/homebrew/bin/dsh';
const ZCODE_RUNTIME = process.env.EXTERNAL_SUBAGENT_LIVE_ZCODE_RUNTIME || '/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs';
const DSH_VERSION = process.env.EXTERNAL_SUBAGENT_LIVE_DSH_VERSION || '0.1.5-rc.1';
const REAL_DSH_HOME = process.env.EXTERNAL_SUBAGENT_LIVE_DSH_BRIDGE || path.join(os.homedir(), '.dsh');
const REAL_ZCODE_CONFIG = process.env.EXTERNAL_SUBAGENT_LIVE_ZCODE_CONFIG_BRIDGE || path.join(os.homedir(), '.zcode', 'cli', 'config.json');

const liveUpstream = testable
  && fs.existsSync(DSH_RUNTIME) && fs.existsSync(path.join(REAL_DSH_HOME, '.credentials.yaml'))
  && fs.existsSync(ZCODE_RUNTIME) && fs.existsSync(REAL_ZCODE_CONFIG);

const skipReason = !testable ? 'darwin-arm64 only'
  : !liveUpstream ? 'real DSH/ZCode upstreams are not installed on this machine (DSH runtime+credentials, ZCode runtime+config)' : null;

const sha256 = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');
const run = (command, args, options = {}) => spawnSync(command, args, { encoding: 'utf8', ...options });

// lstat-based recursive digest so a dangling symlink inside the bridged
// profiles tree is recorded (and drift-checked) instead of crashing the
// baseline like statSync would.
function treeDigest(dir) {
  const lines = [];
  const walk = (current, prefix) => {
    for (const entry of fs.readdirSync(current, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      const child = path.join(current, entry.name);
      const rel = prefix ? `${prefix}/${entry.name}` : entry.name;
      if (entry.isDirectory()) walk(child, rel);
      else if (entry.isFile()) lines.push(`${rel} ${sha256(fs.readFileSync(child))}`);
      else lines.push(`${rel} link:${fs.readlinkSync(child)}`);
    }
  };
  walk(dir, '');
  return `tree:${sha256(Buffer.from(lines.join('\n')))}`;
}
const bridgeDigest = (target) => (fs.statSync(target).isDirectory() ? treeDigest(target) : sha256(fs.readFileSync(target)));

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

// The driver always imports the INSTALLED package's own modules.  Modes:
//   seam        controlled daemon RPC + service seams (vA identity publish)
//   service-up  re-render the plist from the live config, load it through the
//               faithful launchctl seam, wait for the real daemon's status
//   real-update public updateCommand with ONLY the launchctl seam injected
function writeDriver(directory) {
  const driver = path.join(directory, 'r2-driver.mjs');
  const lines = [
    '#!/usr/bin/env node',
    "import fs from 'node:fs';",
    "import path from 'node:path';",
    "import { spawn } from 'node:child_process';",
    "import { pathToFileURL } from 'node:url';",
    'const pkgRoot = process.env.R2_PACKAGE_ROOT;',
    'const home = process.env.R2_HOME;',
    "const mode = process.env.R2_MODE || 'seam';",
    "const args = process.env.R2_ARGS ? JSON.parse(process.env.R2_ARGS) : [];",
    'const claim = process.env.R2_CLAIM;',
    'const healthTimeoutMs = Number(process.env.R2_HEALTH_TIMEOUT_MS || 20000);',
    "const { updateCommand } = await import(pathToFileURL(path.join(pkgRoot, 'cli', 'commands', 'update.mjs')).href);",
    "const { productPaths } = await import(pathToFileURL(path.join(pkgRoot, 'cli', 'paths.mjs')).href);",
    "const { callDaemon } = await import(pathToFileURL(path.join(pkgRoot, 'cli', 'rpc.mjs')).href);",
    "const { installLaunchAgent } = await import(pathToFileURL(path.join(pkgRoot, 'cli', 'install', 'service-macos.mjs')).href);",
    'const paths = productPaths(home);',
    "const emit = (doc) => { process.stdout.write(JSON.stringify(doc) + '\\n'); };",
    '',
    '// Faithful launchd stand-in: print/bootout/bootstrap against the REAL plist.',
    "// bootstrap spawns the plist's exact ProgramArguments (the real daemon",
    '// binary with its real --database/--socket/--runtime flags and plist env),',
    '// exactly what launchd does at load; nothing else is simulated.',
    "const unxml = (s) => s.replaceAll('&lt;', '<').replaceAll('&gt;', '>').replaceAll('&amp;', '&');",
    'const stateFile = process.env.R2_LAUNCHCTL_STATE;',
    'const readState = () => { try { return JSON.parse(fs.readFileSync(stateFile, \'utf8\')); } catch { return { pid: null, program: null }; } };',
    'const alive = (pid) => { try { process.kill(pid, 0); return true; } catch { return false; } };',
    'const launchctl = async (argv) => {',
    "  if (argv[0] === 'print') {",
    '    const state = readState();',
    "    if (!state.pid || !alive(state.pid)) return { action: 'print', absent: true };",
    "    return { action: 'print', status: 0, stdout: '\\tpid = ' + state.pid + '\\n' };",
    '  }',
    "  if (argv[0] === 'bootout') {",
    '    const state = readState();',
    '    if (state.pid && alive(state.pid)) {',
    "      process.kill(state.pid, 'SIGTERM');",
    '      const deadline = Date.now() + 10000;',
    '      while (alive(state.pid) && Date.now() < deadline) await new Promise((resolve) => setTimeout(resolve, 50));',
    '    }',
    '    fs.writeFileSync(stateFile, JSON.stringify({ pid: null, program: null }));',
    "    return { action: 'bootout', status: 0 };",
    '  }',
    "  if (argv[0] === 'bootstrap') {",
    '    const text = fs.readFileSync(argv[2], \'utf8\');',
    '    const plistArgv = [...text.match(/<key>ProgramArguments<\\/key>\\s*<array>([\\s\\S]*?)<\\/array>/)[1].matchAll(/<string>([^<]*)<\\/string>/g)].map((m) => unxml(m[1]));',
    '    const plistEnv = {};',
    '    const dict = text.match(/<key>EnvironmentVariables<\\/key>\\s*<dict>([\\s\\S]*?)<\\/dict>/);',
    '    if (dict) for (const m of dict[1].matchAll(/<key>([^<]+)<\\/key>\\s*<string>([^<]*)<\\/string>/g)) plistEnv[unxml(m[1])] = unxml(m[2]);',
    '    const child = spawn(plistArgv[0], plistArgv.slice(1), { stdio: \'ignore\', cwd: pkgRoot, env: { ...process.env, ...plistEnv } });',
    '    child.unref();',
    '    fs.writeFileSync(stateFile, JSON.stringify({ pid: child.pid, program: plistArgv[0] }));',
    "    return { action: 'bootstrap', status: 0 };",
    '  }',
    "  throw new Error('unexpected launchctl args: ' + JSON.stringify(argv));",
    '};',
    '',
    "if (mode === 'service-up') {",
    '  installLaunchAgent(paths);',
    "  await launchctl(['bootstrap', 'gui/' + process.getuid(), paths.launchAgent]);",
    '  const deadline = Date.now() + 20000;',
    '  let status = null;',
    '  while (!status && Date.now() < deadline) {',
    "    try { status = await callDaemon(paths.socket, 'status', {}); }",
    '    catch { await new Promise((resolve) => setTimeout(resolve, 100)); }',
    '  }',
    "  if (!status) { emit({ ok: false, message: 'daemon never answered status' }); process.exit(1); }",
    '  emit({ ok: true, service: { pid: readState().pid, version: status.identity?.daemon?.version, service_generation: status.service_generation, artifact: status.identity?.daemon?.artifact } });',
    "} else if (mode === 'real-update') {",
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
    "    fs.appendFileSync(process.env.R2_SERVICE_LOG, JSON.stringify({ label: process.env.R2_LABEL, candidate }) + '\\n');",
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
  ];
  fs.writeFileSync(driver, lines.join('\n'));
  return driver;
}

// vB is a REAL second release: copy the shippable tree, bump the package, CLI,
// and daemon-crate versions, rebuild the native payload against the shared
// dependency cache, and let the release script restage the payload manifest.
function packVersionB(workDir) {
  const vbSrc = path.join(workDir, 'vb-src');
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
  const build = run('cargo', ['build', '--release', '-p', 'external-daemon', '-p', 'external-mcp'], { cwd: vbSrc, env: cargoEnv, timeout: 600_000 });
  assert.equal(build.status, 0, `vB cargo build failed: ${build.stderr}`);
  const releaseDir = path.join(vbSrc, 'target', 'release');
  fs.mkdirSync(releaseDir, { recursive: true });
  for (const name of ['external-subagentd', 'external-subagent-mcp']) {
    fs.copyFileSync(path.join(repoRoot, 'target', 'release', name), path.join(releaseDir, name));
  }
  const stage = run(process.execPath, [path.join(vbSrc, 'scripts', 'release', 'build-native-payload.mjs')], { cwd: vbSrc, env: cargoEnv, timeout: 120_000 });
  assert.equal(stage.status, 0, `vB payload staging failed: ${stage.stderr}`);
  const manifest = JSON.parse(fs.readFileSync(path.join(vbSrc, PLATFORM_DIR, 'payload.json'), 'utf8'));
  assert.equal(manifest.version, VERSION_B, 'vB payload manifest must carry the bumped version');
  return manifest.files.find((file) => file.name === 'external-subagentd').sha256;
}

function npmPack(workDir, cwd) {
  const packDir = path.join(workDir, 'pack');
  fs.mkdirSync(packDir, { recursive: true });
  const packed = run('npm', ['pack', '--json', `--pack-destination=${packDir}`], {
    cwd,
    timeout: 120_000,
    env: { ...process.env, HOME: workDir, npm_config_cache: path.join(workDir, 'npm-cache') },
  });
  assert.equal(packed.status, 0, `npm pack failed in ${cwd}: ${packed.stderr}`);
  return path.join(packDir, JSON.parse(packed.stdout)[0].filename);
}

// node:test treats `skip: null` as skip-but-still-run; only pass the option
// when there is an actual reason.
const testOptions = skipReason ? { skip: skipReason } : {};

test('live vA→vB upgrade drains a real active task and activates vB automatically', testOptions, async (t) => {
  const workDir = fs.mkdtempSync('/tmp/esdl-');
  const home = path.join(workDir, 'home');
  const shimDir = path.join(workDir, 'shim');
  const dshHome = path.join(workDir, 'dshhome');
  const wsZcode = path.join(workDir, 'ws-zcode');
  const wsDsh = path.join(workDir, 'ws-dsh');
  for (const dir of [home, shimDir, dshHome, wsZcode, wsDsh, path.join(workDir, 'ws-post-z'), path.join(workDir, 'ws-post-d'),
    path.join(home, '.zcode', 'cli'), path.join(home, '.codex'), path.join(workDir, 'install-home')]) {
    fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  }
  const serviceLog = path.join(workDir, 'service-activations.jsonl');
  const driver = writeDriver(workDir);
  const fake = fakeCodexCli(shimDir);
  let daemonPid = null;
  let settled = false;
  let bridged = [];
  let before = new Map();
  let updateChild = null;
  try {
    // Read-only auth bridges into the real provider configuration; every
    // bridged target (including the profiles tree, recursively) is
    // digest-checked unchanged in the finally below.
    fs.symlinkSync(REAL_ZCODE_CONFIG, path.join(home, '.zcode', 'cli', 'config.json'));
    for (const name of ['.credentials.yaml', 'settings.yaml', '.anonymous-user-id', 'profiles']) {
      fs.symlinkSync(path.join(REAL_DSH_HOME, name), path.join(dshHome, name));
    }
    bridged = [
      ['zcode cli config.json', REAL_ZCODE_CONFIG],
      ['dsh .credentials.yaml', path.join(REAL_DSH_HOME, '.credentials.yaml')],
      ['dsh settings.yaml', path.join(REAL_DSH_HOME, 'settings.yaml')],
      ['dsh .anonymous-user-id', path.join(REAL_DSH_HOME, '.anonymous-user-id')],
      ['dsh profiles/ (recursive tree)', path.join(REAL_DSH_HOME, 'profiles')],
    ];
    for (const [label, target] of bridged) assert.ok(fs.existsSync(target), `live bridge target missing: ${label}`);
    before = new Map(bridged.map(([label, target]) => [label, bridgeDigest(target)]));

    const childEnv = (extra = {}) => {
      const env = {
        ...process.env,
        HOME: home,
        CODEX_HOME: path.join(home, '.codex'),
        EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL: '1',
        PATH: `${fake.dir}:${process.env.PATH || ''}`,
        FAKE_CODEX_LOG: fake.log,
        ...extra,
      };
      delete env.ZCODE_AGENTD_SOCKET;
      return env;
    };
    const paths = () => productPaths(home);
    const readState = () => JSON.parse(fs.readFileSync(paths().state, 'utf8'));
    const readReceipt = () => JSON.parse(fs.readFileSync(`${paths().state}.activation.json`, 'utf8'));
    const packageRoot = () => path.join(workDir, 'prefix', 'lib', 'node_modules', 'external-subagent');
    const cliBin = () => path.join(workDir, 'prefix', 'bin', 'external-subagent');
    const daemonEntry = () => path.join(fs.realpathSync(packageRoot()), PLATFORM_DIR, 'external-subagentd');
    const installedDaemonSha = () => {
      const manifest = JSON.parse(fs.readFileSync(path.join(packageRoot(), PLATFORM_DIR, 'payload.json'), 'utf8'));
      return manifest.files.find((file) => file.name === 'external-subagentd').sha256;
    };
    const launchctlState = () => {
      try { return JSON.parse(fs.readFileSync(path.join(workDir, 'launchctl-state.json'), 'utf8')); } catch { return { pid: null, program: null }; }
    };
    const runDriver = (label, args = [], options = {}) => {
      const result = run(process.execPath, [driver], {
        env: childEnv({
          R2_PACKAGE_ROOT: packageRoot(),
          R2_HOME: home,
          R2_SERVICE_LOG: serviceLog,
          R2_LAUNCHCTL_STATE: path.join(workDir, 'launchctl-state.json'),
          R2_CLAIM: `dl-${label}`,
          R2_LABEL: label,
          R2_ARGS: JSON.stringify(args),
          R2_MODE: options.mode || 'seam',
          R2_HEALTH_TIMEOUT_MS: String(options.healthTimeoutMs ?? 30_000),
        }),
        timeout: options.timeout ?? 120_000,
      });
      assert.equal(result.stderr, '', `driver ${label} stderr: ${result.stderr}`);
      assert.ok(result.stdout.trim().startsWith('{'), `driver ${label} printed no JSON: ${result.stdout.slice(0, 200)}`);
      return JSON.parse(result.stdout);
    };
    const cli = (args, timeout = 300_000) => {
      const result = run(cliBin(), args, { env: childEnv(), timeout });
      assert.equal(result.status, 0, `cli ${args[0]} failed: ${result.stderr || result.stdout}`);
      return JSON.parse(result.stdout).result;
    };
    const cliRaw = (args, timeout = 60_000) => run(cliBin(), args, { env: childEnv(), timeout });
    const waitOn = async (agentId, waitTime = 3) => cli(['wait', '--json', JSON.stringify({ agent_id: agentId, wait_time: waitTime })]);

    // ---- setup: two real tarballs, real vA install, init, dual-provider config
    const build = run(process.execPath, [path.join(repoRoot, 'scripts', 'release', 'build-native-payload.mjs'), '--if-stale'], { cwd: repoRoot, timeout: 600_000 });
    assert.equal(build.status, 0, `vA payload build failed: ${build.stderr}`);
    const tgzA = npmPack(workDir, repoRoot);
    assert.equal(path.basename(tgzA), `external-subagent-${VERSION_A}.tgz`);
    const vbSha = packVersionB(workDir);
    const tgzB = npmPack(workDir, path.join(workDir, 'vb-src'));
    assert.equal(path.basename(tgzB), `external-subagent-${VERSION_B}.tgz`);
    const repoSha = JSON.parse(fs.readFileSync(path.join(repoRoot, PLATFORM_DIR, 'payload.json'), 'utf8'))
      .files.find((file) => file.name === 'external-subagentd').sha256;
    assert.notEqual(vbSha, repoSha, 'the two real tarballs must carry different daemon artifacts');

    // --ignore-scripts represents the disabled-lifecycle-script install: no
    // npm hook fires and the product stays stage-only until its own CLI runs.
    const prefix = path.join(workDir, 'prefix');
    const npmEnv = { ...process.env, HOME: path.join(workDir, 'install-home'), npm_config_cache: path.join(workDir, 'npm-cache') };
    const installA = run('npm', ['install', '--global', `--prefix=${prefix}`, '--ignore-scripts', '--no-audit', '--no-fund', tgzA], { cwd: workDir, timeout: 300_000, env: npmEnv });
    assert.equal(installA.status, 0, `npm install of vA failed: ${installA.stderr}`);
    const shaA = installedDaemonSha();

    const init = run(cliBin(), ['init'], { env: childEnv(), timeout: 120_000 });
    assert.equal(init.status, 0, `init failed: ${init.stderr}`);
    const initReport = JSON.parse(init.stdout);
    assert.equal(initReport.ok, true);
    assert.equal(initReport.service.skipped, true, 'fixtures neutralize launchd through the documented seam');
    // B-3: init publishes the vA active/retention baseline itself — the live
    // standard sequence is `npm A -> init A -> use A -> npm B` with no hidden
    // extra "A update" between init and first use.
    const installedState = readState();
    assert.equal(installedState.schema_version, 2, 'init publishes the activation baseline');
    assert.equal(installedState.candidate, null);
    assert.equal(installedState.active.version, VERSION_A);
    assert.equal(installedState.active.daemon_entry_sha256, shaA, 'the baseline daemon digest is the verified vA payload digest');
    assert.ok(fs.existsSync(path.join(paths().data, 'payload-store', VERSION_A, 'external-subagentd')), 'init retains the verified vA bytes before any update runs');

    // Dual-provider daemon configuration through the product config owner,
    // then re-render the plist so the service env matches production shape.
    const configured = readConfig(paths().config);
    writeConfig(paths().config, {
      ...configured,
      agents: {
        zcode: { enabled: true, spawn_supported: true, default_model: null },
        dsh: { enabled: true, spawn_supported: true, default_model: null, runtime_path: DSH_RUNTIME, home: dshHome, profile: 'acp', version: DSH_VERSION },
      },
    });

    // ---- load the REAL vA service straight from the init-published baseline
    const up = runDriver('service-up', [], { mode: 'service-up', timeout: 120_000 });
    assert.equal(up.ok, true, `service-up failed: ${up.message}`);
    assert.equal(up.service.version, VERSION_A, 'the running service self-reports vA');
    assert.equal(up.service.artifact.path, daemonEntry());
    assert.equal(up.service.artifact.sha256, shaA);
    const pidA = up.service.pid;
    const generationA = up.service.service_generation;
    daemonPid = launchctlState().pid;
    assert.ok(Number.isInteger(pidA) && pidA > 0, 'the vA daemon runs as a real process');
    assert.ok(fs.readFileSync(paths().launchAgent, 'utf8').includes(DSH_RUNTIME), 'the re-rendered plist pins the DSH runtime environment');

    // ---- two REAL tasks on the vA daemon
    const taskA = cli(['spawn', '--subagent', 'zcode', '--repository', wsZcode, '--permission-mode', 'build',
      '--prompt', 'Use the Bash tool to run exactly: sleep 25 && echo DRAIN_LIVE_A_OK > zcode-marker.txt — after it finishes, reply with just: DONE']);
    assert.ok(taskA.agent_id > 0, 'the ZCode task was admitted');
    const taskB = cli(['spawn', '--subagent', 'dsh', '--repository', wsDsh, '--permission-mode', 'build',
      '--prompt', 'Use the Bash tool to run: sleep 600 — then reply with just: SLEPT']);
    assert.ok(taskB.agent_id > 0, 'the DSH task was admitted');

    // Both tasks must be genuinely in flight before the upgrade starts.
    const deadlineRunning = Date.now() + 120_000;
    let sawBRunning = false;
    while (!sawBRunning && Date.now() < deadlineRunning) {
      const polled = await waitOn(taskB.agent_id, 2);
      if (polled.task.status === 'running') sawBRunning = true;
      else assert.notEqual(polled.task.status, 'completed', 'the DSH task reached terminal before running');
    }
    assert.equal(sawBRunning, true, 'the DSH task never observed RUNNING');
    daemonPid = launchctlState().pid;

    // ---- npm installs vB into the SAME prefix while the vA daemon serves tasks
    const installB = run('npm', ['install', '--global', `--prefix=${prefix}`, '--ignore-scripts', '--no-audit', '--no-fund', tgzB], { cwd: workDir, timeout: 300_000, env: npmEnv });
    assert.equal(installB.status, 0, `npm install of vB failed: ${installB.stderr}`);
    const shaB = installedDaemonSha();
    assert.notEqual(shaB, shaA, 'the replaced payload carries a different daemon artifact');
    assert.equal(run(cliBin(), ['version'], { env: childEnv(), timeout: 30_000 }).stdout.trim(), VERSION_B);
    assert.equal(launchctlState().pid, daemonPid, 'the vA daemon process survives the npm replacement');
    const vaStatus = await callDaemon(paths().socket, 'status', {});
    assert.equal(vaStatus.identity.daemon.version, VERSION_A, 'the running daemon still self-reports vA after the file replacement');

    // ---- the default safe drain/update runs in the background
    updateChild = spawn(process.execPath, [driver], {
      env: childEnv({
        R2_PACKAGE_ROOT: packageRoot(),
        R2_HOME: home,
        R2_SERVICE_LOG: serviceLog,
        R2_LAUNCHCTL_STATE: path.join(workDir, 'launchctl-state.json'),
        R2_CLAIM: 'dl-live',
        R2_LABEL: 'live',
        R2_ARGS: '[]',
        R2_MODE: 'real-update',
        R2_HEALTH_TIMEOUT_MS: '30000',
      }),
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let updateOut = ''; let updateErr = '';
    updateChild.stdout.on('data', (chunk) => { updateOut += chunk; });
    updateChild.stderr.on('data', (chunk) => { updateErr += chunk; });
    const updateDone = new Promise((resolve) => updateChild.once('exit', (code) => resolve({ code, stdout: updateOut, stderr: updateErr })));

    const deadlineDraining = Date.now() + 30_000;
    let draining = false;
    while (!draining && Date.now() < deadlineDraining) {
      try {
        draining = (await callDaemon(paths().socket, 'drain-status', {})).is_draining === true;
      } catch { await new Promise((resolve) => setTimeout(resolve, 100)); }
    }
    assert.equal(draining, true, 'the update never started draining the daemon');

    // During drain: new spawns are rejected.
    const rejected = cliRaw(['spawn', '--subagent', 'dsh', '--repository', wsDsh, '--permission-mode', 'build', '--prompt', 'hi'], 30_000);
    assert.notEqual(rejected.status, 0, 'a new spawn during drain must fail');
    const rejection = JSON.parse(rejected.stderr);
    assert.match(rejection.error.message, /daemon_draining/, `new spawn during drain must be rejected as draining, got: ${rejected.stderr}`);

    // During drain: the old ZCode task still waits, surfaces its pending
    // permission request, and accepts the response.
    const deadlinePending = Date.now() + 180_000;
    let requestId = null;
    while (!requestId && Date.now() < deadlinePending) {
      const polled = await waitOn(taskA.agent_id, 2);
      const pending = Array.isArray(polled.pending_requests) ? polled.pending_requests : [];
      if (pending.length > 0) requestId = pending[0].request_id;
      else assert.notEqual(polled.task.status, 'completed', `the ZCode task reached terminal without a pending request (${polled.task.status})`);
    }
    assert.ok(requestId, 'the ZCode task never surfaced a pending permission request');
    cli(['respond', '--json', JSON.stringify({ agent_id: taskA.agent_id, request_id: requestId, decision: 'allow' })]);

    // During drain: the old task runs to completion and yields result/close.
    const deadlineTerminal = Date.now() + 240_000;
    let terminal = null;
    while (!terminal && Date.now() < deadlineTerminal) {
      const polled = await waitOn(taskA.agent_id, 3);
      if (polled.task.status === 'completed') terminal = polled;
    }
    assert.ok(terminal, 'the ZCode task never completed during the drain');
    assert.equal(terminal.task.status, 'completed');
    const taskAResult = cli(['result', '--json', JSON.stringify({ agent_id: taskA.agent_id })]);
    assert.equal(typeof taskAResult.result.final_text, 'string');
    assert.ok(taskAResult.result.final_text.length > 0, 'the completed task produced a final result during drain');
    const taskAClosed = cli(['close', '--json', JSON.stringify({ agent_id: taskA.agent_id })]);
    assert.equal(taskAClosed.task.status, 'closed', 'close stays available during drain');
    assert.equal(fs.readFileSync(path.join(wsZcode, 'zcode-marker.txt'), 'utf8'), 'DRAIN_LIVE_A_OK\n', 'the ZCode task performed its real work');

    // During drain: the long DSH task is cancelled mid-run and reaped.
    const taskBCancelled = cli(['cancel', '--json', JSON.stringify({ agent_id: taskB.agent_id })]);
    assert.equal(taskBCancelled.task.status, 'cancelled');
    const taskBClosed = cli(['close', '--json', JSON.stringify({ agent_id: taskB.agent_id })]);
    assert.equal(taskBClosed.task.status, 'closed');

    // ---- the same update run activates vB automatically after the reap
    const updateExit = await updateDone;
    updateChild = null;
    assert.equal(updateExit.stderr, '', `update driver stderr: ${updateExit.stderr}`);
    assert.ok(updateExit.stdout.trim().startsWith('{'), `update driver printed no JSON: ${updateExit.stdout.slice(0, 300)}`);
    const update = JSON.parse(updateExit.stdout);
    assert.equal(update.ok, true, `the single public update failed: ${update.code} ${update.message}`);
    assert.equal(update.result.phase, 'active');
    assert.equal(update.result.active.version, VERSION_B, 'active advanced from vA to vB');
    assert.equal(update.result.active.daemon_entry, daemonEntry());
    assert.equal(update.result.active.daemon_entry_sha256, shaB, 'the active daemon digest is the verified vB payload digest');

    const service = update.result.service;
    assert.ok(Number.isInteger(service.pid) && service.pid > 0, 'activation health-verified a real daemon pid');
    assert.notEqual(service.pid, pidA, 'the daemon was replaced, not reused');
    assert.ok(service.service_generation && service.service_generation !== generationA, 'the new daemon carries a new service generation');
    assert.equal(service.version, VERSION_B, 'the running daemon self-reports vB');
    assert.equal(service.artifact.path, update.result.active.retained.daemon_entry, 'the daemon self-reports the retained artifact path');
    assert.equal(service.artifact.sha256, update.result.active.retained.daemon_entry_sha256, 'the running identity matches the retained payload digest');
    assert.equal(service.artifact.sha256, shaB, 'the daemon self-reports the verified payload digest');
    daemonPid = launchctlState().pid;
    assert.equal(daemonPid, service.pid, 'the loaded service process is the health-verified daemon');

    const receipt = readReceipt();
    assert.equal(receipt.status, 'success');
    assert.match(receipt.claim, /^agentd-\d+-activation$/, 'the receipt carries the real daemon-issued activation claim');
    assert.equal(receipt.version, VERSION_B);
    const afterState = readState();
    assert.equal(afterState.candidate, null, 'a completed activation leaves no candidate behind');
    assert.equal(afterState.active.version, VERSION_B);
    assert.equal(afterState.active.daemon_entry_sha256, shaB);
    assert.equal(update.result.homes.all_updated, true, 'the claimed Codex home was re-bound');

    // The retained store kept the vA bytes alive through the replacement.
    const retainedA = path.join(paths().data, 'payload-store', VERSION_A, 'external-subagentd');
    assert.ok(fs.existsSync(retainedA), 'the retired vA daemon bytes are retained');
    assert.equal(sha256(fs.readFileSync(retainedA)), shaA, 'the retained vA bytes match the previously verified digest');
    const retainedB = path.join(paths().data, 'payload-store', VERSION_B, 'external-subagentd');
    assert.equal(sha256(fs.readFileSync(retainedB)), shaB);

    // ---- the NEW daemon serves both upstreams again
    const postZcode = cli(['spawn', '--subagent', 'zcode', '--repository', path.join(workDir, 'ws-post-z'), '--permission-mode', 'yolo',
      '--prompt', 'Reply with exactly the single word POST_ZCODE_OK and nothing else. Do not use any tools.']);
    const waitForTerminal = async (agentId, budgetMs = 240_000) => {
      const deadline = Date.now() + budgetMs;
      for (;;) {
        const polled = await waitOn(agentId, 5);
        if (polled.task.status === 'completed') return polled;
        if (Date.now() > deadline) return null;
      }
    };
    const postZcodeDone = await waitForTerminal(postZcode.agent_id);
    assert.ok(postZcodeDone, 'the post-upgrade ZCode task never completed');
    assert.equal(postZcodeDone.task.status, 'completed');
    assert.equal(postZcodeDone.task.input_identity.model_source, 'native');
    assert.ok(cli(['close', '--json', JSON.stringify({ agent_id: postZcode.agent_id })]).task.status === 'closed');

    const postDsh = cli(['spawn', '--subagent', 'dsh', '--repository', path.join(workDir, 'ws-post-d'), '--permission-mode', 'build',
      '--prompt', 'Reply with just: POST_DSH_OK']);
    const postDshDone = await waitForTerminal(postDsh.agent_id);
    assert.ok(postDshDone, 'the post-upgrade DSH task never completed');
    assert.equal(postDshDone.task.status, 'completed');
    assert.ok(cli(['close', '--json', JSON.stringify({ agent_id: postDsh.agent_id })]).task.status === 'closed');

    settled = true;
    console.log([
      `live-drain-evidence va=${VERSION_A}@pid${pidA} vb=${VERSION_B}@pid${service.pid}`,
      'drain: spawn-rejected=daemon_draining zcode=wait/pending/respond-allow/COMPLETED/reaped/result/close dsh=mid-run-CANCELLED/reaped/close',
      `activation: single-update pid=${service.pid} generation-changed=${service.service_generation !== generationA}`,
      'retained: vA bytes preserved through npm replacement',
      'post-upgrade: zcode=COMPLETED(native-model) dsh=COMPLETED',
    ].join('\n'));
  } finally {
    if (updateChild && updateChild.exitCode === null) {
      try { updateChild.kill('SIGTERM'); } catch { /* already gone */ }
      await new Promise((resolve) => updateChild.once('exit', resolve));
    }
    const state = (() => {
      try { return JSON.parse(fs.readFileSync(path.join(workDir, 'launchctl-state.json'), 'utf8')); } catch { return { pid: null }; }
    })();
    const pid = state.pid || daemonPid;
    if (pid) {
      try { process.kill(pid, 'SIGTERM'); } catch { /* already gone */ }
      const deadline = Date.now() + 10_000;
      try {
        while (Date.now() < deadline) { process.kill(pid, 0); await new Promise((resolve) => setTimeout(resolve, 100)); }
        process.kill(pid, 'SIGKILL');
      } catch { /* reaped */ }
    }
    let driftFailure = null;
    const drift = bridged
      .filter(([label, target]) => (fs.existsSync(target)
        ? before.has(label) && bridgeDigest(target) !== before.get(label)
        : before.has(label)))
      .map(([label]) => label);
    if (drift.length > 0) {
      const message = `bridged real provider files were modified: ${drift.join(', ')}`;
      if (settled) driftFailure = new assert.AssertionError({ message });
      else t.diagnostic(`BRIDGE DRIFT in addition to the failure above: ${message}`);
    }
    fs.rmSync(workDir, { recursive: true, force: true });
    if (driftFailure) throw driftFailure;
  }
});
