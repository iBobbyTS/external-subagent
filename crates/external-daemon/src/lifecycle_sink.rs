use crate::{
    activity_tracker::PassiveActivityTracker, projection, terminal_proves_process_group_reaped,
    LifecycleRecord, LifecycleSink, RuntimeEvent, RuntimeLifecycle, RuntimeLifecyclePhase,
    RuntimeTerminal,
};
use external_contract::{
    WireMessage, INTERACTION_REQUEST_PERMISSION, INTERACTION_REQUEST_UNSUPPORTED_INPUT,
    INTERACTION_REQUEST_USER_INPUT,
};
use external_core::{CompletionOutcome, GeneralCompletion, GeneralFinalizer, PreparedGeneralTask};
use external_runtime::Inbound;
use external_store::{
    LifecycleWrite, Store, StoreError, TaskOutcome, TaskPhase, TaskResult, TurnState,
};
use std::sync::{Arc, Mutex};

pub(crate) struct StoreLifecycleSink {
    store: Arc<Store>,
    agent_id: String,
    runtime_agent_id: String,
    owner_epoch: u64,
    pub(crate) runtime_lifecycle: Arc<RuntimeLifecycle>,
    pub(crate) activity: Arc<PassiveActivityTracker>,
    write_state: Mutex<SinkWriteState>,
    #[cfg(test)]
    after_admission_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    before_natural_completion_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    after_result_persist_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NaturalCompletionAdmission {
    Ready,
    Deferred { pending: bool, queued: bool },
    Bypass,
}

#[derive(Default)]
struct SinkWriteState {
    first_error: Option<String>,
    last_source_sequence: u64,
    pending_terminal_sequence: Option<u64>,
    terminal_written: bool,
}

impl StoreLifecycleSink {
    pub(crate) fn new(
        store: Arc<Store>,
        agent_id: String,
        runtime_agent_id: String,
        owner_epoch: u64,
        runtime_lifecycle: Arc<RuntimeLifecycle>,
        activity: Arc<PassiveActivityTracker>,
    ) -> Self {
        Self {
            store,
            agent_id,
            runtime_agent_id,
            owner_epoch,
            runtime_lifecycle,
            activity,
            write_state: Mutex::new(SinkWriteState::default()),
            #[cfg(test)]
            after_admission_hook: Mutex::new(None),
            #[cfg(test)]
            before_natural_completion_hook: Mutex::new(None),
            #[cfg(test)]
            after_result_persist_hook: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn set_after_admission_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.after_admission_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    fn set_before_natural_completion_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.before_natural_completion_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(test)]
    fn set_after_result_persist_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.after_result_persist_hook.lock().unwrap() = Some(hook);
    }

    pub(crate) fn begin_natural_completion(
        &self,
    ) -> Result<NaturalCompletionAdmission, StoreError> {
        #[cfg(test)]
        if let Some(hook) = self.before_natural_completion_hook.lock().unwrap().clone() {
            hook();
        }
        let mut runtime_lifecycle = self.runtime_lifecycle.state.lock().unwrap();
        if runtime_lifecycle.phase != RuntimeLifecyclePhase::Running {
            return Ok(NaturalCompletionAdmission::Bypass);
        }
        let (pending, queued) = self.store.completion_blockers(&self.agent_id)?;
        if pending || queued {
            return Ok(NaturalCompletionAdmission::Deferred { pending, queued });
        }
        runtime_lifecycle.phase = RuntimeLifecyclePhase::Terminal;
        Ok(NaturalCompletionAdmission::Ready)
    }

    pub(crate) fn finish_general(
        &self,
        terminal: &RuntimeTerminal,
        prepared: &PreparedGeneralTask,
        completion: &GeneralCompletion,
    ) -> Result<TaskPhase, StoreError> {
        let mut state = self.write_state.lock().unwrap();
        if let Some(error) = &state.first_error {
            return Err(StoreError::InvalidState(error.clone()));
        }
        if state.terminal_written {
            return self
                .store
                .get_task(&self.agent_id)?
                .map(|job| job.phase)
                .ok_or_else(|| StoreError::InvalidState("terminal task disappeared".into()));
        }
        let source_sequence = state
            .pending_terminal_sequence
            .unwrap_or_else(|| state.last_source_sequence.saturating_add(1));
        let projection =
            projection::lifecycle_projection(&RuntimeEvent::Terminal(terminal.clone()), None);
        self.store.append_lifecycle(&LifecycleWrite {
            agent_id: self.agent_id.clone(),
            runtime_agent_id: self.runtime_agent_id.clone(),
            owner_epoch: self.owner_epoch,
            source_sequence,
            event_type: projection.event_type.into(),
            turn_id: None,
            payload_json: projection.payload_json,
            redaction_level: projection.redaction_level.into(),
            terminal: None,
            turn_state: None,
        })?;
        let reap_after_persist =
            completion.cleaned && terminal_proves_process_group_reaped(terminal);
        persist_general_result(&self.store, &self.agent_id, prepared, completion)?;
        #[cfg(test)]
        if let Some(hook) = self.after_result_persist_hook.lock().unwrap().clone() {
            hook();
        }
        if reap_after_persist {
            self.store.reap_task(&self.agent_id)?;
        }
        state.terminal_written = true;
        self.store
            .get_task(&self.agent_id)?
            .map(|job| job.phase)
            .ok_or_else(|| StoreError::InvalidState("terminal task disappeared".into()))
    }

    pub(crate) fn error(&self) -> Option<String> {
        self.write_state.lock().unwrap().first_error.clone()
    }
}

pub(crate) fn persist_general_result(
    store: &Store,
    agent_id: &str,
    prepared: &PreparedGeneralTask,
    completion: &GeneralCompletion,
) -> Result<(), StoreError> {
    let result = task_result(completion);
    let _ = prepared;
    store_result_with_cancel_precedence(store, agent_id, &result)
}

pub(crate) fn store_result_with_cancel_precedence(
    store: &Store,
    agent_id: &str,
    result: &TaskResult,
) -> Result<(), StoreError> {
    match store.store_task_result(agent_id, result) {
        Ok(()) => Ok(()),
        Err(error @ StoreError::Conflict(_)) => {
            let task = store.get_task(agent_id)?.ok_or_else(|| {
                StoreError::InvalidState("terminal result task disappeared".into())
            })?;
            if (task.stop_requested || task.close_requested)
                && result.outcome != TaskOutcome::Cancelled
                && store.task_result(agent_id)?.is_none()
            {
                store.store_task_result(agent_id, &bounded_cancelled_task_result())
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

fn task_result(completion: &GeneralCompletion) -> TaskResult {
    let summary = if completion.summary.trim().is_empty() {
        completion
            .reason_code
            .clone()
            .unwrap_or_else(|| format!("general task ended with {:?}", completion.outcome))
    } else {
        completion.summary.clone()
    };
    TaskResult {
        outcome: task_outcome(completion.outcome),
        final_text: summary,
        partial: completion.outcome != CompletionOutcome::Completed,
    }
}

fn task_outcome(outcome: CompletionOutcome) -> TaskOutcome {
    match outcome {
        CompletionOutcome::Completed => TaskOutcome::Completed,
        CompletionOutcome::Failed => TaskOutcome::Failed,
        CompletionOutcome::Cancelled => TaskOutcome::Cancelled,
        CompletionOutcome::TimedOut => TaskOutcome::TimedOut,
        CompletionOutcome::RuntimeLost => TaskOutcome::RuntimeLost,
        CompletionOutcome::ResultInvalid => TaskOutcome::ResultInvalid,
    }
}

pub(crate) fn minimal_task_result(
    outcome: CompletionOutcome,
    summary: &str,
    reason_code: &str,
) -> TaskResult {
    TaskResult {
        outcome: task_outcome(outcome),
        final_text: if summary.trim().is_empty() {
            reason_code.into()
        } else {
            summary.into()
        },
        partial: outcome != CompletionOutcome::Completed,
    }
}

fn bounded_cancelled_task_result() -> TaskResult {
    TaskResult {
        outcome: TaskOutcome::Cancelled,
        final_text: "task cancelled".into(),
        partial: true,
    }
}

pub(crate) fn bounded_result_invalid_task_result() -> TaskResult {
    TaskResult {
        outcome: TaskOutcome::ResultInvalid,
        final_text: "result unavailable".into(),
        partial: true,
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct UnstartedTerminal<'a> {
    pub(crate) outcome: CompletionOutcome,
    pub(crate) reason_code: &'a str,
    pub(crate) message: &'a str,
}

pub(crate) fn finalized_general(
    prepared: &PreparedGeneralTask,
    outcome: CompletionOutcome,
    reason_code: &str,
    message: &str,
) -> GeneralCompletion {
    let mut completion = GeneralFinalizer::finalize(prepared, outcome);
    if completion.summary.trim().is_empty() {
        completion.summary = if message.trim().is_empty() {
            reason_code.into()
        } else {
            message.into()
        };
    }
    if completion.reason_code.is_none() && outcome != CompletionOutcome::Completed {
        completion.reason_code = Some(reason_code.into());
    }
    completion
}

pub(crate) fn unreaped_general(
    outcome: CompletionOutcome,
    reason_code: &str,
    message: &str,
) -> GeneralCompletion {
    GeneralCompletion {
        outcome,
        reason_code: (outcome != CompletionOutcome::Completed).then(|| reason_code.into()),
        summary: if message.trim().is_empty() {
            reason_code.into()
        } else {
            message.into()
        },
        residual_gaps: Vec::new(),
        cleaned: false,
    }
}

impl LifecycleSink for StoreLifecycleSink {
    fn emit(&self, record: LifecycleRecord) {
        let Some(_admission) = self.runtime_lifecycle.admit_event() else {
            return;
        };
        #[cfg(test)]
        if let Some(hook) = self.after_admission_hook.lock().unwrap().clone() {
            hook();
        }
        self.activity.observe(&record.event);
        let mut state = self.write_state.lock().unwrap();
        if state.first_error.is_some() {
            return;
        }
        state.last_source_sequence = state.last_source_sequence.max(record.sequence);
        if matches!(record.event, RuntimeEvent::Terminal(_)) {
            state.pending_terminal_sequence = Some(record.sequence);
            return;
        }
        let pending_request_id = match &record.event {
            RuntimeEvent::Driver(Inbound::Message(WireMessage::Request(request)))
                if matches!(
                    request.method.as_str(),
                    INTERACTION_REQUEST_PERMISSION
                        | INTERACTION_REQUEST_USER_INPUT
                        | INTERACTION_REQUEST_UNSUPPORTED_INPUT
                ) =>
            {
                let request_id = format!("{}:request:{}", self.agent_id, record.sequence);
                let correlation_id = match serde_json::to_string(&request.id) {
                    Ok(value) => value,
                    Err(error) => {
                        state.first_error = Some(error.to_string());
                        return;
                    }
                };
                // Only real producers become answerable user input; the
                // unsupported sentinel stays an observable, non-respondable
                // record no caller capability can act on.
                let request_type = match request.method.as_str() {
                    INTERACTION_REQUEST_PERMISSION => "permission",
                    INTERACTION_REQUEST_USER_INPUT => "user_input",
                    _ => "unsupported_input",
                };
                if let Err(error) = self.store.insert_pending_request(
                    &request_id,
                    &self.agent_id,
                    &correlation_id,
                    request_type,
                    &request.params.to_string(),
                ) {
                    state.first_error = Some(error.to_string());
                    return;
                }
                Some(request_id)
            }
            _ => None,
        };
        let projection =
            projection::lifecycle_projection(&record.event, pending_request_id.as_deref());
        let write = LifecycleWrite {
            agent_id: self.agent_id.clone(),
            runtime_agent_id: self.runtime_agent_id.clone(),
            owner_epoch: self.owner_epoch,
            source_sequence: record.sequence,
            event_type: projection.event_type.into(),
            turn_id: None,
            payload_json: projection.payload_json,
            redaction_level: projection.redaction_level.into(),
            terminal: None,
            turn_state: match &record.event {
                RuntimeEvent::Driver(Inbound::Lifecycle { method, .. }) => match method.as_str() {
                    "turn.started" => Some(TurnState::Active),
                    "turn.completed" => Some(TurnState::Idle),
                    "turn.failed" => Some(TurnState::Failed),
                    _ => None,
                },
                _ => None,
            },
        };
        if let Err(error) = self.store.append_lifecycle(&write) {
            state.first_error = Some(error.to_string());
        }
    }
}
