// S03 Windows SCM backend oracles.  The real cli/install/service-windows.mjs
// is exercised through its injectable `options.sc` seam (the twin of the
// launchd/systemd doubles): query parsing, create/start/stop/delete argument
// construction (including binPath quoting), idempotence, bounded stop
// confirmation, self-clean on a failed start, structured AccessDenied, the
// rendered service definition, and the activation health/rollback contract.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import {
  SC_EXE_PATH, WINDOWS_SERVICE_NAME, activateServiceWindows, bootoutServiceWindows, bootstrapServiceWindows,
  installServiceDefinitionWindows, windowsServiceRegistrationStatus,
} from '../../cli/install/service-windows.mjs';

const sha256 = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');
// A real file (with a space in its directory) so bootstrap's executable probe
// succeeds while the command line still exercises quoting.
const fixtureDir = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-scm-'));
const daemonPath = path.join(fixtureDir, 'external subagent', 'external-subagentd.exe');
fs.mkdirSync(path.dirname(daemonPath), { recursive: true });
fs.writeFileSync(daemonPath, 'fixture daemon');
const paths = Object.freeze({
  database: 'C:\\Users\\Test User\\AppData\\Local\\external-subagent\\external-subagent.sqlite3',
  socket: '\\\\.\\pipe\\external-subagent-daemon',
});
const expectedBinPath = `"${daemonPath}" --database "${paths.database}" --socket "${paths.socket}"`;

function queryOutput({ running = true, pid = 4242 } = {}) {
  return [
    `SERVICE_NAME: ${WINDOWS_SERVICE_NAME}`,
    `        STATE              : ${running ? 4 : 1}  ${running ? 'RUNNING' : 'STOPPED'}`,
    `        PID                : ${running ? pid : 0}`,
    '',
  ].join('\r\n');
}

function firstProgram(binPath) {
  const text = String(binPath || '').trim();
  if (!text.startsWith('"')) return text.split(/\s+/u)[0];
  const end = text.indexOf('"', 1);
  return end > 1 ? text.slice(1, end) : null;
}

test('sc.exe is pinned to the System32 absolute path, never a PATH lookup', () => {
  // The structural twin of /bin/launchctl and /usr/bin/systemctl: resolve the
  // tool from SystemRoot (falling back to C:\Windows) instead of letting PATH or
  // the current directory choose the service-control binary.
  const configured = process.env.SystemRoot;
  const expectedRoot = typeof configured === 'string' && configured !== '' && path.win32.isAbsolute(configured)
    ? configured
    : 'C:\\Windows';
  assert.equal(SC_EXE_PATH, path.win32.join(expectedRoot, 'System32', 'sc.exe'));
  assert.ok(path.win32.isAbsolute(SC_EXE_PATH), SC_EXE_PATH);
  assert.equal(path.win32.basename(SC_EXE_PATH).toLowerCase(), 'sc.exe');
  assert.equal(path.win32.basename(path.win32.dirname(SC_EXE_PATH)).toLowerCase(), 'system32');
});

// Stateful sc.exe double: queryex/create/start/stop/delete/config/qc keep one
// tracked service, so each oracle observes exactly what the SCM would report.
function statefulSc({
  registered = false, running = false, pid = 7000, binPath = expectedBinPath,
  overrides = {}, onStart = null, onStop = null,
} = {}) {
  const state = { registered, running, pid, binPath };
  const calls = [];
  const control = (args) => {
    calls.push(args);
    const override = overrides[args[0]];
    if (override) return override(args, state);
    switch (args[0]) {
      case 'queryex':
        return state.registered
          ? { status: 0, stdout: queryOutput({ running: state.running, pid: state.pid }) }
          : { status: 1060, stdout: '' };
      case 'create':
        state.registered = true;
        state.binPath = args[3];
        return { status: 0, stdout: '' };
      case 'start':
        state.running = true;
        state.pid += 1;
        if (onStart) onStart(state);
        return { status: 0, stdout: '' };
      case 'stop':
        state.running = false;
        if (onStop) onStop(state);
        return { status: 0, stdout: '' };
      case 'delete':
        state.registered = false;
        state.running = false;
        return { status: 0, stdout: '' };
      case 'config':
        state.binPath = args[3];
        return { status: 0, stdout: '' };
      case 'qc':
        return { status: 0, stdout: `        BINARY_PATH_NAME   : ${state.binPath}\r\n` };
      default:
        throw new Error(`unexpected sc.exe call: ${args.join(' ')}`);
    }
  };
  return { calls, control, state };
}

test('windowsServiceRegistrationStatus parses a running queryex view', () => {
  const { control } = statefulSc({ registered: true, running: true, pid: 1234 });
  assert.deepEqual(windowsServiceRegistrationStatus({ sc: control }), {
    query: 'available',
    registered: true,
    service: WINDOWS_SERVICE_NAME,
    state: 'running',
    pid: 1234,
  });
});

test('windowsServiceRegistrationStatus maps a missing service and a denied probe', () => {
  const missing = statefulSc();
  assert.deepEqual(windowsServiceRegistrationStatus({ sc: missing.control }), { query: 'available', registered: false });

  const denied = statefulSc({ overrides: { queryex: () => ({ status: 5, stdout: '' }) } });
  const view = windowsServiceRegistrationStatus({ sc: denied.control });
  assert.equal(view.query, 'unavailable');
  assert.equal(view.registered, null);
  assert.match(view.reason, /denied access \(exit code 5\)/u);
  assert.match(view.reason, /Administrator/u);
});

test('a non-AccessDenied sc.exe failure is a structured CliError', () => {
  const { control } = statefulSc({ overrides: { queryex: () => ({ status: 87, stdout: '' }) } });
  assert.throws(() => windowsServiceRegistrationStatus({ sc: control }), (error) => {
    assert.equal(error.code, 'DAEMON_CONTROL_FAILED');
    assert.match(error.message, /queryex.*exit code 87/u);
    return true;
  });
});

test('bootstrap creates the quoted binPath and starts the service', () => {
  const { calls, control } = statefulSc();
  const result = bootstrapServiceWindows(paths, { sc: control, daemonPath });
  assert.equal(result.action, 'bootstrap');
  assert.equal(result.created, true);
  assert.equal(result.started, true);
  assert.deepEqual(calls[0], ['queryex', WINDOWS_SERVICE_NAME]);
  assert.deepEqual(calls[1], ['create', WINDOWS_SERVICE_NAME, 'binPath=', expectedBinPath, 'start=', 'auto']);
  assert.deepEqual(calls[2], ['start', WINDOWS_SERVICE_NAME]);
});

test('bootstrap is idempotent for a live service and starts a stopped one', () => {
  const live = statefulSc({ registered: true, running: true, pid: 900 });
  const already = bootstrapServiceWindows(paths, { sc: live.control, daemonPath });
  assert.equal(already.already_loaded, true);
  assert.equal(already.pid, 900);
  assert.deepEqual(live.calls.map((call) => call[0]), ['queryex']);

  const stopped = statefulSc({ registered: true, running: false });
  const started = bootstrapServiceWindows(paths, { sc: stopped.control, daemonPath });
  assert.equal(started.started, true);
  assert.deepEqual(stopped.calls.map((call) => call[0]), ['queryex', 'start', 'queryex']);
});

test('bootstrap resolves a lost create race through the registration probe', () => {
  let firstQuery = true;
  const { calls, control } = statefulSc({
    overrides: {
      queryex: () => {
        if (firstQuery) { firstQuery = false; return { status: 1060, stdout: '' }; }
        return { status: 0, stdout: queryOutput({ running: false }) };
      },
      create: (_args, state) => { state.registered = true; return { status: 1073, stdout: '' }; },
    },
  });
  const result = bootstrapServiceWindows(paths, { sc: control, daemonPath });
  assert.equal(result.started, true);
  assert.deepEqual(calls.map((call) => call[0]), ['queryex', 'create', 'queryex', 'start', 'queryex']);
});

test('a denied create is projected as SERVICE_ACCESS_DENIED', () => {
  const { control } = statefulSc({ overrides: { create: () => ({ status: 5, stdout: '' }) } });
  assert.throws(() => bootstrapServiceWindows(paths, { sc: control, daemonPath }), (error) => {
    assert.equal(error.code, 'SERVICE_ACCESS_DENIED');
    assert.match(error.message, /Administrator/u);
    return true;
  });
});

test('a failed start deletes the definition this bootstrap created', () => {
  const { calls, control } = statefulSc({
    overrides: { start: () => ({ status: 1053, stdout: '' }) },
  });
  assert.throws(() => bootstrapServiceWindows(paths, { sc: control, daemonPath }), (error) => {
    assert.equal(error.code, 'DAEMON_CONTROL_FAILED');
    assert.match(error.message, /start.*exit code 1053/u);
    return true;
  });
  assert.ok(calls.some((call) => call[0] === 'delete'), 'the half-created service must not survive');
});

test('bootout stops the live service, confirms STOPPED, then deletes it', () => {
  const { calls, control } = statefulSc({ registered: true, running: true, pid: 901 });
  const result = bootoutServiceWindows(paths, { sc: control, unloadTimeoutMs: 1000 });
  assert.equal(result.removed, true);
  assert.deepEqual(calls.map((call) => call[0]), ['queryex', 'stop', 'queryex', 'delete']);
});

test('bootout is idempotent for an absent service and tolerates not-active stop', () => {
  const absent = statefulSc();
  assert.deepEqual(bootoutServiceWindows(paths, { sc: absent.control }), {
    action: 'bootout', service: WINDOWS_SERVICE_NAME, already_stopped: true,
  });
  assert.deepEqual(absent.calls.map((call) => call[0]), ['queryex']);

  const notActive = statefulSc({
    registered: true, running: true,
    overrides: { stop: (_args, state) => { state.running = false; return { status: 1062, stdout: '' }; } },
  });
  const result = bootoutServiceWindows(paths, { sc: notActive.control, unloadTimeoutMs: 1000 });
  assert.equal(result.removed, true);
});

test('a stop that never reaches STOPPED fails bounded instead of pretending removal', () => {
  const { calls, control } = statefulSc({
    registered: true, running: true,
    overrides: { stop: () => ({ status: 0, stdout: '' }) },
  });
  assert.throws(() => bootoutServiceWindows(paths, { sc: control, unloadTimeoutMs: 0 }), (error) => {
    assert.equal(error.code, 'SERVICE_UNLOAD_TIMEOUT');
    return true;
  });
  assert.equal(calls.some((call) => call[0] === 'delete'), false);
});

test('installServiceDefinitionWindows renders the exact sc.exe command line without touching SCM', () => {
  const { calls, control } = statefulSc();
  const definition = installServiceDefinitionWindows(paths, { sc: control, daemonPath });
  assert.deepEqual(definition, {
    installed: true,
    backend: 'scm',
    service: WINDOWS_SERVICE_NAME,
    bin_path: expectedBinPath,
    start_type: 'auto',
  });
  assert.deepEqual(calls, []);
});

test('a path the service command line cannot represent is refused', () => {
  assert.throws(
    () => installServiceDefinitionWindows({ ...paths, database: 'C:\\bad"path\\db.sqlite3' }, { daemonPath }),
    (error) => {
      assert.equal(error.code, 'SERVICE_DEFINITION_INVALID');
      return true;
    },
  );
  assert.throws(
    () => installServiceDefinitionWindows({ ...paths, socket: '\\\\.\\pipe\\bad\nname' }, { daemonPath }),
    (error) => error.code === 'SERVICE_DEFINITION_INVALID',
  );
});

test('activateServiceWindows reconfigures and starts the candidate, then rolls back to the old bytes', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'external-subagent-win-activate-'));
  const oldPath = path.join(root, 'old-external-subagentd.exe');
  const candidatePath = path.join(root, 'new-external-subagentd.exe');
  fs.writeFileSync(oldPath, Buffer.from('old daemon bytes'));
  fs.writeFileSync(candidatePath, Buffer.from('new daemon bytes'));
  const oldSha = sha256(fs.readFileSync(oldPath));
  const candidateSha = sha256(fs.readFileSync(candidatePath));
  const oldBinPath = `"${oldPath}" --database "${paths.database}" --socket "${paths.socket}"`;
  let generation = 10;
  let served = oldPath;
  const sc = statefulSc({
    registered: true, running: true, pid: 100, binPath: oldBinPath,
    onStart: (state) => {
      generation += 1;
      served = firstProgram(state.binPath);
    },
  });
  const callDaemon = async (_socket, method) => {
    if (method === 'status') {
      const sha = served === oldPath ? oldSha : candidateSha;
      return { service_generation: generation, identity: { daemon: { version: '0.5.2', artifact: { path: served, sha256: sha } } } };
    }
    if (method === 'drain-status') return { ready_for_activation: true };
    if (method === 'activate-ready') return { activation_claim: true };
    throw new Error(`unexpected daemon call: ${method}`);
  };
  try {
    const result = await activateServiceWindows(paths, { path: candidatePath, sha256: candidateSha, version: '0.5.2' }, {
      sc: sc.control, callDaemon, healthTimeoutMs: 200, unloadTimeoutMs: 200,
    });
    assert.equal(result.artifact.path, candidatePath);
    assert.equal(result.artifact.sha256, candidateSha);
    assert.equal(result.service_generation, 11);
    assert.ok(sc.calls.some((call) => call[0] === 'config' && call[3].includes(candidatePath)));

    const restored = await result.rollback();
    assert.equal(restored.artifact.path, oldPath);
    assert.equal(restored.artifact.sha256, oldSha);
    assert.ok(sc.calls.some((call) => call[0] === 'config' && call[3].includes(oldPath)));
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
