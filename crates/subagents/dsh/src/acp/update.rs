//! DSH `session/update` parsing and normalization into the internal
//! canonical event envelope.
//!
//! Progress, thought, and tool updates are kept separate from the final
//! message (P06): thought chunks and uncommitted message chunks only ever
//! project as progress/reasoning; only a committed agent message with a real
//! `messageId` is eligible as final text. Two observed discriminator
//! dialects are accepted: the S01-pinned fixture shape (`update.type`) and
//! the upstream standard shape (`update.sessionUpdate`).

use serde_json::{json, Value};

/// Parsed `session/update`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUpdate {
    pub session_id: Option<String>,
    pub kind: UpdateKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateKind {
    /// Assistant message text. `committed` is true only for the S01-pinned
    /// `agent_message` shape that carries a verified `messageId`.
    AgentMessage {
        message_id: Option<String>,
        text: String,
        committed: bool,
    },
    AgentThoughtChunk {
        text: String,
    },
    ToolCall {
        tool_call_id: String,
        title: Option<String>,
        tool_kind: Option<String>,
    },
    ToolCallUpdate {
        tool_call_id: String,
        has_result: bool,
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
    if text.contains('\0') {
        return None;
    }
    Some(text.to_owned())
}

/// Join an ACP content array (blocks) in order; `None` when the update has no
/// usable content array.
fn joined_content(update: &Value) -> Option<String> {
    let content = update.get("content")?;
    match content {
        Value::Array(blocks) => {
            let mut text = String::new();
            for block in blocks {
                if let Some(part) = block_text(block) {
                    text.push_str(&part);
                }
            }
            Some(text)
        }
        single => block_text(single),
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
            text: joined_content(update).unwrap_or_default(),
            committed: true,
        },
        "agent_message_chunk" => UpdateKind::AgentMessage {
            message_id: bounded_id(update.get("messageId")),
            text: block_text(update.get("content").unwrap_or(&Value::Null)).unwrap_or_default(),
            committed: false,
        },
        "agent_thought_chunk" | "agent_thought" | "thought" => UpdateKind::AgentThoughtChunk {
            text: block_text(update.get("content").unwrap_or(&Value::Null)).unwrap_or_default(),
        },
        "tool_call" => UpdateKind::ToolCall {
            tool_call_id: bounded_id(update.get("toolCallId"))?,
            title: bounded_id(update.get("title")),
            tool_kind: bounded_id(update.get("kind")),
        },
        "tool_call_update" => UpdateKind::ToolCallUpdate {
            tool_call_id: bounded_id(update.get("toolCallId"))?,
            has_result: update.get("content").is_some_and(Value::is_array),
        },
        _ => UpdateKind::Untracked,
    };
    Some(SessionUpdate { session_id, kind })
}

/// Name used for tool grouping in projections: the DSH tool kind when
/// present, otherwise the bounded human title.
pub fn tool_name(kind: &UpdateKind) -> Option<String> {
    match kind {
        UpdateKind::ToolCall {
            tool_kind, title, ..
        } => tool_kind
            .clone()
            .or_else(|| title.clone())
            .map(|name| name.chars().take(64).collect::<String>()),
        UpdateKind::ToolCallUpdate { .. } => None,
        _ => None,
    }
}

/// Normalize one update into canonical `session/event` payloads consumed by
/// the shared activity and observation projections:
///
/// - message/thought text projects as `model.streaming` deltas (thoughts as
///   `reasoning_delta`, text as `text_delta` with the assistant message id);
/// - tool activity projects both as the observation-verified
///   `model.streaming`/`tool_call` shape and the activity `tool.updated`
///   shape, because the two projections listen on different types.
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
        UpdateKind::AgentThoughtChunk { text } if !text.is_empty() => {
            vec![base(json!({"kind": "reasoning_delta", "delta": text}))]
        }
        UpdateKind::ToolCall {
            tool_call_id,
            title,
            tool_kind,
        } => {
            let name = tool_kind
                .clone()
                .or_else(|| title.clone())
                .map(|name| name.chars().take(64).collect::<String>())
                .unwrap_or_else(|| "dsh_tool".into());
            vec![
                base(json!({
                    "kind": "tool_call",
                    "toolCallId": tool_call_id,
                    "toolName": name,
                })),
                json!({
                    "type": "tool.updated",
                    "eventId": event_id,
                    "turnId": turn_id,
                    "payload": {
                        "kind": "started",
                        "toolCallId": tool_call_id,
                        "toolName": name,
                    },
                }),
            ]
        }
        UpdateKind::ToolCallUpdate {
            tool_call_id,
            has_result,
        } => {
            vec![json!({
                "type": "tool.updated",
                "eventId": event_id,
                "turnId": turn_id,
                "payload": {
                    "kind": if *has_result { "result" } else { "started" },
                    "toolCallId": tool_call_id,
                },
            })]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s01_tool() -> Value {
        json!({
            "sessionId": "fixture-session",
            "update": {"type": "tool_call", "toolCallId": "tool-1", "title": "read-only probe"}
        })
    }

    fn s01_message() -> Value {
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
    fn both_dialects_parse_and_keep_message_identity() {
        let tool = parse_update(&s01_tool()).unwrap();
        assert_eq!(
            tool.kind,
            UpdateKind::ToolCall {
                tool_call_id: "tool-1".into(),
                title: Some("read-only probe".into()),
                tool_kind: None,
            }
        );
        let message = parse_update(&s01_message()).unwrap();
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
            UpdateKind::AgentThoughtChunk { text: "hmm".into() }
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
        let message = parse_update(&s01_message()).unwrap();
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

        let tool = parse_update(&s01_tool()).unwrap();
        let payloads = canonical_event_payloads(&tool, "evt-3", "turn-1");
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[0]["payload"]["kind"], "tool_call");
        assert_eq!(payloads[0]["payload"]["toolName"], "read-only probe");
        assert_eq!(payloads[1]["type"], "tool.updated");
        assert_eq!(payloads[1]["payload"]["kind"], "started");

        // Uncommitted chunks never emit a finished boundary.
        let chunk = parse_update(&upstream_chunk()).unwrap();
        let payloads = canonical_event_payloads(&chunk, "evt-4", "turn-1");
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["payload"]["kind"], "text_delta");
    }

    #[test]
    fn tool_names_prefer_kind_over_title() {
        let kind = parse_update(&json!({
            "update": {"type": "tool_call", "toolCallId": "t", "kind": "edit", "title": "Edit file"}
        }))
        .unwrap();
        assert_eq!(tool_name(&kind.kind).as_deref(), Some("edit"));
        let title_only = parse_update(&s01_tool()).unwrap();
        assert_eq!(
            tool_name(&title_only.kind).as_deref(),
            Some("read-only probe")
        );
    }
}
