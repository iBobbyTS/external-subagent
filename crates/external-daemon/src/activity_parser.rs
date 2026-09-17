use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivitySource {
    Session,
    Telemetry,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivitySampleKind {
    ReasoningDelta,
    TextDelta,
    ToolStarted { kind: PassiveToolKind },
    ToolCompleted,
    ToolFailed,
}

#[derive(Debug, Clone)]
pub(crate) struct ActivitySample {
    pub(crate) source: ActivitySource,
    pub(crate) observed_at: Instant,
    pub(crate) kind: ActivitySampleKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivityTransition {
    ModelStarted,
    ModelCompleted,
    ToolScheduled,
    ToolStarted,
    ToolCompleted,
    ToolFailed,
    PermissionRequested,
    PermissionResolved,
    TurnStarted,
    TurnCompleted,
    TurnFailed,
}

pub(crate) struct ParsedActivity {
    pub(crate) source: ActivitySource,
    pub(crate) identity: Option<String>,
    pub(crate) stream_key: Option<String>,
    pub(crate) sample: Option<ActivitySampleKind>,
    pub(crate) text_delta: Option<String>,
    pub(crate) transition: Option<ActivityTransition>,
    pub(crate) request_id: Option<String>,
    pub(crate) tool_call_id: Option<String>,
    pub(crate) tool_kind: PassiveToolKind,
    pub(crate) telemetry_known: bool,
    pub(crate) assistant_message_id: Option<String>,
    pub(crate) message_finished: bool,
    pub(crate) terminal_response: Option<String>,
}

impl ParsedActivity {
    fn runtime() -> Self {
        Self {
            source: ActivitySource::Runtime,
            identity: None,
            stream_key: None,
            sample: None,
            text_delta: None,
            transition: None,
            request_id: None,
            tool_call_id: None,
            tool_kind: PassiveToolKind::Other,
            telemetry_known: true,
            assistant_message_id: None,
            message_finished: false,
            terminal_response: None,
        }
    }
}

pub(crate) fn parse_passive_activity(event: &RuntimeEvent) -> ParsedActivity {
    match event {
        RuntimeEvent::Driver(Inbound::Message(WireMessage::Event(event))) => {
            parse_activity_message(&event.method, &event.params, ActivitySource::Session)
        }
        RuntimeEvent::Driver(Inbound::Message(WireMessage::UnknownEvent { method, raw })) => {
            let params = raw.get("params").unwrap_or(&serde_json::Value::Null);
            let source = if method == "v4/telemetry/event" {
                ActivitySource::Telemetry
            } else if method == "session/event" {
                ActivitySource::Session
            } else {
                ActivitySource::Runtime
            };
            parse_activity_message(method, params, source)
        }
        RuntimeEvent::Driver(Inbound::Message(WireMessage::Request(request)))
            if request.method == INTERACTION_REQUEST_PERMISSION
                || request.method == INTERACTION_REQUEST_USER_INPUT
                || request.method == INTERACTION_REQUEST_UNSUPPORTED_INPUT =>
        {
            let mut parsed = ParsedActivity::runtime();
            parsed.transition = Some(ActivityTransition::PermissionRequested);
            parsed.request_id = activity_id(request.params.get("requestId"));
            parsed.tool_call_id = activity_id(request.params.get("toolCallId"));
            parsed.tool_kind = classify_passive_tool(request.params.get("toolName"));
            parsed.identity = parsed
                .request_id
                .as_ref()
                .map(|id| format!("permission:{id}:requested"));
            parsed
        }
        _ => ParsedActivity::runtime(),
    }
}

fn parse_activity_message(
    method: &str,
    params: &serde_json::Value,
    source: ActivitySource,
) -> ParsedActivity {
    let mut parsed = ParsedActivity::runtime();
    parsed.source = source;
    parsed.telemetry_known = source != ActivitySource::Telemetry;
    if method == "session/event" {
        let kind = params.get("type").and_then(serde_json::Value::as_str);
        let payload = params.get("payload").unwrap_or(&serde_json::Value::Null);
        let payload_kind = payload.get("kind").and_then(serde_json::Value::as_str);
        let payload_type = payload.get("type").and_then(serde_json::Value::as_str);
        let event_id = activity_id(params.get("eventId"));
        let turn_id = activity_id(params.get("turnId"));
        match (kind, payload_kind, payload_type) {
            (Some("model.streaming"), Some("reasoning_delta"), _) => {
                parsed.stream_key = stream_key(params, payload, "reasoning");
                parsed.identity = event_id.map(|id| format!("stream:{id}"));
                parsed.sample = Some(ActivitySampleKind::ReasoningDelta);
            }
            (Some("model.streaming"), Some("text_delta"), _) => {
                let delta = payload.get("delta").and_then(serde_json::Value::as_str);
                parsed.stream_key = stream_key(params, payload, "text");
                parsed.identity = event_id.map(|id| format!("stream:{id}"));
                parsed.sample = Some(ActivitySampleKind::TextDelta);
                parsed.text_delta = delta.map(str::to_owned);
                parsed.assistant_message_id = activity_id(payload.get("assistantMessageId"));
            }
            (Some("model.streaming"), kind, _)
                if matches!(
                    kind,
                    Some("message_finished") | Some("message_done") | Some("text_done")
                ) =>
            {
                parsed.assistant_message_id = activity_id(payload.get("assistantMessageId"));
                parsed.message_finished = true;
            }
            (
                Some("message.completed" | "message.finished" | "message.done" | "text.done"),
                _,
                _,
            ) => {
                parsed.assistant_message_id = activity_id(payload.get("assistantMessageId"));
                parsed.message_finished = true;
            }
            (Some("tool.updated" | "streamRecovery.updated"), _, _) => {
                parse_tool_activity(&mut parsed, payload, source);
            }
            (Some("session.updated"), _, Some("model_request_started")) => {
                parse_model_activity(&mut parsed, payload, true);
            }
            (Some("session.updated"), _, Some("model_request_completed")) => {
                parse_model_activity(&mut parsed, payload, false);
            }
            (Some("permission.requested"), _, _) => {
                parse_permission_activity(&mut parsed, payload, true);
            }
            (Some("permission.resolved"), _, _) => {
                parse_permission_activity(&mut parsed, payload, false);
            }
            (Some("turn.started"), _, _) => {
                parsed.transition = Some(ActivityTransition::TurnStarted);
                parsed.identity = event_id.or(turn_id).map(|id| format!("turn:{id}:started"));
            }
            (Some("turn.completed"), _, _) => {
                parsed.transition = Some(ActivityTransition::TurnCompleted);
                parsed.terminal_response = payload
                    .get("response")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                parsed.identity = event_id
                    .or(turn_id)
                    .map(|id| format!("turn:{id}:completed"));
            }
            (Some("turn.failed"), _, _) => {
                parsed.transition = Some(ActivityTransition::TurnFailed);
                parsed.identity = event_id.or(turn_id).map(|id| format!("turn:{id}:failed"));
            }
            _ => {}
        }
    } else if method == "v4/telemetry/event" {
        parsed.telemetry_known = true;
        match params.get("kind").and_then(serde_json::Value::as_str) {
            Some("stream.chunk") => {
                let channel = match params.get("channel").and_then(serde_json::Value::as_str) {
                    Some("thought") => "reasoning",
                    Some("text") => "text",
                    _ => {
                        parsed.telemetry_known = false;
                        return parsed;
                    }
                };
                parsed.stream_key = stream_key(params, params, channel);
                parsed.identity =
                    activity_id(params.get("eventId")).map(|id| format!("stream:{id}"));
                parsed.sample = Some(if channel == "reasoning" {
                    ActivitySampleKind::ReasoningDelta
                } else {
                    ActivitySampleKind::TextDelta
                });
            }
            Some("tool.lifecycle") => parse_tool_activity(&mut parsed, params, source),
            Some("model.request.status") => {
                let started = params.get("status").and_then(serde_json::Value::as_str)
                    == Some("model_request_started");
                let completed = params.get("status").and_then(serde_json::Value::as_str)
                    == Some("model_request_completed");
                if started || completed {
                    parse_model_activity(&mut parsed, params, started);
                } else {
                    parsed.telemetry_known = false;
                }
            }
            Some("permission.lifecycle") => {
                match params.get("phase").and_then(serde_json::Value::as_str) {
                    Some("requested") => parse_permission_activity(&mut parsed, params, true),
                    Some("resolved") => parse_permission_activity(&mut parsed, params, false),
                    _ => parsed.telemetry_known = false,
                }
            }
            Some("turn.started") => parsed.transition = Some(ActivityTransition::TurnStarted),
            Some("turn.completed") => parsed.transition = Some(ActivityTransition::TurnCompleted),
            Some("turn.failed") => parsed.transition = Some(ActivityTransition::TurnFailed),
            Some("usage.delta") => {}
            _ => parsed.telemetry_known = false,
        }
    }
    parsed
}

fn parse_model_activity(parsed: &mut ParsedActivity, payload: &serde_json::Value, started: bool) {
    parsed.request_id = activity_id(payload.get("requestId"));
    let phase = if started { "started" } else { "completed" };
    parsed.identity = parsed
        .request_id
        .as_ref()
        .map(|id| format!("model:{id}:{phase}"));
    parsed.transition = Some(if started {
        ActivityTransition::ModelStarted
    } else {
        ActivityTransition::ModelCompleted
    });
}

fn parse_permission_activity(
    parsed: &mut ParsedActivity,
    payload: &serde_json::Value,
    requested: bool,
) {
    parsed.request_id = activity_id(payload.get("requestId"));
    parsed.tool_call_id = activity_id(payload.get("toolCallId"));
    parsed.tool_kind = classify_passive_tool(payload.get("toolName"));
    let phase = if requested { "requested" } else { "resolved" };
    parsed.identity = parsed
        .request_id
        .as_ref()
        .map(|id| format!("permission:{id}:{phase}"));
    parsed.transition = Some(if requested {
        ActivityTransition::PermissionRequested
    } else {
        ActivityTransition::PermissionResolved
    });
}

fn parse_tool_activity(
    parsed: &mut ParsedActivity,
    payload: &serde_json::Value,
    source: ActivitySource,
) {
    let phase = payload
        .get(if source == ActivitySource::Telemetry {
            "phase"
        } else {
            "kind"
        })
        .and_then(serde_json::Value::as_str)
        .and_then(|phase| match phase {
            "scheduled" => Some(ActivityTransition::ToolScheduled),
            "started" => Some(ActivityTransition::ToolStarted),
            "result" | "tool_result" | "completed" => Some(ActivityTransition::ToolCompleted),
            "error" | "tool_error" | "failed" => Some(ActivityTransition::ToolFailed),
            "batch" => None,
            _ => {
                if source == ActivitySource::Telemetry {
                    parsed.telemetry_known = false;
                }
                None
            }
        });
    parsed.tool_call_id = activity_id(payload.get("toolCallId"));
    parsed.tool_kind = classify_passive_tool(payload.get("toolName"));
    parsed.transition = phase;
    if let (Some(tool_call_id), Some(phase)) = (parsed.tool_call_id.as_ref(), phase) {
        let phase_name = match phase {
            ActivityTransition::ToolScheduled => "scheduled",
            ActivityTransition::ToolStarted => "started",
            ActivityTransition::ToolCompleted => "completed",
            ActivityTransition::ToolFailed => "failed",
            _ => return,
        };
        parsed.identity = Some(format!("tool:{tool_call_id}:{phase_name}"));
        parsed.sample = match phase {
            ActivityTransition::ToolStarted => Some(ActivitySampleKind::ToolStarted {
                kind: parsed.tool_kind,
            }),
            ActivityTransition::ToolCompleted => Some(ActivitySampleKind::ToolCompleted),
            ActivityTransition::ToolFailed => Some(ActivitySampleKind::ToolFailed),
            _ => None,
        };
    }
}

fn stream_key(
    params: &serde_json::Value,
    payload: &serde_json::Value,
    channel: &str,
) -> Option<String> {
    let turn_id = activity_id(params.get("turnId"));
    let message_id = activity_id(payload.get("assistantMessageId"));
    match (turn_id, message_id) {
        (Some(turn_id), Some(message_id)) => Some(format!("{turn_id}:{message_id}:{channel}")),
        (None, Some(message_id)) => Some(format!("{message_id}:{channel}")),
        _ => None,
    }
}

fn activity_id(value: Option<&serde_json::Value>) -> Option<String> {
    value
        .and_then(serde_json::Value::as_str)
        .filter(|value| {
            !value.is_empty() && value.len() <= MAX_ACTIVITY_ID_BYTES && !value.contains('\0')
        })
        .map(str::to_owned)
}

fn classify_passive_tool(value: Option<&serde_json::Value>) -> PassiveToolKind {
    match value.and_then(serde_json::Value::as_str) {
        Some("Read" | "read") => PassiveToolKind::Read,
        Some("Bash" | "bash") => PassiveToolKind::Bash,
        _ => PassiveToolKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_text_and_permission_events_keep_bounded_identity() {
        let text = parse_activity_message(
            "session/event",
            &serde_json::json!({"type":"model.streaming","eventId":"e1","turnId":"t1","payload":{"kind":"text_delta","delta":"hi","assistantMessageId":"m1"}}),
            ActivitySource::Session,
        );
        assert_eq!(text.identity.as_deref(), Some("stream:e1"));
        assert_eq!(text.stream_key.as_deref(), Some("t1:m1:text"));
        assert_eq!(text.text_delta.as_deref(), Some("hi"));

        let permission = parse_activity_message(
            "session/event",
            &serde_json::json!({"type":"permission.requested","payload":{"requestId":"r1","toolCallId":"c1","toolName":"Bash"}}),
            ActivitySource::Session,
        );
        assert_eq!(
            permission.transition,
            Some(ActivityTransition::PermissionRequested)
        );
        assert_eq!(permission.tool_kind, PassiveToolKind::Bash);
    }

    #[test]
    fn telemetry_unknown_fields_degrade_without_inventing_activity() {
        let parsed = parse_activity_message(
            "v4/telemetry/event",
            &serde_json::json!({"kind":"future.event","secret":"not projected"}),
            ActivitySource::Telemetry,
        );
        assert!(!parsed.telemetry_known);
        assert!(parsed.identity.is_none());
        assert!(parsed.sample.is_none());
        assert!(parsed.transition.is_none());
    }
}
