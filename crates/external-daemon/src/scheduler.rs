use external_store::StoreError;
use std::{
    fmt,
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerConfig {
    pub global_max_agents: usize,
    pub per_workspace_max_agents: usize,
    pub stop_grace: Duration,
    pub bootstrap_timeout: Duration,
    pub control_timeout: Duration,
    pub runtime_source: Option<PathBuf>,
}
impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            global_max_agents: usize::MAX,
            per_workspace_max_agents: 1,
            stop_grace: Duration::from_secs(1),
            bootstrap_timeout: Duration::from_secs(2),
            control_timeout: Duration::from_secs(2),
            runtime_source: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ControlDeadline {
    expires_at: Instant,
}
impl ControlDeadline {
    pub(crate) fn new(budget: Duration) -> Self {
        Self {
            expires_at: Instant::now() + budget,
        }
    }
    pub(crate) fn remaining(self) -> Option<Duration> {
        self.expires_at
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
    }
    pub(crate) fn runtime_phase(self, stop_grace: Duration) -> Option<Duration> {
        self.runtime_phase_deadline(stop_grace)?
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
    }
    pub(crate) fn runtime_phase_deadline(self, stop_grace: Duration) -> Option<Instant> {
        let remaining = self.remaining()?;
        let cleanup = stop_grace
            .checked_mul(3)
            .unwrap_or(remaining)
            .min(remaining / 2);
        self.expires_at
            .checked_sub(cleanup)
            .filter(|deadline| *deadline > Instant::now())
    }
    pub(crate) fn cleanup_grace(self, configured: Duration) -> Duration {
        self.remaining()
            .map(|remaining| configured.min(remaining / 3))
            .unwrap_or(Duration::ZERO)
    }
}

#[derive(Debug)]
pub enum SchedulerError {
    Store(StoreError),
    InvalidConfig(String),
    RuntimeSpawn { agent_id: String, message: String },
    LifecycleSink { agent_id: String, message: String },
    RuntimeCommand { agent_id: String, message: String },
}
impl fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::InvalidConfig(m) => write!(f, "invalid scheduler config: {m}"),
            Self::RuntimeSpawn { agent_id, message } => {
                write!(f, "runtime spawn failed for {agent_id}: {message}")
            }
            Self::LifecycleSink { agent_id, message } => {
                write!(f, "lifecycle sink failed for {agent_id}: {message}")
            }
            Self::RuntimeCommand { agent_id, message } => {
                write!(f, "runtime command failed for {agent_id}: {message}")
            }
        }
    }
}
impl std::error::Error for SchedulerError {}
impl From<StoreError> for SchedulerError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

use super::*;

impl Scheduler {
    fn late_ingress_error(agent_id: &str, reason: &'static str) -> SchedulerError {
        SchedulerError::RuntimeCommand {
            agent_id: agent_id.into(),
            message: reason.into(),
        }
    }

    fn require_runtime_ingress(
        agent_id: &str,
        runtime_lifecycle: &RuntimeLifecycle,
    ) -> Result<(), SchedulerError> {
        match runtime_lifecycle.ingress_reason() {
            Some(reason) => Err(Self::late_ingress_error(agent_id, reason)),
            None => Ok(()),
        }
    }

    fn request_cooperative_stop(
        runtime: &Arc<dyn ManagedRuntime>,
        session_id: &str,
        runtime_lifecycle: &RuntimeLifecycle,
        timeout: Duration,
    ) -> Option<String> {
        let current = runtime.turn_snapshot();
        runtime_lifecycle.request_stop(&current);
        if runtime_lifecycle.acknowledge_boundary(&current) {
            return None;
        }
        if current.active {
            match runtime.stop_turn(session_id, timeout) {
                Ok(boundary) if runtime_lifecycle.acknowledge_boundary(&boundary) => return None,
                Ok(_) => {
                    runtime_lifecycle.force_terminating();
                    return Some("session/stop returned without a matching turn boundary".into());
                }
                Err(error) => {
                    runtime_lifecycle.force_terminating();
                    return Some(error.to_string());
                }
            }
        }
        runtime_lifecycle.force_terminating();
        Some("active turn had no matching stop boundary".into())
    }

    fn control_deadline(&self) -> ControlDeadline {
        ControlDeadline::new(self.inner.config.control_timeout)
    }

    fn control_timeout_error(agent_id: &str) -> SchedulerError {
        SchedulerError::RuntimeCommand {
            agent_id: agent_id.into(),
            message: "control operation deadline elapsed".into(),
        }
    }

    fn lock_operation<'a>(
        &self,
        agent_id: &str,
        operation: &'a Mutex<()>,
        deadline: ControlDeadline,
    ) -> Result<MutexGuard<'a, ()>, SchedulerError> {
        loop {
            match operation.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
                Err(TryLockError::WouldBlock) => {
                    let Some(remaining) = deadline.remaining() else {
                        return Err(Self::control_timeout_error(agent_id));
                    };
                    thread::sleep(remaining.min(Duration::from_millis(1)));
                }
            }
        }
    }

    fn runtime_phase_timeout(
        &self,
        agent_id: &str,
        deadline: ControlDeadline,
    ) -> Result<Duration, SchedulerError> {
        deadline
            .runtime_phase(self.inner.config.stop_grace)
            .ok_or_else(|| Self::control_timeout_error(agent_id))
    }

    fn runtime_phase_deadline(
        &self,
        agent_id: &str,
        deadline: ControlDeadline,
    ) -> Result<Instant, SchedulerError> {
        deadline
            .runtime_phase_deadline(self.inner.config.stop_grace)
            .ok_or_else(|| Self::control_timeout_error(agent_id))
    }

    pub fn new(
        owner_id: impl Into<String>,
        store: Arc<Store>,
        factory: Arc<dyn RuntimeFactory>,
        config: SchedulerConfig,
    ) -> Result<Self, SchedulerError> {
        if config.per_workspace_max_agents == 0
            || config.bootstrap_timeout.is_zero()
            || config.control_timeout.is_zero()
        {
            return Err(SchedulerError::InvalidConfig(
                "scheduler limits and bounded control waits must be positive".into(),
            ));
        }
        Ok(Self {
            inner: Arc::new(SchedulerInner {
                owner_id: owner_id.into(),
                store,
                factory,
                config,
                #[cfg(test)]
                preflight_hook: None,
                #[cfg(test)]
                response_claim_hook: Mutex::new(None),
                #[cfg(test)]
                result_persist_hook: Mutex::new(None),
                state: Mutex::new(SchedulerState::default()),
            }),
        })
    }

    #[cfg(test)]
    fn with_preflight_hook(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("preflight hook must attach before scheduler cloning")
            .preflight_hook = Some(Arc::new(hook));
        self
    }

    #[cfg(test)]
    fn run_response_claim_hook(&self, stage: ResponseClaimHookStage, agent_id: &str) {
        if let Some(hook) = self.inner.response_claim_hook.lock().unwrap().clone() {
            hook(stage, agent_id);
        }
    }

    #[cfg(test)]
    fn set_result_persist_hook(&self, hook: Arc<ResultPersistHook>) {
        *self.inner.result_persist_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    fn run_result_persist_hook(&self, agent_id: &str) {
        if let Some(hook) = self.inner.result_persist_hook.lock().unwrap().clone() {
            hook(agent_id);
        }
    }

    pub fn store(&self) -> Arc<Store> {
        Arc::clone(&self.inner.store)
    }

    /// Return only the configured runtime path. Reading this value never
    /// starts or probes the runtime, so callers must not label it observed.
    pub fn configured_runtime_source(&self) -> Option<PathBuf> {
        self.inner.config.runtime_source.clone()
    }

    pub fn enqueue_general(
        &self,
        manifest: &GeneralTaskManifest,
    ) -> Result<SubmittedTask, SchedulerError> {
        let mut manifest = manifest.clone();
        manifest.agent_id = self.inner.store.reserve_task_id()?;
        let prepared = GeneralTaskPreparer::new(Vec::new())
            .and_then(|preparer| preparer.prepare_direct_submission(&manifest))
            .map_err(|error| SchedulerError::InvalidConfig(error.to_string()))?;
        let prepared_json = serde_json::to_string(&prepared)
            .map_err(|error| SchedulerError::InvalidConfig(error.to_string()))?;
        let initial_prompt = general_initial_prompt(&prepared)?;
        let task = NewTask {
            agent_id: prepared.agent_id.clone(),
            repository: prepared.repository.to_string_lossy().into_owned(),
            workspace_path: prepared.workspace.path.to_string_lossy().into_owned(),
            runtime_hash: None,
            prepared_launch_json: prepared_json,
            prepared_launch_sha256: prepared.prepared_sha256.clone(),
            initial_prompt,
        };
        let enqueued = self.inner.store.enqueue_task_authoritative(&task)?;
        Ok(SubmittedTask {
            task: enqueued.task,
            disposition: enqueued.disposition,
        })
    }

    pub fn reconcile_startup(&self) -> Result<Vec<(String, TaskOutcome)>, SchedulerError> {
        // Startup reconciliation is valid only before this scheduler owns a runtime.
        let active = self.inner.state.lock().unwrap().active.is_empty();
        if !active {
            return Err(SchedulerError::InvalidConfig(
                "startup reconciliation requires an empty active set".into(),
            ));
        }
        let tasks = self.inner.store.startup_recovery_tasks()?;
        let mut recovered = Vec::with_capacity(tasks.len());
        for task in tasks {
            self.recover_startup_task(&task)?;
            let terminal = self.inner.store.get_task(&task.agent_id)?.ok_or_else(|| {
                SchedulerError::Store(StoreError::InvalidState(
                    "startup recovery task disappeared".into(),
                ))
            })?;
            let outcome = terminal.outcome.ok_or_else(|| {
                SchedulerError::Store(StoreError::InvalidState(
                    "startup recovery did not terminalize the task".into(),
                ))
            })?;
            recovered.push((task.agent_id, outcome));
        }
        Ok(recovered)
    }

    fn recover_startup_task(&self, task: &TaskRecord) -> Result<(), SchedulerError> {
        match (&task.runtime_agent_id, &task.process_identity) {
            (Some(_), Some(identity)) => {
                stop_and_reap_persisted_process_group(
                    &ProcessIdentity {
                        pid: identity.pid,
                        pgid: identity.process_group_id,
                        uid: identity.uid,
                        start_token: identity.start_token.clone(),
                    },
                    self.inner.config.stop_grace,
                )
                .map_err(|error| SchedulerError::RuntimeCommand {
                    agent_id: task.agent_id.clone(),
                    message: format!("startup process-group recovery failed: {error}"),
                })?;
            }
            (None, None) if matches!(task.phase, TaskPhase::Preparing | TaskPhase::Terminal) => {}
            _ => {
                return Err(SchedulerError::RuntimeCommand {
                    agent_id: task.agent_id.clone(),
                    message: "startup runtime identity is incomplete; refusing unverified reap"
                        .into(),
                })
            }
        }

        let route = task_route(task).map_err(SchedulerError::InvalidConfig)?;
        validate_task_route(Some(task), &route).map_err(SchedulerError::InvalidConfig)?;
        let TaskRoute::General(prepared) = route;
        if task.phase == TaskPhase::Terminal {
            if self.inner.store.task_result(&task.agent_id)?.is_none() {
                return Err(SchedulerError::RuntimeCommand {
                    agent_id: task.agent_id.clone(),
                    message: "terminal startup recovery task has no immutable result".into(),
                });
            }
            let cleanup = GeneralFinalizer::finalize(&prepared, CompletionOutcome::RuntimeLost);
            if !cleanup.cleaned {
                return Err(SchedulerError::RuntimeCommand {
                    agent_id: task.agent_id.clone(),
                    message: "terminal startup worktree recovery did not prove cleanup".into(),
                });
            }
            self.inner.store.reap_task(&task.agent_id)?;
            return Ok(());
        }
        let (outcome, reason_code, message) = if task.stop_requested || task.close_requested {
            (
                CompletionOutcome::Cancelled,
                "CANCELLED",
                "task cancellation was recovered after daemon restart",
            )
        } else {
            (
                CompletionOutcome::RuntimeLost,
                "DAEMON_RESTART_RUNTIME_LOST",
                "daemon restarted while task runtime was active",
            )
        };
        let completion = finalized_general(&prepared, outcome, reason_code, message);
        if !completion.cleaned {
            return Err(SchedulerError::RuntimeCommand {
                agent_id: task.agent_id.clone(),
                message: "startup worktree recovery did not prove cleanup".into(),
            });
        }
        persist_general_result(&self.inner.store, &task.agent_id, &prepared, &completion)?;
        self.inner.store.reap_task(&task.agent_id)?;
        Ok(())
    }

    pub fn start_ready(&self) -> Result<Vec<String>, SchedulerError> {
        let mut started = Vec::new();
        loop {
            let claim = self.inner.store.claim_next(
                &self.inner.owner_id,
                usize::MAX,
                self.inner.config.per_workspace_max_agents,
            )?;
            let Some(claim) = claim else {
                return Ok(started);
            };
            let agent_id = claim.task.agent_id.clone();
            match self.start_claim(claim) {
                Ok(true) => started.push(agent_id),
                Ok(false) => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn start_claim(&self, claim: TaskClaim) -> Result<bool, SchedulerError> {
        let task = self.inner.store.get_task(&claim.task.agent_id)?;
        let route = match task_route(&claim.task) {
            Ok(route) => route,
            Err(message) => {
                if task.is_some() {
                    self.inner.store.store_task_result(
                        &claim.task.agent_id,
                        &minimal_task_result(
                            CompletionOutcome::ResultInvalid,
                            &message,
                            "PREPARED_LAUNCH_INVALID",
                        ),
                    )?;
                } else {
                    self.inner.store.fail_claim(
                        &claim.task.agent_id,
                        claim.owner_epoch,
                        "PREPARED_LAUNCH_INVALID",
                        &message,
                    )?;
                }
                return Err(SchedulerError::InvalidConfig(message));
            }
        };
        if let Err(message) = validate_task_route(task.as_ref(), &route) {
            if task.is_some() {
                self.inner.store.store_task_result(
                    &claim.task.agent_id,
                    &minimal_task_result(
                        CompletionOutcome::ResultInvalid,
                        &message,
                        "TASK_ROUTE_INVALID",
                    ),
                )?;
            } else {
                self.inner.store.fail_claim(
                    &claim.task.agent_id,
                    claim.owner_epoch,
                    "TASK_ROUTE_INVALID",
                    &message,
                )?;
            }
            return Err(SchedulerError::InvalidConfig(message));
        }
        #[cfg(test)]
        if task.is_some() {
            if let Some(hook) = &self.inner.preflight_hook {
                hook();
            }
        }
        let resumed = claim.task.zcode_session_id.is_some();
        let _policy = match route_policy(&route, resumed) {
            Ok(policy) => policy.map(Arc::new),
            Err(error) => {
                let message = error.to_string();
                self.finish_unstarted_route(
                    &claim.task.agent_id,
                    claim.owner_epoch,
                    &route,
                    task.as_ref(),
                    UnstartedTerminal {
                        outcome: CompletionOutcome::ResultInvalid,
                        reason_code: "PREPARED_CONTENT_INVALID",
                        message: &message,
                    },
                    true,
                )?;
                return Err(SchedulerError::InvalidConfig(message));
            }
        };
        let runtime_agent_id = format!("{}:{}", claim.task.agent_id, claim.owner_epoch);
        let runtime_lifecycle = Arc::new(RuntimeLifecycle::new(claim.owner_epoch));
        let activity = Arc::new(PassiveActivityTracker::new(
            observation::runtime_source_verified(self.inner.config.runtime_source.as_deref()),
        ));
        let sink = Arc::new(StoreLifecycleSink::new(
            Arc::clone(&self.inner.store),
            claim.task.agent_id.clone(),
            runtime_agent_id.clone(),
            claim.owner_epoch,
            Arc::clone(&runtime_lifecycle),
            Arc::clone(&activity),
        ));
        let lifecycle_sink: Arc<dyn LifecycleSink> = sink.clone();
        let runtime = match self.inner.factory.spawn(&claim.task, lifecycle_sink) {
            Ok(runtime) => runtime,
            Err(error) => {
                let message = error.to_string();
                self.record_runtime_failure(
                    &claim.task.agent_id,
                    claim.task.zcode_session_id.as_deref(),
                    "spawn",
                    "RUNTIME_SPAWN_FAILED",
                    &message,
                    None,
                );
                if let Err(store_error) = self.finish_unstarted_route(
                    &claim.task.agent_id,
                    claim.owner_epoch,
                    &route,
                    task.as_ref(),
                    UnstartedTerminal {
                        outcome: CompletionOutcome::Failed,
                        reason_code: "RUNTIME_SPAWN_FAILED",
                        message: &message,
                    },
                    true,
                ) {
                    self.record_failure(&claim.task.agent_id, store_error.to_string());
                }
                return Err(SchedulerError::RuntimeSpawn {
                    agent_id: claim.task.agent_id,
                    message,
                });
            }
        };
        activity.confirm_runtime_source(observation::runtime_source_verified(
            self.inner.config.runtime_source.as_deref(),
        ));
        let mcp_servers = Vec::new();
        let bootstrap_timeout = self.inner.config.bootstrap_timeout;
        let session = match if claim.task.zcode_session_id.is_some() {
            runtime.resume_session_with_mcp(&claim.task, &mcp_servers, bootstrap_timeout)
        } else {
            runtime.bootstrap_session_with_mcp(&claim.task, &mcp_servers, bootstrap_timeout)
        } {
            Ok(session) => session,
            Err(error) => {
                let message = error.to_string();
                let terminal = runtime.stop(self.inner.config.stop_grace);
                let resources_reaped = terminal_proves_process_group_reaped(&terminal);
                let (outcome, code) = (CompletionOutcome::Failed, "SESSION_START_FAILED");
                self.record_runtime_failure(
                    &claim.task.agent_id,
                    claim.task.zcode_session_id.as_deref(),
                    "session_start",
                    code,
                    &message,
                    Some(runtime.as_ref()),
                );
                if let Err(store_error) = self.finish_unstarted_route(
                    &claim.task.agent_id,
                    claim.owner_epoch,
                    &route,
                    task.as_ref(),
                    UnstartedTerminal {
                        outcome,
                        reason_code: code,
                        message: &message,
                    },
                    resources_reaped,
                ) {
                    self.record_failure(&claim.task.agent_id, store_error.to_string());
                }
                return Err(SchedulerError::RuntimeCommand {
                    agent_id: claim.task.agent_id,
                    message,
                });
            }
        };
        let requested_model =
            requested_model_from_prepared_launch(Some(claim.task.prepared_launch_json.as_str()));
        if let Err(code) = validate_requested_model(
            requested_model.as_deref(),
            session.configured_model.as_deref(),
        ) {
            let message = "runtime model did not match the prepared request";
            let terminal = runtime.stop(self.inner.config.stop_grace);
            self.record_runtime_failure(
                &claim.task.agent_id,
                Some(&session.session_id),
                "session_start",
                code,
                message,
                Some(runtime.as_ref()),
            );
            let resources_reaped = terminal_proves_process_group_reaped(&terminal);
            if let Err(error) = self.finish_unstarted_route(
                &claim.task.agent_id,
                claim.owner_epoch,
                &route,
                task.as_ref(),
                UnstartedTerminal {
                    outcome: CompletionOutcome::Failed,
                    reason_code: code,
                    message,
                },
                resources_reaped,
            ) {
                self.record_failure(&claim.task.agent_id, error.to_string());
            }
            return Err(SchedulerError::RuntimeCommand {
                agent_id: claim.task.agent_id,
                message: message.into(),
            });
        }
        let identity = runtime.identity().map(|identity| StoredProcessIdentity {
            pid: identity.pid,
            process_group_id: identity.pgid,
            uid: identity.uid,
            start_token: identity.start_token,
        });
        let operation = Arc::new(Mutex::new(()));
        let check = Arc::new(ActiveCheck::default());
        let ready_turn_state = match runtime.turn_snapshot() {
            TurnSnapshot { active: true, .. } => TurnState::Active,
            TurnSnapshot {
                boundary: Some(TurnBoundary::Failed),
                ..
            } => TurnState::Failed,
            _ => TurnState::Idle,
        };
        {
            let mut state = self.inner.state.lock().unwrap();
            state
                .activities
                .insert(claim.task.agent_id.clone(), Arc::clone(&activity));
            state.active.insert(
                claim.task.agent_id.clone(),
                ActiveRuntime {
                    owner_epoch: claim.owner_epoch,
                    runtime: Arc::clone(&runtime),
                    sink: Arc::clone(&sink),
                    session_id: session.session_id.clone(),
                    operation: Arc::clone(&operation),
                    runtime_lifecycle: Arc::clone(&runtime_lifecycle),
                    route: route.clone(),
                    task: task.clone(),
                    check: Arc::clone(&check),
                },
            );
        }
        let marked = match self.inner.store.mark_session_running(
            &claim.task.agent_id,
            claim.owner_epoch,
            &runtime_agent_id,
            identity.as_ref(),
            Some(&session.session_id),
            Some(ready_turn_state),
        ) {
            Ok(marked) => marked,
            Err(error) => {
                let _ = self.cleanup_registered_runtime(
                    &claim.task.agent_id,
                    claim.owner_epoch,
                    &runtime,
                    &sink,
                    Some(("STORE_START_FAILED", error.to_string())),
                );
                return Err(SchedulerError::Store(error));
            }
        };
        if !marked {
            let current = match self.inner.store.get_task(&claim.task.agent_id) {
                Ok(current) => current,
                Err(error) => {
                    let _ = self.cleanup_registered_runtime(
                        &claim.task.agent_id,
                        claim.owner_epoch,
                        &runtime,
                        &sink,
                        Some(("POST_REGISTRATION_READ_FAILED", error.to_string())),
                    );
                    return Err(SchedulerError::Store(error));
                }
            };
            if current.as_ref().is_some_and(|job| {
                job.stop_requested
                    || job.close_requested
                    || job.phase == TaskPhase::Cancelling
                    || job.phase.is_terminal()
            }) {
                self.cleanup_registered_runtime(
                    &claim.task.agent_id,
                    claim.owner_epoch,
                    &runtime,
                    &sink,
                    None,
                )?;
                return Ok(false);
            }
            let message = "running transition was not applied";
            self.cleanup_registered_runtime(
                &claim.task.agent_id,
                claim.owner_epoch,
                &runtime,
                &sink,
                Some(("RUNTIME_START_RACE", message.into())),
            )?;
            return Ok(false);
        }
        let current = match self.inner.store.get_task(&claim.task.agent_id) {
            Ok(current) => current,
            Err(error) => {
                let _ = self.cleanup_registered_runtime(
                    &claim.task.agent_id,
                    claim.owner_epoch,
                    &runtime,
                    &sink,
                    Some(("POST_REGISTRATION_READ_FAILED", error.to_string())),
                );
                return Err(SchedulerError::Store(error));
            }
        };
        if current.as_ref().is_some_and(|job| {
            job.stop_requested || job.close_requested || job.phase != TaskPhase::Running
        }) {
            let state = self.cleanup_registered_runtime(
                &claim.task.agent_id,
                claim.owner_epoch,
                &runtime,
                &sink,
                None,
            )?;
            debug_assert!(state.is_terminal());
            return Ok(false);
        }
        if resumed {
            let _guard = operation.lock().unwrap();
            if let Err(error) = self.deliver_next_message(
                &claim.task.agent_id,
                &session.session_id,
                &runtime,
                &runtime_lifecycle,
                self.control_deadline(),
            ) {
                let message = match &error {
                    SchedulerError::RuntimeCommand { message, .. } => message.clone(),
                    _ => error.to_string(),
                };
                self.cleanup_registered_runtime(
                    &claim.task.agent_id,
                    claim.owner_epoch,
                    &runtime,
                    &sink,
                    Some(("SESSION_SEND_FAILED", message)),
                )?;
                return Err(error);
            }
        }
        self.spawn_monitor(MonitorContext {
            agent_id: claim.task.agent_id,
            owner_epoch: claim.owner_epoch,
            runtime,
            sink,
            session_id: session.session_id,
            operation,
            runtime_lifecycle,
            route,
            task,
            check,
        });
        Ok(true)
    }

    fn finish_unstarted_route(
        &self,
        agent_id: &str,
        _owner_epoch: u64,
        route: &TaskRoute,
        _task: Option<&TaskRecord>,
        terminal: UnstartedTerminal<'_>,
        resources_reaped: bool,
    ) -> Result<TaskPhase, SchedulerError> {
        match route {
            TaskRoute::General(prepared) => {
                let completion = if resources_reaped {
                    finalized_general(
                        prepared,
                        terminal.outcome,
                        terminal.reason_code,
                        terminal.message,
                    )
                } else {
                    unreaped_general(terminal.outcome, terminal.reason_code, terminal.message)
                };
                self.persist_general_completion(
                    agent_id,
                    prepared,
                    &completion,
                    resources_reaped && completion.cleaned,
                )
            }
        }
    }

    fn persist_general_completion(
        &self,
        agent_id: &str,
        prepared: &PreparedGeneralTask,
        completion: &GeneralCompletion,
        reap_after_persist: bool,
    ) -> Result<TaskPhase, SchedulerError> {
        if let Err(error) =
            persist_general_result(&self.inner.store, agent_id, prepared, completion)
        {
            self.record_failure(agent_id, error.to_string());
            if self.inner.store.task_result(agent_id)?.is_none() {
                store_result_with_cancel_precedence(
                    &self.inner.store,
                    agent_id,
                    &bounded_result_invalid_task_result(),
                )?;
            }
        }
        if reap_after_persist {
            self.inner.store.reap_task(agent_id)?;
        }
        Ok(self
            .inner
            .store
            .get_task(agent_id)?
            .ok_or_else(|| {
                SchedulerError::Store(StoreError::InvalidState(
                    "terminal general task disappeared".into(),
                ))
            })?
            .phase)
    }

    fn cleanup_registered_runtime(
        &self,
        agent_id: &str,
        owner_epoch: u64,
        runtime: &Arc<dyn ManagedRuntime>,
        sink: &Arc<StoreLifecycleSink>,
        failure: Option<(&str, String)>,
    ) -> Result<TaskPhase, SchedulerError> {
        self.cleanup_registered_runtime_with_grace(
            agent_id,
            owner_epoch,
            runtime,
            sink,
            failure,
            self.inner.config.stop_grace,
        )
    }

    fn cleanup_registered_runtime_with_grace(
        &self,
        agent_id: &str,
        owner_epoch: u64,
        runtime: &Arc<dyn ManagedRuntime>,
        sink: &Arc<StoreLifecycleSink>,
        failure: Option<(&str, String)>,
        stop_grace: Duration,
    ) -> Result<TaskPhase, SchedulerError> {
        let stop_decision = self.inner.store.request_runtime_stop(agent_id)?;
        let cancellation_wins = stop_decision.prior_stop_or_close || failure.is_none();
        {
            let state = self.inner.state.lock().unwrap();
            if let Some(active) = state
                .active
                .get(agent_id)
                .filter(|active| active.owner_epoch == owner_epoch)
            {
                active.check.cancel();
            }
        }
        let active_route = {
            let state = self.inner.state.lock().unwrap();
            state.active.get(agent_id).and_then(|active| {
                (active.owner_epoch == owner_epoch)
                    .then(|| (active.route.clone(), active.task.clone()))
            })
        };
        if let Some((TaskRoute::General(prepared), task)) = active_route.clone() {
            sink.runtime_lifecycle
                .request_stop(&runtime.turn_snapshot());
            sink.runtime_lifecycle.force_terminating();
            let terminal = runtime.stop(stop_grace);
            let resources_reaped = terminal_proves_process_group_reaped(&terminal);
            let current = self.inner.store.get_task(agent_id)?;
            let result = if current.as_ref().is_some_and(|job| {
                matches!(
                    job.phase,
                    TaskPhase::Running | TaskPhase::Cancelling | TaskPhase::Terminal
                )
            }) {
                let forced = if cancellation_wins {
                    Some((CompletionOutcome::Cancelled, "CANCELLED".into()))
                } else {
                    failure
                        .as_ref()
                        .map(|(code, _)| (CompletionOutcome::Failed, (*code).to_owned()))
                        .or_else(|| Some((CompletionOutcome::Cancelled, "CANCELLED".into())))
                };
                self.finish_routed_terminal(
                    TerminalTarget {
                        agent_id,
                        sink,
                        route: &TaskRoute::General(prepared),
                        runtime,
                    },
                    TerminalDecision {
                        terminal,
                        natural_completion: false,
                        forced_outcome: forced,
                        failure_message: failure.as_ref().map(|(_, message)| message.clone()),
                    },
                )
            } else {
                let (code, message) = failure.unwrap_or((
                    "GENERAL_START_CANCELLED",
                    "general task stopped before entering its runtime phase".into(),
                ));
                let outcome = if cancellation_wins {
                    CompletionOutcome::Cancelled
                } else {
                    CompletionOutcome::Failed
                };
                self.finish_unstarted_route(
                    agent_id,
                    owner_epoch,
                    &TaskRoute::General(prepared),
                    task.as_ref(),
                    UnstartedTerminal {
                        outcome,
                        reason_code: if outcome == CompletionOutcome::Cancelled {
                            "CANCELLED"
                        } else {
                            code
                        },
                        message: &message,
                    },
                    resources_reaped,
                )
            };
            self.release_active(agent_id, owner_epoch);
            return result;
        }
        Err(SchedulerError::InvalidConfig(
            "active generic route disappeared during cleanup".into(),
        ))
    }

    fn fail_closed_control(
        &self,
        agent_id: &str,
        owner_epoch: u64,
        runtime: &Arc<dyn ManagedRuntime>,
        deadline: ControlDeadline,
        failure_code: &str,
        message: String,
    ) -> Result<(), SchedulerError> {
        let sink = {
            let state = self.inner.state.lock().unwrap();
            state.active.get(agent_id).and_then(|active| {
                (active.owner_epoch == owner_epoch).then(|| Arc::clone(&active.sink))
            })
        }
        .ok_or_else(|| SchedulerError::RuntimeCommand {
            agent_id: agent_id.into(),
            message: "active runtime disappeared during fail-closed control cleanup".into(),
        })?;
        self.cleanup_registered_runtime_with_grace(
            agent_id,
            owner_epoch,
            runtime,
            &sink,
            Some((failure_code, message)),
            deadline.cleanup_grace(self.inner.config.stop_grace),
        )?;
        if let Err(error) = self.start_ready() {
            self.record_failure(agent_id, error.to_string());
        }
        Ok(())
    }

    fn finish_routed_terminal(
        &self,
        target: TerminalTarget<'_>,
        decision: TerminalDecision,
    ) -> Result<TaskPhase, SchedulerError> {
        let TerminalTarget {
            agent_id,
            sink,
            route,
            runtime,
        } = target;
        let TerminalDecision {
            terminal,
            natural_completion,
            forced_outcome,
            failure_message,
        } = decision;
        sink.runtime_lifecycle.terminalize();
        match route {
            TaskRoute::General(prepared) => {
                let resumed = !prepared.prompt_path.is_file();
                let (outcome, reason) = forced_outcome.unwrap_or_else(|| {
                    let outcome = match &terminal {
                        RuntimeTerminal::Completed(_) if natural_completion => {
                            CompletionOutcome::Completed
                        }
                        RuntimeTerminal::Stopped(_) => CompletionOutcome::Cancelled,
                        RuntimeTerminal::FailedRuntimeLost(_) | RuntimeTerminal::Orphaned(_) => {
                            CompletionOutcome::RuntimeLost
                        }
                        RuntimeTerminal::Completed(_) | RuntimeTerminal::FailedTurn(_) => {
                            CompletionOutcome::Failed
                        }
                        // A child exit without an observed turn boundary is
                        // a runtime loss, not a model-reported task failure.
                        // This keeps COMPLETED reserved for a matching
                        // turn.completed plus successful daemon finalization.
                        RuntimeTerminal::Exited(_) => CompletionOutcome::RuntimeLost,
                    };
                    (outcome, "RUNTIME_TERMINAL".into())
                });
                if !matches!(
                    outcome,
                    CompletionOutcome::Completed | CompletionOutcome::Cancelled
                ) {
                    let session_id = self.active_session(agent_id).map(|active| active.2);
                    let message = if let Some(cause) = failure_message {
                        let mut detail = serde_json::from_str::<serde_json::Value>(&cause)
                            .ok()
                            .filter(serde_json::Value::is_object)
                            .unwrap_or_else(
                                || serde_json::json!({"message": bounded_error(&cause)}),
                            );
                        detail["cleanup_result"] = format!("{terminal:?}").into();
                        detail.to_string()
                    } else {
                        format!("{terminal:?}")
                    };
                    self.record_runtime_failure(
                        agent_id,
                        session_id.as_deref(),
                        "runtime_terminal",
                        &reason,
                        &message,
                        Some(runtime.as_ref()),
                    );
                }
                let natural_completed =
                    natural_completion && matches!(terminal, RuntimeTerminal::Completed(_));
                let process_group_reaped = terminal_proves_process_group_reaped(&terminal);
                let mut completion = if natural_completed {
                    let terminal_text = sink.activity.take_terminal_text();
                    let mut completion = match &terminal_text {
                        TerminalText::Visible(_) if resumed => GeneralFinalizer::finalize_resumed(
                            prepared,
                            CompletionOutcome::Completed,
                        ),
                        TerminalText::Visible(_) => {
                            GeneralFinalizer::finalize_completed_tree(prepared)
                        }
                        TerminalText::Missing => {
                            if resumed {
                                GeneralFinalizer::finalize_resumed(
                                    prepared,
                                    CompletionOutcome::ResultInvalid,
                                )
                            } else {
                                GeneralFinalizer::finalize(
                                    prepared,
                                    CompletionOutcome::ResultInvalid,
                                )
                            }
                        }
                    };
                    match terminal_text {
                        TerminalText::Visible(text) => completion.summary = text,
                        TerminalText::Missing => {
                            completion.summary =
                                "runtime completed without visible final text".into();
                            if completion.reason_code.is_none() {
                                completion.reason_code = Some("FINAL_TEXT_MISSING".into());
                            } else {
                                completion.residual_gaps.push("FINAL_TEXT_MISSING".into());
                            }
                        }
                    }
                    GeneralFinalizer::finish_cleanup(prepared, completion)
                } else if process_group_reaped {
                    if resumed {
                        GeneralFinalizer::finalize_resumed(prepared, outcome)
                    } else {
                        GeneralFinalizer::finalize(prepared, outcome)
                    }
                } else {
                    unreaped_general(outcome, &reason, &reason)
                };
                if completion.summary.trim().is_empty() {
                    completion.summary = reason.clone();
                }
                if completion.reason_code.is_none()
                    && completion.outcome != CompletionOutcome::Completed
                {
                    completion.reason_code = Some(reason);
                }
                let reap_after_persist = completion.cleaned && process_group_reaped;
                #[cfg(test)]
                self.run_result_persist_hook(agent_id);
                match sink.finish_general(&terminal, prepared, &completion) {
                    Ok(state) => Ok(state),
                    Err(error) => {
                        self.record_failure(agent_id, error.to_string());
                        self.persist_general_completion(
                            agent_id,
                            prepared,
                            &completion,
                            reap_after_persist,
                        )
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_locked_monitor_terminal(
        &self,
        agent_id: &str,
        owner_epoch: u64,
        runtime: &Arc<dyn ManagedRuntime>,
        sink: &StoreLifecycleSink,
        route: &TaskRoute,
        _task: Option<&TaskRecord>,
        terminal: RuntimeTerminal,
        natural_completion: bool,
        forced_outcome: Option<(CompletionOutcome, String)>,
    ) -> Result<TaskPhase, SchedulerError> {
        let current = self.inner.store.get_task(agent_id)?.ok_or_else(|| {
            SchedulerError::Store(StoreError::InvalidState(
                "active monitor task disappeared".into(),
            ))
        })?;
        if current.phase.is_terminal() || current.owner_epoch != owner_epoch {
            return Ok(current.phase);
        }
        let cancellation_wins = current.stop_requested || current.close_requested;
        let (terminal, natural_completion, forced_outcome) = if cancellation_wins {
            sink.runtime_lifecycle
                .request_stop(&runtime.turn_snapshot());
            sink.runtime_lifecycle.force_terminating();
            (
                runtime.stop(self.inner.config.stop_grace),
                false,
                Some((CompletionOutcome::Cancelled, "CANCELLED".into())),
            )
        } else if forced_outcome.is_none() && sink.error().is_some() {
            (
                terminal,
                false,
                Some((
                    CompletionOutcome::RuntimeLost,
                    "LIFECYCLE_SINK_FAILED".into(),
                )),
            )
        } else {
            (terminal, natural_completion, forced_outcome)
        };
        self.finish_routed_terminal(
            TerminalTarget {
                agent_id,
                sink,
                route,
                runtime: &runtime,
            },
            TerminalDecision {
                terminal,
                natural_completion,
                forced_outcome,
                failure_message: None,
            },
        )
    }

    fn spawn_monitor(&self, context: MonitorContext) {
        let MonitorContext {
            agent_id,
            owner_epoch,
            runtime,
            sink,
            session_id,
            operation,
            runtime_lifecycle,
            route,
            task,
            check,
        } = context;
        let scheduler = self.clone();
        thread::spawn(move || {
            let mut handled_generation = 0;
            loop {
                if let Some(terminal) = runtime.wait_terminal(Duration::from_millis(50)) {
                    let _guard = operation.lock().unwrap();
                    let natural = matches!(terminal, RuntimeTerminal::Completed(_));
                    if !natural && runtime_lifecycle.ingress_reason() == Some("LATE_AFTER_STOP") {
                        return;
                    }
                    if natural {
                        match sink.begin_natural_completion() {
                            Ok(NaturalCompletionAdmission::Deferred { pending: true, .. })
                            | Err(_) => {
                                drop(_guard);
                                thread::sleep(Duration::from_millis(10));
                                continue;
                            }
                            Ok(NaturalCompletionAdmission::Deferred {
                                pending: false,
                                queued: true,
                            }) => {
                                match scheduler.deliver_next_message(
                                    &agent_id,
                                    &session_id,
                                    &runtime,
                                    &runtime_lifecycle,
                                    scheduler.control_deadline(),
                                ) {
                                    Ok(Some(_)) => {
                                        drop(_guard);
                                        continue;
                                    }
                                    Ok(None) => {
                                        drop(_guard);
                                        continue;
                                    }
                                    Err(error) => {
                                        let detail = match &error {
                                            SchedulerError::RuntimeCommand { message, .. } => {
                                                message.clone()
                                            }
                                            _ => error.to_string(),
                                        };
                                        scheduler.record_runtime_failure(
                                            &agent_id,
                                            Some(&session_id),
                                            "message_delivery",
                                            "SESSION_SEND_FAILED",
                                            &detail,
                                            Some(runtime.as_ref()),
                                        );
                                        drop(_guard);
                                        continue;
                                    }
                                }
                            }
                            Ok(NaturalCompletionAdmission::Ready)
                            | Ok(NaturalCompletionAdmission::Bypass) => {}
                            Ok(NaturalCompletionAdmission::Deferred {
                                pending: false,
                                queued: false,
                            }) => unreachable!("blocked completion has a blocker"),
                        }
                    }
                    if !natural {
                        check.cancel();
                    }
                    if let Err(error) = scheduler.finish_locked_monitor_terminal(
                        &agent_id,
                        owner_epoch,
                        &runtime,
                        &sink,
                        &route,
                        task.as_ref(),
                        terminal,
                        natural,
                        None,
                    ) {
                        scheduler.record_failure(&agent_id, error.to_string());
                    }
                    check.cancel();
                    scheduler.release_active(&agent_id, owner_epoch);
                    if let Err(error) = scheduler.start_ready() {
                        scheduler.record_failure(&agent_id, error.to_string());
                    }
                    return;
                }
                if sink.error().is_some() {
                    check.cancel();
                    let _guard = operation.lock().unwrap();
                    let Some(error) = sink.error() else {
                        continue;
                    };
                    runtime_lifecycle.request_stop(&runtime.turn_snapshot());
                    runtime_lifecycle.force_terminating();
                    let terminal = runtime.stop(scheduler.inner.config.stop_grace);
                    if let Err(store_error) = scheduler.finish_locked_monitor_terminal(
                        &agent_id,
                        owner_epoch,
                        &runtime,
                        &sink,
                        &route,
                        task.as_ref(),
                        terminal,
                        false,
                        Some((
                            CompletionOutcome::RuntimeLost,
                            "LIFECYCLE_SINK_FAILED".into(),
                        )),
                    ) {
                        scheduler.record_failure(&agent_id, store_error.to_string());
                    }
                    scheduler.record_failure(&agent_id, error);
                    scheduler.release_active(&agent_id, owner_epoch);
                    return;
                }
                let turn = runtime.turn_snapshot();
                if !turn.active && turn.generation > handled_generation {
                    let Some(boundary) = turn.boundary else {
                        continue;
                    };
                    let _guard = operation.lock().unwrap();
                    if boundary != TurnBoundary::Completed
                        && runtime_lifecycle.ingress_reason().is_some()
                    {
                        return;
                    }
                    let current = runtime.turn_snapshot();
                    if current.active
                        || current.generation != turn.generation
                        || current.boundary != Some(boundary)
                    {
                        continue;
                    }
                    handled_generation = turn.generation;
                    let deadline = scheduler.control_deadline();
                    let delivery = if boundary == TurnBoundary::Completed {
                        match sink.begin_natural_completion() {
                            Ok(NaturalCompletionAdmission::Ready)
                            | Ok(NaturalCompletionAdmission::Bypass) => Ok(None),
                            Ok(NaturalCompletionAdmission::Deferred {
                                pending: false,
                                queued: true,
                            }) => scheduler.deliver_next_message(
                                &agent_id,
                                &session_id,
                                &runtime,
                                &runtime_lifecycle,
                                deadline,
                            ),
                            Ok(NaturalCompletionAdmission::Deferred { .. }) | Err(_) => {
                                handled_generation = handled_generation.saturating_sub(1);
                                continue;
                            }
                        }
                    } else {
                        scheduler.deliver_next_message(
                            &agent_id,
                            &session_id,
                            &runtime,
                            &runtime_lifecycle,
                            deadline,
                        )
                    };
                    match delivery {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            if boundary != TurnBoundary::Completed {
                                check.cancel();
                            }
                            let terminal = runtime.finish_turn(
                                boundary,
                                deadline.cleanup_grace(scheduler.inner.config.stop_grace),
                            );
                            if let Err(error) = scheduler.finish_locked_monitor_terminal(
                                &agent_id,
                                owner_epoch,
                                &runtime,
                                &sink,
                                &route,
                                task.as_ref(),
                                terminal,
                                boundary == TurnBoundary::Completed,
                                None,
                            ) {
                                scheduler.record_failure(&agent_id, error.to_string());
                            }
                            check.cancel();
                            scheduler.release_active(&agent_id, owner_epoch);
                            if let Err(error) = scheduler.start_ready() {
                                scheduler.record_failure(&agent_id, error.to_string());
                            }
                            return;
                        }
                        Err(error) => {
                            check.cancel();
                            let cause = match &error {
                                SchedulerError::RuntimeCommand { message, .. } => message.clone(),
                                _ => error.to_string(),
                            };
                            let terminal = runtime.finish_turn(
                                TurnBoundary::Failed,
                                deadline.cleanup_grace(scheduler.inner.config.stop_grace),
                            );
                            let mut detail = serde_json::from_str::<serde_json::Value>(&cause)
                                .ok()
                                .filter(serde_json::Value::is_object)
                                .unwrap_or_else(
                                    || serde_json::json!({"message": bounded_error(&cause)}),
                                );
                            detail["cleanup_result"] = format!("{terminal:?}").into();
                            if let Err(finish_error) = scheduler.finish_locked_monitor_terminal(
                                &agent_id,
                                owner_epoch,
                                &runtime,
                                &sink,
                                &route,
                                task.as_ref(),
                                terminal,
                                false,
                                Some((CompletionOutcome::Failed, "MESSAGE_DELIVERY_FAILED".into())),
                            ) {
                                scheduler.record_failure(&agent_id, finish_error.to_string());
                            }
                            scheduler.record_runtime_failure(
                                &agent_id,
                                Some(&session_id),
                                "message_delivery",
                                "SESSION_SEND_FAILED",
                                &detail.to_string(),
                                Some(runtime.as_ref()),
                            );
                            scheduler.release_active(&agent_id, owner_epoch);
                            if let Err(start_error) = scheduler.start_ready() {
                                scheduler.record_failure(&agent_id, start_error.to_string());
                            }
                            return;
                        }
                    }
                }
            }
        });
    }

    fn deliver_next_message(
        &self,
        agent_id: &str,
        session_id: &str,
        runtime: &Arc<dyn ManagedRuntime>,
        runtime_lifecycle: &RuntimeLifecycle,
        deadline: ControlDeadline,
    ) -> Result<Option<StoredMessage>, SchedulerError> {
        Self::require_runtime_ingress(agent_id, runtime_lifecycle)?;
        let Some(message) = self.inner.store.claim_next_message(agent_id)? else {
            return Ok(None);
        };
        if let Err(error) = Self::require_runtime_ingress(agent_id, runtime_lifecycle) {
            self.inner.store.fail_message(
                &message.message_id,
                "LATE_AFTER_STOP",
                "runtime_lifecycle stopped before message delivery",
            )?;
            return Err(error);
        }
        match runtime.send_turn(
            session_id,
            &message.content,
            self.runtime_phase_timeout(agent_id, deadline)?,
        ) {
            Ok(turn_id) => {
                if !self
                    .inner
                    .store
                    .complete_message(&message.message_id, turn_id.as_deref())?
                {
                    return Err(SchedulerError::Store(StoreError::Conflict(format!(
                        "message {} lost its delivery claim",
                        message.message_id
                    ))));
                }
                Ok(self.inner.store.message(&message.message_id)?)
            }
            Err(error) => {
                let detail = error.diagnostic("session/send");
                self.record_runtime_failure(
                    agent_id,
                    Some(session_id),
                    "message_delivery",
                    "SESSION_SEND_FAILED",
                    &detail,
                    Some(runtime.as_ref()),
                );
                self.inner.store.fail_message(
                    &message.message_id,
                    "SESSION_SEND_FAILED",
                    &detail,
                )?;
                Err(SchedulerError::RuntimeCommand {
                    agent_id: agent_id.into(),
                    message: detail,
                })
            }
        }
    }

    pub fn queue_message(
        &self,
        agent_id: &str,
        message_id: &str,
        mode: &str,
        content: &str,
    ) -> Result<MessageDisposition, SchedulerError> {
        if mode != "queue" {
            return Err(SchedulerError::InvalidConfig(
                "generic agent messages must use queue mode".into(),
            ));
        }
        let deadline = self.control_deadline();
        if let Some(existing) = self.inner.store.message(message_id)? {
            if existing.agent_id == agent_id && existing.mode == mode && existing.content == content
            {
                return Ok(match existing.state {
                    MessageState::Delivered => MessageDisposition::AlreadyDelivered,
                    MessageState::Failed => MessageDisposition::Failed,
                    MessageState::Queued | MessageState::Sending => MessageDisposition::Queued,
                });
            }
            return Err(SchedulerError::Store(StoreError::Conflict(
                "MESSAGE_ID_CONFLICT".into(),
            )));
        }
        if self
            .inner
            .store
            .get_task(agent_id)?
            .is_some_and(|task| task.phase == TaskPhase::Terminal)
        {
            let original = self
                .inner
                .store
                .get_task(agent_id)?
                .ok_or_else(|| StoreError::InvalidState("resume task disappeared".into()))?;
            let original_result = self.inner.store.task_result(agent_id)?;
            self.inner
                .store
                .requeue_task_for_resume_with_message(agent_id, message_id, content)?;
            let started = match self.start_ready() {
                Ok(started) => started,
                Err(error) => {
                    if self
                        .inner
                        .store
                        .get_task(agent_id)?
                        .is_some_and(|task| task.phase == TaskPhase::Terminal)
                    {
                        self.inner.store.restore_terminal_after_resume_failure(
                            &original,
                            original_result.as_ref().map(|stored| &stored.result),
                        )?;
                    }
                    return Err(error);
                }
            };
            if !started.iter().any(|started_id| started_id == agent_id) {
                self.inner.store.restore_terminal_after_resume_failure(
                    &original,
                    original_result.as_ref().map(|stored| &stored.result),
                )?;
                return Err(SchedulerError::RuntimeCommand {
                    agent_id: agent_id.into(),
                    message: "session resume was not started".into(),
                });
            }
            return Ok(MessageDisposition::Queued);
        }
        let active = self.active_session(agent_id);
        let operation = active
            .as_ref()
            .map(|(_, _, _, operation, _)| Arc::clone(operation));
        let _operation = operation
            .as_ref()
            .map(|operation| self.lock_operation(agent_id, operation, deadline))
            .transpose()?;
        if let Some((_, _, _, _, runtime_lifecycle)) = active.as_ref() {
            Self::require_runtime_ingress(agent_id, runtime_lifecycle)?;
        } else if self.inner.store.get_task(agent_id)?.is_some_and(|job| {
            job.phase != TaskPhase::Running || job.stop_requested || job.close_requested
        }) {
            return Err(Self::late_ingress_error(agent_id, "LATE_AFTER_STOP"));
        }
        deadline
            .remaining()
            .ok_or_else(|| Self::control_timeout_error(agent_id))?;
        let created = self
            .inner
            .store
            .insert_message(message_id, agent_id, mode, content)?;
        if !created {
            return Ok(
                match self
                    .inner
                    .store
                    .message(message_id)?
                    .map(|message| message.state)
                {
                    Some(MessageState::Delivered) => MessageDisposition::AlreadyDelivered,
                    Some(MessageState::Failed) => MessageDisposition::Failed,
                    _ => MessageDisposition::Queued,
                },
            );
        }
        Ok(MessageDisposition::Queued)
    }

    pub fn respond_request(
        &self,
        agent_id: &str,
        request_id: &str,
        decision: &str,
        content: Option<&str>,
    ) -> Result<ResponseOutcome, SchedulerError> {
        let deadline = self.control_deadline();
        let request = self
            .inner
            .store
            .pending_request(agent_id, request_id)?
            .ok_or_else(|| {
                SchedulerError::Store(StoreError::InvalidState(format!(
                    "unknown request {request_id}"
                )))
            })?;
        let valid = match request.request_type.as_str() {
            "permission" => matches!(decision, "allow" | "deny"),
            _ => false,
        };
        if !valid {
            return Err(SchedulerError::InvalidConfig(
                if request.request_type == "unsupported_input" {
                    "user-input response is unsupported by the pinned app-server seam".into()
                } else {
                    "response decision does not match the pending request type".into()
                },
            ));
        }
        if request.state != PendingRequestState::Pending {
            let effective_decision = request.response_decision.clone().ok_or_else(|| {
                SchedulerError::InvalidConfig("persisted response outcome is incomplete".into())
            })?;
            let policy_overrode = effective_decision != decision;
            return Ok(ResponseOutcome {
                disposition: if request.state == PendingRequestState::Responded {
                    ResponseDisposition::AlreadyResponded
                } else {
                    ResponseDisposition::InFlight
                },
                requested_decision: decision.to_owned(),
                effective_decision,
                policy_overrode,
                policy_reason_code: policy_overrode
                    .then_some(request.response_content)
                    .flatten(),
            });
        }
        let Some((owner_epoch, runtime, _session_id, operation, runtime_lifecycle)) =
            self.active_session(agent_id)
        else {
            let reason = if self.inner.store.get_task(agent_id)?.is_some_and(|job| {
                !matches!(job.phase, TaskPhase::Running | TaskPhase::WaitingInput)
                    || job.stop_requested
                    || job.close_requested
            }) {
                "LATE_AFTER_STOP"
            } else {
                "runtime is not active"
            };
            return Err(SchedulerError::RuntimeCommand {
                agent_id: agent_id.into(),
                message: reason.into(),
            });
        };
        let _guard = self.lock_operation(agent_id, &operation, deadline)?;
        Self::require_runtime_ingress(agent_id, &runtime_lifecycle)?;
        let current = self.inner.store.get_task(agent_id)?;
        if current.as_ref().is_none_or(|job| {
            job.owner_epoch != owner_epoch
                || !matches!(job.phase, TaskPhase::Running | TaskPhase::WaitingInput)
                || job.stop_requested
                || job.close_requested
        }) {
            return Err(Self::late_ingress_error(agent_id, "TASK_STOPPING"));
        }
        deadline
            .remaining()
            .ok_or_else(|| Self::control_timeout_error(agent_id))?;
        #[cfg(test)]
        self.run_response_claim_hook(ResponseClaimHookStage::BeforeClaim, agent_id);
        let existing_disposition = match self
            .inner
            .store
            .claim_pending_response_if_accepting(agent_id, request_id, decision, content)?
        {
            PendingResponseClaimDisposition::Claimed => None,
            PendingResponseClaimDisposition::TaskStopping => {
                return Err(Self::late_ingress_error(agent_id, "TASK_STOPPING"));
            }
            PendingResponseClaimDisposition::NotFound => {
                return Err(SchedulerError::Store(StoreError::InvalidState(format!(
                    "unknown request {request_id}"
                ))));
            }
            PendingResponseClaimDisposition::NotPending(PendingRequestState::Sending) => {
                Some(ResponseDisposition::InFlight)
            }
            PendingResponseClaimDisposition::NotPending(PendingRequestState::Responded) => {
                Some(ResponseDisposition::AlreadyResponded)
            }
            PendingResponseClaimDisposition::NotPending(PendingRequestState::Pending) => {
                return Err(SchedulerError::Store(StoreError::Conflict(format!(
                    "request {request_id} claim did not change pending state"
                ))));
            }
        };
        if let Some(disposition) = existing_disposition {
            return Ok(ResponseOutcome {
                disposition,
                requested_decision: decision.to_owned(),
                effective_decision: decision.to_owned(),
                policy_overrode: false,
                policy_reason_code: None,
            });
        }
        #[cfg(test)]
        self.run_response_claim_hook(ResponseClaimHookStage::AfterClaim, agent_id);
        if let Err(error) = Self::require_runtime_ingress(agent_id, &runtime_lifecycle) {
            self.inner
                .store
                .release_pending_response(agent_id, request_id)?;
            return Err(error);
        }
        let current = self.inner.store.get_task(agent_id)?;
        if current.as_ref().is_none_or(|job| {
            job.owner_epoch != owner_epoch
                || !matches!(job.phase, TaskPhase::Running | TaskPhase::WaitingInput)
                || job.stop_requested
                || job.close_requested
        }) {
            self.inner
                .store
                .release_pending_response(agent_id, request_id)?;
            return Err(Self::late_ingress_error(agent_id, "TASK_STOPPING"));
        }
        let response_deadline = match self.runtime_phase_deadline(agent_id, deadline) {
            Ok(deadline) => deadline,
            Err(error) => {
                self.inner
                    .store
                    .release_pending_response(agent_id, request_id)?;
                return Err(error);
            }
        };
        if let Err(error) = runtime.respond_request(
            &request.correlation_id,
            decision,
            content,
            None,
            response_deadline,
        ) {
            self.inner
                .store
                .release_pending_response(agent_id, request_id)?;
            let scheduler_error = SchedulerError::RuntimeCommand {
                agent_id: agent_id.into(),
                message: error.to_string(),
            };
            self.fail_closed_control(
                agent_id,
                owner_epoch,
                &runtime,
                deadline,
                control_failure_code(&error),
                error.to_string(),
            )?;
            return Err(scheduler_error);
        }
        if deadline.remaining().is_none() {
            self.inner
                .store
                .release_pending_response(agent_id, request_id)?;
            let error = Self::control_timeout_error(agent_id);
            self.fail_closed_control(
                agent_id,
                owner_epoch,
                &runtime,
                deadline,
                "CONTROL_DEADLINE_EXCEEDED",
                error.to_string(),
            )?;
            return Err(error);
        }
        if !self
            .inner
            .store
            .complete_pending_response(agent_id, request_id)?
        {
            return Err(SchedulerError::Store(StoreError::Conflict(format!(
                "request {request_id} lost its response claim"
            ))));
        }
        Ok(ResponseOutcome {
            disposition: ResponseDisposition::Responded,
            requested_decision: decision.to_owned(),
            effective_decision: decision.to_owned(),
            policy_overrode: false,
            policy_reason_code: None,
        })
    }

    pub fn cancel_task(&self, agent_id: &str) -> Result<TaskPhase, SchedulerError> {
        self.request_stop_or_close(agent_id, false, self.control_deadline())
    }

    pub fn close_task(&self, agent_id: &str) -> Result<TaskPhase, SchedulerError> {
        self.request_stop_or_close(agent_id, true, self.control_deadline())
    }

    fn request_stop_or_close(
        &self,
        agent_id: &str,
        close_session: bool,
        deadline: ControlDeadline,
    ) -> Result<TaskPhase, SchedulerError> {
        let active = self.active_session(agent_id);
        deadline
            .remaining()
            .ok_or_else(|| Self::control_timeout_error(agent_id))?;
        let decision = if close_session {
            self.inner.store.request_close(agent_id)?
        } else {
            self.inner.store.request_stop(agent_id)?
        };
        {
            let state = self.inner.state.lock().unwrap();
            if let Some(active) = state.active.get(agent_id) {
                active.check.cancel();
            }
        }
        if let Some((_, runtime, _, _, runtime_lifecycle)) = active.as_ref() {
            runtime_lifecycle.request_stop(&runtime.turn_snapshot());
        }
        if !decision.needs_runtime_stop {
            if decision.phase == TaskPhase::Cancelling && active.is_none() {
                let job = self.inner.store.get_task(agent_id)?.ok_or_else(|| {
                    SchedulerError::Store(StoreError::InvalidState(format!(
                        "unknown task {agent_id}"
                    )))
                })?;
                let task = self.inner.store.get_task(agent_id)?.ok_or_else(|| {
                    SchedulerError::Store(StoreError::InvalidState(
                        "converging V2 task metadata disappeared".into(),
                    ))
                })?;
                match task_route(&job) {
                    Ok(route) => {
                        validate_task_route(Some(&task), &route)
                            .map_err(SchedulerError::InvalidConfig)?;
                        return self.finish_unstarted_route(
                            agent_id,
                            decision.owner_epoch,
                            &route,
                            Some(&task),
                            UnstartedTerminal {
                                outcome: CompletionOutcome::Cancelled,
                                reason_code: "CANCELLED",
                                message: "task cancelled before runtime launch",
                            },
                            true,
                        );
                    }
                    Err(message) => {
                        self.inner.store.store_task_result(
                            agent_id,
                            &minimal_task_result(
                                CompletionOutcome::Cancelled,
                                "task cancelled with invalid prepared metadata",
                                "CANCELLED_PREPARED_INVALID",
                            ),
                        )?;
                        self.record_failure(agent_id, message);
                        return Ok(self
                            .inner
                            .store
                            .get_task(agent_id)?
                            .expect("cancelled task must remain durable")
                            .phase);
                    }
                }
            }
            return Ok(decision.phase);
        }
        let Some((owner_epoch, runtime, session_id, operation, runtime_lifecycle)) = active else {
            return Ok(decision.phase);
        };
        if owner_epoch != decision.owner_epoch {
            return Ok(decision.phase);
        }
        let _guard = match self.lock_operation(agent_id, &operation, deadline) {
            Ok(guard) => guard,
            Err(error) => {
                self.fail_closed_control(
                    agent_id,
                    owner_epoch,
                    &runtime,
                    deadline,
                    "CONTROL_DEADLINE_EXCEEDED",
                    error.to_string(),
                )?;
                return Err(error);
            }
        };
        let active_route = {
            let state = self.inner.state.lock().unwrap();
            state.active.get(agent_id).map(|active| {
                (
                    Arc::clone(&active.sink),
                    active.route.clone(),
                    active.task.clone(),
                )
            })
        };
        let Some((sink, route, _task)) = active_route else {
            return Ok(self
                .inner
                .store
                .get_task(agent_id)?
                .map(|job| job.phase)
                .unwrap_or(decision.phase));
        };
        let control_error = match self.runtime_phase_timeout(agent_id, deadline) {
            Ok(timeout) => {
                Self::request_cooperative_stop(&runtime, &session_id, &runtime_lifecycle, timeout)
            }
            Err(error) => {
                runtime_lifecycle.force_terminating();
                Some(error.to_string())
            }
        };
        let close_error = if close_session {
            match self.runtime_phase_timeout(agent_id, deadline) {
                Ok(timeout) => runtime
                    .close_session(&session_id, timeout)
                    .err()
                    .map(|error| error.to_string()),
                Err(error) => Some(error.to_string()),
            }
        } else {
            None
        };
        let terminal = runtime.stop(deadline.cleanup_grace(self.inner.config.stop_grace));
        let result = self.finish_routed_terminal(
            TerminalTarget {
                agent_id,
                sink: &sink,
                route: &route,
                runtime: &runtime,
            },
            TerminalDecision {
                terminal,
                natural_completion: false,
                forced_outcome: Some((CompletionOutcome::Cancelled, "CANCELLED".into())),
                failure_message: None,
            },
        );
        self.release_active(agent_id, decision.owner_epoch);
        if let Some(error) = close_error {
            self.record_failure(agent_id, error);
        }
        if let Some(error) = control_error {
            self.record_failure(agent_id, error);
        }
        result
    }

    fn active_session(&self, agent_id: &str) -> Option<ActiveSession> {
        let state = self.inner.state.lock().unwrap();
        state.active.get(agent_id).map(|active| {
            (
                active.owner_epoch,
                Arc::clone(&active.runtime),
                active.session_id.clone(),
                Arc::clone(&active.operation),
                Arc::clone(&active.runtime_lifecycle),
            )
        })
    }

    pub fn active_count(&self) -> usize {
        self.inner.state.lock().unwrap().active.len()
    }

    pub fn active_turn_observation(&self, agent_id: &str) -> Option<(TurnSnapshot, u64)> {
        self.active_session(agent_id)
            .map(|(_, runtime, _, _, _)| (runtime.turn_snapshot(), runtime.stop_boundary_count()))
    }

    pub(crate) fn passive_activity_snapshot(
        &self,
        agent_id: &str,
    ) -> Option<PassiveActivitySnapshot> {
        self.inner
            .state
            .lock()
            .unwrap()
            .activities
            .get(agent_id)
            .map(|activity| activity.snapshot())
    }

    pub(crate) fn observation_snapshot(
        &self,
        agent_id: &str,
    ) -> (observation::ObservationSnapshot, bool) {
        let activity = self
            .inner
            .state
            .lock()
            .unwrap()
            .activities
            .get(agent_id)
            .cloned();
        match activity {
            Some(activity) => (
                activity.observation_snapshot(),
                activity.runtime_source_verified.load(Ordering::Acquire),
            ),
            None => (
                observation::ObservationSnapshot::unavailable(),
                observation::runtime_source_verified(self.inner.config.runtime_source.as_deref()),
            ),
        }
    }

    pub(crate) fn runtime_source_verified(&self) -> bool {
        observation::runtime_source_verified(self.inner.config.runtime_source.as_deref())
    }

    pub fn last_error(&self, agent_id: &str) -> Option<String> {
        self.inner
            .state
            .lock()
            .unwrap()
            .failures
            .get(agent_id)
            .cloned()
    }

    pub fn shutdown_all(&self) {
        let agent_ids = self
            .inner
            .state
            .lock()
            .unwrap()
            .active
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for agent_id in agent_ids {
            if let Err(error) = self.close_task(&agent_id) {
                self.record_failure(&agent_id, error.to_string());
            }
        }
    }

    fn release_active(&self, agent_id: &str, owner_epoch: u64) {
        let mut state = self.inner.state.lock().unwrap();
        if state
            .active
            .get(agent_id)
            .is_some_and(|active| active.owner_epoch == owner_epoch)
        {
            if let Some(active) = state.active.get(agent_id) {
                active.runtime_lifecycle.terminalize();
            }
            state.active.remove(agent_id);
        }
    }

    fn record_runtime_failure(
        &self,
        agent_id: &str,
        session_id: Option<&str>,
        stage: &str,
        error_code: &str,
        message: &str,
        runtime: Option<&dyn ManagedRuntime>,
    ) {
        // Callers already stopped/reaped the runtime or observed its terminal
        // boundary. Do not introduce a diagnostic wait into scheduler control.
        let known_session = runtime.and_then(ManagedRuntime::diagnostic_session_id);
        let tail = runtime
            .map(ManagedRuntime::diagnostic_tail)
            .unwrap_or_default();
        let record = runtime_failure_record(
            agent_id,
            known_session.as_deref().or(session_id),
            stage,
            error_code,
            message,
            &tail,
        );
        self.record_failure_line(agent_id, record.clone(), &record);
    }

    fn record_failure_line(&self, agent_id: &str, message: String, record: &str) {
        update_latest_failure(
            &mut self.inner.state.lock().unwrap().failures,
            agent_id,
            message,
        );
        let line = format!(
            "[zcode-agentd] failure agent={}: {record}\n",
            bounded_error(agent_id)
        );
        if let Some(logger) =
            FAILURE_LOGGER.get_or_init(|| DiagnosticLogger::start(io::stderr()).ok())
        {
            logger.submit(line);
        }
    }

    pub(crate) fn record_failure(&self, agent_id: &str, message: String) {
        let bounded = bounded_error(&message);
        self.record_failure_line(agent_id, message, &bounded);
    }
}

// Every variable field is bounded before JSON escaping, whose worst-case
// expansion is six bytes per input byte. Including framing, records stay below
// 192 KiB; stderr keeps its latest 16 KiB even for invalid UTF-8 input.
pub(crate) fn runtime_failure_record(
    agent_id: &str,
    session_id: Option<&str>,
    stage: &str,
    error_code: &str,
    message: &str,
    stderr_tail: &str,
) -> String {
    let mut start = stderr_tail.len().saturating_sub(16 * 1024);
    while !stderr_tail.is_char_boundary(start) {
        start += 1;
    }
    let detail = serde_json::from_str::<serde_json::Value>(message).ok();
    let mut record = serde_json::json!({
        "agent_id": bounded_error(agent_id),
        "session_id": session_id.map(bounded_error),
        "stage": bounded_prefix(stage, 128),
        "error_code": bounded_prefix(error_code, 128),
        "message": bounded_error(detail.as_ref().and_then(|v| v.get("message"))
            .and_then(serde_json::Value::as_str).unwrap_or(message)),
        "stderr_tail": &stderr_tail[start..],
    });
    if let Some(detail) = detail {
        for field in ["operation", "remote_message", "cleanup_result"] {
            if let Some(value) = detail.get(field).and_then(serde_json::Value::as_str) {
                record[field] = bounded_prefix(value, 1024).into();
            }
        }
        if let Some(code) = detail
            .get("remote_code")
            .and_then(serde_json::Value::as_i64)
        {
            record["remote_code"] = code.into();
        }
    }
    record.to_string()
}

pub(crate) fn bounded_error(message: &str) -> String {
    bounded_prefix(message, 4096)
}

pub(crate) fn bounded_prefix(message: &str, max_bytes: usize) -> String {
    if message.len() <= max_bytes {
        return message.to_owned();
    }
    const MARKER: &str = "…";
    let limit = max_bytes - MARKER.len();
    let end = message
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= limit)
        .last()
        .unwrap_or(0);
    format!("{}{}", &message[..end], MARKER)
}

pub(crate) fn update_latest_failure(
    failures: &mut HashMap<String, String>,
    agent_id: &str,
    message: String,
) {
    failures.insert(agent_id.into(), message);
}

pub(crate) const DIAGNOSTIC_QUEUE_CAPACITY: usize = 32;
pub(crate) const DIAGNOSTIC_RECORD_BYTES: usize = 192 * 1024;
pub(crate) const DIAGNOSTIC_FILE_BYTES: u64 = 1024 * 1024;
static FAILURE_LOGGER: OnceLock<Option<DiagnosticLogger>> = OnceLock::new();

// Installed LaunchAgents pass their existing stderr path here. There is only
// one writer per process; a broken diagnostic sink never prevents startup.
pub fn configure_diagnostic_log(path: Option<PathBuf>) {
    FAILURE_LOGGER.get_or_init(|| match path {
        Some(path) => DiagnosticLogger::start(RotatingDiagnosticWriter { path }).ok(),
        None => DiagnosticLogger::start(io::stderr()).ok(),
    });
}
