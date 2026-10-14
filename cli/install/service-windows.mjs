import { CliError } from '../errors.mjs';

// TODO(windows-real-machine): Windows service integration skeleton.
// Mutating and activation operations fail closed with UNSUPPORTED_PLATFORM_SERVICE
// until native Windows Service control (SCM) or background task management is implemented.
const UNSUPPORTED_ERROR_MESSAGE = 'windows: service integration pending real-machine implementation';

// Read-only registration query returns the unavailable view form isomorphic to
// service-linux.mjs systemdServiceRegistrationStatus NO_USER_SYSTEMD_SESSION branch.
export function windowsServiceRegistrationStatus(_options = {}) {
  return {
    query: 'unavailable',
    registered: null,
    reason: UNSUPPORTED_ERROR_MESSAGE,
  };
}

export function bootstrapServiceWindows(_paths, _options = {}) {
  throw new CliError('UNSUPPORTED_PLATFORM_SERVICE', UNSUPPORTED_ERROR_MESSAGE);
}

export function bootoutServiceWindows(_paths, _options = {}) {
  throw new CliError('UNSUPPORTED_PLATFORM_SERVICE', UNSUPPORTED_ERROR_MESSAGE);
}

export function installServiceDefinitionWindows(_paths, _options = {}) {
  throw new CliError('UNSUPPORTED_PLATFORM_SERVICE', UNSUPPORTED_ERROR_MESSAGE);
}

export async function activateServiceWindows(_paths, _candidate, _options = {}) {
  throw new CliError('UNSUPPORTED_PLATFORM_SERVICE', UNSUPPORTED_ERROR_MESSAGE);
}
