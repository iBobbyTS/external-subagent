use super::*;
use external_store::StoreError;

impl Scheduler {
    pub(super) fn deliver_next_message(
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
        content: &str,
    ) -> Result<MessageDisposition, SchedulerError> {
        // Queue is the only generic message behavior; the fixed mode lives
        // here instead of traveling on the wire.
        let mode = "queue";
        let deadline = self.control_deadline();
        let _admission = self.inner.admission.lock().unwrap();
        #[cfg(test)]
        if let Some(hook) = self.inner.admission_hook.lock().unwrap().clone() {
            hook();
        }
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
        if self.inner.draining.load(Ordering::Acquire) {
            return Err(SchedulerError::RuntimeCommand {
                agent_id: agent_id.into(),
                message: "daemon_draining".into(),
            });
        }
        if self
            .inner
            .store
            .get_task(agent_id)?
            .is_some_and(|task| task.phase == TaskPhase::Terminal)
        {
            return self.resume_terminal_with_message(agent_id, message_id, content);
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

    /// The explicit recovery trigger for a terminal Codex task. Only an
    /// eligible terminal Codex task — a persisted thread id and no
    /// cancellation or close — requeues through the existing store path; a
    /// later claim spawns a fresh app-server, resumes the same thread, and
    /// starts exactly one new turn with the queued message. The interrupted
    /// pre-crash turn is never replayed. The durable eligibility check and
    /// the requeue state update share one store transaction, so a close or
    /// cancel that committed first is never overwritten, and a task whose
    /// old process group was not proven reaped keeps its persisted process
    /// identity instead of being resumed.
    fn resume_terminal_with_message(
        &self,
        agent_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<MessageDisposition, SchedulerError> {
        let task = self.inner.store.get_task(agent_id)?.ok_or_else(|| {
            SchedulerError::Store(StoreError::InvalidState(format!("unknown task {agent_id}")))
        })?;
        // The prepared route is immutable, so this precheck only refuses
        // early; every durable field is re-checked inside the requeue
        // transaction before any state changes.
        let eligible = task_agent(&task) == "codex"
            && task
                .zcode_session_id
                .as_deref()
                .is_some_and(|id| !id.is_empty() && id.len() <= 512)
            && task.outcome != Some(TaskOutcome::Cancelled)
            && !task.stop_requested
            && !task.close_requested
            && task.closed_at.is_none();
        if !eligible {
            return Err(SchedulerError::RuntimeCommand {
                agent_id: agent_id.into(),
                message: "TERMINAL_SEND_UNSUPPORTED".into(),
            });
        }
        if !self
            .inner
            .store
            .requeue_task_for_resume_with_message(agent_id, message_id, content)?
        {
            return Err(SchedulerError::RuntimeCommand {
                agent_id: agent_id.into(),
                message: "TERMINAL_SEND_UNSUPPORTED".into(),
            });
        }
        // The daemon claim loop performs the spawn: requeueing inside this
        // control operation must never block on bootstrap deadlines.
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
            // Answerable user-input requests execute only as answer plus
            // non-empty content; the wait capability is never persisted here.
            "user_input" => {
                decision == "answer" && content.is_some_and(|value| !value.trim().is_empty())
            }
            _ => false,
        };
        if !valid {
            return Err(SchedulerError::InvalidConfig(
                "response decision does not match the pending request type".into(),
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
}

fn control_failure_code(error: &RuntimeCommandError) -> &'static str {
    if matches!(error, RuntimeCommandError::Timeout) {
        "CONTROL_DEADLINE_EXCEEDED"
    } else {
        "CONTROL_RUNTIME_FAILED"
    }
}
