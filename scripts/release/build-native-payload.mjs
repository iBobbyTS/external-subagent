#!/usr/bin/env node
// Build the versioned native payload for the npm package: release binaries
// for external-subagentd and external-subagent-mcp are copied into
// npm/native/darwin-arm64 together with a deterministic payload.json manifest
// (version, platform, per-file bytes/sha256/mode).  Installed-package code
// verifies against this manifest, so it must never contain volatile fields.
//
// --if-stale skips the cargo build when the staged binaries are newer than
// every source input and the manifest already matches the package version.
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const platformDir = path.join(packageRoot, 'npm', 'native', 'darwin-arm64');
const manifestPath = path.join(platformDir, 'payload.json');
const binaries = ['external-subagentd', 'external-subagent-mcp'];
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
  if (manifest.version !== version || manifest.platform !== 'darwin-arm64') return false;
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
if (process.argv.includes('--if-stale') && stagedIsCurrent(version)) {
  process.stdout.write(`native payload already current at ${version}\n`);
  process.exit(0);
}

const build = spawnSync('cargo', ['build', '--release', '-p', 'external-daemon', '-p', 'external-mcp'], {
  cwd: packageRoot, stdio: 'inherit',
});
if (build.status !== 0) {
  process.stderr.write('cargo release build failed\n');
  process.exit(build.status ?? 1);
}

fs.mkdirSync(platformDir, { recursive: true });
const files = [];
for (const name of binaries) {
  const source = path.join(packageRoot, 'target', 'release', name);
  if (!fs.existsSync(source)) {
    process.stderr.write(`release binary missing: ${source}\n`);
    process.exit(1);
  }
  const target = path.join(platformDir, name);
  fs.copyFileSync(source, target);
  fs.chmodSync(target, 0o755);
  const bytes = fs.readFileSync(target);
  files.push({ name, bytes: bytes.length, mode: '755', sha256: sha256(bytes) });
}

const manifest = { schema_version: 1, product: 'external-subagent', version, platform: 'darwin-arm64', files };
fs.writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`, { mode: 0o644 });
process.stdout.write(`native payload ${version} staged: ${files.map((file) => file.name).join(', ')}\n`);
