//! Gated DSH factory: whether the adapter may spawn at all, and the
//! strict-plan build launch resolution against the `external-agent-dsh`
//! profile contracts.

use std::{io, path::PathBuf, sync::Arc};

use external_store::TaskRecord;

use super::owner::DshRuntimeOwner;
use crate::{task_route, LifecycleSink, ManagedRuntime, RuntimeFactory};

/// Whether the DSH factory may spawn adapter processes at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DshSpawnGate {
    /// S04.A production state: the adapter exists but spawn is refused.
    Closed,
    /// Production launch after the managed strict-plan preflight succeeds.
    Enabled,
    /// Controlled test harness only; never constructed by the production
    /// composition root.
    #[cfg(test)]
    TestHarness,
}

/// Factory for DSH ACP runtimes. Closed by default: production routing can
/// register the factory without enabling DSH spawn support.
pub struct DshRuntimeFactory {
    gate: DshSpawnGate,
    #[cfg(test)]
    executable: Option<PathBuf>,
}

impl DshRuntimeFactory {
    pub fn closed() -> Self {
        Self {
            gate: DshSpawnGate::Closed,
            #[cfg(test)]
            executable: None,
        }
    }

    /// Construct the production factory.  The strict patch is resolved from
    /// an explicit environment override so packaged binaries cannot depend on
    /// their current working directory.
    pub fn enabled() -> Self {
        Self {
            gate: DshSpawnGate::Enabled,
            #[cfg(test)]
            executable: None,
        }
    }

    #[cfg(test)]
    pub fn test_harness(executable: Option<PathBuf>) -> Self {
        Self {
            gate: DshSpawnGate::TestHarness,
            executable,
        }
    }
}

impl RuntimeFactory for DshRuntimeFactory {
    fn spawn(
        &self,
        _task: &TaskRecord,
        _sink: Arc<dyn LifecycleSink>,
    ) -> io::Result<Arc<dyn ManagedRuntime>> {
        match self.gate {
            DshSpawnGate::Closed => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "dsh spawn gate is closed; production DSH spawn is not enabled",
            )),
            DshSpawnGate::Enabled => {
                let prepared = match task_route(_task) {
                    Ok(crate::TaskRoute::General(prepared)) => prepared,
                    Err(message) => {
                        return Err(io::Error::new(io::ErrorKind::InvalidInput, message))
                    }
                };
                let executable = std::env::var_os("DSH_RUNTIME_PATH")
                    .map(PathBuf::from)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "DSH_RUNTIME_PATH is unavailable")
                    })?;
                let plan = matches!(
                    prepared.permission_mode,
                    external_core::PermissionMode::Plan
                );
                let managed_patch = if plan && std::env::var_os("DSH_STRICT_PLAN_PATCH").is_none() {
                    Some(
                        tempfile::Builder::new()
                            .prefix("external-dsh-strict-")
                            .tempdir()
                            .map_err(|e| io::Error::other(e.to_string()))?,
                    )
                } else {
                    None
                };
                let patch = std::env::var_os("DSH_STRICT_PLAN_PATCH")
                    .map(PathBuf::from)
                    .or_else(|| {
                        managed_patch
                            .as_ref()
                            .map(|d| d.path().join("strict-plan.patch.yml"))
                    });
                if let Some(dir) = managed_patch.as_ref() {
                    std::fs::write(
                        dir.path().join("strict-plan.patch.yml"),
                        external_agent_dsh::profile::STRICT_PLAN_PATCH_YAML,
                    )
                    .map_err(|e| io::Error::other(e.to_string()))?;
                }
                let mut launch = external_agent_dsh::profile::DshLaunch::new(
                    Some(executable),
                    prepared.workspace.path.clone(),
                    std::env::var_os("DSH_HOME").map(PathBuf::from),
                );
                launch.profile = std::env::var("DSH_PROFILE").ok();
                launch.version = std::env::var("DSH_VERSION").ok();
                if plan {
                    launch.permission_mode = Some("read-only".into());
                    launch.patch = patch;
                    external_agent_dsh::profile::preflight(&launch)
                        .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;
                } else {
                    launch.permission_mode = Some("workspace-write".into());
                    external_agent_dsh::profile::preflight_build(&launch)
                        .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;
                }
                let command = external_agent_dsh::profile::resolve_launch(&launch)?;
                Ok(Arc::new(DshRuntimeOwner::spawn_with_patch(
                    command,
                    _sink,
                    managed_patch,
                )?))
            }
            #[cfg(test)]
            DshSpawnGate::TestHarness => {
                let prepared = match task_route(_task) {
                    Ok(crate::TaskRoute::General(prepared)) => prepared,
                    Err(message) => {
                        return Err(io::Error::new(io::ErrorKind::InvalidInput, message))
                    }
                };
                let executable = self
                    .executable
                    .clone()
                    .or_else(|| std::env::var_os("DSH_RUNTIME_PATH").map(PathBuf::from));
                let mut launch = external_agent_dsh::profile::DshLaunch::new(
                    executable,
                    prepared.workspace.path.clone(),
                    std::env::var_os("DSH_HOME").map(PathBuf::from),
                );
                launch.profile = std::env::var("DSH_PROFILE").ok();
                launch.version = std::env::var("DSH_VERSION").ok();
                let command = external_agent_dsh::profile::resolve_launch(&launch)?;
                let owner = DshRuntimeOwner::spawn(command, _sink)?;
                Ok(Arc::new(owner))
            }
        }
    }
}
