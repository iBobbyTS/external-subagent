import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

import { CliError } from '../../cli/errors.mjs';
import { HELP, main } from '../../cli/main.mjs';
import { productPaths, profilesDir, platform } from '../../cli/paths.mjs';
import { callDaemon } from '../../cli/rpc.mjs';
import {
  parseSpawnArgs,
  prepareSpawnInput,
  profileCommand,
  scanProfileJson,
} from '../../cli/commands/tasks.mjs';

const CORPUS_DIR = path.resolve(import.meta.dirname, 'profiles-corpus');
const CLI_BIN = path.resolve(import.meta.dirname, '../../bin/external-subagent.mjs');
const NO_ENV = Object.freeze({});

// The category stored in each corpus expectation maps to a substring that the
// scanner's diagnostic must contain, so failures point at the right rule.
const REASON_MARKERS = Object.freeze({
  invalid_json: 'invalid JSON',
  invalid_unicode: 'lone surrogate',
  missing_name: "missing required field 'name'",
  name_not_string: "field 'name': must be a string",
  empty_name: "field 'name': cannot be empty",
  oversize_name: "field 'name': exceeds 128 bytes",
  nul_name: "field 'name': cannot contain NUL byte",
  unknown_key: 'unknown top-level key',
  field_type: 'must be a string',
  not_object: 'must be an object',
});

function createTempEnv() {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-profiles-'));
  const paths = productPaths(home);
  const profilesPath = path.join(paths.data, 'profiles');
  fs.mkdirSync(profilesPath, { recursive: true });
  return { home, paths, profilesDir: profilesPath };
}

// The product data profiles directory for the host layout: macOS
// `~/Library/Application Support`, else the XDG data root.
function expectedProfilesDir(home) {
  return platform() === 'darwin'
    ? path.join(home, 'Library', 'Application Support', 'external-subagent', 'profiles')
    : path.join(home, '.local', 'share', 'external-subagent', 'profiles');
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

// Strict, BOM-preserving decode identical to the one `scanProfilesDir` uses.
function decodeProfileBytes(bytes) {
  return new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(bytes);
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
        assert.match(error.message, /specify these in the profile JSON or omit profile/u);
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
// AC3: 管理面 (profile list / show + warnings + duplicates + 未知)
// ---------------------------------------------------------------------------

test('AC3: missing profiles directory returns empty list with path note', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'missing-profiles-dir-'));
  const paths = productPaths(home);
  // Do NOT create profiles directory
  const res = listProfiles(paths);
  assert.equal(res.operation, 'list');
  assert.deepEqual(res.profiles, []);
  assert.deepEqual(res.warnings, []);
  assert.match(res.message, /profiles directory does not exist/u);
  assert.match(res.hint, /no profiles found/u);
  assert.match(res.hint, /"Spawn profiles"/u);
  assert.ok(res.directory.endsWith(path.join('external-subagent', 'profiles')));
  assert.equal(res.directory, expectedProfilesDir(home));

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
  assert.match(res.hint, /no profiles found/u);
  assert.match(res.hint, /"name":"codex-yolo"/u);
  fs.rmSync(paths.home, { recursive: true, force: true });
});

test('AC3: hint appears when every file is invalid and is absent once a profile is usable', () => {
  const { paths, profilesDir } = createTempEnv();

  fs.writeFileSync(path.join(profilesDir, 'broken.json'), '{"name": "broken"');
  const invalidOnly = listProfiles(paths);
  assert.deepEqual(invalidOnly.profiles, []);
  assert.equal(invalidOnly.warnings.length, 1);
  assert.match(invalidOnly.hint, /no profiles found/u);

  fs.writeFileSync(path.join(profilesDir, 'good.json'), '{"name":"usable","subagent":"codex"}');
  const withUsable = listProfiles(paths);
  assert.deepEqual(withUsable.profiles, ['usable']);
  assert.equal(Object.hasOwn(withUsable, 'hint'), false);

  fs.rmSync(paths.home, { recursive: true, force: true });
});

test('AC3: warnings are reported and problematic files excluded from available profiles', () => {
  const { paths, profilesDir } = createTempEnv();

  fs.writeFileSync(path.join(profilesDir, 'good.json'), '{"name":"valid-profile","subagent":"codex"}');
  // Invalid JSON syntax
  fs.writeFileSync(path.join(profilesDir, 'broken.json'), '{"name": "broken"');
  // Unknown top-level key (retains the name owner but is not usable)
  fs.writeFileSync(path.join(profilesDir, 'unknown_key.json'), '{"name":"bad-key","unknown_field":123}');
  // Missing name
  fs.writeFileSync(path.join(profilesDir, 'no_name.json'), '{"subagent":"zcode"}');
  // Name not a string
  fs.writeFileSync(path.join(profilesDir, 'num_name.json'), '{"name":42}');
  // Empty name
  fs.writeFileSync(path.join(profilesDir, 'empty_name.json'), '{"name":"   "}');
  // Oversized name (>128 bytes)
  fs.writeFileSync(path.join(profilesDir, 'huge_name.json'), `{"name":"${'x'.repeat(129)}"}`);
  // Name containing NUL
  fs.writeFileSync(path.join(profilesDir, 'nul_name.json'), '{"name":"nul\\u0000name"}');
  // Non-object top level
  fs.writeFileSync(path.join(profilesDir, 'not_object.json'), '[{"name":"x"}]');
  // Field type error (retains the name owner but is not usable)
  fs.writeFileSync(path.join(profilesDir, 'field_type.json'), '{"name":"real","model":true}');

  const listRes = listProfiles(paths);
  assert.deepEqual(listRes.profiles, ['valid-profile']);
  assert.equal(listRes.warnings.length, 9);
  assert.equal(Object.hasOwn(listRes, 'hint'), false);
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('broken.json') && w.diagnostic.includes('invalid JSON')));
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('unknown_key.json') && w.diagnostic.includes('unknown top-level key')));
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('no_name.json') && w.diagnostic.includes('missing required field')));
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('num_name.json') && w.diagnostic.includes('must be a string')));
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('empty_name.json') && w.diagnostic.includes('cannot be empty')));
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('huge_name.json') && w.diagnostic.includes('exceeds 128 bytes')));
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('nul_name.json') && w.diagnostic.includes('cannot contain NUL byte')));
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('not_object.json') && w.diagnostic.includes('must be an object')));
  assert.ok(listRes.warnings.some((w) => w.file.endsWith('field_type.json') && w.diagnostic.includes('must be a string')));

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

  fs.writeFileSync(path.join(profilesDir, 'a.json'), '{"name":"duplicate_worker","model":"gpt-5"}');
  fs.writeFileSync(path.join(profilesDir, 'b.json'), '{"name":"duplicate_worker","model":"claude-4"}');
  fs.writeFileSync(path.join(profilesDir, 'c.json'), '{"name":"unique_worker","model":"gemini-2"}');

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
      assert.match(err.message, /a\.json/u);
      assert.match(err.message, /b\.json/u);
      return true;
    },
  );

  // show on unique_worker succeeds
  const showRes = showProfile(paths, 'unique_worker');
  assert.equal(showRes.name, 'unique_worker');
  assert.equal(showRes.file, path.join(profilesDir, 'c.json'));

  fs.rmSync(paths.home, { recursive: true, force: true });
});

test('AC3: directory resolution mirrors daemon env priority', () => {
  const customConfigDir = fs.mkdtempSync(path.join(os.tmpdir(), 'custom-config-sibling-'));
  const siblingProfiles = path.join(customConfigDir, 'profiles');
  fs.mkdirSync(siblingProfiles, { recursive: true });
  fs.writeFileSync(path.join(siblingProfiles, 'custom.json'), '{"name":"custom-profile"}');

  const configPath = path.join(customConfigDir, 'agents.json');
  fs.writeFileSync(configPath, '{}');

  // 1. EXTERNAL_SUBAGENT_CONFIG env
  const resolved1 = profilesDir({ EXTERNAL_SUBAGENT_CONFIG: configPath });
  assert.equal(resolved1, siblingProfiles);

  // 2. ZCODE_AGENT_CONFIG env
  const resolved2 = profilesDir({ ZCODE_AGENT_CONFIG: configPath });
  assert.equal(resolved2, siblingProfiles);

  // 3. Fallback when neither env is set (host product data directory)
  const home = '/Users/fakehome';
  const resolvedFallback = profilesDir({}, home);
  assert.equal(resolvedFallback, expectedProfilesDir(home));

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
    fs.writeFileSync(path.join(profilesDir, `${name}.json`), `{"name":"${name}"}`);
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
    fs.rmSync(path.join(profilesDir, `${name}.json`));
  }

  // 2. Unicode long list (>512 bytes): 30 profiles with multi-byte characters
  const unicodeNames = [];
  for (let i = 0; i < 30; i++) {
    const name = `子代理环境预设_智能调优配置_${String(i).padStart(3, '0')}`;
    unicodeNames.push(name);
    fs.writeFileSync(path.join(profilesDir, `u_${i}.json`), `{"name":"${name}"}`);
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
  fs.writeFileSync(path.join(profilesDir, 'my_worker.json'), '{"name":"worker_alpha"}');

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
    assert.match(showOutput.content, /worker_alpha/u);

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
// JSON differential corpus (shared with the daemon's serde_json anchoring test)
// ---------------------------------------------------------------------------

test('corpus: scanner output matches the shared JSON differential corpus item-for-item', () => {
  const files = fs.readdirSync(CORPUS_DIR)
    .filter((file) => file.endsWith('.json') && !file.endsWith('.expected.json'))
    .sort();
  assert.ok(files.length > 0, 'corpus must not be empty');
  for (const file of files) {
    const stem = file.slice(0, -'.json'.length);
    const expected = JSON.parse(fs.readFileSync(path.join(CORPUS_DIR, `${stem}.expected.json`), 'utf8'));
    const bytes = fs.readFileSync(path.join(CORPUS_DIR, file));
    if (expected.reason === 'read-error') {
      // Byte-level corpus items are not decodable UTF-8; the daemon's
      // `fs::read_to_string` fails and skips the file without an owner.
      assert.throws(() => decodeProfileBytes(bytes), `${stem}: expected invalid UTF-8`);
      assert.equal(expected.valid, false, `${stem}: read-error must not be valid`);
      assert.equal(expected.name, null, `${stem}: read-error must not own a name`);
      continue;
    }
    const content = decodeProfileBytes(bytes);
    const result = scanProfileJson(content, file);
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

// ---------------------------------------------------------------------------
// Discovery-side regressions: owner identity, paths, candidate selection
// ---------------------------------------------------------------------------

test('N5: profilesDir mirrors Rust Path::parent()+join() without lexical folding', () => {
  // `..` is preserved (Node's path.join would fold it away).
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: 'a/../agents.json' }), 'a/../profiles');
  // A bare relative name (or ".") has the empty path as parent -> "profiles".
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: 'agents.json' }), 'profiles');
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: '.' }), 'profiles');
  // "./name" has "." as parent, which is preserved verbatim.
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: './agents.json' }), './profiles');
  // Nested and trailing separators.
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: 'a/b/agents.json' }), 'a/b/profiles');
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: 'a/b/' }), 'a/profiles');
  // Root has no parent in Rust.
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: '/' }), null);
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: '//' }), null);
  // Empty exported path -> no parent -> null.
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: '' }), null);
});

test('N5: rustPathParent drops non-leading trailing "." components like Rust Path::components', () => {
  // Rust: `Path::new("a/.").parent() == Some("")` -> "profiles".
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: 'a/.' }), 'profiles');
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: 'a/b/.' }), 'a/profiles');
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: '/.' }), null);
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: 'a/./.' }), 'profiles');
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: './.' }), 'profiles');
  // `..` is retained; only a non-leading `.` component is dropped.
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: 'a/../.' }), 'a/profiles');
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: 'a/..' }), 'a/profiles');
  assert.equal(profilesDir({ EXTERNAL_SUBAGENT_CONFIG: 'a//.' }), 'profiles');
});

test('N5: candidate paths do not fold symlink/.. (daemon DirEntry::path() equivalence)', { skip: process.platform === 'win32' }, () => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-nofold-'));
  const elsewhere = path.join(base, 'elsewhere');
  fs.mkdirSync(path.join(elsewhere, 'subdir'), { recursive: true });
  fs.mkdirSync(path.join(elsewhere, 'profiles'), { recursive: true });
  fs.writeFileSync(path.join(elsewhere, 'profiles', 'linked.json'), '{"name":"elsewhere_profile"}');
  // A decoy that lexical folding (`path.join`) would have selected instead.
  fs.mkdirSync(path.join(base, 'profiles'), { recursive: true });
  fs.writeFileSync(path.join(base, 'profiles', 'local.json'), '{"name":"local_profile"}');
  fs.symlinkSync(path.join(elsewhere, 'subdir'), path.join(base, 'link'));

  // Build the config path by string concatenation: `path.join` would fold
  // `link/..` to `base` before the filesystem ever sees it.
  const configPath = `${base}/link/../agents.json`;
  const env = { EXTERNAL_SUBAGENT_CONFIG: configPath };
  assert.equal(profilesDir(env), `${base}/link/../profiles`);
  // The filesystem resolves `link` -> elsewhere/subdir and `..` -> elsewhere,
  // so the daemon reads elsewhere/profiles, never the base/profiles decoy.
  const res = profileCommand({}, ['list'], env);
  assert.deepEqual(res.profiles, ['elsewhere_profile']);
  assert.deepEqual(res.warnings, []);
  const shown = profileCommand({}, ['show', 'elsewhere_profile'], env);
  assert.equal(shown.content, '{"name":"elsewhere_profile"}');

  fs.rmSync(base, { recursive: true, force: true });
});

test('N6: candidate selection matches Rust Path::extension() ("json", case-sensitive)', () => {
  const { paths, profilesDir } = createTempEnv();
  fs.writeFileSync(path.join(profilesDir, 'real.json'), '{"name":"real"}');
  // A file named exactly ".json" has no extension per Rust's Path::extension().
  fs.writeFileSync(path.join(profilesDir, '.json'), '{"name":"dotfile"}');
  // Extension comparison is case-sensitive.
  fs.writeFileSync(path.join(profilesDir, 'upper.JSON'), '{"name":"upper"}');
  // Non-JSON extension.
  fs.writeFileSync(path.join(profilesDir, 'note.txt'), '{"name":"txt"}');
  // A directory whose name ends in .json is not a file candidate.
  fs.mkdirSync(path.join(profilesDir, 'dir.json'));

  const res = listProfiles(paths);
  assert.deepEqual(res.profiles, ['real']);
  // Non-candidates are silently skipped, exactly like the daemon's filter.
  assert.deepEqual(res.warnings, []);

  fs.rmSync(paths.home, { recursive: true, force: true });
});

test('JSON alignment: lone surrogate escapes are rejected, valid pairs accepted', () => {
  const high = scanProfileJson('{"name":"real\\uD800"}', 'high.json');
  assert.equal(high.valid, false);
  assert.equal(high.name, null);
  assert.ok(high.errors.some((e) => e.includes('lone surrogate')));

  const low = scanProfileJson('{"name":"real\\uDC00"}', 'low.json');
  assert.equal(low.valid, false);
  assert.equal(low.name, null);
  assert.ok(low.errors.some((e) => e.includes('lone surrogate')));

  // A well-formed surrogate pair decodes to a real astral code point.
  const pair = scanProfileJson('{"name":"emoji \\uD83D\\uDE00 ok"}', 'pair.json');
  assert.equal(pair.valid, true);
  assert.equal(pair.name, 'emoji \u{1F600} ok');
});

test('JSON alignment: a shadowed lone surrogate escape still rejects the document', () => {
  // A later duplicate key hides the bad value from a collapsed-object scan; the
  // raw-text gate sees the first value regardless.
  const shadowed = scanProfileJson('{"name":"\\uD800","name":"real"}', 'dup.json');
  assert.equal(shadowed.valid, false);
  assert.equal(shadowed.name, null);
  assert.ok(shadowed.errors.some((e) => e.includes('lone surrogate')));

  // An escaped backslash followed by "uD800" is literal text, not an escape.
  const literal = scanProfileJson('{"name":"real\\\\uD800"}', 'lit.json');
  assert.equal(literal.valid, true);
  assert.equal(literal.name, 'real\\uD800');
});

test('JSON alignment: raw-number gate matches serde_json f64 fallback (not bignum bounds)', () => {
  // serde_json parses integers beyond u64/i64 as a finite f64, so they are
  // accepted; the number is still a shape error for a string field, so the
  // loose name owner is retained.
  const accepted = ['18446744073709551615', '18446744073709551616', '-9223372036854775808', '-9223372036854775809', '1.7976931348623157e308', '1e-400', '0e400'];
  for (const token of accepted) {
    const res = scanProfileJson(`{"name":"real","model":${token}}`, 'n.json');
    assert.equal(res.name, 'real', `${token}: owner retained`);
    assert.equal(res.valid, false, `${token}: not a valid field type`);
    assert.ok(res.errors.some((e) => e.includes('must be a string')), token);
  }
  // A non-finite f64 conversion rejects the whole document.
  const rejected = ['1e400', '-1e400', '1.7976931348623159e308', '1e309'];
  for (const token of rejected) {
    const res = scanProfileJson(`{"name":"real","model":${token}}`, 'n.json');
    assert.equal(res.name, null, `${token}: no owner`);
    assert.equal(res.valid, false, token);
    assert.ok(res.errors.some((e) => e.includes('number out of range')), token);
  }
  const digitRes = scanProfileJson(`{"name":"real","model":${'9'.repeat(400)}}`, 'big.json');
  assert.equal(digitRes.name, null);
  assert.ok(digitRes.errors.some((e) => e.includes('number out of range')));

  // A duplicate key must not hide an overflowing token from the raw gate.
  const shadowed = scanProfileJson('{"name":"real","model":1e400,"model":"ok"}', 'shadow.json');
  assert.equal(shadowed.name, null);
  assert.ok(shadowed.errors.some((e) => e.includes('number out of range')));
});

test('JSON alignment: container-depth gate mirrors serde_json and never overflows the stack', () => {
  // serde_json's boundary: 127 nested containers accepted, the 128th rejected
  // (de.rs remaining_depth starts at 128 and errors when it reaches 0).
  const ok = `{"name":"deep","x":${'['.repeat(126)}${']'.repeat(126)}}`;
  const okRes = scanProfileJson(ok, 'd127.json');
  assert.equal(okRes.name, 'deep');
  assert.ok(okRes.errors.some((e) => e.includes('unknown top-level key')));

  for (const count of [127, 128, 130]) {
    const res = scanProfileJson(`{"name":"deep","x":${'['.repeat(count)}${']'.repeat(count)}}`, 'deep.json');
    assert.equal(res.name, null, `depth ${count + 1}`);
    assert.equal(res.valid, false);
    assert.ok(res.errors.some((e) => e.includes('recursion limit exceeded')), `depth ${count + 1}`);
  }

  // A 10000-deep document must come back as a diagnostic, never a RangeError.
  assert.doesNotThrow(() => {
    const deep = scanProfileJson('['.repeat(10000) + ']'.repeat(10000), 'over.json');
    assert.equal(deep.valid, false);
    assert.equal(deep.name, null);
    assert.ok(deep.errors.some((e) => e.includes('recursion limit exceeded')));
  });
});

test('JSON alignment: whole-document rejects own no name and never evict a valid same name', () => {
  const { paths, profilesDir } = createTempEnv();
  fs.writeFileSync(path.join(profilesDir, 'good-real.json'), '{"name":"real"}');
  fs.writeFileSync(path.join(profilesDir, 'shadow-number.json'), '{"name":"real","model":1e400}');
  fs.writeFileSync(path.join(profilesDir, 'shadow-depth.json'), `{"name":"real","x":${'['.repeat(127)}${']'.repeat(127)}}`);
  fs.writeFileSync(path.join(profilesDir, 'shadow-surrogate.json'), '{"name":"real\\uD800"}');

  const res = listProfiles(paths);
  assert.deepEqual(res.profiles, ['real']);
  assert.equal(res.warnings.length, 3);
  for (const warning of res.warnings) {
    assert.ok(warning.diagnostic.includes('invalid JSON'), warning.diagnostic);
  }
  assert.equal(showProfile(paths, 'real').name, 'real');

  fs.rmSync(paths.home, { recursive: true, force: true });
});

test('JSON alignment: a leading BOM is invalid JSON and never a candidate', () => {
  // `JSON.parse` rejects a leading BOM, matching serde_json.
  const direct = scanProfileJson('\uFEFF{"name":"bom"}', 'bom.json');
  assert.equal(direct.valid, false);
  assert.equal(direct.name, null);
  assert.ok(direct.errors.some((e) => e.includes('invalid JSON')));

  const { paths, profilesDir } = createTempEnv();
  fs.writeFileSync(path.join(profilesDir, 'real.json'), '{"name":"real"}');
  fs.writeFileSync(path.join(profilesDir, 'bom.json'), Buffer.concat([
    Buffer.from([0xEF, 0xBB, 0xBF]),
    Buffer.from('{"name":"bom"}', 'utf8'),
  ]));
  const res = listProfiles(paths);
  assert.deepEqual(res.profiles, ['real']);
  assert.equal(res.warnings.length, 1);
  assert.ok(res.warnings[0].file.endsWith('bom.json'));
  assert.ok(res.warnings[0].diagnostic.includes('invalid JSON'));

  fs.rmSync(paths.home, { recursive: true, force: true });
});

test('owner retention: field-type-invalid files keep their name and evict a valid same name', () => {
  const { paths, profilesDir } = createTempEnv();
  fs.writeFileSync(path.join(profilesDir, 'a-typed.json'), '{"name":"real","model":true}');
  const single = listProfiles(paths);
  assert.deepEqual(single.profiles, []);
  assert.equal(single.warnings.length, 1);
  assert.ok(single.warnings[0].diagnostic.includes('must be a string'));

  // The daemon keeps the loose `name` owner for RawProfile field-type failures,
  // so this file still owns "real" and evicts a valid same name.
  fs.writeFileSync(path.join(profilesDir, 'b-good.json'), '{"name":"real","subagent":"zcode"}');
  const collide = listProfiles(paths);
  assert.deepEqual(collide.profiles, []);
  assert.equal(collide.warnings.filter((w) => w.diagnostic.includes("duplicate profile name 'real'")).length, 2);
  fs.rmSync(paths.home, { recursive: true, force: true });
});

test('invalid UTF-8 bytes are a read warning and never own a name', () => {
  const { paths, profilesDir } = createTempEnv();
  fs.writeFileSync(path.join(profilesDir, 'good.json'), '{"name":"real"}');
  // Same top-level name, but the raw bytes are not valid UTF-8: the daemon's
  // `fs::read_to_string` fails, so this file must not evict the good owner.
  fs.writeFileSync(path.join(profilesDir, 'bad-bytes.json'), Buffer.from('{"name":"real"}\n\xff', 'latin1'));
  const res = listProfiles(paths);
  assert.deepEqual(res.profiles, ['real']);
  assert.equal(res.warnings.length, 1);
  assert.ok(res.warnings[0].file.endsWith('bad-bytes.json'));
  assert.ok(res.warnings[0].diagnostic.includes('is unreadable'));
  fs.rmSync(paths.home, { recursive: true, force: true });
});

test('directory scan follows symlinked .json like the daemon', { skip: process.platform === 'win32' }, () => {
  const { paths, profilesDir } = createTempEnv();
  const outside = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-links-'));
  const target = path.join(outside, 'target.json');
  fs.writeFileSync(target, '{"name":"linked"}');
  fs.symlinkSync(target, path.join(profilesDir, 'link.json'));
  assert.deepEqual(listProfiles(paths).profiles, ['linked']);

  // Two links to different files with the same decoded name must collide.
  const dupA = path.join(outside, 'dup-a.json');
  const dupB = path.join(outside, 'dup-b.json');
  fs.writeFileSync(dupA, '{"name":"duplink"}');
  fs.writeFileSync(dupB, '{"name":"duplink"}');
  fs.symlinkSync(dupA, path.join(profilesDir, 'duplink-a.json'));
  fs.symlinkSync(dupB, path.join(profilesDir, 'duplink-b.json'));
  const res = listProfiles(paths);
  assert.deepEqual(res.profiles, ['linked']);
  assert.equal(res.warnings.filter((w) => w.diagnostic.includes("duplicate profile name 'duplink'")).length, 2);

  // A dangling link is not a file for the daemon either; it is silently skipped.
  fs.symlinkSync(path.join(outside, 'does-not-exist.json'), path.join(profilesDir, 'dangling.json'));
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
  fs.writeFileSync(path.join(realProfiles, 'x.json'), '{"name":"x"}');
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
  assert.match(HELP, /profiles\/\*\.json/u);
  assert.match(HELP, /path-based warnings/u);
  assert.match(HELP, /excluded from the\s+available\s+name set/u);
});
