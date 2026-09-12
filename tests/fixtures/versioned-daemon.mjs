#!/usr/bin/env node
import fs from 'node:fs';
import net from 'node:net';
const version = process.argv[2] || '0.0.0';
const out = process.argv[3]; const socket = process.argv[4];
const server = net.createServer(c => { c.on('data', () => c.end(JSON.stringify({ ok: true, version }))); });
server.listen(socket, () => fs.writeFileSync(out, JSON.stringify({ pid: process.pid, argv: process.argv.slice(2), version })));
process.on('SIGTERM', () => process.exit(0));
setInterval(() => {}, 1000);
