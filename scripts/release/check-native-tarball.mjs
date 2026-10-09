#!/usr/bin/env node
// Release pack static checks.  Two modes:
//   check-native-tarball.mjs [package.tgz]   verify a packed tarball (entries,
//                                             payload manifest consistency,
//                                             forbidden development material)
//   check-native-tarball.mjs --staged        verify only the staged payloads
//                                             (npm/native/<platform>) against
//                                             the package version — the publish
//                                             gate, since `npm publish` packs
//                                             into a private temp directory no
//                                             lifecycle script can inspect.
// A released tarball carries every supported platform payload (darwin-arm64
// and linux-x64), so both entry sets and both manifests are required in either
// mode: a tarball missing one platform is rejected.  Version agreement with the
// packed package is enforced for the running host's payload (prepack rebuilds
// only the host payload from source) and, in --staged mode, for every staged
// platform — the merged publish tree where the foreign payload comes from CI's
// other platform job.  Image checks (Mach-O arm64 / ELF x86-64) and the 755
// mode check use the same probes and layout table as cli/install/payload.mjs,
// so this gate cannot drift from what an installed package verifies.
// Without --staged the tarball is resolved from: the explicit argument, the
// npm --pack-destination directory (set during `npm pack` lifecycles), or the
// newest matching tgz in the working directory.  A dry run (npm pack --dry-run)
// or a publish (no local tarball) skips the tarball checks and still runs the
// staged payload checks.
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { elfArch, machoArch } from '../../cli/install/payload.mjs';
import { nativePlatform } from '../../cli/install/layout.mjs';

const stagedOnly = process.argv.includes('--staged');
const explicit = process.argv[2] && !process.argv[2].startsWith('--') ? process.argv[2] : null;
const dryRun = process.env.npm_config_dry_run === 'true';

// Every platform the single release tarball ships, and the image identity each
// payload must carry.  Keeping both lists here (rather than a fixed
// darwin-arm64 path) is what makes the gate dual-platform.
const RELEASE_PLATFORMS = Object.freeze(['darwin-arm64', 'linux-x64']);
const BINARIES = Object.freeze(['external-subagentd', 'external-subagent-mcp']);
const IMAGE_CHECKS = Object.freeze({
  'darwin-arm64': { describe: 'Mach-O arm64 executable', probe: machoArch, arch: 'arm64' },
  'linux-x64': { describe: 'ELF x86-64 executable', probe: elfArch, arch: 'x64' },
});
const hostPlatform = nativePlatform();

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
      'package/plugins/codex/external-subagent/.codex-plugin/plugin.json',
      'package/plugins/codex/external-subagent/.mcp.json',
      'package/launchd/com.external-subagent.daemon.plist.template',
      'package/LICENSE',
    ];
    // Both platform payloads are mandatory: a tarball that ships only one is
    // rejected here (and by the --staged publish gate) before it can be
    // published.
    for (const platform of RELEASE_PLATFORMS) {
      for (const name of [...BINARIES, 'payload.json']) required.push(`package/npm/native/${platform}/${name}`);
    }

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
    for (const platform of RELEASE_PLATFORMS) {
      const manifestText = spawnSync('tar', ['-xzf', tgz, '-O', `package/npm/native/${platform}/payload.json`], { encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 }).stdout;
      let manifest;
      try {
        manifest = JSON.parse(manifestText);
      } catch (error) {
        process.stderr.write(`payload manifest for ${platform} is not valid JSON: ${error.message}\n`);
        failed = true;
        continue;
      }
      if (manifest.product !== 'external-subagent' || manifest.platform !== platform) {
        process.stderr.write(`payload manifest for ${platform} has an inconsistent identity (${manifest.product}/${manifest.platform})\n`);
        failed = true;
      }
      if (platform === hostPlatform && manifest.version !== packedPackage.version) {
        process.stderr.write(`payload version ${manifest.version} (${platform}) does not match package version ${packedPackage.version}\n`);
        failed = true;
      }
    }
  }
}

// Staged payload checks: the staged native binaries and the package version
// must agree right where the tarball is built from.  The publish gate
// (--staged) checks every platform because CI merges both artifacts into one
// tree before packing; a plain `npm pack` checks the running host's platform,
// whose payload prepack rebuilds from source (the foreign platform is required
// to be present by the tarball entry checks above).
const repoPackage = JSON.parse(fs.readFileSync(path.join(process.cwd(), 'package.json'), 'utf8'));
function checkStagedPlatform(platform) {
  const dir = path.join(process.cwd(), 'npm', 'native', platform);
  const manifestPath = path.join(dir, 'payload.json');
  let manifest;
  try {
    manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
  } catch (error) {
    process.stderr.write(`staged ${platform} payload manifest is unavailable: ${error.message}\n`);
    failed = true;
    return;
  }
  if (manifest.version !== repoPackage.version) {
    process.stderr.write(`staged ${platform} payload version ${manifest.version} does not match package version ${repoPackage.version}\n`);
    failed = true;
  }
  if (manifest.product !== 'external-subagent') {
    process.stderr.write(`staged ${platform} payload product ${manifest.product} is not the release payload\n`);
    failed = true;
  }
  if (manifest.platform !== platform) {
    process.stderr.write(`staged ${platform} payload manifest declares ${manifest.platform}\n`);
    failed = true;
  }
  const image = IMAGE_CHECKS[platform];
  for (const name of BINARIES) {
    const local = path.join(dir, name);
    let bytes;
    try {
      bytes = fs.readFileSync(local);
    } catch (error) {
      process.stderr.write(`${platform}/${name} is missing from the staged payload: ${error.code}\n`);
      failed = true;
      continue;
    }
    if (image.probe(bytes) !== image.arch) {
      process.stderr.write(`${platform}/${name} is not a ${image.describe}\n`);
      failed = true;
    }
    const mode = (fs.statSync(local).mode & 0o777).toString(8);
    if (mode !== '755') { process.stderr.write(`${platform}/${name} has release mode ${mode}, expected 755\n`); failed = true; }
  }
}

const stagedPlatforms = stagedOnly ? RELEASE_PLATFORMS : (hostPlatform ? [hostPlatform] : []);
for (const platform of stagedPlatforms) checkStagedPlatform(platform);
if (stagedPlatforms.length === 0) {
  note(`no staged release platform for ${process.platform}-${process.arch}; staged payload checks skipped\n`);
}

if (failed) process.exit(1);
note(stagedOnly
  ? `staged release payload checks passed (${repoPackage.version})\n`
  : `release tarball static checks passed: ${tgz ? path.basename(tgz) : 'staged payload only'}\n`);
