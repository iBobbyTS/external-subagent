//! Codex `item/*`, `turn/*`, `error`, and MCP-startup notification
//! folding: pure extraction of the projection inputs, construction of the
//! canonical `session/event` payloads and envelopes, and the bounded
//! aggregation helpers (item text, turn-failure detail, MCP diagnostic
//! tail, retired turns) the daemon applies to its shared state. Nothing
//! here holds daemon state or emits; the daemon owns attribution against
//! the live thread and turn and publishes what these functions build.

use std::collections::HashMap;

use external_contract::{classify_lifecycle, EventEnvelope, WireMessage, SESSION_EVENT};
use external_runtime::Inbound;

/// Hard bound on one aggregated agent-message item.
const MAX_ITEM_TEXT_BYTES: usize = 512 * 1024;
/// Hard bound on the number of tracked agent-message items.
const MAX_TRACKED_ITEMS: usize = 128;
/// Hard bound (Unicode chars) on one recorded turn-failure detail.
const MAX_TURN_FAILURE_DETAIL_BYTES: usize = 512;
/// Hard bound on the retained MCP startup-failure diagnostic tail.
const MAX_MCP_DIAGNOSTIC_BYTES: usize = 2 * 1024;
/// Hard bound on the retired-turn memory.
const MAX_RETIRED_TURNS: usize = 64;

/// Whether a notification method is turn-scoped: its traffic must carry
/// the active thread id and is only ever projected onto the current turn.
pub fn is_turn_scoped(method: &str) -> bool {
    matches!(
        method,
        "turn/started" | "item/agentMessage/delta" | "item/completed" | "turn/completed" | "error"
    )
}

/// The non-empty turn id a `turn/started` notification must name.
pub fn started_turn_id(params: &serde_json::Value) -> Option<&str> {
    params
        .pointer("/turn/id")
        .and_then(|value| value.as_str())
        .filter(|id| !id.is_empty())
}

/// The streaming delta (`delta`, `itemId`) an `item/agentMessage/delta`
/// notification must carry.
pub fn agent_message_delta(params: &serde_json::Value) -> Option<(&str, &str)> {
    let delta = params.get("delta").and_then(|value| value.as_str());
    let item_id = params.get("itemId").and_then(|value| value.as_str());
    Some((delta?, item_id?))
}

/// The completed agent-message item (`item_id`, optional `text`) an
/// `item/completed` notification must carry. Any other item type is not a
/// projection input.
pub fn completed_agent_message(params: &serde_json::Value) -> Option<(&str, Option<&str>)> {
    let item = params.get("item").unwrap_or(&serde_json::Value::Null);
    if item.get("type").and_then(|value| value.as_str()) != Some("agentMessage") {
        return None;
    }
    let item_id = item.get("id").and_then(|value| value.as_str())?;
    let text = item.get("text").and_then(|value| value.as_str());
    Some((item_id, text))
}

/// A `turn/completed` notification's terminal view of the turn.
pub struct CompletedTurn<'a> {
    pub turn: &'a serde_json::Value,
    pub status: Option<&'a str>,
    pub turn_id: Option<&'a str>,
}

/// Extract the `turn/completed` notification's turn object, status, and
/// id. Extraction never fails; attribution decides projection.
pub fn completed_turn(params: &serde_json::Value) -> CompletedTurn<'_> {
    let turn = params.get("turn").unwrap_or(&serde_json::Value::Null);
    let status = turn.get("status").and_then(|value| value.as_str());
    let turn_id = turn.get("id").and_then(|value| value.as_str());
    CompletedTurn {
        turn,
        status,
        turn_id,
    }
}

/// The final text of a completed turn: the tracked text of the turn's last
/// completed agent message when it exists, otherwise the first
/// agentMessage item the completed turn itself carries.
pub fn final_text(turn: &serde_json::Value, tracked: Option<String>) -> Option<String> {
    if tracked.is_some() {
        return tracked;
    }
    turn.get("items")?
        .as_array()?
        .iter()
        .find(|item| item.get("type").and_then(|value| value.as_str()) == Some("agentMessage"))
        .and_then(|item| item.get("text"))
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

/// The failure reason for a failed or interrupted turn: a recorded
/// turn-scoped error when one exists, otherwise the turn's own error
/// message (bounded), otherwise a status-derived default.
pub fn turn_failure_reason(
    turn: &serde_json::Value,
    status: &str,
    recorded: Option<String>,
) -> String {
    if let Some(recorded) = recorded {
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

/// The recorded detail line for a turn-scoped application error (for
/// example a serverOverloaded capacity failure): `codexErrorInfo: message`
/// with protocol defaults for either missing field. By itself it never
/// completes a task; only the boundary projection consumes it.
pub fn turn_error_detail(params: &serde_json::Value) -> String {
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
    format!("{info}: {message}")
}

/// A failed MCP server startup (`name`, `error`) with protocol defaults.
/// An unrelated MCP startup failure stays diagnostic: it never fails the
/// turn by itself.
pub fn mcp_startup_failure(params: &serde_json::Value) -> Option<(&str, &str)> {
    if params.get("status").and_then(|value| value.as_str()) != Some("failed") {
        return None;
    }
    let name = params
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let error = params
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("startup failed");
    Some((name, error))
}

/// The canonical event id for a projection sequence number.
pub fn event_id(sequence: u64) -> String {
    format!("codex-event-{sequence}")
}

/// The canonical `session/event` envelope for one projected payload.
pub fn canonical_event(params: serde_json::Value) -> Inbound {
    Inbound::Message(WireMessage::Event(EventEnvelope {
        method: SESSION_EVENT.into(),
        params,
    }))
}

/// The lifecycle observation record for a projected boundary.
pub fn lifecycle_event(sequence: u64, method: &str) -> Inbound {
    Inbound::Lifecycle {
        sequence,
        method: method.into(),
        order: classify_lifecycle(method, false),
    }
}

/// The canonical `turn.started` payload.
pub fn turn_started_payload(event_id: &str, turn_id: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "turn.started",
        "eventId": event_id,
        "turnId": turn_id,
    })
}

/// The canonical `model.streaming` payload for one agent-message delta.
pub fn model_streaming_payload(
    event_id: &str,
    turn_id: &str,
    delta: &str,
    item_id: &str,
) -> serde_json::Value {
    serde_json::json!({
        "type": "model.streaming",
        "eventId": event_id,
        "turnId": turn_id,
        "payload": {
            "kind": "text_delta",
            "delta": delta,
            "assistantMessageId": item_id,
        },
    })
}

/// The canonical `message.finished` payload for one completed
/// agent-message item.
pub fn message_finished_payload(event_id: &str, turn_id: &str, item_id: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "message.finished",
        "eventId": event_id,
        "turnId": turn_id,
        "payload": {"assistantMessageId": item_id},
    })
}

/// The canonical `turn.completed` boundary payload; `response` carries the
/// turn's final text when a verified message backs it.
pub fn turn_completed_payload(
    event_id: &str,
    turn_id: &str,
    final_text: Option<String>,
) -> serde_json::Value {
    let mut payload = serde_json::json!({});
    if let Some(text) = final_text {
        payload["response"] = serde_json::Value::String(text);
    }
    serde_json::json!({
        "type": "turn.completed",
        "eventId": event_id,
        "turnId": turn_id,
        "payload": payload,
    })
}

/// The canonical `turn.failed` boundary payload with the bounded failure
/// reason.
pub fn turn_failed_payload(event_id: &str, turn_id: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "turn.failed",
        "eventId": event_id,
        "turnId": turn_id,
        "payload": {"reason_code": reason},
    })
}

/// Append one delta to an agent-message item's tracked text. A new item is
/// refused once the tracking cap is reached; an existing item keeps
/// appending, and the total stays byte-bounded on char boundaries.
pub fn observe_item_text(items: &mut HashMap<String, String>, item_id: &str, text: &str) {
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

/// Record a completed agent-message item's full text. A new item is
/// refused once the tracking cap is reached; an existing item is always
/// refreshed.
pub fn record_completed_item(items: &mut HashMap<String, String>, item_id: &str, text: &str) {
    if items.contains_key(item_id) || items.len() < MAX_TRACKED_ITEMS {
        items.insert(item_id.to_owned(), text.to_owned());
    }
}

/// Bound one recorded turn-failure detail (Unicode chars).
pub fn bounded_turn_failure_detail(detail: &str) -> String {
    detail.chars().take(MAX_TURN_FAILURE_DETAIL_BYTES).collect()
}

/// Append one MCP startup-failure line to the diagnostic tail and keep the
/// tail byte-bounded on char boundaries (the most recent lines survive).
pub fn record_mcp_failure(tail: &mut String, name: &str, error: &str) {
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

/// Retire one turn: its identity can never become current again. The ring
/// is deduplicated and bounded; the oldest entry is evicted first.
pub fn retire_turn(retired: &mut Vec<String>, turn_id: &str) {
    if !retired.iter().any(|id| id == turn_id) {
        if retired.len() >= MAX_RETIRED_TURNS {
            retired.remove(0);
        }
        retired.push(turn_id.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_scoped_methods_are_the_five_projection_inputs() {
        for method in [
            "turn/started",
            "item/agentMessage/delta",
            "item/completed",
            "turn/completed",
            "error",
        ] {
            assert!(is_turn_scoped(method), "{method}");
        }
        assert!(!is_turn_scoped("mcpServer/startupStatus/updated"));
        assert!(!is_turn_scoped("thread/started"));
    }

    #[test]
    fn started_turn_id_requires_a_non_empty_id() {
        let params = serde_json::json!({"turn": {"id": "turn-1"}});
        assert_eq!(started_turn_id(&params), Some("turn-1"));
        assert_eq!(
            started_turn_id(&serde_json::json!({"turn": {"id": ""}})),
            None
        );
        assert_eq!(started_turn_id(&serde_json::json!({})), None);
    }

    #[test]
    fn agent_message_delta_requires_both_fields() {
        let params = serde_json::json!({"delta": "hello", "itemId": "item-1"});
        assert_eq!(agent_message_delta(&params), Some(("hello", "item-1")));
        assert_eq!(
            agent_message_delta(&serde_json::json!({"delta": "hello"})),
            None
        );
        assert_eq!(
            agent_message_delta(&serde_json::json!({"itemId": "item-1"})),
            None
        );
    }

    #[test]
    fn completed_agent_message_accepts_only_agent_messages_with_ids() {
        let params =
            serde_json::json!({"item": {"type": "agentMessage", "id": "item-1", "text": "hi"}});
        assert_eq!(
            completed_agent_message(&params),
            Some(("item-1", Some("hi")))
        );
        let no_text = serde_json::json!({"item": {"type": "agentMessage", "id": "item-1"}});
        assert_eq!(completed_agent_message(&no_text), Some(("item-1", None)));
        let other_type = serde_json::json!({"item": {"type": "commandExecution", "id": "c-1"}});
        assert_eq!(completed_agent_message(&other_type), None);
        let no_id = serde_json::json!({"item": {"type": "agentMessage"}});
        assert_eq!(completed_agent_message(&no_id), None);
    }

    #[test]
    fn final_text_prefers_the_tracked_item_then_scans_the_turn() {
        let turn = serde_json::json!({"items": [
            {"type": "agentMessage", "text": "from items"},
        ]});
        assert_eq!(
            final_text(&turn, Some("tracked".into())),
            Some("tracked".into())
        );
        assert_eq!(final_text(&turn, None), Some("from items".into()));
        assert_eq!(final_text(&serde_json::json!({}), None), None);
        let no_agent_message = serde_json::json!({"items": [
            {"type": "commandExecution", "text": "not a message"},
        ]});
        assert_eq!(final_text(&no_agent_message, None), None);
    }

    #[test]
    fn turn_failure_reason_prefers_the_recorded_detail() {
        assert_eq!(
            turn_failure_reason(&serde_json::json!({}), "failed", Some("recorded".into())),
            "recorded"
        );
        let error_turn = serde_json::json!({"error": {"message": "capacity exhausted"}});
        assert_eq!(
            turn_failure_reason(&error_turn, "failed", None),
            "capacity exhausted"
        );
        assert_eq!(
            turn_failure_reason(&serde_json::json!({}), "interrupted", None),
            "codex turn ended with status interrupted"
        );
        let long = "é".repeat(300);
        let bounded_turn = serde_json::json!({"error": {"message": long}});
        assert_eq!(
            turn_failure_reason(&bounded_turn, "failed", None)
                .chars()
                .count(),
            256
        );
    }

    #[test]
    fn turn_error_detail_applies_the_protocol_defaults() {
        let params = serde_json::json!({
            "error": {"codexErrorInfo": "serverOverloaded", "message": "try again"}
        });
        assert_eq!(turn_error_detail(&params), "serverOverloaded: try again");
        assert_eq!(
            turn_error_detail(&serde_json::json!({})),
            "codex_error: codex reported a turn error"
        );
        assert_eq!(
            turn_error_detail(&serde_json::json!({"error": {"message": "boom"}})),
            "codex_error: boom"
        );
    }

    #[test]
    fn mcp_startup_failure_extracts_only_failed_startups() {
        let params =
            serde_json::json!({"status": "failed", "name": "search", "error": "no binary"});
        assert_eq!(mcp_startup_failure(&params), Some(("search", "no binary")));
        assert_eq!(
            mcp_startup_failure(&serde_json::json!({"status": "failed"})),
            Some(("unknown", "startup failed"))
        );
        assert_eq!(
            mcp_startup_failure(&serde_json::json!({"status": "ready"})),
            None
        );
    }

    #[test]
    fn event_ids_and_canonical_envelopes_have_the_pinned_shapes() {
        assert_eq!(event_id(7), "codex-event-7");
        let params = serde_json::json!({"type": "turn.started"});
        match canonical_event(params.clone()) {
            Inbound::Message(WireMessage::Event(envelope)) => {
                assert_eq!(envelope.method, SESSION_EVENT);
                assert_eq!(envelope.params, params);
            }
            other => panic!("canonical event is not an event envelope: {other:?}"),
        }
        match lifecycle_event(3, "turn.started") {
            Inbound::Lifecycle {
                sequence,
                method,
                order,
            } => {
                assert_eq!(sequence, 3);
                assert_eq!(method, "turn.started");
                assert_eq!(order, classify_lifecycle("turn.started", false));
            }
            other => panic!("lifecycle record has the wrong shape: {other:?}"),
        }
    }

    #[test]
    fn canonical_payloads_match_the_internal_event_shapes() {
        assert_eq!(
            turn_started_payload("codex-event-1", "turn-1"),
            serde_json::json!({
                "type": "turn.started",
                "eventId": "codex-event-1",
                "turnId": "turn-1",
            })
        );
        assert_eq!(
            model_streaming_payload("codex-event-2", "turn-1", "hello", "item-1"),
            serde_json::json!({
                "type": "model.streaming",
                "eventId": "codex-event-2",
                "turnId": "turn-1",
                "payload": {
                    "kind": "text_delta",
                    "delta": "hello",
                    "assistantMessageId": "item-1",
                },
            })
        );
        assert_eq!(
            message_finished_payload("codex-event-3", "turn-1", "item-1"),
            serde_json::json!({
                "type": "message.finished",
                "eventId": "codex-event-3",
                "turnId": "turn-1",
                "payload": {"assistantMessageId": "item-1"},
            })
        );
        assert_eq!(
            turn_completed_payload("codex-event-4", "turn-1", Some("answer".into())),
            serde_json::json!({
                "type": "turn.completed",
                "eventId": "codex-event-4",
                "turnId": "turn-1",
                "payload": {"response": "answer"},
            })
        );
        assert_eq!(
            turn_completed_payload("codex-event-5", "turn-1", None)["payload"],
            serde_json::json!({})
        );
        assert_eq!(
            turn_failed_payload("codex-event-6", "turn-1", "serverOverloaded: boom"),
            serde_json::json!({
                "type": "turn.failed",
                "eventId": "codex-event-6",
                "turnId": "turn-1",
                "payload": {"reason_code": "serverOverloaded: boom"},
            })
        );
    }

    #[test]
    fn item_text_aggregation_is_capped_and_bounded() {
        let mut items = HashMap::new();
        for index in 0..MAX_TRACKED_ITEMS {
            items.insert(format!("item-{index}"), String::new());
        }
        observe_item_text(&mut items, "item-new", "refused");
        assert!(!items.contains_key("item-new"));
        observe_item_text(&mut items, "item-0", "appended");
        assert_eq!(items.get("item-0").map(String::as_str), Some("appended"));

        let mut single = HashMap::new();
        // 512 KiB + 4 bytes of two-byte chars: the append trims back to the
        // bound on a char boundary.
        let overflow = "é".repeat(MAX_ITEM_TEXT_BYTES / 2 + 2);
        observe_item_text(&mut single, "big", &overflow);
        assert_eq!(single["big"].len(), MAX_ITEM_TEXT_BYTES);
        observe_item_text(&mut single, "big", "more");
        assert_eq!(single["big"].len(), MAX_ITEM_TEXT_BYTES);
    }

    #[test]
    fn completed_items_refresh_existing_entries_and_respect_the_cap() {
        let mut items = HashMap::new();
        record_completed_item(&mut items, "item-1", "text");
        assert_eq!(items.get("item-1").map(String::as_str), Some("text"));
        record_completed_item(&mut items, "item-1", "replaced");
        assert_eq!(items.get("item-1").map(String::as_str), Some("replaced"));
        for index in 0..MAX_TRACKED_ITEMS {
            items.insert(format!("existing-{index}"), String::new());
        }
        record_completed_item(&mut items, "item-new", "refused");
        assert!(!items.contains_key("item-new"));
    }

    #[test]
    fn turn_failure_detail_is_char_bounded() {
        let detail: String = "é".repeat(MAX_TURN_FAILURE_DETAIL_BYTES + 10);
        let bounded = bounded_turn_failure_detail(&detail);
        assert_eq!(bounded.chars().count(), MAX_TURN_FAILURE_DETAIL_BYTES);
    }

    #[test]
    fn mcp_tail_is_byte_bounded_and_keeps_recent_lines() {
        let mut tail = String::new();
        for index in 0..200 {
            record_mcp_failure(&mut tail, &format!("server-{index}"), "boom");
        }
        assert!(tail.len() <= MAX_MCP_DIAGNOSTIC_BYTES);
        record_mcp_failure(&mut tail, "newest", "still visible");
        assert!(
            tail.ends_with("mcp newest failed: still visible\n"),
            "{tail}"
        );
    }

    #[test]
    fn retired_turns_are_deduplicated_and_bounded() {
        let mut retired = Vec::new();
        retire_turn(&mut retired, "turn-1");
        retire_turn(&mut retired, "turn-1");
        assert_eq!(retired.len(), 1);
        for index in 0..MAX_RETIRED_TURNS {
            retire_turn(&mut retired, &format!("turn-{index}"));
        }
        assert_eq!(retired.len(), MAX_RETIRED_TURNS);
        retire_turn(&mut retired, "turn-new");
        assert_eq!(retired.len(), MAX_RETIRED_TURNS);
        assert!(!retired.iter().any(|id| id == "turn-1"));
        assert!(retired.iter().any(|id| id == "turn-new"));
    }
}
