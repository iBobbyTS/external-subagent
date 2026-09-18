import { CliError } from '../errors.mjs';
import { installMcp, installPlugin, uninstallPlugin } from '../install/codex.mjs';
import { installZcodePlugin, uninstallZcodePlugin } from '../install/zcode.mjs';
import { registerCodexHome, unregisterCodexHome } from '../install/reconcile.mjs';
import { verifyPayload } from '../install/payload.mjs';

// Command surface for the managed host bindings.  `install-plugin` takes an
// optional host argument: codex (the default, for backward compatibility)
// stages the plugin and goes through the official codex CLI, claiming the
// Codex home in the D08 registry; zcode stages the same plugin into a
// product-owned tree and registers it through the ZCode config's inline
// plugin dirs.  The MCP TOML binding stays an explicit codex-only
// alternative for hosts without plugin support.

const PLUGIN_HOSTS = ['codex', 'zcode'];

function parseCommon(args, flags) {
  const options = {};
  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index];
    if (!arg.startsWith('--')) {
      if (options.host) throw new CliError('INVALID_ARGUMENT', `plugin host was already given: ${options.host}`, 2);
      if (!PLUGIN_HOSTS.includes(arg)) throw new CliError('INVALID_ARGUMENT', `unsupported plugin host: ${arg} (expected ${PLUGIN_HOSTS.join(' or ')})`, 2);
      options.host = arg;
    } else if (flags.includes(arg)) {
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
  if (options.host === 'zcode') {
    if (options.codexHome) throw new CliError('INVALID_ARGUMENT', '--codex-home applies to the codex host only', 2);
    return options.uninstall ? uninstallZcodePlugin(paths, installerOptions) : installZcodePlugin(paths, installerOptions);
  }
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
