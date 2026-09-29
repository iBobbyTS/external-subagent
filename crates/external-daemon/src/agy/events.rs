//! Shared `agy` projection state and the normalization of parsed stream-json
//! events into the canonical internal event envelope, plus the session-start
//! gate the bootstrap waits on.
//!
//! The pump feeds one parsed [`AgyEvent`] at a time. `init` records the
//! `conversation_id` as the session identity and opens the bootstrap gate;
//! `step_update` text deltas project as `model.streaming`/`text_delta` and
//! tool steps as canonical `tool.updated` activity; `result` re-settles the
//! current turn through the pure [`TurnClassifier`] and projects the boundary.
//!
//! # Turn settlement tri-state
//!
//! A turn is opened by [`AgyRuntimeShared::begin_turn`] (the daemon sent a
//! `user` line) and closed by the **first** `result` after it was opened, not
//! by process exit. A result that arrives with no open turn is stream noise:
//! the measured idle-period signal trap emits a second `ERROR` result after a
//! turn already settled (`docs/compatibility/antigravity.md` §4), and it must
//! never overwrite the settled terminal. Before `init`, a result with no
//! `conversation_id` is an admission/session-start failure, not a turn
//! settlement, and it fails the bootstrap wait instead of emitting a boundary.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

use external_agent_agy::{
    activity::canonical_activity,
    event::{AgyEvent, ResultPayload, StepType},
    session::{TurnClassifier, TurnOutcome, TurnSettlement},
};
use external_contract::{classify_lifecycle, EventEnvelope, WireMessage, SESSION_EVENT};
use external_runtime::Inbound;

use crate::{Publisher, RuntimeCommandError, TurnTracker};

/// Upper bound on the diagnostic text the denied-actions projection retains.
const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;

/// The bootstrap session-start gate: `Pending` until the child reports its
/// identity (`init`) or fails admission, then `Ready`/`Failed` forever.
pub(super) enum AgyStartState {
    Pending,
    Ready,
    Failed(String),
}

/// The per-session turn state: the pure classifier plus whether a turn is
/// currently awaiting its settling `result`.
struct AgyTurnState {
    classifier: TurnClassifier,
    open: bool,
}

pub(super) struct AgyRuntimeShared {
    pub(super) publisher: Arc<Publisher>,
    pub(super) turn_tracker: Arc<TurnTracker>,
    pub(super) session_id: Mutex<Option<String>>,
    pub(super) start: Mutex<AgyStartState>,
    pub(super) start_changed: Condvar,
    turn: Mutex<AgyTurnState>,
    pub(super) sequence: AtomicU64,
    pub(super) stop_boundaries: AtomicU64,
    diagnostics: Mutex<String>,
}

impl AgyRuntimeShared {
    pub(super) fn new(publisher: Arc<Publisher>, turn_tracker: Arc<TurnTracker>) -> Self {
        Self {
            publisher,
            turn_tracker,
            session_id: Mutex::new(None),
            start: Mutex::new(AgyStartState::Pending),
            start_changed: Condvar::new(),
            turn: Mutex::new(AgyTurnState {
                classifier: TurnClassifier::new(),
                open: false,
            }),
            sequence: AtomicU64::new(0),
            stop_boundaries: AtomicU64::new(0),
            diagnostics: Mutex::new(String::new()),
        }
    }

    fn next_event_id(&self) -> String {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        format!("agy-event-{sequence}")
    }

    fn canonical_event(&self, params: serde_json::Value) -> Inbound {
        Inbound::Message(WireMessage::Event(EventEnvelope {
            method: SESSION_EVENT.into(),
            params,
        }))
    }

    fn emit_lifecycle(&self, method: &str) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        self.publisher.emit_driver(
            Inbound::Lifecycle {
                sequence,
                method: method.into(),
                order: classify_lifecycle(method, false),
            },
            None,
        );
    }

    /// Emit one canonical event after refreshing turn liveness, matching the
    /// DSH streaming projection order (tracker first, then sink).
    fn emit_and_observe(&self, params: serde_json::Value) {
        let event = self.canonical_event(params);
        self.turn_tracker.observe(&event);
        self.publisher.emit_driver(event, None);
    }

    /// Emit a turn boundary: the sink must observe the settlement's
    /// authoritative payload before the tracker exposes the boundary, so the
    /// scheduler cannot terminalize the turn before the final text lands.
    fn emit_boundary(&self, params: serde_json::Value, lifecycle: &str) {
        let event = self.canonical_event(params.clone());
        self.publisher.emit_driver(self.canonical_event(params), None);
        self.turn_tracker.observe(&event);
        self.emit_lifecycle(lifecycle);
    }

    fn note_session(&self, conversation_id: &str) {
        if conversation_id.is_empty() {
            return;
        }
        let mut session = self.session_id.lock().unwrap();
        if session.as_deref() != Some(conversation_id) {
            *session = Some(conversation_id.to_owned());
        }
    }

    pub(super) fn session_id(&self) -> Option<String> {
        self.session_id.lock().unwrap().clone()
    }

    fn effective_turn_id(&self) -> String {
        self.session_id()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| "agy-session".into())
    }

    /// The canonical DSH-shaped `model.streaming`/`text_delta` payload. `agy`
    /// has no assistant-message identity, so the id field is present but null
    /// (matching the DSH projection of an anonymous text chunk).
    fn streaming_payload(&self, delta: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "model.streaming",
            "eventId": self.next_event_id(),
            "turnId": self.effective_turn_id(),
            "payload": {
                "kind": "text_delta",
                "delta": delta,
                "assistantMessageId": serde_json::Value::Null,
            },
        })
    }

    /// Open a new turn. Called before a `user` line leaves for the child, so a
    /// result can never be observed without an open turn.
    pub(super) fn begin_turn(&self) {
        self.turn.lock().unwrap().open = true;
        let params = serde_json::json!({"type": "turn.started"});
        let started = self.canonical_event(params.clone());
        self.publisher.emit_driver(self.canonical_event(params), None);
        self.turn_tracker.observe(&started);
        self.emit_lifecycle("turn.started");
    }

    /// Wait for the bootstrap gate: `init` (identity recorded) or an
    /// admission/session-start failure.
    pub(super) fn wait_start(&self, timeout: Duration) -> Result<String, RuntimeCommandError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RuntimeCommandError::Timeout)?;
        let mut start = self.start.lock().unwrap();
        loop {
            match &*start {
                AgyStartState::Ready => {
                    return Ok(self.session_id().unwrap_or_default());
                }
                AgyStartState::Failed(reason) => return Err(start_failure_error(reason)),
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

    fn mark_start_ready(&self) {
        let mut start = self.start.lock().unwrap();
        if matches!(*start, AgyStartState::Pending) {
            *start = AgyStartState::Ready;
            self.start_changed.notify_all();
        }
    }

    /// Project one parsed event. Never called concurrently: the pump is the
    /// single consumer of its driver.
    pub(super) fn project_event(&self, event: &AgyEvent) {
        match event {
            AgyEvent::Init {
                conversation_id, ..
            } => {
                self.note_session(conversation_id);
                self.mark_start_ready();
            }
            AgyEvent::StepUpdate(step) => {
                self.note_session(&step.conversation_id);
                if step.step_type == StepType::AgentResponse {
                    if let Some(delta) = step.text_delta.as_deref().filter(|text| !text.is_empty()) {
                        let params = self.streaming_payload(delta);
                        self.emit_and_observe(params);
                    }
                }
                let event_id = self.next_event_id();
                let turn_id = self.effective_turn_id();
                if let Some(activity) = canonical_activity(event, &event_id, &turn_id) {
                    self.emit_and_observe(activity);
                }
            }
            AgyEvent::Result(result) => self.project_result(event, result),
            AgyEvent::Unknown { .. } => {}
        }
    }

    fn project_result(&self, event: &AgyEvent, result: &ResultPayload) {
        // Before the session is established, a result without an identity is
        // an admission/start failure. It fails the bootstrap wait instead of
        // settling a turn: no `init` ever arrived, so no turn ran.
        {
            let mut start = self.start.lock().unwrap();
            if matches!(*start, AgyStartState::Pending) {
                if result.conversation_id.is_empty() {
                    let reason = result.error.clone().unwrap_or_else(|| {
                        format!("agy session start failed with status {}", result.status.as_str())
                    });
                    *start = AgyStartState::Failed(reason);
                    self.start_changed.notify_all();
                    return;
                }
                self.note_session(&result.conversation_id);
                *start = AgyStartState::Ready;
                self.start_changed.notify_all();
            }
        }
        let settlement = {
            let mut turn = self.turn.lock().unwrap();
            if !turn.open {
                // A result with no open turn is stream noise: the idle-period
                // signal trap emits a second result after the turn settled,
                // and it must never overwrite that terminal.
                return;
            }
            turn.open = false;
            match turn.classifier.observe(event) {
                Some(settlement) => settlement,
                None => return,
            }
        };
        self.apply_settlement(&settlement);
    }

    fn apply_settlement(&self, settlement: &TurnSettlement) {
        let turn_id = self.effective_turn_id();
        match &settlement.outcome {
            TurnOutcome::Completed { text } => {
                let params = serde_json::json!({
                    "type": "turn.completed",
                    "eventId": self.next_event_id(),
                    "turnId": turn_id,
                    "payload": {"response": text},
                });
                self.emit_boundary(params, "turn.completed");
            }
            TurnOutcome::Failed { error } => {
                self.record_denied_actions(&settlement.denied_actions);
                let params = serde_json::json!({
                    "type": "turn.failed",
                    "eventId": self.next_event_id(),
                    "turnId": turn_id,
                    "payload": {"reason_code": error},
                });
                self.emit_boundary(params, "turn.failed");
            }
            TurnOutcome::Cancelled { text, error } => {
                self.record_denied_actions(&settlement.denied_actions);
                // The truncated partial text a signal interrupt carries is not
                // streamed on the measurement (`docs/compatibility/
                // antigravity.md` §4); project it as a text delta so the wait
                // tail and observation keep the partial output.
                if !text.is_empty() {
                    let params = self.streaming_payload(text);
                    self.emit_and_observe(params);
                }
                let params = serde_json::json!({
                    "type": "turn.failed",
                    "eventId": self.next_event_id(),
                    "turnId": turn_id,
                    "payload": {"reason_code": error},
                });
                self.emit_boundary(params, "turn.failed");
            }
        }
    }

    /// Append the structured soft-deny entries of a failed/cancelled
    /// settlement to the bounded diagnostic tail. The success path has no
    /// public projection surface and is a documented gap.
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
        let line = format!("agy denied_actions: {rendered}");
        let mut tail = self.diagnostics.lock().unwrap();
        if !tail.is_empty() {
            tail.push('\n');
        }
        tail.push_str(&line);
        let excess = tail.len().saturating_sub(MAX_DIAGNOSTIC_BYTES);
        if excess > 0 {
            let mut split = excess;
            while split < tail.len() && !tail.is_char_boundary(split) {
                split += 1;
            }
            tail.drain(..split);
        }
    }

    pub(super) fn diagnostic_tail(&self) -> String {
        self.diagnostics.lock().unwrap().clone()
    }
}

/// The settlement never arrived because admission or the session start failed;
/// an `agy` model/effort rejection is reported as such so the scheduler keeps
/// its dedicated public code.
fn start_failure_error(reason: &str) -> RuntimeCommandError {
    let bounded: String = reason.chars().take(512).collect();
    if reason.contains("invalid model")
        || reason.contains("invalid effort")
        || reason.contains("model selection")
    {
        RuntimeCommandError::ModelRejected(bounded)
    } else {
        RuntimeCommandError::InvalidSession(bounded)
    }
}

/// Normalize one inbound `agy` frame. NDJSON frames are parsed and projected;
/// the raw frame is consumed either way. Every other inbound (malformed or
/// oversized lines, child exit, stream closure) passes through unchanged.
pub(super) fn project_agy_inbound(shared: &AgyRuntimeShared, event: &Inbound) -> Option<Inbound> {
    let Inbound::Message(WireMessage::UnknownEvent { raw, .. }) = event else {
        return Some(event.clone());
    };
    let Ok(line) = serde_json::to_string(raw) else {
        return None;
    };
    match external_agent_agy::event::parse_line(&line) {
        Ok(parsed) => {
            shared.project_event(&parsed);
            None
        }
        // A syntactically valid frame this crate cannot model (unknown event
        // name, bad payload) is transport noise: skip it, keep reading.
        Err(_) => None,
    }
}
