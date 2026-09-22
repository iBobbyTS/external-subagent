use super::*;

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
