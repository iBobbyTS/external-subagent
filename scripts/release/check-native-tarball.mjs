#!/usr/bin/env node
// Release pack static checks: verify the packed tarball carries the managed
// CLI entry points and the verified darwin-arm64 native payload, keeps the
// payload manifest consistent with the package version, and excludes
// development material (.agent-work, target, tests, node_modules, databases,
// logs, credentials).  Usage: check-native-tarball.mjs [package.tgz]
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

const tgz = process.argv[2] || (() => {
  const staged = fs.readdirSync(process.cwd()).filter((name) => /^external-subagent-\d+\.\d+\.\d+\.tgz$/.test(name)).sort();
  if (staged.length === 0) {
    process.stderr.write('no packed tarball found; run npm pack first or pass the tgz path\n');
    process.exit(2);
  }
  return path.join(process.cwd(), staged[staged.length - 1]);
})();

const required = [
  'package/package.json',
  'package/bin/external-subagent.mjs',
  'package/bin/external-subagent-mcp.mjs',
  'package/cli/main.mjs',
  'package/npm/native/darwin-arm64/external-subagentd',
  'package/npm/native/darwin-arm64/external-subagent-mcp',
  'package/npm/native/darwin-arm64/payload.json',
  'package/plugins/codex/external-subagent/.codex-plugin/plugin.json',
  'package/plugins/codex/external-subagent/.mcp.json',
  'package/launchd/com.external-subagent.daemon.plist.template',
  'package/LICENSE',
];

const listing = spawnSync('tar', ['-tzf', tgz], { encoding: 'utf8' });
if (listing.status !== 0) {
  process.stderr.write(`cannot list tarball: ${listing.stderr}\n`);
  process.exit(1);
}
const entries = listing.stdout.split('\n').filter(Boolean);

let failed = false;
for (const entry of required) {
  if (!entries.includes(entry)) { process.stderr.write(`missing release entry: ${entry}\n`); failed = true; }
}
const forbidden = /(^|\/)(\.agent-work|\.git|target|tests|node_modules|\.npm|workspace)(\/|$)|\.sqlite3$|\.log$|credentials|\.DS_Store$/u;
for (const entry of entries) {
  if (forbidden.test(entry)) { process.stderr.write(`forbidden release material: ${entry}\n`); failed = true; }
}

const packedPackage = JSON.parse(spawnSync('tar', ['-xzf', tgz, '-O', 'package/package.json'], { encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 }).stdout);
const manifestText = spawnSync('tar', ['-xzf', tgz, '-O', 'package/npm/native/darwin-arm64/payload.json'], { encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 }).stdout;
const manifest = JSON.parse(manifestText);
if (manifest.version !== packedPackage.version) {
  process.stderr.write(`payload version ${manifest.version} does not match package version ${packedPackage.version}\n`);
  failed = true;
}

for (const name of ['external-subagentd', 'external-subagent-mcp']) {
  const local = path.join(process.cwd(), 'npm', 'native', 'darwin-arm64', name);
  const file = spawnSync('file', ['-b', local], { encoding: 'utf8' });
  if (!/Mach-O 64-bit executable arm64/u.test(file.stdout)) {
    process.stderr.write(`${name} is not a Mach-O arm64 executable: ${file.stdout.trim()}\n`);
    failed = true;
  }
  const mode = spawnSync('stat', ['-f', '%Lp', local], { encoding: 'utf8' }).stdout.trim();
  if (mode !== '755') { process.stderr.write(`${name} has release mode ${mode}, expected 755\n`); failed = true; }
}

if (failed) process.exit(1);
process.stdout.write(`release tarball static checks passed: ${path.basename(tgz)}\n`);
