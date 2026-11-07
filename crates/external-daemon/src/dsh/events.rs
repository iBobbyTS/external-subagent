//! The sole interpreter of DSH ACP notifications and prompt settlements.
//!
//! `session/update` frames become typed facts through the shared migration
//! ingress; no canonical `session/event` payload is reconstructed here. Tool
//! names come from the ACP `title` (a category is never a name), execution
//! phases come from the native `status`, and the prompt settlement is the only
//! authority that promotes aggregated assistant text into a turn result.

use std::{
    collections::HashMap,
    sync::{atomic::AtomicU64, Arc, Mutex},
};

use external_agent_dsh::acp::{
    permission::{self, OfferCache},
    result::{fold_settlement, MessageAggregation, SettlementOutcome},
    transport, update,
};
use external_contract::{
    events::*, RequestEnvelope, WireId, WireMessage, INTERACTION_REQUEST_PERMISSION,
    INTERACTION_REQUEST_UNSUPPORTED_INPUT, INTERACTION_REQUEST_USER_INPUT,
};
use external_runtime::Inbound;
use serde_json::Value;

use crate::{Publisher, TurnTracker};

const MAX_TRACKED_TOOLS: usize = 128;

/// One prompt settlement parsed off the wire, waiting to be folded.
///
/// The settlement watcher deposits its result here instead of applying it:
/// the shared pump is the only folder, and only after it has proven it
/// projected every inbound frame that preceded the settlement response on
/// the wire (the driver resolves a prompt response while frames sent before
/// it may still be queued for the pump, so folding from the watcher races
/// those frames and can settle a turn without its verified final message).
pub(super) enum SettledTurn {
    /// The prompt response settled the turn.
    Settled(transport::PromptSettlement),
    /// The response never arrived or violated the settlement shape. `reason`
    /// is the bounded ES label for this layer's operation; `native` is the
    /// peer's JSON-RPC error exactly as received and travels through the
    /// shared typed `NativeFailure` carrier instead of being stringified.
    Failed {
        reason: String,
        native: Option<serde_json::Value>,
    },
}

/// The ES-side label for a prompt that failed with a native JSON-RPC error.
/// The original error is delivered whole beside it; this token never replaces
/// it, and `requestId`/`code` that upstream no longer sends are not invented.
pub(super) const REMOTE_ERROR_REASON: &str = "ACP_REMOTE_ERROR";

pub(super) struct DshRuntimeShared {
    pub(super) publisher: Arc<Publisher>,
    pub(super) turn_tracker: Arc<TurnTracker>,
    pub(super) task: TaskId,
    pub(super) launch: LaunchId,
    pub(super) offers: Mutex<OfferCache>,
    pub(super) tool_names: Mutex<HashMap<String, String>>,
    pub(super) messages: Mutex<MessageAggregation>,
    pub(super) session_id: Mutex<Option<String>>,
    /// The locally reserved prompt turn currently accepting native updates.
    pub(super) current_prompt: Mutex<Option<u64>>,
    /// The last prompt identity this session reserved. Frames that arrive
    /// after that prompt settled belong to no open turn and are late echoes.
    pub(super) last_prompt: Mutex<Option<u64>>,
    pub(super) settled_turn: Mutex<Option<SettledTurn>>,
    pub(super) stop_boundaries: AtomicU64,
}

/// The aggregation key of one local prompt turn. Native DSH ACP has no turn
/// identity; the prompt is the only output boundary the standard flow offers.
fn prompt_key(prompt: u64) -> String {
    format!("dsh-prompt-{prompt}")
}

fn message_id(native: Option<&str>) -> MessageId {
    MessageId(native.unwrap_or("assistant-stream").to_owned())
}

impl DshRuntimeShared {
    pub(super) fn deposit_settled_turn(&self, settled: SettledTurn) {
        *self.settled_turn.lock().unwrap() = Some(settled);
    }

    pub(super) fn take_settled_turn(&self) -> Option<SettledTurn> {
        self.settled_turn.lock().unwrap().take()
    }

    pub(super) fn apply_settled_turn(&self, settled: SettledTurn) {
        match settled {
            SettledTurn::Settled(settlement) => self.apply_settlement(&settlement),
            SettledTurn::Failed { reason, native } => {
                self.apply_failed_settlement(&reason, native.as_ref())
            }
        }
    }

    /// Open the local prompt turn before any of its bytes reach the wire. The
    /// aggregation bucket and the shared tracker both open here; the caller
    /// sends only after return, so every frame of the turn is attributable.
    pub(super) fn begin_turn(&self, prompt_id: u64) {
        let turn = TurnId::Local(prompt_id);
        let mut current = self.current_prompt.lock().unwrap();
        self.messages
            .lock()
            .unwrap()
            .begin_turn(&prompt_key(prompt_id));
        *current = Some(prompt_id);
        *self.last_prompt.lock().unwrap() = Some(prompt_id);
        let input = self.input_context(
            Some(transport::SESSION_PROMPT),
            Some(turn.clone()),
            self.session_id.lock().unwrap().as_deref(),
            None,
        );
        drop(current);
        self.publisher.emit_typed(
            Self::consumed(input, vec![EventFact::TurnStarted { turn_id: turn }]),
            &self.turn_tracker,
        );
    }

    fn open_prompt(&self) -> Option<u64> {
        self.current_prompt
            .lock()
            .unwrap()
            .or_else(|| *self.last_prompt.lock().unwrap())
    }

    /// Apply one prompt settlement as the single turn-boundary authority.
    fn apply_settlement(&self, settlement: &transport::PromptSettlement) {
        let Some(prompt) = self.open_prompt() else {
            // A settlement only exists for a reserved prompt; this is
            // defensive against a transient caller race, never a turn claim.
            return;
        };
        let key = prompt_key(prompt);
        let outcome = {
            let messages = self.messages.lock().unwrap();
            fold_settlement(settlement, &messages, &key)
        };
        // The prompt is settled: whatever arrives afterwards cannot belong to
        // it, so the prompt's text bucket closes before the boundary lands.
        self.messages.lock().unwrap().seal_turn(&key);
        *self.current_prompt.lock().unwrap() = None;
        let stop_reason = SafeToken::new(settlement.stop_reason.as_str());
        match outcome {
            SettlementOutcome::Completed { final_text } => self.apply_boundary(
                prompt,
                TurnOutcome::Completed,
                stop_reason,
                final_text,
                None,
            ),
            SettlementOutcome::Failed { reason_code } => {
                // The peer settled without a success verdict. The native stop
                // reason is the failure signal; ES's classification carries
                // the bounded reason the result already delivered.
                let failure = NativeFailure {
                    original: Value::String(reason_code.clone()),
                    projection: None,
                };
                self.apply_boundary(
                    prompt,
                    TurnOutcome::Failed,
                    // The native stop reason is the settlement's own signal;
                    // the bounded ES classification is the safe fallback.
                    SafeToken::new(settlement.stop_reason.as_str())
                        .or_else(|| SafeToken::new(reason_code.as_str())),
                    None,
                    Some(failure),
                );
            }
        }
    }

    /// Apply a settlement that never produced a usable prompt response.
    ///
    /// `reason` is this layer's bounded label; `native` is the complete
    /// JSON-RPC error when the peer answered with one.
    pub(super) fn apply_failed_settlement(&self, reason: &str, native: Option<&Value>) {
        let Some(prompt) = self.open_prompt() else {
            return;
        };
        let key = prompt_key(prompt);
        self.messages.lock().unwrap().seal_turn(&key);
        *self.current_prompt.lock().unwrap() = None;
        let failure = match native {
            // The peer's own error travels whole; credential redaction is the
            // adapter's responsibility and never summarizes the original.
            Some(native) => NativeFailure {
                original: crate::scheduler::redact_native_error(native),
                projection: None,
            },
            None => NativeFailure {
                original: Value::String(reason.chars().take(256).collect()),
                projection: None,
            },
        };
        self.apply_boundary(
            prompt,
            TurnOutcome::Failed,
            SafeToken::new(reason),
            None,
            Some(failure),
        );
    }

    /// Emit authoritative turn text on the settlement itself. The publisher
    /// delivers the complete batch to the sink before notifying the tracker;
    /// a turn aggregate must never be replayed as one native message snapshot.
    fn apply_boundary(
        &self,
        prompt: u64,
        outcome: TurnOutcome,
        stop_reason: Option<SafeToken>,
        final_text: Option<String>,
        failure: Option<NativeFailure>,
    ) {
        let turn = TurnId::Local(prompt);
        let input = self.input_context(
            Some(transport::SESSION_PROMPT),
            Some(turn.clone()),
            self.session_id.lock().unwrap().as_deref(),
            None,
        );
        let mut facts = Vec::new();
        if let Some(failure) = &failure {
            facts.push(EventFact::ErrorObserved {
                operation: Operation::Turn,
                failure: failure.clone(),
            });
        }
        facts.push(EventFact::TurnSettled(TurnSettlement {
            turn_id: turn,
            outcome,
            stop_reason,
            final_text,
            failure,
        }));
        self.publisher
            .emit_typed(Self::consumed(input, facts), &self.turn_tracker);
    }

    fn input_context(
        &self,
        discriminator: Option<&str>,
        turn: Option<TurnId>,
        session: Option<&str>,
        call: Option<&str>,
    ) -> NativeInputContext {
        NativeInputContext {
            task_id: self.task.clone(),
            launch_id: self.launch.clone(),
            turn_id: turn,
            source: NativeSource {
                protocol: SourceProtocol::DshAcpV1,
                discriminator: discriminator.and_then(SafeToken::new),
                session_id: session.map(|id| SessionId(id.to_owned())),
                // Native DSH ACP exposes no turn or frame identity: the local
                // prompt owns the turn, and no content hash may stand in for
                // a replay identity.
                turn_id: None,
                item_id: None,
                call_id: call.map(str::to_owned),
                event_id: None,
                timestamp: None,
            },
        }
    }

    /// Decode one `session/update` notification payload into typed facts.
    ///
    /// A frame for a foreign session or one that arrived after its prompt
    /// settled is attributed, never consumed. A known frame whose shape is
    /// unusable retains an explicit coverage gap; an unrecognized native
    /// discriminator keeps a safe diagnostic instead of disappearing.
    pub(super) fn decode_session_update(&self, params: &Value) -> Option<DecodedInput> {
        let expected = self.session_id.lock().unwrap().clone();
        let update_type = update_type(params);
        let parsed_session = params
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 512 && !id.contains('\0'));
        if let (Some(expected), Some(actual)) = (expected.as_deref(), parsed_session) {
            if expected != actual {
                let input =
                    self.input_context(update_type, self.turn_scope(), parsed_session, None);
                return Some(Self::ignored(
                    input,
                    Attribution::ForeignSession,
                    IgnoreReason::ForeignSession,
                ));
            }
        }
        let prompt = self.current_prompt.lock().unwrap().clone();
        if prompt.is_none() && self.last_prompt.lock().unwrap().is_some() {
            // The prompt is settled; this frame is a late echo of a closed
            // turn and must not touch the current result. Its own shape is
            // irrelevant: a late damaged frame is still only late.
            let input = self.input_context(update_type, None, parsed_session, None);
            return Some(Self::ignored(input, Attribution::Late, IgnoreReason::Late));
        }
        let Some(parsed) = update::parse_update(params) else {
            let input =
                self.input_context(update_type, prompt.map(TurnId::Local), parsed_session, None);
            return Some(self.invalid(input, LossReason::Malformed, update_impact(update_type)));
        };
        let call = match &parsed.kind {
            update::UpdateKind::ToolCall { tool_call_id, .. }
            | update::UpdateKind::ToolCallUpdate { tool_call_id, .. } => {
                Some(tool_call_id.as_str())
            }
            _ => None,
        };
        let input = self.input_context(
            update_type,
            prompt.map(TurnId::Local),
            parsed.session_id.as_deref(),
            call,
        );
        Some(match self.decode_kind(params, &parsed, prompt) {
            Ok(Ok(facts)) => Self::consumed(input, facts),
            Ok(Err(reason)) => Self::ignored(input, Attribution::Current, reason),
            Err(true) => self.invalid(input, LossReason::Malformed, update_impact(update_type)),
            Err(false) => {
                let diagnostic = Diagnostic {
                    reason: DiagnosticReason::UnknownSemantics,
                    impact: update_impact(update_type),
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

    fn turn_scope(&self) -> Option<TurnId> {
        self.current_prompt.lock().unwrap().map(TurnId::Local)
    }

    fn decode_kind(
        &self,
        params: &Value,
        parsed: &update::SessionUpdate,
        prompt: Option<u64>,
    ) -> Parsed {
        match &parsed.kind {
            update::UpdateKind::AgentMessage {
                message_id: native_id,
                text,
                committed,
            } => {
                let id = message_id(native_id.as_deref());
                let mut facts = Vec::new();
                if !text.is_empty() {
                    if let Some(prompt) = prompt {
                        // The prompt bucket is the local correlation the
                        // standard chunk-only settlement reads.
                        self.messages.lock().unwrap().observe(
                            &prompt_key(prompt),
                            native_id.as_deref(),
                            text,
                        );
                    }
                    facts.push(EventFact::MessageUpdated(MessageUpdate {
                        message_id: id.clone(),
                        text: TextUpdate::Delta(text.clone()),
                    }));
                }
                if *committed {
                    facts.push(EventFact::MessageFinished { message_id: id });
                }
                if facts.is_empty() {
                    // A legal empty chunk contributes nothing to any consumer.
                    Ok(Err(IgnoreReason::NoConsumer))
                } else {
                    Ok(Ok(facts))
                }
            }
            update::UpdateKind::AgentThoughtChunk { text: Some(text) } if !text.is_empty() => {
                Ok(Ok(vec![EventFact::PublicReasoningDelta(PublicReasoning {
                    message_id: None,
                    part_id: None,
                    channel: PublicReasoningChannel::Thought,
                    delta: text.clone(),
                })]))
            }
            update::UpdateKind::AgentThoughtChunk { text: None } => {
                // The public thought could not be extracted: a malformed known
                // input, never a public reasoning delta.
                Err(true)
            }
            update::UpdateKind::AgentThoughtChunk { text: Some(_) } => {
                // A legal empty thought contributes nothing to any consumer.
                Ok(Err(IgnoreReason::StreamBoundary))
            }
            update::UpdateKind::ToolCall {
                tool_call_id,
                title: _,
                tool_kind,
                status,
            } => {
                // The name is the ACP `title`; `kind` is a category and never
                // becomes the name. An unnamed call keeps the stable fallback
                // the permission correlation and observation projections share.
                let name = update::tool_name(&parsed.kind).unwrap_or_else(|| "dsh_tool".into());
                {
                    let mut tools = self.tool_names.lock().unwrap();
                    if tools.len() < MAX_TRACKED_TOOLS || tools.contains_key(tool_call_id) {
                        tools.insert(tool_call_id.clone(), name.clone());
                    }
                }
                let phase = tool_call_phase(*status);
                let fact = EventFact::ToolChanged(ToolChange {
                    call_id: CallId(tool_call_id.clone()),
                    name: Some(name),
                    category: tool_kind.as_deref().and_then(SafeToken::new),
                    arguments: params
                        .pointer("/update/rawInput")
                        .map(|value| ArgumentUpdate::Snapshot(public_tool_input(value))),
                    phase: Some(phase),
                    outcome: tool_outcome(*status),
                    failure: None,
                    denied: None,
                });
                if terminal_status(*status) {
                    // Correlation belongs to pending/in-flight calls, not
                    // finished history: free the bounded slot after projecting.
                    self.tool_names.lock().unwrap().remove(tool_call_id);
                }
                Ok(Ok(vec![fact]))
            }
            update::UpdateKind::ToolCallUpdate {
                tool_call_id,
                status,
            } => {
                let Some(phase) = tool_update_phase(*status) else {
                    // No recognized status proves nothing about execution.
                    return Ok(Err(IgnoreReason::NoConsumer));
                };
                let name = self.tool_names.lock().unwrap().get(tool_call_id).cloned();
                let fact = EventFact::ToolChanged(ToolChange {
                    call_id: CallId(tool_call_id.clone()),
                    name,
                    category: None,
                    arguments: None,
                    phase: Some(phase),
                    outcome: tool_outcome(*status),
                    failure: None,
                    denied: None,
                });
                if terminal_status(*status) {
                    self.tool_names.lock().unwrap().remove(tool_call_id);
                }
                Ok(Ok(vec![fact]))
            }
            update::UpdateKind::Untracked => match update_type(params) {
                // Usage, plan and configuration dialects have no consumer
                // here; they are recognized normal traffic, not loss.
                Some("usage_update") => Ok(Err(IgnoreReason::Usage)),
                Some("plan") => Ok(Err(IgnoreReason::PlanOrDiff)),
                Some(
                    "available_commands_update"
                    | "current_mode_update"
                    | "config_option_update"
                    | "session_info_update",
                ) => Ok(Err(IgnoreReason::Configuration)),
                // A new native discriminator is uncertainty, not absence.
                Some(_) => Err(false),
                None => Err(true),
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

/// The discriminator of one `session/update` payload, as received. Both
/// observed dialects are accepted (upstream `sessionUpdate`, pinned `type`).
fn update_type(params: &Value) -> Option<&str> {
    let update = params.get("update")?;
    update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .or_else(|| update.get("type").and_then(Value::as_str))
}

/// The affected observation dimension of a known update family. Only a
/// recognizably text/tool/reasoning shape narrows the impact; anything else
/// remains explicitly unknown.
fn update_impact(update_type: Option<&str>) -> Impact {
    match update_type {
        Some("agent_message" | "agent_message_chunk") => {
            Impact::Known(vec![CoverageDimension::Text])
        }
        Some("agent_thought_chunk" | "agent_thought" | "thought") => {
            Impact::Known(vec![CoverageDimension::PublicReasoning])
        }
        Some("tool_call" | "tool_call_update") => Impact::Known(vec![CoverageDimension::Tools]),
        _ => Impact::Unknown,
    }
}

fn tool_call_phase(status: Option<update::ToolStatus>) -> ToolPhase {
    match status {
        Some(update::ToolStatus::Pending) => ToolPhase::Scheduled,
        Some(update::ToolStatus::Completed | update::ToolStatus::Failed) => ToolPhase::Finished,
        // An announcement without a status declares the call is being used.
        Some(update::ToolStatus::InProgress) | None => ToolPhase::Started,
    }
}

fn tool_update_phase(status: Option<update::ToolStatus>) -> Option<ToolPhase> {
    match status {
        Some(update::ToolStatus::Pending) => Some(ToolPhase::Scheduled),
        Some(update::ToolStatus::InProgress) => Some(ToolPhase::Started),
        Some(update::ToolStatus::Completed | update::ToolStatus::Failed) => {
            Some(ToolPhase::Finished)
        }
        // An unrecognized status proves nothing, so no phase is invented.
        None => None,
    }
}

fn tool_outcome(status: Option<update::ToolStatus>) -> Option<ToolOutcome> {
    match status {
        // A terminal native status is the explicit result evidence; a content
        // array alone never becomes success.
        Some(update::ToolStatus::Completed) => Some(ToolOutcome::Succeeded),
        Some(update::ToolStatus::Failed) => Some(ToolOutcome::Failed),
        Some(update::ToolStatus::Pending | update::ToolStatus::InProgress) | None => None,
    }
}

fn terminal_status(status: Option<update::ToolStatus>) -> bool {
    matches!(
        status,
        Some(update::ToolStatus::Completed | update::ToolStatus::Failed)
    )
}

/// Public tool input: encrypted protocol fields keep an exclusion marker and
/// credential-shaped keys are redacted before the fact leaves the adapter.
fn public_tool_input(value: &Value) -> Value {
    fn public_shape(value: &Value) -> Value {
        match value {
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| {
                        // Keep an exclusion marker for the observation's
                        // redacted-field count; never copy encrypted content.
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

type Parsed = Result<Result<Vec<EventFact>, IgnoreReason>, bool>; // true = malformed known input

/// Normalize one inbound ACP frame. `session/update` is consumed into typed
/// facts; requests keep their existing correlation/answerability projection.
/// `None` means the frame was fully consumed by this layer.
pub(super) fn project_dsh_inbound(shared: &DshRuntimeShared, event: &Inbound) -> Option<Inbound> {
    let Inbound::Message(message) = event else {
        return Some(event.clone());
    };
    match message {
        WireMessage::UnknownEvent { method, raw } if method == transport::SESSION_UPDATE => {
            let params = raw.get("params").cloned().unwrap_or(Value::Null);
            if let Some(batch) = shared.decode_session_update(&params) {
                shared.publisher.emit_typed(batch, &shared.turn_tracker);
            }
            None
        }
        WireMessage::Request(request) => Some(Inbound::Message(WireMessage::Request(
            project_request(shared, request),
        ))),
        _ => Some(event.clone()),
    }
}

fn project_request(shared: &DshRuntimeShared, request: &RequestEnvelope) -> RequestEnvelope {
    if request.method == transport::SESSION_REQUEST_PERMISSION {
        match permission::parse_offer(&request.params) {
            Some(offer) => {
                let correlated = offer
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| shared.tool_names.lock().unwrap().get(id).cloned());
                let normalized =
                    permission::normalize_permission_request(&offer, correlated.as_deref());
                shared
                    .offers
                    .lock()
                    .unwrap()
                    .observe(permission::correlation_key(&request.id), offer);
                RequestEnvelope::new(
                    request.id.clone(),
                    INTERACTION_REQUEST_PERMISSION,
                    normalized,
                )
            }
            None => unsupported_input_request(
                request.id.clone(),
                "dsh permission request carried no usable offer",
            ),
        }
    } else if request.method == transport::SESSION_REQUEST_INPUT {
        // The only real answerable producer on the DSH path: the question is
        // preserved verbatim and the answer travels back as the plain
        // JSON-RPC result for this request id.
        match transport::request_input_question(&request.params) {
            Some(question) => RequestEnvelope::new(
                request.id.clone(),
                INTERACTION_REQUEST_USER_INPUT,
                serde_json::json!({ "question": question }),
            ),
            None => unsupported_input_request(
                request.id.clone(),
                "dsh input request carried no usable question",
            ),
        }
    } else {
        unsupported_input_request(
            request.id.clone(),
            "dsh server request is not a supported interaction",
        )
    }
}

fn unsupported_input_request(id: WireId, reason: &str) -> RequestEnvelope {
    RequestEnvelope::new(
        id,
        INTERACTION_REQUEST_UNSUPPORTED_INPUT,
        serde_json::json!({"origin": "dsh_acp", "reason": reason}),
    )
}
