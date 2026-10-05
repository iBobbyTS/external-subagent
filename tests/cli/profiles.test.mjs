import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

import { CliError } from '../../cli/errors.mjs';
import { main } from '../../cli/main.mjs';
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

function createTempEnv() {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-profiles-'));
  const paths = productPaths(home);
  const profilesPath = path.join(paths.data, 'profiles');
  fs.mkdirSync(profilesPath, { recursive: true });
  return { home, paths, profilesDir: profilesPath };
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
    const listRes = profileCommand(singleEnv.paths, ['list']);
    assert.deepEqual(listRes.profiles, ['real'], `fixture ${idx} failed to yield 'real'`);
    assert.deepEqual(listRes.warnings, [], `fixture ${idx} unexpectedly reported warnings`);
    fs.rmSync(singleEnv.home, { recursive: true, force: true });
  }

  // Write Fixture ④ into profiles directory: show real MUST hit this file
  const f4Path = path.join(profilesDir, 'escaped_key.toml');
  fs.writeFileSync(f4Path, content4);
  const showRes = profileCommand(paths, ['show', 'real']);
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
  const res = profileCommand(paths, ['list']);
  assert.equal(res.operation, 'list');
  assert.deepEqual(res.profiles, []);
  assert.deepEqual(res.warnings, []);
  assert.match(res.message, /profiles directory does not exist/u);
  assert.ok(res.directory.endsWith(path.join('Application Support', 'external-subagent', 'profiles')));

  // show on missing directory
  assert.throws(
    () => profileCommand(paths, ['show', 'anything']),
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
  const res = profileCommand(paths, ['list']);
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

  const listRes = profileCommand(paths, ['list']);
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
    () => profileCommand(paths, ['show', 'nonexistent']),
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

  const listRes = profileCommand(paths, ['list']);
  // duplicate_worker evicted; only unique_worker is available
  assert.deepEqual(listRes.profiles, ['unique_worker']);
  assert.equal(listRes.warnings.length, 2);
  assert.ok(listRes.warnings.every((w) => w.diagnostic.includes('duplicate profile name \'duplicate_worker\'')));

  // show on duplicate_worker reports ambiguity error with both conflicting files
  assert.throws(
    () => profileCommand(paths, ['show', 'duplicate_worker']),
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
  const showRes = profileCommand(paths, ['show', 'unique_worker']);
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
    () => profileCommand(paths, ['show', 'nonexistent_ascii']),
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
    () => profileCommand(paths, ['show', 'nonexistent_unicode']),
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
