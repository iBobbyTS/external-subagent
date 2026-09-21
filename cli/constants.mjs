// Development variant: EXTERNAL_SUBAGENT_VARIANT=debug selects a fully
// parallel installation (its own binaries, state directory, LaunchAgent
// label, plugin identity, and socket) so a development checkout can run a
// debug build beside the released product without touching it.  The token is
// read once at module load; the debug bin shims set it before importing the
// CLI, so every derived name below is stable for the process lifetime.
const IS_DEBUG_VARIANT = process.env.EXTERNAL_SUBAGENT_VARIANT === 'debug';

export const PRODUCT_NAME = IS_DEBUG_VARIANT ? 'external-subagent-debug' : 'external-subagent';
export const PRODUCT_ID = IS_DEBUG_VARIANT ? 'external_subagent_debug' : 'external_subagent';
export const VERSION = '0.1.1';
export const LAUNCH_AGENT_LABEL = IS_DEBUG_VARIANT ? 'com.external-subagent-debug.daemon' : 'com.external-subagent.daemon';
export const DAEMON_BIN_NAME = IS_DEBUG_VARIANT ? 'external-subagent-debugd' : 'external-subagentd';
export const MCP_BIN_NAME = IS_DEBUG_VARIANT ? 'external-subagent-debug-mcp' : 'external-subagent-mcp';
export const NATIVE_DIR_NAME = IS_DEBUG_VARIANT ? 'native-debug' : 'native';
export const CLI_ENTRY_NAME = IS_DEBUG_VARIANT ? 'external-subagent-debug.mjs' : 'external-subagent.mjs';
export const ZCODE_RUNTIME = '/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs';

export const BUSINESS_COMMANDS = new Set([
  'init', 'hooks', 'status', 'diagnose', 'backup', 'restore', 'start', 'stop',
  'uninstall', 'purge', 'cleanup-legacy', 'install-plugin', 'install-mcp', 'create', 'spawn', 'wait', 'list', 'send',
  'respond', 'cancel', 'result', 'close', 'config', 'agents', 'subagents',
  'observe', 'update', 'reconcile',
]);
