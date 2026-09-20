//! Shared Codex projection state: turn attribution against the live
//! thread, retirement of completed turns, and application of the pure
//! notification folding from [`external_agent_codex::update`] — the crate
//! extracts projection inputs and builds the canonical `session/event`
//! payloads and envelopes; this module owns the daemon-side state and
//! publishes through the Publisher.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use external_agent_codex::update;
use external_runtime::Inbound;

use crate::{Publisher, TurnTracker};

pub(super) struct CodexShared {
    pub(super) publisher: Arc<Publisher>,
    pub(super) turn_tracker: Arc<TurnTracker>,
    pub(super) session_id: Mutex<Option<String>>,
    pub(super) admitted_model: Mutex<Option<String>>,
    pub(super) admitted_effort: Mutex<Option<String>>,
    pub(super) diagnostic_session_id: Mutex<Option<String>>,
    pub(super) current_turn: Mutex<Option<String>>,
    pub(super) retired_turns: Mutex<Vec<String>>,
    pub(super) start_in_flight: AtomicBool,
    pub(super) last_message_item: Mutex<Option<String>>,
    pub(super) items: Mutex<HashMap<String, String>>,
    pub(super) turn_failure: Mutex<Option<String>>,
    pub(super) mcp_tail: Mutex<String>,
    pub(super) sequence: AtomicU64,
    pub(super) stop_boundaries: AtomicU64,
}

impl CodexShared {
    fn next_event_id(&self) -> String {
        update::event_id(self.sequence.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn canonical_event(&self, params: serde_json::Value) -> Inbound {
        update::canonical_event(params)
    }

    fn emit_canonical(&self, params: serde_json::Value) {
        self.publisher
            .emit_driver(self.canonical_event(params), None);
    }

    fn emit_lifecycle(&self, method: &str) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        self.publisher
            .emit_driver(update::lifecycle_event(sequence, method), None);
    }

    fn observe_item_text(&self, item_id: &str, text: &str) {
        update::observe_item_text(&mut self.items.lock().unwrap(), item_id, text);
    }

    fn item_text(&self, item_id: &str) -> Option<String> {
        self.items.lock().unwrap().get(item_id).cloned()
    }

    fn record_turn_failure(&self, detail: String) {
        *self.turn_failure.lock().unwrap() = Some(update::bounded_turn_failure_detail(&detail));
    }

    fn record_mcp_failure(&self, name: &str, error: &str) {
        update::record_mcp_failure(&mut self.mcp_tail.lock().unwrap(), name, error);
    }

    /// The frame's declared turn id pins turn-scoped traffic to the turn
    /// this runtime is currently projecting: the id must be present and
    /// match the current turn. Anything else — a missing id, or a late
    /// frame from an older turn — is unattributable and must not touch the
    /// current result.
    fn attributable_turn(&self, frame_turn_id: Option<&str>) -> Option<String> {
        let current = self.current_turn.lock().unwrap().clone();
        match (current, frame_turn_id) {
            (Some(current), Some(arrived)) if current == arrived => Some(current),
            _ => None,
        }
    }

    /// A turn that reached its terminal boundary is retired: its identity
    /// can never become current again. Late or duplicate `turn/started`
    /// frames for a retired turn stay diagnostic observations instead of
    /// reopening a closed boundary.
    fn retire_turn(&self, turn_id: &str) {
        update::retire_turn(&mut self.retired_turns.lock().unwrap(), turn_id);
        *self.current_turn.lock().unwrap() = None;
    }

    fn turn_is_retired(&self, turn_id: &str) -> bool {
        self.retired_turns
            .lock()
            .unwrap()
            .iter()
            .any(|id| id == turn_id)
    }

    /// One inbound Codex notification, normalized. Returns `None` when the
    /// frame was fully projected into canonical events; otherwise the
    /// original frame is re-emitted unchanged as a diagnostic observation.
    ///
    /// Turn-scoped traffic (`turn/*`, `item/*`, `error`) must carry the
    /// active thread id and the id of the turn it belongs to. A missing or
    /// mismatched identity is never projected onto the current turn: late
    /// deltas, items, errors, and boundaries from an older turn stay
    /// diagnostic observations only.
    pub(super) fn project_notification(&self, method: &str, raw: &serde_json::Value) -> bool {
        let params = raw
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let session = self.session_id.lock().unwrap().clone();
        let thread_id = params.get("threadId").and_then(|value| value.as_str());
        let turn_scoped = update::is_turn_scoped(method);
        if turn_scoped {
            // Without a known thread, or without the frame declaring which
            // thread it belongs to, attribution is impossible: drop it.
            let Some(expected) = session.as_deref() else {
                return true;
            };
            if thread_id != Some(expected) {
                return true;
            }
        } else if let (Some(expected), Some(actual)) = (session.as_deref(), thread_id) {
            if expected != actual {
                // Foreign-thread traffic is never projected onto this task.
                return true;
            }
        }
        let event_id = self.next_event_id();
        match method {
            "turn/started" => {
                let Some(turn_id) = update::started_turn_id(&params) else {
                    return true;
                };
                if self.turn_tracker.snapshot().active {
                    // A started while a turn is already active is a late
                    // replay or duplicate: it must never re-open the active
                    // boundary or reset this turn's recorded state.
                    return true;
                }
                if self.turn_is_retired(turn_id) {
                    // A completed or failed turn stays closed: a late or
                    // duplicate started for it must not reactivate the old
                    // identity after its boundary was projected.
                    return true;
                }
                if !self.start_in_flight.load(Ordering::Acquire) {
                    // A started for an unknown turn with no start request in
                    // flight is unsolicited replay traffic; it must not
                    // reopen a boundary either.
                    return true;
                }
                *self.current_turn.lock().unwrap() = Some(turn_id.to_owned());
                *self.last_message_item.lock().unwrap() = None;
                // A stale failure detail from an earlier turn must never
                // label this turn's boundary.
                *self.turn_failure.lock().unwrap() = None;
                // The sink observes the canonical event before the tracker
                // exposes the new turn, matching the boundary ordering.
                let params = update::turn_started_payload(&event_id, turn_id);
                let event = self.canonical_event(params.clone());
                self.emit_canonical(params);
                self.turn_tracker.observe(&event);
                self.emit_lifecycle("turn.started");
                false
            }
            "item/agentMessage/delta" => {
                let Some((delta, item_id)) = update::agent_message_delta(&params) else {
                    return true;
                };
                let frame_turn = params.get("turnId").and_then(|value| value.as_str());
                let Some(turn_id) = self.attributable_turn(frame_turn) else {
                    // A delta that cannot be pinned to the current turn
                    // must not stream into this task's result.
                    return true;
                };
                self.observe_item_text(item_id, delta);
                self.emit_canonical(update::model_streaming_payload(
                    &event_id, &turn_id, delta, item_id,
                ));
                false
            }
            "item/completed" => {
                let Some((item_id, text)) = update::completed_agent_message(&params) else {
                    return true;
                };
                let frame_turn = params.get("turnId").and_then(|value| value.as_str());
                let Some(turn_id) = self.attributable_turn(frame_turn) else {
                    // A completed item from another (or unidentifiable)
                    // turn must not become this turn's final message.
                    return true;
                };
                if let Some(text) = text {
                    update::record_completed_item(&mut self.items.lock().unwrap(), item_id, text);
                }
                *self.last_message_item.lock().unwrap() = Some(item_id.to_owned());
                self.emit_canonical(update::message_finished_payload(
                    &event_id, &turn_id, item_id,
                ));
                false
            }
            "turn/completed" => {
                let completed = update::completed_turn(&params);
                let Some(current) = self.attributable_turn(completed.turn_id) else {
                    // A boundary without the current turn's identity never
                    // settles this task's active turn.
                    return true;
                };
                match completed.status {
                    Some("completed") => {
                        let tracked = self
                            .last_message_item
                            .lock()
                            .unwrap()
                            .clone()
                            .and_then(|item_id| self.item_text(&item_id));
                        let final_text = update::final_text(completed.turn, tracked);
                        let params =
                            update::turn_completed_payload(&event_id, &current, final_text);
                        let boundary = self.canonical_event(params.clone());
                        self.emit_canonical(params);
                        self.turn_tracker.observe(&boundary);
                        self.retire_turn(&current);
                        self.emit_lifecycle("turn.completed");
                        false
                    }
                    Some("failed") | Some("interrupted") => {
                        let recorded = self.turn_failure.lock().unwrap().clone();
                        let reason = update::turn_failure_reason(
                            completed.turn,
                            completed.status.unwrap_or("failed"),
                            recorded,
                        );
                        self.record_turn_failure(reason.clone());
                        let params = update::turn_failed_payload(&event_id, &current, &reason);
                        let boundary = self.canonical_event(params.clone());
                        self.emit_canonical(params);
                        self.turn_tracker.observe(&boundary);
                        self.retire_turn(&current);
                        self.emit_lifecycle("turn.failed");
                        false
                    }
                    _ => true,
                }
            }
            "error" => {
                // A turn-scoped application error (for example a
                // serverOverloaded capacity failure) is recorded for the
                // boundary projection; by itself it never completes a task.
                // Only an error attributable to the current turn may label
                // this task's failure reason.
                let frame_turn = params.get("turnId").and_then(|value| value.as_str());
                if self.attributable_turn(frame_turn).is_none() {
                    return true;
                }
                self.record_turn_failure(update::turn_error_detail(&params));
                true
            }
            "mcpServer/startupStatus/updated" => {
                if let Some((name, error)) = update::mcp_startup_failure(&params) {
                    self.record_mcp_failure(name, error);
                }
                true
            }
            _ => true,
        }
    }

    pub(super) fn diagnostic_tail(&self) -> String {
        let mut sections = Vec::new();
        if let Some(failure) = self.turn_failure.lock().unwrap().clone() {
            sections.push(format!("codex turn failure: {failure}"));
        }
        let mcp = self.mcp_tail.lock().unwrap().clone();
        if !mcp.is_empty() {
            sections.push(mcp.trim_end().to_owned());
        }
        sections.join("\n")
    }
}
