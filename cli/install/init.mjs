import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { ZCODE_RUNTIME } from '../constants.mjs';
import { CliError } from '../errors.mjs';
import { atomicWrite, jsonBytes } from '../fs-atomic.mjs';
import { readConfig } from '../config/read.mjs';
import { writeConfig } from '../config/write.mjs';
import { codexHomeFor, installPlugin, resolveStaging } from './codex.mjs';
import { verifyPayload } from './payload.mjs';
import { pathReport } from './path.mjs';
import { packageRoot } from './layout.mjs';
import { registerCodexHome } from './reconcile.mjs';
import { updateInstallation } from './update.mjs';
import { loadInstallState, markInstallStep, removeCreatedDirectories, rollbackCodexArtifacts, rollbackFiles, snapshotFile } from './recovery.mjs';
import { bootstrapService, installLaunchAgent } from './service-macos.mjs';

// Fresh-install coordination (S05).  A plain npm install only stages the
// package and payload; every state-changing action below belongs to an
// explicit `init`:
//   verify-payload     the staged native payload matches the package version
//   probe-runtime      the fixed ZCode runtime exists (DSH stays optional and
//                      missing DSH never blocks installation)
//   check-path         PATH findings are reported, never written
//   create-data        private data/log directories
//   write-product-config  paths + fixed runtime for daemon and agents
//   install-launch-agent  the one macOS service template
//   start-service      launchctl bootstrap (best-effort, reported)
//   install-codex-plugin managed staging + official codex add
//   claim-codex-home   D08 registry claim after a successful binding
//   publish-active-payload  the verified active identity and retained bytes
//                      (B-3): a successful init establishes the version and
//                      retention baseline itself, so the standard `npm A ->
//                      init A -> use A -> npm B` sequence never depends on an
//                      extra "A update" the user was never told to run.  The
//                      publication reuses the existing locked update owner —
//                      no second lifecycle or scheduler — and only runs when
//                      this init verified the payload it installed.
// Failures roll tracked files back through recovery.mjs, including the
// product-owned Codex artifacts (staging tree, marketplace manifest, and
// directories this run created) — the official codex cache is never rolled
// back; a resumable journal lets `init --resume` continue after environmental
// failures.

const HOOK_INSTALLER = ['plugins', 'codex', 'external-subagent', 'scripts', 'install-agent-hooks.mjs'];

function hookInstallerPath() {
  return path.join(packageRoot(), ...HOOK_INSTALLER);
}

export function installPlan(paths, options = {}) {
  const plan = [
    { id: 'probe-runtime', action: 'verify fixed ZCode runtime', path: ZCODE_RUNTIME },
    { id: 'verify-payload', action: 'verify staged native payload', path: path.join(packageRoot(), 'npm', 'native', 'darwin-arm64', 'payload.json') },
    { id: 'check-path', action: 'report PATH availability without writing profiles' },
    { id: 'create-data', action: 'create private product data and log directories', paths: [paths.data, paths.logs] },
    { id: 'write-product-config', action: 'write product paths and fixed runtime', path: paths.config },
    { id: 'install-launch-agent', action: 'install daemon LaunchAgent', path: paths.launchAgent, label: 'com.external-subagent.daemon' },
    { id: 'start-service', action: 'bootstrap the daemon service', path: paths.launchAgent, label: 'com.external-subagent.daemon' },
    { id: 'install-codex-plugin', action: 'stage and register the managed Codex plugin', path: path.join(paths.home, 'plugins', 'external-subagent') },
    { id: 'claim-codex-home', action: 'register the claimed Codex home', path: path.join(paths.data, 'codex-homes.json') },
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
  if (!fs.existsSync(ZCODE_RUNTIME) && !options.skipRuntimeProbe) {
    throw new CliError('ZCODE_RUNTIME_NOT_FOUND', `required ZCode runtime is missing: ${ZCODE_RUNTIME}`);
  }
  const pathFindings = pathReport();

  const tracked = {
    zcodeConfig: { file: paths.zcodeConfig, snapshot: snapshotFile(paths.zcodeConfig) },
    hookProvenance: { file: paths.hookProvenance, snapshot: snapshotFile(paths.hookProvenance) },
    state: { file: paths.state, snapshot: snapshotFile(paths.state) },
    config: { file: paths.config, snapshot: snapshotFile(paths.config) },
    launchAgent: { file: paths.launchAgent, snapshot: snapshotFile(paths.launchAgent) },
    registry: { file: path.join(paths.data, 'codex-homes.json'), snapshot: snapshotFile(path.join(paths.data, 'codex-homes.json')) },
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
  let baseline = null;
  const codexHome = codexHomeFor({ codexHome: options.codexHome }, paths);
  // Product-owned Codex artifacts the init may create, snapshotted before any
  // step runs so a later failure can restore them (see rollbackCodexArtifacts).
  const stagingSite = resolveStaging(paths.home, {});
  const codexArtifactDirectories = (target) => {
    const stop = path.resolve(paths.home);
    const levels = [];
    let current = path.resolve(target);
    while (current.startsWith(`${stop}${path.sep}`)) { levels.push(current); current = path.dirname(current); }
    return levels;
  };
  const codexArtifactDirs = new Set([
    path.resolve(codexHome),
    ...codexArtifactDirectories(codexHome),
    ...codexArtifactDirectories(stagingSite.staging),
    ...codexArtifactDirectories(stagingSite.marketplace),
  ]);
  const codexArtifacts = {
    staging: { path: stagingSite.staging, existed: fs.existsSync(stagingSite.staging) },
    marketplace: { file: stagingSite.marketplace, snapshot: snapshotFile(stagingSite.marketplace) },
    directories: [...codexArtifactDirs],
    preexisting: new Set([...codexArtifactDirs].filter((directory) => fs.existsSync(directory))),
    productRoot: path.resolve(path.join(paths.home, '.external-subagent-marketplace')),
  };
  let codex = { status: 'skipped', codex_home: codexHome, reason: options.skipCodexPlugin ? 'skipped by request' : 'not attempted' };
  try {
    if (!completed.has('probe-runtime')) mark('probe-runtime');
    if (!completed.has('verify-payload')) mark('verify-payload');
    if (!completed.has('check-path')) mark('check-path');
    if (!completed.has('create-data')) {
      fs.mkdirSync(paths.data, { recursive: true, mode: 0o700 });
      fs.mkdirSync(paths.logs, { recursive: true, mode: 0o700 });
      mark('create-data');
    }
    if (!completed.has('write-product-config')) {
      const configured = readConfig(paths.config);
      writeConfig(paths.config, {
        ...configured,
        runtime: ZCODE_RUNTIME,
        database: paths.database,
        socket: paths.socket,
      });
      failAt('write-product-config');
      mark('write-product-config');
    }
    if (!completed.has('install-launch-agent')) {
      installLaunchAgent(paths);
      failAt('install-launch-agent');
      mark('install-launch-agent');
    }
    if (!completed.has('start-service') && !options.skipServiceStart) {
      service = bootstrapService(paths);
      mark('start-service');
    }
    if (!completed.has('install-codex-plugin') && !options.skipCodexPlugin) {
      const install = installPlugin(paths, { codexCli: options.codexCli, codexHome });
      codex = { status: 'installed', codex_home: codexHome, cache: install.cache || null, marketplace: install.marketplace_name, digest: install.digest || null };
      failAt('install-codex-plugin');
      mark('install-codex-plugin');
    }
    const pluginDone = completed.has('install-codex-plugin');
    if (!completed.has('claim-codex-home') && (codex.status === 'installed' || (pluginDone && !options.skipCodexPlugin))) {
      const claim = registerCodexHome(paths, codexHome, { version: payload.version, digest: codex.digest });
      codex = { ...codex, claim: { registered: claim.registered, deduplicated: claim.deduplicated, homes: claim.homes } };
      failAt('claim-codex-home');
      mark('claim-codex-home');
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
    // follows; the Codex homes were just bound by this init's own steps, so
    // the update's home sync is deferred to the next reconcile.  The step is
    // recorded in the in-memory report only — writing the schema-1 journal
    // here would clobber the schema-2 activation state it just published.
    if (payload.status === 'verified') {
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
      ...rollbackCodexArtifacts(codexArtifacts),
      ...removeCreatedDirectories(directories),
    ];
    if (rollbackErrors.length > 0) error.rollbackErrors = rollbackErrors;
    throw error;
  }
  return {
    installed: true,
    resumed: Boolean(options.resume),
    completed: [...completed],
    runtime: ZCODE_RUNTIME,
    payload,
    baseline,
    path_report: pathFindings,
    service,
    codex,
  };
}
