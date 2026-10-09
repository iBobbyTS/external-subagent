#!/usr/bin/env node

import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { nativePlatform } from '../cli/install/layout.mjs';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

// The debug shims set EXTERNAL_SUBAGENT_VARIANT before re-entering this file,
// so the facade resolves to the variant payload staged beside the release one.
// The platform tuple derives from the running host (falling back to the raw
// process tuple on an unsupported host so the diagnostic names the path it
// tried), matching cli/install/layout.mjs — a linux-x64 install must spawn the
// linux-x64 payload, never a hard-coded macOS directory.
export function mcpFacadePath(
  platform = nativePlatform() ?? `${process.platform}-${process.arch}`,
  debug = process.env.EXTERNAL_SUBAGENT_VARIANT === 'debug',
) {
  return path.join(packageRoot, 'npm', debug ? 'native-debug' : 'native', platform,
    debug ? 'external-subagent-debug-mcp' : 'external-subagent-mcp');
}

export function runMcpFacade() {
  const child = spawn(mcpFacadePath(), process.argv.slice(2), { stdio: 'inherit', env: process.env });
  child.on('error', (error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
  child.on('exit', (code, signal) => {
    process.exitCode = code ?? (signal ? 1 : 0);
  });
}

// Launch only when this shim is the process entry point (directly or through
// an npm bin symlink).  Importing it — the debug shim does so explicitly, and
// the tests import the pure path resolver — must never spawn a facade.
function isEntryPoint() {
  if (process.argv[1] === undefined) return false;
  try { return fs.realpathSync(process.argv[1]) === fs.realpathSync(fileURLToPath(import.meta.url)); }
  catch { return false; }
}

if (isEntryPoint()) runMcpFacade();
