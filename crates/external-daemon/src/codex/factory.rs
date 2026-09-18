//! Gated Codex factory and launch resolution: whether the adapter may spawn
//! at all, and how the app-server child command is resolved and validated.

use std::{
    io,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use external_store::TaskRecord;

use super::owner::CodexRuntimeOwner;
use crate::{task_route, LifecycleSink, ManagedRuntime, RuntimeFactory};

/// Whether the Codex factory may spawn app-server processes at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexSpawnGate {
    /// Fail-closed default: the adapter exists but spawn is refused.
    Closed,
    /// Production launch after the persisted runtime/home gate succeeds.
    Enabled,
    /// Controlled test harness only; never constructed by the production
    /// composition root.
    #[cfg(test)]
    TestHarness,
}

/// Resolved Codex child launch contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexLaunch {
    runtime_path: PathBuf,
    home: PathBuf,
}

/// Home precedence for the Codex child: the persisted `agents.codex.home`
/// wins over the inherited `CODEX_HOME`; neither being present rejects the
/// launch instead of ever falling back to `~/.codex`.
pub fn resolve_codex_home(
    configured: Option<&str>,
    inherited: Option<&str>,
) -> Option<Result<PathBuf, &'static str>> {
    match (configured, inherited) {
        (Some(configured), _) => Some(
            Path::new(configured)
                .is_absolute()
                .then(|| PathBuf::from(configured))
                .ok_or("agents.codex.home must be absolute"),
        ),
        (None, Some(inherited)) => Some(
            Path::new(inherited)
                .is_absolute()
                .then(|| PathBuf::from(inherited))
                .ok_or("inherited CODEX_HOME must be absolute"),
        ),
        (None, None) => None,
    }
}

impl CodexLaunch {
    /// Resolve the launch contract from the daemon environment. `main` only
    /// exports `CODEX_HOME` when the persisted configuration supplies one, so
    /// an inherited value here is the deliberate second-priority source.
    pub fn from_environment() -> Result<Self, io::Error> {
        let runtime_path = env_runtime_path()?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "CODEX_RUNTIME_PATH is unavailable")
        })?;
        let home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "CODEX_HOME is unconfigured; refusing a ~/.codex fallback",
                )
            })?;
        let launch = Self { runtime_path, home };
        launch.validate()?;
        Ok(launch)
    }

    #[cfg(test)]
    pub fn new(runtime_path: PathBuf, home: PathBuf) -> Self {
        Self { runtime_path, home }
    }

    /// Fail-closed validation of the resolved contract: an absolute,
    /// executable runtime file and an absolute home. Production and the
    /// test harness share this one predicate.
    fn validate(&self) -> io::Result<()> {
        if !runtime_path_is_executable_file(&self.runtime_path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CODEX_RUNTIME_PATH must be an absolute executable file",
            ));
        }
        if !self.home.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CODEX_HOME must be absolute",
            ));
        }
        Ok(())
    }

    pub fn runtime_path(&self) -> &Path {
        &self.runtime_path
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The pinned child command: `<runtime> app-server --listen stdio://`
    /// with the resolved home exported as `CODEX_HOME` and the task
    /// workspace as the working directory (mirroring the observed probe
    /// launch, where the process cwd matched the thread cwd).
    pub fn command(&self, cwd: &Path) -> Command {
        let mut command = Command::new(&self.runtime_path);
        command.args(["app-server", "--listen", "stdio://"]);
        command.env("CODEX_HOME", &self.home);
        command.current_dir(cwd);
        command
    }
}

/// The one executable-bit predicate shared by the production environment
/// path and the harness launch validation: a runtime must be an absolute,
/// existing, executable file (any execute bit on unix; unchecked elsewhere).
fn runtime_path_is_executable_file(path: &Path) -> bool {
    path.is_absolute() && path.is_file() && {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            true
        }
    }
}

fn env_runtime_path() -> Result<Option<PathBuf>, io::Error> {
    let Some(path) = std::env::var_os("CODEX_RUNTIME_PATH").map(PathBuf::from) else {
        return Ok(None);
    };
    if !runtime_path_is_executable_file(&path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CODEX_RUNTIME_PATH must be an absolute executable file",
        ));
    }
    Ok(Some(path))
}

/// Factory for Codex app-server runtimes. Closed by default: production
/// routing can register the factory without enabling Codex spawn support.
pub struct CodexRuntimeFactory {
    gate: CodexSpawnGate,
    #[cfg(test)]
    launch: Option<CodexLaunch>,
}

impl CodexRuntimeFactory {
    pub fn closed() -> Self {
        Self {
            gate: CodexSpawnGate::Closed,
            #[cfg(test)]
            launch: None,
        }
    }

    pub fn enabled() -> Self {
        Self {
            gate: CodexSpawnGate::Enabled,
            #[cfg(test)]
            launch: None,
        }
    }

    #[cfg(test)]
    pub fn test_harness(launch: Option<CodexLaunch>) -> Self {
        Self {
            gate: CodexSpawnGate::TestHarness,
            launch,
        }
    }

    #[cfg_attr(not(test), allow(unused_variables))]
    fn resolve_launch(&self, task: &TaskRecord) -> io::Result<CodexLaunch> {
        match task_route(task) {
            Ok(crate::TaskRoute::General(prepared)) => {
                let admission = prepared.admission.as_ref().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "codex task is missing its admission identity",
                    )
                })?;
                if admission.agent != "codex" {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "codex factory received a non-codex task",
                    ));
                }
                if !matches!(
                    prepared.permission_mode,
                    external_core::PermissionMode::Plan
                ) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "codex runtime supports only the plan permission mode",
                    ));
                }
                if admission.model.as_deref().is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "codex task is missing its admitted model",
                    ));
                }
            }
            Err(message) => {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, message));
            }
        }
        match self.gate {
            CodexSpawnGate::Closed => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "codex spawn gate is closed; production Codex spawn is not enabled",
            )),
            CodexSpawnGate::Enabled => CodexLaunch::from_environment(),
            #[cfg(test)]
            CodexSpawnGate::TestHarness => {
                let launch = self
                    .launch
                    .clone()
                    .ok_or_else(|| CodexLaunch::from_environment().unwrap_err())?;
                launch.validate()?;
                Ok(launch)
            }
        }
    }
}

impl RuntimeFactory for CodexRuntimeFactory {
    fn spawn(
        &self,
        task: &TaskRecord,
        sink: Arc<dyn LifecycleSink>,
    ) -> io::Result<Arc<dyn ManagedRuntime>> {
        let launch = self.resolve_launch(task)?;
        let cwd = PathBuf::from(&task.workspace_path);
        Ok(Arc::new(CodexRuntimeOwner::spawn(
            launch.command(&cwd),
            sink,
        )?))
    }
}
