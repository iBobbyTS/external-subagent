import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

import { CliError } from '../../cli/errors.mjs';
import { HELP, main } from '../../cli/main.mjs';
import { productPaths, profilesDir } from '../../cli/paths.mjs';
import { callDaemon } from '../../cli/rpc.mjs';
import {
  parseProfileArgs,
  parseSpawnArgs,
  prepareSpawnInput,
  profileCommand,
  scanProfileToml,
  scanProfilesDir,
} from '../../cli/commands/tasks.mjs';

const CORPUS_DIR = path.resolve(import.meta.dirname, 'profiles-corpus');
const CLI_BIN = path.resolve(import.meta.dirname, '../../bin/external-subagent.mjs');
const NO_ENV = Object.freeze({});

// The category stored in each corpus expectation maps to a substring that the
// scanner's diagnostic must contain, so failures point at the right rule.
const REASON_MARKERS = Object.freeze({
  unterminated_basic: 'Unterminated basic string',
  unterminated_multiline: 'Unterminated multiline',
  invalid_escape: 'Invalid escape sequence',
  invalid_unicode: 'Invalid Unicode code point',
  illegal_primitive: 'Invalid TOML primitive',
  duplicate_key: 'duplicate key',
  array_error: 'in array',
  inline_table_error: 'in inline table',
  lone_cr: 'Disallowed control character 0x0d',
  basic_newline: 'Unescaped newline in basic string',
  unknown_key: 'unknown top-level key',
  table_header: 'unexpected table header',
  dotted_key: 'dotted key',
  missing_name: "missing required field 'name'",
  name_not_string: "field 'name': must be a string",
  empty_name: "field 'name': cannot be empty",
  oversize_name: "field 'name': exceeds 128 bytes",
  nul_name: "field 'name': cannot contain NUL byte",
  namespace_conflict: 'namespace conflict',
  table_header_syntax: 'malformed table header',
  multiline_quotes: 'Too many consecutive quotes',
  del_char: 'Disallowed control character 0x7f',
  comment_control: 'in comment',
  field_type: 'must be a string',
});

function createTempEnv() {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-profiles-'));
  const paths = productPaths(home);
  const profilesPath = path.join(paths.data, 'profiles');
  fs.mkdirSync(profilesPath, { recursive: true });
  return { home, paths, profilesDir: profilesPath };
}

function listProfiles(paths, env = NO_ENV) {
  return profileCommand(paths, ['list'], env);
}

function showProfile(paths, name, env = NO_ENV) {
  return profileCommand(paths, ['show', name], env);
}

function runCli(args, env) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [CLI_BIN, ...args], { env, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    child.stdout.setEncoding('utf8');
    child.stderr.setEncoding('utf8');
    child.stdout.on('data', (chunk) => { stdout += chunk; });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    child.on('close', (code) => resolve({ code, stdout, stderr }));
  });
}

async function withMockErrorDaemon(socketPath, message, fn) {
  const server = net.createServer((socket) => socket.once('data', (chunk) => {
    const request = JSON.parse(chunk.toString('utf8'));
    socket.end(`${JSON.stringify({
      request_id: request.request_id,
      outcome: 'error',
      error: { code: 'validation', message },
    })}\n`);
  }));
  await new Promise((resolve) => server.listen(socketPath, resolve));
  try {
    return await fn();
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }
}

// ---------------------------------------------------------------------------
// AC1: 透传 (Passthrough & wire JSON shape)
// ---------------------------------------------------------------------------

test('AC1: parseSpawnArgs parses --profile and supports --write-manifest', () => {
  const parsed = parseSpawnArgs([
    '--profile', 'fast-coder',
    '--repository', '/workspace/repo',
    '--prompt', 'write a test',
    '--write-manifest', 'src/**',
    '--write-manifest', 'tests/**',
  ]);
  assert.deepEqual(parsed, {
    profile: 'fast-coder',
    repository: '/workspace/repo',
    prompt: 'write a test',
    write_manifest: ['src/**', 'tests/**'],
  });
  assert.equal(Object.hasOwn(parsed, 'subagent'), false);
  assert.equal(Object.hasOwn(parsed, 'permission_mode'), false);
  assert.equal(Object.hasOwn(parsed, 'model'), false);
  assert.equal(Object.hasOwn(parsed, 'effort'), false);
});

test('AC1: CLI spawn wire forwards top-level profile and omits permission_mode from manifest', async () => {
  const socketPath = path.join(os.tmpdir(), `cli-spawn-profile-${process.pid}-${Date.now()}.sock`);
  const seenRequests = [];
  const server = net.createServer((socket) => socket.once('data', (chunk) => {
    const request = JSON.parse(chunk.toString('utf8'));
    seenRequests.push(request);
    socket.end(JSON.stringify({
      request_id: request.request_id,
      outcome: 'success',
      result: {
        kind: 'general_submitted',
        task: { agent_id: '10000002', status: 'running', session_id: 'session-456', input_identity: null },
      },
    }) + '\n');
  }));
  await new Promise((resolve) => server.listen(socketPath, resolve));

  try {
    const res = await callDaemon(socketPath, 'spawn', {
      profile: 'production-runner',
      repository: '/workspace',
      prompt: 'run deployment check',
    });
    assert.deepEqual(res, { agent_id: 10000002, status: 'running', session_id: 'session-456' });
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }

  assert.equal(seenRequests.length, 1);
  const req = seenRequests[0];
  assert.equal(req.method, 'submit_general');
  assert.equal(req.params.profile, 'production-runner');
  assert.equal(Object.hasOwn(req.params, 'subagent'), false);
  assert.equal(Object.hasOwn(req.params, 'model'), false);
  assert.equal(Object.hasOwn(req.params, 'effort'), false);
  assert.equal(Object.hasOwn(req.params.manifest, 'profile'), false, 'profile is passthrough at top level, not inside manifest');
  assert.equal(Object.hasOwn(req.params.manifest, 'permission_mode'), false, 'profile mode must NOT inject default permission_mode into manifest');
  assert.equal(req.params.manifest.repository, '/workspace');
  assert.equal(req.params.manifest.prompt, 'run deployment check');
  assert.deepEqual(req.params.manifest.write_manifest, []);
});

// ---------------------------------------------------------------------------
// AC2: 互斥 (Mutex validation with exit 2 + guidance)
// ---------------------------------------------------------------------------

test('AC2: --profile combined with any of four mutex flags fails with exit 2 and guidance', () => {
  const mutexFlags = [
    ['--subagent', 'dsh'],
    ['--permission-mode', 'build'],
    ['--model', 'gpt-5'],
    ['--effort', 'high'],
  ];

  for (const [flag, val] of mutexFlags) {
    // 1. CLI flag form
    assert.throws(
      () => parseSpawnArgs(['--profile', 'my-prof', flag, val, '--repository', '/repo', '--prompt', 'hi']),
      (error) => {
        assert.ok(error instanceof CliError);
        assert.equal(error.code, 'INVALID_ARGUMENT');
        assert.equal(error.exitCode, 2);
        assert.match(error.message, /profile cannot be combined with subagent, permission_mode, model, or effort/u);
        assert.match(error.message, /specify these in the profile TOML or omit profile/u);
        return true;
      },
      `failed for ${flag}`,
    );

    // 2. Pre-repository mutex check (fails before required flags check)
    assert.throws(
      () => parseSpawnArgs(['--profile', 'my-prof', flag, val]),
      (error) => {
        assert.ok(error instanceof CliError);
        assert.equal(error.code, 'INVALID_ARGUMENT');
        assert.equal(error.exitCode, 2);
        assert.match(error.message, /profile cannot be combined with/u);
        return true;
      },
    );
  }

  // 3. Structured object / JSON form (prepareSpawnInput)
  const fields = ['subagent', 'permission_mode', 'model', 'effort'];
  for (const field of fields) {
    assert.throws(
      () => prepareSpawnInput({ profile: 'my-prof', [field]: 'val', repository: '/repo', prompt: 'hi' }),
      (error) => {
        assert.ok(error instanceof CliError);
        assert.equal(error.code, 'INVALID_ARGUMENT');
        assert.equal(error.exitCode, 2);
        assert.match(error.message, /profile cannot be combined with/u);
        return true;
      },
    );
  }
});

test('AC2: non-profile spawn retains existing behavior for all four flags', () => {
  const parsed = parseSpawnArgs([
    '--subagent', 'dsh',
    '--permission-mode', 'plan',
    '--model', 'deepseek:deepseek-chat',
    '--effort', 'medium',
    '--repository', '/repo',
    '--prompt', 'test prompt',
  ]);
  assert.equal(parsed.subagent, 'dsh');
  assert.equal(parsed.permission_mode, 'plan');
  assert.equal(parsed.model, 'deepseek:deepseek-chat');
  assert.equal(parsed.effort, 'medium');
  assert.equal(parsed.repository, '/repo');
  assert.equal(parsed.prompt, 'test prompt');
  assert.equal(Object.hasOwn(parsed, 'profile'), false);
});

test('AC2: invalid profile argument values are rejected', () => {
  assert.throws(() => parseSpawnArgs(['--profile']), /--profile requires a non-null value/u);
  assert.throws(() => parseSpawnArgs(['--profile', '']), /--profile requires a non-null value/u);
  assert.throws(() => parseSpawnArgs(['--profile', '--repository', '/repo']), /--profile requires a non-null value/u);
  assert.throws(() => prepareSpawnInput({ profile: null, repository: '/r', prompt: 'h' }), /profile must be omitted or a non-null profile name; null is invalid/u);
  assert.throws(() => prepareSpawnInput({ profile: '', repository: '/r', prompt: 'h' }), /profile must be a non-empty string/u);
});

// ---------------------------------------------------------------------------
// AC3: 管理面 (profile list / show + 四 fixture 必测 + 告警 + 重名 + 未知)
// ---------------------------------------------------------------------------

test('AC3: four mandatory fixtures parse name = "real" identically and show matches fixture 4', () => {
  const { paths, profilesDir } = createTempEnv();

  // Fixture ①: developer_instructions multiline string containing [example] line, name afterwards
  const content1 = `
developer_instructions = """
[example]
some instructions that mention [table] style lines
name = "fake"
"""
name = "real"
`;
  const f1 = scanProfileToml(content1, 'f1.toml');
  assert.equal(f1.valid, true);
  assert.equal(f1.name, 'real');

  // Fixture ②: "name" = "real" quoted basic key
  const content2 = `"name" = "real"`;
  const f2 = scanProfileToml(content2, 'f2.toml');
  assert.equal(f2.valid, true);
  assert.equal(f2.name, 'real');

  // Fixture ③: name = """real""" multiline basic string value
  const content3 = `name = """real"""`;
  const f3 = scanProfileToml(content3, 'f3.toml');
  assert.equal(f3.valid, true);
  assert.equal(f3.name, 'real');

  // Fixture ④: "na\u006de" = "real" escaped quoted basic key
  const content4 = `"na\\u006de" = "real"`;
  const f4 = scanProfileToml(content4, 'f4.toml');
  assert.equal(f4.valid, true);
  assert.equal(f4.name, 'real');

  // Test list for each fixture individually (ensures each yields available profile "real")
  for (const [idx, content] of [[1, content1], [2, content2], [3, content3], [4, content4]]) {
    const singleEnv = createTempEnv();
    fs.writeFileSync(path.join(singleEnv.profilesDir, `test-${idx}.toml`), content);
    const listRes = listProfiles(singleEnv.paths);
    assert.deepEqual(listRes.profiles, ['real'], `fixture ${idx} failed to yield 'real'`);
    assert.deepEqual(listRes.warnings, [], `fixture ${idx} unexpectedly reported warnings`);
    fs.rmSync(singleEnv.home, { recursive: true, force: true });
  }

  // Write Fixture ④ into profiles directory: show real MUST hit this file
  const f4Path = path.join(profilesDir, 'escaped_key.toml');
  fs.writeFileSync(f4Path, content4);
  const showRes = showProfile(paths, 'real');
  assert.equal(showRes.operation, 'show');
  assert.equal(showRes.name, 'real');
  assert.equal(showRes.file, f4Path);
  assert.equal(showRes.content, content4);

  fs.rmSync(paths.home, { recursive: true, force: true });
});

test('AC3: missing profiles directory returns empty list with path note', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'missing-profiles-dir-'));
  const paths = productPaths(home);
  // Do NOT create profiles directory
  const res = listProfiles(paths);
  assert.equal(res.operation, 'list');
  assert.deepEqual(res.profiles, []);
  assert.deepEqual(res.warnings, []);
  assert.match(res.message, /profiles directory does not exist/u);
  assert.ok(res.directory.endsWith(path.join('Application Support', 'external-subagent', 'profiles')));

  // show on missing directory
  assert.throws(
    () => showProfile(paths, 'anything'),
    (err) => {
      assert.equal(err.code, 'INVALID_ARGUMENT');
      assert.equal(err.exitCode, 2);
      assert.match(err.message, /profile 'anything' not found; available profiles: none/u);
      return true;
    },
  );
  fs.rmSync(home, { recursive: true, force: true });
});

test('AC3: empty profiles directory returns empty list', () => {
  const { paths } = createTempEnv();
  const res = listProfiles(paths);
  assert.equal(res.operation, 'list');
  assert.deepEqual(res.profiles, []);
  assert.deepEqual(res.warnings, []);
  fs.rmSync(paths.home, { recursive: true, force: true });
});

test('AC3: warnings are reported and problematic files excluded from available profiles', () => {
  const { paths, profilesDir } = createTempEnv();

  // Valid profile
  fs.writeFileSync(path.join(profilesDir, 'good.toml'), 'name = "valid-profile"\nsubagent = "codex"');
  // Broken TOML syntax
  fs.writeFileSync(path.join(profilesDir, 'broken.toml'), 'name = "broken');
  // Unknown top-level key
  fs.writeFileSync(path.join(profilesDir, 'unknown_key.toml'), 'name = "bad-key"\nunknown_field = 123');
  // Missing name
  fs.writeFileSync(path.join(profilesDir, 'no_name.toml'), 'subagent = "zcode"');
  // Name not a string
  fs.writeFileSync(path.join(profilesDir, 'num_name.toml'), 'name = 42');
  // Empty name
  fs.writeFileSync(path.join(profilesDir, 'empty_name.toml'), 'name = "   "');
  // Oversized name (>128 bytes)
  fs.writeFileSync(path.join(profilesDir, 'huge_name.toml'), `name = "${'x'.repeat(129)}"`);
  // Name containing NUL byte
  fs.writeFileSync(path.join(profilesDir, 'nul_name.toml'), 'name = "nul\\u0000name"');
  // Top-level table header
  fs.writeFileSync(path.join(profilesDir, 'table.toml'), '[unsupported_section]\nname = "in_table"');
  // Dotted name key
  fs.writeFileSync(path.join(profilesDir, 'dotted.toml'), 'name.sub = "dotted"');

  const listRes = listProfiles(paths);
  // Only valid-profile should be in profiles
  assert.deepEqual(listRes.profiles, ['valid-profile']);
  // All others are in warnings
  assert.equal(listRes.warnings.length, 9);
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('broken.toml')));
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('unknown_key.toml') && w.diagnostic.includes('unknown top-level key')));
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('no_name.toml') && w.diagnostic.includes('missing required field')));
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('num_name.toml') && w.diagnostic.includes('must be a string')));
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('empty_name.toml') && w.diagnostic.includes('cannot be empty')));
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('huge_name.toml') && w.diagnostic.includes('exceeds 128 bytes')));
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('nul_name.toml') && w.diagnostic.includes('cannot contain NUL byte')));
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('table.toml') && w.diagnostic.includes('unexpected table header')));
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('dotted.toml') && w.diagnostic.includes('dotted key')));

  // show unknown lists available profiles and includes broken files diagnostic
  assert.throws(
    () => showProfile(paths, 'nonexistent'),
    (err) => {
      assert.equal(err.code, 'INVALID_ARGUMENT');
      assert.equal(err.exitCode, 2);
      assert.match(err.message, /profile 'nonexistent' not found/u);
      assert.match(err.message, /directory contains invalid files/u);
      assert.match(err.message, /available profiles: \[valid-profile\]/u);
      return true;
    },
  );

  fs.rmSync(paths.home, { recursive: true, force: true });
});

test('AC3: duplicate profiles are evicted from list with warnings and trigger ambiguity error in show', () => {
  const { paths, profilesDir } = createTempEnv();

  fs.writeFileSync(path.join(profilesDir, 'a.toml'), 'name = "duplicate_worker"\nmodel = "gpt-5"');
  fs.writeFileSync(path.join(profilesDir, 'b.toml'), 'name = "duplicate_worker"\nmodel = "claude-4"');
  fs.writeFileSync(path.join(profilesDir, 'c.toml'), 'name = "unique_worker"\nmodel = "gemini-2"');

  const listRes = listProfiles(paths);
  // duplicate_worker evicted; only unique_worker is available
  assert.deepEqual(listRes.profiles, ['unique_worker']);
  assert.equal(listRes.warnings.length, 2);
  assert.ok(listRes.warnings.every((w) => w.diagnostic.includes('duplicate profile name \'duplicate_worker\'')));

  // show on duplicate_worker reports ambiguity error with both conflicting files
  assert.throws(
    () => showProfile(paths, 'duplicate_worker'),
    (err) => {
      assert.equal(err.code, 'INVALID_ARGUMENT');
      assert.equal(err.exitCode, 2);
      assert.match(err.message, /profile 'duplicate_worker' is ambiguous; defined in multiple files:/u);
      assert.match(err.message, /a\.toml/u);
      assert.match(err.message, /b\.toml/u);
      return true;
    },
  );

  // show on unique_worker succeeds
  const showRes = showProfile(paths, 'unique_worker');
  assert.equal(showRes.name, 'unique_worker');
  assert.equal(showRes.file, path.join(profilesDir, 'c.toml'));

  fs.rmSync(paths.home, { recursive: true, force: true });
});

test('AC3: directory resolution mirrors daemon env priority', () => {
  const customConfigDir = fs.mkdtempSync(path.join(os.tmpdir(), 'custom-config-sibling-'));
  const siblingProfiles = path.join(customConfigDir, 'profiles');
  fs.mkdirSync(siblingProfiles, { recursive: true });
  fs.writeFileSync(path.join(siblingProfiles, 'custom.toml'), 'name = "custom-profile"');

  const configPath = path.join(customConfigDir, 'agents.json');
  fs.writeFileSync(configPath, '{}');

  // 1. EXTERNAL_SUBAGENT_CONFIG env
  const resolved1 = profilesDir({ EXTERNAL_SUBAGENT_CONFIG: configPath });
  assert.equal(resolved1, siblingProfiles);

  // 2. ZCODE_AGENT_CONFIG env
  const resolved2 = profilesDir({ ZCODE_AGENT_CONFIG: configPath });
  assert.equal(resolved2, siblingProfiles);

  // 3. Fallback when neither env is set
  const home = '/Users/fakehome';
  const resolvedFallback = profilesDir({}, home);
  assert.equal(resolvedFallback, path.join(home, 'Library', 'Application Support', 'external-subagent', 'profiles'));

  fs.rmSync(customConfigDir, { recursive: true, force: true });
});

// ---------------------------------------------------------------------------
// AC4: 回归 (Byte-for-byte JSON shape equivalence without --profile)
// ---------------------------------------------------------------------------

test('AC4: CLI spawn wire without profile is byte-for-byte identical in JSON keys and values', async () => {
  const socketPath = path.join(os.tmpdir(), `cli-spawn-regress-${process.pid}-${Date.now()}.sock`);
  let receivedJson = '';
  const server = net.createServer((socket) => socket.once('data', (chunk) => {
    receivedJson = chunk.toString('utf8').trim();
    const req = JSON.parse(receivedJson);
    socket.end(JSON.stringify({
      request_id: req.request_id,
      outcome: 'success',
      result: { kind: 'general_submitted', task: { agent_id: '10000003', status: 'running', session_id: null } },
    }) + '\n');
  }));
  await new Promise((resolve) => server.listen(socketPath, resolve));

  try {
    await callDaemon(socketPath, 'spawn', {
      repository: '/my/repo',
      prompt: 'do things',
    });
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }

  const parsed = JSON.parse(receivedJson);
  assert.equal(parsed.method, 'submit_general');
  assert.equal(Object.hasOwn(parsed.params, 'profile'), false);
  // Default permission_mode 'build' is injected in non-profile mode
  assert.equal(parsed.params.manifest.permission_mode, 'build');
  assert.equal(parsed.params.manifest.repository, '/my/repo');
  assert.equal(parsed.params.manifest.prompt, 'do things');
  assert.deepEqual(parsed.params.manifest.write_manifest, []);

  // Assert exact manifest key order
  assert.deepEqual(
    Object.keys(parsed.params.manifest),
    ['schema', 'agent_id', 'repository', 'permission_mode', 'prompt', 'write_manifest'],
  );
});

// ---------------------------------------------------------------------------
// AC5: 发现性与长名单 CLI 出口保真 (Fidelity for >512B profile lists)
// ---------------------------------------------------------------------------

test('AC5: >512 byte ASCII and Unicode profile lists are fully preserved at CLI exit without truncation', () => {
  const { paths, profilesDir } = createTempEnv();

  // 1. ASCII long list (>512 bytes): 40 profiles of ~20 bytes each > 800 bytes
  const asciiNames = [];
  for (let i = 0; i < 40; i++) {
    const name = `standard_ascii_profile_item_${String(i).padStart(3, '0')}`;
    asciiNames.push(name);
    fs.writeFileSync(path.join(profilesDir, `${name}.toml`), `name = "${name}"`);
  }
  asciiNames.sort();

  assert.throws(
    () => showProfile(paths, 'nonexistent_ascii'),
    (err) => {
      assert.equal(err.code, 'INVALID_ARGUMENT');
      assert.equal(err.exitCode, 2);
      assert.ok(Buffer.byteLength(err.message, 'utf8') > 512, 'message must exceed 512 bytes');
      for (const name of asciiNames) {
        assert.ok(err.message.includes(name), `message must contain ${name}`);
      }
      return true;
    },
  );

  // Clean ASCII profiles
  for (const name of asciiNames) {
    fs.rmSync(path.join(profilesDir, `${name}.toml`));
  }

  // 2. Unicode long list (>512 bytes): 30 profiles with multi-byte characters
  const unicodeNames = [];
  for (let i = 0; i < 30; i++) {
    const name = `子代理环境预设_智能调优配置_${String(i).padStart(3, '0')}`;
    unicodeNames.push(name);
    fs.writeFileSync(path.join(profilesDir, `u_${i}.toml`), `name = "${name}"`);
  }
  unicodeNames.sort();

  assert.throws(
    () => showProfile(paths, 'nonexistent_unicode'),
    (err) => {
      assert.equal(err.code, 'INVALID_ARGUMENT');
      assert.equal(err.exitCode, 2);
      assert.ok(Buffer.byteLength(err.message, 'utf8') > 512, 'Unicode message must exceed 512 bytes');
      for (const name of unicodeNames) {
        assert.ok(err.message.includes(name), `Unicode message must contain ${name}`);
      }
      return true;
    },
  );

  fs.rmSync(paths.home, { recursive: true, force: true });
});

// ---------------------------------------------------------------------------
// CLI main entry point tests for profile commands
// ---------------------------------------------------------------------------

test('CLI main dispatches profile list and profile show commands', async () => {
  const { paths, profilesDir } = createTempEnv();
  fs.writeFileSync(path.join(profilesDir, 'my_worker.toml'), 'name = "worker_alpha"');

  const oldEnv = process.env.EXTERNAL_SUBAGENT_CONFIG;
  const configPath = path.join(paths.data, 'config.json');
  process.env.EXTERNAL_SUBAGENT_CONFIG = configPath;

  const stdoutChunks = [];
  const originalWrite = process.stdout.write;
  process.stdout.write = (chunk) => {
    stdoutChunks.push(chunk);
    return true;
  };

  try {
    // 1. profile list
    stdoutChunks.length = 0;
    await main(['profile', 'list']);
    const listOutput = JSON.parse(stdoutChunks.join(''));
    assert.equal(listOutput.ok, true);
    assert.equal(listOutput.operation, 'list');
    assert.deepEqual(listOutput.profiles, ['worker_alpha']);

    // 2. profile show worker_alpha
    stdoutChunks.length = 0;
    await main(['profile', 'show', 'worker_alpha']);
    const showOutput = JSON.parse(stdoutChunks.join(''));
    assert.equal(showOutput.ok, true);
    assert.equal(showOutput.operation, 'show');
    assert.equal(showOutput.name, 'worker_alpha');
    assert.match(showOutput.content, /name = "worker_alpha"/u);

    // 3. profile show nonexistent throws CliError
    await assert.rejects(
      () => main(['profile', 'show', 'not_there']),
      (err) => err instanceof CliError && err.code === 'INVALID_ARGUMENT' && err.message.includes('available profiles: [worker_alpha]'),
    );

    // 4. invalid profile operations throw CliError
    await assert.rejects(() => main(['profile']), (err) => err.code === 'INVALID_ARGUMENT');
    await assert.rejects(() => main(['profile', 'unknown_op']), (err) => err.code === 'INVALID_ARGUMENT');
    await assert.rejects(() => main(['profile', 'list', 'extra']), (err) => err.code === 'INVALID_ARGUMENT');
    await assert.rejects(() => main(['profile', 'show']), (err) => err.code === 'INVALID_ARGUMENT');
  } finally {
    process.stdout.write = originalWrite;
    if (oldEnv !== undefined) process.env.EXTERNAL_SUBAGENT_CONFIG = oldEnv;
    else delete process.env.EXTERNAL_SUBAGENT_CONFIG;
    fs.rmSync(paths.home, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// S02 repair wave 1: shared differential corpus and fix-specific regressions
// ---------------------------------------------------------------------------

test('corpus: scanner output matches the shared TOML differential corpus item-for-item', () => {
  const files = fs.readdirSync(CORPUS_DIR).filter((file) => file.endsWith('.toml')).sort();
  assert.ok(files.length > 0, 'corpus must not be empty');
  for (const file of files) {
    const stem = file.slice(0, -'.toml'.length);
    const expected = JSON.parse(fs.readFileSync(path.join(CORPUS_DIR, `${stem}.expected.json`), 'utf8'));
    const bytes = fs.readFileSync(path.join(CORPUS_DIR, file));
    if (expected.reason === 'read-error') {
      // Byte-level corpus items are not decodable UTF-8; the daemon's
      // `fs::read_to_string` fails and skips the file without an owner.
      assert.throws(
        () => new TextDecoder('utf-8', { fatal: true }).decode(bytes),
        `${stem}: expected invalid UTF-8`,
      );
      assert.equal(expected.valid, false, `${stem}: read-error must not be valid`);
      assert.equal(expected.name, null, `${stem}: read-error must not own a name`);
      continue;
    }
    const content = new TextDecoder('utf-8', { fatal: true }).decode(bytes);
    const result = scanProfileToml(content, file);
    assert.equal(result.valid, expected.valid, `${stem}: valid mismatch`);
    assert.equal(result.name ?? null, expected.name, `${stem}: name mismatch`);
    if (expected.valid) {
      assert.deepEqual(result.errors, [], `${stem}: valid item must carry no diagnostics`);
    } else {
      const marker = REASON_MARKERS[expected.reason];
      assert.ok(marker, `${stem}: unknown reason category ${expected.reason}`);
      assert.ok(
        result.errors.some((error) => error.includes(marker)),
        `${stem}: diagnostics ${JSON.stringify(result.errors)} must expose reason ${expected.reason}`,
      );
    }
  }
});

test('fix3: only TOML-syntax-valid files own a name; broken files never evict valid ones', () => {
  // good + broken sharing the same name, both orders.
  const order1 = createTempEnv();
  fs.writeFileSync(path.join(order1.profilesDir, 'a-broken.toml'), 'name = "real"\nmodel = "unclosed\n');
  fs.writeFileSync(path.join(order1.profilesDir, 'b-good.toml'), 'name = "real"\nsubagent = "zcode"\n');
  const res1 = listProfiles(order1.paths);
  assert.deepEqual(res1.profiles, ['real']);
  assert.equal(res1.warnings.length, 1);
  assert.ok(res1.warnings[0].file.endsWith('a-broken.toml'));
  fs.rmSync(order1.home, { recursive: true, force: true });

  const order2 = createTempEnv();
  fs.writeFileSync(path.join(order2.profilesDir, 'a-good.toml'), 'name = "real"\nsubagent = "zcode"\n');
  fs.writeFileSync(path.join(order2.profilesDir, 'b-broken.toml'), 'name = "real"\nmodel = "unclosed\n');
  const res2 = listProfiles(order2.paths);
  assert.deepEqual(res2.profiles, ['real']);
  assert.equal(res2.warnings.length, 1);
  assert.ok(res2.warnings[0].file.endsWith('b-broken.toml'));
  fs.rmSync(order2.home, { recursive: true, force: true });

  // Three files: a valid owner, a syntax-broken same-name file (no owner), and a
  // structurally valid but field-invalid same-name file (still an owner per S01).
  const three = createTempEnv();
  fs.writeFileSync(path.join(three.profilesDir, 'good.toml'), 'name = "real"\nsubagent = "zcode"\n');
  fs.writeFileSync(path.join(three.profilesDir, 'broken.toml'), 'name = "real"\nmodel = "unclosed\n');
  fs.writeFileSync(path.join(three.profilesDir, 'field_error.toml'), 'name = "real"\npermission_mode = "superuser"\n');
  const res3 = listProfiles(three.paths);
  // good and field_error both own "real" -> collision evicts the name entirely;
  // broken contributes only its syntax diagnostic.
  assert.deepEqual(res3.profiles, []);
  assert.equal(res3.warnings.length, 3);
  assert.ok(res3.warnings.some((w) => w.file.endsWith('broken.toml')));
  const duplicates = res3.warnings.filter((w) => w.diagnostic.includes("duplicate profile name 'real'"));
  assert.equal(duplicates.length, 2);
  assert.ok(duplicates.some((w) => w.file.endsWith('good.toml')));
  assert.ok(duplicates.some((w) => w.file.endsWith('field_error.toml')));
  fs.rmSync(three.home, { recursive: true, force: true });
});

test('fix-F1: table headers do not hide later syntax damage; conflicts erase the owner', () => {
  // A good top-level profile plus two same-named files that are damaged only
  // after a (valid) table header. Both damaged files must fail the full-document
  // syntax check and therefore not own "real", leaving the good file usable.
  const { paths, profilesDir } = createTempEnv();
  fs.writeFileSync(path.join(profilesDir, 'good.toml'), 'name = "real"\nsubagent = "zcode"\n');
  fs.writeFileSync(path.join(profilesDir, 'a-broken-table.toml'), 'name = "real"\n[extra]\nx = "unclosed');
  fs.writeFileSync(path.join(profilesDir, 'b-namespace.toml'), 'name = "real"\nname.x = 1\n');
  const res = listProfiles(paths);
  assert.deepEqual(res.profiles, ['real']);
  assert.equal(res.warnings.length, 2);
  assert.ok(res.warnings.some((w) => w.file.endsWith('a-broken-table.toml') && w.diagnostic.includes('Unterminated basic string')));
  assert.ok(res.warnings.some((w) => w.file.endsWith('b-namespace.toml') && w.diagnostic.includes('namespace conflict')));
  fs.rmSync(paths.home, { recursive: true, force: true });

  // A scalar followed by a table of the same name is a syntax error, so a name
  // defined earlier must not survive as an owner.
  const scalar = scanProfileToml('name = "real"\nx = 1\n[x]\n', 'scalar.toml');
  assert.equal(scalar.syntaxValid, false);
  assert.equal(scalar.name, null);
});

test('fix-F4: invalid UTF-8 bytes are a read warning and never own a name', () => {
  const { paths, profilesDir } = createTempEnv();
  fs.writeFileSync(path.join(profilesDir, 'good.toml'), 'name = "real"\nsubagent = "zcode"\n');
  // Same top-level name, but the raw bytes are not valid UTF-8: the daemon's
  // `fs::read_to_string` fails, so this file must not evict the good owner.
  fs.writeFileSync(path.join(profilesDir, 'bad-bytes.toml'), Buffer.from('name = "real"\n# \xff\n', 'latin1'));
  const res = listProfiles(paths);
  assert.deepEqual(res.profiles, ['real']);
  assert.equal(res.warnings.length, 1);
  assert.ok(res.warnings[0].file.endsWith('bad-bytes.toml'));
  assert.ok(res.warnings[0].diagnostic.includes('is unreadable'));
  fs.rmSync(paths.home, { recursive: true, force: true });
});

test('fix-F3: non-string field values keep the name owner but are not usable', () => {
  const { paths, profilesDir } = createTempEnv();
  fs.writeFileSync(path.join(profilesDir, 'a-typed.toml'), 'name = "real"\nmodel = true\n');
  const single = listProfiles(paths);
  assert.deepEqual(single.profiles, []);
  assert.equal(single.warnings.length, 1);
  assert.ok(single.warnings[0].diagnostic.includes('must be a string'));

  // The daemon keeps `validated_name` for RawProfile field-type failures, so a
  // field-type-invalid file still owns its name and evicts a valid same name.
  fs.writeFileSync(path.join(profilesDir, 'b-good.toml'), 'name = "real"\nsubagent = "zcode"\n');
  const collide = listProfiles(paths);
  assert.deepEqual(collide.profiles, []);
  assert.equal(collide.warnings.filter((w) => w.diagnostic.includes("duplicate profile name 'real'")).length, 2);
  fs.rmSync(paths.home, { recursive: true, force: true });
});

test('fix2/fix4: CRLF and LF names collide; malformed primitives and surrogates only warn', () => {
  const { paths, profilesDir } = createTempEnv();
  // LF and CRLF spell the same decoded name "dup".
  fs.writeFileSync(path.join(profilesDir, 'a-lf.toml'), 'name = "dup"\nsubagent = "zcode"\n');
  fs.writeFileSync(path.join(profilesDir, 'b-crlf.toml'), 'name = "dup"\r\nsubagent = "zcode"\r\n');
  const dup = listProfiles(paths);
  assert.deepEqual(dup.profiles, []);
  assert.equal(dup.warnings.filter((w) => w.diagnostic.includes("duplicate profile name 'dup'")).length, 2);
  for (const name of ['a-lf.toml', 'b-crlf.toml']) fs.rmSync(path.join(profilesDir, name));

  // Illegal bare primitive and a NUL-containing name are only warnings.
  fs.writeFileSync(path.join(profilesDir, 'good.toml'), 'name = "ok"\nsubagent = "zcode"\n');
  fs.writeFileSync(path.join(profilesDir, 'bad_primitive.toml'), 'name = "prim"\nmodel = unquoted\n');
  fs.writeFileSync(path.join(profilesDir, 'bad_surrogate.toml'), 'name = "sur\\uD800"\n');
  const res = listProfiles(paths);
  assert.deepEqual(res.profiles, ['ok']);
  assert.equal(res.warnings.length, 2);
  assert.ok(res.warnings.some((w) => w.diagnostic.includes('Invalid TOML primitive')));
  assert.ok(res.warnings.some((w) => w.diagnostic.includes('Invalid Unicode code point')));
  fs.rmSync(paths.home, { recursive: true, force: true });
});

test('fix5: directory scan follows symlinked .toml like the daemon', { skip: process.platform === 'win32' }, () => {
  const { paths, profilesDir } = createTempEnv();
  const outside = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-links-'));
  const target = path.join(outside, 'target.toml');
  fs.writeFileSync(target, 'name = "linked"\n');
  fs.symlinkSync(target, path.join(profilesDir, 'link.toml'));
  assert.deepEqual(listProfiles(paths).profiles, ['linked']);

  // Two links to different files with the same decoded name must collide.
  const dupA = path.join(outside, 'dup-a.toml');
  const dupB = path.join(outside, 'dup-b.toml');
  fs.writeFileSync(dupA, 'name = "duplink"\n');
  fs.writeFileSync(dupB, 'name = "duplink"\n');
  fs.symlinkSync(dupA, path.join(profilesDir, 'duplink-a.toml'));
  fs.symlinkSync(dupB, path.join(profilesDir, 'duplink-b.toml'));
  const res = listProfiles(paths);
  assert.deepEqual(res.profiles, ['linked']);
  assert.equal(res.warnings.filter((w) => w.diagnostic.includes("duplicate profile name 'duplink'")).length, 2);

  // A dangling link is not a file for the daemon either; it is silently skipped.
  fs.symlinkSync(path.join(outside, 'does-not-exist.toml'), path.join(profilesDir, 'dangling.toml'));
  assert.deepEqual(listProfiles(paths).profiles, ['linked']);

  fs.rmSync(outside, { recursive: true, force: true });
  fs.rmSync(paths.home, { recursive: true, force: true });
});

test('fix6: profiles directory follows daemon var_os presence, not truthiness', () => {
  const configDir = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-env-'));
  const configPath = path.join(configDir, 'agents.json');
  fs.mkdirSync(path.join(configDir, 'profiles'), { recursive: true });
  const sibling = path.join(configDir, 'profiles');

  // Presence selects the variable; an exported empty value means "no directory".
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: '' }), null);
  assert.equal(profilesDir({ ZCODE_AGENT_CONFIG: '' }), null);
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: '', ZCODE_AGENT_CONFIG: configPath }), null);
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: configPath, ZCODE_AGENT_CONFIG: '' }), sibling);
  assert.equal(profilesDir({ ZCODE_AGENT_CONFIG: configPath }), sibling);
  assert.equal(profilesDir({}), path.join(productPaths(os.homedir()).data, 'profiles'));

  // profileCommand honours the same presence semantics with explicit injection.
  const { paths, profilesDir: realProfiles } = createTempEnv();
  fs.writeFileSync(path.join(realProfiles, 'x.toml'), 'name = "x"\n');
  const shadowed = listProfiles(paths, { EXTERNAL_SUBAGENT_CONFIG: '', ZCODE_AGENT_CONFIG: configPath });
  assert.deepEqual(shadowed.profiles, []);
  assert.match(shadowed.message, /disabled/u);
  assert.throws(() => showProfile(paths, 'x', { EXTERNAL_SUBAGENT_CONFIG: '' }), /available profiles: none/u);
  assert.deepEqual(listProfiles(paths, { EXTERNAL_SUBAGENT_CONFIG: path.join(paths.data, 'config.json') }).profiles, ['x']);

  fs.rmSync(configDir, { recursive: true, force: true });
  fs.rmSync(paths.home, { recursive: true, force: true });
});

test('fix7: real CLI spawn process preserves >512B ASCII and Unicode daemon error lists', async () => {
  const asciiNames = Array.from({ length: 40 }, (_, i) => `standard_ascii_profile_item_${String(i).padStart(3, '0')}`).sort();
  const asciiMessage = `profile 'missing' not found; available profiles: [${asciiNames.join(', ')}]`;
  assert.ok(Buffer.byteLength(asciiMessage, 'utf8') > 512);

  const asciiEnv = createTempEnv();
  const asciiSocket = path.join(asciiEnv.home, 'mock.sock');
  try {
    const result = await withMockErrorDaemon(asciiSocket, asciiMessage, () => runCli(
      ['spawn', '--profile', 'missing', '--repository', '/repo', '--prompt', 'hi'],
      { ...process.env, EXTERNAL_SUBAGENT_SOCKET: asciiSocket },
    ));
    assert.equal(result.code, 1, `stderr: ${result.stderr}`);
    const parsed = JSON.parse(result.stderr);
    assert.equal(parsed.ok, false);
    assert.equal(parsed.error.code, 'validation');
    assert.equal(parsed.error.message, asciiMessage, 'ASCII list must round-trip byte-for-byte');
    for (const name of asciiNames) assert.ok(parsed.error.message.includes(name), `missing ${name}`);
    assert.ok(!parsed.error.message.includes('\uFFFD'), 'ASCII message must be valid UTF-8');
  } finally {
    fs.rmSync(asciiEnv.home, { recursive: true, force: true });
  }

  const unicodeNames = Array.from({ length: 30 }, (_, i) => `子代理环境预设_智能调优配置_${String(i).padStart(3, '0')}`).sort();
  const unicodeMessage = `profile 'missing' not found; available profiles: [${unicodeNames.join(', ')}]`;
  assert.ok(Buffer.byteLength(unicodeMessage, 'utf8') > 512);

  const unicodeEnv = createTempEnv();
  const unicodeSocket = path.join(unicodeEnv.home, 'mock.sock');
  try {
    const result = await withMockErrorDaemon(unicodeSocket, unicodeMessage, () => runCli(
      ['spawn', '--profile', 'missing', '--repository', '/repo', '--prompt', 'hi'],
      { ...process.env, EXTERNAL_SUBAGENT_SOCKET: unicodeSocket },
    ));
    assert.equal(result.code, 1, `stderr: ${result.stderr}`);
    const parsed = JSON.parse(result.stderr);
    assert.equal(parsed.error.message, unicodeMessage, 'Unicode list must round-trip on character boundaries');
    for (const name of unicodeNames) assert.ok(parsed.error.message.includes(name), `missing ${name}`);
    assert.ok(!parsed.error.message.includes('\uFFFD'), 'Unicode message must not corrupt multi-byte characters');
  } finally {
    fs.rmSync(unicodeEnv.home, { recursive: true, force: true });
  }
});

test('fix8: HELP documents that warned and duplicate files are excluded from available names', () => {
  assert.match(HELP, /Inspect global spawn profiles/u);
  assert.match(HELP, /path-based warnings/u);
  assert.match(HELP, /excluded from the available\s+name set/u);
});
