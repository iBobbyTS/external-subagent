#!/usr/bin/env node
// Debug-variant CLI entry: selects the parallel external-subagent-debug
// installation (own payload, state directory, LaunchAgent, plugin identity,
// and socket) before the CLI derives any name from the variant token.  The
// release installation is never touched by commands run through this shim.
process.env.EXTERNAL_SUBAGENT_VARIANT = 'debug';
const { main } = await import('../cli/main.mjs');

main(process.argv.slice(2)).catch((error) => {
  const code = typeof error?.code === 'string' ? error.code : 'INTERNAL_ERROR';
  process.stderr.write(`${JSON.stringify({ ok: false, error: { code, message: error.message, ...(error.agentId ? { agent_id: error.agentId } : {}), ...(Number.isInteger(error.promptCount) ? { prompt_count: error.promptCount } : {}) } })}\n`);
  process.exitCode = Number.isInteger(error?.exitCode) ? error.exitCode : 1;
});
