//! Sole stream-json interpreter. Typed batches reach the shared publisher before
//! waiters see settlement. The native buffered-input classifier remains local;
//! session identity and global step indices never stand in for local turns.

use std::{
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

use external_agent_agy::{
    event::{AgyEvent, ResultPayload, StepState, StepType},
    session::{TurnClassifier, TurnOutcome as NativeOutcome},
};
use external_contract::{events::*, WireMessage};
use external_runtime::Inbound;
use serde_json::Value;

use crate::{Publisher, RuntimeCommandDetail, RuntimeCommandError, RuntimeState};

const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;

pub(super) enum AgyStartState {
    Pending,
    Ready,
    Failed(RuntimeCommandDetail),
}

struct AgyTurnState {
    classifier: TurnClassifier,
    open: bool,
    local_id: u64,
}

pub(super) struct AgyRuntimeShared {
    pub(super) publisher: Arc<Publisher>,
    pub(super) runtime_state: Arc<RuntimeState>,
    task: TaskId,
    launch: LaunchId,
    pub(super) session_id: Mutex<Option<String>>,
    model: Mutex<Option<String>>,
    pub(super) start: Mutex<AgyStartState>,
    pub(super) start_changed: Condvar,
    turn: Mutex<AgyTurnState>,
    // Registration/withdrawal/consumption are serialized by turn. Snapshot
    // reads this atomically to avoid a publisher -> turn lock dependency.
    pending_inputs: AtomicUsize,
    pub(super) stop_boundaries: AtomicU64,
    diagnostics: Mutex<String>,
}

impl AgyRuntimeShared {
    pub(super) fn new(publisher: Arc<Publisher>) -> Self {
        let runtime_state = Arc::clone(&publisher.runtime_state);
        let (task, launch) = publisher.event_scope();
        Self {
            publisher,
            runtime_state,
            task,
            launch,
            session_id: Mutex::new(None),
            model: Mutex::new(None),
            start: Mutex::new(AgyStartState::Pending),
            start_changed: Condvar::new(),
            turn: Mutex::new(AgyTurnState {
                classifier: TurnClassifier::new(),
                open: false,
                local_id: 0,
            }),
            pending_inputs: AtomicUsize::new(0),
            stop_boundaries: AtomicU64::new(0),
            diagnostics: Mutex::new(String::new()),
        }
    }

    pub(super) fn session_id(&self) -> Option<String> {
        self.session_id.lock().unwrap().clone()
    }
    pub(super) fn model(&self) -> Option<String> {
        self.model.lock().unwrap().clone()
    }

    pub(super) fn begin_turn(&self) {
        let mut turn = self.turn.lock().unwrap();
        self.emit_started(&mut turn);
    }

    fn emit_started(&self, turn: &mut AgyTurnState) {
        turn.local_id = turn.local_id.saturating_add(1);
        turn.open = true;
        let id = TurnId::Local(turn.local_id);
        self.publish(
            self.input_context(Some("user"), Some(id.clone()), None),
            vec![EventFact::TurnStarted { turn_id: id }],
            None,
        );
    }

    /// Register before writing; a write failure withdraws exactly that input.
    pub(super) fn buffer_input(
        &self,
        write: impl FnOnce() -> Result<(), RuntimeCommandError>,
    ) -> Result<(), RuntimeCommandError> {
        let _turn = self.turn.lock().unwrap();
        self.pending_inputs.fetch_add(1, Ordering::AcqRel);
        if let Err(error) = write() {
            self.pending_inputs.fetch_sub(1, Ordering::AcqRel);
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn turn_snapshot(&self) -> crate::TurnSnapshot {
        // Read pending first. Consumption publishes started before decrement,
        // so no reader can see both the old idle turn and the consumed zero.
        let pending = self.pending_inputs.load(Ordering::Acquire);
        let mut snapshot = self.runtime_state.turn_snapshot();
        snapshot.active |= pending > 0;
        snapshot
    }

    fn begin_pending_turn(&self, turn: &mut AgyTurnState) {
        self.emit_started(turn);
        self.pending_inputs.fetch_sub(1, Ordering::AcqRel);
    }

    pub(super) fn wait_start(&self, timeout: Duration) -> Result<String, RuntimeCommandError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RuntimeCommandError::Timeout)?;
        let mut start = self.start.lock().unwrap();
        loop {
            match &*start {
                AgyStartState::Ready => return Ok(self.session_id().unwrap_or_default()),
                AgyStartState::Failed(detail) => return Err(start_failure_error(detail.clone())),
                AgyStartState::Pending => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(RuntimeCommandError::Timeout);
                    }
                    let (next, wait) = self
                        .start_changed
                        .wait_timeout(start, deadline - now)
                        .unwrap();
                    start = next;
                    if wait.timed_out() && matches!(*start, AgyStartState::Pending) {
                        return Err(RuntimeCommandError::Timeout);
                    }
                }
            }
        }
    }

    fn ignore_foreign_session(&self, actual: Option<&str>) -> bool {
        if let (Some(expected), Some(actual)) = (self.session_id(), actual) {
            if expected != actual {
                let turn = self.turn.lock().unwrap();
                let input =
                    self.input_context(Some("foreign_session"), turn_id(&turn), Some(actual));
                self.ignored(
                    input,
                    Attribution::ForeignSession,
                    IgnoreReason::ForeignSession,
                );
                return true;
            }
        }
        false
    }

    /// Pump-only entry for parsed native records. Never called concurrently.
    pub(super) fn project_event(&self, event: &AgyEvent) {
        if self.ignore_foreign_session(event.conversation_id()) {
            return;
        }
        match event {
            AgyEvent::Init {
                conversation_id,
                init,
            } => {
                if conversation_id.is_empty() {
                    let input = self.input_context(Some("init"), None, None);
                    self.invalid(input, Impact::Unknown);
                    return;
                }
                *self.session_id.lock().unwrap() = Some(conversation_id.clone());
                *self.model.lock().unwrap() = init.model.clone();
                let turn = self.turn.lock().unwrap();
                let input = self.input_context(Some("init"), turn_id(&turn), None);
                self.publish(
                    input,
                    vec![EventFact::SessionReady(SessionInfo {
                        session_id: SessionId(conversation_id.clone()),
                        model: init.model.clone(),
                        capabilities: vec![],
                    })],
                    None,
                );
                let mut start = self.start.lock().unwrap();
                if matches!(*start, AgyStartState::Pending) {
                    *start = AgyStartState::Ready;
                    self.start_changed.notify_all();
                }
            }
            AgyEvent::StepUpdate(step) => {
                let mut turn = self.turn.lock().unwrap();
                if step.step_type == StepType::UserInput
                    && !turn.open
                    && self.pending_inputs.load(Ordering::Acquire) > 0
                {
                    self.begin_pending_turn(&mut turn);
                }
                let mut input = self.input_context(
                    Some("step_update"),
                    turn_id(&turn),
                    Some(&step.conversation_id),
                );
                input.source.item_id = Some(step.step_index.to_string());
                if !turn.open {
                    self.ignored(input, Attribution::Late, IgnoreReason::Late);
                    return;
                }
                let mut facts = Vec::new();
                if step.step_type == StepType::AgentResponse {
                    if let Some(delta) = step.text_delta.as_deref().filter(|text| !text.is_empty())
                    {
                        let id = MessageId(format!("step-{}", step.step_index));
                        facts.push(EventFact::MessageUpdated(MessageUpdate {
                            message_id: id.clone(),
                            text: TextUpdate::Delta(delta.to_owned()),
                        }));
                        if step.state == StepState::Done {
                            facts.push(EventFact::MessageFinished { message_id: id });
                        }
                    }
                }
                if step.step_type == StepType::Tool {
                    let call = format!("{}:{}", step.conversation_id, step.step_index);
                    input.source.call_id = Some(call.clone());
                    let info = step.tool_info.as_ref();
                    let failure = info.and_then(|i| i.error.as_ref()).map(native_failure);
                    // Output prose, including strings resembling exit status,
                    // supplies no explicit result evidence.
                    facts.push(EventFact::ToolChanged(ToolChange {
                        call_id: CallId(call),
                        name: info
                            .and_then(|i| i.name.clone())
                            .or_else(|| step.tool_name.clone()),
                        category: None,
                        arguments: info
                            .and_then(|i| i.parameters.as_ref())
                            .map(|v| ArgumentUpdate::Snapshot(public_input(v))),
                        phase: match step.state {
                            StepState::Active => Some(ToolPhase::Started),
                            StepState::Done => Some(ToolPhase::Finished),
                            StepState::Other(_) => None,
                        },
                        outcome: if failure.is_some() {
                            Some(ToolOutcome::Failed)
                        } else if step.state == StepState::Done {
                            Some(ToolOutcome::Unknown)
                        } else {
                            None
                        },
                        failure,
                        denied: None,
                    }));
                }
                let unknown = matches!(step.state, StepState::Other(_))
                    || matches!(step.step_type, StepType::Other(_));
                let disposition =
                    unknown.then(|| unknown_disposition(&input, step_impact(&step.step_type)));
                self.publish(input, facts, disposition);
            }
            AgyEvent::Result(result) => self.project_result(event, result),
            AgyEvent::Unknown { event, .. } => {
                let turn = self.turn.lock().unwrap();
                let input = self.input_context(event.as_deref(), turn_id(&turn), None);
                let disposition = unknown_disposition(&input, Impact::Unknown);
                self.publish(input, vec![], Some(disposition));
            }
        }
    }

    fn project_result(&self, event: &AgyEvent, result: &ResultPayload) {
        {
            let mut start = self.start.lock().unwrap();
            if matches!(*start, AgyStartState::Pending) {
                if result.conversation_id.is_empty() {
                    let reason = result.error.clone().unwrap_or_else(|| {
                        format!(
                            "agy session start failed with status {}",
                            result.status.as_str()
                        )
                    });
                    let detail = match &result.error {
                        Some(error) => RuntimeCommandDetail::from_native(
                            crate::scheduler::redact_native_error(&Value::String(reason))
                                .as_str()
                                .unwrap_or_default()
                                .chars()
                                .take(512)
                                .collect::<String>(),
                            &Value::String(error.clone()),
                        ),
                        None => reason.into(),
                    };
                    *start = AgyStartState::Failed(detail);
                    self.start_changed.notify_all();
                    return;
                }
                *self.session_id.lock().unwrap() = Some(result.conversation_id.clone());
                *start = AgyStartState::Ready;
                self.start_changed.notify_all();
            }
        }
        let mut turn = self.turn.lock().unwrap();
        if !turn.open
            && self.pending_inputs.load(Ordering::Acquire) > 0
            && result.num_turns > turn.classifier.num_turns()
        {
            self.begin_pending_turn(&mut turn);
        }
        // Idle signal-trap duplicate results do not overwrite a settled turn.
        if !turn.open {
            let input = self.input_context(
                Some("result"),
                turn_id(&turn),
                Some(&result.conversation_id),
            );
            self.ignored(input, Attribution::Late, IgnoreReason::Late);
            return;
        }
        let Some(settlement) = turn.classifier.observe(event) else {
            return;
        };
        turn.open = false;
        let id = TurnId::Local(turn.local_id);
        let input = self.input_context(
            Some("result"),
            Some(id.clone()),
            Some(&result.conversation_id),
        );
        let (outcome, text) = match &settlement.outcome {
            NativeOutcome::Completed { text } => (TurnOutcome::Completed, Some(text.clone())),
            NativeOutcome::Failed { .. } => (
                TurnOutcome::Failed,
                (!result.response.is_empty()).then(|| result.response.clone()),
            ),
            NativeOutcome::Cancelled { text, .. } => (TurnOutcome::Cancelled, Some(text.clone())),
        };
        let failure = if outcome == TurnOutcome::Completed {
            None
        } else {
            result
                .error
                .as_ref()
                .map(|e| native_failure(&Value::String(e.clone())))
        };
        let mut facts = Vec::new();
        if outcome == TurnOutcome::Cancelled {
            // An interrupted response can arrive without any earlier text
            // delta. Preserve AGY's existing append-to-wait delivery before
            // the settlement becomes visible, through this same typed batch.
            if let Some(partial) = text.as_deref().filter(|text| !text.is_empty()) {
                facts.push(EventFact::MessageUpdated(MessageUpdate {
                    message_id: MessageId("interrupted-response".into()),
                    text: TextUpdate::Delta(partial.to_owned()),
                }));
            }
        }
        self.record_denied_actions(&settlement.denied_actions);
        if !settlement.denied_actions.is_empty() {
            facts.push(EventFact::Diagnostic(Diagnostic {
                reason: DiagnosticReason::PermissionDenied,
                impact: Impact::Known(vec![CoverageDimension::Tools]),
                discriminator: SafeToken::new("denied_actions"),
            }));
        }
        if let Some(failure) = &failure {
            facts.push(EventFact::ErrorObserved {
                operation: Operation::Turn,
                failure: failure.clone(),
            });
        }
        facts.push(EventFact::TurnSettled(TurnSettlement {
            turn_id: id,
            outcome,
            final_text: text,
            failure,
            stop_reason: SafeToken::new(result.status.as_str()),
        }));
        self.publish(input, facts, None);
    }

    fn input_context(
        &self,
        discriminator: Option<&str>,
        turn_id: Option<TurnId>,
        session: Option<&str>,
    ) -> NativeInputContext {
        NativeInputContext {
            task_id: self.task.clone(),
            launch_id: self.launch.clone(),
            turn_id,
            source: NativeSource {
                protocol: SourceProtocol::AgyStreamJson,
                discriminator: discriminator.and_then(SafeToken::new),
                session_id: session
                    .filter(|s| !s.is_empty())
                    .map(|s| SessionId(s.to_owned()))
                    .or_else(|| self.session_id().map(SessionId)),
                turn_id: None,
                item_id: None,
                call_id: None,
                event_id: None,
                timestamp: None,
            },
        }
    }

    fn publish(
        &self,
        input: NativeInputContext,
        mut facts: Vec<EventFact>,
        disposition: Option<DecodeDisposition>,
    ) {
        let disposition = disposition.unwrap_or_else(|| {
            if facts.is_empty() {
                DecodeDisposition::RecognizedIgnored(IgnoreReason::NoConsumer)
            } else {
                DecodeDisposition::Consumed
            }
        });
        match &disposition {
            DecodeDisposition::Unknown(d) => facts.push(EventFact::Diagnostic(d.clone())),
            DecodeDisposition::InvalidLost(g) => facts.push(EventFact::CoverageGap(g.clone())),
            _ => {}
        }
        let events = facts
            .into_iter()
            .enumerate()
            .map(|(i, fact)| NormalizedEvent {
                context: EventContext {
                    task_id: input.task_id.clone(),
                    launch_id: input.launch_id.clone(),
                    sequence: i as u64 + 1,
                    turn_id: input.turn_id.clone(),
                    source: input.source.clone(),
                },
                fact,
            })
            .collect();
        let batch = DecodedInput::new(input, Attribution::Current, disposition, events)
            .expect("AGY batch invariant");
        self.publisher.emit_typed(batch);
    }

    fn ignored(&self, input: NativeInputContext, attribution: Attribution, reason: IgnoreReason) {
        let batch = DecodedInput::new(
            input,
            attribution,
            DecodeDisposition::RecognizedIgnored(reason),
            vec![],
        )
        .expect("AGY ignored batch invariant");
        self.publisher.emit_typed(batch);
    }

    fn invalid(&self, input: NativeInputContext, impact: Impact) {
        let gap = CoverageGap {
            reason: LossReason::Malformed,
            impact,
            activity: input
                .turn_id
                .clone()
                .map(ActivityScope::Turn)
                .unwrap_or(ActivityScope::None),
        };
        self.publish(input, vec![], Some(DecodeDisposition::InvalidLost(gap)));
    }

    fn record_denied_actions(&self, denied: &[external_agent_agy::event::DeniedAction]) {
        if denied.is_empty() {
            return;
        }
        let rendered = denied
            .iter()
            .map(|entry| {
                if entry.display_name.is_empty() {
                    entry.action.clone()
                } else {
                    format!("{} ({})", entry.action, entry.display_name)
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let line = crate::scheduler::redact_native_error(&Value::String(format!(
            "agy denied_actions: {rendered}"
        )))
        .as_str()
        .unwrap_or_default()
        .to_owned();
        let mut tail = self.diagnostics.lock().unwrap();
        if !tail.is_empty() {
            tail.push('\n');
        }
        tail.push_str(&line);
        let mut split = tail.len().saturating_sub(MAX_DIAGNOSTIC_BYTES);
        while split < tail.len() && !tail.is_char_boundary(split) {
            split += 1;
        }
        tail.drain(..split);
    }

    pub(super) fn diagnostic_tail(&self) -> String {
        self.diagnostics.lock().unwrap().clone()
    }
}

fn turn_id(turn: &AgyTurnState) -> Option<TurnId> {
    (turn.local_id > 0).then_some(TurnId::Local(turn.local_id))
}
fn unknown_disposition(input: &NativeInputContext, impact: Impact) -> DecodeDisposition {
    DecodeDisposition::Unknown(Diagnostic {
        reason: DiagnosticReason::UnknownSemantics,
        impact,
        discriminator: input.source.discriminator.clone(),
    })
}
fn step_impact(step: &StepType) -> Impact {
    match step {
        StepType::Tool => Impact::Known(vec![CoverageDimension::Tools]),
        StepType::AgentResponse => Impact::Known(vec![CoverageDimension::Text]),
        _ => Impact::Unknown,
    }
}
fn native_failure(original: &Value) -> NativeFailure {
    NativeFailure {
        original: crate::scheduler::redact_native_error(original),
        projection: None,
    }
}
fn public_input(value: &Value) -> Value {
    fn public(value: &Value) -> Value {
        match value {
            Value::Object(fields) => fields
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        if k == "encrypted_content" {
                            Value::Null
                        } else {
                            public(v)
                        },
                    )
                })
                .collect(),
            Value::Array(values) => values.iter().map(public).collect(),
            other => other.clone(),
        }
    }
    crate::scheduler::redact_native_error(&public(value))
}

fn start_failure_error(detail: RuntimeCommandDetail) -> RuntimeCommandError {
    if detail.contains("invalid model")
        || detail.contains("invalid effort")
        || detail.contains("model selection")
    {
        RuntimeCommandError::ModelRejected(detail)
    } else {
        RuntimeCommandError::InvalidSession(detail)
    }
}

/// Non-NDJSON transport events retain the existing runtime owner path.
pub(super) fn project_agy_inbound(shared: &AgyRuntimeShared, event: &Inbound) -> Option<Inbound> {
    let Inbound::Message(WireMessage::UnknownEvent { raw, .. }) = event else {
        return Some(event.clone());
    };
    // Attribute readable known envelopes before body validation can fail;
    // malformed foreign traffic must not damage the current turn's coverage.
    let session = external_agent_agy::event::envelope_conversation_id(raw);
    if shared.ignore_foreign_session(session) {
        return None;
    }
    match serde_json::to_string(raw)
        .ok()
        .and_then(|line| external_agent_agy::event::parse_line(&line).ok())
    {
        Some(parsed) => shared.project_event(&parsed),
        None => {
            let turn = shared.turn.lock().unwrap();
            let discriminator = raw.get("event").and_then(Value::as_str);
            let input = shared.input_context(discriminator, turn_id(&turn), session);
            let impact = match discriminator {
                Some("step_update") => raw
                    .pointer("/step_update/step_type")
                    .and_then(Value::as_str)
                    .map(|kind| match kind {
                        "tool" => Impact::Known(vec![CoverageDimension::Tools]),
                        "agent_response" => Impact::Known(vec![CoverageDimension::Text]),
                        _ => Impact::Unknown,
                    })
                    .unwrap_or(Impact::Unknown),
                Some("result") => Impact::Known(vec![CoverageDimension::Turn]),
                _ => Impact::Unknown,
            };
            shared.invalid(input, impact);
        }
    }
    None
}
