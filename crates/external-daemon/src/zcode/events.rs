//! The sole interpreter of ZCode session notifications and v4 telemetry.
//! Telemetry terminal summaries never settle a turn or replace its native error.
use external_contract::{events::*, WireId, WireMessage};
use external_runtime::Inbound;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};

const MAX_REPLAY_FRAMES: usize = 65_536;

pub(crate) struct ZcodeAdapter {
    task: TaskId,
    launch: LaunchId,
    session: Option<SessionId>,
    turn: Option<TurnId>,
    local_turn: u64,
    settled: HashSet<TurnId>,
    frames: HashSet<NativeFrameKey>,
    frame_order: VecDeque<NativeFrameKey>,
    permissions: HashMap<String, WireId>,
    user_inputs: HashMap<String, WireId>,
    message: MessageId,
}

impl ZcodeAdapter {
    pub(crate) fn new(task: TaskId, launch: LaunchId) -> Self {
        Self {
            task,
            launch,
            session: None,
            turn: None,
            local_turn: 0,
            settled: HashSet::new(),
            frames: HashSet::new(),
            frame_order: VecDeque::new(),
            permissions: HashMap::new(),
            user_inputs: HashMap::new(),
            message: MessageId("assistant-stream".into()),
        }
    }

    pub(crate) fn decode(&mut self, inbound: &Inbound) -> Option<DecodedInput> {
        if let Inbound::Message(WireMessage::Request(request)) = inbound {
            if request.method == external_contract::INTERACTION_REQUEST_PERMISSION {
                if let Some(call) = string(&request.params, "toolCallId") {
                    self.permissions.insert(call.into(), request.id.clone());
                }
            } else if request.method == external_contract::INTERACTION_REQUEST_USER_INPUT {
                if let Some(interaction) = string(&request.params, "requestId") {
                    self.user_inputs
                        .insert(interaction.into(), request.id.clone());
                }
            }
            return None; // native RPC response remains owned by command handling
        }
        let (method, params) = match inbound {
            Inbound::Message(message) => message.notification()?,
            Inbound::Malformed(_) | Inbound::OversizedLine { .. } => {
                let reason = if matches!(inbound, Inbound::OversizedLine { .. }) {
                    LossReason::Truncated
                } else {
                    LossReason::Malformed
                };
                let input = self.context(SourceProtocol::Runtime, None, &Value::Null);
                let mut batch = self.invalid(input.clone(), reason, Impact::Unknown);
                if matches!(inbound, Inbound::OversizedLine { .. }) {
                    batch = Self::batch(
                        input,
                        Attribution::Current,
                        batch.disposition().clone(),
                        vec![
                            batch.events()[0].fact.clone(),
                            EventFact::TransportFault {
                                kind: TransportFaultKind::FrameLimit,
                                failure: None,
                                stderr_ref: None,
                            },
                        ],
                    );
                }
                return Some(batch);
            }
            _ => return None,
        };
        let telemetry = method == "v4/telemetry/event";
        let discriminator = if telemetry {
            string(params, "kind")
        } else if method == "session/event" {
            string(params, "type")
        } else {
            Some(method)
        };
        let protocol = if telemetry {
            SourceProtocol::ZcodeTelemetryV4
        } else {
            SourceProtocol::ZcodeSession
        };
        let mut input = self.context(protocol, discriminator, params);
        if let Some(session) = input.source.session_id.as_ref() {
            if self
                .session
                .as_ref()
                .is_some_and(|current| current != session)
            {
                return Some(Self::ignored(
                    input,
                    Attribution::ForeignSession,
                    IgnoreReason::ForeignSession,
                ));
            }
            self.session = Some(session.clone());
        }
        if let Some(key) = input.frame_key() {
            if self.frames.contains(&key) {
                return Some(Self::ignored(
                    input,
                    Attribution::Replay,
                    IgnoreReason::Replay,
                ));
            }
            // Replay proof covers the latest distinct identities in receipt
            // order; replays do not refresh that window. An evicted identity
            // is processed under the native session/turn attribution rules.
            // Eviction loses no received fact and asserts no coverage damage.
            if self.frames.len() >= MAX_REPLAY_FRAMES {
                if let Some(oldest) = self.frame_order.pop_front() {
                    self.frames.remove(&oldest);
                }
            }
            self.frame_order.push_back(key.clone());
            self.frames.insert(key);
        }
        let kind = discriminator.unwrap_or("");
        let payload = if telemetry {
            params
        } else {
            params.get("payload").unwrap_or(&Value::Null)
        };
        if kind == "turn.started" {
            let turn = input.turn_id.clone().unwrap_or_else(|| {
                // The production protocol has a native ID. Scripted/older
                // producers without one use an explicit local control scope.
                self.local_turn += 1;
                TurnId::Local(self.local_turn)
            });
            input.turn_id = Some(turn.clone());
            if self.turn.as_ref() == Some(&turn) || self.settled.contains(&turn) {
                return Some(Self::ignored(
                    input,
                    Attribution::Replay,
                    IgnoreReason::Replay,
                ));
            }
            self.turn = Some(turn.clone());
            self.message = MessageId("assistant-stream".into());
            return Some(Self::consumed(
                input,
                vec![EventFact::TurnStarted { turn_id: turn }],
            ));
        }
        if input
            .turn_id
            .as_ref()
            .is_some_and(|turn| self.settled.contains(turn))
        {
            return Some(Self::ignored(input, Attribution::Late, IgnoreReason::Late));
        }
        if matches!(self.turn, Some(TurnId::Local(_)))
            && matches!(input.turn_id, Some(TurnId::Native(_)))
        {
            self.turn = input.turn_id.clone();
        }
        if let (Some(current), Some(incoming)) = (&self.turn, &input.turn_id) {
            if current != incoming {
                return Some(Self::ignored(input, Attribution::Late, IgnoreReason::Late));
            }
        }
        if input.turn_id.is_none() {
            input.turn_id = self.turn.clone();
        }
        if telemetry && (!params.is_object() || params.get("version").is_some_and(|v| v != 1)) {
            return Some(self.invalid(input, LossReason::Malformed, impact(kind)));
        }
        let result = if telemetry {
            self.telemetry(kind, payload, &input)
        } else if method == "session/event" {
            self.session_event(kind, payload, &input)
        } else if matches!(
            method,
            "state.updated"
                | "v4/conversation/frame"
                | "v4/telemetry/local-ttft"
                | "v4/plugin-ui/tool-result"
                | "v4/cua/permission-observation"
                | "v4/offPeak/lifecycle"
        ) {
            Ok(Err(IgnoreReason::Ui))
        } else {
            Err(false)
        };
        Some(match result {
            Ok(Ok(mut facts)) => {
                if !facts
                    .iter()
                    .any(|fact| matches!(fact, EventFact::InputResolved(_)))
                {
                    if let Some((call, denied)) = facts.iter().find_map(|fact| match fact {
                        EventFact::ToolChanged(tool) => {
                            tool.denied.map(|denied| (tool.call_id.clone(), denied))
                        }
                        _ => None,
                    }) {
                        if let Some(id) = self.permissions.get(&call.0) {
                            facts.push(EventFact::InputResolved(InputResolution {
                                request_id: id.clone(),
                                call_id: Some(call),
                                outcome: if denied {
                                    InputOutcome::Denied
                                } else {
                                    InputOutcome::Allowed
                                },
                            }));
                        }
                    }
                }
                // A notification's requestId may be absent or differ from the
                // RPC envelope id. The adapter alone correlates the two.
                for fact in &mut facts {
                    if let EventFact::InputResolved(resolution) = fact {
                        if let Some(call) = &resolution.call_id {
                            if let Some(id) = self.permissions.get(&call.0) {
                                resolution.request_id = id.clone();
                            }
                        } else if let WireId::String(interaction) = &resolution.request_id {
                            if let Some(envelope_id) = self.user_inputs.remove(interaction) {
                                resolution.request_id = envelope_id;
                            }
                        }
                    }
                }
                for fact in &facts {
                    if let EventFact::InputResolved(resolution) = fact {
                        if let Some(call) = &resolution.call_id {
                            self.permissions.remove(&call.0);
                        }
                    }
                }
                Self::consumed(input, facts)
            }
            Ok(Err(reason)) => Self::ignored(input, Attribution::Current, reason),
            Err(true) => self.invalid(input, LossReason::Malformed, input_impact(kind, payload)),
            Err(false) => {
                let diagnostic = Diagnostic {
                    reason: DiagnosticReason::UnknownSemantics,
                    impact: impact(kind),
                    discriminator: input.source.discriminator.clone(),
                };
                Self::batch(
                    input,
                    Attribution::Current,
                    DecodeDisposition::Unknown(diagnostic.clone()),
                    vec![EventFact::Diagnostic(diagnostic)],
                )
            }
        })
    }

    fn telemetry(&self, kind: &str, p: &Value, input: &NativeInputContext) -> Parsed {
        match kind {
            "model.request.status" => {
                if matches!(
                    string(p, "status"),
                    Some("model_request_queued" | "model_request_admitted")
                ) {
                    Err(false)
                } else {
                    model(p, string(p, "status"))
                }
            }
            "tool.lifecycle" => tool(p, string(p, "phase"), true),
            "permission.lifecycle" => permission(p, string(p, "phase")),
            "turn.terminal" => {
                if !matches!(
                    string(p, "status"),
                    Some("success" | "failed" | "interrupted")
                ) {
                    return Err(true);
                }
                Ok(Ok(vec![EventFact::Diagnostic(Diagnostic {
                    reason: DiagnosticReason::AuxiliaryTerminal,
                    impact: Impact::Known(vec![]),
                    discriminator: input.source.discriminator.clone(),
                })]))
            }
            "stream.chunk" => {
                if !matches!(string(p, "channel"), Some("text" | "thought"))
                    || !p.get("chunkLength").is_some_and(Value::is_u64)
                {
                    return Err(true);
                }
                Ok(Err(IgnoreReason::StreamBoundary))
            } // lengths are not public body/reasoning
            "usage.delta" => Ok(Err(IgnoreReason::Usage)),
            "compaction.terminal" => {
                if string(p, "operationId").is_none()
                    || !matches!(
                        string(p, "status"),
                        Some("completed" | "failed" | "interrupted")
                    )
                {
                    return Err(true);
                }
                Ok(Err(IgnoreReason::Compaction))
            }
            "subagent.lifecycle" => {
                if !matches!(string(p, "phase"), Some("spawned" | "stopped"))
                    || string(p, "agentId").is_none()
                    || string(p, "childSessionId").is_none()
                {
                    return Err(true);
                }
                Ok(Err(IgnoreReason::NoConsumer))
            }
            "workflow.lifecycle" => {
                if string(p, "runId").is_none()
                    || !matches!(string(p, "phase"), Some("actor-spawned" | "run-settled"))
                {
                    return Err(true);
                }
                Ok(Err(IgnoreReason::NoConsumer))
            }
            _ => Err(false),
        }
    }

    fn session_event(&mut self, kind: &str, p: &Value, input: &NativeInputContext) -> Parsed {
        match kind {
            "session.created" | "session.resumed" => {
                let session_id = input.source.session_id.clone().ok_or(true)?;
                Ok(Ok(vec![EventFact::SessionReady(SessionInfo {
                    session_id,
                    model: string(p, "model").map(str::to_owned),
                    capabilities: vec![],
                })]))
            }
            "session.updated" => {
                let status = string(p, "type");
                if matches!(
                    status,
                    Some(
                        "model_request_started"
                            | "model_request_completed"
                            | "model_request_failed"
                            | "model_retry_scheduled"
                            | "model_stream_stalled"
                            | "model_request_queued"
                            | "model_request_admitted"
                    )
                ) {
                    model(p, status)
                } else if status.is_none() && p.is_object() {
                    Ok(Err(IgnoreReason::Configuration))
                } else if status.is_some_and(known_session_update) {
                    Ok(Err(IgnoreReason::NoConsumer))
                } else {
                    Err(false)
                }
            }
            "turn.completed" | "turn.failed" => {
                let turn_id = input.turn_id.clone().ok_or(true)?;
                let result_type = string(p, "resultType");
                let result_failed = matches!(
                    result_type,
                    Some(
                        "error_max_turns"
                            | "error_max_budget"
                            | "error_during_execution"
                            | "error_max_tool_calls"
                    )
                );
                if kind == "turn.completed"
                    && result_type.is_some()
                    && !result_failed
                    && !matches!(result_type, Some("success" | "cancelled"))
                {
                    return Err(false);
                }
                let failure = if result_failed {
                    Some(failure(p))
                } else if kind == "turn.failed" {
                    Some(failure(
                        p.get("error").filter(|v| !v.is_null()).ok_or(true)?,
                    ))
                } else {
                    None
                };
                let final_text = if kind == "turn.completed" {
                    Some(
                        p.get("response")
                            .and_then(Value::as_str)
                            .filter(|text| !text.contains('\0'))
                            .ok_or(true)?
                            .to_owned(),
                    )
                } else {
                    None
                };
                let outcome = if kind == "turn.failed" || result_failed {
                    TurnOutcome::Failed
                } else if string(p, "resultType") == Some("cancelled") {
                    TurnOutcome::Cancelled
                } else {
                    TurnOutcome::Completed
                };
                let mut facts = Vec::new();
                if let Some(text) = &final_text {
                    facts.push(EventFact::MessageUpdated(MessageUpdate {
                        message_id: self.message.clone(),
                        text: TextUpdate::Snapshot(text.clone()),
                    }));
                }
                if let Some(failure) = &failure {
                    facts.push(EventFact::ErrorObserved {
                        operation: Operation::Turn,
                        failure: failure.clone(),
                    });
                }
                facts.push(EventFact::TurnSettled(TurnSettlement {
                    turn_id: turn_id.clone(),
                    outcome,
                    stop_reason: string(p, "resultType").and_then(SafeToken::new),
                    final_text,
                    failure,
                }));
                self.settled.insert(turn_id);
                Ok(Ok(facts))
            }
            "model.streaming" => {
                if let Some(id) = string(p, "assistantMessageId") {
                    self.message = MessageId(id.into());
                }
                stream(p)
            }
            "message.finished" | "message.completed" | "message.done" | "text.done" => {
                Ok(Ok(vec![EventFact::MessageFinished {
                    message_id: MessageId(
                        string(p, "assistantMessageId")
                            .unwrap_or("assistant-stream")
                            .into(),
                    ),
                }]))
            }
            "tool.updated" => tool(p, string(p, "kind"), false),
            "streamRecovery.updated" => {
                if p.is_object() {
                    Ok(Err(IgnoreReason::NoConsumer))
                } else {
                    Err(true)
                }
            }
            "permission.requested" => permission(p, Some("requested")),
            "permission.resolved" => permission(p, Some("resolved")),
            "userInput.requested" => {
                let id = string(p, "requestId").ok_or(true)?;
                Ok(Ok(vec![EventFact::InputRequested(InputRequest {
                    request_id: external_contract::WireId::String(id.into()),
                    kind: InputKind::Question,
                    respondable: false,
                    question: crate::scheduler::redact_native_error(p),
                })]))
            }
            "userInput.resolved" => {
                let id = string(p, "requestId").ok_or(true)?;
                Ok(Ok(vec![EventFact::InputResolved(InputResolution {
                    request_id: external_contract::WireId::String(id.into()),
                    outcome: if p.get("cancelled") == Some(&Value::Bool(true)) {
                        InputOutcome::Cancelled
                    } else {
                        InputOutcome::Answered
                    },
                    call_id: None,
                })]))
            }
            // The public model stream owns text/reasoning/argument deltas.
            // Persisted message/part notifications are overlapping UI snapshots.
            "message.upserted" | "message.removed" | "part.started" | "part.delta"
            | "part.upserted" | "part.removed" => Ok(Err(IgnoreReason::NoConsumer)),
            "session.titleUpdated"
            | "session.closed"
            | "turn.steerQueued"
            | "turn.steerDrained"
            | "checkpoint.created"
            | "rewind.triggered" => Ok(Err(IgnoreReason::NoConsumer)),
            _ => Err(false),
        }
    }

    fn context(
        &self,
        protocol: SourceProtocol,
        kind: Option<&str>,
        p: &Value,
    ) -> NativeInputContext {
        NativeInputContext {
            task_id: self.task.clone(),
            launch_id: self.launch.clone(),
            turn_id: string(p, "turnId")
                .or_else(|| {
                    p.get("payload")
                        .and_then(|payload| string(payload, "turnId"))
                })
                .map(|id| TurnId::Native(id.into())),
            source: NativeSource {
                protocol,
                discriminator: kind.and_then(SafeToken::new),
                session_id: string(p, "sessionId").map(|id| SessionId(id.into())),
                turn_id: string(p, "turnId").map(str::to_owned),
                item_id: None,
                call_id: string(p, "toolCallId")
                    .or_else(|| {
                        p.get("payload")
                            .and_then(|payload| string(payload, "toolCallId"))
                    })
                    .map(str::to_owned),
                event_id: string(p, "eventId").map(str::to_owned),
                timestamp: string(p, "occurredAt")
                    .map(str::to_owned)
                    .or_else(|| p.get("timestamp").map(Value::to_string)),
            },
        }
    }
    fn invalid(
        &self,
        input: NativeInputContext,
        reason: LossReason,
        impact: Impact,
    ) -> DecodedInput {
        let gap = CoverageGap {
            reason,
            impact,
            activity: input
                .turn_id
                .clone()
                .map(ActivityScope::Turn)
                .unwrap_or(ActivityScope::None),
        };
        Self::batch(
            input,
            Attribution::Current,
            DecodeDisposition::InvalidLost(gap.clone()),
            vec![EventFact::CoverageGap(gap)],
        )
    }
    fn ignored(
        input: NativeInputContext,
        attribution: Attribution,
        reason: IgnoreReason,
    ) -> DecodedInput {
        Self::batch(
            input,
            attribution,
            DecodeDisposition::RecognizedIgnored(reason),
            vec![],
        )
    }
    fn consumed(input: NativeInputContext, facts: Vec<EventFact>) -> DecodedInput {
        Self::batch(
            input,
            Attribution::Current,
            DecodeDisposition::Consumed,
            facts,
        )
    }
    fn batch(
        input: NativeInputContext,
        attribution: Attribution,
        disposition: DecodeDisposition,
        facts: Vec<EventFact>,
    ) -> DecodedInput {
        // Provisional order only. Publisher replaces these with its one launch sequence.
        let events = facts
            .into_iter()
            .enumerate()
            .map(|(index, fact)| NormalizedEvent {
                context: EventContext {
                    task_id: input.task_id.clone(),
                    launch_id: input.launch_id.clone(),
                    sequence: index as u64 + 1,
                    turn_id: input.turn_id.clone(),
                    source: input.source.clone(),
                },
                fact,
            })
            .collect();
        DecodedInput::new(input, attribution, disposition, events).expect("adapter batch invariant")
    }
}

type Parsed = Result<Result<Vec<EventFact>, IgnoreReason>, bool>; // true = malformed known input
fn string<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key)?
        .as_str()
        .filter(|s| !s.is_empty() && !s.contains('\0'))
}
fn impact(kind: &str) -> Impact {
    let dimensions = match kind {
        "model.request.status" | "session.updated" => vec![CoverageDimension::ModelActivity],
        "tool.lifecycle" | "tool.updated" => vec![CoverageDimension::Tools],
        "permission.lifecycle" | "permission.requested" | "permission.resolved" => {
            vec![CoverageDimension::Input]
        }
        "turn.completed" | "turn.failed" | "turn.terminal" => vec![CoverageDimension::Turn],
        _ => return Impact::Unknown,
    };
    Impact::Known(dimensions)
}
fn failure(original: &Value) -> NativeFailure {
    NativeFailure {
        original: crate::scheduler::redact_native_error(original),
        projection: None,
    }
}
fn model(p: &Value, status: Option<&str>) -> Parsed {
    let phase = match status {
        Some("model_request_started") => ModelPhase::Started,
        Some("model_request_completed") => ModelPhase::Completed,
        Some("model_request_failed") => ModelPhase::Failed,
        Some("model_retry_scheduled") => ModelPhase::RetryScheduled,
        Some("model_stream_stalled") => ModelPhase::StreamStalled,
        Some("model_request_queued" | "model_request_admitted") => {
            return Ok(Err(IgnoreReason::NoConsumer))
        }
        None => return Err(true),
        _ => return Err(false),
    };
    let request_id = RequestId(string(p, "requestId").ok_or(true)?.into());
    Ok(Ok(vec![EventFact::ModelActivityChanged(ModelActivity {
        request_id: Some(request_id),
        phase,
        retry: Some(NativeRetry {
            retryable: p.get("retryable").and_then(Value::as_bool),
            attempt: p.get("attempt").and_then(Value::as_u64),
            max_attempts: p.get("maxAttempts").and_then(Value::as_u64),
            next_attempt: p.get("nextAttempt").and_then(Value::as_u64),
            delay_ms: p.get("delayMs").and_then(Value::as_u64),
            ..NativeRetry::default()
        }),
        failure: (phase == ModelPhase::Failed).then(|| failure(p)),
    })]))
}
fn tool(p: &Value, phase: Option<&str>, telemetry: bool) -> Parsed {
    let native_error = p.get("error").filter(|error| !error.is_null()).or_else(|| {
        (!telemetry && phase == Some("result"))
            .then(|| p.get("result").and_then(|result| result.get("error")))
            .flatten()
            .filter(|error| !error.is_null())
    });
    let (phase, outcome) = match phase {
        Some("scheduled") => (ToolPhase::Scheduled, None),
        Some("started") => (ToolPhase::Started, None),
        Some("progress") => (ToolPhase::Progress, None),
        Some("result") if !telemetry => {
            let outcome = match p
                .get("result")
                .and_then(|result| result.get("success"))
                .and_then(Value::as_bool)
            {
                Some(true) => ToolOutcome::Succeeded,
                Some(false) => ToolOutcome::Failed,
                None if native_error.is_some() => ToolOutcome::Failed,
                None => ToolOutcome::Unknown,
            };
            (ToolPhase::Finished, Some(outcome))
        }
        Some("completed") if telemetry => (ToolPhase::Finished, Some(ToolOutcome::Succeeded)),
        Some("error") if !telemetry => (ToolPhase::Finished, Some(ToolOutcome::Failed)),
        Some("failed") if telemetry => (ToolPhase::Finished, Some(ToolOutcome::Failed)),
        Some("batch" | "raw") if !telemetry => return Ok(Err(IgnoreReason::NoConsumer)),
        None => return Err(true),
        _ => return Err(false),
    };
    let call_id = CallId(string(p, "toolCallId").ok_or(true)?.into());
    Ok(Ok(vec![EventFact::ToolChanged(ToolChange {
        call_id,
        name: string(p, "toolName").map(str::to_owned),
        category: None,
        arguments: p
            .get("input")
            .map(|v| ArgumentUpdate::Snapshot(public_arguments(v))),
        phase: Some(phase),
        outcome,
        failure: native_error
            .map(failure)
            .or_else(|| (outcome == Some(ToolOutcome::Failed)).then(|| failure(p))),
        denied: None,
    })]))
}
fn permission(p: &Value, phase: Option<&str>) -> Parsed {
    let call_id = CallId(string(p, "toolCallId").ok_or(true)?.into());
    let denied = phase == Some("denied") || string(p, "decision") == Some("deny");
    match phase {
        Some("requested") => Ok(Err(IgnoreReason::NoConsumer)), // RPC request owns answerability
        Some("resolved" | "denied") => {
            let mut facts = vec![];
            if let Some(id) = string(p, "requestId") {
                facts.push(EventFact::InputResolved(InputResolution {
                    request_id: external_contract::WireId::String(id.into()),
                    call_id: Some(call_id.clone()),
                    outcome: if denied {
                        InputOutcome::Denied
                    } else {
                        InputOutcome::Allowed
                    },
                }));
            }
            if denied {
                facts.push(EventFact::Diagnostic(Diagnostic {
                    reason: DiagnosticReason::PermissionDenied,
                    impact: Impact::Known(vec![CoverageDimension::Input]),
                    discriminator: None,
                }));
            }
            facts.push(EventFact::ToolChanged(ToolChange {
                call_id,
                name: None,
                category: None,
                arguments: None,
                phase: None,
                outcome: None,
                failure: None,
                denied: Some(denied),
            }));
            Ok(Ok(facts))
        }
        _ => Err(false),
    }
}
fn stream(p: &Value) -> Parsed {
    let message_id = || {
        MessageId(
            string(p, "assistantMessageId")
                .unwrap_or("assistant-stream")
                .into(),
        )
    };
    match string(p, "kind") {
        Some("text_delta") => Ok(Ok(vec![EventFact::MessageUpdated(MessageUpdate {
            message_id: message_id(),
            text: TextUpdate::Delta(public_delta(p).ok_or(true)?.into()),
        })])),
        Some("reasoning_delta") => Ok(Ok(vec![EventFact::PublicReasoningDelta(PublicReasoning {
            message_id: Some(message_id()),
            part_id: string(p, "partId").map(str::to_owned),
            channel: PublicReasoningChannel::Thought,
            delta: public_delta(p).ok_or(true)?.into(),
        })])),
        Some("tool_call") => {
            let call_id = CallId(string(p, "toolCallId").ok_or(true)?.into());
            Ok(Ok(vec![EventFact::ToolChanged(ToolChange {
                call_id,
                name: Some(string(p, "toolName").ok_or(true)?.into()),
                category: None,
                arguments: Some(ArgumentUpdate::Snapshot(public_arguments(
                    p.get("input").ok_or(true)?,
                ))),
                phase: None,
                outcome: None,
                failure: None,
                denied: None,
            })]))
        }
        Some("message_finished" | "message_done" | "text_done") => {
            Ok(Ok(vec![EventFact::MessageFinished {
                message_id: message_id(),
            }]))
        }
        Some("tool_input_start" | "tool_input_delta" | "tool_input_end") => {
            if string(p, "toolCallId").is_none()
                || (string(p, "kind") == Some("tool_input_start")
                    && string(p, "toolName").is_none())
                || (string(p, "kind") == Some("tool_input_delta") && public_delta(p).is_none())
            {
                return Err(true);
            }
            Ok(Err(IgnoreReason::StreamBoundary))
        }
        Some(
            "start" | "text_start" | "text_end" | "reasoning_start" | "reasoning_end" | "finish"
            | "error",
        ) => Ok(Err(IgnoreReason::StreamBoundary)),
        _ => Err(false),
    }
}

fn public_delta(p: &Value) -> Option<&str> {
    p.get("delta")?.as_str().filter(|text| !text.contains('\0'))
}

fn public_arguments(value: &Value) -> Value {
    fn public_shape(value: &Value) -> Value {
        match value {
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| {
                        // Keep an exclusion marker for the existing projection's
                        // redacted_fields count; never copy encrypted field content.
                        let public = if key == "encrypted_content" {
                            Value::Null
                        } else {
                            public_shape(value)
                        };
                        (key.clone(), public)
                    })
                    .collect(),
            ),
            Value::Array(values) => values.iter().map(public_shape).collect(),
            other => other.clone(),
        }
    }
    crate::scheduler::redact_native_error(&public_shape(value))
}

fn input_impact(kind: &str, payload: &Value) -> Impact {
    if kind == "model.streaming" {
        match string(payload, "kind") {
            Some("reasoning_delta" | "reasoning_start" | "reasoning_end") => {
                Impact::Known(vec![CoverageDimension::PublicReasoning])
            }
            Some("text_delta" | "text_start" | "text_end") => {
                Impact::Known(vec![CoverageDimension::Text])
            }
            Some("tool_call" | "tool_input_start" | "tool_input_delta" | "tool_input_end") => {
                Impact::Known(vec![CoverageDimension::Tools])
            }
            _ => Impact::Unknown,
        }
    } else {
        impact(kind)
    }
}

// Remaining generic session.updated variants emitted by the pinned mapper.
// New native subtypes remain Unknown; this is not a prefix catch-all.
fn known_session_update(kind: &str) -> bool {
    matches!(
        kind,
        "session_forked"
            | "session_compacted"
            | "session_mode_changed"
            | "turn_input_received"
            | "turn_steer_delivery_changed"
            | "turn_steer_dispatch_changed"
            | "turn_steer_rejected"
            | "turn_steer_discarded"
            | "session_input_promoted"
            | "turn_steer_reordered"
            | "queue_auto_drain_changed"
            | "followup_mode_changed"
            | "assistant_feedback_updated"
            | "highspeed_metrics_updated"
            | "model_selected"
            | "turn_execution_model_fallback"
            | "model_anomaly_warning"
            | "network_request_status"
            | "model_complete"
            | "model_error"
            | "background_task_started"
            | "background_task_updated"
            | "background_task_completed"
            | "plugin_ui_resource_updated"
            | "plugin_ui_resource_list_changed"
            | "plugin_ui_instance_closed"
            | "plugin_ui_app_tool_call_requested"
            | "user_input_auto_resolution_updated"
            | "workspace_hook_review_requested"
            | "workspace_hook_review_settled"
            | "workspace_hook_review_superseded"
            | "workspace_hook_admission_updated"
            | "workspace_mcp_admission_updated"
            | "hook_run_started"
            | "hook_run_progress"
            | "hook_run_completed"
            | "hook_run_failed"
            | "hook_run_blocked"
            | "compact_started"
            | "compact_completed"
            | "compact_failed"
            | "compact_boundary"
            | "microcompact_boundary"
            | "target_changed"
            | "target_completion_verification"
            | "subagent_spawned"
            | "subagent_message"
            | "subagent_stopped"
            | "interrupt"
            | "cancel"
            | "resume"
            | "error"
    )
}

#[cfg(test)]
#[path = "event_tests.rs"]
mod tests;
