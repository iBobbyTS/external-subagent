import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { agentsCommand } from '../../cli/commands/agents.mjs';
import { parseAgentsArgs } from '../../cli/commands/agents.mjs';
import { configCommand, parseConfigArgs } from '../../cli/commands/config.mjs';
import { readConfig } from '../../cli/config/read.mjs';
import { parseSpawnArgs, prepareSpawnInput } from '../../cli/commands/tasks.mjs';
import { productPaths } from '../../cli/paths.mjs';
import { launchAgentPlist } from '../../cli/install/service-macos.mjs';

function fixture() { const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-agents-')); return { home, paths: productPaths(home) }; }

test('config has no default and lists layered agent support', async () => {
  const { paths } = fixture();
  const listed = await agentsCommand(paths);
  assert.equal(listed.default_agent, null);
  assert.deepEqual(listed.agents.map((agent) => [agent.agent, agent.spawn_supported]), [['zcode', true], ['dsh', false], ['codex', false]]);
});

test('zcode model is rejected before prompt and dsh remains discovery-only', () => {
  const { paths } = fixture();
  assert.throws(() => configCommand(paths, { operation: 'set', patch: { default_agent: 'zcode', agents: { zcode: { default_model: 'glm-4' } } } }), (error) => error.code === 'model_selection_unsupported');
  const config = configCommand(paths, { operation: 'set', patch: { agents: { dsh: { enabled: true } } } }).config;
  assert.equal(config.agents.dsh.enabled, true);
  assert.equal(config.agents.dsh.spawn_supported, false);
});

test('explicit dsh spawn support survives config validation', () => {
  const { paths } = fixture();
  const config = configCommand(paths, { operation: 'set', patch: { agents: { dsh: { enabled: true, spawn_supported: true } } } }).config;
  assert.equal(config.agents.dsh.enabled, true);
  assert.equal(config.agents.dsh.spawn_supported, true);
  assert.equal(readConfig(paths.config).agents.dsh.spawn_supported, true);
});

test('LaunchAgent captures configured DSH runtime and home for GUI services', () => {
  const { paths } = fixture();
  configCommand(paths, { operation: 'set', patch: { agents: { dsh: {
    runtime_path: '/opt/dsh/runtime with spaces', home: '/var/lib/dsh profile', profile: 'acp', version: '0.1.5-rc.1',
  } } } });
  const plist = launchAgentPlist(paths).toString('utf8');
  assert.match(plist, /<key>DSH_RUNTIME_PATH<\/key><string>\/opt\/dsh\/runtime with spaces<\/string>/u);
  assert.match(plist, /<key>DSH_HOME<\/key><string>\/var\/lib\/dsh profile<\/string>/u);
  assert.match(plist, /<key>DSH_PROFILE<\/key><string>acp<\/string>/u);
});

test('codex config persists runtime, home, and default model through every path', () => {
  const { paths } = fixture();
  const config = configCommand(paths, { operation: 'set', patch: { agents: { codex: {
    enabled: true, spawn_supported: true, default_model: 'gpt-5.6-terra',
    runtime_path: '/opt/homebrew/bin/codex', home: '/Users/fixture/.codex-multi-2',
  } } } }).config;
  assert.equal(config.agents.codex.enabled, true);
  assert.equal(config.agents.codex.spawn_supported, true);
  assert.equal(config.agents.codex.default_model, 'gpt-5.6-terra');
  // Human key paths and unset restore the disabled defaults.
  configCommand(paths, parseConfigArgs(['set', 'agents.codex.home', '/tmp/other-home']));
  assert.equal(readConfig(paths.config).agents.codex.home, '/tmp/other-home');
  assert.deepEqual(
    configCommand(paths, parseConfigArgs(['unset', 'agents.codex.home'])).config.agents.codex,
    { enabled: true, spawn_supported: true, default_model: 'gpt-5.6-terra', runtime_path: '/opt/homebrew/bin/codex', home: null, profile: null, version: null },
  );
  // Unknown agents stay rejected.
  assert.throws(() => parseConfigArgs(['set', 'agents.other.enabled', 'true']), (error) => error.code === 'INVALID_ARGUMENT');
});

test('LaunchAgent forwards the persisted Codex runtime and home exactly', () => {
  const { paths } = fixture();
  configCommand(paths, { operation: 'set', patch: { agents: { codex: {
    runtime_path: '/opt/homebrew/bin/codex with space', home: '/Users/fixture/.codex-multi & 2',
  } } } });
  const plist = launchAgentPlist(paths).toString('utf8');
  assert.match(plist, /<key>CODEX_RUNTIME_PATH<\/key><string>\/opt\/homebrew\/bin\/codex with space<\/string>/u);
  assert.match(plist, /<key>CODEX_HOME<\/key><string>\/Users\/fixture\/\.codex-multi &amp; 2<\/string>/u);
  // Without persisted values the plist forwards neither entry.
  const bare = fixture();
  const barePlist = launchAgentPlist(bare.paths).toString('utf8');
  assert.doesNotMatch(barePlist, /CODEX_RUNTIME_PATH/u);
  assert.doesNotMatch(barePlist, /CODEX_HOME/u);
});

test('config writes a revision and keeps existing task snapshots independent', () => {
  const { paths } = fixture();
  const first = configCommand(paths, { operation: 'set', patch: { default_agent: 'zcode' } }).config;
  const second = configCommand(paths, { operation: 'set', patch: { agents: { dsh: { enabled: true } } } }).config;
  assert.equal(first.revision, 1);
  assert.equal(second.revision, 2);
  assert.equal(first.default_agent, 'zcode');
  assert.equal(second.agents.dsh.spawn_supported, false);
});

test('per-agent patch preserves fields outside the patch', () => {
  const { paths } = fixture();
  const disabled = configCommand(paths, { operation: 'set', patch: { agents: { dsh: { enabled: false, default_model: 'catalog-token' } } } }).config;
  const updated = configCommand(paths, { operation: 'set', patch: { agents: { dsh: { default_model: 'next-token' } } } }).config;
  assert.equal(disabled.agents.dsh.enabled, false);
  assert.equal(updated.agents.dsh.enabled, false);
  assert.equal(updated.agents.dsh.default_model, 'next-token');
  assert.equal(updated.agents.dsh.spawn_supported, false);
});

test('human config forms parse typed values and get one key', () => {
  const { paths } = fixture();
  configCommand(paths, parseConfigArgs(['set', 'default_agent', 'zcode']));
  assert.deepEqual(configCommand(paths, parseConfigArgs(['get', 'default_agent'])), { key: 'default_agent', value: 'zcode', revision: 1 });
  configCommand(paths, parseConfigArgs(['set', 'agents.dsh.enabled', 'true']));
  assert.equal(readConfig(paths.config).agents.dsh.enabled, true);
  assert.throws(() => parseConfigArgs(['set', 'agents.other.enabled', 'true']), (error) => error.code === 'INVALID_ARGUMENT');
});

test('config show and unset use the same validated revision path', () => {
  const { paths } = fixture();
  configCommand(paths, parseConfigArgs(['set', 'default_agent', 'zcode']));
  const shown = configCommand(paths, parseConfigArgs(['show']));
  assert.equal(shown.config.default_agent, 'zcode');
  assert.equal(shown.config.revision, 1);
  const unset = configCommand(paths, parseConfigArgs(['unset', 'default_agent']));
  assert.equal(unset.config.default_agent, null);
  assert.equal(unset.config.revision, 2);
  assert.equal(configCommand(paths, parseConfigArgs(['unset', 'agents.zcode.enabled'])).config.agents.zcode.enabled, true);
  assert.throws(() => parseConfigArgs(['unset', 'revision']), /unsupported config key/u);
});

test('config set cannot override revision or merge an agent null patch', () => {
  const { paths } = fixture();
  assert.throws(() => configCommand(paths, { operation: 'set', patch: { revision: 999, default_agent: 'zcode' } }), /managed by the writer/u);
  assert.throws(() => configCommand(paths, { operation: 'set', patch: { agents: { zcode: null } } }), /must be an object/u);
  assert.throws(() => configCommand(paths, { operation: 'set', patch: { agents: { zcode: ['bad'] } } }), /must be an object/u);
  const first = configCommand(paths, { operation: 'set', patch: { default_agent: 'zcode' } }).config;
  assert.equal(first.revision, 1);
  assert.throws(() => configCommand(paths, { operation: 'set', patch: { revision: first.revision } }), /managed by the writer/u);
});

test('JSON unset accepts only a supported key and cannot reuse revision or null agent patches', () => {
  const { paths } = fixture();
  const first = configCommand(paths, { operation: 'set', patch: { default_agent: 'zcode' } }).config;
  assert.equal(first.revision, 1);
  assert.throws(() => configCommand(paths, { operation: 'unset', patch: { revision: 0, default_agent: null } }), /exactly one supported key/u);
  assert.throws(() => configCommand(paths, { operation: 'unset', patch: { agents: { zcode: null } } }), /exactly one supported key/u);
  const unset = configCommand(paths, { operation: 'unset', key: 'default_agent' }).config;
  assert.equal(unset.default_agent, null);
  assert.equal(unset.revision, 2);
});

test('concurrent config writers serialize revision and preserve both updates', async () => {
  const { paths } = fixture();
  const script = `import { configCommand } from './cli/commands/config.mjs'; configCommand(${JSON.stringify(paths)}, JSON.parse(process.argv[1]));`;
  const run = (patch) => new Promise((resolve, reject) => {
    const child = spawn(process.execPath, ['--input-type=module', '-e', script, JSON.stringify({ operation: 'set', patch })], { cwd: path.resolve('.') });
    child.on('error', reject); child.on('close', (code) => code === 0 ? resolve() : reject(new Error(`writer exited ${code}`)));
  });
  await Promise.all([run({ default_agent: 'zcode' }), run({ agents: { dsh: { enabled: true } } })]);
  const final = readConfig(paths.config);
  assert.equal(final.revision, 2);
  assert.equal(final.default_agent, 'zcode');
  assert.equal(final.agents.dsh.enabled, true);
});

test('config writer recovers a stale lock from a dead owner', () => {
  const { paths } = fixture();
  fs.mkdirSync(path.dirname(paths.config), { recursive: true });
  fs.writeFileSync(paths.config, JSON.stringify({ schema_version: 1, revision: 4, default_agent: null, agents: { zcode: { enabled: true, spawn_supported: true, default_model: null }, dsh: { enabled: false, spawn_supported: false, default_model: null } } }));
  fs.writeFileSync(`${paths.config}.lock`, '999999\n');
  const result = configCommand(paths, { operation: 'set', patch: { default_agent: 'zcode' } }).config;
  assert.equal(result.revision, 5);
  assert.equal(result.default_agent, 'zcode');
});

test('unknown config fields and operations fail closed', () => {
  const { paths } = fixture();
  assert.throws(() => configCommand(paths, { operation: 'wat' }), (error) => error.code === 'INVALID_ARGUMENT');
  assert.throws(() => configCommand(paths, { operation: 'get', surprise: true }), (error) => error.code === 'INVALID_ARGUMENT');
  assert.throws(() => configCommand(paths, { operation: 'set', patch: { agents: { other: { enabled: true } } } }), (error) => error.code === 'CONFIG_INVALID');
  assert.throws(() => configCommand(paths, { operation: 'set', patch: { agents: { zcode: { mystery: true } } } }), (error) => error.code === 'CONFIG_INVALID');
});

test('product runtime paths survive agent config updates', () => {
  const { paths } = fixture();
  fs.mkdirSync(path.dirname(paths.config), { recursive: true });
  fs.writeFileSync(paths.config, JSON.stringify({ schema_version: 1, runtime: '/runtime', database: '/database', socket: '/socket' }));
  const updated = configCommand(paths, { operation: 'set', patch: { default_agent: 'zcode' } }).config;
  assert.equal(updated.runtime, '/runtime');
  assert.equal(updated.database, '/database');
  assert.equal(updated.socket, '/socket');
});

test('agents human operations are strict and unsupported actions are explicit', async () => {
  const { paths } = fixture();
  assert.deepEqual(parseAgentsArgs([]), { operation: 'list' });
  assert.deepEqual(parseAgentsArgs(['status', 'zcode']), { operation: 'status', agent: 'zcode' });
  assert.deepEqual(parseAgentsArgs(['probe', 'zcode', '--hi', '--workspace', '/workspace', '--home', '/home']), {
    operation: 'probe', agent: 'zcode', through: 'hi', workspace: '/workspace', home: '/home',
  });
  assert.deepEqual(parseAgentsArgs(['models', 'dsh', '--workspace', '/workspace', '--home', '/home']), {
    operation: 'models', agent: 'dsh', workspace: '/workspace', home: '/home',
  });
  assert.deepEqual(parseAgentsArgs(['probe', 'future-provider', '--local']), { operation: 'probe', agent: 'future-provider', through: 'local' });
  const probed = await agentsCommand(paths, { operation: 'probe', agent: 'zcode', through: 'hi', workspace: '/workspace' }, {
    socket: '/socket',
    callDaemon: async (socket, command, input) => {
      assert.equal(socket, '/socket');
      assert.equal(command, 'agent-probe');
      assert.deepEqual(input, { agent: 'zcode', through: 'hi', scope: { workspace: '/workspace' } });
      return { evidence: { agent: 'zcode' }, status: { agent: 'zcode' } };
    },
  });
  assert.equal(probed.status.agent, 'zcode');
  const models = await agentsCommand(paths, { operation: 'models', agent: 'dsh' }, {
    socket: '/socket',
    callDaemon: async (socket, command, input) => {
      assert.equal(socket, '/socket');
      assert.equal(command, 'agent-models');
      assert.deepEqual(input, { agent: 'dsh', scope: {} });
      return { agent: 'dsh', scope: {}, models: [] };
    },
  });
  assert.deepEqual(models.models, []);
  await assert.rejects(() => agentsCommand(paths, { operation: 'wat' }), (error) => error.code === 'INVALID_ARGUMENT');
  await assert.rejects(() => agentsCommand(paths, { operation: 'list', unknown: true }), (error) => error.code === 'INVALID_ARGUMENT');
  assert.throws(() => parseAgentsArgs(['list', 'zcode']), (error) => error.code === 'INVALID_ARGUMENT');
  assert.throws(() => parseAgentsArgs(['probe', 'zcode', '--auth', '--hi']), (error) => error.code === 'INVALID_ARGUMENT');
  assert.throws(() => parseAgentsArgs(['probe', 'zcode', '--workspace']), (error) => error.code === 'INVALID_ARGUMENT');
  assert.throws(() => parseAgentsArgs(['probe', 'zcode', '--unknown']), (error) => error.code === 'INVALID_ARGUMENT');
});

test('agents status projects daemon evidence and rejects absent identities', async () => {
  const { paths } = fixture();
  const status = {
    service_generation: 'generation-1',
    agents: [{
      agent: 'zcode', config_revision: 7, configured: true, enabled: true, spawn_supported: true,
      transport_support: { transport: 'zcode_app_server', probe: true, spawn: true },
      permission_modes: ['build', 'edit', 'plan', 'yolo'],
      model_selection: { supported: false, mode: 'native_only' },
      local: { state: 'READY', version: '1.2.3', checked_at_ms: 10, scope: {} },
      auth: { state: 'UNKNOWN', checked_at_ms: null, scope: {} },
      hi: { state: 'UNKNOWN', checked_at_ms: null, scope: {} },
    }],
  };
  const options = { socket: '/socket', callDaemon: async (socket, command, input) => {
    assert.equal(socket, '/socket'); assert.equal(command, 'status'); assert.deepEqual(input, {}); return status;
  } };
  assert.deepEqual(await agentsCommand(paths, { operation: 'status', agent: 'zcode' }, options), { service_generation: 'generation-1', agents: status.agents });
  await assert.rejects(() => agentsCommand(paths, { operation: 'status', agent: 'dsh' }, options), (error) => error.code === 'agent_unknown');
});

test('explicit null spawn selection is rejected while omitted route fields stay omitted', () => {
  assert.throws(() => prepareSpawnInput({ agent: null, repository: '/repo', prompt: 'hi' }), (error) => error.code === 'INVALID_ARGUMENT');
  assert.throws(() => prepareSpawnInput({ agent: 'zcode', model: null, repository: '/repo', prompt: 'hi' }), (error) => error.code === 'INVALID_ARGUMENT');
  const omitted = prepareSpawnInput({ repository: '/repo', prompt: 'hi' });
  assert.equal(Object.hasOwn(omitted, 'agent'), false);
  assert.equal(Object.hasOwn(omitted, 'model'), false);
});

test('spawn flags build the shared DTO and reject malformed values', () => {
  assert.deepEqual(parseSpawnArgs([
    '--agent', 'future-provider', '--repository', '/repo', '--prompt', 'hi', '--permission-mode', 'build',
    '--model', 'catalog-token', '--write-manifest', 'src/**', '--write-manifest', 'tests/**',
  ]), {
    agent: 'future-provider', repository: '/repo', prompt: 'hi', permission_mode: 'build',
    model: 'catalog-token', write_manifest: ['src/**', 'tests/**'],
  });
  assert.throws(() => parseSpawnArgs(['--repository', '/repo']), /--prompt is required/u);
  assert.throws(() => parseSpawnArgs(['--repository', '/repo', '--prompt']), /requires a non-null value/u);
  assert.equal(parseSpawnArgs(['--repository', '/repo', '--prompt', 'null']).prompt, 'null');
  assert.equal(parseSpawnArgs(['--repository', '/repo', '--prompt', 'hi', '--model', 'null']).model, 'null');
  assert.throws(() => parseSpawnArgs(['--repository', '/repo', '--prompt', 'hi', '--unknown', 'x']), /unsupported spawn option/u);
  assert.throws(() => parseSpawnArgs(['--repository', '/repo', '--repository', '/other', '--prompt', 'hi']), /only once/u);
});
