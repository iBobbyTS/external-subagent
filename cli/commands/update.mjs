import { npmUpdateCoordination, reconcileCodexHomes } from '../install/reconcile.mjs';
import { reconcileZcodeBinding } from '../install/zcode.mjs';
import { preflightUpdate, reconcileInstallation, updateInstallation } from '../install/update.mjs';
import { callDaemon, canonicalDaemonCode } from '../rpc.mjs';
import { CliError } from '../errors.mjs';
import fs from 'node:fs';
import path from 'node:path';
import { atomicWrite, jsonBytes } from '../fs-atomic.mjs';
import { activateService, hasInstalledService } from '../install/service-activation.mjs';

const receiptPath = (paths) => `${paths.state}.activation.json`;

export async function updateCommand(paths, args = [], daemon = {}) {
  const cancelActive = args.includes('--cancel-active');
  const yes = args.includes('--yes');
  if (cancelActive && !yes) throw new Error('--cancel-active requires --yes');
  const prior = (() => { try { return JSON.parse(fs.readFileSync(receiptPath(paths), 'utf8')); } catch { return null; } })();
  const priorStateBytes = (() => { try { return fs.readFileSync(paths.state); } catch { return null; } })();
  const registryPath = paths.data ? path.join(paths.data, 'codex-homes.json') : null;
  const priorRegistryBytes = (() => { try { return fs.readFileSync(registryPath); } catch { return null; } })();
  const requestedVersion = args.find((arg) => arg.startsWith('--version='))?.slice(10) || 'current';
  const socket = daemon.socket || process.env.ZCODE_AGENTD_SOCKET || paths.socket;
  const rpc = daemon.callDaemon || callDaemon;
  const drainDeadline = Number.isFinite(daemon.drainTimeoutMs) && daemon.drainTimeoutMs > 0
    ? Date.now() + daemon.drainTimeoutMs : null;
  // npm auto-coordination (U06): an already-initialized product whose
  // installed package drifted from the published active payload coordinates
  // through the full update path even when invoked as `reconcile` — the
  // ignore-scripts flow, where no lifecycle hook ever fired. A matching
  // version merely re-affirms, and a never-initialized product stays
  // stage-only.
  const detected = npmUpdateCoordination(paths);
  const reconciling = args.includes('reconcile');
  const coordinate = !reconciling || detected.update_pending;
  // A doomed candidate must never take the working daemon offline: whenever
  // this run may activate a payload, the SAME pure validation the locked
  // updater owns runs BEFORE the drain RPC, so a bad version or manifest
  // returns with the daemon still serving spawns. A pure reaffirmal that
  // activates nothing drains only for its claim and needs no candidate.
  const preflight = daemon.preflightUpdate || preflightUpdate;
  if (!(reconciling && !coordinate)) preflight({ version: requestedVersion });
  let status = null;
  let online = true;
  try {
    status = await rpc(socket, 'drain', cancelActive ? { cancel_active: true } : {});
  } catch (error) {
    // A legacy daemon answers the cancel_active param with the raw wire code
    // ("validation"), so the check matches the canonicalized form.
    if (cancelActive && ['VALIDATION', 'UNKNOWN_METHOD'].includes(canonicalDaemonCode(error.code))) {
      throw new CliError('CANCEL_ACTIVE_UNSUPPORTED', 'running daemon does not support explicit drain cancellation; cancellation was not downgraded to passive drain');
    }
    // No daemon is listening and none answered: an already-initialized
    // service is (re)started by the activation itself, so there is nothing
    // to drain and the CLI coordination stays reachable after an
    // ignore-scripts install. A daemon that ANSWERED with a protocol-level
    // error still fails loudly instead of being treated as offline.
    if (error.code !== 'SOCKET_UNAVAILABLE' || error.daemonResponded) throw error;
    online = false;
  }
  if (online) {
    while (!status.ready_for_activation) {
      if (drainDeadline !== null && Date.now() >= drainDeadline) {
        try { await rpc(socket, 'drain-abort', {}); } catch { /* preserve timeout as the primary error */ }
        throw new CliError('UPDATE_DRAIN_TIMEOUT', 'update drain exceeded its bounded deadline; daemon drain was aborted');
      }
      await new Promise((resolve) => setTimeout(resolve, 50));
      status = await rpc(socket, 'drain-status', {});
    }
    status = await rpc(socket, 'activate-ready', {});
  }
  const claim = online ? status.activation_claim : `offline-${process.pid}-${Date.now()}-activation`;
  if (!claim) return { ...status, activation_claim: null, update: 'not_activated' };
  if (prior?.status === 'success' && prior.claim === claim
    && (requestedVersion === 'current' || prior.version === requestedVersion)) return prior.result;
  // The rollback source for service activation: the retained copy of the
  // PREVIOUS active payload, staged outside the npm directory the new
  // version may already have replaced.
  const previousActive = (() => {
    if (!priorStateBytes) return null;
    try { return JSON.parse(priorStateBytes)?.active ?? null; } catch { return null; }
  })();
  const rollbackPayload = previousActive?.retained?.daemon_entry
    ? { path: previousActive.retained.daemon_entry, sha256: previousActive.retained.daemon_entry_sha256 ?? previousActive.daemon_entry_sha256 }
    : (previousActive?.daemon_entry ? { path: previousActive.daemon_entry, sha256: previousActive.daemon_entry_sha256 } : null);
  let result;
  // Set when the verified activation completed but the Codex-home sync is
  // partial: the homes are per-home retryable through the public reconcile
  // owner, so the completed activation stays published and the partial sync
  // is reported AFTER the guarded block — never as success, and never as a
  // reason to roll a healthy service back.
  let partialHomes = null;
  try {
    const serviceInstalled = typeof daemon.hasInstalledService === 'function' ? daemon.hasInstalledService(paths) : hasInstalledService(paths);
    const serviceDue = serviceInstalled && !daemon.skipServiceActivation;
    result = (reconciling && !coordinate)
      ? { ...reconcileInstallation(paths, { cancelActive, yes }), homes: reconcileCodexHomes(paths), zcode: reconcileZcodeBinding(paths) }
      : await (daemon.updateInstallation || updateInstallation)(paths, { version: requestedVersion, deferCodexSync: serviceDue });
    // The zcode binding has neither a registry nor a service dependency, so
    // it refreshes on every activation attempt — including service-less
    // installs, where the codex homes sync runs inside updateInstallation
    // instead of the service activation block below.
    if (!result.zcode) result.zcode = reconcileZcodeBinding(paths);
    if (!result || typeof result !== 'object' || result.phase !== 'active') {
      throw new Error(`installation update did not activate payload (phase=${result?.phase ?? 'none'})`);
    }
    if (serviceDue && coordinate && (!result.active?.entry || !result.active?.entry_sha256
      || !result.active?.daemon_entry || !result.active?.daemon_entry_sha256)) {
      // An explicit update exists to switch the service onto the verified
      // daemon payload; an identity it cannot verify must fail rather than
      // silently skip activation.
      throw new CliError('ACTIVE_ENTRY_UNVERIFIED', 'activated payload has no verified daemon artifact to activate');
    }
    if (result?.active?.daemon_entry && result?.active?.daemon_entry_sha256 && serviceDue) {
      const activate = daemon.activateService || activateService;
      // The LaunchAgent runs the native daemon, so activation carries the
      // verified daemon artifact identity — never the npm bin shim. Prefer
      // the retained immutable copy so a later npm replacement cannot alter
      // the bytes the service executes.
      const serviceArtifact = result.active.retained?.daemon_entry
        ? { path: result.active.retained.daemon_entry, sha256: result.active.retained.daemon_entry_sha256 ?? result.active.daemon_entry_sha256 }
        : { path: result.active.daemon_entry, sha256: result.active.daemon_entry_sha256 };
      result.service = await activate(paths, {
        path: serviceArtifact.path,
        sha256: serviceArtifact.sha256,
        version: result.active.version,
      }, { ...daemon, rollbackPayload });
      result.homes = reconcileCodexHomes(paths);
      if (result.homes.homes.length > 0 && !result.homes.all_updated) partialHomes = result.homes;
    }
    const receiptVersion = result?.active?.version || result?.version || requestedVersion;
    atomicWrite(receiptPath(paths), jsonBytes(partialHomes
      ? { claim, version: receiptVersion, status: 'partial', retryable: true, homes: partialHomes, result }
      : { claim, version: receiptVersion, status: 'success', result }));
    if (partialHomes === null) return result;
  } catch (error) {
    // Snapshot what the updater recorded before rollback replaces it, so the
    // receipt keeps the candidate and active evidence the retry will need.
    const failedInstall = (() => {
      try { return JSON.parse(fs.readFileSync(paths.state, 'utf8')); } catch { return null; }
    })();
    let rollback = { attempted: false, restored: false };
    if (priorStateBytes && paths.state) {
      rollback.attempted = true;
      try { atomicWrite(paths.state, priorStateBytes); rollback.restored = true; } catch (restoreError) { rollback.error = restoreError.message; }
    } else if (paths.state) {
      rollback.attempted = true;
      try { fs.rmSync(paths.state, { force: true }); rollback.restored = true; } catch (restoreError) { rollback.error = restoreError.message; }
    }
    if (registryPath) {
      try {
        if (priorRegistryBytes) atomicWrite(registryPath, priorRegistryBytes);
        else if (fs.existsSync(registryPath)) fs.rmSync(registryPath, { force: true });
      } catch (restoreError) {
        rollback.registry_error = restoreError.message;
      }
    }
    if (result?.service?.rollback) {
      try { await result.service.rollback(); rollback.service_restored = true; } catch (restoreError) { rollback.service_error = restoreError.message; }
    }
    if (error.rollback) {
      rollback.service = error.rollback;
      rollback.service_restored = Boolean(error.rollback.pid && error.rollback.artifact);
    }
    const receipt = { claim, version: requestedVersion, status: 'failed', retryable: true, error: error.message, rollback };
    // This run drained a live daemon and never completed an activation:
    // bounded recovery reopens admission on the still-running old daemon so
    // it keeps serving spawns, while its completed-task and reap facts stay
    // untouched. A daemon that cannot abort (legacy, answering the raw
    // snake_case wire code) or will not yet (a --cancel-active worker still
    // in flight) keeps that evidence in the receipt — the canonical abort
    // code plus an explicit marker that the daemon may still be draining —
    // and the failure stays retryable; the abort outcome never masks the
    // original update error.
    if (online && !result?.service) {
      try {
        await rpc(socket, 'drain-abort', {});
        receipt.drain_aborted = true;
      } catch (abortError) {
        receipt.drain_aborted = false;
        receipt.drain_abort_error = { code: canonicalDaemonCode(abortError.code), message: abortError.message };
        receipt.daemon_may_be_draining = true;
      }
    }
    if (failedInstall && typeof failedInstall === 'object' && failedInstall.phase) {
      receipt.install_state = { phase: failedInstall.phase, candidate: failedInstall.candidate ?? null, active: failedInstall.active ?? null };
    }
    atomicWrite(receiptPath(paths), jsonBytes(receipt));
    throw error;
  }
  // Reached only with a completed, verified activation whose home sync is
  // partial.  The per-home facts stay actionable — in the error, the receipt,
  // and the registry reconcileCodexHomes already persisted — and the public
  // reconcile command finishes the remaining homes idempotently.
  const notUpdated = partialHomes.homes
    .filter((home) => home.status !== 'updated')
    .map((home) => `${home.home}: ${home.status}${home.error ? ` (${home.error.code}: ${home.error.message})` : ''}`);
  throw new CliError('CODEX_SYNC_PARTIAL',
    `service activation completed at ${result?.active?.version || requestedVersion}, but Codex home reconciliation is partial — ${notUpdated.join('; ')}; run 'external-subagent reconcile' after fixing the listed homes`);
}
