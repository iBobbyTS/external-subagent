#!/usr/bin/env node
// Release pack static checks.  Two modes:
//   check-native-tarball.mjs [package.tgz]   verify a packed tarball (entries,
//                                             payload manifest consistency,
//                                             forbidden development material)
//   check-native-tarball.mjs --staged        verify only the staged payload
//                                             (npm/native/darwin-arm64) against
//                                             the package version — the publish
//                                             gate, since `npm publish` packs
//                                             into a private temp directory no
//                                             lifecycle script can inspect.
// Without --staged the tarball is resolved from: the explicit argument, the
// npm --pack-destination directory (set during `npm pack` lifecycles), or the
// newest matching tgz in the working directory.  A dry run (npm pack --dry-run)
// or a publish (no local tarball) skips the tarball checks and still runs the
// staged payload checks.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

const stagedOnly = process.argv.includes('--staged');
const explicit = process.argv[2] && !process.argv[2].startsWith('--') ? process.argv[2] : null;
const dryRun = process.env.npm_config_dry_run === 'true';

function findTarball() {
  if (explicit) return explicit;
  const candidates = [];
  const packDestination = process.env.npm_config_pack_destination;
  if (packDestination) candidates.push(packDestination);
  candidates.push(process.cwd());
  for (const dir of candidates) {
    const staged = fs.readdirSync(dir).filter((name) => /^external-subagent-\d+\.\d+\.\d+\.tgz$/.test(name)).sort();
    if (staged.length > 0) return path.join(dir, staged[staged.length - 1]);
  }
  return null;
}

let failed = false;
let tgz = null;
// Under an npm lifecycle (postpack/prepublishOnly) stdout belongs to npm's
// `--json` protocol; diagnostics go to stderr there.
const note = (text) => (process.env.npm_lifecycle_event ? process.stderr : process.stdout).write(text);
if (!stagedOnly) {
  tgz = findTarball();
  if (tgz === null && !dryRun && explicit === null) {
    // `npm publish` packs into a private temp directory: no tarball to check
    // here.  The prepublishOnly staged gate covers the payload; the tarball
    // entry checks run on every `npm pack`.
    note('no local packed tarball to check (publish or explicit run without one); running staged payload checks only\n');
  }
  if (tgz !== null) {
    const required = [
      'package/package.json',
      'package/bin/external-subagent.mjs',
      'package/bin/external-subagent-mcp.mjs',
      'package/bin/external-subagent-debug.mjs',
      'package/bin/external-subagent-debug-mcp.mjs',
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

    for (const entry of required) {
      if (!entries.includes(entry)) { process.stderr.write(`missing release entry: ${entry}\n`); failed = true; }
    }
    // The debug payload and debug plugin source are development-checkout build
    // artifacts; a tarball carrying them would ship ~10 MB of bytes no
    // installed release consumer can verify.
    const forbidden = /(^|\/)(\.agent-work|\.git|target|tests|node_modules|\.npm|workspace)(\/|$)|(^|\/)npm\/native-debug(\/|$)|(^|\/)plugins\/codex\/external-subagent-debug(\/|$)|\.sqlite3$|\.log$|credentials|\.DS_Store$/u;
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
  }
}

// Staged payload checks run in every mode (pack, publish, dry run): the
// package version and the staged native binaries must agree right where the
// tarball is built from.
const repoPackage = JSON.parse(fs.readFileSync(path.join(process.cwd(), 'package.json'), 'utf8'));
const stagedManifest = JSON.parse(fs.readFileSync(path.join(process.cwd(), 'npm', 'native', 'darwin-arm64', 'payload.json'), 'utf8'));
if (stagedManifest.version !== repoPackage.version) {
  process.stderr.write(`staged payload version ${stagedManifest.version} does not match package version ${repoPackage.version}\n`);
  failed = true;
}
if (stagedManifest.product !== 'external-subagent') {
  process.stderr.write(`staged payload product ${stagedManifest.product} is not the release payload\n`);
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
note(stagedOnly
  ? `staged release payload checks passed (${repoPackage.version})\n`
  : `release tarball static checks passed: ${tgz ? path.basename(tgz) : 'staged payload only'}\n`);
