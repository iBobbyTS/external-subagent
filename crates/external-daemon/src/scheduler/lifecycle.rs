use super::*;
use external_store::StoreError;

impl Scheduler {
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
        // Observation trust is launch-scoped: the pinned ZCode runtime proof
        // only ever applies to the adapter the routing factory will actually
        // launch (the same `task_agent` identity it dispatches on). DSH,
        // Codex and unknown adapters start explicitly unverified instead of
        // borrowing the scheduler-global ZCode proof.
        let adapter = task_agent(&claim.task);
        let activity = Arc::new(PassiveActivityTracker::new(
            observation::adapter_runtime_source_verified(
                &adapter,
                self.inner.config.runtime_source.as_deref(),
            ),
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
        activity.confirm_runtime_source(observation::adapter_runtime_source_verified(
            &adapter,
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

    pub(super) fn finish_unstarted_route(
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

    pub(super) fn fail_closed_control(
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

    pub(super) fn finish_routed_terminal(
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
}
