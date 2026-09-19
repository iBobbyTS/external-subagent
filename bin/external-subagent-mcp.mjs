#!/usr/bin/env node

import { spawn } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
// The debug shims set EXTERNAL_SUBAGENT_VARIANT before re-entering this file,
// so the facade resolves to the variant payload staged beside the release one.
const debug = process.env.EXTERNAL_SUBAGENT_VARIANT === 'debug';
const facade = path.join(packageRoot, 'npm', debug ? 'native-debug' : 'native', 'darwin-arm64',
  debug ? 'external-subagent-debug-mcp' : 'external-subagent-mcp');
const child = spawn(facade, process.argv.slice(2), { stdio: 'inherit', env: process.env });
child.on('error', (error) => {
  console.error(error.message);
  process.exitCode = 1;
});
child.on('exit', (code, signal) => {
  process.exitCode = code ?? (signal ? 1 : 0);
});
