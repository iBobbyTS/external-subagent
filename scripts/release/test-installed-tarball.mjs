#!/usr/bin/env node
// Controlled-prefix install check for the packed artifact: install the tgz
// into a throwaway npm prefix with an isolated HOME and npm cache, then verify
// the staged bin entries, install-only HOME purity, an explicit host-neutral
// init (with launchd neutralized), and the resulting standalone-service
// artifacts (LaunchAgent, config, activation state, retained payload bytes).
// This never touches the real user HOME, the real ~/.codex, or launchd.
// Usage: test-installed-tarball.mjs [package.tgz]
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { nativePlatform } from '../../cli/install/layout.mjs';

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
  // The packed payload directory is the running host's tuple (S01), so the
  // smoke checks the platform it is actually installing rather than a fixed
  // darwin-arm64 path.
  const payloadPlatform = nativePlatform() ?? `${process.platform}-${process.arch}`;
  for (const target of [cli, path.join(prefix, 'bin', 'external-subagent-mcp'), path.join(packageRoot, 'npm', 'native', payloadPlatform, 'external-subagentd')]) {
    fs.accessSync(target, fs.constants.X_OK);
  }
  run(cli, ['version'], { env });
  const homeEntries = fs.readdirSync(home).filter((entry) => entry !== '.npm' && entry !== '.cache');
  if (homeEntries.length !== 0) {
    process.stderr.write(`install-only must not write the home; found: ${homeEntries.join(', ')}\n`);
    process.exit(1);
  }

  // Host-neutral init: the standalone service layer only. Codex is never
  // invoked and no host binding is staged — those belong to install-plugin /
  // install-mcp and are covered by the install test suite, not this smoke.
  const init = JSON.parse(run(cli, ['init'], { env }).stdout);
  if (!init.ok || !init.completed?.includes('publish-active-payload')) {
    process.stderr.write(`init did not complete: ${JSON.stringify(init)}\n`);
    process.exit(1);
  }
  const data = path.join(home, 'Library', 'Application Support', 'external-subagent');
  const expectedArtifacts = [
    path.join(home, 'Library', 'LaunchAgents', 'com.external-subagent.daemon.plist'),
    path.join(data, 'config.json'),
    path.join(data, 'install-state.json'),
    path.join(data, 'payload-store', init.payload.version, 'external-subagentd'),
  ];
  for (const target of expectedArtifacts) {
    if (!fs.existsSync(target)) { process.stderr.write(`init artifact missing: ${target}\n`); process.exit(1); }
  }
  for (const forbidden of [path.join(home, 'plugins', 'external-subagent'), path.join(home, '.agents')]) {
    if (fs.existsSync(forbidden)) { process.stderr.write(`init must bind no host: ${forbidden} exists\n`); process.exit(1); }
  }
  process.stdout.write(`${JSON.stringify({ installed: true, prefix: `${prefix} (removed)`, payload: init.payload, service: init.service }, null, 2)}\n`);
} finally {
  fs.rmSync(work, { recursive: true, force: true });
}
