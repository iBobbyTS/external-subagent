import fs from 'node:fs';
import crypto from 'node:crypto';
import path from 'node:path';
import { CliError } from '../errors.mjs';
import { nativeBinary, PLUGIN_NAME } from './layout.mjs';

// Host-agnostic plugin staging: validating the shipped plugin source,
// materializing it into a product-owned staging tree, and fingerprinting
// that tree.  Every host binding (codex, zcode) stages the same source and
// pins the same MCP binding — the facade command plus the daemon socket —
// into the staged `.mcp.json`; only the registration surface around the
// staged tree differs per host.
//
// Publishing is a REPLACEMENT, never an overlay (AUD-002): the candidate
// tree is built, parsed, and rewritten with its final binding paths in an
// owned sibling directory BESIDE the target — never inside the live managed
// tree — so a file deleted from the source disappears from the published
// tree and an invalid candidate never moves a managed byte.  The publish
// itself is two same-directory renames (target -> retained prior tree,
// candidate -> target); the prior tree survives until the caller commits,
// and restore() puts it back when the host binding transaction fails.  The
// boundary is honest, not journaling: a process crash between the two
// publish renames can leave the target absent with the prior tree at its
// sibling path, and nothing here detects or repairs that automatically.

export function pluginManifest(source) {
  const manifest = JSON.parse(fs.readFileSync(path.join(source, '.codex-plugin', 'plugin.json'), 'utf8'));
  if (manifest.name !== PLUGIN_NAME || manifest.skills !== './skills/' || manifest.mcpServers !== './.mcp.json') {
    throw new CliError('INVALID_PLUGIN_SOURCE', 'plugin manifest identity or relative paths are invalid');
  }
  return manifest;
}

// Digest of the staged plugin tree.  Valid symlinks are followed: the linked
// content is hashed under the link's own relative path, so an edit of a
// target that lives OUTSIDE the traversed root and is reachable only through
// the link still changes the digest.  Dangling links are skipped (there is
// no content to hash) and directory loops/diamonds are bounded by resolved
// real path plus a depth backstop, so the walk always terminates.
export function treeDigest(root) {
  const hash = crypto.createHash('sha256');
  const seen = new Set([fs.realpathSync(root)]);
  const walk = (dir, depth) => {
    if (depth > 64) return;
    for (const entry of fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      const target = path.join(dir, entry.name);
      const isLink = entry.isSymbolicLink();
      let resolvedStat = entry;
      if (isLink) {
        try { resolvedStat = fs.statSync(target); } catch { continue; }
      }
      if (resolvedStat.isDirectory()) {
        const real = fs.realpathSync(target);
        if (seen.has(real)) continue;
        seen.add(real);
        walk(target, depth + 1);
      } else if (resolvedStat.isFile() && entry.name !== '.mcp.json') {
        hash.update(path.relative(root, target));
        hash.update(fs.readFileSync(target));
      }
    }
  };
  walk(root, 0); return hash.digest('hex');
}

// `binding` overrides what gets pinned as the staged MCP command.  Codex
// hosts spawn the native facade directly (the default); hosts with
// restricted spawn environments (zcode) pass an interpreter command plus
// args — e.g. the installing node running the staged stdio bridge.  A
// binding may also carry `timeoutMs`, pinned onto the server entry for
// hosts whose MCP client caps tool calls at a short default (zcode:
// 30000ms) that a long `external_subagent_wait` would exceed.
//
// The publish protocol keeps two private sibling trees beside the target,
// named `.<target>.candidate.<pid>.<uuid>` and `.<target>.prior.<pid>.<uuid>`.

const siblingSuffix = () => `${process.pid}.${crypto.randomUUID()}`;

function discardTree(dir) {
  try { fs.rmSync(dir, { recursive: true, force: true }); } catch { /* best-effort cleanup */ }
}

// Parse and rewrite the CANDIDATE tree's `.mcp.json` for the final binding
// paths.  This runs before any managed byte moves, so an unreadable or
// incomplete MCP payload fails the refresh while the live tree stays intact.
function bindCandidate(candidate, paths, command, args, timeoutMs) {
  const mcpPath = path.join(candidate, '.mcp.json');
  let mcp;
  try {
    mcp = JSON.parse(fs.readFileSync(mcpPath, 'utf8'));
  } catch (error) {
    throw new CliError('INVALID_PLUGIN_SOURCE', `plugin MCP config is not readable JSON (${error.message})`);
  }
  const server = mcp.mcpServers?.external_subagent;
  if (!server) throw new CliError('INVALID_PLUGIN_SOURCE', 'plugin MCP server is missing');
  server.command = command;
  if (args !== null) server.args = args;
  if (timeoutMs !== null) server.timeoutMs = timeoutMs;
  server.env = { ...(server.env || {}), ZCODE_AGENTD_SOCKET: paths.socket };
  fs.writeFileSync(mcpPath, `${JSON.stringify(mcp, null, 2)}\n`, { mode: 0o600 });
}

// Prepare a validated replacement for `staging` (AUD-002).  Ownership of an
// existing managed tree is checked first; publish() then builds the
// candidate beside the target — same parent directory, so every swap step
// is a rename — and the returned handle coordinates the publish with the
// caller's host binding transaction: restore() undoes a published swap,
// complete() drops the retained prior tree once the transaction committed.
export function preparePluginStage(source, staging, paths, binding = {}) {
  const command = binding.command || nativeBinary('external-subagent-mcp');
  const args = Array.isArray(binding.args) ? binding.args : null;
  const timeoutMs = Number.isFinite(binding.timeoutMs) ? binding.timeoutMs : null;
  if (fs.existsSync(staging)) {
    const existing = path.join(staging, '.codex-plugin', 'plugin.json');
    if (!fs.existsSync(existing) || JSON.parse(fs.readFileSync(existing, 'utf8')).name !== PLUGIN_NAME) {
      throw new CliError('PLUGIN_STAGING_CONFLICT', `staging path is not managed by ${PLUGIN_NAME}`);
    }
    // Managed staging is refreshable: skill/docs may have changed since the
    // last install. Keep the ownership check above, then replace its contents
    // from the current source.
    const priorMcp = JSON.parse(fs.readFileSync(path.join(staging, '.mcp.json'), 'utf8'));
    const priorServer = priorMcp.mcpServers?.external_subagent;
    const priorCommand = priorServer?.command;
    // Refreshable when the prior binding is any form this product wrote:
    // the requested binding, the native facade (earlier managed binding),
    // or the shipped placeholder.
    const managedCommands = new Set([command, nativeBinary('external-subagent-mcp'), '__EXTERNAL_SUBAGENT_MCP_EXECUTABLE__']);
    const commandMatches = managedCommands.has(priorCommand);
    // An interpreter-form binding also pins its script inside the staged
    // tree itself, so a prior args[0] living in this staging proves the
    // binding is ours even after the pinned interpreter path drifted
    // (e.g. a Homebrew node upgrade replaced the Cellar path).
    const priorArgs = Array.isArray(priorServer?.args) ? priorServer.args : [];
    const argsOwned = typeof priorArgs[0] === 'string'
      && path.resolve(priorArgs[0]).startsWith(`${path.resolve(staging)}${path.sep}`);
    // Args are only load-bearing for interpreter bindings (e.g. node running
    // the staged bridge); a prior native-form binding carried none, and the
    // rewrite below installs the requested form regardless.
    const argsMatch = args === null || priorCommand !== command || JSON.stringify(priorArgs) === JSON.stringify(args);
    if (!priorServer || (!commandMatches && !argsOwned) || !argsMatch || priorServer.env?.ZCODE_AGENTD_SOCKET !== paths.socket) {
      throw new CliError('PLUGIN_STAGING_CONFLICT', 'staging MCP binding differs from the managed product endpoint');
    }
  }
  const parent = path.dirname(staging);
  const candidate = path.join(parent, `.${path.basename(staging)}.candidate.${siblingSuffix()}`);
  const prior = path.join(parent, `.${path.basename(staging)}.prior.${siblingSuffix()}`);
  let published = false;
  return {
    staging,
    // Build and validate the candidate, then swap it in with same-directory
    // renames: target -> prior (when a managed tree exists), candidate ->
    // target.  If the second rename fails, the first is undone before the
    // error surfaces, so publish either replaces the target completely or
    // leaves the prior tree live at the target path.
    publish() {
      fs.mkdirSync(parent, { recursive: true, mode: 0o700 });
      try {
        fs.cpSync(source, candidate, { recursive: true });
        bindCandidate(candidate, paths, command, args, timeoutMs);
        if (fs.existsSync(staging)) {
          fs.renameSync(staging, prior);
          try {
            fs.renameSync(candidate, staging);
          } catch (error) {
            try { fs.renameSync(prior, staging); } catch { /* prior stays at its sibling path; see the crash boundary */ }
            throw error;
          }
        } else {
          fs.renameSync(candidate, staging);
        }
        published = true;
      } catch (error) {
        discardTree(candidate);
        throw error;
      }
    },
    // Undo a published swap: drop the freshly published tree and put the
    // retained prior tree back (or remove the target entirely when this was
    // a first install).  Best-effort by design — the caller's original
    // failure is the one that must surface, so secondary cleanup errors
    // here are suppressed rather than thrown; the boolean return reports
    // whether the pre-publish state was actually restored, so the caller
    // can attach an unrestored staging tree to its error chain (AUD-002)
    // instead of letting the failed restore pass silently.  A restore that
    // cannot put the prior tree back never discards it: the retained
    // sibling stays on disk as the last-good recovery material.
    restore() {
      if (!published) return true;
      const hadPrior = fs.existsSync(prior);
      discardTree(staging);
      if (hadPrior) {
        try { fs.renameSync(prior, staging); } catch { /* prior stays at its sibling path; see the crash boundary */ }
      }
      published = false;
      return hadPrior ? !fs.existsSync(prior) : !fs.existsSync(staging);
    },
    // Drop the retained prior tree once the host transaction committed; a
    // cleanup failure never fails a verified install.
    complete() {
      published = false;
      discardTree(prior);
    },
  };
}

// One-shot form for callers without a host transaction around the swap.
export function stagePlugin(source, staging, paths, binding = {}) {
  const staged = preparePluginStage(source, staging, paths, binding);
  staged.publish();
  staged.complete();
}

// AUD-002: a failed install must still attempt every independent restore.
// Each attempt runs guarded — one restore throwing (a host config or
// marketplace parent that STAYS read-only, say) neither skips the remaining
// restores nor replaces the caller's original error.  Attempts that failed
// are attached to that error as `restoreFailures: [{ resource, code,
// message }]`, so the unrestored-resource list rides the thrown error chain
// where callers and tests can assert it; this is not a log line.
export function guardedRestores(error, attempts) {
  const restoreFailures = [];
  for (const attempt of attempts) {
    try {
      attempt.restore();
    } catch (restoreError) {
      restoreFailures.push({ resource: attempt.resource, code: restoreError.code || 'RESTORE_FAILED', message: restoreError.message });
    }
  }
  if (restoreFailures.length > 0) error.restoreFailures = restoreFailures;
  return error;
}
