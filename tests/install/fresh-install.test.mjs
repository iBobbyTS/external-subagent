// S05 fresh npm install acceptance: the packed artifact installs into a
// throwaway npm prefix, a plain install only stages the package and payload
// (no daemon, no Codex writes, no provider probes), and an explicit `init`
// coordinates the LaunchAgent service, the managed Codex plugin/MCP binding,
// and the D08 Codex-homes claim.  The installed native payload then runs the
// real daemon, answers CLI status with both agents (DSH explicitly missing),
// and exposes exactly ten MCP tools through the stable binary facade.
//
// All writes stay inside mkdtemp fixtures: npm prefix/cache and HOME are
// per-test temp directories; launchd is neutralized through the documented
// EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL seam; the codex CLI is a recording fake.
// Real-home installation, real Codex hosts, and live provider hi are NOT_RUN
// here and remain acceptance-gated outside this file.
import test, { after } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';

const repoRoot = path.resolve(import.meta.dirname, '../..');
const testable = process.platform === 'darwin' && process.arch === 'arm64';

const ctx = { workDir: null, prefix: null, packageRoot: null, cli: null, daemon: null, mcpFacade: null, tgz: null };

function run(command, args, options = {}) {
  return spawnSync(command, args, { encoding: 'utf8', ...options });
}

function jsonOutput(result, label) {
  assert.equal(result.status, 0, `${label} failed: ${result.stderr}`);
  try {
    return JSON.parse(result.stdout);
  } catch (error) {
    assert.fail(`${label} did not print JSON: ${result.stdout.slice(0, 400)}`);
  }
}

function fakeCodexCli(directory) {
  const log = path.join(directory, 'codex-invocations.jsonl');
  // Named `codex` so the subprocess CLI resolves it from PATH like the real
  // binary; init has no codexCli injection point.
  const script = path.join(directory, 'codex');
  fs.writeFileSync(script, `#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const log = process.env.FAKE_CODEX_LOG || path.join(path.dirname(fileURLToPath(import.meta.url)), 'codex-invocations.jsonl');
const args = process.argv.slice(2);
fs.appendFileSync(process.env.FAKE_CODEX_LOG, JSON.stringify({ args, codex_home: process.env.CODEX_HOME }) + '\\n');
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
  return { dir: directory, cli: script, log };
}

function fixtureEnv(home, extra = {}) {
  return {
    ...process.env,
    HOME: home,
    CODEX_HOME: extra.codexHome || path.join(home, '.codex'),
    EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL: '1',
    ...extra.env,
  };
}

// npm pack/install setup is shared by every test through a memoized promise;
// per-test { skip } gates keep unsupported platforms out without relying on
// hook options.
let setupPromise = null;
function ensureInstalled() {
  if (setupPromise === null) setupPromise = doInstall();
  return setupPromise;
}

async function doInstall() {
  ctx.workDir = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-fresh-'));
  const build = run(process.execPath, [path.join(repoRoot, 'scripts/release/build-native-payload.mjs'), '--if-stale'], { cwd: repoRoot, stdio: 'inherit' });
  assert.equal(build.status, 0, 'native payload build failed');

  const packDir = path.join(ctx.workDir, 'pack');
  fs.mkdirSync(packDir, { recursive: true });
  const packed = run('npm', ['pack', '--json', `--pack-destination=${packDir}`], {
    cwd: repoRoot,
    env: { ...process.env, HOME: ctx.workDir, npm_config_cache: path.join(ctx.workDir, 'npm-cache') },
  });
  assert.equal(packed.status, 0, `npm pack failed: ${packed.stderr}`);
  const [artifact] = JSON.parse(packed.stdout);
  ctx.tgz = path.join(packDir, artifact.filename);
  assert.match(artifact.filename, /^external-subagent-\d+\.\d+\.\d+\.tgz$/u);

  ctx.prefix = path.join(ctx.workDir, 'prefix');
  ctx.packageRoot = path.join(ctx.prefix, 'lib', 'node_modules', 'external-subagent');
  ctx.cli = path.join(ctx.prefix, 'bin', 'external-subagent');
  ctx.mcpFacade = path.join(ctx.prefix, 'bin', 'external-subagent-mcp');
  ctx.daemon = path.join(ctx.packageRoot, 'npm', 'native', 'darwin-arm64', 'external-subagentd');

  const installHome = path.join(ctx.workDir, 'install-home');
  fs.mkdirSync(installHome, { recursive: true });
  const install = run('npm', ['install', '--global', `--prefix=${ctx.prefix}`, '--no-audit', '--no-fund', ctx.tgz], {
    cwd: ctx.workDir,
    env: { ...process.env, HOME: installHome, npm_config_cache: path.join(ctx.workDir, 'npm-cache') },
  });
  assert.equal(install.status, 0, `npm install failed: ${install.stderr}`);
}

after(() => {
  if (ctx.workDir) fs.rmSync(ctx.workDir, { recursive: true, force: true });
});

test('install-only stages the package and payload without touching the home', { skip: !testable }, async () => {
  await ensureInstalled();
  assert.ok(fs.existsSync(ctx.cli), 'npm global bin entry must exist');
  assert.ok(fs.existsSync(ctx.mcpFacade), 'npm global MCP facade must exist');

  const installedPackage = JSON.parse(fs.readFileSync(path.join(ctx.packageRoot, 'package.json'), 'utf8'));
  for (const lifecycle of ['preinstall', 'postinstall', 'prepublish', 'prepare']) {
    assert.equal(installedPackage.scripts?.[lifecycle], undefined, `plain install must not auto-run ${lifecycle}`);
  }

  const payload = JSON.parse(fs.readFileSync(path.join(ctx.packageRoot, 'npm', 'native', 'darwin-arm64', 'payload.json'), 'utf8'));
  assert.equal(payload.version, installedPackage.version, 'payload version must match the package version');
  assert.equal(payload.platform, 'darwin-arm64');
  for (const file of payload.files) {
    const target = path.join(ctx.packageRoot, 'npm', 'native', 'darwin-arm64', file.name);
    const stat = fs.statSync(target);
    assert.equal(stat.mode & 0o777, 0o755, `${file.name} must keep release permissions`);
    const bytes = fs.readFileSync(target);
    assert.equal(bytes.length, file.bytes);
    assert.equal(bytes.subarray(0, 4).toString('hex'), 'cffaedfe', `${file.name} must be a little-endian Mach-O 64-bit binary`);
    assert.equal(bytes.readUInt32LE(4), 0x0100000c, `${file.name} must be arm64`);
  }

  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-idle-'));
  for (const args of [['version'], ['help']]) {
    const result = run(ctx.cli, args, { env: fixtureEnv(home) });
    assert.equal(result.status, 0, result.stderr);
  }
  assert.deepEqual(fs.readdirSync(home), [], 'plain install must not start the daemon, write Codex, probe providers, or write any HOME state');
});

test('unsupported platforms allow help/version but reject business commands without writes', { skip: !testable }, async () => {
  await ensureInstalled();
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-unsup-'));
  const env = { ...fixtureEnv(home), ZCODE_AS_SUBAGENT_TEST_PLATFORM: 'win32' };
  const init = run(ctx.cli, ['init'], { env });
  assert.equal(init.status, 1);
  assert.equal(JSON.parse(init.stderr).error.code, 'UNSUPPORTED_PLATFORM');
  assert.deepEqual(fs.readdirSync(home), []);
  assert.equal(run(ctx.cli, ['version'], { env }).status, 0);
});

test('explicit init installs service, binds Codex, and claims the codex home', { skip: !testable }, async () => {
  await ensureInstalled();
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-init-'));
  const shimDir = path.join(home, '.shim');
  fs.mkdirSync(shimDir, { recursive: true });
  const fake = fakeCodexCli(shimDir);
  const env = fixtureEnv(home, { env: { PATH: `${fake.dir}:${process.env.PATH}`, FAKE_CODEX_LOG: fake.log } });
  const report = jsonOutput(run(ctx.cli, ['init'], { env }), 'init');
  assert.equal(report.ok, true);
  for (const step of ['verify-payload', 'install-launch-agent', 'install-codex-plugin', 'claim-codex-home']) {
    assert.ok(report.completed.includes(step), `init must complete ${step}`);
  }
  assert.equal(report.service.skipped, true, 'fixture runs neutralize launchd explicitly');
  assert.equal(report.payload.status, 'verified');

  const data = path.join(home, 'Library', 'Application Support', 'external-subagent');
  const plistPath = path.join(home, 'Library', 'LaunchAgents', 'com.external-subagent.daemon.plist');
  const plist = fs.readFileSync(plistPath, 'utf8');
  assert.ok(plist.includes(ctx.daemon), 'LaunchAgent must point at the installed daemon payload');
  assert.ok(plist.includes(path.join(data, 'external-subagent.sqlite3')));
  assert.ok(plist.includes(path.join(data, 'external-subagent.sock')));
  assert.ok(plist.includes('/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs'));
  assert.ok(plist.includes('/usr/bin:/bin'), 'GUI environment must not inherit the shell PATH');
  const config = JSON.parse(fs.readFileSync(path.join(data, 'config.json'), 'utf8'));
  assert.equal(config.socket, path.join(data, 'external-subagent.sock'));

  const staging = path.join(home, 'plugins', 'external-subagent');
  const staged = JSON.parse(fs.readFileSync(path.join(staging, '.mcp.json'), 'utf8'));
  // Node resolves the CLI through its realpath (/private/var under tmpdirs),
  // which is the stable entry the staging must pin.
  assert.equal(staged.mcpServers.external_subagent.command, fs.realpathSync(path.join(ctx.packageRoot, 'npm', 'native', 'darwin-arm64', 'external-subagent-mcp')));
  assert.equal(staged.mcpServers.external_subagent.env.ZCODE_AGENTD_SOCKET, path.join(data, 'external-subagent.sock'));

  const calls = fs.readFileSync(fake.log, 'utf8').trim().split('\n').map((line) => JSON.parse(line));
  const add = calls.find((call) => call.args[0] === 'plugin' && call.args[1] === 'add' && !call.args.includes('--help'));
  assert.equal(add.args[2], 'external-subagent');
  assert.equal(add.codex_home, path.join(home, '.codex'));

  const registry = JSON.parse(fs.readFileSync(path.join(data, 'codex-homes.json'), 'utf8'));
  assert.deepEqual(registry.homes.map((entry) => entry.home), [fs.realpathSync(path.join(home, '.codex'))], 'init claims exactly the configured codex home');
  assert.equal(registry.homes[0].version, report.payload.version);

  for (const profile of ['.zshrc', '.zprofile', '.bash_profile', '.bashrc']) {
    assert.equal(fs.existsSync(path.join(home, profile)), false, 'init must never edit shell profiles');
  }

  const again = jsonOutput(run(ctx.cli, ['init'], { env }), 'repeat init');
  assert.equal(again.ok, true);
  const registryAfter = JSON.parse(fs.readFileSync(path.join(data, 'codex-homes.json'), 'utf8'));
  assert.equal(registryAfter.homes.length, 1, 'repeat init stays idempotent in the D08 registry');
  const marketplace = JSON.parse(fs.readFileSync(path.join(home, '.agents', 'plugins', 'marketplace.json'), 'utf8'));
  assert.equal(marketplace.plugins.filter((entry) => entry.name === 'external-subagent').length, 1);
});

test('unicode and spaces in the install home keep working', { skip: !testable }, async () => {
  await ensureInstalled();
  const base = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-odd '));
  const home = path.join(base, '安装 目录');
  fs.mkdirSync(home, { recursive: true });
  const shimDir = path.join(base, 'shim-dir');
  fs.mkdirSync(shimDir, { recursive: true });
  const fake = fakeCodexCli(shimDir);
  const env = fixtureEnv(home, { env: { PATH: `${path.dirname(fake.cli)}:${process.env.PATH}`, FAKE_CODEX_LOG: fake.log } });
  const result = run(ctx.cli, ['init'], { env });
  const report = jsonOutput(result, 'unicode init');
  assert.equal(report.ok, true);
  assert.ok(fs.existsSync(path.join(home, 'Library', 'LaunchAgents', 'com.external-subagent.daemon.plist')));
  const registry = JSON.parse(fs.readFileSync(path.join(home, 'Library', 'Application Support', 'external-subagent', 'codex-homes.json'), 'utf8'));
  assert.equal(registry.homes.length, 1);
});

test('installed daemon payload serves status, agent states, and ten MCP tools', { skip: !testable }, async () => {
  await ensureInstalled();
  // Unix socket paths are bounded by SUN_LEN on macOS; the default tmpdir
  // plus the product data tree can exceed it, so the daemon fixture uses a
  // short base exactly like a real (short) HOME would.
  const home = fs.mkdtempSync('/tmp/external-subagent-live-');
  const data = path.join(home, 'Library', 'Application Support', 'external-subagent');
  fs.mkdirSync(data, { recursive: true, mode: 0o700 });
  const logs = path.join(home, 'Library', 'Logs', 'external-subagent');
  fs.mkdirSync(logs, { recursive: true, mode: 0o700 });
  const socket = path.join(data, 'external-subagent.sock');
  const providerHome = path.join(home, 'provider');
  const workspace = path.join(home, 'workspace');
  fs.mkdirSync(providerHome);
  fs.mkdirSync(workspace);
  const runtime = path.join(home, 'dsh-fixture.mjs');
  fs.copyFileSync(path.join(repoRoot, 'tests/fixtures/dsh-hi-probe.mjs'), runtime);
  assert.equal(fs.existsSync(path.join(ctx.packageRoot, 'crates')), false);
  assert.equal(fs.existsSync(path.join(ctx.packageRoot, 'profiles')), false);

  const daemon = spawn(ctx.daemon, [
    '--database', path.join(data, 'external-subagent.sqlite3'),
    '--socket', socket,
    '--runtime', '/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs',
    '--diagnostic-log', path.join(logs, 'daemon-error.log'),
  ], { cwd: ctx.packageRoot, env: fixtureEnv(home, { env: { DSH_RUNTIME_PATH: runtime } }), stdio: ['ignore', 'pipe', 'pipe'] });
  let daemonStderr = '';
  let daemonExited = false;
  daemon.on('exit', () => { daemonExited = true; });
  daemon.stderr.on('data', (chunk) => { daemonStderr += chunk; });

  try {
    const deadline = Date.now() + 20_000;
    while (!fs.existsSync(socket) && Date.now() < deadline) await new Promise((resolve) => setTimeout(resolve, 100));
    assert.ok(fs.existsSync(socket), `daemon socket must appear: ${daemonStderr}`);

    const env = fixtureEnv(home, { env: { ZCODE_AGENTD_SOCKET: socket } });
    const status = jsonOutput(run(ctx.cli, ['status'], { env }), 'status');
    assert.equal(status.ok, true);
    assert.equal(status.daemon_status.components.daemon, 'READY');

    const agents = jsonOutput(run(ctx.cli, ['agents', 'status'], { env }), 'agents status');
    const byAgent = Object.fromEntries(agents.agents.map((agent) => [agent.agent, agent]));
    assert.equal(byAgent.zcode.enabled, true);
    assert.equal(byAgent.zcode.spawn_supported, true);
    assert.equal(byAgent.dsh.enabled, false, 'DSH stays explicitly missing without any auto-install');
    assert.equal(byAgent.dsh.spawn_supported, false);
    assert.equal(byAgent.dsh.local.state, 'UNKNOWN', 'missing DSH must surface as unknown, never as valid');


    // Exercise the installed native binary with no source/resources in its
    // package tree. The provider rejects build-tree patch paths explicitly.
    const authProbe = jsonOutput(run(ctx.cli, ['agents', 'probe', 'dsh', '--auth', '--workspace', workspace, '--home', providerHome], { env }), 'DSH auth');
    assert.equal(fs.existsSync(path.join(providerHome, 'probe.jsonl')), false, 'auth-only must not start ACP or prompt');
    const hiProbe = jsonOutput(run(ctx.cli, ['agents', 'probe', 'dsh', '--hi', '--workspace', workspace, '--home', providerHome], { env }), 'DSH hi');
    assert.equal(hiProbe.hi.state, 'READY');
    assert.equal(authProbe.auth.state, 'UNKNOWN');
    const events = fs.readFileSync(path.join(providerHome, 'probe.jsonl'), 'utf8').trim().split('\n').map(JSON.parse);
    assert.equal(events[0].kind, 'dump');
    assert.equal(events.filter((event) => event.method === 'session/prompt').length, 1);
    for (const event of events.filter((event) => event.kind)) {
      assert.equal(event.cwd, workspace);
      assert.equal(event.home, providerHome);
      assert.equal(event.mode, 'read-only');
      assert.equal(fs.existsSync(event.patch), false, 'probe patch must be reaped');
      assert.equal(event.patch.startsWith(repoRoot), false);
    }
    assert.deepEqual(fs.readdirSync(workspace), []);

    const facade = spawn(ctx.mcpFacade, [], {
      env: fixtureEnv(home, { env: { ZCODE_AGENTD_SOCKET: socket } }),
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    let facadeStderr = '';
    facade.stderr.on('data', (chunk) => { facadeStderr += chunk; });
    const replies = [];
    const reader = readline.createInterface({ input: facade.stdout });
    reader.on('line', (line) => replies.push(JSON.parse(line)));
    const send = (frame) => facade.stdin.write(`${JSON.stringify(frame)}\n`);
    send({ jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 's05-install-check', version: '1' } } });
    await waitFor(() => replies.some((reply) => reply.id === 1), 'MCP initialize');
    send({ jsonrpc: '2.0', method: 'notifications/initialized' });
    send({ jsonrpc: '2.0', id: 2, method: 'tools/list', params: {} });
    await waitFor(() => replies.some((reply) => reply.id === 2), 'MCP tools/list');
    const tools = replies.find((reply) => reply.id === 2).result.tools.map((tool) => tool.name).sort();
    assert.equal(tools.length, 10, 'the daemon MCP surface exposes exactly ten tools');
    assert.deepEqual(tools, ['external_subagent_cancel', 'external_subagent_close', 'external_subagent_list', 'external_subagent_observe', 'external_subagent_respond', 'external_subagent_result', 'external_subagent_send', 'external_subagent_spawn', 'external_subagent_status', 'external_subagent_wait']);
    facade.stdin.end();
    await new Promise((resolve) => facade.on('exit', resolve));
  } finally {
    daemon.kill('SIGTERM');
    if (!daemonExited) await new Promise((resolve) => daemon.on('exit', resolve));
  }
});

async function waitFor(predicate, label, timeoutMs = 10_000) {
  const deadline = Date.now() + timeoutMs;
  while (!predicate() && Date.now() < deadline) await new Promise((resolve) => setTimeout(resolve, 50));
  assert.ok(predicate(), `timed out waiting for ${label}`);
}
