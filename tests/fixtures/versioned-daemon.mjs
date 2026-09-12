#!/usr/bin/env node
import fs from 'node:fs';
const version = process.argv[2] || '0.0.0';
const out = process.argv[3];
fs.writeFileSync(out, JSON.stringify({ pid: process.pid, argv: process.argv.slice(2), version, rpc: 'healthy' }));
process.on('SIGTERM', () => process.exit(0));
setInterval(() => {}, 1000);
