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
const patchIndex = process.argv.indexOf('--patch');
const patch = patchIndex < 0 ? undefined : process.argv[patchIndex + 1];
const patchText = typeof patch === 'string' && fs.existsSync(patch) ? fs.readFileSync(patch, 'utf8') : null;
const mode = process.env.DSH_PERMISSION_MODE;
const fixture = process.env.S05_DSH_SPAWN_FIXTURE === '1';
const dumpConfig = process.argv.includes('--dump-config');
// Legacy build spawn: the fixture-managed spawn with no patch.
const ordinarySpawn = fixture && mode === 'workspace-write' && typeof patch !== 'string';
// Caller write manifest: the same managed spawn carrying the generated
// manifest-build patch (the guard tree plus the manifest row).
const manifestSpawn = fixture && mode === 'workspace-write' && typeof patch === 'string';
const managedSpawn = ordinarySpawn || manifestSpawn;
// A strict probe patch is a real fixture-owned TempDir; a compiled
// source-tree resource must never satisfy this fixture.
if (!managedSpawn) {
  if (!path.isAbsolute(patch) || !(patchText ?? '').includes('sandbox-policy')) process.exit(2);
  if (!path.basename(path.dirname(patch)).startsWith('external-dsh-hi-')) process.exit(3);
}
// The generated manifest-build patch carries exactly one insert row (the
// guard plugin) and the caller manifest. Parse both back out so the
// `--dump-config` branch reports exactly what the daemon preflights.
function readManifestPatch(text) {
  const unquote = (value) => {
    const trimmed = value.trim();
    const quote = trimmed[0];
    if ((quote === '"' || quote === "'") && trimmed.length > 1 && trimmed.endsWith(quote)) return trimmed.slice(1, -1);
    return trimmed;
  };
  const lines = String(text).split('\n');
  const nameLine = lines.find((line) => line.trim().startsWith('- name:'));
  const guard = nameLine ? unquote(nameLine.trim().slice('- name:'.length)) : null;
  const manifestIndex = lines.findIndex((line) => line.trim() === 'manifest:');
  const manifest = [];
  if (manifestIndex !== -1) {
    for (let index = manifestIndex + 1; index < lines.length; index += 1) {
      const match = /^\s*-\s?(.*)$/u.exec(lines[index]);
      if (!match) break;
      manifest.push(unquote(match[1]));
    }
  }
  return { guard, manifest };
}
// The materialized plugin tree lives next to the patch for the task's
// lifetime; recording the link lets the install test assert the nested
// dsh-fs resolution without racing the TempDir reap.
function pluginFsLink(patchPath) {
  if (typeof patchPath !== 'string') return null;
  const link = path.join(path.dirname(patchPath), 'dsh-write-guard', 'node_modules', '@deepseek-ai', 'dsh-fs');
  try {
    return { path: link, symbolic: fs.lstatSync(link).isSymbolicLink(), target: fs.realpathSync(link) };
  } catch {
    return { path: link, symbolic: false, target: null };
  }
}
record({
  kind: dumpConfig ? 'dump' : 'acp',
  cwd: process.cwd(),
  home,
  patch: patch ?? null,
  patchText,
  fsLink: manifestSpawn ? pluginFsLink(patch) : null,
  mode,
});
if (dumpConfig) {
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
  if (manifestSpawn) {
    // Full manifest-build dump, aligned with S02 `validate_manifest_build_dump`:
    // the guard insert (its name normalized to a `file://` URL) carries the
    // caller manifest verbatim, tool-fs stays enabled, every other strict-plan
    // tool stays disabled, and the sandbox/approval/permission controls match
    // the build dump. Anything missing would make the real preflight refuse.
    const { guard, manifest } = readManifestPatch(patchText);
    const disabled = [
      'tool-bash', 'tool-pwsh', 'tool-jobs', 'tool-skill', 'tool-subagent-control',
      'tool-subagent-list-agents', 'tool-subagent', 'tool-subagent-fork', 'subagent',
      'tool-workflow', 'tool-goal', 'tool-ralph', 'skill-filesystem', 'workflow-worker-thread',
      'goal-round-driver', 'subagent-spawn-in-process', 'subagent-fork-in-process',
    ];
    console.log(JSON.stringify([
      { id: 'sandbox-policy', name: '@deepseek-ai/dsh-sandbox-policy', config: { mode: 'workspace-write' } },
      { id: 'approval', name: '@deepseek-ai/dsh-user-approval', config: { policy: 'ask' } },
      { id: 'permission', name: '@deepseek-ai/dsh-permission-presets', config: { presets: { 'workspace-write': { sandbox: 'workspace-write', approval: 'ask' } } } },
      { id: 'sandbox', name: '@deepseek-ai/dsh-sandbox-local' },
      { id: 'fs-sandbox', name: '@deepseek-ai/dsh-fs-sandbox' },
      { id: 'acp', name: '@deepseek-ai/dsh-acp' },
      { id: 'acp-app-startup', name: '@deepseek-ai/dsh-acp-app' },
      { id: 'bash-sandbox', name: '@deepseek-ai/dsh-bash-sandbox', config: { timeoutMs: 60000 } },
      { id: 'pwsh-sandbox', name: '@deepseek-ai/dsh-pwsh-sandbox', disabled: true },
      { id: 'tool-fs', name: '@deepseek-ai/dsh-tool-fs' },
      ...disabled.map((id) => ({ id, disabled: true })),
      { name: `file://${guard}`, config: { manifest } },
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
    if (managedSpawn) console.log(JSON.stringify({ jsonrpc: '2.0', method: 'session/update', params: {
      sessionId: 'hi-session', update: { type: 'agent_message', messageId: 'fixture-message', content: [{ type: 'text', text: 'fixture complete' }] },
    } }));
    result = { stopReason: 'end_turn', ...(managedSpawn ? { messageId: 'fixture-message' } : {}) };
  }
  if (request.id !== undefined) console.log(JSON.stringify({ jsonrpc: '2.0', id: request.id, result }));
});
