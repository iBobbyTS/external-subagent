//! The sole interpreter of Codex app-server notifications.
//!
//! Attribution against the live thread and turn, retirement of completed
//! turns, and application of the pure notification folding from
//! [`external_agent_codex::update`] all live here. The adapter emits typed
//! facts through the shared migration ingress; no canonical `session/event`
//! payload is reconstructed.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use external_agent_codex::session::{sandbox_confirmed, CodexPermissionMode};
use external_agent_codex::update;
use external_contract::events::*;

use crate::{Publisher, RuntimeLoss, RuntimeTerminal, TurnTracker};

pub(super) struct CodexShared {
    pub(super) publisher: Arc<Publisher>,
    pub(super) turn_tracker: Arc<TurnTracker>,
    pub(super) task: TaskId,
    pub(super) launch: LaunchId,
    pub(super) session_id: Mutex<Option<String>>,
    pub(super) admitted_model: Mutex<Option<String>>,
    pub(super) admitted_effort: Mutex<Option<String>>,
    /// The admitted permission posture, set before the first wire request so
    /// every `thread/settings/updated` frame is checked against it.
    pub(super) admitted_posture: Mutex<Option<CodexPermissionMode>>,
    pub(super) diagnostic_session_id: Mutex<Option<String>>,
    pub(super) current_turn: Mutex<Option<String>>,
    pub(super) retired_turns: Mutex<Vec<String>>,
    pub(super) start_in_flight: AtomicBool,
    pub(super) last_message_item: Mutex<Option<String>>,
    pub(super) items: Mutex<HashMap<String, String>>,
    /// First-seen tool item ids for the current turn. A repeated `started`
    /// must not become a second call; a completion is the authoritative
    /// snapshot and always projects, including when its start was missed.
    /// Cleared at every turn boundary (a new `turn/started` and retirement).
    pub(super) seen_tool_items: Mutex<Vec<String>>,
    /// The current turn's native error notification, retained complete and
    /// credential-redacted: the boundary projection needs the original object,
    /// not a summary of it.
    pub(super) turn_failure: Mutex<Option<serde_json::Value>>,
    pub(super) mcp_tail: Mutex<String>,
    pub(super) stop_boundaries: AtomicU64,
}

impl CodexShared {
    fn observe_item_text(&self, item_id: &str, text: &str) {
        update::observe_item_text(&mut self.items.lock().unwrap(), item_id, text);
    }

    fn item_text(&self, item_id: &str) -> Option<String> {
        self.items.lock().unwrap().get(item_id).cloned()
    }

    fn record_turn_failure(&self, native: serde_json::Value) {
        *self.turn_failure.lock().unwrap() = Some(native);
    }

    fn record_mcp_failure(&self, name: &str, error: &str) {
        update::record_mcp_failure(&mut self.mcp_tail.lock().unwrap(), name, error);
    }

    /// The frame's declared turn id pins turn-scoped traffic to the turn
    /// this runtime is currently projecting: the id must be present and
    /// match the current turn. Anything else — a missing id, or a late
    /// frame from an older turn — is unattributable and must not touch the
    /// current result.
    fn attributable_turn(&self, frame_turn_id: Option<&str>) -> Option<TurnId> {
        let current = self.current_turn.lock().unwrap().clone();
        match (current, frame_turn_id) {
            (Some(current), Some(arrived)) if current == arrived => Some(TurnId::Native(current)),
            _ => None,
        }
    }

    /// A turn that reached its terminal boundary is retired: its identity
    /// can never become current again. A late or duplicate `turn/started`
    /// for a retired turn is a recognized replay and never reopens the
    /// closed boundary.
    fn retire_turn(&self, turn_id: &str) {
        update::retire_turn(&mut self.retired_turns.lock().unwrap(), turn_id);
        *self.current_turn.lock().unwrap() = None;
        // The tool-item memory is turn-scoped: a retired turn's dedupe
        // memory must never suppress the next turn's first sighting.
        self.seen_tool_items.lock().unwrap().clear();
    }

    fn turn_is_retired(&self, turn_id: &str) -> bool {
        self.retired_turns
            .lock()
            .unwrap()
            .iter()
            .any(|id| id == turn_id)
    }

    fn context(
        &self,
        method: &str,
        params: &serde_json::Value,
        turn: Option<&TurnId>,
    ) -> NativeInputContext {
        let session = self.session_id.lock().unwrap().clone();
        let frame_turn = params
            .get("turnId")
            .and_then(|value| value.as_str())
            .or_else(|| params.pointer("/turn/id").and_then(|value| value.as_str()));
        NativeInputContext {
            task_id: self.task.clone(),
            launch_id: self.launch.clone(),
            turn_id: turn
                .cloned()
                .or_else(|| frame_turn.map(|id| TurnId::Native(id.to_owned()))),
            source: NativeSource {
                protocol: SourceProtocol::CodexAppServer,
                discriminator: SafeToken::new(method),
                session_id: session.map(SessionId),
                turn_id: frame_turn.map(str::to_owned),
                item_id: params
                    .get("itemId")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
                    .or_else(|| {
                        params
                            .pointer("/item/id")
                            .and_then(|value| value.as_str())
                            .map(str::to_owned)
                    }),
                call_id: params
                    .pointer("/item/id")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned),
                // Codex notifications carry no native event identity; turn and
                // item attribution carry replay/lateness decisions instead.
                event_id: None,
                timestamp: None,
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

    /// A known notification with no public fact consumer. A frame pinned to a
    /// different or retired turn stays late instead of touching the current
    /// turn's diagnostic scope.
    fn ignored_current(
        &self,
        method: &str,
        params: &serde_json::Value,
        reason: IgnoreReason,
    ) -> DecodedInput {
        let input = self.context(method, params, None);
        let frame_turn = params.get("turnId").and_then(|value| value.as_str());
        if update::is_turn_scoped(method) {
            if self.attributable_turn(frame_turn).is_none() {
                return Self::ignored(input, Attribution::Late, IgnoreReason::Late);
            }
        } else if let Some(frame_turn) = frame_turn {
            let current = self.current_turn.lock().unwrap().clone();
            if current.as_deref() != Some(frame_turn) {
                return Self::ignored(input, Attribution::Late, IgnoreReason::Late);
            }
        }
        Self::ignored(input, Attribution::Current, reason)
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

    /// One inbound Codex notification, decoded into typed facts.
    ///
    /// `None` means the frame is not an interpreted notification; the pump
    /// keeps its existing driver path for it. Every recognized notification is
    /// consumed here, including explicitly ignored ones, so no native payload
    /// is reconstructed as an event.
    pub(super) fn decode_notification(
        &self,
        method: &str,
        raw: &serde_json::Value,
    ) -> Option<DecodedInput> {
        let params = raw
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let session = self.session_id.lock().unwrap().clone();
        let thread_id = params.get("threadId").and_then(|value| value.as_str());
        if update::is_turn_scoped(method) {
            match (session.as_deref(), thread_id) {
                (Some(expected), Some(actual)) if expected == actual => {}
                (Some(_), Some(_)) => {
                    let input = self.context(method, &params, None);
                    return Some(Self::ignored(
                        input,
                        Attribution::ForeignSession,
                        IgnoreReason::ForeignSession,
                    ));
                }
                _ => {
                    // Without a known thread, or without the frame declaring
                    // which thread it belongs to, attribution is impossible.
                    let input = self.context(method, &params, None);
                    return Some(Self::ignored(input, Attribution::Late, IgnoreReason::Late));
                }
            }
        } else if let (Some(expected), Some(actual)) = (session.as_deref(), thread_id) {
            if expected != actual {
                // Foreign-thread traffic is never projected onto this task.
                let input = self.context(method, &params, None);
                return Some(Self::ignored(
                    input,
                    Attribution::ForeignSession,
                    IgnoreReason::ForeignSession,
                ));
            }
        }
        match method {
            "turn/started" => Some(self.on_turn_started(&params)),
            "item/started" => Some(self.on_item(&params, false)),
            "item/completed" => Some(self.on_item(&params, true)),
            "item/agentMessage/delta" => Some(self.on_agent_delta(&params)),
            "item/reasoning/summaryTextDelta" => Some(self.on_summary_delta(&params)),
            "item/reasoning/summaryPartAdded" => {
                Some(self.ignored_current(method, &params, IgnoreReason::StreamBoundary))
            }
            // Raw reasoning is deliberately not ingested; only the public
            // summary stream above becomes a fact.
            "item/reasoning/textDelta" => {
                Some(self.ignored_current(method, &params, IgnoreReason::NoConsumer))
            }
            "turn/completed" => Some(self.on_turn_completed(&params)),
            "error" => Some(self.on_error(&params)),
            "mcpServer/startupStatus/updated" => {
                if let Some((name, error)) = update::mcp_startup_failure(&params) {
                    self.record_mcp_failure(name, error);
                }
                Some(self.ignored_current(method, &params, IgnoreReason::NoConsumer))
            }
            "thread/settings/updated" => Some(self.on_thread_settings(&params, thread_id)),
            other => Some(self.unknown(other, &params)),
        }
    }

    fn on_turn_started(&self, params: &serde_json::Value) -> DecodedInput {
        let Some(turn_id) = update::started_turn_id(params) else {
            let input = self.context("turn/started", params, None);
            return self.invalid(
                input,
                LossReason::Malformed,
                Impact::Known(vec![CoverageDimension::Turn]),
            );
        };
        let turn = TurnId::Native(turn_id.to_owned());
        let input = self.context("turn/started", params, Some(&turn));
        if self.turn_tracker.snapshot().active {
            // A started while a turn is already active is a late replay or
            // duplicate: it must never re-open the active boundary or reset
            // this turn's recorded state.
            return Self::ignored(input, Attribution::Replay, IgnoreReason::Replay);
        }
        if self.turn_is_retired(turn_id) {
            // A completed or failed turn stays closed: a late or duplicate
            // started for it must not reactivate the old identity after its
            // boundary was projected.
            return Self::ignored(input, Attribution::Replay, IgnoreReason::Replay);
        }
        if !self.start_in_flight.load(Ordering::Acquire) {
            // A started for an unknown turn with no start request in flight is
            // unsolicited replay traffic; it must not reopen a boundary either.
            return Self::ignored(input, Attribution::Replay, IgnoreReason::Replay);
        }
        *self.current_turn.lock().unwrap() = Some(turn_id.to_owned());
        *self.last_message_item.lock().unwrap() = None;
        self.seen_tool_items.lock().unwrap().clear();
        // A stale failure detail from an earlier turn must never label this
        // turn's boundary.
        *self.turn_failure.lock().unwrap() = None;
        Self::consumed(input, vec![EventFact::TurnStarted { turn_id: turn }])
    }

    fn on_agent_delta(&self, params: &serde_json::Value) -> DecodedInput {
        let frame_turn = params.get("turnId").and_then(|value| value.as_str());
        let Some(turn) = self.attributable_turn(frame_turn) else {
            // A delta that cannot be pinned to the current turn must not
            // stream into this task's result.
            let input = self.context("item/agentMessage/delta", params, None);
            return Self::ignored(input, Attribution::Late, IgnoreReason::Late);
        };
        let input = self.context("item/agentMessage/delta", params, Some(&turn));
        let Some((delta, item_id)) = update::agent_message_delta(params) else {
            return self.invalid(
                input,
                LossReason::Malformed,
                Impact::Known(vec![CoverageDimension::Text]),
            );
        };
        self.observe_item_text(item_id, delta);
        Self::consumed(
            input,
            vec![EventFact::MessageUpdated(MessageUpdate {
                message_id: MessageId(item_id.to_owned()),
                text: TextUpdate::Delta(delta.to_owned()),
            })],
        )
    }

    fn on_item(&self, params: &serde_json::Value, completed: bool) -> DecodedInput {
        let method = if completed {
            "item/completed"
        } else {
            "item/started"
        };
        let item = params.get("item").unwrap_or(&serde_json::Value::Null);
        let frame_turn = params.get("turnId").and_then(|value| value.as_str());
        let item_type = item.get("type").and_then(|value| value.as_str());

        if completed && item_type == Some("agentMessage") {
            let Some(turn) = self.attributable_turn(frame_turn) else {
                let input = self.context(method, params, None);
                return Self::ignored(input, Attribution::Late, IgnoreReason::Late);
            };
            let input = self.context(method, params, Some(&turn));
            let Some((item_id, text)) = update::completed_agent_message(params) else {
                return self.invalid(
                    input,
                    LossReason::Malformed,
                    Impact::Known(vec![CoverageDimension::Text]),
                );
            };
            if let Some(text) = text {
                update::record_completed_item(&mut self.items.lock().unwrap(), item_id, text);
            }
            *self.last_message_item.lock().unwrap() = Some(item_id.to_owned());
            return Self::consumed(
                input,
                vec![EventFact::MessageFinished {
                    message_id: MessageId(item_id.to_owned()),
                }],
            );
        }

        if let Some(tool) = update::tool_item(params) {
            let Some(turn) = self.attributable_turn(frame_turn) else {
                let input = self.context(method, params, None);
                return Self::ignored(input, Attribution::Late, IgnoreReason::Late);
            };
            let input = self.context(method, params, Some(&turn));
            if !completed
                && !update::observe_tool_item(&mut self.seen_tool_items.lock().unwrap(), tool.id)
            {
                // A repeated start is a replay of the same call identity.
                return Self::ignored(input, Attribution::Replay, IgnoreReason::Replay);
            }
            if completed {
                // A completion is the authoritative snapshot; record it even
                // when its start was missed so a late start cannot re-count it.
                update::observe_tool_item(&mut self.seen_tool_items.lock().unwrap(), tool.id);
            }
            let phase = if completed {
                if tool.status == Some("inProgress") {
                    ToolPhase::Progress
                } else {
                    ToolPhase::Finished
                }
            } else {
                ToolPhase::Started
            };
            let outcome = (phase == ToolPhase::Finished).then(|| tool_outcome(tool.status));
            let denied = (tool.status == Some("declined")).then_some(true);
            let failure = tool.error.map(|original| NativeFailure {
                original: crate::scheduler::redact_native_error(&original),
                projection: None,
            });
            return Self::consumed(
                input,
                vec![EventFact::ToolChanged(ToolChange {
                    call_id: CallId(tool.id.to_owned()),
                    name: Some(tool.item_type.to_owned()),
                    category: SafeToken::new(tool.item_type),
                    arguments: tool.arguments.map(ArgumentUpdate::Snapshot),
                    phase: Some(phase),
                    outcome,
                    failure,
                    denied,
                })],
            );
        }

        let input = self.context(method, params, None);
        if item_type.is_some_and(update::is_tool_item) {
            let Some(turn) = self.attributable_turn(frame_turn) else {
                return Self::ignored(input, Attribution::Late, IgnoreReason::Late);
            };
            let input = self.context(method, params, Some(&turn));
            // A whitelisted tool without a usable identity can never be
            // attributed to a call.
            return self.invalid(
                input,
                LossReason::Malformed,
                Impact::Known(vec![CoverageDimension::Tools]),
            );
        }
        if item_type.is_none() {
            return self.invalid(input, LossReason::Malformed, Impact::Unknown);
        }
        self.ignored_current(method, params, IgnoreReason::NoConsumer)
    }

    fn on_summary_delta(&self, params: &serde_json::Value) -> DecodedInput {
        let frame_turn = params.get("turnId").and_then(|value| value.as_str());
        let Some(turn) = self.attributable_turn(frame_turn) else {
            let input = self.context("item/reasoning/summaryTextDelta", params, None);
            return Self::ignored(input, Attribution::Late, IgnoreReason::Late);
        };
        let input = self.context("item/reasoning/summaryTextDelta", params, Some(&turn));
        let Some((delta, item_id, part)) = update::summary_delta(params) else {
            return self.invalid(
                input,
                LossReason::Malformed,
                Impact::Known(vec![CoverageDimension::PublicReasoning]),
            );
        };
        Self::consumed(
            input,
            vec![EventFact::PublicReasoningDelta(PublicReasoning {
                message_id: item_id.map(|id| MessageId(id.to_owned())),
                part_id: part.map(|part| part.to_string()),
                channel: PublicReasoningChannel::Summary,
                delta: delta.to_owned(),
            })],
        )
    }

    fn on_turn_completed(&self, params: &serde_json::Value) -> DecodedInput {
        let completed = update::completed_turn(params);
        let Some(turn_id) = completed.turn_id else {
            let input = self.context("turn/completed", params, None);
            return self.invalid(
                input,
                LossReason::Malformed,
                Impact::Known(vec![CoverageDimension::Turn]),
            );
        };
        let Some(turn) = self.attributable_turn(Some(turn_id)) else {
            // A boundary without the current turn's identity never settles
            // this task's active turn.
            let input = self.context("turn/completed", params, None);
            return Self::ignored(input, Attribution::Late, IgnoreReason::Late);
        };
        let input = self.context("turn/completed", params, Some(&turn));
        match completed.status {
            Some("completed") => {
                let tracked = self
                    .last_message_item
                    .lock()
                    .unwrap()
                    .clone()
                    .and_then(|item_id| self.item_text(&item_id));
                let final_text = update::final_text(completed.turn, tracked);
                self.retire_turn(turn_id);
                Self::consumed(
                    input,
                    vec![EventFact::TurnSettled(TurnSettlement {
                        turn_id: turn,
                        outcome: TurnOutcome::Completed,
                        stop_reason: SafeToken::new("completed"),
                        final_text,
                        failure: None,
                    })],
                )
            }
            Some(status @ ("failed" | "interrupted")) => {
                let recorded = self.turn_failure.lock().unwrap().clone();
                let (_, native) = update::turn_failure(completed.turn, status, recorded);
                let failure = native.map(|original| NativeFailure {
                    original: crate::scheduler::redact_native_error(&original),
                    projection: None,
                });
                if let Some(failure) = &failure {
                    // The boundary's own complete native cause is retained for
                    // the diagnostic fallback and the next turn's isolation.
                    self.record_turn_failure(failure.original.clone());
                }
                self.retire_turn(turn_id);
                Self::consumed(
                    input,
                    vec![EventFact::TurnSettled(TurnSettlement {
                        turn_id: turn,
                        // An interrupted turn keeps the existing failed-boundary
                        // semantics; the native status stays in `stop_reason`.
                        outcome: TurnOutcome::Failed,
                        stop_reason: SafeToken::new(status),
                        final_text: None,
                        failure,
                    })],
                )
            }
            _ => self.invalid(
                input,
                LossReason::Malformed,
                Impact::Known(vec![CoverageDimension::Turn]),
            ),
        }
    }

    fn on_error(&self, params: &serde_json::Value) -> DecodedInput {
        let frame_turn = params.get("turnId").and_then(|value| value.as_str());
        let Some(turn) = self.attributable_turn(frame_turn) else {
            // Only an error attributable to the current turn may label this
            // task's failure reason.
            let input = self.context("error", params, None);
            return Self::ignored(input, Attribution::Late, IgnoreReason::Late);
        };
        let input = self.context("error", params, Some(&turn));
        let Some(native) = update::turn_error_native(params) else {
            return self.ignored_current("error", params, IgnoreReason::NoConsumer);
        };
        let original = crate::scheduler::redact_native_error(&native);
        // A turn-scoped application error never completes a task by itself;
        // the complete native cause is retained for the boundary projection
        // and for an exit before that boundary.
        self.record_turn_failure(original.clone());
        Self::consumed(
            input,
            vec![EventFact::ErrorObserved {
                operation: Operation::Turn,
                failure: NativeFailure {
                    original,
                    projection: None,
                },
            }],
        )
    }

    fn on_thread_settings(
        &self,
        params: &serde_json::Value,
        thread_id: Option<&str>,
    ) -> DecodedInput {
        // A settings frame naming a sandbox that contradicts the admitted
        // posture is a security invariant break (observed live as the
        // sandbox-less-resume downgrade to workspace-write with no network):
        // fail the runtime fast instead of letting turns run with silently
        // reduced capability. Frames before the thread identity is known,
        // foreign-thread frames, or a frame without a sandbox stay
        // diagnostic observations — the notification is experimental, and
        // its absence is not evidence of a downgrade.
        let (Some(mode), Some(expected)) = (
            *self.admitted_posture.lock().unwrap(),
            self.session_id.lock().unwrap().clone(),
        ) else {
            return self.ignored_current(
                "thread/settings/updated",
                params,
                IgnoreReason::Configuration,
            );
        };
        if thread_id != Some(expected.as_str()) {
            let input = self.context("thread/settings/updated", params, None);
            // A frame naming another thread is foreign; a frame that names no
            // thread at all is merely unattributable.
            return if thread_id.is_some() {
                Self::ignored(
                    input,
                    Attribution::ForeignSession,
                    IgnoreReason::ForeignSession,
                )
            } else {
                Self::ignored(input, Attribution::Late, IgnoreReason::Late)
            };
        }
        let Some(sandbox) = params.pointer("/threadSettings/sandboxPolicy") else {
            return self.ignored_current(
                "thread/settings/updated",
                params,
                IgnoreReason::Configuration,
            );
        };
        if sandbox_confirmed(sandbox, mode) {
            return self.ignored_current(
                "thread/settings/updated",
                params,
                IgnoreReason::Configuration,
            );
        }
        let observed = sandbox
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
            .to_owned();
        self.publisher
            .publish_terminal(RuntimeTerminal::FailedRuntimeLost(
                RuntimeLoss::PostureViolation(observed),
            ));
        self.ignored_current(
            "thread/settings/updated",
            params,
            IgnoreReason::Configuration,
        )
    }

    /// A notification whose method this adapter does not model still gets an
    /// explicit disposition. Nothing is silently dropped and no native
    /// payload is reconstructed; uncertainty is an `UnknownSemantics` fact.
    fn unknown(&self, method: &str, params: &serde_json::Value) -> DecodedInput {
        let input = self.context(method, params, None);
        if let Some(frame_turn) = params.get("turnId").and_then(|value| value.as_str()) {
            let current = self.current_turn.lock().unwrap().clone();
            if current.as_deref() != Some(frame_turn) {
                return Self::ignored(input, Attribution::Late, IgnoreReason::Late);
            }
        }
        if let Some(reason) = ignored_notification(method) {
            return Self::ignored(input, Attribution::Current, reason);
        }
        let diagnostic = Diagnostic {
            reason: DiagnosticReason::UnknownSemantics,
            impact: Impact::Unknown,
            discriminator: SafeToken::new(method),
        };
        Self::batch(
            input,
            Attribution::Current,
            DecodeDisposition::Unknown(diagnostic.clone()),
            vec![EventFact::Diagnostic(diagnostic)],
        )
    }

    pub(super) fn diagnostic_tail(&self) -> String {
        let mut sections = Vec::new();
        if let Some(failure) = self.turn_failure.lock().unwrap().clone() {
            sections.push(format!(
                "codex turn failure: {}",
                update::turn_failure_label(&failure)
            ));
        }
        let mcp = self.mcp_tail.lock().unwrap().clone();
        if !mcp.is_empty() {
            sections.push(mcp.trim_end().to_owned());
        }
        sections.join("\n")
    }
}

/// Native status to typed outcome. A finished call without explicit result
/// evidence keeps `Unknown`; a declined call is a cancellation, never a
/// fabricated failure.
fn tool_outcome(status: Option<&str>) -> ToolOutcome {
    match status {
        Some("completed") => ToolOutcome::Succeeded,
        Some("failed") => ToolOutcome::Failed,
        Some("declined" | "interrupted" | "cancelled") => ToolOutcome::Cancelled,
        _ => ToolOutcome::Unknown,
    }
}

/// Recognized notifications that carry no public fact for this adapter. Each
/// reason is explicit: account/rate-limit frames never settle a turn, usage
/// and compaction are informational, and output/UI streams have no consumer.
fn ignored_notification(method: &str) -> Option<IgnoreReason> {
    Some(match method {
        "account/rateLimits/updated" => IgnoreReason::AccountLimits,
        "account/updated"
        | "account/gatewayOAuth/changed"
        | "account/login/completed"
        | "thread/started"
        | "thread/closed"
        | "thread/archived"
        | "thread/deleted"
        | "thread/unarchived"
        | "thread/reverted"
        | "thread/status/changed"
        | "thread/readState/changed"
        | "thread/name/updated"
        | "thread/section/changed"
        | "thread/attachment/updated"
        | "thread/goal/updated"
        | "thread/goal/cleared"
        | "thread/queue/changed"
        | "thread/project/updated"
        | "thread/prediction/updated"
        | "project/changed"
        | "thread/environment/connected"
        | "thread/environment/disconnected"
        | "thread/realtime/started"
        | "thread/realtime/itemAdded"
        | "thread/realtime/item/started"
        | "thread/realtime/item/completed"
        | "thread/realtime/item/transcript/delta"
        | "thread/realtime/transcript/delta"
        | "thread/realtime/transcript/done"
        | "thread/realtime/outputAudio/delta"
        | "thread/realtime/sdp"
        | "thread/realtime/closed"
        | "skills/changed"
        | "app/list/updated"
        | "mcpServer/oauthLogin/completed"
        | "mcpServer/event/stream/notification"
        | "remoteControl/status/changed"
        | "warning"
        | "guardianWarning"
        | "deprecationNotice"
        | "configWarning"
        | "fuzzyFileSearch/sessionUpdated"
        | "fuzzyFileSearch/sessionCompleted"
        | "windows/worldWritableWarning"
        | "windowsSandbox/setupCompleted" => IgnoreReason::NoConsumer,
        "thread/tokenUsage/updated" => IgnoreReason::Usage,
        "thread/compacted" => IgnoreReason::Compaction,
        "turn/plan/updated" | "turn/diff/updated" | "item/plan/delta" => IgnoreReason::PlanOrDiff,
        "item/commandExecution/outputDelta"
        | "item/fileChange/outputDelta"
        | "item/fileChange/patchUpdated"
        | "item/commandExecution/terminalInteraction"
        | "item/mcpToolCall/progress"
        | "command/exec/outputDelta"
        | "process/outputDelta"
        | "process/exited" => IgnoreReason::Ui,
        _ => return None,
    })
}

#[cfg(test)]
mod typed_tests {
    use super::*;
    use crate::activity_tracker::{PassiveActivityTracker, TerminalText};
    use crate::{LifecycleRecord, LifecycleSink};
    use serde_json::json;
    use std::sync::Mutex as StdMutex;

    struct ViewSink {
        activity: PassiveActivityTracker,
        records: StdMutex<Vec<LifecycleRecord>>,
    }

    impl LifecycleSink for ViewSink {
        fn emit(&self, record: LifecycleRecord) {
            self.activity.observe(&record.event);
            self.records.lock().unwrap().push(record);
        }
    }

    struct Trace {
        shared: Arc<CodexShared>,
        sink: Arc<ViewSink>,
    }

    impl Trace {
        fn new() -> Self {
            let sink = Arc::new(ViewSink {
                activity: PassiveActivityTracker::for_adapter("codex"),
                records: StdMutex::new(Vec::new()),
            });
            let publisher = Arc::new(Publisher::new(sink.clone()));
            let shared = Arc::new(CodexShared {
                publisher,
                turn_tracker: Arc::new(TurnTracker::new()),
                task: TaskId("task".into()),
                launch: LaunchId("launch".into()),
                session_id: StdMutex::new(Some("thread-1".into())),
                admitted_model: StdMutex::new(None),
                admitted_effort: StdMutex::new(None),
                admitted_posture: StdMutex::new(None),
                diagnostic_session_id: StdMutex::new(None),
                current_turn: StdMutex::new(None),
                retired_turns: StdMutex::new(Vec::new()),
                start_in_flight: AtomicBool::new(false),
                last_message_item: StdMutex::new(None),
                items: StdMutex::new(HashMap::new()),
                seen_tool_items: StdMutex::new(Vec::new()),
                turn_failure: StdMutex::new(None),
                mcp_tail: StdMutex::new(String::new()),
                stop_boundaries: AtomicU64::new(0),
            });
            Self { shared, sink }
        }

        fn decode(&self, method: &str, params: serde_json::Value) -> DecodedInput {
            let raw = json!({"method": method, "params": params});
            self.shared
                .decode_notification(method, &raw)
                .expect("recognized notification")
        }

        fn send(&self, method: &str, params: serde_json::Value) -> DecodedInput {
            let batch = self.decode(method, params);
            self.shared
                .publisher
                .emit_typed(batch.clone(), &self.shared.turn_tracker);
            batch
        }

        fn start_turn(&self, turn: &str) {
            self.shared.start_in_flight.store(true, Ordering::Release);
            let batch = self.send(
                "turn/started",
                json!({"threadId": "thread-1", "turn": {"id": turn, "status": "inProgress"}}),
            );
            assert_eq!(batch.disposition().kind(), DispositionKind::Consumed);
            self.shared.start_in_flight.store(false, Ordering::Release);
        }

        fn facts(&self) -> Vec<EventFact> {
            self.sink
                .records
                .lock()
                .unwrap()
                .iter()
                .filter_map(|record| match &record.event {
                    crate::RuntimeEvent::Normalized(event) => Some(event.fact.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    fn tool_fact(batch: &DecodedInput) -> &ToolChange {
        let Some(EventFact::ToolChanged(tool)) = batch.events().first().map(|event| &event.fact)
        else {
            panic!("batch has no tool fact: {:?}", batch.events());
        };
        tool
    }

    #[test]
    fn fixture_cases_drive_the_real_adapter_dispositions_and_facts() {
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/codex-events/cases.json"
        ))
        .unwrap();
        for case in cases {
            let input = &case["input"];
            let params = input
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let trace = Trace::new();
            // The fixture starts inside an admitted turn: pin the thread and
            // current turn the notification names.
            let turn = params
                .get("turnId")
                .and_then(|value| value.as_str())
                .or_else(|| params.pointer("/turn/id").and_then(|value| value.as_str()));
            *trace.shared.current_turn.lock().unwrap() = turn.map(str::to_owned);
            let batch = trace.decode(input["method"].as_str().unwrap(), params);
            assert_eq!(
                batch.disposition().kind(),
                match case["expected_disposition"].as_str().unwrap() {
                    "consumed" => DispositionKind::Consumed,
                    other => panic!("unexpected fixture disposition {other}"),
                },
                "{}",
                case["name"]
            );
            let facts: Vec<_> = batch
                .events()
                .iter()
                .map(|event| serde_json::to_value(&event.fact).unwrap()["kind"].clone())
                .collect();
            assert_eq!(json!(facts), case["expected_facts"], "{}", case["name"]);
            if case["name"] == "retry-error-object" {
                let Some(EventFact::ErrorObserved { failure, .. }) =
                    batch.events().first().map(|event| &event.fact)
                else {
                    panic!("error fixture has no ErrorObserved fact");
                };
                assert_eq!(failure.original["willRetry"], true);
                assert_eq!(
                    failure.original["additionalDetails"],
                    "complete native detail"
                );
            }
        }
    }

    #[test]
    fn tool_start_and_completion_are_complete_facts_with_public_arguments() {
        let trace = Trace::new();
        trace.start_turn("turn-1");

        let started = trace.send(
            "item/started",
            json!({"threadId": "thread-1", "turnId": "turn-1", "item": {
                "type": "commandExecution", "id": "c-1",
                "command": "/bin/zsh -lc 'echo 中'", "cwd": "/workspace",
                "aggregatedOutput": "not public", "status": "inProgress",
            }}),
        );
        let tool = tool_fact(&started);
        assert_eq!(tool.call_id, CallId("c-1".into()));
        assert_eq!(tool.name.as_deref(), Some("commandExecution"));
        assert_eq!(tool.phase, Some(ToolPhase::Started));
        assert_eq!(tool.outcome, None);
        assert_eq!(
            tool.arguments,
            Some(ArgumentUpdate::Snapshot(json!({
                "command": "/bin/zsh -lc 'echo 中'",
                "cwd": "/workspace",
            })))
        );

        let completed = trace.send(
            "item/completed",
            json!({"threadId": "thread-1", "turnId": "turn-1", "item": {
                "type": "commandExecution", "id": "c-1", "status": "completed",
            }}),
        );
        let tool = tool_fact(&completed);
        assert_eq!(tool.phase, Some(ToolPhase::Finished));
        assert_eq!(tool.outcome, Some(ToolOutcome::Succeeded));

        // A whitelisted completion whose start was missed is an authoritative
        // snapshot: it becomes a finished call on its own.
        let backfill = trace.send(
            "item/completed",
            json!({"threadId": "thread-1", "turnId": "turn-1", "item": {
                "type": "mcpToolCall", "id": "c-2",
                "server": "search", "tool": "query", "arguments": {"q": "rust"},
                "status": "failed", "error": {"message": "tool failed"},
            }}),
        );
        let tool = tool_fact(&backfill);
        assert_eq!(tool.phase, Some(ToolPhase::Finished));
        assert_eq!(tool.outcome, Some(ToolOutcome::Failed));
        assert_eq!(
            tool.failure
                .as_ref()
                .map(|failure| &failure.original["message"]),
            Some(&json!("tool failed"))
        );

        // A late start for the finished call is a replay, never a re-count.
        let duplicate = trace.decode(
            "item/started",
            json!({"threadId": "thread-1", "turnId": "turn-1", "item": {
                "type": "commandExecution", "id": "c-1",
            }}),
        );
        assert_eq!(
            duplicate.disposition(),
            &DecodeDisposition::RecognizedIgnored(IgnoreReason::Replay)
        );

        let activity = trace.sink.activity.snapshot();
        assert_eq!(activity.window_60s.tool_calls_started, 1);
        assert_eq!(activity.window_60s.tool_calls_completed, 1);
        assert_eq!(activity.window_60s.tool_calls_failed, 1);
        assert!(activity.active_tools.is_empty());

        let observation = trace.sink.activity.observation_snapshot();
        let command = observation
            .tools
            .iter()
            .find(|tool| tool.tool_name == "commandExecution")
            .expect("command observation");
        assert_eq!(command.call_count, 1);
        assert_eq!(
            command.recent_calls[0].arguments["command"],
            json!("/bin/zsh -lc 'echo 中'")
        );
        assert!(!observation.coverage.tool_history_complete);
    }

    #[test]
    fn public_summary_is_typed_and_raw_reasoning_is_never_ingested() {
        let trace = Trace::new();
        trace.start_turn("turn-1");

        let summary = trace.send(
            "item/reasoning/summaryTextDelta",
            json!({"threadId": "thread-1", "turnId": "turn-1",
                   "itemId": "rs-1", "summaryIndex": 0, "delta": "public summary"}),
        );
        let Some(EventFact::PublicReasoningDelta(reasoning)) =
            summary.events().first().map(|event| &event.fact)
        else {
            panic!("summary delta did not become a public reasoning fact");
        };
        assert_eq!(reasoning.channel, PublicReasoningChannel::Summary);
        assert_eq!(reasoning.part_id.as_deref(), Some("0"));
        assert_eq!(reasoning.delta, "public summary");

        let part = trace.decode(
            "item/reasoning/summaryPartAdded",
            json!({"threadId": "thread-1", "turnId": "turn-1",
                   "itemId": "rs-1", "summaryIndex": 1}),
        );
        assert_eq!(
            part.disposition(),
            &DecodeDisposition::RecognizedIgnored(IgnoreReason::StreamBoundary)
        );

        let raw = trace.decode(
            "item/reasoning/textDelta",
            json!({"threadId": "thread-1", "turnId": "turn-1",
                   "itemId": "rs-1", "delta": "private chain of thought"}),
        );
        assert_eq!(
            raw.disposition(),
            &DecodeDisposition::RecognizedIgnored(IgnoreReason::NoConsumer)
        );
        assert!(raw.events().is_empty());

        let activity = trace.sink.activity.snapshot();
        assert_eq!(activity.window_60s.reasoning_delta_events, 1);
        let observation = trace.sink.activity.observation_snapshot();
        assert!(observation.reasoning.text.is_empty());
        assert!(!observation.coverage.reasoning_complete);
    }

    #[test]
    fn rate_limit_and_account_notifications_are_recognized_and_never_settle() {
        let trace = Trace::new();
        trace.start_turn("turn-1");

        let limits = trace.decode(
            "account/rateLimits/updated",
            json!({"rateLimits": {"primary": {"usedPercent": 95}}}),
        );
        assert_eq!(
            limits.disposition(),
            &DecodeDisposition::RecognizedIgnored(IgnoreReason::AccountLimits)
        );
        let account = trace.decode("account/updated", json!({"authMode": "chatgpt"}));
        assert_eq!(
            account.disposition(),
            &DecodeDisposition::RecognizedIgnored(IgnoreReason::NoConsumer)
        );
        assert!(trace.shared.turn_tracker.snapshot().active);
        assert!(trace.shared.turn_tracker.failure_detail().is_none());

        // A usage-limit application error is a native failure observation,
        // never a task failure by itself.
        let error = trace.send(
            "error",
            json!({"threadId": "thread-1", "turnId": "turn-1", "willRetry": false,
                   "error": {"message": "You've hit your usage limit.",
                             "codexErrorInfo": "usageLimitExceeded"}}),
        );
        assert!(matches!(
            error.events()[0].fact,
            EventFact::ErrorObserved {
                operation: Operation::Turn,
                ..
            }
        ));
        assert!(trace.shared.turn_tracker.snapshot().active);
        assert_eq!(trace.shared.turn_tracker.snapshot().boundary, None);

        let completed = trace.send(
            "turn/completed",
            json!({"threadId": "thread-1",
                   "turn": {"id": "turn-1", "status": "completed", "error": null}}),
        );
        let Some(EventFact::TurnSettled(settlement)) =
            completed.events().first().map(|event| &event.fact)
        else {
            panic!("completion did not settle the turn");
        };
        assert_eq!(settlement.outcome, TurnOutcome::Completed);
        assert!(!trace.shared.turn_tracker.snapshot().active);
        assert!(trace.shared.turn_tracker.failure_detail().is_none());
    }

    #[test]
    fn failed_settlement_keeps_the_observed_native_failure() {
        let trace = Trace::new();
        trace.start_turn("turn-1");
        trace.send("error", json!({"threadId": "thread-1", "turnId": "turn-1",
            "willRetry": true, "error": {"message": "native cause", "codexErrorInfo": "serverOverloaded"}}));
        let observed = trace
            .shared
            .turn_tracker
            .failure_detail()
            .expect("active turn failure");
        assert!(trace.shared.turn_tracker.snapshot().active);
        trace.send("turn/completed", json!({"threadId": "thread-1",
            "turn": {"id": "turn-1", "status": "failed", "error": {"message": "boundary fallback"}}}));
        assert_eq!(
            trace.shared.turn_tracker.snapshot().boundary,
            Some(crate::TurnBoundary::Failed)
        );
        assert!(!trace.shared.turn_tracker.snapshot().active);
        assert_eq!(trace.shared.turn_tracker.failure_detail(), Some(observed));
    }

    #[test]
    fn late_and_retired_turn_traffic_never_touches_the_current_result() {
        let trace = Trace::new();
        trace.start_turn("turn-1");
        trace.send(
            "item/agentMessage/delta",
            json!({"threadId": "thread-1", "turnId": "turn-1",
                   "itemId": "msg-1", "delta": "GOOD_"}),
        );

        // A stale started while the current turn is active, and every stale
        // turn-scoped fact, stay late.
        let stale_start = trace.decode(
            "turn/started",
            json!({"threadId": "thread-1", "turn": {"id": "turn-0", "status": "inProgress"}}),
        );
        assert_eq!(
            stale_start.disposition(),
            &DecodeDisposition::RecognizedIgnored(IgnoreReason::Replay)
        );
        for (method, params) in [
            (
                "item/agentMessage/delta",
                json!({"threadId": "thread-1", "turnId": "turn-0",
                       "itemId": "stale", "delta": "STALE"}),
            ),
            (
                "item/completed",
                json!({"threadId": "thread-1", "turnId": "turn-0",
                       "item": {"type": "agentMessage", "id": "stale", "text": "STALE"}}),
            ),
            (
                "item/reasoning/summaryTextDelta",
                json!({"threadId": "thread-1", "turnId": "turn-0",
                       "itemId": "stale", "delta": "STALE"}),
            ),
            (
                "error",
                json!({"threadId": "thread-1", "turnId": "turn-0",
                       "error": {"message": "stale failure"}}),
            ),
            (
                "turn/completed",
                json!({"threadId": "thread-1",
                       "turn": {"id": "turn-0", "status": "completed", "error": null}}),
            ),
        ] {
            let batch = trace.decode(method, params);
            assert_eq!(
                batch.disposition(),
                &DecodeDisposition::RecognizedIgnored(IgnoreReason::Late),
                "{method}"
            );
        }
        // A frame without a thread id cannot be attributed either.
        let unattributable = trace.decode(
            "item/agentMessage/delta",
            json!({"turnId": "turn-1", "itemId": "msg-1", "delta": "NO_THREAD"}),
        );
        assert_eq!(
            unattributable.disposition(),
            &DecodeDisposition::RecognizedIgnored(IgnoreReason::Late)
        );

        trace.send(
            "item/agentMessage/delta",
            json!({"threadId": "thread-1", "turnId": "turn-1",
                   "itemId": "msg-1", "delta": "OK"}),
        );
        trace.send(
            "item/completed",
            json!({"threadId": "thread-1", "turnId": "turn-1",
                   "item": {"type": "agentMessage", "id": "msg-1"}}),
        );
        trace.send(
            "turn/completed",
            json!({"threadId": "thread-1",
                   "turn": {"id": "turn-1", "status": "completed", "error": null}}),
        );
        assert_eq!(
            trace.sink.activity.take_terminal_text(),
            TerminalText::Visible("GOOD_OK".into())
        );

        // After retirement the same turn stays closed.
        let replay = trace.decode(
            "turn/started",
            json!({"threadId": "thread-1", "turn": {"id": "turn-1", "status": "inProgress"}}),
        );
        assert_eq!(
            replay.disposition(),
            &DecodeDisposition::RecognizedIgnored(IgnoreReason::Replay)
        );
        let late = trace.decode(
            "item/agentMessage/delta",
            json!({"threadId": "thread-1", "turnId": "turn-1",
                   "itemId": "msg-1", "delta": "LATE"}),
        );
        assert_eq!(
            late.disposition(),
            &DecodeDisposition::RecognizedIgnored(IgnoreReason::Late)
        );
        let deltas: Vec<_> = trace
            .facts()
            .into_iter()
            .filter_map(|fact| match fact {
                EventFact::MessageUpdated(message) => Some(message.text),
                _ => None,
            })
            .collect();
        assert_eq!(
            deltas,
            [
                TextUpdate::Delta("GOOD_".into()),
                TextUpdate::Delta("OK".into())
            ],
            "late traffic never reaches the sink"
        );
    }

    #[test]
    fn malformed_tool_identity_is_late_before_it_can_damage_current_telemetry() {
        let trace = Trace::new();
        trace.start_turn("turn-1");
        for method in ["item/started", "item/completed"] {
            for id in [
                None,
                Some(json!("")),
                Some(json!("x".repeat(513))),
                Some(json!("bad\0id")),
            ] {
                for turn in [Some("turn-0"), None] {
                    let mut params =
                        json!({"threadId": "thread-1", "item": {"type": "commandExecution"}});
                    if let Some(id) = &id {
                        params["item"]["id"] = id.clone();
                    }
                    if let Some(turn) = turn {
                        params["turnId"] = json!(turn);
                    }
                    let batch = trace.send(method, params.clone());
                    assert_eq!(batch.attribution(), Attribution::Late, "{method}: {params}");
                    assert_eq!(
                        batch.disposition(),
                        &DecodeDisposition::RecognizedIgnored(IgnoreReason::Late)
                    );
                    assert!(batch.events().is_empty());
                    assert!(!trace.sink.activity.snapshot().telemetry_degraded);
                    assert_eq!(
                        trace
                            .sink
                            .activity
                            .observation_snapshot()
                            .coverage
                            .dropped_events,
                        0
                    );

                    // The same malformed identity on the current turn still
                    // reports lost tool coverage rather than silently dropping it.
                    params["turnId"] = json!("turn-1");
                    let current = trace.decode(method, params);
                    assert_eq!(current.attribution(), Attribution::Current);
                    assert!(
                        matches!(current.disposition(), DecodeDisposition::InvalidLost(gap)
                        if gap.reason == LossReason::Malformed
                            && gap.impact == Impact::Known(vec![CoverageDimension::Tools]))
                    );
                }
            }
        }
        assert!(trace.shared.turn_tracker.snapshot().active);
        assert_eq!(
            trace.sink.activity.snapshot().window_60s.tool_calls_started,
            0
        );
        assert_eq!(
            trace
                .sink
                .activity
                .snapshot()
                .window_60s
                .tool_calls_completed,
            0
        );
    }

    #[test]
    fn unknown_notifications_become_explicit_diagnostics_instead_of_raw_payloads() {
        let trace = Trace::new();
        trace.start_turn("turn-1");
        let batch = trace.send(
            "experimental/futureNotification",
            json!({"threadId": "thread-1", "turnId": "turn-1", "payload": "opaque"}),
        );
        assert_eq!(batch.attribution(), Attribution::Current);
        assert!(matches!(
            batch.disposition(),
            DecodeDisposition::Unknown(diagnostic)
                if diagnostic.reason == DiagnosticReason::UnknownSemantics
        ));
        let Some(EventFact::Diagnostic(diagnostic)) =
            batch.events().first().map(|event| &event.fact)
        else {
            panic!("unknown notification has no diagnostic fact");
        };
        assert_eq!(
            diagnostic.discriminator.as_ref().map(SafeToken::as_str),
            Some("experimental/futureNotification")
        );
    }
}
