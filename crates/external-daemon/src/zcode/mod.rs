//! Daemon-owned materialization of ZCode's personal provider registry.
//!
//! The ZCode app-server resolves its provider catalog from a builtin template
//! bundle plus an optional personal provider file selected by environment.
//! This module composes a daemon-owned personal file from the user's CLI
//! credential layer (`~/.zcode/cli/config.json`, zai coding-plan key), the
//! user's existing personal rules (`~/.zcode/v2/provider_config.json`, copied
//! verbatim) and the builtin template's zai model list, then exports the
//! builtin/personal/data-root environment group to every zcode child (spawn
//! and probe). The generated file carries a plaintext API key, so it is
//! written 0600 under the daemon data root and the key never reaches logs,
//! errors or `Debug` output.

mod provider;

#[cfg(test)]
mod tests;

pub use provider::{
    apply_provider_environment, data_root_for_store, data_root_from_environment,
    generate_personal_provider_config, resolve_builtin_provider_config,
};
