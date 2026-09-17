export const PRODUCT_NAME = 'external-subagent';
export const PRODUCT_ID = 'external_subagent';
export const VERSION = '0.1.0';
export const LAUNCH_AGENT_LABEL = 'com.external-subagent.daemon';
export const ZCODE_RUNTIME = '/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs';

export const BUSINESS_COMMANDS = new Set([
  'init', 'hooks', 'status', 'diagnose', 'backup', 'restore', 'start', 'stop',
  'uninstall', 'purge', 'cleanup-legacy', 'install-plugin', 'install-mcp', 'create', 'spawn', 'wait', 'list', 'send',
  'respond', 'cancel', 'result', 'close', 'config', 'agents', 'subagents',
  'observe', 'update', 'reconcile',
]);
