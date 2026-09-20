import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';
const home = process.env.DSH_HOME;
const log = home && path.join(home, 'probe.jsonl');
const record = (value) => fs.appendFileSync(log, JSON.stringify(value) + '\n');
if (process.argv.includes('--version')) {
  console.log('0.1.5-rc.1');
  process.exit(0);
}
const patch = process.argv[process.argv.indexOf('--patch') + 1];
const ordinarySpawn = process.env.S05_DSH_SPAWN_FIXTURE === '1' && process.env.DSH_PERMISSION_MODE === 'workspace-write';
if (!ordinarySpawn && (!path.isAbsolute(patch) || !fs.readFileSync(patch, 'utf8').includes('sandbox-policy'))) process.exit(2);
// A compiled source-tree resource must never satisfy this fixture.
if (!ordinarySpawn && !path.basename(path.dirname(patch)).startsWith('external-dsh-hi-')) process.exit(3);
record({ kind: process.argv.includes('--dump-config') ? 'dump' : 'acp', cwd: process.cwd(), home, patch, mode: process.env.DSH_PERMISSION_MODE });
if (process.argv.includes('--dump-config')) {
  if (ordinarySpawn) {
    console.log(JSON.stringify([
      { id: 'sandbox-policy', name: '@deepseek-ai/dsh-sandbox-policy', config: { mode: 'workspace-write', workspaceRoot: process.cwd() } },
      { id: 'approval', name: '@deepseek-ai/dsh-user-approval', config: { policy: 'ask' } },
      { id: 'permission', name: '@deepseek-ai/dsh-permission-presets', config: { presets: { 'workspace-write': { sandbox: 'workspace-write', approval: 'ask' } } } },
      { id: 'sandbox', name: '@deepseek-ai/dsh-sandbox-local' },
      { id: 'fs-sandbox', name: '@deepseek-ai/dsh-fs-sandbox' },
      { id: 'acp', name: '@deepseek-ai/dsh-acp' },
      { id: 'acp-app-startup', name: '@deepseek-ai/dsh-acp-app' },
      { id: 'bash-sandbox', name: '@deepseek-ai/dsh-bash-sandbox', config: { timeoutMs: 60000 } },
      { id: 'pwsh-sandbox', name: '@deepseek-ai/dsh-pwsh-sandbox', disabled: true },
    ]));
    process.exit(0);
  }
  if (fs.existsSync(path.join(home, 'policy-drift'))) {
    // R0 counterexample: sandbox-policy enabled without config and approval
    // disabled; the strict preflight must refuse this dump.
    console.log('- id: sandbox-policy\n  disabled: false\n- id: approval\n  disabled: true');
  } else {
    console.log('- id: sandbox-policy\n  config:\n    mode: read-only\n- id: approval\n  config:\n    policy: ask');
  }
  if (fs.existsSync(path.join(home, 'unknown-tool'))) console.log('- id: unknown-write-tool');
  process.exit(0);
}
const lines = readline.createInterface({ input: process.stdin });
lines.on('line', (line) => {
  const request = JSON.parse(line);
  record({ method: request.method });
  let result = {};
  if (request.method === 'initialize') result = { protocolVersion: 1, capabilities: { models: true, cancel: true, permission: true } };
  if (request.method === 'session/new') result = { sessionId: 'hi-session' };
  if (request.method === 'session/prompt') {
    if (ordinarySpawn) console.log(JSON.stringify({ jsonrpc: '2.0', method: 'session/update', params: {
      sessionId: 'hi-session', update: { type: 'agent_message', messageId: 'fixture-message', content: [{ type: 'text', text: 'fixture complete' }] },
    } }));
    result = { stopReason: 'end_turn', ...(ordinarySpawn ? { messageId: 'fixture-message' } : {}) };
  }
  if (request.id !== undefined) console.log(JSON.stringify({ jsonrpc: '2.0', id: request.id, result }));
});
