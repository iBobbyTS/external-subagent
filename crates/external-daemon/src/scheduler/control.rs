use super::*;
use external_store::StoreError;

impl Scheduler {
    pub(super) fn late_ingress_error(agent_id: &str, reason: &'static str) -> SchedulerError {
        SchedulerError::RuntimeCommand {
            agent_id: agent_id.into(),
            message: reason.into(),
        }
    }

    pub(super) fn require_runtime_ingress(
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

    pub(super) fn control_deadline(&self) -> ControlDeadline {
        ControlDeadline::new(self.inner.config.control_timeout)
    }

    pub(super) fn control_timeout_error(agent_id: &str) -> SchedulerError {
        SchedulerError::RuntimeCommand {
            agent_id: agent_id.into(),
            message: "control operation deadline elapsed".into(),
        }
    }

    pub(super) fn lock_operation<'a>(
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

    pub(super) fn runtime_phase_timeout(
        &self,
        agent_id: &str,
        deadline: ControlDeadline,
    ) -> Result<Duration, SchedulerError> {
        deadline
            .runtime_phase(self.inner.config.stop_grace)
            .ok_or_else(|| Self::control_timeout_error(agent_id))
    }

    pub(super) fn runtime_phase_deadline(
        &self,
        agent_id: &str,
        deadline: ControlDeadline,
    ) -> Result<Instant, SchedulerError> {
        deadline
            .runtime_phase_deadline(self.inner.config.stop_grace)
            .ok_or_else(|| Self::control_timeout_error(agent_id))
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
}
