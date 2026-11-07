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
pub(crate) mod events;

#[cfg(test)]
mod tests;

pub use provider::{
    apply_provider_environment, data_root_for_store, data_root_from_environment,
    generate_personal_provider_config, resolve_builtin_provider_config,
};

use std::{path::Path, process::Command};

/// The `ZCODE_AGENT_PERMISSION_MODE` environment value for one daemon
/// permission mode. Kept beside the envelope below so the projection and
/// the key it feeds cannot drift; the strings deliberately match the
/// `snake_case` serde wire names of `external_core::PermissionMode`.
pub fn permission_mode_environment_value(mode: external_core::PermissionMode) -> &'static str {
    match mode {
        external_core::PermissionMode::Build => "build",
        external_core::PermissionMode::Edit => "edit",
        external_core::PermissionMode::Plan => "plan",
        external_core::PermissionMode::Yolo => "yolo",
    }
}

/// Apply the daemon-owned `ZCODE_AGENT_*` policy envelope (policy flag,
/// permission mode, workspace root, bootstrap roots, write manifest) to one
/// zcode child command. Single owner of the five-key envelope: production
/// spawn (`runtime_owner.rs`) and both probe paths (`agent_status.rs`)
/// build it here so a probe environment cannot drift from the production one.
pub fn apply_policy_environment(
    command: &mut Command,
    workspace: &Path,
    permission_mode: &str,
    write_manifest: &str,
) {
    command
        .env("ZCODE_AGENT_POLICY", "1")
        .env("ZCODE_AGENT_PERMISSION_MODE", permission_mode)
        .env("ZCODE_AGENT_WORKSPACE_ROOT", workspace)
        .env("ZCODE_AGENT_BOOTSTRAP_ROOTS", "/Applications/ZCode.app")
        .env("ZCODE_AGENT_WRITE_MANIFEST", write_manifest);
}

#[cfg(test)]
mod policy_environment_tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn policy_environment_sets_exactly_the_five_keys() {
        let mut command = Command::new("zcode");
        apply_policy_environment(&mut command, Path::new("/tmp/ws"), "plan", "[]");
        let env: BTreeMap<String, Option<String>> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(env.get("ZCODE_AGENT_POLICY"), Some(&Some("1".into())));
        assert_eq!(
            env.get("ZCODE_AGENT_PERMISSION_MODE"),
            Some(&Some("plan".into()))
        );
        assert_eq!(
            env.get("ZCODE_AGENT_WORKSPACE_ROOT"),
            Some(&Some("/tmp/ws".into()))
        );
        assert_eq!(
            env.get("ZCODE_AGENT_BOOTSTRAP_ROOTS"),
            Some(&Some("/Applications/ZCode.app".into()))
        );
        assert_eq!(
            env.get("ZCODE_AGENT_WRITE_MANIFEST"),
            Some(&Some("[]".into()))
        );
        assert_eq!(env.len(), 5);
    }

    #[test]
    fn permission_mode_values_match_the_serde_wire_names() {
        for mode in [
            external_core::PermissionMode::Build,
            external_core::PermissionMode::Edit,
            external_core::PermissionMode::Plan,
            external_core::PermissionMode::Yolo,
        ] {
            assert_eq!(
                serde_json::Value::String(permission_mode_environment_value(mode).to_owned()),
                serde_json::to_value(mode).unwrap()
            );
        }
    }
}
