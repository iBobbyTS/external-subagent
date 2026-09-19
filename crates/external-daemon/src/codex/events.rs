//! Projection of Codex `item/*`, `turn/*`, and `error` notifications into
//! the canonical internal `session/event` lifecycle, including turn
//! attribution, retirement of completed turns, and bounded failure tails.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use external_contract::{EventEnvelope, WireMessage};
use external_runtime::Inbound;

use crate::{Publisher, TurnTracker};

const MAX_ITEM_TEXT_BYTES: usize = 512 * 1024;
const MAX_TRACKED_ITEMS: usize = 128;
const MAX_TURN_FAILURE_DETAIL_BYTES: usize = 512;
const MAX_MCP_DIAGNOSTIC_BYTES: usize = 2 * 1024;
const MAX_RETIRED_TURNS: usize = 64;

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
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        format!("codex-event-{sequence}")
    }

    fn canonical_event(&self, params: serde_json::Value) -> Inbound {
        Inbound::Message(WireMessage::Event(EventEnvelope {
            method: external_contract::SESSION_EVENT.into(),
            params,
        }))
    }

    fn emit_canonical(&self, params: serde_json::Value) {
        self.publisher
            .emit_driver(self.canonical_event(params), None);
    }

    fn emit_lifecycle(&self, method: &str) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        self.publisher.emit_driver(
            Inbound::Lifecycle {
                sequence,
                method: method.into(),
                order: external_contract::classify_lifecycle(method, false),
            },
            None,
        );
    }

    fn observe_item_text(&self, item_id: &str, text: &str) {
        let mut items = self.items.lock().unwrap();
        if !items.contains_key(item_id) && items.len() >= MAX_TRACKED_ITEMS {
            return;
        }
        let entry = items.entry(item_id.to_owned()).or_default();
        let bounded = text.len() + entry.len() <= MAX_ITEM_TEXT_BYTES;
        if bounded {
            entry.push_str(text);
        } else {
            let remaining = MAX_ITEM_TEXT_BYTES.saturating_sub(entry.len());
            if remaining > 0 {
                let mut end = remaining;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                entry.push_str(&text[..end]);
            }
        }
    }

    fn item_text(&self, item_id: &str) -> Option<String> {
        self.items.lock().unwrap().get(item_id).cloned()
    }

    fn record_turn_failure(&self, detail: String) {
        let bounded: String = detail.chars().take(MAX_TURN_FAILURE_DETAIL_BYTES).collect();
        *self.turn_failure.lock().unwrap() = Some(bounded);
    }

    fn record_mcp_failure(&self, name: &str, error: &str) {
        let mut tail = self.mcp_tail.lock().unwrap();
        let line = format!("mcp {name} failed: {error}\n");
        tail.push_str(&line);
        if tail.len() > MAX_MCP_DIAGNOSTIC_BYTES {
            let mut keep = tail.len() - MAX_MCP_DIAGNOSTIC_BYTES;
            while keep < tail.len() && !tail.is_char_boundary(keep) {
                keep += 1;
            }
            tail.drain(..keep);
        }
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
        let mut retired = self.retired_turns.lock().unwrap();
        if !retired.iter().any(|id| id == turn_id) {
            if retired.len() >= MAX_RETIRED_TURNS {
                retired.remove(0);
            }
            retired.push(turn_id.to_owned());
        }
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
        let turn_scoped = matches!(
            method,
            "turn/started"
                | "item/agentMessage/delta"
                | "item/completed"
                | "turn/completed"
                | "error"
        );
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
                let Some(turn_id) = params
                    .pointer("/turn/id")
                    .and_then(|value| value.as_str())
                    .filter(|id| !id.is_empty())
                else {
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
                let event = self.canonical_event(serde_json::json!({
                    "type": "turn.started",
                    "eventId": event_id,
                    "turnId": turn_id,
                }));
                self.emit_canonical(serde_json::json!({
                    "type": "turn.started",
                    "eventId": event_id,
                    "turnId": turn_id,
                }));
                self.turn_tracker.observe(&event);
                self.emit_lifecycle("turn.started");
                false
            }
            "item/agentMessage/delta" => {
                let (Some(delta), Some(item_id)) = (
                    params.get("delta").and_then(|value| value.as_str()),
                    params.get("itemId").and_then(|value| value.as_str()),
                ) else {
                    return true;
                };
                let frame_turn = params.get("turnId").and_then(|value| value.as_str());
                let Some(turn_id) = self.attributable_turn(frame_turn) else {
                    // A delta that cannot be pinned to the current turn
                    // must not stream into this task's result.
                    return true;
                };
                self.observe_item_text(item_id, delta);
                self.emit_canonical(serde_json::json!({
                    "type": "model.streaming",
                    "eventId": event_id,
                    "turnId": turn_id,
                    "payload": {
                        "kind": "text_delta",
                        "delta": delta,
                        "assistantMessageId": item_id,
                    },
                }));
                false
            }
            "item/completed" => {
                let item = params.get("item").unwrap_or(&serde_json::Value::Null);
                if item.get("type").and_then(|value| value.as_str()) != Some("agentMessage") {
                    return true;
                }
                let Some(item_id) = item.get("id").and_then(|value| value.as_str()) else {
                    return true;
                };
                let frame_turn = params.get("turnId").and_then(|value| value.as_str());
                let Some(turn_id) = self.attributable_turn(frame_turn) else {
                    // A completed item from another (or unidentifiable)
                    // turn must not become this turn's final message.
                    return true;
                };
                if let Some(text) = item.get("text").and_then(|value| value.as_str()) {
                    let mut items = self.items.lock().unwrap();
                    if items.contains_key(item_id) || items.len() < MAX_TRACKED_ITEMS {
                        items.insert(item_id.to_owned(), text.to_owned());
                    }
                }
                *self.last_message_item.lock().unwrap() = Some(item_id.to_owned());
                self.emit_canonical(serde_json::json!({
                    "type": "message.finished",
                    "eventId": event_id,
                    "turnId": turn_id,
                    "payload": {"assistantMessageId": item_id},
                }));
                false
            }
            "turn/completed" => {
                let turn = params.get("turn").unwrap_or(&serde_json::Value::Null);
                let status = turn.get("status").and_then(|value| value.as_str());
                let turn_id = turn.get("id").and_then(|value| value.as_str());
                let Some(current) = self.attributable_turn(turn_id) else {
                    // A boundary without the current turn's identity never
                    // settles this task's active turn.
                    return true;
                };
                match status {
                    Some("completed") => {
                        let final_text = self.final_text(turn);
                        let mut payload = serde_json::json!({});
                        if let Some(text) = final_text {
                            payload["response"] = serde_json::Value::String(text);
                        }
                        let params = serde_json::json!({
                            "type": "turn.completed",
                            "eventId": event_id,
                            "turnId": current,
                            "payload": payload,
                        });
                        let boundary = self.canonical_event(params.clone());
                        self.emit_canonical(params);
                        self.turn_tracker.observe(&boundary);
                        self.retire_turn(&current);
                        self.emit_lifecycle("turn.completed");
                        false
                    }
                    Some("failed") | Some("interrupted") => {
                        let reason = self.turn_failure_reason(turn, status.unwrap_or("failed"));
                        self.record_turn_failure(reason.clone());
                        let params = serde_json::json!({
                            "type": "turn.failed",
                            "eventId": event_id,
                            "turnId": current,
                            "payload": {"reason_code": reason},
                        });
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
                let error = params
                    .get("error")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let info = error
                    .get("codexErrorInfo")
                    .and_then(|value| value.as_str())
                    .unwrap_or("codex_error");
                let message = error
                    .get("message")
                    .and_then(|value| value.as_str())
                    .unwrap_or("codex reported a turn error");
                self.record_turn_failure(format!("{info}: {message}"));
                true
            }
            "mcpServer/startupStatus/updated" => {
                if params.get("status").and_then(|value| value.as_str()) == Some("failed") {
                    let name = params
                        .get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown");
                    let error = params
                        .get("error")
                        .and_then(|value| value.as_str())
                        .unwrap_or("startup failed");
                    // An unrelated MCP startup failure stays diagnostic: it
                    // never fails the turn by itself.
                    self.record_mcp_failure(name, error);
                }
                true
            }
            _ => true,
        }
    }

    fn final_text(&self, turn: &serde_json::Value) -> Option<String> {
        if let Some(item_id) = self.last_message_item.lock().unwrap().clone() {
            if let Some(text) = self.item_text(&item_id) {
                return Some(text);
            }
        }
        turn.get("items")?
            .as_array()?
            .iter()
            .find(|item| item.get("type").and_then(|value| value.as_str()) == Some("agentMessage"))
            .and_then(|item| item.get("text"))
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    }

    fn turn_failure_reason(&self, turn: &serde_json::Value, status: &str) -> String {
        if let Some(recorded) = self.turn_failure.lock().unwrap().clone() {
            return recorded;
        }
        let message = turn
            .pointer("/error/message")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("codex turn ended with status {status}"));
        let bounded: String = message.chars().take(256).collect();
        bounded
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
