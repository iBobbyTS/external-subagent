import { CliError } from '../errors.mjs';
import { installMcp, installPlugin, uninstallPlugin } from '../install/codex.mjs';
import { registerCodexHome, unregisterCodexHome } from '../install/reconcile.mjs';
import { verifyPayload } from '../install/payload.mjs';

// Command surface for the managed Codex bindings.  Installing claims the
// Codex home in the D08 registry; uninstalling releases the claim.  The MCP
// TOML binding stays an explicit alternative for hosts without plugin
// support.

function parseCommon(args, flags) {
  const options = {};
  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index];
    if (flags.includes(arg)) {
      options[flagName(arg)] = true;
    } else if (arg === '--codex-home') {
      const value = args[index + 1];
      if (!value || value.startsWith('--')) throw new CliError('INVALID_ARGUMENT', '--codex-home requires a value', 2);
      options.codexHome = value;
      index += 1;
    } else {
      throw new CliError('INVALID_ARGUMENT', `unsupported plugin option: ${arg}`, 2);
    }
  }
  return options;
}

function flagName(flag) {
  return flag.replace(/^--/u, '').replaceAll('-', '_');
}

export function pluginCommand(paths, args) {
  const options = parseCommon(args, ['--dry-run', '--uninstall']);
  const installerOptions = withInstallerFlags(options);
  if (options.uninstall) {
    const result = uninstallPlugin(paths, installerOptions);
    if (!options.dry_run && result.uninstalled) {
      const claim = unregisterCodexHome(paths, result.codex_home, 'plugin');
      result.claim_released = claim.unregistered;
    }
    return result;
  }
  const result = installPlugin(paths, installerOptions);
  if (!options.dry_run && result.installed) {
    const payload = safePayloadVersion();
    const claim = registerCodexHome(paths, result.codex_home, { version: payload, digest: result.digest, binding_mode: 'plugin' });
    result.claim = { registered: claim.registered, deduplicated: claim.deduplicated, homes: claim.homes };
  }
  return result;
}

export function mcpCommand(paths, args) {
  const options = parseCommon(args, ['--dry-run', '--uninstall']);
  const installerOptions = withInstallerFlags(options);
  const result = installMcp(paths, installerOptions);
  if (!options.dry_run && result.installed) {
    const claim = registerCodexHome(paths, result.codex_home || installerOptions.codexHome, { binding_mode: 'mcp', status: 'claimed' });
    result.claim = { registered: claim.registered, deduplicated: claim.deduplicated, homes: claim.homes };
  } else if (!options.dry_run && options.uninstall) {
    const claim = unregisterCodexHome(paths, result.codex_home || installerOptions.codexHome, 'mcp');
    result.claim_released = claim.unregistered;
  }
  return result;
}

// The install layer speaks camelCase (dryRun); CLI parsing produces snake_case
// flags.  Normalize at the boundary so --dry-run actually holds the installers
// back instead of running a real install/uninstall.
function withInstallerFlags(options) {
  return { ...options, dryRun: Boolean(options.dry_run) };
}

function safePayloadVersion() {
  try { return verifyPayload().version; } catch { return null; }
}
