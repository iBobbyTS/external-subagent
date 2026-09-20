#!/usr/bin/env node
// Stateless stdio bridge for the shared daemon MCP listener, in node.
//
// Mirrors crates/external-mcp (the native facade) byte for byte in
// behavior: resolve the daemon MCP stream endpoint from
// EXTERNAL_SUBAGENT_SOCKET, connect, and pipe client stdio <-> socket until
// either side reaches EOF.  Hosts that spawn MCP servers inside
// restricted execution contexts (ZCode kills ad-hoc-signed native
// binaries there, while node processes and unix sockets are allowed)
// bind this script through the installing node instead of the native
// binary; see docs/compatibility/zcode.md.
import net from 'node:net';
import path from 'node:path';

const socket = process.env.EXTERNAL_SUBAGENT_SOCKET;
if (!socket || !path.isAbsolute(socket)) {
  console.error('mcp-stdio-bridge: EXTERNAL_SUBAGENT_SOCKET must be an absolute path');
  process.exit(1);
}
// The stable daemon RPC socket keeps its name; MCP is exposed by the
// sibling `.mcp` stream endpoint, except in isolated tests and
// non-standard deployments that point at the MCP endpoint directly.
const endpoint = path.basename(socket) === 'external-subagent.sock'
  ? socket.replace(/\.sock$/u, '.mcp')
  : socket;

const stream = net.createConnection(endpoint);
stream.on('error', (error) => {
  console.error(`mcp-stdio-bridge: failed to connect to daemon MCP socket ${endpoint}: ${error.message}`);
  process.exit(1);
});
stream.on('close', () => process.exit(0));
process.stdin.on('end', () => process.exit(0));
process.stdin.on('error', () => process.exit(0));
process.stdin.pipe(stream);
stream.pipe(process.stdout);
