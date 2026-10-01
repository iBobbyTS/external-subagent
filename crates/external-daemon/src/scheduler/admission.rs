use super::*;
use external_store::StoreError;

impl Scheduler {
    pub fn enqueue_general(
        &self,
        manifest: &GeneralTaskManifest,
    ) -> Result<TaskRecord, SchedulerError> {
        self.enqueue_general_with_admission(manifest, None)
    }

    pub fn enqueue_general_with_admission(
        &self,
        manifest: &GeneralTaskManifest,
        admission: Option<external_core::AdmissionIdentity>,
    ) -> Result<TaskRecord, SchedulerError> {
        // Serialize admission with begin_drain so the draining check and the
        // authoritative enqueue form one linearizable operation.
        let _admission = self.inner.admission.lock().unwrap();
        #[cfg(test)]
        if let Some(hook) = self.inner.admission_hook.lock().unwrap().clone() {
            hook();
        }
        if self.inner.draining.load(Ordering::Acquire) {
            return Err(SchedulerError::InvalidConfig("daemon_draining".into()));
        }
        let mut manifest = manifest.clone();
        manifest.agent_id = self.inner.store.reserve_task_id()?;
        let prepared = GeneralTaskPreparer::new(Vec::new())
            .and_then(|preparer| preparer.prepare_direct_submission(&manifest))
            .map_err(|error| SchedulerError::InvalidConfig(error.to_string()))?;
        let prepared = match admission {
            Some(identity) => prepared
                .with_admission(identity)
                .map_err(|error| SchedulerError::InvalidConfig(error.to_string()))?,
            None => prepared,
        };
        let prepared_json = serde_json::to_string(&prepared)
            .map_err(|error| SchedulerError::InvalidConfig(error.to_string()))?;
        // The caller prompt is the first turn verbatim: no daemon-authored
        // control block, separator, or digest is prepended.
        let initial_prompt = manifest.prompt.clone();
        let task = NewTask {
            agent_id: prepared.agent_id.clone(),
            repository: prepared.repository.to_string_lossy().into_owned(),
            workspace_path: prepared.workspace.path.to_string_lossy().into_owned(),
            runtime_hash: None,
            prepared_launch_json: prepared_json,
            initial_prompt,
        };
        let enqueued = self.inner.store.enqueue_task_authoritative(&task)?;
        Ok(enqueued)
    }

    pub fn submit_and_start_general(
        &self,
        manifest: &GeneralTaskManifest,
        admission: Option<external_core::AdmissionIdentity>,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<TaskRecord, SchedulerError> {
        let enqueued = self.enqueue_general_with_admission(manifest, admission)?;
        let agent_id = enqueued.agent_id.clone();
        let starter_scheduler = self.clone();
        let starter_agent_id = agent_id.clone();
        let per_workspace_limit = self.inner.config.per_workspace_max_agents;

        thread::Builder::new()
            .name(format!("starter-{}", starter_agent_id))
            .spawn(move || {
                #[cfg(test)]
                if let Some(hook) = starter_scheduler.inner.before_claim_hook.lock().unwrap().clone() {
                    hook();
                }
                match starter_scheduler.inner.store.claim_specific(
                    &starter_agent_id,
                    &starter_scheduler.inner.owner_id,
                    per_workspace_limit,
                ) {
                    Ok(Some(claim)) => {
                        let _ = starter_scheduler.start_claim(claim);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        starter_scheduler.record_failure(&starter_agent_id, e.to_string());
                    }
                }
            })
            .map_err(|e| SchedulerError::RuntimeSpawn {
                agent_id: agent_id.clone(),
                message: format!("failed to spawn starter thread: {e}"),
            })?;

        let wait_budget = self.spawn_wait_budget();
        let deadline = Instant::now() + wait_budget;

        loop {
            if interrupted() {
                return Err(SchedulerError::Interrupted {
                    agent_id: agent_id.clone(),
                });
            }

            let task_opt = self.inner.store.get_task(&agent_id)?;
            let Some(task) = task_opt else {
                return Err(SchedulerError::Store(StoreError::InvalidState(format!(
                    "task {agent_id} disappeared from store"
                ))));
            };

            #[cfg(test)]
            if let Some(hook) = self.inner.spawn_poll_hook.lock().unwrap().clone() {
                hook(&task);
            }

            if task.session_id.is_some() {
                if !matches!(task.phase, TaskPhase::Queued | TaskPhase::Preparing) {
                    return Ok(task);
                }
                if Instant::now() >= deadline {
                    return Err(SchedulerError::StartTimeout {
                        agent_id: agent_id.clone(),
                        message: format!(
                            "session established for {agent_id}, being driven by resume, uncancelled"
                        ),
                    });
                }
            } else if task.phase.is_terminal() {
                let reason = task.failure_code.unwrap_or_else(|| "START_FAILED".into());
                let message = task
                    .failure_message
                    .unwrap_or_else(|| "task terminated before establishment".into());
                return Err(SchedulerError::StartFailed {
                    agent_id,
                    reason,
                    message,
                });
            } else if Instant::now() >= deadline {
                break;
            }

            thread::sleep(Duration::from_millis(20));
        }

        match self.inner.store.cancel_unstarted_if_still_fresh(&agent_id)? {
            Ok((prior_phase, epoch)) => {
                if prior_phase == TaskPhase::Queued {
                    let route = task_route(&enqueued).map_err(SchedulerError::InvalidConfig)?;
                    let _ = self.finish_unstarted_route(
                        &agent_id,
                        epoch,
                        &route,
                        Some(&enqueued),
                        UnstartedTerminal {
                            outcome: CompletionOutcome::Cancelled,
                            reason_code: "CANCELLED",
                            message: "spawn timed out before claim",
                            failure_message: None,
                        },
                        true,
                    );
                    return Err(SchedulerError::StartTimeout {
                        agent_id,
                        message: "spawn timed out waiting for session establishment (cancelled)".into(),
                    });
                } else {
                    let handle = self.starting_handle(&agent_id);
                    if let Some(handle) = handle {
                        if handle.owner_epoch == epoch {
                            if let Some(identity) = &handle.identity {
                                let pgid = identity.process_group_id;
                                if pgid > 0 {
                                    unsafe {
                                        libc::kill(-pgid, libc::SIGKILL);
                                        libc::kill(pgid, libc::SIGKILL);
                                    }
                                }
                                let pid = identity.pid;
                                if pid > 0 {
                                    unsafe {
                                        libc::kill(pid as i32, libc::SIGKILL);
                                    }
                                }
                            }
                        }
                    }

                    let conv_deadline = Instant::now() + self.convergence_budget();
                    let mut reached_terminal = false;
                    while Instant::now() < conv_deadline {
                        if let Ok(Some(task)) = self.inner.store.get_task(&agent_id) {
                            if task.phase.is_terminal() {
                                reached_terminal = true;
                                break;
                            }
                        }
                        thread::sleep(Duration::from_millis(20));
                    }

                    if reached_terminal {
                        return Err(SchedulerError::StartTimeout {
                            agent_id,
                            message: "spawn timed out waiting for session establishment (cancelled)".into(),
                        });
                    } else {
                        return Err(SchedulerError::StartTimeout {
                            agent_id,
                            message: "spawn timed out waiting for session establishment (cancelled, process termination pending)".into(),
                        });
                    }
                }
            }
            Err(current_task) => {
                if current_task.session_id.is_some() {
                    if !matches!(current_task.phase, TaskPhase::Queued | TaskPhase::Preparing) {
                        return Ok(current_task);
                    }
                    while Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(20));
                        if let Ok(Some(task)) = self.inner.store.get_task(&agent_id) {
                            if !matches!(task.phase, TaskPhase::Queued | TaskPhase::Preparing) {
                                return Ok(task);
                            }
                        }
                    }
                    return Err(SchedulerError::StartTimeout {
                        agent_id: agent_id.clone(),
                        message: format!(
                            "session established for {agent_id}, being driven by resume, uncancelled"
                        ),
                    });
                }
                if current_task.phase.is_terminal() {
                    let reason = current_task.failure_code.unwrap_or_else(|| "START_FAILED".into());
                    let message = current_task
                        .failure_message
                        .unwrap_or_else(|| "task terminated before establishment".into());
                    return Err(SchedulerError::StartFailed {
                        agent_id,
                        reason,
                        message,
                    });
                }
                let conv_deadline = Instant::now() + self.convergence_budget();
                while Instant::now() < conv_deadline {
                    if let Ok(Some(task)) = self.inner.store.get_task(&agent_id) {
                        if task.phase.is_terminal() {
                            break;
                        }
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                return Err(SchedulerError::StartTimeout {
                    agent_id,
                    message: "spawn timed out waiting for session establishment (cancelled)".into(),
                });
            }
        }
    }

    pub fn begin_drain(&self) {
        let _admission = self.inner.admission.lock().unwrap();
        self.inner.draining.store(true, Ordering::Release);
    }
    /// Bounded recovery for an update that drained this daemon but never
    /// activated: reopen admission so the still-running daemon keeps serving
    /// spawns. The flag flip is the entire state change — completed tasks,
    /// reap facts, and an already-issued activation claim stay exactly as
    /// they were, so a retry sees the same evidence. Refused while an
    /// explicit `--cancel-active` worker is still in flight, so cancellation
    /// semantics never change, and refused when no drain is active.
    pub fn abort_drain(&self) -> Result<(), SchedulerError> {
        let _admission = self.inner.admission.lock().unwrap();
        if !self.inner.draining.load(Ordering::Acquire) {
            return Err(SchedulerError::InvalidConfig("drain_not_active".into()));
        }
        if self.inner.drain_cancel_running.load(Ordering::Acquire) {
            return Err(SchedulerError::InvalidConfig(
                "drain_cancel_in_progress".into(),
            ));
        }
        self.inner.draining.store(false, Ordering::Release);
        Ok(())
    }
    /// Admission is already closed. A single worker uses the ordinary cancel
    /// owner so uncooperative providers cannot hold the management RPC open.
    pub(crate) fn cancel_draining_tasks(&self) -> Result<(), SchedulerError> {
        self.inner.store.fence_queued_cancellation()?;
        if self.inner.drain_cancel_running.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let ids = match self.inner.store.nonterminal_task_ids() {
            Ok(ids) => ids,
            Err(error) => {
                self.inner
                    .drain_cancel_running
                    .store(false, Ordering::Release);
                return Err(error.into());
            }
        };
        let scheduler = self.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("drain-cancel".into())
            .spawn(move || {
                for agent_id in ids {
                    if let Err(error) = scheduler.cancel_task(&agent_id) {
                        scheduler.record_failure(&agent_id, error.to_string());
                    }
                }
                scheduler
                    .inner
                    .drain_cancel_running
                    .store(false, Ordering::Release);
            })
        {
            self.inner
                .drain_cancel_running
                .store(false, Ordering::Release);
            return Err(SchedulerError::InvalidConfig(format!(
                "cannot start drain cancellation: {error}"
            )));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn set_admission_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.inner.admission_hook.lock().unwrap() = Some(hook);
    }
    pub fn is_draining(&self) -> bool {
        self.inner.draining.load(Ordering::Acquire)
    }
    pub fn ready_for_activation(&self) -> bool {
        self.is_draining()
            && !self.inner.drain_cancel_running.load(Ordering::Acquire)
            && self.active_count() == 0
            && self.inner.store.all_tasks_reaped().unwrap_or(false)
    }
    pub fn resources_reaped(&self) -> bool {
        self.active_count() == 0 && self.inner.store.all_tasks_reaped().unwrap_or(false)
    }
    pub fn updater_fired(&self) -> bool {
        self.inner.updater_fired.load(Ordering::Acquire)
    }
    pub fn fire_updater_once(&self) -> bool {
        self.claim_activation().is_some()
    }

    /// Atomically claims the single activation handoff. The daemon only issues
    /// an opaque receipt; the installer owns the update state machine.
    pub fn claim_activation(&self) -> Option<String> {
        if !self.ready_for_activation() {
            return None;
        }
        let mut claim = self.inner.activation_claim.lock().unwrap();
        if claim.is_none() {
            let token = format!("{}-activation", self.inner.owner_id);
            *claim = Some(token);
            self.inner.updater_fired.store(true, Ordering::Release);
        }
        claim.clone()
    }
}
