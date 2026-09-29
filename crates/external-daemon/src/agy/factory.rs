//! Gated `agy` factory and launch resolution: whether the adapter may spawn
//! at all, and how the stream-json child command is resolved and validated.

use std::{
    io,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use external_agent_agy::launch::{launch_args, AgyEffort, AgyLaunchOptions, AgyPermissionMode};
use external_store::TaskRecord;

use super::owner::AgyRuntimeOwner;
use crate::{task_route, LifecycleSink, ManagedRuntime, RuntimeFactory};

/// Whether the `agy` factory may spawn stream-json processes at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgySpawnGate {
    /// Fail-closed default: the adapter exists but spawn is refused.
    Closed,
    /// Production launch after the configured runtime-path gate succeeds.
    Enabled,
    /// Controlled test harness only; never constructed by the production
    /// composition root.
    #[cfg(test)]
    TestHarness,
}

/// The resolved `agy` launch contract: an absolute executable and the admitted
/// model/effort/permission selections assembled into the pinned stream-json
/// argv. The model is optional: a `native` admission (no explicit and no
/// configured default) launches without `--model` and lets the `agy` CLI use
/// its own default.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AgyLaunch {
    runtime_path: PathBuf,
    model: Option<String>,
    effort: Option<AgyEffort>,
    permission: AgyPermissionMode,
}

impl AgyLaunch {
    fn validate(&self) -> io::Result<()> {
        if !runtime_path_is_executable_file(&self.runtime_path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AGY_RUNTIME_PATH must be an absolute executable file",
            ));
        }
        Ok(())
    }

    /// The pinned child command: the absolute runtime with the stream-json
    /// framing plus the admitted model/effort/permission, cwd pinned to the
    /// task workspace. A native admission (no admitted model) omits `--model`.
    fn command(&self, cwd: &Path) -> Command {
        let mut command = Command::new(&self.runtime_path);
        command.args(launch_args(&AgyLaunchOptions {
            model: self.model.clone(),
            effort: self.effort,
            permission: Some(self.permission),
            conversation: None,
        }));
        command.current_dir(cwd);
        command
    }
}

/// The one executable-bit predicate shared by the production environment path
/// and the harness launch validation: an absolute, existing, executable file.
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
    let Some(path) = std::env::var_os("AGY_RUNTIME_PATH").map(PathBuf::from) else {
        return Ok(None);
    };
    if !runtime_path_is_executable_file(&path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "AGY_RUNTIME_PATH must be an absolute executable file",
        ));
    }
    Ok(Some(path))
}

/// Factory for `agy` stream-json runtimes. Closed by default: production
/// routing can register the factory without enabling `agy` spawn support.
pub struct AgyRuntimeFactory {
    gate: AgySpawnGate,
    #[cfg(test)]
    executable: Option<PathBuf>,
}

impl AgyRuntimeFactory {
    pub fn closed() -> Self {
        Self {
            gate: AgySpawnGate::Closed,
            #[cfg(test)]
            executable: None,
        }
    }

    pub fn enabled() -> Self {
        Self {
            gate: AgySpawnGate::Enabled,
            #[cfg(test)]
            executable: None,
        }
    }

    #[cfg(test)]
    pub fn test_harness(executable: Option<PathBuf>) -> Self {
        Self {
            gate: AgySpawnGate::TestHarness,
            executable,
        }
    }

    /// Resolve the launch for one admitted task: `agy` identities only, an
    /// optional admitted model (a missing model is the `native` fallback and
    /// leaves `--model` off the argv), and one of the two open permission
    /// postures (`build`, `yolo`). The unopened plan/edit modes are refused
    /// here instead of ever reaching a child.
    fn resolve_launch(&self, task: &TaskRecord) -> io::Result<AgyLaunch> {
        let prepared = match task_route(task) {
            Ok(crate::TaskRoute::General(prepared)) => prepared,
            Err(message) => return Err(io::Error::new(io::ErrorKind::InvalidInput, message)),
        };
        let admission = prepared.admission.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "agy task is missing its admission identity",
            )
        })?;
        if admission.agent != "agy" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "agy factory received a non-agy task",
            ));
        }
        // A missing (or empty) admitted model is the `native` fallback: the
        // launch stays spawnable and omits `--model`, so the `agy` CLI picks
        // its own default model.
        let model = admission.model.clone().filter(|model| !model.is_empty());
        let permission = match prepared.permission_mode {
            external_core::PermissionMode::Build => AgyPermissionMode::Build,
            external_core::PermissionMode::Yolo => AgyPermissionMode::Yolo,
            external_core::PermissionMode::Plan | external_core::PermissionMode::Edit => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "agy admits only build and yolo permission modes",
                ))
            }
        };
        let effort = match admission.effort.as_deref() {
            None => None,
            Some(token) => Some(AgyEffort::parse(token).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unsupported agy effort {token:?}"),
                )
            })?),
        };
        let runtime_path = match self.gate {
            AgySpawnGate::Closed => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "agy spawn gate is closed; production agy spawn is not enabled",
                ))
            }
            AgySpawnGate::Enabled => env_runtime_path()?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "AGY_RUNTIME_PATH is unavailable")
            })?,
            #[cfg(test)]
            AgySpawnGate::TestHarness => self
                .executable
                .clone()
                .or_else(|| std::env::var_os("AGY_RUNTIME_PATH").map(PathBuf::from))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "AGY_RUNTIME_PATH is unavailable")
                })?,
        };
        let launch = AgyLaunch {
            runtime_path,
            model,
            effort,
            permission,
        };
        launch.validate()?;
        Ok(launch)
    }
}

impl RuntimeFactory for AgyRuntimeFactory {
    fn spawn(
        &self,
        task: &TaskRecord,
        sink: Arc<dyn LifecycleSink>,
    ) -> io::Result<Arc<dyn ManagedRuntime>> {
        let launch = self.resolve_launch(task)?;
        let cwd = PathBuf::from(&task.workspace_path);
        Ok(Arc::new(AgyRuntimeOwner::spawn(
            launch.command(&cwd),
            sink,
        )?))
    }
}
