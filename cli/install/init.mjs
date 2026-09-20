import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { LAUNCH_AGENT_LABEL } from '../constants.mjs';
import { CliError } from '../errors.mjs';
import { parseConfig } from '../config/read.mjs';
import { writeConfig } from '../config/write.mjs';
import { verifyPayload } from './payload.mjs';
import { pathReport } from './path.mjs';
import { packageRoot, payloadManifestPath } from './layout.mjs';
import { updateInstallation } from './update.mjs';
import { loadInstallState, markInstallStep, removeCreatedDirectories, rollbackFiles, snapshotFile } from './recovery.mjs';
import { bootstrapService, bootoutService, installLaunchAgent, runtimeObservations } from './service-macos.mjs';

// Standalone-install coordination (S05, AUD-005 decision D1).  A plain npm
// install only stages the package and payload; an explicit `init` installs
// the STANDALONE SERVICE ONLY — every state-changing step below is
// host-neutral:
//   verify-payload     the staged native payload matches the package version
//   check-path         PATH findings are reported, never written
//   create-data        private data/log directories
//   write-product-config  republish the current agent config schema
//   install-launch-agent  the one macOS service template
//   start-service      launchctl bootstrap (best-effort, reported)
//   publish-active-payload  the verified active identity and retained bytes
//                      (B-3): a successful init establishes the version and
//                      retention baseline itself, so the standard `npm A ->
//                      init A -> use A -> npm B` sequence never depends on an
//                      extra "A update" the user was never told to run.  The
//                      publication reuses the existing locked update owner —
//                      no second lifecycle or scheduler — and only runs when
//                      this init verified the payload it installed.
// init no longer probes a fixed ZCode runtime, no longer installs the managed
// Codex plugin, and no longer claims a Codex home (the pre-D1 steps
// probe-runtime, install-codex-plugin, and claim-codex-home are gone).  A
// host is bound through the existing EXPLICIT commands — `install-plugin
// codex|zcode` or `install-mcp` — which own staging, the official codex add,
// and the D08 registry claim.  The pinned ZCode runtime is forwarded to the
// service only when that installation exists (see service-macos.mjs), so a
// missing ZCode app or Codex home never blocks standalone setup.
// Failures roll tracked files back through recovery.mjs; a resumable journal
// lets `init --resume` continue after environmental failures.  Journals from
// pre-D1 inits may still list the retired codex steps; unknown completed ids
// are simply never consulted again.
const HOOK_INSTALLER = ['plugins', 'codex', 'external-subagent', 'scripts', 'install-agent-hooks.mjs'];

function hookInstallerPath() {
  return path.join(packageRoot(), ...HOOK_INSTALLER);
}

export function installPlan(paths, options = {}) {
  const manifest = payloadManifestPath() ?? path.join(packageRoot(), 'npm', 'native', 'darwin-arm64', 'payload.json');
  const plan = [
    { id: 'verify-payload', action: 'verify staged native payload', path: manifest },
    { id: 'check-path', action: 'report PATH availability without writing profiles' },
    { id: 'create-data', action: 'create private product data and log directories', paths: [paths.data, paths.logs] },
    { id: 'write-product-config', action: 'republish the product agent config schema', path: paths.config },
    { id: 'install-launch-agent', action: 'install daemon LaunchAgent', path: paths.launchAgent, label: LAUNCH_AGENT_LABEL },
    { id: 'start-service', action: 'bootstrap the daemon service', path: paths.launchAgent, label: LAUNCH_AGENT_LABEL },
    { id: 'publish-active-payload', action: 'publish the verified active payload and retained-byte baseline', path: paths.state },
  ];
  if (options.installHooks) plan.splice(plan.findIndex((step) => step.id === 'publish-active-payload'), 0, { id: 'install-hooks', action: 'install ZCode policy hooks', path: paths.zcodeConfig, provenance: paths.hookProvenance });
  return plan;
}

export function installHooks(paths, options = {}) {
  if (options.dryRun) return { dry_run: true, plan: [{ id: 'install-hooks', action: 'install ZCode policy hooks', path: paths.zcodeConfig, provenance: paths.hookProvenance }] };
  const result = spawnSync(process.execPath, [hookInstallerPath(), '--config', paths.zcodeConfig, '--provenance', paths.hookProvenance], { encoding: 'utf8' });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    let failure;
    try { failure = JSON.parse((result.stderr || '').trim()); } catch { /* plain-text failure */ }
    throw new CliError(failure?.code || 'HOOK_INSTALL_FAILED', failure?.error || (result.stderr || 'hook installation failed').trim());
  }
  try { return JSON.parse(result.stdout); } catch { throw new CliError('HOOK_INSTALL_FAILED', 'hook installer returned invalid JSON'); }
}

export function runInit(options = {}) {
  const paths = options.paths;
  if (!paths) throw new CliError('INTERNAL_ERROR', 'runInit requires product paths');
  const plan = installPlan(paths, options);
  if (options.dryRun) return { dry_run: true, plan };

  const skipPayload = Boolean(options.skipPayloadProbe);
  const payload = skipPayload ? { status: 'skipped', platform: null, version: null, files: [] } : verifyPayload();
  const pathFindings = pathReport();

  const tracked = {
    zcodeConfig: { file: paths.zcodeConfig, snapshot: snapshotFile(paths.zcodeConfig) },
    hookProvenance: { file: paths.hookProvenance, snapshot: snapshotFile(paths.hookProvenance) },
    state: { file: paths.state, snapshot: snapshotFile(paths.state) },
    config: { file: paths.config, snapshot: snapshotFile(paths.config) },
    launchAgent: { file: paths.launchAgent, snapshot: snapshotFile(paths.launchAgent) },
  };
  const directories = {
    data: { path: paths.data, existed: fs.existsSync(paths.data) },
    logs: { path: paths.logs, existed: fs.existsSync(paths.logs) },
  };
  const state = options.resume ? loadInstallState(paths.state) : { schema_version: 1, completed: [] };
  const completed = new Set(state.completed || []);
  const mark = (id) => markInstallStep(paths.state, completed, id);
  const failAt = (id) => {
    if (options._failStep === id) throw new Error(`injected failure at ${id}`);
  };

  let service = { action: 'bootstrap', skipped: true, reason: 'not attempted' };
  // A rollback must also undo what this run itself loaded: a failure after a
  // successful bootstrap would otherwise leave a launchd job running binary
  // bytes the rolled-back plist/install-state no longer describe (observed
  // live: a codex-binding conflict stranded a running candidate daemon).
  // A service that was already loaded before this init is never touched.
  let serviceLoadedByThisRun = false;
  let baseline = null;
  try {
    if (!completed.has('verify-payload')) mark('verify-payload');
    if (!completed.has('check-path')) mark('check-path');
    if (!completed.has('create-data')) {
      fs.mkdirSync(paths.data, { recursive: true, mode: 0o700 });
      fs.mkdirSync(paths.logs, { recursive: true, mode: 0o700 });
      mark('create-data');
    }
    if (!completed.has('write-product-config')) {
      // The daemon takes its database/socket/runtime from service arguments,
      // never from this file; drop the retired top-level path fields so an
      // install over an older local config republishes the current schema.
      // S05-F01: `subagents.zcode.runtime_path` was a first-class config key
      // before D1 rejected it, so a pre-S05 install can carry it in either
      // the schema-2 or the legacy schema-1 shape.  Retire it here — before
      // parseConfig migrates/validates — so `npm new -> init` self-heals such
      // configs instead of failing closed at the read gate (which stays
      // fully strict for every other read/write path).
      const prior = fs.existsSync(paths.config)
        ? JSON.parse(fs.readFileSync(paths.config, 'utf8'))
        : {};
      for (const field of ['runtime', 'database', 'socket']) delete prior[field];
      for (const map of ['subagents', 'agents']) delete prior[map]?.zcode?.runtime_path;
      writeConfig(paths.config, parseConfig(prior));
      failAt('write-product-config');
      mark('write-product-config');
    }
    if (!completed.has('install-launch-agent')) {
      installLaunchAgent(paths);
      failAt('install-launch-agent');
      mark('install-launch-agent');
    }
    if (!completed.has('start-service') && !options.skipServiceStart) {
      service = bootstrapService(paths, process.getuid(), { launchctl: options.launchctl });
      serviceLoadedByThisRun = !service.skipped && !service.already_loaded;
      mark('start-service');
    }
    if (options.installHooks && !completed.has('install-hooks')) {
      installHooks(paths);
      mark('install-hooks');
    }
    // The baseline publication runs LAST and only for an init that verified
    // the payload it installed (a skipped probe — test harnesses only —
    // leaves the resume journal as the sole state).  It reuses the locked
    // update owner, so the active identity and retained bytes are published
    // under the exact verification/retention rules every later update
    // follows.  The Codex-home sync stays deferred: init binds no host, so
    // any registered homes belong to explicit install-plugin/install-mcp
    // bindings and remain the reconcile owner's to refresh.  The step is
    // recorded in the in-memory report only — writing the schema-1 journal
    // here would clobber the schema-2 activation state it just published.
    if (payload.status === 'verified') {
      failAt('publish-active-payload');
      const published = updateInstallation(paths, { deferCodexSync: true });
      if (published.phase !== 'active' || !published.active) {
        throw new CliError('INIT_BASELINE_FAILED', `activation baseline publication did not complete (phase=${published.phase ?? 'none'})`);
      }
      completed.add('publish-active-payload');
      baseline = {
        version: published.active.version,
        entry_sha256: published.active.entry_sha256,
        daemon_entry: published.active.daemon_entry,
        daemon_entry_sha256: published.active.daemon_entry_sha256,
        retained_root: published.active.retained?.root ?? null,
      };
    }
  } catch (error) {
    const rollbackErrors = [
      ...rollbackFiles(tracked),
      ...removeCreatedDirectories(directories),
    ];
    if (serviceLoadedByThisRun) {
      try { bootoutService(paths, process.getuid(), { launchctl: options.launchctl }); } catch (bootoutError) {
        rollbackErrors.push(`service rollback (bootout) failed: ${bootoutError.message}`);
      }
    }
    if (rollbackErrors.length > 0) error.rollbackErrors = rollbackErrors;
    throw error;
  }
  return {
    installed: true,
    resumed: Boolean(options.resume),
    completed: [...completed],
    // Honest runtime report instead of a hard dependency: the pinned ZCode
    // runtime is an adapter capability, observed and forwarded by the service
    // template only when present.
    runtimes: runtimeObservations(paths),
    payload,
    baseline,
    path_report: pathFindings,
    service,
  };
}
