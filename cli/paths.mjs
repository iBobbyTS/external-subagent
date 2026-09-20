import os from 'node:os';
import path from 'node:path';
import { LAUNCH_AGENT_LABEL, PRODUCT_NAME } from './constants.mjs';
import { PLUGIN_NAME } from './install/layout.mjs';

export function platform() {
  return process.env.EXTERNAL_SUBAGENT_TEST_PLATFORM || process.platform;
}

// State paths derive from PRODUCT_NAME, so the debug variant gets a fully
// parallel tree (its own data directory, database, socket, and logs) and
// never touches the released product's state.
export function productPaths(home = os.homedir()) {
  const data = path.join(home, 'Library', 'Application Support', PRODUCT_NAME);
  return {
    home,
    data,
    config: path.join(data, 'config.json'),
    state: path.join(data, 'install-state.json'),
    hookProvenance: path.join(data, 'zcode-agent-hook-provenance.json'),
    database: path.join(data, `${PRODUCT_NAME}.sqlite3`),
    socket: path.join(data, `${PRODUCT_NAME}.sock`),
    logs: path.join(home, 'Library', 'Logs', PRODUCT_NAME),
    launchAgent: path.join(home, 'Library', 'LaunchAgents', `${LAUNCH_AGENT_LABEL}.plist`),
    zcodeConfig: path.join(home, '.zcode', 'cli', 'config.json'),
    zcodePlugin: path.join(data, 'zcode-plugin', PLUGIN_NAME),
  };
}
