import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { EventEmitter } from 'node:events';
import { subagentsCommand } from '../../cli/commands/agents.mjs';
import { parseSubagentsArgs } from '../../cli/commands/agents.mjs';
import { configCommand, parseConfigArgs } from '../../cli/commands/config.mjs';
import { readConfig } from '../../cli/config/read.mjs';
import { parseSpawnArgs, prepareSpawnInput } from '../../cli/commands/tasks.mjs';
import { productPaths } from '../../cli/paths.mjs';
import { launchAgentPlist } from '../../cli/install/service-macos.mjs';

function fixture() { const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-agents-')); return { home, paths: productPaths(home) }; }

test('persisted config version matrix is shared with Rust startup and RPC', () => {
  const { paths } = fixture();
  fs.mkdirSync(path.dirname(paths.config), { recursive: true });
  const cases = JSON.parse(fs.readFileSync(new URL('../fixtures/subagent-config-matrix.json', import.meta.url), 'utf8'));
  for (const entry of cases) {
    fs.writeFileSync(paths.config, JSON.stringify(entry.input));
    if (!entry.valid) {
      // Per-case error contract (S02/AUD-005): a zcode runtime_path keeps its
      // dedicated `runtime_path_unsupported` code; every other rejection keeps
      // its existing CONFIG_INVALID family code.  schema.mjs codes are frozen.
      const code = entry.error_code ?? 'CONFIG_INVALID';
      assert.throws(() => readConfig(paths.config), { code }, entry.name);
      assert.throws(() => launchAgentPlist(paths), { code }, entry.name);
      continue;
    }
    const result = readConfig(paths.config);
    assert.equal(result.schema_version, 2, entry.name);
    assert.equal(Object.hasOwn(result, 'agents'), false, entry.name);
    assert.equal(Object.hasOwn(result, 'default_agent'), false, entry.name);
    const source = entry.input.agents ?? entry.input.subagents ?? {};
    for (const [name, value] of Object.entries(source)) {
      for (const [key, expected] of Object.entries(value)) assert.equal(result.subagents[name][key], expected, entry.name);
      for (const flag of ['enabled', 'spawn_supported']) if (value[flag] === undefined) assert.equal(result.subagents[name][flag], false, entry.name);
    }
    assert.equal(result.default_subagent, entry.input.default_agent ?? entry.input.default_subagent ?? null, entry.name);
  }
});

test('legacy read preserves bytes and next locked write persists only canonical config', () => {
  const { paths } = fixture();
  fs.mkdirSync(path.dirname(paths.config), { recursive: true });
  const legacy = { schema_version: 1, revision: 7, default_agent: 'dsh', agents: {
    dsh: { enabled: true, spawn_supported: true, runtime_path: '/opt/dsh', home: '/runtime/dsh', profile: 'acp', version: '0.1.5-rc.1' },
    codex: { enabled: true, spawn_supported: true, runtime_path: '/opt/codex', home: '/runtime/codex', default_model: 'model-token' },
  } };
  const bytes = JSON.stringify(legacy);
  fs.writeFileSync(paths.config, bytes);
  assert.equal(readConfig(paths.config).default_subagent, 'dsh');
  assert.equal(fs.readFileSync(paths.config, 'utf8'), bytes);
  const plist = launchAgentPlist(paths).toString();
  assert.match(plist, /<key>DSH_HOME<\/key><string>\/runtime\/dsh<\/string>/);
  assert.match(plist, /<key>CODEX_HOME<\/key><string>\/runtime\/codex<\/string>/);
  const updated = configCommand(paths, parseConfigArgs(['set', 'subagents.codex.profile', 'app-server'])).config;
  const persisted = JSON.parse(fs.readFileSync(paths.config, 'utf8'));
  assert.deepEqual(persisted, updated);
  assert.equal(persisted.revision, 8);
  assert.equal(persisted.schema_version, 2);
  assert.equal(persisted.agents, undefined);
  assert.equal(persisted.default_agent, undefined);
  assert.deepEqual(persisted.subagents.dsh, { ...legacy.agents.dsh, default_model: null });
  assert.equal(persisted.subagents.codex.home, '/runtime/codex');
  assert.equal(fs.existsSync(path.join(paths.data, 'codex-homes.json')), false);
});

test('config has no default and lists layered agent support', async () => {
  const { paths } = fixture();
  const listed = await subagentsCommand(paths);
  assert.equal(listed.default_subagent, null);
  assert.deepEqual(listed.subagents.map((agent) => [agent.subagent, agent.spawn_supported]), [['zcode', false], ['dsh', false], ['codex', false]]);
});

test('zcode model is rejected before prompt and dsh remains discovery-only', () => {
  const { paths } = fixture();
  assert.throws(() => configCommand(paths, { operation: 'set', patch: { default_subagent: 'zcode', subagents: { zcode: { default_model: 'glm-4' } } } }), (error) => error.code === 'model_selection_unsupported');
  const config = configCommand(paths, { operation: 'set', patch: { subagents: { dsh: { enabled: true } } } }).config;
  assert.equal(config.subagents.dsh.enabled, true);
  assert.equal(config.subagents.dsh.spawn_supported, false);
});

test('explicit dsh spawn support survives config validation', () => {
  const { paths } = fixture();
  const config = configCommand(paths, { operation: 'set', patch: { subagents: { dsh: { enabled: true, spawn_supported: true } } } }).config;
  assert.equal(config.subagents.dsh.enabled, true);
  assert.equal(config.subagents.dsh.spawn_supported, true);
  assert.equal(readConfig(paths.config).subagents.dsh.spawn_supported, true);
});

test('LaunchAgent captures configured DSH runtime and home for GUI services', () => {
  const { paths } = fixture();
  configCommand(paths, { operation: 'set', patch: { subagents: { dsh: {
    runtime_path: '/opt/dsh/runtime with spaces', home: '/var/lib/dsh profile', profile: 'acp', version: '0.1.5-rc.1',
  } } } });
  const plist = launchAgentPlist(paths).toString('utf8');
  assert.match(plist, /<key>DSH_RUNTIME_PATH<\/key><string>\/opt\/dsh\/runtime with spaces<\/string>/u);
  assert.match(plist, /<key>DSH_HOME<\/key><string>\/var\/lib\/dsh profile<\/string>/u);
  assert.match(plist, /<key>DSH_PROFILE<\/key><string>acp<\/string>/u);
});

test('codex config persists runtime, home, and default model through every path', () => {
  const { paths } = fixture();
  const config = configCommand(paths, { operation: 'set', patch: { subagents: { codex: {
    enabled: true, spawn_supported: true, default_model: 'gpt-5.6-terra',
    runtime_path: '/opt/homebrew/bin/codex', home: '/Users/fixture/.codex-multi-2',
  } } } }).config;
  assert.equal(config.subagents.codex.enabled, true);
  assert.equal(config.subagents.codex.spawn_supported, true);
  assert.equal(config.subagents.codex.default_model, 'gpt-5.6-terra');
  // Human key paths and unset restore the disabled defaults.
  configCommand(paths, parseConfigArgs(['set', 'subagents.codex.home', '/tmp/other-home']));
  assert.equal(readConfig(paths.config).subagents.codex.home, '/tmp/other-home');
  assert.deepEqual(
    configCommand(paths, parseConfigArgs(['unset', 'subagents.codex.home'])).config.subagents.codex,
    { enabled: true, spawn_supported: true, default_model: 'gpt-5.6-terra', runtime_path: '/opt/homebrew/bin/codex', home: null, profile: null, version: null },
  );
  // Unknown agents stay rejected.
  assert.throws(() => parseConfigArgs(['set', 'subagents.other.enabled', 'true']), (error) => error.code === 'INVALID_ARGUMENT');
});

test('LaunchAgent forwards the persisted Codex runtime and home exactly', () => {
  const { paths } = fixture();
  configCommand(paths, { operation: 'set', patch: { subagents: { codex: {
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

test('a zcode runtime_path is rejected explicitly instead of silently accepted (AUD-005/D1)', () => {
  const { paths } = fixture();
  assert.throws(() => configCommand(paths, { operation: 'set', patch: { subagents: { zcode: { runtime_path: '/opt/zcode.cjs' } } } }), (error) => error.code === 'runtime_path_unsupported');
  assert.throws(() => configCommand(paths, parseConfigArgs(['set', 'subagents.zcode.runtime_path', '/opt/zcode.cjs'])), (error) => error.code === 'runtime_path_unsupported');
  // A hand-written config carrying the unused field fails every read closed,
  // so no service path can quietly accept a no-op configuration.
  fs.mkdirSync(path.dirname(paths.config), { recursive: true });
  fs.writeFileSync(paths.config, JSON.stringify({ schema_version: 2, subagents: { zcode: { runtime_path: '/opt/zcode.cjs' } } }));
  assert.throws(() => readConfig(paths.config), (error) => error.code === 'runtime_path_unsupported');
  assert.throws(() => launchAgentPlist(paths), (error) => error.code === 'runtime_path_unsupported');
  assert.equal(fs.readFileSync(paths.config, 'utf8'), JSON.stringify({ schema_version: 2, subagents: { zcode: { runtime_path: '/opt/zcode.cjs' } } }), 'the rejected config is never rewritten');
});

test('the service template stays adapter-neutral per configured adapter (AUD-005/D1)', () => {
  // DSH-only: the plist forwards the persisted DSH launch contract and omits
  // every Codex entry; the pinned ZCode runtime argument disappears with the
  // (here absent) ZCode installation instead of becoming a hard dependency.
  const dshOnly = fixture();
  configCommand(dshOnly.paths, { operation: 'set', patch: { subagents: { dsh: {
    enabled: true, spawn_supported: true, runtime_path: '/opt/dsh/acp', home: '/var/lib/dsh', profile: 'acp', version: '0.1.5',
  } } } });
  const dshPlist = launchAgentPlist(dshOnly.paths, { zcodeRuntime: '/definitely/absent/zcode.cjs' }).toString('utf8');
  assert.match(dshPlist, /<key>DSH_RUNTIME_PATH<\/key><string>\/opt\/dsh\/acp<\/string>/u);
  assert.match(dshPlist, /<key>DSH_HOME<\/key><string>\/var\/lib\/dsh<\/string>/u);
  assert.doesNotMatch(dshPlist, /CODEX_RUNTIME_PATH/u);
  assert.doesNotMatch(dshPlist, /CODEX_HOME/u);
  assert.doesNotMatch(dshPlist, /--runtime/u, 'no ZCode installation means no runtime argument, never a broken one');

  // Codex-subagent-only: the persisted Codex runtime/home pair is forwarded
  // and no DSH entry appears.
  const codexOnly = fixture();
  configCommand(codexOnly.paths, { operation: 'set', patch: { subagents: { codex: {
    enabled: true, spawn_supported: true, runtime_path: '/opt/homebrew/bin/codex', home: '/Users/fixture/.codex-sub',
  } } } });
  const codexPlist = launchAgentPlist(codexOnly.paths, { zcodeRuntime: '/definitely/absent/zcode.cjs' }).toString('utf8');
  assert.match(codexPlist, /<key>CODEX_RUNTIME_PATH<\/key><string>\/opt\/homebrew\/bin\/codex<\/string>/u);
  assert.match(codexPlist, /<key>CODEX_HOME<\/key><string>\/Users\/fixture\/\.codex-sub<\/string>/u);
  assert.doesNotMatch(codexPlist, /DSH_RUNTIME_PATH/u);
  assert.doesNotMatch(codexPlist, /--runtime/u);

  // ZCode-only (no other adapter configured): a present pinned runtime is
  // forwarded so the zcode adapter keeps its launch contract, and nothing
  // else is injected.
  const zcodeOnly = fixture();
  const present = fixture();
  const pinnedRuntime = path.join(present.home, 'zcode.cjs');
  fs.writeFileSync(pinnedRuntime, 'runtime');
  const zcodePlist = launchAgentPlist(zcodeOnly.paths, { zcodeRuntime: pinnedRuntime }).toString('utf8');
  assert.match(zcodePlist, new RegExp(`<string>--runtime</string><string>${pinnedRuntime.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}</string>`, 'u'));
  assert.doesNotMatch(zcodePlist, /DSH_RUNTIME_PATH/u);
  assert.doesNotMatch(zcodePlist, /CODEX_RUNTIME_PATH/u);
  fs.rmSync(present.home, { recursive: true, force: true });
});

test('config writes a revision and keeps existing task snapshots independent', () => {
  const { paths } = fixture();
  const first = configCommand(paths, { operation: 'set', patch: { default_subagent: 'zcode', subagents: { zcode: { enabled: true, spawn_supported: true } } } }).config;
  const second = configCommand(paths, { operation: 'set', patch: { subagents: { dsh: { enabled: true } } } }).config;
  assert.equal(first.revision, 1);
  assert.equal(second.revision, 2);
  assert.equal(first.default_subagent, 'zcode');
  assert.equal(second.subagents.dsh.spawn_supported, false);
});

test('per-agent patch preserves fields outside the patch', () => {
  const { paths } = fixture();
  const disabled = configCommand(paths, { operation: 'set', patch: { subagents: { dsh: { enabled: false, default_model: 'catalog-token' } } } }).config;
  const updated = configCommand(paths, { operation: 'set', patch: { subagents: { dsh: { default_model: 'next-token' } } } }).config;
  assert.equal(disabled.subagents.dsh.enabled, false);
  assert.equal(updated.subagents.dsh.enabled, false);
  assert.equal(updated.subagents.dsh.default_model, 'next-token');
  assert.equal(updated.subagents.dsh.spawn_supported, false);
});

test('human config forms parse typed values and get one key', () => {
  const { paths } = fixture();
  configCommand(paths, { operation: 'set', patch: { default_subagent: 'zcode', subagents: { zcode: { enabled: true } } } });
  assert.deepEqual(configCommand(paths, parseConfigArgs(['get', 'default_subagent'])), { key: 'default_subagent', value: 'zcode', revision: 1 });
  configCommand(paths, parseConfigArgs(['set', 'subagents.dsh.enabled', 'true']));
  assert.equal(readConfig(paths.config).subagents.dsh.enabled, true);
  assert.throws(() => parseConfigArgs(['set', 'subagents.other.enabled', 'true']), (error) => error.code === 'INVALID_ARGUMENT');
});

test('config show and unset use the same validated revision path', () => {
  const { paths } = fixture();
  configCommand(paths, { operation: 'set', patch: { default_subagent: 'zcode', subagents: { zcode: { enabled: true } } } });
  const shown = configCommand(paths, parseConfigArgs(['show']));
  assert.equal(shown.config.default_subagent, 'zcode');
  assert.equal(shown.config.revision, 1);
  const unset = configCommand(paths, parseConfigArgs(['unset', 'default_subagent']));
  assert.equal(unset.config.default_subagent, null);
  assert.equal(unset.config.revision, 2);
  assert.equal(configCommand(paths, parseConfigArgs(['unset', 'subagents.zcode.enabled'])).config.subagents.zcode.enabled, false);
  assert.throws(() => parseConfigArgs(['unset', 'revision']), /unsupported config key/u);
});

test('config set cannot override revision or merge an agent null patch', () => {
  const { paths } = fixture();
  assert.throws(() => configCommand(paths, { operation: 'set', patch: { revision: 999, default_subagent: 'zcode' } }), /managed by the writer/u);
  assert.throws(() => configCommand(paths, { operation: 'set', patch: { subagents: { zcode: null } } }), /must be an object/u);
  assert.throws(() => configCommand(paths, { operation: 'set', patch: { subagents: { zcode: ['bad'] } } }), /must be an object/u);
  const first = configCommand(paths, { operation: 'set', patch: { default_subagent: 'zcode', subagents: { zcode: { enabled: true, spawn_supported: true } } } }).config;
  assert.equal(first.revision, 1);
  assert.throws(() => configCommand(paths, { operation: 'set', patch: { revision: first.revision } }), /managed by the writer/u);
});

test('JSON unset accepts only a supported key and cannot reuse revision or null agent patches', () => {
  const { paths } = fixture();
  const first = configCommand(paths, { operation: 'set', patch: { default_subagent: 'zcode', subagents: { zcode: { enabled: true, spawn_supported: true } } } }).config;
  assert.equal(first.revision, 1);
  assert.throws(() => configCommand(paths, { operation: 'unset', patch: { revision: 0, default_subagent: null } }), /exactly one supported key/u);
  assert.throws(() => configCommand(paths, { operation: 'unset', patch: { subagents: { zcode: null } } }), /exactly one supported key/u);
  const unset = configCommand(paths, { operation: 'unset', key: 'default_subagent' }).config;
  assert.equal(unset.default_subagent, null);
  assert.equal(unset.revision, 2);
});

test('concurrent config writers serialize revision and preserve both updates', async () => {
  const { paths } = fixture();
  const script = `import { configCommand } from './cli/commands/config.mjs'; configCommand(${JSON.stringify(paths)}, JSON.parse(process.argv[1]));`;
  const run = (patch) => new Promise((resolve, reject) => {
    const child = spawn(process.execPath, ['--input-type=module', '-e', script, JSON.stringify({ operation: 'set', patch })], { cwd: path.resolve('.') });
    child.on('error', reject); child.on('close', (code) => code === 0 ? resolve() : reject(new Error(`writer exited ${code}`)));
  });
  await Promise.all([run({ default_subagent: 'zcode', subagents: { zcode: { enabled: true, spawn_supported: true } } }), run({ subagents: { dsh: { enabled: true } } })]);
  const final = readConfig(paths.config);
  assert.equal(final.revision, 2);
  assert.equal(final.default_subagent, 'zcode');
  assert.equal(final.subagents.dsh.enabled, true);
});

test('config writer recovers a stale lock from a dead owner', () => {
  const { paths } = fixture();
  fs.mkdirSync(path.dirname(paths.config), { recursive: true });
  fs.writeFileSync(paths.config, JSON.stringify({ schema_version: 2, revision: 4, default_subagent: null, subagents: { zcode: { enabled: true, spawn_supported: true, default_model: null }, dsh: { enabled: false, spawn_supported: false, default_model: null } } }));
  fs.writeFileSync(`${paths.config}.lock`, '999999\n');
  const result = configCommand(paths, { operation: 'set', patch: { default_subagent: 'zcode', subagents: { zcode: { enabled: true, spawn_supported: true } } } }).config;
  assert.equal(result.revision, 5);
  assert.equal(result.default_subagent, 'zcode');
});

test('unknown config fields and operations fail closed', () => {
  const { paths } = fixture();
  assert.throws(() => configCommand(paths, { operation: 'wat' }), (error) => error.code === 'INVALID_ARGUMENT');
  assert.throws(() => configCommand(paths, { operation: 'get', surprise: true }), (error) => error.code === 'INVALID_ARGUMENT');
  assert.throws(() => configCommand(paths, { operation: 'set', patch: { subagents: { other: { enabled: true } } } }), (error) => error.code === 'CONFIG_INVALID');
  assert.throws(() => configCommand(paths, { operation: 'set', patch: { subagents: { zcode: { mystery: true } } } }), (error) => error.code === 'CONFIG_INVALID');
});

test('retired top-level product path fields fail closed as unknown config fields', () => {
  const { paths } = fixture();
  fs.mkdirSync(path.dirname(paths.config), { recursive: true });
  fs.writeFileSync(paths.config, JSON.stringify({ schema_version: 2, runtime: '/runtime', database: '/database', socket: '/socket' }));
  assert.throws(() => configCommand(paths, { operation: 'set', patch: { default_subagent: 'zcode', subagents: { zcode: { enabled: true, spawn_supported: true } } } }), (error) => error.code === 'CONFIG_INVALID');
});

test('agents human operations are strict and unsupported actions are explicit', async () => {
  const { paths } = fixture();
  assert.deepEqual(parseSubagentsArgs([]), { operation: 'list' });
  assert.deepEqual(parseSubagentsArgs(['status', 'zcode']), { operation: 'status', subagent: 'zcode' });
  assert.deepEqual(parseSubagentsArgs(['probe', 'zcode', '--hi', '--workspace', '/workspace', '--home', '/home']), {
    operation: 'probe', subagent: 'zcode', through: 'hi', workspace: '/workspace', home: '/home',
  });
  assert.deepEqual(parseSubagentsArgs(['models', 'dsh', '--workspace', '/workspace', '--home', '/home']), {
    operation: 'models', subagent: 'dsh', workspace: '/workspace', home: '/home',
  });
  assert.deepEqual(parseSubagentsArgs(['probe', 'future-provider', '--local']), { operation: 'probe', subagent: 'future-provider', through: 'local' });
  const probed = await subagentsCommand(paths, { operation: 'probe', subagent: 'zcode', through: 'hi', workspace: '/workspace' }, {
    socket: '/socket',
    callDaemon: async (socket, command, input) => {
      assert.equal(socket, '/socket');
      assert.equal(command, 'agent-probe');
      assert.deepEqual(input, { subagent: 'zcode', through: 'hi', scope: { workspace: '/workspace' } });
      return { evidence: { subagent: 'zcode' }, status: { subagent: 'zcode' } };
    },
  });
  assert.equal(probed.status.subagent, 'zcode');
  const models = await subagentsCommand(paths, { operation: 'models', subagent: 'dsh' }, {
    socket: '/socket',
    callDaemon: async (socket, command, input) => {
      assert.equal(socket, '/socket');
      assert.equal(command, 'agent-models');
      assert.deepEqual(input, { subagent: 'dsh', scope: {} });
      return { subagent: 'dsh', scope: {}, models: [] };
    },
  });
  assert.deepEqual(models.models, []);
  await assert.rejects(() => subagentsCommand(paths, { operation: 'wat' }), (error) => error.code === 'INVALID_ARGUMENT');
  await assert.rejects(() => subagentsCommand(paths, { operation: 'list', unknown: true }), (error) => error.code === 'INVALID_ARGUMENT');
  assert.throws(() => parseSubagentsArgs(['list', 'zcode']), (error) => error.code === 'INVALID_ARGUMENT');
  assert.throws(() => parseSubagentsArgs(['probe', 'zcode', '--auth', '--hi']), (error) => error.code === 'INVALID_ARGUMENT');
  assert.throws(() => parseSubagentsArgs(['probe', 'zcode', '--workspace']), (error) => error.code === 'INVALID_ARGUMENT');
  assert.throws(() => parseSubagentsArgs(['probe', 'zcode', '--unknown']), (error) => error.code === 'INVALID_ARGUMENT');
});

test('agents status projects daemon evidence and rejects absent identities', async () => {
  const { paths } = fixture();
  const status = {
    service_generation: 'generation-1',
    subagents: [{
      subagent: 'zcode', config_revision: 7, configured: true, enabled: true, spawn_supported: true,
      transport_support: { transport: 'zcode_app_server', probe: true, spawn: true },
      permission_modes: ['build', 'edit', 'plan', 'yolo'],
      model_selection: { supported: false, mode: 'native_only' },
      effort_selection: { supported: true, mode: 'passthrough_token' },
      local: { state: 'READY', version: '1.2.3', checked_at_ms: 10, scope: {} },
      auth: { state: 'UNKNOWN', checked_at_ms: null, scope: {} },
      hi: { state: 'UNKNOWN', checked_at_ms: null, scope: {} },
    }],
  };
  const options = { socket: '/socket', callDaemon: async (socket, command, input) => {
    assert.equal(socket, '/socket'); assert.equal(command, 'status'); assert.deepEqual(input, {}); return status;
  } };
  assert.deepEqual(await subagentsCommand(paths, { operation: 'status', subagent: 'zcode' }, options), { service_generation: 'generation-1', subagents: status.subagents });
  await assert.rejects(() => subagentsCommand(paths, { operation: 'status', subagent: 'dsh' }, options), (error) => error.code === 'subagent_unknown'
    && error.message === 'daemon did not report subagent: dsh, available subagents are ["zcode"]');
});

test('explicit null spawn selection is rejected while omitted route fields stay omitted', () => {
  assert.throws(() => prepareSpawnInput({ subagent: null, repository: '/repo', prompt: 'hi' }), (error) => error.code === 'INVALID_ARGUMENT');
  assert.throws(() => prepareSpawnInput({ subagent: 'zcode', model: null, repository: '/repo', prompt: 'hi' }), (error) => error.code === 'INVALID_ARGUMENT');
  assert.throws(() => prepareSpawnInput({ subagent: 'zcode', effort: null, repository: '/repo', prompt: 'hi' }), (error) => error.code === 'INVALID_ARGUMENT');
  assert.throws(() => prepareSpawnInput({ subagent: 'zcode', effort: '', repository: '/repo', prompt: 'hi' }), (error) => error.code === 'INVALID_ARGUMENT');
  const omitted = prepareSpawnInput({ repository: '/repo', prompt: 'hi' });
  assert.equal(Object.hasOwn(omitted, 'subagent'), false);
  assert.equal(Object.hasOwn(omitted, 'model'), false);
  assert.equal(Object.hasOwn(omitted, 'effort'), false);
  assert.deepEqual(prepareSpawnInput({ subagent: 'zcode', effort: 'high', repository: '/repo', prompt: 'hi' }), {
    subagent: 'zcode', effort: 'high', repository: '/repo', prompt: 'hi',
  });
});

test('spawn flags build the shared DTO and reject malformed values', () => {
  assert.deepEqual(parseSpawnArgs([
    '--subagent', 'future-provider', '--repository', '/repo', '--prompt', 'hi', '--permission-mode', 'build',
    '--model', 'catalog-token', '--effort', 'high', '--write-manifest', 'src/**', '--write-manifest', 'tests/**',
  ]), {
    subagent: 'future-provider', repository: '/repo', prompt: 'hi', permission_mode: 'build',
    model: 'catalog-token', effort: 'high', write_manifest: ['src/**', 'tests/**'],
  });
  assert.throws(() => parseSpawnArgs(['--repository', '/repo']), /--prompt is required/u);
  assert.throws(() => parseSpawnArgs(['--repository', '/repo', '--prompt']), /requires a non-null value/u);
  assert.equal(parseSpawnArgs(['--repository', '/repo', '--prompt', 'null']).prompt, 'null');
  assert.equal(parseSpawnArgs(['--repository', '/repo', '--prompt', 'hi', '--model', 'null']).model, 'null');
  assert.equal(parseSpawnArgs(['--repository', '/repo', '--prompt', 'hi', '--effort', 'null']).effort, 'null');
  assert.equal(Object.hasOwn(parseSpawnArgs(['--repository', '/repo', '--prompt', 'hi']), 'effort'), false);
  assert.throws(() => parseSpawnArgs(['--repository', '/repo', '--prompt', 'hi', '--unknown', 'x']), /unsupported spawn option/u);
  assert.throws(() => parseSpawnArgs(['--repository', '/repo', '--repository', '/other', '--prompt', 'hi']), /only once/u);
});

test('enable rejects failed or incomplete observations without writing config', async () => {
  const { paths } = fixture();
  for (const local of [
    { state: 'UNAVAILABLE', reason: 'missing' },
    { state: 'READY', version: '1', scope: { home: '/home' } },
    { state: 'READY', runtime_path: '/runtime', version: '1', scope: {} },
  ]) {
    await assert.rejects(() => subagentsCommand(paths, { operation: 'enable', subagent: 'codex' }, {
      socket: '/socket', callDaemon: async () => ({ evidence: { local } }),
    }), { code: 'agent_probe_failed' });
    assert.equal(fs.existsSync(paths.config), false);
  }
  for (const args of [['enable'], ['enable', 'future'], ['enable', 'dsh', '--hi']]) {
    assert.throws(() => parseSubagentsArgs(args), { code: 'INVALID_ARGUMENT' });
  }
});

test('dsh enable rejects incompatible or absent daemon version requirements without writes', async () => {
  const { paths } = fixture();
  for (const required_version of ['0.1.5-rc.1', undefined]) {
    await assert.rejects(() => subagentsCommand(paths, { operation: 'enable', subagent: 'dsh' }, {
      socket: '/socket', callDaemon: async () => ({
        status: { required_version },
        evidence: { local: { state: 'READY', runtime_path: '/runtime/dsh', version: '9.9.9', scope: { home: '/runtime/home' } } },
      }),
    }), (error) => {
      assert.equal(error.code, 'agent_probe_failed');
      assert.match(error.message, required_version ? /expected version 0\.1\.5-rc\.1, observed 9\.9\.9.*install/u : /required_version.*restart/u);
      return true;
    });
    assert.equal(fs.existsSync(paths.config), false);
  }
});

test('daemon startup timeout reaps the child, escalating ignored SIGTERM to SIGKILL', async () => {
  const { daemonHarness } = await import('../fixtures/restart-daemon.mjs');
  for (const ignoreTerm of [false, true]) {
    const { home } = fixture();
    const child = new EventEmitter();
    child.stderr = new EventEmitter();
    child.exitCode = null;
    child.signalCode = null;
    const signals = [];
    child.kill = (signal) => {
      signals.push(signal);
      if (signal === 'SIGTERM' && ignoreTerm) return true;
      setTimeout(() => { child.signalCode = signal; child.emit('exit', null, signal); }, 5);
      return true;
    };
    await assert.rejects(() => daemonHarness({ root: home, home, runtime: '/runtime', spawnProcess: () => child, timeoutMs: 20 }), /daemon startup timeout/u);
    assert.deepEqual(signals, ignoreTerm ? ['SIGTERM', 'SIGKILL'] : ['SIGTERM']);
    assert.equal(child.signalCode, ignoreTerm ? 'SIGKILL' : 'SIGTERM', 'must await exit before rejecting startup');
  }
});

test('fresh init and enable use PATH evidence, live admission, and restarted factories', { timeout: 60000 }, async (t) => {
  const { runInit } = await import('../../cli/install/init.mjs');
  const { callDaemon } = await import('../../cli/rpc.mjs');
  const { daemonHarness } = await import('../fixtures/restart-daemon.mjs');
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'en-'));
  const home = path.join(root, 'h');
  const paths = productPaths(home);
  const bin = path.join(root, 'bin');
  const providerHome = path.join(root, 'provider');
  const workspace = path.join(root, 'repo');
  for (const dir of [home, bin, providerHome, workspace]) fs.mkdirSync(dir, { recursive: true });
  // Absolute Node shebang prevents a host runtime from being discovered accidentally.
  const fixtureSource = path.resolve('tests/fixtures/dsh-hi-probe.mjs');
  fs.writeFileSync(path.join(bin, 'dsh'), '#!' + process.execPath + '\n' + fs.readFileSync(fixtureSource, 'utf8'), { mode: 0o755 });
  fs.writeFileSync(path.join(bin, 'codex'), '#!/bin/sh\nif [ "$1" = "--version" ]; then echo codex-cli 1.2.3; exit 0; fi\nexit 42\n', { mode: 0o755 });
  fs.symlinkSync(process.execPath, path.join(bin, 'node'));
  const init = runInit({ paths, skipPayloadProbe: true, skipServiceStart: true });
  assert.deepEqual(Object.keys(init.runtimes), ['zcode', 'dsh', 'codex']);
  assert.ok(Object.values(readConfig(paths.config).subagents).every((entry) => !entry.enabled && !entry.spawn_supported));
  const daemon = await daemonHarness({ root, home, runtime: path.resolve('tests/fixtures/zcode-general.mjs'), env: {
    PATH: bin, DSH_HOME: providerHome, CODEX_HOME: providerHome, S05_DSH_SPAWN_FIXTURE: '1',
  } });
  t.after(() => daemon.stop());
  const options = { socket: daemon.socket, callDaemon };
  const spawnTask = (subagent) => callDaemon(daemon.socket, 'spawn', {
    subagent, ...(subagent === 'codex' ? { model: 'fixture-model' } : {}), repository: workspace, prompt: 'fixture complete', permission_mode: subagent === 'codex' ? 'yolo' : 'build',
  });
  const status = await callDaemon(daemon.socket, 'status', {});
  assert.ok((status.subagents ?? status.agents).every((entry) => !entry.enabled && !entry.spawn_supported));
  for (const entry of status.subagents ?? status.agents) {
    assert.equal(entry.required_version, (entry.subagent ?? entry.agent) === 'dsh' ? '0.1.5-rc.1' : undefined);
  }
  await assert.rejects(() => spawnTask('zcode'), { code: 'agent_disabled' });
  const listed = await callDaemon(daemon.socket, 'list', { repository: workspace });
  assert.equal(listed.tasks.length, 0);
  const initialBytes = fs.readFileSync(paths.config, 'utf8');
  const observed = await subagentsCommand(paths, { operation: 'probe', subagent: 'dsh' }, options);
  assert.equal(observed.evidence.local.runtime_path, path.join(bin, 'dsh'));
  assert.equal(observed.evidence.local.version, '0.1.5-rc.1');
  assert.equal(observed.status.required_version, '0.1.5-rc.1');
  assert.equal(observed.status.local.runtime_path, path.join(bin, 'dsh'));
  assert.equal(observed.status.spawn_supported, false);
  assert.equal(fs.readFileSync(paths.config, 'utf8'), initialBytes, 'probe must never persist or promote');

  const zcode = await subagentsCommand(paths, parseSubagentsArgs(['enable', 'zcode']), options);
  assert.equal(zcode.restart_required, false);
  const ztask = await spawnTask('zcode');
  const zwait = await callDaemon(daemon.socket, 'wait', { agent_id: ztask.agent_id, wait_time: 10 });
  assert.equal(zwait.task.status, 'completed', JSON.stringify(zwait));

  const dsh = await subagentsCommand(paths, parseSubagentsArgs(['enable', 'dsh']), options);
  assert.match(dsh.message, /重启 daemon 后生效/);
  const saved = readConfig(paths.config).subagents.dsh;
  assert.deepEqual(saved, { enabled: true, spawn_supported: true, default_model: null,
    runtime_path: path.join(bin, 'dsh'), home: providerHome, profile: 'acp', version: '0.1.5-rc.1' });
  const bytes = fs.readFileSync(paths.config, 'utf8');
  await subagentsCommand(paths, parseSubagentsArgs(['enable', 'dsh']), options);
  assert.equal(fs.readFileSync(paths.config, 'utf8'), bytes, 'repeat enable preserves revision and bytes');
  const codex = await subagentsCommand(paths, parseSubagentsArgs(['enable', 'codex']), options);
  assert.equal(codex.restart_required, true);
  assert.match(codex.message, /重启 daemon 后生效/);
  assert.deepEqual(readConfig(paths.config).subagents.codex, {
    enabled: true, spawn_supported: true, default_model: null, runtime_path: path.join(bin, 'codex'),
    home: providerHome, profile: null, version: '1.2.3',
  });
  await daemon.restart();
  const dtask = await spawnTask('dsh');
  const dwait = await callDaemon(daemon.socket, 'wait', { agent_id: dtask.agent_id, wait_time: 10 });
  assert.equal(dwait.task.status, 'completed', JSON.stringify(dwait));
  // The Codex fixture deliberately has no app-server. Admission must still open;
  // main.rs unit tests separately assert the production factory selection.
  const ctask = await spawnTask('codex');
  assert.ok(ctask.agent_id);
});
