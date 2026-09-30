//! Shared tool-activity vocabulary for producers of the daemon's 60-second
//! wait window.
//!
//! Two event vocabularies feed the window over the generic `session/event`
//! stream:
//!
//! - the **detailed** vocabulary (`tool.updated`/`streamRecovery.updated` with
//!   `payload.kind` one of `scheduled`/`started`/`result`/`error`/`batch`)
//!   names one tool call (`toolCallId`) and optionally its `toolName`. Its
//!   sample identity is `tool:{toolCallId}:{phase}`;
//! - the **count-only** vocabulary (`tool.updated`/`streamRecovery.updated`
//!   with `payload.kind == "count"`) carries a pure initiation weight and optional
//!   tool descriptors. Its sample identity is `tool:{eventId}:count`.
//!
//! Both identities are bounded and re-delivery idempotent at the consumer: an
//! already-admitted identity neither adds weight nor refreshes the sample's
//! receipt time. [`MAX_TOOL_COUNT`] is the single upper bound for one
//! count-only event; a count of `0` or above the bound is treated as malformed
//! by the daemon parser.
//!
//! # Purity
//!
//! These constructors are pure serialization helpers: they build the
//! canonical wire shape from whatever arguments they are given and never
//! validate, truncate, or error. Legality is enforced unilaterally by the
//! daemon parser, and the rules it enforces are bounded identities (a
//! non-empty, NUL-free id within the parser's byte cap) and
//! `1 <= count <= MAX_TOOL_COUNT`; a producer that emits an out-of-range
//! count or an unbounded id simply has that event rejected rather than
//! mis-serialized. Tool names are not validated here either: the parser only
//! classifies a `toolName` as Read/Bash/Other and accepts a missing or empty
//! name as Other. (Name validation belongs to the separate observation
//! vocabulary, not this activity contract.)

use serde_json::Value;

/// Upper bound on one count-only tool event's weight; the daemon parser
/// rejects `0` and any value above this bound so a hostile or confused
/// producer cannot inflate the 60-second window with a single frame.
pub const MAX_TOOL_COUNT: u64 = 1024;

/// Build the count-only `tool.updated` event carrying `count` tool
/// initiations.
///
/// 可选 tool/detail 仅描述调用，不参与计数身份；缺省时保持旧 wire 形状。
pub fn tool_count_event(
    event_id: &str,
    turn_id: &str,
    count: u64,
    tool: Option<&str>,
    detail: Option<&str>,
) -> Value {
    let mut payload = serde_json::json!({"kind": "count", "count": count});
    if let Some(tool) = tool {
        payload["tool"] = tool.into();
    }
    if let Some(detail) = detail {
        payload["detail"] = detail.into();
    }
    serde_json::json!({
        "type": "tool.updated",
        "eventId": event_id,
        "turnId": turn_id,
        "payload": payload,
    })
}

/// Build the detailed `tool.updated` started event for one tool call.
///
/// `tool_name` is optional because the DSH update dialect can report a call
/// without a name; when it is `None` the `toolName` field is omitted entirely
/// (never serialized as `null`).
pub fn tool_started_event(
    event_id: &str,
    turn_id: &str,
    tool_call_id: &str,
    tool_name: Option<&str>,
) -> Value {
    let mut payload = serde_json::json!({
        "kind": "started",
        "toolCallId": tool_call_id,
    });
    if let Some(tool_name) = tool_name {
        payload["toolName"] = Value::String(tool_name.to_owned());
    }
    serde_json::json!({
        "type": "tool.updated",
        "eventId": event_id,
        "turnId": turn_id,
        "payload": payload,
    })
}

/// Build the detailed `tool.updated` result event for one tool call. The
/// detailed result vocabulary carries no `toolName`.
pub fn tool_result_event(event_id: &str, turn_id: &str, tool_call_id: &str) -> Value {
    serde_json::json!({
        "type": "tool.updated",
        "eventId": event_id,
        "turnId": turn_id,
        "payload": {"kind": "result", "toolCallId": tool_call_id},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_event_has_the_pinned_count_only_shape() {
        // Hand-written literal: the count vocabulary is exactly this shape,
        // with the params-level eventId as the only identity input.
        assert_eq!(
            tool_count_event("codex-event-9", "turn-1", 3, None, None),
            serde_json::json!({
                "type": "tool.updated",
                "eventId": "codex-event-9",
                "turnId": "turn-1",
                "payload": {"kind": "count", "count": 3},
            })
        );
    }

    #[test]
    fn count_event_descriptors_are_additive_and_optional() {
        let value = tool_count_event("e", "t", 1, Some("commandExecution"), Some("echo 中"));
        assert_eq!(
            value["payload"],
            serde_json::json!({"kind":"count","count":1,"tool":"commandExecution","detail":"echo 中"})
        );
        let no_detail = tool_count_event("e", "t", 1, Some("fileChange"), None);
        assert!(no_detail["payload"].get("detail").is_none());
        let old = tool_count_event("e", "t", 1, None, None);
        assert!(old["payload"].get("tool").is_none());
    }

    #[test]
    fn started_event_carries_an_optional_name() {
        assert_eq!(
            tool_started_event("evt-1", "turn-1", "call-1", Some("Read")),
            serde_json::json!({
                "type": "tool.updated",
                "eventId": "evt-1",
                "turnId": "turn-1",
                "payload": {"kind": "started", "toolCallId": "call-1", "toolName": "Read"},
            })
        );
        // No name: the field is omitted, not null (the DSH unnamed started
        // shape).
        let unnamed = tool_started_event("evt-2", "turn-1", "call-2", None);
        assert_eq!(
            unnamed,
            serde_json::json!({
                "type": "tool.updated",
                "eventId": "evt-2",
                "turnId": "turn-1",
                "payload": {"kind": "started", "toolCallId": "call-2"},
            })
        );
        assert!(unnamed["payload"].get("toolName").is_none());
    }

    #[test]
    fn result_event_has_the_pinned_detailed_shape() {
        assert_eq!(
            tool_result_event("evt-3", "turn-1", "call-3"),
            serde_json::json!({
                "type": "tool.updated",
                "eventId": "evt-3",
                "turnId": "turn-1",
                "payload": {"kind": "result", "toolCallId": "call-3"},
            })
        );
    }

    #[test]
    fn constructors_are_pure_and_do_not_validate() {
        // Out-of-range counts serialize verbatim; the parser is the single
        // legality gate.
        assert_eq!(
            tool_count_event("e", "t", 0, None, None)["payload"]["count"],
            0
        );
        assert_eq!(
            tool_count_event("e", "t", MAX_TOOL_COUNT + 1, None, None)["payload"]["count"],
            MAX_TOOL_COUNT + 1
        );
    }
}
