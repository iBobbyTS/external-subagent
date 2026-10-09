// Development variant: EXTERNAL_SUBAGENT_VARIANT=debug selects a fully
// parallel installation (its own binaries, state directory, LaunchAgent
// label, plugin identity, and socket) so a development checkout can run a
// debug build beside the released product without touching it.  The token is
// read once at module load; the debug bin shims set it before importing the
// CLI, so every derived name below is stable for the process lifetime.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const IS_DEBUG_VARIANT = process.env.EXTERNAL_SUBAGENT_VARIANT === 'debug';

export const PRODUCT_NAME = IS_DEBUG_VARIANT ? 'external-subagent-debug' : 'external-subagent';
export const PRODUCT_ID = IS_DEBUG_VARIANT ? 'external_subagent_debug' : 'external_subagent';
export const VERSION = '0.4.1';
export const LAUNCH_AGENT_LABEL = IS_DEBUG_VARIANT ? 'com.external-subagent-debug.daemon' : 'com.external-subagent.daemon';
export const DAEMON_BIN_NAME = IS_DEBUG_VARIANT ? 'external-subagent-debugd' : 'external-subagentd';
export const MCP_BIN_NAME = IS_DEBUG_VARIANT ? 'external-subagent-debug-mcp' : 'external-subagent-mcp';
export const NATIVE_DIR_NAME = IS_DEBUG_VARIANT ? 'native-debug' : 'native';
export const CLI_ENTRY_NAME = IS_DEBUG_VARIANT ? 'external-subagent-debug.mjs' : 'external-subagent.mjs';
export const ZCODE_RUNTIME = '/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs';

// The zcode runtime the CLI pins for the daemon, resolved per installation:
// explicit configuration first, then the platform's conventional location,
// and — when neither yields a file — the packaged macOS constant, reported
// honestly as an absent pinned location (presence is the caller's
// `fs.existsSync` observation, never an assertion).
//   explicit   ZCODE_RUNTIME_PATH, the same variable the daemon itself
//              resolves (external-subagentd --runtime / ZCODE_RUNTIME_PATH).
//              A configured-but-absent path is returned verbatim; forwarding
//              stays presence-gated so the daemon never starts against a
//              --runtime that is not a regular file.
//   darwin     the app bundle resource (frozen bytes above).
//   linux      the ZCode server runtime the desktop's attached-remote flow
//              deploys under `~/.zcode/server` (`agents/glm/zcode.cjs`) —
//              the same default root the official `zcode-agent` launcher
//              resolves (OBSERVED 2026-10-09; docs/compatibility/zcode.md).
//              The deployment's ZCODE_SERVER_RUNTIME_ROOT variable is
//              deliberately NOT consulted: service generation must stay
//              independent of the interactive shell environment (the same
//              design the fixed unit PATH follows), and the product home
//              already owns `~/.zcode`.
export function zcodeRuntimePath(home = os.homedir(), env = process.env) {
  const configured = env.ZCODE_RUNTIME_PATH;
  if (typeof configured === 'string' && configured !== '') return configured;
  // The process-platform seam, the same expression cli/paths.mjs owns;
  // duplicated here because paths.mjs imports this module (no import cycle).
  if ((env.EXTERNAL_SUBAGENT_TEST_PLATFORM || process.platform) === 'darwin') return ZCODE_RUNTIME;
  const conventional = path.join(home, '.zcode', 'server', 'agents', 'glm', 'zcode.cjs');
  return fs.existsSync(conventional) ? conventional : ZCODE_RUNTIME;
}

export const BUSINESS_COMMANDS = new Set([
  'init', 'hooks', 'status', 'diagnose', 'backup', 'restore', 'start', 'stop',
  'uninstall', 'purge', 'install-plugin', 'install-mcp', 'create', 'spawn', 'wait', 'list', 'send',
  'respond', 'cancel', 'result', 'close', 'config', 'agents', 'subagents',
  'observe', 'update', 'reconcile',
]);
