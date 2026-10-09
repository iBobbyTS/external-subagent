import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { DAEMON_BIN_NAME, MCP_BIN_NAME, NATIVE_DIR_NAME, PRODUCT_NAME, VERSION } from '../constants.mjs';

// Package and payload layout is the single source of truth for where the
// staged npm artifact keeps its managed entry points.  The MCP binding and
// the LaunchAgent reference absolute paths below this root, never a shell or
// GUI PATH lookup.  Identity names derive from the variant token in
// constants.mjs, so the debug variant resolves its own payload directory,
// binary names, and plugin source without a parallel code path.
export const PLUGIN_NAME = PRODUCT_NAME;
// Every platform tuple that ships a prebuilt payload maps to its own staged
// directory name.  The tuple is the package's platform identity: the payload
// directory, the manifest `platform` field, and the image check in
// payload.mjs all key off the same value.  A tuple outside this table has no
// payload and is rejected by the installer, never silently downgraded.
// NATIVE_PLATFORM stays the reference platform (the released macOS identity).
export const NATIVE_PLATFORMS = Object.freeze(['darwin-arm64', 'linux-x64']);
export const NATIVE_PLATFORM = NATIVE_PLATFORMS[0];
export const NATIVE_BINARIES = Object.freeze([DAEMON_BIN_NAME, MCP_BIN_NAME]);

export function packageRoot() {
  return path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
}

export function nativePlatform(platform = process.platform, arch = process.arch) {
  const tuple = `${platform}-${arch}`;
  return NATIVE_PLATFORMS.includes(tuple) ? tuple : null;
}

export function nativePayloadDir(platform = nativePlatform()) {
  if (!NATIVE_PLATFORMS.includes(platform)) return null;
  return path.join(packageRoot(), 'npm', NATIVE_DIR_NAME, platform);
}

export function nativeBinary(name, platform = nativePlatform()) {
  const dir = nativePayloadDir(platform);
  if (!dir || !NATIVE_BINARIES.includes(name)) return null;
  return path.join(dir, name);
}

export function payloadManifestPath(platform = nativePlatform()) {
  const dir = nativePayloadDir(platform);
  return dir === null ? null : path.join(dir, 'payload.json');
}

export function pluginSourceRoot() {
  return path.join(packageRoot(), 'plugins', 'codex', PLUGIN_NAME);
}

// The npm package version and the in-repo CLI version must agree; payload
// verification rejects any drift between the two.
export function packageVersion() {
  const manifest = JSON.parse(fs.readFileSync(path.join(packageRoot(), 'package.json'), 'utf8'));
  return manifest.version;
}

export function cliVersion() {
  return VERSION;
}
