#!/usr/bin/env node
// Controlled-prefix install check for the packed artifact: install the tgz
// into a throwaway npm prefix with an isolated HOME and npm cache, then verify
// the staged bin entries, install-only HOME purity, an explicit init (with
// launchd neutralized), and the resulting LaunchAgent/binding/registry files.
// This never touches the real user HOME, the real ~/.codex, or launchd.
// Usage: test-installed-tarball.mjs [package.tgz]
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

function run(command, args, options = {}) {
  const result = spawnSync(command, args, { encoding: 'utf8', ...options });
  if (result.status !== 0) {
    process.stderr.write(`command failed: ${command} ${args.join(' ')}\n${result.stderr || result.stdout}\n`);
    process.exit(result.status ?? 1);
  }
  return result;
}

const tgzArgument = process.argv[2] || path.join(process.cwd(), (fs.readdirSync(process.cwd()).filter((name) => /^external-subagent-\d+\.\d+\.\d+\.tgz$/.test(name)).sort().slice(-1)[0] || ''));
const tgz = tgzArgument ? path.resolve(tgzArgument) : '';
if (!tgz || !fs.existsSync(tgz)) {
  process.stderr.write('no packed tarball found; run npm pack first or pass the tgz path\n');
  process.exit(2);
}

const work = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-prefix-'));
const prefix = path.join(work, 'prefix');
const home = path.join(work, 'home');
fs.mkdirSync(home, { recursive: true });
const cache = path.join(work, 'npm-cache');
const env = { ...process.env, HOME: home, CODEX_HOME: path.join(home, '.codex'), EXTERNAL_SUBAGENT_TEST_NO_LAUNCHCTL: '1', npm_config_cache: cache };

try {
  run('npm', ['install', '--global', `--prefix=${prefix}`, '--no-audit', '--no-fund', tgz], { cwd: work, env });
  const cli = path.join(prefix, 'bin', 'external-subagent');
  const packageRoot = path.join(prefix, 'lib', 'node_modules', 'external-subagent');
  for (const target of [cli, path.join(prefix, 'bin', 'external-subagent-mcp'), path.join(packageRoot, 'npm', 'native', 'darwin-arm64', 'external-subagentd')]) {
    fs.accessSync(target, fs.constants.X_OK);
  }
  run(cli, ['version'], { env });
  const homeEntries = fs.readdirSync(home).filter((entry) => entry !== '.npm' && entry !== '.cache');
  if (homeEntries.length !== 0) {
    process.stderr.write(`install-only must not write the home; found: ${homeEntries.join(', ')}\n`);
    process.exit(1);
  }

  // Use the real codex CLI when present (it only ever sees the throwaway
  // CODEX_HOME); otherwise fall back to an explicit skip so the check still
  // verifies the install/service layer without pretending Codex ran.
  const codexAvailable = spawnSync('codex', ['--version'], { encoding: 'utf8' }).status === 0;
  const initArgs = ['init', '--skip-runtime-probe'];
  if (!codexAvailable) initArgs.push('--skip-codex-plugin');
  const init = JSON.parse(run(cli, initArgs, { env }).stdout);
  if (!init.ok || !init.installed) {
    process.stderr.write(`init did not complete: ${JSON.stringify(init)}\n`);
    process.exit(1);
  }
  const expectedArtifacts = [
    path.join(home, 'Library', 'LaunchAgents', 'com.external-subagent.daemon.plist'),
    path.join(home, 'Library', 'Application Support', 'external-subagent', 'config.json'),
    path.join(home, 'plugins', 'external-subagent', '.mcp.json'),
  ];
  if (codexAvailable) expectedArtifacts.push(path.join(home, '.agents', 'plugins', 'marketplace.json'));
  for (const target of expectedArtifacts) {
    if (!fs.existsSync(target)) { process.stderr.write(`init artifact missing: ${target}\n`); process.exit(1); }
  }
  process.stdout.write(`${JSON.stringify({ installed: true, codex_cli: codexAvailable ? 'real' : 'skipped', prefix: `${prefix} (removed)`, payload: init.payload, service: init.service }, null, 2)}\n`);
} finally {
  fs.rmSync(work, { recursive: true, force: true });
}
