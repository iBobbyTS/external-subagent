use super::*;
use external_store::StoreError;

impl Scheduler {
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
        // Drain fences survive process exit. They never owned a runtime, so
        // settle them through the same unstarted cancellation owner as RPC.
        let never_claimed_cancellation = task.phase == TaskPhase::Cancelling
            && task.owner_epoch == 0
            && task.session_id.is_none();
        if (task.phase == TaskPhase::Queued || never_claimed_cancellation)
            && task.stop_requested
            && task.runtime_agent_id.is_none()
            && task.process_identity.is_none()
        {
            if task.phase == TaskPhase::Queued {
                self.cancel_task(&task.agent_id)?;
            } else {
                // A prior recovery committed stop intent but crashed before
                // result persistence. Epoch zero proves no claim ever ran.
                let route = task_route(task).map_err(SchedulerError::InvalidConfig)?;
                validate_task_route(Some(task), &route).map_err(SchedulerError::InvalidConfig)?;
                self.finish_unstarted_route(
                    &task.agent_id,
                    task.owner_epoch,
                    &route,
                    Some(task),
                    UnstartedTerminal {
                        outcome: CompletionOutcome::Cancelled,
                        reason_code: "CANCELLED",
                        message: "task cancelled before runtime launch",
                    },
                    true,
                )?;
            }
            return Ok(());
        }
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
}
