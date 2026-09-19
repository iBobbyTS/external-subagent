#!/usr/bin/env node
// Debug-variant MCP facade entry: selects the debug payload before
// delegating to the shared spawn shim.
process.env.EXTERNAL_SUBAGENT_VARIANT = 'debug';
await import('./external-subagent-mcp.mjs');
