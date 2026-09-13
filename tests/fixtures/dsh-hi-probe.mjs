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
if (!path.isAbsolute(patch) || !fs.readFileSync(patch, 'utf8').includes('sandbox-policy')) process.exit(2);
// A compiled source-tree resource must never satisfy this fixture.
if (!path.basename(path.dirname(patch)).startsWith('external-dsh-hi-')) process.exit(3);
record({ kind: process.argv.includes('--dump-config') ? 'dump' : 'acp', cwd: process.cwd(), home, patch, mode: process.env.DSH_PERMISSION_MODE });
if (process.argv.includes('--dump-config')) {
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
  if (request.method === 'session/prompt') result = { stopReason: 'end_turn' };
  if (request.id !== undefined) console.log(JSON.stringify({ jsonrpc: '2.0', id: request.id, result }));
});
