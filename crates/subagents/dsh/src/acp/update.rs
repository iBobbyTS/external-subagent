//! DSH `session/update` parsing and normalization into the internal
//! canonical event envelope.
//!
//! Progress, thought, and tool updates are kept separate from the final
//! message (P06): thought chunks project only as reasoning, and assistant
//! text always projects as a text delta. The prompt settlement is the only
//! authority that promotes aggregated text into a result — the standard ACP
//! flow provides neither a committed flag nor a settlement `messageId`, so
//! this module must not require either. Two observed discriminator dialects
//! are accepted: the pinned fixture shape (`update.type`) and the upstream
//! standard shape (`update.sessionUpdate`).
//!
//! Tool facts follow the upstream public contract: `title` is the tool's real
//! name and `kind` is only its category (`other` for a generic lifecycle), so
//! a tool is never named after its category. A call's execution phase comes
//! from its native `status`; the presence of a `content` array is not
//! completion or success evidence.

use external_contract::activity::{tool_result_event, tool_started_event};
use serde_json::{json, Value};

/// Parsed `session/update`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUpdate {
    pub session_id: Option<String>,
    pub kind: UpdateKind,
}

/// Native tool execution status (the ACP `status` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl ToolStatus {
    fn parse(value: Option<&Value>) -> Option<Self> {
        match value.and_then(Value::as_str)? {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            // An unrecognized status proves nothing, so it maps to no phase
            // instead of being guessed into one.
            _ => None,
        }
    }
}

/// The detailed tool-activity phase a native status projects onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPhase {
    Scheduled,
    Started,
    Completed,
    Failed,
}

impl ToolPhase {
    fn activity_kind(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Started => "started",
            Self::Completed => "result",
            Self::Failed => "error",
        }
    }
}

/// Project a native status onto an activity phase.
///
/// `announcement` marks the initial `tool_call`: it declares that the tool is
/// being used even when the frame omits `status`. A `tool_call_update` without
/// a recognized status carries no phase at all — in particular a content array
/// never becomes completion evidence.
fn tool_phase(announcement: bool, status: Option<ToolStatus>) -> Option<ToolPhase> {
    match status {
        Some(ToolStatus::Pending) => Some(ToolPhase::Scheduled),
        Some(ToolStatus::InProgress) => Some(ToolPhase::Started),
        Some(ToolStatus::Completed) => Some(ToolPhase::Completed),
        Some(ToolStatus::Failed) => Some(ToolPhase::Failed),
        None if announcement => Some(ToolPhase::Started),
        None => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateKind {
    /// Assistant message text. `committed` is true only for the pinned
    /// `agent_message` shape that carries a verified `messageId`; it only
    /// marks that the pinned dialect closed the message, and settlement stays
    /// the result authority either way.
    AgentMessage {
        message_id: Option<String>,
        text: String,
        committed: bool,
    },
    AgentThoughtChunk {
        /// None records an extraction failure; Some("") is a valid empty chunk.
        text: Option<String>,
    },
    ToolCall {
        tool_call_id: String,
        /// The real tool name (the ACP `title`).
        title: Option<String>,
        /// The tool category only — never used as the name.
        tool_kind: Option<String>,
        status: Option<ToolStatus>,
    },
    ToolCallUpdate {
        tool_call_id: String,
        status: Option<ToolStatus>,
    },
    /// Observed but untracked update (usage, config option, unknown kind).
    Untracked,
}

/// Best-effort bounded text of a content block.
fn block_text(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(text) => Some(text.as_str()),
        Value::Object(object) => object
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| object.get("delta").and_then(Value::as_str)),
        _ => None,
    }?;
    // U+0000 is legal JSON body text. Preserve it through assistant output
    // instead of silently discarding the entire containing block.
    Some(text.to_owned())
}

/// Preserve malformed assistant text as a parse failure, distinct from a
/// legal empty string or an explicitly non-text ACP content block.
fn assistant_block_text(content: &Value) -> Option<String> {
    if let Value::Object(object) = content {
        if object
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind != "text")
        {
            return Some(String::new());
        }
        // Do not let a valid fallback delta hide a damaged text field.
        if let Some(text) = object.get("text") {
            return text.as_str().map(str::to_owned);
        }
    }
    block_text(content)
}

/// Join assistant blocks in order. Any malformed text block invalidates the
/// message instead of silently delivering only its usable neighbors.
fn joined_content(update: &Value) -> Option<String> {
    let content = update.get("content")?;
    match content {
        Value::Array(blocks) => {
            let mut text = String::new();
            for block in blocks {
                text.push_str(&assistant_block_text(block)?);
            }
            Some(text)
        }
        single => assistant_block_text(single),
    }
}

fn bounded_id(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty() && text.len() <= 512 && !text.contains('\0'))
        .map(str::to_owned)
}

/// Parse one `session/update` notification payload (`params` of the frame).
pub fn parse_update(params: &Value) -> Option<SessionUpdate> {
    let update = params.get("update")?;
    let discriminator = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .or_else(|| update.get("type").and_then(Value::as_str))?;
    let session_id = bounded_id(params.get("sessionId"));
    let kind = match discriminator {
        "agent_message" => UpdateKind::AgentMessage {
            message_id: bounded_id(update.get("messageId")),
            text: joined_content(update)?,
            committed: true,
        },
        "agent_message_chunk" => UpdateKind::AgentMessage {
            message_id: bounded_id(update.get("messageId")),
            text: assistant_block_text(update.get("content")?)?,
            committed: false,
        },
        "agent_thought_chunk" | "agent_thought" | "thought" => UpdateKind::AgentThoughtChunk {
            text: block_text(update.get("content").unwrap_or(&Value::Null))
                .filter(|text| !text.contains('\0')),
        },
        "tool_call" => UpdateKind::ToolCall {
            tool_call_id: bounded_id(update.get("toolCallId"))?,
            title: bounded_id(update.get("title")),
            tool_kind: bounded_id(update.get("kind")),
            status: ToolStatus::parse(update.get("status")),
        },
        "tool_call_update" => UpdateKind::ToolCallUpdate {
            tool_call_id: bounded_id(update.get("toolCallId"))?,
            status: ToolStatus::parse(update.get("status")),
        },
        _ => UpdateKind::Untracked,
    };
    Some(SessionUpdate { session_id, kind })
}

/// Name used for tool grouping in projections: the real tool name from the
/// ACP `title`. The category (`kind`) is not a name, so it is never used here.
pub fn tool_name(kind: &UpdateKind) -> Option<String> {
    match kind {
        UpdateKind::ToolCall { title, .. } => title
            .clone()
            .map(|name| name.chars().take(64).collect::<String>()),
        UpdateKind::ToolCallUpdate { .. } => None,
        _ => None,
    }
}

/// Legacy projection of one update into canonical `session/event` payloads.
///
/// The production DSH pump no longer calls this: `external-daemon` decodes
/// `session/update` into typed facts through the shared ingress, and the
/// daemon's own canonical constructor is retired. The shape is retained only
/// as a fixture source for the shared tracker's still-live legacy JSON parser
/// tests; it is not a second production interpreter and nothing in the DSH
/// path may consume it again.
///
/// - message/thought text projects as `model.streaming` deltas (thoughts as
///   `reasoning_delta`, text as `text_delta` with the assistant message id);
/// - an announced tool projects both as the observation-verified
///   `model.streaming`/`tool_call` shape and the activity `tool.updated`
///   shape, because the two projections listen on different types;
/// - later tool progress carries only the activity phase, derived from the
///   native status.
///
/// Terminal settlement is deliberately not represented here: only the prompt
/// settlement can promote text into a result.
pub fn canonical_event_payloads(
    update: &SessionUpdate,
    event_id: &str,
    turn_id: &str,
) -> Vec<Value> {
    let base = |payload: Value| {
        json!({
            "type": "model.streaming",
            "eventId": event_id,
            "turnId": turn_id,
            "payload": payload,
        })
    };
    match &update.kind {
        UpdateKind::AgentMessage {
            message_id,
            text,
            committed,
        } => {
            let mut events = Vec::new();
            if !text.is_empty() {
                events.push(base(json!({
                    "kind": "text_delta",
                    "delta": text,
                    "assistantMessageId": message_id,
                })));
            }
            if *committed {
                events.push(base(json!({
                    "kind": "message_finished",
                    "assistantMessageId": message_id,
                })));
            }
            events
        }
        UpdateKind::AgentThoughtChunk { text } if text.as_deref() != Some("") => {
            // A null delta carries the failed extraction through the publisher
            // to observation's existing malformed-reasoning coverage accounting.
            vec![base(json!({"kind": "reasoning_delta", "delta": text}))]
        }
        UpdateKind::ToolCall {
            tool_call_id,
            status,
            ..
        } => {
            let name = tool_name(&update.kind);
            let Some(phase) = tool_phase(true, *status) else {
                return Vec::new();
            };
            let mut events = Vec::new();
            // Observation retains the announcement independently of execution.
            // Even a terminal announcement records identity/name; only
            // tool.updated carries the activity phase, without inventing start.
            events.push(base(json!({
                "kind": "tool_call",
                "toolCallId": tool_call_id,
                "toolName": name.clone().unwrap_or_else(|| "dsh_tool".into()),
            })));
            events.push(tool_activity_payload(
                event_id,
                turn_id,
                tool_call_id,
                phase,
                name.as_deref(),
            ));
            events
        }
        UpdateKind::ToolCallUpdate {
            tool_call_id,
            status,
        } => match tool_phase(false, *status) {
            Some(phase) => vec![tool_activity_payload(
                event_id,
                turn_id,
                tool_call_id,
                phase,
                None,
            )],
            // No recognized status: the call's end is not proven, so nothing
            // is claimed about its phase.
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// One detailed `tool.updated` payload in the shared activity vocabulary.
fn tool_activity_payload(
    event_id: &str,
    turn_id: &str,
    tool_call_id: &str,
    phase: ToolPhase,
    name: Option<&str>,
) -> Value {
    match phase {
        ToolPhase::Completed => tool_result_event(event_id, turn_id, tool_call_id),
        ToolPhase::Started => tool_started_event(event_id, turn_id, tool_call_id, name),
        // The shared vocabulary has no constructor for the remaining phases;
        // these are its exact documented shapes.
        other => {
            let mut payload = json!({
                "kind": other.activity_kind(),
                "toolCallId": tool_call_id,
            });
            if let Some(name) = name {
                payload["toolName"] = Value::String(name.to_owned());
            }
            json!({
                "type": "tool.updated",
                "eventId": event_id,
                "turnId": turn_id,
                "payload": payload,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_tool() -> Value {
        json!({
            "sessionId": "fixture-session",
            "update": {"type": "tool_call", "toolCallId": "tool-1", "title": "read-only probe"}
        })
    }

    fn fixture_message() -> Value {
        json!({
            "sessionId": "fixture-session",
            "update": {
                "type": "agent_message",
                "messageId": "message-1",
                "content": [{"type": "text", "text": "hi"}, {"type": "text", "text": "there"}]
            }
        })
    }

    fn upstream_chunk() -> Value {
        json!({
            "sessionId": "s",
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "partial"}}
        })
    }

    fn upstream_thought() -> Value {
        json!({
            "sessionId": "s",
            "update": {"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hmm"}}
        })
    }

    #[test]
    fn repair4_assistant_text_preserves_nul_without_changing_empty_or_thought_rules() {
        let nul_text = "important\0suffix";
        for update in [
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":nul_text}}),
            json!({"type":"agent_message","messageId":"native","content":[
                {"type":"text","text":""},{"type":"image","data":"ignored"},
                {"type":"text","text":nul_text}]}),
        ] {
            let parsed = parse_update(&json!({"sessionId":"s","update":update})).unwrap();
            let UpdateKind::AgentMessage { text, .. } = &parsed.kind else {
                panic!("assistant text");
            };
            assert_eq!(text, nul_text);
            let payloads = canonical_event_payloads(&parsed, "event", "turn");
            assert_eq!(payloads[0]["payload"]["delta"], nul_text);
        }
        for content in [
            json!({"type":"text","text":""}),
            json!({"type":"image","data":"ignored"}),
        ] {
            let parsed = parse_update(
                &json!({"update":{"sessionUpdate":"agent_message_chunk","content":content}}),
            )
            .unwrap();
            assert!(matches!(&parsed.kind, UpdateKind::AgentMessage { text, .. } if text.is_empty()));
            assert!(canonical_event_payloads(&parsed, "empty", "turn").is_empty());
        }
        let thought = parse_update(&json!({"update":{"sessionUpdate":"agent_thought_chunk",
            "content":{"type":"text","text":nul_text}}}))
        .unwrap();
        assert_eq!(thought.kind, UpdateKind::AgentThoughtChunk { text: None });
    }

    #[test]
    fn both_dialects_parse_and_keep_message_identity() {
        let tool = parse_update(&fixture_tool()).unwrap();
        assert_eq!(
            tool.kind,
            UpdateKind::ToolCall {
                tool_call_id: "tool-1".into(),
                title: Some("read-only probe".into()),
                tool_kind: None,
                status: None,
            }
        );
        let message = parse_update(&fixture_message()).unwrap();
        assert_eq!(
            message.kind,
            UpdateKind::AgentMessage {
                message_id: Some("message-1".into()),
                text: "hithere".into(),
                committed: true,
            }
        );
        let chunk = parse_update(&upstream_chunk()).unwrap();
        assert_eq!(
            chunk.kind,
            UpdateKind::AgentMessage {
                message_id: None,
                text: "partial".into(),
                committed: false,
            }
        );
        let thought = parse_update(&upstream_thought()).unwrap();
        assert_eq!(
            thought.kind,
            UpdateKind::AgentThoughtChunk {
                text: Some("hmm".into())
            }
        );
    }

    #[test]
    fn unknown_or_incomplete_updates_stay_untracked() {
        assert_eq!(
            parse_update(&json!({"update": {"sessionUpdate": "usage_update"}}))
                .unwrap()
                .kind,
            UpdateKind::Untracked
        );
        // tool_call without a toolCallId is not a usable identity.
        assert!(parse_update(&json!({"update": {"type": "tool_call", "title": "x"}})).is_none());
        assert!(parse_update(&json!({})).is_none());
    }

    #[test]
    fn canonical_payloads_separate_text_thought_and_tool() {
        let message = parse_update(&fixture_message()).unwrap();
        let payloads = canonical_event_payloads(&message, "evt-1", "turn-1");
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[0]["type"], "model.streaming");
        assert_eq!(payloads[0]["payload"]["kind"], "text_delta");
        assert_eq!(payloads[0]["payload"]["delta"], "hithere");
        assert_eq!(payloads[0]["payload"]["assistantMessageId"], "message-1");
        assert_eq!(payloads[1]["payload"]["kind"], "message_finished");

        let thought = parse_update(&upstream_thought()).unwrap();
        let payloads = canonical_event_payloads(&thought, "evt-2", "turn-1");
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["payload"]["kind"], "reasoning_delta");

        let tool = parse_update(&fixture_tool()).unwrap();
        let payloads = canonical_event_payloads(&tool, "evt-3", "turn-1");
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[0]["payload"]["kind"], "tool_call");
        assert_eq!(payloads[0]["payload"]["toolName"], "read-only probe");
        assert_eq!(payloads[1]["type"], "tool.updated");
        assert_eq!(payloads[1]["payload"]["kind"], "started");
        assert_eq!(payloads[1]["payload"]["toolName"], "read-only probe");

        // Uncommitted chunks never emit a finished boundary.
        let chunk = parse_update(&upstream_chunk()).unwrap();
        let payloads = canonical_event_payloads(&chunk, "evt-4", "turn-1");
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["payload"]["kind"], "text_delta");
    }

    #[test]
    fn canonical_tool_activity_payloads_are_exact_literal_shapes() {
        // Named started: the observation companion (`model.streaming`/
        // `tool_call`) plus the shared detailed activity event, asserted as
        // hand-written JSON (never generated from the constructors).
        let named = parse_update(&json!({
            "sessionId": "s",
            "update": {"sessionUpdate": "tool_call", "toolCallId": "call-1", "title": "read"}
        }))
        .unwrap();
        assert_eq!(
            canonical_event_payloads(&named, "evt-1", "turn-1"),
            vec![
                json!({
                    "type": "model.streaming", "eventId": "evt-1", "turnId": "turn-1",
                    "payload": {"kind": "tool_call", "toolCallId": "call-1", "toolName": "read"},
                }),
                json!({
                    "type": "tool.updated", "eventId": "evt-1", "turnId": "turn-1",
                    "payload": {"kind": "started", "toolCallId": "call-1", "toolName": "read"},
                }),
            ]
        );

        // Unnamed announced started: no toolName field at all (omitted, not
        // null) on the activity event; the observation sibling keeps its
        // stable grouping fallback.
        let unnamed = parse_update(&json!({
            "sessionId": "s",
            "update": {"sessionUpdate": "tool_call", "toolCallId": "call-2"}
        }))
        .unwrap();
        assert_eq!(
            canonical_event_payloads(&unnamed, "evt-2", "turn-1"),
            vec![
                json!({
                    "type": "model.streaming", "eventId": "evt-2", "turnId": "turn-1",
                    "payload": {"kind": "tool_call", "toolCallId": "call-2", "toolName": "dsh_tool"},
                }),
                json!({
                    "type": "tool.updated", "eventId": "evt-2", "turnId": "turn-1",
                    "payload": {"kind": "started", "toolCallId": "call-2"},
                }),
            ]
        );

        // Completed: the native status is the only completion evidence, and
        // the detailed result vocabulary carries no toolName.
        let completed = parse_update(&json!({
            "sessionId": "s",
            "update": {
                "sessionUpdate": "tool_call_update", "toolCallId": "call-3",
                "status": "completed", "content": [{"type": "text", "text": "ok"}]
            }
        }))
        .unwrap();
        assert_eq!(
            canonical_event_payloads(&completed, "evt-3", "turn-1"),
            vec![json!({
                "type": "tool.updated", "eventId": "evt-3", "turnId": "turn-1",
                "payload": {"kind": "result", "toolCallId": "call-3"},
            })]
        );
    }

    #[test]
    fn tool_status_drives_the_phase_and_content_is_never_success_evidence() {
        let phases = |update: Value| {
            let parsed = parse_update(&update).unwrap();
            canonical_event_payloads(&parsed, "evt", "turn-1")
                .iter()
                .filter(|event| event["type"] == "tool.updated")
                .map(|event| event["payload"]["kind"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let update = |status: &str| {
            json!({
                "sessionId": "s",
                "update": {
                    "sessionUpdate": "tool_call_update", "toolCallId": "call-1",
                    "status": status
                }
            })
        };
        assert_eq!(phases(update("pending")), vec!["scheduled"]);
        assert_eq!(phases(update("in_progress")), vec!["started"]);
        assert_eq!(phases(update("completed")), vec!["result"]);
        // A failed tool is a failure, never a completed call.
        assert_eq!(phases(update("failed")), vec!["error"]);

        // A content array without a terminal status proves nothing: no phase
        // is invented, so the call is never reported as completed.
        let content_only = json!({
            "sessionId": "s",
            "update": {
                "sessionUpdate": "tool_call_update", "toolCallId": "call-2",
                "content": [{"type": "text", "text": "looks finished"}]
            }
        });
        assert!(phases(content_only).is_empty());
        // An unknown status is not guessed into a phase either.
        assert!(phases(update("queued")).is_empty());
    }

    #[test]
    fn announced_tool_status_projects_onto_the_matching_phase() {
        let phases = |status: Value| {
            let parsed = parse_update(&json!({
                "sessionId": "s",
                "update": {
                    "sessionUpdate": "tool_call", "toolCallId": "call-1",
                    "title": "read a file", "kind": "other", "status": status
                }
            }))
            .unwrap();
            canonical_event_payloads(&parsed, "evt", "turn-1")
                .iter()
                .filter(|event| event["type"] == "tool.updated")
                .map(|event| event["payload"]["kind"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(phases(json!("pending")), vec!["scheduled"]);
        assert_eq!(phases(json!("in_progress")), vec!["started"]);
        assert_eq!(phases(json!("completed")), vec!["result"]);
        assert_eq!(phases(json!("failed")), vec!["error"]);
    }

    #[test]
    fn tool_names_come_from_the_title_never_from_the_category() {
        // The upstream generic lifecycle always sends kind="other"; naming the
        // tool after it would collapse every call into "other".
        let kind = parse_update(&json!({
            "update": {"sessionUpdate": "tool_call", "toolCallId": "t", "kind": "other", "title": "Edit file"}
        }))
        .unwrap();
        assert_eq!(tool_name(&kind.kind).as_deref(), Some("Edit file"));
        let title_only = parse_update(&fixture_tool()).unwrap();
        assert_eq!(
            tool_name(&title_only.kind).as_deref(),
            Some("read-only probe")
        );
        // A category alone is not a name; the announced event still keeps the
        // stable grouping fallback instead of claiming the category.
        let category_only = parse_update(&json!({
            "update": {"sessionUpdate": "tool_call", "toolCallId": "t", "kind": "other"}
        }))
        .unwrap();
        assert_eq!(tool_name(&category_only.kind), None);
        assert_eq!(
            canonical_event_payloads(&category_only, "evt", "turn-1")[0]["payload"]["toolName"],
            "dsh_tool"
        );
    }
}
