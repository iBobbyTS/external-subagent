//! Managed launch profile for the DSH ACP adapter.
//!
//! The single launch rule set is shared by the S03 discovery/catalog probe and
//! the adapter spawn: an explicit executable (resolved by the caller, normally
//! `DSH_RUNTIME_PATH`), the Node wrapper for JavaScript entrypoints, the task
//! workspace as cwd, and an explicit home override only when the caller
//! supplied one. The profile never installs a provider, never writes user
//! configuration, and never invents a provider command: a missing executable
//! fails closed instead of searching the PATH.

use serde_json::Value;
use std::io;
use std::path::PathBuf;
use std::process::Command;

/// Managed build composition declared in `profiles/dsh/build.json`.
///
/// The repository file is the declared source; the embedded default must stay
/// byte-equivalent (a drift test pins this) so a missing file never widens the
/// composition at runtime.
pub const BUILD_PROFILE_JSON: &str = include_str!("../../../profiles/dsh/build.json");

/// Launch inputs for one DSH child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DshLaunch {
    /// Explicit executable path. `None` means the provider is unavailable and
    /// the caller must fail closed instead of searching the PATH.
    pub executable: Option<PathBuf>,
    /// Absolute workspace directory used as the child cwd and the ACP
    /// `session/new` cwd.
    pub workspace: PathBuf,
    /// Explicit provider home override. Absence inherits the environment.
    pub home: Option<PathBuf>,
}

impl DshLaunch {
    pub fn new(
        executable: Option<PathBuf>,
        workspace: impl Into<PathBuf>,
        home: Option<PathBuf>,
    ) -> Self {
        Self {
            executable,
            workspace: workspace.into(),
            home,
        }
    }
}

/// Resolve the managed launch composition into a process command.
///
/// Mirrors the S03 catalog probe launch exactly (Node wrapper for `.js`,
/// `.cjs`, `.mjs` entrypoints) so probe evidence cannot diverge from the
/// adapter path.
pub fn resolve_launch(launch: &DshLaunch) -> io::Result<Command> {
    let executable = launch.executable.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "DSH_RUNTIME_PATH is unavailable; refusing to search the PATH",
        )
    })?;
    if !executable.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "DSH runtime path must be absolute",
        ));
    }
    if !executable.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "DSH runtime path is not a regular file",
        ));
    }
    if !launch.workspace.is_absolute() || !launch.workspace.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "DSH workspace must be an absolute directory",
        ));
    }
    let mut command = if matches!(
        executable.extension().and_then(|value| value.to_str()),
        Some("js" | "cjs" | "mjs")
    ) {
        let mut command = Command::new("node");
        command.arg(executable);
        command
    } else {
        Command::new(executable)
    };
    command.arg("--profile").arg("acp");
    command.current_dir(&launch.workspace);
    if let Some(home) = launch.home.as_deref() {
        if !home.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "DSH home override must be absolute",
            ));
        }
        command.env("DSH_HOME", home);
    }
    Ok(command)
}

/// Parse and validate the managed build composition. Unknown fields or a
/// non-`workspace-write` sandbox are rejected: the build composition is the
/// only DSH permission surface S04.A is allowed to drive.
pub fn build_profile() -> Result<Value, String> {
    let profile: Value = serde_json::from_str(BUILD_PROFILE_JSON)
        .map_err(|error| format!("managed dsh build profile is invalid: {error}"))?;
    validate_build_profile(&profile)?;
    Ok(profile)
}

pub fn validate_build_profile(profile: &Value) -> Result<(), String> {
    if profile.get("agent").and_then(Value::as_str) != Some(crate::DSH_AGENT_NAME) {
        return Err("managed dsh build profile must declare agent=dsh".into());
    }
    let composition = profile
        .get("composition")
        .ok_or_else(|| "managed dsh build profile is missing the composition object".to_string())?;
    if composition.get("sandbox").and_then(Value::as_str) != Some("workspace-write") {
        return Err("managed dsh build profile must pin sandbox=workspace-write".into());
    }
    if composition
        .get("respondable_permissions")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err("managed dsh build profile must pin respondable_permissions=true".into());
    }
    if composition.get("prompt_scope").and_then(Value::as_str) != Some("one-in-flight") {
        return Err("managed dsh build profile must pin prompt_scope=one-in-flight".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace() -> PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn missing_executable_fails_closed_without_path_search() {
        let error = resolve_launch(&DshLaunch::new(None, temp_workspace(), None)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("refusing to search the PATH"));
    }

    #[test]
    fn non_absolute_or_missing_runtime_is_rejected() {
        let error = resolve_launch(&DshLaunch::new(
            Some(PathBuf::from("dsh")),
            temp_workspace(),
            None,
        ))
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let error = resolve_launch(&DshLaunch::new(
            Some(PathBuf::from("/definitely/not/here")),
            temp_workspace(),
            None,
        ))
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn js_entrypoints_wrap_node_and_set_cwd() {
        let script = std::env::temp_dir().join("dsh-profile-fixture-check.mjs");
        std::fs::write(&script, b"").unwrap();
        let command = resolve_launch(&DshLaunch::new(
            Some(script.clone()),
            temp_workspace(),
            None,
        ))
        .unwrap();
        assert_eq!(command.get_program().to_string_lossy(), "node");
        assert!(command.get_args().any(|arg| arg == script.as_os_str()));
        assert_eq!(command.get_current_dir(), Some(temp_workspace().as_path()));
        std::fs::remove_file(script).unwrap();
    }

    #[test]
    fn home_override_is_forwarded_only_when_explicit_and_absolute() {
        let absolute = PathBuf::from("/usr/bin/true");
        assert!(absolute.is_file(), "platform true binary is required");
        let command = resolve_launch(&DshLaunch::new(
            Some(absolute.clone()),
            temp_workspace(),
            Some(PathBuf::from("/fixture/home")),
        ))
        .unwrap();
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == "DSH_HOME")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("/fixture/home"))
        );
        let command =
            resolve_launch(&DshLaunch::new(Some(absolute), temp_workspace(), None)).unwrap();
        assert!(command.get_envs().all(|(key, _)| key != "DSH_HOME"));
    }

    #[test]
    fn build_profile_pins_the_managed_composition() {
        let profile = build_profile().unwrap();
        assert_eq!(profile["agent"], "dsh");
        assert_eq!(profile["composition"]["sandbox"], "workspace-write");
    }

    #[test]
    fn build_profile_rejects_drifted_compositions() {
        let baseline = build_profile().unwrap();
        let mut widened = baseline.clone();
        widened["composition"]["sandbox"] = Value::String("danger-full-access".into());
        assert!(validate_build_profile(&widened).is_err());
        let mut unresponsive = baseline.clone();
        unresponsive["composition"]["respondable_permissions"] = Value::Bool(false);
        assert!(validate_build_profile(&unresponsive).is_err());
        let mut pooled = baseline.clone();
        pooled["composition"]["prompt_scope"] = Value::String("concurrent".into());
        assert!(validate_build_profile(&pooled).is_err());
        let mut wrong_agent = baseline.clone();
        wrong_agent["agent"] = Value::String("zcode".into());
        assert!(validate_build_profile(&wrong_agent).is_err());
        assert!(validate_build_profile(&Value::Null).is_err());
    }
}
