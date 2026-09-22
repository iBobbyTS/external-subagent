use crate::{
    task_route, LifecycleSink, RuntimeActivitySnapshot, RuntimeCommandError, RuntimeOwner,
    RuntimeTerminal, SessionReady, TaskRoute, TurnBoundary, TurnSnapshot,
};
use external_contract::StdioMcpServer;
use external_core::ValidatedPermissionDenial;
use external_runtime::ProcessIdentity;
use external_store::TaskRecord;
use std::{
    io,
    path::Path,
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

pub trait ManagedRuntime: Send + Sync + 'static {
    fn identity(&self) -> Option<ProcessIdentity>;
    fn stop(&self, grace: Duration) -> RuntimeTerminal;
    fn wait_terminal(&self, timeout: Duration) -> Option<RuntimeTerminal>;
    fn diagnostic_tail(&self) -> String {
        String::new()
    }
    fn diagnostic_session_id(&self) -> Option<String> {
        None
    }
    fn wait_diagnostics(&self, _timeout: Duration) {}
    fn bootstrap_session(
        &self,
        _job: &TaskRecord,
        _timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        Err(RuntimeCommandError::Unsupported)
    }
    fn bootstrap_session_with_mcp(
        &self,
        task: &TaskRecord,
        _mcp_servers: &[StdioMcpServer],
        timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        self.bootstrap_session(task, timeout)
    }
    fn resume_session_with_mcp(
        &self,
        _task: &TaskRecord,
        _mcp_servers: &[StdioMcpServer],
        _timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        Err(RuntimeCommandError::Unsupported)
    }
    fn send_turn(
        &self,
        _session_id: &str,
        _content: &str,
        _timeout: Duration,
    ) -> Result<Option<String>, RuntimeCommandError> {
        Err(RuntimeCommandError::Unsupported)
    }
    fn stop_turn(
        &self,
        _session_id: &str,
        _timeout: Duration,
    ) -> Result<TurnSnapshot, RuntimeCommandError> {
        Err(RuntimeCommandError::Unsupported)
    }
    fn respond_request(
        &self,
        _correlation_id: &str,
        _decision: &str,
        _content: Option<&str>,
        _validated_denial: Option<&ValidatedPermissionDenial>,
        _deadline: Instant,
    ) -> Result<(), RuntimeCommandError> {
        Err(RuntimeCommandError::Unsupported)
    }
    fn close_session(
        &self,
        _session_id: &str,
        _timeout: Duration,
    ) -> Result<(), RuntimeCommandError> {
        Ok(())
    }
    fn turn_snapshot(&self) -> TurnSnapshot {
        TurnSnapshot {
            generation: 0,
            active: false,
            boundary: None,
        }
    }
    fn activity_snapshot(&self) -> RuntimeActivitySnapshot {
        let turn = self.turn_snapshot();
        RuntimeActivitySnapshot {
            model_request_elapsed: turn.active.then_some(Duration::ZERO),
            transport_idle_elapsed: turn.active.then_some(Duration::ZERO),
            turn,
        }
    }
    fn stop_boundary_count(&self) -> u64 {
        0
    }
    fn finish_turn(&self, boundary: TurnBoundary, grace: Duration) -> RuntimeTerminal {
        let _ = boundary;
        self.stop(grace)
    }
}

impl ManagedRuntime for RuntimeOwner {
    fn identity(&self) -> Option<ProcessIdentity> {
        Some(self.identity())
    }

    fn stop(&self, grace: Duration) -> RuntimeTerminal {
        self.stop(grace)
    }

    fn wait_terminal(&self, timeout: Duration) -> Option<RuntimeTerminal> {
        self.wait_terminal(timeout)
    }

    fn diagnostic_tail(&self) -> String {
        self.driver.diagnostic_tail()
    }

    fn diagnostic_session_id(&self) -> Option<String> {
        self.diagnostic_session_id.lock().unwrap().clone()
    }

    fn wait_diagnostics(&self, timeout: Duration) {
        self.driver.wait_diagnostics(timeout);
    }

    fn bootstrap_session(
        &self,
        task: &TaskRecord,
        timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        self.bootstrap_prepared_session(task, &[], timeout)
    }

    fn bootstrap_session_with_mcp(
        &self,
        task: &TaskRecord,
        mcp_servers: &[StdioMcpServer],
        timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        self.bootstrap_prepared_session(task, mcp_servers, timeout)
    }

    fn resume_session_with_mcp(
        &self,
        task: &TaskRecord,
        mcp_servers: &[StdioMcpServer],
        timeout: Duration,
    ) -> Result<SessionReady, RuntimeCommandError> {
        self.resume_session_with_mcp(task, mcp_servers, timeout)
    }

    fn send_turn(
        &self,
        session_id: &str,
        content: &str,
        timeout: Duration,
    ) -> Result<Option<String>, RuntimeCommandError> {
        self.send_turn(session_id, content, timeout)
    }

    fn stop_turn(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> Result<TurnSnapshot, RuntimeCommandError> {
        self.stop_turn(session_id, timeout)
    }

    fn respond_request(
        &self,
        correlation_id: &str,
        decision: &str,
        content: Option<&str>,
        validated_denial: Option<&ValidatedPermissionDenial>,
        deadline: Instant,
    ) -> Result<(), RuntimeCommandError> {
        self.respond_request(
            correlation_id,
            decision,
            content,
            validated_denial,
            deadline,
        )
    }

    fn close_session(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> Result<(), RuntimeCommandError> {
        self.close_session(session_id, timeout)
    }

    fn turn_snapshot(&self) -> TurnSnapshot {
        self.turn_snapshot()
    }

    fn activity_snapshot(&self) -> RuntimeActivitySnapshot {
        self.turn_tracker.activity_snapshot()
    }

    fn stop_boundary_count(&self) -> u64 {
        self.stop_boundary_count()
    }

    fn finish_turn(&self, boundary: TurnBoundary, grace: Duration) -> RuntimeTerminal {
        self.finish_turn(boundary, grace)
    }
}

pub trait RuntimeFactory: Send + Sync + 'static {
    fn spawn(
        &self,
        task: &TaskRecord,
        sink: Arc<dyn LifecycleSink>,
    ) -> io::Result<Arc<dyn ManagedRuntime>>;
}

pub struct CommandRuntimeFactory<F> {
    command: F,
    require_prepared: bool,
}

impl<F> CommandRuntimeFactory<F> {
    pub fn new(command: F) -> Self {
        Self {
            command,
            require_prepared: false,
        }
    }

    pub fn new_prepared(command: F) -> Self {
        Self {
            command,
            require_prepared: true,
        }
    }
}

/// Bind the daemon-owned policy envelope to every ZCode child.
fn apply_agent_policy_environment(command: &mut Command, task: &TaskRecord) -> io::Result<()> {
    const MAX_AGENT_WRITE_MANIFEST_ENTRIES: usize = 256;
    const MAX_AGENT_WRITE_MANIFEST_BYTES: usize = 64 * 1024;
    let root = Path::new(&task.workspace_path);
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "runtime workspace path must be absolute",
        ));
    }
    let manifest = match task_route(task)
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?
    {
        TaskRoute::General(prepared) => prepared
            .write_manifest
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
    };
    let serialized = serde_json::to_string(&manifest).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("write manifest could not be serialized: {error}"),
        )
    })?;
    if manifest.len() > MAX_AGENT_WRITE_MANIFEST_ENTRIES
        || serialized.len() > MAX_AGENT_WRITE_MANIFEST_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "write manifest exceeds runtime policy bounds",
        ));
    }
    command
        .env("ZCODE_AGENT_POLICY", "1")
        .env(
            "ZCODE_AGENT_PERMISSION_MODE",
            match task_route(task)
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?
            {
                TaskRoute::General(prepared) => match prepared.permission_mode {
                    external_core::PermissionMode::Build => "build",
                    external_core::PermissionMode::Edit => "edit",
                    external_core::PermissionMode::Plan => "plan",
                    external_core::PermissionMode::Yolo => "yolo",
                },
            },
        )
        .env("ZCODE_AGENT_WORKSPACE_ROOT", root)
        .env("ZCODE_AGENT_BOOTSTRAP_ROOTS", "/Applications/ZCode.app")
        .env("ZCODE_AGENT_WRITE_MANIFEST", serialized);
    Ok(())
}

impl<F> RuntimeFactory for CommandRuntimeFactory<F>
where
    F: Fn(&TaskRecord) -> io::Result<Command> + Send + Sync + 'static,
{
    fn spawn(
        &self,
        task: &TaskRecord,
        sink: Arc<dyn LifecycleSink>,
    ) -> io::Result<Arc<dyn ManagedRuntime>> {
        if self.require_prepared {
            match task_route(task)
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?
            {
                TaskRoute::General(prepared) => {
                    prepared
                        .launcher()
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
                }
            }
        }
        let mut command = (self.command)(task)?;
        apply_agent_policy_environment(&mut command, task)?;
        Ok(Arc::new(RuntimeOwner::spawn(command, sink)?))
    }
}
