#!/usr/bin/env node
// Build the versioned native payload for the npm package: release binaries
// for external-subagentd and external-subagent-mcp are copied into
// npm/native/darwin-arm64 together with a deterministic payload.json manifest
// (version, platform, per-file bytes/sha256/mode).  Installed-package code
// verifies against this manifest, so it must never contain volatile fields.
//
// `--variant debug` stages the parallel development payload instead: the same
// cargo artifacts are copied under debug names into npm/native-debug/darwin-arm64
// (manifest product external-subagent-debug), and the debug plugin source is
// staged at plugins/codex/external-subagent-debug.  The debug payload is a
// development checkout product and is rejected from release tarballs by
// scripts/release/check-native-tarball.mjs.
//
// --if-stale skips the cargo build when the staged binaries are newer than
// every source input and the manifest already matches the package version.
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { stageDebugPlugin } from './stage-debug-plugin.mjs';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const variantIndex = process.argv.indexOf('--variant');
const variant = variantIndex === -1 ? '' : (process.argv[variantIndex + 1] ?? '');
if (variant !== '' && variant !== 'debug') {
  process.stderr.write(`unknown variant: ${variant} (supported: debug)\n`);
  process.exit(2);
}
const isDebug = variant === 'debug';
// The cargo profile is release by default so prepack/prepublish keep shipping
// release bytes; EXTERNAL_SUBAGENT_CARGO_PROFILE=debug selects the development
// profile without changing the payload/product naming.
const cargoProfile = process.env.EXTERNAL_SUBAGENT_CARGO_PROFILE || 'release';
if (cargoProfile !== 'release' && cargoProfile !== 'debug') {
  process.stderr.write(`unknown cargo profile: ${cargoProfile} (supported: release, debug)\n`);
  process.exit(2);
}
const nativeDirName = isDebug ? 'native-debug' : 'native';
const productName = isDebug ? 'external-subagent-debug' : 'external-subagent';
const binaries = isDebug
  ? [['external-subagentd', 'external-subagent-debugd'], ['external-subagent-mcp', 'external-subagent-debug-mcp']]
  : [['external-subagentd', 'external-subagentd'], ['external-subagent-mcp', 'external-subagent-mcp']];
const platformDir = path.join(packageRoot, 'npm', nativeDirName, 'darwin-arm64');
const manifestPath = path.join(platformDir, 'payload.json');
const sourceRoots = ['crates', 'profiles'].map((dir) => path.join(packageRoot, dir))
  .concat([path.join(packageRoot, 'Cargo.toml'), path.join(packageRoot, 'Cargo.lock')]);

function packageVersion() {
  return JSON.parse(fs.readFileSync(path.join(packageRoot, 'package.json'), 'utf8')).version;
}

function sha256(bytes) {
  return crypto.createHash('sha256').update(bytes).digest('hex');
}

function newestInputMs() {
  let newest = 0;
  const walk = (entry) => {
    const stat = fs.statSync(entry);
    if (stat.isFile()) { newest = Math.max(newest, stat.mtimeMs); return; }
    for (const child of fs.readdirSync(entry)) {
      if (child === 'target' || child.startsWith('.')) continue;
      walk(path.join(entry, child));
    }
  };
  for (const root of sourceRoots) if (fs.existsSync(root)) walk(root);
  return newest;
}

function stagedIsCurrent(version) {
  if (!fs.existsSync(manifestPath)) return false;
  let manifest;
  try { manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8')); } catch { return false; }
  if (manifest.version !== version || manifest.platform !== 'darwin-arm64' || manifest.product !== productName) return false;
  if ((manifest.profile ?? 'release') !== cargoProfile) return false;
  const inputs = newestInputMs();
  for (const file of manifest.files) {
    const target = path.join(platformDir, file.name);
    if (!fs.existsSync(target)) return false;
    const stat = fs.statSync(target);
    if (stat.mtimeMs < inputs) return false;
    if (stat.size !== file.bytes || sha256(fs.readFileSync(target)) !== file.sha256) return false;
  }
  return true;
}

const version = packageVersion();
// Under an npm lifecycle (prepack) stdout belongs to npm's `--json` protocol;
// diagnostics go to stderr there so pack output stays machine-parseable.
const note = (text) => (process.env.npm_lifecycle_event ? process.stderr : process.stdout).write(text);
if (process.argv.includes('--if-stale') && stagedIsCurrent(version)) {
  note(`native payload already current at ${version}${isDebug ? ' (debug)' : ''}${cargoProfile === 'release' ? '' : ` [${cargoProfile}]`}\n`);
  process.exit(0);
}

const cargoArguments = ['build'];
if (cargoProfile === 'release') cargoArguments.push('--release');
cargoArguments.push('-p', 'external-daemon', '-p', 'external-mcp');
const build = spawnSync('cargo', cargoArguments, {
  cwd: packageRoot, stdio: 'inherit',
});
if (build.status !== 0) {
  process.stderr.write(`cargo ${cargoProfile} build failed\n`);
  process.exit(build.status ?? 1);
}

fs.mkdirSync(platformDir, { recursive: true });
const files = [];
for (const [source, name] of binaries) {
  const sourcePath = path.join(packageRoot, 'target', cargoProfile, source);
  if (!fs.existsSync(sourcePath)) {
    process.stderr.write(`${cargoProfile} binary missing: ${sourcePath}\n`);
    process.exit(1);
  }
  const target = path.join(platformDir, name);
  fs.copyFileSync(sourcePath, target);
  fs.chmodSync(target, 0o755);
  const bytes = fs.readFileSync(target);
  files.push({ name, bytes: bytes.length, mode: '755', sha256: sha256(bytes) });
}

const manifest = { schema_version: 1, product: productName, version, platform: 'darwin-arm64' };
// The debug profile is recorded so --if-stale can tell a debug-staged payload
// from a release one; the release manifest stays byte-identical to before.
if (cargoProfile !== 'release') manifest.profile = cargoProfile;
manifest.files = files;
fs.writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`, { mode: 0o644 });
if (isDebug) {
  const stagedPlugin = stageDebugPlugin(packageRoot);
  note(`native payload ${version} (debug) staged: ${files.map((file) => file.name).join(', ')}\nplugin source staged: ${stagedPlugin}\n`);
} else {
  note(`native payload ${version} staged: ${files.map((file) => file.name).join(', ')}${cargoProfile === 'release' ? '' : ` [${cargoProfile}]`}\n`);
}
